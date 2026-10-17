use crate::keyed_gates::KeyedGates;
use crate::pool_config::{
    configured_multi_db_pool_budget, configured_multi_db_pool_size, multi_db_pool_default_size,
    multi_db_pool_max_size, split_pool_budget, MultiDbPoolKind, MULTI_DB_POOL_BUDGET_MAX,
};
use crate::store::spawn_pool_monitor;
use crate::{PoolHealthSnapshot, SqlMemoryStore};
use chrono::Utc;
use memoria_core::MemoriaError;
use moka::sync::Cache;
use sha2::{Digest, Sha256};
use sqlx::{
    mysql::{MySqlConnectOptions, MySqlPoolOptions},
    MySqlPool, Row,
};
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Semaphore;

fn db_err(e: sqlx::Error) -> MemoriaError {
    MemoriaError::Database(e.to_string())
}

fn is_duplicate_key_error(e: &sqlx::Error) -> bool {
    use sqlx::mysql::MySqlDatabaseError;

    e.as_database_error()
        .and_then(|de| de.as_error().downcast_ref::<MySqlDatabaseError>())
        .map(|me| me.number() == 1062)
        .unwrap_or(false)
}

const USER_DB_CACHE_MAX_CAPACITY: u64 = 10_000;
const USER_STORE_CACHE_MAX_CAPACITY: u64 = 10_000;
const USER_SCHEMA_CACHE_MAX_CAPACITY: u64 = 10_000;
const USER_STORE_CACHE_IDLE_SECS: u64 = 600;
const MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV: &str = "MEMORIA_MERGED_SHARED_POOL_MAX_CONNECTIONS";
const LEGACY_SHARED_COMPONENT_WEIGHTS: [u32; 3] = [40, 40, 20];

// User IDs may contain arbitrary prefixes. Keep scope identity typed in every
// shared cache and gate rather than relying on a reserved string namespace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ScopeKey {
    User(String),
    Group(String),
}

