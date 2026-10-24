/// Core tools E2E tests against real DB.
/// Covers: memory_store, memory_retrieve, memory_search, memory_correct (id + query),
///         memory_purge (single, batch, topic), memory_profile, memory_list,
///         memory_capabilities — all fields verified.
///
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria \
///      cargo test -p memoria-mcp --test core_tools_e2e -- --nocapture
mod support;

use memoria_service::MemoryService;
use serde_json::{json, Value};
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}
fn uid() -> String {
    format!("ct_{}", &Uuid::new_v4().simple().to_string()[..8])
}

async fn spawn_fake_llm() -> (
    Arc<memoria_embedding::LlmClient>,
    tokio::sync::oneshot::Sender<()>,
) {
    memoria_test_utils::spawn_fake_llm(vec![]).await
}

async fn setup() -> (
    Arc<MemoryService>,
    String,
    support::multi_db::McpTestContext,
) {
    let ctx = support::multi_db::setup_mcp_context("core_tools_e2e", test_dim(), None, None).await;
    (ctx.service(), uid(), ctx)
}

async fn call(name: &str, args: Value, svc: &Arc<MemoryService>, uid: &str) -> Value {
    memoria_mcp::tools::call(name, args, svc, uid)
        .await
        .expect(name)
}
fn text(v: &Value) -> &str {
    v["content"][0]["text"].as_str().unwrap_or("")
}

// ── 1. memory_store: all memory_type variants ─────────────────────────────────

#[tokio::test]
async fn test_store_all_memory_types() {
    let (svc, uid, _ctx) = setup().await;
    for mt in &[
        "semantic",
        "profile",
        "procedural",
        "working",
        "tool_result",
        "episodic",
    ] {
        let r = call(
            "memory_store",
            json!({"content": format!("{mt} content"), "memory_type": mt}),
            &svc,
            &uid,
        )
        .await;
        assert!(
            text(&r).contains("Stored memory"),
            "type={mt}: {}",
            text(&r)
        );
    }
    let list = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(list.len(), 6);
    println!("✅ store all 6 memory types");
}

// ── 2. memory_store: session_id and trust_tier round-trip ────────────────────

