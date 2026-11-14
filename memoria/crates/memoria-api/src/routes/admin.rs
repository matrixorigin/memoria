//! Admin endpoints — system stats, user management, governance triggers.
//! All routes require master key auth (same Bearer token).

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use memoria_service::{
    ConsolidationInput, ConsolidationStrategy, DefaultConsolidationStrategy, GovernanceStore,
};
use serde::{Deserialize, Serialize};
use sqlx::{MySqlPool, Row};
use std::sync::Arc;

use crate::{auth::AuthUser, routes::memory::api_err, state::AppState};

fn get_shared_pool(state: &AppState) -> Result<&MySqlPool, (StatusCode, String)> {
    state
        .auth_pool
        .as_ref()
        .or_else(|| state.service.sql_store.as_ref().map(|s| s.pool()))
        .ok_or((
            StatusCode::INTERNAL_SERVER_ERROR,
            "No shared SQL pool".into(),
        ))
}

async fn get_user_store(
    state: &AppState,
    user_id: &str,
) -> Result<Arc<memoria_storage::SqlMemoryStore>, (StatusCode, String)> {
    state.service.user_sql_store(user_id).await.map_err(api_err)
}

/// A `grp_*` scope has its own database only when a `DbRouter` is configured.
/// Without one, `user_sql_store` returns the shared store for every id, so a
/// table-wide statement would touch every user's rows.
fn is_isolated_group_scope(state: &AppState, user_id: &str) -> bool {
    user_id.starts_with("grp_") && state.service.db_router.is_some()
}

async fn list_known_users(state: &AppState) -> Result<Vec<String>, (StatusCode, String)> {
    let sql = state
        .service
        .sql_store
        .as_ref()
        .ok_or((StatusCode::INTERNAL_SERVER_ERROR, "No SQL store".into()))?;
    if let Some(router) = sql.db_router() {
        return router.list_active_users().await.map_err(api_err);
    }
    let rows: Vec<(String,)> = sqlx::query_as(
        "SELECT DISTINCT user_id FROM mem_memories WHERE is_active > 0 ORDER BY user_id",
    )
    .fetch_all(sql.pool())
    .await
    .map_err(db_err)?;
    Ok(rows.into_iter().map(|row| row.0).collect())
}

fn db_err(e: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CursorParams {
    pub cursor: Option<String>,
    pub limit: Option<i64>,
}

#[derive(Serialize)]
pub struct SystemStats {
    pub total_users: i64,
    pub total_memories: i64,
    pub total_snapshots: i64,
}

#[derive(Serialize)]
pub struct UserEntry {
    pub user_id: String,
}

#[derive(Serialize)]
pub struct UserListResponse {
    pub users: Vec<UserEntry>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize)]
pub struct UserStats {
    pub user_id: String,
    pub memory_count: i64,
    pub snapshot_count: i64,
}

#[derive(Deserialize)]
pub struct TriggerParams {
    pub op: Option<String>,
}

// ── Handlers ─────────────────────────────────────────────────────────────────

/// GET /admin/stats
pub async fn system_stats(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<SystemStats>, (StatusCode, String)> {
    auth.require_master()?;
    let user_ids = list_known_users(&state).await?;
    let mut total_memories = 0i64;
    let mut total_snapshots = 0i64;
    for user_id in &user_ids {
        let user_store = get_user_store(&state, user_id).await?;
        let memories_table = user_store.t("mem_memories");
        total_memories += sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE is_active > 0"
        ))
        .fetch_one(user_store.pool())
        .await
        .map_err(db_err)?;
        total_snapshots += user_store
            .list_snapshot_registrations(user_id)
            .await
            .map_err(api_err)?
            .len() as i64;
    }

    Ok(Json(SystemStats {
        total_users: user_ids.len() as i64,
        total_memories,
        total_snapshots,
    }))
}

/// GET /admin/users
pub async fn list_users(
    auth: AuthUser,
    State(state): State<AppState>,
    Query(params): Query<CursorParams>,
) -> Result<Json<UserListResponse>, (StatusCode, String)> {
    auth.require_master()?;
    let limit = validated_user_page_limit(params.limit)?;
    let mut users = list_known_users(&state).await?;
    if let Some(ref cursor) = params.cursor {
        users.retain(|user_id| user_id > cursor);
    }
    // Fetch one extra item to distinguish a full final page from a next page.
    let users: Vec<String> = users.into_iter().take(limit + 1).collect();
    let has_more = users.len() > limit;
    let users: Vec<String> = users.into_iter().take(limit).collect();
    let next_cursor = if has_more { users.last().cloned() } else { None };

    Ok(Json(UserListResponse {
        users: users
            .into_iter()
            .map(|user_id| UserEntry { user_id })
            .collect(),
        next_cursor,
    }))
}

