use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;
use sqlx::Row;

use crate::{auth::AuthUser, models::*, state::AppState};

use memoria_core::nullable_str_from_row;
use memoria_service::ListActiveOptions;

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;
pub fn api_err(e: impl std::fmt::Display) -> (StatusCode, String) {
    tracing::error!(error = %e, "internal server error");
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

/// Map MemoriaError to proper HTTP status codes.
pub fn api_err_typed(e: memoria_core::MemoriaError) -> (StatusCode, String) {
    use memoria_core::MemoriaError::*;
    let status = match &e {
        NotFound(_) => StatusCode::NOT_FOUND,
        Validation(_) | InvalidMemoryType(_) | InvalidTrustTier(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        Blocked(_) => StatusCode::FORBIDDEN,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    if status.is_server_error() {
        tracing::error!(error = %e, "internal server error");
    }
    (status, e.to_string())
}

async fn find_memory_any_user(
    state: &AppState,
    memory_id: &str,
    branch: Option<&str>,
) -> Result<Option<(String, memoria_core::Memory)>, (StatusCode, String)> {
    let shared = state.service.shared_sql_store().map_err(api_err_typed)?;
    let Some(router) = shared.db_router() else {
        if branch.is_some() {
            return Ok(None);
        }
        let memory = state.service.get(memory_id).await.map_err(api_err_typed)?;
        return Ok(memory.map(|m| (m.user_id.clone(), m)));
    };

    let user_ids = router.list_active_users().await.map_err(api_err_typed)?;
    for candidate in user_ids {
        let sql = state
            .service
            .user_sql_store(&candidate)
            .await
            .map_err(api_err_typed)?;
        let table = match sql.table_for_branch(&candidate, branch).await {
            Ok(table) => table,
            Err(memoria_core::MemoriaError::NotFound(_)) if branch.is_some() => continue,
            Err(err) => return Err(api_err_typed(err)),
        };
        if let Some(memory) = sql
            .get_from(&table, memory_id)
            .await
            .map_err(api_err_typed)?
        {
            return Ok(Some((candidate, memory)));
        }
    }

    Ok(None)
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub memory_type: Option<String>,
    pub session_id: Option<String>,
    pub subject_id: Option<String>,
    pub trust_tier: Option<String>,
    pub branch: Option<String>,
    #[serde(default = "default_limit")]
    pub limit: i64,
    pub cursor: Option<String>,
}
fn default_limit() -> i64 {
    100
}

#[derive(Deserialize, Default)]
pub struct BranchQuery {
    pub branch: Option<String>,
}

#[derive(Deserialize, Default)]
pub struct ProfileQuery {
    pub subject_id: Option<String>,
    pub branch: Option<String>,
}

fn branch_param(branch: Option<&str>) -> Option<&str> {
    branch.map(str::trim).filter(|branch| !branch.is_empty())
}

fn nonempty_param(s: Option<&str>) -> Option<&str> {
    s.map(str::trim).filter(|s| !s.is_empty())
}

fn normalize_branch(branch: Option<String>) -> Option<String> {
    branch
        .map(|branch| branch.trim().to_string())
        .filter(|branch| !branch.is_empty())
}

pub async fn health() -> &'static str {
    "ok"
}

pub async fn health_instance(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    let db_ok = if let Some(sql) = &state.service.sql_store {
        sqlx::query("SELECT 1").execute(sql.pool()).await.is_ok()
    } else {
        false
    };
    let status = if db_ok { "ok" } else { "degraded" };
    let code = if db_ok {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    (
        code,
        Json(serde_json::json!({
            "status": status,
            "instance_id": state.instance_id,
            "db": db_ok,
        })),
    )
}

pub async fn list_memories(
    State(state): State<AppState>,
    auth: AuthUser,
    Query(q): Query<ListQuery>,
) -> ApiResult<ListResponse> {
    let limit = q.limit.clamp(1, 500);
    // Cursor is a memory_id; validate it looks like a hex string
    let cursor = q
        .cursor
        .as_deref()
        .filter(|c| c.len() == 32 && c.chars().all(|ch| ch.is_ascii_hexdigit()));
    if let Some(tier) = q.trust_tier.as_deref() {
        parse_trust_tier(tier).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    }
    let fetch_limit = limit + 1;
    let mut memories = state
        .service
        .list_active_paged_on_branch(
            auth.scope_id(),
            branch_param(q.branch.as_deref()),
            ListActiveOptions {
                limit: fetch_limit,
                memory_type: nonempty_param(q.memory_type.as_deref()),
                session_id: nonempty_param(q.session_id.as_deref()),
                trust_tier: nonempty_param(q.trust_tier.as_deref()),
                cursor,
                subject_id: nonempty_param(q.subject_id.as_deref()),
            },
        )
        .await
        .map_err(api_err)?;
    let has_more = memories.len() > limit as usize;
    memories.truncate(limit as usize);
    let next_cursor = if has_more {
        memories.last().map(|m| m.memory_id.clone())
    } else {
        None
    };
    let items: Vec<MemoryResponse> = memories.into_iter().map(Into::into).collect();
    Ok(Json(ListResponse { items, next_cursor }))
}

pub async fn query_memories(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<StructuredQueryRequest>,
) -> ApiResult<ListResponse> {
    let branch = normalize_branch(req.branch.clone());
    let limit = req.limit;
    let mut options = req
        .structured_options()
        .map_err(|err| (StatusCode::UNPROCESSABLE_ENTITY, err))?;
    options.limit = limit + 1;

    let mut memories = state
        .service
        .query_active_structured_on_branch(auth.scope_id(), branch.as_deref(), &options)
        .await
        .map_err(api_err_typed)?;
    let has_more = memories.len() > limit as usize;
    memories.truncate(limit as usize);
    let next_cursor = has_more
        .then(|| memories.last().map(|memory| memory.memory_id.clone()))
        .flatten();
    let items = memories.into_iter().map(Into::into).collect();
    Ok(Json(ListResponse { items, next_cursor }))
}

pub async fn fulltext_search_memories(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<FulltextSearchRequest>,
) -> ApiResult<Vec<MemoryResponse>> {
    let branch = req
        .fulltext_branch()
        .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error))?;
    let options = req
        .fulltext_options()
        .map_err(|error| (StatusCode::UNPROCESSABLE_ENTITY, error))?;
    let memories = state
        .service
        .search_fulltext_structured_on_branch(
            auth.scope_id(),
            branch.as_deref(),
            &req.query,
            &options,
        )
        .await
        .map_err(api_err_typed)?;
    Ok(Json(memories.into_iter().map(Into::into).collect()))
}

pub async fn store_memory(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<StoreRequest>,
) -> Result<(StatusCode, Json<MemoryResponse>), (StatusCode, String)> {
    if req.content.is_empty() {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "content must not be empty".into(),
        ));
    }
    if req.content.len() > 32_768 {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "content exceeds 32 KiB limit".into(),
        ));
    }
    let mt =
        parse_memory_type(&req.memory_type).map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let tier = req
        .trust_tier
        .as_deref()
        .map(parse_trust_tier)
        .transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let observed_at = req
        .observed_at
        .as_deref()
        .map(|s| chrono::DateTime::parse_from_rfc3339(s).map(|dt| dt.with_timezone(&chrono::Utc)))
        .transpose()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e.to_string()))?;
    let author = if auth.group_id.is_some() {
        Some(auth.user_id.clone())
    } else {
        None
    };
    let m = state
        .service
        .store_memory_with_metadata_on_branch(
            auth.scope_id(),
            branch_param(req.branch.as_deref()),
            &req.content,
            mt,
            req.session_id,
            tier,
            observed_at,
            req.initial_confidence,
            author,
            req.subject_id,
            req.extra_metadata,
        )
        .await
        .map_err(|e| {
            if matches!(e, memoria_core::MemoriaError::Blocked(_)) {
                crate::metrics::registry().security.sensitivity_blocks.inc();
            }
            api_err_typed(e)
        })?;
    Ok((StatusCode::CREATED, Json(m.into())))
}

