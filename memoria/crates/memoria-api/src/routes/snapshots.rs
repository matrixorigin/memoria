use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use crate::{
    auth::AuthUser,
    models::*,
    routes::memory::{api_err, api_err_typed},
    state::AppState,
};
use memoria_core::{MemoriaError, TrustTier};
use memoria_git::GitForDataService;
use std::sync::Arc;

#[derive(Deserialize)]
pub struct ListSnapshotsQuery {
    #[serde(default = "default_snap_limit")]
    pub limit: i64,
    #[serde(default)]
    pub offset: i64,
    /// Include automatic safety snapshots; filter before pagination when false.
    #[serde(default = "default_true")]
    pub include_safety: bool,
    /// Backfill missing historical counts. When false, unknown counts are null.
    #[serde(default = "default_true")]
    pub resolve_memory_counts: bool,
}
fn default_snap_limit() -> i64 {
    20
}

fn default_true() -> bool {
    true
}

impl Default for ListSnapshotsQuery {
    fn default() -> Self {
        Self {
            limit: default_snap_limit(),
            offset: 0,
            include_safety: true,
            resolve_memory_counts: true,
        }
    }
}

#[derive(Deserialize, Default)]
pub struct GetSnapshotQuery {
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub detail: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct DiffSnapshotQuery {
    pub limit: Option<i64>,
}

#[derive(Deserialize, Default)]
pub struct DiffBranchItemsQuery {
    pub limit: Option<i64>,
}

pub type ApplyBranchRequest = memoria_git::ApplySelection;

/// Delegate to git_tools::call for snapshot/branch operations.
async fn git_call_text(
    state: &AppState,
    user_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> Result<String, (StatusCode, String)> {
    let result = memoria_mcp::git_tools::call(tool, args, &state.git, &state.service, user_id)
        .await
        .map_err(api_err_typed)?;
    git_result_text(&result)
}

fn git_result_text(result: &Value) -> Result<String, (StatusCode, String)> {
    let text = result["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    use memoria_mcp::tool_result::{error_kind, ErrorKind};
    match error_kind(result) {
        Some(ErrorKind::Input) => Err((StatusCode::BAD_REQUEST, text)),
        Some(ErrorKind::Rejected) => Err((StatusCode::CONFLICT, text)),
        Some(ErrorKind::Backend) => Err((StatusCode::INTERNAL_SERVER_ERROR, text)),
        None => Ok(text),
    }
}

#[cfg(test)]
mod tool_result_tests {
    use super::*;
    use memoria_mcp::tool_result::{classified_error, ErrorKind};

    #[test]
    fn delegated_tool_failures_are_http_errors() {
        for (kind, status) in [
            (ErrorKind::Input, StatusCode::BAD_REQUEST),
            (ErrorKind::Rejected, StatusCode::CONFLICT),
            (ErrorKind::Backend, StatusCode::INTERNAL_SERVER_ERROR),
        ] {
            assert_eq!(
                git_result_text(&classified_error(kind, "failure")),
                Err((status, "failure".into()))
            );
        }
        assert_eq!(
            git_result_text(&json!({"content":[{"type":"text","text":"done"}]})),
            Ok("done".into())
        );
    }
}

async fn git_call(
    state: &AppState,
    user_id: &str,
    tool: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let text = git_call_text(state, user_id, tool, args).await?;
    Ok(json!({ "result": text }))
}

async fn git_call_pick(
    state: &AppState,
    user_id: &str,
    args: serde_json::Value,
) -> Result<serde_json::Value, (StatusCode, String)> {
    let result =
        memoria_mcp::git_tools::call("memory_pick", args, &state.git, &state.service, user_id)
            .await
            .map_err(|e| match e {
                MemoriaError::Validation(msg) if msg.starts_with("Conflict:") => {
                    (StatusCode::CONFLICT, msg)
                }
                other => api_err_typed(other),
            })?;
    let text = git_result_text(&result)?;
    Ok(serde_json::from_str(&text).unwrap_or_else(|_| json!({ "result": text })))
}

async fn user_snapshot_store(
    state: &AppState,
    user_id: &str,
) -> Result<Arc<memoria_storage::SqlMemoryStore>, (StatusCode, String)> {
    state.service.user_sql_store(user_id).await.map_err(api_err)
}

fn user_git_service(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
) -> Result<GitForDataService, (StatusCode, String)> {
    let db_name = sql.database_name().ok_or((
        StatusCode::SERVICE_UNAVAILABLE,
        "SQL store missing database URL".into(),
    ))?;
    Ok(GitForDataService::new(
        sql.pool().clone(),
        db_name.to_string(),
    ))
}

fn validate_snapshot_identifier(name: &str) -> Result<&str, (StatusCode, String)> {
    if !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_') {
        Ok(name)
    } else {
        Err((StatusCode::BAD_REQUEST, "Invalid snapshot name".into()))
    }
}

fn milestone_internal(name: &str) -> Option<String> {
    if let Some(rest) = name.strip_prefix("auto:") {
        Some(format!("mem_milestone_{rest}"))
    } else if name.starts_with("mem_milestone_") || name.starts_with("mem_snap_pre_") {
        Some(name.to_string())
    } else {
        None
    }
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

fn compact_safety_prefix(db_name: &str) -> String {
    format!("mem_snap_{}_pre_", compact_identifier_fragment(db_name, 21))
}

fn safety_snapshot_display_name(internal: &str) -> Option<String> {
    if let Some(rest) = internal.strip_prefix("mem_snap_pre_") {
        return Some(format!("pre_{rest}"));
    }
    let rest = internal.strip_prefix("mem_snap_")?;
    let (_, after_prefix) = rest.split_once("_pre_")?;
    Some(format!("pre_{after_prefix}"))
}

fn format_snapshot_timestamp(timestamp: Option<chrono::NaiveDateTime>) -> Option<String> {
    timestamp.map(|ts| ts.and_utc().to_rfc3339())
}

fn format_snapshot_list_result(snapshots: &[Value], total: usize) -> String {
    if snapshots.is_empty() {
        return "Snapshots (0):".to_string();
    }
    let text = snapshots
        .iter()
        .map(|snapshot| {
            let name = snapshot["name"].as_str().unwrap_or_default();
            let ts = snapshot["timestamp"]
                .as_str()
                .or_else(|| snapshot["created_at"].as_str())
                .unwrap_or_default();
            format!("{name} ({ts})")
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("Snapshots ({total} total):\n{text}")
}

fn format_branch_list_result(branches: &[Value]) -> String {
    if branches.is_empty() {
        return "Branches:\nmain ← active".to_string();
    }
    let text = branches
        .iter()
        .map(|branch| {
            let name = branch["name"].as_str().unwrap_or_default();
            if branch["active"].as_bool().unwrap_or(false) {
                format!("{name} ← active")
            } else {
                name.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!("Branches:\n{text}")
}

/// Like `branch_table_name` but returns the raw table name (no db prefix or backticks),
/// suitable for `data branch diff` which builds its own qualified identifiers.
async fn branch_table_name_raw(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    scope_id: &str,
    branch_name: &str,
) -> Result<String, (StatusCode, String)> {
    if branch_name == "main" {
        return Ok("mem_memories".to_string());
    }
    for (name, table_name, _created_at) in sql.list_branches(scope_id).await.map_err(api_err)? {
        if name == branch_name {
            return Ok(table_name);
        }
    }
    Err((
        StatusCode::NOT_FOUND,
        format!("Branch not found: {branch_name}"),
    ))
}

async fn branch_diff_items_payload(
    state: &AppState,
    scope_id: &str,
    branch_name: &str,
    limit: i64,
) -> Result<Value, (StatusCode, String)> {
    let sql = user_snapshot_store(state, scope_id).await?;
    let git = user_git_service(&sql)?;
    let branch_table_raw = branch_table_name_raw(&sql, scope_id, branch_name).await?;
    let limit = limit.clamp(1, 500);

    // Use MatrixOne native `data branch diff` + classify (with source-aware conflict detection)
    let raw_rows = git
        .diff_branch_rows(&branch_table_raw, "mem_memories", scope_id, limit * 3)
        .await
        .map_err(api_err)?;
    let mut classified = memoria_git::classify_diff_rows(raw_rows, &branch_table_raw);

    // classify_diff_rows cannot distinguish "deleted from main on branch" from
    // "created-then-deleted on branch" without querying the main table (because
    // data branch diff only returns the branch-side row for deletions). Resolve
    // now: ghost removes (never in main) move to classified.ghost_removes.
    git.resolve_ghost_removes(&mut classified, "mem_memories", scope_id)
        .await
        .map_err(api_err)?;

    tracing::debug!(
        branch = %branch_name,
        added = classified.added.len(),
        updated = classified.updated.len(),
        removed = classified.removed.len(),
        ghost_removes = classified.ghost_removes.len(),
        conflicts = classified.conflicts.len(),
        behind_main = classified.behind_main.len(),
        "branch_diff_items_payload: classified result"
    );

    let item_to_json = |item: &memoria_git::DiffItem| -> Value {
        json!({
            "memory_id": item.memory_id,
            "content": item.content,
            "memory_type": item.memory_type,
            "author_id": item.author_id,
        })
    };

    let added: Vec<Value> = classified
        .added
        .iter()
        .take(limit as usize)
        .map(item_to_json)
        .collect();
    let updated: Vec<Value> = classified
        .updated
        .iter()
        .take(limit as usize)
        .map(|pair| {
            json!({
                "memory_id": pair.new_memory_id,
                "content": pair.new_content,
                "memory_type": pair.memory_type,
                "old_memory_id": pair.old_memory_id,
                "old_content": pair.old_content,
                "author_id": pair.author_id,
            })
        })
        .collect();
    let removed: Vec<Value> = classified
        .removed
        .iter()
        .take(limit as usize)
        .map(item_to_json)
        .collect();
    let conflicts: Vec<Value> = classified
        .conflicts
        .iter()
        .take(limit as usize)
        .map(|c| {
            json!({
                "memory_id": c.memory_id,
                "branch": {
                    "content": c.branch_side.content,
                    "is_active": c.branch_side.is_active,
                    "superseded_by": c.branch_side.superseded_by,
                    "superseded_by_content": c.branch_side.superseded_by_content,
                    "author_id": c.branch_side.author_id,
                },
                "main": {
                    "content": c.main_side.content,
                    "is_active": c.main_side.is_active,
                    "superseded_by": c.main_side.superseded_by,
                    "superseded_by_content": c.main_side.superseded_by_content,
                    "author_id": c.main_side.author_id,
                },
            })
        })
        .collect();

    let behind_main: Vec<Value> = classified
        .behind_main
        .iter()
        .take(limit as usize)
        .map(item_to_json)
        .collect();

    Ok(json!({
        "branch": branch_name,
        "against": "main",
        "added": added,
        "updated": updated,
        "removed": removed,
        "conflicts": conflicts,
        "behind_main": behind_main,
    }))
}

fn parse_created_snapshot_result(text: &str) -> Option<(String, String)> {
    let rest = text.strip_prefix("Snapshot '")?;
    let (name, timestamp) = rest.split_once("' created at ")?;
    Some((name.to_string(), timestamp.to_string()))
}

async fn snapshot_summary_value(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    user_id: &str,
    snapshot: &memoria_mcp::git_tools::VisibleSnapshot,
    resolve_memory_counts: bool,
) -> Result<Value, (StatusCode, String)> {
    let snapshot_name = validate_snapshot_identifier(&snapshot.internal_name)?.to_string();
    let memory_count = if snapshot.memory_count.is_some() || !resolve_memory_counts {
        snapshot.memory_count
    } else {
        // Read-through backfill for registrations created before mem_snapshots.extra.
        // The response still returns a concrete count, and registered snapshots are
        // persisted so later list calls avoid historical COUNT queries.
        let started = std::time::Instant::now();
        let memory_count =
            memoria_mcp::git_tools::count_active_memories_at_snapshot(sql, user_id, &snapshot_name)
                .await
                .map_err(api_err_typed)?;
        if snapshot.registered {
            sql.update_snapshot_memory_count(
                user_id,
                &snapshot.display_name,
                &snapshot_name,
                memory_count,
            )
            .await
            .map_err(api_err_typed)?;
        }
        let elapsed_ms = started.elapsed().as_millis();
        if elapsed_ms >= 100 {
            tracing::info!(
                user_id = %user_id,
                snapshot_display = %snapshot.display_name,
                snapshot_internal = %snapshot_name,
                memory_count,
                persisted = snapshot.registered,
                elapsed_ms,
                "snapshot_list_phase: snapshot_memory_count"
            );
        } else {
            tracing::debug!(
                user_id = %user_id,
                snapshot_display = %snapshot.display_name,
                snapshot_internal = %snapshot_name,
                memory_count,
                persisted = snapshot.registered,
                elapsed_ms,
                "snapshot_list_phase: snapshot_memory_count"
            );
        }
        Some(memory_count)
    };
    let timestamp = format_snapshot_timestamp(snapshot.timestamp);
    Ok(json!({
        "name": snapshot.display_name,
        "snapshot_name": snapshot_name,
        "description": Value::Null,
        "memory_count": memory_count,
        "created_at": timestamp,
        "timestamp": timestamp,
        "registered": snapshot.registered,
    }))
}

fn snapshot_page<'a>(
    all: &'a [memoria_mcp::git_tools::VisibleSnapshot],
    query: &ListSnapshotsQuery,
) -> (usize, Vec<&'a memoria_mcp::git_tools::VisibleSnapshot>) {
    let visible: Vec<_> = all
        .iter()
        .filter(|snapshot| query.include_safety || !snapshot.is_safety)
        .collect();
    let total = visible.len();
    let page = visible
        .into_iter()
        .skip(query.offset.max(0) as usize)
        .take(query.limit.max(0) as usize)
        .collect();
    (total, page)
}

async fn snapshot_list_payload(
    state: &AppState,
    user_id: &str,
    query: ListSnapshotsQuery,
) -> Result<Value, (StatusCode, String)> {
    let ListSnapshotsQuery { limit, offset, .. } = query;
    let started = std::time::Instant::now();
    let store_started = std::time::Instant::now();
    let sql = user_snapshot_store(state, user_id).await?;
    let store_elapsed_ms = store_started.elapsed().as_millis();
    let visible_started = std::time::Instant::now();
    let all = memoria_mcp::git_tools::visible_snapshots_for_user(&state.service, user_id)
        .await
        .map_err(api_err_typed)?;
    let visible_elapsed_ms = visible_started.elapsed().as_millis();
    let (total, page) = snapshot_page(&all, &query);
    let summaries_started = std::time::Instant::now();
    let mut snapshots = Vec::new();
    for snapshot in page {
        snapshots.push(
            snapshot_summary_value(&sql, user_id, snapshot, query.resolve_memory_counts).await?,
        );
    }
    let summaries_elapsed_ms = summaries_started.elapsed().as_millis();
    let has_more = offset.max(0) as usize + snapshots.len() < total;
    let result = format_snapshot_list_result(&snapshots, total);
    let total_elapsed_ms = started.elapsed().as_millis();
    if total_elapsed_ms >= 100 {
        tracing::info!(
            user_id = %user_id,
            total_snapshots = total,
            returned_snapshots = snapshots.len(),
            limit = limit.max(0),
            offset = offset.max(0),
            store_elapsed_ms,
            visible_elapsed_ms,
            summaries_elapsed_ms,
            total_elapsed_ms,
            "snapshot_list_phase: list_payload"
        );
    } else {
        tracing::debug!(
            user_id = %user_id,
            total_snapshots = total,
            returned_snapshots = snapshots.len(),
            limit = limit.max(0),
            offset = offset.max(0),
            store_elapsed_ms,
            visible_elapsed_ms,
            summaries_elapsed_ms,
            total_elapsed_ms,
            "snapshot_list_phase: list_payload"
        );
    }
    Ok(json!({
        "snapshots": snapshots,
        "total": total,
        "limit": limit.max(0),
        "offset": offset.max(0),
        "has_more": has_more,
        "result": result,
    }))
}

fn is_safety_snapshot_name(sql: &Arc<memoria_storage::SqlMemoryStore>, name: &str) -> bool {
    if name.starts_with("mem_snap_pre_") {
        return true;
    }
    match sql.database_name() {
        Some(db_name) => {
            name.starts_with(&format!("mem_snap_{db_name}_pre_"))
                || name.starts_with(&compact_safety_prefix(db_name))
        }
        None => false,
    }
}

async fn resolve_snapshot_internal(
    state: &AppState,
    user_id: &str,
    name: &str,
) -> Result<String, (StatusCode, String)> {
    let sql = user_snapshot_store(state, user_id).await?;
    let git = user_git_service(&sql)?;

    let internal = if let Some(milestone) = milestone_internal(name) {
        milestone
    } else if is_safety_snapshot_name(&sql, name) {
        git.get_snapshot(name)
            .await
            .map_err(api_err)?
            .map(|_| name.to_string())
            .ok_or((StatusCode::NOT_FOUND, "Snapshot not found".into()))?
    } else if name.starts_with("mem_snap_") {
        sql.get_snapshot_registration_by_internal(user_id, name)
            .await
            .map_err(api_err)?
            .map(|r| r.snapshot_name)
            .ok_or((StatusCode::NOT_FOUND, "Snapshot not found".into()))?
    } else if let Some(internal) = sql
        .get_snapshot_registration(user_id, name)
        .await
        .map_err(api_err)?
        .map(|r| r.snapshot_name)
    {
        internal
    } else if name.starts_with("pre_") {
        git.list_snapshots()
            .await
            .map_err(api_err)?
            .into_iter()
            .find(|snapshot| {
                safety_snapshot_display_name(&snapshot.snapshot_name).as_deref() == Some(name)
            })
            .map(|snapshot| snapshot.snapshot_name)
            .ok_or((StatusCode::NOT_FOUND, "Snapshot not found".into()))?
    } else {
        return Err((StatusCode::NOT_FOUND, "Snapshot not found".into()));
    };

    let internal = validate_snapshot_identifier(&internal)?.to_string();
    git.get_snapshot(&internal)
        .await
        .map_err(api_err)?
        .ok_or((StatusCode::NOT_FOUND, "Snapshot not found".into()))?;
    Ok(internal)
}

pub async fn create_snapshot(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateSnapshotRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let result = git_call_text(
        &state,
        auth.scope_id(),
        "memory_snapshot",
        json!({ "name": req.name }),
    )
    .await?;
    let mut body = json!({
        "name": req.name.clone(),
        "description": req.description.clone(),
        "result": result.clone(),
    });
    if let Some((display_name, created_at)) =
        parse_created_snapshot_result(body["result"].as_str().unwrap_or_default())
    {
        body["name"] = json!(display_name.clone());
        body["created_at"] = json!(created_at.clone());
        body["timestamp"] = json!(created_at);

        let sql = user_snapshot_store(&state, auth.scope_id()).await?;
        let snapshots =
            memoria_mcp::git_tools::visible_snapshots_for_user(&state.service, auth.scope_id())
                .await
                .map_err(api_err_typed)?;
        if let Some(snapshot) = snapshots
            .iter()
            .find(|snapshot| snapshot.display_name == display_name)
        {
            body = snapshot_summary_value(&sql, auth.scope_id(), snapshot, true).await?;
            body["description"] = json!(req.description.clone());
            body["result"] = json!(result.clone());
        }
    }
    Ok((StatusCode::CREATED, Json(body)))
}

pub async fn list_snapshots(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(q): Query<ListSnapshotsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    Ok(Json(
        snapshot_list_payload(&state, auth.scope_id(), q).await?,
    ))
}

/// GET /v1/snapshots/:name — read snapshot detail with time-travel query
pub async fn get_snapshot(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<GetSnapshotQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = user_snapshot_store(&state, auth.scope_id()).await?;
    let pool = sql.pool();
    let table = sql.active_table(auth.scope_id()).await.map_err(api_err)?;

    let snap_name = resolve_snapshot_internal(&state, auth.scope_id(), &name).await?;
    let limit = q.limit.unwrap_or(50).min(500);
    let offset = q.offset.unwrap_or(0);
    let detail = q.detail.as_deref().unwrap_or("brief");

    // Total count via time-travel
    let count_sql = format!(
        "SELECT COUNT(*) as cnt FROM {table} {{SNAPSHOT = '{snap_name}'}} WHERE user_id = ? AND is_active > 0"
    );
    let total: i64 = sqlx::query_scalar(&count_sql)
        .bind(auth.scope_id())
        .fetch_one(pool)
        .await
        .map_err(api_err)?;

    // Type distribution
    let type_sql = format!(
        "SELECT memory_type, COUNT(*) as cnt FROM {table} {{SNAPSHOT = '{snap_name}'}} \
         WHERE user_id = ? AND is_active > 0 GROUP BY memory_type"
    );
    let type_rows = sqlx::query(&type_sql)
        .bind(auth.scope_id())
        .fetch_all(pool)
        .await
        .map_err(api_err)?;
    let by_type: serde_json::Map<String, serde_json::Value> = type_rows
        .iter()
        .map(|r| {
            let t: String = r.try_get("memory_type").unwrap_or_default();
            let c: i64 = r.try_get("cnt").unwrap_or(0);
            (t, json!(c))
        })
        .collect();

    // Paginated memories
    let content_limit: usize = match detail {
        "full" => 2000,
        "normal" => 200,
        _ => 80,
    };
    let mem_sql = format!(
        "SELECT memory_id, user_id, content, memory_type, trust_tier, initial_confidence, is_active, session_id, observed_at, created_at FROM {table} {{SNAPSHOT = '{snap_name}'}} \
          WHERE user_id = ? AND is_active > 0 ORDER BY observed_at DESC LIMIT ? OFFSET ?"
    );
    let rows = sqlx::query(&mem_sql)
        .bind(auth.scope_id())
        .bind(limit)
        .bind(offset)
        .fetch_all(pool)
        .await
        .map_err(api_err)?;

    let memories: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            let content: String = r.try_get("content").unwrap_or_default();
            let initial_confidence = r.try_get::<f64, _>("initial_confidence").unwrap_or(0.0);
            let truncated = if content.len() > content_limit {
                format!(
                    "{} [truncated]",
                    memoria_core::truncate_utf8(&content, content_limit)
                )
            } else {
                content
            };
            let mut m = json!({
                "memory_id": r.try_get::<String, _>("memory_id").unwrap_or_default(),
                "user_id": r.try_get::<String, _>("user_id").unwrap_or_default(),
                "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
                "content": truncated,
                "initial_confidence": initial_confidence,
                "is_active": r.try_get::<i8, _>("is_active").unwrap_or(1) != 0,
                "session_id": r.try_get::<Option<String>, _>("session_id").ok().flatten(),
                "observed_at": format_snapshot_timestamp(
                    r.try_get::<Option<chrono::NaiveDateTime>, _>("observed_at")
                        .ok()
                        .flatten(),
                ),
                "created_at": format_snapshot_timestamp(
                    r.try_get::<Option<chrono::NaiveDateTime>, _>("created_at")
                        .ok()
                        .flatten(),
                ),
                "trust_tier": r
                    .try_get::<String, _>("trust_tier")
                    .unwrap_or_else(|_| TrustTier::default().to_string()),
                "retrieval_score": Value::Null,
            });
            if detail == "full" {
                m["confidence"] = json!(initial_confidence);
            }
            m
        })
        .collect();

    Ok(Json(json!({
        "name": name,
        "snapshot_name": snap_name,
        "memory_count": total,
        "by_type": by_type,
        "memories": memories,
        "limit": limit,
        "offset": offset,
        "has_more": offset + limit < total,
    })))
}

/// GET /v1/snapshots/:name/diff — compare snapshot vs current state
pub async fn diff_snapshot(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<DiffSnapshotQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = user_snapshot_store(&state, auth.scope_id()).await?;
    let pool = sql.pool();
    let table = sql.active_table(auth.scope_id()).await.map_err(api_err)?;

    let snap_name = resolve_snapshot_internal(&state, auth.scope_id(), &name).await?;
    let limit = q.limit.unwrap_or(50).min(200);

    // Counts
    let snap_count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {table} {{SNAPSHOT = '{snap_name}'}} WHERE user_id = ? AND is_active > 0"
    )).bind(auth.scope_id()).fetch_one(pool).await.map_err(api_err)?;

    let curr_count: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {table} WHERE user_id = ? AND is_active > 0"
    ))
    .bind(auth.scope_id())
    .fetch_one(pool)
    .await
    .map_err(api_err)?;

    // Added (in current but not in snapshot)
    let added_sql = format!(
        "SELECT c.memory_id, c.content, c.memory_type, c.trust_tier FROM {table} c \
          LEFT JOIN {table} {{SNAPSHOT = '{snap_name}'}} s ON c.memory_id = s.memory_id AND s.is_active > 0 \
          WHERE c.user_id = ? AND c.is_active > 0 AND s.memory_id IS NULL LIMIT ?"
    );
    let added_rows = sqlx::query(&added_sql)
        .bind(auth.scope_id())
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(api_err)?;

    // Removed (in snapshot but not in current)
    let removed_sql = format!(
        "SELECT s.memory_id, s.content, s.memory_type, s.trust_tier FROM {table} {{SNAPSHOT = '{snap_name}'}} s \
          LEFT JOIN {table} c ON s.memory_id = c.memory_id AND c.is_active > 0 \
          WHERE s.user_id = ? AND s.is_active > 0 AND c.memory_id IS NULL LIMIT ?"
    );
    let removed_rows = sqlx::query(&removed_sql)
        .bind(auth.scope_id())
        .bind(limit)
        .fetch_all(pool)
        .await
        .map_err(api_err)?;

    let to_json = |rows: &[sqlx::mysql::MySqlRow]| -> Vec<serde_json::Value> {
        rows.iter()
            .map(|r| {
                json!({
                    "memory_id": r.try_get::<String, _>("memory_id").unwrap_or_default(),
                    "content": r.try_get::<String, _>("content").unwrap_or_default(),
                    "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
                    "trust_tier": r
                        .try_get::<String, _>("trust_tier")
                        .unwrap_or_else(|_| TrustTier::default().to_string()),
                })
            })
            .collect()
    };

    Ok(Json(json!({
        "snapshot_name": snap_name,
        "snapshot_count": snap_count,
        "current_count": curr_count,
        "added": to_json(&added_rows),
        "removed": to_json(&removed_rows),
    })))
}

pub async fn delete_snapshot(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    git_call(
        &state,
        auth.scope_id(),
        "memory_snapshot_delete",
        // The path contains one name. The tool splits string selectors on
        // commas, so pass an array to avoid treating one name as a list.
        json!({ "names": [name] }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn delete_snapshot_bulk(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(&state, auth.scope_id(), "memory_snapshot_delete", req).await?;
    Ok(Json(r))
}

pub async fn rollback(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(
        &state,
        auth.scope_id(),
        "memory_rollback",
        json!({ "name": name }),
    )
    .await?;
    Ok(Json(r))
}

pub async fn list_branches(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;
    let active_branch = sql
        .active_branch_name(auth.scope_id())
        .await
        .map_err(api_err)?;
    let mut branches = vec![json!({
        "name": "main",
        "active": active_branch == "main",
    })];
    for (name, _table_name, created_at) in
        sql.list_branches(auth.scope_id()).await.map_err(api_err)?
    {
        let created_at_str = format_snapshot_timestamp(created_at);
        branches.push(json!({
            "name": name,
            "active": name == active_branch,
            "created_at": created_at_str,
        }));
    }
    if !branches
        .iter()
        .any(|branch| branch["active"].as_bool().unwrap_or(false))
    {
        branches[0]["active"] = json!(true);
    }
    let result = format_branch_list_result(&branches);
    Ok(Json(json!({
        "branches": branches,
        "result": result,
    })))
}

pub async fn create_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CreateBranchRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, String)> {
    let r = git_call(
        &state,
        auth.scope_id(),
        "memory_branch",
        json!({
            "name": req.name,
            "from_snapshot": req.from_snapshot,
            "from_timestamp": req.from_timestamp,
        }),
    )
    .await?;
    Ok((StatusCode::CREATED, Json(r)))
}

pub async fn checkout_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(
        &state,
        auth.scope_id(),
        "memory_checkout",
        json!({ "name": name }),
    )
    .await?;
    Ok(Json(r))
}

pub async fn merge_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Json(req): Json<MergeRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(
        &state,
        auth.scope_id(),
        "memory_merge",
        json!({ "source": name, "strategy": req.strategy }),
    )
    .await?;
    Ok(Json(r))
}

pub async fn diff_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call(
        &state,
        auth.scope_id(),
        "memory_diff",
        json!({ "source": name }),
    )
    .await?;
    Ok(Json(r))
}