/// GET /admin/users/:user_id/stats
pub async fn user_stats(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Json<UserStats>, (StatusCode, String)> {
    auth.require_master()?;
    let user_store = get_user_store(&state, &user_id).await?;
    let memories_table = user_store.t("mem_memories");
    // For group scopes (user_id starts with "grp_") count ALL active memories in the
    // group database — the individual user_id filter would always return 0 since
    // memories are attributed to each member's uid, not the group id itself.
    let memory_count = if is_isolated_group_scope(&state, &user_id) {
        sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE is_active > 0"
        ))
        .fetch_one(user_store.pool())
        .await
        .map_err(db_err)?
    } else {
        sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE user_id = ? AND is_active > 0"
        ))
        .bind(&user_id)
        .fetch_one(user_store.pool())
        .await
        .map_err(db_err)?
    };
    let snapshot_count = user_store
        .list_snapshot_registrations(&user_id)
        .await
        .map_err(api_err)?
        .len() as i64;

    Ok(Json(UserStats {
        user_id,
        memory_count,
        snapshot_count,
    }))
}

/// DELETE /admin/users/:user_id
pub async fn delete_user(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    auth.require_master()?;
    // Group memories belong to members, not the group id. Only an isolated
    // group database may be cleared without the owner predicate; in
    // router-less mode that statement would deactivate every user's rows.
    if user_id.starts_with("grp_") && !is_isolated_group_scope(&state, &user_id) {
        return Err((
            StatusCode::CONFLICT,
            "group deletion requires isolated group databases (multi-db mode)".into(),
        ));
    }
    let user_store = get_user_store(&state, &user_id).await?;
    let memories_table = user_store.t("mem_memories");
    if is_isolated_group_scope(&state, &user_id) {
        sqlx::query(&format!(
            "UPDATE {memories_table} SET is_active = 0 WHERE is_active > 0"
        ))
        .execute(user_store.pool())
        .await
        .map_err(db_err)?;
    } else {
        sqlx::query(&format!(
            "UPDATE {memories_table} SET is_active = 0 WHERE user_id = ? AND is_active > 0"
        ))
        .bind(&user_id)
        .execute(user_store.pool())
        .await
        .map_err(db_err)?;
    }
    if let Err(e) = state
        .mark_metrics_dirty(&user_id, crate::metrics_summary::DirtyMask::FULL)
        .await
    {
        tracing::warn!(
            user_id = user_id,
            error = %e,
            "failed to mark metrics summary dirty after admin delete_user"
        );
    }
    Ok(Json(
        serde_json::json!({"status": "ok", "user_id": user_id}),
    ))
}

/// POST /admin/users/:user_id/reset-access-counts
pub async fn reset_access_counts(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(user_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    auth.require_master()?;
    let sql = get_user_store(&state, &user_id).await?;
    let reset = sql.reset_access_counts(&user_id).await.map_err(api_err)?;
    Ok(Json(
        serde_json::json!({"user_id": user_id, "reset": reset}),
    ))
}

/// POST /admin/governance/:user_id/trigger?op=governance|consolidate
/// Skips cooldown checks (admin override).
pub async fn trigger_governance(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Query(params): Query<TriggerParams>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    auth.require_master()?;
    let op = params.op.as_deref().unwrap_or("governance");
    let sql = get_user_store(&state, &user_id).await?;

    match op {
        "governance" => {
            // Diagnostic first — must run before physical deletions destroy evidence
            let pollution_detected = sql.detect_pollution(&user_id, 24).await.map_err(db_err)?;
            let quarantined = sql
                .quarantine_low_confidence(&user_id)
                .await
                .map_err(db_err)?;
            let cleaned_stale = sql.cleanup_stale(&user_id).await.map_err(db_err)?;
            let cleaned_tool_results = sql.cleanup_tool_results(72).await.map_err(db_err)?;
            let archived_working = sql.archive_stale_working(24).await.map_err(db_err)?;
            let compressed = sql
                .compress_redundant(&user_id, 0.95, 30, 10_000)
                .await
                .map_err(db_err)?;
            let cleaned_incrementals = sql
                .cleanup_orphaned_incrementals(&user_id, 24)
                .await
                .map_err(db_err)?;
            let orphan_graph_cleaned = sql.cleanup_orphan_graph_data().await.unwrap_or_else(|e| {
                tracing::warn!("orphan graph cleanup failed: {e}");
                0
            });
            Ok(Json(serde_json::json!({
                "op": op, "user_id": user_id,
                "quarantined": quarantined,
                "cleaned_stale": cleaned_stale,
                "cleaned_tool_results": cleaned_tool_results,
                "archived_working": archived_working,
                "compressed_redundant": compressed,
                "cleaned_incrementals": cleaned_incrementals,
                "pollution_detected": pollution_detected,
                "orphan_graph_cleaned": orphan_graph_cleaned,
            })))
        }
        "consolidate" => {
            let graph = sql.graph_store();
            let report = DefaultConsolidationStrategy::default()
                .consolidate(&graph, &ConsolidationInput::for_user(user_id.clone()))
                .await
                .map_err(db_err)?;
            Ok(Json(serde_json::json!({
                "op": op,
                "user_id": user_id,
                "status": report.status.as_str(),
                "conflicts_detected": report.metrics.get("consolidation.conflicts_detected").copied().unwrap_or(0.0) as i64,
                "orphaned_scenes": report.metrics.get("consolidation.orphaned_scenes").copied().unwrap_or(0.0) as i64,
                "promoted": report.metrics.get("trust.promoted_count").copied().unwrap_or(0.0) as i64,
                "demoted": report.metrics.get("trust.demoted_count").copied().unwrap_or(0.0) as i64,
                "warnings": report.warnings,
            })))
        }
        "extract_entities" => {
            let r = memoria_storage::graph::backfill::backfill_graph(&sql, &user_id)
                .await
                .map_err(db_err)?;
            Ok(Json(serde_json::json!({
                "op": op, "user_id": user_id,
                "processed": r.processed, "skipped": r.skipped,
                "edges_created": r.edges_created, "entities_linked": r.entities_linked,
            })))
        }
        "weekly" => {
            let cleaned_snapshots = sql.cleanup_snapshots(5).await.map_err(db_err)?;
            let cleaned_branches = sql.cleanup_orphan_branches().await.map_err(db_err)?;
            let _ = sql.rebuild_vector_index("mem_memories").await;
            Ok(Json(
                serde_json::json!({"op": op, "user_id": user_id, "cleaned_snapshots": cleaned_snapshots, "cleaned_branches": cleaned_branches}),
            ))
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            format!("Invalid op: {op}. Must be governance|consolidate|extract_entities|weekly"),
        )),
    }
}