#[tokio::test]
async fn test_store_session_and_trust_tier() {
    let (svc, uid, _ctx) = setup().await;
    let r = call(
        "memory_store",
        json!({"content": "session memory", "session_id": "sess-abc", "trust_tier": "T1"}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(t.contains("Stored"), "{t}");
    let mid = t.split_whitespace().nth(2).unwrap().trim_end_matches(':');
    let m = svc.get_for_user(&uid, mid).await.unwrap().unwrap();
    assert_eq!(m.session_id.as_deref(), Some("sess-abc"));
    assert_eq!(m.trust_tier.to_string(), "T1");
    println!(
        "✅ store session_id + trust_tier: session={:?} tier={}",
        m.session_id, m.trust_tier
    );
}

#[tokio::test]
async fn test_store_rejects_invalid_trust_tier() {
    let (svc, uid, _ctx) = setup().await;
    let err = memoria_mcp::tools::call(
        "memory_store",
        json!({"content": "bad tier memory", "trust_tier": "verified"}),
        &svc,
        &uid,
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string().contains("Invalid trust tier: verified"),
        "{err}"
    );
    let list = svc.list_active(&uid, 10).await.unwrap();
    assert!(list.is_empty(), "invalid tier should not store a memory");
    println!("✅ invalid trust_tier rejected explicitly");
}

// ── 2b. memory_store: reject missing/blank content ───────────────────────────

#[tokio::test]
async fn test_store_rejects_empty_content() {
    let (svc, uid, _ctx) = setup().await;
    for args in [json!({}), json!({"content": ""}), json!({"content": "   \t"})] {
        let r = call("memory_store", args, &svc, &uid).await;
        assert!(
            text(&r).contains("content is required"),
            "unexpected response: {}",
            text(&r)
        );
    }
    assert!(
        svc.list_active(&uid, 10).await.unwrap().is_empty(),
        "empty content must not create memories"
    );
    println!("✅ store rejects empty content");
}

#[tokio::test]
async fn test_embedded_store_metadata_round_trip() {
    let (svc, uid, _ctx) = setup().await;
    let metadata = json!({"scene": "embedded", "agent": "mcp"});
    let result = call(
        "memory_store",
        json!({"content": "embedded metadata memory", "extra_metadata": metadata}),
        &svc,
        &uid,
    )
    .await;
    let memory_id = text(&result)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':');
    let stored = svc.get_for_user(&uid, memory_id).await.unwrap().unwrap();
    assert_eq!(
        serde_json::to_value(stored.extra_metadata).unwrap(),
        metadata,
        "embedded MCP must persist metadata"
    );
    let listed = call("memory_list", json!({}), &svc, &uid).await;
    assert!(text(&listed).contains("\"scene\":\"embedded\""));
}

// ── 3. memory_retrieve: returns relevant memories ────────────────────────────

#[tokio::test]
async fn test_retrieve_finds_relevant() {
    let (svc, uid, _ctx) = setup().await;
    call(
        "memory_store",
        json!({"content": "rust ownership model"}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "python list comprehension"}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_retrieve",
        json!({"query": "rust", "top_k": 5}),
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("rust ownership"), "got: {}", text(&r));
    println!("✅ retrieve finds relevant: {}", text(&r));
}

// ── 4. memory_retrieve: empty returns "No relevant memories" ─────────────────

#[tokio::test]
async fn test_retrieve_empty() {
    let (svc, uid, _ctx) = setup().await;
    let r = call(
        "memory_retrieve",
        json!({"query": "nothing here"}),
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("No relevant memories"), "{}", text(&r));
    println!("✅ retrieve empty: {}", text(&r));
}

// ── 3b. memory_retrieve/search: reject missing/blank query ───────────────────

#[tokio::test]
async fn test_retrieve_and_search_reject_missing_query() {
    let (svc, uid, _ctx) = setup().await;

    for (tool, args) in [
        ("memory_retrieve", json!({})),
        ("memory_retrieve", json!({"query": ""})),
        ("memory_retrieve", json!({"query": "   \t"})),
        ("memory_search", json!({})),
        ("memory_search", json!({"query": ""})),
        ("memory_search", json!({"query": "   \t"})),
    ] {
        let r = call(tool, args, &svc, &uid).await;
        assert!(
            text(&r).contains("query is required"),
            "{tool} unexpected response: {}",
            text(&r)
        );
    }
    println!("✅ retrieve/search reject missing query");
}

#[tokio::test]
async fn test_retrieve_session_scope_only() {
    let (svc, uid, _ctx) = setup().await;
    let target_session = format!("session:test-retrieve-{}", Uuid::new_v4().simple());
    let other_session = format!("session:test-retrieve-{}", Uuid::new_v4().simple());

    call(
        "memory_store",
        json!({"content": "shared retrieve token target", "session_id": target_session}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "shared retrieve token other", "session_id": other_session}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "shared retrieve token global"}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_retrieve",
        json!({"query": "shared retrieve token", "session_id": target_session, "session_scope": "only", "top_k": 5}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(t.contains("shared retrieve token target"), "got: {t}");
    assert!(t.contains("shared retrieve token global"), "got: {t}");
    assert!(
        !t.contains("shared retrieve token other"),
        "session_scope=only should exclude other scoped sessions: {t}"
    );
    println!("✅ retrieve session_scope=only");
}

// ── 5. memory_search: top_k respected ────────────────────────────────────────

#[tokio::test]
async fn test_search_top_k() {
    let (svc, uid, _ctx) = setup().await;
    for i in 0..5 {
        call(
            "memory_store",
            json!({"content": format!("search item {i}")}),
            &svc,
            &uid,
        )
        .await;
    }
    let r = call(
        "memory_search",
        json!({"query": "search item", "top_k": 3}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    let count = t.lines().count();
    assert!(count <= 3, "expected ≤3 results, got {count}: {t}");
    println!("✅ search top_k=3: {count} results");
}

#[tokio::test]
async fn test_search_session_scope_only() {
    let (svc, uid, _ctx) = setup().await;
    let target_session = format!("session:test-search-{}", Uuid::new_v4().simple());
    let other_session = format!("session:test-search-{}", Uuid::new_v4().simple());

    call(
        "memory_store",
        json!({"content": "shared search token target", "session_id": target_session}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "shared search token other", "session_id": other_session}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "shared search token global"}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_search",
        json!({"query": "shared search token", "session_id": target_session, "session_scope": "only", "top_k": 5}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(t.contains("shared search token target"), "got: {t}");
    assert!(t.contains("shared search token global"), "got: {t}");
    assert!(
        !t.contains("shared search token other"),
        "session_scope=only should exclude other scoped sessions: {t}"
    );
    println!("✅ search session_scope=only");
}

#[tokio::test]
async fn test_search_session_scope_requires_session_id() {
    let (svc, uid, _ctx) = setup().await;
    let err = memoria_mcp::tools::call(
        "memory_search",
        json!({"query": "shared search token", "session_scope": "only", "top_k": 5}),
        &svc,
        &uid,
    )
    .await
    .expect_err("missing session_id should be rejected");
    assert_eq!(
        err.to_string(),
        "session_id is required when session_scope is set"
    );
}

// ── 6. memory_correct by memory_id ───────────────────────────────────────────

#[tokio::test]
async fn test_correct_by_id() {
    let (svc, uid, _ctx) = setup().await;
    let stored = call(
        "memory_store",
        json!({"content": "old content"}),
        &svc,
        &uid,
    )
    .await;
    let mid = text(&stored)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string();

    let r = call(
        "memory_correct",
        json!({"memory_id": mid, "new_content": "corrected content", "reason": "test"}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(t.contains("corrected content"), "{t}");

    // correct creates a new memory — extract new id from response
    let new_mid = t
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .trim_end_matches(':');
    assert!(!new_mid.is_empty(), "should have new memory_id");
    assert_ne!(new_mid, mid, "new memory should have different id");

    // New memory should have corrected content
    let new = svc.get_for_user(&uid, new_mid).await.unwrap().unwrap();
    assert_eq!(new.content, "corrected content");
    assert!(new.is_active, "new memory should be active");

    // Old memory should be deactivated (get returns None for inactive)
    let old = svc.get_for_user(&uid, &mid).await.unwrap();
    assert!(
        old.is_none(),
        "old memory should not be returned by get (deactivated)"
    );
    println!("✅ correct by id: old={mid} → new={new_mid} (superseded_by chain)");
}

// ── 7. memory_correct by query (semantic search) ─────────────────────────────

#[tokio::test]
async fn test_correct_by_query() {
    let (svc, uid, _ctx) = setup().await;
    call(
        "memory_store",
        json!({"content": "uses black for formatting"}),
        &svc,
        &uid,
    )
    .await;

    let r = call("memory_correct",
        json!({"query": "formatting tool", "new_content": "uses ruff for formatting", "reason": "switched"}),
        &svc, &uid).await;
    assert!(text(&r).contains("ruff"), "got: {}", text(&r));
    println!("✅ correct by query: {}", text(&r));
}

#[tokio::test]
async fn test_correct_by_query_session_scope_only() {
    let (svc, uid, _ctx) = setup().await;
    let target_session = format!("session:test-correct-{}", Uuid::new_v4().simple());
    let other_session = format!("session:test-correct-{}", Uuid::new_v4().simple());

    call(
        "memory_store",
        json!({"content": "formatting target uses black", "session_id": target_session}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "formatting other uses black", "session_id": other_session}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_correct",
        json!({
            "query": "formatting uses black",
            "session_id": target_session,
            "session_scope": "only",
            "new_content": "formatting target uses ruff",
            "reason": "switched"
        }),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(t.contains("formatting target uses ruff"), "got: {t}");

    let active = svc.list_active(&uid, 10).await.unwrap();
    let contents: Vec<&str> = active.iter().map(|m| m.content.as_str()).collect();
    assert!(contents.contains(&"formatting target uses ruff"));
    assert!(contents.contains(&"formatting other uses black"));
    println!("✅ correct by query session_scope=only");
}

#[tokio::test]
async fn test_correct_by_query_session_scope_requires_session_id() {
    let (svc, uid, _ctx) = setup().await;
    let err = memoria_mcp::tools::call(
        "memory_correct",
        json!({
            "query": "formatting uses black",
            "session_scope": "only",
            "new_content": "formatting target uses ruff",
            "reason": "switched"
        }),
        &svc,
        &uid,
    )
    .await
    .expect_err("missing session_id should be rejected");
    assert_eq!(
        err.to_string(),
        "session_id is required when session_scope is set"
    );
}

// ── 8. memory_correct: no target returns error ───────────────────────────────

#[tokio::test]
async fn test_correct_no_target() {
    let (svc, uid, _ctx) = setup().await;
    let r = call(
        "memory_correct",
        json!({"new_content": "something"}),
        &svc,
        &uid,
    )
    .await;
    assert!(
        text(&r).contains("memory_id") || text(&r).contains("query"),
        "{}",
        text(&r)
    );
    println!("✅ correct no target: {}", text(&r));
}

// ── 9. memory_correct: no new_content returns error ──────────────────────────

#[tokio::test]
async fn test_correct_no_content() {
    let (svc, uid, _ctx) = setup().await;
    let r = call(
        "memory_correct",
        json!({"memory_id": "some-id"}),
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("new_content"), "{}", text(&r));
    println!("✅ correct no content: {}", text(&r));
}

// ── 10. memory_purge: single ID ───────────────────────────────────────────────

#[tokio::test]
async fn test_purge_single() {
    let (svc, uid, _ctx) = setup().await;
    let stored = call("memory_store", json!({"content": "to delete"}), &svc, &uid).await;
    let mid = text(&stored)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string();

    let r = call("memory_purge", json!({"memory_id": mid}), &svc, &uid).await;
    assert!(text(&r).contains("1"), "{}", text(&r));
    assert!(svc.get_for_user(&uid, &mid).await.unwrap().is_none());
    println!("✅ purge single: {}", text(&r));
}

// ── 11. memory_purge: batch comma-separated IDs ───────────────────────────────

#[tokio::test]
async fn test_purge_batch() {
    let (svc, uid, _ctx) = setup().await;
    let mut ids = vec![];
    for i in 0..3 {
        let r = call(
            "memory_store",
            json!({"content": format!("batch {i}")}),
            &svc,
            &uid,
        )
        .await;
        let mid = text(&r)
            .split_whitespace()
            .nth(2)
            .unwrap()
            .trim_end_matches(':')
            .to_string();
        ids.push(mid);
    }
    let batch = ids.join(",");
    let r = call("memory_purge", json!({"memory_id": batch}), &svc, &uid).await;
    assert!(text(&r).contains("3"), "{}", text(&r));
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 0);
    println!("✅ purge batch 3: {}", text(&r));
}

// ── 12. memory_purge: topic bulk delete ───────────────────────────────────────

#[tokio::test]
async fn test_purge_topic() {
    let (svc, uid, _ctx) = setup().await;
    call(
        "memory_store",
        json!({"content": "rust ownership rules"}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "rust borrow checker"}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "python is great"}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_purge",
        json!({"topic": "rust", "reason": "cleanup"}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    // Should have deleted the rust memories
    assert!(t.contains("Purged"), "{t}");
    println!("✅ purge topic 'rust': {t}");
}

// ── 12b. memory_purge: exact session cleanup with memory_types filter ─────────

#[tokio::test]
async fn test_purge_session_id_with_memory_types() {
    let (svc, uid, _ctx) = setup().await;
    let target_session = format!("session:test-smp-{}", Uuid::new_v4().simple());
    let other_session = format!("session:test-smp-{}", Uuid::new_v4().simple());

    call(
        "memory_store",
        json!({"content": "local working alpha", "memory_type": "working", "session_id": target_session}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "local working beta", "memory_type": "working", "session_id": target_session}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "local semantic keep", "memory_type": "semantic", "session_id": target_session}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "local other keep", "memory_type": "working", "session_id": other_session}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_purge",
        json!({"session_id": target_session, "memory_types": ["working"]}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(t.contains("Purged 2"), "{t}");

    let active = svc.list_active(&uid, 10).await.unwrap();
    let contents: Vec<&str> = active.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(active.len(), 2);
    assert!(contents.contains(&"local semantic keep"));
    assert!(contents.contains(&"local other keep"));
    assert!(!contents.contains(&"local working alpha"));
    assert!(!contents.contains(&"local working beta"));
    println!("✅ purge session_id with memory_types: {t}");
}

// ── 13. memory_purge: no target returns error ─────────────────────────────────

#[tokio::test]
async fn test_purge_no_target() {
    let (svc, uid, _ctx) = setup().await;
    let r = call("memory_purge", json!({}), &svc, &uid).await;
    assert!(
        text(&r).contains("memory_id") || text(&r).contains("topic"),
        "{}",
        text(&r)
    );
    println!("✅ purge no target: {}", text(&r));
}

// ── 13b. memory_purge: short topic returns validation error ───────────────────

#[tokio::test]
async fn test_purge_short_topic() {
    let (svc, uid, _ctx) = setup().await;
    // 直接调用不会 panic 的版本
    let result = memoria_mcp::tools::call("memory_purge", json!({"topic": "ab"}), &svc, &uid).await;
    assert!(result.is_err(), "Should return error for short topic");
    let err_msg = format!("{:?}", result.unwrap_err());
    assert!(
        err_msg.contains("at least 3 characters"),
        "expected validation error, got: {err_msg}"
    );
    println!("✅ purge short topic rejected");
}

// ── 14. memory_profile: returns profile memories ─────────────────────────────

#[tokio::test]
async fn test_profile() {
    let (svc, uid, _ctx) = setup().await;
    call(
        "memory_store",
        json!({"content": "prefers tabs", "memory_type": "profile"}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "uses vim", "memory_type": "profile"}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "some semantic fact"}),
        &svc,
        &uid,
    )
    .await;

    let r = call("memory_profile", json!({}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("prefers tabs"), "{t}");
    assert!(t.contains("uses vim"), "{t}");
    assert!(
        !t.contains("some semantic fact"),
        "should only show profile memories: {t}"
    );
    println!("✅ profile: {t}");
}

// ── 15. memory_profile: empty returns message ────────────────────────────────

#[tokio::test]
async fn test_profile_empty() {
    let (svc, uid, _ctx) = setup().await;
    let r = call("memory_profile", json!({}), &svc, &uid).await;
    assert!(text(&r).contains("No profile"), "{}", text(&r));
    println!("✅ profile empty: {}", text(&r));
}

// ── 16. memory_list: limit respected, shows all fields ───────────────────────

#[tokio::test]
async fn test_list_limit_and_fields() {
    let (svc, uid, _ctx) = setup().await;
    for i in 0..5 {
        call(
            "memory_store",
            json!({"content": format!("list item {i}")}),
            &svc,
            &uid,
        )
        .await;
    }
    let r = call("memory_list", json!({"limit": 3}), &svc, &uid).await;
    let t = text(&r);
    let count = t.lines().count();
    assert_eq!(count, 3, "expected 3 lines, got {count}: {t}");
    // Each line should have memory_id, type, content
    for line in t.lines() {
        assert!(line.contains('['), "missing memory_id bracket: {line}");
        assert!(line.contains('('), "missing type parens: {line}");
    }
    println!("✅ list limit=3: {count} items with id+type+content");
}

#[tokio::test]
async fn test_list_session_id_filter() {
    let (svc, uid, _ctx) = setup().await;
    let target_session = format!("session:test-list-{}", Uuid::new_v4().simple());
    let other_session = format!("session:test-list-{}", Uuid::new_v4().simple());

    call(
        "memory_store",
        json!({"content": "list target alpha", "session_id": target_session}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "list other beta", "session_id": other_session}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "list global gamma"}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_list",
        json!({"limit": 10, "session_id": target_session}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(t.contains("list target alpha"), "got: {t}");
    assert!(!t.contains("list other beta"), "got: {t}");
    assert!(
        !t.contains("list global gamma"),
        "exact session list should exclude unscoped memories: {t}"
    );
    println!("✅ list exact session filter");
}

// ── 17. memory_capabilities: lists all tools ─────────────────────────────────

#[tokio::test]
async fn test_capabilities() {
    let (svc, uid, _ctx) = setup().await;
    let r = call("memory_capabilities", json!({}), &svc, &uid).await;
    let t = text(&r);
    let parsed: Value = serde_json::from_str(t)
        .unwrap_or_else(|e| panic!("capabilities text should be JSON ({e}), got: {t}"));
    assert_eq!(
        parsed, r["structuredContent"],
        "text and structuredContent should carry the same catalog"
    );
    let mut advertised = memoria_mcp::tools::list().as_array().unwrap().clone();
    advertised.extend(memoria_mcp::git_tools::list().as_array().unwrap().clone());
    assert_eq!(
        parsed,
        json!({ "tools": advertised }),
        "capabilities should match the advertised tool catalog"
    );
    let names: Vec<&str> = parsed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|tool| tool["name"].as_str())
        .collect();
    for tool in &[
        "memory_store",
        "memory_retrieve",
        "memory_search",
        "memory_correct",
        "memory_purge",
        "memory_profile",
        "memory_feedback",
        "memory_observe",
    ] {
        assert!(names.contains(tool), "missing {tool}: {names:?}");
    }
    // Unadvertised ops/debug tools must NOT appear in capabilities
    for hidden in &[
        "memory_rebuild_index",
        "memory_extract_entities",
        "memory_link_entities",
        "memory_get_retrieval_params",
        "memory_tune_params",
    ] {
        assert!(
            !names.contains(hidden),
            "hidden tool {hidden} should not be in capabilities: {names:?}"
        );
    }
    // Trust-tier guidance now lives in the memory_store input schema.
    let store = parsed["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "memory_store")
        .expect("memory_store advertised");
    let tier_desc = store["inputSchema"]["properties"]["trust_tier"]["description"]
        .as_str()
        .expect("trust_tier description");
    for hint in &[
        "T1/T2/T3/T4",
        "T1 =",
        "T2 =",
        "T3 =",
        "prefer T3 if unsure",
        "T4 =",
    ] {
        assert!(
            tier_desc.contains(hint),
            "missing tier hint {hint}: {tier_desc}"
        );
    }
    println!("✅ capabilities: {} tools", names.len());
}

// ── 18. unknown tool returns error ────────────────────────────────────────────

#[tokio::test]
async fn test_unknown_tool_errors() {
    let (svc, uid, _ctx) = setup().await;
    let result = memoria_mcp::tools::call("nonexistent", json!({}), &svc, &uid).await;
    assert!(result.is_err());
    println!("✅ unknown tool → error");
}

// ── 19. memory_governance: quarantine + cleanup + cooldown ───────────────────

#[tokio::test]
async fn test_governance_quarantine_and_cooldown() {
    let (svc, uid, ctx) = setup().await;

    // Store a memory with very low initial_confidence (T4 = 0.4) and old observed_at
    // so effective_confidence = 0.4 * exp(-365/30) ≈ 0 < 0.2 threshold
    let _sql = ctx.user_store(&uid).await;
    let pool = ctx.user_db_pool(&uid).await;
    let mid = format!("gov_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, trust_tier, initial_confidence, observed_at, created_at) \
         VALUES (?, ?, 'semantic', 'old low confidence memory', '[]', 1, 'T4', 0.4, \
         DATE_SUB(NOW(), INTERVAL 365 DAY), NOW())",
    )
    .bind(&mid)
    .bind(&uid)
    .execute(&pool)
    .await
    .expect("insert old memory");

    // Store a high-confidence memory that should NOT be quarantined
    call(
        "memory_store",
        json!({"content": "recent high confidence", "trust_tier": "T1"}),
        &svc,
        &uid,
    )
    .await;

    // Run governance
    let r = call("memory_governance", json!({"force": true}), &svc, &uid).await;
    let t = text(&r);
    assert!(
        t.contains("quarantined=1"),
        "expected 1 quarantined, got: {t}"
    );
    assert!(t.contains("Governance complete"), "{t}");
    println!("✅ governance: {t}");

    // Verify the old memory is physically deleted by quarantine
    let remaining: Vec<(String,)> =
        sqlx::query_as("SELECT memory_id FROM mem_memories WHERE memory_id = ?")
            .bind(&mid)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        remaining.is_empty(),
        "quarantined memory should be physically deleted"
    );
    println!("✅ quarantined memory physically deleted from DB");

    // High-confidence memory should still be active
    let list = svc.list_active(&uid, 10).await.unwrap();
    assert!(list.iter().any(|m| m.content == "recent high confidence"));
    println!("✅ high-confidence memory still active");

    // Cooldown: second call without force should be skipped
    let r2 = call("memory_governance", json!({}), &svc, &uid).await;
    assert!(
        text(&r2).contains("cooldown"),
        "expected cooldown message: {}",
        text(&r2)
    );
    println!("✅ cooldown enforced: {}", text(&r2));

    // force=true bypasses cooldown
    let r3 = call("memory_governance", json!({"force": true}), &svc, &uid).await;
    assert!(text(&r3).contains("Governance complete"), "{}", text(&r3));
    println!("✅ force=true bypasses cooldown");
}

// ── 20. memory_governance: cleanup_stale removes soft-deleted low-confidence ──

#[tokio::test]
async fn test_governance_cleanup_stale() {
    let (svc, uid, ctx) = setup().await;
    let _sql = ctx.user_store(&uid).await;
    let pool = ctx.user_db_pool(&uid).await;

    // Insert a soft-deleted memory with very low confidence
    let mid = format!("stale_{}", uuid::Uuid::new_v4().simple());
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, source_event_ids, \
         is_active, trust_tier, initial_confidence, observed_at, created_at, updated_at) \
         VALUES (?, ?, 'semantic', 'stale deleted memory', '[]', 0, 'T4', 0.05, NOW(), NOW(), \
         DATE_SUB(NOW(), INTERVAL 25 HOUR))",
    )
    .bind(&mid)
    .bind(&uid)
    .execute(&pool)
    .await
    .expect("insert stale");

    let r = call("memory_governance", json!({"force": true}), &svc, &uid).await;
    let t = text(&r);
    assert!(
        t.contains("cleaned_stale=1"),
        "expected 1 cleaned, got: {t}"
    );
    println!("✅ cleanup_stale: {t}");

    // Verify physically deleted
    let row = sqlx::query("SELECT COUNT(*) as cnt FROM mem_memories WHERE memory_id = ?")
        .bind(&mid)
        .fetch_one(&pool)
        .await
        .expect("fetch");
    let cnt: i64 = row.try_get("cnt").unwrap_or(1);
    assert_eq!(cnt, 0, "stale memory should be physically deleted");
    println!("✅ stale memory physically deleted from DB");
}

// ── 21. memory_consolidate: cooldown and basic run ──────────────────────────

#[tokio::test]
async fn test_consolidate_basic() {
    let (svc, uid, _ctx) = setup().await;

    // First run should succeed
    let r = call("memory_consolidate", json!({"force": true}), &svc, &uid).await;
    let t = text(&r);
    assert!(t.contains("Consolidation complete"), "got: {t}");
    assert!(t.contains("conflicts_detected="), "got: {t}");
    println!("✅ consolidate basic: {t}");

    // Second run without force should hit cooldown
    let r2 = call("memory_consolidate", json!({}), &svc, &uid).await;
    let t2 = text(&r2);
    assert!(t2.contains("cooldown"), "expected cooldown, got: {t2}");
    println!("✅ consolidate cooldown: {t2}");
}

// ── 22. memory_reflect: returns candidates ───────────────────────────────────

#[tokio::test]
async fn test_reflect_candidates() {
    let (svc, uid, _ctx) = setup().await;

    // Store a few memories first
    call(
        "memory_store",
        json!({"content": "Uses Rust for backend services", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "Prefers async/await patterns", "memory_type": "profile"}),
        &svc,
        &uid,
    )
    .await;

    let r = call("memory_reflect", json!({"mode": "candidates"}), &svc, &uid).await;
    let t = text(&r);
    assert!(
        t.contains("Cluster") || t.contains("memory clusters") || t.contains("No memories"),
        "got: {t}"
    );
    println!("✅ reflect candidates: {}", &t[..t.len().min(120)]);
}

// ── 23. memory_reflect: internal mode returns error ──────────────────────────

#[tokio::test]
async fn test_reflect_internal_unavailable() {
    let (svc, uid, _ctx) = setup().await;
    let r = call("memory_reflect", json!({"mode": "internal"}), &svc, &uid).await;
    let t = text(&r);
    assert!(
        t.contains("not available") || t.contains("requires"),
        "got: {t}"
    );
    println!("✅ reflect internal unavailable: {t}");
}

// ── 24. memory_extract_entities + memory_link_entities ───────────────────────

#[tokio::test]
async fn test_extract_and_link_entities() {
    let (svc, uid, _ctx) = setup().await;

    // Store a memory
    let store_r = call(
        "memory_store",
        json!({"content": "Project uses Rust and MatrixOne database"}),
        &svc,
        &uid,
    )
    .await;
    let store_t = text(&store_r);
    println!("store: {store_t}");

    // Extract candidates
    let r = call(
        "memory_extract_entities",
        json!({"mode": "candidates"}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    println!("extract: {}", &t[..t.len().min(200)]);

    let parsed: serde_json::Value = serde_json::from_str(&t).unwrap_or(serde_json::Value::Null);
    if parsed["status"] == "complete" {
        println!("✅ no unlinked memories (already linked or empty)");
        return;
    }
    assert_eq!(parsed["status"], "candidates", "got: {t}");
    let memories = parsed["memories"].as_array().expect("memories array");
    assert!(!memories.is_empty(), "should have unlinked memories");

    let memory_id = memories[0]["memory_id"].as_str().expect("memory_id");

    // Link entities
    let link_payload = serde_json::to_string(&json!([{
        "memory_id": memory_id,
        "entities": [{"name": "Rust", "type": "tech"}, {"name": "MatrixOne", "type": "tech"}]
    }]))
    .unwrap();

    let r2 = call(
        "memory_link_entities",
        json!({"entities": link_payload}),
        &svc,
        &uid,
    )
    .await;
    let t2 = text(&r2);
    let parsed2: serde_json::Value = serde_json::from_str(&t2).expect("valid json");
    assert_eq!(parsed2["status"], "done", "got: {t2}");
    assert!(
        parsed2["entities_created"].as_i64().unwrap_or(0) >= 1,
        "got: {t2}"
    );
    println!("✅ link_entities: {t2}");

    // Re-extract: this memory should now be linked
    let r3 = call(
        "memory_extract_entities",
        json!({"mode": "candidates"}),
        &svc,
        &uid,
    )
    .await;
    let t3 = text(&r3);
    let parsed3: serde_json::Value = serde_json::from_str(&t3).unwrap_or(serde_json::Value::Null);
    // The linked memory should no longer appear in unlinked list
    if parsed3["status"] == "candidates" {
        let mems = parsed3["memories"].as_array().unwrap();
        assert!(
            !mems.iter().any(|m| m["memory_id"] == memory_id),
            "linked memory should not appear again"
        );
    }
    println!("✅ extract after link: {}", &t3[..t3.len().min(100)]);
}

// ── 25. memory_link_entities: invalid JSON returns error ─────────────────────

#[tokio::test]
async fn test_link_entities_invalid_json() {
    let (svc, uid, _ctx) = setup().await;
    let r = call(
        "memory_link_entities",
        json!({"entities": "not json"}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    let parsed: serde_json::Value = serde_json::from_str(&t).expect("valid json");
    assert_eq!(parsed["status"], "error");
    println!("✅ link_entities invalid json: {t}");
}

// ── LLM-related tests ────────────────────────────────────────────────────────

/// Helper: create service with explicit LLM client (for testing LLM paths).
async fn setup_with_llm(
    llm: Option<Arc<memoria_embedding::LlmClient>>,
) -> (
    Arc<MemoryService>,
    String,
    support::multi_db::McpTestContext,
) {
    let ctx =
        support::multi_db::setup_mcp_context("core_tools_e2e_llm", test_dim(), None, llm).await;
    (ctx.service(), uid(), ctx)
}

// ── 26. reflect without LLM returns candidates (not error) ───────────────────

#[tokio::test]
async fn test_reflect_no_llm_returns_candidates() {
    let (svc, uid, _ctx) = setup_with_llm(None).await;

    // Store memories in two different "sessions" to create clusters
    call(
        "memory_store",
        json!({"content": "Uses Rust for backend", "session_id": "s1"}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "Prefers async patterns", "session_id": "s1"}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "MatrixOne as database", "session_id": "s2"}),
        &svc,
        &uid,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "Deploys with Docker", "session_id": "s2"}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_reflect",
        json!({"mode": "auto", "force": true}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    // Without LLM: should return candidates or "no clusters" — NOT an error
    assert!(
        t.contains("cluster")
            || t.contains("Cluster")
            || t.contains("No memory")
            || t.contains("memories"),
        "without LLM should return candidates or no-clusters message, got: {t}"
    );
    // Must NOT say "error" or "failed"
    assert!(
        !t.to_lowercase().contains("error"),
        "should not return error without LLM, got: {t}"
    );
    println!("✅ reflect without LLM: {}", &t[..t.len().min(100)]);
}

// ── 27. reflect with mode=candidates always returns candidates ────────────────

#[tokio::test]
async fn test_reflect_candidates_mode_no_llm_needed() {
    let (svc, uid, _ctx) = setup_with_llm(None).await;
    call(
        "memory_store",
        json!({"content": "Test memory for candidates mode"}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_reflect",
        json!({"mode": "candidates", "force": true}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    // candidates mode should always work regardless of LLM
    assert!(
        t.contains("cluster")
            || t.contains("Cluster")
            || t.contains("No memory")
            || t.contains("memories"),
        "candidates mode should work without LLM, got: {t}"
    );
    println!(
        "✅ reflect candidates mode (no LLM): {}",
        &t[..t.len().min(100)]
    );
}

// ── 28. reflect with mode=internal and no LLM returns clear error ─────────────

#[tokio::test]
async fn test_reflect_internal_no_llm_returns_error() {
    let (svc, uid, _ctx) = setup_with_llm(None).await;
    let r = call("memory_reflect", json!({"mode": "internal"}), &svc, &uid).await;
    let t = text(&r);
    assert!(
        t.contains("LLM_API_KEY") || t.contains("requires") || t.contains("not configured"),
        "internal mode without LLM should return clear error, got: {t}"
    );
    println!("✅ reflect internal without LLM: {t}");
}

// ── 29. extract_entities without LLM returns candidates ──────────────────────

#[tokio::test]
async fn test_extract_entities_no_llm_returns_candidates() {
    let (svc, uid, _ctx) = setup_with_llm(None).await;
    call(
        "memory_store",
        json!({"content": "Project uses Rust and MatrixOne"}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_extract_entities",
        json!({"mode": "auto"}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    let parsed: serde_json::Value = serde_json::from_str(&t).unwrap_or(serde_json::Value::Null);
    // Should return candidates or complete (if regex already linked), NOT error
    assert!(
        parsed["status"] == "candidates" || parsed["status"] == "complete",
        "without LLM should return candidates or complete, got: {t}"
    );
    println!(
        "✅ extract_entities without LLM: status={}",
        parsed["status"]
    );
}

// ── 30. extract_entities with mode=internal and no LLM returns error ──────────

#[tokio::test]
async fn test_extract_entities_internal_no_llm_returns_error() {
    let (svc, uid, _ctx) = setup_with_llm(None).await;
    let r = call(
        "memory_extract_entities",
        json!({"mode": "internal"}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(
        t.contains("LLM_API_KEY") || t.contains("requires") || t.contains("not configured"),
        "internal mode without LLM should return clear error, got: {t}"
    );
    println!("✅ extract_entities internal without LLM: {t}");
}

// ── 31. reflect with LLM — deterministic fake runtime ────────────────────────

#[tokio::test]
async fn test_reflect_with_llm_if_configured() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (svc, uid, ctx) = setup_with_llm(Some(llm)).await;
    let pool = ctx.user_db_pool(&uid).await;

    let sql = ctx.user_store(&uid).await;
    let graph = sql.graph_store();
    for (idx, content) in [
        "Uses Rust for backend",
        "Prefers async patterns",
        "MatrixOne as database",
    ]
    .into_iter()
    .enumerate()
    {
        graph
            .create_node(&memoria_storage::GraphNode {
                node_id: format!(
                    "reflect_node_{idx}_{}",
                    &Uuid::new_v4().simple().to_string()[..8]
                ),
                user_id: uid.clone(),
                node_type: memoria_storage::NodeType::Semantic,
                content: content.to_string(),
                entity_type: None,
                embedding: None,
                memory_id: None,
                session_id: Some("llm_cluster".to_string()),
                confidence: 0.8,
                trust_tier: "T3".to_string(),
                importance: 0.5,
                source_nodes: vec![],
                conflicts_with: None,
                conflict_resolution: None,
                access_count: 0,
                cross_session_count: 0,
                is_active: true,
                superseded_by: None,
                created_at: Some(chrono::Utc::now().naive_utc()),
            })
            .await
            .unwrap();
    }

    let r = call(
        "memory_reflect",
        json!({"mode": "auto", "force": true}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(
        t.contains("scenes_created=1"),
        "with LLM should return reflect result, got: {t}"
    );

    let rows = sqlx::query("SELECT content FROM mem_memories WHERE user_id = ? AND is_active = 1")
        .bind(&uid)
        .fetch_all(&pool)
        .await
        .unwrap();
    let contents: Vec<String> = rows
        .iter()
        .filter_map(|row| row.try_get::<String, _>("content").ok())
        .collect();
    assert!(
        contents
            .iter()
            .any(|c| c.contains("Prefer deterministic validation")),
        "reflect should store synthesized memory: {contents:?}"
    );
    println!("✅ reflect with LLM: {t}");
}

// ── 32. extract_entities with LLM — deterministic fake runtime ───────────────

#[tokio::test]
async fn test_extract_entities_with_llm_if_configured() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (svc, uid, ctx) = setup_with_llm(Some(llm)).await;

    call(
        "memory_store",
        json!({"content": "our stack relies on alphamesh for routing and deltafabric for caching"}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_extract_entities",
        json!({"mode": "auto"}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    let parsed: serde_json::Value = serde_json::from_str(&t).unwrap_or(serde_json::Value::Null);
    assert_eq!(
        parsed["status"], "done",
        "with LLM should return done, got: {t}"
    );
    assert!(
        parsed["entities_found"].as_u64().unwrap_or(0) >= 2,
        "fake LLM should create entities: {t}"
    );

    let sql = ctx.user_store(&uid).await;
    let graph = sql.graph_store();
    let entities = graph.get_user_entities(&uid).await.unwrap();
    let names: Vec<String> = entities.iter().map(|(name, _)| name.clone()).collect();
    assert!(
        names.iter().any(|name| name == "alphamesh")
            && names.iter().any(|name| name == "deltafabric"),
        "LLM extraction should persist entities: {names:?}"
    );
    println!("✅ extract_entities with LLM: {t}");
}

// ── 33. retrieve: fulltext fallback when no embedding ────────────────────────

#[tokio::test]
async fn test_retrieve_fulltext_fallback() {
    let (svc, uid, _ctx) = setup().await;
    // Store a memory with a unique keyword
    call(
        "memory_store",
        json!({"content": "xylophone_unique_test_keyword_42"}),
        &svc,
        &uid,
    )
    .await;
    // Retrieve by exact keyword — should work via fulltext even without embedding
    let r = call(
        "memory_retrieve",
        json!({"query": "xylophone_unique_test_keyword_42", "top_k": 5}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(
        t.contains("xylophone_unique_test_keyword_42"),
        "fulltext fallback should find exact keyword: {t}"
    );
    println!("✅ retrieve fulltext fallback works");
}

// ── 34. search: top_k=0 returns empty ────────────────────────────────────────

#[tokio::test]
async fn test_search_top_k_zero() {
    let (svc, uid, _ctx) = setup().await;
    call(
        "memory_store",
        json!({"content": "topk zero test"}),
        &svc,
        &uid,
    )
    .await;
    let r = call(
        "memory_search",
        json!({"query": "topk zero", "top_k": 0}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    // top_k=0 should return no results or "No relevant memories"
    assert!(
        t.contains("No relevant") || !t.contains("topk zero test"),
        "top_k=0 should return empty: {t}"
    );
    println!("✅ search top_k=0: {}", &t[..t.len().min(80)]);
}

// ── 35. store + retrieve with session_id ─────────────────────────────────────

#[tokio::test]
async fn test_store_with_session_id_retrievable() {
    let (svc, uid, _ctx) = setup().await;
    let sid = format!(
        "sess_{}",
        uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
    );
    call(
        "memory_store",
        json!({"content": "session-specific memory alpha", "session_id": sid}),
        &svc,
        &uid,
    )
    .await;
    let r = call(
        "memory_retrieve",
        json!({"query": "session-specific alpha", "top_k": 5}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(
        t.contains("session-specific memory alpha"),
        "should find session memory: {t}"
    );
    println!("✅ store with session_id retrievable");
}

// ── 36. purge by topic then verify gone ──────────────────────────────────────

#[tokio::test]
async fn test_purge_topic_then_search_empty() {
    let (svc, uid, _ctx) = setup().await;
    let tag = format!(
        "purgetopic_{}",
        uuid::Uuid::new_v4().simple().to_string()[..6].to_string()
    );
    for i in 0..3 {
        call(
            "memory_store",
            json!({"content": format!("{tag} item {i}")}),
            &svc,
            &uid,
        )
        .await;
    }
    // Purge by topic
    let r = call("memory_purge", json!({"topic": &tag}), &svc, &uid).await;
    let t = text(&r);
    assert!(
        t.contains("Purged") || t.contains("purged") || t.contains("Deleted"),
        "purge result: {t}"
    );
    println!("✅ purge by topic: {t}");

    // Search should find nothing
    let r = call(
        "memory_search",
        json!({"query": &tag, "top_k": 10}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(
        t.contains("No relevant") || !t.contains(&tag),
        "should be empty after purge: {t}"
    );
    println!("✅ search after purge: empty");
}

// ── 37. explain mode on retrieve ─────────────────────────────────────────────

#[tokio::test]
async fn test_retrieve_explain_mode() {
    let (svc, uid, _ctx) = setup().await;
    call(
        "memory_store",
        json!({"content": "explain test memory alpha"}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_retrieve",
        json!({"query": "explain test alpha", "top_k": 5, "explain": true}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(
        t.contains("--- explain ---"),
        "should contain explain section: {t}"
    );
    assert!(t.contains("\"path\""), "should contain path field: {t}");
    assert!(t.contains("\"total_ms\""), "should contain total_ms: {t}");
    println!(
        "✅ retrieve explain mode: {}",
        &t[t.find("--- explain ---").unwrap_or(0)..]
    );
}

#[tokio::test]
async fn test_retrieve_explain_empty() {
    let (svc, uid, _ctx) = setup().await;
    let r = call(
        "memory_retrieve",
        json!({"query": "nonexistent_xyz_123", "top_k": 5, "explain": true}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(
        t.contains("No relevant memories"),
        "should say no results: {t}"
    );
    assert!(
        t.contains("--- explain ---"),
        "should still have explain: {t}"
    );
    println!("✅ retrieve explain empty: has stats even with no results");
}

#[tokio::test]
async fn test_search_explain_mode() {
    let (svc, uid, _ctx) = setup().await;
    call(
        "memory_store",
        json!({"content": "search explain test beta"}),
        &svc,
        &uid,
    )
    .await;

    let r = call(
        "memory_search",
        json!({"query": "search explain beta", "top_k": 5, "explain": true}),
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(
        t.contains("--- explain ---"),
        "search should also support explain: {t}"
    );
    println!("✅ search explain mode works");
}
