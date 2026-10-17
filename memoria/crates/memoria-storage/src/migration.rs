use crate::{DbRouter, SqlMemoryStore};
use memoria_core::MemoriaError;
use serde::Serialize;
use sqlx::{
    mysql::{MySqlPool, MySqlPoolOptions},
    Row,
};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock};

const MIGRATION_INSTANCE_ID: &str = "migration-cli";
const MAX_IDENTIFIER_LEN: usize = 64;
const PRE_EXECUTE_ACCOUNT_SNAPSHOT_PREFIX: &str = "mem_migrate_account_pre_";
const PRE_EXECUTE_ACCOUNT_SNAPSHOT_SUFFIX_LEN: usize = 8;
const SHARED_DURABLE_TABLES: &[&str] = &[
    "mem_api_keys",
    "mem_governance_runtime_state",
    "mem_plugin_signers",
    "mem_plugin_packages",
    "mem_plugin_bindings",
    "mem_plugin_reviews",
    "mem_plugin_binding_rules",
    "mem_plugin_audit_events",
];
const SHARED_RUNTIME_TABLES: &[&str] = &["mem_distributed_locks", "mem_async_tasks"];
const USER_DISCOVERY_SKIP_TABLES: &[&str] = &[
    "mem_user_registry",
    "mem_distributed_locks",
    "mem_async_tasks",
];
const USER_MIGRATION_SKIP_TABLES: &[&str] =
    &["mem_api_keys", "mem_user_registry", "mem_async_tasks"];
const DEFAULT_DISCOVERY_POOL_MAX_CONNECTIONS: u32 = 4;
const MIGRATION_SOURCE_POOL_MAX_CONNECTIONS_UPPER: u32 = 64;

#[derive(Debug, Clone, Default)]
pub struct LegacyToMultiDbMigrationOptions {
    pub user_ids: Vec<String>,
    pub concurrency: usize,
}

