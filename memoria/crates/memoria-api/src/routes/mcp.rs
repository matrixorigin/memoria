//! Streamable HTTP MCP endpoint.
//!
//! Exposes `POST /mcp` so AI clients (Cursor, Claude, etc.) can reach Memoria
//! without installing a local binary — just a URL + Bearer token:
//!
//! ```json
//! {
//!   "mcpServers": {
//!     "memoria": {
//!       "url": "https://memoria-host:8100/mcp",
//!       "headers": { "Authorization": "Bearer sk-..." }
//!     }
//!   }
//! }
//! ```
//!
//! Auth is handled by the existing `AuthUser` extractor (Bearer → SHA-256 →
//! `mem_api_keys` DB lookup with cache + rate limiting).  Tool dispatch reuses
//! `memoria_mcp::dispatch_http` which drives `Mode::Embedded` internally.

use axum::{extract::State, http::StatusCode, response::IntoResponse, Json};
use serde_json::json;

use crate::{
    auth::{AuthUser, RpcMeta, SCOPE_MEMORY_READ, SCOPE_MEMORY_WRITE},
    state::AppState,
};

/// Derive a stable tracking path from the JSON-RPC method + params so that
/// Streamable HTTP calls appear alongside the existing `/v1/*` entries in
/// `mem_api_call_log` and surface correctly in the Usage / Monitor dashboards.
///
/// Convention:
///   `tools/call` → `/mcp/<tool_name>`   (e.g. `/mcp/memory_store`)
///   anything else → `/mcp/<method>`     (e.g. `/mcp/initialize`, `/mcp/tools.list`)
///
/// The tool name comes from client-supplied `params.name`, so it is sanitized:
///   - only ASCII alphanumerics and `_` are kept (no `/`, `.`, `..`, spaces, etc.)
///   - clamped to 64 chars to bound DB column width
///   - falls back to `/mcp/tools.call` when missing or empty after sanitization
fn tracking_path(method: &str, params: Option<&serde_json::Value>) -> String {
    if method == "tools/call" {
        let raw = params
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        // Allow only [a-zA-Z0-9_] — same character set used by MCP tool names.
        let sanitized: String = raw
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
            .take(64)
            .collect();
        if !sanitized.is_empty() {
            return format!("/mcp/{sanitized}");
        }
        // Unknown / malformed tool name — bucket under a single fixed path.
        return "/mcp/tools.call".to_string();
    }
    // Sanitize and clamp: allow only [a-zA-Z0-9_.] and cap at 64 chars so the result
    // always fits within the VARCHAR(256) path column.
    // Additionally require at least one alphanumeric character so that inputs like
    // "///" (all slashes → all dots) fall back to the unknown bucket rather than
    // producing a misleading "/mcp/..." path.
    let sanitized: String = method
        .chars()
        .map(|c| if c == '/' { '.' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '.')
        .take(64)
        .collect();
    let has_alnum = sanitized
        .chars()
        .any(|c| c.is_ascii_alphanumeric() || c == '_');
    if sanitized.is_empty() || !has_alnum {
        return "/mcp/unknown".to_string();
    }
    format!("/mcp/{sanitized}")
}

/// MCP write tools that mutate the active memory table.
/// These are blocked when the caller is group-scoped and checked out on `main`,
/// mirroring the REST `group_main_write_guard` middleware.
///
/// Read-only tools, branch-management tools (`memory_checkout`, `memory_branch`,
/// `memory_branch_delete`), and snapshot ops are intentionally excluded.
const GROUP_MAIN_WRITE_BLOCKED: &[&str] = &[
    "memory_store",
    "memory_correct",
    "memory_purge",
    "memory_observe",
    "memory_governance",
    "memory_consolidate",
    "memory_reflect",
    "memory_extract_entities",
    "memory_link_entities",
    "memory_rollback",
];

fn mcp_explicit_non_main_branch(params: Option<&serde_json::Value>) -> bool {
    params
        .and_then(|params| params.get("arguments"))
        .and_then(|args| args.get("branch"))
        .and_then(|branch| branch.as_str())
        .map(str::trim)
        .is_some_and(|branch| !branch.is_empty() && branch != "main")
}

fn mcp_tool_supports_explicit_branch(tool: &str) -> bool {
    matches!(
        tool,
        "memory_store" | "memory_correct" | "memory_purge" | "memory_observe"
    )
}

fn mcp_tool_mutates_main_regardless_checkout(tool: &str) -> bool {
    matches!(tool, "memory_governance")
}

fn mcp_tool_dirty_mask(tool: &str) -> Option<crate::metrics_summary::DirtyMask> {
    use crate::metrics_summary::DirtyMask;
    match tool {
        "memory_store" | "memory_correct" | "memory_purge" | "memory_observe" => {
            Some(DirtyMask::MEMORY)
        }
        "memory_governance"
        | "memory_consolidate"
        | "memory_reflect"
        | "memory_extract_entities"
        | "memory_link_entities" => Some(DirtyMask::MEMORY | DirtyMask::GRAPH),
        "memory_feedback" => Some(DirtyMask::FEEDBACK),
        "memory_snapshot" | "memory_snapshot_delete" => Some(DirtyMask::SNAPSHOT),
        "memory_rollback" => Some(DirtyMask::FULL),
        "memory_branch" | "memory_branch_delete" => Some(DirtyMask::BRANCH),
        "memory_checkout" | "memory_merge" | "memory_pick" => Some(DirtyMask::FULL),
        _ => None,
    }
}

/// Authorization is independent of metrics invalidation. Include callable tools
/// that are not advertised by tools/list, and fail closed for unclassified names.
fn mcp_tool_required_scope(tool: &str) -> Option<&'static str> {
    match tool {
        "memory_retrieve"
        | "memory_search"
        | "memory_profile"
        | "memory_list"
        | "memory_capabilities"
        | "memory_get_retrieval_params"
        | "memory_snapshots"
        | "memory_branches"
        | "memory_diff" => Some(SCOPE_MEMORY_READ),
        "memory_store"
        | "memory_correct"
        | "memory_purge"
        | "memory_observe"
        | "memory_governance"
        | "memory_rebuild_index"
        | "memory_consolidate"
        | "memory_reflect"
        | "memory_extract_entities"
        | "memory_link_entities"
        | "memory_feedback"
        | "memory_tune_params"
        | "memory_snapshot"
        | "memory_snapshot_delete"
        | "memory_rollback"
        | "memory_branch"
        | "memory_checkout"
        | "memory_merge"
        | "memory_pick"
        | "memory_branch_delete"
        | "memory_apply" => Some(SCOPE_MEMORY_WRITE),
        _ => None,
    }
}

