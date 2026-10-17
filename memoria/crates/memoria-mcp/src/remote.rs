//! Remote mode: proxy all MCP tool calls to a Memoria REST API server.
//! Mirrors Python's HTTPBackend.

use crate::purge_args::parse_memory_purge_args;
use anyhow::Result;
use reqwest::Client;
use serde_json::{json, Value};

pub struct RemoteClient {
    client: Client,
    base_url: String,
    #[allow(dead_code)]
    user_id: String,
}

impl RemoteClient {
    pub fn new(api_url: &str, token: Option<&str>, user_id: String, tool: Option<&str>) -> Self {
        let mut headers = reqwest::header::HeaderMap::new();
        if let Some(t) = token {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}")) {
                headers.insert(reqwest::header::AUTHORIZATION, v);
            }
        }
        // Set X-User-Id header
        if let Ok(v) = reqwest::header::HeaderValue::from_str(&user_id) {
            headers.insert("X-User-Id", v);
        }
        // Set X-Memoria-Tool header (kiro / cursor / claude / codex)
        if let Some(t) = tool {
            if let Ok(v) = reqwest::header::HeaderValue::from_str(t) {
                headers.insert("X-Memoria-Tool", v);
            }
        }
        let client = Client::builder()
            .default_headers(headers)
            .no_proxy()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .expect("http client");
        Self {
            client,
            base_url: api_url.trim_end_matches('/').to_string(),
            user_id,
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }

    fn mcp_text(text: &str) -> Value {
        json!({"content": [{"type": "text", "text": text}]})
    }

    fn mcp_json(v: &Value) -> Value {
        Self::mcp_text(&serde_json::to_string_pretty(v).unwrap_or_default())
    }

    fn expect_tool_args<'a>(
        args: &'a Value,
        tool: &str,
        allowed: &[&str],
    ) -> Result<&'a serde_json::Map<String, Value>> {
        let map = args.as_object().ok_or_else(|| {
            crate::tool_result::input_error(format!("Invalid {tool} arguments: expected object"))
        })?;
        for key in map.keys() {
            if !allowed.iter().any(|allowed_key| allowed_key == key) {
                return Err(crate::tool_result::input_error(format!(
                    "Invalid {tool} argument '{key}': unknown field"
                )));
            }
        }
        Ok(map)
    }

    fn required_string_arg(args: &Value, tool: &str, field: &str) -> Result<String> {
        args.get(field)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                crate::tool_result::input_error(format!(
                    "Invalid {tool} '{field}': expected non-empty string"
                ))
            })
    }

    fn optional_i64_arg(args: &Value, tool: &str, field: &str, default: i64) -> Result<i64> {
        match args.get(field) {
            None => Ok(default),
            Some(value) => value.as_i64().ok_or_else(|| {
                crate::tool_result::input_error(format!(
                    "Invalid {tool} '{field}': expected integer"
                ))
            }),
        }
    }

    fn json_array_arg(args: &Value, field: &str) -> Result<Value> {
        match args.get(field) {
            None => Ok(Value::Array(Vec::new())),
            Some(Value::Array(values)) => Ok(Value::Array(values.clone())),
            Some(other) => Err(crate::tool_result::input_error(format!(
                "Invalid memory_apply '{field}': expected array, got {}",
                other
            ))),
        }
    }

    #[allow(dead_code)]
    fn mcp_err(e: impl std::fmt::Display) -> Value {
        crate::tool_result::error(e)
    }

    fn incomplete_response(tool: &str) -> anyhow::Error {
        use crate::tool_result::{ErrorKind, RemoteError};
        RemoteError {
            kind: ErrorKind::Backend,
            message: format!(
                "Remote API returned an incomplete {tool} response. Check service health before retrying."
            ),
        }
        .into()
    }

    /// A backend that swaps the pick mode either mutated a branch that was only meant to be
    /// previewed, or skipped a requested mutation. Both are backend contract failures, so
    /// `execution_error` adds the partial-write warning for this mutating tool.
    fn pick_mode_mismatch(preview_requested: bool) -> anyhow::Error {
        use crate::tool_result::{ErrorKind, RemoteError};
        let message = if preview_requested {
            "Remote API ignored the requested memory_pick dry-run and answered with an execution \
             result. Verify whether the target branch changed before retrying."
        } else {
            "Remote API answered a memory_pick execution with a dry-run preview, so nothing is \
             confirmed as applied. Check service health before retrying."
        };
        RemoteError {
            kind: ErrorKind::Backend,
            message: message.to_string(),
        }
        .into()
    }

    fn require_object<'a>(
        body: &'a Value,
        tool: &str,
    ) -> Result<&'a serde_json::Map<String, Value>> {
        body.as_object()
            .ok_or_else(|| Self::incomplete_response(tool))
    }

    fn require_str<'a>(body: &'a Value, tool: &str, field: &str) -> Result<&'a str> {
        Self::require_object(body, tool)?
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| Self::incomplete_response(tool))
    }

    fn require_i64(body: &Value, tool: &str, field: &str) -> Result<i64> {
        Self::require_object(body, tool)?
            .get(field)
            .and_then(Value::as_i64)
            .ok_or_else(|| Self::incomplete_response(tool))
    }

    fn require_array<'a>(body: &'a Value, tool: &str, field: &str) -> Result<&'a Vec<Value>> {
        Self::require_object(body, tool)?
            .get(field)
            .and_then(Value::as_array)
            .ok_or_else(|| Self::incomplete_response(tool))
    }

    /// `/v1/branches/:source/apply` answers with a serialized `ApplyResult`. Without every
    /// outcome list the applied/skipped split is unknown, so the write is unconfirmed.
    fn require_apply_result(body: &Value) -> Result<()> {
        for field in [
            "applied_adds",
            "skipped_adds",
            "applied_updates",
            "skipped_updates",
            "applied_removes",
            "skipped_removes",
            "applied_conflicts",
            "skipped_conflicts",
        ] {
            Self::require_array(body, "memory_apply", field)?;
        }
        Ok(())
    }

    fn require_bool(body: &Value, tool: &str, field: &str) -> Result<bool> {
        Self::require_object(body, tool)?
            .get(field)
            .and_then(Value::as_bool)
            .ok_or_else(|| Self::incomplete_response(tool))
    }

    async fn parse_response(r: reqwest::Response) -> Result<Value> {
        Self::decode_response(r, false).await
    }

    /// Branch delete is the only remote tool whose success contract is an empty 204.
    async fn parse_response_allow_empty(r: reqwest::Response) -> Result<Value> {
        Self::decode_response(r, true).await
    }

    async fn decode_response(r: reqwest::Response, allow_empty: bool) -> Result<Value> {
        let status = r.status();
        if status.is_success() {
            let body = r.text().await?;
            if body.trim().is_empty() {
                if allow_empty {
                    return Ok(Value::Null);
                }
                use crate::tool_result::{ErrorKind, RemoteError};
                return Err(RemoteError {
                    kind: ErrorKind::Backend,
                    message: format!(
                        "Remote API returned {status} with an empty body. Check service health before retrying."
                    ),
                }
                .into());
            }
            return serde_json::from_str(&body).map_err(|_| {
                use crate::tool_result::{ErrorKind, RemoteError};
                anyhow::Error::from(RemoteError {
                    kind: ErrorKind::Backend,
                    message: format!(
                        "Remote API returned {status} with a non-JSON body. Check service health before retrying."
                    ),
                })
            });
        }
        let is_text = r
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| {
                value
                    .split(';')
                    .next()
                    .is_some_and(|mime| mime.trim().eq_ignore_ascii_case("text/plain"))
            });
        let body = r.text().await.unwrap_or_default();
        use crate::tool_result::{ErrorKind, RemoteError};
        if status.is_server_error() {
            return Err(RemoteError {
                kind: ErrorKind::Backend,
                message: format!(
                    "Remote API returned {status}. Check service health before retrying."
                ),
            }
            .into());
        }
        // Preserve actionable validation/conflict messages, without forwarding
        // arbitrary HTML or proxy response bodies to the model.
        let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let message = parsed
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| parsed.get("message").and_then(Value::as_str))
            .or_else(|| (is_text && !body.trim().is_empty()).then_some(body.trim()))
            .unwrap_or("Check the tool arguments and access permissions.");
        let kind = match status.as_u16() {
            400 | 422 => ErrorKind::Input,
            401 | 403 | 404 | 409 => ErrorKind::Rejected,
            _ => ErrorKind::Backend,
        };
        Err(RemoteError {
            kind,
            message: format!("API error {status}: {message}"),
        }
        .into())
    }

    pub async fn call(&self, name: &str, args: Value) -> Result<Value> {
        // Keep business validation errors consistent with embedded execution.
        if let Err(e) = crate::tools::validate_tool_args(name, &args) {
            return Ok(crate::tool_result::error(e));
        }
        match name {
            "memory_store" => {
                let mut payload = json!({
                    "content": args["content"],
                    "memory_type": args["memory_type"].as_str().unwrap_or("semantic"),
                    "session_id": args["session_id"],
                    "trust_tier": args["trust_tier"],
                    "branch": args["branch"],
                });
                if let Some(sid) = args
                    .get("subject_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    payload["subject_id"] = json!(sid);
                }
                if args
                    .get("extra_metadata")
                    .map(Value::is_object)
                    .unwrap_or(false)
                {
                    payload["extra_metadata"] = args["extra_metadata"].clone();
                }
                let r = self
                    .client
                    .post(self.url("/v1/memories"))
                    .json(&payload)
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                let memory_id = Self::require_str(&body, "memory_store", "memory_id")?;
                let content = body["content"].as_str().unwrap_or("");
                Ok(Self::mcp_text(&format!(
                    "Stored memory {memory_id}: {content}"
                )))
            }

            "memory_retrieve" | "memory_search" => {
                let path = if name == "memory_search" {
                    "/v1/memories/search"
                } else {
                    "/v1/memories/retrieve"
                };
                let mut payload = json!({
                    "query": args["query"],
                    "top_k": if name == "memory_search" {
                        args["top_k"].as_i64().unwrap_or(10)
                    } else {
                        args["top_k"].as_i64().unwrap_or(5)
                    },
                    "session_id": args["session_id"],
                    "branch": args["branch"],
                });
                if let Some(session_scope) = args
                    .get("session_scope")
                    .and_then(serde_json::Value::as_str)
                {
                    payload["session_scope"] = json!(session_scope);
                }
                if let Some(subject_id) = args
                    .get("subject_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    payload["subject_id"] = json!(subject_id);
                }
                if let Some(memory_types) = args.get("memory_types").and_then(|v| v.as_array()) {
                    if !memory_types.is_empty() {
                        payload["memory_types"] = json!(memory_types);
                    }
                }
                let r = self
                    .client
                    .post(self.url(path))
                    .json(&payload)
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                let mems = body
                    .as_array()
                    .cloned()
                    .ok_or_else(|| Self::incomplete_response(name))?;
                if mems.is_empty() {
                    return Ok(Self::mcp_text("No relevant memories found."));
                }
                let text = mems
                    .iter()
                    .map(|m| {
                        let meta = m
                            .get("extra_metadata")
                            .filter(|v| v.is_object())
                            .map(|v| format!(" | metadata: {v}"))
                            .unwrap_or_default();
                        format!(
                            "[{}] ({}) {}{}",
                            m["memory_id"].as_str().unwrap_or(""),
                            m["memory_type"].as_str().unwrap_or(""),
                            m["content"].as_str().unwrap_or(""),
                            meta
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Self::mcp_text(&text))
            }

            "memory_correct" => {
                let new_content = args["new_content"].as_str().unwrap_or("");
                let memory_id = args["memory_id"].as_str().unwrap_or("");
                let query = args["query"].as_str().unwrap_or("");
                let r = if !memory_id.is_empty() {
                    self.client
                        .put(self.url(&format!("/v1/memories/{memory_id}/correct")))
                        .json(&json!({
                            "new_content": new_content,
                            "reason": args["reason"],
                            "branch": args["branch"]
                        }))
                        .send()
                        .await?
                } else {
                    let mut correct_payload = json!({
                        "query": query,
                        "new_content": new_content,
                        "session_id": args["session_id"],
                        "session_scope": args["session_scope"],
                        "reason": args["reason"],
                        "branch": args["branch"]
                    });
                    if let Some(sid) = args
                        .get("subject_id")
                        .and_then(Value::as_str)
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                    {
                        correct_payload["subject_id"] = json!(sid);
                    }
                    if let Some(memory_types) = args.get("memory_types").and_then(|v| v.as_array())
                    {
                        if !memory_types.is_empty() {
                            correct_payload["memory_types"] = json!(memory_types);
                        }
                    }
                    self.client
                        .post(self.url("/v1/memories/correct"))
                        .json(&correct_payload)
                        .send()
                        .await?
                };
                let body = Self::parse_response(r).await?;
                if body.get("error").is_some() {
                    return Ok(Self::mcp_text(&format!(
                        "No matching memory found for query '{query}'"
                    )));
                }
                let memory_id = Self::require_str(&body, "memory_correct", "memory_id")?;
                let content = body["content"].as_str().unwrap_or("");
                Ok(Self::mcp_text(&format!(
                    "Corrected memory {memory_id}: {content}"
                )))
            }

            "memory_purge" => {
                let purge_args = parse_memory_purge_args(&args)?;
                if let Some(memory_id) = purge_args.memory_id {
                    let ids: Vec<&str> = memory_id
                        .split(',')
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                        .collect();
                    let r = self
                        .client
                        .post(self.url("/v1/memories/purge"))
                        .json(&json!({"memory_ids": ids, "branch": purge_args.branch}))
                        .send()
                        .await?;
                    let body = Self::parse_response(r).await?;
                    let purged = Self::require_i64(&body, "memory_purge", "purged")?;
                    Ok(Self::mcp_text(&format!("Purged {purged} memory(s)")))
                } else if let Some(topic) = purge_args.topic {
                    let r = self
                        .client
                        .post(self.url("/v1/memories/purge"))
                        .json(&json!({"topic": topic, "branch": purge_args.branch}))
                        .send()
                        .await?;
                    let body = Self::parse_response(r).await?;
                    let purged = Self::require_i64(&body, "memory_purge", "purged")?;
                    Ok(Self::mcp_text(&format!(
                        "Purged {purged} memory(s) matching '{topic}'"
                    )))
                } else if let Some(session_id) = purge_args.session_id {
                    let r = self
                        .client
                        .post(self.url("/v1/memories/purge"))
                        .json(&json!({
                            "session_id": session_id,
                            "memory_types": purge_args.memory_types.map(|memory_types| {
                                memory_types
                                    .into_iter()
                                    .map(|memory_type| memory_type.to_string())
                                    .collect::<Vec<_>>()
                            }),
                            "branch": purge_args.branch,
                        }))
                        .send()
                        .await?;
                    let body = Self::parse_response(r).await?;
                    let purged = Self::require_i64(&body, "memory_purge", "purged")?;
                    Ok(Self::mcp_text(&format!(
                        "Purged {purged} memory(s) for session '{session_id}'"
                    )))
                } else {
                    Ok(crate::tool_result::error(
                        "Provide memory_id, topic, or session_id",
                    ))
                }
            }

            "memory_profile" => {
                let subject_id = args
                    .get("subject_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let branch = args
                    .get("branch")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let mut req = self.client.get(self.url("/v1/profiles/me"));
                if let Some(sid) = subject_id {
                    req = req.query(&[("subject_id", sid)]);
                }
                if let Some(b) = branch {
                    req = req.query(&[("branch", b)]);
                }
                let r = req.send().await?;
                let body = Self::parse_response(r).await?;
                let profile = Self::require_object(&body, "memory_profile")?
                    .get("profile")
                    .and_then(Value::as_str)
                    .ok_or_else(|| Self::incomplete_response("memory_profile"))?;
                if profile.is_empty() {
                    Ok(Self::mcp_text("No profile memories found."))
                } else {
                    Ok(Self::mcp_text(profile))
                }
            }

            "memory_list" => {
                let limit = args["limit"].as_i64().unwrap_or(20);
                let memory_type = args.get("memory_type").and_then(Value::as_str);
                let session_id = args.get("session_id").and_then(Value::as_str);
                let subject_id = args
                    .get("subject_id")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let mut req = self
                    .client
                    .get(self.url("/v1/memories"))
                    .query(&[("limit", limit)]);
                if let Some(memory_type) = memory_type {
                    req = req.query(&[("memory_type", memory_type)]);
                }
                if let Some(session_id) = session_id {
                    req = req.query(&[("session_id", session_id)]);
                }
                if let Some(subject_id) = subject_id {
                    req = req.query(&[("subject_id", subject_id)]);
                }
                if let Some(branch) = args.get("branch").and_then(Value::as_str) {
                    req = req.query(&[("branch", branch)]);
                }
                let body = Self::parse_response(req.send().await?).await?;
                let items = Self::require_object(&body, "memory_list")?
                    .get("items")
                    .and_then(Value::as_array)
                    .cloned()
                    .ok_or_else(|| Self::incomplete_response("memory_list"))?;
                if items.is_empty() {
                    return Ok(Self::mcp_text("No memories found."));
                }
                let text = items
                    .iter()
                    .map(|m| {
                        let meta = m
                            .get("extra_metadata")
                            .filter(|v| v.is_object())
                            .map(|v| format!(" | metadata: {v}"))
                            .unwrap_or_default();
                        format!(
                            "[{}] ({}) {}{}",
                            m["memory_id"].as_str().unwrap_or(""),
                            m["memory_type"].as_str().unwrap_or(""),
                            m["content"].as_str().unwrap_or(""),
                            meta
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                Ok(Self::mcp_text(&text))
            }

            "memory_capabilities" => Ok(crate::tools::capabilities_result()),

            "memory_get_retrieval_params" => {
                let r = self
                    .client
                    .get(self.url("/v1/retrieval-params"))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&serde_json::to_string_pretty(&body)?))
            }

            "memory_tune_params" => {
                let r = self
                    .client
                    .post(self.url("/v1/retrieval-params/tune"))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                if Self::require_bool(&body, "memory_tune_params", "tuned")? {
                    let old = &body["old_params"];
                    let new = &body["new_params"];
                    Ok(Self::mcp_text(&format!(
                        "Parameters tuned:\n  feedback_weight: {:.3} → {:.3}\n  temporal_decay_hours: {:.1} → {:.1}\n  confidence_weight: {:.3} → {:.3}",
                        old["feedback_weight"].as_f64().unwrap_or(0.0),
                        new["feedback_weight"].as_f64().unwrap_or(0.0),
                        old["temporal_decay_hours"].as_f64().unwrap_or(0.0),
                        new["temporal_decay_hours"].as_f64().unwrap_or(0.0),
                        old["confidence_weight"].as_f64().unwrap_or(0.0),
                        new["confidence_weight"].as_f64().unwrap_or(0.0)
                    )))
                } else {
                    Ok(Self::mcp_text(
                        body["message"]
                            .as_str()
                            .unwrap_or("Not enough feedback to tune parameters"),
                    ))
                }
            }

            "memory_governance" => {
                let r = self
                    .client
                    .post(self.url("/v1/governance"))
                    .json(&json!({"force": args["force"].as_bool().unwrap_or(false)}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                if body["skipped"].as_bool() == Some(true) {
                    let remaining =
                        Self::require_i64(&body, "memory_governance", "cooldown_remaining_s")?;
                    return Ok(Self::mcp_text(&format!(
                        "Governance skipped (cooldown: {remaining}s remaining)."
                    )));
                }
                let quarantined = Self::require_i64(&body, "memory_governance", "quarantined")?;
                let cleaned = Self::require_i64(&body, "memory_governance", "cleaned_stale")?;
                Ok(Self::mcp_text(&format!(
                    "Governance complete: quarantined={quarantined}, cleaned_stale={cleaned}"
                )))
            }

            "memory_rebuild_index" => {
                let r = self
                    .client
                    .post(self.url("/v1/governance"))
                    .json(&json!({"force": true}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                if body["skipped"].as_bool() == Some(true) {
                    let remaining =
                        Self::require_i64(&body, "memory_rebuild_index", "cooldown_remaining_s")?;
                    return Ok(Self::mcp_text(&format!(
                        "Index rebuild skipped (cooldown: {remaining}s remaining)."
                    )));
                }
                let _quarantined = Self::require_i64(&body, "memory_rebuild_index", "quarantined")?;
                let _cleaned = Self::require_i64(&body, "memory_rebuild_index", "cleaned_stale")?;
                Ok(Self::mcp_text(
                    "Index rebuild requested via governance endpoint.",
                ))
            }

            "memory_consolidate" => {
                let r = self
                    .client
                    .post(self.url("/v1/consolidate"))
                    .json(&json!({"force": args["force"].as_bool().unwrap_or(false)}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                if body["skipped"].as_bool() == Some(true) {
                    let remaining =
                        Self::require_i64(&body, "memory_consolidate", "cooldown_remaining_s")?;
                    return Ok(Self::mcp_text(&format!(
                        "Consolidation skipped (cooldown: {remaining}s remaining)."
                    )));
                }
                let conflicts =
                    Self::require_i64(&body, "memory_consolidate", "conflicts_detected")?;
                let orphaned = Self::require_i64(&body, "memory_consolidate", "orphaned_scenes")?;
                let promoted = Self::require_i64(&body, "memory_consolidate", "promoted")?;
                let demoted = Self::require_i64(&body, "memory_consolidate", "demoted")?;
                Ok(Self::mcp_text(&format!(
                    "Consolidation complete: conflicts_detected={conflicts}, orphaned_scenes={orphaned}, promoted={promoted}, demoted={demoted}"
                )))
            }

            "memory_reflect" => {
                let r = self
                    .client
                    .post(self.url("/v1/reflect"))
                    .json(&json!({
                        "force": args["force"].as_bool().unwrap_or(false),
                        "mode": args["mode"].as_str().unwrap_or("auto"),
                    }))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                if body["skipped"].as_bool() == Some(true) {
                    let remaining =
                        Self::require_i64(&body, "memory_reflect", "cooldown_remaining_s")?;
                    return Ok(Self::mcp_text(&format!(
                        "Reflection skipped (cooldown: {remaining}s remaining)."
                    )));
                }
                let candidates = match body.get("candidates") {
                    None => None,
                    Some(value) => Some(
                        value
                            .as_array()
                            .ok_or_else(|| Self::incomplete_response("memory_reflect"))?,
                    ),
                };
                if candidates.is_none() && body.get("scenes_created").is_none() {
                    return Err(Self::incomplete_response("memory_reflect"));
                }
                if let Some(candidates) = candidates {
                    if !candidates.is_empty() {
                        let parts: Vec<String> = candidates
                            .iter()
                            .enumerate()
                            .map(|(i, c)| {
                                let signal = c["signal"].as_str().unwrap_or("cluster");
                                let importance = c["importance"].as_f64().unwrap_or(0.5);
                                let mems: Vec<String> = c["memories"]
                                    .as_array()
                                    .unwrap_or(&vec![])
                                    .iter()
                                    .map(|m| format!("  - {}", m["content"].as_str().unwrap_or("")))
                                    .collect();
                                format!(
                                    "Cluster {} ({signal}, importance={importance:.3}):\n{}",
                                    i + 1,
                                    mems.join("\n")
                                )
                            })
                            .collect();
                        return Ok(Self::mcp_text(&format!(
                            "Here are memory clusters for reflection. Synthesize 1-2 insights per cluster, \
                             then store each via memory_store.\n\n{}", parts.join("\n\n"))));
                    }
                }
                // Every remaining success variant of /v1/reflect reports scenes_created, plus
                // either the candidate array or candidates_found.
                let scenes = Self::require_i64(&body, "memory_reflect", "scenes_created")?;
                let found = match candidates {
                    Some(candidates) => candidates.len() as i64,
                    None => Self::require_i64(&body, "memory_reflect", "candidates_found")?,
                };
                Ok(Self::mcp_text(&format!(
                    "Reflection complete: scenes_created={scenes}, candidates_found={found}"
                )))
            }

            "memory_extract_entities" => {
                let mode = args["mode"].as_str().unwrap_or("auto");
                let r = self
                    .client
                    .post(self.url("/v1/extract-entities"))
                    .json(&json!({"mode": mode}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&serde_json::to_string(&body)?))
            }

            "memory_link_entities" => {
                let entities_str = args["entities"].as_str().unwrap_or("[]");
                let entities: Value = serde_json::from_str(entities_str).unwrap_or(json!([]));
                let r = self
                    .client
                    .post(self.url("/v1/extract-entities/link"))
                    .json(&json!({"entities": entities}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_text(&serde_json::to_string(&body)?))
            }

            // Git tools — fully supported (unlike Python which returns "Not available via HTTP")
            "memory_snapshot" => {
                let r = self
                    .client
                    .post(self.url("/v1/snapshots"))
                    .json(&json!({"name": args["name"], "description": args["description"]}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                let name = Self::require_str(&body, "memory_snapshot", "name")?;
                Ok(Self::mcp_text(&format!("Snapshot '{name}' created.")))
            }

            "memory_snapshots" => {
                let limit = args["limit"].as_i64().unwrap_or(20);
                let offset = args["offset"].as_i64().unwrap_or(0);
                let r = self
                    .client
                    .get(self.url(&format!("/v1/snapshots?limit={limit}&offset={offset}")))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_json(&body))
            }

            "memory_snapshot_delete" => {
                // names can be a single string or array
                let mut payload = args.clone();
                if let Some(names_str) = args["names"].as_str() {
                    // Convert comma-separated string to array
                    let names: Vec<&str> = names_str.split(',').map(str::trim).collect();
                    payload["names"] = json!(names);
                }
                let r = self
                    .client
                    .post(self.url("/v1/snapshots/delete"))
                    .json(&payload)
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                if let Some(result) = body["result"].as_str().filter(|text| !text.is_empty()) {
                    return Ok(Self::mcp_text(result));
                }
                let deleted = Self::require_i64(&body, "memory_snapshot_delete", "deleted")?;
                Ok(Self::mcp_text(&format!("Deleted {deleted} snapshot(s).")))
            }

            "memory_rollback" => {
                let name = args["name"].as_str().unwrap_or("");
                let r = self
                    .client
                    .post(self.url(&format!("/v1/snapshots/{name}/rollback")))
                    .json(&json!({}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                let result = Self::require_str(&body, "memory_rollback", "result")?;
                Ok(Self::mcp_text(result))
            }

            "memory_branch" => {
                let r = self
                    .client
                    .post(self.url("/v1/branches"))
                    .json(&json!({
                        "name": args["name"],
                        "from_snapshot": args["from_snapshot"],
                        "from_timestamp": args["from_timestamp"],
                    }))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                let result = Self::require_str(&body, "memory_branch", "result")?;
                Ok(Self::mcp_text(result))
            }

            "memory_branches" => {
                let r = self.client.get(self.url("/v1/branches")).send().await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_json(&body))
            }

            "memory_checkout" => {
                let name = args["name"].as_str().unwrap_or("");
                let r = self
                    .client
                    .post(self.url(&format!("/v1/branches/{name}/checkout")))
                    .json(&json!({}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                let result = Self::require_str(&body, "memory_checkout", "result")?;
                Ok(Self::mcp_text(result))
            }

            "memory_merge" => {
                let source = args["source"].as_str().unwrap_or("");
                let r = self
                    .client
                    .post(self.url(&format!("/v1/branches/{source}/merge")))
                    .json(&json!({"strategy": args["strategy"].as_str().unwrap_or("accept")}))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                let result = Self::require_str(&body, "memory_merge", "result")?;
                Ok(Self::mcp_text(result))
            }

            "memory_pick" => {
                let source = args["source"].as_str().unwrap_or("");
                let target = args["target"].as_str().unwrap_or("main");
                let strategy = args["strategy"].as_str().unwrap_or("fail");
                let dry_run = args.get("dry_run").cloned().unwrap_or(Value::Null);
                let preview_requested = !dry_run.is_null();
                let r = self
                    .client
                    .post(self.url(&format!("/v1/branches/{source}/pick")))
                    .json(&json!({
                        "target": target,
                        "strategy": strategy,
                        "selector": args["selector"],
                        "dry_run": dry_run,
                    }))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                // The caller's request decides which contract applies; trusting the response
                // flag would let a backend that ignores the preview option report a real
                // mutation as a preview.
                if preview_requested != (body["dry_run"].as_bool() == Some(true)) {
                    return Err(Self::pick_mode_mismatch(preview_requested));
                }
                if preview_requested {
                    // A preview is only trustworthy with the full envelope; the flag alone
                    // cannot tell a real preview from a truncated mutation acknowledgement.
                    Self::require_str(&body, "memory_pick", "result")?;
                    Self::require_i64(&body["summary"], "memory_pick", "candidate_count")?;
                    Self::require_i64(&body["page"], "memory_pick", "limit")?;
                    return Ok(Self::mcp_json(&body));
                }
                let result = Self::require_str(&body, "memory_pick", "result")?;
                Ok(Self::mcp_text(result))
            }

            "memory_diff" => {
                Self::expect_tool_args(&args, "memory_diff", &["source", "limit"])?;
                let source = Self::required_string_arg(&args, "memory_diff", "source")?;
                let limit = Self::optional_i64_arg(&args, "memory_diff", "limit", 50)?;
                let r = self
                    .client
                    .get(self.url(&format!("/v1/branches/{source}/diff?limit={limit}")))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_json(&body))
            }

            "memory_apply" => {
                Self::expect_tool_args(
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
                let source = Self::required_string_arg(&args, "memory_apply", "source")?;
                let r = self
                    .client
                    .post(self.url(&format!("/v1/branches/{source}/apply")))
                    .json(&json!({
                        "adds": Self::json_array_arg(&args, "adds")?,
                        "updates": Self::json_array_arg(&args, "updates")?,
                        "removes": Self::json_array_arg(&args, "removes")?,
                        "accept_branch_conflicts": Self::json_array_arg(&args, "accept_branch_conflicts")?,
                    }))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Self::require_apply_result(&body)?;
                Ok(Self::mcp_json(&body))
            }

            "memory_branch_delete" => {
                let name = args["name"].as_str().unwrap_or("");
                let r = self
                    .client
                    .delete(self.url(&format!("/v1/branches/{name}")))
                    .send()
                    .await?;
                let _body = Self::parse_response_allow_empty(r).await?;
                Ok(Self::mcp_text(&format!("Branch '{name}' deleted.")))
            }

            "memory_observe" => {
                let mut payload = json!({
                    "messages": args["messages"],
                    "session_id": args["session_id"],
                    "branch": args["branch"],
                });
                if let Some(sid) = args
                    .get("subject_id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    payload["subject_id"] = json!(sid);
                }
                let r = self
                    .client
                    .post(self.url("/v1/observe"))
                    .json(&payload)
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                Ok(Self::mcp_json(&body))
            }

            "memory_feedback" => {
                let memory_id = args["memory_id"].as_str().unwrap_or("");
                let signal = args["signal"].as_str().unwrap_or("");
                let r = self
                    .client
                    .post(self.url(&format!("/v1/memories/{memory_id}/feedback")))
                    .json(&json!({
                        "signal": signal,
                        "context": args["context"],
                    }))
                    .send()
                    .await?;
                let body = Self::parse_response(r).await?;
                let feedback_id = Self::require_str(&body, "memory_feedback", "feedback_id")?;
                Ok(Self::mcp_text(&format!(
                    "Recorded feedback: memory={memory_id}, signal={signal}, feedback_id={feedback_id}"
                )))
            }

            _ => anyhow::bail!("Unknown tool: {name}"),
        }
    }

    pub async fn call_owned(self, name: String, args: Value) -> Result<Value> {
        self.call(&name, args).await
    }
}

#[cfg(test)]
mod tests {
    use super::RemoteClient;
    use axum::{
        extract::{Path, State},
        routing::{delete, post},
        Json, Router,
    };
    use serde_json::{json, Value};
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn remote_errors_preserve_plain_text_and_json_without_exposing_server_bodies() {
        use crate::tool_result::{error_kind, execution_error, ErrorKind};
        use axum::http::{header::CONTENT_TYPE, StatusCode};
        for (status, content_type, body, expected, kind) in [
            (
                404,
                "text/plain; charset=utf-8",
                "Memory not found: mem_abc",
                "mem_abc",
                ErrorKind::Rejected,
            ),
            (
                422,
                "text/plain",
                "Validation error: content required",
                "content required",
                ErrorKind::Input,
            ),
            (
                403,
                "text/plain",
                "Blocked: governance policy",
                "governance policy",
                ErrorKind::Rejected,
            ),
            (
                409,
                "application/json",
                r#"{"message":"merge conflict"}"#,
                "merge conflict",
                ErrorKind::Rejected,
            ),
            (
                502,
                "text/html",
                "<html>private proxy details</html>",
                "Check service health",
                ErrorKind::Backend,
            ),
            (
                400,
                "text/html",
                "<html>private proxy details</html>",
                "Check the tool arguments",
                ErrorKind::Input,
            ),
        ] {
            let app = Router::new().route(
                "/",
                post(move || async move {
                    (
                        StatusCode::from_u16(status).unwrap(),
                        [(CONTENT_TYPE, content_type)],
                        body,
                    )
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let response = reqwest::Client::new()
                .post(format!("http://{addr}/"))
                .send()
                .await
                .unwrap();
            let error = RemoteClient::parse_response(response).await.unwrap_err();
            let result = execution_error("memory_retrieve", error);
            assert_eq!(error_kind(&result), Some(kind));
            let text = result["content"][0]["text"].as_str().unwrap();
            assert!(text.contains(expected), "{text}");
            assert!(!text.contains("private proxy"));
            assert!(!text.contains("write may"));
            server.abort();
        }
    }

    async fn serve(app: Router) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), server)
    }

    #[tokio::test]
    async fn remote_delete_and_rebuild_failures_are_not_successes() {
        use crate::tool_result::{error_kind, execution_error, is_error, ErrorKind};
        use axum::http::{header::CONTENT_TYPE, StatusCode};

        let app = Router::new().route(
            "/v1/branches/:name",
            delete(|| async {
                (
                    StatusCode::CONFLICT,
                    [(CONTENT_TYPE, "text/plain")],
                    "Branch 'missing' not found",
                )
            }),
        );
        let (base, server) = serve(app).await;
        let remote = RemoteClient::new(&base, None, "test".into(), None);
        let error = remote
            .call("memory_branch_delete", json!({"name": "missing"}))
            .await
            .expect_err("a 409 must not be reported as a deleted branch");
        let result = execution_error("memory_branch_delete", error);
        assert!(is_error(&result));
        assert_eq!(error_kind(&result), Some(ErrorKind::Rejected));
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Branch 'missing' not found"), "{text}");
        assert!(!text.contains("deleted"), "{text}");
        server.abort();

        let app = Router::new().route(
            "/v1/governance",
            post(|| async {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(CONTENT_TYPE, "text/html")],
                    "<html>private sql</html>",
                )
            }),
        );
        let (base, server) = serve(app).await;
        let remote = RemoteClient::new(&base, None, "test".into(), None);
        let error = remote
            .call("memory_rebuild_index", json!({"table": "mem_memories"}))
            .await
            .expect_err("a 5xx must not be reported as a requested rebuild");
        let result = execution_error("memory_rebuild_index", error);
        assert!(is_error(&result));
        assert_eq!(error_kind(&result), Some(ErrorKind::Backend));
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("Check service health"), "{text}");
        assert!(!text.contains("requested"), "{text}");
        assert!(!text.contains("private sql"), "{text}");
        server.abort();
    }

    #[tokio::test]
    async fn remote_branch_delete_accepts_empty_success() {
        use axum::http::StatusCode;
        let app = Router::new().route(
            "/v1/branches/:name",
            delete(|| async { StatusCode::NO_CONTENT }),
        );
        let (base, server) = serve(app).await;
        let remote = RemoteClient::new(&base, None, "test".into(), None);
        let result = remote
            .call("memory_branch_delete", json!({"name": "topic"}))
            .await
            .expect("204 is a successful delete");
        assert_eq!(result["content"][0]["text"], "Branch 'topic' deleted.");
        assert!(result.get("isError").is_none());
        server.abort();
    }

    async fn stub(
        path: &str,
        status: axum::http::StatusCode,
        body: &'static str,
    ) -> (String, tokio::task::JoinHandle<()>) {
        use axum::http::header::CONTENT_TYPE;
        use axum::routing::any;
        let app = Router::new().route(
            path,
            any(move || async move { (status, [(CONTENT_TYPE, "application/json")], body) }),
        );
        serve(app).await
    }

    async fn tool_text(base: &str, tool: &str, args: Value) -> Result<String, String> {
        use crate::tool_result::{execution_error, is_error};
        let remote = RemoteClient::new(base, None, "test".into(), None);
        match remote.call(tool, args).await {
            Ok(result) => {
                if is_error(&result) {
                    Err(result["content"][0]["text"]
                        .as_str()
                        .unwrap_or("")
                        .to_string())
                } else {
                    Ok(result["content"][0]["text"]
                        .as_str()
                        .unwrap_or("")
                        .to_string())
                }
            }
            Err(error) => Err(execution_error(tool, error)["content"][0]["text"]
                .as_str()
                .unwrap_or("")
                .to_string()),
        }
    }

    #[tokio::test]
    async fn remote_json_tools_do_not_invent_success_from_an_incomplete_body() {
        use axum::http::StatusCode;
        let failures = [
            (
                "/v1/memories",
                StatusCode::NO_CONTENT,
                "",
                "memory_store",
                json!({"content": "must be acknowledged"}),
                "Stored memory",
            ),
            (
                "/v1/memories",
                StatusCode::OK,
                "{}",
                "memory_store",
                json!({"content": "must be acknowledged"}),
                "Stored memory",
            ),
            (
                "/v1/governance",
                StatusCode::OK,
                "{}",
                "memory_governance",
                json!({}),
                "Governance complete",
            ),
            (
                "/v1/governance",
                StatusCode::OK,
                "{}",
                "memory_rebuild_index",
                json!({}),
                "requested",
            ),
            (
                "/v1/branches/topic/checkout",
                StatusCode::OK,
                "{}",
                "memory_checkout",
                json!({"name": "topic"}),
                "Switched",
            ),
            (
                "/v1/snapshots",
                StatusCode::OK,
                "{}",
                "memory_snapshot",
                json!({"name": "snap"}),
                "created",
            ),
            (
                "/v1/snapshots/snap/rollback",
                StatusCode::OK,
                "",
                "memory_rollback",
                json!({"name": "snap"}),
                "Rolled",
            ),
            (
                "/v1/memories/purge",
                StatusCode::OK,
                "{}",
                "memory_purge",
                json!({"memory_id": "mem_1"}),
                "Purged",
            ),
            (
                "/v1/memories/retrieve",
                StatusCode::OK,
                "{}",
                "memory_retrieve",
                json!({"query": "topic"}),
                "No relevant",
            ),
            (
                "/v1/branches",
                StatusCode::OK,
                "{}",
                "memory_branch",
                json!({"name": "topic"}),
                "Created branch",
            ),
            (
                "/v1/branches/:name/pick",
                StatusCode::OK,
                r#"{"unexpected":1}"#,
                "memory_pick",
                json!({"source": "topic", "target": "main", "selector": {"type": "key_list", "keys": ["m1"]}}),
                "unexpected",
            ),
            (
                "/v1/branches/:name/pick",
                StatusCode::OK,
                r#"{"dry_run":true}"#,
                "memory_pick",
                json!({"source": "topic", "target": "main", "selector": {"type": "key_list", "keys": ["m1"]}, "dry_run": {}}),
                "dry_run",
            ),
            (
                "/v1/reflect",
                StatusCode::OK,
                r#"{"candidates":null}"#,
                "memory_reflect",
                json!({"mode": "candidates"}),
                "Reflection complete",
            ),
            (
                "/v1/reflect",
                StatusCode::OK,
                r#"{"scenes_created":2}"#,
                "memory_reflect",
                json!({}),
                "Reflection complete",
            ),
            (
                "/v1/branches/:name/merge",
                StatusCode::OK,
                "{}",
                "memory_merge",
                json!({"source": "topic"}),
                "{}",
            ),
            (
                "/v1/branches/:source/apply",
                StatusCode::OK,
                "{}",
                "memory_apply",
                json!({"source": "topic", "adds": ["m1"]}),
                "{}",
            ),
            (
                "/v1/branches/:source/apply",
                StatusCode::OK,
                r#"{"applied_adds":["m1"],"applied_updates":[],"applied_removes":[],"applied_conflicts":[]}"#,
                "memory_apply",
                json!({"source": "topic", "adds": ["m1"]}),
                "applied_adds",
            ),
        ];
        for (path, status, body, tool, args, forbidden) in failures {
            let (base, server) = stub(path, status, body).await;
            let text = tool_text(&base, tool, args)
                .await
                .expect_err("incomplete response must not be a success");
            assert!(!text.contains(forbidden), "{tool}: {text}");
            assert!(
                text.contains("empty body") || text.contains("incomplete"),
                "{tool}: {text}"
            );
            server.abort();
        }

        let (base, server) = stub(
            "/v1/memories",
            StatusCode::CREATED,
            r#"{"memory_id":"mem_1","content":"must be acknowledged"}"#,
        )
        .await;
        let text = tool_text(
            &base,
            "memory_store",
            json!({"content": "must be acknowledged"}),
        )
        .await
        .expect("complete store response");
        assert!(text.contains("mem_1"), "{text}");
        server.abort();

        let (base, server) = stub("/v1/memories/retrieve", StatusCode::OK, "[]").await;
        let text = tool_text(&base, "memory_retrieve", json!({"query": "topic"}))
            .await
            .expect("an empty result array is a real response");
        assert!(text.contains("No relevant memories found."), "{text}");
        server.abort();

        let (base, server) = stub(
            "/v1/governance",
            StatusCode::OK,
            r#"{"quarantined":2,"cleaned_stale":1}"#,
        )
        .await;
        let text = tool_text(&base, "memory_governance", json!({}))
            .await
            .expect("governance counts are a real response");
        assert!(text.contains("quarantined=2"), "{text}");
        server.abort();

        let (base, server) = stub(
            "/v1/branches/topic/checkout",
            StatusCode::OK,
            r#"{"result":"Switched to branch 'topic'. 3 memories on this branch."}"#,
        )
        .await;
        let text = tool_text(&base, "memory_checkout", json!({"name": "topic"}))
            .await
            .expect("checkout result text");
        assert!(text.contains("3 memories"), "{text}");
        assert!(!text.contains("0 memories"), "{text}");
        server.abort();

        let pick_args = json!({"source": "topic", "target": "main", "selector": {"type": "key_list", "keys": ["m1"]}});
        let (base, server) = stub(
            "/v1/branches/:name/pick",
            StatusCode::OK,
            r#"{"result":"Picked 1 memory from branch 'topic' into 'main'."}"#,
        )
        .await;
        let text = tool_text(&base, "memory_pick", pick_args.clone())
            .await
            .expect("a confirmed pick result is a real response");
        assert!(text.contains("Picked 1 memory"), "{text}");
        server.abort();

        let (base, server) = stub(
            "/v1/branches/:name/pick",
            StatusCode::OK,
            r#"{"dry_run":true,"result":"Previewed 1 candidate change(s).","summary":{"candidate_count":1},"page":{"limit":20,"offset":0}}"#,
        )
        .await;
        let mut dry_run_args = pick_args;
        dry_run_args["dry_run"] = json!({});
        let text = tool_text(&base, "memory_pick", dry_run_args)
            .await
            .expect("a full preview envelope is a real response");
        assert!(text.contains("Previewed 1 candidate"), "{text}");
        server.abort();

        let (base, server) = stub(
            "/v1/reflect",
            StatusCode::OK,
            r#"{"candidates":[],"scenes_created":0}"#,
        )
        .await;
        let text = tool_text(&base, "memory_reflect", json!({"mode": "candidates"}))
            .await
            .expect("an empty candidate array is a real response");
        assert!(
            text.contains("scenes_created=0, candidates_found=0"),
            "{text}"
        );
        server.abort();

        let (base, server) = stub(
            "/v1/reflect",
            StatusCode::OK,
            r#"{"scenes_created":2,"candidates_found":3}"#,
        )
        .await;
        let text = tool_text(&base, "memory_reflect", json!({}))
            .await
            .expect("reflect counts are a real response");
        assert!(
            text.contains("scenes_created=2, candidates_found=3"),
            "{text}"
        );
        server.abort();

        let (base, server) = stub(
            "/v1/branches/:name/merge",
            StatusCode::OK,
            r#"{"result":"Merged 2 memories from 'topic' into main."}"#,
        )
        .await;
        let text = tool_text(&base, "memory_merge", json!({"source": "topic"}))
            .await
            .expect("merge result text");
        assert!(text.contains("Merged 2 memories"), "{text}");
        server.abort();
    }

    #[tokio::test]
    async fn remote_pick_requires_the_mode_the_caller_asked_for() {
        use axum::http::StatusCode;
        let args = json!({"source": "topic", "target": "main", "selector": {"type": "key_list", "keys": ["m1"]}});
        let execution_body = r#"{"result":"Picked 1 memory from branch 'topic' into 'main'."}"#;
        let preview_body = r#"{"dry_run":true,"result":"Previewed 1 candidate change(s).","summary":{"candidate_count":1},"page":{"limit":20,"offset":0}}"#;

        // A backend that ignores the preview option may already have changed the target
        // branch, so a preview request answered with an execution result must not succeed.
        let (base, server) = stub("/v1/branches/:name/pick", StatusCode::OK, execution_body).await;
        let mut preview_args = args.clone();
        preview_args["dry_run"] = json!({});
        let text = tool_text(&base, "memory_pick", preview_args)
            .await
            .expect_err("an execution result cannot answer a preview request");
        assert!(!text.contains("Picked 1 memory"), "{text}");
        assert!(
            text.contains("ignored the requested memory_pick dry-run"),
            "{text}"
        );
        assert!(text.contains("write may"), "{text}");
        server.abort();

        // The opposite swap applied nothing at all.
        let (base, server) = stub("/v1/branches/:name/pick", StatusCode::OK, preview_body).await;
        let text = tool_text(&base, "memory_pick", args)
            .await
            .expect_err("a preview cannot answer an execution request");
        assert!(!text.contains("Previewed"), "{text}");
        assert!(text.contains("nothing is confirmed as applied"), "{text}");
        server.abort();
    }

    #[derive(Clone)]
    struct ApplyCapture {
        branch: Arc<Mutex<Option<String>>>,
        body: Arc<Mutex<Option<Value>>>,
    }

    #[tokio::test]
    async fn memory_apply_remote_call_uses_branch_apply_endpoint() {
        let capture = ApplyCapture {
            branch: Arc::new(Mutex::new(None)),
            body: Arc::new(Mutex::new(None)),
        };

        let app = Router::new()
            .route(
                "/v1/branches/:source/apply",
                post(
                    |State(capture): State<ApplyCapture>,
                     Path(source): Path<String>,
                     Json(payload): Json<Value>| async move {
                        *capture.branch.lock().unwrap() = Some(source);
                        *capture.body.lock().unwrap() = Some(payload);
                        Json(json!({
                            "applied_adds": ["new-id"],
                            "skipped_adds": [],
                            "applied_updates": [],
                            "skipped_updates": [],
                            "applied_removes": [],
                            "skipped_removes": [],
                            "applied_conflicts": [],
                            "skipped_conflicts": []
                        }))
                    },
                ),
            )
            .with_state(capture.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("serve test router");
        });

        let remote = RemoteClient::new(
            &format!("http://{addr}"),
            Some("sk-test"),
            "default".to_string(),
            Some("kiro"),
        );
        let result = remote
            .call(
                "memory_apply",
                json!({
                    "source": "feature-1",
                    "adds": ["new-id"],
                    "updates": [{"old_id": "old-id", "new_id": "new-id"}],
                    "removes": ["gone-id"],
                    "accept_branch_conflicts": ["conflict-id"],
                }),
            )
            .await
            .expect("memory_apply should succeed");

        let text = result["content"][0]["text"].as_str().expect("text result");
        assert!(text.contains("applied_adds"));
        assert_eq!(capture.branch.lock().unwrap().as_deref(), Some("feature-1"));
        assert_eq!(
            capture.body.lock().unwrap().clone(),
            Some(json!({
                "adds": ["new-id"],
                "updates": [{"old_id": "old-id", "new_id": "new-id"}],
                "removes": ["gone-id"],
                "accept_branch_conflicts": ["conflict-id"],
            }))
        );

        let _ = shutdown_tx.send(());
        let _ = server.await;
    }

    #[tokio::test]
    async fn memory_apply_remote_defaults_missing_arrays_to_empty() {
        let capture = ApplyCapture {
            branch: Arc::new(Mutex::new(None)),
            body: Arc::new(Mutex::new(None)),
        };

        let app = Router::new()
            .route(
                "/v1/branches/:source/apply",
                post(
                    |State(capture): State<ApplyCapture>,
                     Path(source): Path<String>,
                     Json(payload): Json<Value>| async move {
                        *capture.branch.lock().unwrap() = Some(source);
                        *capture.body.lock().unwrap() = Some(payload);
                        Json(json!({
                            "applied_adds": [],
                            "skipped_adds": [],
                            "applied_updates": [],
                            "skipped_updates": [],
                            "applied_removes": [],
                            "skipped_removes": [],
                            "applied_conflicts": [],
                            "skipped_conflicts": []
                        }))
                    },
                ),
            )
            .with_state(capture.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("local addr");
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
                .expect("serve test router");
        });

        let remote = RemoteClient::new(
            &format!("http://{addr}"),
            Some("sk-test"),
            "default".to_string(),
            Some("kiro"),
        );
        remote
            .call("memory_apply", json!({"source": "feature-2"}))
            .await
            .expect("memory_apply should succeed with missing arrays");

        assert_eq!(
            capture.body.lock().unwrap().clone(),
            Some(json!({
                "adds": [],
                "updates": [],
                "removes": [],
                "accept_branch_conflicts": [],
            }))
        );

        let _ = shutdown_tx.send(());
        let _ = server.await;
    }

    #[tokio::test]
    async fn memory_diff_remote_rejects_unknown_fields() {
        let remote = RemoteClient::new(
            "http://127.0.0.1:9",
            Some("sk-test"),
            "default".to_string(),
            Some("kiro"),
        );
        let err = remote
            .call(
                "memory_diff",
                json!({
                    "source": "feature-3",
                    "adds": ["unexpected"]
                }),
            )
            .await
            .expect_err("memory_diff should reject unknown fields before sending");

        assert!(err
            .to_string()
            .contains("Invalid memory_diff argument 'adds': unknown field"));
    }
}