#[derive(Debug, Clone, Serialize)]
pub struct LegacyToMultiDbMigrationReport {
    pub dry_run: bool,
    pub legacy_db_name: String,
    pub shared_db_name: String,
    pub pre_execute_account_snapshot: Option<String>,
    pub selected_users: Vec<String>,
    pub shared_tables: Vec<TableMigrationReport>,
    pub skipped_shared_runtime_tables: Vec<String>,
    pub users: Vec<UserMigrationReport>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct UserMigrationReport {
    pub user_id: String,
    pub target_db: String,
    pub active_branch: Option<String>,
    pub active_snapshot_count: i64,
    pub tables: Vec<TableMigrationReport>,
    pub branch_tables: Vec<TableMigrationReport>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TableMigrationReport {
    pub table_name: String,
    pub source_rows: i64,
    pub target_rows: Option<i64>,
    pub status: String,
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingLegacyMultiDbMigration {
    pub legacy_db_name: String,
    pub shared_db_name: String,
    pub legacy_users: Vec<String>,
    pub missing_users: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeTopology {
    FreshSingleDb,
    PendingLegacyMigration(PendingLegacyMultiDbMigration),
    MultiDbReady,
}

#[derive(Debug, Clone)]
struct ColumnSpec {
    name: String,
    nullable: bool,
    has_default: bool,
    auto_increment: bool,
}

#[derive(Debug, Clone)]
struct BranchRecord {
    name: String,
    table_name: String,
}

#[derive(Debug, Clone, Copy, Default)]
struct SourceSchemaAvailability {
    has_snapshots: bool,
    has_user_state: bool,
    has_branches: bool,
}

impl SourceSchemaAvailability {
    async fn load(
        cache: &MigrationExecutionCache,
        pool: &MySqlPool,
        db_name: &str,
    ) -> Result<Self, MemoriaError> {
        Ok(Self {
            has_snapshots: cache
                .source_table_exists(pool, db_name, "mem_snapshots")
                .await?,
            has_user_state: cache
                .source_table_exists(pool, db_name, "mem_user_state")
                .await?,
            has_branches: cache
                .source_table_exists(pool, db_name, "mem_branches")
                .await?,
        })
    }
}

#[derive(Debug, Default)]
struct MigrationExecutionCache {
    source_table_exists: Mutex<HashMap<String, bool>>,
    copy_columns: Mutex<HashMap<String, Vec<String>>>,
    user_target_tables: OnceLock<Vec<String>>,
}

struct TableCopyContext<'a> {
    source_pool: &'a MySqlPool,
    source_db: &'a str,
    target_pool: &'a MySqlPool,
    target_db: &'a str,
    cache: &'a MigrationExecutionCache,
}

fn execute_row_count_note(context: Option<&str>) -> String {
    const NOTE: &str = "execute mode reports copied rows for source_rows and target_rows; \
         these are not independent source/target table row counts";
    match context {
        Some(context) => format!("{context}; {NOTE}"),
        None => NOTE.to_string(),
    }
}

impl MigrationExecutionCache {
    async fn source_table_exists(
        &self,
        pool: &MySqlPool,
        db_name: &str,
        table_name: &str,
    ) -> Result<bool, MemoriaError> {
        if let Some(exists) = lock_or_recover(&self.source_table_exists)
            .get(table_name)
            .copied()
        {
            return Ok(exists);
        }
        let exists = table_exists(pool, db_name, table_name).await?;
        lock_or_recover(&self.source_table_exists).insert(table_name.to_string(), exists);
        Ok(exists)
    }

    async fn copy_columns(
        &self,
        source_pool: &MySqlPool,
        source_db: &str,
        target_pool: &MySqlPool,
        target_db: &str,
        table_name: &str,
    ) -> Result<Vec<String>, MemoriaError> {
        if let Some(columns) = lock_or_recover(&self.copy_columns).get(table_name).cloned() {
            return Ok(columns);
        }
        let columns = copyable_columns(
            &list_columns(source_pool, source_db, table_name).await?,
            &list_columns(target_pool, target_db, table_name).await?,
            table_name,
        )?;
        lock_or_recover(&self.copy_columns).insert(table_name.to_string(), columns.clone());
        Ok(columns)
    }

    async fn user_target_tables(
        &self,
        target_pool: &MySqlPool,
        target_db: &str,
    ) -> Result<Vec<String>, MemoriaError> {
        if let Some(tables) = self.user_target_tables.get() {
            return Ok(tables.clone());
        }
        let tables = list_tables_with_user_id(target_pool, target_db)
            .await?
            .into_iter()
            .filter(|table| !is_physical_branch_table(table))
            .collect::<Vec<_>>();
        let _ = self.user_target_tables.set(tables.clone());
        Ok(self.user_target_tables.get().cloned().unwrap_or(tables))
    }
}

fn lock_or_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

pub async fn plan_legacy_single_db_to_multi_db(
    legacy_db_url: &str,
    shared_db_url: &str,
    embedding_dim: usize,
    options: LegacyToMultiDbMigrationOptions,
) -> Result<LegacyToMultiDbMigrationReport, MemoriaError> {
    run_legacy_single_db_to_multi_db(legacy_db_url, shared_db_url, embedding_dim, options, true)
        .await
}

pub async fn execute_legacy_single_db_to_multi_db(
    legacy_db_url: &str,
    shared_db_url: &str,
    embedding_dim: usize,
    options: LegacyToMultiDbMigrationOptions,
) -> Result<LegacyToMultiDbMigrationReport, MemoriaError> {
    run_legacy_single_db_to_multi_db(legacy_db_url, shared_db_url, embedding_dim, options, false)
        .await
}

pub async fn detect_runtime_topology(
    legacy_db_url: &str,
    shared_db_url: &str,
) -> Result<RuntimeTopology, MemoriaError> {
    let legacy_db_name = parse_db_name(legacy_db_url)?;
    let shared_db_name = parse_db_name(shared_db_url)?;
    if legacy_db_name == shared_db_name {
        return Ok(RuntimeTopology::FreshSingleDb);
    }

    let legacy_pool = match connect_pool(legacy_db_url).await {
        Ok(pool) => pool,
        Err(MemoriaError::Database(msg))
            if is_unknown_database_error_message(&msg, &legacy_db_name) =>
        {
            let shared_users =
                load_active_shared_registry_users_or_empty(shared_db_url, &shared_db_name).await?;
            return Ok(classify_runtime_topology(
                legacy_db_name,
                shared_db_name,
                vec![],
                shared_users,
            ));
        }
        Err(err) => return Err(err),
    };
    let legacy_users = discover_users(&legacy_pool, &legacy_db_name).await?;
    let shared_users =
        load_active_shared_registry_users_or_empty(shared_db_url, &shared_db_name).await?;

    Ok(classify_runtime_topology(
        legacy_db_name,
        shared_db_name,
        legacy_users,
        shared_users,
    ))
}

async fn run_legacy_single_db_to_multi_db(
    legacy_db_url: &str,
    shared_db_url: &str,
    embedding_dim: usize,
    options: LegacyToMultiDbMigrationOptions,
    dry_run: bool,
) -> Result<LegacyToMultiDbMigrationReport, MemoriaError> {
    let legacy_db_name = parse_db_name(legacy_db_url)?;
    let shared_db_name = parse_db_name(shared_db_url)?;
    let concurrency = options.concurrency.max(1);
    if legacy_db_name == shared_db_name {
        return Err(MemoriaError::Validation(
            "legacy_db_url and shared_db_url must point to different databases".into(),
        ));
    }

    let legacy_pool = connect_pool_with_max(
        legacy_db_url,
        migration_source_pool_max_connections(concurrency),
    )
    .await?;
    let mut selected_users = if options.user_ids.is_empty() {
        discover_users(&legacy_pool, &legacy_db_name).await?
    } else {
        normalize_user_ids(&options.user_ids)
    };
    selected_users.sort();
    selected_users.dedup();
    let cache = Arc::new(MigrationExecutionCache::default());
    let source_schema =
        SourceSchemaAvailability::load(cache.as_ref(), &legacy_pool, &legacy_db_name).await?;

    let mut warnings = Vec::new();
    if !options.user_ids.is_empty() {
        warnings.push(
            "User filters apply to per-user table copy; shared durable tables are still synced in full."
                .to_string(),
        );
    }

    let snapshot_violations = collect_active_snapshot_violations(
        &legacy_pool,
        &legacy_db_name,
        &selected_users,
        source_schema.has_snapshots,
    )
    .await?;
    if !snapshot_violations.is_empty() && !dry_run {
        let details = snapshot_violations
            .iter()
            .map(|(user_id, count)| format!("{user_id} ({count})"))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(MemoriaError::Validation(format!(
            "legacy database still has active snapshots for: {details}. \
             Delete/recreate those snapshots manually before cutover; this CLI does not materialize \
             per-user snapshots from legacy shared-db snapshots."
        )));
    }

    for (user_id, count) in &snapshot_violations {
        warnings.push(format!(
            "user {user_id} still has {count} active legacy snapshots; execute mode would refuse cutover"
        ));
    }

    let pre_execute_account_snapshot = if dry_run {
        None
    } else {
        let snapshot_name = create_required_account_snapshot(&legacy_pool, &legacy_db_name).await?;
        eprintln!("Created pre-execute account snapshot {snapshot_name}");
        Some(snapshot_name)
    };

    let shared_store = if dry_run {
        None
    } else {
        eprintln!("Resetting target shared database {shared_db_name}");
        reset_database(shared_db_url).await?;
        let shared_store =
            SqlMemoryStore::connect(shared_db_url, embedding_dim, MIGRATION_INSTANCE_ID.into())
                .await?;
        shared_store.migrate_shared().await?;
        Some(shared_store)
    };
    let router = if dry_run {
        None
    } else {
        Some(Arc::new(
            DbRouter::connect(shared_db_url, embedding_dim, MIGRATION_INSTANCE_ID.into()).await?,
        ))
    };

    let shared_target_pool = shared_store
        .as_ref()
        .map(|store| store.pool())
        .unwrap_or(&legacy_pool);

    let mut shared_tables = Vec::new();
    for table in SHARED_DURABLE_TABLES {
        if !dry_run {
            eprintln!("Copying shared table {table}");
        }
        shared_tables.push(
            copy_shared_table(
                &legacy_pool,
                legacy_db_name.as_str(),
                shared_target_pool,
                shared_db_name.as_str(),
                table,
                !dry_run,
                cache.as_ref(),
            )
            .await?,
        );
    }

    let mut users = Vec::with_capacity(selected_users.len());
    let report_legacy_db_name = legacy_db_name.clone();

    if concurrency == 1 {
        // Serial path (original behavior)
        for user_id in &selected_users {
            if !dry_run {
                eprintln!("Migrating user {user_id}");
            }
            users.push(
                migrate_user(
                    &legacy_pool,
                    &legacy_db_name,
                    router.as_deref(),
                    user_id,
                    source_schema,
                    !dry_run,
                    cache.as_ref(),
                )
                .await?,
            );
        }
    } else {
        // Concurrent path using buffer_unordered (no tokio::spawn needed,
        // avoids Send + 'static lifetime constraints on async fns).
        use futures::stream::{self, StreamExt};

        let results: Vec<(&str, Result<UserMigrationReport, MemoriaError>)> =
            stream::iter(selected_users.iter())
                .map(|user_id| {
                    let pool = &legacy_pool;
                    let db_name: &str = &legacy_db_name;
                    let router_ref = router.as_deref();
                    let execute = !dry_run;
                    let cache = cache.clone();
                    async move {
                        if execute {
                            eprintln!("Migrating user {user_id}");
                        }
                        let res = migrate_user(
                            pool,
                            db_name,
                            router_ref,
                            user_id,
                            source_schema,
                            execute,
                            &cache,
                        )
                        .await;
                        (user_id.as_str(), res)
                    }
                })
                .buffer_unordered(concurrency)
                .collect()
                .await;

        for (user_id, result) in results {
            match result {
                Ok(report) => users.push(report),
                Err(e) => {
                    return Err(MemoriaError::Internal(format!(
                        "migration failed for user {user_id}: {e}"
                    )));
                }
            }
        }
    }
    users.sort_by(|a, b| a.user_id.cmp(&b.user_id));

    Ok(LegacyToMultiDbMigrationReport {
        dry_run,
        legacy_db_name: report_legacy_db_name,
        shared_db_name,
        pre_execute_account_snapshot,
        selected_users,
        shared_tables,
        skipped_shared_runtime_tables: SHARED_RUNTIME_TABLES
            .iter()
            .map(|table| (*table).to_string())
            .collect(),
        users,
        warnings,
    })
}

async fn migrate_user(
    legacy_pool: &MySqlPool,
    legacy_db_name: &str,
    router: Option<&DbRouter>,
    user_id: &str,
    source_schema: SourceSchemaAvailability,
    execute: bool,
    cache: &MigrationExecutionCache,
) -> Result<UserMigrationReport, MemoriaError> {
    let target_db = if execute {
        let router = router
            .ok_or_else(|| MemoriaError::Internal("missing router for execute mode".into()))?;
        let target_db = DbRouter::user_db_name_for_id(user_id);
        let target_url = router.user_db_url(&target_db)?;
        eprintln!("  Resetting target user database {target_db}");
        reset_database(&target_url).await?;
        router.invalidate_user(user_id).await?;
        // Register user in shared DB so runtime can discover it later
        router.register_user_db(user_id, &target_db).await?;
        target_db
    } else {
        DbRouter::user_db_name_for_id(user_id)
    };

    let mut tables = Vec::new();
    let mut branch_tables = Vec::new();
    let mut warnings = Vec::new();
    let active_branch = fetch_optional_active_branch(
        legacy_pool,
        legacy_db_name,
        user_id,
        source_schema.has_user_state,
    )
    .await?;
    let active_snapshot_count = count_active_snapshots(
        legacy_pool,
        legacy_db_name,
        user_id,
        source_schema.has_snapshots,
    )
    .await?;

    if execute {
        let router = router
            .ok_or_else(|| MemoriaError::Internal("missing router for execute mode".into()))?;
        // Use a lightweight temporary pool (max 1 connection) instead of the
        // cached user_store which keeps connections alive in a cache.
        let target_url = router.user_db_url(&target_db)?;
        let target_pool = connect_migration_pool(&target_url).await?;
        // Run user-schema migration on the fresh database
        let mut tmp_store = SqlMemoryStore::new(
            target_pool.clone(),
            router.embedding_dim(),
            MIGRATION_INSTANCE_ID.into(),
        );
        tmp_store.set_db_name(target_db.clone());
        tmp_store.set_database_url(target_url.clone());
        tmp_store.migrate_user_fresh().await?;
        let target_tables = cache.user_target_tables(&target_pool, &target_db).await?;
        let copy_ctx = TableCopyContext {
            source_pool: legacy_pool,
            source_db: legacy_db_name,
            target_pool: &target_pool,
            target_db: &target_db,
            cache,
        };
        for table in target_tables {
            eprintln!("  Copying user table {table}");
            tables.push(copy_user_scoped_table(&copy_ctx, &table, user_id, true).await?);
        }

        tables.push({
            eprintln!("  Copying user table mem_memories_stats");
            copy_memories_stats_table(
                legacy_pool,
                legacy_db_name,
                &target_pool,
                &target_db,
                user_id,
                true,
                cache,
            )
            .await?
        });

        let branches = load_branch_records(
            legacy_pool,
            legacy_db_name,
            user_id,
            source_schema.has_branches,
        )
        .await?;
        for branch in branches {
            eprintln!("  Copying branch table {}", branch.table_name);
            let mut report = copy_branch_table(
                legacy_pool,
                legacy_db_name,
                &target_pool,
                &target_db,
                &branch.table_name,
                true,
                cache,
            )
            .await?;
            let branch_note = format!("branch '{}'", branch.name);
            report.note = Some(match report.note.take() {
                Some(note) => format!("{branch_note}; {note}"),
                None => branch_note,
            });
            branch_tables.push(report);
        }

        if let Some(active_branch) = active_branch.as_deref() {
            if active_branch != "main" {
                let exists = branch_tables.iter().any(|table| {
                    table.table_name == active_branch
                        || table
                            .note
                            .as_deref()
                            .map(|note| note.contains(active_branch))
                            .unwrap_or(false)
                });
                if !exists {
                    warnings.push(format!(
                        "active branch '{active_branch}' is set in mem_user_state but was not copied as a physical branch table"
                    ));
                }
            }
        }
        // MatrixOne can stall on graceful sqlx pool shutdown here; dropping the
        // one-shot pool still releases the connection without blocking cutover.
        drop(target_pool);
    } else {
        let copy_ctx = TableCopyContext {
            source_pool: legacy_pool,
            source_db: legacy_db_name,
            target_pool: legacy_pool,
            target_db: &target_db,
            cache,
        };
        for table in list_source_user_scoped_tables(legacy_pool, legacy_db_name).await? {
            tables.push(copy_user_scoped_table(&copy_ctx, &table, user_id, false).await?);
        }
        tables.push(
            copy_memories_stats_table(
                legacy_pool,
                legacy_db_name,
                legacy_pool,
                &target_db,
                user_id,
                false,
                cache,
            )
            .await?,
        );
        for branch in load_branch_records(
            legacy_pool,
            legacy_db_name,
            user_id,
            source_schema.has_branches,
        )
        .await?
        {
            branch_tables.push(TableMigrationReport {
                table_name: branch.table_name.clone(),
                source_rows: count_all_rows(legacy_pool, legacy_db_name, &branch.table_name)
                    .await?,
                target_rows: None,
                status: "planned".to_string(),
                note: Some(format!("branch '{}'", branch.name)),
            });
        }
    }

    Ok(UserMigrationReport {
        user_id: user_id.to_string(),
        target_db,
        active_branch,
        active_snapshot_count,
        tables,
        branch_tables,
        warnings,
    })
}

fn migration_source_pool_max_connections(concurrency: usize) -> u32 {
    concurrency
        .max(DEFAULT_DISCOVERY_POOL_MAX_CONNECTIONS as usize)
        .min(MIGRATION_SOURCE_POOL_MAX_CONNECTIONS_UPPER as usize) as u32
}

async fn connect_pool(database_url: &str) -> Result<MySqlPool, MemoriaError> {
    connect_pool_with_max(database_url, DEFAULT_DISCOVERY_POOL_MAX_CONNECTIONS).await
}

async fn connect_pool_with_max(
    database_url: &str,
    max_connections: u32,
) -> Result<MySqlPool, MemoriaError> {
    MySqlPoolOptions::new()
        .max_connections(max_connections)
        .connect(database_url)
        .await
        .map_err(db_err)
}

async fn create_required_account_snapshot(
    pool: &MySqlPool,
    legacy_db_name: &str,
) -> Result<String, MemoriaError> {
    let snapshot_name = pre_execute_account_snapshot_name(legacy_db_name);
    sqlx::raw_sql(&format!("CREATE SNAPSHOT {snapshot_name} FOR ACCOUNT"))
        .execute(pool)
        .await
        .map_err(|err| {
            MemoriaError::Database(format!(
                "failed to create pre-migration account snapshot '{snapshot_name}': {err}"
            ))
        })?;
    if !snapshot_exists(pool, &snapshot_name).await? {
        return Err(MemoriaError::Internal(format!(
            "pre-migration account snapshot '{snapshot_name}' was not visible in SHOW SNAPSHOTS after creation"
        )));
    }
    Ok(snapshot_name)
}

async fn reset_database(database_url: &str) -> Result<(), MemoriaError> {
    let (base_url, db_name, _suffix) = split_database_url(database_url).ok_or_else(|| {
        MemoriaError::Validation("database URL is missing a database name".into())
    })?;
    let pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect(base_url)
        .await
        .map_err(db_err)?;
    sqlx::raw_sql(&format!("DROP DATABASE IF EXISTS {}", quote_ident(db_name)))
        .execute(&pool)
        .await
        .map_err(db_err)?;
    sqlx::raw_sql(&format!("CREATE DATABASE {}", quote_ident(db_name)))
        .execute(&pool)
        .await
        .map_err(db_err)?;
    // MatrixOne can stall on graceful sqlx pool shutdown after DROP/CREATE
    // DATABASE; dropping this one-shot pool avoids hanging execute mode.
    drop(pool);
    Ok(())
}

/// Lightweight pool (max 1 conn) for migration data copy. Caller should drop
/// this pool after finishing the per-user migration to release the connection.
async fn connect_migration_pool(database_url: &str) -> Result<MySqlPool, MemoriaError> {
    MySqlPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(30))
        .idle_timeout(std::time::Duration::from_secs(60))
        .connect(database_url)
        .await
        .map_err(db_err)
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

fn pre_execute_account_snapshot_name(legacy_db_name: &str) -> String {
    let suffix =
        &uuid::Uuid::new_v4().simple().to_string()[..PRE_EXECUTE_ACCOUNT_SNAPSHOT_SUFFIX_LEN];
    let max_fragment_len = MAX_IDENTIFIER_LEN.saturating_sub(
        PRE_EXECUTE_ACCOUNT_SNAPSHOT_PREFIX.len() + 1 + PRE_EXECUTE_ACCOUNT_SNAPSHOT_SUFFIX_LEN,
    );
    let fragment: String = sanitize_identifier_fragment(legacy_db_name)
        .chars()
        .take(max_fragment_len)
        .collect();
    format!("{PRE_EXECUTE_ACCOUNT_SNAPSHOT_PREFIX}{fragment}_{suffix}")
}

async fn snapshot_exists(pool: &MySqlPool, snapshot_name: &str) -> Result<bool, MemoriaError> {
    let rows = sqlx::query("SHOW SNAPSHOTS")
        .fetch_all(pool)
        .await
        .map_err(db_err)?;
    Ok(rows.iter().any(|row| {
        row.try_get::<String, _>("SNAPSHOT_NAME")
            .map(|name| name == snapshot_name)
            .unwrap_or(false)
    }))
}

fn parse_db_name(database_url: &str) -> Result<String, MemoriaError> {
    split_database_url(database_url)
        .map(|(_, db_name, _)| db_name.to_string())
        .ok_or_else(|| MemoriaError::Validation("database URL is missing a database name".into()))
}

fn split_database_url(database_url: &str) -> Option<(&str, &str, &str)> {
    let suffix_start = database_url.find(['?', '#']).unwrap_or(database_url.len());
    let (without_suffix, suffix) = database_url.split_at(suffix_start);
    let (base, db_name) = without_suffix.rsplit_once('/')?;
    if db_name.is_empty() {
        return None;
    }
    Some((base, db_name, suffix))
}

fn classify_runtime_topology(
    legacy_db_name: String,
    shared_db_name: String,
    mut legacy_users: Vec<String>,
    shared_users: BTreeSet<String>,
) -> RuntimeTopology {
    legacy_users.sort();
    legacy_users.dedup();
    if legacy_users.is_empty() {
        return if shared_users.is_empty() {
            RuntimeTopology::FreshSingleDb
        } else {
            RuntimeTopology::MultiDbReady
        };
    }

    let missing_users = legacy_users
        .iter()
        .filter(|user_id| !shared_users.contains(*user_id))
        .cloned()
        .collect::<Vec<_>>();
    if missing_users.is_empty() {
        RuntimeTopology::MultiDbReady
    } else {
        RuntimeTopology::PendingLegacyMigration(PendingLegacyMultiDbMigration {
            legacy_db_name,
            shared_db_name,
            legacy_users,
            missing_users,
        })
    }
}

async fn discover_users(pool: &MySqlPool, db_name: &str) -> Result<Vec<String>, MemoriaError> {
    let rows = sqlx::query(
        "SELECT DISTINCT table_name FROM information_schema.columns \
         WHERE table_schema = ? AND column_name = 'user_id' ORDER BY table_name",
    )
    .bind(db_name)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    let mut users = BTreeSet::new();
    for row in rows {
        let table_name: String = row.try_get("table_name").map_err(db_err)?;
        if USER_DISCOVERY_SKIP_TABLES.contains(&table_name.as_str()) {
            continue;
        }
        let sql = format!(
            "SELECT DISTINCT user_id FROM {} WHERE user_id <> ''",
            qualified_table(db_name, &table_name)
        );
        let user_rows = sqlx::query(&sql).fetch_all(pool).await.map_err(db_err)?;
        for row in user_rows {
            let user_id: String = row.try_get("user_id").map_err(db_err)?;
            if !user_id.is_empty() {
                users.insert(user_id);
            }
        }
    }
    Ok(users.into_iter().collect())
}

async fn load_active_shared_registry_users_or_empty(
    shared_db_url: &str,
    shared_db_name: &str,
) -> Result<BTreeSet<String>, MemoriaError> {
    let shared_pool = match connect_pool(shared_db_url).await {
        Ok(pool) => pool,
        Err(MemoriaError::Database(msg))
            if is_unknown_database_error_message(&msg, shared_db_name) =>
        {
            return Ok(BTreeSet::new());
        }
        Err(err) => return Err(err),
    };

    if !table_exists(&shared_pool, shared_db_name, "mem_user_registry").await? {
        return Ok(BTreeSet::new());
    }

    let rows = sqlx::query("SELECT user_id FROM mem_user_registry WHERE status = 'active'")
        .fetch_all(&shared_pool)
        .await
        .map_err(db_err)?;
    rows.into_iter()
        .map(|row| row.try_get("user_id").map_err(db_err))
        .collect::<Result<BTreeSet<String>, MemoriaError>>()
}

async fn collect_active_snapshot_violations(
    pool: &MySqlPool,
    db_name: &str,
    users: &[String],
    has_snapshots: bool,
) -> Result<Vec<(String, i64)>, MemoriaError> {
    if users.is_empty() || !has_snapshots {
        return Ok(vec![]);
    }
    let mut violations = Vec::new();
    for user_id in users {
        let count = count_active_snapshots(pool, db_name, user_id, true).await?;
        if count > 0 {
            violations.push((user_id.clone(), count));
        }
    }
    Ok(violations)
}

async fn fetch_optional_active_branch(
    pool: &MySqlPool,
    db_name: &str,
    user_id: &str,
    has_user_state: bool,
) -> Result<Option<String>, MemoriaError> {
    if !has_user_state {
        return Ok(None);
    }
    let sql = format!(
        "SELECT active_branch FROM {} WHERE user_id = ?",
        qualified_table(db_name, "mem_user_state")
    );
    let row = sqlx::query(&sql)
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .map_err(db_err)?;
    row.map(|row| row.try_get("active_branch").map_err(db_err))
        .transpose()
}

async fn count_active_snapshots(
    pool: &MySqlPool,
    db_name: &str,
    user_id: &str,
    has_snapshots: bool,
) -> Result<i64, MemoriaError> {
    if !has_snapshots {
        return Ok(0);
    }
    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE user_id = ? AND status = 'active'",
        qualified_table(db_name, "mem_snapshots")
    );
    sqlx::query_scalar::<_, i64>(&sql)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .map_err(db_err)
}

async fn load_branch_records(
    pool: &MySqlPool,
    db_name: &str,
    user_id: &str,
    has_branches: bool,
) -> Result<Vec<BranchRecord>, MemoriaError> {
    if !has_branches {
        return Ok(vec![]);
    }
    let sql = format!(
        "SELECT name, table_name FROM {} WHERE user_id = ? AND status = 'active' ORDER BY name",
        qualified_table(db_name, "mem_branches")
    );
    let rows = sqlx::query(&sql)
        .bind(user_id)
        .fetch_all(pool)
        .await
        .map_err(db_err)?;
    rows.iter()
        .map(|row| {
            Ok(BranchRecord {
                name: row.try_get("name").map_err(db_err)?,
                table_name: row.try_get("table_name").map_err(db_err)?,
            })
        })
        .collect()
}

async fn copy_shared_table(
    source_pool: &MySqlPool,
    source_db: &str,
    target_pool: &MySqlPool,
    target_db: &str,
    table_name: &str,
    execute: bool,
    cache: &MigrationExecutionCache,
) -> Result<TableMigrationReport, MemoriaError> {
    if !cache
        .source_table_exists(source_pool, source_db, table_name)
        .await?
    {
        return Ok(TableMigrationReport {
            table_name: table_name.to_string(),
            source_rows: 0,
            target_rows: None,
            status: "skipped".to_string(),
            note: Some("source table missing".to_string()),
        });
    }

    if !execute {
        let source_rows = count_all_rows(source_pool, source_db, table_name).await?;
        return Ok(TableMigrationReport {
            table_name: table_name.to_string(),
            source_rows,
            target_rows: None,
            status: "planned".to_string(),
            note: None,
        });
    }

    let columns = cache
        .copy_columns(source_pool, source_db, target_pool, target_db, table_name)
        .await?;
    let insert_sql = format!(
        "INSERT INTO {} ({cols}) SELECT {cols} FROM {}",
        qualified_table(target_db, table_name),
        qualified_table(source_db, table_name),
        cols = column_list(&columns),
    );
    let rows_copied = sqlx::raw_sql(&insert_sql)
        .execute(target_pool)
        .await
        .map_err(db_err)?
        .rows_affected() as i64;
    Ok(TableMigrationReport {
        table_name: table_name.to_string(),
        source_rows: rows_copied,
        target_rows: Some(rows_copied),
        status: "copied".to_string(),
        note: Some(execute_row_count_note(None)),
    })
}

async fn copy_user_scoped_table(
    ctx: &TableCopyContext<'_>,
    table_name: &str,
    user_id: &str,
    execute: bool,
) -> Result<TableMigrationReport, MemoriaError> {
    if !ctx
        .cache
        .source_table_exists(ctx.source_pool, ctx.source_db, table_name)
        .await?
    {
        return Ok(TableMigrationReport {
            table_name: table_name.to_string(),
            source_rows: 0,
            target_rows: None,
            status: "skipped".to_string(),
            note: Some("source table missing".to_string()),
        });
    }
    if !execute {
        let source_rows =
            count_rows_for_user(ctx.source_pool, ctx.source_db, table_name, user_id).await?;
        return Ok(TableMigrationReport {
            table_name: table_name.to_string(),
            source_rows,
            target_rows: None,
            status: "planned".to_string(),
            note: None,
        });
    }

    let columns = ctx
        .cache
        .copy_columns(
            ctx.source_pool,
            ctx.source_db,
            ctx.target_pool,
            ctx.target_db,
            table_name,
        )
        .await?;
    let quoted_user_id = quote_string_literal(user_id);
    let insert_sql = format!(
        "INSERT INTO {} ({cols}) SELECT {cols} FROM {} WHERE user_id = {quoted_user_id}",
        qualified_table(ctx.target_db, table_name),
        qualified_table(ctx.source_db, table_name),
        cols = column_list(&columns),
    );
    let rows_copied = sqlx::raw_sql(&insert_sql)
        .execute(ctx.target_pool)
        .await
        .map_err(db_err)?
        .rows_affected() as i64;
    Ok(TableMigrationReport {
        table_name: table_name.to_string(),
        source_rows: rows_copied,
        target_rows: Some(rows_copied),
        status: "copied".to_string(),
        note: Some(execute_row_count_note(Some("filtered by user_id"))),
    })
}

async fn copy_memories_stats_table(
    source_pool: &MySqlPool,
    source_db: &str,
    target_pool: &MySqlPool,
    target_db: &str,
    user_id: &str,
    execute: bool,
    cache: &MigrationExecutionCache,
) -> Result<TableMigrationReport, MemoriaError> {
    let table_name = "mem_memories_stats";
    if !cache
        .source_table_exists(source_pool, source_db, table_name)
        .await?
    {
        return Ok(TableMigrationReport {
            table_name: table_name.to_string(),
            source_rows: 0,
            target_rows: None,
            status: "skipped".to_string(),
            note: Some("source table missing".to_string()),
        });
    }
    if !execute && !table_exists(target_pool, target_db, table_name).await? {
        return Ok(TableMigrationReport {
            table_name: table_name.to_string(),
            source_rows: 0,
            target_rows: None,
            status: "skipped".to_string(),
            note: Some("target table missing".to_string()),
        });
    }

    if !execute {
        let source_rows = count_memories_stats_rows(source_pool, source_db, user_id).await?;
        return Ok(TableMigrationReport {
            table_name: table_name.to_string(),
            source_rows,
            target_rows: None,
            status: "planned".to_string(),
            note: Some("matched by memory_id ownership".to_string()),
        });
    }

    let columns = cache
        .copy_columns(source_pool, source_db, target_pool, target_db, table_name)
        .await?;
    let quoted_user_id = quote_string_literal(user_id);
    let insert_sql = format!(
        "INSERT INTO {} ({cols}) \
         SELECT {cols} FROM {} s \
         WHERE EXISTS (SELECT 1 FROM {} m WHERE m.memory_id = s.memory_id AND m.user_id = {quoted_user_id})",
        qualified_table(target_db, table_name),
        qualified_table(source_db, table_name),
        qualified_table(source_db, "mem_memories"),
        cols = column_list(&columns),
    );
    let rows_copied = sqlx::raw_sql(&insert_sql)
        .execute(target_pool)
        .await
        .map_err(db_err)?
        .rows_affected() as i64;
    Ok(TableMigrationReport {
        table_name: table_name.to_string(),
        source_rows: rows_copied,
        target_rows: Some(rows_copied),
        status: "copied".to_string(),
        note: Some(execute_row_count_note(Some(
            "matched by memory_id ownership",
        ))),
    })
}

async fn copy_branch_table(
    source_pool: &MySqlPool,
    source_db: &str,
    target_pool: &MySqlPool,
    target_db: &str,
    table_name: &str,
    execute: bool,
    cache: &MigrationExecutionCache,
) -> Result<TableMigrationReport, MemoriaError> {
    if !cache
        .source_table_exists(source_pool, source_db, table_name)
        .await?
    {
        return Err(MemoriaError::Validation(format!(
            "branch table '{table_name}' is registered but missing in source database"
        )));
    }
    if !execute {
        let source_rows = count_all_rows(source_pool, source_db, table_name).await?;
        return Ok(TableMigrationReport {
            table_name: table_name.to_string(),
            source_rows,
            target_rows: None,
            status: "planned".to_string(),
            note: None,
        });
    }

    let drop_sql = format!(
        "DROP TABLE IF EXISTS {}",
        qualified_table(target_db, table_name)
    );
    sqlx::raw_sql(&drop_sql)
        .execute(target_pool)
        .await
        .map_err(db_err)?;

    let create_like_sql = format!(
        "CREATE TABLE {} LIKE {}",
        qualified_table(target_db, table_name),
        qualified_table(source_db, table_name),
    );
    if sqlx::raw_sql(&create_like_sql)
        .execute(target_pool)
        .await
        .is_err()
    {
        let create_ctas_sql = format!(
            "CREATE TABLE {} AS SELECT * FROM {} WHERE 1 = 0",
            qualified_table(target_db, table_name),
            qualified_table(source_db, table_name),
        );
        sqlx::raw_sql(&create_ctas_sql)
            .execute(target_pool)
            .await
            .map_err(db_err)?;
    }

    let columns = cache
        .copy_columns(source_pool, source_db, target_pool, target_db, table_name)
        .await?;
    let insert_sql = format!(
        "INSERT INTO {} ({cols}) SELECT {cols} FROM {}",
        qualified_table(target_db, table_name),
        qualified_table(source_db, table_name),
        cols = column_list(&columns),
    );
    let rows_copied = sqlx::raw_sql(&insert_sql)
        .execute(target_pool)
        .await
        .map_err(db_err)?
        .rows_affected() as i64;
    Ok(TableMigrationReport {
        table_name: table_name.to_string(),
        source_rows: rows_copied,
        target_rows: Some(rows_copied),
        status: "copied".to_string(),
        note: Some(execute_row_count_note(None)),
    })
}

async fn table_exists(
    pool: &MySqlPool,
    db_name: &str,
    table_name: &str,
) -> Result<bool, MemoriaError> {
    let count = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = ? AND table_name = ?",
    )
    .bind(db_name)
    .bind(table_name)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    Ok(count > 0)
}

async fn list_tables_with_user_id(
    pool: &MySqlPool,
    db_name: &str,
) -> Result<Vec<String>, MemoriaError> {
    let rows = sqlx::query(
        "SELECT DISTINCT table_name FROM information_schema.columns \
         WHERE table_schema = ? AND column_name = 'user_id' ORDER BY table_name",
    )
    .bind(db_name)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    rows.iter()
        .map(|row| row.try_get("table_name").map_err(db_err))
        .collect()
}

async fn list_source_user_scoped_tables(
    pool: &MySqlPool,
    db_name: &str,
) -> Result<Vec<String>, MemoriaError> {
    Ok(list_tables_with_user_id(pool, db_name)
        .await?
        .into_iter()
        .filter(|table| {
            !USER_MIGRATION_SKIP_TABLES.contains(&table.as_str())
                && !is_physical_branch_table(table)
        })
        .collect())
}

async fn list_columns(
    pool: &MySqlPool,
    db_name: &str,
    table_name: &str,
) -> Result<Vec<ColumnSpec>, MemoriaError> {
    let rows = sqlx::query(
        "SELECT column_name, is_nullable, column_default, extra \
         FROM information_schema.columns \
         WHERE table_schema = ? AND table_name = ? ORDER BY ordinal_position",
    )
    .bind(db_name)
    .bind(table_name)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    rows.iter()
        .map(|row| {
            let extra: String = row.try_get("extra").unwrap_or_default();
            Ok(ColumnSpec {
                name: row.try_get("column_name").map_err(db_err)?,
                nullable: row
                    .try_get::<String, _>("is_nullable")
                    .map_err(db_err)?
                    .eq_ignore_ascii_case("YES"),
                has_default: row
                    .try_get::<Option<String>, _>("column_default")
                    .map_err(db_err)?
                    .is_some(),
                auto_increment: extra.contains("auto_increment"),
            })
        })
        .collect()
}

fn copyable_columns(
    source_columns: &[ColumnSpec],
    target_columns: &[ColumnSpec],
    table_name: &str,
) -> Result<Vec<String>, MemoriaError> {
    let source_names = source_columns
        .iter()
        .filter(|column| !is_internal_matrixone_column(&column.name))
        .map(|column| column.name.as_str())
        .collect::<BTreeSet<_>>();
    let mut columns = Vec::new();
    for column in target_columns {
        if is_internal_matrixone_column(&column.name) {
            continue;
        }
        if column.auto_increment {
            continue;
        }
        if source_names.contains(column.name.as_str()) {
            columns.push(column.name.clone());
        } else if !column.nullable && !column.has_default && !column.auto_increment {
            return Err(MemoriaError::Validation(format!(
                "cannot migrate table '{table_name}': source is missing required target column '{}'",
                column.name
            )));
        }
    }
    if columns.is_empty() {
        return Err(MemoriaError::Validation(format!(
            "cannot migrate table '{table_name}': no shared columns between source and target"
        )));
    }
    Ok(columns)
}

fn is_internal_matrixone_column(name: &str) -> bool {
    name.starts_with("__mo_")
}

fn quote_string_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

async fn count_all_rows(
    pool: &MySqlPool,
    db_name: &str,
    table_name: &str,
) -> Result<i64, MemoriaError> {
    let sql = format!(
        "SELECT COUNT(*) FROM {}",
        qualified_table(db_name, table_name)
    );
    sqlx::query_scalar::<_, i64>(&sql)
        .fetch_one(pool)
        .await
        .map_err(db_err)
}

async fn count_rows_for_user(
    pool: &MySqlPool,
    db_name: &str,
    table_name: &str,
    user_id: &str,
) -> Result<i64, MemoriaError> {
    let sql = format!(
        "SELECT COUNT(*) FROM {} WHERE user_id = ?",
        qualified_table(db_name, table_name)
    );
    sqlx::query_scalar::<_, i64>(&sql)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .map_err(db_err)
}

async fn count_memories_stats_rows(
    pool: &MySqlPool,
    db_name: &str,
    user_id: &str,
) -> Result<i64, MemoriaError> {
    let sql = format!(
        "SELECT COUNT(*) FROM {} s \
         WHERE EXISTS (SELECT 1 FROM {} m WHERE m.memory_id = s.memory_id AND m.user_id = ?)",
        qualified_table(db_name, "mem_memories_stats"),
        qualified_table(db_name, "mem_memories"),
    );
    sqlx::query_scalar::<_, i64>(&sql)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .map_err(db_err)
}

fn normalize_user_ids(user_ids: &[String]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for user_id in user_ids {
        let trimmed = user_id.trim();
        if !trimmed.is_empty() {
            set.insert(trimmed.to_string());
        }
    }
    set.into_iter().collect()
}

fn qualified_table(db_name: &str, table_name: &str) -> String {
    format!("{}.{}", quote_ident(db_name), quote_ident(table_name))
}

fn column_list(columns: &[String]) -> String {
    columns
        .iter()
        .map(|column| quote_ident(column))
        .collect::<Vec<_>>()
        .join(", ")
}

fn quote_ident(ident: &str) -> String {
    format!("`{}`", ident.replace('`', "``"))
}

fn is_physical_branch_table(table_name: &str) -> bool {
    table_name.starts_with("br_")
}

fn db_err(e: sqlx::Error) -> MemoriaError {
    MemoriaError::Database(e.to_string())
}

fn is_unknown_database_error_message(message: &str, db_name: &str) -> bool {
    message.contains("1049")
        && message.contains("Unknown database")
        && (message.contains(db_name)
            || message.contains(&format!("'{db_name}'"))
            || message.contains(&format!("`{db_name}`")))
}

#[cfg(test)]
mod tests {
    use super::{
        classify_runtime_topology, copyable_columns, migration_source_pool_max_connections,
        pre_execute_account_snapshot_name, quote_ident, sanitize_identifier_fragment, ColumnSpec,
        PendingLegacyMultiDbMigration, RuntimeTopology, DEFAULT_DISCOVERY_POOL_MAX_CONNECTIONS,
        MAX_IDENTIFIER_LEN, MIGRATION_SOURCE_POOL_MAX_CONNECTIONS_UPPER,
        PRE_EXECUTE_ACCOUNT_SNAPSHOT_PREFIX, PRE_EXECUTE_ACCOUNT_SNAPSHOT_SUFFIX_LEN,
    };
    use std::collections::BTreeSet;

