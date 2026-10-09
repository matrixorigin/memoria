pub mod auth;
pub mod instrumented_embedder;
pub mod metrics;
pub mod metrics_summary;
pub mod models;
pub mod otel;
pub mod rate_limit;
pub mod routes;
pub mod state;

pub use instrumented_embedder::InstrumentedEmbedder;
pub use state::AppState;

use axum::{
    extract::DefaultBodyLimit,
    http::StatusCode,
    middleware,
    response::IntoResponse,
    routing::{delete, get, post, put},
    Json, Router,
};
use tower_http::catch_panic::CatchPanicLayer;

// ── Call-log middleware ───────────────────────────────────────────────────────

/// Wraps every `/v1/*` request to record timing and HTTP status into
/// `mem_api_call_log` via the batched `CallLogBatcher`.
///
/// The `CallLogContext` extension is inserted into the request *before* the
/// handler runs.  The `AuthUser` extractor fills in the resolved `user_id`
/// inside that context.  After `next.run()` the middleware reads back the
/// user_id and enqueues the entry for batched persistence.
async fn call_log_mw(
    axum::extract::State(state): axum::extract::State<AppState>,
    mut req: axum::http::Request<axum::body::Body>,
    next: middleware::Next,
) -> axum::response::Response {
    // Only instrument /v1/* endpoints.
    if !req.uri().path().starts_with("/v1/") {
        return next.run(req).await;
    }

    // ── Dashboard exclusion filter ─────────────────────────────────────────
    // The website backend tags its own proxy calls with `X-Memoria-Source: dashboard`.
    // These are dashboard-initiated operations (Memories panel, Playground, etc.)
    // that should NOT inflate agent call statistics.
    //
    // All other callers (MCP agents, CLI, external integrations) are recorded
    // by default — no special header required from them.
    let is_dashboard = req
        .headers()
        .get("x-memoria-source")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("dashboard"));

    let t = std::time::Instant::now();
    let method = req.method().as_str().to_string();
    let path = req.uri().path().to_string();

    // Inject shared context so AuthUser extractor can deposit the resolved user_id.
    let ctx = auth::CallLogContext::default();
    req.extensions_mut().insert(ctx.clone());

    let response = next.run(req).await;

    let latency_ms = t.elapsed().as_millis() as u32;
    let status_code = response.status().as_u16();

    if let Ok(guard) = ctx.0.lock() {
        if let Some(uid) = guard.as_ref() {
            if !is_dashboard {
                state.call_log_batcher.record(
                    uid.clone(),
                    method.clone(),
                    path.clone(),
                    status_code,
                    latency_ms,
                );
            }
            // Push-based ops metrics (non-blocking, best-effort).
            // Dashboard-originated calls are excluded to avoid inflating agent stats.
            // MCP calls are skipped here: mcp_handler reports ApiCallLogged with the
            // accurate track_path (e.g. /mcp/remember) and RPC-level success flag,
            // which is necessary because JSON-RPC errors return HTTP 200.
            if !is_dashboard && !path.starts_with("/v1/mcp") {
                if let Some(reporter) = &state.stats_reporter {
                    reporter.report(memoria_service::stats_reporter::StatsEvent::ApiCallLogged {
                        user_id: uid.clone(),
                        path: path.clone(),
                        is_mcp: false,
                        is_success: status_code < 400,
                    });
                }
            }
            if let Some(mask) = should_mark_metrics_dirty(&method, &path, status_code) {
                let state = state.clone();
                let user_id = uid.clone();
                tokio::spawn(async move {
                    if let Err(e) = state.mark_metrics_dirty(&user_id, mask).await {
                        tracing::warn!(
                            user_id = user_id,
                            error = %e,
                            "failed to mark metrics summary dirty"
                        );
                    }
                });
            }
        }
    }

    response
}

