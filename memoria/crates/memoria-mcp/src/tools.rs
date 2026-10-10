//! 8 core MCP tools for Phase 2.
//! Phase 4 will add 14 more (Git-for-Data, admin, graph).

use crate::purge_args::parse_memory_purge_args;
use crate::tool_result::error as mcp_error;
use anyhow::{Context, Result};
use memoria_core::{MemoryType, TrustTier};
use memoria_git::GitForDataService;
use memoria_service::{
    ConsolidationInput, ConsolidationStrategy, DefaultConsolidationStrategy, ListActiveOptions,
    MemoryService,
};
use memoria_storage::SqlMemoryStore;
use serde_json::{json, Value};
use sqlx::Row;
use std::str::FromStr;
use std::sync::Arc;
use uuid::Uuid;

async fn user_sql_store(
    service: &Arc<MemoryService>,
    user_id: &str,
) -> Result<Arc<SqlMemoryStore>> {
    service
        .user_sql_store(user_id)
        .await
        .map_err(anyhow::Error::from)
}

fn git_for_store(sql: &Arc<SqlMemoryStore>) -> Option<GitForDataService> {
    sql.database_name()
        .map(|db_name| GitForDataService::new(sql.pool().clone(), db_name.to_string()))
}

fn parse_session_scope_arg(args: &Value) -> Result<Option<memoria_service::SessionScope>> {
    args.get("session_scope")
        .and_then(Value::as_str)
        .map(memoria_service::SessionScope::from_str)
        .transpose()
        .map_err(crate::tool_result::input_error)
}

fn parse_retrieve_options_arg(args: &Value) -> Result<memoria_service::RetrieveOptions> {
    let session_id = args
        .get("session_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty());
    let session_scope = parse_session_scope_arg(args)?;
    if session_scope.is_some() && session_id.is_none() {
        return Err(crate::tool_result::input_error(
            "session_id is required when session_scope is set",
        ));
    }
    let subject_id = args
        .get("subject_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from);
    let memory_types: Option<Vec<memoria_core::MemoryType>> = match args.get("memory_types") {
        None | Some(Value::Null) => None,
        Some(v) => {
            let arr = v.as_array().ok_or_else(|| {
                crate::tool_result::input_error(format!("memory_types must be an array, got: {v}"))
            })?;
            let types = arr
                .iter()
                .map(|v| {
                    let s = v.as_str().ok_or_else(|| {
                        crate::tool_result::input_error(format!(
                            "memory_types elements must be strings, got: {v}"
                        ))
                    })?;
                    let trimmed = s.trim();
                    if trimmed.is_empty() {
                        return Ok(None);
                    }
                    memoria_core::MemoryType::from_str(trimmed)
                        .map(Some)
                        .map_err(|_| {
                            crate::tool_result::input_error(format!(
                                "unknown memory_type: '{trimmed}'"
                            ))
                        })
                })
                .filter_map(|r: Result<Option<_>, _>| r.transpose())
                .collect::<Result<Vec<_>, _>>()?;
            Some(types).filter(|v| !v.is_empty())
        }
    };
    Ok(
        memoria_service::RetrieveOptions::from_session_scope(session_id, session_scope)
            .with_subject_id(subject_id)
            .with_memory_types(memory_types),
    )
}

fn branch_arg(args: &Value) -> Option<&str> {
    args.get("branch")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|branch| !branch.is_empty())
}

fn parse_required_str(args: &Value, key: &str, err: &'static str) -> Result<String, &'static str> {
    let raw = args[key].as_str().unwrap_or("");
    if raw.trim().is_empty() {
        Err(err)
    } else {
        Ok(raw.to_string())
    }
}

fn parse_store_content(args: &Value) -> Result<String, &'static str> {
    parse_required_str(args, "content", "content is required")
}

/// 校验各工具的必填非空字符串参数。embedded 分发在各 handler 内部已隐式校验（parse_required_str），
/// 但 remote 模式直接拼 REST payload 会绕过校验——remote::call 前置调用本函数以保持一致。
pub fn validate_tool_args(name: &str, args: &Value) -> Result<(), &'static str> {
    // extra_metadata 若存在必须是 object（与 REST StoreRequest 一致）；非 object 明确拒绝，
    // 不能静默丢弃当作缺失。
    if let Some(v) = args.get("extra_metadata") {
        if !v.is_null() && !v.is_object() {
            return Err("extra_metadata must be an object");
        }
    }
    match name {
        "memory_store" => parse_required_str(args, "content", "content is required").map(|_| ()),
        "memory_retrieve" | "memory_search" => {
            parse_required_str(args, "query", "query is required").map(|_| ())
        }
        "memory_correct" => {
            parse_required_str(args, "new_content", "new_content is required").map(|_| ())
        }
        _ => Ok(()),
    }
}

fn parse_retrieve_query(args: &Value) -> Result<String, &'static str> {
    parse_required_str(args, "query", "query is required")
}

enum ToolCallName {
    MemoryStore,
    MemoryRetrieve,
    MemorySearch,
    MemoryCorrect,
    MemoryPurge,
    MemoryProfile,
    MemoryList,
    MemoryCapabilities,
    MemoryGovernance,
    MemoryRebuildIndex,
    MemoryConsolidate,
    MemoryReflect,
    MemoryExtractEntities,
    MemoryLinkEntities,
    MemoryFeedback,
    MemoryGetRetrievalParams,
    MemoryTuneParams,
    MemoryObserve,
    Unknown(String),
}

const MEMORY_STORE_DESCRIPTION: &str = concat!(
    "Store a new memory. Prefer a distilled, durable fact over information that is only true ",
    "in the moment (e.g. extract the fact behind a live query, not the live answer itself), and ",
    "extract the relevant fact from raw or structured content (JSON, logs, tool output) rather ",
    "than storing it verbatim. ",
    "Set trust_tier explicitly when certainty matters: ",
    "T1 for directly stated or explicitly confirmed facts/preferences/decisions, ",
    "T2 for curated or corrected records, T3 for inferred summaries or soft conclusions ",
    "(prefer T3 if unsure), T4 for speculative or unverified hypotheses."
);

const TRUST_TIER_DESCRIPTION: &str = concat!(
    "Use exact values T1/T2/T3/T4 only. ",
    "T1 = direct user-stated or explicitly confirmed fact/preference/decision. ",
    "T2 = curated or corrected memory replacing an older record. ",
    "T3 = inferred summary, extracted pattern, or soft conclusion not explicitly confirmed; ",
    "prefer T3 if unsure. ",
    "T4 = speculative, reflective, or otherwise unverified hypothesis."
);

pub(crate) const MEMORY_CAPABILITIES_TEXT: &str = concat!(
    "Available tools: memory_store, memory_retrieve, memory_search, ",
    "memory_correct, memory_purge, memory_profile, memory_list, ",
    "memory_capabilities, memory_governance, memory_consolidate, ",
    "memory_reflect, memory_feedback",
    "\n\nmemory_store trust_tier guide:",
    "\n- T1 (Verified): directly stated or explicitly confirmed facts, preferences, or decisions.",
    "\n- T2 (Curated): corrected or manually curated memory replacing an older record.",
    "\n- T3 (Inferred): summaries, extracted patterns, or soft conclusions not explicitly confirmed. Prefer T3 if unsure.",
    "\n- T4 (Unverified): speculative, reflective, or otherwise unverified hypotheses.",
    "\nUse exact values T1/T2/T3/T4; natural-language labels like 'verified' are invalid."
);

