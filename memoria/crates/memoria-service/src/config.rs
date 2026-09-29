//! Unified configuration for Memoria MCP server.
//! All settings read from environment variables (matching Python's config.py).
//!
//! Environment variables (no prefix, matching README):
//!   DATABASE_URL              — full MySQL URL
//!   EMBEDDING_PROVIDER        — "openai" | "local" | "mock" (default: "mock")
//!   EMBEDDING_MODEL           — e.g. "BAAI/bge-m3"
//!   EMBEDDING_DIM             — integer, e.g. 1024
//!   EMBEDDING_API_KEY         — API key for embedding service (single-backend)
//!   EMBEDDING_BASE_URL        — base URL for embedding service (single-backend)
//!   EMBEDDING_ENDPOINTS       — JSON array for multi-backend round-robin, e.g.
//!                               `[{"url":"https://api1.example.com/v1","api_key":"sk-1"},
//!                                 {"url":"https://api2.example.com/v1","api_key":"sk-2"}]`
//!                               When set, supersedes EMBEDDING_BASE_URL/EMBEDDING_API_KEY.
//!                               All entries must serve the same EMBEDDING_MODEL.
//!   LLM_API_KEY               — OpenAI-compatible API key (optional)
//!   LLM_BASE_URL              — LLM base URL (default: https://api.openai.com/v1)
//!   LLM_MODEL                 — LLM model name (default: gpt-4o-mini)
//!   MEMORIA_USER              — default user ID (default: "default")
//!   MEMORIA_DB_NAME           — database name for git-for-data (default: "memoria")
//!   MEMORIA_GOVERNANCE_PLUGIN_BINDING — shared governance plugin binding (default: "default")
//!   MEMORIA_GOVERNANCE_PLUGIN_SUBJECT — deterministic subject key for shared binding selection

/// A single embedding backend endpoint used for multi-backend round-robin.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct EmbeddingEndpoint {
    pub url: String,
    pub api_key: String,
}

#[derive(Debug, Clone)]
pub struct Config {
    // Database
    pub db_url: String,
    pub db_name: String,
    pub shared_db_url: String,
    pub multi_db: bool,

    // Embedding
    pub embedding_provider: String,
    pub embedding_model: String,
    pub embedding_dim: usize,
    /// Single-backend API key — used when `embedding_endpoints` is empty.
    pub embedding_api_key: String,
    /// Single-backend base URL — used when `embedding_endpoints` is empty.
    pub embedding_base_url: String,
    /// Multi-backend round-robin endpoints (parsed from `EMBEDDING_ENDPOINTS`).
    /// When non-empty, supersedes `embedding_base_url` / `embedding_api_key`.
    pub embedding_endpoints: Vec<EmbeddingEndpoint>,

    // LLM (optional)
    pub llm_api_key: Option<String>,
    pub llm_base_url: String,
    pub llm_model: String,

    // Server
    pub user: String,

    // Governance plugin runtime
    pub governance_plugin_binding: String,
    pub governance_plugin_subject: String,
    /// Local plugin directory for dev hot-reload (skips publish/review).
    /// Set via MEMORIA_GOVERNANCE_PLUGIN_DIR.
    pub governance_plugin_dir: Option<String>,

    // Distributed coordination
    /// Unique instance identifier. Set via MEMORIA_INSTANCE_ID, defaults to random UUID.
    pub instance_id: String,
    /// Lock TTL in seconds for distributed leader election. Default: 120.
    pub lock_ttl_secs: u64,

    /// Enable writing operational metrics to the shared DB for admin dashboard.
    /// Only aggregate counters are written — no user memory content is stored.
    /// Env: MEMORIA_OPS_METRICS (default: true).
    pub ops_metrics_enabled: bool,
}