fn should_mark_metrics_dirty(
    method: &str,
    path: &str,
    status_code: u16,
) -> Option<metrics_summary::DirtyMask> {
    use metrics_summary::DirtyMask;

    if status_code >= 400 {
        return None;
    }

    // Fixed-path POST routes
    if method == "POST" {
        return match path {
            "/v1/memories"
            | "/v1/memories/batch"
            | "/v1/memories/correct"
            | "/v1/memories/purge"
            | "/v1/observe"
            | "/v1/observe/deduplicated"
            | "/v1/pipeline/run" => Some(DirtyMask::MEMORY),
            "/v1/governance"
            | "/v1/consolidate"
            | "/v1/reflect"
            | "/v1/extract-entities"
            | "/v1/extract-entities/link" => Some(DirtyMask::MEMORY | DirtyMask::GRAPH),
            "/v1/snapshots" | "/v1/snapshots/delete" => Some(DirtyMask::SNAPSHOT),
            "/v1/branches" => Some(DirtyMask::BRANCH),
            _ => {
                // Dynamic path patterns
                if path.starts_with("/v1/memories/") && path.ends_with("/feedback") {
                    Some(DirtyMask::FEEDBACK)
                } else if path.starts_with("/v1/snapshots/") && path.ends_with("/rollback")
                    || path.starts_with("/v1/branches/") && path.ends_with("/checkout")
                    || path.starts_with("/v1/branches/") && path.ends_with("/merge")
                    || path.starts_with("/v1/branches/") && path.ends_with("/pick")
                {
                    Some(DirtyMask::FULL)
                } else if (path.starts_with("/v1/branches/") && path.ends_with("/apply"))
                    || (path.starts_with("/v1/sessions/") && path.ends_with("/summary"))
                {
                    Some(DirtyMask::MEMORY)
                } else {
                    None
                }
            }
        };
    }

    if method == "DELETE" {
        if path.starts_with("/v1/memories/") {
            return Some(DirtyMask::MEMORY);
        }
        if path.starts_with("/v1/snapshots/") {
            return Some(DirtyMask::SNAPSHOT);
        }
        if path.starts_with("/v1/branches/") {
            return Some(DirtyMask::BRANCH);
        }
    }

    if method == "PUT" && path.starts_with("/v1/memories/") && path.ends_with("/correct") {
        return Some(DirtyMask::MEMORY);
    }

    None
}

