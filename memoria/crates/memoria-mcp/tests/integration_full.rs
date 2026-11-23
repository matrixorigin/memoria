/// Full-stack integration test: exercises every MCP tool in a realistic workflow,
/// then verifies every DB field directly.
///
/// Scenario: user sets up preferences, works on a project, creates a branch to
/// experiment, snapshots, rolls back, merges — then we check every column in DB.
///
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria \
///      cargo test -p memoria-mcp --test integration_full -- --test-threads=1 --nocapture
mod support;

use chrono::Utc;
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use serde_json::{json, Value};
use sqlx::mysql::MySqlPool;
use sqlx::Row;
use std::sync::Arc;
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

async fn setup() -> (
    Arc<MemoryService>,
    Arc<GitForDataService>,
    String,
    support::multi_db::McpTestContext,
) {
    let ctx =
        support::multi_db::setup_mcp_context("integration_full", test_dim(), None, None).await;
    let uid = format!("integ_{}", &Uuid::new_v4().simple().to_string()[..8]);
    (ctx.service(), ctx.git(), uid, ctx)
}

async fn tc(name: &str, args: Value, svc: &Arc<MemoryService>, uid: &str) -> Value {
    // Keep the dispatch future off this long scenario's stack. In debug builds,
    // nesting it inline overflows the default Rust test-thread stack.
    Box::pin(memoria_mcp::tools::call(name, args, svc, uid))
        .await
        .expect(name)
}
async fn gc(
    name: &str,
    args: Value,
    git: &Arc<GitForDataService>,
    svc: &Arc<MemoryService>,
    uid: &str,
) -> Value {
    // Match the regular tool helper's bounded async state for git dispatch.
    Box::pin(memoria_mcp::git_tools::call(name, args, git, svc, uid))
        .await
        .expect(name)
}
fn text(v: &Value) -> &str {
    v["content"][0]["text"].as_str().unwrap_or("")
}

/// Read a memory row directly from DB and return all columns as a map.
async fn db_row(
    pool: &MySqlPool,
    table: &str,
    memory_id: &str,
) -> std::collections::HashMap<String, String> {
    let sql = format!(
        "SELECT memory_id, user_id, memory_type, content, \
         embedding AS embedding, \
         session_id, CAST(source_event_ids AS CHAR) as source_event_ids, \
         CAST(extra_metadata AS CHAR) as extra_metadata, \
         is_active, superseded_by, trust_tier, initial_confidence, \
         observed_at, created_at, updated_at \
         FROM {table} WHERE memory_id = ?"
    );
    let row = sqlx::query(&sql)
        .bind(memory_id)
        .fetch_one(pool)
        .await
        .expect("db_row");
    let mut map = std::collections::HashMap::new();
    let str_cols = [
        "memory_id",
        "user_id",
        "memory_type",
        "content",
        "session_id",
        "source_event_ids",
        "extra_metadata",
        "superseded_by",
        "trust_tier",
    ];
    for col in &str_cols {
        let val = row
            .try_get::<Option<String>, _>(*col)
            .unwrap_or(None)
            .unwrap_or_default();
        map.insert(col.to_string(), val);
    }
    // TINYINT(1)
    let active: i8 = row.try_get("is_active").unwrap_or(0);
    map.insert("is_active".to_string(), active.to_string());
    // FLOAT
    let conf: f32 = row.try_get("initial_confidence").unwrap_or(0.0);
    map.insert("initial_confidence".to_string(), conf.to_string());
    // DATETIME(6) — stored as NaiveDateTime
    for col in &["observed_at", "created_at", "updated_at"] {
        let val = row
            .try_get::<Option<chrono::NaiveDateTime>, _>(*col)
            .unwrap_or(None)
            .map(|dt| dt.to_string())
            .unwrap_or_default();
        map.insert(col.to_string(), val);
    }
    // vecf32 embedding
    let emb: Option<String> = row.try_get("embedding").unwrap_or(None);
    map.insert("embedding".to_string(), emb.unwrap_or_default());
    map
}

fn extract_mid(store_response: &str) -> String {
    // "Stored memory <id>: <content>"
    store_response
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .trim_end_matches(':')
        .to_string()
}

#[test]
fn full_stack_future_stays_bounded() {
    let future = full_stack_all_fields();
    let size = std::mem::size_of_val(&future);
    assert!(size <= 16 * 1024, "full-stack future uses {size} bytes");
}

#[tokio::test]
async fn test_full_stack_all_fields() {
    full_stack_all_fields().await;
}