impl Config {
    /// Load from environment variables with sensible defaults.
    pub fn from_env() -> Self {
        let db_url = std::env::var("DATABASE_URL")
            .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string());
        let multi_db = env_bool("MEMORIA_MULTI_DB");
        let shared_db_url = std::env::var("MEMORIA_SHARED_DATABASE_URL")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| {
                replace_db_name(&db_url, "memoria_shared").unwrap_or_else(|| db_url.clone())
            });

        // Extract db_name from URL (last path segment) or from MEMORIA_DB_NAME
        let db_name = std::env::var("MEMORIA_DB_NAME")
            .unwrap_or_else(|_| db_url.rsplit('/').next().unwrap_or("memoria").to_string());

        let embedding_dim = std::env::var("EMBEDDING_DIM")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0usize); // 0 = auto-infer from embedding service at startup

        let llm_api_key = std::env::var("LLM_API_KEY").ok().filter(|s| !s.is_empty());

        Self {
            db_url,
            db_name,
            shared_db_url,
            multi_db,
            embedding_provider: std::env::var("EMBEDDING_PROVIDER")
                .unwrap_or_else(|_| "mock".to_string()),
            embedding_model: std::env::var("EMBEDDING_MODEL")
                .unwrap_or_else(|_| "BAAI/bge-m3".to_string()),
            embedding_dim,
            embedding_api_key: std::env::var("EMBEDDING_API_KEY").unwrap_or_default(),
            embedding_base_url: std::env::var("EMBEDDING_BASE_URL").unwrap_or_default(),
            embedding_endpoints: std::env::var("EMBEDDING_ENDPOINTS")
                .ok()
                .filter(|s| !s.is_empty())
                .and_then(|s| {
                    serde_json::from_str::<Vec<EmbeddingEndpoint>>(&s)
                        .map_err(|e| {
                            tracing::warn!(
                                error = %e,
                                raw = %s,
                                "EMBEDDING_ENDPOINTS JSON is malformed — ignoring; \
                                 no HTTP embedder will be initialised"
                            );
                        })
                        .ok()
                })
                .unwrap_or_default(),
            llm_api_key,
            llm_base_url: std::env::var("LLM_BASE_URL")
                .unwrap_or_else(|_| "https://api.openai.com/v1".to_string()),
            llm_model: std::env::var("LLM_MODEL").unwrap_or_else(|_| "gpt-4o-mini".to_string()),
            user: std::env::var("MEMORIA_USER").unwrap_or_else(|_| "default".to_string()),
            governance_plugin_binding: std::env::var("MEMORIA_GOVERNANCE_PLUGIN_BINDING")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "default".to_string()),
            governance_plugin_subject: std::env::var("MEMORIA_GOVERNANCE_PLUGIN_SUBJECT")
                .ok()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| "system".to_string()),
            governance_plugin_dir: std::env::var("MEMORIA_GOVERNANCE_PLUGIN_DIR")
                .ok()
                .filter(|s| !s.trim().is_empty()),
            instance_id: {
                let base = std::env::var("MEMORIA_INSTANCE_ID")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .unwrap_or_else(|| uuid::Uuid::new_v4().simple().to_string());
                // Append PID so two processes with the same MEMORIA_INSTANCE_ID
                // still get distinct holder IDs and cannot bypass the distributed lock.
                format!("{}-{}", base, std::process::id())
            },
            lock_ttl_secs: std::env::var("MEMORIA_LOCK_TTL_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(120),
            ops_metrics_enabled: env_bool_default("MEMORIA_OPS_METRICS", true),
        }
    }

    pub fn effective_sql_url(&self) -> &str {
        if self.multi_db {
            &self.shared_db_url
        } else {
            &self.db_url
        }
    }

    /// Returns true if LLM is configured.
    pub fn has_llm(&self) -> bool {
        self.llm_api_key.is_some()
    }

    /// Returns true if embedding is configured (non-mock provider with base URL, or local).
    pub fn has_embedding(&self) -> bool {
        if self.embedding_provider == "local" {
            return false; // local is handled separately, not via HttpEmbedder
        }
        if self.embedding_provider == "mock" {
            return false;
        }
        !self.embedding_endpoints.is_empty() || !self.embedding_base_url.is_empty()
    }

    /// Returns the effective list of HTTP embedding endpoints.
    ///
    /// `EMBEDDING_ENDPOINTS` (multi-backend) takes precedence over the single
    /// `EMBEDDING_BASE_URL` / `EMBEDDING_API_KEY` pair for backward compatibility.
    pub fn resolved_embedding_endpoints(&self) -> Vec<EmbeddingEndpoint> {
        if !self.embedding_endpoints.is_empty() {
            return self.embedding_endpoints.clone();
        }
        if !self.embedding_base_url.is_empty() {
            return vec![EmbeddingEndpoint {
                url: self.embedding_base_url.clone(),
                api_key: self.embedding_api_key.clone(),
            }];
        }
        vec![]
    }

    /// Returns true if a governance plugin binding is configured.
    pub fn has_governance_plugin(&self) -> bool {
        !self.governance_plugin_binding.trim().is_empty()
    }
}