/// Build the full API router with all routes.
///
/// Routes are organised into three groups:
///
/// 1. **Write routes** — guarded by [`auth::group_main_write_guard`] so that
///    group-scoped requests on `main` are automatically rejected (403) without
///    each handler needing a manual check.
/// 2. **Merge route** — guarded by [`auth::group_merge_guard`] which blocks
///    native merge entirely in group mode.
/// 3. **Read / other routes** — no group-mode restrictions.
pub fn build_router(state: AppState) -> Router {
    // ── Write routes (group main-write guard) ────────────────────────────
    // All endpoints that mutate the active memory table.
    // In group mode these are forbidden when checked-out on `main`.
    let write_routes = Router::new()
        .route("/v1/memories", post(routes::memory::store_memory))
        .route("/v1/memories/batch", post(routes::memory::batch_store))
        .route(
            "/v1/memories/correct",
            post(routes::memory::correct_by_query),
        )
        .route("/v1/memories/purge", post(routes::memory::purge_memories))
        .route(
            "/v1/memories/:id/correct",
            put(routes::memory::correct_memory),
        )
        .route("/v1/memories/:id", delete(routes::memory::delete_memory))
        .route("/v1/observe", post(routes::memory::observe_turn))
        .route("/v1/observe/deduplicated", post(routes::memory::observe_turn_deduplicated))
        .route(
            "/v1/sessions/:session_id/summary",
            post(routes::sessions::create_session_summary),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::group_main_write_guard,
        ));

    // ── Merge route (group merge guard) ──────────────────────────────────
    // Native merge is entirely disabled in group mode.
    let merge_route = Router::new()
        .route(
            "/v1/branches/:name/merge",
            post(routes::snapshots::merge_branch),
        )
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::group_merge_guard,
        ));

    // ── Read / ungrouped routes ──────────────────────────────────────────
    Router::new()
        // Streamable HTTP MCP endpoint
        .route("/mcp", post(routes::mcp::mcp_handler))
        // Health
        .route("/health", get(routes::memory::health))
        .route("/health/instance", get(routes::memory::health_instance))
        // Metrics
        .route("/metrics", get(routes::metrics::prometheus_metrics))
        // Memory reads
        .route("/v1/memories", get(routes::memory::list_memories))
        .route("/v1/memories/query", post(routes::memory::query_memories))
        .route(
            "/v1/memories/fulltext-search",
            post(routes::memory::fulltext_search_memories),
        )
        .route("/v1/memories/retrieve", post(routes::memory::retrieve))
        .route("/v1/memories/search", post(routes::memory::search))
        .route("/v1/memories/:id", get(routes::memory::get_memory))
        .route(
            "/v1/memories/:id/history",
            get(routes::memory::get_memory_history),
        )
        .route(
            "/v1/profiles/:target_user_id",
            get(routes::memory::get_profile),
        )
        // Feedback
        .route(
            "/v1/memories/:id/feedback",
            post(routes::memory::record_feedback),
        )
        .route(
            "/v1/feedback/stats",
            get(routes::memory::get_feedback_stats),
        )
        .route(
            "/v1/feedback/by-tier",
            get(routes::memory::get_feedback_by_tier),
        )
        // Retrieval params
        .route(
            "/v1/retrieval-params",
            get(routes::memory::get_retrieval_params).put(routes::memory::set_retrieval_params),
        )
        .route(
            "/v1/retrieval-params/tune",
            post(routes::memory::tune_retrieval_params),
        )
        // Governance
        .route("/v1/governance", post(routes::governance::governance))
        .route("/v1/consolidate", post(routes::governance::consolidate))
        .route("/v1/reflect", post(routes::governance::reflect))
        .route(
            "/v1/extract-entities",
            post(routes::governance::extract_entities),
        )
        .route(
            "/v1/extract-entities/link",
            post(routes::governance::link_entities),
        )
        .route("/v1/entities", get(routes::governance::get_entities))
        // Snapshots
        .route("/v1/snapshots", get(routes::snapshots::list_snapshots))
        .route("/v1/snapshots", post(routes::snapshots::create_snapshot))
        .route(
            "/v1/snapshots/delete",
            post(routes::snapshots::delete_snapshot_bulk),
        )
        .route("/v1/snapshots/:name", get(routes::snapshots::get_snapshot))
        .route(
            "/v1/snapshots/:name",
            delete(routes::snapshots::delete_snapshot),
        )
        .route(
            "/v1/snapshots/:name/rollback",
            post(routes::snapshots::rollback),
        )
        .route(
            "/v1/snapshots/:name/diff",
            get(routes::snapshots::diff_snapshot),
        )
        // Branches
        .route("/v1/branches", get(routes::snapshots::list_branches))
        .route("/v1/branches", post(routes::snapshots::create_branch))
        .route(
            "/v1/branches/:name/checkout",
            post(routes::snapshots::checkout_branch),
        )
        .route(
            "/v1/branches/:name/diff",
            get(routes::snapshots::diff_branch),
        )
        .route(
            "/v1/branches/:name/pick",
            post(routes::snapshots::pick_branch),
        )
        .route(
            "/v1/branches/:name/diff-items",
            get(routes::snapshots::diff_branch_items),
        )
        .route(
            "/v1/branches/:name/apply",
            post(routes::snapshots::apply_branch),
        )
        .route(
            "/v1/branches/:name",
            delete(routes::snapshots::delete_branch),
        )
        // Sessions
        .route("/v1/tasks/:task_id", get(routes::sessions::get_task_status))
        // API key management
        .route("/auth/whoami", get(routes::auth::whoami))
        .route("/auth/keys", post(routes::auth::create_key))
        .route("/auth/keys", get(routes::auth::list_keys))
        .route("/auth/keys/:id", get(routes::auth::get_key))
        .route("/auth/keys/:id/rotate", put(routes::auth::rotate_key))
        .route("/auth/keys/:id", delete(routes::auth::revoke_key))
        // Group management (user-facing)
        .route("/v1/groups", post(routes::groups::create_group))
        .route("/v1/groups", get(routes::groups::list_my_groups))
        .route("/v1/groups/:group_id", get(routes::groups::get_group))
        .route(
            "/v1/groups/:group_id/members/:user_id",
            post(routes::groups::add_member),
        )
        .route(
            "/v1/groups/:group_id/members/:user_id",
            delete(routes::groups::remove_member),
        )
        .route("/v1/groups/:group_id", delete(routes::groups::delete_group))
        // Merge write routes and merge-guarded route
        .merge(write_routes)
        .merge(merge_route)
        // Admin
        .route("/admin/stats", get(routes::admin::system_stats))
        .route("/admin/config", get(routes::admin::get_config))
        .route("/admin/users", get(routes::admin::list_users))
        .route(
            "/admin/users/:user_id/stats",
            get(routes::admin::user_stats),
        )
        .route(
            "/admin/users/:user_id/branch-stats",
            get(routes::admin::user_branch_stats),
        )
        .route(
            "/admin/users/:user_id/call-stats",
            get(routes::admin::user_call_stats),
        )
        .route("/admin/users/:user_id", delete(routes::admin::delete_user))
        .route(
            "/admin/users/:user_id/keys",
            get(routes::admin::list_user_keys),
        )
        .route(
            "/admin/users/:user_id/keys",
            delete(routes::admin::revoke_all_user_keys),
        )
        .route(
            "/admin/users/:user_id/params",
            post(routes::admin::set_user_params),
        )
        .route(
            "/admin/users/:user_id/reset-access-counts",
            post(routes::admin::reset_access_counts),
        )
        .route(
            "/admin/users/:user_id/strategy",
            post(routes::admin::set_user_strategy),
        )
        .route(
            "/admin/governance/:user_id/trigger",
            post(routes::admin::trigger_governance),
        )
        // Health
        .route("/v1/health/analyze", get(routes::admin::health_analyze))
        .route("/v1/health/storage", get(routes::admin::health_storage))
        .route("/v1/health/capacity", get(routes::admin::health_capacity))
        .route("/v1/health/hygiene", get(routes::admin::health_hygiene))
        .route(
            "/admin/health/hygiene",
            get(routes::admin::health_hygiene_global),
        )
        // Pipeline
        .route("/v1/pipeline/run", post(routes::memory::run_pipeline))
        // Tool usage (in-memory cache, no DB hit)
        .route("/v1/tool-usage", get(routes::memory::get_tool_usage))
        // Plugins
        .route("/admin/plugins/signers", get(routes::plugins::list_signers))
        .route(
            "/admin/plugins/signers",
            post(routes::plugins::upsert_signer),
        )
        .route("/admin/plugins", get(routes::plugins::list_packages))
        .route("/admin/plugins", post(routes::plugins::publish_package))
        .route(
            "/admin/plugins/:plugin_key/:version/review",
            post(routes::plugins::review_package),
        )
        .route(
            "/admin/plugins/:plugin_key/:version/score",
            post(routes::plugins::score_package),
        )
        .route(
            "/admin/plugins/domains/:domain/bindings",
            get(routes::plugins::list_binding_rules),
        )
        .route(
            "/admin/plugins/domains/:domain/bindings",
            post(routes::plugins::upsert_binding_rule),
        )
        .route(
            "/admin/plugins/domains/:domain/activate",
            post(routes::plugins::activate_binding),
        )
        .route(
            "/admin/plugins/matrix",
            get(routes::plugins::list_compatibility_matrix),
        )
        .route(
            "/admin/plugins/events",
            get(routes::plugins::list_audit_events),
        )
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state.clone(),
            auth::actor_scope_layer,
        ))
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024)) // 2 MB
        .layer(axum::middleware::from_fn(metrics::middleware::http_metrics))
        .layer(
            tower_http::trace::TraceLayer::new_for_http()
                .make_span_with(|req: &axum::http::Request<_>| {
                    tracing::info_span!(
                        "http",
                        method = %req.method(),
                        path = %req.uri().path(),
                    )
                })
                .on_response(
                    |res: &axum::http::Response<_>,
                     latency: std::time::Duration,
                     _span: &tracing::Span| {
                        let status = res.status().as_u16();
                        if status >= 500 {
                            tracing::error!(status, latency_ms = latency.as_millis(), "response");
                        } else if status == 429 {
                            // rate-limit hits are operationally important
                            tracing::warn!(status, latency_ms = latency.as_millis(), "response");
                        } else if latency.as_secs() >= 2 {
                            tracing::warn!(
                                status,
                                latency_ms = latency.as_millis(),
                                "slow response"
                            );
                        } else {
                            tracing::debug!(status, latency_ms = latency.as_millis(), "response");
                        }
                    },
                ),
        )
        .layer(middleware::from_fn_with_state(state, call_log_mw))
        .layer(CatchPanicLayer::custom(
            |err: Box<dyn std::any::Any + Send>| {
                let detail = err
                    .downcast_ref::<String>()
                    .map(|s| s.as_str())
                    .or_else(|| err.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown");
                tracing::error!(panic = detail, "handler panicked");
                let body = serde_json::json!({ "error": "Internal server error" });
                (StatusCode::INTERNAL_SERVER_ERROR, Json(body)).into_response()
            },
        ))
}

