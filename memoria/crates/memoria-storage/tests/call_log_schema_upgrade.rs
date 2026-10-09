use memoria_core::interfaces::MemoryStore;
use memoria_storage::store::CURRENT_USER_SCHEMA_VERSION;
use memoria_storage::{DbRouter, SqlMemoryStore};
use sqlx::{mysql::MySqlPoolOptions, MySqlPool, Row};
use uuid::Uuid;

struct LegacyFixture {
    router: DbRouter,
    pool: MySqlPool,
    user: String,
    user_url: String,
    shared_db: String,
    user_db: String,
}

impl LegacyFixture {
    async fn cleanup(self) {
        self.router.invalidate_user(&self.user).await;
        self.pool.close().await;
        // Only databases owned by this fixture are removed.
        for db in [&self.user_db, &self.shared_db] {
            sqlx::query(&format!("DROP DATABASE IF EXISTS `{db}`"))
                .execute(self.router.global_user_pool())
                .await
                .expect("clean up fixture database");
        }
        self.router.global_user_pool().close().await;
        self.router.shared_pool().close().await;
    }
}

async fn legacy_fixture(version: i64, partial: bool) -> LegacyFixture {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".into());
    let (base, _) = url.rsplit_once('/').expect("database URL");
    let shared_db = format!("call_log_upgrade_{}", Uuid::new_v4().simple());
    let shared_url = format!("{base}/{shared_db}");
    let router = DbRouter::connect(&shared_url, 1024, Uuid::new_v4().to_string())
        .await
        .expect("connect router");
    let user = format!("log_upgrade_{}", Uuid::new_v4().simple());
    router.user_store(&user).await.expect("create user schema");
    let db = router.user_db_name(&user).await.expect("user database");
    let user_url = format!("{base}/{db}");
    let pool = MySqlPoolOptions::new()
        .max_connections(4)
        .connect(&user_url)
        .await
        .expect("connect user pool");

    // Recreate the actual pre-#258 table, including an existing history row.
    sqlx::query("DROP TABLE mem_api_call_log")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "CREATE TABLE mem_api_call_log (
            id BIGINT NOT NULL AUTO_INCREMENT PRIMARY KEY,
            user_id VARCHAR(64) NOT NULL,
            method VARCHAR(10) NOT NULL DEFAULT '',
            path VARCHAR(256) NOT NULL,
            status_code SMALLINT NOT NULL DEFAULT 0,
            latency_ms INT NOT NULL DEFAULT 0,
            called_at DATETIME(6) NOT NULL DEFAULT NOW(6),
            rpc_success TINYINT(1) NOT NULL DEFAULT 1,
            rpc_error_code INT NULL,
            INDEX idx_user_called (user_id, called_at)
        )",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mem_api_call_log (user_id, method, path, status_code, latency_ms)
         VALUES (?, 'GET', '/historical', 200, 17)",
    )
    .bind(&user)
    .execute(&pool)
    .await
    .unwrap();
    if partial {
        sqlx::query("ALTER TABLE mem_api_call_log ADD COLUMN tool_success TINYINT(1) NULL")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE mem_api_call_log SET tool_success = 0 WHERE path = '/historical'")
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO mem_memories
         (memory_id, user_id, memory_type, content, initial_confidence, trust_tier,
          source_event_ids, extra_metadata, observed_at, created_at)
         VALUES ('preserved-memory', ?, 'semantic', 'Keep existing memory content', 0.8, 'T3',
          '[]', '{}', NOW(6), NOW(6))",
    )
    .bind(&user)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE mem_schema_meta SET schema_version = ?, updated_at = '2000-01-01 00:00:00'
         WHERE schema_key = 'user_schema'",
    )
    .bind(version)
    .execute(&pool)
    .await
    .unwrap();
    router.invalidate_user(&user).await;
    LegacyFixture {
        router,
        pool,
        user,
        user_url,
        shared_db,
        user_db: db,
    }
}