pub async fn batch_store(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<BatchStoreRequest>,
) -> Result<(StatusCode, Json<Vec<MemoryResponse>>), (StatusCode, String)> {
    if req.memories.len() > 100 {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "batch exceeds 100 items".into(),
        ));
    }
    for m in &req.memories {
        if m.content.len() > 32_768 {
            return Err((
                StatusCode::UNPROCESSABLE_ENTITY,
                "content exceeds 32 KiB limit".into(),
            ));
        }
    }
    let batch_subject_id = req.subject_id;
    let top_branch = normalize_branch(req.branch);
    let items: Vec<_> = req
        .memories
        .into_iter()
        .map(|r| {
            let item_branch = normalize_branch(r.branch);
            if item_branch.is_some() && item_branch.as_deref() != top_branch.as_deref() {
                return Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    "per-memory branch in batch must match the batch branch".to_string(),
                ));
            }
            let mt = parse_memory_type(&r.memory_type)
                .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e));
            let tier = r
                .trust_tier
                .as_deref()
                .map(parse_trust_tier)
                .transpose()
                .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e));
            // item-level subject_id takes priority; fall back to batch-level
            let subject_id = r.subject_id.or_else(|| batch_subject_id.clone());
            Ok((
                r.content,
                mt,
                tier,
                r.session_id,
                top_branch.clone(),
                subject_id,
                r.extra_metadata,
            ))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Validate all types upfront
    let mut validated = Vec::with_capacity(items.len());
    for (content, mt_result, tier_result, session_id, branch, subject_id, extra_metadata) in items {
        let mt = mt_result?;
        let tier = tier_result?;
        validated.push((content, mt, session_id, tier, branch, subject_id, extra_metadata));
    }

    let author = if auth.group_id.is_some() {
        Some(auth.user_id.clone())
    } else {
        None
    };
    let batch_items = validated
        .into_iter()
        .map(|(content, mt, session_id, tier, _branch, subject_id, extra_metadata)| {
            (content, mt, session_id, tier, subject_id, extra_metadata)
        })
        .collect();
    let results = state
        .service
        .store_batch_with_metadata_on_branch(
            auth.scope_id(),
            branch_param(top_branch.as_deref()),
            batch_items,
            author,
        )
        .await
        .map_err(api_err)?;
    Ok((
        StatusCode::CREATED,
        Json(results.into_iter().map(Into::into).collect()),
    ))
}

