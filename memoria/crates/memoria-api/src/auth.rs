//! Bearer token auth extractor.
//! Validates Bearer token against master key OR API key (sk-... hashed lookup).
//! When authenticated via API key, user_id is resolved from the key's owner.
//!
//! `last_used_at` updates are batched: a background task flushes accumulated
//! key hashes every 5 seconds in a single UPDATE, avoiding per-request DB writes
//! that can exhaust the connection pool under load (see #62).

use axum::{
    extract::FromRequestParts,
    http::{request::Parts, StatusCode},
};
use memoria_service::MemoryService;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use sqlx::Row;
use std::collections::HashSet;
use std::sync::Mutex;
use subtle::ConstantTimeEq;
use tracing::warn;

use crate::state::{AppState, CachedApiKeyPrincipal};

pub const SCOPE_IDENTITY_READ: &str = "identity:read";
pub const SCOPE_MEMORY_READ: &str = "memory:read";
pub const SCOPE_MEMORY_WRITE: &str = "memory:write";
pub const SCOPE_KEYS_MANAGE: &str = "keys:manage";
pub const DEFAULT_API_KEY_SCOPES: &str = "identity:read,memory:read,memory:write,keys:manage";
const OWNER_SCOPED_MASTER_SCOPES: &[&str] =
    &[SCOPE_IDENTITY_READ, SCOPE_MEMORY_READ, SCOPE_MEMORY_WRITE];
const MAX_OWNER_SCOPED_USER_ID_LEN: usize = 64;

pub fn parse_scopes(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_string)
        .collect()
}

fn owner_scoped_master_scopes() -> Vec<String> {
    OWNER_SCOPED_MASTER_SCOPES
        .iter()
        .map(|scope| (*scope).to_string())
        .collect()
}

fn validate_owner_scoped_master_request(
    token: &str,
    master_key: &str,
    headers: &axum::http::HeaderMap,
) -> Result<String, (StatusCode, String)> {
    let master_match = !master_key.is_empty()
        && token.len() == master_key.len()
        && token.as_bytes().ct_eq(master_key.as_bytes()).into();
    if !master_match {
        return Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()));
    }

    let mut owner_values = headers.get_all("X-User-Id").iter();
    let owner = owner_values
        .next()
        .filter(|_| owner_values.next().is_none())
        .and_then(|value| value.to_str().ok())
        .filter(|value| {
            !value.is_empty()
                && value.trim() == *value
                && value.len() <= MAX_OWNER_SCOPED_USER_ID_LEN
                && !value.starts_with("grp_")
                && !value.chars().any(char::is_control)
        })
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                "Owner-scoped master authentication requires exactly one valid X-User-Id"
                    .to_string(),
            )
        })?;

    Ok(owner.to_string())
}

/// Agent labels are recorded only after the request's scope admission. MCP
/// defers this until its tool-specific admission, not merely bearer validation.
pub(crate) fn request_tool_name(headers: &axum::http::HeaderMap) -> Option<String> {
    headers
        .get("X-Memoria-Tool")
        .or_else(|| headers.get("X-Tool-Name"))
        .and_then(|v| v.to_str().ok())
        .filter(|v| !v.is_empty())
        .map(String::from)
}

fn required_scope_for_request(method: &axum::http::Method, path: &str) -> Option<&'static str> {
    let under = |prefix: &str| path == prefix || path.starts_with(&format!("{prefix}/"));
    if (path == "/auth/whoami" && method == axum::http::Method::GET)
        || (path == "/mcp" && method == axum::http::Method::POST)
    {
        // MCP additionally authorizes each tool in its handler.
        return Some(SCOPE_IDENTITY_READ);
    }
    if under("/auth/keys") || under("/v1/groups") {
        // Group administration can copy personal memories, grant access to
        // other accounts and delete databases. Memory scopes never grant it.
        return Some(SCOPE_KEYS_MANAGE);
    }
    let memory_path = [
        "/v1/memories",
        "/v1/profiles",
        "/v1/feedback",
        "/v1/retrieval-params",
        "/v1/governance",
        "/v1/consolidate",
        "/v1/reflect",
        "/v1/extract-entities",
        "/v1/entities",
        "/v1/snapshots",
        "/v1/branches",
        "/v1/tasks",
        "/v1/observe",
        "/v1/sessions",
        "/v1/pipeline",
        "/v1/tool-usage",
        "/v1/health",
    ]
    .iter()
    .any(|prefix| path == *prefix || path.starts_with(&format!("{prefix}/")));

    if !memory_path {
        return None;
    }
    if method == axum::http::Method::GET
        || method == axum::http::Method::HEAD
        || (method == axum::http::Method::POST
            && matches!(
                path,
                "/v1/memories/query"
                    | "/v1/memories/fulltext-search"
                    | "/v1/memories/retrieve"
                    | "/v1/memories/search"
            ))
    {
        Some(SCOPE_MEMORY_READ)
    } else {
        Some(SCOPE_MEMORY_WRITE)
    }
}

#[derive(Clone)]
pub struct AuthUser {
    pub user_id: String,
    /// Routing scope: equals `user_id` in personal mode, `group_id` (e.g. `grp_xxx`)
    /// in group mode.  Passed to service-layer methods to select the correct
    /// physical database via `DbRouter::scope_store()`.
    pub scope_id: String,
    pub group_id: Option<String>,
    pub is_master: bool,
    pub key_id: Option<String>,
    pub key_prefix: Option<String>,
    pub scopes: Vec<String>,
}

impl AuthUser {
    pub fn require_master(&self) -> Result<(), (StatusCode, String)> {
        if !self.is_master {
            Err((StatusCode::FORBIDDEN, "Master key required".to_string()))
        } else {
            Ok(())
        }
    }

    pub fn scope_id(&self) -> &str {
        &self.scope_id
    }

    pub fn is_group_scoped(&self) -> bool {
        self.group_id.is_some()
    }

    pub fn has_scope(&self, scope: &str) -> bool {
        self.is_master || self.scopes.iter().any(|granted| granted == scope)
    }

    pub fn require_scope(&self, scope: &str) -> Result<(), (StatusCode, String)> {
        if self.has_scope(scope) {
            Ok(())
        } else {
            Err((
                StatusCode::FORBIDDEN,
                format!("API key missing required scope: {scope}"),
            ))
        }
    }
}