async fn verify_usable_history(f: &LegacyFixture, partial: bool) {
    let columns = sqlx::query(
        "SELECT column_name, is_nullable FROM information_schema.columns
         WHERE table_schema = DATABASE() AND table_name = 'mem_api_call_log'
           AND column_name IN ('tool_success', 'tool_error_kind')",
    )
    .fetch_all(&f.pool)
    .await
    .unwrap();
    assert_eq!(columns.len(), 2);
    assert!(columns
        .iter()
        .all(|r| r.get::<String, _>("is_nullable") == "YES"));
    let historical = sqlx::query(
        "SELECT latency_ms, tool_success, tool_error_kind FROM mem_api_call_log
         WHERE path = '/historical'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(historical.get::<i32, _>("latency_ms"), 17);
    assert_eq!(
        historical.get::<Option<i8>, _>("tool_success"),
        partial.then_some(0)
    );
    assert_eq!(historical.get::<Option<String>, _>("tool_error_kind"), None);

    // Old and new API instances must both be able to write during a rolling update.
    sqlx::query(
        "INSERT INTO mem_api_call_log
         (user_id, method, path, status_code, latency_ms, rpc_success, rpc_error_code)
         VALUES (?, 'GET', '/old-writer', 200, 11, 1, NULL)",
    )
    .bind(&f.user)
    .execute(&f.pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO mem_api_call_log
         (user_id, method, path, status_code, latency_ms, rpc_success, rpc_error_code,
          tool_success, tool_error_kind)
         VALUES (?, 'POST', '/new-writer', 200, 12, 1, NULL, 0, 'backend')",
    )
    .bind(&f.user)
    .execute(&f.pool)
    .await
    .unwrap();
    let stats = sqlx::query(
        "SELECT COUNT(*) AS total,
            CAST(SUM(CASE WHEN status_code >= 400 OR rpc_success = 0 OR tool_success = 0
                THEN 1 ELSE 0 END) AS SIGNED) AS errors
         FROM mem_api_call_log",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(stats.get::<i64, _>("total"), 3);
    assert_eq!(stats.get::<i64, _>("errors"), if partial { 2 } else { 1 });
    let content: String =
        sqlx::query_scalar("SELECT content FROM mem_memories WHERE memory_id = 'preserved-memory'")
            .fetch_one(&f.pool)
            .await
            .unwrap();
    assert_eq!(content, "Keep existing memory content");
}

async fn routed_upgrade(version: i64, partial: bool) {
    let f = legacy_fixture(version, partial).await;
    f.router
        .user_store(&f.user)
        .await
        .expect("upgrade legacy user");
    verify_usable_history(&f, partial).await;
    let marker = sqlx::query("SELECT schema_version, updated_at FROM mem_schema_meta")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(
        marker.get::<i64, _>("schema_version"),
        CURRENT_USER_SCHEMA_VERSION
    );
    let updated_at = marker.get::<chrono::NaiveDateTime, _>("updated_at");
    if version == CURRENT_USER_SCHEMA_VERSION {
        assert_eq!(updated_at.to_string(), "2000-01-01 00:00:00");
    }
    f.router.invalidate_user(&f.user).await;
    f.router
        .user_store(&f.user)
        .await
        .expect("idempotent reentry");
    let after: chrono::NaiveDateTime = sqlx::query_scalar(
        "SELECT updated_at FROM mem_schema_meta WHERE schema_key = 'user_schema'",
    )
    .fetch_one(&f.pool)
    .await
    .unwrap();
    assert_eq!(after, updated_at);
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mem_api_call_log")
        .fetch_one(&f.pool)
        .await
        .unwrap();
    assert_eq!(count, 3);
    f.cleanup().await;
}

#[tokio::test]
async fn version_1_user_can_upgrade_and_preserve_call_history() {
    routed_upgrade(1, false).await;
}

#[tokio::test]
async fn version_2_user_repairs_missing_tool_outcomes_without_rewriting_marker() {
    routed_upgrade(2, false).await;
}

#[tokio::test]
async fn version_2_user_finishes_partial_tool_outcome_migration() {
    routed_upgrade(2, true).await;
}

#[tokio::test]
async fn two_instances_can_repair_version_2_tool_outcomes() {
    let f = legacy_fixture(2, false).await;
    let a = SqlMemoryStore::connect(&f.user_url, 1024, "instance-a".into())
        .await
        .unwrap();
    let b = SqlMemoryStore::connect(&f.user_url, 1024, "instance-b".into())
        .await
        .unwrap();
    let (a_result, b_result) = tokio::join!(a.migrate_user(), b.migrate_user());
    a_result.expect("first instance migration");
    b_result.expect("second instance migration");
    verify_usable_history(&f, false).await;
    a.pool().close().await;
    b.pool().close().await;
    f.cleanup().await;
}

#[tokio::test]
async fn failed_log_repair_keeps_memories_available_and_retries_on_cached_store() {
    let f = legacy_fixture(2, false).await;
    sqlx::query("ALTER TABLE mem_api_call_log RENAME TO saved_call_log")
        .execute(&f.pool)
        .await
        .unwrap();
    let store = f
        .router
        .user_store(&f.user)
        .await
        .expect("memory store stays available");
    let memory = store
        .get("preserved-memory")
        .await
        .unwrap()
        .expect("existing memory");
    assert_eq!(memory.content, "Keep existing memory content");
    assert!(store.ensure_call_log_tool_outcome_schema().await.is_err());
    sqlx::query("ALTER TABLE saved_call_log RENAME TO mem_api_call_log")
        .execute(&f.pool)
        .await
        .unwrap();
    // Retry through the same cached store without invalidating the router.
    let cached = f
        .router
        .user_store(&f.user)
        .await
        .expect("cached user remains available");
    assert!(std::sync::Arc::ptr_eq(&store, &cached));
    cached
        .ensure_call_log_tool_outcome_schema()
        .await
        .expect("retry repairs the restored log table");
    verify_usable_history(&f, false).await;
    f.cleanup().await;
}
