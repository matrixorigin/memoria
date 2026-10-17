use crate::{git_tools, remote::RemoteClient, tools};
use anyhow::Result;
use memoria_git::GitForDataService;
use memoria_service::{shutdown_signal, MemoryService};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Structured JSON-RPC error returned by [`dispatch`] and [`dispatch_http`].
/// Carries the standard error code so callers can forward it verbatim.
#[derive(Debug)]
pub struct McpRpcError {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for McpRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for McpRpcError {}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    #[allow(dead_code)]
    jsonrpc: String,
    #[serde(default, deserialize_with = "present_id")]
    id: Option<Value>,
    method: String,
    #[serde(default)]
    params: Option<Value>,
}

// Preserve explicit null as a present id; only an absent member is a notification.
fn present_id<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

#[derive(Serialize)]
struct Response {
    jsonrpc: &'static str,
    id: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<Value>,
}

enum RpcMethod {
    Initialize,
    Ping,
    ToolsList,
    ToolsCall,
    Unknown(String),
}

fn parse_rpc_method(method: &str) -> RpcMethod {
    match method {
        "initialize" => RpcMethod::Initialize,
        "ping" => RpcMethod::Ping,
        "tools/list" => RpcMethod::ToolsList,
        "tools/call" => RpcMethod::ToolsCall,
        _ => RpcMethod::Unknown(method.to_string()),
    }
}

/// Accept control notifications without invoking tools or storage.
pub fn accept_notification(method: &str, id: Option<&Value>) -> bool {
    if id.is_some() || !method.starts_with("notifications/") {
        return false;
    }
    if method == "notifications/cancelled" {
        // Intentionally a no-op: execution tracking and cooperative cancellation
        // are follow-up #256. In particular, aborting the HTTP waiter would not
        // stop its spawn_blocking worker, and stdio currently dispatches serially.
        tracing::debug!(
            "MCP cancellation accepted; execution cancellation is not implemented (#256)"
        );
    } else {
        // Roots are not used by this server. Unknown vendor notifications are
        // ignored as well; accepting them does not advertise their capability.
        tracing::debug!(method, "MCP notification accepted");
    }
    true
}

const GIT_TOOL_NAMES: &[&str] = &[
    "memory_snapshot",
    "memory_snapshots",
    "memory_snapshot_delete",
    "memory_rollback",
    "memory_branch",
    "memory_branches",
    "memory_checkout",
    "memory_merge",
    "memory_pick",
    "memory_diff",
    "memory_apply",
    "memory_branch_delete",
];

fn is_git_tool(name: &str) -> bool {
    GIT_TOOL_NAMES.contains(&name)
}

/// Validate the protocol envelope before authorization or tool execution.
/// Tool-specific input validation remains an execution error visible to the model.
pub fn validate_tool_call(params: Option<&Value>) -> Result<(String, Value), McpRpcError> {
    let invalid = |message: String| McpRpcError {
        code: -32602,
        message,
    };
    let p = params
        .and_then(Value::as_object)
        .ok_or_else(|| invalid("tools/call params must be an object".into()))?;
    let name = p
        .get("name")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| invalid("tools/call name must be a non-empty string".into()))?;
    if !is_git_tool(name) && !tools::is_known_tool(name) {
        return Err(invalid(format!("Unknown tool: {name}")));
    }
    let args = match p.get("arguments") {
        None | Some(Value::Null) => json!({}),
        Some(value) if value.is_object() => value.clone(),
        Some(_) => return Err(invalid("tools/call arguments must be an object".into())),
    };
    Ok((name.to_string(), args))
}

