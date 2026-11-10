//! Typed memory pipeline: Sensitivity → Sandbox → Persist.
//!
//! Simplified Rust version (no LLM-based observer — caller provides candidates).
//! Pipeline phases:
//!   Phase 1: Sensitivity filter — block HIGH, redact MEDIUM
//!   Phase 2: Sandbox validation (optional, requires git service)
//!   Phase 3: Persist validated memories

use std::sync::Arc;

use chrono::Utc;
use memoria_core::{Memory, MemoryType, TrustTier};
use memoria_git::GitForDataService;
use uuid::Uuid;

use crate::{content_filter::check_content_sensitivity, MemoryService};

#[derive(Debug, Default, serde::Serialize)]
pub struct PipelineResult {
    pub memories_stored: usize,
    pub memories_rejected: usize,
    pub memories_redacted: usize,
    pub errors: Vec<String>,
}

pub struct MemoryPipeline {
    service: Arc<MemoryService>,
    git: Option<Arc<GitForDataService>>,
    sandbox_enabled: bool,
}

impl MemoryPipeline {
    pub fn new(service: Arc<MemoryService>, git: Option<Arc<GitForDataService>>) -> Self {
        let sandbox_enabled = std::env::var("MEMORIA_SANDBOX_ENABLED")
            .map(|v| v.to_lowercase() == "true")
            .unwrap_or(false);
        Self {
            service,
            git,
            sandbox_enabled,
        }
    }

    /// Run pipeline for a list of (content, memory_type) candidates.
    pub async fn run(
        &self,
        user_id: &str,
        candidates: Vec<(String, MemoryType, Option<TrustTier>)>,
        sandbox_query: Option<&str>,
    ) -> PipelineResult {
        let mut result = PipelineResult::default();
        let mut validated: Vec<Memory> = Vec::new();

        // Phase 1: Sensitivity filter
        for (content, mtype, tier) in candidates {
            let sensitivity = check_content_sensitivity(user_id, &content);
            if sensitivity.blocked {
                result.memories_rejected += 1;
                result
                    .errors
                    .push(format!("blocked: {}", sensitivity.matched_labels.join(",")));
                continue;
            }
            let content = sensitivity.redacted_content.unwrap_or(content);
            if !sensitivity.matched_labels.is_empty() {
                result.memories_redacted += 1;
            }

            let embedding = match self.service.embed(&content).await {
                Ok(e) => e,
                Err(e) => {
                    result.errors.push(format!("embedding: {e}"));
                    continue;
                }
            };
            validated.push(Memory {
                memory_id: Uuid::now_v7().simple().to_string(),
                user_id: user_id.to_string(),
                memory_type: mtype,
                content,
                subject_id: None,
                initial_confidence: tier
                    .as_ref()
                    .map(|t| t.initial_confidence())
                    .unwrap_or(0.75),
                embedding,
                source_event_ids: vec![],
                superseded_by: None,
                is_active: true,
                access_count: 0,
                session_id: None,
                observed_at: Some(Utc::now()),
                created_at: None,
                updated_at: None,
                extra_metadata: None,
                trust_tier: tier.unwrap_or_default(),
                retrieval_score: None,
                author_id: None,
            });
        }

        // Phase 2: Sandbox validation (optional)
        if self.sandbox_enabled {
            if let (Some(git), Some(query)) = (&self.git, sandbox_query) {
                let passed = self
                    .service
                    .validate_in_sandbox(user_id, &validated, query, git)
                    .await;
                if !passed {
                    result.memories_rejected += validated.len();
                    return result;
                }
            }
        }

        // Phase 3: Persist
        if self.service.sql_store.is_some() {
            let sql = match self.service.user_sql_store(user_id).await {
                Ok(sql) => sql,
                Err(e) => {
                    result.errors.push(e.to_string());
                    return result;
                }
            };
            let table = match sql.active_table(user_id).await {
                Ok(t) => t,
                Err(e) => {
                    result.errors.push(e.to_string());
                    return result;
                }
            };
            for mem in &validated {
                match sql.insert_into(&table, mem).await {
                    Ok(_) => result.memories_stored += 1,
                    Err(e) => result.errors.push(e.to_string()),
                }
            }
        }

        result
    }
}