// ── Health endpoints (per-user, no admin required) ───────────────────────────

/// GET /v1/health/hygiene — per-user orphan/stale diagnostics
pub async fn health_hygiene(
    AuthUser { user_id, .. }: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = get_user_store(&state, &user_id).await?;
    let result = sql.health_hygiene(&user_id).await.map_err(db_err)?;
    Ok(Json(result))
}

/// GET /admin/health/hygiene — global orphan/stale diagnostics
pub async fn health_hygiene_global(
    auth: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    auth.require_master()?;
    let sql = state
        .service
        .sql_store
        .as_ref()
        .ok_or((StatusCode::SERVICE_UNAVAILABLE, "SQL store required".into()))?;
    if let Some(router) = sql.db_router() {
        let user_ids = router.list_active_users().await.map_err(api_err)?;
        let mut inactive = 0i64;
        let mut stale_working = 0i64;
        let mut orphan_mel = 0i64;
        let mut orphan_el = 0i64;
        let mut orphan_graph_nodes = 0i64;
        let mut orphan_stats = 0i64;

        for user_id in user_ids {
            let user_store = state
                .service
                .user_sql_store(&user_id)
                .await
                .map_err(api_err)?;
            let hygiene = user_store.health_hygiene(&user_id).await.map_err(db_err)?;
            inactive += hygiene["inactive_memories"].as_i64().unwrap_or(0);
            stale_working += hygiene["stale_working_memories"].as_i64().unwrap_or(0);
            orphan_mel += hygiene["orphan_memory_entity_links"].as_i64().unwrap_or(0);
            orphan_el += hygiene["orphan_entity_links"].as_i64().unwrap_or(0);
            orphan_graph_nodes += hygiene["orphan_graph_nodes"].as_i64().unwrap_or(0);

            let memory_stats_table = user_store.t("mem_memories_stats");
            let memories_table = user_store.t("mem_memories");
            let (user_orphan_stats,): (i64,) = sqlx::query_as(&format!(
                "SELECT COUNT(*) FROM {memory_stats_table} s \
                 LEFT JOIN {memories_table} m ON s.memory_id = m.memory_id \
                 WHERE m.memory_id IS NULL"
            ))
            .fetch_one(user_store.pool())
            .await
            .map_err(db_err)?;
            orphan_stats += user_orphan_stats;
        }

        return Ok(Json(serde_json::json!({
            "inactive_memories": inactive,
            "stale_working_memories": stale_working,
            "orphan_memory_entity_links": orphan_mel,
            "orphan_entity_links": orphan_el,
            "orphan_graph_nodes": orphan_graph_nodes,
            "orphan_stats": orphan_stats,
        })));
    }
    let result = sql.health_hygiene_global().await.map_err(db_err)?;
    Ok(Json(result))
}

