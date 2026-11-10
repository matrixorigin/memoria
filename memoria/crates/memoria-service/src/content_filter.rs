//! Request-scoped sensitivity policy for trusted, owner-scoped adapters.
//!
//! Filtering is always enabled outside an explicit trusted scope. The HTTP
//! boundary must authenticate the proxy before installing an override. Binding
//! the override to the resolved owner prevents nested work for another owner
//! from inheriting it. Spawned background tasks do not inherit task locals.

use std::future::Future;

use memoria_core::{check_sensitivity, SensitivityResult};

tokio::task_local! {
    static UNFILTERED_OWNER: Option<String>;
}

/// Run inline work under a server-authorized content policy. `Some(owner)`
/// preserves that owner's content; `None` explicitly enforces filtering.
///
/// This is a trusted embedding API, not a caller-controlled request option.
/// The policy is restored on completion, cancellation, or unwinding. Background
/// jobs deliberately retain the default filtering policy.
pub async fn with_content_filter_scope<F: Future>(owner: Option<String>, work: F) -> F::Output {
    UNFILTERED_OWNER.scope(owner, work).await
}

/// Apply the built-in filter unless this exact owner has an authenticated,
/// request-local override. No memory text or matched substring is logged.
pub fn check_content_sensitivity(owner: &str, content: &str) -> SensitivityResult {
    let unfiltered = UNFILTERED_OWNER
        .try_with(|scope| scope.as_deref() == Some(owner))
        .unwrap_or(false);
    if unfiltered {
        return SensitivityResult {
            blocked: false,
            redacted_content: None,
            matched_labels: vec![],
        };
    }

    let result = check_sensitivity(content);
    if result.blocked {
        tracing::warn!(
            rules = ?result.matched_labels,
            "memory content rejected by sensitivity filter"
        );
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // An ordinary sentence currently matches the built-in Bearer pattern.
    const TEXT: &str = "The standard bearer carried the flag.";

    #[tokio::test]
    async fn override_skips_both_blocking_and_redaction_for_all_builtin_rules() {
        // Synthetic format fixtures only; no real credentials or personal data.
        let cases = [
            (format!("AKIA{}", "0".repeat(16)), true),
            ("-----BEGIN PRIVATE KEY-----".into(), true),
            ("Bearer SYNTHETIC_EXAMPLE".into(), true),
            ("password=synthetic-example".into(), true),
            ("person@example.com".into(), false),
            ("555-123-4567".into(), false),
            ("123-45-6789".into(), false),
            ("4111 1111 1111 1111".into(), false),
        ];
        for (text, blocked) in cases {
            let default = check_content_sensitivity("owner-a", &text);
            assert_eq!(default.blocked, blocked);
            assert_eq!(default.redacted_content.is_some(), !blocked);
            with_content_filter_scope(Some("owner-a".into()), async {
                let result = check_content_sensitivity("owner-a", &text);
                assert!(!result.blocked);
                assert!(result.redacted_content.is_none());
                assert!(result.matched_labels.is_empty());
            })
            .await;
        }
    }

    #[tokio::test]
    async fn override_is_exact_owner_bound_and_nested_policy_is_restored() {
        assert!(check_content_sensitivity("owner-a", TEXT).blocked);
        with_content_filter_scope(Some("owner-a".into()), async {
            assert!(!check_content_sensitivity("owner-a", TEXT).blocked);
            assert!(check_content_sensitivity("owner-b", TEXT).blocked);
            with_content_filter_scope(None, async {
                assert!(check_content_sensitivity("owner-a", TEXT).blocked);
            })
            .await;
            assert!(!check_content_sensitivity("owner-a", TEXT).blocked);
        })
        .await;
        assert!(check_content_sensitivity("owner-a", TEXT).blocked);
    }

    #[tokio::test]
    async fn override_is_not_inherited_by_spawned_jobs_or_other_requests() {
        let (unfiltered, filtered) = tokio::join!(
            with_content_filter_scope(Some("owner-a".into()), async {
                tokio::task::yield_now().await;
                assert!(
                    tokio::spawn(async { check_content_sensitivity("owner-a", TEXT).blocked })
                        .await
                        .unwrap()
                );
                check_content_sensitivity("owner-a", TEXT).blocked
            }),
            async {
                tokio::task::yield_now().await;
                check_content_sensitivity("owner-a", TEXT).blocked
            }
        );
        assert!(!unfiltered);
        assert!(filtered);
    }

    #[tokio::test]
    async fn dropping_scoped_work_does_not_leak_override() {
        let mut work = Box::pin(with_content_filter_scope(Some("owner-a".into()), async {
            assert!(!check_content_sensitivity("owner-a", TEXT).blocked);
            std::future::pending::<()>().await;
        }));
        std::future::poll_fn(|cx| {
            assert!(work.as_mut().poll(cx).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(work);
        assert!(check_content_sensitivity("owner-a", TEXT).blocked);
    }
}
