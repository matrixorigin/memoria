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
        regex: r"(?i)\bBearer\s+[A-Za-z0-9\-._~+/]{20,}=*",
        replacement: "",
    },
    Pattern {
        label: "password_assign",
        tier: SensitivityTier::High,
        // Keep legacy password/passwd detection and secret suffixes in arbitrary
        // identifiers (env vars, camelCase, snake_case and hyphenated names).
        // Bare `secret = value` is an assignment, but `share a secret: ...`
        // is ordinary prose. For bare `secret: value`, require a field boundary
        // (line start or JSON/object separator), including YAML list markers
        // and quoted field names.
        regex: concat!(
            r#"(?im)(?:(?:password|passwd|[\w-]secret|(?:api|client|auth)[ _-]secret|secret[_-]key|aws_secret_access_key)["']?\s*[:=]\s*\S+"#,
            r#"|\bsecret\b["']?\s*=\s*\S+"#,
            r#"|(?:^|[{\[,])[\t ]*(?:-[\t ]+)?["']?secret["']?[\t ]*:[\t ]*\S+)"#,
        ),
        replacement: "",
    },
    // MEDIUM — redact
    Pattern {
        label: "email",
        tier: SensitivityTier::Medium,
        regex: r"[a-zA-Z0-9_.+-]+@(?:[a-zA-Z0-9-]+\.)+[a-zA-Z]{2,}",
        replacement: "[email]",
    },
    Pattern {
        label: "phone",
        tier: SensitivityTier::Medium,
        // Require at least one separator so opaque 10-11 digit identifiers
        // (for example GitHub comment IDs) are not mistaken for phone numbers.
        regex: r"\b(?:\d{3}[-.]\d{3,4}[-.]?\d{4}|\d{3}[-.]?\d{3,4}[-.]\d{4})\b",
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

// Use the same Unicode-aware word-boundary semantics as the original card
// regex, while scanning starts independently so a failed candidate cannot
// consume a later valid one.
static WORD_BOUNDARY: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"\b").expect("valid word-boundary regex"));

static COMPILED: Lazy<Vec<(&'static Pattern, Regex)>> = Lazy::new(|| {
    PATTERNS
        .iter()
        .map(|p| (p, Regex::new(p.regex).expect("valid regex")))
        .collect()
});

/// Zero-digit code points for each decimal-digit block matched by the regex
/// crate's `\d` class. Each set contains ten consecutive code points starting
/// here. `decimal_digit_table_matches_regex_digit_class` checks this table
/// against `\d` for every code point, so a regex Unicode upgrade or a table
/// edit that disagrees fails the test.
const DECIMAL_DIGIT_ZEROES: [u32; 76] = [
    0x0030, 0x0660, 0x06F0, 0x07C0, 0x0966, 0x09E6, 0x0A66, 0x0AE6, 0x0B66, 0x0BE6, 0x0C66, 0x0CE6,
    0x0D66, 0x0DE6, 0x0E50, 0x0ED0, 0x0F20, 0x1040, 0x1090, 0x17E0, 0x1810, 0x1946, 0x19D0, 0x1A80,
    0x1A90, 0x1B50, 0x1BB0, 0x1C40, 0x1C50, 0xA620, 0xA8D0, 0xA900, 0xA9D0, 0xA9F0, 0xAA50, 0xABF0,
    0xFF10, 0x104A0, 0x10D30, 0x10D40, 0x11066, 0x110F0, 0x11136, 0x111D0, 0x112F0, 0x11450,
    0x114D0, 0x11650, 0x116C0, 0x116D0, 0x116DA, 0x11730, 0x118E0, 0x11950, 0x11BF0, 0x11C50,
    0x11D50, 0x11DA0, 0x11F50, 0x16130, 0x16A60, 0x16AC0, 0x16B50, 0x16D70, 0x1CCF0, 0x1D7CE,
    0x1D7D8, 0x1D7E2, 0x1D7EC, 0x1D7F6, 0x1E140, 0x1E2F0, 0x1E4F0, 0x1E5F1, 0x1E950, 0x1FBF0,
];

/// Decimal value for Unicode decimal digits accepted in card numbers.
fn decimal_digit(c: char) -> Option<u32> {
    let codepoint = c as u32;
    DECIMAL_DIGIT_ZEROES
        .iter()
        .find_map(|&zero| codepoint.checked_sub(zero).filter(|&value| value < 10))
}

