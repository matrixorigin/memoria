//! Policy admission and production REST/MCP routing without a database.
use std::sync::Arc;

use axum::{extract::State, middleware, routing::post, Json, Router};
use memoria_api::{auth::AuthUser, state::CachedApiKeyPrincipal, AppState};
use memoria_service::{content_filter::check_content_sensitivity, MemoryService};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const MASTER: &str = "synthetic-owner-policy-test-master";
const OWNER: &str = "synthetic-owner";
const TEXT: &str = "The standard bearer carried the flag. Contact person@example.com.";

async fn probe(auth: AuthUser, State(_state): State<AppState>) -> Json<Value> {
    let result = check_content_sensitivity(auth.scope_id(), TEXT);
    Json(json!({
        "blocked": result.blocked,
        "redacted": result.redacted_content.is_some(),
        "other_owner_blocked": check_content_sensitivity("other-owner", TEXT).blocked,
    }))
}

fn test_state(enabled: bool) -> AppState {
    // A lazy local pool avoids DB/network setup for policy admission probes.
    // Production-router probes close it to make storage failures deterministic.
    let pool = sqlx::mysql::MySqlPoolOptions::new()
        .acquire_timeout(std::time::Duration::from_millis(500))
        .connect_lazy("mysql://test:test@127.0.0.1:1/policy_test")
        .unwrap();
    let store = Arc::new(memoria_storage::SqlMemoryStore::new(
        pool.clone(),
        4,
        "policy-test".into(),
    ));
    let service = Arc::new(MemoryService::new(store.clone(), None, Some(store)));
    let git = Arc::new(memoria_git::GitForDataService::new(pool, "policy_test"));
    let mut state = AppState::new(service, git, MASTER.into());
    state.owner_content_filter_enabled = enabled;
    state.api_key_cache.insert(
        format!("{:x}", Sha256::digest("synthetic-personal-api-key")),
        CachedApiKeyPrincipal {
            key_id: "test-key".into(),
            user_id: OWNER.into(),
            group_id: None,
            key_prefix: "test".into(),
            scopes: vec!["memory:write".into()],
            expires_at: None,
        },
    );
    state
}

async fn start(enabled: bool) -> (String, reqwest::Client, tokio::task::JoinHandle<()>) {
    let state = test_state(enabled);
    let app = Router::new()
        .route("/v1/memories", post(probe))
        .route("/v1/memories/idempotent", post(probe))
        .route("/mcp", post(probe))
        .with_state(state.clone())
        .layer(middleware::from_fn_with_state(
            state,
            memoria_api::auth::owner_content_filter_layer,
        ));
    start_router(app).await
}

async fn start_router(app: Router) -> (String, reqwest::Client, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .no_proxy()
        .build()
        .unwrap();
    (base, client, task)
}

#[tokio::test]
async fn only_valid_owner_auth_can_use_server_configured_override() {
    for enabled in [true, false] {
        let (base, client, server) = start(enabled).await;
        for endpoint in ["/v1/memories", "/v1/memories/idempotent", "/mcp"] {
            let response: Value = client
                .post(format!("{base}{endpoint}"))
                .header("Authorization", format!("Memoria-Owner {MASTER}"))
                .header("X-User-Id", OWNER)
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(response["blocked"], enabled || endpoint == "/mcp");
            assert_eq!(response["other_owner_blocked"], true);
        }

        for token in [MASTER, "synthetic-personal-api-key"] {
            let response: Value = client
                .post(format!("{base}/v1/memories?content_filter_enabled=false"))
                .bearer_auth(token)
                .header("X-User-Id", OWNER)
                .header("X-Content-Filter-Enabled", "false")
                .json(&json!({"content_filter_enabled":false}))
                .send()
                .await
                .unwrap()
                .error_for_status()
                .unwrap()
                .json()
                .await
                .unwrap();
            assert_eq!(response["blocked"], true);
        }

        for (token, owner, expected) in [
            ("incorrect-master", Some(OWNER), 401),
            (MASTER, None, 400),
            (MASTER, Some("grp_reserved"), 400),
        ] {
            let mut request = client
                .post(format!("{base}/v1/memories"))
                .header("Authorization", format!("Memoria-Owner {token}"));
            if let Some(owner) = owner {
                request = request.header("X-User-Id", owner);
            }
            assert_eq!(request.send().await.unwrap().status().as_u16(), expected);
        }
        let duplicate_owner = client
            .post(format!("{base}/v1/memories"))
            .header("Authorization", format!("Memoria-Owner {MASTER}"))
            .header("X-User-Id", OWNER)
            .header("X-User-Id", "other-owner")
            .send()
            .await
            .unwrap();
        assert_eq!(duplicate_owner.status(), 400);
        server.abort();
    }
}

#[tokio::test]
async fn production_router_applies_rest_override_but_keeps_remote_mcp_filtered() {
    for enabled in [true, false] {
        let state = test_state(enabled);
        // Deliberately close this local lazy pool: an accepted REST write must
        // reach a deterministic storage error rather than a content rejection.
        state
            .service
            .sql_store
            .as_ref()
            .unwrap()
            .pool()
            .close()
            .await;
        let (base, client, server) = start_router(memoria_api::build_router(state)).await;
        let response = client
            .post(format!("{base}/v1/memories"))
            .header("Authorization", format!("Memoria-Owner {MASTER}"))
            .header("X-User-Id", OWNER)
            .json(&json!({"content": TEXT}))
            .send()
            .await
            .unwrap();
        let status = response.status().as_u16();
        let body = response.text().await.unwrap();
        if enabled {
            assert_eq!(status, 403);
            assert!(body.contains("bearer_token"));
        } else {
            assert_eq!(status, 500);
            assert!(body.contains("closed pool"));
            assert!(!body.contains("Blocked"));
        }

        // The production router must keep ordinary Bearer writes filtered even
        // while the trusted-owner REST override is enabled.
        let response = client
            .post(format!("{base}/v1/memories"))
            .bearer_auth(MASTER)
            .header("X-User-Id", OWNER)
            .json(&json!({"content": TEXT}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 403);
        assert!(response.text().await.unwrap().contains("bearer_token"));

        let response = client
            .post(format!("{base}/mcp"))
            .header("Authorization", format!("Memoria-Owner {MASTER}"))
            .header("X-User-Id", OWNER)
            .json(&json!({
                "jsonrpc":"2.0", "id":1, "method":"tools/call",
                "params":{"name":"memory_store", "arguments":{"content":TEXT}}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert!(body.get("error").is_none());
        let result = body["result"].to_string();
        assert!(result.contains("Memory blocked"));
        assert!(result.contains("bearer_token"));
        server.abort();
        let _ = server.await;
    }
}