fn env_bool(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .ok()
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

fn env_bool_default(name: &str, default: bool) -> bool {
    match std::env::var(name)
        .ok()
        .as_deref()
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("1" | "true" | "yes" | "on") => true,
        Some("0" | "false" | "no" | "off") => false,
        _ => default,
    }
}

fn replace_db_name(database_url: &str, db_name: &str) -> Option<String> {
    let (base, _, suffix) = split_database_url(database_url)?;
    Some(format!("{base}/{db_name}{suffix}"))
}

fn split_database_url(database_url: &str) -> Option<(&str, &str, &str)> {
    let suffix_start = database_url.find(['?', '#']).unwrap_or(database_url.len());
    let (without_suffix, suffix) = database_url.split_at(suffix_start);
    let (base, db_name) = without_suffix.rsplit_once('/')?;
    if db_name.is_empty() {
        return None;
    }
    Some((base, db_name, suffix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn config_reads_governance_plugin_binding() {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("env test lock should not be poisoned");
        let old_binding = std::env::var_os("MEMORIA_GOVERNANCE_PLUGIN_BINDING");
        let old_subject = std::env::var_os("MEMORIA_GOVERNANCE_PLUGIN_SUBJECT");
        std::env::set_var("MEMORIA_GOVERNANCE_PLUGIN_BINDING", "governance/default");
        std::env::set_var("MEMORIA_GOVERNANCE_PLUGIN_SUBJECT", "tenant-a");

        let cfg = Config::from_env();
        assert_eq!(cfg.governance_plugin_binding, "governance/default");
        assert_eq!(cfg.governance_plugin_subject, "tenant-a");
        assert!(cfg.has_governance_plugin());

        match old_binding {
            Some(value) => std::env::set_var("MEMORIA_GOVERNANCE_PLUGIN_BINDING", value),
            None => std::env::remove_var("MEMORIA_GOVERNANCE_PLUGIN_BINDING"),
        }
        match old_subject {
            Some(value) => std::env::set_var("MEMORIA_GOVERNANCE_PLUGIN_SUBJECT", value),
            None => std::env::remove_var("MEMORIA_GOVERNANCE_PLUGIN_SUBJECT"),
        }
    }

    // ── EmbeddingEndpoint / resolved_embedding_endpoints tests ───────────────

    fn with_env<F: FnOnce()>(vars: &[(&str, Option<&str>)], f: F) {
        let _lock = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner()); // recover from a previously poisoned lock

        // RAII guard: restores env vars in Drop, even if the closure panics.
        struct EnvGuard(Vec<(String, Option<std::ffi::OsString>)>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                for (k, old) in &self.0 {
                    match old {
                        Some(value) => std::env::set_var(k, value),
                        None => std::env::remove_var(k),
                    }
                }
            }
        }

        let _restore = EnvGuard(
            vars.iter()
                .map(|(k, v)| {
                    let old = std::env::var_os(k);
                    match v {
                        Some(val) => std::env::set_var(k, val),
                        None => std::env::remove_var(k),
                    }
                    (k.to_string(), old)
                })
                .collect(),
        );

        f();
    }

    #[test]
    fn embedding_endpoint_json_roundtrips() {
        let ep = EmbeddingEndpoint {
            url: "https://api.example.com/v1".into(),
            api_key: "sk-test".into(),
        };
        let json = serde_json::to_string(&ep).unwrap();
        let decoded: EmbeddingEndpoint = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.url, ep.url);
        assert_eq!(decoded.api_key, ep.api_key);
    }

    #[test]
    fn resolved_endpoints_parses_multi_json() {
        with_env(
            &[
                ("EMBEDDING_PROVIDER", Some("openai")),
                (
                    "EMBEDDING_ENDPOINTS",
                    Some(
                        r#"[{"url":"https://a.com/v1","api_key":"key-a"},{"url":"https://b.com/v1","api_key":"key-b"}]"#,
                    ),
                ),
                ("EMBEDDING_BASE_URL", None),
                ("EMBEDDING_API_KEY", None),
            ],
            || {
                let cfg = Config::from_env();
                let eps = cfg.resolved_embedding_endpoints();
                assert_eq!(eps.len(), 2);
                assert_eq!(eps[0].url, "https://a.com/v1");
                assert_eq!(eps[0].api_key, "key-a");
                assert_eq!(eps[1].url, "https://b.com/v1");
                assert_eq!(eps[1].api_key, "key-b");
            },
        );
    }

    #[test]
    fn resolved_endpoints_multi_supersedes_single() {
        with_env(
            &[
                ("EMBEDDING_PROVIDER", Some("openai")),
                (
                    "EMBEDDING_ENDPOINTS",
                    Some(r#"[{"url":"https://multi.com/v1","api_key":"mk"}]"#),
                ),
                ("EMBEDDING_BASE_URL", Some("https://single.com/v1")),
                ("EMBEDDING_API_KEY", Some("sk-single")),
            ],
            || {
                let cfg = Config::from_env();
                let eps = cfg.resolved_embedding_endpoints();
                // EMBEDDING_ENDPOINTS wins
                assert_eq!(eps.len(), 1);
                assert_eq!(eps[0].url, "https://multi.com/v1");
            },
        );
    }

    #[test]
    fn resolved_endpoints_falls_back_to_single_base_url() {
        with_env(
            &[
                ("EMBEDDING_PROVIDER", Some("openai")),
                ("EMBEDDING_ENDPOINTS", None),
                ("EMBEDDING_BASE_URL", Some("https://single.com/v1")),
                ("EMBEDDING_API_KEY", Some("sk-fallback")),
            ],
            || {
                let cfg = Config::from_env();
                let eps = cfg.resolved_embedding_endpoints();
                assert_eq!(eps.len(), 1);
                assert_eq!(eps[0].url, "https://single.com/v1");
                assert_eq!(eps[0].api_key, "sk-fallback");
            },
        );
    }

    #[test]
    fn resolved_endpoints_empty_when_nothing_configured() {
        with_env(
            &[
                ("EMBEDDING_PROVIDER", Some("openai")),
                ("EMBEDDING_ENDPOINTS", None),
                ("EMBEDDING_BASE_URL", None),
                ("EMBEDDING_API_KEY", None),
            ],
            || {
                let cfg = Config::from_env();
                assert!(cfg.resolved_embedding_endpoints().is_empty());
            },
        );
    }

    #[test]
    fn has_embedding_true_with_multi_endpoints() {
        with_env(
            &[
                ("EMBEDDING_PROVIDER", Some("openai")),
                (
                    "EMBEDDING_ENDPOINTS",
                    Some(r#"[{"url":"https://a.com/v1","api_key":"k"}]"#),
                ),
                ("EMBEDDING_BASE_URL", None),
            ],
            || {
                let cfg = Config::from_env();
                assert!(cfg.has_embedding());
            },
        );
    }

    #[test]
    fn has_embedding_true_with_base_url_only() {
        with_env(
            &[
                ("EMBEDDING_PROVIDER", Some("openai")),
                ("EMBEDDING_ENDPOINTS", None),
                ("EMBEDDING_BASE_URL", Some("https://single.com/v1")),
            ],
            || {
                let cfg = Config::from_env();
                assert!(cfg.has_embedding());
            },
        );
    }

    #[test]
    fn has_embedding_false_for_mock_provider() {
        with_env(
            &[
                ("EMBEDDING_PROVIDER", Some("mock")),
                (
                    "EMBEDDING_ENDPOINTS",
                    Some(r#"[{"url":"https://a.com/v1","api_key":"k"}]"#),
                ),
                ("EMBEDDING_BASE_URL", Some("https://single.com/v1")),
            ],
            || {
                let cfg = Config::from_env();
                // mock provider always returns false regardless of endpoints
                assert!(!cfg.has_embedding());
            },
        );
    }

    #[test]
    fn has_embedding_false_for_local_provider() {
        with_env(
            &[
                ("EMBEDDING_PROVIDER", Some("local")),
                (
                    "EMBEDDING_ENDPOINTS",
                    Some(r#"[{"url":"https://a.com/v1","api_key":"k"}]"#),
                ),
            ],
            || {
                let cfg = Config::from_env();
                // local provider is handled separately, not via HttpEmbedder
                assert!(!cfg.has_embedding());
            },
        );
    }

    #[test]
    fn invalid_json_in_endpoints_falls_back_to_empty() {
        with_env(
            &[
                ("EMBEDDING_PROVIDER", Some("openai")),
                ("EMBEDDING_ENDPOINTS", Some("not-valid-json")),
                ("EMBEDDING_BASE_URL", None),
            ],
            || {
                let cfg = Config::from_env();
                // Malformed JSON → silently ignored → empty endpoints
                assert!(cfg.embedding_endpoints.is_empty());
            },
        );
    }

    // ── env_bool_default / ops_metrics_enabled tests ─────────────────────────

    #[test]
    fn ops_metrics_defaults_to_true_when_unset() {
        with_env(&[("MEMORIA_OPS_METRICS", None)], || {
            let cfg = Config::from_env();
            assert!(cfg.ops_metrics_enabled, "should be enabled by default");
        });
    }

    #[test]
    fn ops_metrics_recognised_truthy_values() {
        for val in &["1", "true", "True", "TRUE", "yes", "YES", "on", "ON"] {
            with_env(&[("MEMORIA_OPS_METRICS", Some(val))], || {
                let cfg = Config::from_env();
                assert!(
                    cfg.ops_metrics_enabled,
                    "MEMORIA_OPS_METRICS={val:?} should be truthy"
                );
            });
        }
    }

    #[test]
    fn ops_metrics_recognised_falsy_values() {
        for val in &["0", "false", "False", "FALSE", "no", "NO", "off", "OFF"] {
            with_env(&[("MEMORIA_OPS_METRICS", Some(val))], || {
                let cfg = Config::from_env();
                assert!(
                    !cfg.ops_metrics_enabled,
                    "MEMORIA_OPS_METRICS={val:?} should be falsy"
                );
            });
        }
    }

    #[test]
    fn ops_metrics_unknown_value_falls_back_to_default() {
        with_env(&[("MEMORIA_OPS_METRICS", Some("maybe"))], || {
            let cfg = Config::from_env();
            assert!(
                cfg.ops_metrics_enabled,
                "unrecognised value should fall back to the default (true)"
            );
        });
    }
}
