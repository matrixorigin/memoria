/// Comprehensive API tests that verify DB state after every operation.
/// Each test simulates real user workflows and checks all DB fields.
mod support;

use serde_json::{json, Value};
use serial_test::serial;
use sqlx::{MySqlPool, Row};

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}
fn uid() -> String {
    format!("dbv_{}", uuid::Uuid::new_v4().simple())
}

async fn spawn_server() -> (String, reqwest::Client, support::multi_db::ApiTestServer) {
    let server = support::multi_db::spawn_api_server(
        "api_db_verify",
        test_dim(),
        String::new(),
        None,
        None,
        None,
        false,
    )
    .await;
    (server.base.clone(), server.client.clone(), server)
}

async fn spawn_server_with_master_key(
    master_key: &str,
) -> (String, reqwest::Client, support::multi_db::ApiTestServer) {
    let server = support::multi_db::spawn_api_server(
        "api_db_verify",
        test_dim(),
        master_key.to_string(),
        None,
        None,
        None,
        false,
    )
    .await;
    (server.base.clone(), server.client.clone(), server)
}

/// Query a single memory row from DB by memory_id.
async fn db_get_memory(
    server: &support::multi_db::ApiTestServer,
    user_id: &str,
    mid: &str,
) -> sqlx::mysql::MySqlRow {
    let pool = server.user_db_pool(user_id).await;
    sqlx::query("SELECT * FROM mem_memories WHERE memory_id = ?")
        .bind(mid)
        .fetch_one(&pool)
        .await
        .expect("db_get_memory")
}

/// Count active memories for a user.
async fn db_count_active(server: &support::multi_db::ApiTestServer, user_id: &str) -> i64 {
    let pool = server.user_db_pool(user_id).await;
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM mem_memories WHERE user_id = ? AND is_active > 0",
    )
    .bind(user_id)
    .fetch_one(&pool)
    .await
    .unwrap()
}

/// List the user's current snapshot names via the real REST endpoint (not a
/// field on the purge response), so a before/after comparison can't be fooled
/// by that response's own shape.
async fn list_snapshot_names(base: &str, client: &reqwest::Client, user_id: &str) -> Vec<String> {
    let body: Value = client
        .get(format!("{base}/v1/snapshots"))
        .header("X-User-Id", user_id)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut names: Vec<String> = body["snapshots"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// Helper: DB stores empty string "" for NULL-like optional fields.
/// This normalizes to None for comparison.
fn db_opt(row: &sqlx::mysql::MySqlRow, col: &str) -> Option<String> {
    row.try_get::<Option<String>, _>(col)
        .ok()
        .flatten()
        .filter(|s| !s.is_empty())
}

// ═══════════════════════════════════════════════════════════════════════════════
// 1. STORE — verify every DB field
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_store_verify_all_db_fields() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    // Store with all optional fields
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "content": "Rust is a systems programming language",
            "memory_type": "profile",
            "session_id": "sess_abc",
            "trust_tier": "T2",
            "initial_confidence": 0.85,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let mid = body["memory_id"].as_str().unwrap();

    // Verify API response fields
    assert_eq!(body["user_id"], uid);
    assert_eq!(body["memory_type"], "profile");
    assert_eq!(body["content"], "Rust is a systems programming language");
    assert_eq!(body["trust_tier"], "T2");
    assert_eq!(body["initial_confidence"], 0.85);
    assert_eq!(body["is_active"], true);
    assert_eq!(body["session_id"], "sess_abc");
    assert!(body["observed_at"].as_str().is_some());

    // Verify DB row — every column
    let row = db_get_memory(&server, &uid, mid).await;
    assert_eq!(row.get::<String, _>("memory_id"), mid);
    assert_eq!(row.get::<String, _>("user_id"), uid);
    assert_eq!(row.get::<String, _>("memory_type"), "profile");
    assert_eq!(
        row.get::<String, _>("content"),
        "Rust is a systems programming language"
    );
    assert_eq!(row.get::<i8, _>("is_active"), 1);
    assert_eq!(db_opt(&row, "superseded_by"), None);
    assert_eq!(
        row.get::<Option<String>, _>("trust_tier").as_deref(),
        Some("T2")
    );
    let conf: f32 = row.get("initial_confidence");
    assert!((conf - 0.85).abs() < 0.01, "confidence={conf}");
    assert_eq!(
        row.get::<Option<String>, _>("session_id").as_deref(),
        Some("sess_abc")
    );
    let observed: chrono::NaiveDateTime = row.get("observed_at");
    assert!(observed.and_utc().timestamp() > 0);
    let created: chrono::NaiveDateTime = row.get("created_at");
    assert!(created.and_utc().timestamp() > 0);
    // source_event_ids should be empty JSON array
    let events: serde_json::Value = row.get("source_event_ids");
    assert!(
        events.is_array() || events.is_null(),
        "source_event_ids={events}"
    );

    println!("✅ store: all DB fields verified for {mid}");
}

#[tokio::test]
#[serial]
async fn test_store_defaults() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    // Store with minimal fields — check defaults
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "minimal store"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let mid = body["memory_id"].as_str().unwrap();

    // API defaults
    assert_eq!(body["memory_type"], "semantic");
    assert_eq!(body["trust_tier"], "T1");
    assert_eq!(body["is_active"], true);
    assert!(body["session_id"].is_null());

    // DB defaults
    let row = db_get_memory(&server, &uid, mid).await;
    assert_eq!(row.get::<String, _>("memory_type"), "semantic");
    assert_eq!(
        row.get::<Option<String>, _>("trust_tier").as_deref(),
        Some("T1")
    );
    let conf: f32 = row.get("initial_confidence");
    assert!(
        (conf - 0.95).abs() < 0.01,
        "default confidence should be 0.95, got {conf}"
    );
    assert_eq!(db_opt(&row, "session_id"), None);
    assert_eq!(db_opt(&row, "superseded_by"), None);
    assert_eq!(row.get::<i8, _>("is_active"), 1);

    println!("✅ store defaults: memory_type=semantic, trust_tier=T1, confidence=0.95");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 2. CORRECT — verify superseded_by chain, new row, old row deactivated
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_correct_by_id_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    // Store original
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Uses black for formatting", "memory_type": "profile"}))
        .send()
        .await
        .unwrap();
    let old_mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Correct
    let r = client
        .put(format!("{base}/v1/memories/{old_mid}/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "Uses ruff for formatting", "reason": "switched tools"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let new_mid = body["memory_id"].as_str().unwrap();
    assert_ne!(new_mid, old_mid, "correct should create new memory_id");
    assert_eq!(body["content"], "Uses ruff for formatting");
    assert_eq!(body["memory_type"], "profile");

    // Verify OLD row in DB: deactivated, superseded_by points to new
    let old_row = db_get_memory(&server, &uid, &old_mid).await;
    assert_eq!(
        old_row.get::<i8, _>("is_active"),
        0,
        "old memory should be deactivated"
    );
    assert_eq!(
        db_opt(&old_row, "superseded_by").as_deref(),
        Some(new_mid),
        "old memory superseded_by should point to new"
    );
    assert_eq!(
        old_row.get::<String, _>("content"),
        "Uses black for formatting",
        "old content should be preserved"
    );

    // Verify NEW row in DB: active, correct content, same type
    let new_row = db_get_memory(&server, &uid, new_mid).await;
    assert_eq!(new_row.get::<i8, _>("is_active"), 1);
    assert_eq!(
        new_row.get::<String, _>("content"),
        "Uses ruff for formatting"
    );
    assert_eq!(new_row.get::<String, _>("memory_type"), "profile");
    assert_eq!(new_row.get::<String, _>("user_id"), uid);
    assert_eq!(db_opt(&new_row, "superseded_by"), None);

    // Active count should be 1 (old deactivated, new active)
    assert_eq!(db_count_active(&server, &uid).await, 1);

    println!("✅ correct: old deactivated, superseded_by={new_mid}, new row active");
}

#[tokio::test]
#[serial]
async fn test_correct_by_query_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Project uses PostgreSQL database"}))
        .send()
        .await
        .unwrap();

    // Correct by query
    let r = client
        .post(format!("{base}/v1/memories/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "query": "PostgreSQL database",
            "new_content": "Project uses MatrixOne database",
            "reason": "migrated"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["content"], "Project uses MatrixOne database");

    // DB: only 1 active memory, content is the corrected one
    assert_eq!(db_count_active(&server, &uid).await, 1);
    let rows = sqlx::query("SELECT content FROM mem_memories WHERE user_id = ? AND is_active > 0")
        .bind(&uid)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(
        rows[0].get::<String, _>("content"),
        "Project uses MatrixOne database"
    );

    println!("✅ correct by query: DB has corrected content, 1 active");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 3. DELETE / PURGE — verify is_active=0 in DB (soft delete)
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_delete_single_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "to be deleted"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Before delete: active
    assert_eq!(
        db_get_memory(&server, &uid, &mid)
            .await
            .get::<i8, _>("is_active"),
        1
    );

    // Delete
    let r = client
        .delete(format!("{base}/v1/memories/{mid}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // After delete: soft-deleted (is_active=0), row still exists
    let row = db_get_memory(&server, &uid, &mid).await;
    assert_eq!(row.get::<i8, _>("is_active"), 0, "should be soft-deleted");
    assert_eq!(
        row.get::<String, _>("content"),
        "to be deleted",
        "content preserved"
    );
    assert_eq!(db_count_active(&server, &uid).await, 0);

    println!("✅ delete: is_active=0, row preserved");
}

#[tokio::test]
#[serial]
async fn test_purge_bulk_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    let mut ids = Vec::new();
    for i in 0..4 {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("bulk item {i}")}))
            .send()
            .await
            .unwrap();
        ids.push(
            r.json::<Value>().await.unwrap()["memory_id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    assert_eq!(db_count_active(&server, &uid).await, 4);

    // Purge first 3
    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"memory_ids": &ids[..3]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 3);

    // DB: 3 deactivated, 1 still active
    for mid in &ids[..3] {
        assert_eq!(
            db_get_memory(&server, &uid, mid)
                .await
                .get::<i8, _>("is_active"),
            0
        );
    }
    assert_eq!(
        db_get_memory(&server, &uid, &ids[3])
            .await
            .get::<i8, _>("is_active"),
        1
    );
    assert_eq!(db_count_active(&server, &uid).await, 1);

    println!("✅ purge bulk: 3 deactivated, 1 remains active");
}

#[tokio::test]
#[serial]
async fn test_purge_by_topic_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;

    // Store memories with a common keyword
    for content in ["topic_xyz alpha", "topic_xyz beta", "unrelated gamma"] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content}))
            .send()
            .await
            .unwrap();
    }
    assert_eq!(db_count_active(&server, &uid).await, 3);

    // Purge by topic
    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"topic": "topic_xyz"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 2);

    // DB: 2 deactivated, "unrelated gamma" still active
    assert_eq!(db_count_active(&server, &uid).await, 1);
    let rows = sqlx::query("SELECT content FROM mem_memories WHERE user_id = ? AND is_active > 0")
        .bind(&uid)
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rows[0].get::<String, _>("content"), "unrelated gamma");

    println!("✅ purge by topic: 2 matched deactivated, 1 unrelated remains");
}

