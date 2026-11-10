use axum::{extract::State, http::StatusCode, Json};
use memoria_service::{
    ConsolidationInput, ConsolidationStrategy, DefaultConsolidationStrategy, GovernanceStore,
};
use serde_json::json;
use tracing::warn;

use crate::{
    auth::{group_main_write_allowed_for_solo_owner, AuthUser},
    models::*,
    routes::memory::{api_err, api_err_typed},
    state::AppState,
};

pub async fn get_confidence_policy(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<memoria_core::ConfidenceDecayPolicy>, (StatusCode, String)> {
    let store = state.service.shared_sql_store().map_err(api_err_typed)?;
    Ok(Json(
        store
            .confidence_policy(auth.scope_id())
            .await
            .map_err(api_err_typed)?,
    ))
}

pub async fn set_confidence_policy(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(policy): Json<memoria_core::ConfidenceDecayPolicy>,
) -> Result<Json<memoria_core::ConfidenceDecayPolicy>, (StatusCode, String)> {
    if let Some(group) = auth.group_id.as_deref() {
        if !group_main_write_allowed_for_solo_owner(&state, group, &auth.user_id).await {
            return Err((
                StatusCode::FORBIDDEN,
                "confidence lifecycle requires the group owner".into(),
            ));
        }
    }
    policy.validate().map_err(api_err_typed)?;
    let store = state.service.shared_sql_store().map_err(api_err_typed)?;
    store
        .set_confidence_policy(auth.scope_id(), &policy)
        .await
        .map_err(api_err_typed)?;
    Ok(Json(policy))
}

#[derive(serde::Deserialize)]
pub struct ConfidenceInactiveQuery {
    subject_id: String,
    cursor: Option<String>,
    limit: Option<i64>,
}

pub async fn list_confidence_inactive(
    State(state): State<AppState>,
    auth: AuthUser,
    axum::extract::Query(query): axum::extract::Query<ConfidenceInactiveQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let limit = query.limit.unwrap_or(100);
    let cursor = query.cursor.as_deref().unwrap_or("");
    if query.subject_id.trim().is_empty()
        || query.subject_id.len() > 128
        || cursor.len() > 128
        || !(1..=500).contains(&limit)
    {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            "invalid inactive memory query".into(),
        ));
    }
    let store = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err_typed)?;
    let mut items = store
        .list_confidence_inactive(auth.scope_id(), &query.subject_id, cursor, limit + 1)
        .await
        .map_err(api_err_typed)?;
    let next = if items.len() > limit as usize {
        items.truncate(limit as usize);
        items.last().map(|m| m.memory_id.clone())
    } else {
        None
    };
    let items: Vec<MemoryResponse> = items.into_iter().map(Into::into).collect();
    Ok(Json(json!({"items": items, "next_cursor": next})))
}