fn configured_router_timeout(name: &str, default_secs: u64) -> Duration {
    let secs = std::env::var(name)
        .ok()
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(default_secs)
        .clamp(1, 3600);
    Duration::from_secs(secs)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SharedPoolPlan {
    max_connections: u32,
    routing_component_max_connections: u32,
    shared_main_component_max_connections: u32,
    git_component_max_connections: u32,
    explicit_override: bool,
}

#[derive(Debug, Clone)]
pub struct UserDatabaseRecord {
    pub user_id: String,
    pub db_name: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct GroupDatabaseRecord {
    pub group_id: String,
    pub db_name: String,
    pub owner_user_id: String,
    pub status: String,
}

#[derive(Clone)]
pub struct DbRouter {
    shared_pool: MySqlPool,
    shared_pool_max_connections: u32,
    /// Global pool for all per-user DB queries. Routed stores target the
    /// correct database via fully-qualified table names. Routed pools explicitly
    /// disable statement caching as a configuration policy.
    global_user_pool: MySqlPool,
    global_user_pool_max_connections: u32,
    user_init_pool: MySqlPool,
    user_init_pool_max_connections: u32,
    shared_db_url: String,
    shared_db_name: String,
    embedding_dim: usize,
    instance_id: String,
    user_db_cache: Cache<String, String>,
    user_schema_cache: Cache<ScopeKey, bool>,
    user_store_cache: Cache<ScopeKey, Arc<SqlMemoryStore>>,
    cache_gates: KeyedGates<ScopeKey>,
    init_timeout: Duration,
    invalidation_timeout: Duration,
    user_init_semaphore: Arc<Semaphore>,
}

impl DbRouter {
    pub async fn connect(
        shared_db_url: &str,
        embedding_dim: usize,
        instance_id: String,
    ) -> Result<Self, MemoriaError> {
        create_database_if_missing_from_url(shared_db_url).await?;
        let pool_budget = configured_multi_db_pool_budget();
        tracing::info!(
            pool_budget,
            pool_budget_max = MULTI_DB_POOL_BUDGET_MAX,
            shared_merged_default = multi_db_pool_default_size(MultiDbPoolKind::SharedMerged),
            global_user_default = multi_db_pool_default_size(MultiDbPoolKind::GlobalUser),
            auth_default = multi_db_pool_default_size(MultiDbPoolKind::Auth),
            user_init_default = multi_db_pool_default_size(MultiDbPoolKind::UserInit),
            governance_default = multi_db_pool_default_size(MultiDbPoolKind::Governance),
            "Multi-db pool budget resolved"
        );
        let shared_plan = configured_shared_pool_plan();
        let pool = MySqlPoolOptions::new()
            .max_connections(shared_plan.max_connections)
            .max_lifetime(std::time::Duration::from_secs(3600))
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(shared_db_url)
            .await
            .map_err(db_err)?;
        tracing::info!(
            max_connections = shared_plan.max_connections,
            routing_component_max_connections = shared_plan.routing_component_max_connections,
            shared_main_component_max_connections =
                shared_plan.shared_main_component_max_connections,
            git_component_max_connections = shared_plan.git_component_max_connections,
            explicit_override = shared_plan.explicit_override,
            "Shared database pool initialized"
        );

        // Global pool for all per-user DB queries.
        // Apply the existing disabled-cache policy with typed connect options.
        let global_max = configured_multi_db_pool_size(
            "MEMORIA_GLOBAL_USER_POOL_MAX",
            MultiDbPoolKind::GlobalUser,
        );
        let user_options = user_pool_options(shared_db_url)?;
        let global_user_pool = MySqlPoolOptions::new()
            .max_connections(global_max)
            .min_connections(global_max.min(2))
            .max_lifetime(std::time::Duration::from_secs(3600))
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(15))
            .connect_with(user_options.clone())
            .await
            .map_err(db_err)?;
        tracing::info!(
            max_connections = global_max,
            "Global user pool initialized (statement_cache=0)"
        );
        spawn_pool_monitor(
            global_user_pool.clone(),
            Some(global_max),
            Arc::new(std::sync::Mutex::new(PoolHealthSnapshot::new(Some(
                global_max,
            )))),
            "global_user_pool",
        );
        let user_init_pool_max = configured_multi_db_pool_size(
            "MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS",
            MultiDbPoolKind::UserInit,
        );
        let user_init_pool = MySqlPoolOptions::new()
            .max_connections(user_init_pool_max)
            .min_connections(0)
            .max_lifetime(std::time::Duration::from_secs(3600))
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect_with(user_options)
            .await
            .map_err(db_err)?;
        tracing::info!(
            max_connections = user_init_pool_max,
            "User schema init pool initialized (statement_cache=0)"
        );

        let shared_db_name = parse_db_name(shared_db_url)
            .ok_or_else(|| MemoriaError::Internal("invalid shared_db_url".into()))?;
        let user_init_max: usize = std::env::var("MEMORIA_USER_SCHEMA_INIT_MAX_CONCURRENCY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(6)
            .clamp(1, 64);
        let router = Self {
            shared_pool: pool,
            shared_pool_max_connections: shared_plan.max_connections,
            global_user_pool,
            global_user_pool_max_connections: global_max,
            user_init_pool,
            user_init_pool_max_connections: user_init_pool_max,
            shared_db_url: shared_db_url.to_string(),
            shared_db_name,
            embedding_dim,
            instance_id,
            user_db_cache: Cache::builder()
                .max_capacity(USER_DB_CACHE_MAX_CAPACITY)
                .time_to_live(std::time::Duration::from_secs(300))
                .build(),
            user_schema_cache: Cache::builder()
                .max_capacity(USER_SCHEMA_CACHE_MAX_CAPACITY)
                .build(),
            user_store_cache: Cache::builder()
                .max_capacity(USER_STORE_CACHE_MAX_CAPACITY)
                .time_to_idle(std::time::Duration::from_secs(USER_STORE_CACHE_IDLE_SECS))
                .build(),
            cache_gates: KeyedGates::default(),
            init_timeout: configured_router_timeout("MEMORIA_ROUTER_INIT_TIMEOUT_SECS", 120),
            invalidation_timeout: configured_router_timeout(
                "MEMORIA_ROUTER_INVALIDATION_TIMEOUT_SECS",
                15,
            ),
            user_init_semaphore: Arc::new(Semaphore::new(user_init_max)),
        };
        router.ensure_user_registry_table().await?;
        Ok(router)
    }

    pub fn shared_pool(&self) -> &MySqlPool {
        &self.shared_pool
    }

    pub fn shared_pool_max_connections(&self) -> u32 {
        self.shared_pool_max_connections
    }

    pub fn global_user_pool(&self) -> &MySqlPool {
        &self.global_user_pool
    }

    pub fn global_user_pool_max_connections(&self) -> u32 {
        self.global_user_pool_max_connections
    }

    pub fn user_init_pool_max_connections(&self) -> u32 {
        self.user_init_pool_max_connections
    }

    pub fn shared_db_name(&self) -> &str {
        &self.shared_db_name
    }

    pub fn shared_db_url(&self) -> &str {
        &self.shared_db_url
    }

    pub fn user_db_name_for_id(user_id: &str) -> String {
        let digest = Sha256::digest(user_id.as_bytes());
        let mut hex = String::with_capacity(16);
        for b in digest.iter().take(8) {
            use std::fmt::Write as _;
            let _ = write!(&mut hex, "{b:02x}");
        }
        format!("mem_u_{hex}")
    }

    pub fn user_db_url(&self, db_name: &str) -> Result<String, MemoriaError> {
        replace_db_name(&self.shared_db_url, db_name)
            .ok_or_else(|| MemoriaError::Internal("invalid shared_db_url".into()))
    }

    pub fn embedding_dim(&self) -> usize {
        self.embedding_dim
    }

    /// Register (or re-activate) a user→db mapping in mem_user_registry.
    pub async fn register_user_db(&self, user_id: &str, db_name: &str) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        let insert = sqlx::query(
            r#"INSERT INTO mem_user_registry (user_id, db_name, status, created_at, updated_at)
               VALUES (?, ?, 'active', ?, ?)"#,
        )
        .bind(user_id)
        .bind(db_name)
        .bind(now)
        .bind(now)
        .execute(&self.shared_pool)
        .await;

        match insert {
            Ok(_) => Ok(()),
            Err(e) if is_duplicate_key_error(&e) => {
                sqlx::query(
                    "UPDATE mem_user_registry SET db_name = ?, status = 'active', updated_at = ? WHERE user_id = ?",
                )
                .bind(db_name)
                .bind(now)
                .bind(user_id)
                .execute(&self.shared_pool)
                .await
                .map_err(db_err)?;
                Ok(())
            }
            Err(e) => Err(db_err(e)),
        }
    }

    pub async fn list_registered_users(&self) -> Result<Vec<UserDatabaseRecord>, MemoriaError> {
        let rows =
            sqlx::query("SELECT user_id, db_name, status FROM mem_user_registry ORDER BY user_id")
                .fetch_all(&self.shared_pool)
                .await
                .map_err(db_err)?;
        rows.iter()
            .map(|row| {
                Ok(UserDatabaseRecord {
                    user_id: row.try_get("user_id").map_err(db_err)?,
                    db_name: row.try_get("db_name").map_err(db_err)?,
                    status: row.try_get("status").map_err(db_err)?,
                })
            })
            .collect()
    }

    pub async fn list_active_users(&self) -> Result<Vec<String>, MemoriaError> {
        let rows = sqlx::query("SELECT user_id FROM mem_user_registry WHERE status = 'active'")
            .fetch_all(&self.shared_pool)
            .await
            .map_err(db_err)?;
        rows.iter()
            .map(|row| row.try_get("user_id").map_err(db_err))
            .collect()
    }

    pub async fn user_db_name(&self, user_id: &str) -> Result<String, MemoriaError> {
        if let Some(cached) = self.user_db_cache.get(user_id) {
            return Ok(cached);
        }

        let key = ScopeKey::User(user_id.to_owned());
        self.cache_operation(key, self.init_timeout, "database mapping lookup", async {
            if let Some(cached) = self.user_db_cache.get(user_id) {
                return Ok(cached);
            }
            // Avoid moka single-flight here: its init future makes upstream callers
            // fail Send bounds under axum handlers and tokio::spawn.
            let shared_pool = self.shared_pool.clone();
            let user_id_owned = user_id.to_string();
            let row = sqlx::query(
                "SELECT db_name FROM mem_user_registry WHERE user_id = ? AND status = 'active'",
            )
            .bind(&user_id_owned)
            .fetch_optional(&shared_pool)
            .await
            .map_err(db_err)?;
            let db_name = match row {
                Some(row) => row.try_get("db_name").map_err(db_err)?,
                None => provision_user_db_with_pool(&shared_pool, &user_id_owned).await?,
            };
            self.user_db_cache.insert(user_id_owned, db_name.clone());
            Ok(db_name)
        })
        .await
    }

    pub async fn user_store(&self, user_id: &str) -> Result<Arc<SqlMemoryStore>, MemoriaError> {
        let cache_key = ScopeKey::User(user_id.to_owned());
        if let Some(cached) = self.user_store_cache.get(&cache_key) {
            return Ok(cached);
        }

        self.cache_operation(
            cache_key.clone(),
            self.init_timeout,
            "user initialization",
            async {
                if let Some(cached) = self.user_store_cache.get(&cache_key) {
                    return Ok(cached);
                }
                // Avoid moka single-flight here: its init future makes upstream callers
                // fail Send bounds under axum handlers and tokio::spawn.
                let shared_pool = self.shared_pool.clone();
                let global_user_pool = self.global_user_pool.clone();
                let user_init_pool = self.user_init_pool.clone();
                let shared_db_url = self.shared_db_url.clone();
                let embedding_dim = self.embedding_dim;
                let instance_id = self.instance_id.clone();
                let user_db_cache = self.user_db_cache.clone();
                let user_schema_cache = self.user_schema_cache.clone();
                let user_init_semaphore = self.user_init_semaphore.clone();
                let user_id_owned = user_id.to_string();
                let existing = sqlx::query(
                    "SELECT db_name FROM mem_user_registry WHERE user_id = ? AND status = 'active'",
                )
                .bind(&user_id_owned)
                .fetch_optional(&shared_pool)
                .await
                .map_err(db_err)?;
                let (db_name, needs_init) = match existing {
                    Some(row) => (row.try_get("db_name").map_err(db_err)?, false),
                    None => (
                        provision_user_db_with_pool(&shared_pool, &user_id_owned).await?,
                        true,
                    ),
                };
                if user_schema_cache.get(&cache_key).is_none() {
                    let _permit = user_init_semaphore.acquire_owned().await.map_err(|_| {
                        MemoriaError::Internal("user schema init semaphore closed".into())
                    })?;
                    if user_schema_cache.get(&cache_key).is_none() {
                        let init_store = build_routed_store(
                            user_init_pool.clone(),
                            &shared_db_url,
                            embedding_dim,
                            &instance_id,
                            &db_name,
                        )?;
                        let init_result = init_store.migrate_user().await;
                        if let Err(err) = init_result {
                            if needs_init {
                                let delete_registration =
                                    "DELETE FROM mem_user_registry WHERE user_id = ? AND db_name = ?";
                                let _ = sqlx::query(delete_registration)
                                    .bind(&user_id_owned)
                                    .bind(&db_name)
                                    .execute(&shared_pool)
                                    .await;
                            }
                            return Err(err);
                        }
                        user_schema_cache.insert(cache_key.clone(), true);
                    }
                }
                user_db_cache.insert(user_id_owned.clone(), db_name.clone());
                let store = Arc::new(build_routed_store(
                    global_user_pool,
                    &shared_db_url,
                    embedding_dim,
                    &instance_id,
                    &db_name,
                )?);
                self.user_store_cache
                    .insert(cache_key.clone(), store.clone());
                Ok(store)
            },
        )
        .await
    }

    pub fn routed_store_for_user(&self, user_id: &str) -> Result<SqlMemoryStore, MemoriaError> {
        let db_name = Self::user_db_name_for_id(user_id);
        self.routed_store_for_db_name(&db_name)
    }

    pub fn routed_store_for_db_name(&self, db_name: &str) -> Result<SqlMemoryStore, MemoriaError> {
        build_routed_store(
            self.global_user_pool.clone(),
            &self.shared_db_url,
            self.embedding_dim,
            &self.instance_id,
            db_name,
        )
    }

    /// Wait for an in-flight cold path, then clear its published caches.
    /// A timeout returns an error without reporting successful invalidation.
    /// Existing store handles and database mutations are not cancelled.
    pub async fn invalidate_user(&self, user_id: &str) -> Result<(), MemoriaError> {
        self.invalidate_cached_store(ScopeKey::User(user_id.to_owned()))
            .await
    }

    pub async fn invalidate_group(&self, group_id: &str) -> Result<(), MemoriaError> {
        self.invalidate_cached_store(ScopeKey::Group(group_id.to_owned()))
            .await
    }

    /// Use the same user/group dispatch as `scope_store()`.
    pub async fn invalidate_scope(&self, scope_id: &str) -> Result<(), MemoriaError> {
        if scope_id.starts_with("grp_") {
            self.invalidate_group(scope_id).await
        } else {
            self.invalidate_user(scope_id).await
        }
    }

    async fn invalidate_cached_store(&self, cache_key: ScopeKey) -> Result<(), MemoriaError> {
        self.cache_operation(
            cache_key.clone(),
            self.invalidation_timeout,
            "cache invalidation",
            async {
                if let ScopeKey::User(user_id) = &cache_key {
                    self.user_db_cache.invalidate(user_id);
                }
                self.user_schema_cache.invalidate(&cache_key);
                self.user_store_cache.invalidate(&cache_key);
                Ok(())
            },
        )
        .await
    }

    async fn cache_operation<T>(
        &self,
        key: ScopeKey,
        deadline: Duration,
        operation: &'static str,
        work: impl Future<Output = Result<T, MemoriaError>>,
    ) -> Result<T, MemoriaError> {
        // Do not spawn the work: a timeout must drop its future and release the
        // gate, rather than let a detached initializer publish stale caches.
        self.cache_gates
            .run(key, deadline, work)
            .await
            .map_err(|_| {
                MemoriaError::Database(format!(
                    "routed {operation} timed out after {}ms",
                    deadline.as_millis()
                ))
            })?
    }

    async fn ensure_user_registry_table(&self) -> Result<(), MemoriaError> {
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_user_registry (
                user_id     VARCHAR(64)  PRIMARY KEY,
                db_name     VARCHAR(128) NOT NULL UNIQUE,
                status      VARCHAR(20)  NOT NULL DEFAULT 'active',
                created_at  DATETIME(6)  NOT NULL,
                updated_at  DATETIME(6)  NOT NULL,
                INDEX idx_status (status)
            )"#,
        )
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;

        // ── Operational metrics tables (push-based aggregation for admin dashboard) ──
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS srv_user_stats (
                user_id           VARCHAR(64) NOT NULL PRIMARY KEY,
                total_memories    BIGINT      NOT NULL DEFAULT 0,
                active_memories   BIGINT      NOT NULL DEFAULT 0,
                inactive_memories BIGINT      NOT NULL DEFAULT 0,
                total_entities    BIGINT      NOT NULL DEFAULT 0,
                total_edits       BIGINT      NOT NULL DEFAULT 0,
                updated_at        DATETIME    NOT NULL DEFAULT CURRENT_TIMESTAMP
                                  ON UPDATE CURRENT_TIMESTAMP
            )"#,
        )
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS srv_user_metric_detail (
                user_id   VARCHAR(64)  NOT NULL,
                metric    VARCHAR(64)  NOT NULL,
                dim_key   VARCHAR(128) NOT NULL,
                cnt       BIGINT       NOT NULL DEFAULT 0,
                PRIMARY KEY (user_id, metric, dim_key)
            )"#,
        )
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS srv_daily_stats (
                dt      DATE        NOT NULL,
                metric  VARCHAR(64) NOT NULL,
                cnt     BIGINT      NOT NULL DEFAULT 0,
                PRIMARY KEY (dt, metric)
            )"#,
        )
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS srv_user_api_stats (
                user_id        VARCHAR(64) NOT NULL PRIMARY KEY,
                total_calls    BIGINT      NOT NULL DEFAULT 0,
                mcp_calls      BIGINT      NOT NULL DEFAULT 0,
                mcp_errors     BIGINT      NOT NULL DEFAULT 0,
                first_mcp_call DATETIME    DEFAULT NULL,
                updated_at     DATETIME    NOT NULL DEFAULT CURRENT_TIMESTAMP
                               ON UPDATE CURRENT_TIMESTAMP
            )"#,
        )
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS srv_mcp_path_stats (
                path  VARCHAR(128) NOT NULL PRIMARY KEY,
                cnt   BIGINT       NOT NULL DEFAULT 0
            )"#,
        )
        .execute(&self.shared_pool)
        .await
        .map_err(db_err)?;

        Ok(())
    }

    pub async fn group_record(
        &self,
        group_id: &str,
    ) -> Result<Option<GroupDatabaseRecord>, MemoriaError> {
        let row = sqlx::query(
            "SELECT group_id, db_name, owner_user_id, status \
             FROM mem_groups WHERE group_id = ? LIMIT 1",
        )
        .bind(group_id)
        .fetch_optional(&self.shared_pool)
        .await
        .map_err(db_err)?;

        row.map(|row| {
            Ok(GroupDatabaseRecord {
                group_id: row.try_get("group_id").map_err(db_err)?,
                db_name: row.try_get("db_name").map_err(db_err)?,
                owner_user_id: row.try_get("owner_user_id").map_err(db_err)?,
                status: row.try_get("status").map_err(db_err)?,
            })
        })
        .transpose()
    }

    pub async fn group_db_name(&self, group_id: &str) -> Result<String, MemoriaError> {
        let row =
            sqlx::query("SELECT db_name FROM mem_groups WHERE group_id = ? AND status = 'active'")
                .bind(group_id)
                .fetch_optional(&self.shared_pool)
                .await
                .map_err(db_err)?;

        let row = row.ok_or_else(|| MemoriaError::NotFound(format!("Group '{group_id}'")))?;
        row.try_get("db_name").map_err(db_err)
    }

    pub async fn group_store(&self, group_id: &str) -> Result<Arc<SqlMemoryStore>, MemoriaError> {
        let cache_key = ScopeKey::Group(group_id.to_owned());
        if let Some(cached) = self.user_store_cache.get(&cache_key) {
            return Ok(cached);
        }

        self.cache_operation(
            cache_key.clone(),
            self.init_timeout,
            "group initialization",
            async {
                if let Some(cached) = self.user_store_cache.get(&cache_key) {
                    return Ok(cached);
                }

                let db_name = self.group_db_name(group_id).await?;
                let cache_key_owned = cache_key.clone();
                if self.user_schema_cache.get(&cache_key_owned).is_none() {
                    let _permit = self
                        .user_init_semaphore
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|_| {
                            MemoriaError::Internal("group schema init semaphore closed".into())
                        })?;
                    if self.user_schema_cache.get(&cache_key_owned).is_none() {
                        let db_url = self.user_db_url(&db_name)?;
                        let init_result = match SqlMemoryStore::connect_routed(
                            &db_url,
                            self.embedding_dim,
                            self.instance_id.clone(),
                        )
                        .await
                        {
                            Ok(init_store) => init_store.migrate_user().await,
                            Err(err) => Err(err),
                        };
                        init_result?;
                        self.user_schema_cache.insert(cache_key_owned.clone(), true);
                    }
                }
                let mut store = build_routed_store(
                    self.global_user_pool.clone(),
                    &self.shared_db_url,
                    self.embedding_dim,
                    &self.instance_id,
                    &db_name,
                )?;
                store.set_db_router(Arc::new(self.clone()));
                let store = Arc::new(store);
                self.user_store_cache.insert(cache_key, store.clone());
                Ok(store)
            },
        )
        .await
    }

    pub async fn scope_store(&self, scope_id: &str) -> Result<Arc<SqlMemoryStore>, MemoriaError> {
        if scope_id.starts_with("grp_") {
            self.group_store(scope_id).await
        } else {
            self.user_store(scope_id).await
        }
    }
}