    fn col(name: &str, nullable: bool, has_default: bool, auto_increment: bool) -> ColumnSpec {
        ColumnSpec {
            name: name.to_string(),
            nullable,
            has_default,
            auto_increment,
        }
    }

    #[test]
    fn copyable_columns_skip_defaulted_target_columns() {
        let source = vec![
            col("id", false, false, false),
            col("value", false, false, false),
        ];
        let target = vec![
            col("id", false, false, false),
            col("value", false, false, false),
            col("method", false, true, false),
        ];
        let columns = copyable_columns(&source, &target, "demo").expect("copy columns");
        assert_eq!(columns, vec!["id".to_string(), "value".to_string()]);
    }

    #[test]
    fn copyable_columns_reject_missing_required_target_columns() {
        let source = vec![col("user_id", false, false, false)];
        let target = vec![
            col("id", false, false, false),
            col("user_id", false, false, false),
        ];
        let err = copyable_columns(&source, &target, "mem_branches").expect_err("should fail");
        assert!(err
            .to_string()
            .contains("missing required target column 'id'"));
    }

    #[test]
    fn quote_ident_escapes_backticks() {
        assert_eq!(quote_ident("a`b"), "`a``b`");
    }

    #[test]
    fn classify_runtime_topology_detects_fresh_single_db() {
        let topology = classify_runtime_topology(
            "memoria".to_string(),
            "memoria_shared".to_string(),
            vec![],
            BTreeSet::new(),
        );

        assert_eq!(topology, RuntimeTopology::FreshSingleDb);
    }

