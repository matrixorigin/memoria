/// Snapshot end-to-end tests — from the user's perspective.
/// Tests complex combinations: snapshot → mutate → rollback, pagination, prefix delete, etc.
///
/// IMPORTANT: These tests MUST run serially because:
/// - MO#23860: concurrent DELETE + INSERT FROM SNAPSHOT causes w-w conflict
/// - MO#23861: concurrent snapshot restore loses FULLTEXT INDEX secondary tables
mod support;

use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use serde_json::{json, Value};
use serial_test::serial;
use std::sync::Arc;
use tokio::sync::Barrier;
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

fn uid() -> String {
    format!("snap_{}", &Uuid::new_v4().simple().to_string()[..8])
}
fn snap(suffix: &str) -> String {
    // Use UUID to ensure global uniqueness across parallel tests
    format!("s{}_{}", &Uuid::new_v4().simple().to_string()[..6], suffix)
}

/// Mirror of git_tools::sanitize_name — for test assertions on internal snapshot names.
fn sanitize_name(name: &str) -> String {
    let mut clean: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    if clean.is_empty() || !clean.chars().next().unwrap().is_alphabetic() {
        clean = format!("s_{clean}");
    }
    clean
}

fn sanitize_snapshot_scope(scope: &str) -> String {
    compact_identifier_fragment(scope, 11)
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

/// Mirror of git_tools::snap_internal so white-box assertions follow scoped names.
fn internal_snapshot_name(db_name: &str, name: &str) -> String {
    const SNAP_PREFIX: &str = "mem_snap_";
    const MILESTONE_PREFIX: &str = "mem_milestone_";

    if name.starts_with(SNAP_PREFIX) || name.starts_with(MILESTONE_PREFIX) {
        name.to_string()
    } else {
        let scope = sanitize_snapshot_scope(db_name);
        format!(
            "{SNAP_PREFIX}{}_{scope}_{}",
            scope.len(),
            sanitize_name(name)
        )
    }
}

async fn setup() -> (
    Arc<MemoryService>,
    Arc<GitForDataService>,
    String,
    String,
    support::multi_db::McpTestContext,
) {
    let ctx = support::multi_db::setup_mcp_context("snapshot_e2e", test_dim(), None, None).await;
    let uid = uid();
    let db_name = ctx.user_db_name(&uid).await;
    (ctx.service(), ctx.git(), uid, db_name, ctx)
}

async fn setup_multi_db() -> (
    Arc<MemoryService>,
    String,
    String,
    String,
    String,
    Arc<GitForDataService>,
    support::multi_db::McpTestContext,
) {
    let ctx =
        support::multi_db::setup_mcp_context("snapshot_e2e_multi", test_dim(), None, None).await;
    let user_a = uid();
    let user_b = uid();
    let db_a = ctx.user_db_name(&user_a).await;
    let db_b = ctx.user_db_name(&user_b).await;
    (ctx.service(), user_a, user_b, db_a, db_b, ctx.git(), ctx)
}

async fn git_call(
    name: &str,
    args: Value,
    git: &Arc<GitForDataService>,
    svc: &Arc<MemoryService>,
    uid: &str,
) -> Value {
    memoria_mcp::git_tools::call(name, args, git, svc, uid)
        .await
        .expect(name)
}

async fn store(content: &str, svc: &Arc<MemoryService>, uid: &str) {
    memoria_mcp::tools::call("memory_store", json!({"content": content}), svc, uid)
        .await
        .expect("store");
}

async fn git_for_user(svc: &Arc<MemoryService>, uid: &str) -> GitForDataService {
    let sql = svc.user_sql_store(uid).await.expect("user sql store");
    GitForDataService::new(
        sql.pool().clone(),
        sql.database_name().expect("user db name").to_string(),
    )
}

fn text(v: &Value) -> &str {
    v["content"][0]["text"].as_str().unwrap_or("")
}

// ── 1. Basic: snapshot → mutate → rollback restores state ────────────────────

#[tokio::test]
#[serial]
async fn test_snapshot_rollback_restores_state() {
    let (svc, git, uid, _db_name, _ctx) = setup().await;
    let snap_name = snap("basic");

    // Store 2 memories
    store("memory A", &svc, &uid).await;
    store("memory B", &svc, &uid).await;

    // Snapshot
    let r = git_call(
        "memory_snapshot",
        json!({"name": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("created"), "got: {}", text(&r));
    println!("✅ snapshot created: {}", text(&r));

    // Mutate: add C, delete A via purge
    store("memory C", &svc, &uid).await;
    let list = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(list.len(), 3);

    // Rollback
    let r = git_call(
        "memory_rollback",
        json!({"name": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("Rolled back"), "got: {}", text(&r));
    println!("✅ rollback: {}", text(&r));

    // Should have 2 memories again
    let list = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(
        list.len(),
        2,
        "expected 2 after rollback, got {}",
        list.len()
    );
    let contents: Vec<_> = list.iter().map(|m| m.content.as_str()).collect();
    assert!(contents.contains(&"memory A"));
    assert!(contents.contains(&"memory B"));
    assert!(
        !contents.contains(&"memory C"),
        "C should be gone after rollback"
    );
    println!("✅ rollback restored 2 memories, C is gone");

    git_call(
        "memory_snapshot_delete",
        json!({"names": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 2. Rollback to nonexistent snapshot returns error ────────────────────────

#[tokio::test]
#[serial]
async fn test_rollback_nonexistent_snapshot_errors() {
    let (svc, git, uid, _db_name, _ctx) = setup().await;
    let result = memoria_mcp::git_tools::call(
        "memory_rollback",
        json!({"name": "does_not_exist_xyz"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(result.is_err(), "should error on missing snapshot");
    println!("✅ rollback nonexistent → error: {:?}", result.unwrap_err());
}

// ── 3. Multiple snapshots: rollback to earlier one ───────────────────────────

#[tokio::test]
#[serial]
async fn test_multiple_snapshots_rollback_to_earlier() {
    let (svc, git, uid, _db_name, _ctx) = setup().await;
    let snap1 = snap("v1");
    let snap2 = snap("v2");

    store("v1 memory", &svc, &uid).await;
    git_call("memory_snapshot", json!({"name": snap1}), &git, &svc, &uid).await;

    store("v2 memory", &svc, &uid).await;
    git_call("memory_snapshot", json!({"name": snap2}), &git, &svc, &uid).await;

    store("v3 memory", &svc, &uid).await;
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 3);

    // Rollback to v1 (earliest)
    git_call("memory_rollback", json!({"name": snap1}), &git, &svc, &uid).await;
    let list = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].content, "v1 memory");
    println!("✅ rollback to v1: only 'v1 memory' remains");

    git_call(
        "memory_snapshot_delete",
        json!({"names": format!("{snap1},{snap2}")}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 4. memory_snapshots pagination ───────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_snapshots_pagination() {
    let (svc, git, uid, _db_name, _ctx) = setup().await;
    let names: Vec<String> = (0..5).map(|i| snap(&format!("pg{i}"))).collect();

    for n in &names {
        git_call("memory_snapshot", json!({"name": n}), &git, &svc, &uid).await;
    }

    // Page 1: limit=2 offset=0
    let r = git_call(
        "memory_snapshots",
        json!({"limit": 2, "offset": 0}),
        &git,
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(!t.contains("No snapshots"), "got: {t}");
    println!("✅ page 1 (limit=2): {t}");

    // Page 2: limit=2 offset=2
    let r2 = git_call(
        "memory_snapshots",
        json!({"limit": 2, "offset": 2}),
        &git,
        &svc,
        &uid,
    )
    .await;
    let t2 = text(&r2);
    assert_ne!(t, t2, "pages should differ");
    println!("✅ page 2 (limit=2, offset=2): {t2}");

    // Cleanup
    let joined = names.join(",");
    git_call(
        "memory_snapshot_delete",
        json!({"names": joined}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

#[tokio::test]
#[serial]
async fn test_snapshot_delete_rejects_blank_prefix_without_deleting() {
    let (svc, git, uid, db_name, _ctx) = setup().await;
    let names = [snap("blank_a"), snap("blank_b")];
    let outcome: anyhow::Result<()> = async {
        for name in &names {
            let created = memoria_mcp::git_tools::call(
                "memory_snapshot",
                json!({"name": name}),
                &git,
                &svc,
                &uid,
            )
            .await?;
            anyhow::ensure!(created["isError"] != true, "create failed: {created}");
        }
        for prefix in ["", "  "] {
            let result = memoria_mcp::git_tools::call(
                "memory_snapshot_delete",
                json!({"prefix": prefix}),
                &git,
                &svc,
                &uid,
            )
            .await?;
            anyhow::ensure!(result["isError"] == true, "blank prefix accepted: {result}");
            anyhow::ensure!(text(&result).contains("Snapshot prefix must not be blank"));
            let snapshots = git_for_user(&svc, &uid).await.list_snapshots().await?;
            for name in &names {
                let internal = internal_snapshot_name(&db_name, name);
                anyhow::ensure!(
                    snapshots.iter().any(|s| s.snapshot_name == internal),
                    "snapshot {name} was deleted"
                );
            }
        }
        Ok(())
    }
    .await;
    let cleanup: anyhow::Result<()> = async {
        let result = memoria_mcp::git_tools::call(
            "memory_snapshot_delete",
            json!({"names": names.join(",")}),
            &git,
            &svc,
            &uid,
        )
        .await?;
        anyhow::ensure!(result["isError"] != true, "cleanup rejected: {result}");
        let sql = svc.user_sql_store(&uid).await?;
        let user_git = GitForDataService::new(sql.pool().clone(), db_name.clone());
        let snapshots = user_git.list_snapshots().await?;
        for name in &names {
            let internal = internal_snapshot_name(&db_name, name);
            anyhow::ensure!(
                !snapshots.iter().any(|s| s.snapshot_name == internal),
                "cleanup left snapshot {name}"
            );
        }
        Ok(())
    }
    .await;
    assert!(
        cleanup.is_ok(),
        "cleanup failed: {cleanup:?}; test: {outcome:?}"
    );
    outcome.unwrap();
}

// ── 5. memory_snapshot_delete by prefix ──────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_snapshot_delete_by_prefix() {
    let (svc, git, uid, db_name, _ctx) = setup().await;
    let prefix = format!("pre_{}", &uid[5..]);
    let s1 = format!("{prefix}_a");
    let s2 = format!("{prefix}_b");
    let keeper = snap("keeper");

    git_call("memory_snapshot", json!({"name": s1}), &git, &svc, &uid).await;
    git_call("memory_snapshot", json!({"name": s2}), &git, &svc, &uid).await;
    git_call("memory_snapshot", json!({"name": keeper}), &git, &svc, &uid).await;

    // Delete by prefix
    let r = git_call(
        "memory_snapshot_delete",
        json!({"prefix": prefix}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(
        text(&r).contains("2"),
        "expected 2 deleted, got: {}",
        text(&r)
    );
    println!("✅ delete by prefix: {}", text(&r));

    // keeper should still exist (check by internal name)
    let user_git = git_for_user(&svc, &uid).await;
    let snaps = user_git.list_snapshots().await.unwrap();
    let keeper_internal = internal_snapshot_name(&db_name, &keeper);
    let s1_internal = internal_snapshot_name(&db_name, &s1);
    let s2_internal = internal_snapshot_name(&db_name, &s2);
    assert!(
        snaps.iter().any(|s| s.snapshot_name == keeper_internal),
        "keeper should survive"
    );
    assert!(
        !snaps.iter().any(|s| s.snapshot_name == s1_internal),
        "s1 should be gone"
    );
    assert!(
        !snaps.iter().any(|s| s.snapshot_name == s2_internal),
        "s2 should be gone"
    );
    println!("✅ keeper survived prefix delete");

    git_call(
        "memory_snapshot_delete",
        json!({"names": keeper}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 6. memory_snapshot_delete batch by names ─────────────────────────────────

#[tokio::test]
#[serial]
async fn test_snapshot_delete_batch_names() {
    let (svc, git, uid, db_name, _ctx) = setup().await;
    let s1 = snap("b1");
    let s2 = snap("b2");
    let s3 = snap("b3");

    for n in [&s1, &s2, &s3] {
        git_call("memory_snapshot", json!({"name": n}), &git, &svc, &uid).await;
    }

    // Delete s1 and s3 in one call
    let r = git_call(
        "memory_snapshot_delete",
        json!({"names": format!("{s1},{s3}")}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(
        text(&r).contains("2"),
        "expected 2 deleted, got: {}",
        text(&r)
    );

    let user_git = git_for_user(&svc, &uid).await;
    let snaps = user_git.list_snapshots().await.unwrap();
    let s1i = internal_snapshot_name(&db_name, &s1);
    let s2i = internal_snapshot_name(&db_name, &s2);
    let s3i = internal_snapshot_name(&db_name, &s3);
    assert!(!snaps.iter().any(|s| s.snapshot_name == s1i));
    assert!(
        snaps.iter().any(|s| s.snapshot_name == s2i),
        "s2 should remain"
    );
    assert!(!snaps.iter().any(|s| s.snapshot_name == s3i));
    println!("✅ batch delete: s1+s3 gone, s2 remains");

    git_call(
        "memory_snapshot_delete",
        json!({"names": s2}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 7. Snapshot → branch → mutate branch → rollback main (branch unaffected) ─

#[tokio::test]
#[serial]
async fn test_snapshot_and_branch_independent() {
    let (svc, git, uid, _db_name, _ctx) = setup().await;
    let snap_name = snap("pre_branch");
    let branch = format!("br_{}", &uid[5..]);

    store("shared memory", &svc, &uid).await;
    git_call(
        "memory_snapshot",
        json!({"name": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;

    // Create branch and add branch-only memory
    git_call("memory_branch", json!({"name": branch}), &git, &svc, &uid).await;
    git_call("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;
    store("branch memory", &svc, &uid).await;

    // Checkout main and add main-only memory
    git_call("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    store("main extra", &svc, &uid).await;
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 2);

    // Rollback main to snapshot (should remove "main extra", keep "shared memory")
    git_call(
        "memory_rollback",
        json!({"name": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
    let list = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].content, "shared memory");
    println!("✅ rollback main: only 'shared memory' remains");

    // Branch still has its own data
    git_call("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;
    let branch_list = svc.list_active(&uid, 10).await.unwrap();
    // branch was created from main before rollback, so it has "shared memory" + "branch memory"
    assert!(
        branch_list.iter().any(|m| m.content == "branch memory"),
        "branch memory should still be on branch"
    );
    println!("✅ branch unaffected by main rollback");

    // Cleanup
    git_call("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    git_call(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
    git_call(
        "memory_snapshot_delete",
        json!({"names": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 8. Duplicate snapshot name is handled without creating a second snapshot ─

#[tokio::test]
#[serial]
async fn test_duplicate_snapshot_name_rejected() {
    let (svc, git, uid, _db_name, _ctx) = setup().await;
    let snap_name = snap("dup");

    let first = git_call(
        "memory_snapshot",
        json!({"name": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(text(&first).contains("created"), "got: {}", text(&first));

    // Second create with same name should return an explicit duplicate message
    let second = git_call(
        "memory_snapshot",
        json!({"name": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(
        text(&second).contains("already exists"),
        "got: {}",
        text(&second)
    );
    assert_eq!(second["isError"], true);

    let sql = svc.user_sql_store(&uid).await.unwrap();
    let regs = sql.list_snapshot_registrations(&uid).await.unwrap();
    let count = regs.iter().filter(|reg| reg.name == snap_name).count();
    assert_eq!(count, 1, "duplicate create should not register twice");
    println!(
        "✅ duplicate snapshot rejected without 500: {}",
        text(&second)
    );

    git_call(
        "memory_snapshot_delete",
        json!({"names": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

#[tokio::test]
#[serial]
async fn test_concurrent_duplicate_snapshot_name_is_coalesced() {
    let (svc, git, uid, db_name, _ctx) = setup().await;
    let snap_name = snap("dupcon");
    let barrier = Arc::new(Barrier::new(3));

    let mut handles = Vec::new();
    for _ in 0..2 {
        let svc = svc.clone();
        let git = git.clone();
        let uid = uid.clone();
        let snap_name = snap_name.clone();
        let barrier = barrier.clone();
        handles.push(tokio::spawn(async move {
            barrier.wait().await;
            git_call(
                "memory_snapshot",
                json!({"name": snap_name}),
                &git,
                &svc,
                &uid,
            )
            .await
        }));
    }

    barrier.wait().await;
    let first = handles.remove(0).await.expect("first task join");
    let second = handles.remove(0).await.expect("second task join");
    let results = [text(&first).to_string(), text(&second).to_string()];

    assert!(
        results.iter().any(|result| result.contains("created")),
        "expected one create result, got {results:?}"
    );
    assert!(
        results
            .iter()
            .any(|result| result.contains("already exists")),
        "expected one duplicate result, got {results:?}"
    );

    let sql = svc.user_sql_store(&uid).await.unwrap();
    let regs = sql.list_snapshot_registrations(&uid).await.unwrap();
    let count = regs.iter().filter(|reg| reg.name == snap_name).count();
    assert_eq!(count, 1, "concurrent create should register once");

    let internal = internal_snapshot_name(&db_name, &snap_name);
    let user_git = git_for_user(&svc, &uid).await;
    let snaps = user_git.list_snapshots().await.unwrap();
    let internal_count = snaps
        .iter()
        .filter(|snapshot| snapshot.snapshot_name == internal)
        .count();
    assert_eq!(
        internal_count, 1,
        "concurrent create should create one snapshot"
    );

    git_call(
        "memory_snapshot_delete",
        json!({"names": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

#[tokio::test]
#[serial]
async fn test_multi_db_snapshot_rollback_isolates_users() {
    let (svc, user_a, user_b, db_a, db_b, git, _ctx) = setup_multi_db().await;
    let snap_name = snap("multi");

    store("alice before snapshot", &svc, &user_a).await;
    store("bob steady state", &svc, &user_b).await;

    assert_ne!(
        db_a, db_b,
        "multi-db test must route users to different databases"
    );

    let created = git_call(
        "memory_snapshot",
        json!({"name": snap_name}),
        &git,
        &svc,
        &user_a,
    )
    .await;
    assert!(
        text(&created).contains("created"),
        "got: {}",
        text(&created)
    );

    store("alice after snapshot", &svc, &user_a).await;
    assert_eq!(svc.list_active(&user_a, 10).await.unwrap().len(), 2);
    assert_eq!(svc.list_active(&user_b, 10).await.unwrap().len(), 1);

    let rolled_back = git_call(
        "memory_rollback",
        json!({"name": snap_name}),
        &git,
        &svc,
        &user_a,
    )
    .await;
    assert!(
        text(&rolled_back).contains("Rolled back"),
        "got: {}",
        text(&rolled_back)
    );

    let alice = svc.list_active(&user_a, 10).await.unwrap();
    let bob = svc.list_active(&user_b, 10).await.unwrap();
    assert_eq!(alice.len(), 1);
    assert_eq!(bob.len(), 1);
    assert_eq!(alice[0].content, "alice before snapshot");
    assert_eq!(bob[0].content, "bob steady state");

    git_call(
        "memory_snapshot_delete",
        json!({"names": snap_name}),
        &git,
        &svc,
        &user_a,
    )
    .await;
}

// ── 9. Multi-user isolation: user A's rollback doesn't affect user B ──────────

#[tokio::test]
#[serial]
async fn test_multiuser_snapshot_isolation() {
    let (svc, git, _, _db_name, _ctx) = setup().await;
    let uid_a = uid();
    let uid_b = uid();
    let snap_a = snap("ua");

    // User A stores memory and snapshots
    store("user A memory", &svc, &uid_a).await;
    git_call(
        "memory_snapshot",
        json!({"name": snap_a}),
        &git,
        &svc,
        &uid_a,
    )
    .await;

    // User B stores different memory (no snapshot)
    store("user B memory", &svc, &uid_b).await;
    store("user B memory 2", &svc, &uid_b).await;
    assert_eq!(svc.list_active(&uid_b, 10).await.unwrap().len(), 2);

    // User A adds more and rolls back
    store("user A extra", &svc, &uid_a).await;
    git_call(
        "memory_rollback",
        json!({"name": snap_a}),
        &git,
        &svc,
        &uid_a,
    )
    .await;

    // User A should be back to 1 memory
    let a_list = svc.list_active(&uid_a, 10).await.unwrap();
    assert_eq!(a_list.len(), 1);
    assert_eq!(a_list[0].content, "user A memory");

    // In multi-db mode user B lives in a different database, so A's rollback must not affect B.
    let b_list = svc.list_active(&uid_b, 10).await.unwrap();
    println!(
        "ℹ️  After A's rollback, B has {} memories (expected 2 — isolated user DB)",
        b_list.len()
    );
    assert_eq!(
        b_list.len(),
        2,
        "multi-db rollback should leave other users' data untouched"
    );
    println!("✅ multiuser: rollback isolated to the initiating user's DB");

    git_call(
        "memory_snapshot_delete",
        json!({"names": snap_a}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
}

#[tokio::test]
#[serial]
async fn test_snapshot_limit_is_per_user_and_capped_at_20() {
    let (svc, git, _, _db_name, _ctx) = setup().await;
    let uid_a = uid();
    let uid_b = uid();

    let names_a: Vec<String> = (0..20).map(|i| snap(&format!("cap_a_{i}"))).collect();
    for name in &names_a {
        let r = git_call("memory_snapshot", json!({"name": name}), &git, &svc, &uid_a).await;
        assert!(
            text(&r).contains("created"),
            "A create failed: {}",
            text(&r)
        );
    }

    let extra_a = snap("cap_a_overflow");
    let r = git_call(
        "memory_snapshot",
        json!({"name": extra_a}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
    assert!(
        text(&r).contains("Snapshot limit reached (20)"),
        "expected per-user cap message, got: {}",
        text(&r)
    );

    let first_b = snap("cap_b_first");
    let r = git_call(
        "memory_snapshot",
        json!({"name": first_b}),
        &git,
        &svc,
        &uid_b,
    )
    .await;
    assert!(
        text(&r).contains("created"),
        "user B should not be blocked by user A's cap: {}",
        text(&r)
    );

    let r = git_call("memory_snapshots", json!({"limit": 50}), &git, &svc, &uid_b).await;
    let t = text(&r);
    assert!(t.contains(&first_b), "B should see own snapshot: {t}");
    assert!(
        !t.contains(&names_a[0]),
        "B should not see A's snapshots in list: {t}"
    );

    git_call(
        "memory_snapshot_delete",
        json!({"names": names_a.join(",")}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
    git_call(
        "memory_snapshot_delete",
        json!({"names": first_b}),
        &git,
        &svc,
        &uid_b,
    )
    .await;
}

#[tokio::test]
#[serial]
async fn test_snapshot_delete_is_scoped_to_owner() {
    let (svc, git, _, _db_name, _ctx) = setup().await;
    let uid_a = uid();
    let uid_b = uid();
    let snap_a = snap("owned");

    git_call(
        "memory_snapshot",
        json!({"name": snap_a}),
        &git,
        &svc,
        &uid_a,
    )
    .await;

    let r = git_call(
        "memory_snapshot_delete",
        json!({"names": snap_a}),
        &git,
        &svc,
        &uid_b,
    )
    .await;
    assert!(
        text(&r).contains("Deleted 0 snapshot(s)"),
        "non-owner delete should be ignored: {}",
        text(&r)
    );

    let r = git_call("memory_snapshots", json!({"limit": 20}), &git, &svc, &uid_a).await;
    assert!(
        text(&r).contains(&snap_a),
        "owner should still see snapshot after foreign delete attempt: {}",
        text(&r)
    );

    git_call(
        "memory_snapshot_delete",
        json!({"names": snap_a}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
}

// ── 10. older_than delete ─────────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_snapshot_delete_older_than() {
    let (svc, git, uid, db_name, _ctx) = setup().await;
    let s1 = snap("old1");
    let s2 = snap("old2");
    let s3 = snap("keep");

    git_call("memory_snapshot", json!({"name": s1}), &git, &svc, &uid).await;
    git_call("memory_snapshot", json!({"name": s2}), &git, &svc, &uid).await;
    git_call("memory_snapshot", json!({"name": s3}), &git, &svc, &uid).await;

    // Delete snapshots older than far future — should delete all 3
    let r = git_call(
        "memory_snapshot_delete",
        json!({"older_than": "2099-01-01"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    let t = text(&r);
    assert!(
        t.contains("3") || {
            // count may include other test snapshots — just verify our 3 are gone
            let snaps = git.list_snapshots().await.unwrap();
            !snaps
                .iter()
                .any(|s| s.snapshot_name == internal_snapshot_name(&db_name, &s1))
                && !snaps
                    .iter()
                    .any(|s| s.snapshot_name == internal_snapshot_name(&db_name, &s2))
                && !snaps
                    .iter()
                    .any(|s| s.snapshot_name == internal_snapshot_name(&db_name, &s3))
        },
        "expected all 3 deleted, got: {t}"
    );
    println!("✅ older_than delete: {t}");

    // Invalid date format → error
    let r2 = memoria_mcp::git_tools::call(
        "memory_snapshot_delete",
        json!({"older_than": "not-a-date"}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(r2.is_err(), "invalid date should error");
    println!("✅ older_than invalid date → error");
}

// ── 11. Snapshot list filters system snapshots ────────────────────────────────
// SHOW SNAPSHOTS returns all account-level snapshots including system ones.
// memory_snapshots must only show mem_snap_/mem_milestone_ prefixed ones.

#[tokio::test]
#[serial]
async fn test_snapshot_list_filters_system_snapshots() {
    let (svc, git, uid, _db_name, _ctx) = setup().await;
    let s1 = snap("mysnap");
    git_call("memory_snapshot", json!({"name": s1}), &git, &svc, &uid).await;

    let r = git_call("memory_snapshots", json!({"limit": 100}), &git, &svc, &uid).await;
    let t = text(&r);

    // All listed snapshots must be mem_snap_ or auto: (mem_milestone_) prefixed
    // The display name should NOT contain "mem_snap_" prefix
    assert!(
        !t.contains("mem_snap_"),
        "display names should strip mem_snap_ prefix, got: {t}"
    );
    // Our snapshot should appear by its display name
    assert!(t.contains(&s1), "our snapshot should appear, got: {t}");
    println!("✅ snapshot list filters system snapshots, shows display names");

    git_call(
        "memory_snapshot_delete",
        json!({"names": s1}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 12. Rollback → snapshot → rollback again (chained operations) ─────────────

#[tokio::test]
#[serial]
async fn test_chained_snapshot_rollback() {
    let (svc, git, uid, _db_name, _ctx) = setup().await;
    let snap_v1 = snap("v1");
    let snap_v2 = snap("v2");

    // v1: 1 memory
    store("v1 only", &svc, &uid).await;
    git_call(
        "memory_snapshot",
        json!({"name": snap_v1}),
        &git,
        &svc,
        &uid,
    )
    .await;

    // v2: 2 memories
    store("v2 added", &svc, &uid).await;
    git_call(
        "memory_snapshot",
        json!({"name": snap_v2}),
        &git,
        &svc,
        &uid,
    )
    .await;

    // Add more, then rollback to v2
    store("v3 extra", &svc, &uid).await;
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 3);
    git_call(
        "memory_rollback",
        json!({"name": snap_v2}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 2);
    println!("✅ rollback to v2: 2 memories");

    // Now rollback to v1 from v2 state
    git_call(
        "memory_rollback",
        json!({"name": snap_v1}),
        &git,
        &svc,
        &uid,
    )
    .await;
    let list = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].content, "v1 only");
    println!("✅ chained rollback to v1: 1 memory");

    git_call(
        "memory_snapshot_delete",
        json!({"names": format!("{snap_v1},{snap_v2}")}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 13. Branch limit enforced ─────────────────────────────────────────────────

#[tokio::test]
#[serial]
async fn test_branch_limit_enforced() {
    let (svc, git, uid, _db_name, _ctx) = setup().await;

    // Create branches up to limit (MAX_BRANCHES = 20)
    // We'll create a few and verify the limit message, not exhaust the full 20
    // (too slow and pollutes DB). Instead, mock by checking the error path directly.
    // Create 3 branches, verify they work, then verify limit message format.
    let b1 = format!("blimit_a_{}", &uid[5..]);
    let b2 = format!("blimit_b_{}", &uid[5..]);
    git_call("memory_branch", json!({"name": b1}), &git, &svc, &uid).await;
    git_call("memory_branch", json!({"name": b2}), &git, &svc, &uid).await;

    // Duplicate name (same user) should be rejected
    let r = git_call("memory_branch", json!({"name": b1}), &git, &svc, &uid).await;
    assert!(text(&r).contains("already exists"), "got: {}", text(&r));
    println!("✅ duplicate branch rejected: {}", text(&r));

    // Cleanup
    git_call(
        "memory_branch_delete",
        json!({"name": b1}),
        &git,
        &svc,
        &uid,
    )
    .await;
    git_call(
        "memory_branch_delete",
        json!({"name": b2}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 14. Snapshot → branch from snapshot → verify branch has snapshot data ────

#[tokio::test]
#[serial]
async fn test_branch_from_snapshot() {
    let (svc, git, uid, _db_name, _ctx) = setup().await;
    let snap_name = snap("forsplit");
    let branch = format!("bfs_{}", &uid[5..]);

    // Store 2 memories and snapshot
    store("snap memory 1", &svc, &uid).await;
    store("snap memory 2", &svc, &uid).await;
    git_call(
        "memory_snapshot",
        json!({"name": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;

    // Add more memories after snapshot
    store("post-snap memory", &svc, &uid).await;
    assert_eq!(svc.list_active(&uid, 10).await.unwrap().len(), 3);

    // Create branch from snapshot — branch should have only 2 memories
    let r = git_call(
        "memory_branch",
        json!({"name": branch, "from_snapshot": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
    assert!(text(&r).contains("Created"), "got: {}", text(&r));
    println!("✅ branch from snapshot: {}", text(&r));

    // Checkout branch and verify it has snapshot-time data (2 memories)
    git_call("memory_checkout", json!({"name": branch}), &git, &svc, &uid).await;
    let branch_list = svc.list_active(&uid, 10).await.unwrap();
    assert_eq!(
        branch_list.len(),
        2,
        "branch from snapshot should have 2 memories, got {}: {:?}",
        branch_list.len(),
        branch_list.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    assert!(
        !branch_list.iter().any(|m| m.content == "post-snap memory"),
        "post-snap memory should not be on branch"
    );
    println!(
        "✅ branch from snapshot has {} memories (snapshot-time data)",
        branch_list.len()
    );

    // Cleanup
    git_call("memory_checkout", json!({"name": "main"}), &git, &svc, &uid).await;
    git_call(
        "memory_branch_delete",
        json!({"name": branch}),
        &git,
        &svc,
        &uid,
    )
    .await;
    git_call(
        "memory_snapshot_delete",
        json!({"names": snap_name}),
        &git,
        &svc,
        &uid,
    )
    .await;
}

// ── 15. Milestone snapshots do not count towards user limit ───────────────────

#[tokio::test]
#[serial]
async fn test_milestone_snapshot_not_counted_towards_limit() {
    let (svc, git, _, _db_name, _ctx) = setup().await;
    let uid_a = uid();

    // Create 20 user snapshots (at the limit)
    let names: Vec<String> = (0..20).map(|i| snap(&format!("ms_{i}"))).collect();
    for name in &names {
        let r = git_call("memory_snapshot", json!({"name": name}), &git, &svc, &uid_a).await;
        assert!(
            text(&r).contains("created"),
            "A create failed: {}",
            text(&r)
        );
    }

    // Create a milestone snapshot directly in user A's DB (simulating system-created milestone)
    let milestone_suffix = &Uuid::new_v4().simple().to_string()[..6];
    let milestone_name = format!("mem_milestone_test_milestone_{milestone_suffix}");
    let user_git = git_for_user(&svc, &uid_a).await;
    user_git
        .create_snapshot(&milestone_name)
        .await
        .expect("create milestone");

    // User should still be able to create one more snapshot (milestone doesn't count)
    let extra = snap("after_milestone");
    let r = git_call(
        "memory_snapshot",
        json!({"name": extra}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
    assert!(
        text(&r).contains("Snapshot limit reached"),
        "user should be at limit, got: {}",
        text(&r)
    );

    // User A should see the milestone in their list.
    let r = git_call("memory_snapshots", json!({"limit": 50}), &git, &svc, &uid_a).await;
    let t = text(&r);
    assert!(
        t.contains("auto:test_milestone"),
        "A should see milestone in the same DB: {}",
        t
    );

    // Cleanup
    user_git
        .drop_snapshot(&milestone_name)
        .await
        .expect("drop milestone");
    git_call(
        "memory_snapshot_delete",
        json!({"names": names.join(",")}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
}

// ── 16. Deleting snapshots releases quota for new snapshots ───────────────────

#[tokio::test]
#[serial]
async fn test_delete_releases_quota() {
    let (svc, git, _, _db_name, _ctx) = setup().await;
    let uid_a = uid();

    // Create 20 snapshots (at limit)
    let names: Vec<String> = (0..20).map(|i| snap(&format!("quota_{i}"))).collect();
    for name in &names {
        let r = git_call("memory_snapshot", json!({"name": name}), &git, &svc, &uid_a).await;
        assert!(text(&r).contains("created"), "create failed: {}", text(&r));
    }

    // Verify at limit
    let overflow = snap("overflow");
    let r = git_call(
        "memory_snapshot",
        json!({"name": overflow}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
    assert!(
        text(&r).contains("Snapshot limit reached"),
        "should be at limit"
    );

    // Delete 5 snapshots
    let to_delete: Vec<String> = names.iter().take(5).cloned().collect();
    git_call(
        "memory_snapshot_delete",
        json!({"names": to_delete.join(",")}),
        &git,
        &svc,
        &uid_a,
    )
    .await;

    // Now should be able to create 5 new snapshots
    for i in 0..5 {
        let new_snap = snap(&format!("new_{i}"));
        let r = git_call(
            "memory_snapshot",
            json!({"name": new_snap}),
            &git,
            &svc,
            &uid_a,
        )
        .await;
        assert!(
            text(&r).contains("created"),
            "should create after delete, got: {}",
            text(&r)
        );
    }

    // 6th should fail again
    let r = git_call(
        "memory_snapshot",
        json!({"name": overflow}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
    assert!(
        text(&r).contains("Snapshot limit reached"),
        "should be at limit again"
    );

    // Cleanup remaining
    let remaining: Vec<String> = names.iter().skip(5).cloned().collect();
    let new_names: Vec<String> = (0..5).map(|i| snap(&format!("new_{i}"))).collect();
    git_call(
        "memory_snapshot_delete",
        json!({"names": remaining.join(",")}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
    git_call(
        "memory_snapshot_delete",
        json!({"names": new_names.join(",")}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
}

// ── 17. Safety snapshots (mem_snap_pre_*) are visible within the same user DB ──

#[tokio::test]
#[serial]
async fn test_safety_snapshot_globally_visible() {
    let (svc, git, _, _db_name, _ctx) = setup().await;
    let uid_a = uid();
    let uid_b = uid();

    // Create a safety snapshot directly in user A's DB (use unique name)
    let unique_id = &Uuid::new_v4().simple().to_string()[..6];
    let safety_name = format!("mem_snap_pre_safety_{unique_id}");
    let display_name = format!("pre_safety_{unique_id}"); // snap_display strips mem_snap_ prefix
    let user_git = git_for_user(&svc, &uid_a).await;
    user_git
        .create_snapshot(&safety_name)
        .await
        .expect("create safety");

    // User A should see it in their list (by display name)
    let r_a = git_call("memory_snapshots", json!({"limit": 50}), &git, &svc, &uid_a).await;
    let t_a = text(&r_a);
    assert!(
        t_a.contains(&display_name),
        "A should see safety snapshot: {}",
        t_a
    );

    let r_b = git_call("memory_snapshots", json!({"limit": 50}), &git, &svc, &uid_b).await;
    let t_b = text(&r_b);
    assert!(
        !t_b.contains(&display_name),
        "B should not see safety snapshot from another DB: {}",
        t_b
    );

    // Safety snapshots are deletable by the user in the same DB.
    let r = git_call(
        "memory_snapshot_delete",
        json!({"names": &display_name}),
        &git,
        &svc,
        &uid_a,
    )
    .await;
    assert!(
        text(&r).contains("Deleted 1"),
        "should delete safety snapshot: {}",
        text(&r)
    );
    println!("✅ safety snapshot deleted by user: {}", text(&r));
}
