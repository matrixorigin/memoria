//! Field preservation across ordinary and pre-subject snapshot branches.
//! Uses the actual MCP dispatch and disposable per-test databases, no LLM service.
mod support;

use serde_json::{json, Value};
use sqlx::MySqlPool;

struct Context {
    inner: support::multi_db::McpTestContext,
    user: String,
}

impl std::ops::Deref for Context {
    type Target = support::multi_db::McpTestContext;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

async fn call(ctx: &Context, tool: &str, args: Value) -> Value {
    memoria_mcp::git_tools::call(tool, args, &ctx.git(), &ctx.service(), &ctx.user)
        .await
        .unwrap_or_else(|error| panic!("{tool}: {error}"))
}

async fn seed(pool: &MySqlPool, table: &str, id: &str, user: &str) {
    // The table is obtained from our branch registry, not test/user input.
    sqlx::query(&format!(
        "INSERT INTO `{table}` (memory_id,user_id,memory_type,content,source_event_ids,observed_at,created_at)
         VALUES (?, ?, 'semantic', ?, '[]', NOW(), NOW())"
    ))
    .bind(id).bind(user).bind(id).execute(pool).await.unwrap();
}

async fn fixture(historical: bool) -> (Context, MySqlPool, String) {
    let dim = std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024);
    let ctx = Context {
        inner: support::multi_db::setup_mcp_context("branch_scope", dim, None, None).await,
        user: uuid::Uuid::new_v4().to_string(),
    };
    ctx.user_store(&ctx.user).await;
    let pool = ctx.user_db_pool(&ctx.user).await;
    for id in ["restore", "update-old", "remove", "conflict"] {
        seed(&pool, "mem_memories", id, &ctx.user).await;
    }
    if historical {
        for ddl in [
            "ALTER TABLE mem_memories DROP INDEX idx_scope_subject_active",
            "ALTER TABLE mem_memories DROP COLUMN subject_id",
        ] {
            sqlx::raw_sql(ddl).execute(&pool).await.unwrap();
        }
        call(&ctx, "memory_snapshot", json!({"name":"历史快照"})).await;
        // Deliberately put the new column in the middle. The clone must match it,
        // not append the column and rely on version-dependent native behavior.
        sqlx::raw_sql("ALTER TABLE mem_memories ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL AFTER user_id")
            .execute(&pool).await.unwrap();
        ctx.user_store(&ctx.user)
            .await
            .migrate_user()
            .await
            .unwrap();
    }
    let args = if historical {
        json!({"name":"工作分支", "from_snapshot":"历史快照"})
    } else {
        json!({"name":"工作分支"})
    };
    call(&ctx, "memory_branch", args).await;
    let table: String =
        sqlx::query_scalar("SELECT table_name FROM mem_branches WHERE name = '工作分支'")
            .fetch_one(&pool)
            .await
            .unwrap();
    for target in ["mem_memories", &table] {
        sqlx::raw_sql(&format!(
            "UPDATE `{target}` SET subject_id='subject-a', author_id='author-a'"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    (ctx, pool, table)
}

async fn scoped_write(ctx: &Context, content: &str) -> String {
    call(ctx, "memory_checkout", json!({"name":"工作分支"})).await;
    memoria_mcp::tools::call(
        "memory_store",
        json!({"content":content,"subject_id":"subject-a"}),
        &ctx.service(),
        &ctx.user,
    )
    .await
    .unwrap();
    let memory = ctx
        .service()
        .list_active(&ctx.user, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.content == content)
        .unwrap();
    assert_eq!(memory.subject_id.as_deref(), Some("subject-a"));
    call(ctx, "memory_checkout", json!({"name":"main"})).await;
    memory.memory_id
}

async fn assert_scoped_read(ctx: &Context, id: &str) {
    let store = ctx.user_store(&ctx.user).await;
    for (subject, expected) in [("subject-a", true), ("other-subject", false)] {
        let rows = store
            .search_fulltext_from_scoped(
                &store.t("mem_memories"),
                &ctx.user,
                "scoped",
                100,
                None,
                Some(subject),
                None,
                &memoria_core::MemoryFilter::default(),
            )
            .await
            .unwrap();
        assert_eq!(
            rows.iter().any(|m| m.memory_id == id),
            expected,
            "{subject}"
        );
    }
}

async fn apply_preserves_scope(historical: bool) {
    let (ctx, pool, table) = fixture(historical).await;
    let added = scoped_write(&ctx, "scoped branch addition").await;
    seed(&pool, &table, "update-new", &ctx.user).await;
    seed(&pool, &table, "null-scope-add", &ctx.user).await;
    for statement in [
        "UPDATE mem_memories SET is_active=0 WHERE memory_id='restore'".to_string(),
        "UPDATE mem_memories SET content='main conflict version' WHERE memory_id='conflict'".into(),
        format!("UPDATE `{table}` SET is_active=0, superseded_by='update-new' WHERE memory_id='update-old'"),
        format!("UPDATE `{table}` SET is_active=0 WHERE memory_id='remove'"),
        format!("UPDATE `{table}` SET content='branch conflict version' WHERE memory_id='conflict'"),
        format!("UPDATE `{table}` SET subject_id='subject-a', author_id='branch-author' WHERE memory_id <> 'null-scope-add'"),
    ] {
        sqlx::raw_sql(&statement).execute(&pool).await.unwrap();
    }
    let result = call(
        &ctx,
        "memory_apply",
        json!({
            "source":"工作分支", "adds":[added, "restore", "null-scope-add"],
            "updates":[{"old_id":"update-old","new_id":"update-new"}],
            "removes":["remove"], "accept_branch_conflicts":["conflict"]
        }),
    )
    .await;
    let report = result["content"][0]["text"].as_str().unwrap();
    for expected in ["3 added", "1 updated", "1 removed", "1 conflicts accepted"] {
        assert!(report.contains(expected), "{report}");
    }
    // Assert inactive history as well as active reads: remove/update reinsert
    // the old record and must not erase its ownership metadata either.
    let rows: Vec<(String, Option<String>, Option<String>, i8)> = sqlx::query_as(
        "SELECT memory_id,subject_id,author_id,is_active FROM mem_memories ORDER BY memory_id",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(rows.len(), 7);
    for (id, subject, author, active) in rows {
        if id == "null-scope-add" {
            assert_eq!((subject, author, active), (None, None, 1));
        } else {
            assert_eq!(subject.as_deref(), Some("subject-a"), "{id}");
            assert_eq!(author.as_deref(), Some("branch-author"), "{id}");
            assert_eq!(
                active,
                if id == "remove" || id == "update-old" {
                    0
                } else {
                    1
                }
            );
        }
    }
    for (subject, expected) in [("subject-a", 4), ("other-subject", 0)] {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mem_memories WHERE subject_id=? AND is_active=1",
        )
        .bind(subject)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, expected);
    }
    let memories = ctx.service().list_active(&ctx.user, 100).await.unwrap();
    assert_eq!(
        memories
            .iter()
            .filter(|m| m.subject_id.as_deref() == Some("subject-a"))
            .count(),
        4
    );
    assert_scoped_read(&ctx, &added).await;
    call(&ctx, "memory_branch_delete", json!({"name":"工作分支"})).await;
    if historical {
        call(&ctx, "memory_snapshot_delete", json!({"name":"历史快照"})).await;
    }
}

#[tokio::test]
async fn apply_preserves_scope_on_current_branch() {
    apply_preserves_scope(false).await;
}

#[tokio::test]
async fn apply_preserves_scope_on_legacy_branch() {
    apply_preserves_scope(true).await;
}

async fn merge_preserves_scope(historical: bool) {
    let (ctx, pool, table) = fixture(historical).await;
    let added = scoped_write(&ctx, "scoped merge addition").await;
    sqlx::query(&format!(
        "UPDATE `{table}` SET author_id='branch-author' WHERE memory_id=?"
    ))
    .bind(&added)
    .execute(&pool)
    .await
    .unwrap();
    call(&ctx, "memory_merge", json!({"source":"工作分支"})).await;
    let row: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT subject_id,author_id FROM mem_memories WHERE memory_id=?")
            .bind(&added)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        row,
        (Some("subject-a".into()), Some("branch-author".into()))
    );
    let memory = ctx
        .service()
        .list_active(&ctx.user, 100)
        .await
        .unwrap()
        .into_iter()
        .find(|m| m.memory_id == added)
        .unwrap();
    assert_eq!(memory.subject_id.as_deref(), Some("subject-a"));
    assert_scoped_read(&ctx, &added).await;
    call(&ctx, "memory_branch_delete", json!({"name":"工作分支"})).await;
    if historical {
        call(&ctx, "memory_snapshot_delete", json!({"name":"历史快照"})).await;
    }
}

#[tokio::test]
async fn merge_preserves_scope_on_current_branch() {
    merge_preserves_scope(false).await;
}

#[tokio::test]
async fn merge_preserves_scope_on_legacy_branch() {
    merge_preserves_scope(true).await;
}

#[tokio::test]
async fn existing_parent_and_branch_migrate_with_lineage_intact() {
    let (ctx, pool, _) = fixture(false).await;
    call(&ctx, "memory_branch_delete", json!({"name":"工作分支"})).await;
    // The legacy branch must be created AFTER reconstructing the old main
    // schema. Dropping/re-adding a column on an already-new-schema branch tests
    // a different operation: MO intentionally distinguishes that column identity.
    for ddl in [
        "ALTER TABLE mem_memories DROP INDEX idx_scope_subject_active",
        "ALTER TABLE mem_memories DROP COLUMN subject_id",
    ] {
        sqlx::raw_sql(ddl).execute(&pool).await.unwrap();
    }
    let branch = "br_pre_subject";
    let git = memoria_git::GitForDataService::new(pool.clone(), ctx.user_db_name(&ctx.user).await);
    git.create_branch(branch, "mem_memories").await.unwrap();
    let store = ctx.user_store(&ctx.user).await;
    store
        .register_branch(&ctx.user, "old-deployment", branch)
        .await
        .unwrap();
    store.migrate_user().await.unwrap();
    for table in ["mem_memories", branch] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM `{table}` WHERE subject_id IS NULL AND author_id='author-a'"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 4, "legacy data must remain on {table}");
    }
    call(&ctx, "memory_diff", json!({"source":"old-deployment"})).await;
    call(
        &ctx,
        "memory_branch_delete",
        json!({"name":"old-deployment"}),
    )
    .await;
    let probes: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name LIKE 'mem_lineage_probe_%'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(probes, 0);
}