async fn full_stack_all_fields() {
    let (svc, git, uid, ctx) = setup().await;
    let pool = ctx.user_db_pool(&uid).await;
    let before = Utc::now();

    // ── Phase 1: Store memories with all variants ─────────────────────────────

    // Profile memory with session_id and T1 trust
    let r = tc(
        "memory_store",
        json!({
            "content": "prefers Rust over Python",
            "memory_type": "profile",
            "session_id": "sess-001",
            "trust_tier": "T1"
        }),
        &svc,
        &uid,
    )
    .await;
    let mid_profile = extract_mid(text(&r));
    assert!(!mid_profile.is_empty(), "no memory_id in: {}", text(&r));

    // Semantic memory
    let r = tc(
        "memory_store",
        json!({
            "content": "project uses MatrixOne as primary database",
            "memory_type": "semantic"
        }),
        &svc,
        &uid,
    )
    .await;
    let mid_semantic = extract_mid(text(&r));

    // Procedural memory
    let r = tc(
        "memory_store",
        json!({
            "content": "deploy with: make build && kubectl apply",
            "memory_type": "procedural"
        }),
        &svc,
        &uid,
    )
    .await;
    let _mid_procedural = extract_mid(text(&r));

    // Working memory
    let r = tc(
        "memory_store",
        json!({
            "content": "currently debugging embedding issue",
            "memory_type": "working"
        }),
        &svc,
        &uid,
    )
    .await;
    let mid_working = extract_mid(text(&r));

    println!("✅ Phase 1: stored 4 memories");

    // ── Phase 2: Verify all DB fields for profile memory ─────────────────────

    let row = db_row(&pool, "mem_memories", &mid_profile).await;

    assert_eq!(row["memory_id"], mid_profile, "memory_id mismatch");
    assert_eq!(row["user_id"], uid, "user_id mismatch");
    assert_eq!(row["memory_type"], "profile", "memory_type mismatch");
    assert_eq!(
        row["content"], "prefers Rust over Python",
        "content mismatch"
    );
    assert_eq!(row["session_id"], "sess-001", "session_id mismatch");
    assert_eq!(row["trust_tier"], "T1", "trust_tier mismatch");
    assert_eq!(row["is_active"], "1", "is_active should be 1");
    assert!(
        row["superseded_by"].is_empty(),
        "superseded_by should be NULL"
    );
    assert!(
        row["source_event_ids"].contains('['),
        "source_event_ids should be JSON array"
    );
    // MO#23859 workaround: NULL stored as "{}" to avoid ByteJson corruption
    assert!(
        row["extra_metadata"].is_empty() || row["extra_metadata"] == "{}",
        "extra_metadata should be NULL or {{}} (MO#23859 workaround)"
    );
    assert!(
        row["embedding"].is_empty(),
        "embedding should be NULL without embedder"
    );
    assert!(!row["created_at"].is_empty(), "created_at must be set");
    assert!(!row["observed_at"].is_empty(), "observed_at must be set");
    // T1 trust tier → initial_confidence = 0.95
    let conf: f64 = row["initial_confidence"].parse().unwrap_or(0.0);
    assert!(
        (conf - 0.95).abs() < 0.01,
        "T1 confidence should be 0.95, got {conf}"
    );

    println!("✅ Phase 2: all DB fields verified for profile memory");

    // ── Phase 3: Retrieve and search ─────────────────────────────────────────

    let r = tc(
        "memory_retrieve",
        json!({"query": "Rust programming", "top_k": 5}),
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("prefers Rust"), "retrieve: {}", text(&r));

    let r = tc(
        "memory_search",
        json!({"query": "database", "top_k": 3}),
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("MatrixOne"), "search: {}", text(&r));

    println!("✅ Phase 3: retrieve and search work");

    // ── Phase 4: Correct by ID — verify DB updated ───────────────────────────

    let r = tc(
        "memory_correct",
        json!({
            "memory_id": mid_working,
            "new_content": "debugging resolved: embedding service was misconfigured",
            "reason": "issue fixed"
        }),
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("debugging resolved"), "{}", text(&r));

    // correct creates a new memory — extract new id
    let new_mid_working = text(&r)
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .trim_end_matches(':')
        .to_string();
    assert_ne!(
        new_mid_working, mid_working,
        "correct should create new memory"
    );

    // Old memory should be deactivated with superseded_by
    let old_row = db_row(&pool, "mem_memories", &mid_working).await;
    assert_eq!(
        old_row["is_active"], "0",
        "old memory should be deactivated"
    );
    assert_eq!(
        old_row["superseded_by"], new_mid_working,
        "old should point to new"
    );

    // New memory should have corrected content
    let new_row = db_row(&pool, "mem_memories", &new_mid_working).await;
    assert_eq!(
        new_row["content"],
        "debugging resolved: embedding service was misconfigured"
    );
    assert!(
        !new_row["updated_at"].is_empty() || !new_row["created_at"].is_empty(),
        "timestamps must be set"
    );
    println!("✅ Phase 4: correct by ID — superseded_by chain verified");

    // ── Phase 5: Correct by query ─────────────────────────────────────────────

    let r = tc(
        "memory_correct",
        json!({
            "query": "deploy command",
            "new_content": "deploy with: make release && helm upgrade",
            "reason": "switched to helm"
        }),
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("helm"), "{}", text(&r));

    let new_mid_proc = text(&r)
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .trim_end_matches(':')
        .to_string();
    let new_row = db_row(&pool, "mem_memories", &new_mid_proc).await;
    assert_eq!(
        new_row["content"],
        "deploy with: make release && helm upgrade"
    );
    println!("✅ Phase 5: correct by query — DB verified");

    // ── Phase 6: Profile tool ─────────────────────────────────────────────────

    let r = tc("memory_profile", json!({}), &svc, &uid).await;
    assert!(text(&r).contains("prefers Rust"), "{}", text(&r));
    println!("✅ Phase 6: profile shows profile memories");

    // ── Phase 7: Snapshot → branch → store → diff → merge ────────────────────

    let snap_name = format!("integ_{}", &uid[6..]);
    gc(
        "memory_snapshot",
        json!({"name": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;

    let branch = format!("exp_{}", &uid[6..]);
    gc("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    gc("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;

    // Store on branch
    let r = tc(
        "memory_store",
        json!({
            "content": "experiment: try DuckDB instead",
            "memory_type": "working"
        }),
        &svc,
        &uid,
    )
    .await;
    let mid_branch = extract_mid(text(&r));

    // Verify branch memory is in branch table, not main
    let sql_store = ctx.user_store(&uid).await;
    let branches = sql_store.list_branches(&uid).await.unwrap();
    let branch_table = branches
        .iter()
        .find(|(n, _, _)| n == &branch)
        .map(|(_, t, _)| t.clone())
        .unwrap();

    let branch_row = db_row(&pool, &branch_table, &mid_branch).await;
    assert_eq!(branch_row["content"], "experiment: try DuckDB instead");
    assert_eq!(branch_row["user_id"], uid);

    // Verify NOT in main
    let main_check = sqlx::query("SELECT COUNT(*) as cnt FROM mem_memories WHERE memory_id = ?")
        .bind(&mid_branch)
        .fetch_one(&pool)
        .await
        .unwrap();
    let cnt: i64 = main_check.try_get("cnt").unwrap_or(0);
    assert_eq!(cnt, 0, "branch memory should not be in main yet");

    println!("✅ Phase 7a: branch memory in branch table, not in main");

    // Diff
    gc("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    let r = gc("memory_diff", json!({"source": branch}), &git, &svc, &uid).await;
    assert!(
        text(&r).contains("DuckDB"),
        "diff should show branch memory: {}",
        text(&r)
    );
    println!("✅ Phase 7b: diff shows branch-only memory");

    // Merge
    let r = gc(
        "memory_merge",
        json!({"source": branch, "strategy": "append"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("1 new"), "merge: {}", text(&r));

    // Verify merged memory now in main with all fields intact
    let merged_row = db_row(&pool, "mem_memories", &mid_branch).await;
    assert_eq!(merged_row["content"], "experiment: try DuckDB instead");
    assert_eq!(merged_row["user_id"], uid);
    assert_eq!(merged_row["memory_type"], "working");
    assert_eq!(merged_row["is_active"], "1");
    assert!(!merged_row["created_at"].is_empty());
    println!("✅ Phase 7c: merged memory in main with all fields intact");

    // ── Phase 8: Purge batch ──────────────────────────────────────────────────

    // mid_working was already deactivated by correct in Phase 4, so purge it + mid_branch
    let r = tc(
        "memory_purge",
        json!({
            "memory_id": format!("{new_mid_working},{mid_branch}"),
            "reason": "cleanup working memories"
        }),
        &svc,
        &uid,
    )
    .await;
    // Both should be purged (new_mid_working is active, mid_branch is active)
    assert!(text(&r).contains("2"), "{}", text(&r));

    // Verify soft-deleted in DB (is_active = 0)
    let sql = "SELECT is_active FROM mem_memories WHERE memory_id = ?";
    let row = sqlx::query(sql)
        .bind(&new_mid_working)
        .fetch_one(&pool)
        .await
        .unwrap();
    let active: i8 = row.try_get("is_active").unwrap_or(1);
    assert_eq!(active, 0, "purged memory should have is_active=0");
    println!("✅ Phase 8: purge batch — is_active=0 verified in DB");

    // ── Phase 9: Rollback to snapshot ────────────────────────────────────────

    let count_before = svc.list_active(&uid, 100).await.unwrap().len();
    gc(
        "memory_rollback",
        json!({"name": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
    let count_after = svc.list_active(&uid, 100).await.unwrap().len();
    // After rollback, should have snapshot-time count (4 memories)
    assert_eq!(
        count_after, 4,
        "after rollback should have 4 memories, got {count_after}"
    );
    println!("✅ Phase 9: rollback — {count_before} → {count_after} memories");

    // ── Phase 10: Timestamps are reasonable ──────────────────────────────────

    let row = db_row(&pool, "mem_memories", &mid_semantic).await;
    // observed_at and created_at should be after test start
    let created_str = &row["created_at"];
    assert!(!created_str.is_empty(), "created_at must not be empty");
    // Just verify it's a valid datetime string
    assert!(
        created_str.contains("2026") || created_str.contains("2025"),
        "created_at should be recent: {created_str}"
    );
    println!("✅ Phase 10: timestamps are valid: created_at={created_str}");

    // ── Cleanup ───────────────────────────────────────────────────────────────
    gc(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
    gc(
        "memory_snapshot_delete",
        json!({"names": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;

    let after = Utc::now();
    println!(
        "✅ Full integration test passed in {}ms",
        (after - before).num_milliseconds()
    );
}
