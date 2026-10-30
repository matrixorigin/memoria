//! Request/Response types for the REST API.
//! Mirrors Python's api/models.py and api/_model_types.py.

use memoria_core::{Memory, MemoryType, TrustTier};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::str::FromStr;

// ── Memory ────────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct StoreRequest {
    /// Stable owner/branch-scoped write identity; retries return the same receipt.
    #[serde(default)]
    pub idempotency_key: Option<String>,
    pub content: String,
    #[serde(default = "default_memory_type")]
    pub memory_type: String,
    pub session_id: Option<String>,
    pub subject_id: Option<String>,
    pub trust_tier: Option<String>,
    pub initial_confidence: Option<f64>,
    pub observed_at: Option<String>,
    pub source: Option<String>,
    pub branch: Option<String>,
    /// 任意业务元数据（如 scene/agent）。透传落库到 memories.extra_metadata，并在读取时原样
    /// 返回给调用方；结构化 query 可做精确过滤，但不参与相关性检索或打分。
    #[serde(default)]
    pub extra_metadata: Option<std::collections::HashMap<String, serde_json::Value>>,
}
fn default_memory_type() -> String {
    "semantic".to_string()
}

#[derive(Deserialize)]
pub struct BatchStoreRequest {
    pub memories: Vec<StoreRequest>,
    /// Batch-level default subject_id; overridden by per-item subject_id.
    pub subject_id: Option<String>,
    pub branch: Option<String>,
}

#[derive(Deserialize)]
pub struct RetrieveRequest {
    pub query: String,
    #[serde(default = "default_top_k")]
    pub top_k: i64,
    pub session_id: Option<String>,
    pub session_scope: Option<String>,
    pub subject_id: Option<String>,
    pub memory_types: Option<Vec<String>>,
    pub branch: Option<String>,
    /// Explain level: false/"none" = off, true/"basic" = basic, "verbose" = per-candidate scores, "analyze" = full
    #[serde(default, deserialize_with = "deserialize_explain")]
    pub explain: String,
}
fn default_top_k() -> i64 {
    5
}
fn default_search_top_k() -> i64 {
    10
}
fn parse_session_scope(
    value: Option<&str>,
) -> Result<Option<memoria_service::SessionScope>, String> {
    value
        .map(memoria_service::SessionScope::from_str)
        .transpose()
}

fn parse_memory_types_opt(
    types: Option<&Vec<String>>,
) -> Result<Option<Vec<MemoryType>>, String> {
    let mut parsed = types
        .map(|ts| {
            ts.iter()
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(parse_memory_type)
                .collect::<Result<Vec<_>, _>>()
        })
        .transpose()
        .map(|v| v.filter(|t| !t.is_empty()))?;
    if let Some(types) = parsed.as_mut() {
        let mut seen = std::collections::HashSet::new();
        types.retain(|memory_type| seen.insert(memory_type.clone()));
    }
    Ok(parsed)
}

fn parse_fulltext_memory_types_opt(
    types: Option<&Vec<String>>,
) -> Result<Option<Vec<MemoryType>>, String> {
    if types.is_some_and(|types| types.iter().any(|value| value.trim().is_empty())) {
        return Err("memory_types entries must not be empty when provided".to_string());
    }
    parse_memory_types_opt(types)
}

impl RetrieveRequest {
    pub fn session_scope(&self) -> Result<Option<memoria_service::SessionScope>, String> {
        parse_session_scope(self.session_scope.as_deref())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.session_scope.is_some() && self.session_id.is_none() {
            return Err("session_id is required when session_scope is set".to_string());
        }
        Ok(())
    }

    pub fn retrieve_options(&self) -> Result<memoria_service::RetrieveOptions, String> {
        let memory_types = parse_memory_types_opt(self.memory_types.as_ref())?;
        Ok(
            memoria_service::RetrieveOptions::from_session_scope(
                self.session_id.as_deref(),
                self.session_scope()?,
            )
            .with_subject_id(self.subject_id.clone())
            .with_memory_types(memory_types),
        )
    }
}