/// Dispatch a single JSON-RPC method in embedded mode.
/// Used by the server-side Streamable HTTP MCP endpoint.
pub async fn dispatch_http(
    method: String,
    params: Option<Value>,
    service: Arc<MemoryService>,
    git: Arc<GitForDataService>,
    user_id: String,
) -> Result<Value, McpRpcError> {
    let handle = tokio::runtime::Handle::current();

    // `spawn_blocking` runs on a separate thread-pool thread and does NOT inherit
    // Tokio task-local variables.  In group/space mode the `actor_scope_layer`
    // middleware sets `ACTOR_USER_ID` to the real human user_id so that per-user
    // state (active branch) is keyed on the individual rather than the group scope.
    // Without propagating it here, MCP tool calls inside `spawn_blocking` fall back
    // to `scope_id` (the group id), causing a mismatch: the Dashboard REST checkout
    // writes `mem_user_state.user_id = real_user_id` while the MCP code-agent reads
    // `mem_user_state.user_id = grp_xxx` — two different rows, always out of sync.
    let actor_user_id = memoria_storage::ACTOR_USER_ID
        .try_with(|id| id.clone())
        .ok();

    tokio::task::spawn_blocking(move || {
        let fut = dispatch_embedded_owned(method, params, service, git, user_id);
        // Restore ACTOR_USER_ID inside the blocking thread so storage methods
        // (`active_branch_name`, `set_active_branch`, `active_table`) see the
        // same per-user scope they would in the original async task.
        match actor_user_id {
            Some(actor_id) => handle.block_on(memoria_storage::ACTOR_USER_ID.scope(actor_id, fut)),
            None => handle.block_on(fut),
        }
    })
    .await
    .map_err(|e| McpRpcError {
        code: -32000,
        message: e.to_string(),
    })?
}

/// Run in embedded mode (direct DB).
pub async fn run_stdio(
    service: Arc<MemoryService>,
    git: Arc<GitForDataService>,
    user_id: String,
) -> Result<()> {
    run_loop(Mode::Embedded { service, git }, user_id).await
}

/// Run in remote mode (proxy to REST API).
pub async fn run_stdio_remote(remote: RemoteClient, user_id: String) -> Result<()> {
    run_loop(Mode::Remote(remote), user_id).await
}

/// Run SSE transport — MCP over HTTP.
/// Clients connect to GET /sse for server-sent events, POST /message to send requests.
pub async fn run_sse(
    service: Arc<MemoryService>,
    git: Arc<GitForDataService>,
    user_id: String,
    port: u16,
) -> Result<()> {
    let app = sse_router(service, git, user_id);
    let addr = format!("0.0.0.0:{port}");
    tracing::info!("Memoria MCP SSE transport listening on {addr}");
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

fn sse_router(
    service: Arc<MemoryService>,
    git: Arc<GitForDataService>,
    user_id: String,
) -> axum::Router {
    use axum::{
        extract::State,
        response::sse::{Event, Sse},
        routing::{get, post},
        Router,
    };
    use futures::stream::{self};
    use std::convert::Infallible;
    use tokio::sync::broadcast;

    #[derive(Clone)]
    struct SseState {
        tx: broadcast::Sender<String>,
        service: Arc<MemoryService>,
        git: Arc<GitForDataService>,
        user_id: String,
    }

    let (tx, _) = broadcast::channel::<String>(64);
    let state = SseState {
        tx: tx.clone(),
        service,
        git,
        user_id,
    };

    Router::new()
        .route("/sse", get(|State(s): State<SseState>| async move {
            let rx = s.tx.subscribe();
            let stream = stream::unfold(rx, |mut rx| async move {
                match rx.recv().await {
                    Ok(msg) => Some((Ok::<Event, Infallible>(Event::default().data(msg)), rx)),
                    Err(_) => None,
                }
            });
            Sse::new(stream)
        }))
        .route("/message", post(|State(s): State<SseState>, body: String| async move {
            let req: serde_json::Value = match serde_json::from_str(&body) {
                Ok(v) => v,
                Err(e) => {
                    let resp = serde_json::json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}});
                    let _ = s.tx.send(serde_json::to_string(&resp).unwrap_or_default());
                    return;
                }
            };
            let id = req["id"].clone();
            let method = req["method"].as_str().unwrap_or("").to_string();
            // Only a valid request without an id is a notification. Malformed
            // payloads still need an Invalid Request response, even without id.
            // Preserve the legacy endpoint's jsonrpc-field compatibility here;
            // tightening envelope validation is separate from notification handling.
            if !req.is_object() || method.is_empty() {
                let response_id = req.get("id").filter(|id| id.is_string() || id.is_number()).cloned().unwrap_or(Value::Null);
                let resp = serde_json::json!({"jsonrpc":"2.0","id":response_id,
                    "error":{"code":-32600,"message":"Invalid Request"}});
                let _ = s.tx.send(serde_json::to_string(&resp).unwrap_or_default());
                return;
            }
            if accept_notification(&method, req.get("id")) {
                return;
            }
            let params = req["params"].clone();
            let result = dispatch_http(
                method,
                Some(params),
                s.service.clone(),
                s.git.clone(),
                s.user_id.clone(),
            )
            .await;
            if req.get("id").is_none() {
                return;
            }
            let resp = match result {
                Ok(v) => serde_json::json!({"jsonrpc":"2.0","id":id,"result":if v.is_null(){serde_json::json!({})}else{v}}),
                Err(e) => serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":e.code,"message":e.message}}),
            };
            let _ = s.tx.send(serde_json::to_string(&resp).unwrap_or_default());
        }))
        .with_state(state)
}