pub fn list() -> Value {
    json!([
        {
            "name": "memory_store",
            "description": MEMORY_STORE_DESCRIPTION,
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": {"type": "string"},
                    "memory_type": {"type": "string", "default": "semantic"},
                    "session_id": {"type": "string"},
                    "extra_metadata": {"type": "object", "description": "Optional business metadata (e.g. scene, agent). Persisted with the memory and returned verbatim on read. Memoria itself does not score by it; downstream consumers may use it for their own retrieval-time ranking."},
                    "subject_id": {"type": "string", "description": "Stable business ID of the memory subject (e.g. end-user ID). Set by the integration layer; optional."},
                    "branch": {"type": "string", "description": "Optional branch to read/write without changing the active checkout"},
                    "trust_tier": {
                        "type": "string",
                        "enum": ["T1", "T2", "T3", "T4"],
                        "description": TRUST_TIER_DESCRIPTION
                    }
                },
                "required": ["content"]
            }
        },
        {
            "name": "memory_retrieve",
            "description": "Retrieve relevant memories for a query, optionally scoped to a session",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "top_k": {"type": "integer", "default": 5},
                    "session_id": {"type": "string"},
                    "session_scope": {"type": "string", "enum": ["prefer", "only"], "description": "How to use session_id: prefer=non-strict retrieval using the provided session_id as context; only=limit results to that session plus unscoped memories"},
                    "subject_id": {"type": "string", "description": "Filter by memory subject ID. Only returns memories belonging to this subject."},
                    "memory_types": {"type": "array", "items": {"type": "string", "enum": ["semantic", "working", "episodic", "profile", "tool_result", "procedural"]}, "description": "Optional memory type filter. Narrows results to the specified types."},
                    "branch": {"type": "string", "description": "Optional branch to read from without changing the active checkout"},
                    "explain": {"type": ["boolean", "string"], "default": false, "description": "Explain level: false/\"none\"=off, true/\"basic\"=timing+path, \"verbose\"=+per-candidate scores, \"analyze\"=full"}
                },
                "required": ["query"]
            }
        },
        {
            "name": "memory_search",
            "description": "Semantic search across memories, optionally scoped to a session",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "top_k": {"type": "integer", "default": 10},
                    "session_id": {"type": "string"},
                    "session_scope": {"type": "string", "enum": ["prefer", "only"], "description": "How to use session_id: prefer=non-strict search using the provided session_id as context; only=limit results to that session plus unscoped memories"},
                    "subject_id": {"type": "string", "description": "Filter by memory subject ID. Only returns memories belonging to this subject."},
                    "memory_types": {"type": "array", "items": {"type": "string", "enum": ["semantic", "working", "episodic", "profile", "tool_result", "procedural"]}, "description": "Optional memory type filter. Narrows results to the specified types."},
                    "branch": {"type": "string", "description": "Optional branch to search without changing the active checkout"},
                    "explain": {"type": ["boolean", "string"], "default": false, "description": "Explain level: false/\"none\"=off, true/\"basic\"=timing+path, \"verbose\"=+per-candidate scores, \"analyze\"=full"}
                },
                "required": ["query"]
            }
        },
        {
            "name": "memory_correct",
            "description": "Update an existing memory with new content",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string"},
                    "query": {"type": "string", "description": "Semantic search to find memory to correct"},
                    "new_content": {"type": "string"},
                    "session_id": {"type": "string", "description": "Optional session to use when resolving query-based correction"},
                    "session_scope": {"type": "string", "enum": ["prefer", "only"], "description": "Only used with query-based correction. prefer=non-strict lookup using the provided session_id as context; only=restrict lookup to the given session plus unscoped memories"},
                    "subject_id": {"type": "string", "description": "Optional subject ID to scope query-based correction. Only memories belonging to this subject will be candidates."},
                    "memory_types": {"type": "array", "items": {"type": "string", "enum": ["semantic", "working", "episodic", "profile", "tool_result", "procedural"]}, "description": "Optional memory type filter for query-based correction. Only memories of the specified types will be candidates."},
                    "branch": {"type": "string", "description": "Optional branch to correct without changing the active checkout"},
                    "reason": {"type": "string"}
                },
                "required": ["new_content"]
            }
        },
        {
            "name": "memory_purge",
            "description": "Delete memories by ID, by topic keyword, or by exact session_id with optional memory type filtering",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string", "description": "Single ID or comma-separated batch"},
                    "topic": {"type": "string", "description": "Keyword — bulk-delete all matching memories"},
                    "session_id": {"type": "string", "description": "Exact session identifier — bulk-delete memories from that session"},
                    "memory_types": {"type": "array", "items": {"type": "string", "enum": ["semantic", "working", "episodic", "profile", "tool_result", "procedural"]}, "description": "Optional memory type filter. Only valid with session_id"},
                    "branch": {"type": "string", "description": "Optional branch to purge from without changing the active checkout"},
                    "reason": {"type": "string"}
                }
            }
        },
        {
            "name": "memory_profile",
            "description": "Get user memory-derived profile summary",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "subject_id": {"type": "string", "description": "Filter by memory subject ID. Only returns profile memories belonging to this subject."},
                    "branch": {"type": "string", "description": "Optional branch to read without changing the active checkout"}
                }
            }
        },
        {
            "name": "memory_capabilities",
            "description": "List available memory tools",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "memory_list",
            "description": "List active memories for the user, with optional exact session filtering",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "default": 20},
                    "memory_type": {"type": "string"},
                    "session_id": {"type": "string", "description": "Exact session identifier — only list memories from that session"},
                    "subject_id": {"type": "string", "description": "Filter by memory subject ID. Only returns memories belonging to this subject."},
                    "branch": {"type": "string", "description": "Optional branch to list without changing the active checkout"}
                }
            }
        },
        {
            "name": "memory_governance",
            "description": "Run memory governance: quarantine low-confidence memories, clean stale data. 1-hour cooldown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "force": {"type": "boolean", "default": false}
                }
            }
        },
        {
            "name": "memory_consolidate",
            "description": "Run graph consolidation: detect contradicting memories, fix orphaned nodes, manage trust tiers. 30-minute cooldown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "force": {"type": "boolean", "default": false}
                }
            }
        },
        {
            "name": "memory_reflect",
            "description": "Analyze memory clusters and synthesize high-level insights. 2-hour cooldown.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "force": {"type": "boolean", "default": false},
                    "mode": {"type": "string", "default": "auto"}
                }
            }
        },
        {
            "name": "memory_feedback",
            "description": "Record explicit relevance feedback for a memory. Helps improve retrieval over time.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": {"type": "string", "description": "ID of the memory to provide feedback on"},
                    "signal": {"type": "string", "enum": ["useful", "irrelevant", "outdated", "wrong"], "description": "Feedback signal"},
                    "context": {"type": "string", "description": "Optional context about why this feedback was given"}
                },
                "required": ["memory_id", "signal"]
            }
        },
        {
            "name": "memory_observe",
            "description": "Extract and store memories from a conversation turn. With LLM: uses structured extraction. Without LLM: stores each non-empty message as-is.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "messages": {"type": "array", "items": {"type": "object"}, "description": "Conversation messages (role + content)"},
                    "session_id": {"type": "string", "description": "Optional session identifier to attach to stored memories"},
                    "subject_id": {"type": "string", "description": "Stable business ID of the memory subject (e.g. end-user ID). Stored on each extracted memory for subject-scoped retrieval."},
                    "branch": {"type": "string", "description": "Optional branch to write to without changing the active checkout"}
                },
                "required": ["messages"]
            }
        }
    ])
}

