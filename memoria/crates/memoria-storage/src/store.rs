use crate::router::DbRouter;
use async_trait::async_trait;
use chrono::{NaiveDateTime, Utc};
use memoria_core::{
    interfaces::MemoryStore, nullable_str, nullable_str_from_row, MemoriaError, Memory, MemoryType,
    TrustTier,
};
use sqlx::{mysql::MySqlPool, MySql, QueryBuilder, Row};
use std::borrow::Cow;
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

pub const EXTRA_METADATA_FILTER_MAX_FIELDS: usize = 16;
pub const EXTRA_METADATA_FILTER_MAX_KEY_BYTES: usize = 64;
pub const EXTRA_METADATA_FILTER_MAX_VALUE_BYTES: usize = 1024;
pub const FULLTEXT_SEARCH_DEFAULT_LIMIT: i64 = 20;
pub const FULLTEXT_SEARCH_MAX_LIMIT: i64 = 100;
pub const FULLTEXT_QUERY_MAX_BYTES: usize = 4096;

const FULLTEXT_MEMORY_COLUMNS: &str =
    "memory_id, user_id, author_id, subject_id, memory_type, content, \
    embedding AS emb_str, session_id, CAST(source_event_ids AS CHAR) AS src_ids, \
    CAST(extra_metadata AS CHAR) AS extra_meta, is_active, superseded_by, trust_tier, \
    initial_confidence, observed_at, created_at, updated_at";

/// Validate the public structured-query metadata contract at the storage boundary.
/// Keys become JSON paths, so the first character must be an ASCII letter or
/// underscore and remaining characters are limited to ASCII alphanumerics/underscore.
pub fn validate_extra_metadata_filter(
    filter: &std::collections::HashMap<String, serde_json::Value>,
) -> Result<(), MemoriaError> {
    if filter.len() > EXTRA_METADATA_FILTER_MAX_FIELDS {
        return Err(MemoriaError::Validation(format!(
            "extra_metadata_filter must not contain more than {EXTRA_METADATA_FILTER_MAX_FIELDS} fields"
        )));
    }
    for (key, value) in filter {
        let mut chars = key.chars();
        let valid_first = chars
            .next()
            .is_some_and(|ch| ch.is_ascii_alphabetic() || ch == '_');
        let valid_rest = chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_');
        if key.len() > EXTRA_METADATA_FILTER_MAX_KEY_BYTES || !valid_first || !valid_rest {
            return Err(MemoriaError::Validation(format!(
                "extra_metadata_filter key '{key}' must start with an ASCII letter or underscore, contain only ASCII letters, digits, or underscore, and be at most {EXTRA_METADATA_FILTER_MAX_KEY_BYTES} bytes"
            )));
        }
        if value.is_null() || value.is_array() || value.is_object() {
            return Err(MemoriaError::Validation(format!(
                "extra_metadata_filter value for '{key}' must be a string, number, or boolean"
            )));
        }
        if serde_json::to_string(value)?.len() > EXTRA_METADATA_FILTER_MAX_VALUE_BYTES {
            return Err(MemoriaError::Validation(format!(
                "extra_metadata_filter value for '{key}' must not exceed {EXTRA_METADATA_FILTER_MAX_VALUE_BYTES} bytes"
            )));
        }
    }
    Ok(())
}

tokio::task_local! {
    /// Real user ID for per-user state (active branch).
    /// In group mode the "user_id" flowing through the service layer is the
    /// group's scope_id (e.g. "grp_xxx").  The task-local carries the **real**
    /// human user so that `active_branch_name` / `set_active_branch` key on
    /// the individual, not the group.  In non-group mode this is never set and
    /// the functions fall back to the passed `user_id`.
    pub static ACTOR_USER_ID: String;
}

pub(crate) fn db_err(e: sqlx::Error) -> MemoriaError {
    if let Some(kind) = detect_connection_anomaly(&e) {
        record_connection_anomaly(kind);
        tracing::error!(
            error_kind = kind.as_str(),
            error = %e,
            "database connection anomaly"
        );
    }
    MemoriaError::Database(e.to_string())
}

/// MatrixOne currently reports an empty tokenized full-text pattern as generic
/// internal error 20101. The code is shared by many internal errors, so retain
/// the specific message check in one place until MatrixOne exposes a dedicated
/// error code. See MatrixOne `pkg/fulltext/fulltext.go`.
fn is_empty_fulltext_pattern_error(error: &sqlx::Error) -> bool {
    use sqlx::mysql::MySqlDatabaseError;

    error
        .as_database_error()
        .and_then(|database_error| {
            database_error
                .as_error()
                .downcast_ref::<MySqlDatabaseError>()
        })
        .is_some_and(|mysql_error| {
            mysql_error.number() == 20101
                && mysql_error.message().contains("empty pattern")
        })
}

pub(crate) fn fulltext_rows_or_empty(
    result: Result<Vec<sqlx::mysql::MySqlRow>, sqlx::Error>,
) -> Result<Vec<sqlx::mysql::MySqlRow>, MemoriaError> {
    match result {
        Ok(rows) => Ok(rows),
        Err(error) if is_empty_fulltext_pattern_error(&error) => Ok(Vec::new()),
        Err(error) => Err(db_err(error)),
    }
}

fn fulltext_score(row: &sqlx::mysql::MySqlRow) -> Option<f64> {
    row.try_get::<f64, _>("ft_score")
        .or_else(|_| row.try_get::<f32, _>("ft_score").map(f64::from))
        .ok()
}

fn apply_fulltext_score(row: &sqlx::mysql::MySqlRow, memory: &mut Memory) {
    memory.retrieval_score = fulltext_score(row);
}

fn merge_hybrid_results(
    vec_results: Vec<Memory>,
    ft_results: Vec<Memory>,
) -> (Vec<Memory>, std::collections::HashMap<String, f64>) {
    let ft_map = ft_results
        .iter()
        .filter_map(|m| m.retrieval_score.map(|s| (m.memory_id.clone(), s)))
        .collect();

    let mut seen: std::collections::HashSet<String> =
        vec_results.iter().map(|m| m.memory_id.clone()).collect();
    let mut candidates = vec_results;
    for mut memory in ft_results {
        if seen.insert(memory.memory_id.clone()) {
            // Fulltext scores live in ft_map; retrieval_score on candidates is the vector input.
            memory.retrieval_score = None;
            candidates.push(memory);
        }
    }

    (candidates, ft_map)
}

/// Returns true when a failed ALTER TABLE ADD COLUMN was rejected because
/// the column already exists (MySQL/MatrixOne error 1060).
/// This is the expected outcome when the column was created by CREATE TABLE
/// before information_schema reflects it, so it should be treated as a no-op.
fn is_duplicate_column(e: &sqlx::Error) -> bool {
    use sqlx::mysql::MySqlDatabaseError;
    e.as_database_error()
        .and_then(|de| de.as_error().downcast_ref::<MySqlDatabaseError>())
        .map(|me| me.number() == 1060)
        .unwrap_or(false)
}

/// Returns true for MySQL/MatrixOne error 1061 "duplicate key name" — the index
/// already exists. Treat as non-fatal idempotency, log at DEBUG.
fn is_duplicate_index(e: &sqlx::Error) -> bool {
    use sqlx::mysql::MySqlDatabaseError;
    e.as_database_error()
        .and_then(|de| de.as_error().downcast_ref::<MySqlDatabaseError>())
        .map(|me| me.number() == 1061)
        .unwrap_or(false)
}

/// Returns true when MatrixOne returns error 1146 "no such table" for an internal
/// secondary-index metadata table (e.g. `schema.__mo_index_secondary_<uuid>`).
///
/// This happens when multiple concurrent requests all try to `ALTER TABLE ADD COLUMN`
/// on the same table at the same time. MatrixOne serialises DDL internally and the
/// losing transactions observe a stale view of the internal index-metadata table
/// (which was renamed / recreated by the winner). The net result is identical to
/// "duplicate column" — the column is already present — so callers should treat
/// this as non-fatal and log at WARN, not ERROR.
fn is_mo_concurrent_ddl_race(e: &sqlx::Error) -> bool {
    use sqlx::mysql::MySqlDatabaseError;
    let is_1146 = e
        .as_database_error()
        .and_then(|de| de.as_error().downcast_ref::<MySqlDatabaseError>())
        .map(|me| me.number() == 1146)
        .unwrap_or(false);
    is_1146 && e.to_string().contains("__mo_index_secondary_")
}

/// Returns true for MatrixOne's transient "txn need retry in rc mode, def changed"
/// error (code 20631). This can occur when a DDL statement (e.g. ALTER TABLE on a
/// branch table) races with a concurrent `data branch merge` that also modifies the
/// same table definition.
fn is_mo_retry_error(e: &sqlx::Error) -> bool {
    let msg = e.to_string();
    msg.contains("20631") || msg.contains("txn need retry")
}

/// Execute a raw DDL string, retrying up to 3 times on MatrixOne error 20631
/// ("txn need retry in rc mode, def changed") with exponential backoff.
/// Used for ALTER TABLE statements that may race with concurrent DDL in migrations.
async fn exec_ddl_with_retry(pool: &sqlx::mysql::MySqlPool, sql: &str) -> Result<(), sqlx::Error> {
    const MAX_ATTEMPTS: u32 = 3;
    let mut attempt = 0u32;
    loop {
        match sqlx::query(sql).execute(pool).await {
            Ok(_) => return Ok(()),
            Err(e) if attempt < MAX_ATTEMPTS - 1 && is_mo_retry_error(&e) => {
                attempt += 1;
                let backoff_ms = 50u64 * (1 << attempt); // 100 ms, 200 ms
                tracing::warn!(
                    attempt,
                    backoff_ms,
                    error = %e,
                    sql = sql,
                    "exec_ddl_with_retry: MatrixOne 20631 — retrying migration DDL",
                );
                tokio::time::sleep(std::time::Duration::from_millis(backoff_ms)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

fn normalized_count_sql(sql: &str) -> String {
    sql.trim()
        .trim_end_matches(';')
        .trim()
        .strip_suffix("> 0")
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| sql.to_string())
}

#[allow(dead_code)]
async fn query_has_rows(pool: &MySqlPool, sql: &str) -> bool {
    // Strip "> 0" suffix if present — MatrixOne returns bool for "COUNT(*) > 0"
    // which sqlx cannot decode as i64. Use plain COUNT(*) and compare in Rust.
    let sql = normalized_count_sql(sql);
    sqlx::query_scalar::<_, i64>(&sql)
        .fetch_one(pool)
        .await
        .unwrap_or(0)
        > 0
}

async fn info_schema_column_exists(
    pool: &MySqlPool,
    schema_name: &str,
    table_name: &str,
    column_name: &str,
) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.columns \
         WHERE table_schema = ? AND table_name = ? AND column_name = ?",
    )
    .bind(schema_name)
    .bind(table_name)
    .bind(column_name)
    .fetch_one(pool)
    .await
    .unwrap_or(0)
        > 0
}

async fn info_schema_index_exists(
    pool: &MySqlPool,
    schema_name: &str,
    table_name: &str,
    index_name: &str,
) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.statistics \
         WHERE table_schema = ? AND table_name = ? AND index_name = ?",
    )
    .bind(schema_name)
    .bind(table_name)
    .bind(index_name)
    .fetch_one(pool)
    .await
    .unwrap_or(0)
        > 0
}

async fn info_schema_index_column_exists(
    pool: &MySqlPool,
    schema_name: &str,
    table_name: &str,
    index_name: &str,
    column_name: &str,
) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.statistics \
         WHERE table_schema = ? AND table_name = ? AND index_name = ? \
         AND column_name = ?",
    )
    .bind(schema_name)
    .bind(table_name)
    .bind(index_name)
    .bind(column_name)
    .fetch_one(pool)
    .await
    .unwrap_or(0)
        > 0
}

async fn is_fresh_database(pool: &MySqlPool, schema_name: &str) -> Result<bool, MemoriaError> {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema = ?",
    )
    .bind(schema_name)
    .fetch_one(pool)
    .await
    .map(|count| count == 0)
    .map_err(db_err)
}

async fn info_schema_table_exists(pool: &MySqlPool, schema_name: &str, table_name: &str) -> bool {
    sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(*) FROM information_schema.tables \
         WHERE table_schema = ? AND table_name = ?",
    )
    .bind(schema_name)
    .bind(table_name)
    .fetch_one(pool)
    .await
    .unwrap_or(0)
        > 0
}

/// Bump whenever user-DB schema expectations change, including tables created
/// by `bootstrap_user_schema()`, graph schema bootstrapping, or any compat
/// migration handled by `apply_user_compat_migrations()`.
pub const CURRENT_USER_SCHEMA_VERSION: i64 = 2;
const USER_SCHEMA_META_KEY: &str = "user_schema";

async fn ensure_user_schema_meta_table(pool: &MySqlPool, table: &str) -> Result<(), MemoriaError> {
    sqlx::query(&format!(
        r#"CREATE TABLE IF NOT EXISTS {table} (
            schema_key     VARCHAR(64) PRIMARY KEY,
            schema_version BIGINT      NOT NULL,
            updated_at     DATETIME(6) NOT NULL
        )"#
    ))
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

async fn load_user_schema_version(
    pool: &MySqlPool,
    table: &str,
) -> Result<Option<i64>, MemoriaError> {
    let row = sqlx::query(&format!(
        "SELECT schema_version FROM {table} WHERE schema_key = ? LIMIT 1"
    ))
    .bind(USER_SCHEMA_META_KEY)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;

    row.map(|r| r.try_get::<i64, _>("schema_version").map_err(db_err))
        .transpose()
}

async fn store_user_schema_version(
    pool: &MySqlPool,
    table: &str,
    version: i64,
) -> Result<(), MemoriaError> {
    let now = Utc::now().naive_utc();
    sqlx::query(&format!(
        r#"INSERT INTO {table} (schema_key, schema_version, updated_at)
           VALUES (?, ?, ?)
           ON DUPLICATE KEY UPDATE
               schema_version = VALUES(schema_version),
               updated_at = VALUES(updated_at)"#
    ))
    .bind(USER_SCHEMA_META_KEY)
    .bind(version)
    .bind(now)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(())
}

const POOL_MONITOR_INTERVAL_SECS: u64 = 30;
const POOL_MONITOR_REPEAT_AFTER_TICKS: u32 = 10;
const POOL_SATURATED_LOG_DELAY_SECS: u64 = 300;
const POOL_ANOMALY_RECENT_WINDOW_SECS: u64 = 600;
const MAX_IDENTIFIER_LEN: usize = 64;
const SAFETY_SNAPSHOT_SCOPE_MAX_LEN: usize = 21;
const SAFETY_SNAPSHOT_OPERATION_MAX_LEN: usize = 20;
const SAFETY_SNAPSHOT_UUID_LEN: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectionAnomalyKind {
    None = 0,
    PoolTimedOut = 1,
    PoolClosed = 2,
    Io = 3,
    Tls = 4,
    Protocol = 5,
    TooManyConnections = 6,
}

impl ConnectionAnomalyKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PoolTimedOut => "pool_timed_out",
            Self::PoolClosed => "pool_closed",
            Self::Io => "io",
            Self::Tls => "tls",
            Self::Protocol => "protocol",
            Self::TooManyConnections => "too_many_connections",
        }
    }
}