/// POST /v1/branches/:name/pick
///
/// Request body:
/// - target: optional target branch, defaults to main
/// - strategy: optional conflict strategy, defaults to fail
/// - selector:
///   - key_list { keys[] }
///   - snapshot_range { from_snapshot, to_snapshot }
///   - retrieve { query, top_k, min_score? }
/// - dry_run: optional preview output shaping { limit, offset, include_content_preview, include_scores }
///
/// Response:
/// - normal execution: { "result": "Picked ..." }
/// - dry_run preview: structured JSON summary + paginated candidates (never includes embeddings)
pub async fn pick_branch(
    State(state): State<AppState>,
    AuthUser { user_id, .. }: AuthUser,
    Path(name): Path<String>,
    Json(req): Json<PickRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let r = git_call_pick(
        &state,
        &user_id,
        json!({
            "source": name,
            "target": req.target,
            "strategy": req.strategy,
            "selector": req.selector,
            "dry_run": req.dry_run,
        }),
    )
    .await?;
    Ok(Json(r))
}

pub async fn diff_branch_items(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Query(q): Query<DiffBranchItemsQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    Ok(Json(
        branch_diff_items_payload(&state, auth.scope_id(), &name, q.limit.unwrap_or(100)).await?,
    ))
}

pub async fn apply_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
    Json(req): Json<ApplyBranchRequest>,
) -> Result<Json<memoria_git::ApplyResult>, (StatusCode, String)> {
    if name == "main" {
        return Err((StatusCode::BAD_REQUEST, "Cannot apply from main".into()));
    }
    let sql = user_snapshot_store(&state, auth.scope_id()).await?;
    let git = user_git_service(&sql)?;
    let branch_table = branch_table_name_raw(&sql, auth.scope_id(), &name).await?;
    let response = git
        .selective_apply(&branch_table, "mem_memories", auth.scope_id(), req)
        .await
        .map_err(api_err)?;
    let payload = json!({
        "actor_user_id": auth.user_id,
        "group_id": auth.group_id,
        "source_branch": name,
        "applied_adds": response.applied_adds.clone(),
        "applied_updates": response.applied_updates.clone(),
        "applied_removes": response.applied_removes.clone(),
        "applied_conflicts": response.applied_conflicts.clone(),
        "skipped_adds": response.skipped_adds.clone(),
        "skipped_updates": response.skipped_updates.clone(),
        "skipped_removes": response.skipped_removes.clone(),
        "skipped_conflicts": response.skipped_conflicts.clone(),
    })
    .to_string();
    state.service.send_edit_log(
        auth.scope_id(),
        "branch_apply",
        None,
        Some(&payload),
        "selective apply to main",
        None,
    );

    Ok(Json(response))
}