/// GET /v1/health/analyze — per-type stats
pub async fn health_analyze(
    AuthUser { user_id, .. }: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = get_user_store(&state, &user_id).await?;
    let result = sql.health_analyze(&user_id).await.map_err(db_err)?;
    Ok(Json(result))
}

/// GET /v1/health/storage — storage stats
pub async fn health_storage(
    AuthUser { user_id, .. }: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = get_user_store(&state, &user_id).await?;
    let result = sql.health_storage_stats(&user_id).await.map_err(db_err)?;
    Ok(Json(result))
}

/// GET /v1/health/capacity — IVF capacity estimate
pub async fn health_capacity(
    AuthUser { user_id, .. }: AuthUser,
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = get_user_store(&state, &user_id).await?;
    let result = sql.health_capacity(&user_id).await.map_err(db_err)?;
    Ok(Json(result))
}

/// POST /admin/users/:id/strategy?strategy=... — set retrieval strategy (no-op stub for benchmark compat)
pub async fn set_user_strategy(
    State(_state): State<AppState>,
    auth: AuthUser,
    Path(user_id): Path<String>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    auth.require_master()?;
    let strategy = params
        .get("strategy")
        .cloned()
        .unwrap_or_else(|| "vector:v1".to_string());
    Ok(Json(serde_json::json!({
        "user_id": user_id,
        "strategy": strategy,
        "previous": "vector:v1",
        "status": "ok",
    })))
}

/// GET /admin/users/:user_id/keys — list all active API keys for a user (admin only)
pub async fn list_user_keys(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    auth.require_master()?;
    let pool = get_shared_pool(&state)?;
    let rows = sqlx::query(
        "SELECT key_id, name, key_prefix, created_at, expires_at, last_used_at \
         FROM mem_api_keys WHERE user_id = ? AND is_active = 1 ORDER BY created_at DESC",
    )
    .bind(&user_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;

    let keys: Vec<serde_json::Value> = rows.iter().map(|r| {
        serde_json::json!({
            "key_id": r.try_get::<String, _>("key_id").unwrap_or_default(),
            "name": r.try_get::<String, _>("name").unwrap_or_default(),
            "key_prefix": r.try_get::<String, _>("key_prefix").unwrap_or_default(),
            "created_at": r.try_get::<chrono::NaiveDateTime, _>("created_at").map(|d| d.to_string()).unwrap_or_default(),
            "expires_at": r.try_get::<Option<chrono::NaiveDateTime>, _>("expires_at").ok().flatten().map(|d| d.to_string()),
            "last_used_at": r.try_get::<Option<chrono::NaiveDateTime>, _>("last_used_at").ok().flatten().map(|d| d.to_string()),
        })
    }).collect();

    Ok(Json(serde_json::json!({"user_id": user_id, "keys": keys})))
}

/// DELETE /admin/users/:user_id/keys — revoke all active API keys for a user (admin only)
pub async fn revoke_all_user_keys(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    auth.require_master()?;
    let pool = get_shared_pool(&state)?;
    let result =
        sqlx::query("UPDATE mem_api_keys SET is_active = 0 WHERE user_id = ? AND is_active = 1")
            .bind(&user_id)
            .execute(pool)
            .await
            .map_err(db_err)?;
    Ok(Json(
        serde_json::json!({"user_id": user_id, "revoked": result.rows_affected()}),
    ))
}

/// POST /admin/users/:user_id/params — set per-user activation param overrides
pub async fn set_user_params(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Json(params): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    auth.require_master()?;
    let pool = get_shared_pool(&state)?;
    let pj = serde_json::to_string(&params).map_err(db_err)?;
    sqlx::query(
        "UPDATE mem_user_memory_config SET params_json = ?, updated_at = NOW() WHERE user_id = ?",
    )
    .bind(&pj)
    .bind(&user_id)
    .execute(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(
        serde_json::json!({"user_id": user_id, "params": params}),
    ))
}

/// GET /admin/config — view current runtime configuration (redacted secrets).
pub async fn get_config(auth: AuthUser) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    auth.require_master()?;
    let cfg = memoria_service::Config::from_env();
    Ok(Json(serde_json::json!({
        "db_url": redact_url(&cfg.db_url),
        "db_name": cfg.db_name,
        "embedding_provider": cfg.embedding_provider,
        "embedding_model": cfg.embedding_model,
        "embedding_dim": cfg.embedding_dim,
        "embedding_base_url": cfg.embedding_base_url,
        "has_embedding": cfg.has_embedding(),
        "has_llm": cfg.has_llm(),
        "llm_model": cfg.llm_model,
        "llm_base_url": cfg.llm_base_url,
        "user": cfg.user,
        "governance_plugin_binding": cfg.governance_plugin_binding,
        "governance_plugin_subject": cfg.governance_plugin_subject,
        "governance_plugin_dir": cfg.governance_plugin_dir,
        "instance_id": cfg.instance_id,
        "lock_ttl_secs": cfg.lock_ttl_secs,
        "governance_enabled": std::env::var("MEMORIA_GOVERNANCE_ENABLED")
            .unwrap_or_else(|_| "false".into()),
    })))
}