pub async fn retrieve(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<RetrieveRequest>,
) -> ApiResult<serde_json::Value> {
    req.validate()
        .map_err(|err| (StatusCode::UNPROCESSABLE_ENTITY, err))?;
    let top_k = req.top_k.clamp(1, 100);
    let level = memoria_service::ExplainLevel::from_str_or_bool(&req.explain);
    let retrieve_options = req
        .retrieve_options()
        .map_err(|err| (StatusCode::UNPROCESSABLE_ENTITY, err))?;

    if level != memoria_service::ExplainLevel::None {
        let (results, explain) = state
            .service
            .retrieve_explain_level_with_options_on_branch(
                auth.scope_id(),
                branch_param(req.branch.as_deref()),
                &req.query,
                top_k,
                level,
                &retrieve_options,
            )
            .await
            .map_err(api_err)?;
        let items: Vec<MemoryResponse> = results.into_iter().map(Into::into).collect();
        Ok(Json(
            serde_json::json!({"results": items, "explain": explain}),
        ))
    } else {
        let results = state
            .service
            .retrieve_with_options_on_branch(
                auth.scope_id(),
                branch_param(req.branch.as_deref()),
                &req.query,
                top_k,
                &retrieve_options,
            )
            .await
            .map_err(api_err)?;
        let items: Vec<MemoryResponse> = results.into_iter().map(Into::into).collect();
        Ok(Json(serde_json::json!(items)))
    }
}

pub async fn search(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<SearchRequest>,
) -> ApiResult<serde_json::Value> {
    req.validate()
        .map_err(|err| (StatusCode::UNPROCESSABLE_ENTITY, err))?;
    let top_k = req.top_k.clamp(1, 100);
    let level = memoria_service::ExplainLevel::from_str_or_bool(&req.explain);
    let retrieve_options = req
        .retrieve_options()
        .map_err(|err| (StatusCode::UNPROCESSABLE_ENTITY, err))?;
    if level != memoria_service::ExplainLevel::None {
        let (results, explain) = state
            .service
            .retrieve_explain_level_with_options_on_branch(
                auth.scope_id(),
                branch_param(req.branch.as_deref()),
                &req.query,
                top_k,
                level,
                &retrieve_options,
            )
            .await
            .map_err(api_err)?;
        let items: Vec<MemoryResponse> = results.into_iter().map(Into::into).collect();
        Ok(Json(
            serde_json::json!({"results": items, "explain": explain}),
        ))
    } else {
        let results = state
            .service
            .retrieve_with_options_on_branch(
                auth.scope_id(),
                branch_param(req.branch.as_deref()),
                &req.query,
                top_k,
                &retrieve_options,
            )
            .await
            .map_err(api_err)?;
        Ok(Json(serde_json::json!(results
            .into_iter()
            .map(Into::into)
            .collect::<Vec<MemoryResponse>>())))
    }
}