#[derive(Deserialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default = "default_search_top_k")]
    pub top_k: i64,
    pub session_id: Option<String>,
    pub session_scope: Option<String>,
    pub subject_id: Option<String>,
    pub memory_types: Option<Vec<String>>,
    pub branch: Option<String>,
    #[serde(default, deserialize_with = "deserialize_explain")]
    pub explain: String,
}

impl SearchRequest {
    pub fn session_scope(&self) -> Result<Option<memoria_service::SessionScope>, String> {
        parse_session_scope(self.session_scope.as_deref())
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.session_scope.is_some() && self.session_id.is_none() {
            return Err("session_id is required when session_scope is set".to_string());
        }
        Ok(())
    }

    pub fn retrieve_options(&self) -> Result<memoria_service::RetrieveOptions, String> {
        let memory_types = parse_memory_types_opt(self.memory_types.as_ref())?;
        Ok(
            memoria_service::RetrieveOptions::from_session_scope(
                self.session_id.as_deref(),
                self.session_scope()?,
            )
            .with_subject_id(self.subject_id.clone())
            .with_memory_types(memory_types),
        )
    }
}

fn default_structured_query_limit() -> i64 {
    100
}

/// A pure structured query. All supplied selectors are combined with AND.
/// Metadata equality preserves JSON type families (for example, string `"2"`
/// does not equal number `2`); JSON numbers `2` and `2.0` may compare equal.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredQueryRequest {
    #[serde(default)]
    pub extra_metadata_filter: HashMap<String, serde_json::Value>,
    pub subject_id: Option<String>,
    pub memory_types: Option<Vec<String>>,
    pub session_id: Option<String>,
    pub trust_tier: Option<String>,
    pub branch: Option<String>,
    #[serde(default = "default_structured_query_limit")]
    pub limit: i64,
    pub cursor: Option<String>,
}

impl StructuredQueryRequest {
    pub fn structured_options(&self) -> Result<memoria_service::StructuredQueryOptions, String> {
        if !(1..=500).contains(&self.limit) {
            return Err("limit must be between 1 and 500".to_string());
        }
        memoria_storage::validate_extra_metadata_filter(&self.extra_metadata_filter)
            .map_err(|err| err.to_string())?;

        let subject_id = normalized_filter("subject_id", self.subject_id.as_deref())?;
        let session_id = normalized_filter("session_id", self.session_id.as_deref())?;
        let normalized_trust_tier =
            normalized_filter("trust_tier", self.trust_tier.as_deref())?;
        let branch = normalized_filter("branch", self.branch.as_deref())?;
        let trust_tier = normalized_trust_tier
            .as_deref()
            .map(parse_trust_tier)
            .transpose()?;
        if let Some(memory_types) = self.memory_types.as_ref() {
            if memory_types.is_empty() || memory_types.iter().any(|value| value.trim().is_empty()) {
                return Err(
                    "memory_types must contain only non-empty values when provided".to_string(),
                );
            }
        }
        let memory_types = parse_memory_types_opt(self.memory_types.as_ref())?;
        if self.extra_metadata_filter.is_empty()
            && subject_id.is_none()
            && session_id.is_none()
            && trust_tier.is_none()
            && memory_types.is_none()
            && branch.is_none()
        {
            return Err("structured query requires at least one filter selector".to_string());
        }

        // An explicitly blank cursor is invalid, not a request for page one.
        let cursor = self.cursor.as_deref().map(str::trim).map(str::to_string);
        if let Some(cursor) = cursor.as_deref() {
            if cursor.len() != 32 || !cursor.chars().all(|ch| ch.is_ascii_hexdigit()) {
                return Err("cursor must be a 32-character hexadecimal memory_id".to_string());
            }
        }

        Ok(memoria_service::StructuredQueryOptions {
            limit: self.limit,
            memory_types,
            session_id,
            trust_tier,
            cursor,
            subject_id,
            extra_metadata_filter: self.extra_metadata_filter.clone(),
        })
    }
}