// ── Per-user API call statistics ─────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CallStatsQuery {
    pub days: Option<u32>,
}

/// GET /admin/users/:user_id/call-stats?days=7
///
/// Returns aggregated MCP/API call statistics for the requested user, sourced
/// from `mem_api_call_log`.  Requires master key.
///
/// Response shape (mirrors the `summary` + `by_tool` sections of the Monitor
/// dashboard endpoint so the website backend can swap in this data directly):
/// ```json
/// {
///   "total_calls": 12345,
///   "avg_latency_ms": 125,
///   "error_count": 42,
///   "error_rate": 0.34,
///   "days": 7,
///   "by_path": [
///     { "path": "/v1/memories/search", "count": 5000, "avg_ms": 110,
///       "max_ms": 900, "error_count": 10 },
///     ...
///   ]
/// }
/// ```
pub async fn user_call_stats(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
    Query(params): Query<CallStatsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    auth.require_master()?;
    let store = get_user_store(&state, &user_id).await?;
    let call_log = store.t("mem_api_call_log");

    let days = params.days.unwrap_or(7).clamp(1, 90) as i64;

    // Aggregate totals for the requested time window.
    // Error counting unifies HTTP errors (/v1/*) and JSON-RPC errors (/mcp/*):
    //   - /v1/* REST calls: HTTP status_code >= 400 signals an error
    //   - /mcp/*: protocol and tool execution errors are independent dimensions
    let row = sqlx::query(&format!(
        "SELECT \
            CAST(COUNT(*) AS SIGNED) AS total, \
            CAST(COALESCE(AVG(latency_ms), 0) AS DOUBLE) AS avg_ms, \
            CAST(SUM(CASE WHEN status_code >= 400 OR rpc_success = 0 OR tool_success = 0 THEN 1 ELSE 0 END) AS SIGNED) AS errors \
         FROM {call_log} \
         WHERE user_id = ? AND called_at >= DATE_SUB(NOW(6), INTERVAL ? DAY)",
    ))
    .bind(&user_id)
    .bind(days)
    .fetch_one(store.pool())
    .await
    .map_err(db_err)?;

    let total: i64 = row.try_get("total").unwrap_or(0);
    let avg_ms: f64 = row.try_get("avg_ms").unwrap_or(0.0);
    let errors: i64 = row.try_get("errors").unwrap_or(0);

    // Per-(method, path) breakdown — used as "by_tool" in the Monitor dashboard.
    // Grouping by method disambiguates e.g. POST /v1/memories (store) vs
    // GET /v1/memories (list).
    let by_path_rows = sqlx::query(&format!(
        "SELECT \
            method, \
            path, \
            CAST(COUNT(*) AS SIGNED) AS cnt, \
            CAST(COALESCE(AVG(latency_ms), 0) AS DOUBLE) AS avg_ms, \
            CAST(COALESCE(MAX(latency_ms), 0) AS SIGNED) AS max_ms, \
            CAST(SUM(CASE WHEN status_code >= 400 THEN 1 ELSE 0 END) AS SIGNED) AS err_cnt, \
            CAST(SUM(CASE WHEN rpc_success = 0 THEN 1 ELSE 0 END) AS SIGNED) AS rpc_err_cnt, \
            CAST(SUM(CASE WHEN tool_success = 0 THEN 1 ELSE 0 END) AS SIGNED) AS tool_err_cnt, \
            CAST(SUM(CASE WHEN tool_error_kind = 'input' THEN 1 ELSE 0 END) AS SIGNED) AS tool_input_cnt, \
            CAST(SUM(CASE WHEN tool_error_kind = 'rejected' THEN 1 ELSE 0 END) AS SIGNED) AS tool_rejected_cnt, \
            CAST(SUM(CASE WHEN tool_error_kind = 'backend' THEN 1 ELSE 0 END) AS SIGNED) AS tool_backend_cnt \
         FROM {call_log} \
         WHERE user_id = ? AND called_at >= DATE_SUB(NOW(6), INTERVAL ? DAY) \
         GROUP BY method, path \
         ORDER BY cnt DESC \
         LIMIT 50",
    ))
    .bind(&user_id)
    .bind(days)
    .fetch_all(store.pool())
    .await
    .map_err(db_err)?;

    let by_path: Vec<serde_json::Value> = by_path_rows
        .iter()
        .map(|r| {
            let method: String = r.try_get("method").unwrap_or_default();
            let path: String = r.try_get("path").unwrap_or_default();
            let cnt: i64 = r.try_get("cnt").unwrap_or(0);
            let p_avg: f64 = r.try_get("avg_ms").unwrap_or(0.0);
            let max_ms: i64 = r.try_get("max_ms").unwrap_or(0);
            let err_cnt: i64 = r.try_get("err_cnt").unwrap_or(0);
            let rpc_err_cnt: i64 = r.try_get("rpc_err_cnt").unwrap_or(0);
            // HTTP, protocol, and execution failures remain independently queryable.
            let tool_err_cnt: i64 = r.try_get("tool_err_cnt").unwrap_or(0);
            let total_err = err_cnt + rpc_err_cnt + tool_err_cnt;
            serde_json::json!({
                "method": method,
                "path": path,
                "count": cnt,
                "avg_ms": p_avg as i64,
                "max_ms": max_ms,
                "error_count": total_err,
                "rpc_error_count": rpc_err_cnt,
                "tool_error_count": tool_err_cnt,
                "tool_input_error_count": r.try_get::<i64, _>("tool_input_cnt").unwrap_or(0),
                "tool_rejected_count": r.try_get::<i64, _>("tool_rejected_cnt").unwrap_or(0),
                "tool_backend_error_count": r.try_get::<i64, _>("tool_backend_cnt").unwrap_or(0),
                "error_rate": if cnt > 0 {
                    (total_err as f64 / cnt as f64 * 100.0).round() / 100.0
                } else { 0.0 },
            })
        })
        .collect();

    // Most recent 50 calls for the live "Recent Calls" feed.
    // Include both protocol and tool outcomes for MCP responses with HTTP 200.
    let recent_rows = sqlx::query(&format!(
        "SELECT method, path, status_code, latency_ms, called_at, rpc_success, tool_success, tool_error_kind \
         FROM {call_log} \
         WHERE user_id = ? \
         ORDER BY called_at DESC \
         LIMIT 50",
    ))
    .bind(&user_id)
    .fetch_all(store.pool())
    .await
    .unwrap_or_default();

    let recent_calls: Vec<serde_json::Value> = recent_rows
        .iter()
        .map(|r| {
            let method: String = r.try_get("method").unwrap_or_default();
            let path: String = r.try_get("path").unwrap_or_default();
            let status_code: i16 = r.try_get("status_code").unwrap_or(0);
            let latency_ms: i32 = r.try_get("latency_ms").unwrap_or(0);
            let called_at: chrono::DateTime<chrono::Utc> = r
                .try_get("called_at")
                .unwrap_or_else(|_| chrono::Utc::now());
            // rpc_success defaults to true (1) for /v1/* rows that predate the column.
            let rpc_success: i8 = r.try_get("rpc_success").unwrap_or(1);
            let tool_success: Option<i8> = r.try_get("tool_success").unwrap_or(None);
            let tool_error_kind: Option<String> = r.try_get("tool_error_kind").unwrap_or(None);
            let is_err = status_code >= 400 || rpc_success == 0 || tool_success == Some(0);
            serde_json::json!({
                "method": method,
                "path": path,
                "status_code": status_code,
                "latency_ms": latency_ms,
                "called_at": called_at.to_rfc3339(),
                // Unified status: HTTP error (/v1/*) OR JSON-RPC error (/mcp/*)
                "status": if is_err { "err" } else { "ok" },
                "tool_success": tool_success.map(|v| v != 0),
                "tool_error_kind": tool_error_kind,
            })
        })
        .collect();

    // ── All-time per-type aggregates (no days filter) ─────────────────────────
    // Used by the Usage panel's stats cards: total_writes, total_searches, etc.
    let at_row = sqlx::query(&format!(
        "SELECT \
            CAST(COUNT(*) AS SIGNED) AS total, \
            CAST(SUM(CASE WHEN (path = '/v1/memories' AND method = 'POST') \
                              OR path IN ('/mcp/memory_store','/mcp/memory_correct') \
                         THEN 1 ELSE 0 END) AS SIGNED) AS writes, \
            CAST(SUM(CASE WHEN path IN ('/v1/memories/search','/v1/memories/retrieve', \
                                        '/mcp/memory_retrieve','/mcp/memory_search') \
                         THEN 1 ELSE 0 END) AS SIGNED) AS retrieves, \
            CAST(SUM(CASE WHEN (method = 'DELETE' AND path LIKE '/v1/memories/%') \
                              OR path = '/mcp/memory_purge' \
                         THEN 1 ELSE 0 END) AS SIGNED) AS deletes, \
            CAST(COALESCE(AVG(CASE WHEN path IN ('/v1/memories/search','/v1/memories/retrieve', \
                                                  '/mcp/memory_retrieve','/mcp/memory_search') \
                              THEN latency_ms END), 0) AS DOUBLE) AS avg_retrieval_ms \
         FROM {call_log} \
         WHERE user_id = ?",
    ))
    .bind(&user_id)
    .fetch_one(store.pool())
    .await
    .map_err(db_err)?;

    let at_total: i64 = at_row.try_get("total").unwrap_or(0);
    let at_writes: i64 = at_row.try_get("writes").unwrap_or(0);
    let at_retrieves: i64 = at_row.try_get("retrieves").unwrap_or(0);
    let at_deletes: i64 = at_row.try_get("deletes").unwrap_or(0);
    let at_avg_ret: f64 = at_row.try_get("avg_retrieval_ms").unwrap_or(0.0);

    // ── Per-day series within the requested window ─────────────────────────────
    // Used by the Usage panel's API Call Tracking chart.
    // day_idx = 0 → oldest calendar day,  day_idx = days-1 → today  (DB timezone).
    let series_rows = sqlx::query(&format!(
        "SELECT \
            CAST(DATEDIFF(DATE(called_at), \
                          DATE(DATE_SUB(NOW(6), INTERVAL ? DAY))) AS SIGNED) AS day_idx, \
            CAST(SUM(CASE WHEN (path = '/v1/memories' AND method = 'POST') \
                              OR path IN ('/mcp/memory_store','/mcp/memory_correct') \
                         THEN 1 ELSE 0 END) AS SIGNED) AS writes, \
            CAST(SUM(CASE WHEN (method = 'DELETE' AND path LIKE '/v1/memories/%') \
                              OR path = '/mcp/memory_purge' \
                         THEN 1 ELSE 0 END) AS SIGNED) AS deletes, \
            CAST(SUM(CASE WHEN path IN ('/v1/memories/search','/v1/memories/retrieve', \
                                        '/mcp/memory_retrieve','/mcp/memory_search') \
                         THEN 1 ELSE 0 END) AS SIGNED) AS retrieves, \
            CAST(COUNT(*) AS SIGNED) AS total \
          FROM {call_log} \
          WHERE user_id = ? \
            AND DATE(called_at) >= DATE(DATE_SUB(NOW(6), INTERVAL ? DAY)) \
          GROUP BY DATE(called_at) \
          ORDER BY DATE(called_at) ASC",
    ))
    .bind(days - 1) // offset = days - 1 so day_idx 0 = oldest day
    .bind(&user_id)
    .bind(days - 1)
    .fetch_all(store.pool())
    .await
    .unwrap_or_default();

    let series: Vec<serde_json::Value> = series_rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "day_idx":   r.try_get::<i64,_>("day_idx").unwrap_or(0),
                "writes":    r.try_get::<i64,_>("writes").unwrap_or(0),
                "deletes":   r.try_get::<i64,_>("deletes").unwrap_or(0),
                "retrieves": r.try_get::<i64,_>("retrieves").unwrap_or(0),
                "total":     r.try_get::<i64,_>("total").unwrap_or(0),
            })
        })
        .collect();

    Ok(Json(serde_json::json!({
        // ── Monitor panel fields ───────────────────────────────────────────────
        "total_calls": total,
        "avg_latency_ms": avg_ms as i64,
        "error_count": errors,
        "error_rate": if total > 0 {
            (errors as f64 / total as f64 * 100.0).round() / 100.0
        } else { 0.0 },
        "days": days,
        "by_path": by_path,
        "recent_calls": recent_calls,
        // ── Usage panel fields ─────────────────────────────────────────────────
        "all_time": {
            "total_calls":     at_total,
            "total_writes":    at_writes,
            "total_retrieves": at_retrieves,
            "total_deletes":   at_deletes,
            "avg_retrieval_ms": at_avg_ret as i64,
        },
        "series": series,
    })))
}