pub async fn get_memory(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    Query(q): Query<BranchQuery>,
) -> ApiResult<Option<MemoryResponse>> {
    let branch = branch_param(q.branch.as_deref());
    let owned = match state
        .service
        .get_for_user_on_branch(auth.scope_id(), branch, &id)
        .await
    {
        Ok(memory) => memory,
        Err(memoria_core::MemoriaError::NotFound(_)) if auth.is_master && branch.is_some() => None,
        Err(err) => return Err(api_err_typed(err)),
    };
    if let Some(memory) = owned {
        return Ok(Json(Some(memory.into())));
    }

    if !auth.is_master {
        return Ok(Json(None));
    }

    if let Some((_, memory)) = find_memory_any_user(&state, &id, branch).await? {
        return Ok(Json(Some(memory.into())));
    }

    Ok(Json(None))
}

pub async fn correct_memory(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    Json(req): Json<CorrectRequest>,
) -> ApiResult<MemoryResponse> {
    let effective_user_id = if auth.is_master {
        find_memory_any_user(&state, &id, branch_param(req.branch.as_deref()))
            .await?
            .map(|(owner_id, _)| owner_id)
            .unwrap_or_else(|| auth.scope_id().to_string())
    } else {
        if state
            .service
            .get_for_user_on_branch(auth.scope_id(), branch_param(req.branch.as_deref()), &id)
            .await
            .map_err(api_err_typed)?
            .is_none()
        {
            return Err((StatusCode::NOT_FOUND, "Memory not found".to_string()));
        }
        auth.scope_id().to_string()
    };
    let m = state
        .service
        .correct_on_branch(
            &effective_user_id,
            branch_param(req.branch.as_deref()),
            &id,
            &req.new_content,
        )
        .await
        .map_err(api_err_typed)?;
    Ok(Json(m.into()))
}

pub async fn correct_by_query(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<CorrectByQueryRequest>,
) -> ApiResult<MemoryResponse> {
    req.validate()
        .map_err(|err| (StatusCode::UNPROCESSABLE_ENTITY, err))?;
    let retrieve_options = req
        .retrieve_options()
        .map_err(|err| (StatusCode::UNPROCESSABLE_ENTITY, err))?;
    let results = state
        .service
        .retrieve_with_options_on_branch(
            auth.scope_id(),
            branch_param(req.branch.as_deref()),
            &req.query,
            1,
            &retrieve_options,
        )
        .await
        .map_err(api_err)?;
    let found = results.into_iter().next().ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "No matching memory found".to_string(),
        )
    })?;
    let m = state
        .service
        .correct_on_branch(
            auth.scope_id(),
            branch_param(req.branch.as_deref()),
            &found.memory_id,
            &req.new_content,
        )
        .await
        .map_err(api_err)?;
    Ok(Json(m.into()))
}