    #[test]
    fn classify_runtime_topology_detects_pending_migration() {
        let topology = classify_runtime_topology(
            "memoria".to_string(),
            "memoria_shared".to_string(),
            vec!["bob".to_string(), "alice".to_string()],
            BTreeSet::from(["alice".to_string()]),
        );

        assert_eq!(
            topology,
            RuntimeTopology::PendingLegacyMigration(PendingLegacyMultiDbMigration {
                legacy_db_name: "memoria".to_string(),
                shared_db_name: "memoria_shared".to_string(),
                legacy_users: vec!["alice".to_string(), "bob".to_string()],
                missing_users: vec!["bob".to_string()],
            })
        );
    }

    #[test]
    fn classify_runtime_topology_detects_multi_db_ready() {
        let topology = classify_runtime_topology(
            "memoria".to_string(),
            "memoria_shared".to_string(),
            vec!["bob".to_string(), "alice".to_string()],
            BTreeSet::from(["alice".to_string(), "bob".to_string()]),
        );

        assert_eq!(topology, RuntimeTopology::MultiDbReady);
    }

    #[test]
    fn classify_runtime_topology_detects_multi_db_ready_without_legacy_users() {
        let topology = classify_runtime_topology(
            "memoria".to_string(),
            "memoria_shared".to_string(),
            vec![],
            BTreeSet::from(["alice".to_string()]),
        );

        assert_eq!(topology, RuntimeTopology::MultiDbReady);
    }

