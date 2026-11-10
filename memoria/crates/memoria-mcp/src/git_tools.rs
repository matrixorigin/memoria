//! Git-for-Data MCP tools: snapshot, branch, merge, rollback, diff.
//! 9 tools — brings total to 17 (8 core + 9 git).
//!
//! Parity with Python version:
//! - snapshot names prefixed with "mem_snap_", scoped by user DB, sanitized to 40 chars
//! - snapshot list filters to current user's mem_snap_ + global mem_milestone_, strips prefix for display
//! - snapshot delete supports names, prefix, older_than
//! - snapshot limit: 20 per user
//! - rollback restores mem_memories + graph tables
//! - branch limit: 20 per user
//! - branch duplicate name rejected (including deleted)
//! - branch name sanitized to 40 chars

use chrono::NaiveDateTime;
use memoria_core::MemoriaError;
use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sqlx::{MySql, QueryBuilder, Row};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use uuid::Uuid;

/// Convert sqlx::Error to MemoriaError::Database
fn db_err(e: sqlx::Error) -> MemoriaError {
    MemoriaError::Database(e.to_string())
}

/// Convert memoria_git errors to MemoriaError
fn git_err(e: impl std::fmt::Display) -> MemoriaError {
    MemoriaError::Internal(e.to_string())
}

fn validate_identifier(name: &str) -> Result<&str, MemoriaError> {
    if memoria_core::is_safe_sql_identifier(name) {
        Ok(name)
    } else {
        Err(MemoriaError::Internal(format!(
            "Invalid identifier: {name:?} — only alphanumeric and underscore allowed"
        )))
    }
}

const MAX_USER_SNAPSHOTS: i64 = 20;
const MAX_BRANCHES: i64 = 20;
const MAX_IDENTIFIER_LEN: usize = 64;
const SNAP_PREFIX: &str = "mem_snap_";
const MILESTONE_PREFIX: &str = "mem_milestone_";
const SAFETY_PREFIX: &str = "mem_snap_pre_";
const SNAP_SCOPE_MAX_LEN: usize = MAX_IDENTIFIER_LEN - SNAP_PREFIX.len() - 2 - 2 - 40;
const SAFETY_SCOPE_MAX_LEN: usize = 21;
const DEFAULT_PICK_TARGET: &str = "main";
const DEFAULT_PICK_STRATEGY: &str = "fail";
const DEFAULT_PICK_TOP_K: i64 = 5;
const DEFAULT_PICK_PREVIEW_LIMIT: i64 = 10;
const MAX_PICK_TOP_K: i64 = 100;
const MAX_PICK_PREVIEW_LIMIT: i64 = 100;
const MAX_PICK_KEYS: usize = 1000;

fn safety_prefix(db_name: Option<&str>) -> String {
    match db_name {
        Some(db_name) => format!(
            "mem_snap_{}_pre_",
            compact_identifier_fragment(db_name, SAFETY_SCOPE_MAX_LEN)
        ),
        None => SAFETY_PREFIX.to_string(),
    }
}

fn legacy_safety_prefix(db_name: Option<&str>) -> Option<String> {
    db_name.map(|db_name| format!("mem_snap_{db_name}_pre_"))
}

fn is_safety_snapshot_name(name: &str, db_name: Option<&str>) -> bool {
    name.starts_with(SAFETY_PREFIX)
        || name.starts_with(&safety_prefix(db_name))
        || legacy_safety_prefix(db_name)
            .as_deref()
            .is_some_and(|prefix| name.starts_with(prefix))
}

/// Sanitize a user-provided name: keep alphanumeric+underscore, truncate to 40 chars.
/// If result starts with non-alpha, prepend "s_".
fn sanitize_name(name: &str) -> String {
    let mut clean: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    if clean.is_empty() || !clean.chars().next().unwrap().is_alphabetic() {
        clean = format!("s_{clean}");
    }
    clean
}

fn sanitize_branch_table_suffix(name: &str) -> String {
    // Keep the previous spelling for ASCII names; only physical branch suffixes
    // change for Unicode names. The random prefix disambiguates equal suffixes.
    let ascii: String = name
        .chars()
        .take(40)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sanitize_name(&ascii)
}