pub async fn delete_memory(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    Query(q): Query<BranchQuery>,
) -> Result<StatusCode, (StatusCode, String)> {
    let effective_user_id = if auth.is_master {
        find_memory_any_user(&state, &id, branch_param(q.branch.as_deref()))
            .await?
            .map(|(owner_id, _)| owner_id)
            .unwrap_or_else(|| auth.scope_id().to_string())
    } else {
        if state
            .service
            .get_for_user_on_branch(auth.scope_id(), branch_param(q.branch.as_deref()), &id)
            .await
            .map_err(api_err_typed)?
            .is_none()
        {
            return Err((StatusCode::NOT_FOUND, "Memory not found".to_string()));
        }
        auth.scope_id().to_string()
    };
    let _ = state
        .service
        .purge_on_branch(&effective_user_id, branch_param(q.branch.as_deref()), &id)
        .await
        .map_err(api_err_typed)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn purge_memories(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<PurgeRequest>,
) -> ApiResult<PurgeResponse> {
    let selector = req
        .selector()
        .map_err(|e| (StatusCode::UNPROCESSABLE_ENTITY, e))?;
    let branch = branch_param(req.branch.as_deref());
    let result = match selector {
        PurgeSelector::MemoryIds(ids) => {
            if !auth.is_master {
                for id in &ids {
                    let mem = state
                        .service
                        .get_for_user_on_branch(auth.scope_id(), branch, id)
                        .await
                        .map_err(api_err)?
                        .ok_or_else(|| {
                            (StatusCode::NOT_FOUND, format!("Memory not found: {id}"))
                        })?;
                    if !auth.is_group_scoped() && mem.user_id != auth.user_id {
                        return Err((StatusCode::FORBIDDEN, format!("Not your memory: {id}")));
                    }
                }
            }
            let id_refs: Vec<&str> = ids.iter().map(|s| s.as_str()).collect();
            state
                .service
                .purge_batch_on_branch(auth.scope_id(), branch, &id_refs)
                .await
                .map_err(api_err)?
        }
        PurgeSelector::Topic(topic) => state
            .service
            .purge_by_topic_on_branch(auth.scope_id(), branch, &topic)
            .await
            .map_err(api_err)?,
        PurgeSelector::Session {
            session_id,
            memory_types,
        } => state
            .service
            .purge_by_session_id_on_branch(
                auth.scope_id(),
                branch,
                &session_id,
                memory_types.as_deref(),
            )
            .await
            .map_err(api_err)?,
        PurgeSelector::None => memoria_service::PurgeResult {
            purged: 0,
            snapshot_name: None,
            warning: None,
        },
    };
    Ok(Json(PurgeResponse {
        purged: result.purged,
        snapshot_name: result.snapshot_name,
        warning: result.warning,
    }))
}

pub async fn get_profile(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(target): Path<String>,
    Query(q): Query<ProfileQuery>,
) -> ApiResult<serde_json::Value> {
    let resolved = if target == "me" || target == auth.user_id {
        auth.scope_id().to_string()
    } else if auth.is_master {
        target
    } else {
        return Err((
            StatusCode::FORBIDDEN,
            "Cannot access other users' profiles".to_string(),
        ));
    };
    let sql = state
        .service
        .user_sql_store(&resolved)
        .await
        .map_err(api_err)?;
    let branch = branch_param(q.branch.as_deref());
    // Use the same branch-resolved table for both the profile listing and the
    // stats aggregation so that both halves of the response are consistent.
    let table = sql
        .table_for_branch(&resolved, branch)
        .await
        .map_err(api_err_typed)?;
    let subject_id = q
        .subject_id
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let memories = state
        .service
        .list_active_paged_on_branch(
            &resolved,
            branch,
            ListActiveOptions {
                limit: 50,
                memory_type: Some("profile"),
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id,
            },
        )
        .await
        .map_err(api_err)?;
    let profile: Vec<_> = memories
        .iter()
        .map(|m| m.content.as_str())
        .collect();

    // Stats enrichment follows the same subject_id scope as the profile query.
    let stats_rows = if let Some(sid) = subject_id {
        let q = format!(
            "SELECT memory_type, COUNT(*) as cnt, \
             CAST(ROUND(AVG(initial_confidence), 2) AS DOUBLE) as avg_conf, \
             MIN(observed_at) as oldest, MAX(observed_at) as newest \
             FROM {table} WHERE user_id = ? AND subject_id = ? AND is_active = 1 GROUP BY memory_type"
        );
        sqlx::query(&q).bind(&resolved).bind(sid).fetch_all(sql.pool()).await
    } else {
        let q = format!(
            "SELECT memory_type, COUNT(*) as cnt, \
             CAST(ROUND(AVG(initial_confidence), 2) AS DOUBLE) as avg_conf, \
             MIN(observed_at) as oldest, MAX(observed_at) as newest \
             FROM {table} WHERE user_id = ? AND is_active = 1 GROUP BY memory_type"
        );
        sqlx::query(&q).bind(&resolved).fetch_all(sql.pool()).await
    };
    let stats: serde_json::Value = stats_rows
    .map(|rows| {
        let mut by_type = serde_json::Map::new();
        let mut total = 0i64;
        let mut oldest: Option<String> = None;
        let mut newest: Option<String> = None;
        let mut conf_sum = 0.0f64;
        let mut conf_n = 0i64;
        for r in &rows {
            let mt: String = r.try_get("memory_type").unwrap_or_default();
            let cnt: i64 = r.try_get("cnt").unwrap_or(0);
            by_type.insert(mt, serde_json::json!(cnt));
            total += cnt;
            match r.try_get::<Option<f64>, _>("avg_conf") {
                Ok(Some(c)) => {
                    conf_sum += c * cnt as f64;
                    conf_n += cnt;
                }
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    error = %error,
                    "failed to decode profile average confidence"
                ),
            }
            if let Ok(Some(d)) = r.try_get::<Option<chrono::NaiveDateTime>, _>("oldest") {
                let s = d.to_string();
                if oldest.as_ref().is_none_or(|o| s < *o) { oldest = Some(s); }
            }
            if let Ok(Some(d)) = r.try_get::<Option<chrono::NaiveDateTime>, _>("newest") {
                let s = d.to_string();
                if newest.as_ref().is_none_or(|n| s > *n) { newest = Some(s); }
            }
        }
        let avg_conf = if conf_n > 0 { ((conf_sum / conf_n as f64) * 100.0).round() / 100.0 } else { 0.0 };
        serde_json::json!({"by_type": by_type, "total": total, "avg_confidence": avg_conf, "oldest": oldest, "newest": newest})
    }).unwrap_or_else(|_| serde_json::json!({}));

    Ok(Json(
        serde_json::json!({"user_id": resolved, "profile": profile.join("\n"), "stats": stats}),
    ))
}