enum Mode {
    Embedded {
        service: Arc<MemoryService>,
        git: Arc<GitForDataService>,
    },
    Remote(RemoteClient),
}

async fn run_loop(mode: Mode, user_id: String) -> Result<()> {
    run_io(tokio::io::stdin(), tokio::io::stdout(), mode, user_id).await
}

async fn run_io(
    stdin: impl tokio::io::AsyncRead + Unpin,
    mut stdout: impl tokio::io::AsyncWrite + Unpin,
    mode: Mode,
    user_id: String,
) -> Result<()> {
    let mut reader = BufReader::new(stdin).lines();

    loop {
        let line = tokio::select! {
            result = reader.next_line() => {
                match result? {
                    Some(l) => l,
                    None => break, // EOF
                }
            }
            _ = shutdown_signal() => break,
        };
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("PARSE_ERROR: {e} | RAW: {line}");
                let resp = json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":e.to_string()}});
                write_line(&mut stdout, &resp).await?;
                continue;
            }
        };

        let id = req.id.clone().unwrap_or(Value::Null);

        if accept_notification(&req.method, req.id.as_ref()) {
            continue;
        }
        if req.id.is_none() {
            let _ = dispatch(&req.method, req.params, &mode, &user_id).await;
            continue;
        }

        let result = dispatch(&req.method, req.params, &mode, &user_id).await;

        let resp = match result {
            Ok(v) => {
                let result_val = if v.is_null() { json!({}) } else { v };
                Response {
                    jsonrpc: "2.0",
                    id,
                    result: Some(result_val),
                    error: None,
                }
            }
            Err(e) => Response {
                jsonrpc: "2.0",
                id,
                result: None,
                error: Some(json!({"code": e.code, "message": e.message})),
            },
        };
        write_line(&mut stdout, &resp).await?;
    }
    Ok(())
}

async fn write_line(
    stdout: &mut (impl tokio::io::AsyncWrite + Unpin),
    v: &impl Serialize,
) -> Result<()> {
    let mut line = serde_json::to_string(v)?;
    line.push('\n');
    stdout.write_all(line.as_bytes()).await?;
    stdout.flush().await?;
    Ok(())
}

async fn dispatch(
    method: &str,
    params: Option<Value>,
    mode: &Mode,
    user_id: &str,
) -> Result<Value, McpRpcError> {
    let p = params.unwrap_or(Value::Null);
    let method = parse_rpc_method(method);
    match method {
        RpcMethod::Initialize => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "memoria-mcp-rs", "version": "0.1.0"}
        })),
        RpcMethod::Ping => Ok(json!({})),
        RpcMethod::ToolsList => {
            Ok(tools::tool_catalog())
        }
        RpcMethod::ToolsCall => {
            let (name, args) = validate_tool_call(Some(&p))?;
            match mode {
                Mode::Remote(client) => Ok(client
                    .call(&name, args)
                    .await
                    .unwrap_or_else(|error| crate::tool_result::execution_error(&name, error))),
                Mode::Embedded { service, git } => {
                    if is_git_tool(&name) {
                        Ok(git_tools::call(&name, args, git, service, user_id)
                            .await
                            .unwrap_or_else(|error| {
                                crate::tool_result::execution_error(&name, error)
                            }))
                    } else {
                        Ok(tools::call(&name, args, service, user_id)
                            .await
                            .unwrap_or_else(|error| {
                                crate::tool_result::execution_error(&name, error)
                            }))
                    }
                }
            }
        }
        RpcMethod::Unknown(method) => Err(McpRpcError {
            code: -32601,
            message: format!("Method not found: {method}"),
        }),
    }
}