fn parse_tool_name(name: &str) -> ToolCallName {
    match name {
        "memory_store" => ToolCallName::MemoryStore,
        "memory_retrieve" => ToolCallName::MemoryRetrieve,
        "memory_search" => ToolCallName::MemorySearch,
        "memory_correct" => ToolCallName::MemoryCorrect,
        "memory_purge" => ToolCallName::MemoryPurge,
        "memory_profile" => ToolCallName::MemoryProfile,
        "memory_list" => ToolCallName::MemoryList,
        "memory_capabilities" => ToolCallName::MemoryCapabilities,
        "memory_governance" => ToolCallName::MemoryGovernance,
        "memory_rebuild_index" => ToolCallName::MemoryRebuildIndex,
        "memory_consolidate" => ToolCallName::MemoryConsolidate,
        "memory_reflect" => ToolCallName::MemoryReflect,
        "memory_extract_entities" => ToolCallName::MemoryExtractEntities,
        "memory_link_entities" => ToolCallName::MemoryLinkEntities,
        "memory_feedback" => ToolCallName::MemoryFeedback,
        "memory_get_retrieval_params" => ToolCallName::MemoryGetRetrievalParams,
        "memory_tune_params" => ToolCallName::MemoryTuneParams,
        "memory_observe" => ToolCallName::MemoryObserve,
        _ => ToolCallName::Unknown(name.to_string()),
    }
}

pub(crate) fn is_known_tool(name: &str) -> bool {
    !matches!(parse_tool_name(name), ToolCallName::Unknown(_))
}