    #[test]
    fn sanitize_identifier_fragment_rewrites_non_identifier_chars() {
        assert_eq!(
            sanitize_identifier_fragment("memoria-legacy.cli/e2e"),
            "memoria_legacy_cli_e2e"
        );
    }

    #[test]
    fn pre_execute_account_snapshot_name_is_bounded() {
        let name =
            pre_execute_account_snapshot_name("memoria_immersive_r1_legacy_e8c60c_extra_suffix");
        assert!(name.len() <= MAX_IDENTIFIER_LEN);
        assert!(name.starts_with(PRE_EXECUTE_ACCOUNT_SNAPSHOT_PREFIX));
        let suffix = name.rsplit('_').next().expect("snapshot suffix");
        assert_eq!(suffix.len(), PRE_EXECUTE_ACCOUNT_SNAPSHOT_SUFFIX_LEN);
    }

    #[test]
    fn migration_source_pool_scales_with_concurrency() {
        assert_eq!(
            migration_source_pool_max_connections(1),
            DEFAULT_DISCOVERY_POOL_MAX_CONNECTIONS
        );
        assert_eq!(migration_source_pool_max_connections(6), 6);
        assert_eq!(
            migration_source_pool_max_connections(256),
            MIGRATION_SOURCE_POOL_MAX_CONNECTIONS_UPPER
        );
    }
}