async fn cached_or_db_principal(token: &str, state: &AppState) -> Option<CachedApiKeyPrincipal> {
    let key_hash = format!("{:x}", Sha256::digest(token.as_bytes()));
    if let Some(principal) = state.api_key_cache.get(&key_hash) {
        return Some(principal);
    }

    let pool = state
        .auth_pool
        .as_ref()
        .or_else(|| state.service.sql_store.as_ref().map(|s| s.pool()))?;

    let row = sqlx::query(
        "SELECT key_id, user_id, group_id, key_prefix, scopes, expires_at FROM mem_api_keys \
         WHERE key_hash = ? AND is_active = 1 \
         AND (expires_at IS NULL OR expires_at > NOW(6))",
    )
    .bind(&key_hash)
    .fetch_optional(pool)
    .await
    .map_err(|e| warn!("cached_or_db_principal: DB query failed: {e}"))
    .ok()??;

    let principal = CachedApiKeyPrincipal {
        key_id: row.try_get("key_id").ok()?,
        user_id: row.try_get("user_id").ok()?,
        group_id: row.try_get("group_id").ok().flatten(),
        key_prefix: row.try_get("key_prefix").ok()?,
        scopes: parse_scopes(&row.try_get::<String, _>("scopes").ok()?),
        expires_at: row.try_get("expires_at").ok()?,
    };
    state.api_key_cache.insert(key_hash, principal.clone());
    Some(principal)
}

pub(crate) async fn group_main_write_allowed_for_solo_owner(
    state: &AppState,
    group_id: &str,
    user_id: &str,
) -> bool {
    let Some(pool) = state
        .auth_pool
        .as_ref()
        .or_else(|| state.service.sql_store.as_ref().map(|s| s.pool()))
    else {
        return false;
    };

    let row = match sqlx::query(
        "SELECT g.owner_user_id, CAST(COUNT(m.user_id) AS SIGNED) AS active_member_count \
         FROM mem_groups g \
         JOIN mem_group_members m ON g.group_id = m.group_id AND m.is_active = 1 \
         WHERE g.group_id = ? AND g.status = 'active' \
         GROUP BY g.owner_user_id \
         LIMIT 1",
    )
    .bind(group_id)
    .fetch_optional(pool)
    .await
    {
        Ok(row) => row,
        Err(e) => {
            warn!("group_main_write_allowed_for_solo_owner: DB query failed: {e}");
            return false;
        }
    };

    let Some(row) = row else {
        return false;
    };

    let Ok(owner_user_id) = row.try_get::<String, _>("owner_user_id") else {
        return false;
    };
    let Ok(active_member_count) = row.try_get::<i64, _>("active_member_count") else {
        return false;
    };

    owner_user_id == user_id && active_member_count == 1
}

// ── Group main-write guard (middleware) ──────────────────────────────────────

/// Axum middleware that rejects write requests to `main` in group mode.
///
/// Applied as a route-layer on all memory-write endpoints so that individual
/// handlers never need to call `reject_group_main_writes` manually.
/// The token is resolved from the in-memory cache (no extra DB round-trip
/// for the auth lookup itself).
pub async fn group_main_write_guard(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::body::{to_bytes, Body};
    use axum::response::IntoResponse;

    // Extract bearer token from Authorization header
    let principal = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with("Bearer "))
        .map(|v| v[7..].to_string());

    if let Some(token) = principal {
        let master_match = !state.master_key.is_empty()
            && token.len() == state.master_key.len()
            && token.as_bytes().ct_eq(state.master_key.as_bytes()).into();
        if master_match {
            return next.run(req).await;
        }
        if let Some(p) = cached_or_db_principal(&token, &state)
            .await
            .filter(|p| p.group_id.is_some())
        {
            if let Err(rejection) =
                authorize_api_key_route(req.method(), req.uri().path(), &p.scopes)
            {
                return rejection.into_response();
            }
            let gid = p.group_id.as_ref().unwrap();
            // Set task-local so active_branch_name resolves per-member state
            let user_id = p.user_id.clone();
            // Check whether the user's active branch is `main`
            if let Ok(sql) = state.service.user_sql_store(gid).await {
                let branch = memoria_storage::ACTOR_USER_ID
                    .scope(user_id, sql.active_branch_name(gid))
                    .await;
                if let Ok(branch) = branch {
                    if branch == "main" {
                        if group_main_write_allowed_for_solo_owner(&state, gid, &p.user_id).await {
                            return next.run(req).await;
                        }
                        let path = req.uri().path();
                        let explicit_query_branch = req
                            .uri()
                            .query()
                            .and_then(|query| {
                                serde_urlencoded::from_str::<
                                    std::collections::HashMap<String, String>,
                                >(query)
                                .ok()
                            })
                            .and_then(|query| query.get("branch").cloned())
                            .map(|branch| {
                                let branch = branch.trim();
                                !branch.is_empty() && branch != "main"
                            })
                            .unwrap_or(false);
                        if req.method() == axum::http::Method::DELETE
                            && path.starts_with("/v1/memories/")
                            && !path.ends_with("/correct")
                            && explicit_query_branch
                        {
                            return next.run(req).await;
                        }
                        let branch_aware_body_route = matches!(
                            path,
                            "/v1/memories"
                                | "/v1/memories/batch"
                                | "/v1/memories/correct"
                                | "/v1/memories/purge"
                                | "/v1/observe"
                                | "/v1/observe/deduplicated"
                        ) || (path.starts_with("/v1/memories/")
                            && path.ends_with("/correct"));
                        if !branch_aware_body_route {
                            return (
                                StatusCode::FORBIDDEN,
                                "main is read-only in group mode; \
                                 create or checkout a branch, then use selective apply"
                                    .to_string(),
                            )
                                .into_response();
                        }
                        let (parts, body) = req.into_parts();
                        let bytes = match to_bytes(body, 8 * 1024 * 1024).await {
                            Ok(bytes) => bytes,
                            Err(e) => {
                                return (
                                    StatusCode::PAYLOAD_TOO_LARGE,
                                    format!("request body could not be buffered: {e}"),
                                )
                                    .into_response();
                            }
                        };
                        let explicit_non_main_branch =
                            serde_json::from_slice::<serde_json::Value>(&bytes)
                                .ok()
                                .and_then(|body| {
                                    body.get("branch")
                                        .and_then(|branch| branch.as_str())
                                        .map(str::to_string)
                                })
                                .map(|branch| {
                                    let branch = branch.trim();
                                    !branch.is_empty() && branch != "main"
                                })
                                .unwrap_or(false);
                        if explicit_non_main_branch {
                            let req = axum::http::Request::from_parts(parts, Body::from(bytes));
                            return next.run(req).await;
                        }
                        return (
                            StatusCode::FORBIDDEN,
                            "main is read-only in group mode; \
                             create or checkout a branch, then use selective apply"
                                .to_string(),
                        )
                            .into_response();
                    }
                }
            }
        }
    }

    next.run(req).await
}