pub async fn call(
    name: &str,
    args: Value,
    service: &Arc<MemoryService>,
    user_id: &str,
) -> Result<Value> {
    tracing::debug!(tool = name, user_id, "MCP tool call");
    if let Err(e) = validate_tool_args(name, &args) {
        return Ok(mcp_error(e));
    }
    let tool = parse_tool_name(name);
    match tool {
        ToolCallName::MemoryStore => {
            let content = match parse_store_content(&args) {
                Ok(content) => content,
                Err(msg) => return Ok(mcp_error(msg)),
            };
            let memory_type = args["memory_type"].as_str().unwrap_or("semantic");
            let session_id = args["session_id"].as_str().map(String::from);
            let trust_tier = args["trust_tier"]
                .as_str()
                .map(TrustTier::from_str)
                .transpose()?;
            let mt = MemoryType::from_str(memory_type).unwrap_or(MemoryType::Semantic);
            let subject_id = args["subject_id"]
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from);
            let extra_metadata = args
                .get("extra_metadata")
                .filter(|v| v.is_object())
                .and_then(|v| {
                    serde_json::from_value::<std::collections::HashMap<String, serde_json::Value>>(
                        v.clone(),
                    )
                    .ok()
                });
            let m = match service
                .store_memory_with_metadata_on_branch(
                    user_id,
                    branch_arg(&args),
                    &content,
                    mt,
                    session_id.clone(),
                    trust_tier,
                    None,
                    None,
                    None,
                    subject_id,
                    extra_metadata,
                )
                .await
            {
                Ok(m) => m,
                Err(memoria_core::MemoriaError::Blocked(reason)) => {
                    return Ok(json!({"result": format!("⚠️ Memory blocked: {reason}")}));
                }
                Err(e) => return Err(e.into()),
            };

            // Graph sync: create SEMANTIC node + auto entity extraction (best-effort)
            if let Ok(sql) = user_sql_store(service, user_id).await {
                let graph = sql.graph_store();
                let node = memoria_storage::GraphNode {
                    node_id: uuid::Uuid::new_v4().simple().to_string()[..32].to_string(),
                    user_id: user_id.to_string(),
                    node_type: memoria_storage::NodeType::Semantic,
                    content: m.content.clone(),
                    entity_type: None,
                    embedding: None,
                    memory_id: Some(m.memory_id.clone()),
                    session_id: session_id.clone(),
                    confidence: m.initial_confidence as f32,
                    trust_tier: format!("{}", m.trust_tier),
                    importance: 0.5,
                    source_nodes: vec![],
                    conflicts_with: None,
                    conflict_resolution: None,
                    access_count: 0,
                    cross_session_count: 0,
                    is_active: true,
                    superseded_by: None,
                    created_at: m.created_at.map(|dt| dt.naive_utc()),
                };
                let _ = graph.create_node(&node).await; // best-effort

                // Auto entity extraction (regex, lightweight)
                let entities = memoria_storage::extract_entities(&m.content);
                let mut links: Vec<(String, String, &str)> = Vec::new();
                for ent in &entities {
                    if let Ok((entity_id, _)) = graph
                        .upsert_entity(user_id, &ent.name, &ent.display, &ent.entity_type)
                        .await
                    {
                        links.push((m.memory_id.clone(), entity_id, "regex"));
                    }
                }
                if !links.is_empty() {
                    let refs: Vec<(&str, &str, &str)> = links
                        .iter()
                        .map(|(m, e, s)| (m.as_str(), e.as_str(), *s))
                        .collect();
                    let _ = graph.batch_upsert_memory_entity_links(user_id, &refs).await;
                }
            }

            Ok(mcp_text(&format!(
                "Stored memory {}: {}",
                m.memory_id, m.content
            )))
        }

        ToolCallName::MemoryRetrieve | ToolCallName::MemorySearch => {
            let query = match parse_retrieve_query(&args) {
                Ok(query) => query,
                Err(msg) => return Ok(mcp_error(msg)),
            };
            let top_k = if matches!(tool, ToolCallName::MemorySearch) {
                args["top_k"].as_i64().unwrap_or(10)
            } else {
                args["top_k"].as_i64().unwrap_or(5)
            };
            let retrieve_options = parse_retrieve_options_arg(&args)?;
            // explain accepts bool or string: true/"basic"/"verbose"/"analyze"
            let explain_str = match &args["explain"] {
                serde_json::Value::Bool(true) => "basic",
                serde_json::Value::String(s) => s.as_str(),
                _ => "none",
            };
            let level = memoria_service::ExplainLevel::from_str_or_bool(explain_str);

            if level != memoria_service::ExplainLevel::None {
                let (results, stats) = service
                    .retrieve_explain_level_with_options_on_branch(
                        user_id,
                        branch_arg(&args),
                        &query,
                        top_k,
                        level,
                        &retrieve_options,
                    )
                    .await?;
                if results.is_empty() {
                    let explain_json = serde_json::to_string_pretty(&stats).unwrap_or_default();
                    return Ok(mcp_text(&format!(
                        "No relevant memories found.\n\n--- explain ---\n{explain_json}"
                    )));
                }
                let text = results
                    .iter()
                    .map(|m| format!("[{}] ({}) {}{}", m.memory_id, m.memory_type, m.content, m.extra_metadata.as_ref().filter(|md| !md.is_empty()).map(|md| format!(" | metadata: {}", serde_json::to_string(md).unwrap_or_default())).unwrap_or_default()))
                    .collect::<Vec<_>>()
                    .join("\n");
                let explain_json = serde_json::to_string_pretty(&stats).unwrap_or_default();
                Ok(mcp_text(&format!(
                    "{text}\n\n--- explain ---\n{explain_json}"
                )))
            } else {
                let results = service
                    .retrieve_with_options_on_branch(
                        user_id,
                        branch_arg(&args),
                        &query,
                        top_k,
                        &retrieve_options,
                    )
                    .await?;
                if results.is_empty() {
                    return Ok(mcp_text("No relevant memories found."));
                }
                let text = results
                    .iter()
                    .map(|m| format!("[{}] ({}) {}{}", m.memory_id, m.memory_type, m.content, m.extra_metadata.as_ref().filter(|md| !md.is_empty()).map(|md| format!(" | metadata: {}", serde_json::to_string(md).unwrap_or_default())).unwrap_or_default()))
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(mcp_text(&text))
            }
        }

        ToolCallName::MemoryCorrect => {
            let new_content = match parse_required_str(&args, "new_content", "new_content is required") {
                Ok(s) => s,
                Err(msg) => return Ok(mcp_error(msg)),
            };
            let memory_id = args["memory_id"].as_str().unwrap_or("");
            let query = args["query"].as_str().unwrap_or("");

            // Resolve old memory_id for graph sync
            let old_mid = if !memory_id.is_empty() {
                memory_id.to_string()
            } else if !query.is_empty() {
                let retrieve_options = parse_retrieve_options_arg(&args)?;
                let results = service
                    .retrieve_with_options_on_branch(
                        user_id,
                        branch_arg(&args),
                        query,
                        1,
                        &retrieve_options,
                    )
                    .await?;
                match results.into_iter().next() {
                    Some(found) => found.memory_id,
                    None => return Ok(mcp_error("No matching memory found for query; provide an existing memory_id or refine the query.")),
                }
            } else {
                return Ok(mcp_error("Provide memory_id or query"));
            };

            let m = service
                .correct_on_branch(user_id, branch_arg(&args), &old_mid, &new_content)
                .await?;

            Ok(mcp_text(&format!(
                "Corrected memory {}: {}",
                m.memory_id, m.content
            )))
        }

        ToolCallName::MemoryPurge => {
            let purge_args = parse_memory_purge_args(&args)?;
            if let Some(memory_id) = purge_args.memory_id {
                // Batch: comma-separated IDs
                let ids: Vec<&str> = memory_id
                    .split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .collect();
                let result = service
                    .purge_batch_on_branch(user_id, branch_arg(&args), &ids)
                    .await?;
                Ok(mcp_text(&format_purge_msg(
                    &format!("Purged {} memory(s)", result.purged),
                    &result,
                )))
            } else if let Some(topic) = purge_args.topic {
                // Bulk by keyword: exact text match then purge
                let result = service
                    .purge_by_topic_on_branch(user_id, branch_arg(&args), &topic)
                    .await?;
                Ok(mcp_text(&format_purge_msg(
                    &format!("Purged {} memory(s) matching '{topic}'", result.purged),
                    &result,
                )))
            } else if let Some(session_id) = purge_args.session_id {
                let result = service
                    .purge_by_session_id_on_branch(
                        user_id,
                        branch_arg(&args),
                        &session_id,
                        purge_args.memory_types.as_deref(),
                    )
                    .await?;
                Ok(mcp_text(&format_purge_msg(
                    &format!(
                        "Purged {} memory(s) for session '{session_id}'",
                        result.purged
                    ),
                    &result,
                )))
            } else {
                Ok(mcp_error("Provide memory_id, topic, or session_id"))
            }
        }

        ToolCallName::MemoryProfile => {
            let subject_id = args
                .get("subject_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty());
            let memories = service
                .list_active_filtered_on_branch(
                    user_id,
                    branch_arg(&args),
                    ListActiveOptions {
                        limit: 50,
                        memory_type: Some("profile"),
                        session_id: None,
                        trust_tier: None,
                        cursor: None,
                        subject_id,
                    },
                )
                .await?;
            if memories.is_empty() {
                return Ok(mcp_text("No profile memories found."));
            }
            let text = memories
                .iter()
                .map(|m| m.content.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            Ok(mcp_text(&text))
        }

        ToolCallName::MemoryList => {
            let limit = args["limit"].as_i64().unwrap_or(20);
            let memories = service
                .list_active_filtered_on_branch(
                    user_id,
                    branch_arg(&args),
                    ListActiveOptions {
                        limit,
                        memory_type: args.get("memory_type").and_then(Value::as_str),
                        session_id: args.get("session_id").and_then(Value::as_str),
                        trust_tier: args.get("trust_tier").and_then(Value::as_str),
                        cursor: None,
                        subject_id: args
                            .get("subject_id")
                            .and_then(Value::as_str)
                            .map(str::trim)
                            .filter(|s| !s.is_empty()),
                    },
                )
                .await?;
            if memories.is_empty() {
                return Ok(mcp_text("No memories found."));
            }
            let text = memories
                .iter()
                .map(|m| format!("[{}] ({}) {}{}", m.memory_id, m.memory_type, m.content, m.extra_metadata.as_ref().filter(|md| !md.is_empty()).map(|md| format!(" | metadata: {}", serde_json::to_string(md).unwrap_or_default())).unwrap_or_default()))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(mcp_text(&text))
        }

        ToolCallName::MemoryCapabilities => Ok(mcp_text(MEMORY_CAPABILITIES_TEXT)),

        ToolCallName::MemoryGovernance => {
            let force = args["force"].as_bool().unwrap_or(false);
            let sql = user_sql_store(service, user_id).await?;
            const COOLDOWN_SECS: i64 = 3600; // 1 hour
            if !force {
                if let Some(remaining) = sql
                    .check_cooldown(user_id, "governance", COOLDOWN_SECS)
                    .await?
                {
                    return Ok(mcp_text(&format!(
                        "Governance skipped (cooldown: {remaining}s remaining). Use force=true to override."
                    )));
                }
            }
            let quarantined = sql.quarantine_low_confidence(user_id).await?;
            let cleaned = sql.cleanup_stale(user_id).await?;
            sql.set_cooldown(user_id, "governance").await?;

            // Audit log for quarantine/cleanup
            if quarantined > 0 {
                let payload = serde_json::json!({"quarantined": quarantined}).to_string();
                service.send_edit_log(
                    user_id,
                    "governance:quarantine",
                    None,
                    Some(&payload),
                    &format!("quarantined {quarantined}"),
                    None,
                );
            }
            if cleaned > 0 {
                let payload = serde_json::json!({"cleaned_stale": cleaned}).to_string();
                service.send_edit_log(
                    user_id,
                    "governance:cleanup",
                    None,
                    Some(&payload),
                    &format!("cleaned_stale {cleaned}"),
                    None,
                );
            }

            // Snapshot health
            let snap_health = {
                let snaps = git_for_store(&sql)
                    .map(|git| async move { git.list_snapshots().await.unwrap_or_default() });
                let snaps = match snaps {
                    Some(fut) => fut.await,
                    None => Vec::new(),
                };
                let total = snaps.len();
                let auto = snaps
                    .iter()
                    .filter(|snap| {
                        snap.snapshot_name.starts_with("mem_milestone_")
                            || snap.snapshot_name.contains("_pre_")
                    })
                    .count();
                let ratio = if total > 0 {
                    auto as f64 / total as f64
                } else {
                    0.0
                };
                format!(
                    "snapshots: {total} total, {:.0}% auto-generated",
                    ratio * 100.0
                )
            };

            Ok(mcp_text(&format!(
                "Governance complete: quarantined={quarantined}, cleaned_stale={cleaned}. {snap_health}"
            )))
        }

        ToolCallName::MemoryRebuildIndex => {
            let table = args["table"].as_str().unwrap_or("mem_memories");
            if !["mem_memories", "memory_graph_nodes"].contains(&table) {
                return Ok(mcp_error(format!(
                    "Invalid table '{table}'. Use mem_memories or memory_graph_nodes"
                )));
            }
            let sql = user_sql_store(service, user_id).await?;
            let total_rows = sql
                .rebuild_vector_index(table)
                .await
                .context("rebuild index failed")?;
            Ok(mcp_text(&format!(
                "Rebuilt IVF index for {table}: rows={total_rows}"
            )))
        }

        ToolCallName::MemoryConsolidate => {
            let force = args["force"].as_bool().unwrap_or(false);
            let sql = user_sql_store(service, user_id).await?;
            const COOLDOWN_SECS: i64 = 1800; // 30 minutes
            if !force {
                if let Some(remaining) = sql
                    .check_cooldown(user_id, "consolidate", COOLDOWN_SECS)
                    .await?
                {
                    return Ok(mcp_text(&format!(
                        "Consolidation skipped (cooldown: {remaining}s remaining). Use force=true to override."
                    )));
                }
            }

            let graph = sql.graph_store();
            let result = DefaultConsolidationStrategy::default()
                .consolidate(&graph, &ConsolidationInput::for_user(user_id))
                .await?;

            sql.set_cooldown(user_id, "consolidate").await?;

            let mut msg = format!(
                "Consolidation complete: status={}, conflicts_detected={}, orphaned_scenes={}, promoted={}, demoted={}",
                result.status.as_str(),
                result.metrics.get("consolidation.conflicts_detected").copied().unwrap_or(0.0) as i64,
                result.metrics.get("consolidation.orphaned_scenes").copied().unwrap_or(0.0) as i64,
                result.metrics.get("trust.promoted_count").copied().unwrap_or(0.0) as i64,
                result.metrics.get("trust.demoted_count").copied().unwrap_or(0.0) as i64
            );
            if !result.warnings.is_empty() {
                msg.push_str(&format!(" (warnings: {})", result.warnings.join("; ")));
            }
            Ok(mcp_text(&msg))
        }

        ToolCallName::MemoryReflect => {
            let force = args["force"].as_bool().unwrap_or(false);
            let mode = args["mode"].as_str().unwrap_or("auto");
            let sql = user_sql_store(service, user_id).await?;

            if mode == "internal" && service.llm.is_none() {
                return Ok(mcp_error(
                    "Reflection with internal LLM requires LLM_API_KEY to be set.",
                ));
            }

            const COOLDOWN_SECS: i64 = 7200; // 2 hours
            if mode != "candidates" && !force {
                if let Some(remaining) = sql
                    .check_cooldown(user_id, "reflect", COOLDOWN_SECS)
                    .await?
                {
                    return Ok(mcp_text(&format!(
                        "Reflection skipped (cooldown: {remaining}s remaining). Use force=true to override."
                    )));
                }
            }

            let graph = sql.graph_store();
            let clusters = build_reflect_clusters(&graph, user_id).await?;

            if clusters.is_empty() {
                return Ok(mcp_text(
                    "No memory clusters found for reflection. \
                     Store more memories across multiple sessions first.",
                ));
            }

            // candidates mode: return raw clusters for agent to synthesize
            if mode == "candidates" || service.llm.is_none() {
                let mut parts = Vec::new();
                for (i, (signal, importance, mems)) in clusters.iter().enumerate() {
                    let mem_lines: Vec<String> = mems
                        .iter()
                        .map(|(_, c, _)| format!("  - [semantic] {c}"))
                        .collect();
                    parts.push(format!(
                        "Cluster {} ({signal}, importance={importance:.3}):\n{}",
                        i + 1,
                        mem_lines.join("\n")
                    ));
                }
                return Ok(mcp_text(&format!(
                    "Here are memory clusters for reflection. Synthesize 1-2 insights per cluster, \
                     then store each via memory_store(content=..., memory_type='semantic').\n\n{}",
                    parts.join("\n\n")
                )));
            }

            // auto/internal mode: use LLM to synthesize insights
            let llm = service.llm.as_ref().unwrap();
            let mut scenes_created = 0usize;
            let table = sql.active_table(user_id).await?;

            // Get existing high-confidence memories as "existing knowledge"
            let existing_sql = format!(
                "SELECT content FROM {table} WHERE user_id = ? AND is_active = 1 \
                 AND trust_tier IN ('T1','T2') ORDER BY created_at DESC LIMIT 10"
            );
            let existing_rows = sqlx::query(&existing_sql)
                .bind(user_id)
                .fetch_all(sql.pool())
                .await?;
            let existing_knowledge = existing_rows
                .iter()
                .filter_map(|r| r.try_get::<String, _>("content").ok())
                .collect::<Vec<_>>()
                .join("\n");

            for (_, _, mems) in &clusters {
                let experiences = mems
                    .iter()
                    .map(|(_, c, _)| format!("- {c}"))
                    .collect::<Vec<_>>()
                    .join("\n");

                let prompt = reflection_prompt(&experiences, &existing_knowledge);
                let msgs = vec![memoria_embedding::ChatMessage {
                    role: "user".to_string(),
                    content: prompt,
                }];
                let raw = match llm.chat(&msgs, 0.3, Some(400)).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!("LLM reflect call failed: {e}");
                        continue;
                    }
                };

                // Parse JSON array from response
                let start = raw.find('[').unwrap_or(raw.len());
                let end = raw.rfind(']').map(|i| i + 1).unwrap_or(raw.len());
                if start >= end {
                    continue;
                }
                let items: Vec<serde_json::Value> = match serde_json::from_str(&raw[start..end]) {
                    Ok(v) => v,
                    Err(_) => continue,
                };

                for item in &items {
                    let content = item["content"].as_str().unwrap_or("").trim().to_string();
                    if content.is_empty() {
                        continue;
                    }
                    let mt_str = item["type"].as_str().unwrap_or("semantic");
                    let mt = MemoryType::from_str(mt_str).unwrap_or(MemoryType::Semantic);
                    let confidence = item["confidence"].as_f64().unwrap_or(0.5) as f32;
                    // Store as T4 (unverified insight from reflection)
                    let _ = service
                        .store_memory(
                            user_id,
                            &content,
                            mt,
                            None,
                            Some(TrustTier::from_str("T4").unwrap_or(TrustTier::T4Unverified)),
                            None,
                            None,
                            None,
                            None,
                        )
                        .await;
                    scenes_created += 1;
                    let _ = confidence; // used in future for graph node confidence
                }
            }

            sql.set_cooldown(user_id, "reflect").await?;
            Ok(mcp_text(&format!(
                "Reflection complete: scenes_created={scenes_created}, candidates_found={}",
                clusters.len()
            )))
        }

        ToolCallName::MemoryExtractEntities => {
            let mode = args["mode"].as_str().unwrap_or("auto");
            let sql = user_sql_store(service, user_id).await?;

            if mode == "internal" && service.llm.is_none() {
                return Ok(mcp_error(
                    "LLM entity extraction requires LLM_API_KEY to be set.",
                ));
            }

            let graph = sql.graph_store();
            let unlinked = graph.get_unlinked_memories(user_id, 50).await?;

            if unlinked.is_empty() {
                return Ok(mcp_text(&serde_json::to_string(&json!({
                    "status": "complete",
                    "unlinked": 0,
                    "message": "All memories already have entity links."
                }))?));
            }

            // auto with LLM: extract via LLM and write directly
            if mode != "candidates" {
                if let Some(llm) = service.llm.as_ref() {
                    let mut total_created = 0usize;
                    let mut total_edges = 0usize;

                    for (memory_id, content) in &unlinked {
                        let prompt = entity_extract_prompt(content);
                        let msgs = vec![memoria_embedding::ChatMessage {
                            role: "user".to_string(),
                            content: prompt,
                        }];
                        let raw = match llm.chat(&msgs, 0.0, Some(300)).await {
                            Ok(r) => r,
                            Err(_) => continue,
                        };
                        let start = raw.find('[').unwrap_or(raw.len());
                        let end = raw.rfind(']').map(|i| i + 1).unwrap_or(raw.len());
                        if start >= end {
                            continue;
                        }
                        let items: Vec<serde_json::Value> =
                            match serde_json::from_str(&raw[start..end]) {
                                Ok(v) => v,
                                Err(_) => continue,
                            };
                        let mut links: Vec<(String, String, &str)> = Vec::new();
                        for item in &items {
                            let name = item["name"].as_str().unwrap_or("").trim().to_lowercase();
                            if name.is_empty() {
                                continue;
                            }
                            let display = item["name"].as_str().unwrap_or("").trim().to_string();
                            let etype = item["type"].as_str().unwrap_or("concept").to_string();
                            if let Ok((entity_id, is_new)) =
                                graph.upsert_entity(user_id, &name, &display, &etype).await
                            {
                                links.push((memory_id.to_string(), entity_id, "llm"));
                                if is_new {
                                    total_created += 1;
                                    total_edges += 1;
                                }
                            }
                        }
                        if !links.is_empty() {
                            let refs: Vec<(&str, &str, &str)> = links
                                .iter()
                                .map(|(m, e, s)| (m.as_str(), e.as_str(), *s))
                                .collect();
                            let _ = graph.batch_upsert_memory_entity_links(user_id, &refs).await;
                        }
                    }

                    return Ok(mcp_text(&serde_json::to_string(&json!({
                        "status": "done",
                        "total_memories": unlinked.len(),
                        "entities_found": total_created,
                        "edges_created": total_edges
                    }))?));
                }
            }

            // candidates mode (or auto without LLM): return for agent to process
            let existing = graph.get_user_entities(user_id).await?;
            let existing_json: Vec<serde_json::Value> = existing
                .iter()
                .map(|(name, etype)| json!({"name": name, "entity_type": etype}))
                .collect();
            let memories_json: Vec<serde_json::Value> = unlinked
                .iter()
                .map(|(mid, content)| json!({"memory_id": mid, "content": content}))
                .collect();

            Ok(mcp_text(&serde_json::to_string(&json!({
                "status": "candidates",
                "unlinked": memories_json.len(),
                "memories": memories_json,
                "existing_entities": existing_json,
                "instruction": "Extract named entities (people, tech, projects, repos) from each memory, then call memory_link_entities."
            }))?))
        }

        ToolCallName::MemoryLinkEntities => {
            let entities_str = args["entities"].as_str().unwrap_or("");
            let sql = user_sql_store(service, user_id).await?;

            let parsed: Vec<serde_json::Value> = match serde_json::from_str(entities_str) {
                Ok(v) => v,
                Err(_) => {
                    return Ok(mcp_error(&serde_json::to_string(&json!({
                        "status": "error",
                        "error": "Invalid JSON",
                        "expected_format": [{"memory_id": "...", "entities": [{"name": "...", "type": "..."}]}]
                    }))?))
                }
            };

            let graph = sql.graph_store();
            let mut total_created = 0usize;
            let mut total_reused = 0usize;
            let mut total_edges = 0usize;

            for item in &parsed {
                let memory_id = match item["memory_id"].as_str() {
                    Some(id) => id,
                    None => continue,
                };
                let ents: Vec<(String, String, String)> = item["entities"]
                    .as_array()
                    .unwrap_or(&vec![])
                    .iter()
                    .filter_map(|e| {
                        let name = e["name"].as_str()?.trim().to_lowercase();
                        if name.is_empty() {
                            return None;
                        }
                        let display = e["name"].as_str().unwrap_or("").trim().to_string();
                        let etype = e["type"].as_str().unwrap_or("concept").to_string();
                        Some((name, display, etype))
                    })
                    .collect();

                if ents.is_empty() {
                    continue;
                }
                let mut links: Vec<(String, String, &str)> = Vec::new();
                for (name, display, etype) in &ents {
                    let (entity_id, is_new) =
                        graph.upsert_entity(user_id, name, display, etype).await?;
                    links.push((memory_id.to_string(), entity_id, "manual"));
                    if is_new {
                        total_created += 1;
                        total_edges += 1;
                    } else {
                        total_reused += 1;
                    }
                }
                if !links.is_empty() {
                    let refs: Vec<(&str, &str, &str)> = links
                        .iter()
                        .map(|(m, e, s)| (m.as_str(), e.as_str(), *s))
                        .collect();
                    graph
                        .batch_upsert_memory_entity_links(user_id, &refs)
                        .await?;
                }
            }

            Ok(mcp_text(&serde_json::to_string(&json!({
                "status": "done",
                "entities_created": total_created,
                "entities_reused": total_reused,
                "edges_created": total_edges
            }))?))
        }

        ToolCallName::MemoryFeedback => {
            let memory_id = args["memory_id"]
                .as_str()
                .ok_or_else(|| crate::tool_result::input_error("memory_id is required"))?;
            let signal = args["signal"]
                .as_str()
                .ok_or_else(|| crate::tool_result::input_error("signal is required"))?;
            let context = args["context"].as_str();

            let feedback_id = service
                .record_feedback(user_id, memory_id, signal, context)
                .await?;

            Ok(mcp_text(&format!(
                "Recorded feedback: memory={}, signal={}, feedback_id={}",
                memory_id, signal, feedback_id
            )))
        }

        ToolCallName::MemoryGetRetrievalParams => {
            let sql = user_sql_store(service, user_id).await?;
            let params = sql.get_user_retrieval_params(user_id).await?;
            Ok(mcp_text(&serde_json::to_string_pretty(&params)?))
        }

        ToolCallName::MemoryTuneParams => {
            use memoria_service::scoring::{DefaultScoringPlugin, ScoringPlugin};

            let sql = user_sql_store(service, user_id).await?;

            let old_params = sql.get_user_retrieval_params(user_id).await?;
            let plugin = DefaultScoringPlugin;
            match plugin.tune_params(sql.as_ref(), user_id).await? {
                Some(new_params) => Ok(mcp_text(&format!(
                    "Parameters tuned:\n  feedback_weight: {:.3} → {:.3}\n  temporal_decay_hours: {:.1} → {:.1}\n  confidence_weight: {:.3} → {:.3}",
                    old_params.feedback_weight, new_params.feedback_weight,
                    old_params.temporal_decay_hours, new_params.temporal_decay_hours,
                    old_params.confidence_weight, new_params.confidence_weight
                ))),
                None => Ok(mcp_text(
                    "Not enough feedback to tune parameters (minimum 10 feedback signals required)"
                )),
            }
        }

        ToolCallName::MemoryObserve => {
            let messages = args["messages"].as_array().cloned().unwrap_or_default();
            let session_id = args["session_id"].as_str().map(String::from);
            let subject_id = args
                .get("subject_id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from);

            let (memories, has_llm) = service
                .observe_turn_on_branch(
                    user_id,
                    branch_arg(&args),
                    &messages,
                    session_id.clone(),
                    subject_id,
                )
                .await?;

            // Graph sync (best-effort) for each stored memory
            let graph = user_sql_store(service, user_id)
                .await
                .ok()
                .map(|sql| sql.graph_store());
            for m in &memories {
                if let Some(graph) = graph.as_ref() {
                    let node = memoria_storage::GraphNode {
                        node_id: Uuid::new_v4().simple().to_string()[..32].to_string(),
                        user_id: user_id.to_string(),
                        node_type: memoria_storage::NodeType::Semantic,
                        content: m.content.clone(),
                        entity_type: None,
                        embedding: None,
                        memory_id: Some(m.memory_id.clone()),
                        session_id: session_id.clone(),
                        confidence: m.initial_confidence as f32,
                        trust_tier: format!("{}", m.trust_tier),
                        importance: 0.5,
                        source_nodes: vec![],
                        conflicts_with: None,
                        conflict_resolution: None,
                        access_count: 0,
                        cross_session_count: 0,
                        is_active: true,
                        superseded_by: None,
                        created_at: m.created_at.map(|dt| dt.naive_utc()),
                    };
                    let _ = graph.create_node(&node).await;
                    let entities = memoria_storage::extract_entities(&m.content);
                    let mut links: Vec<(String, String, &str)> = Vec::new();
                    for ent in &entities {
                        if let Ok((entity_id, _)) = graph
                            .upsert_entity(user_id, &ent.name, &ent.display, &ent.entity_type)
                            .await
                        {
                            links.push((m.memory_id.clone(), entity_id, "regex"));
                        }
                    }
                    if !links.is_empty() {
                        let refs: Vec<(&str, &str, &str)> = links
                            .iter()
                            .map(|(m, e, s)| (m.as_str(), e.as_str(), *s))
                            .collect();
                        let _ = graph.batch_upsert_memory_entity_links(user_id, &refs).await;
                    }
                }
            }

            let stored: Vec<_> = memories
                .iter()
                .map(|m| {
                    json!({
                        "memory_id": m.memory_id,
                        "content": m.content,
                        "memory_type": m.memory_type.to_string(),
                    })
                })
                .collect();

            let mut result = json!({ "memories": stored });
            if !has_llm {
                result["warning"] =
                    json!("LLM not configured — storing messages as-is without extraction");
            }
            Ok(mcp_text(&serde_json::to_string_pretty(&result)?))
        }

        ToolCallName::Unknown(name) => Err(anyhow::anyhow!("Unknown tool: {name}")),
    }
}