async fn dispatch_embedded_owned(
    method: String,
    params: Option<Value>,
    service: Arc<MemoryService>,
    git: Arc<GitForDataService>,
    user_id: String,
) -> Result<Value, McpRpcError> {
    let p = params.unwrap_or(Value::Null);
    let method = parse_rpc_method(&method);
    match method {
        RpcMethod::Initialize => Ok(json!({
            "protocolVersion": "2024-11-05",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "memoria-mcp-rs", "version": "0.1.0"}
        })),
        RpcMethod::Ping => Ok(json!({})),
        RpcMethod::ToolsList => {
            Ok(tools::tool_catalog())
        }
        RpcMethod::ToolsCall => {
            let (name, args) = validate_tool_call(Some(&p))?;
            if is_git_tool(&name) {
                Ok(
                    git_tools::call_owned(name.clone(), args, git, service, user_id)
                        .await
                        .unwrap_or_else(|error| crate::tool_result::execution_error(&name, error)),
                )
            } else {
                Ok(tools::call_owned(name.clone(), args, service, user_id)
                    .await
                    .unwrap_or_else(|error| crate::tool_result::execution_error(&name, error)))
            }
        }
        RpcMethod::Unknown(method) => Err(McpRpcError {
            code: -32601,
            message: format!("Method not found: {method}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        dispatch, is_git_tool, parse_rpc_method, Mode, RemoteClient, RpcMethod, GIT_TOOL_NAMES,
    };
    use serde_json::json;

    #[test]
    fn ping_is_a_known_rpc_method() {
        assert!(matches!(parse_rpc_method("ping"), RpcMethod::Ping));
    }

    #[test]
    fn null_arguments_preserve_legacy_compatibility() {
        for name in [
            "memory_capabilities",
            "memory_branches",
            "memory_tune_params",
            "memory_get_retrieval_params",
        ] {
            for params in [json!({"name":name}), json!({"name":name,"arguments":null})] {
                assert_eq!(
                    super::validate_tool_call(Some(&params)).unwrap().1,
                    json!({})
                );
            }
        }
        for args in [json!([]), json!("invalid"), json!(42)] {
            assert_eq!(
                super::validate_tool_call(Some(
                    &json!({"name":"memory_capabilities", "arguments":args})
                ))
                .unwrap_err()
                .code,
                -32602
            );
        }
    }

    #[tokio::test]
    async fn stdio_notifications_are_silent_and_do_not_break_following_requests() {
        let mode = Mode::Remote(RemoteClient::new(
            "http://127.0.0.1:1",
            None,
            "test".into(),
            None,
        ));
        let input = concat!(
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":42}}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/roots/list_changed\"}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"notifications/trae/session_stop\"}\n",
            "{\"jsonrpc\":\"2.0\",\"method\":\"ping\",\"id\":\"after\"}\n"
        );
        let mut output = Vec::new();
        super::run_io(input.as_bytes(), &mut output, mode, "test".into())
            .await
            .unwrap();
        let messages: Vec<serde_json::Value> = String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(
            messages,
            vec![json!({"jsonrpc":"2.0", "id":"after", "result":{}})]
        );
    }

    #[tokio::test]
    async fn control_notifications_are_accepted_only_at_request_boundaries() {
        let mode = Mode::Remote(RemoteClient::new(
            "http://127.0.0.1:1",
            None,
            "test".into(),
            None,
        ));
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
        for method in [
            "notifications/initialized",
            "notifications/cancelled",
            "notifications/roots/list_changed",
            "notifications/trae/session_stop",
        ] {
            assert!(super::accept_notification(method, None));
            assert!(!super::accept_notification(method, Some(&json!(1))));
            assert_eq!(
                dispatch(method, None, &mode, "test")
                    .await
                    .unwrap_err()
                    .code,
                -32601
            );
            assert_eq!(
                super::dispatch_http(
                    method.into(),
                    None,
                    service.clone(),
                    git.clone(),
                    "test".into()
                )
                .await
                .unwrap_err()
                .code,
                -32601
            );
        }
        assert_eq!(
            dispatch("resources/list", None, &mode, "test")
                .await
                .unwrap_err()
                .code,
            -32601
        );
        assert!(!super::accept_notification("tools/call", None));
    }

    #[tokio::test]
    async fn stdio_notification_methods_with_ids_are_requests() {
        let mode = Mode::Remote(RemoteClient::new(
            "http://127.0.0.1:1",
            None,
            "test".into(),
            None,
        ));
        let mut input = String::new();
        for id in [json!(7), json!("request"), serde_json::Value::Null] {
            input.push_str(
                &json!({"jsonrpc":"2.0","id":id,"method":"notifications/cancelled"}).to_string(),
            );
            input.push('\n');
        }
        let mut output = Vec::new();
        super::run_io(input.as_bytes(), &mut output, mode, "test".into())
            .await
            .unwrap();
        let output = String::from_utf8(output).unwrap();
        let messages: Vec<serde_json::Value> = output
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(messages.len(), 3);
        for (message, id) in
            messages
                .iter()
                .zip([json!(7), json!("request"), serde_json::Value::Null])
        {
            assert_eq!(message["id"], id);
            assert_eq!(message["error"]["code"], -32601);
        }
    }

    #[tokio::test]
    async fn sse_notifications_do_not_emit_json_rpc_responses() {
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
        let app = super::sse_router(service, git, "test".into());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let client = reqwest::Client::new();
        let mut stream = client
            .get(format!("http://{address}/sse"))
            .send()
            .await
            .unwrap();
        for method in [
            "notifications/cancelled",
            "notifications/roots/list_changed",
            "notifications/trae/session_stop",
        ] {
            client
                .post(format!("http://{address}/message"))
                .json(&json!({"jsonrpc":"2.0","method":method}))
                .send()
                .await
                .unwrap();
        }
        client
            .post(format!("http://{address}/message"))
            .json(&json!({"jsonrpc":"2.0","method":"ping","id":"after"}))
            .send()
            .await
            .unwrap();
        let chunk = tokio::time::timeout(std::time::Duration::from_secs(2), stream.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let text = String::from_utf8(chunk.to_vec()).unwrap();
        assert!(text.contains("\"id\":\"after\""), "{text}");
        assert!(!text.contains("\"id\":null"), "{text}");
        client
            .post(format!("http://{address}/message"))
            .json(&json!([]))
            .send()
            .await
            .unwrap();
        let invalid = tokio::time::timeout(std::time::Duration::from_secs(2), stream.chunk())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(String::from_utf8(invalid.to_vec())
            .unwrap()
            .contains("-32600"));
        for (request, code) in [
            (json!({"id":7,"method":42}), Some(-32600)),
            (json!({"id":"legacy","method":"tools/list"}), None),
            (
                json!({"jsonrpc":"2.0","id":9,"method":"notifications/cancelled"}),
                Some(-32601),
            ),
        ] {
            client
                .post(format!("http://{address}/message"))
                .json(&request)
                .send()
                .await
                .unwrap();
            // A large tools/list event can span multiple HTTP chunks.
            let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                let mut bytes = Vec::new();
                while !bytes.windows(2).any(|window| window == b"\n\n") {
                    bytes.extend_from_slice(&stream.chunk().await.unwrap().unwrap());
                }
                bytes
            })
            .await
            .unwrap();
            let text = String::from_utf8(bytes).unwrap();
            let data = text
                .lines()
                .find_map(|line| line.strip_prefix("data: "))
                .unwrap();
            let response: serde_json::Value = serde_json::from_str(data).unwrap();
            assert_eq!(response["id"], request["id"]);
            if let Some(code) = code {
                assert_eq!(response["error"]["code"], code);
            } else {
                assert!(response.get("result").is_some());
            }
        }
        server.abort();
    }

    #[tokio::test]
    async fn tool_errors_are_results_in_both_dispatch_paths() {
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
        let embedded = Mode::Embedded {
            service: service.clone(),
            git: git.clone(),
        };
        let remote = Mode::Remote(RemoteClient::new(
            "http://127.0.0.1:1",
            None,
            "test".into(),
            None,
        ));
        for params in [
            json!({"name":"memory_store", "arguments":{"content":""}}),
            json!({"name":"memory_store", "arguments":{"content":"a valid fact", "memory_type":"invalid_xyz"}}),
            json!({"name":"memory_store", "arguments":{"content":"a valid fact", "memory_type":42}}),
            json!({"name":"memory_apply", "arguments":{"source":"main"}}),
            json!({"name":"memory_apply", "arguments":{"source":42}}),
        ] {
            for result in [
                dispatch("tools/call", Some(params.clone()), &embedded, "test").await,
                dispatch("tools/call", Some(params.clone()), &remote, "test").await,
                super::dispatch_http(
                    "tools/call".into(),
                    Some(params.clone()),
                    service.clone(),
                    git.clone(),
                    "test".into(),
                )
                .await,
            ] {
                let result = result.expect("execution errors must be results");
                assert_eq!(result["isError"], true, "{result}");
                assert!(!result["content"][0]["text"].as_str().unwrap().is_empty());
                if params["arguments"].get("memory_type").is_some() {
                    assert!(result["content"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains("memory_type"));
                }
                if params["arguments"]["source"] == 42 {
                    assert!(
                        result["content"][0]["text"]
                            .as_str()
                            .unwrap()
                            .contains("source"),
                        "{result}"
                    );
                }
            }
        }
        for params in [
            json!({"name":"unknown"}),
            json!({"name":42}),
            json!({"name":"memory_store", "arguments":[]}),
        ] {
            assert_eq!(
                dispatch("tools/call", Some(params.clone()), &remote, "test")
                    .await
                    .unwrap_err()
                    .code,
                -32602
            );
            assert_eq!(
                super::dispatch_http(
                    "tools/call".into(),
                    Some(params),
                    service.clone(),
                    git.clone(),
                    "test".into()
                )
                .await
                .unwrap_err()
                .code,
                -32602
            );
        }
        let success = dispatch(
            "tools/call",
            Some(json!({"name":"memory_capabilities"})),
            &embedded,
            "test",
        )
        .await
        .unwrap();
        assert!(!crate::tool_result::is_error(&success));
    }

    /// Oversized memory_store content (service-layer MemoriaError::Validation,
    /// not an argument-parsing error) must surface as an isError tool result on
    /// both embedded dispatch paths — not a JSON-RPC protocol error. Remote mode
    /// is intentionally excluded here: validate_tool_args doesn't pre-check
    /// content length, so it would make a real (slow, unreachable-address) network
    /// call instead of failing fast like the embedded paths do.
    #[tokio::test]
    async fn oversized_store_content_is_a_tool_error_on_embedded_paths() {
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
        let embedded = Mode::Embedded {
            service: service.clone(),
            git: git.clone(),
        };
        let params = json!({"name":"memory_store", "arguments":{"content":"x".repeat(32_769)}});

        let via_dispatch = dispatch("tools/call", Some(params.clone()), &embedded, "test")
            .await
            .expect("execution errors must be results");
        assert_eq!(via_dispatch["isError"], true, "{via_dispatch}");
        assert!(!via_dispatch["content"][0]["text"]
            .as_str()
            .unwrap()
            .is_empty());

        let via_http = super::dispatch_http(
            "tools/call".into(),
            Some(params),
            service,
            git,
            "test".into(),
        )
        .await
        .expect("execution errors must be results");
        assert_eq!(via_http["isError"], true, "{via_http}");
    }

    #[tokio::test]
    async fn downstream_failure_is_a_tool_error_without_url_leakage() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mode = Mode::Remote(RemoteClient::new(
            &format!("http://{address}"),
            None,
            "test".into(),
            None,
        ));
        let result = dispatch(
            "tools/call",
            Some(json!({"name":"memory_store", "arguments":{"content":"hello"}})),
            &mode,
            "test",
        )
        .await
        .unwrap();
        assert_eq!(result["isError"], true);
        assert!(!result.to_string().contains(&address.to_string()));
    }

    #[tokio::test]
    async fn ping_returns_an_empty_result_without_calling_the_backend() {
        let mode = Mode::Remote(RemoteClient::new(
            "http://127.0.0.1:1",
            None,
            "test-user".to_string(),
            None,
        ));

        let result = dispatch("ping", None, &mode, "test-user")
            .await
            .expect("ping should succeed");

        assert_eq!(result, json!({}));
    }

    #[test]
    fn git_dispatch_list_includes_memory_apply() {
        assert!(is_git_tool("memory_apply"));
    }

    #[test]
    fn git_tool_dispatch_matches_declared_git_tools() {
        let declared_names: Vec<String> = crate::git_tools::list()
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|tool| tool.get("name").and_then(|name| name.as_str()))
            .map(str::to_string)
            .collect();

        for name in GIT_TOOL_NAMES {
            assert!(
                declared_names.iter().any(|declared| declared == name),
                "dispatch marked '{name}' as a git tool but git_tools::list() does not declare it"
            );
        }

        for name in declared_names {
            assert!(
                is_git_tool(&name),
                "git_tools::list() declares '{name}' but server dispatch does not route it"
            );
        }
    }
}