/// Axum middleware that blocks native merge in group mode.
///
/// Applied only to the `/v1/branches/:name/merge` route.
pub async fn group_merge_guard(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    let token = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with("Bearer "))
        .map(|v| v[7..].to_string());

    if let Some(token) = token {
        let master_match = !state.master_key.is_empty()
            && token.len() == state.master_key.len()
            && token.as_bytes().ct_eq(state.master_key.as_bytes()).into();
        if !master_match
            && cached_or_db_principal(&token, &state)
                .await
                .and_then(|p| p.group_id)
                .is_some()
        {
            return (
                StatusCode::FORBIDDEN,
                "native branch merge is disabled in group mode; use selective apply instead"
                    .to_string(),
            )
                .into_response();
        }
    }

    next.run(req).await
}

// ── Actor scope middleware ───────────────────────────────────────────────────

/// Axum middleware that sets the `ACTOR_USER_ID` task-local for group-scoped
/// requests.  This lets the storage layer key per-user state (active branch)
/// on the real human user rather than the group scope ID.
pub async fn actor_scope_layer(
    axum::extract::State(state): axum::extract::State<AppState>,
    req: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let token = req
        .headers()
        .get("Authorization")
        .and_then(|v| v.to_str().ok())
        .filter(|v| v.starts_with("Bearer "))
        .map(|v| v[7..].to_string());

    if let Some(token) = token {
        let master_match = !state.master_key.is_empty()
            && token.len() == state.master_key.len()
            && token.as_bytes().ct_eq(state.master_key.as_bytes()).into();
        if master_match {
            return next.run(req).await;
        }
        if let Some(uid) = cached_or_db_principal(&token, &state)
            .await
            .and_then(|p| p.group_id.as_ref().map(|_| p.user_id))
        {
            return memoria_storage::ACTOR_USER_ID
                .scope(uid, next.run(req))
                .await;
        }
    }

    next.run(req).await
}

#[derive(Deserialize)]
struct UserQuery {
    user_id: Option<String>,
}

/// Batched `last_used_at` updater.
/// Collects key hashes in memory and flushes them in a single UPDATE periodically.
pub struct LastUsedBatcher {
    pending: Mutex<HashSet<String>>,
}

impl Default for LastUsedBatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl LastUsedBatcher {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashSet::new()),
        }
    }

    /// Enqueue a key hash for deferred `last_used_at` update. Lock-free hot path.
    pub fn mark_used(&self, key_hash: String) {
        if let Ok(mut set) = self.pending.lock() {
            set.insert(key_hash);
        }
    }

    /// Drain pending hashes and flush to DB in a single batched UPDATE.
    /// Called by the background flush task.
    pub async fn flush(&self, pool: &sqlx::MySqlPool) {
        let hashes: Vec<String> = {
            let mut set = match self.pending.lock() {
                Ok(s) => s,
                Err(_) => return,
            };
            if set.is_empty() {
                return;
            }
            set.drain().collect()
        };

        // Batch UPDATE with IN clause — single round-trip regardless of batch size.
        // Cap at 500 per flush to keep the query reasonable.
        for chunk in hashes.chunks(500) {
            let placeholders: String = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "UPDATE mem_api_keys SET last_used_at = NOW(6) WHERE key_hash IN ({placeholders})"
            );
            let mut query = sqlx::query(&sql);
            for h in chunk {
                query = query.bind(h);
            }
            if let Err(e) = query.execute(pool).await {
                warn!(
                    "last_used_at batch flush failed ({} keys): {e}",
                    chunk.len()
                );
            }
        }
    }
}

/// Spawn the background flush loop. Call once at server startup.
pub fn spawn_last_used_flusher(
    batcher: std::sync::Arc<LastUsedBatcher>,
    pool: sqlx::MySqlPool,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        interval.tick().await; // skip immediate
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.changed() => {
                    batcher.flush(&pool).await;
                    break;
                }
            }
            batcher.flush(&pool).await;
        }
        tracing::debug!("last_used flusher exiting");
    })
}

// ── Tool usage tracking ───────────────────────────────────────────────────────

use chrono::{DateTime, Utc};

type ToolUsageMap = std::collections::HashMap<(String, String), (DateTime<Utc>, bool)>;

/// In-memory cache of per-user tool access times, periodically flushed to DB.
/// On startup, rebuilt from `mem_tool_usage` so restarts don't lose data.
pub struct ToolUsageBatcher {
    /// (user_id, tool_name) → (last_used_at, dirty)
    entries: Mutex<ToolUsageMap>,
}