#[tokio::test]
async fn unicode_branch_skipped_by_old_author_migration_is_repaired_at_version_2() {
    let (ctx, pool, _) = fixture(false).await;
    call(&ctx, "memory_branch_delete", json!({"name":"工作分支"})).await;
    // Reconstruct the state left by the old migration: it added author_id to
    // main, skipped the Unicode branch, and still recorded schema version 2.
    for ddl in [
        "ALTER TABLE mem_memories DROP INDEX idx_author",
        "ALTER TABLE mem_memories DROP COLUMN author_id",
        "DATA BRANCH CREATE TABLE br_1234abcd_legacy FROM mem_memories",
        "ALTER TABLE br_1234abcd_legacy RENAME TO `br_1234abcd_实验`",
        "ALTER TABLE mem_memories ADD COLUMN author_id VARCHAR(64) DEFAULT NULL",
        "ALTER TABLE mem_memories ADD INDEX idx_author (author_id)",
        "UPDATE mem_schema_meta SET schema_version=2 WHERE schema_key='user_schema'",
    ] {
        sqlx::raw_sql(ddl).execute(&pool).await.unwrap();
    }
    let branch = "br_1234abcd_实验";
    let store = ctx.user_store(&ctx.user).await;
    store
        .register_branch(&ctx.user, "工作分支", branch)
        .await
        .unwrap();
    let missing_before: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns WHERE table_schema=DATABASE() AND table_name=? AND column_name='author_id'"
    ).bind(branch).fetch_one(&pool).await.unwrap();
    assert_eq!(
        missing_before, 0,
        "fixture must model the skipped old migration"
    );
    store.migrate_user().await.unwrap();
    // The targeted repair must run despite the current schema-version marker.
    for table in ["mem_memories", branch] {
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM `{table}` WHERE subject_id='subject-a' AND author_id IS NULL"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 4, "legacy data must remain on {table}");
        let index: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.statistics WHERE table_schema=DATABASE() AND table_name=? AND index_name='idx_author'"
        ).bind(table).fetch_one(&pool).await.unwrap();
        assert!(index > 0, "author index missing on {table}");
    }
    store.migrate_user().await.unwrap();
    let added = scoped_write(&ctx, "scoped write after Unicode author migration").await;
    sqlx::query(&format!(
        "UPDATE `{branch}` SET author_id='new-author' WHERE memory_id=?"
    ))
    .bind(&added)
    .execute(&pool)
    .await
    .unwrap();
    call(&ctx, "memory_checkout", json!({"name":"工作分支"})).await;
    let memories = ctx.service().list_active(&ctx.user, 100).await.unwrap();
    assert_eq!(memories.len(), 5);
    let memory = memories.iter().find(|m| m.memory_id == added).unwrap();
    assert_eq!(memory.author_id.as_deref(), Some("new-author"));
    assert_eq!(memory.subject_id.as_deref(), Some("subject-a"));
    call(&ctx, "memory_checkout", json!({"name":"main"})).await;
    let git = memoria_git::GitForDataService::new(pool.clone(), ctx.user_db_name(&ctx.user).await);
    let diff = git
        .diff_branch_rows(branch, "mem_memories", &ctx.user, 100)
        .await
        .unwrap();
    assert!(diff.iter().any(|row| row.memory_id == added));
    git.merge_branch(branch, "mem_memories").await.unwrap();
    let merged: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT author_id,subject_id FROM mem_memories WHERE memory_id=?")
            .bind(&added)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        merged,
        (Some("new-author".into()), Some("subject-a".into()))
    );
    call(&ctx, "memory_branch_delete", json!({"name":"工作分支"})).await;
    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name=?"
    ).bind(branch).fetch_one(&pool).await.unwrap();
    assert_eq!(remaining, 0);
}

