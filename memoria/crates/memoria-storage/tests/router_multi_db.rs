use chrono::Utc;
use memoria_core::{interfaces::MemoryStore, Memory, MemoryType, TrustTier};
use memoria_storage::store::CURRENT_USER_SCHEMA_VERSION;
use memoria_storage::{DbRouter, SqlMemoryStore};
use sqlx::Row;
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

fn database_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string())
}

fn replace_db_name(database_url: &str, db_name: &str) -> String {
    let suffix_start = database_url.find(['?', '#']).unwrap_or(database_url.len());
    let (without_suffix, suffix) = database_url.split_at(suffix_start);
    let (base, _) = without_suffix
        .rsplit_once('/')
        .expect("database url must include db name");
    format!("{base}/{db_name}{suffix}")
}

fn shared_db_url() -> String {
    let db_name = format!(
        "memoria_router_shared_{}",
        &Uuid::new_v4().simple().to_string()[..8]
    );
    replace_db_name(&database_url(), &db_name)
}

fn make_memory(id: &str, content: &str, user_id: &str) -> Memory {
    Memory {
        memory_id: id.to_string(),
        user_id: user_id.to_string(),
        memory_type: MemoryType::Semantic,
        content: content.to_string(),
        initial_confidence: 0.8,
        embedding: Some(vec![0.1; test_dim()]),
        source_event_ids: vec!["evt-router".to_string()],
        superseded_by: None,
        is_active: true,
        access_count: 0,
        session_id: Some("router-test".to_string()),
        observed_at: Some(Utc::now()),
        created_at: None,
        updated_at: None,
        extra_metadata: None,
        trust_tier: TrustTier::T3Inferred,
        retrieval_score: None,
        author_id: None,
        subject_id: None,
    }
}