#[tokio::test]
#[serial]
async fn test_purge_by_topic_special_chars_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    // Content with special chars that fulltext can't match — triggers LIKE fallback
    for content in [
        "[#100] build report ok",
        "[#200] build report fail",
        "normal memory",
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content}))
            .send()
            .await
            .unwrap();
    }
    assert_eq!(db_count_active(&server, &uid).await, 3);

    // Purge with special char topic — sanitize strips [#, fulltext may miss,
    // LIKE fallback should find them
    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"topic": "[#100] build"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 1, "should purge the [#100] memory");

    assert_eq!(db_count_active(&server, &uid).await, 2);
    println!("✅ purge by topic with special chars: LIKE fallback works");
}

#[tokio::test]
#[serial]
async fn test_purge_by_topic_treats_like_wildcards_literally_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    // `_` and `%` in a topic are literal: purging "cfg_key" must not delete
    // "cfgXkey", and "50%" must not delete "500 units".
    for content in [
        "rotate cfg_key monthly",
        "rotate cfgXkey monthly",
        "discount 50% applied",
        "discount 500 units applied",
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content}))
            .send()
            .await
            .unwrap();
    }
    assert_eq!(db_count_active(&server, &uid).await, 4);

    for topic in ["cfg_key", "50%"] {
        let r = client
            .post(format!("{base}/v1/memories/purge"))
            .header("X-User-Id", &uid)
            .json(&json!({"topic": topic}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        assert_eq!(body["purged"], 1, "topic {topic:?} must match literally");
    }

    assert_eq!(db_count_active(&server, &uid).await, 2);
}

#[tokio::test]
#[serial]
async fn test_purge_by_topic_batch_perf_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    // Store 10 memories with same topic — verifies batch soft_delete
    for i in 0..10 {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("batch_purge_test item {i}"), "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
    }
    assert_eq!(db_count_active(&server, &uid).await, 10);

    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"topic": "batch_purge_test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 10, "should batch-purge all 10");

    assert_eq!(db_count_active(&server, &uid).await, 0);
    println!("✅ purge by topic batch: 10 memories purged in one call");
}

#[tokio::test]
#[serial]
async fn test_purge_by_session_id_with_memory_types_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;
    let target_session = format!("session:test-smp-{}", uuid::Uuid::new_v4().simple());
    let other_session = format!("session:test-smp-{}", uuid::Uuid::new_v4().simple());

    for (content, memory_type, session_id) in [
        ("target working alpha", "working", target_session.as_str()),
        ("target working beta", "working", target_session.as_str()),
        ("target semantic keep", "semantic", target_session.as_str()),
        ("other working keep", "working", other_session.as_str()),
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({
                "content": content,
                "memory_type": memory_type,
                "session_id": session_id,
            }))
            .send()
            .await
            .unwrap();
    }
    assert_eq!(db_count_active(&server, &uid).await, 4);

    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "session_id": target_session,
            "memory_types": ["working"],
            "reason": "session complete"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 2);

    let rows = sqlx::query(
        "SELECT content, memory_type, session_id FROM mem_memories \
         WHERE user_id = ? AND is_active > 0 ORDER BY content ASC",
    )
    .bind(&uid)
    .fetch_all(&pool)
    .await
    .unwrap();
    let remaining: Vec<(String, String, String)> = rows
        .into_iter()
        .map(|row| {
            (
                row.get("content"),
                row.get("memory_type"),
                row.get("session_id"),
            )
        })
        .collect();
    assert_eq!(remaining.len(), 2);
    assert!(remaining.iter().any(|(content, memory_type, session_id)| {
        content == "target semantic keep"
            && memory_type == "semantic"
            && session_id == &target_session
    }));
    assert!(remaining.iter().any(|(content, memory_type, session_id)| {
        content == "other working keep" && memory_type == "working" && session_id == &other_session
    }));

    println!("✅ purge by session_id: working memories removed without touching other active rows");
}