/// Luhn checksum over decimal digits.
fn luhn_valid_digits(digits: &[u32]) -> bool {
    let sum: u32 = digits
        .iter()
        .rev()
        .enumerate()
        .map(|(i, &d)| {
            if i % 2 == 1 {
                let doubled = d * 2;
                if doubled > 9 {
                    doubled - 9
                } else {
                    doubled
                }
            } else {
                d
            }
        })
        .sum();
    digits.len() >= 13 && sum % 10 == 0
}

/// Find Luhn-valid card candidates, including candidates that overlap an
/// earlier invalid numeric match. The regex `replace_all` API consumes a
/// failed match, which can hide a valid card beginning later in that match.
fn luhn_valid_card_spans(text: &str) -> Vec<(usize, usize, usize)> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let word_boundaries: std::collections::HashSet<usize> =
        WORD_BOUNDARY.find_iter(text).map(|m| m.start()).collect();
    let mut candidates = Vec::new();

    for (start_index, &(start, first)) in chars.iter().enumerate() {
        if decimal_digit(first).is_none() || !word_boundaries.contains(&start) {
            continue;
        }

        let mut digits = Vec::new();
        for &(byte, c) in chars.iter().skip(start_index) {
            if let Some(digit) = decimal_digit(c) {
                digits.push(digit);
                let end = byte + c.len_utf8();
                if digits.len() > 19 {
                    break;
                }

                if (13..=19).contains(&digits.len())
                    && word_boundaries.contains(&(end))
                    && luhn_valid_digits(&digits)
                {
                    candidates.push((start, end, digits.len()));
                }
            } else if !matches!(c, ' ' | '-') {
                break;
            }
        }
    }

    // Prefer the longest valid candidate at a shared start. Then choose
    // non-overlapping spans; later starts remain available when an earlier
    // prefix fails Luhn and therefore produced no candidate.
    candidates.sort_by_key(|(start, _, digit_count)| (*start, std::cmp::Reverse(*digit_count)));
    let mut selected = Vec::new();
    let mut last_end = 0;
    for (start, end, digit_count) in candidates {
        if start < last_end {
            continue;
        }
        selected.push((start, end, digit_count));
        last_end = end;
    }
    selected
}

fn redact_luhn_valid_cards(text: &str, replacement: &str) -> String {
    let spans = luhn_valid_card_spans(text);
    if spans.is_empty() {
        return text.to_owned();
    }

    let mut redacted = String::with_capacity(text.len());
    let mut last_end = 0;
    for (start, end, _) in spans {
        redacted.push_str(&text[last_end..start]);
        redacted.push_str(replacement);
        last_end = end;
    }
    redacted.push_str(&text[last_end..]);
    redacted
}