pub async fn delete_branch(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(name): Path<String>,
) -> Result<StatusCode, (StatusCode, String)> {
    git_call(
        &state,
        auth.scope_id(),
        "memory_branch_delete",
        json!({ "name": name }),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod snapshot_list_tests {
    use super::*;
    use memoria_mcp::git_tools::VisibleSnapshot;

    async fn closed_store() -> Arc<memoria_storage::SqlMemoryStore> {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy_with(sqlx::mysql::MySqlConnectOptions::new());
        pool.close().await;
        let mut store = memoria_storage::SqlMemoryStore::new(pool, 4, "test".into());
        store.set_db_name("snapshot_test".into());
        Arc::new(store)
    }

    fn snapshot(name: &str, registered: bool, count: Option<i64>) -> VisibleSnapshot {
        VisibleSnapshot {
            display_name: name.into(),
            internal_name: format!("mem_snap_snapshot_test_{name}"),
            memory_count: count,
            timestamp: None,
            registered,
            is_safety: false,
        }
    }

    #[test]
    fn snapshot_query_defaults_preserve_exact_counts_and_safety_visibility() {
        let query: ListSnapshotsQuery = serde_urlencoded::from_str("").unwrap();
        assert_eq!(query.limit, 20);
        assert_eq!(query.offset, 0);
        assert!(query.include_safety);
        assert!(query.resolve_memory_counts);

        let query: ListSnapshotsQuery = serde_urlencoded::from_str(
            "include_safety=false&resolve_memory_counts=false&limit=2&offset=1",
        )
        .unwrap();
        assert!(!query.include_safety);
        assert!(!query.resolve_memory_counts);
        assert_eq!((query.limit, query.offset), (2, 1));
        assert!(
            serde_urlencoded::from_str::<ListSnapshotsQuery>("include_safety=invalid").is_err()
        );
    }

    #[test]
    fn safety_filter_precedes_pagination_and_preserves_registered_pre_names() {
        let mut legacy = snapshot("pre_legacy", false, None);
        legacy.internal_name = "mem_snap_pre_legacy".into();
        legacy.is_safety = true;
        let mut milestone = snapshot("auto:checkpoint", false, None);
        milestone.internal_name = "mem_milestone_checkpoint".into();
        let mut compact = snapshot("pre_compact", false, None);
        compact.internal_name = format!("{}compact", compact_safety_prefix("snapshot_test"));
        compact.is_safety = true;
        let mut purge = snapshot("pre_purge", false, None);
        purge.is_safety = true;
        let all = vec![
            legacy,
            snapshot("first", true, Some(3)),
            purge,
            snapshot("pre_release", true, Some(4)),
            compact,
            milestone,
        ];
        let mut query = ListSnapshotsQuery {
            include_safety: false,
            resolve_memory_counts: false,
            limit: 1,
            offset: 1,
        };
        let (total, page) = snapshot_page(&all, &query);
        assert_eq!(total, 3);
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].display_name, "pre_release");

        query.offset = 2;
        let (_, page) = snapshot_page(&all, &query);
        assert_eq!(page[0].internal_name, "mem_milestone_checkpoint");
        query.offset = 20;
        assert!(snapshot_page(&all, &query).1.is_empty());

        query.include_safety = true;
        assert_eq!(snapshot_page(&all, &query).0, all.len());
    }

    #[test]
    fn safety_filter_uses_discovery_metadata_instead_of_reparsing_names() {
        let mut safety = snapshot("backup", false, None);
        safety.internal_name = "another_internal_naming_format".into();
        safety.is_safety = true;
        let mut registered = snapshot("pre_release", true, None);
        registered.internal_name = "mem_snap_pre_release".into();
        let all = vec![safety, registered];
        let query = ListSnapshotsQuery {
            include_safety: false,
            ..Default::default()
        };
        let (total, page) = snapshot_page(&all, &query);
        assert_eq!(total, 1);
        assert_eq!(page[0].display_name, "pre_release");
    }

    #[tokio::test]
    async fn cached_only_summaries_never_query_or_backfill_a_closed_database() {
        let sql = closed_store().await;
        for registered in [true, false] {
            for count in [None, Some(0), Some(7)] {
                let snapshot = snapshot("backup", registered, count);
                for _ in 0..2 {
                    let summary = snapshot_summary_value(&sql, "test_user", &snapshot, false)
                        .await
                        .expect("cached-only listing must not access the closed pool");
                    assert_eq!(summary["memory_count"], json!(count));
                    assert_eq!(summary["registered"], registered);
                    assert_eq!(summary["name"], "backup");
                }
            }
        }
    }

    #[tokio::test]
    async fn exact_count_mode_keeps_cached_counts_and_propagates_database_failures() {
        let sql = closed_store().await;
        let cached = snapshot("cached", true, Some(8));
        let summary = snapshot_summary_value(&sql, "test_user", &cached, true)
            .await
            .unwrap();
        assert_eq!(summary["memory_count"], 8);

        let legacy = snapshot("legacy", true, None);
        let error = snapshot_summary_value(&sql, "test_user", &legacy, true)
            .await
            .unwrap_err();
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
    }
}