fn spawn_metrics_dirty_mark(
    state: AppState,
    user_id: String,
    mask: crate::metrics_summary::DirtyMask,
) {
    tokio::spawn(async move {
        if let Err(e) = state.mark_metrics_dirty(&user_id, mask).await {
            tracing::warn!(
                user_id = user_id,
                error = %e,
                "failed to mark metrics summary dirty after mcp mutation"
            );
        }
    });
}

fn result_outcome(method: &str, result: &serde_json::Value) -> RpcMeta {
    let mut rpc = RpcMeta::ok();
    if method == "tools/call" {
        let kind = memoria_mcp::tool_result::error_kind(result);
        rpc.tool_success = Some(kind.is_none());
        rpc.tool_error_kind = kind.map(|kind| kind.as_str());
    }
    rpc
}

fn report_tool_outcome(tool: Option<&str>, rpc: &RpcMeta) {
    if let Some(kind) = rpc.tool_error_kind {
        if kind == "backend" {
            tracing::warn!(tool, kind, "MCP tool backend failure");
        } else {
            tracing::debug!(tool, kind, "MCP tool input or operation rejected");
        }
    }
}

pub async fn mcp_handler(
    State(state): State<AppState>,
    auth: AuthUser,
    headers: axum::http::HeaderMap,
    body: String,
) -> impl IntoResponse {
    // Start timing after auth. Billable MCP requests are recorded below; transport
    // liveness checks short-circuit before entering the usage-accounting pipeline.
    let t = std::time::Instant::now();

    // Helper: record a validation-failure entry and return early.
    // Uses underscore-prefixed paths so they never collide with real tool names.
    macro_rules! validation_err {
        ($path:expr, $code:expr, $body:expr) => {{
            // No tool has been admitted. Per-user log flushing can provision a
            // memory DB, so malformed requests use non-memory telemetry only.
            tracing::warn!(user_id = %auth.user_id, path = $path, rpc_code = $code, "invalid MCP request");
            if let Some(reporter) = &state.stats_reporter {
                reporter.report(memoria_service::stats_reporter::StatsEvent::ApiCallLogged {
                    user_id: auth.user_id.clone(),
                    path: $path.to_string(),
                    is_mcp: true,
                    is_success: false,
                });
            }
            return Json($body).into_response();
        }};
    }

    let req: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => validation_err!(
            "/mcp/_parse_error",
            -32700,
            json!({"jsonrpc": "2.0", "id": null,
                   "error": {"code": -32700, "message": e.to_string()}})
        ),
    };

    // Per JSON-RPC 2.0 §4, a Request object MUST be a JSON object.
    // Anything else (array, string, number, …) is Invalid Request.
    if !req.is_object() {
        validation_err!(
            "/mcp/_invalid_request",
            -32600,
            json!({"jsonrpc": "2.0", "id": null,
                   "error": {"code": -32600,
                              "message": "Invalid Request: payload must be a JSON object"}})
        );
    }

    // Reject non-conforming IDs before ping, notification, or tool dispatch.
    if req.get("id").is_some_and(|id| !(id.is_string() || id.is_number() || id.is_null())) {
        validation_err!(
            "/mcp/_invalid_request",
            -32600,
            json!({"jsonrpc": "2.0", "id": null,
                   "error": {"code": -32600, "message": "Invalid Request: id must be string, number or null"}})
        );
    }

    // "jsonrpc" MUST equal exactly "2.0".
    if req.get("jsonrpc").and_then(|v| v.as_str()) != Some("2.0") {
        let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
        validation_err!(
            "/mcp/_invalid_request",
            -32600,
            json!({"jsonrpc": "2.0", "id": id,
                   "error": {"code": -32600,
                              "message": "Invalid Request: jsonrpc must be \"2.0\""}})
        );
    }

    // "method" MUST be a non-empty string.
    let method = match req.get("method").and_then(|v| v.as_str()) {
        Some(m) if !m.is_empty() => m.to_string(),
        _ => {
            let id = req.get("id").cloned().unwrap_or(serde_json::Value::Null);
            validation_err!(
                "/mcp/_invalid_request",
                -32600,
                json!({"jsonrpc": "2.0", "id": id,
                       "error": {"code": -32600,
                                  "message": "Invalid Request: method must be a non-empty string"}})
            );
        }
    };

    // `ping` is a transport-level liveness check and can be sent frequently by MCP
    // clients. Keep authentication and rate limiting, but do not count it as product
    // usage or persist it in the API call log.
    if method == "ping" {
        if req.get("id").is_none() {
            return StatusCode::NO_CONTENT.into_response();
        }
        let id = req["id"].clone();
        return Json(json!({"jsonrpc": "2.0", "id": id, "result": {}})).into_response();
    }

    // Control notifications are not business calls. Authentication and request
    // validation have already run; keep them out of usage/error statistics.
    // This intentionally includes initialized and its agent-header usage marker;
    // the initialize request remains an accounted handshake call.
    // Do not bypass tool authorization for arbitrary id-less tools/call messages.
    if memoria_mcp::accept_notification(&method, req.get("id")) {
        return StatusCode::NO_CONTENT.into_response();
    }

    let params = req.get("params").cloned();
    let track_path = tracking_path(&method, params.as_ref());
    let tracked_tool = if method == "tools/call" {
        track_path
            .strip_prefix("/mcp/")
            .filter(|name| !name.is_empty() && *name != "tools.call")
            .map(str::to_string)
    } else {
        None
    };
    let user_id = auth.user_id.clone();
    let scope_id = auth.scope_id.clone();

    // Single reporting point for MCP call stats, shared by both the
    // notification path and the regular-request path below.
    let report_stats = {
        let reporter = state.stats_reporter.clone();
        let uid = user_id.clone();
        move |path: &str, is_success: bool| {
            if let Some(r) = &reporter {
                r.report(memoria_service::stats_reporter::StatsEvent::ApiCallLogged {
                    user_id: uid.clone(),
                    path: path.to_string(),
                    is_mcp: true,
                    is_success,
                });
            }
        }
    };
    // Use the exact dispatch name, never a sanitized/truncated metrics label.
    if method == "tools/call" {
        if let Err(error) = memoria_mcp::validate_tool_call(params.as_ref()) {
            report_stats(&track_path, false);
            if req.get("id").is_none() {
                return StatusCode::NO_CONTENT.into_response();
            }
            return Json(json!({"jsonrpc": "2.0", "id": req["id"],
                "error": {"code": error.code, "message": error.message}}))
            .into_response();
        }
    }
    let authorization_error = if method == "tools/call" {
        let name = params
            .as_ref()
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("");
        match mcp_tool_required_scope(name) {
            Some(scope) if !auth.has_scope(scope) => {
                Some(format!("API key missing required scope: {scope}"))
            }
            Some(_) => None,
            None => Some("MCP tool is not authorized: unclassified tool".to_string()),
        }
    } else {
        None
    };

    // Reject before *any* per-user instrumentation or group storage lookup.
    // A missing memory grant is not authorization to create a database merely
    // to log the refusal. Shared stats and tracing retain the denial evidence.
    if let Some(message) = authorization_error {
        report_stats(&track_path, false);
        tracing::warn!(user_id = %user_id, path = %track_path, rpc_code = -32003, "MCP scope admission denied");
        if req.get("id").is_none() {
            return StatusCode::NO_CONTENT.into_response();
        }
        return Json(json!({"jsonrpc": "2.0", "id": req["id"],
            "error": {"code": -32003, "message": message}}))
        .into_response();
    }

    // Identity-only metadata calls (initialize, tools/list, etc.) must not
    // implicitly enable memory storage either. AuthUser defers MCP headers here.
    let memory_telemetry = auth.has_scope(SCOPE_MEMORY_READ) || auth.has_scope(SCOPE_MEMORY_WRITE);
    if memory_telemetry {
        if let Some(tool) = tracked_tool.clone() {
            state.tool_usage_batcher.mark_used(user_id.clone(), tool);
        }
        if let Some(agent) = crate::auth::request_tool_name(&headers) {
            state.tool_usage_batcher.mark_used(user_id.clone(), agent);
        }
    }
    let record_call = |status_code, rpc| {
        if memory_telemetry {
            state.call_log_batcher.record_rpc(
                user_id.clone(),
                "POST".to_string(),
                track_path.clone(),
                status_code,
                t.elapsed().as_millis() as u32,
                rpc,
            );
        }
    };

    // ── Group main-write guard (computed once, shared by both code paths) ─────
    // Resolved here — before the Notification early-return — so that
    // Notification-form write calls to `main` are also blocked instead of
    // silently bypassing the restriction.
    //
    // Returns Some(tool_name) when the call must be blocked, None otherwise.
    let blocked_tool: Option<String> = if let Some(gid) = &auth.group_id {
        if let Some(tool) = tracked_tool.as_deref() {
            if GROUP_MAIN_WRITE_BLOCKED.contains(&tool) {
                if mcp_tool_mutates_main_regardless_checkout(tool) {
                    if crate::auth::group_main_write_allowed_for_solo_owner(
                        &state,
                        gid,
                        &auth.user_id,
                    )
                    .await
                    {
                        None
                    } else {
                        Some(tool.to_string())
                    }
                } else if mcp_tool_supports_explicit_branch(tool)
                    && mcp_explicit_non_main_branch(params.as_ref())
                {
                    None
                } else {
                    // ACTOR_USER_ID is already set by the global actor_scope_layer.
                    let maybe_blocked = state.service.user_sql_store(gid).await.ok().map(|sql| {
                        let gid_owned = gid.clone();
                        memoria_storage::ACTOR_USER_ID.scope(auth.user_id.clone(), async move {
                            sql.active_branch_name(&gid_owned).await
                        })
                    });
                    if let Some(fut) = maybe_blocked {
                        if let Ok(branch) = fut.await {
                            let is_solo_owner =
                                crate::auth::group_main_write_allowed_for_solo_owner(
                                    &state,
                                    gid,
                                    &auth.user_id,
                                )
                                .await;
                            if branch == "main" && !is_solo_owner {
                                Some(tool.to_string())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                }
            } else {
                None
            }
        } else {
            None
        }
    } else {
        None
    };

    // JSON-RPC 2.0: a Notification is a *valid* Request without an "id" member.
    // The server MUST NOT reply to Notifications.
    if req.get("id").is_none() {
        // Write guard: per JSON-RPC 2.0 the server MUST NOT reply to Notifications,
        // so we silently drop blocked writes without dispatching.
        if blocked_tool.is_some() {
            report_stats(&track_path, false);
            record_call(204, RpcMeta::err(-32001));
            return StatusCode::NO_CONTENT.into_response();
        }
        let dispatch_result = memoria_mcp::dispatch_http(
            method.clone(),
            params,
            state.service.clone(),
            state.git.clone(),
            scope_id.clone(),
        )
        .await;
        let rpc = match &dispatch_result {
            Ok(result) => result_outcome(&method, result),
            Err(e) => RpcMeta::err(e.code),
        };
        let tool_success = dispatch_result
            .as_ref()
            .is_ok_and(|result| !memoria_mcp::tool_result::is_error(result));
        if tool_success {
            if let Some(mask) = tracked_tool.as_deref().and_then(mcp_tool_dirty_mask) {
                spawn_metrics_dirty_mark(state.clone(), scope_id.clone(), mask);
            }
        }
        // Service health excludes expected input/rejection errors; call logs
        // retain both protocol and tool outcomes.
        report_tool_outcome(tracked_tool.as_deref(), &rpc);
        report_stats(
            &track_path,
            rpc.success && rpc.tool_error_kind != Some("backend"),
        );
        record_call(204, rpc);
        return StatusCode::NO_CONTENT.into_response();
    }

    let id = req["id"].clone();

    // Use the pre-computed write-guard decision (see above).
    if let Some(tool) = &blocked_tool {
        let err_body = Json(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32001,
                "message": format!(
                    "Cannot call '{}' on 'main' in a shared space — \
                     main is read-only in collaboration mode. \
                     Your active branch is currently 'main'. \
                     Call memory_checkout with a branch name \
                     (e.g. {{\"name\": \"my-branch\"}}) to switch \
                     to your own branch, then retry.",
                    tool
                )
            }
        }));
        report_stats(&track_path, false);
        record_call(200, RpcMeta::err(-32001));
        return err_body.into_response();
    }

    // This endpoint returns HTTP 200 for dispatched JSON-RPC responses. Protocol
    // and tool execution outcomes must be tracked separately.
    let (response, rpc, tool_success) = match memoria_mcp::dispatch_http(
        method.clone(),
        params,
        state.service.clone(),
        state.git.clone(),
        scope_id.clone(),
    )
    .await
    {
        Ok(v) => {
            let tool_success = !memoria_mcp::tool_result::is_error(&v);
            let rpc = result_outcome(&method, &v);
            let result = if v.is_null() { json!({}) } else { v };
            (
                Json(json!({"jsonrpc": "2.0", "id": id, "result": result})).into_response(),
                rpc,
                tool_success,
            )
        }
        Err(e) => (
            Json(json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": e.code, "message": e.message}
            }))
            .into_response(),
            RpcMeta::err(e.code),
            false,
        ),
    };

    if tool_success {
        if let Some(mask) = tracked_tool.as_deref().and_then(mcp_tool_dirty_mask) {
            spawn_metrics_dirty_mark(state.clone(), scope_id.clone(), mask);
        }
    }

    // Service health excludes input/rejection errors; the per-tool call log
    // retains every execution failure and its classification separately.
    report_tool_outcome(tracked_tool.as_deref(), &rpc);
    report_stats(
        &track_path,
        rpc.success && rpc.tool_error_kind != Some("backend"),
    );
    record_call(200, rpc);

    response
}