pub async fn governance(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<GovernanceRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    if let Some(group_id) = auth.group_id.as_deref() {
        if !group_main_write_allowed_for_solo_owner(&state, group_id, &auth.user_id).await {
            return Err((
                StatusCode::FORBIDDEN,
                "governance is disabled in group mode because it mutates main memory state"
                    .to_string(),
            ));
        }
    }

    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;

    const COOLDOWN_SECS: i64 = 3600;
    if !req.force {
        if let Some(remaining) = sql
            .check_cooldown(auth.scope_id(), "governance", COOLDOWN_SECS)
            .await
            .map_err(api_err)?
        {
            return Ok(Json(
                json!({ "skipped": true, "cooldown_remaining_s": remaining }),
            ));
        }
    }
    let quarantined = sql
        .quarantine_low_confidence(auth.scope_id())
        .await
        .map_err(api_err)?;
    let cleaned = sql.cleanup_stale(auth.scope_id()).await.map_err(api_err)?;
    let orphan_graph_cleaned = if req.force {
        sql.cleanup_orphan_graph_data().await.unwrap_or_else(|e| {
            warn!("orphan graph cleanup failed: {e}");
            0
        })
    } else {
        match sql
            .check_cooldown(auth.scope_id(), "orphan_graph_cleanup", COOLDOWN_SECS)
            .await
        {
            Ok(Some(_)) => 0,
            Ok(None) => sql.cleanup_orphan_graph_data().await.unwrap_or_else(|e| {
                warn!("orphan graph cleanup failed: {e}");
                0
            }),
            Err(e) => {
                warn!("orphan graph cooldown check failed, proceeding: {e}");
                sql.cleanup_orphan_graph_data().await.unwrap_or_else(|e| {
                    warn!("orphan graph cleanup failed: {e}");
                    0
                })
            }
        }
    };
    if orphan_graph_cleaned > 0 {
        let _ = sql
            .set_cooldown(auth.scope_id(), "orphan_graph_cleanup")
            .await;
    }
    if quarantined > 0 {
        let payload = serde_json::json!({"quarantined": quarantined}).to_string();
        state.service.send_edit_log(
            auth.scope_id(),
            "governance:quarantine",
            None,
            Some(&payload),
            &format!("quarantined {quarantined}"),
            None,
        );
    }
    if cleaned > 0 {
        let payload = serde_json::json!({"cleaned_stale": cleaned}).to_string();
        state.service.send_edit_log(
            auth.scope_id(),
            "governance:cleanup_stale",
            None,
            Some(&payload),
            &format!("cleaned {cleaned}"),
            None,
        );
    }
    if orphan_graph_cleaned > 0 {
        let payload = serde_json::json!({"orphan_graph_cleaned": orphan_graph_cleaned}).to_string();
        state.service.send_edit_log(
            auth.scope_id(),
            "governance:cleanup_orphan_graph",
            None,
            Some(&payload),
            &format!("cleaned {orphan_graph_cleaned} orphan graph entries"),
            None,
        );
    }
    sql.set_cooldown(auth.scope_id(), "governance")
        .await
        .map_err(api_err)?;
    Ok(Json(
        json!({ "quarantined": quarantined, "cleaned_stale": cleaned, "orphan_graph_cleaned": orphan_graph_cleaned }),
    ))
}

pub async fn consolidate(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<GovernanceRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;

    const COOLDOWN_SECS: i64 = 1800;
    if !req.force {
        if let Some(remaining) = sql
            .check_cooldown(auth.scope_id(), "consolidate", COOLDOWN_SECS)
            .await
            .map_err(api_err)?
        {
            return Ok(Json(
                json!({ "skipped": true, "cooldown_remaining_s": remaining }),
            ));
        }
    }
    let graph = sql.graph_store();
    let result = DefaultConsolidationStrategy::default()
        .consolidate(
            &graph,
            &ConsolidationInput::for_user(auth.scope_id().to_string()),
        )
        .await
        .map_err(api_err)?;
    sql.set_cooldown(auth.scope_id(), "consolidate")
        .await
        .map_err(api_err)?;
    Ok(Json(json!({
        "status": result.status.as_str(),
        "conflicts_detected": result.metrics.get("consolidation.conflicts_detected").copied().unwrap_or(0.0) as i64,
        "orphaned_scenes": result.metrics.get("consolidation.orphaned_scenes").copied().unwrap_or(0.0) as i64,
        "promoted": result.metrics.get("trust.promoted_count").copied().unwrap_or(0.0) as i64,
        "demoted": result.metrics.get("trust.demoted_count").copied().unwrap_or(0.0) as i64,
        "warnings": result.warnings,
        "decision_count": result.decisions.len(),
    })))
}

