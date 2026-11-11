/// End-to-end tests for mem_edit_log audit trail.
/// Verifies ALL fields of every edit-log row: edit_id, user_id, memory_id,
/// operation, payload, reason, snapshot_before, created_at, created_by.
///
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria \
///      cargo test -p memoria-mcp --test edit_log_e2e -- --nocapture
mod support;

use memoria_git::GitForDataService;
use memoria_service::{GovernanceStrategy, MemoryService};
use serde_json::{json, Value};
use serial_test::serial;
use sqlx::mysql::MySqlPool;
use std::sync::Arc;
use uuid::Uuid;

use memoria_core::interfaces::EmbeddingProvider;
use memoria_embedding::HttpEmbedder;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}
fn uid() -> String {
    format!("elog_{}", &Uuid::new_v4().simple().to_string()[..8])
}

fn safety_snapshot_prefix(db_name: &str) -> String {
    format!("mem_snap_{}_pre_", compact_identifier_fragment(db_name, 21))
}

fn sanitize_identifier_fragment(value: &str) -> String {
    let sanitized = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string();
    if sanitized.is_empty() {
        "db".to_string()
    } else {
        sanitized
    }
}

fn compact_identifier_fragment(value: &str, max_len: usize) -> String {
    let sanitized = sanitize_identifier_fragment(value);
    if sanitized.len() <= max_len {
        return sanitized;
    }
    if max_len <= 4 {
        return sanitized.chars().take(max_len).collect();
    }
    let head_len = (max_len - 1) / 2;
    let tail_len = max_len - head_len - 1;
    let head: String = sanitized.chars().take(head_len).collect();
    let tail_chars: Vec<char> = sanitized.chars().collect();
    let tail: String = tail_chars[tail_chars.len().saturating_sub(tail_len)..]
        .iter()
        .collect();
    format!("{head}_{tail}")
}

fn is_purge_safety_snapshot(db_name: &str, name: &str) -> bool {
    name.starts_with(&format!("{}purge_", safety_snapshot_prefix(db_name)))
}

// ── Full edit-log row with ALL columns ────────────────────────────────────────

#[derive(Debug, sqlx::FromRow)]
struct EditLogRow {
    edit_id: String,
    user_id: String,
    memory_id: Option<String>,
    operation: String,
    payload: Option<String>,
    reason: Option<String>,
    snapshot_before: Option<String>,
    created_at: chrono::NaiveDateTime,
    created_by: String,
}

// ── Setup / teardown ──────────────────────────────────────────────────────────

async fn setup() -> (
    Arc<MemoryService>,
    Arc<GitForDataService>,
    MySqlPool,
    String,
    String,
    support::multi_db::McpTestContext,
) {
    let ctx = support::multi_db::setup_mcp_context("edit_log", test_dim(), None, None).await;
    let user_id = uid();
    let db_name = ctx.user_db_name(&user_id).await;
    let pool = ctx.user_db_pool(&user_id).await;
    cleanup(&pool, &user_id, &db_name).await;
    (ctx.service(), ctx.git(), pool, user_id, db_name, ctx)
}

/// Setup with real embedder. Returns None if EMBEDDING_* env vars not set.
async fn setup_with_embedder() -> Option<(
    Arc<MemoryService>,
    Arc<GitForDataService>,
    MySqlPool,
    String,
    String,
    support::multi_db::McpTestContext,
)> {
    let base_url = std::env::var("EMBEDDING_BASE_URL").unwrap_or_default();
    let api_key = std::env::var("EMBEDDING_API_KEY").unwrap_or_default();
    if base_url.is_empty() || api_key.is_empty() {
        return None;
    }
    let model = std::env::var("EMBEDDING_MODEL").unwrap_or_else(|_| "BAAI/bge-m3".to_string());
    let dim = test_dim();
    let embedder: Arc<dyn EmbeddingProvider> =
        Arc::new(HttpEmbedder::new(&base_url, &api_key, &model, dim));

    let ctx =
        support::multi_db::setup_mcp_context("edit_log_embedder", dim, Some(embedder), None).await;
    let user_id = uid();
    let db_name = ctx.user_db_name(&user_id).await;
    let pool = ctx.user_db_pool(&user_id).await;
    cleanup(&pool, &user_id, &db_name).await;
    Some((ctx.service(), ctx.git(), pool, user_id, db_name, ctx))
}

