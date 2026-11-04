//! Pure request-validation tests for the REST models: JSON defaults, selector
//! rules, limits and normalization that run before any storage call.

use memoria_api::models::{
    FulltextSearchRequest, PurgeRequest, PurgeSelector, RetrieveRequest, SearchRequest,
    StructuredQueryRequest,
};
use memoria_core::{MemoryType, TrustTier};
use serde_json::json;

fn parse<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> T {
    serde_json::from_value(value).expect("request json parses")
}

fn structured(value: serde_json::Value) -> Result<memoria_service::StructuredQueryOptions, String> {
    parse::<StructuredQueryRequest>(value).structured_options()
}

fn fulltext(value: serde_json::Value) -> Result<memoria_service::FulltextSearchOptions, String> {
    parse::<FulltextSearchRequest>(value).fulltext_options()
}

// ── retrieve / search ────────────────────────────────────────────────────────

#[test]
fn retrieve_and_search_defaults() {
    let r: RetrieveRequest = parse(json!({"query": "q"}));
    assert_eq!(r.top_k, 5);
    assert_eq!(r.explain, "");
    assert!(r.validate().is_ok());
    let s: SearchRequest = parse(json!({"query": "q"}));
    assert_eq!(s.top_k, 10);
    assert_eq!(s.explain, "");
}

#[test]
fn explain_accepts_bool_and_string() {
    for (input, expected) in [
        (json!(true), "basic"),
        (json!(false), "none"),
        (json!("verbose"), "verbose"),
        (json!("analyze"), "analyze"),
    ] {
        let r: RetrieveRequest = parse(json!({"query": "q", "explain": input}));
        assert_eq!(r.explain, expected);
    }
    assert!(
        serde_json::from_value::<RetrieveRequest>(json!({"query": "q", "explain": 1})).is_err()
    );
}

#[test]
fn session_scope_requires_session_id() {
    let r: RetrieveRequest = parse(json!({"query": "q", "session_scope": "session"}));
    assert_eq!(
        r.validate().unwrap_err(),
        "session_id is required when session_scope is set"
    );
    let s: SearchRequest = parse(json!({"query": "q", "session_scope": "session"}));
    assert!(s.validate().is_err());
    let ok: RetrieveRequest =
        parse(json!({"query": "q", "session_scope": "session", "session_id": "s1"}));
    assert!(ok.validate().is_ok());
}

#[test]
fn retrieve_rejects_unknown_memory_type() {
    let r: RetrieveRequest = parse(json!({"query": "q", "memory_types": ["semantic", "bogus"]}));
    assert!(r.retrieve_options().is_err());
    let ok: RetrieveRequest =
        parse(json!({"query": "q", "memory_types": ["semantic", "semantic", "profile"]}));
    assert!(ok.retrieve_options().is_ok());
}

#[test]
fn query_field_is_required() {
    assert!(serde_json::from_value::<RetrieveRequest>(json!({"top_k": 3})).is_err());
    assert!(serde_json::from_value::<SearchRequest>(json!({})).is_err());
}

// ── structured query ─────────────────────────────────────────────────────────

#[test]
fn structured_query_needs_a_selector() {
    assert_eq!(
        structured(json!({})).unwrap_err(),
        "structured query requires at least one filter selector"
    );
    assert!(structured(json!({"limit": 10})).is_err());
    let opts = structured(json!({"session_id": "s1"})).expect("valid");
    assert_eq!(opts.limit, 100);
    assert_eq!(opts.session_id.as_deref(), Some("s1"));
}

#[test]
fn structured_query_limit_bounds() {
    for limit in [0, -1, 501] {
        assert_eq!(
            structured(json!({"session_id": "s", "limit": limit})).unwrap_err(),
            "limit must be between 1 and 500"
        );
    }
    for limit in [1, 500] {
        assert_eq!(
            structured(json!({"session_id": "s", "limit": limit}))
                .unwrap()
                .limit,
            limit
        );
    }
}