pub async fn reflect(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<ReflectRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;

    if req.mode == "internal" && state.service.llm.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM_API_KEY not configured".to_string(),
        ));
    }

    const COOLDOWN_SECS: i64 = 7200;
    if req.mode != "candidates" && !req.force {
        if let Some(remaining) = sql
            .check_cooldown(auth.scope_id(), "reflect", COOLDOWN_SECS)
            .await
            .map_err(api_err)?
        {
            return Ok(Json(
                json!({ "skipped": true, "cooldown_remaining_s": remaining }),
            ));
        }
    }

    let graph = sql.graph_store();
    let clusters = memoria_mcp::tools::build_reflect_clusters(&graph, auth.scope_id())
        .await
        .map_err(api_err)?;

    if clusters.is_empty() {
        return Ok(Json(json!({ "candidates": [], "scenes_created": 0 })));
    }

    if req.mode == "candidates" || state.service.llm.is_none() {
        let candidates: Vec<serde_json::Value> = clusters.iter().map(|(signal, importance, mems)| {
            json!({
                "signal": signal,
                "importance": importance,
                "memories": mems.iter().map(|(mid, content, _)| json!({"memory_id": mid, "content": content})).collect::<Vec<_>>()
            })
        }).collect();
        return Ok(Json(json!({ "candidates": candidates })));
    }

    // LLM synthesis
    let llm = state.service.llm.as_ref().unwrap();
    let mut scenes_created = 0usize;
    for (_, _, mems) in &clusters {
        let experiences = mems
            .iter()
            .map(|(_, c, _)| format!("- {c}"))
            .collect::<Vec<_>>()
            .join("\n");
        let prompt = memoria_mcp::tools::reflection_prompt(&experiences, "");
        let msgs = vec![memoria_embedding::ChatMessage {
            role: "user".to_string(),
            content: prompt,
        }];
        let raw = match llm.chat(&msgs, 0.3, Some(400)).await {
            Ok(r) => r,
            Err(e) => {
                warn!("reflect LLM chat failed: {e}");
                continue;
            }
        };
        let start = raw.find('[').unwrap_or(raw.len());
        let end = raw.rfind(']').map(|i| i + 1).unwrap_or(raw.len());
        if start >= end {
            continue;
        }
        let items: Vec<serde_json::Value> = match serde_json::from_str(&raw[start..end]) {
            Ok(v) => v,
            Err(e) => {
                warn!("reflect LLM response parse failed: {e}");
                continue;
            }
        };
        for item in &items {
            let content = item["content"].as_str().unwrap_or("").trim().to_string();
            if content.is_empty() {
                continue;
            }
            let mt_str = item["type"].as_str().unwrap_or("semantic");
            let mt = memoria_core::MemoryType::from_str(mt_str)
                .unwrap_or(memoria_core::MemoryType::Semantic);
            let _ = state
                .service
                .store_memory(
                    auth.scope_id(),
                    &content,
                    mt,
                    None,
                    Some(memoria_core::TrustTier::T4Unverified),
                    None,
                    None,
                    None,
                    None,
                )
                .await;
            scenes_created += 1;
        }
    }
    sql.set_cooldown(auth.scope_id(), "reflect")
        .await
        .map_err(api_err)?;
    Ok(Json(
        json!({ "scenes_created": scenes_created, "candidates_found": clusters.len() }),
    ))
}