fn default_fulltext_search_limit() -> i64 {
    memoria_storage::FULLTEXT_SEARCH_DEFAULT_LIMIT
}

/// Pure MatrixOne full-text search with exact structured SQL pre-filters.
/// Session filtering is strict and does not include unscoped memories.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FulltextSearchRequest {
    pub query: String,
    #[serde(default)]
    pub extra_metadata_filter: HashMap<String, serde_json::Value>,
    pub subject_id: Option<String>,
    pub memory_types: Option<Vec<String>>,
    pub session_id: Option<String>,
    pub trust_tier: Option<String>,
    pub branch: Option<String>,
    #[serde(default = "default_fulltext_search_limit")]
    pub limit: i64,
}

impl FulltextSearchRequest {
    pub fn fulltext_options(&self) -> Result<memoria_service::FulltextSearchOptions, String> {
        if !(1..=memoria_storage::FULLTEXT_SEARCH_MAX_LIMIT).contains(&self.limit) {
            return Err(format!(
                "limit must be between 1 and {}",
                memoria_storage::FULLTEXT_SEARCH_MAX_LIMIT
            ));
        }
        memoria_storage::validate_fulltext_query(&self.query).map_err(|error| error.to_string())?;
        memoria_storage::validate_extra_metadata_filter(&self.extra_metadata_filter)
            .map_err(|error| error.to_string())?;

        let session_id = normalized_filter("session_id", self.session_id.as_deref())?;
        let subject_id = normalized_filter("subject_id", self.subject_id.as_deref())?;
        let trust_tier = normalized_filter("trust_tier", self.trust_tier.as_deref())?
            .as_deref()
            .map(parse_trust_tier)
            .transpose()?;
        Ok(memoria_service::FulltextSearchOptions {
            limit: self.limit,
            memory_types: parse_fulltext_memory_types_opt(self.memory_types.as_ref())?,
            session_id,
            trust_tier,
            subject_id,
            extra_metadata_filter: self.extra_metadata_filter.clone(),
        })
    }

    pub fn fulltext_branch(&self) -> Result<Option<String>, String> {
        normalized_filter("branch", self.branch.as_deref())
    }
}

fn normalized_filter(name: &str, value: Option<&str>) -> Result<Option<String>, String> {
    value
        .map(|value| {
            let value = value.trim();
            if value.is_empty() {
                Err(format!("{name} must not be empty when provided"))
            } else {
                Ok(value.to_string())
            }
        })
        .transpose()
}

fn deserialize_explain<'de, D: serde::Deserializer<'de>>(d: D) -> Result<String, D::Error> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum ExplainInput {
        Bool(bool),
        Str(String),
    }
    Ok(match ExplainInput::deserialize(d)? {
        ExplainInput::Bool(true) => "basic".to_string(),
        ExplainInput::Bool(false) => "none".to_string(),
        ExplainInput::Str(s) => s,
    })
}

#[derive(Deserialize)]
pub struct CorrectRequest {
    pub new_content: String,
    pub reason: Option<String>,
    pub branch: Option<String>,
}

#[derive(Deserialize)]
pub struct CorrectByQueryRequest {
    pub query: String,
    pub new_content: String,
    pub session_id: Option<String>,
    pub session_scope: Option<String>,
    pub subject_id: Option<String>,
    pub memory_types: Option<Vec<String>>,
    pub reason: Option<String>,
    pub branch: Option<String>,
}

impl CorrectByQueryRequest {
    pub fn validate(&self) -> Result<(), String> {
        if self.session_scope.is_some() && self.session_id.is_none() {
            return Err("session_id is required when session_scope is set".to_string());
        }
        Ok(())
    }

