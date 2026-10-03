//! Sensitivity filter — tiered PII/credential detection for long-term memory.
//!
//! Three tiers:
//!   HIGH   (passwords, API keys, private keys) → block entire memory
//!   MEDIUM (email, phone, SSN, credit card)    → redact in-place, keep memory
//!   LOW    (usernames)                          → allow through unchanged

use std::borrow::Cow;

#[derive(Debug, PartialEq)]
pub enum SensitivityTier {
    High,
    Medium,
}

#[derive(Debug)]
pub struct SensitivityResult {
    /// True if memory must be discarded (HIGH tier match)
    pub blocked: bool,
    /// Redacted content if MEDIUM tier matched; None if content is safe as-is
    pub redacted_content: Option<String>,
    /// Labels of matched patterns
    pub matched_labels: Vec<&'static str>,
}

struct Pattern {
    label: &'static str,
    tier: SensitivityTier,
    regex: &'static str,
    replacement: &'static str,
}

// Patterns defined as static strings; compiled lazily via once_cell
static PATTERNS: &[Pattern] = &[
    // HIGH — block
    Pattern {
        label: "aws_key",
        tier: SensitivityTier::High,
        regex: r"(?:AKIA|ABIA|ACCA|ASIA)[0-9A-Z]{16}",
        replacement: "",
    },
    Pattern {
        label: "private_key",
        tier: SensitivityTier::High,
        regex: r"-----BEGIN (?:RSA |EC |DSA )?PRIVATE KEY-----",
        replacement: "",
    },
    Pattern {
        label: "bearer_token",
        tier: SensitivityTier::High,
        regex: r"(?i)Bearer\s+[A-Za-z0-9\-._~+/]+=*",
        replacement: "",
    },
    Pattern {
        label: "password_assign",
        tier: SensitivityTier::High,
        regex: r"(?i)(?:password|passwd|secret)\s*[:=]\s*\S+",
        replacement: "",
    },
    // MEDIUM — redact
    Pattern {
        label: "email",
        tier: SensitivityTier::Medium,
        regex: r"[a-zA-Z0-9_.+-]+@[a-zA-Z0-9-]+\.[a-zA-Z]{2,}",
        replacement: "[email]",
    },
    Pattern {
        label: "phone",
        tier: SensitivityTier::Medium,
        regex: r"\b\d{3}[-.]?\d{3,4}[-.]?\d{4}\b",
        replacement: "[phone]",
    },
    Pattern {
        label: "ssn",
        tier: SensitivityTier::Medium,
        regex: r"\b\d{3}-\d{2}-\d{4}\b",
        replacement: "[ssn]",
    },
    Pattern {
        label: "credit_card",
        tier: SensitivityTier::Medium,
        regex: r"\b(?:\d[ -]*?){13,19}\b",
        replacement: "[card]",
    },
];

use once_cell::sync::Lazy;
use regex::Regex;

static COMPILED: Lazy<Vec<(&'static Pattern, Regex)>> = Lazy::new(|| {
    PATTERNS
        .iter()
        .map(|p| (p, Regex::new(p.regex).expect("valid regex")))
        .collect()
});

// A reference to a value in source code is not itself a credential. Only
// recognize expressions with explicit code syntax, not arbitrary bare words.
static VALUE_REFERENCE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"^(?:[A-Za-z_][A-Za-z0-9_]*\.)*[A-Za-z_][A-Za-z0-9_]*[ \t]*[\[(]|^(?:hashed_password|password_hash|password|passwd)[ \t]*[,)]")
        .expect("valid value reference regex")
});

static VALUE_LOOKUP: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r#"^(?:[A-Za-z_][A-Za-z0-9_]*\.)*[A-Za-z_][A-Za-z0-9_]*(?:\.get\(\s*(?:'[^'\r\n]*'|"[^"\r\n]*")\s*\)|\[\s*(?:'[^'\r\n]*'|"[^"\r\n]*")\s*\])$"#)
        .expect("valid value lookup regex")
});

static NUMERIC_LITERAL: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?:^|[^A-Za-z0-9_])[0-9]").expect("valid numeric literal regex"));
static COLUMN_TYPE_LENGTH: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\bdb\.String\(\s*[0-9]+\s*\)").expect("valid column type regex"));