fn user_db_url_from_shared(shared_db_url: &str, db_name: &str) -> Result<String, MemoriaError> {
    replace_db_name(shared_db_url, db_name)
        .ok_or_else(|| MemoriaError::Internal("invalid shared_db_url".into()))
}

fn build_routed_store(
    global_user_pool: MySqlPool,
    shared_db_url: &str,
    embedding_dim: usize,
    instance_id: &str,
    db_name: &str,
) -> Result<SqlMemoryStore, MemoriaError> {
    let db_url = user_db_url_from_shared(shared_db_url, db_name)?;
    let mut store = SqlMemoryStore::new(global_user_pool, embedding_dim, instance_id.to_string());
    store.set_db_name(db_name.to_string());
    store.set_database_url(db_url);
    Ok(store)
}

async fn provision_user_db_with_pool(
    shared_pool: &MySqlPool,
    user_id: &str,
) -> Result<String, MemoriaError> {
    let db_name = DbRouter::user_db_name_for_id(user_id);
    create_database_if_missing(shared_pool, &db_name).await?;
    let now = Utc::now().naive_utc();
    let insert = sqlx::query(
        r#"INSERT INTO mem_user_registry (user_id, db_name, status, created_at, updated_at)
           VALUES (?, ?, 'active', ?, ?)"#,
    )
    .bind(user_id)
    .bind(&db_name)
    .bind(now)
    .bind(now)
    .execute(shared_pool)
    .await;

    match insert {
        Ok(_) => Ok(db_name),
        Err(e) if is_duplicate_key_error(&e) => {
            let existing = sqlx::query("SELECT db_name FROM mem_user_registry WHERE user_id = ?")
                .bind(user_id)
                .fetch_optional(shared_pool)
                .await
                .map_err(db_err)?;

            let Some(existing) = existing else {
                return Err(MemoriaError::Internal(format!(
                    "user db registration collision for {user_id} -> {db_name}"
                )));
            };

            let existing_db_name: String = existing.try_get("db_name").map_err(db_err)?;
            if existing_db_name != db_name {
                return Err(MemoriaError::Internal(format!(
                    "user {user_id} already registered to {existing_db_name}, expected {db_name}"
                )));
            }

            sqlx::query(
                "UPDATE mem_user_registry SET status = 'active', updated_at = ? WHERE user_id = ?",
            )
            .bind(now)
            .bind(user_id)
            .execute(shared_pool)
            .await
            .map_err(db_err)?;

            Ok(existing_db_name)
        }
        Err(e) => Err(db_err(e)),
    }
}