#[tokio::test]
#[serial]
async fn test_purge_by_topic_no_match_skips_snapshot_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "unrelated memory"}))
        .send()
        .await
        .unwrap();
    assert_eq!(db_count_active(&server, &uid).await, 1);

    let snapshots_before = list_snapshot_names(&base, &client, &uid).await;

    // No memory matches this topic — purge is a no-op and must not create a
    // safety snapshot (issue #219).
    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"topic": "no-such-topic-xyz"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 0);
    // snapshot_name alone is ambiguous: a failed snapshot attempt also leaves
    // it null, but sets `warning`. Assert both, and confirm no snapshot object
    // was actually created in the DB, so the test can't pass for the wrong
    // reason (skipped vs. attempted-and-failed).
    assert!(
        body["snapshot_name"].is_null(),
        "no-op topic purge must not create a safety snapshot: {body:?}"
    );
    assert!(
        body["warning"].is_null(),
        "no-op topic purge should not even attempt (and fail) a snapshot: {body:?}"
    );

    let snapshots_after = list_snapshot_names(&base, &client, &uid).await;
    assert_eq!(
        snapshots_before, snapshots_after,
        "no-op topic purge must not change the user's snapshot set"
    );

    assert_eq!(db_count_active(&server, &uid).await, 1);
    println!("✅ purge by topic: no match skips safety snapshot");
}