/// GET /admin/users/:user_id/branch-stats
///
/// Returns memory counts per branch for a given user (or group).
/// Response:
/// ```json
/// {
///   "branches": [
///     {"name": "main",    "count": 170},
///     {"name": "another", "count": 320}
///   ],
///   "total": 490
/// }
/// ```
pub async fn user_branch_stats(
    auth: AuthUser,
    State(state): State<AppState>,
    Path(user_id): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    auth.require_master()?;
    let user_store = get_user_store(&state, &user_id).await?;

    // --- main branch ---
    // Personal users have rows from multiple users in mem_memories (shared table), so
    // we filter by user_id. Group users own an isolated per-group database where
    // mem_memories contains only group data, so no user_id filter is needed there.
    let memories_table = user_store.t("mem_memories");
    let main_count: i64 = if is_isolated_group_scope(&state, &user_id) {
        sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE is_active > 0"
        ))
        .fetch_one(user_store.pool())
        .await
        .map_err(db_err)?
    } else {
        sqlx::query_scalar::<_, i64>(&format!(
            "SELECT COUNT(*) FROM {memories_table} WHERE user_id = ? AND is_active > 0"
        ))
        .bind(&user_id)
        .fetch_one(user_store.pool())
        .await
        .map_err(db_err)?
    };

    let mut branches_stats = vec![serde_json::json!({"name": "main", "count": main_count})];
    let mut total = main_count;
    let mut degraded_branches: Vec<String> = Vec::new();

    // --- non-main branches from mem_branches ---
    // Branch tables are per-user physical tables (created via `data branch create table`),
    // so they only contain data belonging to the owning user/group. No user_id filter is
    // required — this is consistent with the group-scoped main count above.
    let branch_rows = user_store.list_branches(&user_id).await.map_err(api_err)?;
    for (name, raw_table_name, _created_at) in &branch_rows {
        if name == "main" || raw_table_name.is_empty() {
            continue;
        }
        if !raw_table_name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            tracing::warn!(
                user_id = %user_id,
                branch = %name,
                table = %raw_table_name,
                "user_branch_stats: skipping branch with invalid table identifier"
            );
            degraded_branches.push(name.clone());
            continue;
        }
        let bt = user_store.t(raw_table_name);
        let count_result =
            sqlx::query_scalar::<_, i64>(&format!("SELECT COUNT(*) FROM {bt} WHERE is_active > 0"))
                .fetch_one(user_store.pool())
                .await;
        let count = match count_result {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(
                    user_id = %user_id,
                    branch = %name,
                    table = %raw_table_name,
                    error = %e,
                    "user_branch_stats: failed to count memories for branch"
                );
                // Record degraded branch so the caller knows the total is unreliable.
                degraded_branches.push(name.clone());
                0
            }
        };
        branches_stats.push(serde_json::json!({"name": name, "count": count}));
        total += count;
    }

    Ok(Json(serde_json::json!({
        "branches": branches_stats,
        "total": total,
        // `degraded` is true when at least one branch table could not be counted.
        // In that case `total` is a lower bound and `degraded_branches` lists the
        // affected branches — callers should treat the numbers as approximate.
        "degraded": !degraded_branches.is_empty(),
        "degraded_branches": degraded_branches,
    })))
}