/// Check content for PII/credentials. Returns a `SensitivityResult`.
pub fn check_sensitivity(text: &str) -> SensitivityResult {
    // HIGH tier — any match blocks immediately
    for (p, re) in COMPILED.iter() {
        if p.tier == SensitivityTier::High && re.is_match(text) {
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
            let result = if p.label == "credit_card" {
                // Only redact digit runs that pass Luhn; keep timestamps and IDs.
                Cow::Owned(redact_luhn_valid_cards(&redacted, p.replacement))
            } else {
                re.replace_all(&redacted, p.replacement)
            };
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

    fn luhn_card(length: usize) -> String {
        let mut digits = vec![0; length];
        digits[0] = 4;
        for check_digit in 0..=9 {
            digits[length - 1] = check_digit;
            if luhn_valid_digits(&digits) {
                return digits
                    .iter()
                    .map(|digit| char::from_digit(*digit, 10).unwrap())
                    .collect();
            }
        }
        unreachable!("a Luhn check digit exists for every card length")
    }

    #[test]
    fn ordinary_bearer_prose_is_not_an_access_token() {
        for text in [
            "the bearer of bad news arrived",
            "Pay to the bearer on demand",
            "She was a pallbearer at the funeral",
            "ring bearer for the wedding",
        ] {
            let result = check_sensitivity(text);
            assert!(!result.blocked, "ordinary bearer prose rejected: {text}");
        }
    }

    #[test]
    fn bearer_credentials_remain_blocked() {
        for text in [
            "Authorization: Bearer abcdefghijklmnopqrstuvwxyz123456",
            "Bearer synthetic-test-token",
        ] {
            let result = check_sensitivity(text);
            assert!(result.blocked, "bearer credential allowed: {text}");
            assert_eq!(result.matched_labels, vec!["bearer_token"]);
        }
    }

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
    fn ordinary_secret_prose_is_not_a_credential_assignment() {
        for text in [
            "The secret: practice every day.",
            "Since you mentioned the ribs, I'll share a secret: **dry brining**.",
            "She shared her SECRET: patience and kindness.",
            "A business secret: listen to your customers.",
        ] {
            let result = check_sensitivity(text);
            assert!(!result.blocked, "ordinary prose rejected: {text}");
            assert!(result.redacted_content.is_none());
        }
    }

    #[test]
    fn explicit_credential_assignments_remain_blocked() {
        for text in [
            "password=synthetic-test-value",
            "My PASSWORD : synthetic-test-value",
            "passwd = synthetic-test-value",
            "db_password: synthetic-test-value",
            "export SECRET=synthetic-test-value",
            "client_secret: synthetic-test-value",
            "api-secret = synthetic-test-value",
            "API secret: synthetic-test-value",
            "secret_key: synthetic-test-value",
            "aws_secret_access_key=synthetic-test-value",
            r#"{"password": "synthetic-test-value"}"#,
            r#"{"client_secret": "synthetic-test-value"}"#,
        ] {
            let result = check_sensitivity(text);
            assert!(result.blocked, "credential assignment allowed: {text}");
            assert_eq!(result.matched_labels, vec!["password_assign"]);
        }
    }

    #[test]
    fn bare_secret_fields_remain_blocked_at_structured_boundaries() {
        for text in [
            "secret: synthetic-test-value",
            "config:\n  SECRET : synthetic-test-value",
            r#"{"secret": "synthetic-test-value"}"#,
            r#"{"name": "test", "secret": "synthetic-test-value"}"#,
            "{'secret': 'synthetic-test-value'}",
            "const config = {secret: 'synthetic-test-value'};",
            "- secret: synthetic-test-value",
            "config:\n  - secret: synthetic-test-value",
            "- 'SECRET': synthetic-test-value",
        ] {
            let result = check_sensitivity(text);
            assert!(result.blocked, "secret field allowed: {text}");
            assert_eq!(result.matched_labels, vec!["password_assign"]);
        }
    }

    #[test]
    fn credential_identifiers_keep_legacy_protection() {
        for text in [
            "GITHUB_CLIENT_SECRET=ghs_synthetic_test_value",
            "JWT_SECRET=synthetic-test-value",
            "app_secret: synthetic-test-value",
            "webhook_secret = synthetic-test-value",
            "clientSecret: synthetic-test-value",
            "userPassword=synthetic-test-value",
            "dbPassword: synthetic-test-value",
            "rootpassword=synthetic-test-value",
            "dbpasswd=synthetic-test-value",
            "APP-SECRET: synthetic-test-value",
            "密钥secret: synthetic-test-value",
            r#"{"clientSecret": "synthetic-test-value"}"#,
        ] {
            let result = check_sensitivity(text);
            assert!(result.blocked, "credential identifier allowed: {text}");
            assert_eq!(result.matched_labels, vec!["password_assign"]);
        }
    }

    #[test]
    fn benign_prose_does_not_bypass_other_credentials_or_redaction() {
        for credential in [
            "password=synthetic-test-value",
            "Bearer synthetic-test-token",
            "AKIAIOSFODNN7EXAMPLE",
            "-----BEGIN RSA PRIVATE KEY-----",
        ] {
            let text = format!("The secret: practice every day.\n{credential}");
            assert!(check_sensitivity(&text).blocked);
        }
        let result = check_sensitivity("The secret: ask alice@example.com for advice.");
        assert!(!result.blocked);
        assert_eq!(result.matched_labels, vec!["email"]);
        assert_eq!(
            result.redacted_content.as_deref(),
            Some("The secret: ask [email] for advice.")
        );
    }

    #[test]
    fn current_policy_blocks_password_literal_expressions() {
        // The current HIGH-tier rule matches the assignment prefix without
        // parsing the RHS. Pin its blocking behavior for these literal forms.
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
            let result = check_sensitivity(text);
            assert!(result.blocked, "credential assignment allowed: {text}");
            assert_eq!(result.matched_labels, vec!["password_assign"]);
            assert!(result.redacted_content.is_none());
        }
    }

    #[test]
    fn current_policy_documents_password_reference_false_positives() {
        // Known false positives under the broad assignment rule. A future
        // policy/detector change should revise these expectations explicitly.
        for text in [
            "password = None",
            "password: str",
            "def login(username, password=None):",
            "password = self.password",
            "password = getpass.getpass(\"Enter password: \")",
            "password = data.get('password')",
            "password = os.environ['PASSWORD']  # default was hunter2",
            "password = db.Column(db.String(120))",
            "password = models.CharField(max_length=128)",
            "password = mapped_column(String(255))",
            "password = sa.Column(String(120))",
        ] {
            let result = check_sensitivity(text);
            assert!(result.blocked, "false-positive baseline changed: {text}");
            assert_eq!(result.matched_labels, vec!["password_assign"]);
            assert!(result.redacted_content.is_none());
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
    fn email_redaction_covers_entire_multilabel_domain() {
        for (address, expected) in [
            ("bob@mail.example.com", "mail [email] now"),
            ("bob@example.co.uk", "mail [email] now"),
            ("alice@example.com", "mail [email] now"),
        ] {
            let input = format!("mail {address} now");
            let result = check_sensitivity(&input);
            assert!(!result.blocked, "email unexpectedly blocked: {address}");
            assert_eq!(
                result.redacted_content.as_deref(),
                Some(expected),
                "email domain tail survived redaction: {address}"
            );
            assert_eq!(result.matched_labels, vec!["email"]);
        }
    }

    #[test]
    fn test_phone_redacted() {
        let r = check_sensitivity("call 555-867-5309 anytime");
        assert!(!r.blocked);
        assert!(r.redacted_content.as_deref().unwrap().contains("[phone]"));
    }

    #[test]
    fn unformatted_numeric_identifiers_are_not_redacted_as_phone_numbers() {
        for text in [
            "see https://github.com/org/repo/issues/1#issuecomment-4345955198",
            "order 4345955198 is ready",
            "transaction 12345678901 completed",
        ] {
            let result = check_sensitivity(text);
            assert!(!result.blocked, "numeric identifier was blocked: {text}");
            assert!(
                result.redacted_content.is_none(),
                "numeric identifier was redacted: {text}"
            );
            assert!(result.matched_labels.is_empty());
        }
    }

    #[test]
    fn phone_redaction_keeps_supported_separator_forms() {
        for phone in [
            "555-867-5309",
            "555.867.5309",
            "555-8675-5309",
            "555867-5309",
            "555-8675309",
        ] {
            let result = check_sensitivity(&format!("call {phone} anytime"));
            assert!(!result.blocked);
            assert_eq!(
                result.redacted_content.as_deref(),
                Some("call [phone] anytime")
            );
            assert_eq!(result.matched_labels, vec!["phone"]);
        }
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

    #[test]
    fn credit_card_redaction_requires_luhn_valid_number() {
        // Epoch-millisecond timestamps and 64-bit snowflake IDs are 13-19 digits
        // but fail the Luhn check, so they must not be redacted as cards.
        for text in [
            "event at 1730284800123 ms",
            "snowflake id 1849203948572039485",
        ] {
            let r = check_sensitivity(text);
            assert_eq!(r.redacted_content, None, "non-card digits redacted: {text}");
        }
        // Luhn-invalid numbers stay unredacted in ASCII, fullwidth and
        // non-fullwidth Unicode decimal digits.
        for text in [
            "card 4111 1111 1111 1112 on file",
            "card ４１１１ １１１１ １１１１ １１１２ on file",
            "card ٤١١١ ١١١١ ١١١١ ١١١٢ on file",
        ] {
            let r = check_sensitivity(text);
            assert_eq!(
                r.redacted_content, None,
                "Luhn-invalid card redacted: {text}"
            );
        }
        let r = check_sensitivity("card 4111 1111 1111 1111 on file");
        assert_eq!(r.redacted_content.as_deref(), Some("card [card] on file"));
    }

    #[test]
    fn credit_card_redaction_checks_overlapping_candidates() {
        let result = check_sensitivity("payment 1 4111 1111 1111 1111 on file");
        assert_eq!(
            result.redacted_content.as_deref(),
            Some("payment 1 [card] on file")
        );
    }

    #[test]
    fn credit_card_redaction_supports_valid_lengths_and_separators() {
        for length in 13..=19 {
            let number = luhn_card(length);
            let result = check_sensitivity(&format!("card {number} on file"));
            assert_eq!(
                result.redacted_content.as_deref(),
                Some("card [card] on file"),
                "valid {length}-digit card was not redacted"
            );
        }

        for number in ["4111 1111 1111 1111", "4111-1111-1111-1111"] {
            let result = check_sensitivity(&format!("card {number} on file"));
            assert_eq!(
                result.redacted_content.as_deref(),
                Some("card [card] on file"),
                "valid separated card was not redacted: {number}"
            );
        }
    }

    #[test]
    fn credit_card_redaction_supports_fullwidth_digits() {
        let result = check_sensitivity("card ４１１１ １１１１ １１１１ １１１１ on file");
        assert_eq!(
            result.redacted_content.as_deref(),
            Some("card [card] on file")
        );
    }

    #[test]
    fn credit_card_redaction_supports_unicode_decimal_digits() {
        for number in [
            "٤١١١ ١١١١ ١١١١ ١١١١", // Arabic-Indic
            "४१११ ११११ ११११ ११११", // Devanagari
            "4111 ١١١١ 1111 1111", // Mixed ASCII and Arabic-Indic (review reproduction)
            "٤111 1١11 ١1١1 111١", // Mixed ASCII and Arabic-Indic
        ] {
            let result = check_sensitivity(&format!("card {number} on file"));
            assert_eq!(
                result.redacted_content.as_deref(),
                Some("card [card] on file"),
                "valid Unicode decimal card was not redacted: {number}"
            );
        }
    }

    #[test]
    fn credit_card_redaction_preserves_adjacent_numeric_fields() {
        let result = check_sensitivity(
            "event 1730284800123; card 4111 1111 1111 1111; reference 1849203948572039485",
        );
        assert_eq!(
            result.redacted_content.as_deref(),
            Some("event 1730284800123; card [card]; reference 1849203948572039485")
        );
    }

    #[test]
    fn credit_card_redaction_preserves_original_word_boundaries() {
        for text in [
            "x4111111111111111 on file",
            "_4111111111111111 on file",
            "4111111111111111x on file",
            "4111111111111111_ on file",
        ] {
            let result = check_sensitivity(text);
            assert_eq!(
                result.redacted_content, None,
                "unexpected redaction: {text}"
            );
        }
    }

    #[test]
    fn credit_card_redaction_bounds_candidates_at_separators() {
        let cases = [
            (
                "cards 4111-1111-1111-1111 4111 1111 1111 1111 end",
                "cards [card] [card] end",
            ),
            (
                "ref 1730284800123 card 4111-1111-1111-1111 end",
                "ref 1730284800123 card [card] end",
            ),
        ];
        for (input, expected) in cases {
            let result = check_sensitivity(input);
            assert_eq!(
                result.redacted_content.as_deref().unwrap_or(input),
                expected,
                "unexpected redaction for: {input}"
            );
        }
    }

    #[test]
    fn credit_card_redaction_preserves_grouped_19_digit_cards() {
        let result = check_sensitivity("card 4000 0000 0000 0000 006 on file");
        assert_eq!(
            result.redacted_content.as_deref(),
            Some("card [card] on file")
        );
    }

    #[test]
    fn decimal_digit_table_matches_regex_digit_class() {
        // The card pattern uses `\d`, so every code point it accepts must be
        // scanned as a digit, and no other code point may be.
        let digit_class = Regex::new(r"\A\d\z").expect("valid digit regex");
        let mut buf = [0u8; 4];
        let mut mismatches = Vec::new();
        for codepoint in 0..=char::MAX as u32 {
            let Some(c) = char::from_u32(codepoint) else {
                continue;
            };
            let expected = digit_class.is_match(c.encode_utf8(&mut buf));
            if decimal_digit(c).is_some() != expected {
                mismatches.push(format!("U+{codepoint:04X} regex_digit={expected}"));
            }
        }
        assert!(
            mismatches.is_empty(),
            "decimal_digit disagrees with \\d: {mismatches:?}"
        );
    }
}