#[cfg(test)]
mod tests {
    use super::{mcp_tool_dirty_mask, mcp_tool_required_scope, tracking_path};
    use crate::auth::{SCOPE_MEMORY_READ, SCOPE_MEMORY_WRITE};
    use serde_json::json;

    #[test]
    fn protocol_and_tool_outcomes_remain_separate() {
        use memoria_mcp::tool_result::{classified_error, ErrorKind};
        for kind in [ErrorKind::Input, ErrorKind::Rejected, ErrorKind::Backend] {
            let rpc = super::result_outcome("tools/call", &classified_error(kind, "failure"));
            assert!(rpc.success);
            assert_eq!(rpc.error_code, None);
            assert_eq!(rpc.tool_success, Some(false));
            assert_eq!(rpc.tool_error_kind, Some(kind.as_str()));
        }
        assert_eq!(
            super::result_outcome("tools/call", &json!({"content":[]})).tool_success,
            Some(true)
        );
        assert_eq!(
            super::result_outcome("tools/list", &json!({"tools":[]})).tool_success,
            None
        );
    }

    fn test_state() -> crate::state::AppState {
        let pool = sqlx::mysql::MySqlPoolOptions::new()
            .connect_lazy("mysql://test:test@127.0.0.1/test")
            .unwrap();
        let store = std::sync::Arc::new(memoria_storage::SqlMemoryStore::new(
            pool.clone(),
            3,
            "test".into(),
        ));
        let service = std::sync::Arc::new(memoria_service::MemoryService::new(store, None, None));
        let git = std::sync::Arc::new(memoria_git::GitForDataService::new(pool, "test"));
        crate::state::AppState::new(service, git, "test-master".into())
    }

