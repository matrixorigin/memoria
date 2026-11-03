//! User SQL runs only on a separate, view-only identity. Administrative DDL is
//! fixed server code; the original memory pool never receives the submitted SQL.
use super::{valid_identifier, MemorySqlError, PreparedMemoryQuery};
use crate::SqlMemoryStore;
use chrono::NaiveDateTime;
use futures::TryStreamExt;
use hmac::{Hmac, Mac};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{
    mysql::MySqlConnectOptions, Column, ConnectOptions, Connection, Executor, MySqlConnection, Row,
    TypeInfo, ValueRef,
};
use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{oneshot, Mutex, Semaphore};
use tracing::Instrument;

const MAX_RESULT_BYTES: usize = 1 << 20;
const QUERY_TIMEOUT: Duration = Duration::from_secs(5);

fn database_failure(phase: &'static str, error: sqlx::Error) -> MemorySqlError {
    let code = error.as_database_error().and_then(|error| error.code());
    let number = error
        .as_database_error()
        .and_then(|error| error.try_downcast_ref::<sqlx::mysql::MySqlDatabaseError>())
        .map(|error| error.number());
    tracing::warn!(
        phase,
        database_code = code.as_deref().unwrap_or("transport"),
        database_number = number,
        "memory SQL database operation failed"
    );
    MemorySqlError::Unavailable
}

#[derive(Serialize)]
pub struct MemorySqlResult {
    pub query_id: String,
    pub fingerprint: String,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

pub struct MemorySqlGateway {
    seed: Vec<u8>,
    ready: Mutex<HashSet<String>>,
    permits: Arc<Semaphore>,
    healthy: AtomicBool,
}

impl MemorySqlGateway {
    pub fn new(seed: Vec<u8>) -> Result<Arc<Self>, MemorySqlError> {
        if seed.len() < 32 || seed.len() > 1024 {
            return Err(MemorySqlError::Invalid);
        }
        Ok(Arc::new(Self {
            seed,
            ready: Mutex::new(HashSet::new()),
            permits: Arc::new(Semaphore::new(4)),
            healthy: AtomicBool::new(true),
        }))
    }

    pub fn validate_parameters(
        plan: &PreparedMemoryQuery,
        parameters: &[Value],
    ) -> Result<(), MemorySqlError> {
        let bound = plan.bound_parameters(parameters)?;
        if parameters.len() != plan.parameter_count
            || parameters.len() > 32
            || serde_json::to_vec(&bound)
                .map_err(|_| MemorySqlError::Invalid)?
                .len()
                > 128 * 1024
            || parameters.iter().any(|value| {
                !matches!(
                    value,
                    Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_)
                )
            })
        {
            return Err(MemorySqlError::Invalid);
        }
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    pub async fn execute(
        self: &Arc<Self>,
        store: Arc<SqlMemoryStore>,
        scope: String,
        plan: PreparedMemoryQuery,
        parameters: Vec<Value>,
    ) -> Result<MemorySqlResult, MemorySqlError> {
        Self::validate_parameters(&plan, &parameters)?;
        if !self.healthy.load(Ordering::Acquire) {
            return Err(MemorySqlError::Unavailable);
        }
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| MemorySqlError::Busy)?;
        let gateway = self.clone();
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let (result_tx, result_rx) = oneshot::channel();
        // The bounded task owns its permit until the DB query is drained and
        // its connection closed, including when an HTTP caller disconnects.
        tokio::spawn(
            async move {
                let _permit = permit;
                let started = std::time::Instant::now();
                let query_id = uuid::Uuid::new_v4().simple().to_string();
                let fingerprint = plan.fingerprint.clone();
                let scope_hash = format!("{:x}", Sha256::digest(scope.as_bytes()));
                let result = gateway
                    .run(store, &scope, plan, parameters, cancel_rx, &query_id)
                    .await;
                tracing::info!(
                    query_id,
                    fingerprint,
                    scope_hash,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    outcome = ?result.as_ref().err(),
                    "memory SQL query completed"
                );
                // A dropped receiver is the explicit caller-cancelled terminal state.
                let _ = result_tx.send(result);
            }
            .in_current_span(),
        );
        let result = result_rx.await.map_err(|_| MemorySqlError::Unavailable)?;
        drop(cancel_tx);
        result
    }