impl From<u8> for ConnectionAnomalyKind {
    fn from(value: u8) -> Self {
        match value {
            1 => Self::PoolTimedOut,
            2 => Self::PoolClosed,
            3 => Self::Io,
            4 => Self::Tls,
            5 => Self::Protocol,
            6 => Self::TooManyConnections,
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct ConnectionAnomalySnapshot {
    kind: ConnectionAnomalyKind,
    age_secs: Option<u64>,
    total: u64,
    pool_timeouts_total: u64,
}

static LAST_CONNECTION_ANOMALY_AT_SECS: AtomicU64 = AtomicU64::new(0);
static LAST_CONNECTION_ANOMALY_KIND: AtomicU8 = AtomicU8::new(ConnectionAnomalyKind::None as u8);
static CONNECTION_ANOMALIES_TOTAL: AtomicU64 = AtomicU64::new(0);
static POOL_TIMEOUTS_TOTAL: AtomicU64 = AtomicU64::new(0);

fn unix_now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn detect_connection_anomaly(e: &sqlx::Error) -> Option<ConnectionAnomalyKind> {
    match e {
        sqlx::Error::PoolTimedOut => Some(ConnectionAnomalyKind::PoolTimedOut),
        sqlx::Error::PoolClosed => Some(ConnectionAnomalyKind::PoolClosed),
        sqlx::Error::Io(_) => Some(ConnectionAnomalyKind::Io),
        sqlx::Error::Tls(_) => Some(ConnectionAnomalyKind::Tls),
        sqlx::Error::Protocol(_) => Some(ConnectionAnomalyKind::Protocol),
        _ => e
            .as_database_error()
            .and_then(|de| {
                de.as_error()
                    .downcast_ref::<sqlx::mysql::MySqlDatabaseError>()
            })
            .and_then(|me| {
                if me.number() == 1040 {
                    Some(ConnectionAnomalyKind::TooManyConnections)
                } else {
                    None
                }
            }),
    }
}

fn record_connection_anomaly(kind: ConnectionAnomalyKind) {
    LAST_CONNECTION_ANOMALY_AT_SECS.store(unix_now_secs(), Ordering::Relaxed);
    LAST_CONNECTION_ANOMALY_KIND.store(kind as u8, Ordering::Relaxed);
    CONNECTION_ANOMALIES_TOTAL.fetch_add(1, Ordering::Relaxed);
    if matches!(kind, ConnectionAnomalyKind::PoolTimedOut) {
        POOL_TIMEOUTS_TOTAL.fetch_add(1, Ordering::Relaxed);
    }
}

fn connection_anomaly_snapshot() -> ConnectionAnomalySnapshot {
    let last_at = LAST_CONNECTION_ANOMALY_AT_SECS.load(Ordering::Relaxed);
    let age_secs = if last_at == 0 {
        None
    } else {
        Some(unix_now_secs().saturating_sub(last_at))
    };
    ConnectionAnomalySnapshot {
        kind: ConnectionAnomalyKind::from(LAST_CONNECTION_ANOMALY_KIND.load(Ordering::Relaxed)),
        age_secs,
        total: CONNECTION_ANOMALIES_TOTAL.load(Ordering::Relaxed),
        pool_timeouts_total: POOL_TIMEOUTS_TOTAL.load(Ordering::Relaxed),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolHealthLevel {
    Healthy,
    HighUtilization,
    Saturated,
    Empty,
}

impl PoolHealthLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Healthy => "healthy",
            Self::HighUtilization => "high_utilization",
            Self::Saturated => "saturated",
            Self::Empty => "empty",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PoolHealthSnapshot {
    pub configured_max_connections: Option<u32>,
    pub size: u32,
    pub active: u32,
    pub idle: u32,
    pub level: PoolHealthLevel,
    pub since: std::time::Instant,
    pub consecutive_observations: u32,
    pub last_connection_anomaly_kind: &'static str,
    pub last_connection_anomaly_age_secs: Option<u64>,
    pub connection_anomalies_total: u64,
    pub pool_timeouts_total: u64,
    pub saturated_warning_emitted: bool,
}

impl PoolHealthSnapshot {
    pub fn new(configured_max_connections: Option<u32>) -> Self {
        Self {
            configured_max_connections,
            size: 0,
            active: 0,
            idle: 0,
            level: PoolHealthLevel::Healthy,
            since: std::time::Instant::now(),
            consecutive_observations: 0,
            last_connection_anomaly_kind: ConnectionAnomalyKind::None.as_str(),
            last_connection_anomaly_age_secs: None,
            connection_anomalies_total: 0,
            pool_timeouts_total: 0,
            saturated_warning_emitted: false,
        }
    }
}

fn classify_pool_health(
    size: u32,
    idle: u32,
    configured_max_connections: Option<u32>,
) -> PoolHealthLevel {
    if size == 0 {
        PoolHealthLevel::Empty
    } else if idle == 0
        && configured_max_connections
            .map(|max| max > 0 && size >= max)
            .unwrap_or(false)
    {
        PoolHealthLevel::Saturated
    } else if idle == 0 || idle < size / 10 + 1 {
        PoolHealthLevel::HighUtilization
    } else {
        PoolHealthLevel::Healthy
    }
}

fn should_repeat_pool_log(consecutive_observations: u32) -> bool {
    consecutive_observations.checked_rem(POOL_MONITOR_REPEAT_AFTER_TICKS) == Some(0)
}

fn should_emit_saturated_warning(state_duration_secs: u64, warning_emitted: bool) -> bool {
    state_duration_secs >= POOL_SATURATED_LOG_DELAY_SECS && !warning_emitted
}

/// Spawn a background task that periodically logs pool utilization.
/// Warns when idle connections drop below 10% of pool size.
/// Stops automatically when the pool is closed.
pub fn spawn_pool_monitor(
    pool: MySqlPool,
    configured_max_connections: Option<u32>,
    health: Arc<std::sync::Mutex<PoolHealthSnapshot>>,
    pool_name: &'static str,
) {
    ::tokio::spawn(async move {
        let mut interval =
            ::tokio::time::interval(std::time::Duration::from_secs(POOL_MONITOR_INTERVAL_SECS));
        interval.tick().await; // skip immediate
        loop {
            interval.tick().await;
            if pool.is_closed() {
                tracing::debug!("pool monitor stopping — pool closed");
                break;
            }
            let size = pool.size();
            let idle = pool.num_idle();
            let active = size.saturating_sub(idle as u32);
            let level = classify_pool_health(size, idle as u32, configured_max_connections);
            let anomaly = connection_anomaly_snapshot();

            let mut guard = health.lock().unwrap();
            let previous_level = guard.level;
            let previous_since = guard.since;
            let previous_saturated_warning_emitted = guard.saturated_warning_emitted;

            if previous_level == level {
                guard.consecutive_observations = guard.consecutive_observations.saturating_add(1);
            } else {
                guard.level = level;
                guard.since = std::time::Instant::now();
                guard.consecutive_observations = 1;
                guard.saturated_warning_emitted = false;
            }

            guard.configured_max_connections = configured_max_connections;
            guard.size = size;
            guard.active = active;
            guard.idle = idle as u32;
            guard.last_connection_anomaly_kind = anomaly.kind.as_str();
            guard.last_connection_anomaly_age_secs = anomaly.age_secs;
            guard.connection_anomalies_total = anomaly.total;
            guard.pool_timeouts_total = anomaly.pool_timeouts_total;

            let recent_anomaly = anomaly
                .age_secs
                .map(|age| age <= POOL_ANOMALY_RECENT_WINDOW_SECS)
                .unwrap_or(false);
            let state_duration_secs = guard.since.elapsed().as_secs();

            let should_log = match level {
                PoolHealthLevel::Healthy => {
                    previous_level != PoolHealthLevel::Healthy
                        && (previous_level != PoolHealthLevel::Saturated
                            || previous_saturated_warning_emitted)
                }
                PoolHealthLevel::Empty => {
                    if recent_anomaly {
                        previous_level != level
                            || guard.consecutive_observations == 1
                            || should_repeat_pool_log(guard.consecutive_observations)
                    } else {
                        previous_level != level
                    }
                }
                PoolHealthLevel::Saturated => should_emit_saturated_warning(
                    state_duration_secs,
                    guard.saturated_warning_emitted,
                ),
                PoolHealthLevel::HighUtilization => {
                    previous_level != level
                        || guard.consecutive_observations == 1
                        || should_repeat_pool_log(guard.consecutive_observations)
                }
            };

            if !should_log {
                continue;
            }

            let unhealthy_for_secs = previous_since.elapsed().as_secs();
            match level {
                PoolHealthLevel::Healthy => {
                    tracing::info!(
                        pool_name,
                        previous_state = previous_level.as_str(),
                        previous_state_duration_secs = unhealthy_for_secs,
                        pool_size = size,
                        pool_active = active,
                        pool_idle = idle,
                        configured_max_connections,
                        "connection pool recovered"
                    );
                }
                PoolHealthLevel::Empty => {
                    if recent_anomaly {
                        tracing::warn!(
                            pool_name,
                            pool_size = size,
                            pool_active = active,
                            pool_idle = idle,
                            configured_max_connections,
                            state = level.as_str(),
                            state_duration_secs,
                            consecutive_observations = guard.consecutive_observations,
                            last_connection_anomaly_kind = anomaly.kind.as_str(),
                            last_connection_anomaly_age_secs = anomaly.age_secs.unwrap_or_default(),
                            connection_anomalies_total = anomaly.total,
                            pool_timeouts_total = anomaly.pool_timeouts_total,
                            "connection pool has no established connections after recent connectivity failures; existing connections may have been dropped or new ones cannot be established"
                        );
                    } else {
                        tracing::info!(
                            pool_name,
                            pool_size = size,
                            pool_active = active,
                            pool_idle = idle,
                            configured_max_connections,
                            state = level.as_str(),
                            state_duration_secs,
                            consecutive_observations = guard.consecutive_observations,
                            "connection pool currently has no established connections; this is expected when idle_timeout has drained the pool and no requests are using it"
                        );
                    }
                }
                PoolHealthLevel::Saturated => {
                    tracing::warn!(
                        pool_name,
                        pool_size = size,
                        pool_active = active,
                        pool_idle = idle,
                        configured_max_connections,
                        state = level.as_str(),
                        state_duration_secs,
                        consecutive_observations = guard.consecutive_observations,
                        "connection pool saturated — pool is at configured max and all established connections are busy"
                    );
                    guard.saturated_warning_emitted = true;
                }
                PoolHealthLevel::HighUtilization => {
                    if idle == 0 {
                        tracing::warn!(
                            pool_name,
                            pool_size = size,
                            pool_active = active,
                            pool_idle = idle,
                            configured_max_connections,
                            state = level.as_str(),
                            state_duration_secs,
                            consecutive_observations = guard.consecutive_observations,
                            "connection pool has no idle connections; pool can still expand"
                        );
                    } else {
                        tracing::warn!(
                            pool_name,
                            pool_size = size,
                            pool_active = active,
                            pool_idle = idle,
                            configured_max_connections,
                            state = level.as_str(),
                            state_duration_secs,
                            consecutive_observations = guard.consecutive_observations,
                            "connection pool high utilization"
                        );
                    }
                }
            }
        }
    });
}

/// Owned edit-log entry for async batched writes.
#[derive(Clone)]
pub struct OwnedEditLogEntry {
    pub edit_id: String,
    pub user_id: String,
    pub operation: String,
    pub memory_id: Option<String>,
    pub payload: Option<String>,
    pub reason: String,
    pub snapshot_before: Option<String>,
}

/// Generate a UUID v7 (time-ordered) as a simple hex string.
fn uuid7_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()
}

// Workaround: MO#24001 — nullable_str / nullable_str_from_row imported from memoria_core.

/// Sanitize a string for safe interpolation inside a SQL single-quoted literal.
/// Escapes `'`, `\`, and strips NUL bytes.
#[allow(dead_code)]
fn sanitize_sql_literal(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\0')
        .fold(String::with_capacity(s.len()), |mut out, c| {
            match c {
                '\'' => out.push_str("''"),
                '\\' => out.push_str("\\\\"),
                _ => out.push(c),
            }
            out
        })
}

/// Sanitize a string for use inside MATCH ... AGAINST('...' IN BOOLEAN MODE).
/// Strips boolean-mode operators and SQL-injection characters.
fn sanitize_fulltext_query(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\0')
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        // MatrixOne rejects the entire OR query if even one term tokenizes
        // to nothing (for example a code diagnostic's "________" underline).
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Keep generated full-text SQL bounded without discarding the tail of a
/// long context query. Duplicate terms need only be searched once.
fn fulltext_query_batches(query: &str) -> Vec<String> {
    let safe = sanitize_fulltext_query(query).to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let words: Vec<_> = safe
        .split_whitespace()
        .filter(|word| seen.insert(*word))
        .collect();
    words.chunks(64).map(|batch| batch.join(" ")).collect()
}

/// Validate the public full-text query contract. Stopword-only queries remain
/// valid and may return no rows after MatrixOne tokenization.
pub fn validate_fulltext_query(query: &str) -> Result<(), MemoriaError> {
    if query.len() > FULLTEXT_QUERY_MAX_BYTES {
        return Err(MemoriaError::Validation(format!(
            "fulltext query must not exceed {FULLTEXT_QUERY_MAX_BYTES} bytes"
        )));
    }
    if sanitize_fulltext_query(query).is_empty() {
        return Err(MemoriaError::Validation(
            "fulltext query must contain at least one Unicode letter, number, or underscore"
                .to_string(),
        ));
    }
    Ok(())
}

/// Sanitize a string for use in a LIKE pattern (escapes `%`).
fn sanitize_like_pattern(s: &str) -> String {
    s.chars()
        .filter(|c| *c != '\0')
        .map(|c| match c {
            '\'' | '\\' => ' ',
            '%' => ' ',
            _ => c,
        })
        .collect()
}

fn vec_to_mo(v: &[f32]) -> String {
    format!(
        "[{}]",
        v.iter()
            .map(|f| f.to_string())
            .collect::<Vec<_>>()
            .join(",")
    )
}

/// Build an SQL `AND memory_type IN ('a', 'b')` clause with inlined literals.
/// Used for search paths that cannot use bind parameters (MatrixOne bug workaround).
/// Values are enum variants serialised via `Display` — no user-supplied input.
fn build_memory_types_in_clause_inline(memory_types: Option<&[MemoryType]>) -> String {
    match memory_types {
        Some(types) if !types.is_empty() => {
            let vals = types
                .iter()
                .map(|t| format!("'{}'", sanitize_sql_literal(&t.to_string())))
                .collect::<Vec<_>>()
                .join(", ");
            format!(" AND memory_type IN ({vals})")
        }
        _ => String::new(),
    }
}

fn mo_to_vec(s: &str) -> Result<Vec<f32>, MemoriaError> {
    let inner = s.trim_matches(|c| c == '[' || c == ']');
    if inner.is_empty() {
        return Ok(vec![]);
    }
    inner
        .split(',')
        .map(|x| {
            x.trim()
                .parse::<f32>()
                .map_err(|e| MemoriaError::Internal(format!("vec parse: {e}")))
        })
        .collect()
}

#[derive(Clone)]
pub struct SqlMemoryStore {
    pool: MySqlPool,
    branch_alter_capability: Arc<crate::branch_capability::BranchAlterCapability>,
    /// Successful optional log-schema checks are shared by clones of this store.
    call_log_tool_outcomes_ready: Arc<AtomicBool>,
    embedding_dim: usize,
    instance_id: String,
    database_url: Option<String>,
    configured_max_connections: Option<u32>,
    pool_health: Arc<std::sync::Mutex<PoolHealthSnapshot>>,
    /// Cache: user_id → active table name (TTL 5s, invalidated on branch switch)
    active_table_cache: moka::sync::Cache<String, String>,
    /// Cache: (user_id, operation) → last_run Instant (avoids DB query for cooldown checks)
    cooldown_cache: moka::sync::Cache<String, std::time::Instant>,
    /// Cache: user_id → graph node count (TTL 2 min, shared across GraphStore instances)
    node_count_cache: moka::sync::Cache<String, i64>,
    /// Optional: route log_edit through async buffer instead of direct INSERT.
    /// Shared across main store and background pool clones so a single clear drains all.
    edit_log_tx: Arc<std::sync::RwLock<Option<tokio::sync::mpsc::Sender<OwnedEditLogEntry>>>>,
    db_router: Option<Arc<DbRouter>>,
    /// When Some, `t()` qualifies table names with this database prefix.
    /// Enables a single global pool to serve queries for different per-user databases.
    db_name: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotRegistration {
    pub name: String,
    pub snapshot_name: String,
    /// Extensible per-snapshot metadata. Keep derived fields here so future
    /// snapshot attributes do not require another per-user schema migration.
    pub extra: Option<serde_json::Value>,
    pub created_at: chrono::NaiveDateTime,
}

pub fn snapshot_extra_with_memory_count(memory_count: i64) -> serde_json::Value {
    serde_json::json!({ "memory_count": memory_count })
}

pub fn snapshot_extra_memory_count(extra: Option<&serde_json::Value>) -> Option<i64> {
    extra?.as_object()?.get("memory_count")?.as_i64()
}

fn snapshot_extra_from_row(
    row: &sqlx::mysql::MySqlRow,
) -> Result<Option<serde_json::Value>, MemoriaError> {
    row.try_get("extra").map_err(db_err)
}

/// Aggregated feedback statistics for a user.
#[derive(Debug, Clone, serde::Serialize)]
pub struct FeedbackStats {
    pub total: i64,
    pub useful: i64,
    pub irrelevant: i64,
    pub outdated: i64,
    pub wrong: i64,
}

/// Feedback breakdown by trust tier.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TierFeedback {
    pub tier: String,
    pub signal: String,
    pub count: i64,
}

/// Feedback counts for a single memory (denormalized, no JOIN needed).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MemoryFeedback {
    pub useful: i32,
    pub irrelevant: i32,
    pub outdated: i32,
    pub wrong: i32,
}

/// Per-user adaptive retrieval parameters.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UserRetrievalParams {
    pub user_id: String,
    pub feedback_weight: f64,
    pub temporal_decay_hours: f64,
    pub confidence_weight: f64,
}

impl Default for UserRetrievalParams {
    fn default() -> Self {
        Self {
            user_id: String::new(),
            feedback_weight: 0.1,
            temporal_decay_hours: 168.0,
            confidence_weight: 0.1,
        }
    }
}

impl SqlMemoryStore {
    pub fn new(pool: MySqlPool, embedding_dim: usize, instance_id: String) -> Self {
        Self {
            pool,
            branch_alter_capability: Arc::new(
                crate::branch_capability::BranchAlterCapability::default(),
            ),
            call_log_tool_outcomes_ready: Arc::new(AtomicBool::new(false)),
            embedding_dim,
            instance_id,
            database_url: None,
            configured_max_connections: None,
            pool_health: Arc::new(std::sync::Mutex::new(PoolHealthSnapshot::new(None))),
            // Short TTL: multi-instance deployments without sticky sessions could
            // serve stale branch mappings after a branch switch on another instance.
            // 5s keeps the hot-path benefit while limiting the inconsistency window.
            active_table_cache: moka::sync::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(std::time::Duration::from_secs(5))
                .build(),
            cooldown_cache: moka::sync::Cache::builder()
                .max_capacity(1_000)
                .time_to_live(std::time::Duration::from_secs(7200)) // max cooldown is 2h
                .build(),
            node_count_cache: moka::sync::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(std::time::Duration::from_secs(120))
                .build(),
            edit_log_tx: Arc::new(std::sync::RwLock::new(None)),
            db_router: None,
            db_name: None,
        }
    }

    /// Set the edit-log channel sender (once, at startup).
    pub fn set_edit_log_tx(&self, tx: tokio::sync::mpsc::Sender<OwnedEditLogEntry>) {
        *self.edit_log_tx.write().unwrap() = Some(tx);
    }

    /// Clear the edit-log sender (shutdown drain). After this, log_edit falls back to direct INSERT.
    pub fn clear_edit_log_tx(&self) {
        *self.edit_log_tx.write().unwrap() = None;
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    /// Acquire a connection from the pool.
    ///
    /// NOTE: due to rust-lang/rust#100013 (async_trait + sqlx Executor lifetime),
    /// only use `conn()` in methods with ≤~18 queries. Routed stores must use
    /// qualified table names via `self.t()` (or receive an already-qualified
    /// `table` argument) instead of relying on session-level database switching.
    #[allow(dead_code)]
    async fn conn(&self) -> Result<sqlx::pool::PoolConnection<sqlx::MySql>, MemoriaError> {
        self.pool.acquire().await.map_err(db_err)
    }

    #[allow(dead_code)]
    async fn migration_pool(&self) -> Result<MySqlPool, MemoriaError> {
        if self.db_name.is_some() {
            let database_url = self.database_url.as_ref().ok_or_else(|| {
                MemoriaError::Internal("routed store missing database_url".into())
            })?;
            return sqlx::mysql::MySqlPoolOptions::new()
                .max_connections(1)
                .min_connections(0)
                .acquire_timeout(std::time::Duration::from_secs(10))
                .connect(database_url)
                .await
                .map_err(db_err);
        }
        Ok(self.pool.clone())
    }

    pub fn db_name(&self) -> Option<&str> {
        self.db_name.as_deref()
    }

    async fn current_schema_name(&self) -> Result<Cow<'_, str>, MemoriaError> {
        if let Some(schema_name) = self.db_name.as_deref().or_else(|| {
            self.database_url
                .as_deref()
                .and_then(parse_db_name_from_url)
        }) {
            return Ok(Cow::Borrowed(schema_name));
        }
        let schema_name = sqlx::query_scalar::<_, Option<String>>("SELECT DATABASE()")
            .fetch_one(&self.pool)
            .await
            .map_err(db_err)?
            .filter(|name| !name.is_empty())
            .ok_or_else(|| MemoriaError::Internal("store missing database name".into()))?;
        Ok(Cow::Owned(schema_name))
    }

    pub fn set_db_name(&mut self, name: String) {
        self.branch_alter_capability =
            Arc::new(crate::branch_capability::BranchAlterCapability::default());
        self.call_log_tool_outcomes_ready = Arc::new(AtomicBool::new(false));
        self.db_name = Some(name);
    }

    pub fn set_database_url(&mut self, url: String) {
        self.branch_alter_capability =
            Arc::new(crate::branch_capability::BranchAlterCapability::default());
        self.call_log_tool_outcomes_ready = Arc::new(AtomicBool::new(false));
        self.database_url = Some(url);
    }

    /// Qualify a table name with the database prefix when `db_name` is set.
    /// Returns bare table name for shared-DB stores, or `` `db`.table `` for per-user stores.
    pub fn t(&self, table: &str) -> String {
        if table.contains('.') || table.contains('`') {
            return table.to_string();
        }
        // Legacy Unicode physical names need identifier quoting on older MO
        // parsers. Preserve the existing representation of ASCII table names.
        let table = if table.is_ascii() {
            std::borrow::Cow::Borrowed(table)
        } else {
            std::borrow::Cow::Owned(quote_ident(table))
        };
        match &self.db_name {
            None => table.into_owned(),
            Some(db) => format!("`{}`.{}", db.replace('`', "``"), table),
        }
    }

    pub fn set_db_router(&mut self, router: Arc<DbRouter>) {
        self.db_router = Some(router);
    }

    pub fn db_router(&self) -> Option<Arc<DbRouter>> {
        self.db_router.clone()
    }

    pub fn database_name(&self) -> Option<&str> {
        self.db_name.as_deref().or_else(|| {
            self.database_url
                .as_deref()
                .and_then(parse_db_name_from_url)
        })
    }

    pub fn configured_max_connections(&self) -> Option<u32> {
        self.configured_max_connections
    }

    pub fn pool_health_snapshot(&self) -> PoolHealthSnapshot {
        self.pool_health.lock().unwrap().clone()
    }

    pub fn graph_store(&self) -> crate::graph::GraphStore {
        let mut gs = crate::graph::GraphStore::with_node_count_cache(
            self.pool.clone(),
            self.embedding_dim,
            self.node_count_cache.clone(),
        );
        if let Some(db) = &self.db_name {
            gs.set_db_name(db.clone());
        }
        gs
    }

    pub async fn connect(
        database_url: &str,
        embedding_dim: usize,
        instance_id: String,
    ) -> Result<Self, MemoriaError> {
        const DB_MAX_CONNECTIONS_UPPER: u32 = 512;
        let max_conns =
            configured_max_connections("DB_MAX_CONNECTIONS", 64, DB_MAX_CONNECTIONS_UPPER);
        Self::connect_with_pool_limit(
            database_url,
            embedding_dim,
            instance_id,
            max_conns,
            true,
            true,
            "main_sql_store",
        )
        .await
    }

    pub async fn connect_shared(
        database_url: &str,
        embedding_dim: usize,
        instance_id: String,
    ) -> Result<Self, MemoriaError> {
        const SHARED_DB_MAX_CONNECTIONS_UPPER: u32 = 128;
        let max_conns = configured_max_connections(
            "MEMORIA_SHARED_MAIN_POOL_MAX_CONNECTIONS",
            12,
            SHARED_DB_MAX_CONNECTIONS_UPPER,
        );
        Self::connect_with_pool_limit(
            database_url,
            embedding_dim,
            instance_id,
            max_conns,
            true,
            true,
            "shared_sql_store",
        )
        .await
    }

    pub async fn connect_routed(
        database_url: &str,
        embedding_dim: usize,
        instance_id: String,
    ) -> Result<Self, MemoriaError> {
        const ROUTED_DB_MAX_CONNECTIONS_UPPER: u32 = 64;
        let max_conns = configured_max_connections(
            "MEMORIA_ROUTED_DB_MAX_CONNECTIONS",
            1,
            ROUTED_DB_MAX_CONNECTIONS_UPPER,
        );
        Self::connect_with_pool_limit(
            database_url,
            embedding_dim,
            instance_id,
            max_conns,
            false,
            false,
            "routed_sql_store",
        )
        .await
    }

    pub fn from_existing_pool(
        pool: MySqlPool,
        embedding_dim: usize,
        instance_id: String,
        database_url: Option<String>,
        configured_max_connections: Option<u32>,
        pool_name: &'static str,
    ) -> Self {
        let mut store = Self::new(pool.clone(), embedding_dim, instance_id);
        store.database_url = database_url;
        store.configured_max_connections = configured_max_connections;
        {
            let mut health = store.pool_health.lock().unwrap();
            health.configured_max_connections = configured_max_connections;
        }
        spawn_pool_monitor(
            pool,
            configured_max_connections,
            store.pool_health.clone(),
            pool_name,
        );
        store
    }

    async fn connect_with_pool_limit(
        database_url: &str,
        embedding_dim: usize,
        instance_id: String,
        max_conns: u32,
        log_info: bool,
        ensure_database_exists: bool,
        pool_name: &'static str,
    ) -> Result<Self, MemoriaError> {
        if ensure_database_exists {
            // Auto-create database if it doesn't exist
            if let Some((base_url, db_name, _suffix)) = split_database_url(database_url) {
                let base_pool = sqlx::mysql::MySqlPoolOptions::new()
                    .max_connections(1)
                    .connect(base_url)
                    .await;
                if let Ok(base_pool) = base_pool {
                    let _ = sqlx::raw_sql(&format!(
                        "CREATE DATABASE IF NOT EXISTS {}",
                        quote_ident(db_name)
                    ))
                    .execute(&base_pool)
                    .await;
                }
            }
        }
        let max_lifetime_secs: u64 = std::env::var("DB_MAX_LIFETIME_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(3600);
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(max_conns)
            .max_lifetime(std::time::Duration::from_secs(max_lifetime_secs))
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(10))
            .connect(database_url)
            .await
            .map_err(db_err)?;
        if log_info {
            tracing::info!(
                max_connections = max_conns,
                max_lifetime_secs = max_lifetime_secs,
                "Main connection pool initialized"
            );
        }
        Ok(Self::from_existing_pool(
            pool,
            embedding_dim,
            instance_id,
            Some(database_url.to_string()),
            Some(max_conns),
            pool_name,
        ))
    }

    /// Create a small isolated pool for background tasks (DDL, maintenance).
    /// Returns an error if no database_url was stored or if pool creation fails.
    pub async fn spawn_background_store(
        &self,
        max_connections: u32,
    ) -> Result<std::sync::Arc<Self>, MemoriaError> {
        let url = self.database_url.as_deref().ok_or_else(|| {
            MemoriaError::Internal("background pool requires database_url".into())
        })?;
        match sqlx::mysql::MySqlPoolOptions::new()
            .max_connections(max_connections)
            .max_lifetime(std::time::Duration::from_secs(3600))
            .idle_timeout(std::time::Duration::from_secs(300))
            .acquire_timeout(std::time::Duration::from_secs(30))
            .connect(url)
            .await
        {
            Ok(pool) => {
                tracing::info!(
                    max_connections = max_connections,
                    "Background connection pool initialized"
                );
                let mut s = Self::new(pool, self.embedding_dim, self.instance_id.clone());
                s.database_url = self.database_url.clone();
                s.configured_max_connections = Some(max_connections);
                {
                    let mut health = s.pool_health.lock().unwrap();
                    health.configured_max_connections = Some(max_connections);
                }
                // Share the same edit_log_tx Arc so clear_edit_log_tx drains all stores at once
                s.edit_log_tx = self.edit_log_tx.clone();
                s.db_router = self.db_router.clone();
                Ok(std::sync::Arc::new(s))
            }
            Err(e) => Err(db_err(e)),
        }
    }

    pub async fn migrate(&self) -> Result<(), MemoriaError> {
        self.migrate_user().await?;
        self.migrate_shared().await?;
        Ok(())
    }

    async fn bootstrap_user_schema(&self, pool: &MySqlPool) -> Result<(), MemoriaError> {
        let memories_table = self.t("mem_memories");
        let user_state_table = self.t("mem_user_state");
        let branches_table = self.t("mem_branches");
        let snapshots_table = self.t("mem_snapshots");
        let cooldown_table = self.t("mem_governance_cooldown");
        let entity_links_table = self.t("mem_entity_links");
        let memories_stats_table = self.t("mem_memories_stats");
        let edit_log_table = self.t("mem_edit_log");
        let retrieval_feedback_table = self.t("mem_retrieval_feedback");
        let retrieval_params_table = self.t("mem_user_retrieval_params");
        let tool_usage_table = self.t("mem_tool_usage");
        let api_call_log_table = self.t("mem_api_call_log");
        let sql = format!(
            r#"CREATE TABLE IF NOT EXISTS {memories_table} (
                memory_id       VARCHAR(64)  PRIMARY KEY,
                user_id         VARCHAR(64)  NOT NULL,
                author_id       VARCHAR(64)  DEFAULT NULL,
                subject_id      VARCHAR(128) DEFAULT NULL,
                memory_type     VARCHAR(20)  NOT NULL,
                content         TEXT         NOT NULL,
                embedding       vecf32({dim}),
                session_id      VARCHAR(64),
                source_event_ids JSON        NOT NULL,
                extra_metadata  JSON, -- MO#23859: NULL avoided at bind level
                is_active       TINYINT(1)   NOT NULL DEFAULT 1,
                superseded_by   VARCHAR(64),
                trust_tier      VARCHAR(10)  DEFAULT 'T1',
                initial_confidence FLOAT     DEFAULT 0.95,
                observed_at     DATETIME(6)  NOT NULL,
                created_at      DATETIME(6)  NOT NULL,
                updated_at      DATETIME(6),
                INDEX idx_user_active (user_id, is_active, memory_type),
                INDEX idx_user_session (user_id, session_id),
                INDEX idx_memories_user_observed (user_id, observed_at),
                INDEX idx_author (author_id),
                INDEX idx_scope_subject_active (user_id, subject_id, is_active, memory_type),
                FULLTEXT INDEX ft_content (content) WITH PARSER ngram -- MO#23861: breaks on concurrent snapshot restore
            )"#,
            memories_table = memories_table,
            dim = self.embedding_dim
        );
        sqlx::query(&sql).execute(pool).await.map_err(db_err)?;

        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {user_state_table} (
                user_id       VARCHAR(64)  PRIMARY KEY,
                active_branch VARCHAR(100) NOT NULL DEFAULT 'main',
                updated_at    DATETIME(6)
            )"#,
        ))
        .execute(pool)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {branches_table} (
                id          VARCHAR(64)  PRIMARY KEY,
                user_id     VARCHAR(64)  NOT NULL,
                name        VARCHAR(100) NOT NULL,
                table_name  VARCHAR(100) NOT NULL,
                status      VARCHAR(20)  NOT NULL DEFAULT 'active',
                created_at  DATETIME(6)  NOT NULL,
                INDEX idx_user_name (user_id, name)
            )"#,
        ))
        .execute(pool)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {snapshots_table} (
                id             VARCHAR(64)  PRIMARY KEY,
                user_id        VARCHAR(64)  NOT NULL,
                name           VARCHAR(100) NOT NULL,
                snapshot_name  VARCHAR(100) NOT NULL,
                extra          JSON         DEFAULT NULL,
                status         VARCHAR(20)  NOT NULL DEFAULT 'active',
                created_at     DATETIME(6)  NOT NULL,
                INDEX idx_user_snapshot_name (user_id, name, status),
                INDEX idx_user_snapshot_internal (user_id, snapshot_name, status)
            )"#,
        ))
        .execute(pool)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {cooldown_table} (
                user_id     VARCHAR(64)  NOT NULL,
                operation   VARCHAR(32)  NOT NULL,
                last_run_at DATETIME(6)  NOT NULL,
                PRIMARY KEY (user_id, operation)
            )"#,
        ))
        .execute(pool)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {entity_links_table} (
                id          VARCHAR(64)  PRIMARY KEY,
                user_id     VARCHAR(64)  NOT NULL,
                memory_id   VARCHAR(64)  NOT NULL,
                entity_name VARCHAR(200) NOT NULL,
                entity_type VARCHAR(50)  NOT NULL DEFAULT 'concept',
                source      VARCHAR(20)  NOT NULL DEFAULT 'manual',
                created_at  DATETIME(6)  NOT NULL,
                INDEX idx_user_memory (user_id, memory_id),
                INDEX idx_user_entity (user_id, entity_name)
            )"#,
        ))
        .execute(pool)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {memories_stats_table} (
                memory_id        VARCHAR(64)  PRIMARY KEY,
                access_count     INT          NOT NULL DEFAULT 0,
                last_accessed_at DATETIME(6),
                feedback_useful  INT          NOT NULL DEFAULT 0,
                feedback_irrelevant INT       NOT NULL DEFAULT 0,
                feedback_outdated INT         NOT NULL DEFAULT 0,
                feedback_wrong   INT          NOT NULL DEFAULT 0,
                last_feedback_at DATETIME(6)
            )"#,
        ))
        .execute(pool)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {edit_log_table} (
                edit_id         VARCHAR(64)  NOT NULL,
                user_id         VARCHAR(64)  NOT NULL,
                memory_id       VARCHAR(64)  DEFAULT NULL,
                operation       VARCHAR(64)  NOT NULL,
                payload         JSON         DEFAULT NULL,
                reason          TEXT         DEFAULT NULL,
                snapshot_before VARCHAR(64)  DEFAULT NULL,
                created_at      DATETIME(6)  NOT NULL DEFAULT NOW(),
                created_by      VARCHAR(64)  NOT NULL,
                INDEX idx_user_time (user_id, created_at),
                INDEX idx_memory_time (memory_id, created_at)
            ) CLUSTER BY (created_at, user_id)"#,
        ))
        .execute(pool)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {retrieval_feedback_table} (
                id          VARCHAR(64)  PRIMARY KEY,
                user_id     VARCHAR(64)  NOT NULL,
                memory_id   VARCHAR(64)  NOT NULL,
                signal      VARCHAR(16)  NOT NULL,
                context     TEXT         DEFAULT NULL,
                created_at  DATETIME(6)  NOT NULL,
                INDEX idx_feedback_user (user_id, created_at),
                INDEX idx_feedback_memory (memory_id),
                INDEX idx_feedback_memory_user (user_id, memory_id),
                INDEX idx_feedback_created_at (created_at)
            )"#,
        ))
        .execute(pool)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {retrieval_params_table} (
                user_id              VARCHAR(64)  PRIMARY KEY,
                feedback_weight      DOUBLE       NOT NULL DEFAULT 0.1,
                temporal_decay_hours DOUBLE       NOT NULL DEFAULT 168.0,
                confidence_weight    DOUBLE       NOT NULL DEFAULT 0.1,
                updated_at           DATETIME(6)  NOT NULL
            )"#,
        ))
        .execute(pool)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {tool_usage_table} (
                user_id      VARCHAR(64)  NOT NULL,
                tool_name    VARCHAR(128) NOT NULL,
                last_used_at DATETIME(6)  NOT NULL,
                PRIMARY KEY (user_id, tool_name)
            )"#,
        ))
        .execute(pool)
        .await
        .map_err(db_err)?;

        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {api_call_log_table} (
                id              BIGINT       NOT NULL AUTO_INCREMENT,
                user_id         VARCHAR(64)  NOT NULL,
                method          VARCHAR(10)  NOT NULL DEFAULT '',
                path            VARCHAR(256) NOT NULL,
                status_code     SMALLINT     NOT NULL DEFAULT 0,
                latency_ms      INT          NOT NULL DEFAULT 0,
                called_at       DATETIME(6)  NOT NULL DEFAULT NOW(6),
                rpc_success     TINYINT(1)   NOT NULL DEFAULT 1,
                rpc_error_code  INT          NULL,
                tool_success    TINYINT(1)   NULL,
                tool_error_kind VARCHAR(32) NULL,
                PRIMARY KEY (id),
                INDEX idx_user_called (user_id, called_at)
            )"#,
        ))
        .execute(pool)
        .await
        .map_err(db_err)?;

        self.graph_store().migrate().await?;
        Ok(())
    }

    async fn apply_user_compat_migrations(&self, pool: &MySqlPool) -> Result<(), MemoriaError> {
        // This legacy migration may ALTER both the parent and its branches.
        // Check before any such DDL, not after an older engine materializes them.
        if self.has_registered_branches().await? {
            self.ensure_native_branch_alter_capability().await?;
        }
        let schema_name = self.current_schema_name().await?;
        let schema_name = schema_name.as_ref();
        let memories_stats_table = self.t("mem_memories_stats");
        let edit_log_table = self.t("mem_edit_log");
        let memories_table = self.t("mem_memories");
        let branches_table = self.t("mem_branches");
        let api_call_log_table = self.t("mem_api_call_log");
        let retrieval_feedback_table = self.t("mem_retrieval_feedback");
        let graph_nodes_table = self.t("memory_graph_nodes");

        let _ = sqlx::query(&format!(
            "ALTER TABLE {memories_stats_table} ADD COLUMN feedback_useful INT NOT NULL DEFAULT 0"
        ))
        .execute(pool)
        .await;
        let _ = sqlx::query(&format!(
            "ALTER TABLE {memories_stats_table} ADD COLUMN feedback_irrelevant INT NOT NULL DEFAULT 0"
        ))
        .execute(pool)
        .await;
        let _ = sqlx::query(&format!(
            "ALTER TABLE {memories_stats_table} ADD COLUMN feedback_outdated INT NOT NULL DEFAULT 0"
        ))
        .execute(pool)
        .await;
        let _ = sqlx::query(&format!(
            "ALTER TABLE {memories_stats_table} ADD COLUMN feedback_wrong INT NOT NULL DEFAULT 0"
        ))
        .execute(pool)
        .await;
        let _ = sqlx::query(&format!(
            "ALTER TABLE {memories_stats_table} ADD COLUMN last_feedback_at DATETIME(6)"
        ))
        .execute(pool)
        .await;

        let _ = sqlx::query(&format!(
            "ALTER TABLE {edit_log_table} ADD COLUMN memory_id VARCHAR(64) DEFAULT NULL"
        ))
        .execute(pool)
        .await;
        let _ = sqlx::query(&format!(
            "ALTER TABLE {edit_log_table} ADD COLUMN payload JSON DEFAULT NULL"
        ))
        .execute(pool)
        .await;

        let _ = sqlx::query(&format!(
            "ALTER TABLE {memories_table} ADD COLUMN extra_metadata JSON AFTER source_event_ids"
        ))
        .execute(pool)
        .await;

        let needs_upgrade = !info_schema_index_column_exists(
            pool,
            schema_name,
            "mem_memories",
            "idx_user_active",
            "memory_type",
        )
        .await;
        if needs_upgrade {
            let _ = sqlx::query(&format!(
                "ALTER TABLE {memories_table} DROP INDEX idx_user_active"
            ))
            .execute(pool)
            .await;
            let _ = sqlx::query(&format!(
                "ALTER TABLE {memories_table} ADD INDEX idx_user_active (user_id, is_active, memory_type)"
            ))
            .execute(pool)
            .await;
        }

        let has_table_name =
            info_schema_column_exists(pool, schema_name, "mem_branches", "table_name").await;
        if !has_table_name {
            let _ = sqlx::query(&format!(
                "ALTER TABLE {branches_table} ADD COLUMN table_name VARCHAR(100) NOT NULL DEFAULT ''"
            ))
            .execute(pool)
            .await;
        }

        let has_id = info_schema_column_exists(pool, schema_name, "mem_branches", "id").await;
        if !has_id {
            let _ = sqlx::query(&format!("DROP TABLE IF EXISTS {branches_table}"))
                .execute(pool)
                .await;
            sqlx::query(&format!(
                r#"CREATE TABLE IF NOT EXISTS {branches_table} (
                    id          VARCHAR(64)  PRIMARY KEY,
                    user_id     VARCHAR(64)  NOT NULL,
                    name        VARCHAR(100) NOT NULL,
                    table_name  VARCHAR(100) NOT NULL,
                    status      VARCHAR(20)  NOT NULL DEFAULT 'active',
                    created_at  DATETIME(6)  NOT NULL,
                    INDEX idx_user_name (user_id, name)
                )"#
            ))
            .execute(pool)
            .await
            .map_err(db_err)?;
        }

        let has_method_col =
            info_schema_column_exists(pool, schema_name, "mem_api_call_log", "method").await;
        if !has_method_col {
            let _ = sqlx::query(&format!(
                "ALTER TABLE {api_call_log_table} ADD COLUMN method VARCHAR(10) NOT NULL DEFAULT ''"
            ))
            .execute(pool)
            .await;
        }

        let add_rpc_success = sqlx::query(&format!(
            "ALTER TABLE {api_call_log_table} ADD COLUMN rpc_success TINYINT(1) NOT NULL DEFAULT 1"
        ))
        .execute(pool)
        .await;
        if let Err(e) = add_rpc_success {
            if !is_duplicate_column(&e) {
                tracing::error!(
                    error = %e,
                    "Migration fatal: mem_api_call_log.rpc_success could not be added. \
                     The call-log writer always inserts this column; without it ALL \
                     call-log flushes will fail with 'unknown column', silently dropping \
                     every /v1/* and /mcp monitoring entry. \
                     Fix DB permissions or add the column manually, then restart."
                );
                return Err(db_err(e));
            }
        }

        let add_rpc_error_code = sqlx::query(&format!(
            "ALTER TABLE {api_call_log_table} ADD COLUMN rpc_error_code INT NULL"
        ))
        .execute(pool)
        .await;
        if let Err(e) = add_rpc_error_code {
            if !is_duplicate_column(&e) {
                tracing::error!(
                    error = %e,
                    "Migration fatal: mem_api_call_log.rpc_error_code could not be added. \
                     The call-log writer always inserts this column; without it ALL \
                     call-log flushes will fail with 'unknown column'. \
                     Fix DB permissions or add the column manually, then restart."
                );
                return Err(db_err(e));
            }
        }

        self.ensure_call_log_tool_outcome_columns(pool, schema_name)
            .await?;

        let has_feedback_memory_user_idx = info_schema_index_exists(
            pool,
            schema_name,
            "mem_retrieval_feedback",
            "idx_feedback_memory_user",
        )
        .await;
        if !has_feedback_memory_user_idx {
            let _ = sqlx::query(&format!(
                "ALTER TABLE {retrieval_feedback_table} ADD INDEX idx_feedback_memory_user (user_id, memory_id)"
            ))
            .execute(pool)
            .await;
        }

        let has_feedback_created_at_idx = info_schema_index_exists(
            pool,
            schema_name,
            "mem_retrieval_feedback",
            "idx_feedback_created_at",
        )
        .await;
        if !has_feedback_created_at_idx {
            let _ = sqlx::query(&format!(
                "ALTER TABLE {retrieval_feedback_table} ADD INDEX idx_feedback_created_at (created_at)"
            ))
            .execute(pool)
            .await;
        }

        let has_memories_user_observed_idx = info_schema_index_exists(
            pool,
            schema_name,
            "mem_memories",
            "idx_memories_user_observed",
        )
        .await;
        if !has_memories_user_observed_idx {
            let _ = sqlx::query(&format!(
                "ALTER TABLE {memories_table} ADD INDEX idx_memories_user_observed (user_id, observed_at)"
            ))
            .execute(pool)
            .await;
        }

        let has_user_active_created_idx =
            info_schema_index_exists(pool, schema_name, "mem_memories", "idx_user_active_created")
                .await;
        if has_user_active_created_idx {
            let _ = sqlx::query(&format!(
                "ALTER TABLE {memories_table} DROP INDEX idx_user_active_created"
            ))
            .execute(pool)
            .await;
        }

        let has_empty_superseded = query_has_rows(
            pool,
            &format!("SELECT COUNT(*) FROM {memories_table} WHERE superseded_by = ''"),
        )
        .await;
        if has_empty_superseded {
            for (tbl, col) in [
                (memories_table.clone(), "superseded_by"),
                (memories_table.clone(), "session_id"),
                (graph_nodes_table.clone(), "superseded_by"),
                (graph_nodes_table.clone(), "session_id"),
                (graph_nodes_table.clone(), "memory_id"),
                (graph_nodes_table.clone(), "entity_type"),
                (graph_nodes_table.clone(), "conflicts_with"),
                (graph_nodes_table.clone(), "conflict_resolution"),
            ] {
                if let Err(e) =
                    sqlx::query(&format!("UPDATE {tbl} SET {col} = NULL WHERE {col} = ''"))
                        .execute(pool)
                        .await
                {
                    tracing::warn!(table = tbl, column = col, error = %e, "MO#24001 migration: failed to normalize empty strings");
                }
            }
        }

        let _ = sqlx::raw_sql(&format!(
            "UPDATE {memories_table} SET embedding = NULL \
             WHERE embedding IS NOT NULL AND vector_dims(embedding) = 0"
        ))
        .execute(pool)
        .await;

        self.ensure_author_id_column(pool, schema_name).await?;

        // subject_id migration is handled unconditionally by ensure_subject_id_column(),
        // which is called before the schema-version short-circuit in migrate_user().
        // Do NOT add subject_id DDL here — it would duplicate work and use an older,
        // less robust implementation (no exec_ddl_with_retry / is_mo_concurrent_ddl_race).

        Ok(())
    }

    /// Idempotently repair `author_id` on the parent and registered branches.
    ///
    /// This must run before the schema-version short-circuit: an older migration
    /// skipped Unicode branch names but still stored schema version 2.
    async fn ensure_author_id_column(
        &self,
        pool: &MySqlPool,
        schema_name: &str,
    ) -> Result<(), MemoriaError> {
        let memories_exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=? AND table_name='mem_memories'",
        )
        .bind(schema_name)
        .fetch_one(pool)
        .await
        .map_err(db_err)?;
        if memories_exists == 0 {
            return Ok(());
        }
        let memories_table = self.t("mem_memories");
        let branches_table = self.t("mem_branches");
        // Very old/partial schemas are repaired by apply_user_compat_migrations
        // after bootstrap. A version-2 registry has both of these columns.
        let registry_columns: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.columns WHERE table_schema=? AND table_name='mem_branches' AND column_name IN ('table_name','status')",
        )
        .bind(schema_name)
        .fetch_one(pool)
        .await
        .map_err(db_err)?;
        let registry_ready = registry_columns == 2;
        let branch_table_names: Vec<String> = if registry_ready {
            sqlx::query_scalar(&format!(
                "SELECT table_name FROM {branches_table} WHERE status='active' AND table_name != ''"
            ))
            .fetch_all(pool)
            .await
            .map_err(db_err)?
        } else {
            Vec::new()
        };
        let has_registered_branches = !branch_table_names.is_empty();

        if !info_schema_column_exists(pool, schema_name, "mem_memories", "author_id").await {
            if has_registered_branches {
                self.ensure_native_branch_alter_capability().await?;
            }
            let ddl = format!(
                "ALTER TABLE {memories_table} ADD COLUMN author_id VARCHAR(64) DEFAULT NULL"
            );
            if let Err(error) = exec_ddl_with_retry(pool, &ddl).await {
                if !is_duplicate_column(&error) && !is_mo_concurrent_ddl_race(&error) {
                    return Err(db_err(error));
                }
                tracing::debug!(%error, "rechecking parent author_id after concurrent DDL");
            }
        }
        if !info_schema_column_exists(pool, schema_name, "mem_memories", "author_id").await {
            return Err(MemoriaError::Database(
                "Parent author_id migration did not complete".into(),
            ));
        }

        if !info_schema_index_exists(pool, schema_name, "mem_memories", "idx_author").await {
            let may_alter = if has_registered_branches {
                match self.ensure_native_branch_alter_capability().await {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(%error, "skipping optional parent author index: ALTER capability unverified");
                        false
                    }
                }
            } else {
                true
            };
            if may_alter {
                let ddl = format!("ALTER TABLE {memories_table} ADD INDEX idx_author (author_id)");
                if let Err(error) = exec_ddl_with_retry(pool, &ddl).await {
                    tracing::warn!(%error, "parent author index unavailable; continuing without it");
                }
            }
        }
        if branch_table_names.is_empty() {
            return Ok(());
        }

        let current =
            crate::table_schema::read_table_columns(pool, schema_name, "mem_memories").await?;
        let position = current
            .iter()
            .position(|column| column.name.eq_ignore_ascii_case("author_id"))
            .ok_or_else(|| MemoriaError::Database("Current table is missing author_id".into()))?;
        for table in branch_table_names {
            if !memoria_core::is_safe_sql_identifier(&table) {
                tracing::warn!(%table, "skipping branch with invalid physical identifier");
                continue;
            }
            if !info_schema_column_exists(pool, schema_name, &table, "author_id").await {
                self.ensure_native_branch_alter_capability().await?;
                let mut ddl = sqlx::QueryBuilder::<sqlx::MySql>::new("ALTER TABLE ");
                ddl.push(self.t(&table))
                    .push(" ADD COLUMN author_id VARCHAR(64) DEFAULT NULL");
                if position == 0 {
                    ddl.push(" FIRST");
                } else {
                    ddl.push(" AFTER `")
                        .push(current[position - 1].name.replace('`', "``"))
                        .push("`");
                }
                if let Err(error) = exec_ddl_with_retry(pool, &ddl.into_sql()).await {
                    if !is_duplicate_column(&error) && !is_mo_concurrent_ddl_race(&error) {
                        return Err(db_err(error));
                    }
                    tracing::debug!(%error, %table, "rechecking branch author_id after concurrent DDL");
                }
            }
            if !info_schema_column_exists(pool, schema_name, &table, "author_id").await {
                return Err(MemoriaError::Database(format!(
                    "Branch author_id migration did not complete for '{table}'"
                )));
            }
            if !info_schema_index_exists(pool, schema_name, &table, "idx_author").await {
                if let Err(error) = self.ensure_native_branch_alter_capability().await {
                    tracing::warn!(%error, %table, "skipping optional branch author index: ALTER capability unverified");
                    continue;
                }
                let ddl = format!(
                    "ALTER TABLE {} ADD INDEX idx_author (author_id)",
                    self.t(&table)
                );
                if let Err(error) = exec_ddl_with_retry(pool, &ddl).await {
                    tracing::warn!(%error, %table, "branch author index unavailable; continuing without it");
                }
            }
        }
        Ok(())
    }

    /// Idempotent migration: add `subject_id` column + composite index to `mem_memories`
    /// and any existing branch tables.
    ///
    /// Must run unconditionally on every `migrate_user` call (before the schema-version
    /// short-circuit), because it was introduced after `CURRENT_USER_SCHEMA_VERSION` was
    /// already set to 2 for all live deployments.
    ///
    /// Failure semantics:
    /// - Unexpected failure adding the column to the **main table** → returns `Err` (fail-fast,
    ///   because all subsequent SELECT/INSERT referencing `subject_id` would panic at the DB layer).
    /// - Index failures → warn-logged only (performance degradation, not correctness).
    /// - Branch-table column failures → error-logged only (branch operations degrade gracefully).
    async fn ensure_subject_id_column(
        &self,
        pool: &MySqlPool,
        schema_name: &str,
    ) -> Result<(), MemoriaError> {
        let memories_table = self.t("mem_memories");
        let branches_table = self.t("mem_branches");

        // ── main table ────────────────────────────────────────────────────────
        // If mem_memories does not exist yet (non-fresh DB that hasn't been fully
        // bootstrapped), skip the ALTER — bootstrap_user_schema() will create the
        // table with subject_id already in the schema definition.
        if !info_schema_table_exists(pool, schema_name, "mem_memories").await {
            return Ok(());
        }

        let has_col =
            info_schema_column_exists(pool, schema_name, "mem_memories", "subject_id").await;
        if !has_col {
            if self.has_registered_branches().await? {
                self.ensure_native_branch_alter_capability().await?;
            }
            match exec_ddl_with_retry(
                pool,
                &format!(
                    "ALTER TABLE {memories_table} ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL"
                ),
            )
            .await
            {
                Ok(_) => tracing::info!("migration: added subject_id column to {memories_table}"),
                Err(e) if is_duplicate_column(&e) => tracing::debug!(
                    "migration: subject_id column already exists in {memories_table}, skipping"
                ),
                Err(e) if is_mo_concurrent_ddl_race(&e) => tracing::warn!(
                    "migration: concurrent DDL race for subject_id on {memories_table} \
                     (column was added by a concurrent request): {e}"
                ),
                Err(e) => {
                    tracing::error!(
                        "migration: failed to add subject_id to {memories_table}: {e}"
                    );
                    return Err(db_err(e));
                }
            }
        }

        let has_idx = info_schema_index_exists(
            pool,
            schema_name,
            "mem_memories",
            "idx_scope_subject_active",
        )
        .await;
        if !has_idx {
            let may_alter = if self.has_registered_branches().await? {
                match self.ensure_native_branch_alter_capability().await {
                    Ok(()) => true,
                    Err(error) => {
                        tracing::warn!(%error, "skipping optional parent index: ALTER capability unverified");
                        false
                    }
                }
            } else {
                true
            };
            if may_alter {
                match exec_ddl_with_retry(
                    pool,
                    &format!(
                        "ALTER TABLE {memories_table} ADD INDEX idx_scope_subject_active \
                         (user_id, subject_id, is_active, memory_type)"
                    ),
                )
                .await
                {
                    Ok(_) => tracing::info!(
                        "migration: added idx_scope_subject_active on {memories_table}"
                    ),
                    Err(e) if is_duplicate_index(&e) => tracing::debug!(
                        "migration: idx_scope_subject_active already exists on {memories_table}, skipping"
                    ),
                    Err(e) if is_mo_concurrent_ddl_race(&e) => tracing::warn!(
                        "migration: concurrent DDL race for idx_scope_subject_active on \
                         {memories_table}: {e}"
                    ),
                    Err(e) => tracing::warn!(
                        "migration: failed to add idx_scope_subject_active on {memories_table}: {e}"
                    ),
                }
            }
        }

        // ── branch tables ─────────────────────────────────────────────────────
        let branch_table_names: Vec<String> = match sqlx::query_scalar(&format!(
            "SELECT table_name FROM {branches_table} WHERE status = 'active' AND table_name != ''"
        ))
        .fetch_all(pool)
        .await
        {
            Ok(names) => names,
            Err(e) => {
                tracing::warn!(
                    "migration: failed to load branch table names from {branches_table}, \
                     skipping subject_id migration for branch tables: {e}"
                );
                vec![]
            }
        };

        for table in &branch_table_names {
            // Existing branches remain best-effort at startup. New clones use
            // the same migration below, but must pass it before registration.
            if let Err(error) = self.migrate_branch_subject_id(table, schema_name).await {
                tracing::error!(%error, %table, "branch subject_id migration failed");
            }
        }

        Ok(())
    }

    /// A historical clone must have usable columns before it can be registered.
    /// The subject index is a best-effort performance optimization.
    pub async fn ensure_branch_subject_id(&self, table: &str) -> Result<(), MemoriaError> {
        if !table.starts_with("br_") {
            return Err(MemoriaError::Validation("Invalid branch table name".into()));
        }
        let schema = self.current_schema_name().await?;
        self.migrate_branch_subject_id(table, schema.as_ref()).await
    }

    /// Check on disposable tables before ALTER may affect native branch lineage.
    pub async fn ensure_native_branch_alter_capability(&self) -> Result<(), MemoriaError> {
        let schema = self.current_schema_name().await?;
        self.branch_alter_capability
            .ensure(&self.pool, &schema)
            .await
    }

    async fn has_registered_branches(&self) -> Result<bool, MemoriaError> {
        let schema = self.current_schema_name().await?;
        // Unlike best-effort metadata helpers, a safety gate must not turn a
        // catalog read failure into "no branches" and allow an unchecked ALTER.
        let exists: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM information_schema.tables WHERE table_schema=? AND table_name='mem_branches'",
        ).bind(schema.as_ref()).fetch_one(&self.pool).await.map_err(db_err)?;
        if exists == 0 {
            return Ok(false);
        }
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {} WHERE status='active' AND table_name != ''",
            self.t("mem_branches")
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(count != 0)
    }

    async fn migrate_branch_subject_id(
        &self,
        table: &str,
        schema: &str,
    ) -> Result<(), MemoriaError> {
        if !memoria_core::is_safe_sql_identifier(table) {
            return Err(MemoriaError::Validation("Invalid branch table name".into()));
        }
        let current =
            crate::table_schema::read_table_columns(&self.pool, schema, "mem_memories").await?;
        if !info_schema_column_exists(&self.pool, schema, table, "subject_id").await {
            self.ensure_native_branch_alter_capability().await?;
            let position = current
                .iter()
                .position(|column| column.name.eq_ignore_ascii_case("subject_id"))
                .ok_or_else(|| {
                    MemoriaError::Database("Current table is missing subject_id".into())
                })?;
            let mut ddl = sqlx::QueryBuilder::<sqlx::MySql>::new("ALTER TABLE ");
            ddl.push(self.t(table))
                .push(" ADD COLUMN subject_id VARCHAR(128) DEFAULT NULL");
            // Match the live ordinal, not merely the name: some MO versions
            // silently misinterpret native diffs between differently ordered tables.
            if position == 0 {
                ddl.push(" FIRST");
            } else {
                ddl.push(" AFTER `")
                    .push(current[position - 1].name.replace('`', "``"))
                    .push("`");
            }
            if let Err(error) = exec_ddl_with_retry(&self.pool, &ddl.into_sql()).await {
                if !is_duplicate_column(&error) && !is_mo_concurrent_ddl_race(&error) {
                    return Err(db_err(error));
                }
                tracing::debug!(%error, %table, "rechecking branch column after concurrent DDL");
            }
        }
        // Subject migration is the only schema change this path performs.
        // Require ALL live column names afterwards, including nullable/defaulted
        // columns: current INSERT/SELECT statements explicitly reference them.
        let columns = crate::table_schema::read_table_columns(&self.pool, schema, table).await?;
        let present: Vec<String> = columns.iter().map(|c| c.name.clone()).collect();
        if !present
            .iter()
            .any(|name| name.eq_ignore_ascii_case("subject_id"))
        {
            return Err(MemoriaError::Database(
                "Branch subject_id migration did not complete".into(),
            ));
        }
        if let Some(column) = crate::table_schema::missing_columns(&current, &present).first() {
            return Err(MemoriaError::Database(format!(
                "Branch schema is missing required column '{}'",
                column.name
            )));
        }
        crate::table_schema::validate_native_branch_schema(&current, &columns)?;
        if !info_schema_index_exists(&self.pool, schema, table, "idx_scope_subject_active").await {
            if let Err(error) = self.ensure_native_branch_alter_capability().await {
                tracing::warn!(%error, %table, "skipping optional branch index: ALTER capability unverified");
                return Ok(());
            }
            let mut ddl = sqlx::QueryBuilder::<sqlx::MySql>::new("ALTER TABLE ");
            ddl.push(self.t(table)).push(
                " ADD INDEX idx_scope_subject_active (user_id, subject_id, is_active, memory_type)",
            );
            if let Err(error) = exec_ddl_with_retry(&self.pool, &ddl.into_sql()).await {
                // Missing columns have already failed above. Index creation
                // errors, including MO concurrent-DDL races, must not discard a
                // usable branch. A subsequent migration can retry the index.
                tracing::warn!(%error, %table, "branch subject index unavailable; continuing without it");
            }
        }
        Ok(())
    }

    async fn ensure_snapshot_extra_column(
        &self,
        pool: &MySqlPool,
        schema_name: &str,
    ) -> Result<(), MemoriaError> {
        // Targeted migration: old user DBs can stay at the same schema version,
        // and missing derived values are populated lazily when snapshots are read.
        if !info_schema_table_exists(pool, schema_name, "mem_snapshots").await
            || info_schema_column_exists(pool, schema_name, "mem_snapshots", "extra").await
        {
            return Ok(());
        }

        let snapshots_table = self.t("mem_snapshots");
        let started = std::time::Instant::now();
        let result = sqlx::query(&format!(
            "ALTER TABLE {snapshots_table} ADD COLUMN extra JSON DEFAULT NULL AFTER snapshot_name"
        ))
        .execute(pool)
        .await;
        match result {
            Ok(_) => {
                tracing::info!(
                    schema_name,
                    elapsed_ms = started.elapsed().as_millis(),
                    "migration: added mem_snapshots.extra"
                );
                Ok(())
            }
            Err(e) if is_duplicate_column(&e) => Ok(()),
            Err(e) => {
                tracing::error!(
                    schema_name,
                    error = %e,
                    "migration: failed to add mem_snapshots.extra"
                );
                Err(db_err(e))
            }
        }
    }

    /// Retry optional call-log repairs from the background writer, even when the
    /// router has already cached an otherwise usable user schema. Failed checks
    /// are not cached; successful checks avoid metadata queries on later batches.
    pub async fn ensure_call_log_tool_outcome_schema(&self) -> Result<(), MemoriaError> {
        if self.call_log_tool_outcomes_ready.load(Ordering::Relaxed) {
            return Ok(());
        }
        let schema_name = self.current_schema_name().await?;
        self.ensure_call_log_tool_outcome_columns(&self.pool, schema_name.as_ref())
            .await
    }

    /// Tool outcomes were added after version 2 was introduced, without a version
    /// bump. Repair just these additive columns, without replaying historical
    /// memory/branch migrations or changing the version marker during a rollout.
    async fn ensure_call_log_tool_outcome_columns(
        &self,
        pool: &MySqlPool,
        schema_name: &str,
    ) -> Result<(), MemoriaError> {
        if self.call_log_tool_outcomes_ready.load(Ordering::Relaxed) {
            return Ok(());
        }
        let table = self.t("mem_api_call_log");
        // NULL preserves historical/non-tool outcomes and permits older API
        // instances to continue inserting their original column list.
        for (name, definition) in [
            ("tool_success", "tool_success TINYINT(1) NULL"),
            ("tool_error_kind", "tool_error_kind VARCHAR(32) NULL"),
        ] {
            if info_schema_column_exists(pool, schema_name, "mem_api_call_log", name).await {
                continue;
            }
            let sql = format!("ALTER TABLE {table} ADD COLUMN {definition}");
            if let Err(error) = exec_ddl_with_retry(pool, &sql).await {
                // Another instance can win between the metadata read and ALTER.
                // MatrixOne may also report a stale secondary-index table; accept
                // that race only when the requested column actually exists.
                let repaired_by_other_instance = is_duplicate_column(&error)
                    || (is_mo_concurrent_ddl_race(&error)
                        && info_schema_column_exists(pool, schema_name, "mem_api_call_log", name)
                            .await);
                if !repaired_by_other_instance {
                    tracing::error!(%error, column = name, "Cannot migrate tool outcome call-log columns");
                    return Err(db_err(error));
                }
            }
        }
        self.call_log_tool_outcomes_ready
            .store(true, Ordering::Relaxed);
        Ok(())
    }

    pub async fn migrate_user(&self) -> Result<(), MemoriaError> {
        let pool = &self.pool;
        let meta_table = self.t("mem_schema_meta");
        let schema_name = self.current_schema_name().await?;
        let schema_name = schema_name.as_ref();

        let is_fresh = is_fresh_database(pool, schema_name).await?;
        if is_fresh {
            return self.migrate_user_fresh().await;
        }

        ensure_user_schema_meta_table(pool, &meta_table).await?;
        // Non-fatal: snapshot extra column failure must not block subsequent migrations.
        if let Err(e) = self.ensure_snapshot_extra_column(pool, schema_name).await {
            tracing::warn!("migration: ensure_snapshot_extra_column failed (non-fatal): {e}");
        }
        // These repairs must run regardless of schema version. author_id was
        // previously skipped on Unicode physical branch names even though the
        // migration stored version 2; subject_id was added after version 2.
        self.ensure_author_id_column(pool, schema_name).await?;
        self.ensure_subject_id_column(pool, schema_name).await?;
        // Short-circuit only when the schema version is current AND the main table
        // actually exists. If mem_memories is somehow missing on a non-fresh database
        // (e.g. a partial migration was interrupted), fall through to bootstrap so
        // the table is recreated rather than letting the service start with a broken
        // schema that would only surface as runtime 1146 errors.
        if load_user_schema_version(pool, &meta_table).await? == Some(CURRENT_USER_SCHEMA_VERSION)
            && info_schema_table_exists(pool, schema_name, "mem_memories").await
        {
            // Monitoring failures must not make an existing user's memories
            // unavailable. The background log writer retries this optional repair.
            if let Err(error) = self
                .ensure_call_log_tool_outcome_columns(pool, schema_name)
                .await
            {
                tracing::warn!(%error, schema_name, "Optional call-log schema repair deferred to background writer");
            }
            return Ok(());
        }

        self.bootstrap_user_schema(pool).await?;
        self.apply_user_compat_migrations(pool).await?;
        store_user_schema_version(pool, &meta_table, CURRENT_USER_SCHEMA_VERSION).await?;
        Ok(())
    }

    pub(crate) async fn migrate_user_fresh(&self) -> Result<(), MemoriaError> {
        let pool = &self.pool;
        let meta_table = self.t("mem_schema_meta");
        self.bootstrap_user_schema(pool).await?;
        ensure_user_schema_meta_table(pool, &meta_table).await?;
        store_user_schema_version(pool, &meta_table, CURRENT_USER_SCHEMA_VERSION).await?;
        Ok(())
    }

    pub async fn migrate_shared(&self) -> Result<(), MemoriaError> {
        let schema_name = self.current_schema_name().await?;
        let schema_name = schema_name.as_ref();
        let mut conn = self.conn().await?;
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
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_api_keys (
                key_id       VARCHAR(36)  NOT NULL,
                user_id      VARCHAR(64)  NOT NULL,
                group_id     VARCHAR(64)  DEFAULT NULL,
                name         VARCHAR(100) NOT NULL,
                key_hash     VARCHAR(64)  NOT NULL,
                key_prefix   VARCHAR(12)  NOT NULL,
                scopes       VARCHAR(512) NOT NULL DEFAULT 'identity:read,memory:read,memory:write,keys:manage',
                is_active    TINYINT(1)   NOT NULL DEFAULT 1,
                created_at   DATETIME(6)  NOT NULL,
                expires_at   DATETIME(6)  DEFAULT NULL,
                last_used_at DATETIME(6)  DEFAULT NULL,
                PRIMARY KEY (key_id),
                KEY idx_key_hash (key_hash),
                KEY idx_user_active (user_id, is_active),
                KEY idx_group_active (group_id, is_active)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        let key_group_col_exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = 'mem_api_keys' AND column_name = 'group_id' \
             LIMIT 1",
        )
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_err)?;
        if key_group_col_exists.is_none() {
            let alter = sqlx::query(
                "ALTER TABLE mem_api_keys ADD COLUMN group_id VARCHAR(64) DEFAULT NULL",
            );
            if let Err(e) = alter.execute(&mut *conn).await {
                if !is_duplicate_column(&e) {
                    return Err(db_err(e));
                }
            }
            let alter_idx = sqlx::query(
                "ALTER TABLE mem_api_keys ADD KEY idx_group_active (group_id, is_active)",
            );
            let _ = alter_idx.execute(&mut *conn).await;
        }

        let key_scopes_col_exists: Option<i64> = sqlx::query_scalar(
            "SELECT 1 FROM information_schema.columns \
             WHERE table_schema = DATABASE() AND table_name = 'mem_api_keys' AND column_name = 'scopes' \
             LIMIT 1",
        )
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_err)?;
        if key_scopes_col_exists.is_none() {
            let alter = sqlx::query(
                "ALTER TABLE mem_api_keys ADD COLUMN scopes VARCHAR(512) NOT NULL \
                 DEFAULT 'identity:read,memory:read,memory:write,keys:manage' AFTER key_prefix",
            );
            if let Err(e) = alter.execute(&mut *conn).await {
                if !is_duplicate_column(&e) {
                    return Err(db_err(e));
                }
            }
        }

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_groups (
                group_id      VARCHAR(64)  PRIMARY KEY,
                group_name    VARCHAR(128) NOT NULL,
                db_name       VARCHAR(128) NOT NULL UNIQUE,
                owner_user_id VARCHAR(64)  NOT NULL,
                status        VARCHAR(20)  NOT NULL DEFAULT 'active',
                created_at    DATETIME(6)  NOT NULL,
                updated_at    DATETIME(6)  NOT NULL,
                KEY idx_group_status (status)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_governance_runtime_state (
                strategy_key       VARCHAR(128) NOT NULL,
                `task`             VARCHAR(32)  NOT NULL,
                failure_count      INT          NOT NULL DEFAULT 0,
                circuit_open_until DATETIME(6)  DEFAULT NULL,
                updated_at         DATETIME(6)  NOT NULL,
                PRIMARY KEY (strategy_key, `task`)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_signers (
                signer       VARCHAR(128) PRIMARY KEY,
                algorithm    VARCHAR(32)  NOT NULL,
                public_key   TEXT         NOT NULL,
                is_active    TINYINT(1)   NOT NULL DEFAULT 1,
                created_at   DATETIME(6)  NOT NULL,
                updated_at   DATETIME(6)  NOT NULL,
                created_by   VARCHAR(64)  NOT NULL
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_packages (
                plugin_key      VARCHAR(128) NOT NULL,
                version         VARCHAR(32)  NOT NULL,
                domain          VARCHAR(32)  NOT NULL,
                name            VARCHAR(128) NOT NULL,
                runtime         VARCHAR(32)  NOT NULL,
                manifest_json   TEXT         NOT NULL,
                package_payload LONGTEXT     NOT NULL,
                sha256          VARCHAR(128) NOT NULL,
                signature       TEXT         NOT NULL,
                signer          VARCHAR(128) NOT NULL,
                status          VARCHAR(16)  NOT NULL DEFAULT 'active',
                published_at    DATETIME(6)  NOT NULL,
                published_by    VARCHAR(64)  NOT NULL,
                PRIMARY KEY (plugin_key, version),
                INDEX idx_plugin_domain_status (domain, status),
                INDEX idx_plugin_signer (signer)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_bindings (
                domain      VARCHAR(32)  NOT NULL,
                binding_key VARCHAR(64)  NOT NULL,
                plugin_key  VARCHAR(128) NOT NULL,
                version     VARCHAR(32)  NOT NULL,
                updated_at  DATETIME(6)  NOT NULL,
                updated_by  VARCHAR(64)  NOT NULL,
                PRIMARY KEY (domain, binding_key)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_reviews (
                plugin_key    VARCHAR(128) NOT NULL,
                version       VARCHAR(32)  NOT NULL,
                review_status VARCHAR(16)  NOT NULL DEFAULT 'pending',
                score         DOUBLE       NOT NULL DEFAULT 0,
                review_notes  TEXT         NOT NULL,
                reviewed_at   DATETIME(6)  NOT NULL,
                reviewed_by   VARCHAR(64)  NOT NULL,
                PRIMARY KEY (plugin_key, version)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_binding_rules (
                rule_id             VARCHAR(64)  PRIMARY KEY,
                domain              VARCHAR(32)  NOT NULL,
                binding_key         VARCHAR(64)  NOT NULL,
                subject_key         VARCHAR(128) NOT NULL,
                priority            BIGINT       NOT NULL DEFAULT 100,
                plugin_key          VARCHAR(128) NOT NULL,
                selector_kind       VARCHAR(16)  NOT NULL,
                selector_value      VARCHAR(64)  NOT NULL,
                rollout_percent     BIGINT       NOT NULL DEFAULT 100,
                transport_endpoint  TEXT         NOT NULL,
                status              VARCHAR(16)  NOT NULL DEFAULT 'active',
                updated_at          DATETIME(6)  NOT NULL,
                updated_by          VARCHAR(64)  NOT NULL,
                UNIQUE KEY uniq_binding_rule (domain, binding_key, subject_key, priority)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_plugin_audit_events (
                event_id      VARCHAR(64)  PRIMARY KEY,
                domain        VARCHAR(32)  NOT NULL,
                binding_key   VARCHAR(64)  NOT NULL,
                subject_key   VARCHAR(128) NOT NULL,
                plugin_key    VARCHAR(128) NOT NULL,
                version       VARCHAR(32)  NOT NULL,
                event_type    VARCHAR(32)  NOT NULL,
                status        VARCHAR(16)  NOT NULL,
                message       TEXT         NOT NULL,
                metadata_json JSON         NOT NULL,
                created_at    DATETIME(6)  NOT NULL,
                actor         VARCHAR(64)  NOT NULL,
                INDEX idx_plugin_audit_lookup (domain, binding_key, plugin_key, created_at)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_distributed_locks (
                lock_key    VARCHAR(128) PRIMARY KEY,
                holder_id   VARCHAR(128) NOT NULL,
                acquired_at DATETIME(6)  NOT NULL,
                expires_at  DATETIME(6)  NOT NULL,
                INDEX idx_lock_expires (expires_at)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_async_tasks (
                task_id     VARCHAR(64)  PRIMARY KEY,
                instance_id VARCHAR(128) NOT NULL,
                user_id     VARCHAR(64)  NOT NULL DEFAULT '',
                status      VARCHAR(16)  NOT NULL DEFAULT 'processing',
                result_json JSON         DEFAULT NULL,
                error_json  JSON         DEFAULT NULL,
                created_at  DATETIME(6)  NOT NULL,
                updated_at  DATETIME(6)  NOT NULL,
                INDEX idx_task_status (status, created_at)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        let has_async_task_user_id =
            info_schema_column_exists(&self.pool, schema_name, "mem_async_tasks", "user_id").await;
        if !has_async_task_user_id {
            let add_async_task_user_id = sqlx::query(
                "ALTER TABLE mem_async_tasks ADD COLUMN user_id VARCHAR(64) NOT NULL DEFAULT '' AFTER instance_id",
            )
            .execute(&mut *conn)
            .await;
            if let Err(e) = add_async_task_user_id {
                if !is_duplicate_column(&e) {
                    tracing::warn!(
                        error = %e,
                        "shared migration: failed to add mem_async_tasks.user_id compatibility column"
                    );
                }
            }
        }

        // ── Ops-metrics aggregate tables (push-based stats written by Memoria) ──
        // Created here so they exist in both single-db and multi-db deployments.
        // DbRouter::ensure_user_registry_table also creates these for multi-db
        // startup — CREATE TABLE IF NOT EXISTS makes all paths idempotent.
        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS srv_user_stats (
                user_id          VARCHAR(64)  NOT NULL PRIMARY KEY,
                total_memories   BIGINT       NOT NULL DEFAULT 0,
                active_memories  BIGINT       NOT NULL DEFAULT 0,
                inactive_memories BIGINT      NOT NULL DEFAULT 0,
                total_entities   BIGINT       NOT NULL DEFAULT 0,
                total_edits      BIGINT       NOT NULL DEFAULT 0,
                updated_at       DATETIME     NOT NULL DEFAULT CURRENT_TIMESTAMP
                                 ON UPDATE CURRENT_TIMESTAMP
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS srv_user_metric_detail (
                user_id  VARCHAR(64)  NOT NULL,
                metric   VARCHAR(64)  NOT NULL,
                dim_key  VARCHAR(128) NOT NULL,
                cnt      BIGINT       NOT NULL DEFAULT 0,
                PRIMARY KEY (user_id, metric, dim_key)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS srv_daily_stats (
                dt      DATE         NOT NULL,
                metric  VARCHAR(64)  NOT NULL,
                cnt     BIGINT       NOT NULL DEFAULT 0,
                PRIMARY KEY (dt, metric)
            )"#,
        )
        .execute(&mut *conn)
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
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS srv_mcp_path_stats (
                path  VARCHAR(128) NOT NULL PRIMARY KEY,
                cnt   BIGINT       NOT NULL DEFAULT 0
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS mem_group_members (
                group_id     VARCHAR(64)  NOT NULL,
                user_id      VARCHAR(64)  NOT NULL,
                display_name VARCHAR(128) DEFAULT NULL,
                role         VARCHAR(20)  NOT NULL DEFAULT 'member',
                is_active    TINYINT(1)   NOT NULL DEFAULT 1,
                joined_at    DATETIME(6)  NOT NULL,
                removed_at   DATETIME(6)  DEFAULT NULL,
                PRIMARY KEY (group_id, user_id),
                INDEX idx_user_active (user_id, is_active)
            )"#,
        )
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        Ok(())
    }

    // ── Audit log ─────────────────────────────────────────────────────────────

    /// Create a safety snapshot before destructive operations. Best-effort.
    /// If creation fails (e.g. quota exhausted), tries to drop the 10 oldest
    /// `pre_` safety snapshots and retries once.
    /// Returns `(snapshot_name_or_none, warning_message_or_none)`.
    pub async fn create_safety_snapshot(
        &self,
        operation: &str,
    ) -> (Option<String>, Option<String>) {
        let snapshot_prefix = safety_snapshot_prefix(self.database_name());
        let legacy_prefix = legacy_safety_snapshot_prefix(self.database_name());
        let name = build_safety_snapshot_name(self.database_name(), operation);
        debug_assert!(name.len() <= MAX_IDENTIFIER_LEN);
        let sql = match self.database_name() {
            Some(db_name) => format!(
                "CREATE SNAPSHOT {name} FOR DATABASE {}",
                quote_ident(db_name)
            ),
            None => format!("CREATE SNAPSHOT {name} FOR ACCOUNT"),
        };

        // First attempt
        if sqlx::raw_sql(&sql).execute(&self.pool).await.is_ok() {
            return (Some(name), None);
        }

        // Failed — try to reclaim space by dropping oldest pre_ snapshots
        let mut dropped = self
            .cleanup_oldest_safety_snapshots(&snapshot_prefix, 10)
            .await;
        if let Some(legacy_prefix) = legacy_prefix.as_deref() {
            if legacy_prefix != snapshot_prefix && dropped < 10 {
                dropped += self
                    .cleanup_oldest_safety_snapshots(legacy_prefix, 10 - dropped)
                    .await;
            }
        }
        if dropped > 0 {
            // Retry
            if sqlx::raw_sql(&sql).execute(&self.pool).await.is_ok() {
                return (Some(name), Some(format!(
                    "⚠️ Snapshot quota was full. Auto-deleted {dropped} oldest safety snapshots to make room. \
                     Consider running memory_snapshot_delete(prefix=\"pre_\") to free more space."
                )));
            }
        }

        // Still failed
        (None, Some(
            "⚠️ Safety snapshot could not be created (snapshot quota exhausted). \
             Purge proceeded without rollback protection. \
             Run memory_snapshot_delete(prefix=\"pre_\") or memory_snapshot_delete(older_than=\"...\") to free quota."
            .to_string()
        ))
    }

    /// Drop the N oldest `mem_snap_pre_` snapshots. Returns count dropped.
    async fn cleanup_oldest_safety_snapshots(&self, prefix: &str, n: usize) -> usize {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT sname FROM mo_catalog.mo_snapshots \
             WHERE prefix_eq(sname, ?) ORDER BY ts ASC",
        )
        .bind(prefix)
        .fetch_all(&self.pool)
        .await
        .unwrap_or_default();

        let mut dropped = 0;
        for (name,) in rows.iter().take(n) {
            if sqlx::raw_sql(&format!("DROP SNAPSHOT {name}"))
                .execute(&self.pool)
                .await
                .is_ok()
            {
                dropped += 1;
            }
        }
        dropped
    }

    /// Write an audit record to mem_edit_log. Best-effort — never fails the caller.
    /// For batch operations, call once per memory_id.
    pub async fn log_edit(
        &self,
        user_id: &str,
        operation: &str,
        memory_id: Option<&str>,
        payload: Option<&str>,
        reason: &str,
        snapshot_before: Option<&str>,
    ) {
        if let Some(tx) = self.edit_log_tx.read().unwrap().clone() {
            let entry = OwnedEditLogEntry {
                edit_id: uuid7_id(),
                user_id: user_id.to_string(),
                operation: operation.to_string(),
                memory_id: memory_id.map(String::from),
                payload: payload.map(String::from),
                reason: reason.to_string(),
                snapshot_before: snapshot_before.map(String::from),
            };
            match tx.try_send(entry) {
                Ok(()) => return,
                Err(tokio::sync::mpsc::error::TrySendError::Full(entry)) => {
                    tracing::warn!(
                        user_id = %entry.user_id,
                        operation = %entry.operation,
                        memory_id = ?entry.memory_id,
                        "edit log async buffer full, dropping entry"
                    );
                    return;
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                    // Channel closed (drain in progress) — fall through to direct INSERT
                }
            }
        }
        if let Some(router) = self.db_router.clone() {
            match router.scope_store(user_id).await {
                Ok(store) => {
                    let _ = store
                        .insert_edit_log_direct(
                            user_id,
                            operation,
                            memory_id,
                            payload,
                            reason,
                            snapshot_before,
                        )
                        .await;
                }
                Err(e) => {
                    tracing::warn!(
                        user_id,
                        operation,
                        error = %e,
                        "failed to route edit log entry to user store"
                    );
                }
            }
            return;
        }
        let _ = self
            .insert_edit_log_direct(
                user_id,
                operation,
                memory_id,
                payload,
                reason,
                snapshot_before,
            )
            .await;
    }

    async fn insert_edit_log_direct(
        &self,
        user_id: &str,
        operation: &str,
        memory_id: Option<&str>,
        payload: Option<&str>,
        reason: &str,
        snapshot_before: Option<&str>,
    ) -> Result<(), MemoriaError> {
        let edit_log_table = self.t("mem_edit_log");
        let edit_id = uuid7_id();
        // MO workaround (revert when fixed): MatrixOne prepared-statement bind of
        // Option<String>::None to nullable columns inherits the previous row's value
        // instead of SQL NULL in multi-row INSERTs. Use SQL NULL literal instead.
        // TODO: revert to plain `(?, ?, ?, ?, ?, ?, ?, ?)` + direct bind once MO fixes this.
        let mid_ph = if memory_id.is_some() { "?" } else { "NULL" };
        let pay_ph = if payload.is_some() { "?" } else { "NULL" };
        let snap_ph = if snapshot_before.is_some() {
            "?"
        } else {
            "NULL"
        };
        let sql = format!(
            "INSERT INTO {edit_log_table} (edit_id, user_id, memory_id, operation, payload, reason, snapshot_before, created_by) \
             VALUES (?, ?, {mid_ph}, ?, {pay_ph}, ?, {snap_ph}, ?)"
        );
        let mut q = sqlx::query(&sql).bind(&edit_id).bind(user_id);
        if let Some(v) = memory_id {
            q = q.bind(v);
        }
        q = q.bind(operation);
        if let Some(v) = payload {
            q = q.bind(v);
        }
        q = q.bind(reason);
        if let Some(v) = snapshot_before {
            q = q.bind(v);
        }
        q.bind(user_id).execute(&self.pool).await.map_err(db_err)?;
        Ok(())
    }

    /// Batch-insert edit log entries in a single multi-row INSERT.
    pub async fn flush_edit_log_batch(
        &self,
        entries: &[OwnedEditLogEntry],
    ) -> Result<(), MemoriaError> {
        if entries.is_empty() {
            return Ok(());
        }
        if let Some(router) = self.db_router.clone() {
            let mut by_user: std::collections::HashMap<&str, Vec<OwnedEditLogEntry>> =
                std::collections::HashMap::new();
            for entry in entries {
                by_user
                    .entry(entry.user_id.as_str())
                    .or_default()
                    .push(entry.clone());
            }
            let mut first_err = None;
            let mut flushed_any = false;
            for (user_id, user_entries) in by_user {
                match router.scope_store(user_id).await {
                    Ok(store) => match store.flush_edit_log_batch_direct(&user_entries).await {
                        Ok(()) => flushed_any = true,
                        Err(err) => {
                            tracing::warn!(
                                user_id,
                                error = %err,
                                "failed to flush edit log batch for routed user"
                            );
                            if first_err.is_none() {
                                first_err = Some(err);
                            }
                        }
                    },
                    Err(err) => {
                        tracing::warn!(
                            user_id,
                            error = %err,
                            "failed to resolve routed user store for edit log flush"
                        );
                        if first_err.is_none() {
                            first_err = Some(err);
                        }
                    }
                }
            }
            if let Some(err) = first_err {
                if flushed_any {
                    tracing::warn!(
                        error = %err,
                        "edit log batch flushed partially; some routed user entries were dropped"
                    );
                    return Ok(());
                }
                return Err(err);
            }
            return Ok(());
        }
        self.flush_edit_log_batch_direct(entries).await
    }

    async fn flush_edit_log_batch_direct(
        &self,
        entries: &[OwnedEditLogEntry],
    ) -> Result<(), MemoriaError> {
        let edit_log_table = self.t("mem_edit_log");
        for chunk in entries.chunks(100) {
            // MO workaround (revert when fixed): MatrixOne prepared-statement bind of
            // Option<String>::None to nullable columns inherits the previous row's value
            // instead of SQL NULL in multi-row INSERTs. Use SQL NULL literal instead.
            // TODO: revert to plain `(?, ?, ?, ?, ?, ?, ?, ?)` + direct bind once MO fixes this.
            let placeholders: Vec<String> = chunk
                .iter()
                .map(|e| {
                    let mid = if e.memory_id.is_some() { "?" } else { "NULL" };
                    let pay = if e.payload.is_some() { "?" } else { "NULL" };
                    let snap = if e.snapshot_before.is_some() {
                        "?"
                    } else {
                        "NULL"
                    };
                    format!("(?, ?, {mid}, ?, {pay}, ?, {snap}, ?)")
                })
                .collect();
            let sql = format!(
                "INSERT INTO {edit_log_table} (edit_id, user_id, memory_id, operation, payload, reason, snapshot_before, created_by) VALUES {}",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for e in chunk {
                q = q.bind(&e.edit_id).bind(&e.user_id);
                if let Some(v) = &e.memory_id {
                    q = q.bind(v);
                }
                q = q.bind(&e.operation);
                if let Some(v) = &e.payload {
                    q = q.bind(v);
                }
                q = q.bind(&e.reason);
                if let Some(v) = &e.snapshot_before {
                    q = q.bind(v);
                }
                q = q.bind(&e.user_id);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(())
    }

    // ── Branch state ──────────────────────────────────────────────────────────

    /// Resolve the user ID for per-user state (active branch).
    /// In group mode `ACTOR_USER_ID` carries the real human user;
    /// in personal mode the task-local is unset and we fall back to `scope_id`.
    fn state_user<'a>(&self, scope_id: &'a str) -> Cow<'a, str> {
        ACTOR_USER_ID
            .try_with(|id| Cow::Owned(id.clone()))
            .unwrap_or(Cow::Borrowed(scope_id))
    }

    pub async fn active_branch_name(&self, user_id: &str) -> Result<String, MemoriaError> {
        let state_user = self.state_user(user_id);
        let user_state_table = self.t("mem_user_state");
        let row = sqlx::query(&format!(
            "SELECT active_branch FROM {user_state_table} WHERE user_id = ?"
        ))
        .bind(state_user.as_ref())
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(row
            .and_then(|r| r.try_get::<String, _>("active_branch").ok())
            .filter(|branch| !branch.trim().is_empty())
            .unwrap_or_else(|| "main".to_string()))
    }

    /// Returns the active table name for a user: "mem_memories" or branch table name.
    /// Uses `ACTOR_USER_ID` (task-local) for the per-user active-branch state,
    /// and `user_id` (the scope) for looking up the branch's table in `mem_branches`.
    pub async fn active_table(&self, user_id: &str) -> Result<String, MemoriaError> {
        let cache_key = self.state_user(user_id);
        if let Some(cached) = self.active_table_cache.get(cache_key.as_ref()) {
            return Ok(cached);
        }

        let branch = self.active_branch_name(user_id).await?;

        if branch == "main" {
            let table = self.t("mem_memories");
            self.active_table_cache
                .insert(cache_key.into_owned(), table.clone());
            return Ok(table);
        }

        // Branch metadata is owned by the scope (group_id or user_id), not the actor.
        let branches_table = self.t("mem_branches");
        let branch_row = sqlx::query(&format!(
            "SELECT table_name FROM {branches_table} WHERE user_id = ? AND name = ? AND status = 'active'"
        ))
        .bind(user_id)
        .bind(&branch)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        match branch_row {
            Some(r) => {
                let raw = r.try_get::<String, _>("table_name").map_err(db_err)?;
                let table = self.t(&raw);
                // Branch metadata found in mem_branches — trust it.
                // information_schema.tables is NOT reliable for MatrixOne zero-copy branch
                // tables created via `data branch create table`, so we skip the existence probe
                // here. If the physical table is genuinely gone, the subsequent DML query will
                // return a DB error that propagates to the caller.
                self.active_table_cache
                    .insert(cache_key.into_owned(), table.clone());
                Ok(table)
            }
            None => {
                self.set_active_branch(user_id, "main").await?;
                let table = self.t("mem_memories");
                self.active_table_cache
                    .insert(cache_key.into_owned(), table.clone());
                Ok(table)
            }
        }
    }

    /// Resolve a memory table for an optional explicit branch.
    ///
    /// `None` preserves checkout-based behavior via `active_table`.  A concrete
    /// branch name bypasses `mem_user_state`, so concurrent agents can read/write
    /// different branches without racing on the active-branch pointer.
    pub async fn table_for_branch(
        &self,
        user_id: &str,
        branch: Option<&str>,
    ) -> Result<String, MemoriaError> {
        let Some(branch) = branch.map(str::trim).filter(|branch| !branch.is_empty()) else {
            return self.active_table(user_id).await;
        };

        if branch == "main" {
            return Ok(self.t("mem_memories"));
        }

        let branches_table = self.t("mem_branches");
        let branch_row = sqlx::query(&format!(
            "SELECT table_name FROM {branches_table} WHERE user_id = ? AND name = ? AND status = 'active'"
        ))
        .bind(user_id)
        .bind(branch)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        match branch_row {
            Some(r) => {
                let raw = r.try_get::<String, _>("table_name").map_err(db_err)?;
                Ok(self.t(&raw))
            }
            None => Err(MemoriaError::NotFound(format!("Branch '{branch}'"))),
        }
    }

    pub async fn set_active_branch(&self, user_id: &str, branch: &str) -> Result<(), MemoriaError> {
        let state_user = self.state_user(user_id);
        let user_state_table = self.t("mem_user_state");
        let now = Utc::now().naive_utc();
        sqlx::query(&format!(
            r#"INSERT INTO {user_state_table} (user_id, active_branch, updated_at)
               VALUES (?, ?, ?)
               ON DUPLICATE KEY UPDATE active_branch = ?, updated_at = ?"#,
        ))
        .bind(state_user.as_ref())
        .bind(branch)
        .bind(now)
        .bind(branch)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        self.active_table_cache.invalidate(state_user.as_ref());
        Ok(())
    }

    pub async fn invalidate_user_caches(&self, user_id: &str) {
        // Invalidate both the scope key and (if different) the actor key
        self.active_table_cache.invalidate(user_id);
        let state_user = self.state_user(user_id);
        if state_user.as_ref() != user_id {
            self.active_table_cache.invalidate(state_user.as_ref());
        }

        // Keep rollback reconciliation scoped to the known per-user governance cooldowns.
        for operation in [
            "governance",
            "consolidate",
            "reflect",
            "orphan_graph_cleanup",
        ] {
            let key = format!("{user_id}:{operation}");
            self.cooldown_cache.invalidate(&key);
        }
    }

    pub async fn register_branch(
        &self,
        user_id: &str,
        name: &str,
        table_name: &str,
    ) -> Result<(), MemoriaError> {
        let branches_table = self.t("mem_branches");
        let now = Utc::now().naive_utc();
        let id = uuid::Uuid::new_v4().simple().to_string();
        sqlx::query(&format!(
            r#"INSERT INTO {branches_table} (id, user_id, name, table_name, status, created_at)
               VALUES (?, ?, ?, ?, 'active', ?)"#
        ))
        .bind(id)
        .bind(user_id)
        .bind(name)
        .bind(table_name)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn deregister_branch(&self, user_id: &str, name: &str) -> Result<(), MemoriaError> {
        let was_active = self.active_branch_name(user_id).await? == name;
        let branches_table = self.t("mem_branches");
        sqlx::query(&format!(
            "UPDATE {branches_table} SET status = 'deleted' WHERE user_id = ? AND name = ?"
        ))
        .bind(user_id)
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if was_active {
            self.set_active_branch(user_id, "main").await?;
        } else {
            self.active_table_cache.invalidate(user_id);
        }
        Ok(())
    }

    pub async fn list_branches(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, String, Option<chrono::NaiveDateTime>)>, MemoriaError> {
        let branches_table = self.t("mem_branches");
        // ORDER BY: NULL created_at (legacy rows) sort last via IS NULL trick;
        // tie-break by name for stable output when timestamps collide.
        let rows = sqlx::query(&format!(
            "SELECT name, table_name, created_at FROM {branches_table} \
             WHERE user_id = ? AND status = 'active' \
             ORDER BY created_at IS NULL ASC, created_at ASC, name ASC"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get::<String, _>("name").map_err(db_err)?,
                    r.try_get::<String, _>("table_name").map_err(db_err)?,
                    // Lenient: a corrupt/NULL timestamp yields None rather than failing the
                    // entire branch list. name and table_name are strict because they drive
                    // branch operations.
                    r.try_get::<Option<chrono::NaiveDateTime>, _>("created_at")
                        .ok()
                        .flatten(),
                ))
            })
            .collect()
    }

    pub async fn register_snapshot(
        &self,
        user_id: &str,
        name: &str,
        snapshot_name: &str,
        extra: Option<&serde_json::Value>,
    ) -> Result<(), MemoriaError> {
        let snapshots_table = self.t("mem_snapshots");
        let now = Utc::now().naive_utc();
        let id = uuid::Uuid::new_v4().simple().to_string();
        let extra = extra.map(serde_json::to_string).transpose()?;
        sqlx::query(&format!(
            r#"INSERT INTO {snapshots_table} (id, user_id, name, snapshot_name, extra, status, created_at)
               VALUES (?, ?, ?, ?, ?, 'active', ?)"#
        ))
        .bind(id)
        .bind(user_id)
        .bind(name)
        .bind(snapshot_name)
        .bind(extra)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn get_snapshot_registration(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Option<SnapshotRegistration>, MemoriaError> {
        let snapshots_table = self.t("mem_snapshots");
        let row = sqlx::query(&format!(
            "SELECT name, snapshot_name, extra, created_at \
             FROM {snapshots_table} \
             WHERE user_id = ? AND name = ? AND status = 'active' \
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(user_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        row.map(|r| {
            Ok(SnapshotRegistration {
                name: r.try_get("name").map_err(db_err)?,
                snapshot_name: r.try_get("snapshot_name").map_err(db_err)?,
                extra: snapshot_extra_from_row(&r)?,
                created_at: r.try_get("created_at").map_err(db_err)?,
            })
        })
        .transpose()
    }

    pub async fn get_snapshot_registration_by_internal(
        &self,
        user_id: &str,
        snapshot_name: &str,
    ) -> Result<Option<SnapshotRegistration>, MemoriaError> {
        let snapshots_table = self.t("mem_snapshots");
        let row = sqlx::query(&format!(
            "SELECT name, snapshot_name, extra, created_at \
             FROM {snapshots_table} \
             WHERE user_id = ? AND snapshot_name = ? AND status = 'active' \
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(user_id)
        .bind(snapshot_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        row.map(|r| {
            Ok(SnapshotRegistration {
                name: r.try_get("name").map_err(db_err)?,
                snapshot_name: r.try_get("snapshot_name").map_err(db_err)?,
                extra: snapshot_extra_from_row(&r)?,
                created_at: r.try_get("created_at").map_err(db_err)?,
            })
        })
        .transpose()
    }

    pub async fn list_snapshot_registrations(
        &self,
        user_id: &str,
    ) -> Result<Vec<SnapshotRegistration>, MemoriaError> {
        let snapshots_table = self.t("mem_snapshots");
        let rows = sqlx::query(&format!(
            "SELECT name, snapshot_name, extra, created_at \
             FROM {snapshots_table} \
             WHERE user_id = ? AND status = 'active' \
             ORDER BY created_at DESC"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        rows.iter()
            .map(|r| {
                Ok(SnapshotRegistration {
                    name: r.try_get("name").map_err(db_err)?,
                    snapshot_name: r.try_get("snapshot_name").map_err(db_err)?,
                    extra: snapshot_extra_from_row(r)?,
                    created_at: r.try_get("created_at").map_err(db_err)?,
                })
            })
            .collect()
    }

    pub async fn update_snapshot_memory_count(
        &self,
        user_id: &str,
        name: &str,
        snapshot_name: &str,
        memory_count: i64,
    ) -> Result<(), MemoriaError> {
        let snapshots_table = self.t("mem_snapshots");
        let row = sqlx::query(&format!(
            "SELECT extra FROM {snapshots_table} \
             WHERE user_id = ? AND name = ? AND snapshot_name = ? AND status = 'active' \
             ORDER BY created_at DESC LIMIT 1"
        ))
        .bind(user_id)
        .bind(name)
        .bind(snapshot_name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        let Some(row) = row else {
            return Err(MemoriaError::Internal(format!(
                "active snapshot registration not found for user={user_id:?}, name={name:?}, snapshot={snapshot_name:?}"
            )));
        };

        let mut extra = snapshot_extra_from_row(&row)?
            .unwrap_or_else(|| serde_json::Value::Object(Default::default()));
        let Some(obj) = extra.as_object_mut() else {
            return Err(MemoriaError::Internal(format!(
                "snapshot extra must be a JSON object for user={user_id:?}, name={name:?}, snapshot={snapshot_name:?}"
            )));
        };
        obj.insert("memory_count".to_string(), serde_json::json!(memory_count));
        let extra = serde_json::to_string(&extra)?;

        let result = sqlx::query(&format!(
            "UPDATE {snapshots_table} SET extra = ? \
             WHERE user_id = ? AND name = ? AND snapshot_name = ? AND status = 'active'"
        ))
        .bind(extra)
        .bind(user_id)
        .bind(name)
        .bind(snapshot_name)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        if result.rows_affected() == 0 {
            return Err(MemoriaError::Internal(format!(
                "failed to update snapshot memory_count for user={user_id:?}, name={name:?}, snapshot={snapshot_name:?}"
            )));
        }
        Ok(())
    }

    pub async fn deregister_snapshot(&self, user_id: &str, name: &str) -> Result<(), MemoriaError> {
        let snapshots_table = self.t("mem_snapshots");
        sqlx::query(&format!(
            "UPDATE {snapshots_table} SET status = 'deleted' WHERE user_id = ? AND name = ? AND status = 'active'"
        ))
        .bind(user_id)
        .bind(name)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn deregister_snapshot_by_internal(
        &self,
        user_id: &str,
        snapshot_name: &str,
    ) -> Result<(), MemoriaError> {
        let snapshots_table = self.t("mem_snapshots");
        sqlx::query(&format!(
            "UPDATE {snapshots_table} SET status = 'deleted' \
             WHERE user_id = ? AND snapshot_name = ? AND status = 'active'"
        ))
        .bind(user_id)
        .bind(snapshot_name)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    // ── Governance ────────────────────────────────────────────────────────────

    /// Check cooldown. Returns Some(remaining_seconds) if still in cooldown, None if can run.
    /// Uses in-memory cache as fast path; falls back to DB for cross-instance consistency.
    pub async fn check_cooldown(
        &self,
        user_id: &str,
        operation: &str,
        cooldown_secs: i64,
    ) -> Result<Option<i64>, MemoriaError> {
        let mut conn = self.conn().await?;
        let cooldown_table = self.t("mem_governance_cooldown");
        let key = format!("{}:{}", user_id, operation);
        if let Some(last_run) = self.cooldown_cache.get(&key) {
            let elapsed = last_run.elapsed().as_secs() as i64;
            if elapsed < cooldown_secs {
                return Ok(Some(cooldown_secs - elapsed));
            }
            // Expired in memory — can run
            return Ok(None);
        }
        // Cache miss — check DB (cold start or cross-instance)
        let row = sqlx::query(&format!(
            "SELECT TIMESTAMPDIFF(SECOND, last_run_at, NOW()) as elapsed \
             FROM {cooldown_table} WHERE user_id = ? AND operation = ?"
        ))
        .bind(user_id)
        .bind(operation)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_err)?;
        match row {
            None => Ok(None),
            Some(r) => {
                let elapsed: i64 = r.try_get("elapsed").unwrap_or(cooldown_secs + 1);
                if elapsed >= cooldown_secs {
                    Ok(None)
                } else {
                    // Backfill cache from DB
                    let age = std::time::Duration::from_secs(elapsed as u64);
                    let approx_start = std::time::Instant::now() - age;
                    self.cooldown_cache.insert(key, approx_start);
                    Ok(Some(cooldown_secs - elapsed))
                }
            }
        }
    }

    pub async fn set_cooldown(&self, user_id: &str, operation: &str) -> Result<(), MemoriaError> {
        let mut conn = self.conn().await?;
        let cooldown_table = self.t("mem_governance_cooldown");
        let now = Utc::now().naive_utc();
        sqlx::query(&format!(
            "INSERT INTO {cooldown_table} (user_id, operation, last_run_at) \
             VALUES (?, ?, ?) ON DUPLICATE KEY UPDATE last_run_at = ?"
        ))
        .bind(user_id)
        .bind(operation)
        .bind(now)
        .bind(now)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
        let key = format!("{}:{}", user_id, operation);
        self.cooldown_cache.insert(key, std::time::Instant::now());
        Ok(())
    }

    pub async fn check_governance_runtime_breaker(
        &self,
        strategy_key: &str,
        task: &str,
    ) -> Result<Option<i64>, MemoriaError> {
        let mut conn = self.conn().await?;
        let runtime_table = self.t("mem_governance_runtime_state");
        let row = sqlx::query(&format!(
            "SELECT TIMESTAMPDIFF(SECOND, NOW(), circuit_open_until) AS remaining \
             FROM {runtime_table} \
             WHERE strategy_key = ? AND `task` = ? \
                AND circuit_open_until IS NOT NULL AND circuit_open_until > NOW()"
        ))
        .bind(strategy_key)
        .bind(task)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_err)?;

        Ok(row.and_then(|r| r.try_get::<i64, _>("remaining").ok()))
    }

    pub async fn record_governance_runtime_failure(
        &self,
        strategy_key: &str,
        task: &str,
        threshold: usize,
        cooldown_secs: i64,
    ) -> Result<Option<i64>, MemoriaError> {
        let mut conn = self.conn().await?;
        let runtime_table = self.t("mem_governance_runtime_state");
        let open_on_insert = threshold <= 1;
        let initial_failures = if open_on_insert { 0 } else { 1 };
        sqlx::query(&format!(
            "INSERT INTO {runtime_table} \
                 (strategy_key, `task`, failure_count, circuit_open_until, updated_at) \
              VALUES (?, ?, ?, CASE WHEN ? THEN DATE_ADD(NOW(), INTERVAL ? SECOND) ELSE NULL END, NOW()) \
              ON DUPLICATE KEY UPDATE \
                 failure_count = CASE \
                     WHEN circuit_open_until IS NOT NULL AND circuit_open_until > NOW() THEN failure_count \
                     WHEN failure_count + 1 >= ? THEN 0 \
                     ELSE failure_count + 1 \
                 END, \
                 circuit_open_until = CASE \
                     WHEN circuit_open_until IS NOT NULL AND circuit_open_until > NOW() THEN circuit_open_until \
                     WHEN failure_count + 1 >= ? THEN DATE_ADD(NOW(), INTERVAL ? SECOND) \
                     ELSE NULL \
                  END, \
                  updated_at = NOW()"
        ))
        .bind(strategy_key)
        .bind(task)
        .bind(initial_failures)
        .bind(open_on_insert)
        .bind(cooldown_secs)
        .bind(threshold as i64)
        .bind(threshold as i64)
        .bind(cooldown_secs)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        self.check_governance_runtime_breaker(strategy_key, task)
            .await
    }

    pub async fn clear_governance_runtime_breaker(
        &self,
        strategy_key: &str,
        task: &str,
    ) -> Result<(), MemoriaError> {
        let mut conn = self.conn().await?;
        let runtime_table = self.t("mem_governance_runtime_state");
        sqlx::query(&format!(
            "INSERT INTO {runtime_table} \
                 (strategy_key, `task`, failure_count, circuit_open_until, updated_at) \
              VALUES (?, ?, 0, NULL, NOW()) \
              ON DUPLICATE KEY UPDATE failure_count = 0, circuit_open_until = NULL, updated_at = NOW()"
        ))
        .bind(strategy_key)
        .bind(task)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Quarantine memories whose effective confidence has decayed below threshold.
    /// effective_confidence = initial_confidence * EXP(-age_days / half_life)
    pub async fn quarantine_low_confidence(&self, user_id: &str) -> Result<i64, MemoriaError> {
        let memories_table = self.t("mem_memories");
        const THRESHOLD: f64 = 0.2;
        const BATCH: i64 = 500;
        let tiers: &[(&str, f64)] = &[("T1", 365.0), ("T2", 180.0), ("T3", 60.0), ("T4", 30.0)];
        let mut total = 0i64;
        for (tier, hl) in tiers {
            loop {
                let res = sqlx::query(&format!(
                    "DELETE FROM {memories_table} \
                     WHERE user_id = ? AND is_active = 1 AND trust_tier = ? \
                        AND (initial_confidence * EXP(-TIMESTAMPDIFF(DAY, observed_at, NOW()) / {hl})) < {THRESHOLD} \
                      LIMIT {BATCH}"
                ))
                .bind(user_id)
                .bind(tier)
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
                let n = res.rows_affected() as i64;
                total += n;
                if n < BATCH {
                    break;
                }
            }
        }
        Ok(total)
    }

    /// Delete inactive memories that are not part of a version chain.
    /// Rows with superseded_by are kept — they form the history trail
    /// exposed by `/v1/memories/:id/history`.
    /// A 24-hour grace period prevents deleting freshly-archived memories
    /// (e.g. working memories archived by the same governance run).
    pub async fn cleanup_stale(&self, user_id: &str) -> Result<i64, MemoriaError> {
        let memories_table = self.t("mem_memories");
        const BATCH: u64 = 500;
        let mut total = 0i64;
        // Phase 1: delete plain inactive (no version chain, past grace period)
        loop {
            let res = sqlx::query(&format!(
                "DELETE FROM {memories_table} WHERE user_id = ? AND is_active = 0 \
                  AND (superseded_by IS NULL OR superseded_by = '') \
                  AND updated_at < DATE_SUB(NOW(), INTERVAL 24 HOUR) LIMIT 500"
            ))
            .bind(user_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            let n = res.rows_affected();
            total += n as i64;
            if n < BATCH {
                break;
            }
        }
        // Phase 2: delete broken chain rows (superseded_by target no longer exists)
        loop {
            let ids: Vec<(String,)> = sqlx::query_as(&format!(
                "SELECT old.memory_id FROM {memories_table} old \
                 LEFT JOIN {memories_table} new ON old.superseded_by = new.memory_id \
                 WHERE old.user_id = ? AND old.is_active = 0 \
                   AND old.superseded_by IS NOT NULL AND old.superseded_by != '' \
                   AND new.memory_id IS NULL LIMIT 500"
            ))
            .bind(user_id)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
            if ids.is_empty() {
                break;
            }
            let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("DELETE FROM {memories_table} WHERE memory_id IN ({placeholders})");
            let mut q = sqlx::query(&sql);
            for (id,) in &ids {
                q = q.bind(id);
            }
            let r = q.execute(&self.pool).await.map_err(db_err)?;
            total += r.rows_affected() as i64;
        }
        Ok(total)
    }

    /// Delete expired tool_result memories (TTL = 72h by default).
    pub async fn cleanup_tool_results(&self, ttl_hours: i64) -> Result<i64, MemoriaError> {
        let memories_table = self.t("mem_memories");
        let mut total = 0i64;
        loop {
            let res = sqlx::query(&format!(
                "DELETE FROM {memories_table} \
                 WHERE memory_type = 'tool_result' \
                   AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > ? \
                 LIMIT 5000"
            ))
            .bind(ttl_hours)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            let n = res.rows_affected() as i64;
            total += n;
            if n < 5000 {
                break;
            }
        }
        Ok(total)
    }

    /// Soft-delete working memories inactive for more than `stale_hours`.
    /// Returns per-user counts for audit logging.
    pub async fn archive_stale_working(
        &self,
        stale_hours: i64,
    ) -> Result<Vec<(String, i64)>, MemoriaError> {
        let memories_table = self.t("mem_memories");
        const BATCH: i64 = 500;

        // Collect affected users first (cheap DISTINCT query)
        let users: Vec<(String,)> = sqlx::query_as(&format!(
            "SELECT DISTINCT user_id FROM {memories_table} \
             WHERE memory_type = 'working' AND is_active = 1 \
               AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > ?"
        ))
        .bind(stale_hours)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        if users.is_empty() {
            return Ok(vec![]);
        }

        // Batched UPDATE per user to avoid global lock
        let mut result = Vec::with_capacity(users.len());
        for (uid,) in users {
            let mut total = 0i64;
            loop {
                let res = sqlx::query(&format!(
                    "UPDATE {memories_table} SET is_active = 0, updated_at = NOW() \
                     WHERE user_id = ? AND memory_type = 'working' AND is_active = 1 \
                       AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > ? \
                     LIMIT 500"
                ))
                .bind(&uid)
                .bind(stale_hours)
                .execute(&self.pool)
                .await
                .map_err(db_err)?;
                let n = res.rows_affected() as i64;
                total += n;
                if n < BATCH {
                    break;
                }
            }
            if total > 0 {
                result.push((uid, total));
            }
        }
        Ok(result)
    }

    /// Deactivate near-duplicate memories (same user, same type, cosine sim > threshold).
    /// Uses L2² ≈ 2(1 - cos_sim) for normalized embeddings.
    /// Returns count of deactivated memories.
    pub async fn compress_redundant(
        &self,
        user_id: &str,
        similarity_threshold: f64,
        window_days: i64,
        max_pairs: usize,
    ) -> Result<i64, MemoriaError> {
        let memories_table = self.t("mem_memories");
        // Cap the fetch at 5,000 rows to bound memory usage: each embedding can be
        // several KB, so loading unbounded rows risks exhausting heap for active users.
        // The max_pairs limit already caps pair-comparison work in the loop below.
        let rows: Vec<(String, String, chrono::NaiveDateTime, String)> = sqlx::query_as(&format!(
            "SELECT memory_id, memory_type, observed_at, embedding \
             FROM {memories_table} \
             WHERE user_id = ? AND is_active = 1 AND embedding IS NOT NULL \
               AND TIMESTAMPDIFF(DAY, observed_at, NOW()) <= ? \
               ORDER BY memory_type, observed_at DESC \
             LIMIT 5000"
        ))
        .bind(user_id)
        .bind(window_days)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        if rows.len() < 2 {
            return Ok(0);
        }

        let l2_sq_threshold = 2.0 * (1.0 - similarity_threshold);

        // Group by memory_type with flat embedding storage for cache locality
        struct Entry {
            id: String,
            ts: chrono::NaiveDateTime,
            emb_offset: usize,
            dim: usize,
        }
        let mut by_type: std::collections::HashMap<String, Vec<Entry>> = Default::default();
        let mut flat_embs: Vec<f32> = Vec::new();

        for (mid, mtype, ts, emb_str) in &rows {
            if let Ok(emb) = mo_to_vec(emb_str) {
                let offset = flat_embs.len();
                let dim = emb.len();
                flat_embs.extend_from_slice(&emb);
                by_type.entry(mtype.clone()).or_default().push(Entry {
                    id: mid.clone(),
                    ts: *ts,
                    emb_offset: offset,
                    dim,
                });
            }
        }

        let mut to_delete: Vec<String> = vec![];
        let mut deactivated_ids: std::collections::HashSet<String> = Default::default();
        let mut pairs_checked = 0;

        'outer: for group in by_type.values() {
            if group.len() < 2 {
                continue;
            }
            for i in 0..group.len() {
                if deactivated_ids.contains(&group[i].id) {
                    continue;
                }
                let emb_i = &flat_embs[group[i].emb_offset..group[i].emb_offset + group[i].dim];
                // Vectorized: compute L2² from i to all j > i
                for j in (i + 1)..group.len() {
                    if pairs_checked >= max_pairs {
                        break 'outer;
                    }
                    if deactivated_ids.contains(&group[j].id) {
                        continue;
                    }
                    pairs_checked += 1;
                    let emb_j = &flat_embs[group[j].emb_offset..group[j].emb_offset + group[j].dim];
                    let dist_sq: f32 = emb_i
                        .iter()
                        .zip(emb_j)
                        .map(|(a, b)| {
                            let d = a - b;
                            d * d
                        })
                        .sum();
                    if (dist_sq as f64) < l2_sq_threshold {
                        let older = if group[i].ts >= group[j].ts {
                            group[j].id.clone()
                        } else {
                            group[i].id.clone()
                        };
                        deactivated_ids.insert(older.clone());
                        to_delete.push(older);
                    }
                }
            }
        }

        if to_delete.is_empty() {
            return Ok(0);
        }

        // Batch DELETE redundant memories (edit_log provides audit trail)
        for chunk in to_delete.chunks(100) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!("DELETE FROM {memories_table} WHERE memory_id IN ({placeholders})");
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }

        Ok(to_delete.len() as i64)
    }

    /// Rebuild IVF vector index for a table. lists = max(1, rows/50), capped at 1024.
    pub async fn rebuild_vector_index(&self, table: &str) -> Result<i64, MemoriaError> {
        Self::validate_table_name(table)?;
        let qualified_table = self.t(table);

        // Workaround: MO PREPARE/EXECUTE stores None vecf32 as '[]' instead of NULL.
        // Nullify zero-dimension vectors before counting/indexing.
        let _ = sqlx::raw_sql(&format!(
            "UPDATE {qualified_table} SET embedding = NULL \
              WHERE embedding IS NOT NULL AND vector_dims(embedding) = 0"
        ))
        .execute(&self.pool)
        .await;

        let row: (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {qualified_table} WHERE embedding IS NOT NULL"
        ))
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        let total_rows = row.0;
        if total_rows == 0 {
            return Ok(0);
        }
        let idx_name = format!("{table}_embedding_ivf");
        // IVF hurts recall on small datasets; only build when rows >= 500
        if total_rows < 500 {
            let _ = sqlx::raw_sql(&format!("DROP INDEX {idx_name} ON {qualified_table}"))
                .execute(&self.pool)
                .await;
            return Ok(total_rows);
        }
        let lists = (total_rows / 50).clamp(1, 1024);
        let _ = sqlx::raw_sql(&format!("DROP INDEX {idx_name} ON {qualified_table}"))
            .execute(&self.pool)
            .await;
        sqlx::raw_sql(&format!(
            "CREATE INDEX {idx_name} USING ivfflat ON {qualified_table}(embedding) LISTS {lists} op_type 'vector_l2_ops'"
        ))
        .execute(&self.pool).await.map_err(db_err)?;
        Ok(total_rows)
    }

    /// Deactivate orphaned incremental session summaries (session never closed, >24h old).
    pub async fn cleanup_orphaned_incrementals(
        &self,
        user_id: &str,
        older_than_hours: i64,
    ) -> Result<i64, MemoriaError> {
        let memories_table = self.t("mem_memories");
        let ids: Vec<(String,)> = sqlx::query_as(&format!(
            "SELECT inc.memory_id FROM {memories_table} AS inc \
             WHERE inc.user_id = ? \
               AND inc.is_active = 1 \
               AND LOCATE('[session_summary:incremental]', inc.content) = 1 \
               AND inc.session_id IS NOT NULL AND inc.session_id != '' \
               AND TIMESTAMPDIFF(HOUR, inc.observed_at, NOW()) > ? \
               AND NOT EXISTS ( \
                   SELECT 1 FROM {memories_table} AS full_s \
                   WHERE full_s.user_id = ? \
                     AND full_s.is_active = 1 \
                     AND full_s.session_id IS NULL \
                     AND LOCATE('[session_summary]', full_s.content) = 1 \
                     AND full_s.observed_at > inc.observed_at \
               )"
        ))
        .bind(user_id)
        .bind(older_than_hours)
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        if ids.is_empty() {
            return Ok(0);
        }
        for chunk in ids.chunks(500) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "UPDATE {memories_table} SET is_active = 0, updated_at = NOW() WHERE memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&sql);
            for (id,) in chunk {
                q = q.bind(id);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(ids.len() as i64)
    }

    /// Drop old milestone snapshots, keep last N (weekly).
    pub async fn cleanup_snapshots(&self, keep_last_n: usize) -> Result<i64, MemoriaError> {
        let rows: Vec<(String,)> = if let Some(db_name) = self.database_name() {
            let mut snapshots: Vec<(String, Option<NaiveDateTime>)> = sqlx::query("SHOW SNAPSHOTS")
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?
                .into_iter()
                .filter_map(|row| {
                    let snapshot_name: String = row.try_get("SNAPSHOT_NAME").ok()?;
                    if !snapshot_name.starts_with("mem_milestone_") {
                        return None;
                    }
                    let snapshot_db = row.try_get::<String, _>("DATABASE_NAME").ok()?;
                    if snapshot_db != db_name {
                        return None;
                    }
                    let timestamp =
                        row.try_get::<NaiveDateTime, _>("TIMESTAMP")
                            .ok()
                            .or_else(|| {
                                row.try_get::<String, _>("TIMESTAMP").ok().and_then(|s| {
                                    NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S%.f")
                                        .ok()
                                        .or_else(|| {
                                            NaiveDateTime::parse_from_str(&s, "%Y-%m-%d %H:%M:%S")
                                                .ok()
                                        })
                                })
                            });
                    Some((snapshot_name, timestamp))
                })
                .collect();
            snapshots.sort_by_key(|b| std::cmp::Reverse(b.1));
            snapshots.into_iter().map(|(name, _)| (name,)).collect()
        } else {
            sqlx::query_as(
                "SELECT sname FROM mo_catalog.mo_snapshots \
                 WHERE prefix_eq(sname, 'mem_milestone_') ORDER BY ts DESC",
            )
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?
        };

        if rows.len() <= keep_last_n {
            return Ok(0);
        }
        let mut dropped = 0i64;
        for (name,) in &rows[keep_last_n..] {
            let _ = sqlx::raw_sql(&format!("DROP SNAPSHOT {name}"))
                .execute(&self.pool)
                .await;
            dropped += 1;
        }
        Ok(dropped)
    }

    /// Clean up sandbox branches that were not properly dropped (weekly).
    pub async fn cleanup_orphan_branches(&self) -> Result<i64, MemoriaError> {
        let Some(db_name) = self.database_name().map(str::to_string) else {
            return Ok(0);
        };
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = ? AND table_name LIKE 'memories_sandbox_%'",
        )
        .bind(&db_name)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        let mut cleaned = 0i64;
        for (table_name,) in rows {
            let _ = sqlx::raw_sql(&format!(
                "DATA BRANCH DELETE TABLE {}.{}",
                quote_ident(&db_name),
                quote_ident(&table_name)
            ))
            .execute(&self.pool)
            .await;
            cleaned += 1;
        }
        Ok(cleaned)
    }

    /// Bump access_count for retrieved memories (fire-and-forget style).
    pub async fn bump_access_counts(&self, memory_ids: &[String]) -> Result<(), MemoriaError> {
        if memory_ids.is_empty() {
            return Ok(());
        }
        let memory_stats_table = self.t("mem_memories_stats");
        for chunk in memory_ids.chunks(100) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "(?, 1, NOW())").collect();
            let sql = format!(
                "INSERT INTO {memory_stats_table} (memory_id, access_count, last_accessed_at) VALUES {} \
                 ON DUPLICATE KEY UPDATE access_count = access_count + 1, last_accessed_at = NOW()",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(())
    }

    /// Batch bump with pre-aggregated counts (used by AccessCounter flush).
    pub async fn bump_access_counts_batch(
        &self,
        batch: &[(String, u64)],
    ) -> Result<(), MemoriaError> {
        if batch.is_empty() {
            return Ok(());
        }
        let memory_stats_table = self.t("mem_memories_stats");
        for chunk in batch.chunks(100) {
            let placeholders: Vec<String> =
                chunk.iter().map(|_| "(?, ?, NOW())".to_string()).collect();
            let sql = format!(
                "INSERT INTO {memory_stats_table} (memory_id, access_count, last_accessed_at) VALUES {} \
                 ON DUPLICATE KEY UPDATE access_count = access_count + VALUES(access_count), last_accessed_at = NOW()",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for (id, count) in chunk {
                q = q.bind(id).bind(*count as i64);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(())
    }

    /// Reset access_count to 0 for all memories of a user.
    pub async fn reset_access_counts(&self, user_id: &str) -> Result<i64, MemoriaError> {
        let mut conn = self.conn().await?;
        let stats_table = self.t("mem_memories_stats");
        let memories_table = self.t("mem_memories");
        let result = sqlx::query(&format!(
            "UPDATE {stats_table} s \
             JOIN {memories_table} m ON s.memory_id = m.memory_id \
             SET s.access_count = 0 \
             WHERE m.user_id = ?"
        ))
        .bind(user_id)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
        Ok(result.rows_affected() as i64)
    }

    /// Clean up orphaned stats records (stats without corresponding memory).
    /// Runs in batches of 1,000 to limit lock pressure.
    ///
    /// Multi-table DELETE with LIMIT is not valid MySQL/MatrixOne syntax, so we
    /// first SELECT the orphan IDs and then DELETE them by primary key.
    pub async fn cleanup_orphan_stats(&self) -> Result<i64, MemoriaError> {
        let stats_table = self.t("mem_memories_stats");
        let memories_table = self.t("mem_memories");
        const BATCH: i64 = 1000;
        let mut total = 0i64;
        loop {
            // Step 1: collect up to BATCH orphan IDs.
            let ids: Vec<(String,)> = sqlx::query_as(&format!(
                "SELECT s.memory_id \
                 FROM {stats_table} s \
                 LEFT JOIN {memories_table} m ON s.memory_id = m.memory_id \
                 WHERE m.memory_id IS NULL \
                 LIMIT 1000"
            ))
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;

            if ids.is_empty() {
                break;
            }

            // Step 2: delete by primary key (single-table, so LIMIT is allowed, though
            // not needed here since the IN-list is already capped at BATCH).
            let placeholders: Vec<&str> = ids.iter().map(|_| "?").collect();
            let sql = format!(
                "DELETE FROM {stats_table} WHERE memory_id IN ({})",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for (id,) in &ids {
                q = q.bind(id);
            }
            let n = q.execute(&self.pool).await.map_err(db_err)?.rows_affected() as i64;
            total += n;

            if (ids.len() as i64) < BATCH {
                break;
            }
        }
        Ok(total)
    }

    /// Delete old audit-log rows older than `retain_days` days, in batches to avoid lock pressure.
    pub async fn cleanup_edit_log(&self, retain_days: i64) -> Result<i64, MemoriaError> {
        let edit_log_table = self.t("mem_edit_log");
        const BATCH: u64 = 1000;
        let mut total = 0i64;
        loop {
            let res = sqlx::query(&format!(
                "DELETE FROM {edit_log_table} WHERE created_at < DATE_SUB(NOW(), INTERVAL ? DAY) LIMIT 1000"
            ))
            .bind(retain_days)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            let n = res.rows_affected();
            total += n as i64;
            if n < BATCH {
                break;
            }
        }
        Ok(total)
    }

    /// Delete old feedback rows older than `retain_days` days, in batches.
    pub async fn cleanup_feedback(&self, retain_days: i64) -> Result<i64, MemoriaError> {
        let feedback_table = self.t("mem_retrieval_feedback");
        const BATCH: u64 = 1000;
        let mut total = 0i64;
        loop {
            let res = sqlx::query(&format!(
                "DELETE FROM {feedback_table} WHERE created_at < DATE_SUB(NOW(), INTERVAL ? DAY) LIMIT 1000"
            ))
            .bind(retain_days)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
            let n = res.rows_affected();
            total += n as i64;
            if n < BATCH {
                break;
            }
        }
        Ok(total)
    }

    /// Remove orphaned rows from `mem_entity_links` whose memory_id
    /// no longer exists or is inactive in `mem_memories`. Idempotent, batch-safe.
    pub async fn cleanup_orphan_entity_links(&self) -> Result<i64, MemoriaError> {
        let entity_links_table = self.t("mem_entity_links");
        let memories_table = self.t("mem_memories");
        // Two-step: find orphan IDs, then delete by primary key.
        let orphans: Vec<(String,)> = sqlx::query_as(&format!(
            "SELECT l.id FROM {entity_links_table} l \
             LEFT JOIN {memories_table} m ON l.memory_id = m.memory_id AND m.is_active = 1 \
             WHERE m.memory_id IS NULL \
             LIMIT 5000"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        if orphans.is_empty() {
            return Ok(0);
        }
        let placeholders = orphans.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!("DELETE FROM {entity_links_table} WHERE id IN ({placeholders})");
        let mut q = sqlx::query(&sql);
        for (id,) in &orphans {
            q = q.bind(id);
        }
        let r = q.execute(&self.pool).await.map_err(db_err)?;
        Ok(r.rows_affected() as i64)
    }

    /// Delete entity links in `mem_entity_links` for a specific memory_id.
    pub async fn delete_entity_links_by_memory_id(
        &self,
        memory_id: &str,
    ) -> Result<i64, MemoriaError> {
        let table = self.t("mem_entity_links");
        let r = sqlx::query(&format!("DELETE FROM {table} WHERE memory_id = ?"))
            .bind(memory_id)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(r.rows_affected() as i64)
    }

    /// Validate table name to prevent SQL injection
    fn validate_table_name(table: &str) -> Result<(), MemoriaError> {
        // 只允许字母、数字、下划线
        if !table.chars().all(|c| c.is_alphanumeric() || c == '_') {
            return Err(MemoriaError::Validation(format!(
                "Invalid table name: {}",
                table
            )));
        }
        // 白名单验证（允许 mem_ 和 test_ 前缀）
        if !table.starts_with("mem_") && !table.starts_with("test_") {
            return Err(MemoriaError::Validation(format!(
                "Table not allowed for vector index operations: {}",
                table
            )));
        }
        Ok(())
    }

    /// Check if vector index needs rebuild and is not in cooldown.
    /// Returns (should_rebuild, current_row_count, cooldown_remaining_secs)
    pub async fn should_rebuild_vector_index(
        &self,
        table: &str,
    ) -> Result<(bool, i64, Option<i64>), MemoriaError> {
        let mut conn = self.conn().await?;
        Self::validate_table_name(table)?;
        let qualified_table = self.t(table);
        let runtime_table = self.t("mem_governance_runtime_state");
        let key = format!("vector_index_rebuild:{table}");

        // 1. 检查冷却
        let cooldown_check: Option<(chrono::NaiveDateTime,)> = sqlx::query_as(&format!(
            "SELECT circuit_open_until FROM {runtime_table} \
             WHERE strategy_key = ? AND `task` = 'rebuild'"
        ))
        .bind(&key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_err)?;

        if let Some((until,)) = cooldown_check {
            let now = chrono::Utc::now().naive_utc();
            if until > now {
                let remaining = (until - now).num_seconds();
                return Ok((false, 0, Some(remaining)));
            }
        }

        // 2. 查当前行数（表可能不存在）
        let current_rows: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {qualified_table} WHERE embedding IS NOT NULL"
        ))
        .fetch_one(&mut *conn)
        .await
        .unwrap_or_default(); // 表不存在或查询失败，返回0

        // 3. 查上次重建时的行数
        let last_rows: Option<(i32,)> = sqlx::query_as(&format!(
            "SELECT failure_count FROM {runtime_table} \
             WHERE strategy_key = ? AND `task` = 'rebuild'"
        ))
        .bind(&key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_err)?;

        let last_rows = last_rows.map(|(c,)| c as i64).unwrap_or(0);

        // 4. 判断是否需要重建
        let should_rebuild = if current_rows < 500 {
            false // 小数据集不需要 IVF
        } else if last_rows == 0 {
            true // 首次重建
        } else {
            let growth_ratio = (current_rows - last_rows) as f64 / last_rows as f64;
            growth_ratio > 0.2 // 增长超过 20%
        };

        Ok((should_rebuild, current_rows, None))
    }

    /// Record vector index rebuild and set adaptive cooldown.
    pub async fn record_vector_index_rebuild(
        &self,
        table: &str,
        row_count: i64,
        cooldown_secs: i64,
    ) -> Result<(), MemoriaError> {
        let mut conn = self.conn().await?;
        Self::validate_table_name(table)?;
        let runtime_table = self.t("mem_governance_runtime_state");
        let key = format!("vector_index_rebuild:{table}");

        let cooldown_until =
            chrono::Utc::now().naive_utc() + chrono::Duration::seconds(cooldown_secs);

        sqlx::query(&format!(
            "INSERT INTO {runtime_table} \
             (strategy_key, `task`, failure_count, circuit_open_until, updated_at) \
             VALUES (?, 'rebuild', ?, ?, NOW()) \
             ON DUPLICATE KEY UPDATE \
             failure_count = VALUES(failure_count), \
             circuit_open_until = VALUES(circuit_open_until), \
             updated_at = NOW()"
        ))
        .bind(&key)
        .bind(row_count as i32) // 复用 failure_count 字段存行数
        .bind(cooldown_until)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        Ok(())
    }

    /// Record vector index rebuild failure with exponential backoff.
    /// Returns the cooldown seconds applied.
    pub async fn record_vector_index_rebuild_failure(
        &self,
        table: &str,
    ) -> Result<i64, MemoriaError> {
        let mut conn = self.conn().await?;
        Self::validate_table_name(table)?;
        let runtime_table = self.t("mem_governance_runtime_state");
        let key = format!("vector_index_rebuild:{table}");

        // 查询当前失败次数（存储在 failure_count 的负数）
        let current_failures: Option<(i32,)> = sqlx::query_as(&format!(
            "SELECT failure_count FROM {runtime_table} \
             WHERE strategy_key = ? AND `task` = 'rebuild' AND failure_count < 0"
        ))
        .bind(&key)
        .fetch_optional(&mut *conn)
        .await
        .map_err(db_err)?;

        let failure_count = current_failures.map(|(c,)| -c).unwrap_or(0) + 1;

        // 指数退避：5分钟 → 15分钟 → 1小时
        let cooldown_secs = match failure_count {
            1 => 300,  // 5分钟
            2 => 900,  // 15分钟
            _ => 3600, // 1小时
        };

        let cooldown_until =
            chrono::Utc::now().naive_utc() + chrono::Duration::seconds(cooldown_secs);

        sqlx::query(&format!(
            "INSERT INTO {runtime_table} \
             (strategy_key, `task`, failure_count, circuit_open_until, updated_at) \
             VALUES (?, 'rebuild', ?, ?, NOW()) \
             ON DUPLICATE KEY UPDATE \
             failure_count = VALUES(failure_count), \
             circuit_open_until = VALUES(circuit_open_until), \
             updated_at = NOW()"
        ))
        .bind(&key)
        .bind(-(failure_count as i32)) // 负数表示失败次数
        .bind(cooldown_until)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        Ok(cooldown_secs)
    }

    /// Try to acquire a distributed lock (returns true if acquired).
    pub async fn try_acquire_lock(&self, key: &str, ttl_secs: i64) -> Result<bool, MemoriaError> {
        let mut conn = self.conn().await?;
        let lock_table = self.t("mem_distributed_locks");
        let expires_at = chrono::Utc::now().naive_utc() + chrono::Duration::seconds(ttl_secs);

        // 方案1：尝试更新过期的锁
        let update_result = sqlx::query(&format!(
            "UPDATE {lock_table} \
             SET holder_id = ?, acquired_at = NOW(), expires_at = ? \
             WHERE lock_key = ? AND expires_at < NOW()"
        ))
        .bind(&self.instance_id)
        .bind(expires_at)
        .bind(key)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        if update_result.rows_affected() > 0 {
            return Ok(true); // 成功更新过期锁
        }

        // 方案2：尝试插入新锁
        let insert_result = sqlx::query(&format!(
            "INSERT INTO {lock_table} (lock_key, holder_id, acquired_at, expires_at) \
             VALUES (?, ?, NOW(), ?)"
        ))
        .bind(key)
        .bind(&self.instance_id)
        .bind(expires_at)
        .execute(&mut *conn)
        .await;

        match insert_result {
            Ok(_) => Ok(true), // 成功插入新锁
            Err(e) => {
                // 检查是否是主键冲突（锁已存在且未过期）
                let err_str = e.to_string();
                if err_str.contains("Duplicate") || err_str.contains("1062") {
                    // MatrixOne SI: the row may have been deleted by another
                    // connection but our snapshot still sees the old key.
                    // A fresh SELECT forces a snapshot refresh.
                    let exists: (i64,) = sqlx::query_as(&format!(
                        "SELECT COUNT(*) FROM {lock_table} \
                         WHERE lock_key = ? AND expires_at >= NOW()"
                    ))
                    .bind(key)
                    .fetch_one(&mut *conn)
                    .await
                    .map_err(db_err)?;
                    if exists.0 > 0 {
                        return Ok(false); // lock genuinely held
                    }
                    // Row was deleted — retry INSERT with refreshed snapshot
                    let retry = sqlx::query(&format!(
                        "INSERT INTO {lock_table} (lock_key, holder_id, acquired_at, expires_at) \
                         VALUES (?, ?, NOW(), ?)"
                    ))
                    .bind(key)
                    .bind(&self.instance_id)
                    .bind(expires_at)
                    .execute(&mut *conn)
                    .await;
                    match retry {
                        Ok(_) => Ok(true),
                        Err(e2) => {
                            let s = e2.to_string();
                            if s.contains("Duplicate") || s.contains("1062") {
                                Ok(false)
                            } else {
                                Err(db_err(e2))
                            }
                        }
                    }
                } else {
                    Err(db_err(e))
                }
            }
        }
    }

    /// Release a distributed lock.
    pub async fn release_lock(&self, key: &str) -> Result<(), MemoriaError> {
        let mut conn = self.conn().await?;
        let lock_table = self.t("mem_distributed_locks");
        sqlx::query(&format!(
            "DELETE FROM {lock_table} WHERE lock_key = ? AND holder_id = ?"
        ))
        .bind(key)
        .bind(&self.instance_id)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    // ── Retrieval feedback ────────────────────────────────────────────────────

    /// Record explicit relevance feedback for a memory.
    /// signal: "useful" | "irrelevant" | "outdated" | "wrong"
    pub async fn record_feedback(
        &self,
        user_id: &str,
        memory_id: &str,
        signal: &str,
        context: Option<&str>,
    ) -> Result<String, MemoriaError> {
        let mut conn = self.conn().await?;
        let memories_table = self.t("mem_memories");
        let feedback_table = self.t("mem_retrieval_feedback");
        let memory_stats_table = self.t("mem_memories_stats");
        // Validate signal
        if !["useful", "irrelevant", "outdated", "wrong"].contains(&signal) {
            return Err(MemoriaError::Validation(format!(
                "Invalid signal '{}'. Must be one of: useful, irrelevant, outdated, wrong",
                signal
            )));
        }
        // Verify memory exists and belongs to user
        let count: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE memory_id = ? AND user_id = ?"
        ))
        .bind(memory_id)
        .bind(user_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;
        if count == 0 {
            return Err(MemoriaError::NotFound(format!(
                "Memory {} not found or not owned by user",
                memory_id
            )));
        }

        let id = uuid::Uuid::new_v4().simple().to_string();
        sqlx::query(&format!(
            "INSERT INTO {feedback_table} (id, user_id, memory_id, signal, context, created_at) \
             VALUES (?, ?, ?, ?, ?, NOW())"
        ))
        .bind(&id)
        .bind(user_id)
        .bind(memory_id)
        .bind(signal)
        .bind(context)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;

        // Update denormalized feedback counters in mem_memories_stats
        let col = match signal {
            "useful" => "feedback_useful",
            "irrelevant" => "feedback_irrelevant",
            "outdated" => "feedback_outdated",
            "wrong" => "feedback_wrong",
            _ => unreachable!(),
        };
        let sql = format!(
            "INSERT INTO {memory_stats_table} (memory_id, {col}, last_feedback_at) VALUES (?, 1, NOW()) \
             ON DUPLICATE KEY UPDATE {col} = {col} + 1, last_feedback_at = NOW()"
        );
        sqlx::query(&sql)
            .bind(memory_id)
            .execute(&mut *conn)
            .await
            .map_err(db_err)?;

        Ok(id)
    }

    /// Get feedback statistics for a user (for adaptive tuning analysis).
    pub async fn get_feedback_stats(&self, user_id: &str) -> Result<FeedbackStats, MemoriaError> {
        let feedback_table = self.t("mem_retrieval_feedback");
        let row: (i64, i64, i64, i64, i64) = sqlx::query_as(&format!(
            "SELECT \
               COUNT(*) as total, \
               COUNT(CASE WHEN signal = 'useful' THEN 1 END) as useful, \
               COUNT(CASE WHEN signal = 'irrelevant' THEN 1 END) as irrelevant, \
               COUNT(CASE WHEN signal = 'outdated' THEN 1 END) as outdated, \
               COUNT(CASE WHEN signal = 'wrong' THEN 1 END) as wrong \
             FROM {feedback_table} WHERE user_id = ?"
        ))
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(FeedbackStats {
            total: row.0,
            useful: row.1,
            irrelevant: row.2,
            outdated: row.3,
            wrong: row.4,
        })
    }

    /// Get feedback breakdown by trust tier (for adaptive tuning).
    pub async fn get_feedback_by_tier(
        &self,
        user_id: &str,
    ) -> Result<Vec<TierFeedback>, MemoriaError> {
        let feedback_table = self.t("mem_retrieval_feedback");
        let memories_table = self.t("mem_memories");
        let rows: Vec<(String, String, i64)> = sqlx::query_as(&format!(
            "SELECT m.trust_tier, f.signal, COUNT(*) as cnt \
             FROM {feedback_table} f \
             JOIN {memories_table} m ON f.memory_id = m.memory_id \
             WHERE f.user_id = ? \
             GROUP BY m.trust_tier, f.signal \
             ORDER BY m.trust_tier, f.signal"
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .into_iter()
            .map(|(tier, signal, count)| TierFeedback {
                tier,
                signal,
                count,
            })
            .collect())
    }

    /// Get feedback counts for a single memory (from denormalized stats, no JOIN).
    pub async fn get_memory_feedback(
        &self,
        memory_id: &str,
    ) -> Result<MemoryFeedback, MemoriaError> {
        let memory_stats_table = self.t("mem_memories_stats");
        let row: Option<(i32, i32, i32, i32)> = sqlx::query_as(&format!(
            "SELECT feedback_useful, feedback_irrelevant, feedback_outdated, feedback_wrong \
             FROM {memory_stats_table} WHERE memory_id = ?"
        ))
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(row
            .map(|(useful, irrelevant, outdated, wrong)| MemoryFeedback {
                useful,
                irrelevant,
                outdated,
                wrong,
            })
            .unwrap_or_default())
    }

    /// Get feedback counts for multiple memories (batch, for retrieval scoring).
    pub async fn get_feedback_batch(
        &self,
        memory_ids: &[String],
    ) -> Result<std::collections::HashMap<String, MemoryFeedback>, MemoriaError> {
        let memory_stats_table = self.t("mem_memories_stats");
        let mut map = std::collections::HashMap::new();
        if memory_ids.is_empty() {
            return Ok(map);
        }
        for chunk in memory_ids.chunks(500) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT memory_id, feedback_useful, feedback_irrelevant, feedback_outdated, feedback_wrong \
                 FROM {memory_stats_table} WHERE memory_id IN ({})",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
            for row in &rows {
                let id: String = row.try_get("memory_id").map_err(db_err)?;
                let useful: i32 = row.try_get("feedback_useful").unwrap_or(0);
                let irrelevant: i32 = row.try_get("feedback_irrelevant").unwrap_or(0);
                let outdated: i32 = row.try_get("feedback_outdated").unwrap_or(0);
                let wrong: i32 = row.try_get("feedback_wrong").unwrap_or(0);
                map.insert(
                    id,
                    MemoryFeedback {
                        useful,
                        irrelevant,
                        outdated,
                        wrong,
                    },
                );
            }
        }
        Ok(map)
    }

    // ── Per-User Retrieval Parameters ─────────────────────────────────────────

    /// Get user's retrieval parameters, or default if not set.
    pub async fn get_user_retrieval_params(
        &self,
        user_id: &str,
    ) -> Result<UserRetrievalParams, MemoriaError> {
        let retrieval_params_table = self.t("mem_user_retrieval_params");
        let row = sqlx::query(&format!(
            "SELECT feedback_weight, temporal_decay_hours, confidence_weight \
             FROM {retrieval_params_table} WHERE user_id = ?"
        ))
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;

        match row {
            Some(r) => Ok(UserRetrievalParams {
                user_id: user_id.to_string(),
                feedback_weight: r.try_get("feedback_weight").unwrap_or(0.1),
                temporal_decay_hours: r.try_get("temporal_decay_hours").unwrap_or(168.0),
                confidence_weight: r.try_get("confidence_weight").unwrap_or(0.1),
            }),
            None => Ok(UserRetrievalParams {
                user_id: user_id.to_string(),
                ..Default::default()
            }),
        }
    }

    /// Update user's retrieval parameters.
    pub async fn set_user_retrieval_params(
        &self,
        params: &UserRetrievalParams,
    ) -> Result<(), MemoriaError> {
        let retrieval_params_table = self.t("mem_user_retrieval_params");
        let now = Utc::now().format("%Y-%m-%d %H:%M:%S%.6f").to_string();
        sqlx::query(&format!(
            "INSERT INTO {retrieval_params_table} \
             (user_id, feedback_weight, temporal_decay_hours, confidence_weight, updated_at) \
             VALUES (?, ?, ?, ?, ?) \
             ON DUPLICATE KEY UPDATE \
             feedback_weight = VALUES(feedback_weight), \
             temporal_decay_hours = VALUES(temporal_decay_hours), \
             confidence_weight = VALUES(confidence_weight), \
             updated_at = VALUES(updated_at)"
        ))
        .bind(&params.user_id)
        .bind(params.feedback_weight)
        .bind(params.temporal_decay_hours)
        .bind(params.confidence_weight)
        .bind(&now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Get access_count for a set of memory IDs.
    pub async fn get_access_counts(
        &self,
        memory_ids: &[String],
    ) -> Result<std::collections::HashMap<String, i32>, MemoriaError> {
        let memory_stats_table = self.t("mem_memories_stats");
        let mut map = std::collections::HashMap::new();
        if memory_ids.is_empty() {
            return Ok(map);
        }
        for chunk in memory_ids.chunks(500) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT memory_id, access_count FROM {memory_stats_table} WHERE memory_id IN ({})",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
            for row in &rows {
                let id: String = row.try_get("memory_id").map_err(db_err)?;
                let count: i32 = row.try_get("access_count").map_err(db_err)?;
                map.insert(id, count);
            }
        }
        Ok(map)
    }

    /// Combined fetch of access_count + feedback in a single query (replaces
    /// separate get_access_counts + get_feedback_batch calls).
    pub async fn get_stats_batch(
        &self,
        memory_ids: &[String],
    ) -> Result<
        (
            std::collections::HashMap<String, i32>,
            std::collections::HashMap<String, MemoryFeedback>,
        ),
        MemoriaError,
    > {
        let memory_stats_table = self.t("mem_memories_stats");
        let mut ac_map = std::collections::HashMap::new();
        let mut fb_map = std::collections::HashMap::new();
        if memory_ids.is_empty() {
            return Ok((ac_map, fb_map));
        }
        for chunk in memory_ids.chunks(500) {
            let placeholders: Vec<&str> = chunk.iter().map(|_| "?").collect();
            let sql = format!(
                "SELECT memory_id, access_count, \
                 feedback_useful, feedback_irrelevant, feedback_outdated, feedback_wrong \
                 FROM {memory_stats_table} WHERE memory_id IN ({})",
                placeholders.join(", ")
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
            for row in &rows {
                let id: String = row.try_get("memory_id").map_err(db_err)?;
                let count: i32 = row.try_get("access_count").unwrap_or(0);
                ac_map.insert(id.clone(), count);
                fb_map.insert(
                    id,
                    MemoryFeedback {
                        useful: row.try_get("feedback_useful").unwrap_or(0),
                        irrelevant: row.try_get("feedback_irrelevant").unwrap_or(0),
                        outdated: row.try_get("feedback_outdated").unwrap_or(0),
                        wrong: row.try_get("feedback_wrong").unwrap_or(0),
                    },
                );
            }
        }
        Ok((ac_map, fb_map))
    }

    /// Detect pollution: high supersede ratio in recent changes (threshold=0.3).
    pub async fn detect_pollution(
        &self,
        user_id: &str,
        since_hours: i64,
    ) -> Result<bool, MemoriaError> {
        let mut conn = self.conn().await?;
        let memories_table = self.t("mem_memories");
        let row: (i64, i64) = sqlx::query_as(&format!(
            "SELECT COUNT(*) as total_changes, \
             COUNT(CASE WHEN superseded_by IS NOT NULL AND superseded_by != '' THEN 1 END) as supersedes \
             FROM {memories_table} \
             WHERE user_id = ? AND updated_at >= DATE_SUB(NOW(), INTERVAL ? HOUR)"
        ))
        .bind(user_id)
        .bind(since_hours)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;
        let (total, supersedes) = row;
        if total == 0 {
            return Ok(false);
        }
        Ok(supersedes as f64 / total as f64 > 0.3)
    }

    /// Hygiene diagnostics: orphan counts and stale data that governance can clean.
    pub async fn health_hygiene(&self, user_id: &str) -> Result<serde_json::Value, MemoriaError> {
        let mut conn = self.conn().await?;
        let memories_table = self.t("mem_memories");
        let memory_entity_links_table = self.t("mem_memory_entity_links");
        let entity_links_table = self.t("mem_entity_links");
        let graph_nodes_table = self.t("memory_graph_nodes");
        let (inactive,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE user_id = ? AND is_active = 0 \
             AND (superseded_by IS NULL OR superseded_by = '') \
             AND updated_at < DATE_SUB(NOW(), INTERVAL 24 HOUR)"
        ))
        .bind(user_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let (stale_working,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE user_id = ? AND memory_type = 'working' \
             AND is_active = 1 AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > 24"
        ))
        .bind(user_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let (orphan_mel,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {memory_entity_links_table} l \
             LEFT JOIN {memories_table} m ON l.memory_id = m.memory_id AND m.is_active = 1 \
             WHERE l.user_id = ? AND m.memory_id IS NULL"
        ))
        .bind(user_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let (orphan_el,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {entity_links_table} l \
             LEFT JOIN {memories_table} m ON l.memory_id = m.memory_id AND m.is_active = 1 \
             WHERE l.user_id = ? AND m.memory_id IS NULL"
        ))
        .bind(user_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let (orphan_graph_nodes,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {graph_nodes_table} g \
             LEFT JOIN {memories_table} m ON g.memory_id = m.memory_id \
             WHERE g.user_id = ? AND g.is_active = 1 AND g.memory_id IS NOT NULL \
               AND (m.is_active = 0 OR m.memory_id IS NULL)"
        ))
        .bind(user_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        Ok(serde_json::json!({
            "inactive_memories": inactive,
            "stale_working_memories": stale_working,
            "orphan_memory_entity_links": orphan_mel,
            "orphan_entity_links": orphan_el,
            "orphan_graph_nodes": orphan_graph_nodes,
        }))
    }

    /// Global hygiene diagnostics (admin).
    pub async fn health_hygiene_global(&self) -> Result<serde_json::Value, MemoriaError> {
        let mut conn = self.conn().await?;
        let memories_table = self.t("mem_memories");
        let memory_entity_links_table = self.t("mem_memory_entity_links");
        let entity_links_table = self.t("mem_entity_links");
        let graph_nodes_table = self.t("memory_graph_nodes");
        let memory_stats_table = self.t("mem_memories_stats");
        let (inactive,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE is_active = 0 \
             AND (superseded_by IS NULL OR superseded_by = '') \
             AND updated_at < DATE_SUB(NOW(), INTERVAL 24 HOUR)"
        ))
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let (stale_working,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE memory_type = 'working' \
             AND is_active = 1 AND TIMESTAMPDIFF(HOUR, observed_at, NOW()) > 24"
        ))
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let (orphan_mel,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {memory_entity_links_table} l \
             LEFT JOIN {memories_table} m ON l.memory_id = m.memory_id AND m.is_active = 1 \
             WHERE m.memory_id IS NULL"
        ))
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let (orphan_el,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {entity_links_table} l \
             LEFT JOIN {memories_table} m ON l.memory_id = m.memory_id AND m.is_active = 1 \
             WHERE m.memory_id IS NULL"
        ))
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let (orphan_graph_nodes,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {graph_nodes_table} g \
             LEFT JOIN {memories_table} m ON g.memory_id = m.memory_id \
             WHERE g.is_active = 1 AND g.memory_id IS NOT NULL \
               AND (m.is_active = 0 OR m.memory_id IS NULL)"
        ))
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let (orphan_stats,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {memory_stats_table} s \
             LEFT JOIN {memories_table} m ON s.memory_id = m.memory_id \
             WHERE m.memory_id IS NULL"
        ))
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        Ok(serde_json::json!({
            "inactive_memories": inactive,
            "stale_working_memories": stale_working,
            "orphan_memory_entity_links": orphan_mel,
            "orphan_entity_links": orphan_el,
            "orphan_graph_nodes": orphan_graph_nodes,
            "orphan_stats": orphan_stats,
        }))
    }

    /// Per-type stats: count, avg_confidence, contradiction_rate, avg_staleness_hours.
    pub async fn health_analyze(&self, user_id: &str) -> Result<serde_json::Value, MemoriaError> {
        let mut conn = self.conn().await?;
        let memories_table = self.t("mem_memories");
        let rows: Vec<(String, i64, Option<f64>, i64, f64)> = sqlx::query_as(&format!(
            "SELECT memory_type, COUNT(*) as total, \
             CAST(AVG(initial_confidence) AS DOUBLE) as avg_conf, \
             COUNT(CASE WHEN superseded_by IS NOT NULL AND superseded_by != '' THEN 1 END) as superseded, \
             CAST(AVG(TIMESTAMPDIFF(HOUR, observed_at, NOW())) AS DOUBLE) as avg_stale_h \
             FROM {memories_table} WHERE user_id = ? GROUP BY memory_type"
        ))
        .bind(user_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?;

        let mut stats = serde_json::Map::new();
        for (mtype, total, avg_conf, superseded, avg_stale) in rows {
            let contradiction_rate = if total > 0 {
                superseded as f64 / total as f64
            } else {
                0.0
            };
            stats.insert(
                mtype,
                serde_json::json!({
                    "total": total,
                    "avg_confidence": avg_conf,
                    "contradiction_rate": contradiction_rate,
                    "avg_staleness_hours": avg_stale,
                }),
            );
        }
        Ok(serde_json::Value::Object(stats))
    }

    /// Storage stats: total, active, inactive, avg_content_size, oldest, newest.
    pub async fn health_storage_stats(
        &self,
        user_id: &str,
    ) -> Result<serde_json::Value, MemoriaError> {
        let mut conn = self.conn().await?;
        let memories_table = self.t("mem_memories");
        let row: (i64, i64, f64) = sqlx::query_as(&format!(
            "SELECT COUNT(*) as total, \
             COUNT(CASE WHEN is_active = 1 THEN 1 END) as active, \
             CAST(COALESCE(AVG(LENGTH(content)), 0) AS DOUBLE) as avg_content_size \
             FROM {memories_table} WHERE user_id = ?"
        ))
        .bind(user_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        Ok(serde_json::json!({
            "total": row.0,
            "active": row.1,
            "inactive": row.0 - row.1,
            "avg_content_size": row.2,
        }))
    }

    /// IVF capacity estimate: global vector count + growth rate + recommendation.
    pub async fn health_capacity(&self, user_id: &str) -> Result<serde_json::Value, MemoriaError> {
        let mut conn = self.conn().await?;
        let memories_table = self.t("mem_memories");
        const IVF_OPTIMAL: i64 = 50_000;
        const IVF_DEGRADED: i64 = 200_000;

        let (user_active,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE user_id = ? AND is_active = 1"
        ))
        .bind(user_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let (global_total,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE is_active = 1"
        ))
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let (added_30d,): (i64,) = sqlx::query_as(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE user_id = ? AND observed_at >= NOW() - INTERVAL 30 DAY"
        ))
        .bind(user_id)
        .fetch_one(&mut *conn)
        .await
        .map_err(db_err)?;

        let recommendation = if global_total > IVF_DEGRADED {
            "partition_required"
        } else if global_total > IVF_OPTIMAL {
            "monitor_query_latency"
        } else {
            "ok"
        };

        Ok(serde_json::json!({
            "user_active_memories": user_active,
            "global_vector_count": global_total,
            "monthly_growth_rate": added_30d,
            "ivf_thresholds": {"optimal": IVF_OPTIMAL, "degraded": IVF_DEGRADED},
            "recommendation": recommendation,
        }))
    }

    // ── Batch reads ─────────────────────────────────────────────────────────

    /// Fetch multiple memories by IDs. Returns map of memory_id → Memory.
    pub async fn get_by_ids(
        &self,
        ids: &[String],
    ) -> Result<std::collections::HashMap<String, Memory>, MemoriaError> {
        let memories_table = self.t("mem_memories");
        if ids.is_empty() {
            return Ok(Default::default());
        }
        let mut map = std::collections::HashMap::new();
        // Batch in chunks of 500 to avoid SQL length limits
        for chunk in ids.chunks(500) {
            let ph = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT memory_id, user_id, author_id, subject_id, memory_type, content, \
                 embedding AS emb_str, session_id, \
                 CAST(source_event_ids AS CHAR) AS src_ids, \
                 CAST(extra_metadata AS CHAR) AS extra_meta, \
                 is_active, superseded_by, trust_tier, initial_confidence, \
                 observed_at, created_at, updated_at \
                 FROM {memories_table} WHERE memory_id IN ({ph}) AND is_active = 1"
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
            for r in &rows {
                let m = row_to_memory(r)?;
                map.insert(m.memory_id.clone(), m);
            }
        }
        Ok(map)
    }

    // ── Table-aware CRUD ──────────────────────────────────────────────────────

    /// Find the nearest active memory by embedding distance.
    /// Returns (memory_id, content, l2_distance) if within threshold.
    ///
    /// NOTE: l2_threshold assumes normalized embeddings (unit vectors).
    /// For normalized vectors: L2 = sqrt(2 * (1 - cosine_similarity)).
    /// The IVF index uses vector_l2_ops, so this query benefits from the index.
    #[allow(clippy::too_many_arguments)]
    pub async fn find_near_duplicate(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        memory_type: &str,
        exclude_id: &str,
        l2_threshold: f64,
        subject_id: Option<&str>,
    ) -> Result<Option<(String, String, f64)>, MemoriaError> {
        let table = self.t(table);
        let vec_literal = vec_to_mo(embedding);
        // Scope dedup to the same subject partition so that memories belonging
        // to different subjects never supersede or deduplicate each other.
        // None means the memory is not subject-scoped; only compare against
        // other unscoped rows (subject_id IS NULL).
        let subject_clause = match subject_id {
            Some(sid) => format!(
                " AND subject_id = '{}'",
                sanitize_sql_literal(sid)
            ),
            None => " AND subject_id IS NULL".to_string(),
        };
        let sql = format!(
            "SELECT memory_id, content, \
             l2_distance(embedding, '{vec_literal}') AS l2_dist \
             FROM {table} \
             WHERE user_id = ? AND is_active = 1 \
               AND memory_type = ? \
               AND embedding IS NOT NULL AND vector_dims(embedding) > 0 \
               AND memory_id != ? \
               {subject_clause} \
              ORDER BY l2_dist ASC LIMIT 1 by rank with option 'mode=post'"
        );
        let rows = sqlx::query(&sql)
            .bind(user_id)
            .bind(memory_type)
            .bind(exclude_id)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;

        for r in &rows {
            let dist: f64 = r
                .try_get::<f64, _>("l2_dist")
                .or_else(|_| r.try_get::<f32, _>("l2_dist").map(|v| v as f64))
                .unwrap_or(f64::MAX);
            if dist > l2_threshold {
                continue;
            }
            let mid: String = r.try_get("memory_id").map_err(db_err)?;
            let content: String = r.try_get("content").map_err(db_err)?;
            return Ok(Some((mid, content, dist)));
        }
        Ok(None)
    }

    /// Mark a memory as superseded by another.
    /// Branch-aware soft-delete: deactivate a memory in the given table.
    /// Returns the number of rows actually deactivated (0 means the memory
    /// was already inactive or not found — idempotent, not an error).
    pub async fn soft_delete_from(
        &self,
        table: &str,
        memory_id: &str,
    ) -> Result<u64, MemoriaError> {
        let mut conn = self.conn().await?;
        let table = self.t(table);
        let now = Utc::now().naive_utc();
        let res = sqlx::query(&format!(
            "UPDATE {table} SET is_active = 0, updated_at = ? WHERE memory_id = ? AND is_active = 1"
        ))
        .bind(now)
        .bind(memory_id)
        .execute(&mut *conn)
        .await
        .map_err(db_err)?;
        Ok(res.rows_affected())
    }

    /// Batch soft-delete: deactivate multiple memories in one round trip per chunk.
    pub async fn soft_delete_batch_from(
        &self,
        table: &str,
        ids: &[String],
    ) -> Result<(), MemoriaError> {
        let mut conn = self.conn().await?;
        let table = self.t(table);
        for chunk in ids.chunks(200) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "UPDATE {table} SET is_active = 0, updated_at = NOW() WHERE memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            q.execute(&mut *conn).await.map_err(db_err)?;
        }
        Ok(())
    }

    /// Batch cleanup entity data for multiple memory IDs.
    pub async fn cleanup_entity_data_batch(&self, ids: &[String]) {
        let mut conn = match self.conn().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("failed to acquire connection: {e}");
                return;
            }
        };
        let graph = self.graph_store();
        let graph_nodes_table = graph.t("memory_graph_nodes");
        let memory_entity_links_table = graph.t("mem_memory_entity_links");
        let entity_links_table = self.t("mem_entity_links");
        for chunk in ids.chunks(200) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");

            // Deactivate graph nodes
            let sql = format!(
                "UPDATE {graph_nodes_table} SET is_active = 0 WHERE memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            if let Err(e) = q.execute(&mut *conn).await {
                tracing::warn!("batch deactivate graph nodes failed: {e}");
            }

            // Delete memory_entity_links
            let sql = format!(
                "DELETE FROM {memory_entity_links_table} WHERE memory_id IN ({placeholders})"
            );
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            if let Err(e) = q.execute(&mut *conn).await {
                tracing::warn!("batch delete memory_entity_links failed: {e}");
            }

            // Delete entity_links
            let sql =
                format!("DELETE FROM {entity_links_table} WHERE memory_id IN ({placeholders})");
            let mut q = sqlx::query(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            if let Err(e) = q.execute(&mut *conn).await {
                tracing::warn!("batch delete entity_links failed: {e}");
            }
        }
    }

    /// Branch-aware get: fetch an active memory from the given table.
    pub async fn get_from(
        &self,
        table: &str,
        memory_id: &str,
    ) -> Result<Option<Memory>, MemoriaError> {
        let row = sqlx::query(&format!(
            "SELECT memory_id, user_id, author_id, subject_id, memory_type, content, \
             embedding AS emb_str, session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at \
             FROM {table} WHERE memory_id = ? AND is_active = 1"
        ))
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row_to_memory(&r)).transpose()
    }

    /// Internal observe lookup: inactive facts still need to be excluded after
    /// correction/deletion. Scope predicates apply before content leaves storage.
    pub async fn observe_exclusion_content_from(
        &self,
        table: &str,
        user_id: &str,
        subject_id: Option<&str>,
        memory_id: &str,
    ) -> Result<Option<String>, MemoriaError> {
        sqlx::query_scalar(&format!(
            "SELECT content FROM {table} WHERE memory_id = ? AND user_id = ? \
             AND (subject_id = ? OR (subject_id IS NULL AND ? IS NULL))"
        ))
        .bind(memory_id)
        .bind(user_id)
        .bind(subject_id)
        .bind(subject_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)
    }

    pub async fn supersede_memory(
        &self,
        table: &str,
        old_id: &str,
        new_id: &str,
    ) -> Result<(), MemoriaError> {
        let table = self.t(table);
        sqlx::query(&format!(
            "UPDATE {table} SET is_active = 0, superseded_by = ?, updated_at = NOW() WHERE memory_id = ?"
        ))
        .bind(new_id)
        .bind(old_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Refresh only the extra_metadata JSON of an existing memory. Used by the single-store
    /// dedup path: when a same-content near-duplicate already exists, we update the survivor's
    /// metadata (so the caller's newer scene/agent isn't silently dropped) instead of creating
    /// a phantom, never-inserted record.
    pub async fn update_extra_metadata(
        &self,
        table: &str,
        memory_id: &str,
        extra_metadata: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<(), MemoriaError> {
        let table = self.t(table);
        let json = serde_json::to_string(extra_metadata)?;
        // is_active = 1：与读取侧（get_from）「幸存者必须 active」契约对齐——若该记忆在去重
        // 检查与本次更新之间被竞态置为 inactive，则不改动已失效记录（UPDATE 命中 0 行，
        // 上层 get_from 返回 None 后走 race-insert）。
        sqlx::query(&format!(
            "UPDATE {table} SET extra_metadata = ?, updated_at = NOW() WHERE memory_id = ? AND is_active = 1"
        ))
        .bind(json)
        .bind(memory_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    #[tracing::instrument(skip(self, memory), fields(memory_id = %memory.memory_id))]
    pub async fn insert_into(&self, table: &str, memory: &Memory) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        let observed_at = memory.observed_at.map(|dt| dt.naive_utc()).unwrap_or(now);
        let created_at = memory.created_at.map(|dt| dt.naive_utc()).unwrap_or(now);
        let source_event_ids = serde_json::to_string(&memory.source_event_ids)?;
        // Workaround: MO#23859 — PREPARE/EXECUTE corrupts NULL JSON on 2nd+ execution.
        let extra_metadata = memory
            .extra_metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_else(|| "{}".to_string());
        let embedding = memory
            .embedding
            .as_deref()
            .filter(|v| !v.is_empty()) // Some([]) → None → SQL NULL
            .map(vec_to_mo);

        // MatrixOne 4.2 can retain a prepared parameter's NULL state across
        // executions (matrixorigin/matrixone#26874). Keep nullable values out
        // of bind parameters:
        // each cached SQL shape then binds a value or contains a literal NULL,
        // but never transitions the same parameter from NULL back to a value.
        let nullable = |present| if present { "?" } else { "NULL" };
        let session_id = nullable_str(&memory.session_id);
        let superseded_by = nullable_str(&memory.superseded_by);
        let author_param = nullable(memory.author_id.is_some());
        let subject_param = nullable(memory.subject_id.is_some());
        let embedding_param = nullable(embedding.is_some());
        let session_param = nullable(session_id.is_some());
        let superseded_param = nullable(superseded_by.is_some());
        let sql = format!(
            r#"INSERT INTO {table}
               (memory_id, user_id, author_id, subject_id, memory_type, content, embedding,
                session_id, source_event_ids, extra_metadata, is_active, superseded_by,
                trust_tier, initial_confidence, observed_at, created_at, updated_at)
               VALUES (?, ?, {author_param}, {subject_param}, ?, ?, {embedding_param},
                       {session_param}, ?, ?, 1, {superseded_param}, ?, ?, ?, ?, ?)"#
        );
        let mut query = sqlx::query(&sql)
            .bind(&memory.memory_id)
            .bind(&memory.user_id);
        if let Some(author_id) = memory.author_id.as_deref() {
            query = query.bind(author_id);
        }
        if let Some(subject_id) = memory.subject_id.as_deref() {
            query = query.bind(subject_id);
        }
        query = query
            .bind(memory.memory_type.to_string())
            .bind(&memory.content);
        if let Some(embedding) = embedding {
            query = query.bind(embedding);
        }
        if let Some(session_id) = session_id {
            query = query.bind(session_id);
        }
        query = query.bind(source_event_ids).bind(extra_metadata);
        if let Some(superseded_by) = superseded_by {
            query = query.bind(superseded_by);
        }
        query
            .bind(memory.trust_tier.to_string())
            .bind(memory.initial_confidence as f32)
            .bind(observed_at)
            .bind(created_at)
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Batch-insert multiple memories in a single multi-row INSERT statement.
    /// Falls back to single inserts if the batch is empty.
    pub async fn batch_insert_into(
        &self,
        table: &str,
        memories: &[&Memory],
    ) -> Result<(), MemoriaError> {
        if memories.is_empty() {
            return Ok(());
        }
        // Chunk to avoid oversized SQL statements
        for chunk in memories.chunks(50) {
            let placeholders = chunk
                .iter()
                .map(|m| {
                    let nullable = |present| if present { "?" } else { "NULL" };
                    format!(
                        "(?, ?, {}, {}, ?, ?, {}, {}, ?, ?, 1, {}, ?, ?, ?, ?, ?)",
                        nullable(m.author_id.is_some()),
                        nullable(m.subject_id.is_some()),
                        nullable(m.embedding.as_ref().is_some_and(|v| !v.is_empty())),
                        nullable(nullable_str(&m.session_id).is_some()),
                        nullable(nullable_str(&m.superseded_by).is_some())
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO {table} \
                 (memory_id, user_id, author_id, subject_id, memory_type, content, embedding, \
                  session_id, source_event_ids, extra_metadata, is_active, superseded_by, \
                  trust_tier, initial_confidence, observed_at, created_at, updated_at) \
                 VALUES {placeholders}"
            );
            let now = Utc::now().naive_utc();
            let mut q = sqlx::query(&sql);
            for m in chunk {
                let observed_at = m.observed_at.map(|dt| dt.naive_utc()).unwrap_or(now);
                let created_at = m.created_at.map(|dt| dt.naive_utc()).unwrap_or(now);
                let source_event_ids = serde_json::to_string(&m.source_event_ids)?;
                let extra_metadata = m
                    .extra_metadata
                    .as_ref()
                    .map(serde_json::to_string)
                    .transpose()?
                    .unwrap_or_else(|| "{}".to_string());
                let embedding = m
                    .embedding
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .map(vec_to_mo);
                q = q.bind(m.memory_id.clone()).bind(m.user_id.clone());
                if let Some(author_id) = &m.author_id {
                    q = q.bind(author_id.clone());
                }
                if let Some(subject_id) = &m.subject_id {
                    q = q.bind(subject_id.clone());
                }
                q = q
                    .bind(m.memory_type.to_string())
                    .bind(m.content.clone());
                if let Some(embedding) = embedding {
                    q = q.bind(embedding);
                }
                if let Some(session_id) = nullable_str(&m.session_id) {
                    q = q.bind(session_id.to_string());
                }
                q = q.bind(source_event_ids).bind(extra_metadata);
                if let Some(superseded_by) = nullable_str(&m.superseded_by) {
                    q = q.bind(superseded_by.to_string());
                }
                q = q
                    .bind(m.trust_tier.to_string())
                    .bind(m.initial_confidence as f32)
                    .bind(observed_at)
                    .bind(created_at)
                    .bind(now);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(())
    }

    pub async fn list_active_from(
        &self,
        table: &str,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let rows = sqlx::query(&format!(
            "SELECT memory_id, user_id, author_id, subject_id, memory_type, content, \
             embedding AS emb_str, session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at \
             FROM {table} WHERE memory_id IN (\
               SELECT memory_id FROM {table} \
               WHERE user_id = ? AND is_active = 1 \
               ORDER BY memory_id DESC LIMIT ?\
             ) ORDER BY memory_id DESC"
        ))
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        rows.iter().map(row_to_memory).collect()
    }

    pub fn qualified_table(&self, table: &str) -> String {
        self.t(table)
    }

    /// Lightweight list for API responses — skips embedding, source_event_ids,
    /// extra_metadata to reduce I/O and deserialization cost.
    #[allow(clippy::too_many_arguments)]
    /// "Lite" list: skips the heavy `embedding` and `source_event_ids` columns for performance,
    /// but DOES select `extra_metadata` (small JSON — needed by callers for scene/agent display),
    /// mapped via `row_to_memory_lite`. Do not assume extra_metadata is omitted here.
    pub async fn list_active_lite(
        &self,
        table: &str,
        user_id: &str,
        limit: i64,
        memory_type: Option<&str>,
        session_id: Option<&str>,
        trust_tier: Option<&str>,
        cursor: Option<&str>,
        subject_id: Option<&str>,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let table = self.t(table);
        // Cap at 501 (not 500) so the caller can request limit+1 for has_more detection.
        let safe_limit = limit.clamp(1, 501);
        // Subquery: sort only memory_id in the index, then fetch full rows for the top-N.
        let mut inner =
            format!("SELECT memory_id FROM {table} WHERE user_id = ? AND is_active = 1");
        if memory_type.is_some() {
            inner.push_str(" AND memory_type = ?");
        }
        if session_id.is_some() {
            inner.push_str(" AND session_id = ?");
        }
        if trust_tier.is_some() {
            inner.push_str(" AND trust_tier = ?");
        }
        if subject_id.is_some() {
            inner.push_str(" AND subject_id = ?");
        }
        if cursor.is_some() {
            inner.push_str(" AND memory_id < ?");
        }
        inner.push_str(" ORDER BY memory_id DESC LIMIT ?");

        let sql = format!(
            "SELECT memory_id, user_id, author_id, subject_id, memory_type, content, \
             session_id, is_active, superseded_by, trust_tier, \
             initial_confidence, observed_at, created_at, updated_at, \
             CAST(extra_metadata AS CHAR) AS extra_meta \
             FROM {table} WHERE memory_id IN ({inner}) \
             ORDER BY memory_id DESC"
        );

        let mut q = sqlx::query(&sql).bind(user_id);
        if let Some(mt) = memory_type {
            q = q.bind(mt);
        }
        if let Some(session_id) = session_id {
            q = q.bind(session_id);
        }
        if let Some(tt) = trust_tier {
            q = q.bind(tt);
        }
        if let Some(sid) = subject_id {
            q = q.bind(sid);
        }
        if let Some(c) = cursor {
            q = q.bind(c);
        }
        q = q.bind(safe_limit);
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        rows.iter().map(row_to_memory_lite).collect()
    }

    /// Exact structured query over ordinary columns and scalar extra_metadata fields.
    /// This intentionally bypasses all vector/fulltext retrieval machinery.
    #[allow(clippy::too_many_arguments)]
    pub async fn query_active_structured_lite(
        &self,
        table: &str,
        user_id: &str,
        limit: i64,
        memory_types: Option<&[MemoryType]>,
        session_id: Option<&str>,
        trust_tier: Option<&str>,
        cursor: Option<&str>,
        subject_id: Option<&str>,
        extra_metadata_filter: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<Vec<Memory>, MemoriaError> {
        if !(1..=501).contains(&limit) {
            return Err(MemoriaError::Validation(
                "structured query storage limit must be between 1 and 501".to_string(),
            ));
        }
        validate_extra_metadata_filter(extra_metadata_filter)?;
        let table = self.t(table);
        let mut metadata_filters: Vec<_> = extra_metadata_filter.iter().collect();
        metadata_filters.sort_by_key(|(key, _)| *key);

        let mut inner =
            format!("SELECT memory_id FROM {table} WHERE user_id = ? AND is_active = 1");
        if let Some(types) = memory_types.filter(|types| !types.is_empty()) {
            inner.push_str(" AND memory_type IN (");
            inner.push_str(&vec!["?"; types.len()].join(", "));
            inner.push(')');
        }
        if session_id.is_some() {
            inner.push_str(" AND session_id = ?");
        }
        if trust_tier.is_some() {
            inner.push_str(" AND trust_tier = ?");
        }
        if subject_id.is_some() {
            inner.push_str(" AND subject_id = ?");
        }
        if cursor.is_some() {
            inner.push_str(" AND memory_id < ?");
        }
        for (key, _) in &metadata_filters {
            inner.push_str(&format!(
                " AND json_extract(extra_metadata, '$.{key}') = CAST(? AS JSON)"
            ));
        }
        inner.push_str(" ORDER BY memory_id DESC LIMIT ?");

        let sql = format!(
            "SELECT memory_id, user_id, author_id, subject_id, memory_type, content, \
             session_id, is_active, superseded_by, trust_tier, \
             initial_confidence, observed_at, created_at, updated_at, \
             CAST(extra_metadata AS CHAR) AS extra_meta \
             FROM {table} WHERE memory_id IN ({inner}) \
             ORDER BY memory_id DESC"
        );

        let mut query = sqlx::query(&sql).bind(user_id);
        if let Some(types) = memory_types.filter(|types| !types.is_empty()) {
            for memory_type in types {
                query = query.bind(memory_type.to_string());
            }
        }
        if let Some(value) = session_id {
            query = query.bind(value);
        }
        if let Some(value) = trust_tier {
            query = query.bind(value);
        }
        if let Some(value) = subject_id {
            query = query.bind(value);
        }
        if let Some(value) = cursor {
            query = query.bind(value);
        }
        for (_, value) in metadata_filters {
            query = query.bind(serde_json::to_string(value)?);
        }
        query = query.bind(limit);
        let rows = query.fetch_all(&self.pool).await.map_err(db_err)?;
        rows.iter().map(row_to_memory_lite).collect()
    }

    /// Find memory IDs whose content contains `topic` (exact substring match).
    /// Uses fulltext boolean MUST with LIKE refinement. Requires topic >= 3 chars.
    pub async fn find_ids_by_topic(
        &self,
        table: &str,
        user_id: &str,
        topic: &str,
    ) -> Result<Vec<String>, MemoriaError> {
        let mut conn = self.conn().await?;
        let table = self.t(table);
        // Require minimum length to avoid full table scan
        if topic.trim().len() < 3 {
            return Err(MemoriaError::Validation(
                "topic must be at least 3 characters".into(),
            ));
        }
        let ft_safe = sanitize_fulltext_query(topic);
        let like_safe = sanitize_like_pattern(topic);
        let like_pat = format!("%{like_safe}%");

        // Try fulltext + LIKE first (fast path)
        if !ft_safe.is_empty() {
            let ft_terms: String = ft_safe
                .split_whitespace()
                .map(|w| format!("+{w}"))
                .collect::<Vec<_>>()
                .join(" ");
            let sql = format!(
                "SELECT memory_id FROM {table} \
                 WHERE user_id = ? AND is_active = 1 \
                   AND MATCH(content) AGAINST('{ft_terms}' IN BOOLEAN MODE) \
                   AND content LIKE ?"
            );
            let rows: Vec<(String,)> = sqlx::query_as(&sql)
                .bind(user_id)
                .bind(&like_pat)
                .fetch_all(&mut *conn)
                .await
                .map_err(db_err)?;
            if !rows.is_empty() {
                return Ok(rows.into_iter().map(|r| r.0).collect());
            }
        }

        // Fallback: LIKE on user's active memories (idx_user_active narrows scan)
        let sql = format!(
            "SELECT memory_id FROM {table} \
             WHERE user_id = ? AND is_active = 1 AND content LIKE ? LIMIT 500"
        );
        let rows: Vec<(String,)> = sqlx::query_as(&sql)
            .bind(user_id)
            .bind(&like_pat)
            .fetch_all(&mut *conn)
            .await
            .map_err(db_err)?;
        Ok(rows.into_iter().map(|r| r.0).collect())
    }

    pub async fn find_ids_by_session_id(
        &self,
        table: &str,
        user_id: &str,
        session_id: &str,
        memory_types: Option<&[MemoryType]>,
    ) -> Result<Vec<String>, MemoriaError> {
        let mut conn = self.conn().await?;
        let table = self.t(table);
        let mut query_builder: QueryBuilder<MySql> =
            QueryBuilder::new(format!("SELECT memory_id FROM {table} WHERE user_id = "));
        query_builder
            .push_bind(user_id)
            .push(" AND session_id = ")
            .push_bind(session_id)
            .push(" AND is_active = 1");
        if let Some(memory_types) = memory_types.filter(|types| !types.is_empty()) {
            query_builder.push(" AND memory_type IN (");
            let mut separated = query_builder.separated(", ");
            for memory_type in memory_types {
                separated.push_bind(memory_type.to_string());
            }
            separated.push_unseparated(")");
        }
        let rows = query_builder
            .build_query_as::<(String,)>()
            .fetch_all(&mut *conn)
            .await
            .map_err(db_err)?;
        Ok(rows.into_iter().map(|row| row.0).collect())
    }

    #[tracing::instrument(skip(self))]
    pub async fn search_fulltext_from(
        &self,
        table: &str,
        user_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        self.search_fulltext_from_scoped(table, user_id, query, limit, None, None, None)
            .await
    }

    /// Search all distinct terms in bounded batches. Fusion is approximate:
    /// only each batch's top `limit` candidates contribute scores, so a memory
    /// below every batch's cutoff can be absent even with a high total score.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_fulltext_from_scoped(
        &self,
        table: &str,
        user_id: &str,
        query: &str,
        limit: i64,
        session_id: Option<&str>,
        subject_id: Option<&str>,
        memory_types: Option<&[MemoryType]>,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let batches = fulltext_query_batches(query);
        if batches.is_empty() || limit <= 0 {
            return Ok(Vec::new());
        }
        if batches.len() == 1 {
            let rows = self
                .search_fulltext_batch(
                    table,
                    user_id,
                    &batches[0],
                    limit,
                    session_id,
                    subject_id,
                    memory_types,
                    true,
                )
                .await?;
            return rows
                .iter()
                .map(|row| {
                    let mut memory = row_to_memory(row)?;
                    apply_fulltext_score(row, &mut memory);
                    Ok(memory)
                })
                .collect();
        }
        // An empty owner scope must not run a large full-text plan over the
        // index. This also covers search-only benchmark tasks before any Add.
        let exists: Option<i32> = sqlx::query_scalar(&format!(
            "SELECT 1 FROM {table} WHERE user_id = ? AND is_active = 1 LIMIT 1"
        ))
        .bind(user_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        if exists.is_none() {
            return Ok(Vec::new());
        }
        // Fetch only IDs and scores until fusion determines the final winners.
        // Keep concurrency bounded and consume in query order so floating-point
        // score summation is deterministic regardless of completion order.
        use futures::{stream, StreamExt};
        let mut pending = stream::iter(batches.into_iter().map(|batch| async move {
            self.search_fulltext_batch(
                table,
                user_id,
                &batch,
                limit,
                session_id,
                subject_id,
                memory_types,
                false,
            )
            .await
        }))
        .buffered(4);
        let mut merged = std::collections::HashMap::<String, f64>::new();
        while let Some(rows) = pending.next().await {
            for row in rows? {
                let id: String = row.try_get("memory_id").map_err(db_err)?;
                *merged.entry(id).or_default() += fulltext_score(&row).unwrap_or(0.0);
            }
        }
        let mut ranked: Vec<_> = merged.into_iter().collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        ranked.truncate(limit as usize);

        // Reapply the same scopes when hydrating, in case a row was updated or
        // deactivated between candidate selection and this read. Use the caller's
        // table (which can be a branch), not the default memories table.
        let mut memories = std::collections::HashMap::new();
        for chunk in ranked.chunks(500) {
            let mut stmt = QueryBuilder::<MySql>::new(format!(
                "SELECT {FULLTEXT_MEMORY_COLUMNS} FROM {table} WHERE user_id = "
            ));
            stmt.push_bind(user_id).push(" AND is_active = 1");
            if let Some(session) = session_id {
                stmt.push(" AND (session_id = ")
                    .push_bind(session)
                    .push(" OR session_id IS NULL)");
            }
            if let Some(subject) = subject_id {
                stmt.push(" AND subject_id = ").push_bind(subject);
            }
            if let Some(types) = memory_types.filter(|types| !types.is_empty()) {
                stmt.push(" AND memory_type IN (");
                let mut separated = stmt.separated(", ");
                for memory_type in types {
                    separated.push_bind(memory_type.to_string());
                }
                stmt.push(")");
            }
            stmt.push(" AND memory_id IN (");
            let mut separated = stmt.separated(", ");
            for (id, _) in chunk {
                separated.push_bind(id);
            }
            stmt.push(")");
            for row in stmt.build().fetch_all(&self.pool).await.map_err(db_err)? {
                let memory = row_to_memory(&row)?;
                memories.insert(memory.memory_id.clone(), memory);
            }
        }
        let results = ranked
            .into_iter()
            .filter_map(|(id, score)| {
                memories.remove(&id).map(|mut memory| {
                    memory.retrieval_score = Some(score);
                    memory
                })
            })
            .collect();
        Ok(results)
    }

    #[allow(clippy::too_many_arguments)]
    async fn search_fulltext_batch(
        &self,
        table: &str,
        user_id: &str,
        safe: &str,
        limit: i64,
        session_id: Option<&str>,
        subject_id: Option<&str>,
        memory_types: Option<&[MemoryType]>,
        include_memory: bool,
    ) -> Result<Vec<sqlx::mysql::MySqlRow>, MemoriaError> {
        let session_clause = match session_id {
            Some(session) => format!(
                " AND (session_id = '{}' OR session_id IS NULL)",
                sanitize_sql_literal(session)
            ),
            None => String::new(),
        };
        let subject_clause = match subject_id {
            Some(subject) => format!(" AND subject_id = '{}'", sanitize_sql_literal(subject)),
            None => String::new(),
        };
        let types_clause = build_memory_types_in_clause_inline(memory_types);
        let columns = if include_memory {
            FULLTEXT_MEMORY_COLUMNS
        } else {
            "memory_id"
        };
        // Use OR semantics (no + prefix) — AND is too strict for natural language queries
        // because stopwords are removed from the index but +stopword still requires a match.
        let sql = format!(
            "SELECT {columns}, \
             MATCH(content) AGAINST('{safe}' IN BOOLEAN MODE) AS ft_score \
             FROM {table} \
             WHERE user_id = '{}' AND is_active = 1{session_clause}{subject_clause}{types_clause} \
               AND MATCH(content) AGAINST('{safe}' IN BOOLEAN MODE) \
             ORDER BY ft_score DESC, memory_id ASC LIMIT {limit}",
            sanitize_sql_literal(user_id),
        );
        // MatrixOne 4.1.2 can return no rows when re-executing a prepared MATCH
        // query. Use the text protocol, as in vector search, with escaped scope
        // literals and an already-sanitized query. Hydration still uses binds.
        fulltext_rows_or_empty(sqlx::raw_sql(&sql).fetch_all(&self.pool).await)
    }

    /// Pure MatrixOne full-text search with exact structured SQL pre-filters.
    /// This path performs no embedding, vector, graph, hybrid, temporal, or
    /// confidence scoring. `session_id` is strict and does not include unscoped
    /// rows. Metadata equality follows JSON type-family semantics: number `2`
    /// may equal `2.0`, but does not equal string `"2"`.
    #[allow(clippy::too_many_arguments)]
    pub async fn search_fulltext_structured_lite(
        &self,
        table: &str,
        user_id: &str,
        query: &str,
        limit: i64,
        memory_types: Option<&[MemoryType]>,
        session_id: Option<&str>,
        trust_tier: Option<&str>,
        subject_id: Option<&str>,
        extra_metadata_filter: &std::collections::HashMap<String, serde_json::Value>,
    ) -> Result<Vec<Memory>, MemoriaError> {
        if !(1..=FULLTEXT_SEARCH_MAX_LIMIT).contains(&limit) {
            return Err(MemoriaError::Validation(
                format!(
                    "fulltext search storage limit must be between 1 and {FULLTEXT_SEARCH_MAX_LIMIT}"
                ),
            ));
        }
        validate_fulltext_query(query)?;
        validate_extra_metadata_filter(extra_metadata_filter)?;

        let safe_query = sanitize_fulltext_query(query);
        let table = self.t(table);
        let mut metadata_filters: Vec<_> = extra_metadata_filter.iter().collect();
        metadata_filters.sort_by_key(|(key, _)| *key);

        let mut sql = format!(
            "SELECT memory_id, user_id, author_id, subject_id, memory_type, content, \
             session_id, is_active, superseded_by, trust_tier, \
             initial_confidence, observed_at, created_at, updated_at, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             MATCH(content) AGAINST('{safe_query}' IN BOOLEAN MODE) AS ft_score \
             FROM {table} WHERE user_id = ? AND is_active = 1"
        );
        if let Some(types) = memory_types.filter(|types| !types.is_empty()) {
            sql.push_str(" AND memory_type IN (");
            sql.push_str(&vec!["?"; types.len()].join(", "));
            sql.push(')');
        }
        if session_id.is_some() {
            sql.push_str(" AND session_id = ?");
        }
        if trust_tier.is_some() {
            sql.push_str(" AND trust_tier = ?");
        }
        if subject_id.is_some() {
            sql.push_str(" AND subject_id = ?");
        }
        for (key, _) in &metadata_filters {
            sql.push_str(&format!(
                " AND json_extract(extra_metadata, '$.{key}') = CAST(? AS JSON)"
            ));
        }
        sql.push_str(&format!(
            " AND MATCH(content) AGAINST('{safe_query}' IN BOOLEAN MODE) \
             ORDER BY ft_score DESC, memory_id DESC LIMIT ?"
        ));

        let mut statement = sqlx::query(&sql).bind(user_id);
        if let Some(types) = memory_types.filter(|types| !types.is_empty()) {
            for memory_type in types {
                statement = statement.bind(memory_type.to_string());
            }
        }
        if let Some(value) = session_id {
            statement = statement.bind(value);
        }
        if let Some(value) = trust_tier {
            statement = statement.bind(value);
        }
        if let Some(value) = subject_id {
            statement = statement.bind(value);
        }
        for (_, value) in metadata_filters {
            statement = statement.bind(serde_json::to_string(value)?);
        }

        let rows = fulltext_rows_or_empty(statement.bind(limit).fetch_all(&self.pool).await)?;
        rows.iter()
            .map(|row| {
                let mut memory = row_to_memory_lite(row)?;
                apply_fulltext_score(row, &mut memory);
                Ok(memory)
            })
            .collect()
    }

    pub async fn search_vector_from(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        self.search_vector_from_scoped(table, user_id, embedding, limit, None)
            .await
    }

    pub async fn search_vector_from_scoped(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        limit: i64,
        session_id: Option<&str>,
    ) -> Result<Vec<Memory>, MemoriaError> {
        self.search_vector_from_filtered_scoped(
            table, user_id, embedding, limit, None, session_id, None, None,
        )
        .await
    }

    /// Vector search with optional memory_type pre-filter to reduce scan set.
    pub async fn search_vector_from_filtered(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        limit: i64,
        memory_type: Option<&str>,
    ) -> Result<Vec<Memory>, MemoriaError> {
        self.search_vector_from_filtered_scoped(
            table, user_id, embedding, limit, memory_type, None, None, None,
        )
        .await
    }

    /// Vector search with optional memory_type, strict session, subject_id, and memory_types pre-filters.
    /// When session_id is provided, the candidate set includes that session plus
    /// unscoped memories (session_id IS NULL).
    #[allow(clippy::too_many_arguments)]
    pub async fn search_vector_from_filtered_scoped(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        limit: i64,
        memory_type: Option<&str>,
        session_id: Option<&str>,
        subject_id: Option<&str>,
        memory_types: Option<&[MemoryType]>,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let vec_literal = vec_to_mo(embedding);
        let type_clause = match memory_type {
            Some(mt) => format!(" AND memory_type = '{}'", sanitize_sql_literal(mt)),
            None => build_memory_types_in_clause_inline(memory_types),
        };
        let session_clause = match session_id {
            Some(session_id) => format!(
                " AND (session_id = '{}' OR session_id IS NULL)",
                sanitize_sql_literal(session_id)
            ),
            None => String::new(),
        };
        let subject_clause = match subject_id {
            Some(sid) => format!(" AND subject_id = '{}'", sanitize_sql_literal(sid)),
            None => String::new(),
        };
        let rank_mode = if session_id.is_some() { "pre" } else { "post" };
        let build_sql = |rank_mode: Option<&str>| {
            let limit_clause = match rank_mode {
                Some(mode) => format!("LIMIT {limit} by rank with option 'mode={mode}'"),
                None => format!("LIMIT {limit}"),
            };
            format!(
                "SELECT memory_id, user_id, author_id, subject_id, memory_type, content, \
                 session_id, \
                 CAST(source_event_ids AS CHAR) AS src_ids, \
                 CAST(extra_metadata AS CHAR) AS extra_meta, \
                 is_active, superseded_by, trust_tier, initial_confidence, \
                 observed_at, created_at, updated_at, \
                 l2_distance(embedding, '{vec_literal}') AS l2_dist \
                 FROM {table} \
                 WHERE user_id = '{}' AND is_active = 1 AND embedding IS NOT NULL{type_clause}{session_clause}{subject_clause} \
                 ORDER BY l2_distance(embedding, '{vec_literal}') ASC \
                 {limit_clause}",
                sanitize_sql_literal(user_id),
            )
        };
        // MatrixOne bug workaround: prepared statement with l2_distance in ORDER BY returns 0 rows
        // Solution: inline all parameters instead of using bind()
        let mut rows = sqlx::query(&build_sql(Some(rank_mode)))
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        // Strict session retrieval must preserve session-scoped semantics even if
        // MatrixOne's IVF pre-filter path under-fills top_k on a tiny candidate set.
        if session_id.is_some() && rows.len() < limit as usize {
            rows = sqlx::query(&build_sql(None))
                .fetch_all(&self.pool)
                .await
                .map_err(db_err)?;
        }

        rows.iter()
            .map(|r| {
                let mut m = row_to_memory(r)?;
                if let Ok(dist) = r
                    .try_get::<f64, _>("l2_dist")
                    .or_else(|_| r.try_get::<f32, _>("l2_dist").map(|v| v as f64))
                {
                    m.retrieval_score = Some(1.0 / (1.0 + dist.max(0.0)));
                }
                Ok(m)
            })
            .collect()
    }

    /// Hybrid search: vector + fulltext, merged with 4-dimension weighted scoring.
    /// Weights: vector=0.3, keyword=0.2, temporal=0.2, confidence=0.3 (matches Python "default")
    #[tracing::instrument(skip(self, embedding))]
    pub async fn search_hybrid_from(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        query: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        self.search_hybrid_from_scoped(table, user_id, embedding, query, limit, None)
            .await
    }

    pub async fn search_hybrid_from_scoped(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        query: &str,
        limit: i64,
        session_id: Option<&str>,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let params = self
            .get_user_retrieval_params(user_id)
            .await
            .unwrap_or_default();
        let (mems, _) = self
            .search_hybrid_from_scored_scoped(
                table,
                user_id,
                embedding,
                query,
                limit,
                params.feedback_weight,
                session_id,
                None,
                None,
            )
            .await?;
        Ok(mems)
    }

    /// Like search_hybrid_from but also returns per-candidate score breakdown.
    /// scores: (memory_id, vec_score, kw_score, time_score, conf_score, final_score)
    pub async fn search_hybrid_from_scored(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        query: &str,
        limit: i64,
        feedback_weight: f64,
    ) -> Result<(Vec<Memory>, Vec<(String, f64, f64, f64, f64, f64)>), MemoriaError> {
        self.search_hybrid_from_scored_scoped(
            table,
            user_id,
            embedding,
            query,
            limit,
            feedback_weight,
            None,
            None,
            None,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn search_hybrid_from_scored_scoped(
        &self,
        table: &str,
        user_id: &str,
        embedding: &[f32],
        query: &str,
        limit: i64,
        feedback_weight: f64,
        session_id: Option<&str>,
        subject_id: Option<&str>,
        memory_types: Option<&[MemoryType]>,
    ) -> Result<(Vec<Memory>, Vec<(String, f64, f64, f64, f64, f64)>), MemoriaError> {
        let fetch_k = (limit * 3).max(20);
        let (vec_results, ft_results) = tokio::join!(
            self.search_vector_from_filtered_scoped(
                table, user_id, embedding, fetch_k, None, session_id, subject_id, memory_types
            ),
            self.search_fulltext_from_scoped(
                table, user_id, query, fetch_k, session_id, subject_id, memory_types
            )
        );
        let vec_results = vec_results?;
        let ft_results = ft_results.unwrap_or_default();

        let (mut candidates, ft_map) = merge_hybrid_results(vec_results, ft_results);

        let now = chrono::Utc::now();
        const DECAY_HOURS: f64 = 168.0;
        const W_VEC: f64 = 0.3;
        const W_KW: f64 = 0.2;
        const W_TIME: f64 = 0.2;
        const W_CONF: f64 = 0.3;

        // Per-tier half-life (days) — higher-trust memories decay slower.
        fn half_life_for(tier: &memoria_core::TrustTier) -> f64 {
            tier.default_half_life_days()
        }

        let mut score_breakdown: Vec<(String, f64, f64, f64, f64, f64)> = Vec::new();

        // Fetch access counts + feedback in a single query
        let ac_ids: Vec<String> = candidates.iter().map(|m| m.memory_id.clone()).collect();
        let (ac_map, fb_map) = self.get_stats_batch(&ac_ids).await.unwrap_or_default();

        for m in &mut candidates {
            let vec_score = m.retrieval_score.unwrap_or(0.0);
            let raw_ft = ft_map.get(&m.memory_id).copied().unwrap_or(0.0);
            let kw_score = if raw_ft > 0.0 {
                raw_ft / (raw_ft + 1.0)
            } else {
                0.0
            };
            let (time_score, conf_score) = if let Some(obs) = m.observed_at {
                let age_hours = (now - obs).num_seconds() as f64 / 3600.0;
                let age_days = age_hours / 24.0;
                let ts = (-age_hours / DECAY_HOURS).max(-500.0).exp();
                let hl = half_life_for(&m.trust_tier);
                let cs = m.initial_confidence * (-age_days / hl).max(-500.0).exp();
                (ts, cs)
            } else {
                (0.0, m.initial_confidence)
            };
            let mut final_score =
                W_VEC * vec_score + W_KW * kw_score + W_TIME * time_score + W_CONF * conf_score;
            let ac = ac_map.get(&m.memory_id).copied().unwrap_or(0);
            // Keep access_count for observability, but exclude it from ranking.
            // Repeated evaluation otherwise creates self-reinforcing winners that
            // swamp fresher, more relevant memories.

            // Feedback adjustment: boost useful, penalize negative feedback
            if let Some(fb) = fb_map.get(&m.memory_id) {
                let positive = fb.useful as f64;
                let negative = (fb.irrelevant + fb.outdated + fb.wrong) as f64;
                // Net feedback score: positive boosts, negative penalizes
                // Formula: multiplier = 1 + feedback_weight * (useful - 0.5 * negative)
                let feedback_delta = positive - 0.5 * negative;
                if feedback_delta.abs() > 0.01 {
                    final_score *= (1.0 + feedback_weight * feedback_delta).clamp(0.5, 2.0);
                }
            }

            m.access_count = ac;
            m.retrieval_score = Some(final_score);
            score_breakdown.push((
                m.memory_id.clone(),
                vec_score,
                kw_score,
                time_score,
                conf_score,
                final_score,
            ));
        }

        // Drop memories whose effective confidence has decayed to near-zero.
        // This prevents long-expired facts from appearing in results.
        const MIN_CONF: f64 = 0.05;
        let live: std::collections::HashSet<String> = score_breakdown
            .iter()
            .filter(|(_, _, _, _, cs, _)| *cs >= MIN_CONF)
            .map(|(id, ..)| id.clone())
            .collect();
        candidates.retain(|m| live.contains(&m.memory_id));
        score_breakdown.retain(|(id, ..)| live.contains(id));

        candidates.sort_by(|a, b| {
            b.retrieval_score
                .partial_cmp(&a.retrieval_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        // Re-sort score_breakdown to match candidate order
        let order: std::collections::HashMap<String, usize> = candidates
            .iter()
            .enumerate()
            .map(|(i, m)| (m.memory_id.clone(), i))
            .collect();
        score_breakdown.sort_by_key(|(id, ..)| order.get(id).copied().unwrap_or(usize::MAX));
        candidates.truncate(limit as usize);
        score_breakdown.truncate(limit as usize);
        Ok((candidates, score_breakdown))
    }

    // ── Entity links ──────────────────────────────────────────────────────────

    // TODO(perf): When mem_entity_links grows large, add indexes:
    //   - (user_id, memory_id) for get_linked_memory_ids
    //   - (user_id, entity_name, entity_type) for get_entity_names

    /// Returns memory_ids that already have entity links for a user.
    pub async fn get_linked_memory_ids(
        &self,
        user_id: &str,
    ) -> Result<std::collections::HashSet<String>, MemoriaError> {
        let mut conn = self.conn().await?;
        let entity_links_table = self.t("mem_entity_links");
        let rows = sqlx::query(&format!(
            "SELECT DISTINCT memory_id FROM {entity_links_table} WHERE user_id = ?"
        ))
        .bind(user_id)
        .fetch_all(&mut *conn)
        .await
        .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| r.try_get::<String, _>("memory_id").ok())
            .collect())
    }

    /// Returns all entity names for a user (for existing_entities list).
    pub async fn get_entity_names(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, String)>, MemoriaError> {
        let mut conn = self.conn().await?;
        let entity_links_table = self.t("mem_entity_links");
        let rows = sqlx::query(&format!(
            "SELECT DISTINCT entity_name, entity_type FROM {entity_links_table} WHERE user_id = ? ORDER BY entity_name"
        ))
        .bind(user_id)
        .fetch_all(&mut *conn).await.map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let name = r.try_get::<String, _>("entity_name").ok()?;
                let etype = r.try_get::<String, _>("entity_type").ok()?;
                Some((name, etype))
            })
            .collect())
    }

    /// Insert entity links for a memory. Skips duplicates.
    pub async fn insert_entity_links(
        &self,
        user_id: &str,
        memory_id: &str,
        entities: &[(String, String)], // (name, type)
    ) -> Result<(usize, usize), MemoriaError> {
        let mut conn = self.conn().await?;
        let entity_links_table = self.t("mem_entity_links");
        if entities.is_empty() {
            return Ok((0, 0));
        }
        // Fetch existing entity names for this (user, memory) pair
        let existing: std::collections::HashSet<String> = {
            let rows = sqlx::query(&format!(
                "SELECT entity_name FROM {entity_links_table} WHERE user_id = ? AND memory_id = ?"
            ))
            .bind(user_id)
            .bind(memory_id)
            .fetch_all(&mut *conn)
            .await
            .map_err(db_err)?;
            rows.iter()
                .filter_map(|r| r.try_get::<String, _>("entity_name").ok())
                .collect()
        };
        // Partition into new vs reused, dedup by lowercased name within the batch
        let mut seen = std::collections::HashSet::new();
        let mut to_insert: Vec<(String, &str)> = Vec::new(); // (name_lc, entity_type)
        let mut reused = 0usize;
        for (name, etype) in entities {
            let name_lc = name.to_lowercase();
            if existing.contains(&name_lc) || !seen.insert(name_lc.clone()) {
                reused += 1;
                continue;
            }
            to_insert.push((name_lc, etype.as_str()));
        }
        if to_insert.is_empty() {
            return Ok((0, reused));
        }
        let now = chrono::Utc::now().naive_utc();
        for chunk in to_insert.chunks(50) {
            let placeholders = chunk
                .iter()
                .map(|_| "(?, ?, ?, ?, ?, 'manual', ?)")
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO {entity_links_table} \
                 (id, user_id, memory_id, entity_name, entity_type, source, created_at) \
                 VALUES {placeholders}"
            );
            let mut q = sqlx::query(&sql);
            for (name_lc, etype) in chunk {
                let id = uuid::Uuid::new_v4().to_string().replace('-', "");
                q = q
                    .bind(id)
                    .bind(user_id)
                    .bind(memory_id)
                    .bind(name_lc.as_str())
                    .bind(*etype)
                    .bind(now);
            }
            q.execute(&mut *conn).await.map_err(db_err)?;
        }
        Ok((to_insert.len(), reused))
    }
}

fn parse_db_name_from_url(database_url: &str) -> Option<&str> {
    split_database_url(database_url).map(|(_, db_name, _)| db_name)
}

fn configured_max_connections(env_name: &str, default: u32, upper: u32) -> u32 {
    std::env::var(env_name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
        .clamp(1, upper)
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

fn safety_snapshot_scope(db_name: &str) -> String {
    compact_identifier_fragment(db_name, SAFETY_SNAPSHOT_SCOPE_MAX_LEN)
}

fn safety_snapshot_prefix(db_name: Option<&str>) -> String {
    match db_name {
        Some(db_name) => format!("mem_snap_{}_pre_", safety_snapshot_scope(db_name)),
        None => "mem_snap_pre_".to_string(),
    }
}

fn legacy_safety_snapshot_prefix(db_name: Option<&str>) -> Option<String> {
    db_name.map(|db_name| format!("mem_snap_{db_name}_pre_"))
}

fn build_safety_snapshot_name(db_name: Option<&str>, operation: &str) -> String {
    let prefix = safety_snapshot_prefix(db_name);
    let operation = compact_identifier_fragment(operation, SAFETY_SNAPSHOT_OPERATION_MAX_LEN);
    let suffix = &uuid::Uuid::new_v4().simple().to_string()[..SAFETY_SNAPSHOT_UUID_LEN];
    let name = format!("{prefix}{operation}_{suffix}");
    debug_assert!(name.len() <= MAX_IDENTIFIER_LEN);
    name
}

#[cfg(test)]
mod tests {
    use super::{
        classify_pool_health, detect_connection_anomaly, merge_hybrid_results,
        should_emit_saturated_warning, validate_extra_metadata_filter, validate_fulltext_query,
        ConnectionAnomalyKind, OwnedEditLogEntry, PoolHealthLevel, PoolHealthSnapshot,
        SqlMemoryStore, FULLTEXT_QUERY_MAX_BYTES,
    };
    use memoria_core::{Memory, MemoryType, TrustTier};
    use sqlx::mysql::MySqlPoolOptions;
    use std::io::{self, Write};
    use std::sync::{Arc, Mutex, OnceLock};

    static LOG_TEST_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn fulltext_code_underline_does_not_poison_other_terms() {
        assert_eq!(
            super::sanitize_fulltext_query("ruff |_______________^ UP028"),
            "ruff UP028"
        );
        assert!(super::fulltext_query_batches("____ !@").is_empty());
    }

    #[test]
    fn long_fulltext_queries_preserve_all_distinct_terms_in_bounded_batches() {
        let words: Vec<_> = (0..3000).map(|i| format!("term{i}")).collect();
        let query = format!("{} TERM0 term2999", words.join(" "));
        let batches = super::fulltext_query_batches(&query);
        assert!(batches.iter().all(|b| b.split_whitespace().count() <= 64));
        assert_eq!(batches.join(" "), words.join(" "));
    }

    #[test]
    fn hybrid_merge_keeps_fulltext_scores_out_of_vector_score() {
        let scored_memory = |memory_id: &str, score: f64| Memory {
            memory_id: memory_id.to_string(),
            user_id: "u1".to_string(),
            author_id: None,
            subject_id: None,
            memory_type: MemoryType::Semantic,
            content: String::new(),
            initial_confidence: 1.0,
            embedding: None,
            source_event_ids: Vec::new(),
            superseded_by: None,
            is_active: true,
            access_count: 0,
            session_id: None,
            observed_at: None,
            created_at: None,
            updated_at: None,
            extra_metadata: None,
            trust_tier: TrustTier::T2Curated,
            retrieval_score: Some(score),
        };

        let (candidates, ft_map) = merge_hybrid_results(
            vec![scored_memory("V", 0.8), scored_memory("B", 0.6)],
            vec![scored_memory("B", 1.0), scored_memory("F", 2.0)],
        );

        assert_eq!(ft_map.get("B"), Some(&1.0));
        assert_eq!(ft_map.get("F"), Some(&2.0));
        assert_eq!(
            candidates
                .iter()
                .map(|m| (m.memory_id.as_str(), m.retrieval_score))
                .collect::<Vec<_>>(),
            vec![("V", Some(0.8)), ("B", Some(0.6)), ("F", None)]
        );
    }

    #[test]
    fn metadata_and_fulltext_validation_are_enforced_at_storage_boundary() {
        let valid = std::collections::HashMap::from([
            ("_scene".to_string(), serde_json::json!("incident")),
            ("rank2".to_string(), serde_json::json!(2)),
        ]);
        assert!(validate_extra_metadata_filter(&valid).is_ok());

        for invalid in [
            std::collections::HashMap::from([("1scene".to_string(), serde_json::json!(true))]),
            std::collections::HashMap::from([("scene".to_string(), serde_json::json!([1]))]),
            std::collections::HashMap::from([(
                "scene".to_string(),
                serde_json::json!("x".repeat(1025)),
            )]),
        ] {
            assert!(validate_extra_metadata_filter(&invalid).is_err());
        }

        let too_many = (0..17)
            .map(|index| (format!("key_{index}"), serde_json::json!(index)))
            .collect();
        assert!(validate_extra_metadata_filter(&too_many).is_err());
        assert!(validate_fulltext_query("MatrixOne database").is_ok());
        assert!(validate_fulltext_query(" !@#$ ").is_err());
        assert!(validate_fulltext_query(&"a".repeat(FULLTEXT_QUERY_MAX_BYTES)).is_ok());
        assert!(validate_fulltext_query(&"a".repeat(FULLTEXT_QUERY_MAX_BYTES + 1)).is_err());
        assert!(validate_fulltext_query(&"界".repeat(FULLTEXT_QUERY_MAX_BYTES / 3 + 1)).is_err());
    }

    #[test]
    fn saturated_warning_requires_full_delay_and_only_emits_once() {
        assert!(!should_emit_saturated_warning(299, false));
        assert!(should_emit_saturated_warning(300, false));
        assert!(!should_emit_saturated_warning(600, true));
    }

    #[test]
    fn saturated_warning_rearms_after_relief() {
        let mut snapshot = PoolHealthSnapshot::new(Some(20));
        snapshot.saturated_warning_emitted = true;
        snapshot.level = PoolHealthLevel::HighUtilization;
        snapshot.saturated_warning_emitted = false;

        assert!(should_emit_saturated_warning(
            300,
            snapshot.saturated_warning_emitted
        ));
    }

    #[derive(Clone)]
    struct SharedWriter(Arc<Mutex<Vec<u8>>>);

    impl Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn log_edit_warns_when_async_buffer_is_full() {
        let _guard = LOG_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("log test lock should not be poisoned");

        let pool = MySqlPoolOptions::new()
            .connect_lazy("mysql://root:111@localhost:6001/memoria")
            .expect("lazy pool");
        let store = SqlMemoryStore::new(pool, 4, "test-instance".to_string());
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        tx.try_send(OwnedEditLogEntry {
            edit_id: "existing".to_string(),
            user_id: "u1".to_string(),
            operation: "inject".to_string(),
            memory_id: Some("m1".to_string()),
            payload: None,
            reason: "prefill".to_string(),
            snapshot_before: None,
        })
        .expect("prefill channel");
        store.set_edit_log_tx(tx);

        let logs = Arc::new(Mutex::new(Vec::new()));
        let make_writer = {
            let logs = Arc::clone(&logs);
            move || SharedWriter(Arc::clone(&logs))
        };
        let subscriber = tracing_subscriber::fmt()
            .with_writer(make_writer)
            .with_ansi(false)
            .without_time()
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);

        store
            .log_edit("u1", "purge", Some("m2"), None, "test", None)
            .await;

        let output = String::from_utf8(logs.lock().unwrap().clone()).expect("utf8 logs");
        assert!(
            output.contains("edit log async buffer full, dropping entry"),
            "expected warning in logs, got: {output}"
        );
        assert!(
            output.contains("operation=purge"),
            "expected operation field in logs: {output}"
        );
    }

    /// Verify that log_edit falls back to direct INSERT when the async channel is closed
    /// (simulates the race between clear_edit_log_tx and a concurrent log_edit call
    /// that already cloned the sender before it was cleared).
    #[tokio::test(flavor = "current_thread")]
    async fn log_edit_falls_back_on_closed_channel() {
        let _guard = LOG_TEST_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("log test lock should not be poisoned");

        let pool = MySqlPoolOptions::new()
            .connect_lazy("mysql://root:111@localhost:6001/memoria")
            .expect("lazy pool");
        let store = SqlMemoryStore::new(pool, 4, "test-instance".to_string());
        // Create a channel and immediately drop the receiver → sender is closed
        let (tx, _rx) = tokio::sync::mpsc::channel(16);
        drop(_rx);
        store.set_edit_log_tx(tx);

        // log_edit should NOT warn about "dropping entry" — it should fall through
        // to direct INSERT (which will fail on lazy pool, but that's fine for this test)
        let logs = Arc::new(Mutex::new(Vec::new()));
        let make_writer = {
            let logs = Arc::clone(&logs);
            move || SharedWriter(Arc::clone(&logs))
        };
        let subscriber = tracing_subscriber::fmt()
            .with_writer(make_writer)
            .with_ansi(false)
            .without_time()
            .finish();
        let _subscriber = tracing::subscriber::set_default(subscriber);

        store
            .log_edit("u1", "purge", Some("m1"), None, "closed-test", None)
            .await;

        let output = String::from_utf8(logs.lock().unwrap().clone()).expect("utf8 logs");
        assert!(
            !output.contains("dropping entry"),
            "closed channel should fall back to direct INSERT, not drop: {output}"
        );
    }

    #[test]
    fn classify_pool_health_reserves_saturated_for_maxed_pools() {
        assert_eq!(classify_pool_health(0, 0, Some(8)), PoolHealthLevel::Empty);
        assert_eq!(
            classify_pool_health(2, 0, Some(4)),
            PoolHealthLevel::HighUtilization
        );
        assert_eq!(
            classify_pool_health(4, 0, Some(4)),
            PoolHealthLevel::Saturated
        );
        assert_eq!(
            classify_pool_health(20, 1, Some(20)),
            PoolHealthLevel::HighUtilization
        );
        assert_eq!(
            classify_pool_health(8, 3, Some(8)),
            PoolHealthLevel::Healthy
        );
        assert_eq!(
            classify_pool_health(8, 0, None),
            PoolHealthLevel::HighUtilization
        );
    }

    #[test]
    fn detect_connection_anomaly_catches_pool_timeout() {
        assert_eq!(
            detect_connection_anomaly(&sqlx::Error::PoolTimedOut),
            Some(ConnectionAnomalyKind::PoolTimedOut)
        );
    }
}

#[async_trait]
impl MemoryStore for SqlMemoryStore {
    async fn insert(&self, memory: &Memory) -> Result<(), MemoriaError> {
        let table = self.t("mem_memories");
        self.insert_into(&table, memory).await
    }

    async fn get(&self, memory_id: &str) -> Result<Option<Memory>, MemoriaError> {
        let table = self.t("mem_memories");
        let row = sqlx::query(&format!(
            "SELECT memory_id, user_id, author_id, subject_id, memory_type, content, \
             embedding AS emb_str, session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at \
             FROM {table} WHERE memory_id = ? AND is_active = 1"
        ))
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row_to_memory(&r)).transpose()
    }

    /// Reads inactive history from main (`mem_memories`) only. For branch-scoped
    /// observe exclusions, use `observe_exclusion_content_from` with that table.
    async fn get_including_inactive(&self, memory_id: &str) -> Result<Option<Memory>, MemoriaError> {
        let table = self.t("mem_memories");
        let row = sqlx::query(&format!(
            "SELECT memory_id, user_id, author_id, subject_id, memory_type, content, \
             embedding AS emb_str, session_id, \
             CAST(source_event_ids AS CHAR) AS src_ids, \
             CAST(extra_metadata AS CHAR) AS extra_meta, \
             is_active, superseded_by, trust_tier, initial_confidence, \
             observed_at, created_at, updated_at \
             FROM {table} WHERE memory_id = ?"
        ))
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        row.map(|r| row_to_memory(&r)).transpose()
    }

    async fn update(&self, memory: &Memory) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        let table = self.t("mem_memories");
        // Workaround: MO#23859 — PREPARE/EXECUTE corrupts NULL JSON on 2nd+ execution.
        let extra_metadata = memory
            .extra_metadata
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
            .unwrap_or_else(|| "{}".to_string());
        sqlx::query(&format!(
            "UPDATE {table} \
             SET content = ?, memory_type = ?, trust_tier = ?, \
                 initial_confidence = ?, extra_metadata = ?, \
                 superseded_by = ?, updated_at = ? \
             WHERE memory_id = ?"
        ))
        .bind(&memory.content)
        .bind(memory.memory_type.to_string())
        .bind(memory.trust_tier.to_string())
        .bind(memory.initial_confidence as f32)
        .bind(extra_metadata)
        .bind(nullable_str(&memory.superseded_by))
        .bind(now)
        .bind(&memory.memory_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn soft_delete(&self, memory_id: &str) -> Result<(), MemoriaError> {
        let now = Utc::now().naive_utc();
        let table = self.t("mem_memories");
        sqlx::query(&format!(
            "UPDATE {table} SET is_active = 0, updated_at = ? WHERE memory_id = ?"
        ))
        .bind(now)
        .bind(memory_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn list_active(&self, user_id: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        let table = self.t("mem_memories");
        self.list_active_from(&table, user_id, limit).await
    }

    async fn search_fulltext(
        &self,
        user_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let table = self.t("mem_memories");
        self.search_fulltext_from(&table, user_id, query, limit)
            .await
    }

    async fn search_vector(
        &self,
        user_id: &str,
        embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        let table = self.t("mem_memories");
        self.search_vector_from(&table, user_id, embedding, limit)
            .await
    }
}

/// Shared base fields for both full and lite row mappers.
fn row_to_memory_base(row: &sqlx::mysql::MySqlRow) -> Result<Memory, MemoriaError> {
    let memory_type_str: String = row.try_get("memory_type").map_err(db_err)?;
    let trust_tier_str: String = row.try_get("trust_tier").map_err(db_err)?;
    let observed_at = row
        .try_get::<chrono::NaiveDateTime, _>("observed_at")
        .ok()
        .map(|dt| dt.and_utc());
    let created_at = row
        .try_get::<chrono::NaiveDateTime, _>("created_at")
        .ok()
        .map(|dt| dt.and_utc());
    let updated_at = row
        .try_get::<chrono::NaiveDateTime, _>("updated_at")
        .ok()
        .map(|dt| dt.and_utc());

    Ok(Memory {
        memory_id: row.try_get("memory_id").map_err(db_err)?,
        user_id: row.try_get("user_id").map_err(db_err)?,
        author_id: row
            .try_get::<Option<String>, _>("author_id")
            .unwrap_or(None),
        subject_id: row
            .try_get::<Option<String>, _>("subject_id")
            .unwrap_or(None),
        memory_type: MemoryType::from_str(&memory_type_str)?,
        content: row.try_get("content").map_err(db_err)?,
        initial_confidence: row
            .try_get::<f32, _>("initial_confidence")
            .map_err(db_err)? as f64,
        embedding: None,
        source_event_ids: Vec::new(),
        superseded_by: nullable_str_from_row(row.try_get("superseded_by").map_err(db_err)?),
        is_active: {
            let v: i8 = row.try_get("is_active").map_err(db_err)?;
            v != 0
        },
        access_count: 0,
        session_id: nullable_str_from_row(row.try_get("session_id").map_err(db_err)?),
        observed_at,
        created_at,
        updated_at,
        extra_metadata: None,
        trust_tier: TrustTier::from_str(&trust_tier_str)?,
        retrieval_score: None,
    })
}

fn row_to_memory(row: &sqlx::mysql::MySqlRow) -> Result<Memory, MemoriaError> {
    let mut m = row_to_memory_base(row)?;

    m.source_event_ids = {
        let s: String = row.try_get("src_ids").map_err(db_err)?;
        serde_json::from_str(&s)?
    };
    m.extra_metadata = {
        let s: Option<String> = row.try_get("extra_meta").map_err(db_err)?;
        // Workaround: MO#23859 — we store "{}" instead of NULL; treat empty object as None.
        s.filter(|v| v != "{}")
            .map(|v| serde_json::from_str(&v))
            .transpose()?
    };
    m.embedding = {
        // Try emb_str first (for compatibility with old queries that use CAST)
        if let Ok(Some(s)) = row.try_get::<Option<String>, _>("emb_str") {
            Some(mo_to_vec(&s)?)
        } else if let Ok(Some(s)) = row.try_get::<Option<String>, _>("embedding") {
            // Direct embedding column (MatrixOne returns vector as string)
            Some(mo_to_vec(&s)?)
        } else {
            // No embedding column in result set (e.g., vector search queries)
            None
        }
    };
    Ok(m)
}

/// Lightweight row mapper — skips embedding and source_event_ids, but DOES read
/// extra_metadata (small JSON; needed by list callers for scene/agent display).
/// Requires the query to SELECT `CAST(extra_metadata AS CHAR) AS extra_meta`.
fn row_to_memory_lite(row: &sqlx::mysql::MySqlRow) -> Result<Memory, MemoriaError> {
    let mut m = row_to_memory_base(row)?;
    m.extra_metadata = {
        let s: Option<String> = row.try_get("extra_meta").map_err(db_err)?;
        // MO#23859: we store "{}" instead of NULL; treat empty object as None.
        s.filter(|v| v != "{}")
            .map(|v| serde_json::from_str(&v))
            .transpose()?
    };
    Ok(m)
}