/// Redact password from database URL for safe display.
fn redact_url(url: &str) -> String {
    // Only inspect the authority: '@' in a password is not the host separator,
    // and '@' in a path, query, or fragment is never userinfo.
    if let Some(scheme_end) = url.find("://") {
        let authority_start = scheme_end + 3;
        let authority_end = url[authority_start..]
            .find(&['/', '?', '#'][..])
            .map_or(url.len(), |offset| authority_start + offset);
        let authority = &url[authority_start..authority_end];
        if let Some(at) = authority.rfind('@') {
            if let Some(colon) = authority[..at].find(':') {
                let password_start = authority_start + colon;
                let host_start = authority_start + at;
                return format!("{}:***{}", &url[..password_start], &url[host_start..]);
            }
        }
    }
    url.to_string()
}

fn validated_user_page_limit(limit: Option<i64>) -> Result<usize, (StatusCode, String)> {
    let limit = limit.unwrap_or(100);
    if !(1..=1000).contains(&limit) {
        return Err((StatusCode::BAD_REQUEST, "limit must be between 1 and 1000".into()));
    }
    Ok(limit as usize)
}

#[cfg(test)]
mod redact_url_tests {
    use super::redact_url;

    #[test]
    fn redacts_entire_password_even_when_it_contains_at_signs() {
        assert_eq!(
            redact_url("mysql://user:p@ss@host:3306/db"),
            "mysql://user:***@host:3306/db"
        );
        assert_eq!(
            redact_url("mysql://user:p@ss@word@host/db?x=1"),
            "mysql://user:***@host/db?x=1"
        );
        assert_eq!(
            redact_url("mysql://user:pass@host/db"),
            "mysql://user:***@host/db"
        );
    }

    #[test]
    fn does_not_confuse_path_query_or_fragment_with_userinfo() {
        for url in [
            "mysql://host/db@archive",
            "mysql://host/db?contact=user@example.com",
            "mysql://host/db#user:pass@comment",
            "mysql://host/db",
        ] {
            assert_eq!(redact_url(url), url);
        }
    }
}

#[cfg(test)]
mod admin_user_pagination_tests {
    use super::validated_user_page_limit;
    use axum::http::StatusCode;

    #[test]
    fn rejects_invalid_limits_and_defaults_safely() {
        assert_eq!(validated_user_page_limit(None).unwrap(), 100);
        assert_eq!(validated_user_page_limit(Some(1)).unwrap(), 1);
        assert_eq!(validated_user_page_limit(Some(1000)).unwrap(), 1000);
        for limit in [i64::MIN, -1, 0, 1001, i64::MAX] {
            assert_eq!(
                validated_user_page_limit(Some(limit)).unwrap_err().0,
                StatusCode::BAD_REQUEST
            );
        }
    }
}