    pub fn retrieve_options(&self) -> Result<memoria_service::RetrieveOptions, String> {
        let memory_types = parse_memory_types_opt(self.memory_types.as_ref())?;
        Ok(
            memoria_service::RetrieveOptions::from_session_scope(
                self.session_id.as_deref(),
                parse_session_scope(self.session_scope.as_deref())?,
            )
            .with_subject_id(self.subject_id.clone())
            .with_memory_types(memory_types),
        )
    }
}

#[derive(Deserialize)]
pub struct PurgeRequest {
    pub memory_ids: Option<Vec<String>>,
    pub topic: Option<String>,
    pub session_id: Option<String>,
    pub memory_types: Option<Vec<String>>,
    pub reason: Option<String>,
    pub branch: Option<String>,
}

pub enum PurgeSelector {
    MemoryIds(Vec<String>),
    Topic(String),
    Session {
        session_id: String,
        memory_types: Option<Vec<MemoryType>>,
    },
    None,
}

impl PurgeRequest {
    pub fn selector(&self) -> Result<PurgeSelector, String> {
        let memory_ids = self.memory_ids.as_ref().filter(|ids| !ids.is_empty());
        let memory_types = self.memory_types.as_ref().filter(|types| !types.is_empty());
        let topic = self
            .topic
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let session_id = self
            .session_id
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if memory_types.is_some() && session_id.is_none() {
            return Err("memory_types requires session_id".to_string());
        }

        let selector_count = usize::from(memory_ids.is_some())
            + usize::from(topic.is_some())
            + usize::from(session_id.is_some());
        if selector_count > 1 {
            return Err("provide only one of memory_ids, topic, or session_id".to_string());
        }

        if let Some(ids) = memory_ids {
            return Ok(PurgeSelector::MemoryIds(ids.clone()));
        }
        if let Some(topic) = topic {
            return Ok(PurgeSelector::Topic(topic.to_string()));
        }
        if let Some(session_id) = session_id {
            let memory_types = memory_types
                .map(|types| {
                    types
                        .iter()
                        .map(|memory_type| parse_memory_type(memory_type))
                        .collect::<Result<Vec<_>, _>>()
                })
                .transpose()?
                .filter(|types| !types.is_empty());
            return Ok(PurgeSelector::Session {
                session_id: session_id.to_string(),
                memory_types,
            });
        }
        Ok(PurgeSelector::None)
    }
}

#[derive(Serialize)]
pub struct MemoryResponse {
    pub memory_id: String,
    pub user_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub author_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject_id: Option<String>,
    pub memory_type: String,
    pub content: String,
    pub trust_tier: String,
    pub initial_confidence: f64,
    pub is_active: bool,
    pub session_id: Option<String>,
    pub observed_at: Option<String>,
    pub created_at: Option<String>,
    pub retrieval_score: Option<f64>,
    /// 业务元数据（如 scene/agent）从 memories.extra_metadata 原样透传回给调用方；结构化 query
    /// 可做精确过滤，但不参与相关性检索或打分。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_metadata: Option<std::collections::HashMap<String, serde_json::Value>>,
}

impl From<Memory> for MemoryResponse {
    fn from(mut m: Memory) -> Self {
        if let Some(metadata) = m.extra_metadata.as_mut() {
            metadata.remove(memoria_core::STORE_REQUEST_HASH_FIELD);
        }
        Self {
            memory_id: m.memory_id,
            user_id: m.user_id,
            author_id: m.author_id,
            subject_id: m.subject_id,
            memory_type: m.memory_type.to_string(),
            content: m.content,
            trust_tier: m.trust_tier.to_string(),
            initial_confidence: m.initial_confidence,
            is_active: m.is_active,
            session_id: m.session_id,
            observed_at: m.observed_at.map(|dt| dt.to_rfc3339()),
            created_at: m.created_at.map(|dt| dt.to_rfc3339()),
            retrieval_score: m.retrieval_score,
            // 空 map 在响应里归一为 None（省略），与读取侧「"{}" → None」一致：新写记录直接
            // 返回内存对象时也不会出现「POST 带 {} 而后续 list/get 无该字段」的不一致。
            extra_metadata: m.extra_metadata.filter(|md| !md.is_empty()),
        }
    }
}

