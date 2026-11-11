//! Contract tests for the sensitivity filter: positives that must stay blocked
//! or redacted, tier precedence, and benign text that must pass unchanged.
//! Known gaps are tracked in #373-#377 and #384 and are not asserted here.

use memoria_core::check_sensitivity;

fn blocked_label(text: &str) -> Option<&'static str> {
    let r = check_sensitivity(text);
    if r.blocked {
        assert!(
            r.redacted_content.is_none(),
            "blocked result must not carry content: {text:?}"
        );
        assert_eq!(
            r.matched_labels.len(),
            1,
            "blocked result reports one label: {text:?}"
        );
        Some(r.matched_labels[0])
    } else {
        None
    }
}

fn redacted(text: &str) -> Option<String> {
    let r = check_sensitivity(text);
    assert!(
        !r.blocked,
        "unexpected block for {text:?}: {:?}",
        r.matched_labels
    );
    r.redacted_content
}

#[test]
fn aws_access_key_prefixes_are_blocked() {
    for key in [
        "AKIAIOSFODNN7EXAMPLE",
        "ASIAIOSFODNN7EXAMPLE",
        "ABIAIOSFODNN7EXAMPLE",
        "ACCAIOSFODNN7EXAMPLE",
    ] {
        assert_eq!(
            blocked_label(&format!("key {key} here")),
            Some("aws_key"),
            "{key}"
        );
        assert_eq!(blocked_label(key), Some("aws_key"), "{key} alone");
    }
}

#[test]
fn aws_like_strings_with_wrong_shape_are_not_blocked() {
    for text in ["AKIA1234", "akiaiosfodnn7example", "AKIAiosfodnn7example"] {
        assert_eq!(blocked_label(text), None, "{text}");
    }
}

#[test]
fn rsa_ec_dsa_and_pkcs8_pem_headers_are_blocked() {
    for kind in ["RSA ", "EC ", "DSA ", ""] {
        let pem = format!("-----BEGIN {kind}PRIVATE KEY-----\nMIIBOgIBAAJBAKj34GkxFhD90vcNLYLInFEX\n-----END {kind}PRIVATE KEY-----");
        assert_eq!(blocked_label(&pem), Some("private_key"), "{kind:?}");
        assert_eq!(
            blocked_label(&format!("config:\n  key: |\n    {pem}")),
            Some("private_key"),
            "{kind:?} embedded"
        );
    }
}

#[test]
fn public_keys_and_certificates_are_not_blocked() {
    for text in [
        "-----BEGIN PUBLIC KEY-----\nMFwwDQYJKoZIhvcNAQEBBQADSwAwSAJBAKj34GkxFhD90vcN\n-----END PUBLIC KEY-----",
        "-----BEGIN CERTIFICATE-----\nMIIBszCCAV2gAwIBAgIJAOgN\n-----END CERTIFICATE-----",
    ] {
        assert_eq!(blocked_label(text), None, "{text}");
    }
}

#[test]
fn bearer_tokens_are_blocked_in_any_case() {
    for text in [
        "Authorization: Bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl",
        "authorization: bearer abc.def-ghi_jkl~mno+pqr/stu==",
        "curl -H 'Authorization: BEARER token123'",
    ] {
        assert_eq!(blocked_label(text), Some("bearer_token"), "{text}");
    }
}

#[test]
fn password_and_secret_assignments_are_blocked_across_formats() {
    for text in [
        "password=hunter2",
        "PASSWORD = hunter2",
        "passwd: hunter2",
        "db_password: \"hunter2\"",
        "{\"password\": \"hunter2\"}",
        "export MYSQL_PASSWORD=hunter2",
        "client_secret=abc123",
        "api-secret: abc123",
        "secret_key=abc123",
        "aws_secret_access_key = wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY",
        "secret = abc123",
        "- secret: abc123",
        "{\"secret\": \"abc123\"}",
    ] {
        assert_eq!(blocked_label(text), Some("password_assign"), "{text}");
    }
}

#[test]
fn high_tier_wins_over_medium_tier() {
    let r = check_sensitivity("mail alice@example.com, password=hunter2, call 555-123-4567");
    assert!(r.blocked);
    assert_eq!(r.matched_labels, vec!["password_assign"]);
    assert!(r.redacted_content.is_none());
}

#[test]
fn email_phone_ssn_and_card_are_redacted_in_place() {
    assert_eq!(
        redacted("write to alice.smith+tag@example.org today").as_deref(),
        Some("write to [email] today")
    );
    assert_eq!(
        redacted("call 555.867.5309 now").as_deref(),
        Some("call [phone] now")
    );
    assert_eq!(
        redacted("call 555-8675-3090 now").as_deref(),
        Some("call [phone] now")
    );
    assert_eq!(
        redacted("ssn 078-05-1120 on file").as_deref(),
        Some("ssn [ssn] on file")
    );
    assert_eq!(
        redacted("card 4111111111111111 ok").as_deref(),
        Some("card [card] ok")
    );
    assert_eq!(
        redacted("card 5500-0000-0000-0004 ok").as_deref(),
        Some("card [card] ok")
    );
}

#[test]
fn every_occurrence_is_redacted_and_labels_follow_pattern_order() {
    let r = check_sensitivity("a@example.com, b@example.net; ssn 123-45-6789; phone 555-123-4567");
    assert!(!r.blocked);
    assert_eq!(
        r.redacted_content.as_deref(),
        Some("[email], [email]; ssn [ssn]; phone [phone]")
    );
    assert_eq!(r.matched_labels, vec!["email", "phone", "ssn"]);
}

#[test]
fn redaction_is_idempotent() {
    let first = redacted("contact bob@example.com or 555-123-4567").expect("redacted");
    assert_eq!(
        redacted(&first),
        None,
        "re-checking redacted text must be a no-op: {first}"
    );
}

#[test]
fn redaction_keeps_surrounding_unicode_intact() {
    assert_eq!(
        redacted("请联系 alice@example.com 谢谢 🙏").as_deref(),
        Some("请联系 [email] 谢谢 🙏")
    );
}

#[test]
fn benign_text_passes_unchanged() {
    for text in [
        "",
        "   ",
        "I prefer tabs over spaces",
        "the meeting is at 10:30 on 2026-10-30",
        "version 1.2.3 released",
        "order #12345 shipped",
        "use the @mention syntax in docs",
        "user@localhost is not an email with a tld",
        "the password policy requires 12 characters",
        "我喜欢用 Rust 写系统程序",
    ] {
        let r = check_sensitivity(text);
        assert!(!r.blocked, "{text:?} blocked by {:?}", r.matched_labels);
        assert_eq!(r.redacted_content, None, "{text:?}");
        assert!(r.matched_labels.is_empty(), "{text:?}");
    }
}