fn sanitize_snapshot_scope(scope: &str) -> String {
    compact_identifier_fragment(scope, SNAP_SCOPE_MAX_LEN)
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

/// Convert user-facing snapshot name → internal MatrixOne snapshot name.
fn snap_internal(db_name: &str, name: &str) -> String {
    if name.starts_with(SNAP_PREFIX) || name.starts_with(MILESTONE_PREFIX) {
        name.to_string()
    } else {
        let scope = sanitize_snapshot_scope(db_name);
        let internal = format!(
            "{SNAP_PREFIX}{}_{scope}_{}",
            scope.len(),
            sanitize_name(name)
        );
        debug_assert!(internal.len() <= MAX_IDENTIFIER_LEN);
        internal
    }
}

/// Convert internal snapshot name → user-facing display name.
fn snap_display(internal: &str) -> String {
    if let Some(rest) = internal.strip_prefix(SNAP_PREFIX) {
        if let Some((len_part, scoped_rest)) = rest.split_once('_') {
            if let Ok(scope_len) = len_part.parse::<usize>() {
                if scoped_rest.len() > scope_len
                    && scoped_rest.as_bytes().get(scope_len) == Some(&b'_')
                {
                    return scoped_rest[scope_len + 1..].to_string();
                }
            }
        }
        rest.to_string()
    } else if let Some(rest) = internal.strip_prefix(MILESTONE_PREFIX) {
        format!("auto:{rest}")
    } else {
        internal.to_string()
    }
}

fn safety_display_name(internal: &str) -> Option<String> {
    if let Some(rest) = internal.strip_prefix(SAFETY_PREFIX) {
        return Some(format!("pre_{rest}"));
    }
    let rest = internal.strip_prefix(SNAP_PREFIX)?;
    let (_, after_prefix) = rest.split_once("_pre_")?;
    Some(format!("pre_{after_prefix}"))
}

#[derive(Clone)]
pub struct VisibleSnapshot {
    pub display_name: String,
    pub internal_name: String,
    pub memory_count: Option<i64>,
    pub timestamp: Option<NaiveDateTime>,
    pub registered: bool,
    /// Classified during discovery; registered user snapshots are never safety snapshots.
    pub is_safety: bool,
}

#[derive(Debug, Clone)]
struct ReplaceCandidate {
    main_memory_id: String,
    branch_memory_id: String,
    replacement_content: String,
    conflict_distance: f64,
}

#[derive(Debug, Clone)]
struct PickCandidate {
    memory_id: String,
    content: String,
    memory_type: String,
    target_exists: bool,
}

#[derive(Debug, Clone)]
struct RankedPickCandidate {
    memory_id: String,
    content: String,
    score: Option<f64>,
    target_exists: bool,
}

#[derive(Debug, Clone)]
struct SnapshotPickRow {
    memory_id: String,
    content: String,
    memory_type: String,
    is_active: bool,
}

#[derive(Debug, Clone)]
struct TargetPickRow {
    content: String,
    is_active: bool,
}

#[derive(Debug, Deserialize)]
struct MemoryPickArgs {
    source: String,
    #[serde(default = "default_pick_target")]
    target: String,
    #[serde(default = "default_pick_strategy")]
    strategy: String,
    selector: PickSelector,
    dry_run: Option<PickDryRunOptions>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum PickSelector {
    KeyList {
        keys: Vec<String>,
    },
    SnapshotRange {
        from_snapshot: String,
        to_snapshot: String,
    },
    Retrieve {
        query: String,
        #[serde(default = "default_pick_top_k")]
        top_k: i64,
        min_score: Option<f64>,
    },
}

fn default_pick_target() -> String {
    DEFAULT_PICK_TARGET.to_string()
}

fn default_pick_strategy() -> String {
    DEFAULT_PICK_STRATEGY.to_string()
}

fn default_pick_top_k() -> i64 {
    DEFAULT_PICK_TOP_K
}

fn default_pick_preview_limit() -> i64 {
    DEFAULT_PICK_PREVIEW_LIMIT
}

fn default_pick_include_content_preview() -> bool {
    true
}

fn default_pick_include_scores() -> bool {
    true
}

#[derive(Debug, Deserialize, Clone)]
struct PickDryRunOptions {
    #[serde(default = "default_pick_preview_limit")]
    limit: i64,
    #[serde(default)]
    offset: i64,
    #[serde(default = "default_pick_include_content_preview")]
    include_content_preview: bool,
    #[serde(default = "default_pick_include_scores")]
    include_scores: bool,
}

#[derive(Debug, Serialize)]
struct PickPreviewSummary {
    candidate_count: usize,
    shown_count: usize,
    would_apply: usize,
    would_skip: usize,
    conflict_count: usize,
    would_abort: bool,
}

#[derive(Debug, Serialize)]
struct PickPreviewPage {
    limit: usize,
    offset: usize,
    has_more: bool,
}

#[derive(Debug, Serialize)]
struct PickPreviewCandidate {
    memory_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content_preview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    score: Option<f64>,
    status: &'static str,
    reason: &'static str,
}

fn milestone_internal(name: &str) -> Option<String> {
    if let Some(rest) = name.strip_prefix("auto:") {
        Some(format!("{MILESTONE_PREFIX}{rest}"))
    } else if name.starts_with(MILESTONE_PREFIX) {
        Some(name.to_string())
    } else {
        None
    }
}

async fn snapshot_store(
    svc: &Arc<MemoryService>,
    user_id: &str,
) -> Result<Arc<memoria_storage::SqlMemoryStore>, MemoriaError> {
    svc.user_sql_store(user_id).await
}

fn git_for_store(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
) -> Result<GitForDataService, MemoriaError> {
    let db_name = sql.database_name().ok_or_else(|| {
        MemoriaError::Internal("Git ops require a database-backed SQL store".into())
    })?;
    Ok(GitForDataService::new(
        sql.pool().clone(),
        db_name.to_string(),
    ))
}

pub async fn count_active_memories_at_snapshot(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    user_id: &str,
    snapshot_name: &str,
) -> Result<i64, MemoriaError> {
    let snapshot_name = validate_identifier(snapshot_name)?;
    let table = sql.t("mem_memories");
    let count_sql = format!(
        "SELECT COUNT(*) as cnt FROM {table} {{SNAPSHOT = '{snapshot_name}'}} WHERE user_id = ? AND is_active > 0"
    );
    sqlx::query_scalar(&count_sql)
        .bind(user_id)
        .fetch_one(sql.pool())
        .await
        .map_err(db_err)
}

pub async fn visible_snapshots_for_user(
    svc: &Arc<MemoryService>,
    user_id: &str,
) -> Result<Vec<VisibleSnapshot>, MemoriaError> {
    let started = std::time::Instant::now();
    let sql = snapshot_store(svc, user_id).await?;
    let db_name = sql.database_name();
    let git = git_for_store(&sql)?;
    let list_started = std::time::Instant::now();
    let all = git.list_snapshots().await.map_err(git_err)?;
    let list_elapsed_ms = list_started.elapsed().as_millis();
    let actual_by_name: HashMap<String, memoria_git::Snapshot> = all
        .into_iter()
        .filter(|s| {
            s.snapshot_name.starts_with(SNAP_PREFIX)
                || s.snapshot_name.starts_with(MILESTONE_PREFIX)
        })
        .map(|s| (s.snapshot_name.clone(), s))
        .collect();

    let mut snapshots = Vec::new();
    let mut seen_internal = HashSet::new();
    let registrations_started = std::time::Instant::now();
    let registrations = sql.list_snapshot_registrations(user_id).await?;
    let registrations_elapsed_ms = registrations_started.elapsed().as_millis();
    for reg in registrations {
        if let Some(actual) = actual_by_name.get(&reg.snapshot_name) {
            seen_internal.insert(reg.snapshot_name.clone());
            snapshots.push(VisibleSnapshot {
                display_name: reg.name,
                internal_name: reg.snapshot_name,
                memory_count: memoria_storage::snapshot_extra_memory_count(reg.extra.as_ref()),
                timestamp: actual.timestamp.or(Some(reg.created_at)),
                registered: true,
                is_safety: false,
            });
        }
    }

    for actual in actual_by_name.values() {
        let is_safety = is_safety_snapshot_name(&actual.snapshot_name, db_name);
        if !seen_internal.contains(&actual.snapshot_name)
            && (actual.snapshot_name.starts_with(MILESTONE_PREFIX) || is_safety)
        {
            snapshots.push(VisibleSnapshot {
                display_name: if is_safety {
                    safety_display_name(&actual.snapshot_name)
                        .unwrap_or_else(|| snap_display(&actual.snapshot_name))
                } else {
                    snap_display(&actual.snapshot_name)
                },
                internal_name: actual.snapshot_name.clone(),
                memory_count: None,
                timestamp: actual.timestamp,
                registered: false,
                is_safety,
            });
        }
    }

    snapshots.sort_by_key(|b| std::cmp::Reverse(b.timestamp));
    let total_elapsed_ms = started.elapsed().as_millis();
    if total_elapsed_ms >= 100 {
        tracing::info!(
            user_id = %user_id,
            db_name = ?db_name,
            actual_snapshot_count = actual_by_name.len(),
            visible_snapshot_count = snapshots.len(),
            show_snapshots_elapsed_ms = list_elapsed_ms,
            registrations_elapsed_ms,
            total_elapsed_ms,
            "snapshot_list_phase: visible_snapshots_for_user"
        );
    } else {
        tracing::debug!(
            user_id = %user_id,
            db_name = ?db_name,
            actual_snapshot_count = actual_by_name.len(),
            visible_snapshot_count = snapshots.len(),
            show_snapshots_elapsed_ms = list_elapsed_ms,
            registrations_elapsed_ms,
            total_elapsed_ms,
            "snapshot_list_phase: visible_snapshots_for_user"
        );
    }
    Ok(snapshots)
}

async fn resolve_snapshot_for_user(
    svc: &Arc<MemoryService>,
    user_id: &str,
    name: &str,
) -> Result<Option<String>, MemoriaError> {
    let sql = snapshot_store(svc, user_id).await?;
    let git = git_for_store(&sql)?;
    let db_name = sql.database_name();
    if let Some(internal) = milestone_internal(name) {
        return Ok(git
            .get_snapshot(&internal)
            .await
            .map_err(git_err)?
            .map(|_| internal));
    }
    if is_safety_snapshot_name(name, db_name) {
        return Ok(git
            .get_snapshot(name)
            .await
            .map_err(git_err)?
            .map(|_| name.to_string()));
    }

    let reg = if name.starts_with(SNAP_PREFIX) {
        sql.get_snapshot_registration_by_internal(user_id, name)
            .await?
    } else {
        sql.get_snapshot_registration(user_id, name).await?
    };
    if let Some(reg) = reg {
        return Ok(Some(reg.snapshot_name));
    }
    Ok(visible_snapshots_for_user(svc, user_id)
        .await?
        .into_iter()
        .find(|snapshot| snapshot.display_name == name)
        .map(|snapshot| snapshot.internal_name))
}

async fn acquire_snapshot_create_lock(
    lock_store: &Arc<memoria_storage::SqlMemoryStore>,
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    git: &GitForDataService,
    user_id: &str,
    display: &str,
    internal: &str,
) -> Result<Option<String>, MemoriaError> {
    let lock_key = format!("snapshot_create:{user_id}:{internal}");
    for _ in 0..20 {
        if lock_store.try_acquire_lock(&lock_key, 30).await? {
            return Ok(Some(lock_key));
        }
        if sql
            .get_snapshot_registration(user_id, display)
            .await?
            .is_some()
            || sql
                .get_snapshot_registration_by_internal(user_id, internal)
                .await?
                .is_some()
            || git.get_snapshot(internal).await.map_err(git_err)?.is_some()
        {
            return Ok(None);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    Ok(None)
}

pub fn list() -> Value {
    json!([
        {
            "name": "memory_snapshot",
            "description": "Create a named snapshot of current memory state",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "description": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "memory_snapshots",
            "description": "List snapshots with pagination",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": {"type": "integer", "default": 20},
                    "offset": {"type": "integer", "default": 0}
                }
            }
        },
        {
            "name": "memory_snapshot_delete",
            "description": "Delete snapshots by name(s), prefix, or age",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "names": {"type": "string"},
                    "prefix": {"type": "string"},
                    "older_than": {"type": "string", "description": "ISO date e.g. 2026-03-01"}
                }
            }
        },
        {
            "name": "memory_rollback",
            "description": "Restore memories to a previous snapshot",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "memory_branch",
            "description": "Create a new memory branch for isolated experimentation",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "from_snapshot": {"type": "string"},
                    "from_timestamp": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "memory_branches",
            "description": "List all memory branches",
            "inputSchema": {"type": "object", "properties": {}}
        },
        {
            "name": "memory_checkout",
            "description": "Switch to a different memory branch",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "memory_merge",
            "description": "Merge a branch back into main",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {"type": "string"},
                    "strategy": {
                        "type": "string",
                        "default": "accept",
                        "description": "accept | replace | append (accept is the default and an alias of replace / branch-wins on detected conflicts)"
                    }
                },
                "required": ["source"]
            }
        },
        {
            "name": "memory_pick",
            "description": "Selectively apply branch changes from source into a target branch (default main). Use key_list only when you already know the exact memory_ids to bring over. Use snapshot_range only when you already have two existing named snapshots and want every source-branch change between them. Use retrieve only when you have a natural-language topic and want semantic ranking over rows that differ between source and target. This tool mutates branch state unless dry_run is provided. When uncertain, prefer dry_run first and keep the default strategy=fail. Do not use accept unless the user explicitly wants source changes to overwrite conflicting target rows.",
            "inputSchema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "source": {"type": "string", "description": "Source branch name to pick from."},
                    "target": {"type": "string", "default": "main", "description": "Target branch name. Defaults to main."},
                    "strategy": {
                        "type": "string",
                        "enum": ["fail", "skip", "accept"],
                        "default": "fail",
                        "description": "Conflict strategy. fail = safest default, abort if any picked row conflicts with target; use this for safe preview-first flows and whenever overwrite policy is not explicit. skip = apply only non-conflicting rows; use this only if the user explicitly wants partial apply despite conflicts. accept = source branch wins and overwrites conflicting target rows; use this only if the user explicitly requests overwrite/source-wins behavior."
                    },
                    "selector": {
                        "type": "object",
                        "description": "Exactly one selector object. Pass the selector object itself here; do not use selector:'snapshot_range' and do not add sibling top-level fields like retrieve or snapshot_range.",
                        "oneOf": [
                            {
                                "type": "object",
                                "additionalProperties": false,
                                "description": "Use key_list when you already know the exact memory_ids to apply. Example: selector={type:'key_list', keys:['m_101','m_205']}.",
                                "properties": {
                                    "type": {"const": "key_list"},
                                    "keys": {
                                        "type": "array",
                                        "items": {"type": "string"},
                                        "description": "Specific memory_ids from the source branch to apply. Use this only for explicit known IDs; maximum 1000 unique keys."
                                    }
                                },
                                "required": ["type", "keys"]
                            },
                            {
                                "type": "object",
                                "additionalProperties": false,
                                "description": "Use snapshot_range when you already have two existing named snapshots on the source branch and want all changes between them. Example: selector={type:'snapshot_range', from_snapshot:'snap_a', to_snapshot:'snap_b'}.",
                                "properties": {
                                    "type": {"const": "snapshot_range"},
                                    "from_snapshot": {"type": "string", "description": "Existing start snapshot name on the source branch."},
                                    "to_snapshot": {"type": "string", "description": "Existing end snapshot name on the source branch."}
                                },
                                "required": ["type", "from_snapshot", "to_snapshot"]
                            },
                            {
                                "type": "object",
                                "additionalProperties": false,
                                "description": "Use retrieve when you only know a topic/intent and want semantic ranking over changed rows. Example: selector={type:'retrieve', query:'OAuth callback bugs', top_k:2, min_score:0.7}. Keep query/top_k/min_score inside selector.",
                                "properties": {
                                    "type": {"const": "retrieve"},
                                    "query": {"type": "string", "description": "Natural-language query used to rank changed source memories."},
                                    "top_k": {"type": "integer", "default": 5, "description": "Retrieve only. Keeps at most the top-k ranked changed memories in the actual apply set."},
                                    "min_score": {"type": "number", "description": "Optional retrieve-only score threshold. Candidates below this score are excluded from both dry_run previews and the final apply set."}
                                },
                                "required": ["type", "query"]
                            }
                        ]
                    },
                    "dry_run": {
                        "type": "object",
                        "additionalProperties": false,
                        "description": "Optional preview object. Any object here enables preview mode and prevents mutation. dry_run must be an object, not a boolean. limit/offset only shape preview output; they do not change which memories would be picked. For token-sensitive preview, keep limit small. In preview-first flows, usually leave strategy at the default fail unless the user explicitly wants to preview skip or accept behavior.",
                        "properties": {
                            "limit": {"type": "integer", "default": 10, "description": "Maximum number of preview candidates to return. Use 1-3 when token-sensitive."},
                            "offset": {"type": "integer", "default": 0, "description": "Preview pagination offset."},
                            "include_content_preview": {"type": "boolean", "default": true, "description": "When true, include a short content_preview for each candidate. Embeddings are never returned."},
                            "include_scores": {"type": "boolean", "default": true, "description": "When true, include retrieve scores when available. Non-retrieve selectors omit scores. Set false when you only need a tiny preview."}
                        }
                    }
                },
                "required": ["source", "selector"]
            }
        },
        {
            "name": "memory_branch_delete",
            "description": "Delete a memory branch",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": {"type": "string"}
                },
                "required": ["name"]
            }
        },
        {
            "name": "memory_diff",
            "description": "Preview what would change if a branch were merged into main",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {"type": "string"},
                    "limit": {"type": "integer", "default": 50}
                },
                "required": ["source"]
            }
        },
        {
            "name": "memory_apply",
            "description": "Selectively apply specific changes from a branch to main (cherry-pick). Use memory_diff first to see available changes, then pass the memory_ids you want to apply.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": {"type": "string", "description": "Branch name to apply from"},
                    "adds": {
                        "type": "array",
                        "items": {"type": "string"},
                        "default": [],
                        "description": "memory_ids to add (new in branch, absent from main)"
                    },
                    "updates": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_id": {"type": "string", "description": "Original memory_id (will be superseded)"},
                                "new_id": {"type": "string", "description": "Corrected memory_id (active replacement)"}
                            },
                            "required": ["old_id", "new_id"]
                        },
                        "default": [],
                        "description": "Correction pairs: old_id (superseded) → new_id (replacement)"
                    },
                    "removes": {
                        "type": "array",
                        "items": {"type": "string"},
                        "default": [],
                        "description": "memory_ids to remove (soft-delete from main)"
                    },
                    "accept_branch_conflicts": {
                        "type": "array",
                        "items": {"type": "string"},
                        "default": [],
                        "description": "Conflict memory_ids to resolve with branch-wins semantics. Unspecified conflicts stay on main."
                    }
                },
                "required": ["source"]
            }
        }
    ])
}