    async fn reader_options(
        &self,
        store: &SqlMemoryStore,
        scope: &str,
    ) -> Result<(MySqlConnectOptions, String), MemorySqlError> {
        if scope.is_empty()
            || scope.len() > 128
            || !scope
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return Err(MemorySqlError::Forbidden);
        }
        let admin_options = store.pool().connect_options();
        let database = store
            .database_name()
            .or(admin_options.get_database())
            .ok_or(MemorySqlError::Unavailable)?;
        if !valid_identifier(database) {
            return Err(MemorySqlError::Unavailable);
        }
        let digest = format!(
            "{:x}",
            Sha256::digest(format!("{database}\0{scope}").as_bytes())
        );
        let user = format!("memq_{}", &digest[..32]);
        let role = format!("memqr_{}", &digest[..32]);
        let view_name = format!("memoria_sql_{}", &digest[..32]);
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&self.seed).map_err(|_| MemorySqlError::Invalid)?;
        mac.update(b"memoria.sql.reader.v1\0");
        mac.update(digest.as_bytes());
        let password = format!("{:x}", mac.finalize().into_bytes());
        // Only explicitly used SQL scopes are prepared. No startup scan of all
        // users/workspaces, no per-scope connection pool, at most four requests.
        let mut ready = self.ready.lock().await;
        if !ready.contains(&digest) {
            if ready.len() >= 1024 {
                return Err(MemorySqlError::Busy);
            }
            let view = format!("`{database}`.`{view_name}`");
            // Scope comes from authentication, not the SQL WHERE clause. Only
            // formal columns are published; internal idempotency metadata is removed.
            let definition = format!("CREATE OR REPLACE VIEW {view} AS SELECT memory_id, subject_id, content, memory_type, trust_tier, initial_confidence, session_id, observed_at, created_at, JSON_REMOVE(COALESCE(extra_metadata, JSON_OBJECT()), '$._memoria_store_request_sha256') AS extra_metadata, embedding FROM {} WHERE user_id = '{scope}' AND is_active = 1", store.t("mem_memories"));
            // Preparation may contain a derived database password. SQLx
            // statement logging must be disabled even on the administrative
            // preparation connection, not just on the read-only data connection.
            let options = admin_options.as_ref().clone().disable_statement_logging();
            let mut admin = MySqlConnection::connect_with(&options)
                .await
                .map_err(|e| database_failure("prepare_connect", e))?;
            let prepared = async {
              for (step, ddl) in [definition,
                format!("CREATE ROLE IF NOT EXISTS `{role}`"),
                format!("CREATE USER IF NOT EXISTS `{user}` IDENTIFIED BY '{password}' DEFAULT ROLE `{role}`"),
                format!("GRANT `{role}` TO `{user}`"),
                format!("GRANT CONNECT ON ACCOUNT * TO `{role}`"),
                format!("GRANT SELECT ON VIEW {view} TO `{role}`"),
            ].into_iter().enumerate() {
                // MatrixOne's privilege DDL is not supported by COM_STMT_PREPARE.
                // These fixed server statements contain only validated server
                // identities; client SELECTs below still use bound parameters.
                admin.execute(ddl.as_str()).await.map_err(|e| {
                    tracing::warn!(step, "memory SQL preparation step failed");
                    database_failure("prepare_ddl", e)
                })?;
              }
              Ok::<(), MemorySqlError>(())
            }.await;
            close_bounded(admin).await?;
            prepared?;
            ready.insert(digest);
        }
        let username = match admin_options.get_username().split_once(':') {
            Some((account, _)) => format!("{account}:{user}:{role}"),
            None => user,
        };
        let mut options = admin_options
            .as_ref()
            .clone()
            .username(&username)
            .password(&password)
            .database(database)
            .statement_cache_capacity(0);
        options = options.disable_statement_logging();
        Ok((options, view_name))
    }

    async fn run(
        &self,
        store: Arc<SqlMemoryStore>,
        scope: &str,
        plan: PreparedMemoryQuery,
        parameters: Vec<Value>,
        mut cancel: oneshot::Receiver<()>,
        query_id: &str,
    ) -> Result<MemorySqlResult, MemorySqlError> {
        let parameters = plan.bound_parameters(&parameters)?;
        let (options, view_name) =
            tokio::time::timeout(QUERY_TIMEOUT, self.reader_options(&store, scope))
                .await
                .map_err(|_| MemorySqlError::Timeout)??;
        if matches!(
            cancel.try_recv(),
            Err(oneshot::error::TryRecvError::Closed) | Ok(())
        ) {
            return Err(MemorySqlError::Timeout);
        }
        let database = options.get_database().ok_or(MemorySqlError::Unavailable)?;
        let sql = plan.sql_for_view(database, &view_name)?;
        let mut connection = tokio::time::timeout(
            Duration::from_secs(3),
            MySqlConnection::connect_with(&options),
        )
        .await
        .map_err(|_| MemorySqlError::Timeout)?
        .map_err(|e| database_failure("reader_connect", e))?;
        let id: (u64,) = tokio::time::timeout(
            Duration::from_secs(2),
            sqlx::query_as("SELECT CONNECTION_ID()").fetch_one(&mut connection),
        )
        .await
        .map_err(|_| MemorySqlError::Timeout)?
        .map_err(|_| MemorySqlError::Unavailable)?;
        let result = {
            let fetch = fetch_bounded(&mut connection, &sql, &parameters);
            tokio::pin!(fetch);
            tokio::select! {
                value = &mut fetch => value,
                _ = tokio::time::sleep(QUERY_TIMEOUT) => {
                    self.cancel_and_drain(&options, id.0, &mut fetch).await?;
                    Err(MemorySqlError::Timeout)
                },
                _ = &mut cancel => {
                    self.cancel_and_drain(&options, id.0, &mut fetch).await?;
                    Err(MemorySqlError::Timeout)
                },
            }
        };
        close_bounded(connection).await?;
        let (columns, rows) = result?;
        let result = MemorySqlResult {
            query_id: query_id.into(),
            fingerprint: plan.fingerprint,
            columns,
            rows,
        };
        if serde_json::to_vec(&result)
            .map_err(|_| MemorySqlError::Unavailable)?
            .len()
            > MAX_RESULT_BYTES
        {
            return Err(MemorySqlError::ResultLimit);
        }
        Ok(result)
    }

    async fn cancel_and_drain<F>(
        &self,
        options: &MySqlConnectOptions,
        id: u64,
        fetch: F,
    ) -> Result<(), MemorySqlError>
    where
        F: std::future::Future<Output = Result<(Vec<String>, Vec<Vec<Value>>), MemorySqlError>>,
    {
        let cancelled = async {
            let mut control = MySqlConnection::connect_with(options)
                .await
                .map_err(|_| MemorySqlError::Unavailable)?;
            let killed = sqlx::query(&format!("KILL QUERY {id}"))
                .execute(&mut control)
                .await;
            control
                .close()
                .await
                .map_err(|_| MemorySqlError::Unavailable)?;
            killed.map_err(|_| MemorySqlError::Unavailable)?;
            // Drain the interrupted protocol before closing. Never return an
            // unread connection to a pool (there is deliberately no query pool).
            let _terminal = fetch.await;
            Ok::<(), MemorySqlError>(())
        };
        match tokio::time::timeout(Duration::from_secs(3), cancelled).await {
            Ok(Ok(())) => Ok(()),
            _ => {
                // A cancellation whose DB terminal cannot be confirmed disables
                // subsequent SQL; ordinary memory APIs remain unaffected.
                self.healthy.store(false, Ordering::Release);
                Err(MemorySqlError::Unavailable)
            }
        }
    }
}