async fn create_database_if_missing(
    shared_pool: &MySqlPool,
    db_name: &str,
) -> Result<(), MemoriaError> {
    sqlx::raw_sql(&format!(
        "CREATE DATABASE IF NOT EXISTS {}",
        quote_ident(db_name)
    ))
    .execute(shared_pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

async fn create_database_if_missing_from_url(database_url: &str) -> Result<(), MemoriaError> {
    let Some((base_url, db_name, _suffix)) = split_database_url(database_url) else {
        return Err(MemoriaError::Internal(
            "database_url missing db name".into(),
        ));
    };
    let base_pool = MySqlPoolOptions::new()
        .max_connections(1)
        .connect(base_url)
        .await
        .map_err(db_err)?;
    create_database_if_missing(&base_pool, db_name).await
}

fn parse_db_name(database_url: &str) -> Option<String> {
    split_database_url(database_url).map(|(_, db_name, _)| db_name.to_string())
}

fn replace_db_name(database_url: &str, db_name: &str) -> Option<String> {
    let (base, _, suffix) = split_database_url(database_url)?;
    Some(format!("{base}/{db_name}{suffix}"))
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

fn quote_ident(name: &str) -> String {
    format!("`{}`", name.replace('`', "``"))
}

fn user_pool_options(database_url: &str) -> Result<MySqlConnectOptions, MemoriaError> {
    // Use the typed setter: the URL parser ignores unknown keys, including the
    // Rust-style `statement_cache_capacity` spelling. Override any URL setting
    // consistently for both the shared user-query pool and schema-init pool.
    Ok(database_url
        .parse::<MySqlConnectOptions>()
        .map_err(db_err)?
        .statement_cache_capacity(0))
}

fn configured_shared_pool_plan() -> SharedPoolPlan {
    let max_pool_size = multi_db_pool_max_size(MultiDbPoolKind::SharedMerged);
    let explicit_override = std::env::var(MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV)
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|max_connections| max_connections.clamp(1, max_pool_size));
    let mut default_components = split_pool_budget(
        explicit_override
            .unwrap_or_else(|| multi_db_pool_default_size(MultiDbPoolKind::SharedMerged)),
        &LEGACY_SHARED_COMPONENT_WEIGHTS,
    );
    let routing_component_override = std::env::var("MEMORIA_SHARED_POOL_MAX_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|raw| raw.clamp(1, max_pool_size));
    let routing_component_max_connections =
        routing_component_override.unwrap_or(default_components.remove(0));
    let shared_main_component_override = std::env::var("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|raw| raw.clamp(1, max_pool_size));
    let shared_main_component_max_connections =
        shared_main_component_override.unwrap_or(default_components.remove(0));
    let git_component_override = std::env::var("MEMORIA_GIT_POOL_MAX_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .map(|raw| raw.clamp(1, max_pool_size));
    let git_component_max_connections =
        git_component_override.unwrap_or(default_components.remove(0));
    let legacy_component_override = routing_component_override.is_some()
        || shared_main_component_override.is_some()
        || git_component_override.is_some();
    let max_connections = explicit_override.unwrap_or_else(|| {
        if legacy_component_override {
            routing_component_max_connections
                .saturating_add(shared_main_component_max_connections)
                .saturating_add(git_component_max_connections)
                .clamp(1, max_pool_size)
        } else {
            multi_db_pool_default_size(MultiDbPoolKind::SharedMerged)
        }
    });
    SharedPoolPlan {
        max_connections,
        routing_component_max_connections,
        shared_main_component_max_connections,
        git_component_max_connections,
        explicit_override: explicit_override.is_some() || legacy_component_override,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        configured_shared_pool_plan, user_pool_options, SharedPoolPlan,
        MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV,
    };
    use crate::pool_config::{
        configured_multi_db_pool_size, multi_db_pool_default_size, MultiDbPoolKind,
    };
    use std::sync::{Mutex, OnceLock};

    #[test]
    fn routed_pool_options_disable_statement_cache_and_preserve_url_settings() {
        use sqlx::ConnectOptions;
        for suffix in [
            "",
            "?statement-cache-capacity=17",
            "?statement_cache_capacity=0",
        ] {
            let url = format!("mysql://localhost/test_schema{suffix}");
            let options = user_pool_options(&url).expect("valid test URL");
            let parsed = options.to_url_lossy();
            assert_eq!(parsed.host_str(), Some("localhost"));
            assert_eq!(parsed.path(), "/test_schema");
            assert_eq!(
                parsed
                    .query_pairs()
                    .find(|(key, _)| key == "statement-cache-capacity")
                    .map(|(_, value)| value.into_owned()),
                Some("0".into())
            );
        }
    }

    #[test]
    fn routed_pool_options_keep_transport_settings() {
        use sqlx::ConnectOptions;
        let options = user_pool_options(
            "mysql://localhost:3307/test_schema?ssl-mode=REQUIRED&charset=latin1&statement-cache-capacity=17"
        ).expect("valid test URL");
        let parsed = options.to_url_lossy();
        assert_eq!(parsed.port(), Some(3307));
        let params: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(params.get("ssl-mode").map(String::as_str), Some("REQUIRED"));
        assert_eq!(params.get("charset").map(String::as_str), Some("latin1"));
        assert_eq!(
            params.get("statement-cache-capacity").map(String::as_str),
            Some("0")
        );
    }

    // Lazy pools keep deadline tests independent of database/network behavior.
    fn cache_test_router(
        init_timeout: std::time::Duration,
        invalidation_timeout: std::time::Duration,
    ) -> super::DbRouter {
        let options = sqlx::mysql::MySqlConnectOptions::new().database("router_deadline_test");
        let pool = sqlx::mysql::MySqlPoolOptions::new().connect_lazy_with(options);
        super::DbRouter {
            shared_pool: pool.clone(),
            shared_pool_max_connections: 1,
            global_user_pool: pool.clone(),
            global_user_pool_max_connections: 1,
            user_init_pool: pool,
            user_init_pool_max_connections: 1,
            shared_db_url: "mysql://localhost/router_deadline_test".into(),
            shared_db_name: "router_deadline_test".into(),
            embedding_dim: 8,
            instance_id: "synthetic-deadline-test".into(),
            user_db_cache: moka::sync::Cache::new(10),
            user_schema_cache: moka::sync::Cache::new(10),
            user_store_cache: moka::sync::Cache::new(10),
            cache_gates: crate::keyed_gates::KeyedGates::default(),
            init_timeout,
            invalidation_timeout,
            user_init_semaphore: std::sync::Arc::new(tokio::sync::Semaphore::new(1)),
        }
    }

    #[tokio::test]
    async fn invalidation_timeout_is_an_error_and_leaves_busy_scope_caches_intact() {
        use super::ScopeKey;
        use std::time::Duration;
        for key in [
            ScopeKey::User("synthetic-user".into()),
            ScopeKey::Group("grp_synthetic".into()),
        ] {
            let router = cache_test_router(Duration::from_secs(30), Duration::from_millis(1));
            let scope_id = match &key {
                ScopeKey::User(id) | ScopeKey::Group(id) => id,
            };
            router.user_schema_cache.insert(key.clone(), true);
            let store =
                std::sync::Arc::new(router.routed_store_for_db_name("synthetic_scope").unwrap());
            router.user_store_cache.insert(key.clone(), store.clone());
            if let ScopeKey::User(id) = &key {
                router
                    .user_db_cache
                    .insert(id.clone(), "synthetic_scope".into());
            }
            let mut busy = Box::pin(router.cache_operation(
                key.clone(),
                router.init_timeout,
                "synthetic initialization",
                std::future::pending::<Result<(), memoria_core::MemoriaError>>(),
            ));
            assert!(futures::poll!(&mut busy).is_pending());
            let error = router.invalidate_scope(scope_id).await.unwrap_err();
            assert!(
                matches!(error, memoria_core::MemoriaError::Database(ref message) if message.contains("cache invalidation timed out"))
            );
            assert!(router.user_schema_cache.get(&key).is_some());
            assert!(std::sync::Arc::ptr_eq(
                &store,
                &router.user_store_cache.get(&key).unwrap()
            ));
            if let ScopeKey::User(id) = &key {
                assert!(router.user_db_cache.get(id).is_some());
            }
            drop(busy);
            router
                .invalidate_scope(scope_id)
                .await
                .expect("retry after cancellation succeeds");
            assert!(router.user_schema_cache.get(&key).is_none());
            assert!(router.user_store_cache.get(&key).is_none());
            if let ScopeKey::User(id) = &key {
                assert!(router.user_db_cache.get(id).is_none());
            }
        }
    }

    #[tokio::test]
    async fn every_cold_cache_entry_point_has_a_gate_wait_deadline() {
        use std::time::Duration;
        let router = cache_test_router(Duration::from_millis(1), Duration::from_secs(1));
        for group_scope in [false, true] {
            let key = if group_scope {
                super::ScopeKey::Group("grp_synthetic".into())
            } else {
                super::ScopeKey::User("synthetic-user".into())
            };
            let mut blocker = Box::pin(router.cache_operation(
                key,
                Duration::from_secs(30),
                "blocker",
                std::future::pending::<Result<(), memoria_core::MemoriaError>>(),
            ));
            assert!(futures::poll!(&mut blocker).is_pending());
            let error = if group_scope {
                router
                    .group_store("grp_synthetic")
                    .await
                    .err()
                    .expect("group must time out")
            } else {
                router
                    .user_store("synthetic-user")
                    .await
                    .err()
                    .expect("user must time out")
            };
            assert!(
                matches!(error, memoria_core::MemoriaError::Database(ref message) if message.contains("initialization timed out"))
            );
            if !group_scope {
                let error = router.user_db_name("synthetic-user").await.unwrap_err();
                assert!(
                    matches!(error, memoria_core::MemoriaError::Database(ref message) if message.contains("database mapping lookup timed out"))
                );
            }
            drop(blocker);
        }
    }

    #[tokio::test]
    async fn initialization_deadline_drops_work_and_unblocks_invalidation() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;
        struct OnDrop<'a>(&'a AtomicBool);
        impl Drop for OnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        let router = cache_test_router(Duration::from_millis(1), Duration::from_secs(1));
        let dropped = AtomicBool::new(false);
        let key = super::ScopeKey::User("synthetic-user".into());
        let mut initializer = Box::pin(router.cache_operation(
            key,
            router.init_timeout,
            "user initialization",
            async {
                let _drop_signal = OnDrop(&dropped);
                std::future::pending::<Result<(), memoria_core::MemoriaError>>().await
            },
        ));
        assert!(futures::poll!(&mut initializer).is_pending());
        let mut invalidation = Box::pin(router.invalidate_user("synthetic-user"));
        assert!(futures::poll!(&mut invalidation).is_pending());
        let error = initializer.await.unwrap_err();
        assert!(
            matches!(error, memoria_core::MemoriaError::Database(ref message) if message.contains("user initialization timed out"))
        );
        assert!(dropped.load(Ordering::Relaxed));
        invalidation
            .await
            .expect("timed-out initializer releases the gate");
        router
            .cache_operation(
                super::ScopeKey::User("synthetic-user".into()),
                Duration::from_secs(1),
                "retry",
                async { Ok(()) },
            )
            .await
            .expect("later retry can enter");
    }

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        struct EnvGuard(Vec<(String, Option<std::ffi::OsString>)>);

        impl Drop for EnvGuard {
            fn drop(&mut self) {
                for (key, old) in &self.0 {
                    match old {
                        Some(value) => unsafe { std::env::set_var(key, value) },
                        None => unsafe { std::env::remove_var(key) },
                    }
                }
            }
        }

        let _restore = EnvGuard(
            vars.iter()
                .map(|(key, value)| {
                    let old = std::env::var_os(key);
                    match value {
                        Some(value) => unsafe { std::env::set_var(key, value) },
                        None => unsafe { std::env::remove_var(key) },
                    }
                    (key.to_string(), old)
                })
                .collect(),
        );

        f();
    }

    #[test]
    fn shared_pool_plan_defaults_merge_legacy_components() {
        with_env(
            &[
                ("MEMORIA_MULTI_DB_POOL_BUDGET", None),
                ("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", None),
                ("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", None),
                ("MEMORIA_GIT_POOL_MAX_CONNECTIONS", None),
                (MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV, None),
            ],
            || {
                assert_eq!(
                    configured_shared_pool_plan(),
                    SharedPoolPlan {
                        max_connections: 102,
                        routing_component_max_connections: 41,
                        shared_main_component_max_connections: 41,
                        git_component_max_connections: 20,
                        explicit_override: false,
                    }
                );
            },
        );
    }

    #[test]
    fn shared_pool_plan_honors_explicit_merged_override() {
        with_env(
            &[
                ("MEMORIA_MULTI_DB_POOL_BUDGET", Some("512")),
                ("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", Some("20")),
                ("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", Some("10")),
                ("MEMORIA_GIT_POOL_MAX_CONNECTIONS", Some("6")),
                (MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV, Some("40")),
            ],
            || {
                assert_eq!(
                    configured_shared_pool_plan(),
                    SharedPoolPlan {
                        max_connections: 40,
                        routing_component_max_connections: 20,
                        shared_main_component_max_connections: 10,
                        git_component_max_connections: 6,
                        explicit_override: true,
                    }
                );
            },
        );
    }

    #[test]
    fn shared_pool_plan_clamps_explicit_merged_override() {
        with_env(
            &[
                ("MEMORIA_MULTI_DB_POOL_BUDGET", Some("512")),
                ("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", Some("20")),
                ("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", Some("10")),
                ("MEMORIA_GIT_POOL_MAX_CONNECTIONS", Some("6")),
                (MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV, Some("0")),
            ],
            || {
                assert_eq!(
                    configured_shared_pool_plan(),
                    SharedPoolPlan {
                        max_connections: 1,
                        routing_component_max_connections: 20,
                        shared_main_component_max_connections: 10,
                        git_component_max_connections: 6,
                        explicit_override: true,
                    }
                );
            },
        );
    }

    #[test]
    fn shared_pool_plan_ignores_blank_legacy_component_envs() {
        with_env(
            &[
                ("MEMORIA_MULTI_DB_POOL_BUDGET", Some("512")),
                ("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", Some("")),
                ("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", Some("")),
                ("MEMORIA_GIT_POOL_MAX_CONNECTIONS", Some("")),
                (MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV, None),
            ],
            || {
                assert_eq!(
                    configured_shared_pool_plan(),
                    SharedPoolPlan {
                        max_connections: 102,
                        routing_component_max_connections: 41,
                        shared_main_component_max_connections: 41,
                        git_component_max_connections: 20,
                        explicit_override: false,
                    }
                );
            },
        );
    }

    #[test]
    fn shared_pool_plan_splits_explicit_merged_override_without_legacy_components() {
        with_env(
            &[
                ("MEMORIA_MULTI_DB_POOL_BUDGET", Some("512")),
                ("MEMORIA_SHARED_POOL_MAX_CONNECTIONS", None),
                ("MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS", None),
                ("MEMORIA_GIT_POOL_MAX_CONNECTIONS", None),
                (MERGED_SHARED_POOL_MAX_CONNECTIONS_ENV, Some("3")),
            ],
            || {
                assert_eq!(
                    configured_shared_pool_plan(),
                    SharedPoolPlan {
                        max_connections: 3,
                        routing_component_max_connections: 1,
                        shared_main_component_max_connections: 1,
                        git_component_max_connections: 1,
                        explicit_override: true,
                    }
                );
            },
        );
    }

    #[test]
    fn user_pool_defaults_match_budget_split() {
        with_env(
            &[
                ("MEMORIA_MULTI_DB_POOL_BUDGET", None),
                ("MEMORIA_GLOBAL_USER_POOL_MAX", None),
                ("MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS", None),
            ],
            || {
                assert_eq!(
                    configured_multi_db_pool_size(
                        "MEMORIA_GLOBAL_USER_POOL_MAX",
                        MultiDbPoolKind::GlobalUser,
                    ),
                    multi_db_pool_default_size(MultiDbPoolKind::GlobalUser)
                );
                assert_eq!(
                    configured_multi_db_pool_size(
                        "MEMORIA_USER_SCHEMA_INIT_POOL_MAX_CONNECTIONS",
                        MultiDbPoolKind::UserInit,
                    ),
                    multi_db_pool_default_size(MultiDbPoolKind::UserInit)
                );
            },
        );
    }

    #[test]
    fn user_schema_init_concurrency_defaults_to_six() {
        with_env(
            &[("MEMORIA_USER_SCHEMA_INIT_MAX_CONCURRENCY", None)],
            || {
                let user_init_max: usize =
                    std::env::var("MEMORIA_USER_SCHEMA_INIT_MAX_CONCURRENCY")
                        .ok()
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(6)
                        .clamp(1, 64);
                assert_eq!(user_init_max, 6);
            },
        );
    }
}