#[derive(serde::Deserialize)]
pub struct ObserveRequest {
    pub messages: Vec<serde_json::Value>,
    pub source_event_ids: Option<Vec<String>>,
    pub session_id: Option<String>,
    pub subject_id: Option<String>,
    pub branch: Option<String>,
    #[serde(default)]
    pub exclude_memory_ids: Vec<String>,
}

/// Extract and store memories from a conversation turn.
/// With LLM: uses structured extraction (type, content, confidence).
/// Without LLM: stores each non-empty assistant/user message as a semantic memory.
pub async fn observe_turn(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<ObserveRequest>,
) -> ApiResult<serde_json::Value> {
    if !req.exclude_memory_ids.is_empty() {
        auth.require_scope(crate::auth::SCOPE_MEMORY_READ)?;
    }
    observe_result(&state, &auth, req).await.map_err(api_err)
}

async fn observe_result(
    state: &AppState,
    auth: &AuthUser,
    req: ObserveRequest,
) -> Result<Json<serde_json::Value>, memoria_core::MemoriaError> {
    let (memories, has_llm) = state
        .service
        .observe_turn_excluding_on_branch(
            auth.scope_id(),
            branch_param(req.branch.as_deref()),
            &req.messages,
            req.session_id,
            req.subject_id,
            &req.exclude_memory_ids,
        )
        .await?;

    let stored: Vec<_> = memories
        .iter()
        .map(|m| {
            serde_json::json!({
                "memory_id": m.memory_id,
                "content": m.content,
                "memory_type": m.memory_type.to_string(),
            })
        })
        .collect();

    let mut result = serde_json::json!({ "memories": stored });
    if !has_llm {
        result["warning"] = serde_json::json!("LLM not configured — storing messages as-is");
    }
    Ok(Json(result))
}

/// Explicit exclusion support is a separate route so old deployments cannot
/// silently ignore the new request field and write duplicate memories.
pub async fn observe_turn_deduplicated(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<ObserveRequest>,
) -> Response {
    if req.exclude_memory_ids.is_empty() {
        return deduplicated_response(
            (
                StatusCode::UNPROCESSABLE_ENTITY,
                "exclude_memory_ids is required",
            )
                .into_response(),
        );
    }
    if let Err(error) = auth.require_scope(crate::auth::SCOPE_MEMORY_READ) {
        return deduplicated_response(error.into_response());
    }
    let response = match observe_result(&state, &auth, req).await {
        Ok(result) => result.into_response(),
        Err(memoria_core::MemoriaError::ObserveExtractionUnavailable(error)) => {
            tracing::warn!(error, "observe extraction failed before persistence");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [("x-memoria-observe-error", "extraction_unavailable")],
                Json(serde_json::json!({"error": "observe_extraction_unavailable"})),
            )
                .into_response()
        }
        Err(error) => api_err_typed(error).into_response(),
    };
    deduplicated_response(response)
}

fn deduplicated_response(mut response: Response) -> Response {
    // Route-generated responses distinguish missing resources from an old server's
    // missing route. The extraction_unavailable marker alone certifies no writes.
    response.headers_mut().insert(
        "x-memoria-observe-deduplicated",
        axum::http::HeaderValue::from_static("1"),
    );
    response
}