#[derive(Serialize)]
pub struct ListResponse {
    pub items: Vec<MemoryResponse>,
    pub next_cursor: Option<String>,
}

#[derive(Serialize)]
pub struct PurgeResponse {
    pub purged: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
}

// ── Governance ────────────────────────────────────────────────────────────────

#[derive(Deserialize, Default)]
pub struct GovernanceRequest {
    #[serde(default)]
    pub force: bool,
}

#[derive(Deserialize, Default)]
pub struct ReflectRequest {
    #[serde(default)]
    pub force: bool,
    #[serde(default = "default_mode")]
    pub mode: String,
}
fn default_mode() -> String {
    "auto".to_string()
}

#[derive(Deserialize, Default)]
pub struct ExtractEntitiesRequest {
    #[serde(default = "default_mode")]
    pub mode: String,
}

#[derive(Deserialize)]
pub struct LinkEntitiesRequest {
    pub entities: Vec<EntityLink>,
}

#[derive(Deserialize)]
pub struct EntityLink {
    pub memory_id: String,
    pub entities: Vec<EntityItem>,
}

#[derive(Deserialize)]
pub struct EntityItem {
    pub name: String,
    #[serde(rename = "type", default = "default_entity_type")]
    pub entity_type: String,
}
fn default_entity_type() -> String {
    "concept".to_string()
}

// ── Snapshots ─────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateSnapshotRequest {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Deserialize)]
pub struct DeleteSnapshotsRequest {
    pub names: Option<Vec<String>>,
    pub prefix: Option<String>,
    pub older_than: Option<String>,
}

// ── Branches ──────────────────────────────────────────────────────────────────

#[derive(Deserialize)]
pub struct CreateBranchRequest {
    pub name: String,
    pub from_snapshot: Option<String>,
    pub from_timestamp: Option<String>,
}