// Inspect the entire RHS expression, not the credential pattern's first
// whitespace-delimited token. Defaults and multiline call arguments can hold
// credentials even when the expression starts with a variable lookup.
fn value_expression(rhs: &str) -> Option<&str> {
    let mut closing = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for (index, ch) in rhs.char_indices() {
        if let Some(delimiter) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == delimiter {
                quote = None;
            }
            continue;
        }
        match ch {
            '\'' | '"' => quote = Some(ch),
            '(' => closing.push(')'),
            '[' => closing.push(']'),
            '{' => closing.push('}'),
            ')' | ']' | '}' => match closing.pop() {
                Some(expected) if expected == ch => (),
                None => return Some(rhs[..index].trim_end()),
                _ => return None,
            },
            ',' | ';' | '\n' | '\r' if closing.is_empty() => {
                return Some(rhs[..index].trim_end());
            }
            _ => (),
        }
    }
    (closing.is_empty() && quote.is_none()).then_some(rhs.trim_end())
}

fn is_credential_match(label: &str, text: &str, matched: regex::Match<'_>) -> bool {
    let prefix = &text[..matched.start()];
    let value = matched.as_str();
    if label == "bearer_token" {
        // "a bearer of responsibility" is prose. An explicit Authorization
        // header still treats even a short token such as "of" as a credential.
        let token = value.split_whitespace().nth(1).unwrap_or("");
        let header = prefix.trim_end().to_ascii_lowercase();
        if header.ends_with("authorization:") {
            return true;
        }
        // Ordinary compound nouns, including prose split across lines, do not
        // introduce the HTTP authentication scheme.
        let previous = header.split_whitespace().next_back().unwrap_or("");
        return !token.eq_ignore_ascii_case("of")
            && !matches!(previous, "standard" | "flag" | "torch" | "pall");
    }
    if label == "password_assign" {
        let (name, rhs) = value.split_once([':', '=']).expect("assignment match");
        let line_prefix = prefix.rsplit('\n').next().unwrap_or("").trim_start();
        if value.contains(':')
            && rhs.starts_with(['\n', '\r'])
            && ["if ", "elif ", "while "]
                .iter()
                .any(|start| line_prefix.starts_with(start))
        {
            return false;
        }
        let rhs = text[matched.start() + name.len() + 1..].trim_start();
        // Natural-language "share a secret: ..." is not a config field.
        if name.trim().eq_ignore_ascii_case("secret") && value.contains(':') {
            let previous = prefix.split_whitespace().next_back().unwrap_or("");
            if matches!(previous.to_ascii_lowercase().as_str(), "a" | "the") {
                return false;
            }
        }
        let reference = VALUE_REFERENCE.is_match(rhs)
            && value_expression(rhs).is_some_and(|expression| {
                if VALUE_LOOKUP.is_match(expression) {
                    return true;
                }
                // A schema type's length is not a password value. Other
                // numeric call arguments/defaults must not bypass the filter.
                let value = if expression.starts_with("db.Column(") {
                    COLUMN_TYPE_LENGTH.replace_all(expression, "db.String()")
                } else {
                    Cow::Borrowed(expression)
                };
                !value.contains(['\'', '"']) && !NUMERIC_LITERAL.is_match(&value)
            });
        return !reference;
    }
    true
}

fn has_credential_match(label: &str, re: &Regex, text: &str) -> bool {
    let mut offset = 0;
    while let Some(matched) = re.find_at(text, offset) {
        if is_credential_match(label, text, matched) {
            return true;
        }
        // A benign match can consume a nested assignment, e.g.
        // "if password:\n password='literal'". Check overlapping matches too.
        offset = matched.start() + text[matched.start()..].chars().next().unwrap().len_utf8();
    }
    false
}