impl Default for ToolUsageBatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolUsageBatcher {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(std::collections::HashMap::new()),
        }
    }

    fn merge_rebuilt_entries(&self, rebuilt: ToolUsageMap) {
        use std::collections::hash_map::Entry;

        let Ok(mut map) = self.entries.lock() else {
            return;
        };
        for (key, (rebuilt_ts, _)) in rebuilt {
            match map.entry(key) {
                Entry::Vacant(entry) => {
                    entry.insert((rebuilt_ts, false));
                }
                Entry::Occupied(mut entry) => {
                    let (current_ts, dirty) = *entry.get();
                    if dirty || current_ts >= rebuilt_ts {
                        continue;
                    }
                    entry.insert((rebuilt_ts, false));
                }
            }
        }
    }

    /// Record a tool access. Cheap in-memory write.
    pub fn mark_used(&self, user_id: String, tool: String) {
        if let Ok(mut map) = self.entries.lock() {
            map.insert((user_id, tool), (Utc::now(), true));
        }
    }

    /// Query last access times for a user. Returns from memory, no DB hit.
    pub fn get_user_tool_usage(&self, user_id: &str) -> Vec<(String, DateTime<Utc>)> {
        let map = match self.entries.lock() {
            Ok(m) => m,
            Err(_) => return vec![],
        };
        map.iter()
            .filter(|((uid, _), _)| uid == user_id)
            .map(|((_, tool), (ts, _))| (tool.clone(), *ts))
            .collect()
    }

    /// Rebuild cache from DB. Call once at startup.
    pub async fn rebuild_from_db(&self, service: &MemoryService) {
        let mut rebuilt = std::collections::HashMap::new();
        let Some(sql) = service.sql_store.as_ref() else {
            return;
        };

        if let Some(router) = sql.db_router() {
            let user_ids = match router.list_active_users().await {
                Ok(user_ids) => user_ids,
                Err(e) => {
                    warn!("tool_usage rebuild failed to list users: {e}");
                    return;
                }
            };
            for user_id in user_ids {
                let user_store = match service.user_sql_store(&user_id).await {
                    Ok(user_store) => user_store,
                    Err(e) => {
                        warn!("tool_usage rebuild failed to route user {user_id}: {e}");
                        continue;
                    }
                };
                let tool_usage_table = user_store.t("mem_tool_usage");
                let rows = match sqlx::query(&format!(
                    "SELECT user_id, tool_name, last_used_at FROM {tool_usage_table}",
                ))
                .fetch_all(user_store.pool())
                .await
                {
                    Ok(rows) => rows,
                    Err(e) => {
                        warn!("tool_usage rebuild failed for user {user_id}: {e}");
                        continue;
                    }
                };
                for row in &rows {
                    let uid: String = row.get("user_id");
                    let tool: String = row.get("tool_name");
                    let ts: DateTime<Utc> = row.get("last_used_at");
                    rebuilt.insert((uid, tool), (ts, false));
                }
            }
        } else {
            let rows =
                match sqlx::query("SELECT user_id, tool_name, last_used_at FROM mem_tool_usage")
                    .fetch_all(sql.pool())
                    .await
                {
                    Ok(rows) => rows,
                    Err(e) => {
                        warn!("tool_usage rebuild failed: {e}");
                        return;
                    }
                };
            for row in &rows {
                let uid: String = row.get("user_id");
                let tool: String = row.get("tool_name");
                let ts: DateTime<Utc> = row.get("last_used_at");
                rebuilt.insert((uid, tool), (ts, false));
            }
        }

        self.merge_rebuilt_entries(rebuilt);
    }

    /// Load one user's persisted tool usage on demand without fan-out across all user DBs.
    pub async fn load_user_from_db(&self, service: &MemoryService, user_id: &str) {
        let Some(sql) = service.sql_store.as_ref() else {
            return;
        };
        let mut rebuilt = std::collections::HashMap::new();

        if sql.db_router().is_some() {
            let user_store = match service.user_sql_store(user_id).await {
                Ok(user_store) => user_store,
                Err(e) => {
                    warn!("tool_usage lazy load failed to route user {user_id}: {e}");
                    return;
                }
            };
            let tool_usage_table = user_store.t("mem_tool_usage");
            let rows = match sqlx::query(&format!(
                "SELECT user_id, tool_name, last_used_at FROM {tool_usage_table}"
            ))
            .fetch_all(user_store.pool())
            .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    warn!("tool_usage lazy load failed for user {user_id}: {e}");
                    return;
                }
            };
            for row in &rows {
                let uid: String = row.get("user_id");
                let tool: String = row.get("tool_name");
                let ts: DateTime<Utc> = row.get("last_used_at");
                rebuilt.insert((uid, tool), (ts, false));
            }
        } else {
            let rows = match sqlx::query(
                "SELECT user_id, tool_name, last_used_at FROM mem_tool_usage WHERE user_id = ?",
            )
            .bind(user_id)
            .fetch_all(sql.pool())
            .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    warn!("tool_usage lazy load failed for user {user_id}: {e}");
                    return;
                }
            };
            for row in &rows {
                let uid: String = row.get("user_id");
                let tool: String = row.get("tool_name");
                let ts: DateTime<Utc> = row.get("last_used_at");
                rebuilt.insert((uid, tool), (ts, false));
            }
        }

        self.merge_rebuilt_entries(rebuilt);
    }

    /// Flush dirty entries to DB.
    pub async fn flush(&self, service: &MemoryService) {
        let dirty: Vec<(String, String, DateTime<Utc>)> = {
            let map = match self.entries.lock() {
                Ok(m) => m,
                Err(_) => return,
            };
            map.iter()
                .filter(|(_, (_, d))| *d)
                .map(|((uid, tool), (ts, _))| (uid.clone(), tool.clone(), *ts))
                .collect()
        };
        if dirty.is_empty() {
            return;
        }

        let Some(sql) = service.sql_store.as_ref() else {
            return;
        };
        if let Some(_router) = sql.db_router() {
            let mut by_user: std::collections::HashMap<
                String,
                Vec<(String, String, DateTime<Utc>)>,
            > = std::collections::HashMap::new();
            for (uid, tool, ts) in &dirty {
                by_user
                    .entry(uid.clone())
                    .or_default()
                    .push((uid.clone(), tool.clone(), *ts));
            }
            for (user_id, entries) in by_user {
                let user_store = match service.user_sql_store(&user_id).await {
                    Ok(user_store) => user_store,
                    Err(e) => {
                        warn!("tool_usage flush failed to route user {user_id}: {e}");
                        return;
                    }
                };
                let table = user_store.t("mem_tool_usage");
                if let Err(e) = flush_tool_usage_chunked(user_store.pool(), &table, &entries).await
                {
                    warn!("tool_usage batch flush failed for user {user_id}: {e}");
                    return;
                }
            }
        } else if let Err(e) = flush_tool_usage_chunked(sql.pool(), "mem_tool_usage", &dirty).await
        {
            warn!(
                "tool_usage batch flush failed ({} entries): {e}",
                dirty.len()
            );
            return;
        }

        // Only clear dirty flags after all chunks succeed.
        if let Ok(mut map) = self.entries.lock() {
            for (uid, tool, _) in &dirty {
                if let Some((_, d)) = map.get_mut(&(uid.clone(), tool.clone())) {
                    *d = false;
                }
            }
        }
    }
}

async fn flush_tool_usage_chunked(
    pool: &sqlx::MySqlPool,
    table: &str,
    dirty: &[(String, String, DateTime<Utc>)],
) -> Result<(), sqlx::Error> {
    for chunk in dirty.chunks(500) {
        let placeholders: String = chunk
            .iter()
            .map(|_| "(?, ?, ?)")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "INSERT INTO {table} (user_id, tool_name, last_used_at) VALUES {placeholders} \
                 ON DUPLICATE KEY UPDATE last_used_at = VALUES(last_used_at)"
        );
        let mut query = sqlx::query(&sql);
        for (uid, tool, ts) in chunk {
            query = query.bind(uid).bind(tool).bind(ts);
        }
        query.execute(pool).await?;
    }
    Ok(())
}