enum GitToolCallName {
    MemorySnapshot,
    MemorySnapshots,
    MemorySnapshotDelete,
    MemoryRollback,
    MemoryBranch,
    MemoryBranches,
    MemoryCheckout,
    MemoryMerge,
    MemoryPick,
    MemoryBranchDelete,
    MemoryDiff,
    MemoryApply,
    Unknown(String),
}

pub async fn call(
    name: &str,
    args: Value,
    _git: &Arc<GitForDataService>,
    svc: &Arc<MemoryService>,
    user_id: &str,
) -> Result<Value, MemoriaError> {
    let tool = match name {
        "memory_snapshot" => GitToolCallName::MemorySnapshot,
        "memory_snapshots" => GitToolCallName::MemorySnapshots,
        "memory_snapshot_delete" => GitToolCallName::MemorySnapshotDelete,
        "memory_rollback" => GitToolCallName::MemoryRollback,
        "memory_branch" => GitToolCallName::MemoryBranch,
        "memory_branches" => GitToolCallName::MemoryBranches,
        "memory_checkout" => GitToolCallName::MemoryCheckout,
        "memory_merge" => GitToolCallName::MemoryMerge,
        "memory_pick" => GitToolCallName::MemoryPick,
        "memory_branch_delete" => GitToolCallName::MemoryBranchDelete,
        "memory_diff" => GitToolCallName::MemoryDiff,
        "memory_apply" => GitToolCallName::MemoryApply,
        _ => GitToolCallName::Unknown(name.to_string()),
    };
    match tool {
        GitToolCallName::MemorySnapshot => {
            let user_snapshots = visible_snapshots_for_user(svc, user_id)
                .await?
                .into_iter()
                .filter(|s| s.registered)
                .count() as i64;
            if user_snapshots >= MAX_USER_SNAPSHOTS {
                return Ok(crate::tool_result::error(format!(
                    "Snapshot limit reached ({MAX_USER_SNAPSHOTS}) for user {user_id}. Delete old snapshots first."
                )));
            }
            let snap_name = args["name"].as_str().unwrap_or("");
            let sql = snapshot_store(svc, user_id).await?;
            let internal = snap_internal(
                sql.database_name().ok_or_else(|| {
                    MemoriaError::Internal(
                        "Snapshot ops require a database-backed SQL store".into(),
                    )
                })?,
                snap_name,
            );
            let display = if snap_name.starts_with(SNAP_PREFIX) {
                snap_display(snap_name)
            } else {
                sanitize_name(snap_name)
            };
            let git = git_for_store(&sql)?;
            let lock_store = svc.sql_store.as_ref().cloned().ok_or_else(|| {
                MemoriaError::Internal("Snapshot ops require a database-backed SQL store".into())
            })?;
            let Some(lock_key) =
                acquire_snapshot_create_lock(&lock_store, &sql, &git, user_id, &display, &internal)
                    .await?
            else {
                return Ok(crate::tool_result::classified_error(
                    crate::tool_result::ErrorKind::Rejected,
                    format!(
                        "Snapshot '{}' already exists. Choose a different name.",
                        display
                    ),
                ));
            };
            let result = async {
                if sql
                    .get_snapshot_registration(user_id, &display)
                    .await?
                    .is_some()
                    || sql
                        .get_snapshot_registration_by_internal(user_id, &internal)
                        .await?
                        .is_some()
                {
                    return Ok(crate::tool_result::classified_error(
                        crate::tool_result::ErrorKind::Rejected,
                        format!(
                            "Snapshot '{}' already exists. Choose a different name.",
                            display
                        ),
                    ));
                }
                let snap = match git.create_snapshot(&internal).await {
                    Ok(snap) => snap,
                    Err(err) => {
                        if git
                            .get_snapshot(&internal)
                            .await
                            .map_err(git_err)?
                            .is_some()
                        {
                            return Ok(crate::tool_result::classified_error(
                                crate::tool_result::ErrorKind::Rejected,
                                format!(
                                    "Snapshot '{}' already exists. Choose a different name.",
                                    display
                                ),
                            ));
                        }
                        return Err(git_err(err));
                    }
                };
                let memory_count =
                    count_active_memories_at_snapshot(&sql, user_id, &snap.snapshot_name).await?;
                let extra = memoria_storage::snapshot_extra_with_memory_count(memory_count);
                sql.register_snapshot(user_id, &display, &snap.snapshot_name, Some(&extra))
                    .await?;
                Ok(mcp_text(&format!(
                    "Snapshot '{}' created at {:?}",
                    display, snap.timestamp
                )))
            }
            .await;
            let _ = lock_store.release_lock(&lock_key).await;
            result
        }

        GitToolCallName::MemorySnapshots => {
            let limit = args["limit"].as_i64().unwrap_or(20) as usize;
            let offset = args["offset"].as_i64().unwrap_or(0) as usize;
            let snaps = visible_snapshots_for_user(svc, user_id).await?;
            let total = snaps.len();
            let page: Vec<_> = snaps.into_iter().skip(offset).take(limit).collect();
            if page.is_empty() {
                return Ok(mcp_text("No snapshots found."));
            }
            let text = page
                .iter()
                .map(|s| {
                    format!(
                        "{} ({})",
                        s.display_name,
                        s.timestamp.map(|t| t.to_string()).unwrap_or_default()
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(mcp_text(&format!("Snapshots ({total} total):\n{text}")))
        }

        GitToolCallName::MemorySnapshotDelete => {
            if args["prefix"]
                .as_str()
                .is_some_and(|prefix| prefix.trim().is_empty())
            {
                return Ok(crate::tool_result::error(
                    "Snapshot prefix must not be blank",
                ));
            }
            let sql = snapshot_store(svc, user_id).await?;
            let git = git_for_store(&sql)?;
            let snaps = visible_snapshots_for_user(svc, user_id).await?;

            let to_delete: Vec<VisibleSnapshot> = if let Some(names) = args["names"].as_str() {
                let name_set: HashSet<String> =
                    names.split(',').map(|n| n.trim().to_string()).collect();
                snaps
                    .iter()
                    .filter(|s| {
                        name_set.contains(&s.display_name) || name_set.contains(&s.internal_name)
                    })
                    .cloned()
                    .collect()
            } else if let Some(names) = args["names"].as_array() {
                let name_set: HashSet<String> = names
                    .iter()
                    .filter_map(|name| name.as_str().map(|s| s.trim().to_string()))
                    .collect();
                snaps
                    .iter()
                    .filter(|s| {
                        name_set.contains(&s.display_name) || name_set.contains(&s.internal_name)
                    })
                    .cloned()
                    .collect()
            } else if let Some(prefix) = args["prefix"].as_str() {
                snaps
                    .iter()
                    .filter(|s| s.display_name.starts_with(prefix))
                    .cloned()
                    .collect()
            } else if let Some(older_than) = args["older_than"].as_str() {
                let cutoff = NaiveDateTime::parse_from_str(
                    &format!("{older_than} 00:00:00"),
                    "%Y-%m-%d %H:%M:%S",
                )
                .or_else(|_| NaiveDateTime::parse_from_str(older_than, "%Y-%m-%dT%H:%M:%S"))
                .map_err(|_| {
                    MemoriaError::Validation("older_than must be ISO date e.g. '2026-03-01'".into())
                })?;
                snaps
                    .iter()
                    .filter(|s| s.timestamp.map(|t| t < cutoff).unwrap_or(false))
                    .cloned()
                    .collect()
            } else {
                return Ok(crate::tool_result::error(
                    "Specify 'names', 'prefix', or 'older_than'",
                ));
            };

            let count = to_delete.len();
            for snapshot in &to_delete {
                git.drop_snapshot(&snapshot.internal_name)
                    .await
                    .map_err(git_err)?;
                if snapshot.registered {
                    sql.deregister_snapshot_by_internal(user_id, &snapshot.internal_name)
                        .await?;
                }
            }
            let display: Vec<_> = to_delete.iter().map(|s| s.display_name.clone()).collect();
            Ok(mcp_text(&format!(
                "Deleted {count} snapshot(s): {}",
                display.join(", ")
            )))
        }

        GitToolCallName::MemoryRollback => {
            let sql = snapshot_store(svc, user_id).await?;
            let git = git_for_store(&sql)?;
            let snap_name = args["name"].as_str().unwrap_or("");
            let internal = resolve_snapshot_for_user(svc, user_id, snap_name)
                .await?
                .ok_or_else(|| MemoriaError::NotFound(format!("Snapshot '{snap_name}'")))?;
            // Restore mem_memories (required) + graph tables (best-effort, like Python)
            git.restore_table_from_snapshot("mem_memories", &internal)
                .await
                .map_err(|e| MemoriaError::Internal(format!("Rollback failed: {e}")))?;
            for table in &["memory_graph_nodes", "memory_graph_edges", "mem_edit_log"] {
                let _ = git.restore_table_from_snapshot(table, &internal).await;
            }
            sql.invalidate_user_caches(user_id).await;
            Ok(mcp_text(&format!("Rolled back to snapshot '{snap_name}'")))
        }

        GitToolCallName::MemoryBranch => {
            let branch_name = args["name"].as_str().unwrap_or("");
            let from_snapshot = args["from_snapshot"].as_str();
            let from_timestamp = args["from_timestamp"].as_str();

            if from_snapshot.is_some() && from_timestamp.is_some() {
                return Ok(crate::tool_result::error(
                    "Specify from_snapshot or from_timestamp, not both.",
                ));
            }

            // from_timestamp validation: must be within last 30 minutes, not future
            if let Some(ts_str) = from_timestamp {
                let ts =
                    NaiveDateTime::parse_from_str(ts_str, "%Y-%m-%d %H:%M:%S").map_err(|_| {
                        MemoriaError::Validation(
                            "from_timestamp must be 'YYYY-MM-DD HH:MM:SS'".into(),
                        )
                    })?;
                let now = chrono::Utc::now().naive_utc();
                if ts > now {
                    return Ok(crate::tool_result::error(
                        "from_timestamp cannot be in the future",
                    ));
                }
                if now - ts > chrono::Duration::minutes(30) {
                    return Ok(crate::tool_result::error(
                        "from_timestamp must be within the last 30 minutes",
                    ));
                }
            }

            let sql = svc.user_sql_store(user_id).await?;
            let git = git_for_store(&sql)?;

            // Global branch limit
            let all_branches = sql.list_branches(user_id).await?;
            if all_branches.len() as i64 >= MAX_BRANCHES {
                return Ok(crate::tool_result::error(format!(
                    "Branch limit reached ({MAX_BRANCHES}). Delete old branches first."
                )));
            }

            // Duplicate name check — only reject if an active branch with same name exists
            let branches_table = sql.t("mem_branches");
            let dup = sqlx::query(&format!(
                "SELECT COUNT(*) as cnt FROM {branches_table} WHERE user_id = ? AND name = ? AND status = 'active'"
            ))
            .bind(user_id)
            .bind(branch_name)
            .fetch_one(sql.pool())
            .await
            .map_err(db_err)?;
            let cnt: i64 = dup.try_get("cnt").unwrap_or(0);
            if cnt > 0 {
                return Ok(crate::tool_result::classified_error(
                    crate::tool_result::ErrorKind::Rejected,
                    format!("Branch '{branch_name}' already exists. Choose a different name."),
                ));
            }

            // Physical branch names are ASCII; the registry preserves the user's
            // original display name. Do not change sanitize_name: snapshots use
            // its historical Unicode mapping when resolving existing names.
            let safe = sanitize_branch_table_suffix(branch_name);
            let table_name = format!("br_{}_{}", &Uuid::new_v4().simple().to_string()[..8], safe);

            if let Some(snap) = from_snapshot {
                // Snapshot clones inherit the historical schema, which may predate
                // subject_id. Reconcile it before the branch becomes visible.
                let internal = resolve_snapshot_for_user(svc, user_id, snap)
                    .await?
                    .ok_or_else(|| MemoriaError::NotFound(format!("Snapshot '{snap}'")))?;
                git.create_branch_from_snapshot(&table_name, "mem_memories", &internal)
                    .await
                    .map_err(git_err)?;
                if let Err(error) = sql.ensure_branch_subject_id(&table_name).await {
                    if let Err(cleanup_error) = git.drop_branch(&table_name).await {
                        tracing::warn!(%cleanup_error, table = %table_name, "failed to clean up incompatible snapshot branch");
                    }
                    return Err(error);
                }
            } else {
                // Source is always mem_memories (never another branch table).
                // schema.subject_id is guaranteed present after migrate_user(), so MO's
                // zero-copy branch DDL will inherit the column automatically.
                git.create_branch(&table_name, "mem_memories")
                    .await
                    .map_err(git_err)?;
            }
            sql.register_branch(user_id, branch_name, &table_name)
                .await?;
            Ok(mcp_text(&format!("Created branch '{branch_name}'")))
        }

        GitToolCallName::MemoryBranches => {
            let branches = match svc.user_sql_store(user_id).await {
                Ok(sql) => sql.list_branches(user_id).await?,
                Err(_) => vec![],
            };
            let active_branch = match svc.user_sql_store(user_id).await {
                Ok(sql) => sql
                    .active_branch_name(user_id)
                    .await
                    .unwrap_or_else(|_| "main".to_string()),
                Err(_) => "main".to_string(),
            };
            if branches.is_empty() {
                return Ok(mcp_text("No branches. On main."));
            }
            let main_marker = if active_branch == "main" {
                " ← active"
            } else {
                ""
            };
            let text = branches
                .iter()
                .map(|(name, _table, _created_at)| {
                    let marker = if *name == active_branch {
                        " ← active"
                    } else {
                        ""
                    };
                    format!("{name}{marker}")
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok(mcp_text(&format!("Branches:\nmain{main_marker}\n{text}")))
        }

        GitToolCallName::MemoryCheckout => {
            let branch = args["name"].as_str().unwrap_or("main");
            let sql = svc.user_sql_store(user_id).await?;
            if branch == "main" {
                sql.set_active_branch(user_id, "main").await?;
                return Ok(mcp_text("Switched to branch 'main'"));
            }
            let branches = sql.list_branches(user_id).await?;
            if !branches.iter().any(|(name, _, _)| name == branch) {
                return Err(MemoriaError::NotFound(format!("Branch '{branch}'")));
            }
            sql.set_active_branch(user_id, branch).await?;
            let count = svc.list_active(user_id, 50).await?.len();
            Ok(mcp_text(&format!(
                "Switched to branch '{branch}'. {count} memories on this branch."
            )))
        }

        GitToolCallName::MemoryMerge => {
            let source_branch = args["source"].as_str().unwrap_or("");
            let strategy = args["strategy"].as_str().unwrap_or("accept");
            let strategy = match strategy {
                "append" => "append",
                "replace" | "accept" => "replace",
                other => {
                    return Err(MemoriaError::Validation(format!(
                        "Unsupported merge strategy '{other}'. Use append, replace, or accept."
                    )));
                }
            };
            let sql = svc.user_sql_store(user_id).await?;
            let git = git_for_store(&sql)?;
            let main_table = sql.t("mem_memories");
            let branches = sql.list_branches(user_id).await?;
            let table_name = branches
                .iter()
                .find(|(name, _, _)| name == source_branch)
                .map(|(_, t, _)| t.clone())
                .ok_or_else(|| MemoriaError::NotFound(format!("Branch '{source_branch}'")))?;
            let branch_table_name = validate_identifier(&table_name)?.to_string();
            let branch_table = sql.t(&branch_table_name);

            // Safety limit: count new memories (branch rows not in main by PK)
            let count_sql = format!(
                "SELECT COUNT(*) as cnt FROM {branch_table} b WHERE b.user_id = ? \
                 AND NOT EXISTS (SELECT 1 FROM {main_table} m WHERE m.memory_id = b.memory_id)"
            );
            let new_count: i64 = sqlx::query(&count_sql)
                .bind(user_id)
                .fetch_one(sql.pool())
                .await
                .map_err(db_err)?
                .try_get("cnt")
                .unwrap_or(0);
            if new_count > 5000 {
                return Ok(crate::tool_result::error(format!(
                    "Too many changes ({new_count}). Max 5000. Reduce branch scope."
                )));
            }

            // Cosine 0.9 → L2 for normalized vectors: sqrt(2*(1-0.9)) ≈ 0.4472
            // Uses l2_distance instead of cosine_similarity to leverage IVF vector_l2_ops index.
            const L2_CONFLICT: f64 = 0.4472;

            if strategy != "replace" {
                // append currently means "native append-only branch merge with skip-on-conflict".
                // MatrixOne v1.3.0 passes the current API/MCP regression suite with this path.
                //
                // Important semantic boundary:
                // - this is not a full git-style reconcile/three-way merge;
                // - native merge may also carry branch-only inactive rows (is_active = 0) into main
                //   when their PK does not already exist there.
                //
                // We intentionally keep the native behavior here for now and leave richer
                // reconcile/delete-propagation semantics for a future strategy.
                if new_count > 0 {
                    git.merge_branch(&branch_table_name, "mem_memories")
                        .await
                        .map_err(|e| MemoriaError::Internal(format!("merge failed: {e}")))?;
                }
                return Ok(mcp_text(&format!(
                    "Merged branch '{source_branch}' into main ({new_count} new, 0 replaced, 0 skipped)"
                )));
            }

            // replace strategy: SQL merge with cosine conflict detection
            // Single-pass INSERT using OR short-circuit to avoid cosine on null/empty embeddings
            let insert_sql = format!(
                "INSERT INTO {main_table} \
                    (memory_id, user_id, memory_type, content, embedding, session_id, \
                     source_event_ids, extra_metadata, is_active, superseded_by, \
                     trust_tier, initial_confidence, observed_at, created_at, updated_at, author_id, subject_id) \
                  SELECT b.memory_id, b.user_id, b.memory_type, b.content, b.embedding, b.session_id, \
                      b.source_event_ids, b.extra_metadata, b.is_active, b.superseded_by, \
                      b.trust_tier, b.initial_confidence, b.observed_at, b.created_at, b.updated_at, b.author_id, b.subject_id \
                  FROM {branch_table} b \
                  WHERE b.user_id = ? AND b.is_active = 1 \
                    AND NOT EXISTS (SELECT 1 FROM {main_table} m WHERE m.memory_id = b.memory_id) \
                    AND ( \
                      b.embedding IS NULL OR vector_dims(b.embedding) = 0 \
                      OR NOT EXISTS ( \
                        SELECT 1 FROM {main_table} m \
                        WHERE m.user_id = ? AND m.is_active = 1 \
                          AND m.embedding IS NOT NULL AND vector_dims(m.embedding) > 0 \
                          AND m.memory_type = b.memory_type \
                          AND m.subject_id <=> b.subject_id \
                          AND l2_distance(m.embedding, b.embedding) < {L2_CONFLICT} \
                      ) \
                   )"
            );
            let inserted = sqlx::query(&insert_sql)
                .bind(user_id)
                .bind(user_id)
                .execute(sql.pool())
                .await
                .map_err(db_err)?
                .rows_affected();

            let replacements = collect_replace_candidates(
                sql.pool(),
                &main_table,
                &branch_table,
                user_id,
                L2_CONFLICT,
            )
            .await?;
            let (replaced, skipped) = if !replacements.is_empty() {
                let mut tx = sql.pool().begin().await.map_err(db_err)?;
                for candidate in &replacements {
                    let update_sql = format!(
                        "UPDATE {main_table} SET content = ?, updated_at = NOW() WHERE memory_id = ?"
                    );
                    sqlx::query(&update_sql)
                        .bind(&candidate.replacement_content)
                        .bind(&candidate.main_memory_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(db_err)?;
                }
                tx.commit().await.map_err(db_err)?;
                (replacements.len() as u64, 0u64)
            } else {
                (0u64, 0u64)
            };

            Ok(mcp_text(&format!(
                "Merged branch '{source_branch}' into main ({inserted} new, {replaced} replaced, {skipped} skipped)"
            )))
        }

        GitToolCallName::MemoryPick => {
            let pick_args: MemoryPickArgs = serde_json::from_value(args.clone())
                .map_err(|e| MemoriaError::Validation(e.to_string()))?;
            let strategy = normalize_pick_strategy(&pick_args.strategy)?;
            if pick_args.source == pick_args.target {
                return Err(MemoriaError::Validation(
                    "source and target must be different branches".into(),
                ));
            }

            let sql = svc.user_sql_store(user_id).await?;
            let git = git_for_store(&sql)?;
            let source_table_name =
                resolve_branch_table_name(&sql, user_id, &pick_args.source, false).await?;
            let target_table_name =
                resolve_branch_table_name(&sql, user_id, &pick_args.target, true).await?;
            let source_table = sql.t(&source_table_name);
            let target_table = sql.t(&target_table_name);
            let dry_run = normalize_pick_dry_run(pick_args.dry_run.clone())?;
            let selector_value = args["selector"].clone();

            match pick_args.selector {
                PickSelector::KeyList { keys } => {
                    let requested = normalize_keys(keys)?;
                    let candidates = load_pick_candidates(
                        &sql,
                        user_id,
                        &source_table,
                        &target_table,
                        Some(&requested),
                        None,
                    )
                    .await?;
                    let pick_keys: Vec<String> = candidates
                        .iter()
                        .map(|candidate| candidate.memory_id.clone())
                        .collect();
                    let ranked = candidates
                        .iter()
                        .map(|candidate| RankedPickCandidate {
                            memory_id: candidate.memory_id.clone(),
                            content: candidate.content.clone(),
                            score: None,
                            target_exists: candidate.target_exists,
                        })
                        .collect::<Vec<_>>();
                    let ignored = requested.len().saturating_sub(pick_keys.len());
                    if let Some(dry_run) = dry_run.as_ref() {
                        return Ok(mcp_json(&build_pick_preview_response(
                            &pick_args.source,
                            &pick_args.target,
                            strategy,
                            selector_value.clone(),
                            dry_run,
                            &ranked,
                        )));
                    }
                    if pick_keys.is_empty() {
                        return Ok(mcp_text(&format!(
                            "No pickable changes found in branch '{}' for the selected keys.",
                            pick_args.source
                        )));
                    }

                    if let Err(err) = git
                        .pick_branch_keys(
                            &source_table_name,
                            &target_table_name,
                            &pick_keys,
                            strategy,
                        )
                        .await
                    {
                        return map_pick_conflict(
                            err,
                            &pick_args.source,
                            &pick_args.target,
                            Some(pick_keys.len()),
                        );
                    }

                    let remaining = count_pick_candidates(
                        &sql,
                        user_id,
                        &source_table,
                        &target_table,
                        Some(&pick_keys),
                    )
                    .await?;
                    let applied = pick_keys.len().saturating_sub(remaining as usize);
                    let skipped = if strategy == "skip" {
                        remaining.max(0) as usize
                    } else {
                        0
                    };
                    let ignored_suffix = if ignored > 0 {
                        format!(", {ignored} ignored")
                    } else {
                        String::new()
                    };
                    Ok(mcp_text(&format!(
                        "Picked {applied} change(s) from branch '{}' into '{}' ({skipped} skipped{ignored_suffix})",
                        pick_args.source, pick_args.target
                    )))
                }
                PickSelector::SnapshotRange {
                    from_snapshot,
                    to_snapshot,
                } => {
                    let from_internal = resolve_snapshot_for_user(svc, user_id, &from_snapshot)
                        .await?
                        .ok_or_else(|| {
                            MemoriaError::NotFound(format!("Snapshot '{from_snapshot}'"))
                        })?;
                    let to_internal = resolve_snapshot_for_user(svc, user_id, &to_snapshot)
                        .await?
                        .ok_or_else(|| {
                            MemoriaError::NotFound(format!("Snapshot '{to_snapshot}'"))
                        })?;
                    if from_internal == to_internal {
                        if let Some(dry_run) = dry_run.as_ref() {
                            return Ok(mcp_json(&build_pick_preview_response(
                                &pick_args.source,
                                &pick_args.target,
                                strategy,
                                selector_value.clone(),
                                dry_run,
                                &[],
                            )));
                        }
                        return Ok(mcp_text(&format!(
                            "No pickable changes found in branch '{}' between snapshots '{}' and '{}'.",
                            pick_args.source, from_snapshot, to_snapshot
                        )));
                    }
                    if let Some(dry_run) = dry_run.as_ref() {
                        let candidates = load_snapshot_range_pick_candidates(
                            &sql,
                            user_id,
                            &source_table,
                            &target_table,
                            &from_internal,
                            &to_internal,
                        )
                        .await?;
                        let ranked = candidates
                            .into_iter()
                            .map(|candidate| RankedPickCandidate {
                                memory_id: candidate.memory_id,
                                content: candidate.content,
                                score: None,
                                target_exists: candidate.target_exists,
                            })
                            .collect::<Vec<_>>();
                        return Ok(mcp_json(&build_pick_preview_response(
                            &pick_args.source,
                            &pick_args.target,
                            strategy,
                            selector_value.clone(),
                            dry_run,
                            &ranked,
                        )));
                    }
                    if let Err(err) = git
                        .pick_branch_snapshot_range(
                            &source_table_name,
                            &target_table_name,
                            &from_internal,
                            &to_internal,
                            strategy,
                        )
                        .await
                    {
                        return map_pick_conflict(err, &pick_args.source, &pick_args.target, None);
                    }
                    Ok(mcp_text(&format!(
                        "Picked snapshot range '{}'..'{}' from branch '{}' into '{}'.",
                        from_snapshot, to_snapshot, pick_args.source, pick_args.target
                    )))
                }
                PickSelector::Retrieve {
                    query,
                    top_k,
                    min_score,
                } => {
                    if top_k <= 0 || top_k > MAX_PICK_TOP_K {
                        return Err(MemoriaError::Validation(format!(
                            "retrieve top_k must be between 1 and {MAX_PICK_TOP_K}"
                        )));
                    }
                    let candidates = load_pick_candidates(
                        &sql,
                        user_id,
                        &source_table,
                        &target_table,
                        None,
                        None,
                    )
                    .await?;
                    let select_ctx = PickSelectionContext {
                        svc,
                        sql: &sql,
                        user_id,
                        source_table: &source_table,
                        target_table: &target_table,
                    };
                    let selected = select_retrieved_pick_candidates(
                        &select_ctx,
                        &query,
                        top_k,
                        min_score,
                        candidates,
                    )
                    .await?;
                    if let Some(dry_run) = dry_run.as_ref() {
                        return Ok(mcp_json(&build_pick_preview_response(
                            &pick_args.source,
                            &pick_args.target,
                            strategy,
                            selector_value.clone(),
                            dry_run,
                            &selected,
                        )));
                    }
                    let selected_keys = selected
                        .iter()
                        .map(|candidate| candidate.memory_id.clone())
                        .collect::<Vec<_>>();
                    if selected_keys.is_empty() {
                        return Ok(mcp_text(&format!(
                            "No pickable changes matched query '{}' in branch '{}'.",
                            query, pick_args.source
                        )));
                    }
                    if let Err(err) = git
                        .pick_branch_keys(
                            &source_table_name,
                            &target_table_name,
                            &selected_keys,
                            strategy,
                        )
                        .await
                    {
                        return map_pick_conflict(
                            err,
                            &pick_args.source,
                            &pick_args.target,
                            Some(selected_keys.len()),
                        );
                    }
                    let remaining = count_pick_candidates(
                        &sql,
                        user_id,
                        &source_table,
                        &target_table,
                        Some(&selected_keys),
                    )
                    .await?;
                    let applied = selected_keys.len().saturating_sub(remaining as usize);
                    let skipped = if strategy == "skip" {
                        remaining.max(0) as usize
                    } else {
                        0
                    };
                    Ok(mcp_text(&format!(
                        "Picked {applied} of top {top_k} retrieved change(s) from branch '{}' into '{}' ({skipped} skipped)",
                        pick_args.source, pick_args.target
                    )))
                }
            }
        }

        GitToolCallName::MemoryBranchDelete => {
            let branch = args["name"].as_str().unwrap_or("");
            if branch == "main" {
                return Ok(crate::tool_result::error("Cannot delete main"));
            }
            let sql = svc.user_sql_store(user_id).await?;
            let git = git_for_store(&sql)?;
            let branches = sql.list_branches(user_id).await?;
            if let Some((_, table_name, _)) = branches.iter().find(|(name, _, _)| name == branch) {
                let was_active = sql.active_branch_name(user_id).await? == branch;
                if was_active {
                    sql.set_active_branch(user_id, "main").await?;
                }
                git.drop_branch(table_name).await.map_err(git_err)?;
                sql.deregister_branch(user_id, branch).await?;
                Ok(mcp_text(&format!("Deleted branch '{branch}'")))
            } else {
                Ok(crate::tool_result::classified_error(crate::tool_result::ErrorKind::Rejected,
                    format!("Branch '{branch}' not found. List branches and check the name before retrying.")))
            }
        }

        GitToolCallName::MemoryDiff => {
            expect_tool_args(&args, "memory_diff", &["source", "limit"])?;
            let source_branch = parse_required_string_arg(&args, "memory_diff", "source")?;
            let limit = parse_optional_i64_arg(&args, "memory_diff", "limit", 50)?.clamp(1, 500);
            let sql = svc.user_sql_store(user_id).await?;
            let user_git = git_for_store(&sql)?;
            let branches = sql.list_branches(user_id).await?;
            let table_name = branches
                .iter()
                .find(|(name, _, _)| name == source_branch.as_str())
                .map(|(_, t, _)| t.clone())
                .ok_or_else(|| MemoriaError::NotFound(format!("Branch '{source_branch}'")))?;

            // Use MatrixOne native `data branch diff` + classify
            let raw_rows = user_git
                .diff_branch_rows(&table_name, "mem_memories", user_id, limit * 3)
                .await?;
            let mut classified = memoria_git::classify_diff_rows(raw_rows, &table_name);
            // Resolve ghost removes (created-then-deleted on branch, never in main)
            user_git
                .resolve_ghost_removes(&mut classified, "mem_memories", user_id)
                .await?;

            let total = classified.added.len()
                + classified.updated.len()
                + classified.removed.len()
                + classified.conflicts.len();
            if total == 0 {
                return Ok(mcp_text(&format!(
                    "No changes in branch '{source_branch}' vs main."
                )));
            }

            let preview = |content: &str| -> String {
                if content.len() > 80 {
                    let mut end = 80;
                    while !content.is_char_boundary(end) {
                        end -= 1;
                    }
                    format!("{}...", &content[..end])
                } else {
                    content.to_string()
                }
            };
            let short_id = |id: &str| -> String { id[..8.min(id.len())].to_string() };

            let mut lines: Vec<String> = Vec::new();
            let limit_usize = limit.max(0) as usize;

            for item in classified.added.iter().take(limit_usize) {
                lines.push(format!(
                    "[new] {}: {}",
                    short_id(&item.memory_id),
                    preview(&item.content)
                ));
            }
            for pair in classified.updated.iter().take(limit_usize) {
                lines.push(format!(
                    "[modified] {} → {}: {}",
                    short_id(&pair.old_memory_id),
                    short_id(&pair.new_memory_id),
                    preview(&pair.new_content)
                ));
            }
            for item in classified.removed.iter().take(limit_usize) {
                lines.push(format!(
                    "[removed] {}: {}",
                    short_id(&item.memory_id),
                    preview(&item.content)
                ));
            }
            for c in classified.conflicts.iter().take(limit_usize) {
                let br_sup = c
                    .branch_side
                    .superseded_by_content
                    .as_deref()
                    .map(|s| format!(" →new: {}", preview(s)))
                    .unwrap_or_default();
                let mn_sup = c
                    .main_side
                    .superseded_by_content
                    .as_deref()
                    .map(|s| format!(" →new: {}", preview(s)))
                    .unwrap_or_default();
                lines.push(format!(
                    "[CONFLICT] {}: branch({}{}), main({}{})",
                    short_id(&c.memory_id),
                    preview(&c.branch_side.content),
                    br_sup,
                    preview(&c.main_side.content),
                    mn_sup,
                ));
            }
            for item in classified.behind_main.iter().take(limit_usize) {
                lines.push(format!(
                    "[behind] {}: {}",
                    short_id(&item.memory_id),
                    preview(&item.content)
                ));
            }

            let shown = lines.len();
            let truncated = if total > shown {
                format!(" (showing {shown}/{total})")
            } else {
                String::new()
            };
            Ok(mcp_text(&format!(
                "Diff '{source_branch}' vs main{truncated}:\n{}",
                lines.join("\n")
            )))
        }

        GitToolCallName::MemoryApply => {
            expect_tool_args(
                &args,
                "memory_apply",
                &[
                    "source",
                    "adds",
                    "updates",
                    "removes",
                    "accept_branch_conflicts",
                ],
            )?;
            let source_branch = parse_required_string_arg(&args, "memory_apply", "source")?;
            if source_branch == "main" {
                return Ok(crate::tool_result::error("Cannot apply from main"));
            }
            let selection = memoria_git::ApplySelection {
                adds: parse_apply_string_array(&args, "adds")?,
                updates: parse_apply_updates(&args)?,
                removes: parse_apply_string_array(&args, "removes")?,
                accept_branch_conflicts: parse_apply_string_array(
                    &args,
                    "accept_branch_conflicts",
                )?,
            };
            if selection.adds.is_empty()
                && selection.updates.is_empty()
                && selection.removes.is_empty()
                && selection.accept_branch_conflicts.is_empty()
            {
                return Ok(crate::tool_result::error(
                    "Nothing to apply. Provide at least one of: adds, updates, removes, accept_branch_conflicts.",
                ));
            }

            let sql = svc.user_sql_store(user_id).await?;
            let branches = sql.list_branches(user_id).await?;
            let table_name = branches
                .iter()
                .find(|(name, _, _)| name == &source_branch)
                .map(|(_, t, _)| t.clone())
                .ok_or_else(|| MemoriaError::NotFound(format!("Branch '{source_branch}'")))?;
            let branch_table_name = validate_identifier(&table_name)?.to_string();
            let user_git = git_for_store(&sql)?;
            let report = user_git
                .selective_apply(&branch_table_name, "mem_memories", user_id, selection)
                .await?;
            Ok(mcp_text(&format!(
                "Applied from '{source_branch}': {} added, {} updated, {} removed, {} conflicts accepted",
                report.applied_adds.len(),
                report.applied_updates.len(),
                report.applied_removes.len(),
                report.applied_conflicts.len()
            )))
        }

        GitToolCallName::Unknown(name) => {
            Err(MemoriaError::NotFound(format!("Unknown git tool: {name}")))
        }
    }
}

pub async fn call_owned(
    name: String,
    args: Value,
    git: Arc<GitForDataService>,
    svc: Arc<MemoryService>,
    user_id: String,
) -> Result<Value, MemoriaError> {
    call(&name, args, &git, &svc, &user_id).await
}

async fn collect_replace_candidates(
    pool: &sqlx::MySqlPool,
    main_table: &str,
    branch_table: &str,
    user_id: &str,
    l2_conflict: f64,
) -> Result<Vec<ReplaceCandidate>, MemoriaError> {
    let sql = format!(
        "SELECT m.memory_id AS main_memory_id, \
                b.memory_id AS branch_memory_id, \
                b.content AS replacement_content, \
                CAST(l2_distance(m.embedding, b.embedding) AS DOUBLE) AS conflict_distance \
         FROM {main_table} m \
         JOIN {branch_table} b \
           ON b.user_id = ? AND b.is_active = 1 \
           AND b.content IS NOT NULL \
           AND b.memory_type = m.memory_type \
           AND b.subject_id <=> m.subject_id \
           AND b.embedding IS NOT NULL AND vector_dims(b.embedding) > 0 \
         WHERE m.user_id = ? AND m.is_active = 1 \
           AND m.embedding IS NOT NULL AND vector_dims(m.embedding) > 0 \
           AND NOT EXISTS (SELECT 1 FROM {main_table} m2 WHERE m2.memory_id = b.memory_id AND m2.is_active = 1) \
           AND l2_distance(m.embedding, b.embedding) < {l2_conflict}"
    );
    let rows = sqlx::query(&sql)
        .bind(user_id)
        .bind(user_id)
        .fetch_all(pool)
        .await
        .map_err(db_err)?;

    let mut chosen: HashMap<String, ReplaceCandidate> = HashMap::new();
    for row in rows {
        let candidate = ReplaceCandidate {
            main_memory_id: row.try_get("main_memory_id").map_err(db_err)?,
            branch_memory_id: row.try_get("branch_memory_id").map_err(db_err)?,
            replacement_content: row.try_get("replacement_content").map_err(db_err)?,
            conflict_distance: row.try_get("conflict_distance").map_err(db_err)?,
        };
        match chosen.get_mut(&candidate.main_memory_id) {
            Some(current)
                if candidate.conflict_distance < current.conflict_distance
                    || (candidate.conflict_distance == current.conflict_distance
                        && candidate.branch_memory_id < current.branch_memory_id) =>
            {
                *current = candidate;
            }
            None => {
                chosen.insert(candidate.main_memory_id.clone(), candidate);
            }
            _ => {}
        }
    }

    let mut replacements = chosen.into_values().collect::<Vec<_>>();
    replacements.sort_by(|a, b| a.main_memory_id.cmp(&b.main_memory_id));
    Ok(replacements)
}

fn normalize_pick_strategy(strategy: &str) -> Result<&str, MemoriaError> {
    match strategy {
        "fail" | "skip" | "accept" => Ok(strategy),
        other => Err(MemoriaError::Validation(format!(
            "Unsupported pick strategy '{other}'. Use fail, skip, or accept."
        ))),
    }
}

fn normalize_keys(keys: Vec<String>) -> Result<Vec<String>, MemoriaError> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for key in keys {
        let trimmed = key.trim();
        if trimmed.is_empty() {
            continue;
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    if out.is_empty() {
        return Err(MemoriaError::Validation(
            "key_list selector requires at least one non-empty key".into(),
        ));
    }
    if out.len() > MAX_PICK_KEYS {
        return Err(MemoriaError::Validation(format!(
            "key_list selector supports at most {MAX_PICK_KEYS} unique keys"
        )));
    }
    Ok(out)
}

#[derive(Debug, Clone)]
struct NormalizedPickDryRunOptions {
    limit: usize,
    offset: usize,
    include_content_preview: bool,
    include_scores: bool,
}

fn normalize_pick_dry_run(
    dry_run: Option<PickDryRunOptions>,
) -> Result<Option<NormalizedPickDryRunOptions>, MemoriaError> {
    let Some(dry_run) = dry_run else {
        return Ok(None);
    };
    if dry_run.limit <= 0 || dry_run.limit > MAX_PICK_PREVIEW_LIMIT {
        return Err(MemoriaError::Validation(format!(
            "dry_run.limit must be between 1 and {MAX_PICK_PREVIEW_LIMIT}"
        )));
    }
    if dry_run.offset < 0 {
        return Err(MemoriaError::Validation(
            "dry_run.offset must be greater than or equal to 0".into(),
        ));
    }
    Ok(Some(NormalizedPickDryRunOptions {
        limit: dry_run.limit as usize,
        offset: dry_run.offset as usize,
        include_content_preview: dry_run.include_content_preview,
        include_scores: dry_run.include_scores,
    }))
}

fn preview_content(text: &str) -> String {
    const MAX_PREVIEW_CHARS: usize = 160;
    let mut out = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= MAX_PREVIEW_CHARS {
            out.push_str("...");
            break;
        }
        out.push(ch);
    }
    out
}

fn preview_status(strategy: &str, target_exists: bool) -> (&'static str, &'static str) {
    if !target_exists {
        return ("would_apply", "new_or_updated_row");
    }
    match strategy {
        "skip" => ("would_skip", "conflict_skip"),
        "accept" => ("would_apply", "conflict_accept"),
        _ => ("conflict", "conflict_fail"),
    }
}

fn build_pick_preview_response(
    source: &str,
    target: &str,
    strategy: &str,
    selector: Value,
    dry_run: &NormalizedPickDryRunOptions,
    ranked: &[RankedPickCandidate],
) -> Value {
    let candidate_count = ranked.len();
    let conflict_count = ranked
        .iter()
        .filter(|candidate| candidate.target_exists)
        .count();
    let would_abort = strategy == "fail" && conflict_count > 0;
    let would_skip = if strategy == "skip" {
        conflict_count
    } else {
        0
    };
    let would_apply = if would_abort {
        0
    } else if strategy == "skip" {
        candidate_count.saturating_sub(conflict_count)
    } else {
        candidate_count
    };
    let shown: Vec<_> = ranked
        .iter()
        .skip(dry_run.offset)
        .take(dry_run.limit)
        .map(|candidate| {
            let (status, reason) = preview_status(strategy, candidate.target_exists);
            PickPreviewCandidate {
                memory_id: candidate.memory_id.clone(),
                content_preview: dry_run
                    .include_content_preview
                    .then(|| preview_content(&candidate.content)),
                score: if dry_run.include_scores {
                    candidate.score
                } else {
                    None
                },
                status,
                reason,
            }
        })
        .collect();
    let shown_count = shown.len();
    let has_more = dry_run.offset.saturating_add(shown_count) < candidate_count;
    json!({
        "dry_run": true,
        "result": format!(
            "Previewed {candidate_count} candidate change(s) from branch '{source}' into '{target}'. Showing {shown_count}."
        ),
        "source": source,
        "target": target,
        "strategy": strategy,
        "selector": selector,
        "summary": PickPreviewSummary {
            candidate_count,
            shown_count,
            would_apply,
            would_skip,
            conflict_count,
            would_abort,
        },
        "page": PickPreviewPage {
            limit: dry_run.limit,
            offset: dry_run.offset,
            has_more,
        },
        "candidates": shown,
    })
}

struct PickSelectionContext<'a> {
    svc: &'a Arc<MemoryService>,
    sql: &'a Arc<memoria_storage::SqlMemoryStore>,
    user_id: &'a str,
    source_table: &'a str,
    target_table: &'a str,
}

async fn resolve_branch_table_name(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    user_id: &str,
    branch: &str,
    allow_main: bool,
) -> Result<String, MemoriaError> {
    if allow_main && branch == "main" {
        return Ok("mem_memories".to_string());
    }
    sql.list_branches(user_id)
        .await?
        .into_iter()
        .find(|(name, _, _)| name == branch)
        .map(|(_, table, _)| table)
        .ok_or_else(|| MemoriaError::NotFound(format!("Branch '{branch}'")))
}

fn pickable_diff_clause(source_alias: &str, target_alias: &str) -> String {
    format!(
        "{target_alias}.memory_id IS NULL \
         OR COALESCE({target_alias}.content, '') <> COALESCE({source_alias}.content, '') \
         OR COALESCE({target_alias}.is_active, 0) <> COALESCE({source_alias}.is_active, 0)"
    )
}

async fn load_pick_candidates(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    user_id: &str,
    source_table: &str,
    target_table: &str,
    keys: Option<&[String]>,
    limit: Option<i64>,
) -> Result<Vec<PickCandidate>, MemoriaError> {
    let diff_clause = pickable_diff_clause("s", "t");
    let mut qb: QueryBuilder<MySql> = QueryBuilder::new(format!(
        "SELECT s.memory_id, COALESCE(s.content, '') AS content, \
         COALESCE(s.memory_type, 'semantic') AS memory_type, \
         CASE WHEN t.memory_id IS NULL THEN 0 ELSE 1 END AS target_exists \
         FROM {source_table} s \
         LEFT JOIN {target_table} t ON t.memory_id = s.memory_id \
         WHERE s.user_id = "
    ));
    qb.push_bind(user_id);
    qb.push(" AND (");
    qb.push(&diff_clause);
    qb.push(")");
    if let Some(keys) = keys {
        qb.push(" AND s.memory_id IN (");
        {
            let mut separated = qb.separated(", ");
            for key in keys {
                separated.push_bind(key);
            }
        }
        qb.push(")");
    }
    qb.push(" ORDER BY s.updated_at DESC, s.created_at DESC");
    if let Some(limit) = limit {
        qb.push(" LIMIT ");
        qb.push_bind(limit.max(1));
    }

    let rows = qb.build().fetch_all(sql.pool()).await.map_err(db_err)?;
    rows.into_iter()
        .map(|row| {
            Ok(PickCandidate {
                memory_id: row.try_get("memory_id").map_err(db_err)?,
                content: row.try_get("content").unwrap_or_default(),
                memory_type: row
                    .try_get("memory_type")
                    .unwrap_or_else(|_| "semantic".to_string()),
                target_exists: row.try_get::<i64, _>("target_exists").unwrap_or(0) != 0,
            })
        })
        .collect()
}

async fn load_snapshot_range_pick_candidates(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    user_id: &str,
    source_table: &str,
    target_table: &str,
    from_snapshot: &str,
    to_snapshot: &str,
) -> Result<Vec<PickCandidate>, MemoriaError> {
    let from_rows = load_snapshot_pick_rows(sql, user_id, source_table, from_snapshot).await?;
    let from_map: HashMap<_, _> = from_rows
        .into_iter()
        .map(|row| (row.memory_id.clone(), row))
        .collect();

    let changed_rows: Vec<_> = load_snapshot_pick_rows(sql, user_id, source_table, to_snapshot)
        .await?
        .into_iter()
        .filter(|row| {
            from_map
                .get(&row.memory_id)
                .is_none_or(|prev| prev.content != row.content || prev.is_active != row.is_active)
        })
        .collect();

    let candidate_ids = changed_rows
        .iter()
        .map(|row| row.memory_id.clone())
        .collect::<Vec<_>>();
    let target_rows = load_target_pick_rows(sql, user_id, target_table, &candidate_ids).await?;

    Ok(changed_rows
        .into_iter()
        .filter_map(|row| {
            let target_row = target_rows.get(&row.memory_id);
            if target_row.is_some_and(|target| {
                target.content == row.content && target.is_active == row.is_active
            }) {
                return None;
            }
            Some(PickCandidate {
                target_exists: target_row.is_some(),
                memory_id: row.memory_id,
                content: row.content,
                memory_type: row.memory_type,
            })
        })
        .collect())
}

async fn load_snapshot_pick_rows(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    user_id: &str,
    source_table: &str,
    snapshot: &str,
) -> Result<Vec<SnapshotPickRow>, MemoriaError> {
    let mut qb: QueryBuilder<MySql> = QueryBuilder::new(format!(
        "SELECT memory_id, COALESCE(content, '') AS content, \
         COALESCE(memory_type, 'semantic') AS memory_type, COALESCE(is_active, 0) AS is_active \
         FROM {source_table} {{SNAPSHOT = '"
    ));
    qb.push(snapshot);
    qb.push("'} WHERE user_id = ");
    qb.push_bind(user_id);
    qb.push(" ORDER BY updated_at DESC, created_at DESC");

    let rows = qb.build().fetch_all(sql.pool()).await.map_err(db_err)?;
    rows.into_iter()
        .map(|row| {
            Ok(SnapshotPickRow {
                memory_id: row.try_get("memory_id").map_err(db_err)?,
                content: row.try_get("content").unwrap_or_default(),
                memory_type: row
                    .try_get("memory_type")
                    .unwrap_or_else(|_| "semantic".to_string()),
                is_active: row.try_get::<i64, _>("is_active").unwrap_or(0) != 0,
            })
        })
        .collect()
}

async fn load_target_pick_rows(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    user_id: &str,
    target_table: &str,
    memory_ids: &[String],
) -> Result<HashMap<String, TargetPickRow>, MemoriaError> {
    if memory_ids.is_empty() {
        return Ok(HashMap::new());
    }

    let mut qb: QueryBuilder<MySql> = QueryBuilder::new(format!(
        "SELECT memory_id, COALESCE(content, '') AS content, COALESCE(is_active, 0) AS is_active \
         FROM {target_table} WHERE user_id = "
    ));
    qb.push_bind(user_id);
    qb.push(" AND memory_id IN (");
    {
        let mut separated = qb.separated(", ");
        for memory_id in memory_ids {
            separated.push_bind(memory_id);
        }
    }
    qb.push(")");

    let rows = qb.build().fetch_all(sql.pool()).await.map_err(db_err)?;
    Ok(rows
        .into_iter()
        .filter_map(|row| {
            Some((
                row.try_get::<String, _>("memory_id").ok()?,
                TargetPickRow {
                    content: row.try_get("content").unwrap_or_default(),
                    is_active: row.try_get::<i64, _>("is_active").unwrap_or(0) != 0,
                },
            ))
        })
        .collect())
}

async fn count_pick_candidates(
    sql: &Arc<memoria_storage::SqlMemoryStore>,
    user_id: &str,
    source_table: &str,
    target_table: &str,
    keys: Option<&[String]>,
) -> Result<i64, MemoriaError> {
    let diff_clause = pickable_diff_clause("s", "t");
    let mut qb: QueryBuilder<MySql> = QueryBuilder::new(format!(
        "SELECT COUNT(*) AS cnt FROM {source_table} s \
         LEFT JOIN {target_table} t ON t.memory_id = s.memory_id \
         WHERE s.user_id = "
    ));
    qb.push_bind(user_id);
    qb.push(" AND (");
    qb.push(&diff_clause);
    qb.push(")");
    if let Some(keys) = keys {
        qb.push(" AND s.memory_id IN (");
        {
            let mut separated = qb.separated(", ");
            for key in keys {
                separated.push_bind(key);
            }
        }
        qb.push(")");
    }
    let row = qb.build().fetch_one(sql.pool()).await.map_err(db_err)?;
    row.try_get::<i64, _>("cnt").map_err(db_err)
}

fn sanitize_sql_literal(value: &str) -> String {
    value
        .chars()
        .filter(|c| *c != '\0')
        .fold(String::with_capacity(value.len()), |mut out, c| {
            match c {
                '\'' => out.push_str("''"),
                '\\' => out.push_str("\\\\"),
                _ => out.push(c),
            }
            out
        })
}

fn sanitize_fulltext_query(value: &str) -> String {
    value
        .chars()
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
        .collect::<Vec<_>>()
        .join(" ")
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

async fn select_retrieved_pick_candidates(
    ctx: &PickSelectionContext<'_>,
    query: &str,
    top_k: i64,
    min_score: Option<f64>,
    candidates: Vec<PickCandidate>,
) -> Result<Vec<RankedPickCandidate>, MemoriaError> {
    if let Some(min_score) = min_score {
        if !min_score.is_finite() || min_score < 0.0 {
            return Err(MemoriaError::Validation(
                "retrieve min_score must be a finite number greater than or equal to 0".into(),
            ));
        }
    }
    if candidates.is_empty() {
        return Ok(vec![]);
    }

    let fetch_k = (top_k * 3).max(20);
    let mut scores: HashMap<String, f64> = HashMap::new();
    let diff_clause = pickable_diff_clause("s", "t");
    let safe_user_id = sanitize_sql_literal(ctx.user_id);

    if let Some(embedding) = ctx.svc.embed(query).await? {
        let vec_literal = vec_to_mo(&embedding);
        let sql_text = format!(
            "SELECT s.memory_id, CAST(l2_distance(s.embedding, '{vec_literal}') AS DOUBLE) AS l2_dist \
             FROM {source_table} s \
             LEFT JOIN {target_table} t ON t.memory_id = s.memory_id \
              WHERE s.user_id = '{safe_user_id}' AND s.is_active = 1 \
                AND s.embedding IS NOT NULL AND vector_dims(s.embedding) > 0 \
                AND ({diff_clause}) \
              ORDER BY l2_distance(s.embedding, '{vec_literal}') ASC \
              LIMIT {fetch_k} by rank with option 'mode=post'"
            ,
            source_table = ctx.source_table,
            target_table = ctx.target_table
        );
        let rows = sqlx::query(&sql_text)
            .fetch_all(ctx.sql.pool())
            .await
            .map_err(db_err)?;
        for row in rows {
            let memory_id: String = row.try_get("memory_id").map_err(db_err)?;
            let l2_dist: f64 = row.try_get("l2_dist").map_err(db_err)?;
            scores.insert(memory_id, 0.7 * (1.0 / (1.0 + l2_dist.max(0.0))));
        }
    }

    let safe_query = sanitize_fulltext_query(query);
    if !safe_query.is_empty() {
        let sql_text = format!(
            "SELECT s.memory_id, CAST(MATCH(s.content) AGAINST('{safe_query}' IN BOOLEAN MODE) AS DOUBLE) AS ft_score \
             FROM {source_table} s \
             LEFT JOIN {target_table} t ON t.memory_id = s.memory_id \
             WHERE s.user_id = ? AND s.is_active = 1 \
                AND MATCH(s.content) AGAINST('{safe_query}' IN BOOLEAN MODE) \
                AND ({diff_clause}) \
              ORDER BY ft_score DESC LIMIT ?"
            ,
            source_table = ctx.source_table,
            target_table = ctx.target_table
        );
        let rows = match sqlx::query(&sql_text)
            .bind(ctx.user_id)
            .bind(fetch_k)
            .fetch_all(ctx.sql.pool())
            .await
        {
            Ok(rows) => rows,
            Err(err) => {
                let msg = err.to_string();
                if (msg.contains("20101") && msg.contains("empty pattern"))
                    || (msg.contains("20105") && msg.contains("not supported"))
                {
                    vec![]
                } else {
                    return Err(db_err(err));
                }
            }
        };
        for row in rows {
            let memory_id: String = row.try_get("memory_id").map_err(db_err)?;
            let ft_score: f64 = row.try_get("ft_score").unwrap_or(0.0);
            scores
                .entry(memory_id)
                .and_modify(|score| *score += 0.3 * ft_score.max(0.0))
                .or_insert(0.3 * ft_score.max(0.0));
        }
    }

    let mut ranked = candidates
        .into_iter()
        .map(|candidate| {
            let lexical_overlap = if safe_query.is_empty() {
                0.0
            } else {
                safe_query
                    .split_whitespace()
                    .filter(|token| {
                        candidate
                            .content
                            .to_lowercase()
                            .contains(&token.to_lowercase())
                    })
                    .count() as f64
            };
            let type_bias = if candidate.memory_type == "semantic" {
                0.001
            } else {
                0.0
            };
            let score = scores
                .get(&candidate.memory_id)
                .copied()
                .unwrap_or(lexical_overlap)
                + type_bias;
            RankedPickCandidate {
                memory_id: candidate.memory_id,
                content: candidate.content,
                score: Some(score),
                target_exists: candidate.target_exists,
            }
        })
        .collect::<Vec<_>>();
    ranked.sort_by(|a, b| {
        b.score
            .unwrap_or(0.0)
            .partial_cmp(&a.score.unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.memory_id.cmp(&b.memory_id))
    });
    if let Some(min_score) = min_score {
        ranked.retain(|candidate| candidate.score.unwrap_or(0.0) >= min_score);
    }
    ranked.truncate(top_k as usize);
    Ok(ranked)
}

fn map_pick_conflict(
    err: MemoriaError,
    source: &str,
    target: &str,
    selected: Option<usize>,
) -> Result<Value, MemoriaError> {
    let message = err.to_string();
    if is_pick_conflict_error_message(&message) {
        let selection_detail = selected
            .map(|count| format!(" for {count} selected change(s)"))
            .unwrap_or_default();
        return Err(MemoriaError::Validation(format!(
            "Conflict: pick from branch '{source}' into '{target}' aborted{selection_detail}: {message}"
        )));
    }
    Err(err)
}

fn is_pick_parser_error_message(message: &str) -> bool {
    let lower = message.to_lowercase();
    lower.contains("sql parser error")
        && (lower.contains("data branch pick")
            || (lower.contains("near \" pick") && lower.contains("when conflict")))
}

fn is_pick_conflict_error_message(message: &str) -> bool {
    if is_pick_parser_error_message(message) {
        return false;
    }
    message.to_lowercase().contains("conflict")
}

fn mcp_text(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

fn mcp_json(value: &Value) -> Value {
    mcp_text(&serde_json::to_string_pretty(value).unwrap_or_default())
}

fn expect_tool_args<'a>(
    args: &'a Value,
    tool: &str,
    allowed: &[&str],
) -> Result<&'a serde_json::Map<String, Value>, MemoriaError> {
    let map = args.as_object().ok_or_else(|| {
        MemoriaError::Validation(format!("Invalid {tool} arguments: expected object"))
    })?;
    for key in map.keys() {
        if !allowed.iter().any(|allowed_key| allowed_key == key) {
            return Err(MemoriaError::Validation(format!(
                "Invalid {tool} argument '{key}': unknown field"
            )));
        }
    }
    Ok(map)
}

fn parse_required_string_arg(
    args: &Value,
    tool: &str,
    field: &str,
) -> Result<String, MemoriaError> {
    args.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .ok_or_else(|| {
            MemoriaError::Validation(format!(
                "Invalid {tool} '{field}': expected non-empty string"
            ))
        })
}

fn parse_optional_i64_arg(
    args: &Value,
    tool: &str,
    field: &str,
    default: i64,
) -> Result<i64, MemoriaError> {
    match args.get(field) {
        None => Ok(default),
        Some(value) => value.as_i64().ok_or_else(|| {
            MemoriaError::Validation(format!("Invalid {tool} '{field}': expected integer"))
        }),
    }
}

fn parse_apply_string_array(args: &Value, field: &str) -> Result<Vec<String>, MemoriaError> {
    match args.get(field) {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .enumerate()
            .map(|(idx, value)| {
                value.as_str().map(str::to_string).ok_or_else(|| {
                    MemoriaError::Validation(format!(
                        "Invalid memory_apply '{field}[{idx}]': expected string"
                    ))
                })
            })
            .collect(),
        Some(_) => Err(MemoriaError::Validation(format!(
            "Invalid memory_apply '{field}': expected array"
        ))),
    }
}

fn parse_apply_updates(args: &Value) -> Result<Vec<memoria_git::ApplyUpdatePair>, MemoriaError> {
    match args.get("updates") {
        None => Ok(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .enumerate()
            .map(|(idx, value)| {
                let old_id = value.get("old_id").and_then(Value::as_str).ok_or_else(|| {
                    MemoriaError::Validation(format!(
                        "Invalid memory_apply 'updates[{idx}].old_id': expected string"
                    ))
                })?;
                let new_id = value.get("new_id").and_then(Value::as_str).ok_or_else(|| {
                    MemoriaError::Validation(format!(
                        "Invalid memory_apply 'updates[{idx}].new_id': expected string"
                    ))
                })?;
                Ok(memoria_git::ApplyUpdatePair {
                    old_id: old_id.to_string(),
                    new_id: new_id.to_string(),
                })
            })
            .collect(),
        Some(_) => Err(MemoriaError::Validation(
            "Invalid memory_apply 'updates': expected array".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn snapshot_delete_rejects_blank_prefix_before_storage() {
        use std::sync::Arc;

        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy("mysql://test:test@127.0.0.1/test")
            .unwrap();
        let store = Arc::new(memoria_storage::SqlMemoryStore::new(
            pool.clone(),
            3,
            "test".into(),
        ));
        let service = Arc::new(memoria_service::MemoryService::new(store, None, None));
        let git = Arc::new(memoria_git::GitForDataService::new(pool, "test"));
        for args in [
            serde_json::json!({"prefix": ""}),
            serde_json::json!({"prefix": " "}),
            serde_json::json!({"prefix": "\t\n"}),
            serde_json::json!({"prefix": "\u{2003}"}),
            serde_json::json!({"prefix": "", "names": "test"}),
        ] {
            let result = super::call(
                "memory_snapshot_delete",
                args.clone(),
                &git,
                &service,
                "test",
            )
            .await
            .unwrap();
            assert_eq!(result["isError"], true, "args: {args:?}");
            assert!(result
                .to_string()
                .contains("Snapshot prefix must not be blank"));
        }
    }

    #[test]
    fn branch_suffix_is_ascii_without_changing_unicode_snapshot_names() {
        for name in ["实验", "café", "分支 / 测试", "🚀"] {
            let suffix = super::sanitize_branch_table_suffix(name);
            assert!(suffix.is_ascii());
            assert!(memoria_core::is_safe_sql_identifier(&suffix));
        }
        assert_eq!(
            super::sanitize_branch_table_suffix("my_branch-1"),
            super::sanitize_name("my_branch-1")
        );
        assert_eq!(super::sanitize_name("实验"), "实验");
        assert!(super::snap_internal("test", "实验").ends_with("_实验"));
    }

    use super::{
        expect_tool_args, is_pick_conflict_error_message, is_pick_parser_error_message,
        map_pick_conflict, normalize_keys, parse_apply_string_array, parse_apply_updates,
        safety_prefix, snap_internal, validate_identifier, MAX_IDENTIFIER_LEN, MAX_PICK_KEYS,
    };
    use memoria_core::MemoriaError;
    use serde_json::json;

    #[test]
    fn scoped_snapshot_internal_names_stay_within_matrixone_limit() {
        let internal = snap_internal(
            "memoria_shared_db_with_a_really_long_name_for_product_runs",
            "snapshot_name_that_is_long_but_should_still_fit_with_db_scope",
        );
        assert!(internal.len() <= MAX_IDENTIFIER_LEN, "{internal}");
    }

    #[test]
    fn scoped_safety_prefix_stays_bounded() {
        let prefix = safety_prefix(Some(
            "memoria_shared_db_with_a_really_long_name_for_product_runs",
        ));
        assert!(prefix.len() < MAX_IDENTIFIER_LEN, "{prefix}");
    }

    #[test]
    fn safety_classification_covers_legacy_scoped_and_compacted_prefixes() {
        assert!(super::is_safety_snapshot_name("mem_snap_pre_purge", None));
        for db_name in ["snapshot_test", "snapshot_test_database_with_a_long_name"] {
            assert!(super::is_safety_snapshot_name(
                "mem_snap_pre_purge",
                Some(db_name),
            ));
            assert!(super::is_safety_snapshot_name(
                &format!("mem_snap_{db_name}_pre_purge"),
                Some(db_name),
            ));
            assert!(super::is_safety_snapshot_name(
                &format!("{}purge", safety_prefix(Some(db_name))),
                Some(db_name),
            ));
            assert!(!super::is_safety_snapshot_name(
                "mem_snap_another_database_pre_purge",
                Some(db_name),
            ));
            assert!(!super::is_safety_snapshot_name(
                &snap_internal("test_scope", "pre_release"),
                Some(db_name),
            ));
            assert!(!super::is_safety_snapshot_name(
                "mem_milestone_checkpoint",
                Some(db_name),
            ));
        }
        let long_db = "snapshot_test_database_with_a_long_name";
        assert_ne!(
            safety_prefix(Some(long_db)),
            format!("mem_snap_{long_db}_pre_")
        );
    }

    #[test]
    fn pick_key_list_rejects_too_many_keys() {
        let keys = (0..=MAX_PICK_KEYS)
            .map(|idx| format!("memory_{idx}"))
            .collect::<Vec<_>>();
        let err = normalize_keys(keys).expect_err("should reject oversized key lists");
        assert!(err.to_string().contains("at most"), "{err}");
    }

    #[test]
    fn parser_errors_are_not_classified_as_pick_conflicts() {
        let parser_error = "SQL parser error: syntax error at line 1 column 16 near \" pick foo into bar keys('x') when conflict FAIL\";";
        assert!(is_pick_parser_error_message(parser_error));
        assert!(!is_pick_conflict_error_message(parser_error));
        let err = MemoriaError::Database(parser_error.to_string());
        let wrapped = map_pick_conflict(err.clone(), "src", "dst", Some(1)).unwrap_err();
        assert_eq!(wrapped.to_string(), err.to_string());
    }

    #[test]
    fn real_conflicts_are_still_wrapped_for_api_mapping() {
        let err = MemoriaError::Database("branch pick conflict: target row differs".into());
        let wrapped = map_pick_conflict(err, "src", "dst", Some(2)).unwrap_err();
        match wrapped {
            MemoriaError::Validation(msg) => {
                assert!(msg.starts_with("Conflict: pick from branch 'src' into 'dst' aborted"));
                assert!(msg.contains("2 selected change(s)"));
            }
            other => panic!("expected validation conflict, got {other:?}"),
        }
    }

    #[test]
    fn validate_identifier_accepts_safe_names() {
        assert_eq!(validate_identifier("br_valid_123").unwrap(), "br_valid_123");
    }

    #[test]
    fn validate_identifier_rejects_unsafe_names() {
        assert!(validate_identifier("br_bad-name").is_err());
        assert!(validate_identifier("br bad").is_err());
        assert!(validate_identifier("br`bad").is_err());
    }

    #[test]
    fn apply_arrays_default_to_empty_when_missing() {
        let args = json!({"source": "feature"});
        assert!(parse_apply_string_array(&args, "adds").unwrap().is_empty());
        assert!(parse_apply_string_array(&args, "removes")
            .unwrap()
            .is_empty());
        assert!(parse_apply_updates(&args).unwrap().is_empty());
    }

    #[test]
    fn apply_arrays_error_on_type_mismatch() {
        let args = json!({"source": "feature", "adds": "oops"});
        assert!(parse_apply_string_array(&args, "adds").is_err());
    }

    #[test]
    fn apply_updates_error_on_missing_fields() {
        let args = json!({"source": "feature", "updates": [{"old_id": "x"}]});
        assert!(parse_apply_updates(&args).is_err());
    }

    #[test]
    fn tool_args_error_on_unknown_field() {
        let args = json!({"source": "feature", "adds": [], "bogus": true});
        assert!(expect_tool_args(&args, "memory_apply", &["source", "adds"]).is_err());
    }
}