/// Check content for PII/credentials. Returns a `SensitivityResult`.
pub fn check_sensitivity(text: &str) -> SensitivityResult {
    // HIGH tier — any match blocks immediately
    for (p, re) in COMPILED.iter() {
        if p.tier == SensitivityTier::High && has_credential_match(p.label, re, text) {
            return SensitivityResult {
                blocked: true,
                redacted_content: None,
                matched_labels: vec![p.label],
            };
        }
    }

    // MEDIUM tier — redact all matches
    let mut redacted = Cow::Borrowed(text);
    let mut hits: Vec<&'static str> = Vec::new();

    for (p, re) in COMPILED.iter() {
        if p.tier == SensitivityTier::Medium {
            let result = re.replace_all(&redacted, p.replacement);
            if result != redacted {
                hits.push(p.label);
                redacted = Cow::Owned(result.into_owned());
            }
        }
    }

    if !hits.is_empty() {
        return SensitivityResult {
            blocked: false,
            redacted_content: Some(redacted.into_owned()),
            matched_labels: hits,
        };
    }

    SensitivityResult {
        blocked: false,
        redacted_content: None,
        matched_labels: vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_aws_key_blocked() {
        let r = check_sensitivity("my key is AKIAIOSFODNN7EXAMPLE");
        assert!(r.blocked);
        assert_eq!(r.matched_labels, vec!["aws_key"]);
    }

    #[test]
    fn test_private_key_blocked() {
        let r = check_sensitivity("-----BEGIN RSA PRIVATE KEY-----\nMIIE...");
        assert!(r.blocked);
    }

    #[test]
    fn test_password_blocked() {
        let r = check_sensitivity("password=supersecret123");
        assert!(r.blocked);
        assert_eq!(r.matched_labels, vec!["password_assign"]);
    }

    #[test]
    fn prose_and_code_references_are_not_credentials() {
        for text in [
            "I'll share a secret: **dry brining**.",
            "a bearer of certain responsibilities",
            "The regiment's Standard Bearer grants the Unstoppable rule.",
            "STANDARD BEARER\nA Regiment gains a rule.",
            "password = db.Column(db.String(120), nullable=False)",
            "password = data.get('password')",
            "password = os.environ['PASSWORD']",
            "if not username or not password:\n    return error",
            "hashed_password = generate_password_hash(password)",
            "User(username=username, password=hashed_password)",
        ] {
            assert!(!check_sensitivity(text).blocked, "false positive: {text}");
        }
    }

    #[test]
    fn credential_literals_still_block_after_benign_matches() {
        for text in [
            "password=hunter2",
            "password = 'hunter2'",
            "password: hunter2",
            "password:\n  hunter2",
            "db_password='hunter2'",
            "password = str('hunter2')",
            "password = utils.hash('hunter2')",
            "secret=abc123",
            "secret: abc123",
            "passwd=abc123",
            "Authorization: Bearer of",
            "Bearer abc123",
            "password = data.get('password')\npassword = 'hunter2'",
            "a secret: cooking\nsecret: abc123",
            "a bearer of responsibility; Authorization: Bearer abc123",
        ] {
            assert!(check_sensitivity(text).blocked, "missed credential: {text}");
        }
    }

    #[test]
    fn credential_exceptions_do_not_hide_literal_arguments() {
        for text in [
            "if password:\n    password='hunter2'",
            "password = str(123456)",
            "password = data.get('password', 123456)",
            "password = db.Column(db.String(120), default=123456)",
            "password = data.get('password', 'hunter2')",
            "password = data.get('password','hunter2')",
            "password = str( 'hunter2')",
            "password = str(\n    'hunter2'\n)",
            "password = os.environ['PASSWORD'] or 'hunter2'",
        ] {
            assert!(check_sensitivity(text).blocked, "missed credential: {text}");
        }
    }

    #[test]
    fn test_email_redacted() {
        let r = check_sensitivity("contact me at alice@example.com please");
        assert!(!r.blocked);
        assert_eq!(
            r.redacted_content.as_deref(),
            Some("contact me at [email] please")
        );
        assert!(r.matched_labels.contains(&"email"));
    }

    #[test]
    fn test_phone_redacted() {
        let r = check_sensitivity("call 555-867-5309 anytime");
        assert!(!r.blocked);
        assert!(r.redacted_content.as_deref().unwrap().contains("[phone]"));
    }

    #[test]
    fn test_ssn_redacted() {
        let r = check_sensitivity("SSN is 123-45-6789");
        assert!(!r.blocked);
        assert!(r.redacted_content.as_deref().unwrap().contains("[ssn]"));
    }

    #[test]
    fn test_clean_content_passes() {
        let r = check_sensitivity("I prefer Rust over Python for systems programming");
        assert!(!r.blocked);
        assert!(r.redacted_content.is_none());
        assert!(r.matched_labels.is_empty());
    }

    #[test]
    fn test_multiple_medium_redacted() {
        let r = check_sensitivity("email alice@example.com phone 555-123-4567");
        assert!(!r.blocked);
        let redacted = r.redacted_content.unwrap();
        assert!(redacted.contains("[email]"));
        assert!(redacted.contains("[phone]"));
    }
}