/// Spawn the background tool-usage flush loop (10-minute interval).
pub fn spawn_tool_usage_flusher(
    batcher: std::sync::Arc<ToolUsageBatcher>,
    service: std::sync::Arc<MemoryService>,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10 * 60));
        interval.tick().await; // skip immediate
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.changed() => {
                    batcher.flush(&service).await;
                    break;
                }
            }
            batcher.flush(&service).await;
        }
        tracing::debug!("tool_usage flusher exiting");
    })
}

// ── API call log tracking ─────────────────────────────────────────────────────

/// Request-scoped context shared between the call-log middleware and the AuthUser
/// extractor.  The middleware inserts this into request extensions before calling
/// `next`; the extractor fills in the resolved `user_id` so the middleware can
/// record the call after the handler returns.
#[derive(Clone, Default)]
pub struct CallLogContext(pub std::sync::Arc<Mutex<Option<String>>>);

/// RPC-level outcome metadata for `/mcp` calls.
/// Kept separate from the HTTP status so the two observability dimensions
/// (transport health vs. business logic errors) don't pollute each other.
pub struct RpcMeta {
    /// false when the JSON-RPC dispatch returned an error result.
    pub success: bool,
    /// JSON-RPC error code (e.g. -32601) when success = false; None otherwise.
    pub error_code: Option<i32>,
    pub tool_success: Option<bool>,
    pub tool_error_kind: Option<&'static str>,
}

impl RpcMeta {
    pub fn ok() -> Self {
        Self {
            success: true,
            error_code: None,
            tool_success: None,
            tool_error_kind: None,
        }
    }
    pub fn err(code: i32) -> Self {
        Self {
            success: false,
            error_code: Some(code),
            tool_success: None,
            tool_error_kind: None,
        }
    }
}

/// A single pending call log entry buffered in memory.
struct CallLogEntry {
    user_id: String,
    method: String,
    path: String,
    status_code: u16,
    latency_ms: u32,
    /// Always true for /v1/* REST calls.
    /// For /mcp JSON-RPC calls: false when the dispatch returned an error result.
    rpc_success: bool,
    /// JSON-RPC error code (e.g. -32601) when rpc_success = false; NULL otherwise.
    rpc_error_code: Option<i32>,
    tool_success: Option<bool>,
    tool_error_kind: Option<&'static str>,
}

/// Accumulates call log entries in memory and flushes them in batches to DB.
pub struct CallLogBatcher {
    pending: Mutex<Vec<CallLogEntry>>,
}

impl Default for CallLogBatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl CallLogBatcher {
    #[cfg(test)]
    pub(crate) fn pending_rpc_outcomes(&self) -> Vec<(bool, Option<i32>)> {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .map(|entry| (entry.rpc_success, entry.rpc_error_code))
            .collect()
    }

    pub fn new() -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
        }
    }

    /// Enqueue a REST call log entry (`/v1/*`). RPC fields default to success.
    pub fn record(
        &self,
        user_id: String,
        method: String,
        path: String,
        status_code: u16,
        latency_ms: u32,
    ) {
        self.record_rpc(
            user_id,
            method,
            path,
            status_code,
            latency_ms,
            RpcMeta::ok(),
        );
    }

    /// Enqueue a call log entry with explicit JSON-RPC success/error metadata.
    /// Use this for `/mcp` calls so that HTTP status (always 200 for JSON-RPC)
    /// and business-level error tracking are kept separate.
    pub fn record_rpc(
        &self,
        user_id: String,
        method: String,
        path: String,
        status_code: u16,
        latency_ms: u32,
        rpc: RpcMeta,
    ) {
        if let Ok(mut v) = self.pending.lock() {
            v.push(CallLogEntry {
                user_id,
                method,
                path,
                status_code,
                latency_ms,
                rpc_success: rpc.success,
                rpc_error_code: rpc.error_code,
                tool_success: rpc.tool_success,
                tool_error_kind: rpc.tool_error_kind,
            });
        }
    }

    /// Drain pending entries and write them to `mem_api_call_log` in chunks.
    pub async fn flush(&self, service: &MemoryService) {
        let entries: Vec<CallLogEntry> = {
            let mut v = match self.pending.lock() {
                Ok(v) => v,
                Err(_) => return,
            };
            if v.is_empty() {
                return;
            }
            v.drain(..).collect()
        };

        let Some(sql) = service.sql_store.as_ref() else {
            return;
        };
        if let Some(_router) = sql.db_router() {
            let mut by_user: std::collections::HashMap<String, Vec<CallLogEntry>> =
                std::collections::HashMap::new();
            for entry in entries {
                by_user
                    .entry(entry.user_id.clone())
                    .or_default()
                    .push(entry);
            }
            let mut retry_entries = Vec::new();
            for (user_id, entries) in by_user {
                let user_store = match service.user_sql_store(&user_id).await {
                    Ok(user_store) => user_store,
                    Err(e) => {
                        warn!("call_log flush failed to route user {user_id}: {e}");
                        retry_entries.extend(entries);
                        continue;
                    }
                };
                if let Err(e) = user_store.ensure_call_log_tool_outcome_schema().await {
                    warn!("call_log schema repair failed for user {user_id}: {e}");
                    continue;
                }
                let table = user_store.t("mem_api_call_log");
                if let Err(e) = flush_call_log_chunked(user_store.pool(), &table, &entries).await {
                    warn!("call_log batch flush failed for user {user_id}: {e}");
                }
            }
            if !retry_entries.is_empty() {
                if let Ok(mut pending) = self.pending.lock() {
                    pending.extend(retry_entries);
                }
            }
            return;
        }
        if let Err(e) = sql.ensure_call_log_tool_outcome_schema().await {
            warn!("call_log schema repair failed: {e}");
            return;
        }
        if let Err(e) = flush_call_log_chunked(sql.pool(), "mem_api_call_log", &entries).await {
            warn!(
                "call_log batch flush failed ({} entries): {e}",
                entries.len()
            );
        }
    }
}

async fn flush_call_log_chunked(
    pool: &sqlx::MySqlPool,
    table: &str,
    entries: &[CallLogEntry],
) -> Result<(), sqlx::Error> {
    for chunk in entries.chunks(200) {
        let placeholders: String = chunk
            .iter()
            .map(|_| "(?, ?, ?, ?, ?, ?, ?, ?, ?)")
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "INSERT INTO {table} \
                 (user_id, method, path, status_code, latency_ms, rpc_success, rpc_error_code, tool_success, tool_error_kind) \
                 VALUES {placeholders}"
        );
        let mut query = sqlx::query(&sql);
        for e in chunk {
            query = query
                .bind(&e.user_id)
                .bind(&e.method)
                .bind(&e.path)
                .bind(e.status_code as i16)
                .bind(e.latency_ms as i32)
                .bind(e.rpc_success as i8)
                .bind(e.rpc_error_code)
                .bind(e.tool_success.map(i8::from))
                .bind(e.tool_error_kind);
        }
        query.execute(pool).await?;
    }
    Ok(())
}