    fn test_auth(scopes: Vec<String>) -> crate::auth::AuthUser {
        crate::auth::AuthUser {
            user_id: "test".into(),
            scope_id: "test".into(),
            group_id: None,
            is_master: false,
            key_id: None,
            key_prefix: None,
            scopes,
        }
    }

    #[tokio::test]
    async fn control_notifications_have_no_body_or_call_log() {
        use axum::response::IntoResponse;
        let state = test_state();
        for method in [
            "notifications/initialized",
            "notifications/cancelled",
            "notifications/roots/list_changed",
            "notifications/trae/session_stop",
        ] {
            let response = super::mcp_handler(
                axum::extract::State(state.clone()),
                test_auth(vec![crate::auth::SCOPE_MEMORY_READ.into()]),
                Default::default(),
                json!({"jsonrpc":"2.0", "method":method, "params":{"requestId":42}}).to_string(),
            )
            .await
            .into_response();
            assert_eq!(response.status(), 204);
            assert!(axum::body::to_bytes(response.into_body(), 1024)
                .await
                .unwrap()
                .is_empty());
        }
        assert!(state.call_log_batcher.pending_rpc_outcomes().is_empty());
        // An id-less tool call is not a control notification and cannot bypass scopes.
        use tracing::instrument::WithSubscriber;
        #[derive(Clone)]
        struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Capture {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let capture = Capture(Default::default());
        let writer = capture.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        let response = super::mcp_handler(
            axum::extract::State(state.clone()),
            test_auth(vec![]),
            Default::default(),
            json!({"jsonrpc":"2.0", "method":"tools/call",
                "params":{"name":"memory_store", "arguments":{"content":"denied"}}})
            .to_string(),
        )
        .with_subscriber(subscriber)
        .await
        .into_response();
        assert_eq!(response.status(), 204);
        assert!(axum::body::to_bytes(response.into_body(), 1024)
            .await
            .unwrap()
            .is_empty());
        let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
        assert!(logs.contains("MCP scope admission denied"), "{logs}");
        assert!(logs.contains("/mcp/memory_store"), "{logs}");
        assert!(logs.contains("-32003"), "{logs}");
        assert!(state.call_log_batcher.pending_rpc_outcomes().is_empty());
    }

    #[tokio::test]
    async fn notification_methods_with_ids_are_not_successful_calls() {
        use axum::response::IntoResponse;
        let state = test_state();
        for id in [json!(7), json!("request"), serde_json::Value::Null] {
            let response = super::mcp_handler(
                axum::extract::State(state.clone()),
                test_auth(vec![SCOPE_MEMORY_READ.into()]),
                Default::default(),
                json!({"jsonrpc":"2.0","id":id,"method":"notifications/cancelled"}).to_string(),
            )
            .await;
            let body = axum::body::to_bytes(response.into_response().into_body(), 4096)
                .await
                .unwrap();
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(body["id"], id);
            assert_eq!(body["error"]["code"], -32601);
            assert!(body.get("result").is_none());
        }
        assert_eq!(
            state.call_log_batcher.pending_rpc_outcomes(),
            vec![(false, Some(-32601)); 3]
        );
    }

    #[tokio::test]
    async fn http_tool_errors_preserve_protocol_success_and_scope_denials() {
        use axum::response::IntoResponse;
        let state = test_state();
        for (params, scopes, expected) in [
            (
                json!({"name":"memory_store","arguments":{"content":""}}),
                vec![crate::auth::SCOPE_MEMORY_WRITE.into()],
                None,
            ),
            (
                json!({"name":"memory_future_tool"}),
                vec![crate::auth::SCOPE_MEMORY_WRITE.into()],
                Some(-32602),
            ),
            (
                json!({"name":"memory_store","arguments":[]}),
                vec![crate::auth::SCOPE_MEMORY_WRITE.into()],
                Some(-32602),
            ),
            (
                json!({"name":"memory_store","arguments":{"content":"denied"}}),
                vec![],
                Some(-32003),
            ),
        ] {
            let response = super::mcp_handler(
                axum::extract::State(state.clone()),
                test_auth(scopes),
                Default::default(),
                json!({"jsonrpc":"2.0","id":"request","method":"tools/call","params":params})
                    .to_string(),
            )
            .await
            .into_response();
            assert_eq!(response.status(), 200);
            let body: serde_json::Value = serde_json::from_slice(
                &axum::body::to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(body["id"], "request");
            if let Some(code) = expected {
                assert_eq!(body["error"]["code"], code, "{body}");
                assert!(body.get("result").is_none());
            } else {
                assert_eq!(body["result"]["isError"], true, "{body}");
                assert!(body.get("error").is_none());
            }
        }
        // Rejected protocol envelopes and scope denials do not provision per-user
        // storage. The execution error is logged as a successful RPC exchange.
        assert_eq!(
            state.call_log_batcher.pending_rpc_outcomes(),
            vec![(true, None)]
        );
    }

    // ── tools/call — happy path ───────────────────────────────────────────────

    #[test]
    fn tools_call_known_tool() {
        let params = json!({"name": "memory_store", "arguments": {}});
        assert_eq!(
            tracking_path("tools/call", Some(&params)),
            "/mcp/memory_store"
        );
    }

    #[test]
    fn tools_call_representative_names() {
        // Spot-checks a sample of valid tool names to verify the happy-path formatting.
        // This is NOT an exhaustive registry check — tool names are defined in
        // memoria-mcp and may change independently of this list.
        for name in &["memory_store", "memory_search", "memory_purge"] {
            let params = json!({"name": name});
            assert_eq!(
                tracking_path("tools/call", Some(&params)),
                format!("/mcp/{name}"),
                "unexpected path for tool {name}"
            );
        }
    }

    #[test]
    fn mutating_tool_classification_matches_write_tools() {
        assert!(mcp_tool_dirty_mask("memory_store").is_some());
        assert!(mcp_tool_dirty_mask("memory_snapshot").is_some());
        assert!(mcp_tool_dirty_mask("memory_merge").is_some());
        assert!(mcp_tool_dirty_mask("memory_search").is_none());
        assert!(mcp_tool_dirty_mask("memory_branches").is_none());
    }

    #[test]
    fn memory_tools_require_matching_scopes() {
        assert_eq!(
            mcp_tool_required_scope("memory_search"),
            Some(SCOPE_MEMORY_READ)
        );
        assert_eq!(
            mcp_tool_required_scope("memory_store"),
            Some(SCOPE_MEMORY_WRITE)
        );
        assert_eq!(
            mcp_tool_required_scope("memory_checkout"),
            Some(SCOPE_MEMORY_WRITE)
        );
        for name in ["memory_apply", "memory_rebuild_index", "memory_tune_params"] {
            assert_eq!(mcp_tool_required_scope(name), Some(SCOPE_MEMORY_WRITE));
        }
        for name in ["", "memory_future_tool", "memory_search/", " memory_search"] {
            assert_eq!(mcp_tool_required_scope(name), None);
        }
    }

    #[test]
    fn all_advertised_tools_have_explicit_authorization() {
        let mut tools = memoria_mcp::tools::list().as_array().unwrap().clone();
        tools.extend(memoria_mcp::git_tools::list().as_array().unwrap().clone());
        for tool in tools {
            let name = tool["name"].as_str().unwrap();
            assert!(
                mcp_tool_required_scope(name).is_some(),
                "unclassified advertised tool: {name}"
            );
        }
    }

    // ── tools/call — missing / malformed name ─────────────────────────────────

    #[test]
    fn tools_call_missing_name_falls_back() {
        let params = json!({});
        assert_eq!(
            tracking_path("tools/call", Some(&params)),
            "/mcp/tools.call"
        );
    }

    #[test]
    fn tools_call_no_params_falls_back() {
        assert_eq!(tracking_path("tools/call", None), "/mcp/tools.call");
    }

    #[test]
    fn tools_call_empty_name_falls_back() {
        let params = json!({"name": ""});
        assert_eq!(
            tracking_path("tools/call", Some(&params)),
            "/mcp/tools.call"
        );
    }

    // ── tools/call — sanitization ─────────────────────────────────────────────

    #[test]
    fn tools_call_name_with_slash_is_stripped() {
        // A malicious or buggy client sends a name containing path separators.
        let params = json!({"name": "../../etc/passwd"});
        let path = tracking_path("tools/call", Some(&params));
        // The tool segment (everything after "/mcp/") must contain no '/' or '.'.
        let segment = path
            .strip_prefix("/mcp/")
            .expect("path must start with /mcp/");
        assert!(
            !segment.contains('/'),
            "slash must not appear in tool segment: {segment}"
        );
        assert!(
            !segment.contains('.'),
            "dot must not appear in tool segment: {segment}"
        );
        assert!(
            !segment.contains(".."),
            "path traversal must not survive: {segment}"
        );
        // Only alphanumeric + '_' survive → "etcpasswd"
        assert_eq!(path, "/mcp/etcpasswd");
    }

    #[test]
    fn tools_call_name_only_special_chars_falls_back() {
        let params = json!({"name": "/../"});
        assert_eq!(
            tracking_path("tools/call", Some(&params)),
            "/mcp/tools.call"
        );
    }

    #[test]
    fn tools_call_name_clamped_to_64_chars() {
        let long_name = "a".repeat(200);
        let params = json!({"name": long_name});
        let path = tracking_path("tools/call", Some(&params));
        // "/mcp/" is 5 chars; the rest must be ≤ 64.
        let segment = path.strip_prefix("/mcp/").unwrap();
        assert!(
            segment.len() <= 64,
            "segment length {} exceeds 64",
            segment.len()
        );
    }

    // ── non-tools/call methods ────────────────────────────────────────────────

    #[test]
    fn initialize_method() {
        assert_eq!(tracking_path("initialize", None), "/mcp/initialize");
    }

    #[test]
    fn tools_list_method() {
        // '/' in the method name is replaced with '.'
        assert_eq!(tracking_path("tools/list", None), "/mcp/tools.list");
    }

    #[test]
    fn unknown_method_sanitized() {
        assert_eq!(
            tracking_path("notifications/initialized", None),
            "/mcp/notifications.initialized"
        );
    }

    #[test]
    fn empty_method_falls_back_to_unknown() {
        assert_eq!(tracking_path("", None), "/mcp/unknown");
    }

    #[test]
    fn method_with_only_special_chars_falls_back_to_unknown() {
        assert_eq!(tracking_path("///", None), "/mcp/unknown");
    }
}