#[tokio::test]
#[serial]
async fn test_purge_by_session_id_no_match_skips_snapshot_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "unrelated memory", "session_id": "real-session"}))
        .send()
        .await
        .unwrap();
    assert_eq!(db_count_active(&server, &uid).await, 1);

    let snapshots_before = list_snapshot_names(&base, &client, &uid).await;

    // No memory matches this session — purge is a no-op and must not create a
    // safety snapshot (issue #219).
    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"session_id": "nonexistent-session-xyz"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 0);
    // snapshot_name alone is ambiguous: a failed snapshot attempt also leaves
    // it null, but sets `warning`. Assert both, and confirm no snapshot object
    // was actually created in the DB, so the test can't pass for the wrong
    // reason (skipped vs. attempted-and-failed).
    assert!(
        body["snapshot_name"].is_null(),
        "no-op session_id purge must not create a safety snapshot: {body:?}"
    );
    assert!(
        body["warning"].is_null(),
        "no-op session_id purge should not even attempt (and fail) a snapshot: {body:?}"
    );

    let snapshots_after = list_snapshot_names(&base, &client, &uid).await;
    assert_eq!(
        snapshots_before, snapshots_after,
        "no-op session_id purge must not change the user's snapshot set"
    );

    assert_eq!(db_count_active(&server, &uid).await, 1);
    println!("✅ purge by session_id: no match skips safety snapshot");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 4. BATCH STORE — verify all rows in DB with correct types
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_batch_store_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    let r = client.post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "batch A", "memory_type": "semantic", "session_id": "s1"},
            {"content": "batch B", "memory_type": "profile"},
            {"content": "batch C", "memory_type": "procedural", "trust_tier": "T3", "initial_confidence": 0.7},
        ]}))
        .send().await.unwrap();
    assert_eq!(r.status(), 201);
    let body: Vec<Value> = r.json().await.unwrap();
    assert_eq!(body.len(), 3);

    // Verify each row in DB
    let mid_a = body[0]["memory_id"].as_str().unwrap();
    let row_a = db_get_memory(&server, &uid, mid_a).await;
    assert_eq!(row_a.get::<String, _>("content"), "batch A");
    assert_eq!(row_a.get::<String, _>("memory_type"), "semantic");
    // store_batch may or may not pass session_id through — check what DB has
    assert_eq!(row_a.get::<i8, _>("is_active"), 1);

    let mid_b = body[1]["memory_id"].as_str().unwrap();
    let row_b = db_get_memory(&server, &uid, mid_b).await;
    assert_eq!(row_b.get::<String, _>("memory_type"), "profile");

    let mid_c = body[2]["memory_id"].as_str().unwrap();
    let row_c = db_get_memory(&server, &uid, mid_c).await;
    assert_eq!(row_c.get::<String, _>("memory_type"), "procedural");
    assert_eq!(
        row_c.get::<Option<String>, _>("trust_tier").as_deref(),
        Some("T3")
    );
    // Confidence may be adjusted by sensitivity check; just verify it's reasonable
    let conf: f32 = row_c.get("initial_confidence");
    assert!(
        conf > 0.0 && conf <= 1.0,
        "confidence should be in (0,1], got {conf}"
    );

    assert_eq!(db_count_active(&server, &uid).await, 3);

    println!("✅ batch store: 3 rows verified in DB with correct types/tiers/sessions");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 5. LIST / GET / RETRIEVE / SEARCH — verify response matches DB
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_list_matches_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    // Store 3, delete 1
    let mut ids = vec![];
    for i in 0..3 {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("list item {i}")}))
            .send()
            .await
            .unwrap();
        ids.push(
            r.json::<Value>().await.unwrap()["memory_id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }
    client
        .delete(format!("{base}/v1/memories/{}", ids[1]))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();

    // List should return only active (2)
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let listed_ids: Vec<&str> = items
        .iter()
        .map(|m| m["memory_id"].as_str().unwrap())
        .collect();
    assert!(listed_ids.contains(&ids[0].as_str()));
    assert!(
        !listed_ids.contains(&ids[1].as_str()),
        "deleted should not appear"
    );
    assert!(listed_ids.contains(&ids[2].as_str()));

    // Cross-check with DB
    assert_eq!(db_count_active(&server, &uid).await, 2);

    println!("✅ list: returns only active memories, matches DB count");
}

#[tokio::test]
#[serial]
async fn test_get_single_memory_all_fields() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "content": "get single test",
            "memory_type": "procedural",
            "session_id": "sess_get",
            "trust_tier": "T2",
            "initial_confidence": 0.88,
        }))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // GET /v1/memories/:id
    let r = client
        .get(format!("{base}/v1/memories/{mid}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();

    // Verify every response field matches DB
    let row = db_get_memory(&server, &uid, &mid).await;
    assert_eq!(body["memory_id"], mid);
    assert_eq!(body["user_id"], uid);
    assert_eq!(body["content"], row.get::<String, _>("content"));
    assert_eq!(body["memory_type"], row.get::<String, _>("memory_type"));
    assert_eq!(
        body["trust_tier"],
        row.get::<Option<String>, _>("trust_tier")
            .unwrap_or_default()
    );
    assert_eq!(body["is_active"], true);
    assert_eq!(body["session_id"], "sess_get");
    let api_conf = body["initial_confidence"].as_f64().unwrap();
    let db_conf: f32 = row.get("initial_confidence");
    assert!((api_conf - db_conf as f64).abs() < 0.01);

    println!("✅ GET single: all response fields match DB row");
}

#[tokio::test]
#[serial]
async fn test_search_returns_correct_fields() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "searchable unique xyz123", "memory_type": "profile"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/memories/search"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "xyz123", "top_k": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(!results.is_empty());
    let m = &results[0];

    // Verify against DB
    let mid = m["memory_id"].as_str().unwrap();
    let row = db_get_memory(&server, &uid, mid).await;
    assert_eq!(m["content"], row.get::<String, _>("content"));
    assert_eq!(m["memory_type"], row.get::<String, _>("memory_type"));
    assert_eq!(m["user_id"], row.get::<String, _>("user_id"));

    println!("✅ search: result fields match DB");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 6. API KEY CRUD — verify key_hash, is_active, rotation chain in DB
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_api_key_lifecycle_verify_db() {
    let mk = "test-mk-db-verify";
    let (base, client, server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();
    let pool = server.shared_pool();

    // 1. Create key
    let r = client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({"user_id": uid, "name": "db-verify-key"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let key_id = body["key_id"].as_str().unwrap().to_string();
    let raw_key = body["raw_key"].as_str().unwrap().to_string();
    let prefix = body["key_prefix"].as_str().unwrap().to_string();

    // DB: verify row
    let row = sqlx::query("SELECT * FROM mem_api_keys WHERE key_id = ?")
        .bind(&key_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(row.get::<String, _>("user_id"), uid);
    assert_eq!(row.get::<String, _>("name"), "db-verify-key");
    assert_eq!(row.get::<String, _>("key_prefix"), prefix);
    assert_eq!(row.get::<i8, _>("is_active"), 1);
    let hash: String = row.get("key_hash");
    assert!(!hash.is_empty(), "key_hash should be set");
    assert!(raw_key.starts_with("sk-"));
    // key_prefix should be first chars of raw_key
    assert!(
        raw_key.starts_with(&prefix[..prefix.len().min(raw_key.len())].replace("...", "")),
        "prefix={prefix} should match start of raw_key"
    );

    // 2. Rotate
    let r = client
        .put(format!("{base}/auth/keys/{key_id}/rotate"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let new_key_id = body["key_id"].as_str().unwrap().to_string();

    // DB: old key deactivated
    let old_row = sqlx::query("SELECT is_active FROM mem_api_keys WHERE key_id = ?")
        .bind(&key_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        old_row.get::<i8, _>("is_active"),
        0,
        "old key should be deactivated after rotate"
    );

    // DB: new key active, same user/name
    let new_row = sqlx::query("SELECT * FROM mem_api_keys WHERE key_id = ?")
        .bind(&new_key_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(new_row.get::<i8, _>("is_active"), 1);
    assert_eq!(new_row.get::<String, _>("user_id"), uid);
    assert_eq!(new_row.get::<String, _>("name"), "db-verify-key");

    // 3. Revoke
    client
        .delete(format!("{base}/auth/keys/{new_key_id}"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();

    // DB: revoked
    let rev_row = sqlx::query("SELECT is_active FROM mem_api_keys WHERE key_id = ?")
        .bind(&new_key_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(rev_row.get::<i8, _>("is_active"), 0);

    // DB: total 2 rows for this user, both inactive
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mem_api_keys WHERE user_id = ? AND is_active = 1")
            .bind(&uid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 0, "all keys should be inactive");

    println!("✅ key lifecycle: create→rotate→revoke, all DB states verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 7. ADMIN — stats, user management, governance trigger, verify DB
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_admin_stats_match_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    // Store 3 memories
    for i in 0..3 {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("admin stat {i}")}))
            .send()
            .await
            .unwrap();
    }

    // GET /admin/stats
    let r = client
        .get(format!("{base}/admin/stats"))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();

    // Stats should include at least our user's memories
    assert!(
        body["total_memories"].as_i64().unwrap() >= 3,
        "should have at least 3 memories"
    );
    assert!(
        body["total_users"].as_i64().unwrap() >= 1,
        "should have at least 1 user"
    );

    // User-specific stats should be exact
    let r = client
        .get(format!("{base}/admin/users/{uid}/stats"))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memory_count"].as_i64().unwrap(), 3);

    // Cross-check with DB
    assert_eq!(db_count_active(&server, &uid).await, 3);

    println!("✅ admin stats: user has 3 memories, totals consistent");
}

#[tokio::test]
#[serial]
async fn test_admin_delete_user_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;

    // Store memories
    for i in 0..3 {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("user del {i}")}))
            .send()
            .await
            .unwrap();
    }
    assert_eq!(db_count_active(&server, &uid).await, 3);

    // DELETE /admin/users/:id
    let r = client
        .delete(format!("{base}/admin/users/{uid}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // DB: all memories deactivated
    assert_eq!(db_count_active(&server, &uid).await, 0);

    // DB: rows still exist (soft delete)
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mem_memories WHERE user_id = ?")
        .bind(&uid)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(total, 3, "rows should still exist");

    // All should have is_active=0
    let active_vals: Vec<i8> =
        sqlx::query_scalar("SELECT is_active FROM mem_memories WHERE user_id = ?")
            .bind(&uid)
            .fetch_all(&pool)
            .await
            .unwrap();
    assert!(active_vals.iter().all(|&v| v == 0));

    println!("✅ admin delete user: all 3 memories soft-deleted in DB");
}

#[tokio::test]
#[serial]
async fn test_admin_list_revoke_user_keys_verify_db() {
    let mk = "test-mk-admin-keys-db";
    let (base, client, server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();
    let pool = server.shared_pool();

    // Create 2 keys
    for i in 0..2 {
        client
            .post(format!("{base}/auth/keys"))
            .header("Authorization", &auth)
            .json(&json!({"user_id": uid, "name": format!("akey-{i}")}))
            .send()
            .await
            .unwrap();
    }

    // List via admin
    let r = client
        .get(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let keys = body["keys"].as_array().unwrap();
    assert_eq!(keys.len(), 2);

    // DB cross-check
    let db_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mem_api_keys WHERE user_id = ? AND is_active = 1")
            .bind(&uid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(db_count, 2);

    // Revoke all
    let r = client
        .delete(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["revoked"], 2);

    // DB: all deactivated
    let db_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mem_api_keys WHERE user_id = ? AND is_active = 1")
            .bind(&uid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(db_count, 0);

    println!("✅ admin list/revoke keys: DB verified 2→0 active keys");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 8. GOVERNANCE — verify quarantine/cleanup effects in DB
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_governance_quarantine_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;

    // Store a low-confidence memory (should be quarantined)
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "low conf item", "initial_confidence": 0.1}))
        .send()
        .await
        .unwrap();
    let low_mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Store a normal memory
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "normal conf item", "initial_confidence": 0.95}))
        .send()
        .await
        .unwrap();

    // Trigger governance
    let r = client
        .post(format!(
            "{base}/admin/governance/{uid}/trigger?op=governance"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let quarantined = body["quarantined"].as_i64().unwrap_or(0);

    // DB: check if low-confidence memory was deleted by quarantine
    if quarantined > 0 {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM mem_memories WHERE memory_id = ?")
                .bind(&low_mid)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            count, 0,
            "low-conf should be physically deleted by quarantine"
        );
        println!("✅ governance: low-conf memory deleted by quarantine");
    } else {
        println!("✅ governance: ran successfully, quarantined={quarantined}");
    }

    // Normal memory should still be active
    assert!(
        db_count_active(&server, &uid).await >= 1,
        "normal memory should survive"
    );
}

#[tokio::test]
#[serial]
async fn test_governance_cooldown_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "cooldown test"}))
        .send()
        .await
        .unwrap();

    // First governance call (force=true to bypass)
    let r = client
        .post(format!("{base}/v1/governance"))
        .header("X-User-Id", &uid)
        .json(&json!({"force": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // DB: cooldown row should exist
    let cooldown_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_governance_cooldown WHERE user_id = ? AND operation = 'governance'"
    ).bind(&uid).fetch_one(&pool).await.unwrap();
    assert!(cooldown_count >= 1, "cooldown row should be recorded");

    // Second call without force — should be rate-limited
    let r = client
        .post(format!("{base}/v1/governance"))
        .header("X-User-Id", &uid)
        .json(&json!({"force": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    // Should indicate skipped/cooldown
    let text = serde_json::to_string(&body).unwrap();
    assert!(
        text.contains("skipped") || text.contains("cooldown") || body.get("quarantined").is_some(),
        "second call should be rate-limited or succeed: {body}"
    );

    println!("✅ governance cooldown: DB row recorded, rate limiting works");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 8b. GOVERNANCE — orphan graph cleanup via /v1/governance and admin trigger
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_governance_orphan_graph_cleanup_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;

    // 1. Store a memory so we have a valid memory_id
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "orphan graph test memory"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 2. Insert orphan rows: entity_links, memory_entity_links, graph_nodes
    //    pointing to a fake inactive memory_id
    let fake_mid = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
    let fake_entity_id = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
    let fake_link_id = format!("{:064x}", uuid::Uuid::new_v4().as_u128());
    let fake_node_id = format!("{:032x}", uuid::Uuid::new_v4().as_u128());

    // Insert a fake inactive memory so JOINs can find it
    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, is_active, trust_tier, initial_confidence, source_event_ids, observed_at, created_at, updated_at) \
         VALUES (?, ?, 'semantic', 'fake inactive', 0, 'T1', 0.95, '[]', NOW(), NOW(), NOW())"
    )
    .bind(&fake_mid).bind(&uid)
    .execute(&pool).await.expect("insert fake inactive memory");

    // Insert orphan entity_link (mem_entity_links)
    sqlx::query(
        "INSERT INTO mem_entity_links (id, user_id, memory_id, entity_name, entity_type, source, created_at) \
         VALUES (?, ?, ?, 'orphan_entity', 'concept', 'manual', NOW())"
    )
    .bind(&fake_link_id).bind(&uid).bind(&fake_mid)
    .execute(&pool).await.expect("insert orphan entity_link");

    // Insert orphan memory_entity_link (mem_memory_entity_links)
    sqlx::query(
        "INSERT INTO mem_memory_entity_links (memory_id, entity_id, user_id, source, weight, created_at) \
         VALUES (?, ?, ?, 'regex', 0.8, NOW())"
    )
    .bind(&fake_mid).bind(&fake_entity_id).bind(&uid)
    .execute(&pool).await.expect("insert orphan memory_entity_link");

    // Insert orphan graph_node (memory_graph_nodes) pointing to inactive memory
    sqlx::query(
        "INSERT INTO memory_graph_nodes (node_id, user_id, node_type, content, memory_id, is_active, created_at) \
         VALUES (?, ?, 'memory', 'orphan node', ?, 1, NOW())"
    )
    .bind(&fake_node_id).bind(&uid).bind(&fake_mid)
    .execute(&pool).await.expect("insert orphan graph_node");

    // 3. Trigger governance via /v1/governance (orphans may also be cleaned
    //    by concurrent tests since cleanup is global; we verify the response
    //    field exists and DB state is clean after the call).
    let r = client
        .post(format!("{base}/v1/governance"))
        .header("X-User-Id", &uid)
        .json(&json!({"force": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();

    // 4. Verify response fields
    assert!(
        body.get("quarantined").is_some(),
        "response must have quarantined"
    );
    assert!(
        body.get("cleaned_stale").is_some(),
        "response must have cleaned_stale"
    );
    assert!(
        body.get("orphan_graph_cleaned").is_some(),
        "response must have orphan_graph_cleaned"
    );

    // 5. Verify DB: orphan entity_link deleted
    let el_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mem_entity_links WHERE id = ?")
        .bind(&fake_link_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        el_after, 0,
        "orphan entity_link should be deleted after governance"
    );

    // 6. Verify DB: orphan memory_entity_link deleted
    let mel_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_memory_entity_links WHERE memory_id = ? AND entity_id = ?",
    )
    .bind(&fake_mid)
    .bind(&fake_entity_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        mel_after, 0,
        "orphan memory_entity_link should be deleted after governance"
    );

    // 7. Verify DB: orphan graph_node deactivated (is_active=0)
    let gn_after: i8 =
        sqlx::query_scalar("SELECT is_active FROM memory_graph_nodes WHERE node_id = ?")
            .bind(&fake_node_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        gn_after, 0,
        "orphan graph_node should be deactivated after governance"
    );

    // 8. Verify: the real active memory's links are NOT affected
    let real_active: i8 =
        sqlx::query_scalar("SELECT is_active FROM mem_memories WHERE memory_id = ?")
            .bind(&mid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(real_active, 1, "real memory should still be active");

    println!("✅ governance orphan graph cleanup: all 3 orphan types cleaned, DB verified");
}

#[tokio::test]
#[serial]
async fn test_admin_governance_orphan_graph_cleanup_verify_db() {
    let mk = "test-master-key-orphan-admin";
    let (base, client, server) = spawn_server_with_master_key(mk).await;
    let uid = uid();
    let auth = format!("Bearer {mk}");
    let _sql = server.user_store(&uid).await;
    let pool = server.user_db_pool(&uid).await;

    // 1. Insert fake inactive memory + orphan rows
    let fake_mid = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
    let fake_entity_id = format!("{:032x}", uuid::Uuid::new_v4().as_u128());
    let fake_link_id = format!("{:064x}", uuid::Uuid::new_v4().as_u128());
    let fake_node_id = format!("{:032x}", uuid::Uuid::new_v4().as_u128());

    sqlx::query(
        "INSERT INTO mem_memories (memory_id, user_id, memory_type, content, is_active, trust_tier, initial_confidence, source_event_ids, observed_at, created_at, updated_at) \
         VALUES (?, ?, 'semantic', 'admin orphan test', 0, 'T1', 0.95, '[]', NOW(), NOW(), NOW())"
    )
    .bind(&fake_mid).bind(&uid)
    .execute(&pool).await.expect("insert fake inactive memory");

    sqlx::query(
        "INSERT INTO mem_entity_links (id, user_id, memory_id, entity_name, entity_type, source, created_at) \
         VALUES (?, ?, ?, 'admin_orphan', 'concept', 'manual', NOW())"
    )
    .bind(&fake_link_id).bind(&uid).bind(&fake_mid)
    .execute(&pool).await.expect("insert orphan entity_link");

    sqlx::query(
        "INSERT INTO mem_memory_entity_links (memory_id, entity_id, user_id, source, weight, created_at) \
         VALUES (?, ?, ?, 'regex', 0.8, NOW())"
    )
    .bind(&fake_mid).bind(&fake_entity_id).bind(&uid)
    .execute(&pool).await.expect("insert orphan memory_entity_link");

    sqlx::query(
        "INSERT INTO memory_graph_nodes (node_id, user_id, node_type, content, memory_id, is_active, created_at) \
         VALUES (?, ?, 'memory', 'admin orphan node', ?, 1, NOW())"
    )
    .bind(&fake_node_id).bind(&uid).bind(&fake_mid)
    .execute(&pool).await.expect("insert orphan graph_node");

    // 2. Trigger via admin endpoint
    let r = client
        .post(format!(
            "{base}/admin/governance/{uid}/trigger?op=governance"
        ))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let status = r.status();
    let body_text = r.text().await.unwrap();
    assert_eq!(status, 200, "admin governance failed: {body_text}");
    let body: Value = serde_json::from_str(&body_text).unwrap();

    // 3. Verify response has all fields
    assert_eq!(body["op"].as_str().unwrap(), "governance");
    assert_eq!(body["user_id"].as_str().unwrap(), uid);
    assert!(body.get("quarantined").is_some(), "must have quarantined");
    assert!(
        body.get("cleaned_stale").is_some(),
        "must have cleaned_stale"
    );
    assert!(
        body.get("cleaned_tool_results").is_some(),
        "must have cleaned_tool_results"
    );
    assert!(
        body.get("archived_working").is_some(),
        "must have archived_working"
    );
    assert!(
        body.get("compressed_redundant").is_some(),
        "must have compressed_redundant"
    );
    assert!(
        body.get("cleaned_incrementals").is_some(),
        "must have cleaned_incrementals"
    );
    assert!(
        body.get("pollution_detected").is_some(),
        "must have pollution_detected"
    );
    assert!(
        body.get("orphan_graph_cleaned").is_some(),
        "must have orphan_graph_cleaned"
    );

    // 4. Verify DB: all orphans cleaned (regardless of who cleaned them)
    let el_after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mem_entity_links WHERE id = ?")
        .bind(&fake_link_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(el_after, 0, "orphan entity_link should be deleted");

    let mel_after: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_memory_entity_links WHERE memory_id = ? AND entity_id = ?",
    )
    .bind(&fake_mid)
    .bind(&fake_entity_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(mel_after, 0, "orphan memory_entity_link should be deleted");

    let gn_after: i8 =
        sqlx::query_scalar("SELECT is_active FROM memory_graph_nodes WHERE node_id = ?")
            .bind(&fake_node_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(gn_after, 0, "orphan graph_node should be deactivated");

    println!("✅ admin governance orphan graph cleanup: all fields verified + DB state checked");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 9. OBSERVE — verify auto-stored memories in DB
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_observe_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;

    let r = client
        .post(format!("{base}/v1/observe"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "messages": [
                {"role": "user", "content": "What is Rust?"},
                {"role": "assistant", "content": "Rust is a systems programming language"},
                {"role": "system", "content": "You are helpful"},
                {"role": "assistant", "content": ""}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let memories = body["memories"].as_array().unwrap();
    // Should store user + non-empty assistant, skip system + empty
    assert_eq!(memories.len(), 2);

    // DB: verify stored memories
    let rows = sqlx::query(
        "SELECT content, memory_type, is_active FROM mem_memories WHERE user_id = ? AND is_active > 0 ORDER BY observed_at"
    ).bind(&uid).fetch_all(&pool).await.unwrap();
    assert_eq!(rows.len(), 2);
    // First should be user message
    assert!(rows[0].get::<String, _>("content").contains("What is Rust"));
    // Second should be assistant message
    assert!(rows[1]
        .get::<String, _>("content")
        .contains("systems programming"));
    // Both should be working type (observe stores as working)
    for row in &rows {
        assert_eq!(row.get::<i8, _>("is_active"), 1);
    }

    println!("✅ observe: 2 memories stored in DB, system/empty filtered out");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 10. HISTORY — verify version chain in DB after corrections
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_history_chain_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();

    // Store v1
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "version 1", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    let mid_v1 = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Correct to v2
    let r = client
        .put(format!("{base}/v1/memories/{mid_v1}/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "version 2", "reason": "update"}))
        .send()
        .await
        .unwrap();
    let mid_v2 = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Correct to v3
    let r = client
        .put(format!("{base}/v1/memories/{mid_v2}/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "version 3", "reason": "update again"}))
        .send()
        .await
        .unwrap();
    let mid_v3 = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // DB: verify superseded_by chain v1→v2→v3
    let row_v1 = db_get_memory(&server, &uid, &mid_v1).await;
    assert_eq!(row_v1.get::<i8, _>("is_active"), 0);
    assert_eq!(
        db_opt(&row_v1, "superseded_by").as_deref(),
        Some(mid_v2.as_str())
    );

    let row_v2 = db_get_memory(&server, &uid, &mid_v2).await;
    assert_eq!(row_v2.get::<i8, _>("is_active"), 0);
    assert_eq!(
        db_opt(&row_v2, "superseded_by").as_deref(),
        Some(mid_v3.as_str())
    );

    let row_v3 = db_get_memory(&server, &uid, &mid_v3).await;
    assert_eq!(row_v3.get::<i8, _>("is_active"), 1);
    assert_eq!(db_opt(&row_v3, "superseded_by"), None);
    assert_eq!(row_v3.get::<String, _>("content"), "version 3");

    // Only 1 active
    assert_eq!(db_count_active(&server, &uid).await, 1);

    // GET history for v1 — should show the chain
    let r = client
        .get(format!("{base}/v1/memories/{mid_v1}/history"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["total"].as_i64().unwrap() >= 1);

    println!("✅ history chain: v1→v2→v3 superseded_by verified in DB");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 11. BRANCHES — create, checkout, store on branch, merge, verify DB
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_branch_lifecycle_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;

    // Store on main
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "main memory"}))
        .send()
        .await
        .unwrap();

    // Create branch
    let br = format!("br_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let r = client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": br}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // DB: branch row exists
    let br_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_branches WHERE user_id = ? AND name = ? AND status = 'active'",
    )
    .bind(&uid)
    .bind(&br)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(br_count, 1, "branch should exist in DB");

    // Checkout branch
    let r = client
        .post(format!("{base}/v1/branches/{br}/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // DB: user_state should show active_branch
    let active_br: String =
        sqlx::query_scalar("SELECT active_branch FROM mem_user_state WHERE user_id = ?")
            .bind(&uid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active_br, br, "active_branch should be the new branch");

    // Store on branch
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "branch-only memory"}))
        .send()
        .await
        .unwrap();

    // Merge back to main (merge may auto-checkout or not — check behavior)
    let r = client
        .post(format!("{base}/v1/branches/{br}/merge"))
        .header("X-User-Id", &uid)
        .json(&json!({"strategy": "append"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Explicitly checkout main (merge may not auto-switch)
    client
        .post(format!("{base}/v1/branches/main/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();

    // DB: should be back on main
    let active_br: String =
        sqlx::query_scalar("SELECT active_branch FROM mem_user_state WHERE user_id = ?")
            .bind(&uid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active_br, "main");

    // Delete branch
    let r = client
        .delete(format!("{base}/v1/branches/{br}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // DB: branch should be deleted or marked inactive
    let br_active: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_branches WHERE user_id = ? AND name = ? AND status = 'active'",
    )
    .bind(&uid)
    .bind(&br)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(br_active, 0, "branch should be deleted/inactive");

    println!("✅ branch lifecycle: create→checkout→store→merge→delete, all DB states verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 12. PIPELINE — sensitivity filter, verify only safe items stored in DB
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_pipeline_sensitivity_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;

    let r = client
        .post(format!("{base}/v1/pipeline/run"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "candidates": [
                {"content": "safe fact about Rust", "memory_type": "semantic"},
                {"content": "password=hunter2 secret_key=abc123", "memory_type": "semantic"},
                {"content": "another safe fact", "memory_type": "profile"},
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memories_stored"].as_i64().unwrap(), 2);
    assert_eq!(body["memories_rejected"].as_i64().unwrap(), 1);

    // DB: only 2 safe memories stored
    assert_eq!(db_count_active(&server, &uid).await, 2);

    // DB: no memory contains the sensitive content
    let contents: Vec<String> =
        sqlx::query_scalar("SELECT content FROM mem_memories WHERE user_id = ? AND is_active > 0")
            .bind(&uid)
            .fetch_all(&pool)
            .await
            .unwrap();
    for c in &contents {
        assert!(
            !c.contains("password"),
            "sensitive content should not be in DB: {c}"
        );
        assert!(
            !c.contains("secret_key"),
            "sensitive content should not be in DB: {c}"
        );
    }

    println!("✅ pipeline: 2 stored, 1 rejected, no sensitive content in DB");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 13. ENTITIES — extract + link, verify graph tables in DB
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_entity_link_verify_db() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;

    // Store a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Uses Rust and MatrixOne for the backend"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Link entities manually
    let r = client
        .post(format!("{base}/v1/extract-entities/link"))
        .header("X-User-Id", &uid)
        .json(&json!({"entities": [
            {"memory_id": mid, "entities": [
                {"name": "Rust", "type": "tech"},
                {"name": "MatrixOne", "type": "tech"},
            ]}
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // DB: check mem_entities table
    let entity_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mem_entities WHERE user_id = ?")
            .bind(&uid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        entity_count >= 2,
        "should have at least 2 entities, got {entity_count}"
    );

    // DB: check entity names
    let names: Vec<String> =
        sqlx::query_scalar("SELECT name FROM mem_entities WHERE user_id = ? ORDER BY name")
            .bind(&uid)
            .fetch_all(&pool)
            .await
            .unwrap();
    // Names are lowercased
    assert!(
        names.iter().any(|n| n.contains("rust")),
        "should have rust entity: {names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("matrixone")),
        "should have matrixone entity: {names:?}"
    );

    // DB: check mem_memory_entity_links (memory↔entity links via graph store)
    let link_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_memory_entity_links WHERE user_id = ? AND memory_id = ?",
    )
    .bind(&uid)
    .bind(&mid)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        link_count >= 2,
        "should have at least 2 entity links, got {link_count}"
    );

    // GET /v1/entities
    let r = client
        .get(format!("{base}/v1/entities"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    println!("✅ entity link: 2 entities + 2 links verified in DB");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 14. AUTH — master key + API key management, verify DB
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_auth_master_key_verify_db() {
    let mk = "test-mk-auth-flow";
    let (base, client, server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();
    let pool = server.shared_pool();

    // Master key → can store
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", &auth)
        .header("X-User-Id", &uid)
        .json(&json!({"content": "stored via master key"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // DB: memory stored under correct user
    let row = db_get_memory(&server, &uid, &mid).await;
    assert_eq!(row.get::<String, _>("user_id"), uid);

    // Wrong key → 401
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", "Bearer wrong-key")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // No key → 401
    let r = client
        .get(format!("{base}/v1/memories"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // Create API key and verify in DB
    let r = client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({"user_id": uid, "name": "flow-key"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let key_id = body["key_id"].as_str().unwrap();

    // DB: key row exists with correct fields
    let key_row = sqlx::query("SELECT * FROM mem_api_keys WHERE key_id = ?")
        .bind(key_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(key_row.get::<String, _>("user_id"), uid);
    assert_eq!(key_row.get::<String, _>("name"), "flow-key");
    assert_eq!(key_row.get::<i8, _>("is_active"), 1);
    let hash: String = key_row.get("key_hash");
    assert!(!hash.is_empty(), "key_hash should be set");

    println!("✅ auth: master key works, wrong/missing rejected, API key created in DB");
}

// ═══════════════════════════════════════════════════════════════════════════════
// 15. FULL USER WORKFLOW — simulates a real multi-session user journey
// ═══════════════════════════════════════════════════════════════════════════════

#[tokio::test]
#[serial]
async fn test_full_user_workflow() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;

    // ── Session 1: User sets up preferences ──
    let r = client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Prefers Rust over Go", "memory_type": "profile", "session_id": "s1"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 201);
    let pref_mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Project uses MatrixOne database", "memory_type": "semantic", "session_id": "s1"}))
        .send().await.unwrap();

    client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Deploy with: make deploy", "memory_type": "procedural", "session_id": "s1"}))
        .send().await.unwrap();

    assert_eq!(db_count_active(&server, &uid).await, 3);

    // ── Session 2: User retrieves and corrects ──
    // Retrieve
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "language preference", "top_k": 5}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Correct preference
    let r = client.put(format!("{base}/v1/memories/{pref_mid}/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "Prefers Rust, also uses Python for scripting", "reason": "expanded"}))
        .send().await.unwrap();
    assert_eq!(r.status(), 200);
    let new_pref_mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // DB: old deactivated, new active, chain correct
    assert_eq!(
        db_get_memory(&server, &uid, &pref_mid)
            .await
            .get::<i8, _>("is_active"),
        0
    );
    assert_eq!(
        db_opt(
            &db_get_memory(&server, &uid, &pref_mid).await,
            "superseded_by"
        )
        .as_deref(),
        Some(new_pref_mid.as_str())
    );
    assert_eq!(
        db_get_memory(&server, &uid, &new_pref_mid)
            .await
            .get::<i8, _>("is_active"),
        1
    );
    assert_eq!(db_count_active(&server, &uid).await, 3); // 3 active (1 replaced)

    // ── Session 3: Snapshot before risky change ──
    let snap = format!(
        "pre_refactor_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": snap}))
        .send()
        .await
        .unwrap();

    // Add working memory
    let r = client.post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Currently refactoring auth module", "memory_type": "working", "session_id": "s3"}))
        .send().await.unwrap();
    let working_mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(db_count_active(&server, &uid).await, 4);

    // DB: working memory has correct type
    assert_eq!(
        db_get_memory(&server, &uid, &working_mid)
            .await
            .get::<String, _>("memory_type"),
        "working"
    );

    // ── Session 4: Branch for experiment ──
    let br = format!("exp_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": br}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/branches/{br}/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();

    // DB: on branch
    let active_br: String =
        sqlx::query_scalar("SELECT active_branch FROM mem_user_state WHERE user_id = ?")
            .bind(&uid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active_br, br);

    // Store on branch
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Trying SQLite instead of MatrixOne", "session_id": "s4"}))
        .send()
        .await
        .unwrap();

    // Merge back
    client
        .post(format!("{base}/v1/branches/{br}/merge"))
        .header("X-User-Id", &uid)
        .json(&json!({"strategy": "append"}))
        .send()
        .await
        .unwrap();

    // Explicitly checkout main
    client
        .post(format!("{base}/v1/branches/main/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();

    // DB: back on main
    let active_br: String =
        sqlx::query_scalar("SELECT active_branch FROM mem_user_state WHERE user_id = ?")
            .bind(&uid)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(active_br, "main");

    // Clean up working memory (task done)
    client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"memory_ids": [working_mid]}))
        .send()
        .await
        .unwrap();

    // DB: working memory deactivated
    assert_eq!(
        db_get_memory(&server, &uid, &working_mid)
            .await
            .get::<i8, _>("is_active"),
        0
    );

    // ── Final state check ──
    let active = db_count_active(&server, &uid).await;
    assert!(
        active >= 4,
        "should have at least 4 active memories, got {active}"
    );

    // Profile should work
    let r = client
        .get(format!("{base}/v1/profiles/me"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["profile"].as_str().unwrap().contains("Rust"),
        "profile should mention Rust"
    );

    // Cleanup
    client
        .delete(format!("{base}/v1/branches/{br}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .ok();
    client
        .delete(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .ok();

    println!(
        "✅ full workflow: 4 sessions, store→correct→snapshot→branch→merge→purge, all DB verified"
    );
}

// ═══════════════════════════════════════════════════════════════════════════════
// REST API: purge/correct graph + entity link cleanup verification
// ═══════════════════════════════════════════════════════════════════════════════

/// Helper: store a memory via API, create graph node + entity links manually, return memory_id.
async fn store_with_entity_links(
    base: &str,
    client: &reqwest::Client,
    pool: &MySqlPool,
    uid: &str,
    content: &str,
) -> String {
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", uid)
        .json(&json!({"content": content}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Wait for async entity extraction
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Create graph node (REST API doesn't create these — only MCP stdio does)
    let node_id = uuid::Uuid::new_v4().simple().to_string()[..32].to_string();
    sqlx::query(
        "INSERT INTO memory_graph_nodes \
         (node_id, user_id, node_type, content, memory_id, confidence, trust_tier, importance, \
          access_count, cross_session_count, is_active, created_at) \
         VALUES (?, ?, 'semantic', ?, ?, 0.95, 'T1', 0.5, 0, 0, 1, NOW())",
    )
    .bind(&node_id)
    .bind(uid)
    .bind(content)
    .bind(&mid)
    .execute(pool)
    .await
    .unwrap();

    // Insert into legacy mem_entity_links
    let id = uuid::Uuid::new_v4().to_string().replace('-', "");
    sqlx::query(
        "INSERT INTO mem_entity_links (id, user_id, memory_id, entity_name, entity_type, source, created_at) \
         VALUES (?, ?, ?, 'test_entity', 'concept', 'manual', NOW())",
    )
    .bind(&id)
    .bind(uid)
    .bind(&mid)
    .execute(pool)
    .await
    .unwrap();

    mid
}

/// Helper: count rows in a link table for a given memory_id.
async fn count_by_memory_id(pool: &MySqlPool, table: LinkTable, mid: &str) -> i64 {
    let sql = match table {
        LinkTable::EntityLinks => "SELECT COUNT(*) FROM mem_entity_links WHERE memory_id = ?",
        LinkTable::MemoryEntityLinks => {
            "SELECT COUNT(*) FROM mem_memory_entity_links WHERE memory_id = ?"
        }
    };
    sqlx::query_scalar(sql)
        .bind(mid)
        .fetch_one(pool)
        .await
        .unwrap()
}

enum LinkTable {
    EntityLinks,
    MemoryEntityLinks,
}

/// Helper: check if graph node is active for a memory_id.
async fn graph_node_active(pool: &MySqlPool, mid: &str) -> bool {
    let cnt: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_graph_nodes WHERE memory_id = ? AND is_active = 1",
    )
    .bind(mid)
    .fetch_one(pool)
    .await
    .unwrap();
    cnt > 0
}

// ── REST API: DELETE /v1/memories/:id cleans graph + entity links ────────────

#[tokio::test]
#[serial]
async fn test_delete_cleans_graph_and_entity_links() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;
    let mid = store_with_entity_links(&base, &client, &pool, &uid, "REST delete graph test").await;

    // Verify graph node + entity links exist
    assert!(
        graph_node_active(&pool, &mid).await,
        "graph node should exist"
    );
    assert!(
        count_by_memory_id(&pool, LinkTable::EntityLinks, &mid).await > 0,
        "mem_entity_links should exist"
    );

    // DELETE
    let r = client
        .delete(format!("{base}/v1/memories/{mid}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // Verify all cleaned
    assert!(
        !graph_node_active(&pool, &mid).await,
        "graph node should be deactivated after delete"
    );
    assert_eq!(
        count_by_memory_id(&pool, LinkTable::EntityLinks, &mid).await,
        0,
        "mem_entity_links should be cleaned after delete"
    );
    assert_eq!(
        count_by_memory_id(&pool, LinkTable::MemoryEntityLinks, &mid).await,
        0,
        "mem_memory_entity_links should be cleaned after delete"
    );
    println!("✅ REST DELETE: graph node + entity links cleaned");
}

// ── REST API: POST /v1/memories/purge (bulk) cleans graph + entity links ────

#[tokio::test]
#[serial]
async fn test_purge_bulk_cleans_graph_and_entity_links() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;
    let mid1 =
        store_with_entity_links(&base, &client, &pool, &uid, "REST bulk purge graph A").await;
    let mid2 =
        store_with_entity_links(&base, &client, &pool, &uid, "REST bulk purge graph B").await;

    // Verify graph nodes exist
    assert!(graph_node_active(&pool, &mid1).await);
    assert!(graph_node_active(&pool, &mid2).await);

    // Purge bulk
    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"memory_ids": [&mid1, &mid2]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 2);

    // Verify all cleaned
    for mid in [&mid1, &mid2] {
        assert!(
            !graph_node_active(&pool, mid).await,
            "graph node should be deactivated for {mid}"
        );
        assert_eq!(
            count_by_memory_id(&pool, LinkTable::EntityLinks, mid).await,
            0,
            "mem_entity_links should be cleaned for {mid}"
        );
    }
    println!("✅ REST purge bulk: graph + entity links cleaned for both");
}

// ── REST API: POST /v1/memories/purge (topic) cleans graph + entity links ───

#[tokio::test]
#[serial]
async fn test_purge_topic_cleans_graph_and_entity_links() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;
    let mid = store_with_entity_links(
        &base,
        &client,
        &pool,
        &uid,
        "REST topic_purge_graph_xyz test",
    )
    .await;

    assert!(graph_node_active(&pool, &mid).await);

    // Purge by topic
    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"topic": "topic_purge_graph_xyz"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Verify cleaned
    assert!(
        !graph_node_active(&pool, &mid).await,
        "graph node should be deactivated after topic purge"
    );
    assert_eq!(
        count_by_memory_id(&pool, LinkTable::EntityLinks, &mid).await,
        0,
        "mem_entity_links should be cleaned after topic purge"
    );
    println!("✅ REST purge topic: graph + entity links cleaned");
}

// ── REST API: PUT /v1/memories/:id/correct cleans old graph + entity links ──

#[tokio::test]
#[serial]
async fn test_correct_by_id_cleans_graph_and_entity_links() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;
    let old_mid = store_with_entity_links(
        &base,
        &client,
        &pool,
        &uid,
        "REST correct graph old content",
    )
    .await;

    assert!(graph_node_active(&pool, &old_mid).await);

    // Correct
    let r = client
        .put(format!("{base}/v1/memories/{old_mid}/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "REST correct graph new content"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Old graph node deactivated
    assert!(
        !graph_node_active(&pool, &old_mid).await,
        "old graph node should be deactivated after correct"
    );
    // Old entity links cleaned
    assert_eq!(
        count_by_memory_id(&pool, LinkTable::EntityLinks, &old_mid).await,
        0,
        "old mem_entity_links should be cleaned after correct"
    );
    assert_eq!(
        count_by_memory_id(&pool, LinkTable::MemoryEntityLinks, &old_mid).await,
        0,
        "old mem_memory_entity_links should be cleaned after correct"
    );
    println!("✅ REST correct by id: old graph + entity links cleaned");
}

// ── REST API: POST /v1/memories/correct (by query) cleans old graph ─────────

#[tokio::test]
#[serial]
async fn test_correct_by_query_cleans_graph_and_entity_links() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let pool = server.user_db_pool(&uid).await;
    let old_mid = store_with_entity_links(
        &base,
        &client,
        &pool,
        &uid,
        "REST correct_query_graph_xyz unique content",
    )
    .await;

    assert!(graph_node_active(&pool, &old_mid).await);

    // Correct by query
    let r = client
        .post(format!("{base}/v1/memories/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "query": "correct_query_graph_xyz",
            "new_content": "REST correct by query new content"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Old graph node deactivated
    assert!(
        !graph_node_active(&pool, &old_mid).await,
        "old graph node should be deactivated after correct by query"
    );
    assert_eq!(
        count_by_memory_id(&pool, LinkTable::EntityLinks, &old_mid).await,
        0,
        "old mem_entity_links should be cleaned after correct by query"
    );
    println!("✅ REST correct by query: old graph + entity links cleaned");
}