async fn fetch_bounded(
    connection: &mut MySqlConnection,
    sql: &str,
    parameters: &[Value],
) -> Result<(Vec<String>, Vec<Vec<Value>>), MemorySqlError> {
    // A zero-row result still has a schema. Read it from the same restricted
    // connection; never infer columns from the first data row.
    let description = connection
        .describe(sql)
        .await
        .map_err(|e| database_failure("describe", e))?;
    let columns: Vec<String> = description
        .columns()
        .iter()
        .map(|c| c.name().to_owned())
        .collect();
    if columns.len() > 32 {
        return Err(MemorySqlError::ResultLimit);
    }
    let mut query = sqlx::query(sql).persistent(false);
    for parameter in parameters {
        query = match parameter {
            Value::Null => query.bind(Option::<String>::None),
            Value::Bool(value) => query.bind(*value),
            Value::String(value) => query.bind(value),
            Value::Number(value) if value.is_i64() => {
                query.bind(value.as_i64().ok_or(MemorySqlError::Invalid)?)
            }
            Value::Number(value) if value.is_u64() => {
                query.bind(value.as_u64().ok_or(MemorySqlError::Invalid)?)
            }
            Value::Number(value) => query.bind(value.as_f64().ok_or(MemorySqlError::Invalid)?),
            _ => return Err(MemorySqlError::Invalid),
        };
    }
    let mut stream = query.fetch(&mut *connection);
    let mut rows = Vec::new();
    let mut bytes = 0;
    let mut terminal_error = None;
    while let Some(row) = stream
        .try_next()
        .await
        .map_err(|e| database_failure("execute", e))?
    {
        if terminal_error.is_some() {
            continue;
        }
        if rows.len() >= super::MEMORY_SQL_MAX_ROWS || row.columns().len() > 32 {
            terminal_error = Some(MemorySqlError::ResultLimit);
            continue;
        }
        let decoded = (|| {
            let mut values = Vec::with_capacity(row.columns().len());
            for (index, column) in row.columns().iter().enumerate() {
                let raw = row
                    .try_get_raw(index)
                    .map_err(|_| MemorySqlError::Unavailable)?;
                let value = if raw.is_null() {
                    Value::Null
                } else {
                    match column.type_info().name() {
                        "TINYINT" | "SMALLINT" | "INT" | "MEDIUMINT" | "BIGINT" => Value::from(
                            row.try_get::<i64, _>(index)
                                .map_err(|_| MemorySqlError::Unavailable)?,
                        ),
                        "TINYINT UNSIGNED" | "SMALLINT UNSIGNED" | "INT UNSIGNED"
                        | "MEDIUMINT UNSIGNED" | "BIGINT UNSIGNED" => Value::from(
                            row.try_get::<u64, _>(index)
                                .map_err(|_| MemorySqlError::Unavailable)?,
                        ),
                        "DOUBLE" | "FLOAT" => finite_number(
                            row.try_get::<f64, _>(index)
                                .map_err(|_| MemorySqlError::Unavailable)?,
                        )?,
                        "JSON" => {
                            row.try_get::<sqlx::types::Json<Value>, _>(index)
                                .map_err(|_| MemorySqlError::Unavailable)?
                                .0
                        }
                        "DATETIME" | "TIMESTAMP" => Value::String(
                            row.try_get::<NaiveDateTime, _>(index)
                                .map_err(|_| MemorySqlError::Unavailable)?
                                .format("%Y-%m-%dT%H:%M:%S%.fZ")
                                .to_string(),
                        ),
                        "CHAR" | "VARCHAR" | "TEXT" | "TINYTEXT" | "MEDIUMTEXT" | "LONGTEXT"
                        | "BLOB" | "VARBINARY" => Value::String(
                            row.try_get::<String, _>(index)
                                .map_err(|_| MemorySqlError::Unavailable)?,
                        ),
                        _ => return Err(MemorySqlError::Unsupported),
                    }
                };
                bytes += serde_json::to_vec(&value)
                    .map_err(|_| MemorySqlError::Unavailable)?
                    .len();
                if bytes > MAX_RESULT_BYTES {
                    return Err(MemorySqlError::ResultLimit);
                }
                values.push(value);
            }
            Ok(values)
        })();
        // Even rejected rows are drained to a terminal protocol state under the
        // same query deadline. Dropping an unread stream is not cancellation.
        match decoded {
            Ok(values) => rows.push(values),
            Err(error) => terminal_error = Some(error),
        }
    }
    match terminal_error {
        Some(error) => Err(error),
        None => Ok((columns, rows)),
    }
}

async fn close_bounded(connection: MySqlConnection) -> Result<(), MemorySqlError> {
    tokio::time::timeout(Duration::from_secs(2), connection.close())
        .await
        .map_err(|_| MemorySqlError::Timeout)?
        .map_err(|_| MemorySqlError::Unavailable)
}

fn finite_number(value: f64) -> Result<Value, MemorySqlError> {
    serde_json::Number::from_f64(value)
        .map(Value::Number)
        .ok_or(MemorySqlError::Unsupported)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_finite_results_are_not_silently_reported_as_null() {
        assert_eq!(finite_number(0.25), Ok(serde_json::json!(0.25)));
        for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(finite_number(value), Err(MemorySqlError::Unsupported));
        }
    }
}