pub async fn extract_entities(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<ExtractEntitiesRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;

    if req.mode == "internal" && state.service.llm.is_none() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "LLM_API_KEY not configured".to_string(),
        ));
    }

    let graph = sql.graph_store();
    let unlinked = graph
        .get_unlinked_memories(auth.scope_id(), 50)
        .await
        .map_err(api_err)?;
    if unlinked.is_empty() {
        return Ok(Json(json!({ "status": "complete", "unlinked": 0 })));
    }

    if req.mode == "candidates" || state.service.llm.is_none() {
        let existing = graph
            .get_user_entities(auth.scope_id())
            .await
            .map_err(api_err)?;
        return Ok(Json(json!({
            "status": "candidates",
            "unlinked": unlinked.len(),
            "memories": unlinked.iter().map(|(mid, c)| json!({"memory_id": mid, "content": c})).collect::<Vec<_>>(),
            "existing_entities": existing.iter().map(|(n, t)| json!({"name": n, "entity_type": t})).collect::<Vec<_>>(),
        })));
    }

    let llm = state.service.llm.as_ref().unwrap();
    let mut total_created = 0usize;
    for (memory_id, content) in &unlinked {
        let prompt = memoria_mcp::tools::entity_extract_prompt(content);
        let msgs = vec![memoria_embedding::ChatMessage {
            role: "user".to_string(),
            content: prompt,
        }];
        let raw = match llm.chat(&msgs, 0.0, Some(300)).await {
            Ok(r) => r,
            Err(e) => {
                warn!("entity extract LLM failed: {e}");
                continue;
            }
        };
        let start = raw.find('[').unwrap_or(raw.len());
        let end = raw.rfind(']').map(|i| i + 1).unwrap_or(raw.len());
        if start >= end {
            continue;
        }
        let items: Vec<serde_json::Value> = match serde_json::from_str(&raw[start..end]) {
            Ok(v) => v,
            Err(e) => {
                warn!("entity extract parse failed: {e}");
                continue;
            }
        };
        let mut links: Vec<(String, String, &str)> = Vec::new();
        for item in &items {
            let name = item["name"].as_str().unwrap_or("").trim().to_lowercase();
            if name.is_empty() {
                continue;
            }
            let display = item["name"].as_str().unwrap_or("").trim().to_string();
            let etype = item["type"].as_str().unwrap_or("concept").to_string();
            if let Ok((entity_id, is_new)) = graph
                .upsert_entity(auth.scope_id(), &name, &display, &etype)
                .await
            {
                links.push((memory_id.to_string(), entity_id, "llm"));
                if is_new {
                    total_created += 1;
                }
            }
        }
        if !links.is_empty() {
            let refs: Vec<(&str, &str, &str)> = links
                .iter()
                .map(|(m, e, s)| (m.as_str(), e.as_str(), *s))
                .collect();
            let _ = graph
                .batch_upsert_memory_entity_links(auth.scope_id(), &refs)
                .await;
        }
    }
    Ok(Json(
        json!({ "status": "done", "total_memories": unlinked.len(), "entities_found": total_created }),
    ))
}

pub async fn link_entities(
    State(state): State<AppState>,
    auth: AuthUser,
    Json(req): Json<LinkEntitiesRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;
    let graph = sql.graph_store();
    let mut created = 0usize;
    let mut reused = 0usize;
    let mut links: Vec<(String, String, &str)> = Vec::new();
    for link in &req.entities {
        for ent in &link.entities {
            let name = ent.name.trim().to_lowercase();
            if name.is_empty() {
                continue;
            }
            let (entity_id, is_new) = graph
                .upsert_entity(auth.scope_id(), &name, &ent.name, &ent.entity_type)
                .await
                .map_err(api_err)?;
            links.push((link.memory_id.clone(), entity_id, "manual"));
            if is_new {
                created += 1;
            } else {
                reused += 1;
            }
        }
    }
    if !links.is_empty() {
        let refs: Vec<(&str, &str, &str)> = links
            .iter()
            .map(|(m, e, s)| (m.as_str(), e.as_str(), *s))
            .collect();
        graph
            .batch_upsert_memory_entity_links(auth.scope_id(), &refs)
            .await
            .map_err(api_err)?;
    }
    Ok(Json(
        json!({ "entities_created": created, "entities_reused": reused, "edges_created": created }),
    ))
}

pub async fn get_entities(
    State(state): State<AppState>,
    auth: AuthUser,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let sql = state
        .service
        .user_sql_store(auth.scope_id())
        .await
        .map_err(api_err)?;
    let graph = sql.graph_store();
    let entities = graph
        .get_user_entities(auth.scope_id())
        .await
        .map_err(api_err)?;
    Ok(Json(json!({
        "entities": entities.iter().map(|(n, t)| json!({"name": n, "entity_type": t})).collect::<Vec<_>>()
    })))
}

use std::str::FromStr;