/// Spawn the background call-log flush loop (5-second interval).
pub fn spawn_call_log_flusher(
    batcher: std::sync::Arc<CallLogBatcher>,
    service: std::sync::Arc<MemoryService>,
    mut shutdown: tokio::sync::watch::Receiver<()>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(5));
        interval.tick().await; // skip immediate first tick
        loop {
            tokio::select! {
                _ = interval.tick() => {}
                _ = shutdown.changed() => {
                    batcher.flush(&service).await;
                    break;
                }
            }
            batcher.flush(&service).await;
        }
        tracing::debug!("call_log flusher exiting");
    })
}

#[axum::async_trait]
impl FromRequestParts<AppState> for AuthUser {
    type Rejection = (StatusCode, String);

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        // Extract optional tool/agent name for usage tracking (skip empty values).
        // Agents send X-Memoria-Tool with their name: cursor / kiro / claude / codex / openclaw.
        // Fall back to X-Tool-Name for backwards compatibility with older clients.
        // Any non-empty value is accepted — no whitelist, so new agents work automatically.
        let tool_name = if parts.uri.path() == "/mcp" {
            None
        } else {
            request_tool_name(&parts.headers)
        };

        let authorization = parts
            .headers
            .get("Authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        let owner_scoped_master = authorization.strip_prefix("Memoria-Owner ");
        let bearer = authorization.strip_prefix("Bearer ");

        // A trusted proxy may authenticate with the deployment master secret
        // while explicitly attenuating it to one owner.  This is a distinct
        // scheme so older Memoria servers fail closed with 401 instead of
        // silently treating the request as an unrestricted Bearer master key.
        if let Some(token) = owner_scoped_master {
            let user_id = match validate_owner_scoped_master_request(
                token,
                &state.master_key,
                &parts.headers,
            ) {
                Ok(user_id) => user_id,
                Err(rejection) => {
                    if rejection.0 == StatusCode::UNAUTHORIZED {
                        crate::metrics::registry().security.auth_failures.inc();
                        warn!(
                            scheme = "Memoria-Owner",
                            path = %parts.uri.path(),
                            "auth: invalid token"
                        );
                    }
                    return Err(rejection);
                }
            };
            let scopes = owner_scoped_master_scopes();
            authorize_api_key_route(&parts.method, parts.uri.path(), &scopes)?;
            if let Some(tool) = tool_name {
                state.tool_usage_batcher.mark_used(user_id.clone(), tool);
            }
            if let Some(ctx) = parts.extensions.get::<CallLogContext>() {
                if let Ok(mut guard) = ctx.0.lock() {
                    *guard = Some(user_id.clone());
                }
            }
            return Ok(AuthUser {
                scope_id: user_id.clone(),
                group_id: None,
                user_id,
                is_master: false,
                key_id: None,
                key_prefix: None,
                scopes,
            });
        }

        if let Some(token) = bearer {
            // 1) Master key — full access, fall through to X-User-Id extraction
            let master_match = !state.master_key.is_empty()
                && token.len() == state.master_key.len()
                && token.as_bytes().ct_eq(state.master_key.as_bytes()).into();
            if master_match {
                // fall through
            }
            // 2) API key — user_id resolved from DB, never master
            else if let Some(principal) =
                validate_api_key(token, state, parts.uri.path() == "/auth/whoami").await
            {
                authorize_api_key_route(&parts.method, parts.uri.path(), &principal.scopes)?;
                let uid = principal.user_id.clone();
                let group_id = principal.group_id.clone();
                let memory_telemetry = principal
                    .scopes
                    .iter()
                    .any(|scope| matches!(scope.as_str(), SCOPE_MEMORY_READ | SCOPE_MEMORY_WRITE));
                if memory_telemetry {
                    if let Some(tool) = tool_name {
                        state.tool_usage_batcher.mark_used(uid.clone(), tool);
                    }
                    // Only admitted memory-capable requests may enqueue logs
                    // whose persistence can provision a personal memory DB.
                    if let Some(ctx) = parts.extensions.get::<CallLogContext>() {
                        if let Ok(mut guard) = ctx.0.lock() {
                            *guard = Some(uid.clone());
                        }
                    }
                }
                let scope_id = group_id.clone().unwrap_or_else(|| uid.clone());
                return Ok(AuthUser {
                    user_id: uid,
                    scope_id,
                    group_id,
                    is_master: false,
                    key_id: Some(principal.key_id),
                    key_prefix: Some(principal.key_prefix),
                    scopes: principal.scopes,
                });
            } else {
                crate::metrics::registry().security.auth_failures.inc();
                warn!(
                    token_prefix = &token[..token.len().min(8)],
                    "auth: invalid token"
                );
                return Err((StatusCode::UNAUTHORIZED, "Invalid token".to_string()));
            }
        } else if !state.master_key.is_empty() {
            // master_key is configured but caller sent no Bearer token
            crate::metrics::registry().security.auth_failures.inc();
            warn!("auth: missing Bearer token");
            return Err((StatusCode::UNAUTHORIZED, "Missing Bearer token".to_string()));
        }
        // Reached here: master key validated, or no-auth open mode (master_key not configured)

        let user_id = parts
            .headers
            .get("X-User-Id")
            .or_else(|| parts.headers.get("X-Impersonate-User"))
            .and_then(|v| v.to_str().ok())
            .map(String::from)
            .or_else(|| {
                let uri = parts.uri.query().unwrap_or("");
                serde_urlencoded::from_str::<UserQuery>(uri)
                    .ok()
                    .and_then(|q| q.user_id)
            })
            .unwrap_or_else(|| "default".to_string());

        if let Some(tool) = tool_name {
            state.tool_usage_batcher.mark_used(user_id.clone(), tool);
        }

        if let Some(ctx) = parts.extensions.get::<CallLogContext>() {
            if let Ok(mut guard) = ctx.0.lock() {
                *guard = Some(user_id.clone());
            }
        }

        Ok(AuthUser {
            scope_id: user_id.clone(),
            group_id: None,
            user_id,
            is_master: true,
            key_id: None,
            key_prefix: None,
            scopes: parse_scopes(DEFAULT_API_KEY_SCOPES),
        })
    }
}