#[tokio::test]
async fn router_isolates_users_into_distinct_databases() {
    let router = DbRouter::connect(&shared_db_url(), test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");

    let user_a = format!("router_a_{}", Uuid::new_v4().simple());
    let user_b = format!("router_b_{}", Uuid::new_v4().simple());
    let memory_id = format!("shared-memory-{}", Uuid::new_v4().simple());

    let store_a = router.user_store(&user_a).await.expect("user A store");
    let store_b = router.user_store(&user_b).await.expect("user B store");

    store_a
        .insert(&make_memory(&memory_id, "alpha content", &user_a))
        .await
        .expect("insert user A memory");
    store_b
        .insert(&make_memory(&memory_id, "beta content", &user_b))
        .await
        .expect("insert user B memory");

    let got_a = store_a
        .get(&memory_id)
        .await
        .expect("get user A memory")
        .expect("user A memory exists");
    let got_b = store_b
        .get(&memory_id)
        .await
        .expect("get user B memory")
        .expect("user B memory exists");

    assert_eq!(got_a.user_id, user_a);
    assert_eq!(got_a.content, "alpha content");
    assert_eq!(got_b.user_id, user_b);
    assert_eq!(got_b.content, "beta content");

    let db_a = router.user_db_name(&user_a).await.expect("user A db");
    let db_b = router.user_db_name(&user_b).await.expect("user B db");
    assert_ne!(
        db_a, db_b,
        "users must route to distinct physical databases"
    );

    let registry_count: i64 =
        sqlx::query("SELECT COUNT(*) AS cnt FROM mem_user_registry WHERE user_id IN (?, ?)")
            .bind(&user_a)
            .bind(&user_b)
            .fetch_one(router.shared_pool())
            .await
            .expect("query registry")
            .try_get("cnt")
            .expect("registry count");
    assert_eq!(registry_count, 2);
}

#[tokio::test]
async fn router_persists_and_repairs_user_schema_version_marker() {
    let shared_url = shared_db_url();
    let router = DbRouter::connect(&shared_url, test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");

    let user = format!("router_schema_{}", Uuid::new_v4().simple());
    router.user_store(&user).await.expect("user store");

    let db_name = router.user_db_name(&user).await.expect("user db");
    let user_db_url = replace_db_name(&shared_url, &db_name);
    let direct_pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(1)
        .connect(&user_db_url)
        .await
        .expect("connect user db");

    let row =
        sqlx::query("SELECT schema_version, updated_at FROM mem_schema_meta WHERE schema_key = ?")
            .bind("user_schema")
            .fetch_one(&direct_pool)
            .await
            .expect("fetch schema marker");
    let initial_version: i64 = row.try_get("schema_version").expect("initial version");
    let initial_updated_at = row
        .try_get::<chrono::NaiveDateTime, _>("updated_at")
        .expect("initial updated_at");
    assert_eq!(initial_version, CURRENT_USER_SCHEMA_VERSION);

    router
        .invalidate_user(&user)
        .await
        .expect("invalidate user caches");
    router.user_store(&user).await.expect("user store reentry");

    let row_after_skip =
        sqlx::query("SELECT schema_version, updated_at FROM mem_schema_meta WHERE schema_key = ?")
            .bind("user_schema")
            .fetch_one(&direct_pool)
            .await
            .expect("fetch schema marker after skip");
    let skipped_version: i64 = row_after_skip
        .try_get("schema_version")
        .expect("version after skip");
    let skipped_updated_at = row_after_skip
        .try_get::<chrono::NaiveDateTime, _>("updated_at")
        .expect("updated_at after skip");
    assert_eq!(skipped_version, CURRENT_USER_SCHEMA_VERSION);
    assert_eq!(skipped_updated_at, initial_updated_at);

    let stale_at = chrono::NaiveDate::from_ymd_opt(2000, 1, 1)
        .expect("stale date")
        .and_hms_micro_opt(0, 0, 0, 0)
        .expect("stale datetime");
    sqlx::query(
        "UPDATE mem_schema_meta SET schema_version = ?, updated_at = ? WHERE schema_key = ?",
    )
    .bind(0_i64)
    .bind(stale_at)
    .bind("user_schema")
    .execute(&direct_pool)
    .await
    .expect("set stale schema marker");

    router
        .invalidate_user(&user)
        .await
        .expect("invalidate user caches");
    router
        .user_store(&user)
        .await
        .expect("user store repairs version");

    let repaired_row =
        sqlx::query("SELECT schema_version, updated_at FROM mem_schema_meta WHERE schema_key = ?")
            .bind("user_schema")
            .fetch_one(&direct_pool)
            .await
            .expect("fetch repaired schema marker");
    let repaired_version: i64 = repaired_row
        .try_get("schema_version")
        .expect("repaired version");
    let repaired_updated_at = repaired_row
        .try_get::<chrono::NaiveDateTime, _>("updated_at")
        .expect("repaired updated_at");
    assert_eq!(repaired_version, CURRENT_USER_SCHEMA_VERSION);
    assert!(repaired_updated_at > stale_at);
}

#[tokio::test]
async fn shared_migrate_is_idempotent_for_async_task_user_id() {
    let shared_url = shared_db_url();
    let shared_store = SqlMemoryStore::connect(&shared_url, test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect shared store");

    shared_store
        .migrate_shared()
        .await
        .expect("first shared migrate");
    shared_store
        .migrate_shared()
        .await
        .expect("second shared migrate");

    let user_id_column_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = DATABASE() AND table_name = 'mem_async_tasks' AND column_name = 'user_id'",
    )
    .fetch_one(shared_store.pool())
    .await
    .expect("query async task user_id column count");
    assert_eq!(user_id_column_count, 1);
}

// Exercise first access without caller retries. Use distinct users to keep
// cross-session visibility independent of same-user initialization races.
#[tokio::test]
async fn router_initializes_distinct_fresh_users_without_retry() {
    let shared_url = shared_db_url();
    let router = DbRouter::connect(&shared_url, test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");
    let mut outcomes = Vec::new();
    for _ in 0..16 {
        let users: Vec<_> = (0..4)
            .map(|_| format!("schema_init_{}", Uuid::new_v4().simple()))
            .collect();
        let results = futures::future::join_all(users.iter().map(|user| {
            let router = router.clone();
            async move {
                let store = router.user_store(user).await?;
                let memory = make_memory(
                    &Uuid::new_v4().to_string(),
                    "schema initialization probe",
                    user,
                );
                store.insert(&memory).await?;
                assert!(store.get(&memory.memory_id).await?.is_some());
                Ok::<_, memoria_core::MemoriaError>(())
            }
        }))
        .await;
        for user in &users {
            sqlx::raw_sql(&format!(
                "DROP DATABASE IF EXISTS `{}`",
                DbRouter::user_db_name_for_id(user)
            ))
            .execute(router.shared_pool())
            .await
            .expect("clean up test user database");
        }
        outcomes.extend(results);
    }
    sqlx::raw_sql(&format!("DROP DATABASE `{}`", router.shared_db_name()))
        .execute(router.shared_pool())
        .await
        .expect("clean up test registry database");
    for result in outcomes {
        assert!(
            result.is_ok(),
            "fresh-user initialization failed: {result:?}"
        );
    }
}

#[tokio::test]
async fn router_initializes_one_fresh_user_under_concurrent_access() {
    let router = DbRouter::connect(&shared_db_url(), test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");
    let user = format!("schema_race_{}", Uuid::new_v4().simple());
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(16));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..16 {
        let router = router.clone();
        let user = user.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            router.user_store(&user).await
        });
    }
    let mut outcomes = Vec::new();
    while let Some(result) = tasks.join_next().await {
        outcomes.push(result.expect("join initialization"));
    }
    sqlx::raw_sql(&format!(
        "DROP DATABASE IF EXISTS `{}`",
        DbRouter::user_db_name_for_id(&user)
    ))
    .execute(router.shared_pool())
    .await
    .expect("clean up test user database");
    sqlx::raw_sql(&format!("DROP DATABASE `{}`", router.shared_db_name()))
        .execute(router.shared_pool())
        .await
        .expect("clean up test registry database");
    let first = outcomes[0].as_ref().expect("first initializer succeeded");
    for result in &outcomes {
        let store = result.as_ref().expect("same-user initialization succeeded");
        assert!(
            std::sync::Arc::ptr_eq(first, store),
            "concurrent first access must reuse one initialized store"
        );
    }
}

#[tokio::test]
async fn router_retries_initialization_after_a_schema_failure() {
    let router = DbRouter::connect(&shared_db_url(), test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");
    let user = format!("schema_retry_{}", Uuid::new_v4().simple());
    let db = router.user_db_name(&user).await.expect("provision user DB");
    // A partially initialized schema must fail without publishing readiness or
    // stranding the next caller behind an initialization gate.
    sqlx::raw_sql(&format!(
        "CREATE TABLE `{db}`.mem_schema_meta (schema_key VARCHAR(64) PRIMARY KEY)"
    ))
    .execute(router.shared_pool())
    .await
    .expect("create incomplete schema marker");
    let first = router.user_store(&user).await;
    sqlx::raw_sql(&format!("DROP TABLE `{db}`.mem_schema_meta"))
        .execute(router.shared_pool())
        .await
        .expect("remove incomplete marker");

    let retry = tokio::time::timeout(std::time::Duration::from_secs(30), router.user_store(&user))
        .await
        .expect("failed initialization must release its gate")
        .expect("retry initializes repaired schema");
    let cached = router.user_store(&user).await.expect("cached store");
    let version: i64 = sqlx::query_scalar(&format!(
        "SELECT schema_version FROM `{db}`.mem_schema_meta WHERE schema_key = 'user_schema'"
    ))
    .fetch_one(router.shared_pool())
    .await
    .expect("read repaired schema marker");
    sqlx::raw_sql(&format!("DROP DATABASE `{db}`"))
        .execute(router.shared_pool())
        .await
        .expect("clean up test user database");
    sqlx::raw_sql(&format!("DROP DATABASE `{}`", router.shared_db_name()))
        .execute(router.shared_pool())
        .await
        .expect("clean up test registry database");

    assert!(first.is_err(), "incomplete schema must fail initialization");
    assert!(std::sync::Arc::ptr_eq(&retry, &cached));
    assert_eq!(version, CURRENT_USER_SCHEMA_VERSION);
}

#[tokio::test]
async fn router_recovers_when_a_cold_initializer_or_waiter_is_cancelled() {
    let router = DbRouter::connect(&shared_db_url(), test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");
    for cancel_initializer in [true, false] {
        let user = format!("schema_cancel_{}", Uuid::new_v4().simple());
        // Exhaust the registry pool so the first cold call stops before
        // provisioning. Polling both futures makes cancellation deterministic.
        let mut held_connections = Vec::new();
        for _ in 0..router.shared_pool_max_connections() {
            held_connections.push(
                router
                    .shared_pool()
                    .acquire()
                    .await
                    .expect("hold pool slot"),
            );
        }
        let mut initializer = Box::pin(router.user_store(&user));
        assert!(futures::poll!(&mut initializer).is_pending());
        let mut waiter = Box::pin(router.user_store(&user));
        assert!(futures::poll!(&mut waiter).is_pending());
        let survivor = if cancel_initializer {
            drop(initializer);
            waiter
        } else {
            drop(waiter);
            initializer
        };
        drop(held_connections);
        let store = tokio::time::timeout(std::time::Duration::from_secs(30), survivor)
            .await
            .expect("cancellation must not strand the surviving caller")
            .expect("surviving caller initializes the user");
        let cached = router.user_store(&user).await.expect("cached store");
        assert!(std::sync::Arc::ptr_eq(&store, &cached));
        sqlx::raw_sql(&format!(
            "DROP DATABASE `{}`",
            DbRouter::user_db_name_for_id(&user)
        ))
        .execute(router.shared_pool())
        .await
        .expect("clean up test user database");
    }
    sqlx::raw_sql(&format!("DROP DATABASE `{}`", router.shared_db_name()))
        .execute(router.shared_pool())
        .await
        .expect("clean up test registry database");
}

async fn register_test_group(router: &DbRouter) -> (String, String) {
    SqlMemoryStore::new(
        router.shared_pool().clone(),
        test_dim(),
        "group-init-test".into(),
    )
    .migrate_shared()
    .await
    .expect("initialize shared group registry");
    let group = format!("grp_{}", Uuid::new_v4().simple());
    let db = format!("memoria_group_init_{}", Uuid::new_v4().simple());
    sqlx::raw_sql(&format!("CREATE DATABASE `{db}`"))
        .execute(router.shared_pool())
        .await
        .expect("create test group database");
    let now = Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO mem_groups (group_id, group_name, db_name, owner_user_id, status, created_at, updated_at) \
         VALUES (?, 'Synthetic group', ?, 'synthetic-owner', 'active', ?, ?)",
    )
    .bind(&group)
    .bind(&db)
    .bind(now)
    .bind(now)
    .execute(router.shared_pool())
    .await
    .expect("register test group");
    (group, db)
}

#[tokio::test]
async fn router_coalesces_group_initialization_through_scope_dispatch() {
    let router = DbRouter::connect(&shared_db_url(), test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");
    let (group, db) = register_test_group(&router).await;
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(8));
    let mut tasks = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let router = router.clone();
        let group = group.clone();
        let barrier = barrier.clone();
        tasks.spawn(async move {
            barrier.wait().await;
            router.scope_store(&group).await
        });
    }
    let mut stores = Vec::new();
    while let Some(result) = tasks.join_next().await {
        stores.push(result.expect("join group initializer"));
    }
    sqlx::raw_sql(&format!("DROP DATABASE `{db}`"))
        .execute(router.shared_pool())
        .await
        .expect("clean up group database");
    sqlx::raw_sql(&format!("DROP DATABASE `{}`", router.shared_db_name()))
        .execute(router.shared_pool())
        .await
        .expect("clean up registry database");
    let first = stores[0].as_ref().expect("group initializes");
    for result in &stores {
        let store = result.as_ref().expect("concurrent group initializes");
        assert!(
            std::sync::Arc::ptr_eq(first, store),
            "concurrent group first access must reuse one initialized store"
        );
    }
}

#[tokio::test]
async fn router_invalidation_cannot_be_overtaken_by_a_cold_initializer() {
    let router = DbRouter::connect(&shared_db_url(), test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");
    for group_scope in [false, true] {
        let (scope, db) = if group_scope {
            register_test_group(&router).await
        } else {
            let user = format!("invalidate_race_{}", Uuid::new_v4().simple());
            let db = DbRouter::user_db_name_for_id(&user);
            (user, db)
        };
        let held_connections = hold_registry_connections(&router).await;
        let mut initializer = Box::pin(router.scope_store(&scope));
        assert!(futures::poll!(&mut initializer).is_pending());
        let mut invalidation = Box::pin(router.invalidate_scope(&scope));
        let waited = futures::poll!(&mut invalidation).is_pending();
        drop(held_connections);
        let original = initializer.await.expect("cold initializer succeeds");
        sqlx::query(&format!(
            "UPDATE `{db}`.mem_schema_meta SET schema_version=0 WHERE schema_key='user_schema'"
        ))
        .execute(router.shared_pool())
        .await
        .expect("make initialized schema marker stale");
        if waited {
            invalidation.await.expect("invalidate scoped caches");
        }
        let refreshed = router
            .scope_store(&scope)
            .await
            .expect("reload scoped store");
        let version: i64 = sqlx::query_scalar(&format!(
            "SELECT schema_version FROM `{db}`.mem_schema_meta WHERE schema_key='user_schema'"
        ))
        .fetch_one(router.shared_pool())
        .await
        .expect("read revalidated schema marker");
        sqlx::raw_sql(&format!("DROP DATABASE `{db}`"))
            .execute(router.shared_pool())
            .await
            .expect("clean up scoped database");
        assert!(
            waited,
            "invalidation must wait for in-flight initialization"
        );
        assert!(!std::sync::Arc::ptr_eq(&original, &refreshed));
        assert_eq!(version, CURRENT_USER_SCHEMA_VERSION);
    }
    sqlx::raw_sql(&format!("DROP DATABASE `{}`", router.shared_db_name()))
        .execute(router.shared_pool())
        .await
        .expect("clean up registry database");
}

async fn hold_registry_connections(
    router: &DbRouter,
) -> Vec<sqlx::pool::PoolConnection<sqlx::MySql>> {
    let mut connections = Vec::new();
    for _ in 0..router.shared_pool_max_connections() {
        connections.push(
            router
                .shared_pool()
                .acquire()
                .await
                .expect("hold pool slot"),
        );
    }
    connections
}

#[tokio::test]
async fn router_invalidation_also_orders_an_inflight_database_mapping_lookup() {
    let router = DbRouter::connect(&shared_db_url(), test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");
    let user = format!("mapping_race_{}", Uuid::new_v4().simple());
    let replacement = format!("mapping_replacement_{}", Uuid::new_v4().simple());
    let held_connections = hold_registry_connections(&router).await;
    let mut lookup = Box::pin(router.user_db_name(&user));
    assert!(futures::poll!(&mut lookup).is_pending());
    let mut invalidation = Box::pin(router.invalidate_user(&user));
    let waited = futures::poll!(&mut invalidation).is_pending();
    drop(held_connections);
    let original = lookup.await.expect("mapping lookup succeeds");
    sqlx::raw_sql(&format!("CREATE DATABASE `{replacement}`"))
        .execute(router.shared_pool())
        .await
        .expect("create replacement database");
    router
        .register_user_db(&user, &replacement)
        .await
        .expect("replace mapping");
    if waited {
        invalidation.await.expect("invalidate scoped caches");
    }
    let refreshed = router.user_db_name(&user).await.expect("reload mapping");
    for db in [&original, &replacement, router.shared_db_name()] {
        sqlx::raw_sql(&format!("DROP DATABASE `{db}`"))
            .execute(router.shared_pool())
            .await
            .expect("clean up mapping test database");
    }
    assert!(waited, "invalidation must wait for the mapping publisher");
    assert_eq!(refreshed, replacement);
}

#[tokio::test]
async fn router_isolates_user_ids_that_match_the_legacy_group_cache_prefix() {
    let router = DbRouter::connect(&shared_db_url(), test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect router");
    for group_first in [true, false] {
        let (group, group_db) = register_test_group(&router).await;
        let user = format!("group:{group}");
        let user_db = DbRouter::user_db_name_for_id(&user);
        let (user_store, group_store) = if group_first {
            let group_store = router
                .scope_store(&group)
                .await
                .expect("initialize group first");
            (
                router
                    .user_store(&user)
                    .await
                    .expect("initialize user second"),
                group_store,
            )
        } else {
            let user_store = router
                .user_store(&user)
                .await
                .expect("initialize user first");
            (
                user_store,
                router
                    .scope_store(&group)
                    .await
                    .expect("initialize group second"),
            )
        };
        let isolated = user_store.db_name() == Some(user_db.as_str())
            && group_store.db_name() == Some(group_db.as_str())
            && !std::sync::Arc::ptr_eq(&user_store, &group_store);
        // Keep cleanup valid on the unfixed implementation too: a collision can
        // skip user provisioning entirely.
        if !isolated {
            for db in [&user_db, &group_db, router.shared_db_name()] {
                sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS `{db}`"))
                    .execute(router.shared_pool())
                    .await
                    .expect("clean up collision reproduction");
            }
            assert!(
                isolated,
                "user and group must never return each other's store"
            );
        }
        assert_eq!(
            router.user_db_name(&user).await.expect("user mapping"),
            user_db
        );
        router
            .invalidate_user(&user)
            .await
            .expect("invalidate only the user");
        assert!(std::sync::Arc::ptr_eq(
            &group_store,
            &router
                .group_store(&group)
                .await
                .expect("group stays cached")
        ));
        let refreshed_user = router.user_store(&user).await.expect("reload user");
        assert!(!std::sync::Arc::ptr_eq(&user_store, &refreshed_user));
        router
            .invalidate_group(&group)
            .await
            .expect("invalidate only the group");
        assert!(std::sync::Arc::ptr_eq(
            &refreshed_user,
            &router.user_store(&user).await.expect("user stays cached")
        ));
        let refreshed_group = router.group_store(&group).await.expect("reload group");
        assert!(!std::sync::Arc::ptr_eq(&group_store, &refreshed_group));
        for db in [&user_db, &group_db] {
            sqlx::raw_sql(&format!("DROP DATABASE `{db}`"))
                .execute(router.shared_pool())
                .await
                .expect("clean up isolated database");
        }
    }
    sqlx::raw_sql(&format!("DROP DATABASE `{}`", router.shared_db_name()))
        .execute(router.shared_pool())
        .await
        .expect("clean up registry database");
}