#[tokio::test]
async fn incompatible_historical_type_is_not_registered() {
    let (ctx, pool, _) = fixture(true).await;
    call(&ctx, "memory_branch_delete", json!({"name":"工作分支"})).await;
    // Current schema has evolved beyond the one supported subject migration.
    sqlx::raw_sql("ALTER TABLE mem_memories MODIFY COLUMN content VARCHAR(4096) NOT NULL")
        .execute(&pool)
        .await
        .unwrap();
    let error = memoria_mcp::git_tools::call(
        "memory_branch",
        json!({"name":"incompatible", "from_snapshot":"历史快照"}),
        &ctx.git(),
        &ctx.service(),
        &ctx.user,
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("Branch schema is incompatible"),
        "{error}"
    );
    let registered: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mem_branches WHERE name='incompatible'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(registered, 0);
    let tables: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=DATABASE() AND table_name LIKE 'br_%'")
        .fetch_one(&pool).await.unwrap();
    assert_eq!(tables, 0, "failed clone must be cleaned up");
    call(&ctx, "memory_snapshot_delete", json!({"name":"历史快照"})).await;
}

#[tokio::test]
async fn default_merge_does_not_replace_similar_memories_in_other_subjects() {
    let (ctx, pool, table) = fixture(false).await;
    let dim = std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024);
    let mut vector = vec![0.0_f32; dim];
    vector[0] = 1.0;
    let vector = serde_json::to_string(&vector).unwrap();
    sqlx::query(
        "UPDATE mem_memories SET subject_id='subject-b', embedding=? WHERE memory_id='restore'",
    )
    .bind(&vector)
    .execute(&pool)
    .await
    .unwrap();
    for (id, subject) in [("scoped-new", Some("subject-a")), ("unscoped-new", None)] {
        seed(&pool, &table, id, &ctx.user).await;
        sqlx::query(&format!(
            "UPDATE `{table}` SET subject_id=?, embedding=? WHERE memory_id=?"
        ))
        .bind(subject)
        .bind(&vector)
        .bind(id)
        .execute(&pool)
        .await
        .unwrap();
    }
    call(&ctx, "memory_merge", json!({"source":"工作分支"})).await;
    let content: String =
        sqlx::query_scalar("SELECT content FROM mem_memories WHERE memory_id='restore'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        content, "restore",
        "another subject must not replace this row"
    );
    let rows: Vec<(String, Option<String>)> = sqlx::query_as("SELECT memory_id,subject_id FROM mem_memories WHERE memory_id IN ('scoped-new','unscoped-new') ORDER BY memory_id")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(
        rows,
        vec![
            ("scoped-new".into(), Some("subject-a".into())),
            ("unscoped-new".into(), None)
        ]
    );
    // Same-subject replacement still works, without selecting either the
    // NULL-subject row or the subject-b row as additional conflict candidates.
    seed(&pool, &table, "scoped-replacement", &ctx.user).await;
    sqlx::query(&format!("UPDATE `{table}` SET subject_id='subject-a', embedding=? WHERE memory_id='scoped-replacement'"))
        .bind(&vector).execute(&pool).await.unwrap();
    call(&ctx, "memory_merge", json!({"source":"工作分支"})).await;
    let contents: Vec<(String, String)> = sqlx::query_as("SELECT memory_id,content FROM mem_memories WHERE memory_id IN ('restore','scoped-new','unscoped-new') ORDER BY memory_id")
        .fetch_all(&pool).await.unwrap();
    assert_eq!(
        contents,
        vec![
            ("restore".into(), "restore".into()),
            ("scoped-new".into(), "scoped-replacement".into()),
            ("unscoped-new".into(), "unscoped-new".into()),
        ]
    );
    call(&ctx, "memory_branch_delete", json!({"name":"工作分支"})).await;
}