/// GET /v1/memories/:id/history — version chain via superseded_by links.
pub async fn get_memory_history(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(id): Path<String>,
    Query(q): Query<BranchQuery>,
) -> ApiResult<serde_json::Value> {
    use sqlx::Row;

    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;
    let table = sql
        .table_for_branch(auth.scope_id(), branch_param(q.branch.as_deref()))
        .await
        .map_err(api_err_typed)?;

    let mut chain = Vec::new();
    let mut visited = std::collections::HashSet::new();

    // Walk forward from the given id following superseded_by
    let mut current_id = Some(id.clone());
    while let Some(cid) = current_id {
        if !visited.insert(cid.clone()) {
            break;
        }
        let row = sqlx::query(&format!(
            "SELECT memory_id, content, is_active, superseded_by, observed_at, memory_type \
                 FROM {} WHERE memory_id = ? AND user_id = ?",
            table
        ))
        .bind(&cid)
        .bind(auth.scope_id())
        .fetch_optional(sql.pool())
        .await
        .map_err(api_err)?;

        match row {
            Some(r) => {
                let mid: String = r.try_get("memory_id").unwrap_or_default();
                let sup: Option<String> =
                    nullable_str_from_row(r.try_get("superseded_by").ok().flatten());
                chain.push(serde_json::json!({
                    "memory_id": mid,
                    "content": r.try_get::<String, _>("content").unwrap_or_default(),
                    "is_active": r.try_get::<i8, _>("is_active").unwrap_or(0) != 0,
                    "superseded_by": sup,
                    "observed_at": r.try_get::<Option<String>, _>("observed_at").ok().flatten(),
                    "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
                }));
                current_id = sup;
            }
            None => {
                if chain.is_empty() {
                    return Err((StatusCode::NOT_FOUND, "Memory not found".to_string()));
                }
                break;
            }
        }
    }

    // Walk backwards: find older versions that point to our root
    if let Some(root_id) = chain.first().and_then(|v| v["memory_id"].as_str()) {
        let mut prev_id = root_id.to_string();
        loop {
            let older = sqlx::query(&format!(
                "SELECT memory_id, content, is_active, superseded_by, observed_at, memory_type \
                     FROM {} WHERE superseded_by = ? AND user_id = ?",
                table
            ))
            .bind(&prev_id)
            .bind(auth.scope_id())
            .fetch_optional(sql.pool())
            .await
            .map_err(api_err)?;

            match older {
                Some(r) => {
                    let mid: String = r.try_get("memory_id").unwrap_or_default();
                    if !visited.insert(mid.clone()) {
                        break;
                    }
                    prev_id = mid.clone();
                    chain.insert(0, serde_json::json!({
                        "memory_id": mid,
                        "content": r.try_get::<String, _>("content").unwrap_or_default(),
                        "is_active": r.try_get::<i8, _>("is_active").unwrap_or(0) != 0,
                        "superseded_by": r.try_get::<Option<String>, _>("superseded_by").ok().flatten(),
                        "observed_at": r.try_get::<Option<String>, _>("observed_at").ok().flatten(),
                        "memory_type": r.try_get::<String, _>("memory_type").unwrap_or_default(),
                    }));
                }
                None => break,
            }
        }
    }

    Ok(Json(serde_json::json!({
        "memory_id": id,
        "versions": chain,
        "total": chain.len(),
    })))
}

// ── Pipeline ──────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct PipelineRequest {
    pub candidates: Vec<PipelineCandidate>,
    pub sandbox_query: Option<String>,
}

#[derive(serde::Deserialize)]
pub struct PipelineCandidate {
    pub content: String,
    pub memory_type: Option<String>,
    pub trust_tier: Option<String>,
}

pub async fn run_pipeline(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<PipelineRequest>,
) -> ApiResult<serde_json::Value> {
    use crate::models::{parse_memory_type, parse_trust_tier};
    use memoria_core::MemoryType;
    use memoria_service::MemoryPipeline;

    let candidates = req
        .candidates
        .into_iter()
        .map(|c| {
            let mt = c
                .memory_type
                .as_deref()
                .map(|s| parse_memory_type(s).unwrap_or(MemoryType::Semantic))
                .unwrap_or(MemoryType::Semantic);
            let tier = c
                .trust_tier
                .as_deref()
                .map(parse_trust_tier)
                .transpose()
                .ok()
                .flatten();
            (c.content, mt, tier)
        })
        .collect();

    let pipeline = MemoryPipeline::new(state.service.clone(), Some(state.git.clone()));
    let result = pipeline
        .run(auth.scope_id(), candidates, req.sandbox_query.as_deref())
        .await;

    Ok(Json(serde_json::json!({
        "memories_stored": result.memories_stored,
        "memories_rejected": result.memories_rejected,
        "memories_redacted": result.memories_redacted,
        "errors": result.errors,
    })))
}

// ── Feedback ──────────────────────────────────────────────────────────────────

#[derive(serde::Deserialize)]
pub struct FeedbackRequest {
    pub signal: String,
    pub context: Option<String>,
}

#[derive(serde::Serialize)]
pub struct FeedbackResponse {
    pub feedback_id: String,
    pub memory_id: String,
    pub signal: String,
}