#[test]
fn structured_query_trims_and_rejects_blank_filters() {
    let opts = structured(json!({"subject_id": "  u1  ", "session_id": " s1 "})).unwrap();
    assert_eq!(opts.subject_id.as_deref(), Some("u1"));
    assert_eq!(opts.session_id.as_deref(), Some("s1"));
    for field in ["subject_id", "session_id", "trust_tier", "branch"] {
        let mut body = json!({});
        body[field] = json!("   ");
        if field != "subject_id" {
            body["subject_id"] = json!("u1");
        } else {
            body["session_id"] = json!("s1");
        }
        let err = structured(body).unwrap_err();
        assert!(err.contains("must not be empty"), "{field}: {err}");
    }
}

#[test]
fn structured_query_trust_tier_and_memory_types() {
    let opts =
        structured(json!({"trust_tier": "T2", "memory_types": ["profile", "profile"]})).unwrap();
    assert_eq!(opts.trust_tier, Some(TrustTier::T2Curated));
    assert_eq!(opts.memory_types, Some(vec![MemoryType::Profile]));
    assert!(structured(json!({"trust_tier": "t2"})).is_err());
    assert!(structured(json!({"trust_tier": "T5"})).is_err());
    assert!(structured(json!({"memory_types": []})).is_err());
    assert!(structured(json!({"memory_types": [" "]})).is_err());
    assert!(structured(json!({"memory_types": ["Semantic"]})).is_err());
}

#[test]
fn structured_query_cursor_shape() {
    let good = "0123456789abcdefABCDEF0123456789";
    assert_eq!(
        structured(json!({"session_id": "s", "cursor": good}))
            .unwrap()
            .cursor
            .as_deref(),
        Some(good)
    );
    assert_eq!(
        structured(json!({"session_id": "s", "cursor": "  "}))
            .unwrap()
            .cursor,
        None
    );
    for bad in [
        "abc",
        "0123456789abcdef0123456789abcdeg",
        "0123456789abcdef0123456789abcdef0",
    ] {
        assert_eq!(
            structured(json!({"session_id": "s", "cursor": bad})).unwrap_err(),
            "cursor must be a 32-character hexadecimal memory_id"
        );
    }
}

#[test]
fn structured_query_metadata_filter_rules() {
    assert!(
        structured(json!({"extra_metadata_filter": {"source": "chat", "n": 2, "ok": true}}))
            .is_ok()
    );
    for bad in [
        json!({"1abc": "x"}),
        json!({"a-b": "x"}),
        json!({"": "x"}),
        json!({"k": null}),
        json!({"k": [1]}),
        json!({"k": {"x": 1}}),
        json!({"k": "x".repeat(2000)}),
        json!({ memoria_core::STORE_REQUEST_HASH_FIELD: "x" }),
    ] {
        assert!(
            structured(json!({"extra_metadata_filter": bad.clone()})).is_err(),
            "{bad}"
        );
    }
    let too_many: serde_json::Map<String, serde_json::Value> =
        (0..17).map(|i| (format!("k{i}"), json!(i))).collect();
    assert!(structured(json!({"extra_metadata_filter": too_many})).is_err());
    let max: serde_json::Map<String, serde_json::Value> =
        (0..16).map(|i| (format!("k{i}"), json!(i))).collect();
    assert!(structured(json!({"extra_metadata_filter": max})).is_ok());
}

#[test]
fn structured_query_rejects_unknown_fields() {
    assert!(serde_json::from_value::<StructuredQueryRequest>(
        json!({"session_id": "s", "query": "x"})
    )
    .is_err());
}

// ── full-text search ─────────────────────────────────────────────────────────