#[derive(Deserialize)]
pub struct MergeRequest {
    #[serde(default = "default_strategy")]
    pub strategy: String,
}
fn default_strategy() -> String {
    "accept".to_string()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PickRequest {
    /// Target branch for the selected changes. Defaults to main.
    #[serde(default = "default_pick_target")]
    pub target: String,
    /// Conflict strategy for selected changes: fail | skip | accept. Defaults to fail.
    #[serde(default = "default_pick_strategy")]
    pub strategy: String,
    /// Selector that decides which branch changes are eligible to apply.
    pub selector: PickSelector,
    /// Optional dry-run preview settings. When present, returns a preview instead of mutating state.
    pub dry_run: Option<PickDryRunOptions>,
}

fn default_pick_target() -> String {
    "main".to_string()
}

fn default_pick_strategy() -> String {
    "fail".to_string()
}

fn default_pick_top_k() -> i64 {
    5
}

fn default_pick_preview_limit() -> i64 {
    10
}

fn default_pick_include_content_preview() -> bool {
    true
}

fn default_pick_include_scores() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize, Clone)]
pub struct PickDryRunOptions {
    /// Maximum number of preview candidates to return.
    #[serde(default = "default_pick_preview_limit")]
    pub limit: i64,
    /// Preview pagination offset.
    #[serde(default)]
    pub offset: i64,
    /// Include a short content_preview for each candidate. Embeddings are never returned.
    #[serde(default = "default_pick_include_content_preview")]
    pub include_content_preview: bool,
    /// Include retrieve scores when available. Non-retrieve selectors omit scores.
    #[serde(default = "default_pick_include_scores")]
    pub include_scores: bool,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PickSelector {
    KeyList {
        /// Explicit memory_ids from the source branch to apply.
        keys: Vec<String>,
    },
    SnapshotRange {
        /// Start snapshot name in the source branch history.
        from_snapshot: String,
        /// End snapshot name in the source branch history.
        to_snapshot: String,
    },
    Retrieve {
        /// Natural-language query used to rank changed source rows.
        query: String,
        /// Maximum number of ranked rows eligible for application.
        #[serde(default = "default_pick_top_k")]
        top_k: i64,
        /// Optional retrieve threshold. Rows below this score are excluded.
        min_score: Option<f64>,
    },
}

// ── Helpers ───────────────────────────────────────────────────────────────────

pub fn parse_memory_type(s: &str) -> Result<MemoryType, String> {
    MemoryType::from_str(s).map_err(|e| e.to_string())
}

pub fn parse_trust_tier(s: &str) -> Result<TrustTier, String> {
    TrustTier::from_str(s).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::{
        parse_fulltext_memory_types_opt, parse_memory_types_opt, PurgeRequest, PurgeSelector,
        StructuredQueryRequest,
    };
    use memoria_core::MemoryType;

    #[test]
    fn memory_type_parser_deduplicates_before_sql_option_construction() {
        let raw = vec![
            "semantic".to_string(),
            " semantic ".to_string(),
            "profile".to_string(),
            "semantic".to_string(),
        ];

        assert_eq!(
            parse_memory_types_opt(Some(&raw)).unwrap(),
            Some(vec![MemoryType::Semantic, MemoryType::Profile])
        );
    }
    #[test]
    fn fulltext_memory_type_parser_rejects_blank_entries_but_allows_empty_arrays() {
        let blank = vec!["semantic".to_string(), "   ".to_string()];
        assert!(parse_fulltext_memory_types_opt(Some(&blank)).is_err());

        let empty = vec![];
        assert_eq!(parse_fulltext_memory_types_opt(Some(&empty)).unwrap(), None);
    }

    #[test]
    fn structured_query_rejects_blank_cursors() {
        for cursor in [
            "",
            " ",
            "\t\n",
            "\u{2003}",
            "not-a-cursor",
            "g0123456789abcdefABCDEF012345678",
        ] {
            let request: StructuredQueryRequest = serde_json::from_value(serde_json::json!({
                "subject_id": "subject-1",
                "cursor": cursor
            }))
            .unwrap();
            assert!(
                request.structured_options().is_err(),
                "invalid cursor {cursor:?} must not restart pagination"
            );
        }
    }

    #[test]
    fn structured_query_keeps_optional_and_valid_cursors() {
        for cursor in [
            None,
            Some("0123456789abcdefABCDEF0123456789"),
            Some("  0123456789abcdefABCDEF0123456789  "),
        ] {
            let request: StructuredQueryRequest = serde_json::from_value(serde_json::json!({
                "subject_id": "subject-1",
                "cursor": cursor
            }))
            .unwrap();
            let options = request.structured_options().unwrap();
            assert_eq!(options.cursor.as_deref(), cursor.map(str::trim));
        }
    }

    #[test]
    fn purge_selector_ignores_empty_arrays() {
        let request = PurgeRequest {
            memory_ids: Some(vec![]),
            topic: None,
            session_id: Some("sess-1".to_string()),
            memory_types: Some(vec![]),
            reason: None,
            branch: None,
        };

        match request.selector().unwrap() {
            PurgeSelector::Session {
                session_id,
                memory_types,
            } => {
                assert_eq!(session_id, "sess-1");
                assert!(memory_types.is_none());
            }
            _ => panic!("expected session selector"),
        }
    }

    #[test]
    fn purge_selector_empty_memory_types_do_not_require_session() {
        let request = PurgeRequest {
            memory_ids: None,
            topic: None,
            session_id: None,
            memory_types: Some(vec![]),
            reason: None,
            branch: None,
        };

        assert!(matches!(request.selector().unwrap(), PurgeSelector::None));
    }
}
