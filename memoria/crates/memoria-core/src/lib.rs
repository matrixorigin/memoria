pub mod error;
pub mod interfaces;
pub mod sensitivity;
pub mod types;

pub use error::MemoriaError;
pub use sensitivity::{check_sensitivity, SensitivityResult, SensitivityTier};
pub use types::{Memory, MemoryType, TrustTier, FEEDBACK_SIGNALS};

/// Identifier allowlist used for generated and legacy physical table names.
/// Legacy names may contain Unicode letters/digits. New branch names are ASCII,
/// but validation must not strand previously created Unicode tables.
pub fn is_safe_sql_identifier(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_')
}

/// Workaround: MO#24001 — PREPARE/EXECUTE stores `Option<String>::None` as empty
/// string `''` instead of SQL NULL for VARCHAR columns.  Normalize at both write
/// (bind) and read boundaries so the rest of the codebase can treat `None` and
/// `Some("")` identically.
///
/// Write-side: normalize for `.bind()` — returns `None` for both `None` and `Some("")`.
#[inline]
pub fn nullable_str(opt: &Option<String>) -> Option<&str> {
    opt.as_deref().filter(|s| !s.is_empty())
}

/// Read-side: normalize after `row.try_get()` — converts `Some("")` back to `None`.
#[inline]
pub fn nullable_str_from_row(opt: Option<String>) -> Option<String> {
    opt.filter(|s| !s.is_empty())
}

/// Truncate a string to at most `max_bytes` bytes, rounding down to a valid UTF-8 char boundary.
pub fn truncate_utf8(s: &str, max_bytes: usize) -> &str {
    let len = s.len().min(max_bytes);
    // Find the nearest valid UTF-8 char boundary at or before `len`
    let mut end = len;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Reserved persistence fingerprint for atomic store receipts; not caller metadata.
pub const STORE_REQUEST_HASH_FIELD: &str = "_memoria_store_request_sha256";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_sql_identifier_accepts_alphanumeric_and_underscore_only() {
        for name in [
            "mem_memories",
            "br_1a2b3c4d_feature",
            "A1",
            "_",
            "记忆表",
            "émoire_1",
        ] {
            assert!(is_safe_sql_identifier(name), "rejected safe name: {name}");
        }
        for name in [
            "",
            "a-b",
            "a b",
            "a.b",
            "a`b",
            "a;drop table t",
            "a'b",
            "a\"b",
            "a\nb",
            "a/*x*/",
            "tab\u{0}le",
        ] {
            assert!(
                !is_safe_sql_identifier(name),
                "accepted unsafe name: {name:?}"
            );
        }
    }

    #[test]
    fn nullable_str_treats_empty_as_null_on_write() {
        assert_eq!(nullable_str(&None), None);
        assert_eq!(nullable_str(&Some(String::new())), None);
        assert_eq!(nullable_str(&Some("s1".into())), Some("s1"));
        // Whitespace is preserved; only the MO#24001 empty-string sentinel is mapped.
        assert_eq!(nullable_str(&Some(" ".into())), Some(" "));
    }

    #[test]
    fn nullable_str_from_row_treats_empty_as_null_on_read() {
        assert_eq!(nullable_str_from_row(None), None);
        assert_eq!(nullable_str_from_row(Some(String::new())), None);
        assert_eq!(nullable_str_from_row(Some("s1".into())), Some("s1".into()));
        assert_eq!(nullable_str_from_row(Some(" ".into())), Some(" ".into()));
    }

    #[test]
    fn truncate_utf8_keeps_short_strings_intact() {
        assert_eq!(truncate_utf8("", 0), "");
        assert_eq!(truncate_utf8("", 10), "");
        assert_eq!(truncate_utf8("hello", 5), "hello");
        assert_eq!(truncate_utf8("hello", 100), "hello");
        assert_eq!(truncate_utf8("hello", 0), "");
        assert_eq!(truncate_utf8("hello", 3), "hel");
    }

    #[test]
    fn truncate_utf8_rounds_down_to_char_boundaries() {
        // 'é' = 2 bytes, '中' = 3 bytes, '🦀' = 4 bytes.
        let s = "aé中🦀z";
        assert_eq!(s.len(), 1 + 2 + 3 + 4 + 1);
        let expected = [
            (0, ""),
            (1, "a"),
            (2, "a"),
            (3, "aé"),
            (4, "aé"),
            (5, "aé"),
            (6, "aé中"),
            (7, "aé中"),
            (8, "aé中"),
            (9, "aé中"),
            (10, "aé中🦀"),
            (11, "aé中🦀z"),
            (12, "aé中🦀z"),
        ];
        for (max_bytes, want) in expected {
            let got = truncate_utf8(s, max_bytes);
            assert_eq!(got, want, "max_bytes={max_bytes}");
            assert!(got.len() <= max_bytes);
        }
    }

    #[test]
    fn truncate_utf8_never_panics_for_any_limit() {
        let s = "数据库🦀é-x";
        for max_bytes in 0..=s.len() + 2 {
            let got = truncate_utf8(s, max_bytes);
            assert!(s.starts_with(got));
            assert!(got.len() <= max_bytes);
            // The result is maximal: the next char would exceed the limit.
            if let Some(next) = s[got.len()..].chars().next() {
                assert!(got.len() + next.len_utf8() > max_bytes);
            }
        }
    }
}