/// POST /v1/memories/:id/feedback — record explicit relevance feedback.
pub async fn record_feedback(
    State(state): State<AppState>,
    auth: AuthUser,
    Path(memory_id): Path<String>,
    Json(req): Json<FeedbackRequest>,
) -> Result<(StatusCode, Json<FeedbackResponse>), (StatusCode, String)> {
    let feedback_id = state
        .service
        .record_feedback(
            auth.scope_id(),
            &memory_id,
            &req.signal,
            req.context.as_deref(),
        )
        .await
        .map_err(api_err_typed)?;
    Ok((
        StatusCode::CREATED,
        Json(FeedbackResponse {
            feedback_id,
            memory_id,
            signal: req.signal,
        }),
    ))
}

/// GET /v1/feedback/stats — get feedback statistics for the user.
pub async fn get_feedback_stats(
    State(state): State<AppState>,
    auth: AuthUser,
) -> ApiResult<serde_json::Value> {
    let stats = state
        .service
        .get_feedback_stats(auth.scope_id())
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::json!(stats)))
}

/// GET /v1/feedback/by-tier — get feedback breakdown by trust tier.
pub async fn get_feedback_by_tier(
    State(state): State<AppState>,
    auth: AuthUser,
) -> ApiResult<serde_json::Value> {
    let breakdown = state
        .service
        .get_feedback_by_tier(auth.scope_id())
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::json!({"breakdown": breakdown})))
}

/// GET /v1/retrieval-params — get user's adaptive retrieval parameters.
pub async fn get_retrieval_params(
    State(state): State<AppState>,
    auth: AuthUser,
) -> ApiResult<serde_json::Value> {
    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;
    let params = sql
        .get_user_retrieval_params(auth.scope_id())
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::to_value(params).unwrap()))
}

#[derive(Debug, serde::Deserialize)]
pub struct SetRetrievalParamsRequest {
    pub feedback_weight: Option<f64>,
    pub temporal_decay_hours: Option<f64>,
    pub confidence_weight: Option<f64>,
}

/// PUT /v1/retrieval-params — update user's adaptive retrieval parameters.
pub async fn set_retrieval_params(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<SetRetrievalParamsRequest>,
) -> ApiResult<serde_json::Value> {
    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;

    let mut params = sql
        .get_user_retrieval_params(auth.scope_id())
        .await
        .map_err(api_err)?;

    if let Some(v) = req.feedback_weight {
        params.feedback_weight = v.clamp(0.01, 0.5);
    }
    if let Some(v) = req.temporal_decay_hours {
        params.temporal_decay_hours = v.clamp(1.0, 720.0);
    }
    if let Some(v) = req.confidence_weight {
        params.confidence_weight = v.clamp(0.0, 0.5);
    }

    sql.set_user_retrieval_params(&params)
        .await
        .map_err(api_err)?;
    Ok(Json(serde_json::to_value(params).unwrap()))
}

/// POST /v1/retrieval-params/tune — trigger auto-tuning of retrieval parameters.
pub async fn tune_retrieval_params(
    State(state): State<AppState>,
    auth: AuthUser,
) -> ApiResult<serde_json::Value> {
    use memoria_service::scoring::{DefaultScoringPlugin, ScoringPlugin};

    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;

    let old_params = sql
        .get_user_retrieval_params(auth.scope_id())
        .await
        .map_err(api_err)?;

    let plugin = DefaultScoringPlugin;
    match plugin
        .tune_params(sql.as_ref(), auth.scope_id())
        .await
        .map_err(api_err)?
    {
        Some(new_params) => Ok(Json(serde_json::json!({
            "tuned": true,
            "old_params": old_params,
            "new_params": new_params
        }))),
        None => Ok(Json(serde_json::json!({
            "tuned": false,
            "message": "Not enough feedback to tune parameters (minimum 10 feedback signals required)",
            "current_params": old_params
        }))),
    }
}

pub async fn get_tool_usage(
    State(state): State<AppState>,
    auth: AuthUser,
) -> ApiResult<serde_json::Value> {
    let mut usage = state.tool_usage_batcher.get_user_tool_usage(&auth.user_id);
    if usage.is_empty() {
        state
            .tool_usage_batcher
            .load_user_from_db(&state.service, &auth.user_id)
            .await;
        usage = state.tool_usage_batcher.get_user_tool_usage(&auth.user_id);
    }
    let items: Vec<serde_json::Value> = usage
        .into_iter()
        .map(|(tool, ts)| serde_json::json!({"tool_name": tool, "last_used_at": ts.to_rfc3339()}))
        .collect();
    Ok(Json(serde_json::json!(items)))
}