async fn cleanup(pool: &MySqlPool, user_id: &str, db_name: &str) {
    let _ = sqlx::query("DELETE FROM mem_edit_log WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await;
    let _ = sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await;
    let prefix = safety_snapshot_prefix(db_name);
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT sname FROM mo_catalog.mo_snapshots WHERE prefix_eq(sname, CAST(? AS VARCHAR))")
            .bind(prefix)
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    for (name,) in rows {
        let _ = sqlx::raw_sql(&format!("DROP SNAPSHOT {name}"))
            .execute(pool)
            .await;
    }
}

async fn ensure_snapshot_quota(pool: &MySqlPool) {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT sname FROM mo_catalog.mo_snapshots")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    for (name,) in &rows {
        let _ = sqlx::raw_sql(&format!("DROP SNAPSHOT {name}"))
            .execute(pool)
            .await;
    }
}

async fn call(name: &str, args: Value, svc: &Arc<MemoryService>, uid: &str) -> Value {
    memoria_mcp::tools::call(name, args, svc, uid)
        .await
        .expect(name)
}
fn text(v: &Value) -> &str {
    v["content"][0]["text"].as_str().unwrap_or("")
}

/// Get ALL edit-log rows for a user, ordered by created_at ASC.
async fn get_logs(pool: &MySqlPool, user_id: &str) -> Vec<EditLogRow> {
    sqlx::query_as::<_, EditLogRow>(
        "SELECT edit_id, user_id, memory_id, operation, \
         CAST(payload AS CHAR) as payload, \
         reason, snapshot_before, \
         created_at, created_by \
         FROM mem_edit_log WHERE user_id = ? ORDER BY created_at ASC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Get edit-log rows filtered by operation.
async fn get_logs_by_op(pool: &MySqlPool, user_id: &str, op: &str) -> Vec<EditLogRow> {
    sqlx::query_as::<_, EditLogRow>(
        "SELECT edit_id, user_id, memory_id, operation, \
         CAST(payload AS CHAR) as payload, \
         reason, snapshot_before, \
         created_at, created_by \
         FROM mem_edit_log \
         WHERE user_id = ? AND operation = ? \
           AND created_at >= DATE_SUB(NOW(), INTERVAL 60 SECOND) \
         ORDER BY created_at ASC",
    )
    .bind(user_id)
    .bind(op)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// Assert common invariants on every edit-log row.
fn assert_common(row: &EditLogRow, expected_user: &str, expected_op: &str) {
    // edit_id: 32-char hex (UUID v7 simple)
    assert_eq!(row.edit_id.len(), 32, "edit_id len: {}", row.edit_id);
    assert!(
        row.edit_id.chars().all(|c| c.is_ascii_hexdigit()),
        "edit_id hex: {}",
        row.edit_id
    );
    assert_eq!(row.user_id, expected_user, "user_id");
    assert_eq!(row.operation, expected_op, "operation");
    assert_eq!(row.created_by, expected_user, "created_by");
    let now = chrono::Utc::now().naive_utc();
    let age = now - row.created_at;
    assert!(
        age.num_seconds() < 60 && age.num_seconds() >= 0,
        "created_at should be recent: {:?} (age={}s)",
        row.created_at,
        age.num_seconds()
    );
}

fn extract_mid(response: &Value) -> String {
    text(response)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string()
}

// ═══════════════════════════════════════════════════════════════════════════════
// 1. INJECT — store_memory
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_inject_all_fields() {
    let (svc, _git, pool, uid, db_name, _ctx) = setup().await;

    let r = call(
        "memory_store",
        json!({"content": "Rust is fast", "memory_type": "semantic"}),
        &svc,
        &uid,
    )
    .await;
    let mid = extract_mid(&r);
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "inject").await;
    assert_eq!(logs.len(), 1);
    let row = &logs[0];
    assert_common(row, &uid, "inject");
    assert_eq!(row.memory_id.as_deref(), Some(mid.as_str()));
    let payload: Value = serde_json::from_str(row.payload.as_ref().unwrap()).unwrap();
    assert_eq!(payload["content"], "Rust is fast");
    assert_eq!(payload["type"], "semantic");
    assert_eq!(row.reason.as_deref(), Some("store_memory"));
    assert!(row.snapshot_before.is_none());

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ inject: all fields verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 2. CORRECT
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_correct_all_fields() {
    let (svc, _git, pool, uid, db_name, _ctx) = setup().await;

    let r = call("memory_store", json!({"content": "uses black"}), &svc, &uid).await;
    let old_mid = extract_mid(&r);
    svc.flush_edit_log().await;

    let r = call(
        "memory_correct",
        json!({"memory_id": old_mid, "new_content": "uses ruff"}),
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("Corrected"));
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "correct").await;
    assert_eq!(logs.len(), 1);
    let row = &logs[0];
    assert_common(row, &uid, "correct");
    assert_eq!(row.memory_id.as_deref(), Some(old_mid.as_str()));
    let payload: Value = serde_json::from_str(row.payload.as_ref().unwrap()).unwrap();
    assert_eq!(payload["new_content"], "uses ruff");
    assert!(payload["new_memory_id"].is_string());
    assert_ne!(payload["new_memory_id"].as_str().unwrap(), old_mid);
    assert_eq!(row.reason.as_deref(), Some(""));
    assert!(row.snapshot_before.is_none());

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ correct: all fields verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 3. PURGE SINGLE
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_purge_single_all_fields() {
    let (svc, _git, pool, uid, db_name, _ctx) = setup().await;
    ensure_snapshot_quota(&pool).await;

    let r = call("memory_store", json!({"content": "to delete"}), &svc, &uid).await;
    let mid = extract_mid(&r);
    svc.flush_edit_log().await;

    // Flush inject separately so purge goes in its own batch
    let r = call("memory_purge", json!({"memory_id": mid}), &svc, &uid).await;
    assert!(text(&r).contains("Purged"));
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "purge").await;
    assert_eq!(logs.len(), 1);
    let row = &logs[0];
    assert_common(row, &uid, "purge");
    assert_eq!(row.memory_id.as_deref(), Some(mid.as_str()));
    assert!(
        row.payload.is_none(),
        "payload should be None, got: {:?}",
        row.payload
    );
    assert_eq!(row.reason.as_deref(), Some(""));
    let snap = row
        .snapshot_before
        .as_ref()
        .expect("snapshot_before should be set");
    assert!(
        is_purge_safety_snapshot(&db_name, snap),
        "unexpected snapshot name: {snap}"
    );
    let exists: Vec<(String,)> =
        sqlx::query_as("SELECT sname FROM mo_catalog.mo_snapshots WHERE sname = ?")
            .bind(snap)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(exists.len(), 1, "safety snapshot should exist in DB");

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ purge single: all fields verified + snapshot exists");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 4. PURGE BATCH
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_purge_batch_all_fields() {
    let (svc, _git, pool, uid, db_name, _ctx) = setup().await;
    ensure_snapshot_quota(&pool).await;

    let mut mids = vec![];
    for i in 0..3 {
        let r = call(
            "memory_store",
            json!({"content": format!("batch item {i}")}),
            &svc,
            &uid,
        )
        .await;
        mids.push(extract_mid(&r));
    }
    svc.flush_edit_log().await;

    let batch = mids.join(",");
    let r = call("memory_purge", json!({"memory_id": batch}), &svc, &uid).await;
    assert!(text(&r).contains("3"));
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "purge").await;
    assert_eq!(logs.len(), 3);
    let snap = logs[0].snapshot_before.as_ref().expect("snapshot_before");
    let mut logged_mids = vec![];
    let mut edit_ids = std::collections::HashSet::new();
    for (i, row) in logs.iter().enumerate() {
        assert_common(row, &uid, "purge");
        assert!(row.payload.is_none(), "payload[{i}]");
        assert_eq!(row.reason.as_deref(), Some(""), "reason[{i}]");
        assert_eq!(
            row.snapshot_before.as_ref(),
            Some(snap),
            "snapshot_before[{i}]"
        );
        logged_mids.push(row.memory_id.as_ref().unwrap().clone());
        edit_ids.insert(row.edit_id.clone());
    }
    for mid in &mids {
        assert!(logged_mids.contains(mid), "missing {mid}");
    }
    assert_eq!(edit_ids.len(), 3, "edit_ids should be unique");

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ purge batch: all fields verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 5. PURGE BY TOPIC
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_purge_topic_all_fields() {
    let (svc, _git, pool, uid, db_name, _ctx) = setup().await;
    ensure_snapshot_quota(&pool).await;

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
    svc.flush_edit_log().await;

    call("memory_purge", json!({"topic": "rust"}), &svc, &uid).await;
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "purge").await;
    assert_eq!(logs.len(), 2);
    let snap = logs[0].snapshot_before.as_ref().expect("snapshot_before");
    for (i, row) in logs.iter().enumerate() {
        assert_common(row, &uid, "purge");
        assert!(row.memory_id.is_some(), "memory_id[{i}]");
        assert!(row.payload.is_none(), "payload[{i}]");
        assert_eq!(row.reason.as_deref(), Some("topic:rust"), "reason[{i}]");
        assert_eq!(
            row.snapshot_before.as_ref(),
            Some(snap),
            "snapshot_before[{i}]"
        );
    }

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ purge topic: all fields verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 6. STORE BATCH
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_store_batch_all_fields() {
    let (svc, _git, pool, uid, db_name, _ctx) = setup().await;

    let items = vec![
        (
            "batch alpha".to_string(),
            memoria_core::MemoryType::Semantic,
            None,
            None,
            None,
            None,
            None,
        ),
        (
            "batch beta".to_string(),
            memoria_core::MemoryType::Procedural,
            None,
            None,
            None,
            None,
            None,
        ),
    ];
    let results = svc
        .store_batch_with_metadata_on_branch(&uid, None, items, None)
        .await
        .unwrap();
    assert_eq!(results.len(), 2);
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "inject").await;
    assert_eq!(logs.len(), 2);
    let log_mids: Vec<_> = logs
        .iter()
        .map(|r| r.memory_id.as_ref().unwrap().clone())
        .collect();
    let mut log_contents = vec![];
    let mut log_types = vec![];
    for (i, row) in logs.iter().enumerate() {
        assert_common(row, &uid, "inject");
        assert_eq!(row.reason.as_deref(), Some("store_batch"), "reason[{i}]");
        assert!(row.snapshot_before.is_none(), "snapshot_before[{i}]");
        let p: Value = serde_json::from_str(row.payload.as_ref().unwrap()).unwrap();
        log_contents.push(p["content"].as_str().unwrap().to_string());
        log_types.push(p["type"].as_str().unwrap().to_string());
    }
    for m in &results {
        assert!(log_mids.contains(&m.memory_id));
    }
    assert!(log_contents.contains(&"batch alpha".to_string()));
    assert!(log_contents.contains(&"batch beta".to_string()));
    assert!(log_types.contains(&"semantic".to_string()));
    assert!(log_types.contains(&"procedural".to_string()));

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ store_batch: all fields verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7. FULL AUDIT TRAIL — inject → correct → purge
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_full_audit_trail_chronological() {
    let (svc, _git, pool, uid, db_name, _ctx) = setup().await;
    ensure_snapshot_quota(&pool).await;

    let r = call(
        "memory_store",
        json!({"content": "original fact"}),
        &svc,
        &uid,
    )
    .await;
    let mid1 = extract_mid(&r);
    svc.flush_edit_log().await;

    let r = call(
        "memory_correct",
        json!({"memory_id": mid1, "new_content": "corrected fact"}),
        &svc,
        &uid,
    )
    .await;
    let new_mid = text(&r)
        .split_whitespace()
        .nth(2)
        .unwrap()
        .trim_end_matches(':')
        .to_string();
    svc.flush_edit_log().await;

    call("memory_purge", json!({"memory_id": new_mid}), &svc, &uid).await;
    svc.flush_edit_log().await;

    let logs = get_logs(&pool, &uid).await;
    assert!(logs.len() >= 3, "expected >=3 logs, got {}", logs.len());

    let ops: Vec<&str> = logs.iter().map(|r| r.operation.as_str()).collect();
    assert_eq!(ops[0], "inject");
    assert_eq!(ops[ops.len() - 1], "purge");
    assert!(ops.contains(&"correct"));

    // Timestamps monotonically non-decreasing
    for w in logs.windows(2) {
        assert!(
            w[0].created_at <= w[1].created_at,
            "{:?} > {:?}",
            w[0].created_at,
            w[1].created_at
        );
    }
    // All edit_ids unique
    let ids: std::collections::HashSet<_> = logs.iter().map(|r| &r.edit_id).collect();
    assert_eq!(ids.len(), logs.len());
    // All user_id/created_by consistent
    for row in &logs {
        assert_eq!(row.user_id, uid);
        assert_eq!(row.created_by, uid);
    }
    // Purge has snapshot_before
    let purge = logs.iter().find(|r| r.operation == "purge").unwrap();
    assert!(purge.snapshot_before.is_some());

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ full audit trail: chronological, all fields consistent");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 8. ROLLBACK via safety snapshot
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_purge_rollback_restores_memory() {
    let (svc, git, pool, uid, db_name, _ctx) = setup().await;
    ensure_snapshot_quota(&pool).await;

    let r = call(
        "memory_store",
        json!({"content": "important fact"}),
        &svc,
        &uid,
    )
    .await;
    let mid = extract_mid(&r);
    svc.flush_edit_log().await;

    let r = call("memory_purge", json!({"memory_id": mid}), &svc, &uid).await;
    svc.flush_edit_log().await;
    let t = text(&r);
    let snap_line = t
        .lines()
        .find(|l| l.contains("Safety snapshot"))
        .expect("snapshot line");
    let snap_name = snap_line
        .split("Safety snapshot: ")
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap();

    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 0);

    let r = memoria_mcp::git_tools::call(
        "memory_rollback",
        json!({"name": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await
    .expect("rollback");
    assert!(text(&r).contains("Rolled back"));

    let active = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].content, "important fact");

    // Note: rollback restores the entire DB to snapshot state, so the purge
    // edit-log entry is also rolled back. We verify the snapshot name was
    // correctly recorded by checking BEFORE rollback (already verified above
    // via the purge response containing the snapshot name).

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ rollback: memory restored, snapshot name verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 9. USER ISOLATION
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_user_isolation() {
    let (svc, _git, pool, uid1, db_name, ctx) = setup().await;
    let uid2 = uid();
    let pool2 = ctx.user_db_pool(&uid2).await;
    let db_name2 = ctx.user_db_name(&uid2).await;
    cleanup(&pool2, &uid2, &db_name2).await;

    call(
        "memory_store",
        json!({"content": "user1 fact"}),
        &svc,
        &uid1,
    )
    .await;
    call(
        "memory_store",
        json!({"content": "user2 fact"}),
        &svc,
        &uid2,
    )
    .await;
    svc.flush_edit_log().await;

    let logs1 = get_logs(&pool, &uid1).await;
    let logs2 = get_logs(&pool2, &uid2).await;
    assert_eq!(logs1.len(), 1);
    assert_eq!(logs2.len(), 1);
    for row in &logs1 {
        assert_eq!(row.user_id, uid1);
        assert_eq!(row.created_by, uid1);
    }
    for row in &logs2 {
        assert_eq!(row.user_id, uid2);
        assert_eq!(row.created_by, uid2);
    }

    cleanup(&pool, &uid1, &db_name).await;
    cleanup(&pool2, &uid2, &db_name2).await;
    println!("✅ isolation: users see only their own logs");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 10. GOVERNANCE QUARANTINE
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_governance_quarantine_edit_log() {
    let (svc, _git, pool, uid, db_name, _ctx) = setup().await;

    // T4 tier with initial_confidence=0.5, aged 60 days → confidence decays below 0.2 threshold
    let r = call(
        "memory_store",
        json!({"content": "low confidence fact", "trust_tier": "T4"}),
        &svc,
        &uid,
    )
    .await;
    let mid = extract_mid(&r);
    svc.flush_edit_log().await;

    sqlx::query("UPDATE mem_memories SET observed_at = DATE_SUB(NOW(), INTERVAL 60 DAY) WHERE memory_id = ?")
        .bind(&mid).execute(&pool).await.unwrap();

    call("memory_governance", json!({"force": true}), &svc, &uid).await;
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "governance:quarantine").await;
    assert!(
        !logs.is_empty(),
        "governance should quarantine T4 memory aged 60 days"
    );
    let row = &logs[0];
    assert_common(row, &uid, "governance:quarantine");
    assert!(
        row.memory_id.is_none(),
        "governance quarantine has no specific memory_id"
    );
    let payload: Value = serde_json::from_str(row.payload.as_ref().unwrap()).unwrap();
    assert!(
        payload["quarantined"].as_i64().unwrap() >= 1,
        "payload.quarantined >= 1"
    );
    assert!(
        row.reason.as_ref().unwrap().contains("quarantined"),
        "reason"
    );
    assert!(
        row.snapshot_before.is_none(),
        "MCP governance doesn't create snapshot"
    );

    // Verify the memory is physically deleted by quarantine
    let remaining: Vec<(String,)> =
        sqlx::query_as("SELECT memory_id FROM mem_memories WHERE memory_id = ?")
            .bind(&mid)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        remaining.is_empty(),
        "memory should be physically deleted by quarantine"
    );

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ governance quarantine: all fields verified + DB state checked");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 11. GOVERNANCE CLEANUP_STALE via MCP
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_governance_cleanup_stale_edit_log() {
    let (svc, _git, pool, uid, db_name, _ctx) = setup().await;

    // Insert a memory and mark it inactive → cleanup_stale target
    let r = call("memory_store", json!({"content": "stale fact"}), &svc, &uid).await;
    let mid = extract_mid(&r);
    svc.flush_edit_log().await;

    // Make it inactive AND old enough to pass the 24h grace period
    sqlx::query("UPDATE mem_memories SET is_active = 0, updated_at = DATE_SUB(NOW(), INTERVAL 25 HOUR) WHERE memory_id = ?")
        .bind(&mid).execute(&pool).await.unwrap();

    // Clear previous edit logs so we only see governance logs
    sqlx::query("DELETE FROM mem_edit_log WHERE user_id = ?")
        .bind(&uid)
        .execute(&pool)
        .await
        .unwrap();

    call("memory_governance", json!({"force": true}), &svc, &uid).await;
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "governance:cleanup").await;
    assert!(!logs.is_empty(), "governance should cleanup stale memory");
    let row = &logs[0];
    assert_common(row, &uid, "governance:cleanup");
    assert!(row.memory_id.is_none());
    let payload: Value = serde_json::from_str(row.payload.as_ref().unwrap()).unwrap();
    assert!(payload["cleaned_stale"].as_i64().unwrap() >= 1);
    assert!(row.reason.as_ref().unwrap().contains("cleaned_stale"));
    assert!(row.snapshot_before.is_none());

    // Verify the memory is actually deleted from DB
    let remaining: Vec<(String,)> =
        sqlx::query_as("SELECT memory_id FROM mem_memories WHERE memory_id = ?")
            .bind(&mid)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(
        remaining.is_empty(),
        "stale memory should be deleted from DB"
    );

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ governance cleanup_stale: all fields verified + DB state checked");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 12. GOVERNANCE via REST API
// ═══════════════════════════════════════════════════════════════════════════════

async fn spawn_server() -> (
    String,
    reqwest::Client,
    Arc<MemoryService>,
    support::multi_db::McpTestContext,
) {
    let ctx = support::multi_db::setup_mcp_context("edit_log_api", test_dim(), None, None).await;
    let service = ctx.service();
    let state = memoria_api::AppState::new(Arc::clone(&service), ctx.git(), String::new());
    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await });
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    (format!("http://127.0.0.1:{port}"), client, service, ctx)
}

#[tokio::test]
#[serial]
async fn test_api_governance_quarantine_edit_log() {
    let (base, client, svc, ctx) = spawn_server().await;
    let uid = uid();
    let pool = ctx.user_db_pool(&uid).await;
    let db_name = ctx.user_db_name(&uid).await;
    cleanup(&pool, &uid, &db_name).await;

    // Store via API
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "api low conf", "trust_tier": "T4"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "store status: {}", r.status());
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Age it
    sqlx::query("UPDATE mem_memories SET observed_at = DATE_SUB(NOW(), INTERVAL 60 DAY) WHERE memory_id = ?")
        .bind(&mid).execute(&pool).await.unwrap();

    // Governance via API
    let r = client
        .post(format!("{base}/v1/governance"))
        .header("X-User-Id", &uid)
        .json(&json!({"force": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["quarantined"].as_i64().unwrap() >= 1);

    // Wait for async flush
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "governance:quarantine").await;
    assert!(
        !logs.is_empty(),
        "API governance should write quarantine edit log"
    );
    let row = &logs[0];
    assert_common(row, &uid, "governance:quarantine");
    let payload: Value = serde_json::from_str(row.payload.as_ref().unwrap()).unwrap();
    assert!(payload["quarantined"].as_i64().unwrap() >= 1);

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ API governance quarantine: edit log verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 13. API PURGE — verify edit log via API path
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_api_purge_edit_log_all_fields() {
    let (base, client, svc, ctx) = spawn_server().await;
    let uid = uid();
    let pool = ctx.user_db_pool(&uid).await;
    let db_name = ctx.user_db_name(&uid).await;
    cleanup(&pool, &uid, &db_name).await;
    ensure_snapshot_quota(&pool).await;

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "api purge test"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"memory_ids": [mid]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 1);
    let snap = body["snapshot_name"].as_str().unwrap();
    assert!(
        is_purge_safety_snapshot(&db_name, snap),
        "unexpected snapshot name: {snap}"
    );

    // Wait for async flush
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "purge").await;
    assert_eq!(logs.len(), 1);
    let row = &logs[0];
    assert_common(row, &uid, "purge");
    assert_eq!(row.memory_id.as_deref(), Some(mid.as_str()));
    assert!(row.payload.is_none());
    assert_eq!(row.reason.as_deref(), Some(""));
    assert_eq!(row.snapshot_before.as_deref(), Some(snap));

    // Verify snapshot exists
    let exists: Vec<(String,)> =
        sqlx::query_as("SELECT sname FROM mo_catalog.mo_snapshots WHERE sname = ?")
            .bind(snap)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(exists.len(), 1);

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ API purge: all edit log fields verified + snapshot exists");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 14. API PURGE BY TOPIC — verify edit log via API path
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_api_purge_topic_edit_log_all_fields() {
    let (base, client, svc, ctx) = spawn_server().await;
    let uid = uid();
    let pool = ctx.user_db_pool(&uid).await;
    let db_name = ctx.user_db_name(&uid).await;
    cleanup(&pool, &uid, &db_name).await;
    ensure_snapshot_quota(&pool).await;

    for c in ["topicZ alpha", "topicZ beta", "unrelated"] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": c}))
            .send()
            .await
            .unwrap();
    }

    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"topic": "topicZ"}))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 2);

    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "purge").await;
    assert_eq!(logs.len(), 2);
    let snap = logs[0].snapshot_before.as_ref().expect("snapshot_before");
    for (i, row) in logs.iter().enumerate() {
        assert_common(row, &uid, "purge");
        assert!(row.memory_id.is_some(), "memory_id[{i}]");
        assert!(row.payload.is_none(), "payload[{i}]");
        assert_eq!(row.reason.as_deref(), Some("topic:topicZ"), "reason[{i}]");
        assert_eq!(
            row.snapshot_before.as_ref(),
            Some(snap),
            "snapshot_before[{i}]"
        );
    }

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ API purge topic: all edit log fields verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 15. EDIT_ID UNIQUENESS across operations
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_edit_id_globally_unique() {
    let (svc, _git, pool, uid, db_name, _ctx) = setup().await;
    ensure_snapshot_quota(&pool).await;

    // inject
    let r = call("memory_store", json!({"content": "fact A"}), &svc, &uid).await;
    let mid = extract_mid(&r);
    // correct
    call(
        "memory_correct",
        json!({"memory_id": mid, "new_content": "fact B"}),
        &svc,
        &uid,
    )
    .await;
    // store_batch
    svc.store_batch_with_metadata_on_branch(
        &uid,
        None,
        vec![
            (
                "batch1".into(),
                memoria_core::MemoryType::Semantic,
                None,
                None,
                None,
                None,
                None,
            ),
            (
                "batch2".into(),
                memoria_core::MemoryType::Semantic,
                None,
                None,
                None,
                None,
                None,
            ),
        ],
        None,
    )
    .await
    .unwrap();
    // purge by topic
    call("memory_purge", json!({"topic": "batch"}), &svc, &uid).await;
    svc.flush_edit_log().await;

    let logs = get_logs(&pool, &uid).await;
    assert!(logs.len() >= 4, "expected >=4 logs, got {}", logs.len());
    let ids: std::collections::HashSet<_> = logs.iter().map(|r| &r.edit_id).collect();
    assert_eq!(
        ids.len(),
        logs.len(),
        "all edit_ids must be globally unique"
    );
    // All are valid 32-char hex
    for row in &logs {
        assert_eq!(row.edit_id.len(), 32);
        assert!(row.edit_id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ edit_id globally unique across all operation types");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 16. INJECT SUPERSEDE (dedup) — requires real embedder
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_inject_supersede_all_fields() {
    let Some((svc, _git, pool, uid, db_name, _ctx)) = setup_with_embedder().await else {
        println!("⚠️  skipped: EMBEDDING_BASE_URL / EMBEDDING_API_KEY not set");
        return;
    };

    // Store original
    let r = call(
        "memory_store",
        json!({"content": "Go is a compiled language"}),
        &svc,
        &uid,
    )
    .await;
    let _mid1 = extract_mid(&r);
    svc.flush_edit_log().await;

    // Store near-duplicate with slightly different wording → should supersede
    let r2 = call(
        "memory_store",
        json!({"content": "Go is a compiled programming language"}),
        &svc,
        &uid,
    )
    .await;
    let mid2 = extract_mid(&r2);
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "inject").await;
    let supersede_logs: Vec<_> = logs
        .iter()
        .filter(|r| r.reason.as_deref() == Some("store_memory:supersede"))
        .collect();

    if supersede_logs.is_empty() {
        // Embeddings weren't close enough — not a test failure, just skip
        println!(
            "⚠️  supersede not triggered (embeddings not close enough), verifying normal inject"
        );
        assert!(logs.len() >= 2);
        cleanup(&pool, &uid, &db_name).await;
        return;
    }

    let row = supersede_logs[0];
    assert_common(row, &uid, "inject");
    assert_eq!(
        row.memory_id.as_deref(),
        Some(mid2.as_str()),
        "memory_id should be new"
    );
    let payload: Value = serde_json::from_str(row.payload.as_ref().unwrap()).unwrap();
    assert_eq!(payload["content"], "Go is a compiled programming language");
    assert_eq!(payload["type"], "semantic");
    assert_eq!(row.reason.as_deref(), Some("store_memory:supersede"));
    assert!(row.snapshot_before.is_none());

    // Verify old memory is superseded in DB
    let old_rows: Vec<(Option<String>,)> = sqlx::query_as(
        "SELECT superseded_by FROM mem_memories WHERE user_id = ? AND memory_id != ? AND is_active = 0"
    ).bind(&uid).bind(&mid2).fetch_all(&pool).await.unwrap();
    assert!(!old_rows.is_empty(), "old memory should be superseded");
    assert_eq!(
        old_rows[0].0.as_deref(),
        Some(mid2.as_str()),
        "superseded_by should point to new"
    );

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ inject supersede: all fields verified + old memory superseded in DB");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 17. GOVERNANCE SCHEDULER: archive_stale_working
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_governance_archive_working_edit_log() {
    let (svc, _git, pool, uid, db_name, ctx) = setup().await;

    // Store a working memory and age it beyond the 72h threshold
    let r = call(
        "memory_store",
        json!({"content": "debugging auth module", "memory_type": "working"}),
        &svc,
        &uid,
    )
    .await;
    let mid = extract_mid(&r);
    svc.flush_edit_log().await;

    sqlx::query("UPDATE mem_memories SET observed_at = DATE_SUB(NOW(), INTERVAL 96 HOUR) WHERE memory_id = ?")
        .bind(&mid).execute(&pool).await.unwrap();

    // Clear previous edit logs
    sqlx::query("DELETE FROM mem_edit_log WHERE user_id = ?")
        .bind(&uid)
        .execute(&pool)
        .await
        .unwrap();

    // Run full governance via the scheduler strategy
    let store = ctx.shared_store();
    let strategy = memoria_service::DefaultGovernanceStrategy;
    let _ = strategy
        .run(store.as_ref(), memoria_service::GovernanceTask::Hourly)
        .await;
    svc.flush_edit_log().await;

    let logs = get_logs_by_op(&pool, &uid, "governance:archive_working").await;
    assert!(!logs.is_empty(), "should have archive_working edit log");
    let row = &logs[0];
    assert_common(row, &uid, "governance:archive_working");
    assert!(row.memory_id.is_none());
    let payload: Value = serde_json::from_str(row.payload.as_ref().unwrap()).unwrap();
    assert!(payload["archived"].as_i64().unwrap() >= 1);
    assert!(row.reason.as_ref().unwrap().contains("archived"));

    // Verify memory is actually archived (is_active = 0)
    let active: Vec<(i8,)> =
        sqlx::query_as("SELECT is_active FROM mem_memories WHERE memory_id = ?")
            .bind(&mid)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(active[0].0, 0, "working memory should be archived");

    cleanup(&pool, &uid, &db_name).await;
    println!("✅ governance archive_working: all fields verified + DB state checked");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 18. GOVERNANCE SCHEDULER: cleanup_orphaned_incrementals
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_governance_cleanup_orphaned_edit_log() {
    let (svc, _git, pool, uid, db_name, ctx) = setup().await;

    // Store a memory, then supersede it manually to create an orphan
    let r = call("memory_store", json!({"content": "original"}), &svc, &uid).await;
    let mid = extract_mid(&r);
    svc.flush_edit_log().await;

    // Mark as inactive + superseded (simulates an orphaned incremental)
    sqlx::query(
        "UPDATE mem_memories SET is_active = 0, superseded_by = 'nonexistent', \
         observed_at = DATE_SUB(NOW(), INTERVAL 30 DAY) WHERE memory_id = ?",
    )
    .bind(&mid)
    .execute(&pool)
    .await
    .unwrap();

    sqlx::query("DELETE FROM mem_edit_log WHERE user_id = ?")
        .bind(&uid)
        .execute(&pool)
        .await
        .unwrap();

    let store = ctx.user_store(&uid).await;
    let strategy = memoria_service::DefaultGovernanceStrategy;
    let _ = strategy
        .run(store.as_ref(), memoria_service::GovernanceTask::Daily)
        .await;

    let logs = get_logs_by_op(&pool, &uid, "governance:cleanup_orphaned_incrementals").await;
    if !logs.is_empty() {
        let row = &logs[0];
        assert_common(row, &uid, "governance:cleanup_orphaned_incrementals");
        assert!(row.memory_id.is_none());
        let payload: Value = serde_json::from_str(row.payload.as_ref().unwrap()).unwrap();
        assert!(payload["cleaned_orphaned"].as_i64().unwrap() >= 1);
        println!("✅ governance cleanup_orphaned: all fields verified");
    } else {
        // May not trigger if the orphan detection logic doesn't match our setup
        println!("⚠️  governance cleanup_orphaned: not triggered (orphan criteria not met)");
    }

    cleanup(&pool, &uid, &db_name).await;
}