#[test]
fn fulltext_defaults_and_limit_bounds() {
    assert_eq!(fulltext(json!({"query": "rust"})).unwrap().limit, 20);
    for limit in [0, 101] {
        assert_eq!(
            fulltext(json!({"query": "rust", "limit": limit})).unwrap_err(),
            "limit must be between 1 and 100"
        );
    }
    assert_eq!(
        fulltext(json!({"query": "rust", "limit": 100}))
            .unwrap()
            .limit,
        100
    );
}

#[test]
fn fulltext_query_content_rules() {
    assert!(fulltext(json!({"query": "数据库"})).is_ok());
    assert!(fulltext(json!({"query": "snake_case"})).is_ok());
    // Underscore-only terms are dropped (MatrixOne rejects terms that tokenize to nothing).
    for bad in ["", "   ", "+-*()\"<>~", "____"] {
        assert!(fulltext(json!({"query": bad})).is_err(), "{bad:?}");
    }
    assert!(fulltext(json!({"query": "a".repeat(4096)})).is_ok());
    assert!(fulltext(json!({"query": "a".repeat(4097)})).is_err());
}

#[test]
fn fulltext_memory_types_blank_entries_vs_empty_list() {
    assert_eq!(
        fulltext(json!({"query": "q", "memory_types": []}))
            .unwrap()
            .memory_types,
        None
    );
    assert!(fulltext(json!({"query": "q", "memory_types": [""]})).is_err());
    assert_eq!(
        fulltext(json!({"query": "q", "memory_types": ["episodic", "episodic"]}))
            .unwrap()
            .memory_types,
        Some(vec![MemoryType::Episodic])
    );
}

#[test]
fn fulltext_branch_is_trimmed_and_blank_is_rejected() {
    let r: FulltextSearchRequest = parse(json!({"query": "q", "branch": " dev "}));
    assert_eq!(r.fulltext_branch().unwrap().as_deref(), Some("dev"));
    let r: FulltextSearchRequest = parse(json!({"query": "q", "branch": " "}));
    assert!(r.fulltext_branch().is_err());
    let r: FulltextSearchRequest = parse(json!({"query": "q"}));
    assert_eq!(r.fulltext_branch().unwrap(), None);
}

// ── purge ────────────────────────────────────────────────────────────────────

#[test]
fn purge_selector_exclusivity() {
    let p: PurgeRequest = parse(json!({"memory_ids": ["a"], "topic": "t"}));
    assert_eq!(
        p.selector().err().unwrap(),
        "provide only one of memory_ids, topic, or session_id"
    );
    let p: PurgeRequest = parse(json!({"topic": "t", "session_id": "s"}));
    assert!(p.selector().is_err());
    let p: PurgeRequest = parse(json!({"memory_types": ["semantic"]}));
    assert_eq!(
        p.selector().err().unwrap(),
        "memory_types requires session_id"
    );
}

#[test]
fn purge_selector_trims_and_parses() {
    let p: PurgeRequest = parse(json!({"topic": "  rust  "}));
    assert!(matches!(p.selector(), Ok(PurgeSelector::Topic(t)) if t == "rust"));
    let p: PurgeRequest = parse(json!({"topic": "   "}));
    assert!(matches!(p.selector(), Ok(PurgeSelector::None)));
    let p: PurgeRequest = parse(json!({"session_id": " s1 ", "memory_types": ["working"]}));
    match p.selector() {
        Ok(PurgeSelector::Session {
            session_id,
            memory_types,
        }) => {
            assert_eq!(session_id, "s1");
            assert_eq!(memory_types, Some(vec![MemoryType::Working]));
        }
        _ => panic!("expected session selector"),
    }
    let p: PurgeRequest = parse(json!({"session_id": "s1", "memory_types": ["nope"]}));
    assert!(p.selector().is_err());
    let p: PurgeRequest = parse(json!({"memory_ids": ["m1", "m2"]}));
    assert!(
        matches!(p.selector(), Ok(PurgeSelector::MemoryIds(ids)) if ids == vec!["m1".to_string(), "m2".to_string()])
    );
}