pub async fn call_owned(
    name: String,
    args: Value,
    service: Arc<MemoryService>,
    user_id: String,
) -> Result<Value> {
    call(&name, args, &service, &user_id).await
}

fn mcp_text(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

fn format_purge_msg(base: &str, result: &memoria_service::PurgeResult) -> String {
    let mut msg = base.to_string();
    if let Some(snap) = &result.snapshot_name {
        msg.push_str(&format!(
            "\nSafety snapshot: {snap} (use memory_rollback to undo)"
        ));
    }
    if let Some(warning) = &result.warning {
        msg.push_str(&format!("\n{warning}"));
    }
    msg
}

// ── Graph-based reflection helpers ───────────────────────────────────────────

/// Build reflection clusters from graph nodes using connected components.
/// Returns Vec<(signal, importance, Vec<(memory_id, content, session_id)>)>
pub async fn build_reflect_clusters(
    graph: &memoria_storage::GraphStore,
    user_id: &str,
) -> anyhow::Result<Vec<(String, f32, Vec<(String, String, Option<String>)>)>> {
    const MIN_CLUSTER_SIZE: usize = 3;
    const MIN_SESSIONS: usize = 2;

    let count = graph.count_user_nodes(user_id).await?;
    if count < MIN_CLUSTER_SIZE as i64 {
        return Ok(vec![]);
    }

    // Get recent semantic nodes as candidates
    let semantic_nodes = graph
        .get_user_nodes(user_id, &memoria_storage::NodeType::Semantic, true)
        .await?;
    if semantic_nodes.len() < MIN_CLUSTER_SIZE {
        return Ok(vec![]);
    }

    // Take most recent 50 nodes
    let mut nodes = semantic_nodes;
    nodes.sort_by(|a, b| b.node_id.cmp(&a.node_id));
    nodes.truncate(50);

    let node_ids: Vec<String> = nodes.iter().map(|n| n.node_id.clone()).collect();
    let edges = graph.get_edges_for_nodes(&node_ids).await?;

    // Build adjacency for connected components
    let node_set: std::collections::HashSet<&str> = node_ids.iter().map(|s| s.as_str()).collect();
    let mut adjacency: std::collections::HashMap<&str, Vec<&str>> = Default::default();
    for (src, tgt) in &edges {
        if node_set.contains(src.as_str()) && node_set.contains(tgt.as_str()) {
            adjacency
                .entry(src.as_str())
                .or_default()
                .push(tgt.as_str());
            adjacency
                .entry(tgt.as_str())
                .or_default()
                .push(src.as_str());
        }
    }

    let node_map: std::collections::HashMap<&str, &memoria_storage::GraphNode> =
        nodes.iter().map(|n| (n.node_id.as_str(), n)).collect();

    // BFS connected components
    let mut visited = std::collections::HashSet::new();
    let mut clusters = Vec::new();
    for nid in &node_ids {
        if visited.contains(nid.as_str()) {
            continue;
        }
        let mut component = Vec::new();
        let mut queue = vec![nid.as_str()];
        while let Some(cur) = queue.pop() {
            if !visited.insert(cur) {
                continue;
            }
            if let Some(node) = node_map.get(cur) {
                component.push(*node);
            }
            for &neighbor in adjacency.get(cur).unwrap_or(&vec![]) {
                if !visited.contains(neighbor) {
                    queue.push(neighbor);
                }
            }
        }
        if component.len() >= MIN_CLUSTER_SIZE {
            let sessions: std::collections::HashSet<&str> = component
                .iter()
                .filter_map(|n| n.session_id.as_deref())
                .collect();
            if sessions.len() >= MIN_SESSIONS {
                let has_conflict = component.iter().any(|n| n.conflicts_with.is_some());
                let signal = if has_conflict {
                    "contradiction"
                } else {
                    "semantic_cluster"
                };
                let importance = 0.5 + (component.len() as f32 * 0.05).min(0.3);
                let mems: Vec<(String, String, Option<String>)> = component
                    .iter()
                    .map(|n| {
                        (
                            n.memory_id.clone().unwrap_or_else(|| n.node_id.clone()),
                            n.content.clone(),
                            n.session_id.clone(),
                        )
                    })
                    .collect();
                clusters.push((signal.to_string(), importance, mems));
            }
        }
    }

    // If no graph clusters (no edges yet), fall back to session-based grouping
    if clusters.is_empty() {
        let mut by_session: std::collections::HashMap<
            String,
            Vec<(String, String, Option<String>)>,
        > = Default::default();
        for node in &nodes {
            let sid = node
                .session_id
                .clone()
                .unwrap_or_else(|| "default".to_string());
            by_session.entry(sid.clone()).or_default().push((
                node.memory_id
                    .clone()
                    .unwrap_or_else(|| node.node_id.clone()),
                node.content.clone(),
                node.session_id.clone(),
            ));
        }
        for (_, mems) in by_session {
            if mems.len() >= MIN_CLUSTER_SIZE {
                clusters.push(("semantic_cluster".to_string(), 0.5, mems));
            }
        }
    }

    Ok(clusters)
}

/// LLM prompt for reflection synthesis (matches Python's REFLECTION_SYNTHESIS_PROMPT).
pub fn reflection_prompt(experiences: &str, existing_knowledge: &str) -> String {
    format!(
        "SYSTEM:\nYou are analyzing an agent's experiences across multiple sessions with the same user.\n\
         Your goal: extract 1-2 reusable insights that will help the agent serve this user better.\n\n\
         RULES:\n\
         - Each insight must be ACTIONABLE (a behavioral rule or factual pattern), not just descriptive\n\
         - Each insight must be grounded in the evidence — do not speculate beyond what's shown\n\
         - If the experiences don't reveal a clear pattern, return an empty array []\n\
         - Assign confidence conservatively: 0.3-0.5 for weak patterns, 0.5-0.7 for strong ones\n\
         - Type \"procedural\" = how to do something; \"semantic\" = what is true about the user/project\n\n\
         EXISTING KNOWLEDGE (do not repeat these):\n{existing_knowledge}\n\n\
         EXPERIENCES TO ANALYZE:\n{experiences}\n\n\
         OUTPUT FORMAT (JSON array, 0-2 items):\n\
         [\n  {{\"type\": \"procedural\" | \"semantic\", \"content\": \"One clear sentence\", \
         \"confidence\": 0.3-0.7, \"evidence_summary\": \"Which experiences support this\"}}\n]"
    )
}

/// LLM prompt for entity extraction (matches Python's _LLM_EXTRACT_PROMPT).
pub fn entity_extract_prompt(text: &str) -> String {
    format!(
        "Extract named entities from the following text. Return a JSON array of objects.\n\
         Each object: {{\"name\": \"canonical name\", \"type\": \"tech|person|repo|project|concept\"}}\n\n\
         Rules:\n\
         - Only extract specific, named entities (not generic words)\n\
         - For tech terms: use canonical form (React, Spark, OAuth)\n\
         - Deduplicate: include each entity once\n\
         - Max 10 entities per text\n\
         - Do NOT extract: generic verbs, common nouns, numbers, dates\n\n\
         Text:\n{}\n\nJSON array:",
        &memoria_core::truncate_utf8(text, 2000)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_session_id_preserves_message_and_input_classification() {
        for args in [
            json!({"session_scope":"only"}),
            json!({"session_scope":"only", "session_id":"  "}),
        ] {
            let error = parse_retrieve_options_arg(&args)
                .err()
                .expect("missing session id");
            let message = "session_id is required when session_scope is set";
            assert_eq!(error.to_string(), message);
            let result = crate::tool_result::execution_error("memory_search", error);
            assert_eq!(
                crate::tool_result::error_kind(&result),
                Some(crate::tool_result::ErrorKind::Input)
            );
            assert_eq!(result["content"][0]["text"], message);
        }
    }

    #[test]
    fn parse_required_str_rejects_missing_and_blank() {
        for val in [json!({}), json!({"k": ""}), json!({"k": "   \t\n"})] {
            assert!(parse_required_str(&val, "k", "k is required").is_err());
        }
    }

    #[test]
    fn parse_required_str_preserves_whitespace_when_valid() {
        assert_eq!(
            parse_required_str(&json!({"k": "  hello  "}), "k", "k is required").unwrap(),
            "  hello  "
        );
    }

    #[test]
    fn parse_store_content_rejects_missing_and_blank() {
        assert_eq!(parse_store_content(&json!({})), Err("content is required"));
        assert_eq!(
            parse_store_content(&json!({"content": ""})),
            Err("content is required")
        );
        assert_eq!(
            parse_store_content(&json!({"content": "   \t\n"})),
            Err("content is required")
        );
    }

    #[test]
    fn parse_retrieve_query_rejects_missing_and_blank() {
        assert_eq!(parse_retrieve_query(&json!({})), Err("query is required"));
        assert_eq!(
            parse_retrieve_query(&json!({"query": ""})),
            Err("query is required")
        );
        assert_eq!(
            parse_retrieve_query(&json!({"query": "   \t\n"})),
            Err("query is required")
        );
    }

    #[test]
    fn parse_correct_new_content_rejects_blank() {
        // memory_correct now uses parse_required_str — verify whitespace is rejected
        assert_eq!(
            parse_required_str(&json!({"new_content": ""}), "new_content", "new_content is required"),
            Err("new_content is required")
        );
        assert_eq!(
            parse_required_str(&json!({"new_content": "   "}), "new_content", "new_content is required"),
            Err("new_content is required")
        );
        assert_eq!(
            parse_required_str(&json!({"new_content": "ok"}), "new_content", "new_content is required").unwrap(),
            "ok"
        );
    }

    #[test]
    fn entity_extract_prompt_truncates_at_char_boundary() {
        // 2000 ASCII bytes then a 3-byte Chinese char — must not panic
        let text = format!("{}你好世界", "x".repeat(2000));
        let prompt = entity_extract_prompt(&text);
        assert!(prompt.contains(&"x".repeat(2000)));
        assert!(!prompt.contains('你'));
    }

    #[test]
    fn entity_extract_prompt_multibyte_at_boundary() {
        // Place a 3-byte char so byte 2000 lands inside it
        let text = format!("{}，after", "a".repeat(1999));
        let prompt = entity_extract_prompt(&text);
        assert!(prompt.contains(&"a".repeat(1999)));
        assert!(!prompt.contains('，'));
    }
}