#[cfg(test)]
mod tests {
    use super::should_mark_metrics_dirty;
    use crate::metrics_summary::DirtyMask;

    #[test]
    fn marks_summary_dirty_for_mutating_v1_routes() {
        assert!(should_mark_metrics_dirty("POST", "/v1/memories", 200).is_some());
        assert!(should_mark_metrics_dirty("POST", "/v1/snapshots/demo/rollback", 200).is_some());
        assert!(should_mark_metrics_dirty("DELETE", "/v1/branches/demo", 204).is_some());
        assert!(should_mark_metrics_dirty("POST", "/v1/sessions/abc/summary", 200).is_some());
    }

    #[test]
    fn skips_summary_dirty_mark_for_reads_and_failures() {
        assert!(should_mark_metrics_dirty("GET", "/v1/memories", 200).is_none());
        assert!(should_mark_metrics_dirty("POST", "/v1/memories/retrieve", 200).is_none());
        assert!(should_mark_metrics_dirty("POST", "/v1/memories", 500).is_none());
    }

    #[test]
    fn correct_dirty_bits_for_operations() {
        // Memory operations → MEMORY bit
        let m = should_mark_metrics_dirty("POST", "/v1/memories", 200).unwrap();
        assert!(m.contains(DirtyMask::MEMORY));
        assert!(!m.contains(DirtyMask::GRAPH));

        // Feedback → FEEDBACK bit
        let m = should_mark_metrics_dirty("POST", "/v1/memories/abc/feedback", 200).unwrap();
        assert!(m.contains(DirtyMask::FEEDBACK));
        assert!(!m.contains(DirtyMask::MEMORY));

        // Governance → MEMORY + GRAPH
        let m = should_mark_metrics_dirty("POST", "/v1/governance", 200).unwrap();
        assert!(m.contains(DirtyMask::MEMORY));
        assert!(m.contains(DirtyMask::GRAPH));

        // Rollback → FULL
        let m = should_mark_metrics_dirty("POST", "/v1/snapshots/s1/rollback", 200).unwrap();
        assert_eq!(m, DirtyMask::FULL);

        // Snapshot → SNAPSHOT
        let m = should_mark_metrics_dirty("POST", "/v1/snapshots", 200).unwrap();
        assert!(m.contains(DirtyMask::SNAPSHOT));

        // Branch → BRANCH
        let m = should_mark_metrics_dirty("POST", "/v1/branches", 200).unwrap();
        assert!(m.contains(DirtyMask::BRANCH));
    }
}