/// Hash the raw API key and look it up in mem_api_keys.
/// Returns Some(user_id) if valid, None otherwise.
///
/// Uses a dedicated auth connection pool so that auth validation is never
/// blocked by slow business queries on the main pool.
/// `last_used_at` is updated via batched writes (see [`LastUsedBatcher`]).
async fn validate_api_key(
    token: &str,
    state: &AppState,
    fresh: bool,
) -> Option<CachedApiKeyPrincipal> {
    state.service.sql_store.as_ref()?;
    let key_hash = format!("{:x}", Sha256::digest(token.as_bytes()));

    // Rate limit check (before cache, to count all attempts)
    if !state.rate_limiter.allow(&key_hash).await {
        crate::metrics::registry().security.auth_failures.inc();
        return None;
    }

    // Check cache first — no DB hit at all.
    // Note: cached entries skip the membership check below.  This is acceptable
    // because (a) cache TTL is 5 min, and (b) remove_member / delete_group invalidate
    // the cache for revoked keys.  The DB-level check on cache miss is the
    // authoritative membership gate.
    // Login/refresh must observe revocations made through any API replica.
    // Bypass cache reads entirely: another in-flight request may repopulate
    // an old grant between invalidation and lookup.
    if fresh {
        state.api_key_cache.invalidate(&key_hash);
    } else if let Some(principal) = state.api_key_cache.get(&key_hash) {
        // Still enqueue last_used_at update (batched, no DB pressure)
        state.last_used_batcher.mark_used(key_hash);
        return Some(principal);
    }

    let Some(pool) = state.auth_pool.as_ref() else {
        warn!("validate_api_key: dedicated auth pool unavailable");
        return None;
    };

    let row = sqlx::query(
        "SELECT key_id, user_id, group_id, key_prefix, scopes, expires_at FROM mem_api_keys \
         WHERE key_hash = ? AND is_active = 1 \
         AND (expires_at IS NULL OR expires_at > NOW(6))",
    )
    .bind(&key_hash)
    .fetch_optional(pool)
    .await
    .map_err(|e| warn!("validate_api_key: DB query failed: {e}"))
    .ok()??;

    let user_id: String = row.try_get("user_id").ok()?;
    let group_id: Option<String> = row.try_get("group_id").ok().flatten();
    let principal = CachedApiKeyPrincipal {
        key_id: row.try_get("key_id").ok()?,
        user_id: user_id.clone(),
        group_id: group_id.clone(),
        key_prefix: row.try_get("key_prefix").ok()?,
        scopes: parse_scopes(&row.try_get::<String, _>("scopes").ok()?),
        expires_at: row.try_get("expires_at").ok()?,
    };

    // Enforce real-time group membership: even if the key references a group,
    // the user must still be an active member in `mem_group_members` and the
    // group must be active.  This prevents access after member removal (the
    // key-revocation path is eventually-consistent; this check is the
    // authoritative gate).
    if let Some(gid) = &group_id {
        let cnt: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mem_groups g \
             JOIN mem_group_members m ON g.group_id = m.group_id \
             WHERE g.group_id = ? AND g.status = 'active' \
             AND m.user_id = ? AND m.is_active = 1",
        )
        .bind(gid)
        .bind(&user_id)
        .fetch_one(pool)
        .await
        .map_err(|e| warn!("validate_api_key: membership check failed: {e}"))
        .ok()?;
        if cnt == 0 {
            warn!(
                user_id = %user_id,
                group_id = %gid,
                "auth: user not a member of group (or group inactive)"
            );
            return None;
        }
    }

    // Cache the result (TTL 5 min)
    state
        .api_key_cache
        .insert(key_hash.clone(), principal.clone());

    // Enqueue batched last_used_at update — zero DB pressure on hot path
    state.last_used_batcher.mark_used(key_hash);

    Some(principal)
}
/// Unknown authenticated routes are master-only until explicitly classified.
fn authorize_api_key_route(
    method: &axum::http::Method,
    path: &str,
    scopes: &[String],
) -> Result<(), (StatusCode, String)> {
    let required = required_scope_for_request(method, path).ok_or((
        StatusCode::FORBIDDEN,
        "API key access is not enabled for this route".to_string(),
    ))?;
    if scopes.iter().any(|scope| scope == required) {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            format!("API key missing required scope: {required}"),
        ))
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_memory_read_and_write_routes() {
        assert_eq!(
            required_scope_for_request(&axum::http::Method::POST, "/v1/observe/deduplicated"),
            Some(SCOPE_MEMORY_WRITE)
        );
        assert!(authorize_api_key_route(
            &axum::http::Method::POST,
            "/v1/observe/deduplicated",
            &parse_scopes("memory:read")
        ).is_err());
        assert_eq!(
            required_scope_for_request(&axum::http::Method::POST, "/v1/memories/retrieve"),
            Some(SCOPE_MEMORY_READ)
        );
        assert_eq!(
            required_scope_for_request(&axum::http::Method::GET, "/v1/memories/abc"),
            Some(SCOPE_MEMORY_READ)
        );
        assert_eq!(
            required_scope_for_request(&axum::http::Method::POST, "/v1/memories"),
            Some(SCOPE_MEMORY_WRITE)
        );
        assert_eq!(
            required_scope_for_request(&axum::http::Method::DELETE, "/v1/snapshots/one"),
            Some(SCOPE_MEMORY_WRITE)
        );
        assert_eq!(
            required_scope_for_request(&axum::http::Method::GET, "/auth/whoami"),
            Some(SCOPE_IDENTITY_READ)
        );
    }

    #[test]
    fn restricted_keys_cannot_administer_groups_or_use_unclassified_routes() {
        use axum::http::Method;
        for scopes in [
            "identity:read",
            "identity:read,memory:read",
            "identity:read,memory:read,memory:write",
        ] {
            let scopes = parse_scopes(scopes);
            for (method, path) in [
                (Method::GET, "/v1/groups"),
                (Method::POST, "/v1/groups"),
                (Method::POST, "/v1/groups/group/members/another-user"),
                (Method::DELETE, "/v1/groups/group"),
                (Method::DELETE, "/v1/groups/group/members/another-user"),
                (Method::GET, "/unclassified-sensitive-route"),
                (Method::POST, "/admin/users"),
            ] {
                assert_eq!(
                    authorize_api_key_route(&method, path, &scopes)
                        .unwrap_err()
                        .0,
                    StatusCode::FORBIDDEN
                );
            }
            assert!(authorize_api_key_route(&Method::GET, "/auth/whoami", &scopes).is_ok());
        }
        assert!(authorize_api_key_route(
            &Method::POST,
            "/v1/groups",
            &parse_scopes(DEFAULT_API_KEY_SCOPES)
        )
        .is_ok());
        assert!(authorize_api_key_route(
            &Method::GET,
            "/unclassified-sensitive-route",
            &parse_scopes(DEFAULT_API_KEY_SCOPES)
        )
        .is_err());
        assert!(authorize_api_key_route(
            &Method::GET,
            "/v1/health/analyze",
            &parse_scopes("identity:read")
        )
        .is_err());
    }

    fn owner_headers(values: &[&'static str]) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        for value in values {
            headers.append("X-User-Id", axum::http::HeaderValue::from_static(value));
        }
        headers
    }

    #[test]
    fn owner_scoped_master_rejects_invalid_secret() {
        let rejection = validate_owner_scoped_master_request(
            "attacker-secret",
            "expected-master",
            &owner_headers(&["alice"]),
        )
        .unwrap_err();
        assert_eq!(rejection.0, StatusCode::UNAUTHORIZED);

        let rejection =
            validate_owner_scoped_master_request("", "", &owner_headers(&["alice"])).unwrap_err();
        assert_eq!(rejection.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn owner_scoped_master_requires_exactly_one_owner_header() {
        let rejection =
            validate_owner_scoped_master_request("master", "master", &owner_headers(&[]))
                .unwrap_err();
        assert_eq!(rejection.0, StatusCode::BAD_REQUEST);

        let rejection = validate_owner_scoped_master_request(
            "master",
            "master",
            &owner_headers(&["victim", "authenticated-user"]),
        )
        .unwrap_err();
        assert_eq!(rejection.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn owner_scoped_master_enforces_storage_identity_width() {
        let max_owner = "a".repeat(MAX_OWNER_SCOPED_USER_ID_LEN);
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("X-User-Id", max_owner.parse().unwrap());
        assert_eq!(
            validate_owner_scoped_master_request("master", "master", &headers).unwrap(),
            max_owner
        );

        let oversized_owner = "a".repeat(MAX_OWNER_SCOPED_USER_ID_LEN + 1);
        headers.insert("X-User-Id", oversized_owner.parse().unwrap());
        let rejection =
            validate_owner_scoped_master_request("master", "master", &headers).unwrap_err();
        assert_eq!(rejection.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn owner_scoped_master_rejects_reserved_group_namespace() {
        let rejection = validate_owner_scoped_master_request(
            "master",
            "master",
            &owner_headers(&["grp_existing_group"]),
        )
        .unwrap_err();
        assert_eq!(rejection.0, StatusCode::BAD_REQUEST);
    }

    #[test]
    fn owner_scoped_master_scopes_exclude_administration() {
        use axum::http::Method;

        let scopes = owner_scoped_master_scopes();
        assert_eq!(
            scopes,
            OWNER_SCOPED_MASTER_SCOPES
                .iter()
                .map(|scope| (*scope).to_string())
                .collect::<Vec<_>>()
        );
        assert!(!scopes.iter().any(|scope| scope == SCOPE_KEYS_MANAGE));
        assert_eq!(
            authorize_api_key_route(&Method::GET, "/admin/stats", &scopes)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            authorize_api_key_route(&Method::POST, "/auth/keys", &scopes)
                .unwrap_err()
                .0,
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn test_tool_usage_mark_and_query() {
        let b = ToolUsageBatcher::new();
        b.mark_used("alice".into(), "memory_store".into());
        b.mark_used("alice".into(), "memory_retrieve".into());
        b.mark_used("bob".into(), "memory_store".into());

        let alice = b.get_user_tool_usage("alice");
        assert_eq!(alice.len(), 2);
        let tools: Vec<&str> = alice.iter().map(|(t, _)| t.as_str()).collect();
        assert!(tools.contains(&"memory_store"));
        assert!(tools.contains(&"memory_retrieve"));

        let bob = b.get_user_tool_usage("bob");
        assert_eq!(bob.len(), 1);
        assert_eq!(bob[0].0, "memory_store");

        assert!(b.get_user_tool_usage("nobody").is_empty());
    }

    #[test]
    fn test_tool_usage_overwrite_updates_time() {
        let b = ToolUsageBatcher::new();
        b.mark_used("alice".into(), "memory_store".into());
        let t1 = b.get_user_tool_usage("alice")[0].1;

        std::thread::sleep(std::time::Duration::from_millis(10));
        b.mark_used("alice".into(), "memory_store".into());
        let t2 = b.get_user_tool_usage("alice")[0].1;

        assert!(t2 > t1);
    }

    #[test]
    fn test_tool_usage_empty_tool_not_stored() {
        // Simulates what the AuthUser extractor does: filter(|v| !v.is_empty())
        let raw = "";
        let tool_name = Some(raw).filter(|v| !v.is_empty()).map(String::from);
        assert!(tool_name.is_none());
    }

    #[test]
    fn test_tool_usage_rebuild_merge_fills_missing_entry() {
        let b = ToolUsageBatcher::new();
        let ts = Utc::now() - chrono::Duration::minutes(5);
        let mut rebuilt = std::collections::HashMap::new();
        rebuilt.insert(
            ("alice".to_string(), "memory_store".to_string()),
            (ts, false),
        );

        b.merge_rebuilt_entries(rebuilt);

        let usage = b.get_user_tool_usage("alice");
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].0, "memory_store");
        assert_eq!(usage[0].1, ts);
    }

    #[test]
    fn test_tool_usage_rebuild_merge_preserves_dirty_entry() {
        let b = ToolUsageBatcher::new();
        b.mark_used("alice".into(), "memory_store".into());
        let dirty_ts = b.get_user_tool_usage("alice")[0].1;

        let mut rebuilt = std::collections::HashMap::new();
        rebuilt.insert(
            ("alice".to_string(), "memory_store".to_string()),
            (dirty_ts + chrono::Duration::minutes(5), false),
        );

        b.merge_rebuilt_entries(rebuilt);

        let usage = b.get_user_tool_usage("alice");
        assert_eq!(usage.len(), 1);
        assert_eq!(usage[0].0, "memory_store");
        assert_eq!(usage[0].1, dirty_ts);
    }
}
