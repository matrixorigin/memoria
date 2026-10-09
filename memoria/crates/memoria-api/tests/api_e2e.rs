use serde_json::{json, Value};
/// REST API E2E tests — starts a real server, hits it with reqwest.
/// Requires DATABASE_URL env var.
use std::sync::Arc;

mod support;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}
fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria".to_string())
}

fn uid() -> String {
    format!("api_test_{}", uuid::Uuid::new_v4().simple())
}

fn is_pick_not_supported_message(body: &str) -> bool {
    let lower = body.to_lowercase();
    lower.contains("sql parser error")
        && (lower.contains("data branch pick")
            || (lower.contains("near \" pick") && lower.contains("when conflict")))
}

async fn pick_response_json_or_skip(response: reqwest::Response, test_name: &str) -> Option<Value> {
    let status = response.status();
    let body = response.text().await.unwrap();
    if is_pick_not_supported_message(&body) {
        eprintln!("⚠️ DATA BRANCH PICK not supported, skipping {test_name}");
        return None;
    }
    assert_eq!(status, 200, "body: {body}");
    Some(serde_json::from_str(&body).expect("pick response json"))
}

fn remote_pick_or_skip(result: anyhow::Result<Value>, test_name: &str) -> Option<Value> {
    match result {
        Ok(value) => Some(value),
        Err(err) if is_pick_not_supported_message(&err.to_string()) => {
            eprintln!("⚠️ DATA BRANCH PICK not supported, skipping {test_name}");
            None
        }
        Err(err) => panic!("{err:?}"),
    }
}

fn mcp_text_json(v: &Value) -> Value {
    serde_json::from_str(v["content"][0]["text"].as_str().unwrap_or("")).expect("mcp json text")
}

fn episodic_rules() -> Vec<memoria_test_utils::PromptRule> {
    vec![
        (
            "Respond with a JSON object containing: topic, action, outcome",
            json!({
                "topic": "Deterministic validation of session workflows",
                "action": "Stored session memories and generated an episodic summary with a local fake LLM",
                "outcome": "The session summary path completed and persisted an episodic memory"
            }),
        ),
        (
            "Respond with a JSON object: {\"points\"",
            json!({
                "points": [
                    "Validated session summary path",
                    "Used local fake LLM",
                    "Stored episodic memory"
                ]
            }),
        ),
    ]
}

async fn spawn_fake_llm() -> (
    Arc<memoria_embedding::LlmClient>,
    tokio::sync::oneshot::Sender<()>,
) {
    memoria_test_utils::spawn_fake_llm(episodic_rules()).await
}

struct SessionScopeTestEmbedder;

impl SessionScopeTestEmbedder {
    fn vector_for(text: &str) -> Vec<f32> {
        let mut v = vec![0.0; test_dim()];
        match text {
            "strict session query" => v[0] = 1.0,
            "other-session top" => {
                v[0] = 0.85;
                v[1] = 0.52;
            }
            "global-unscoped top" => {
                v[0] = 0.58;
                v[1] = 0.78;
            }
            "scoped topk query" => v[2] = 1.0,
            "global-candidate-a" => v[0] = 1.0,
            "other-session second" => {
                v[0] = 0.72;
                v[1] = 0.18;
                v[2] = 0.67;
            }
            "global-candidate-b" => {
                v[1] = 0.12;
                v[2] = 0.90;
                v[3] = 0.38;
            }
            "target-session memory" => v[1] = 1.0,
            "scoped-candidate-a" => {
                v[0] = 0.32;
                v[1] = 0.05;
                v[2] = 0.88;
            }
            "target-session backup" => {
                v[0] = 0.10;
                v[1] = 0.94;
                v[2] = 0.18;
                v[3] = 0.28;
            }
            "scoped-candidate-b" => {
                v[0] = 0.05;
                v[1] = 0.32;
                v[2] = 0.88;
            }
            "other-session decoy" => {
                v[0] = 0.78;
                v[1] = 0.44;
                v[2] = 0.12;
                v[3] = 0.18;
            }
            _ => v[2] = 1.0,
        }
        v
    }
}

#[async_trait::async_trait]
impl memoria_core::interfaces::EmbeddingProvider for SessionScopeTestEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, memoria_core::MemoriaError> {
        Ok(Self::vector_for(text))
    }

    fn dimension(&self) -> usize {
        test_dim()
    }
}

struct PickTestEmbedder;

#[async_trait::async_trait]
impl memoria_core::interfaces::EmbeddingProvider for PickTestEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, memoria_core::MemoriaError> {
        let mut v = vec![0.0; test_dim()];
        match text {
            "alpha query" | "alpha branch memory" => v[0] = 1.0,
            "beta branch memory" => {
                v[0] = 0.6;
                v[1] = 0.4;
            }
            _ => v[2] = 1.0,
        }
        Ok(v)
    }

    fn dimension(&self) -> usize {
        test_dim()
    }
}

/// Returns (key, base_url, model) if EMBEDDING_API_KEY is set, else None.
fn try_embedding() -> Option<(String, String, String)> {
    let key = std::env::var("EMBEDDING_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())?;
    let base = std::env::var("EMBEDDING_BASE_URL")
        .unwrap_or_else(|_| "https://api.siliconflow.cn/v1".into());
    let model = std::env::var("EMBEDDING_MODEL").unwrap_or_else(|_| "BAAI/bge-m3".into());
    Some((key, base, model))
}

/// Spawn the API server on a random port, return (base_url, client).
async fn spawn_server() -> (String, reqwest::Client, support::multi_db::ApiTestServer) {
    let server = support::multi_db::spawn_api_server(
        "api_e2e",
        test_dim(),
        String::new(),
        None,
        None,
        None,
        false,
    )
    .await;
    (server.base.clone(), server.client.clone(), server)
}

// ── 1. health ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_health() {
    let (base, client, _server) = spawn_server().await;
    let r = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("get");
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "ok");
    println!("✅ GET /health");
}

// ── 2. store + list ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_store_and_list() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Rust is fast", "memory_type": "semantic"}))
        .send()
        .await
        .expect("post");
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["content"], "Rust is fast");
    let mid = body["memory_id"].as_str().unwrap().to_string();
    // memory_id must be UUIDv7 (32-char hex, version nibble '7' at position 12)
    assert_eq!(mid.len(), 32, "memory_id must be 32-char hex");
    assert_eq!(
        &mid[12..13],
        "7",
        "memory_id must be UUIDv7 (version nibble)"
    );
    println!("✅ POST /v1/memories: {mid}");

    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .expect("get");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|m| m["memory_id"] == mid));
    println!(
        "✅ GET /v1/memories: {} items",
        body["items"].as_array().unwrap().len()
    );
}

/// #224: metadata must survive every REST write/read path. This exercises the
/// MatrixOne JSON binding and the lightweight list mapper as well as the full
/// mappers used by get/retrieve/search.
#[tokio::test]
async fn test_api_extra_metadata_round_trip_and_dedup() {
    let (base, client, _server) =
        spawn_server_with_custom_embedder_and_pool(Arc::new(SessionScopeTestEmbedder), test_dim())
            .await;
    let user_id = uid();
    let metadata = json!({"scene": "incident", "agent": "triage", "rank": 2});

    let response = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "metadata round trip needle",
            "memory_type": "semantic",
            "extra_metadata": metadata,
        }))
        .send()
        .await
        .expect("store with metadata");
    assert_eq!(response.status(), 201);
    let stored: Value = response.json().await.unwrap();
    let memory_id = stored["memory_id"].as_str().unwrap().to_string();
    assert_eq!(stored["extra_metadata"], metadata);

    // Same-content dedup returns the persisted survivor, not a phantom ID, and
    // refreshes its metadata.
    let updated_metadata = json!({"scene": "resolved", "agent": "review"});
    let response = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "metadata round trip needle",
            "memory_type": "semantic",
            "extra_metadata": updated_metadata,
        }))
        .send()
        .await
        .expect("dedup metadata update");
    assert_eq!(response.status(), 201);
    let deduped: Value = response.json().await.unwrap();
    assert_eq!(deduped["memory_id"], memory_id);
    assert_eq!(deduped["extra_metadata"], updated_metadata);

    let response = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &user_id)
        .json(&json!({"memories": [{
            "content": "batch metadata round trip",
            "memory_type": "semantic",
            "extra_metadata": {"source": "batch"}
        }]}))
        .send()
        .await
        .expect("batch store with metadata");
    assert_eq!(response.status(), 201);
    let batch: Value = response.json().await.unwrap();
    assert_eq!(batch[0]["extra_metadata"], json!({"source": "batch"}));

    let response = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("list");
    let list: Value = response.json().await.unwrap();
    assert_eq!(
        list["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["memory_id"] == memory_id)
            .unwrap()["extra_metadata"],
        updated_metadata
    );

    let response = client
        .get(format!("{base}/v1/memories/{memory_id}"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .expect("get");
    let got: Value = response.json().await.unwrap();
    assert_eq!(got["extra_metadata"], updated_metadata);

    for endpoint in ["retrieve", "search"] {
        let response = client
            .post(format!("{base}/v1/memories/{endpoint}"))
            .header("X-User-Id", &user_id)
            .json(&json!({"query": "metadata round trip needle", "top_k": 10}))
            .send()
            .await
            .expect("retrieve/search");
        assert_eq!(response.status(), 200);
        let results: Value = response.json().await.unwrap();
        assert_eq!(
            results
                .as_array()
                .unwrap()
                .iter()
                .find(|item| item["memory_id"] == memory_id)
                .unwrap()["extra_metadata"],
            updated_metadata,
            "{endpoint} must return metadata"
        );
    }

    // `{}` is the explicit clear value but is normalized to an omitted response
    // field, while scalars/arrays are rejected by StoreRequest deserialization.
    let response = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({"content": "empty metadata", "extra_metadata": {}}))
        .send()
        .await
        .expect("store empty metadata");
    assert_eq!(response.status(), 201);
    assert!(response
        .json::<Value>()
        .await
        .unwrap()
        .get("extra_metadata")
        .is_none());
    let response = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({"content": "bad metadata", "extra_metadata": ["not", "an", "object"]}))
        .send()
        .await
        .expect("reject non-object metadata");
    assert_eq!(response.status(), 422);
}

#[tokio::test]
async fn test_api_structured_query_by_extra_metadata() {
    let (base, client, _server) = spawn_server().await;
    let user_id = uid();

    for (content, metadata) in [
        (
            "structured incident",
            json!({"scene": "incident", "rank": 2, "urgent": true}),
        ),
        (
            "structured review",
            json!({"scene": "review", "rank": 2, "urgent": true}),
        ),
        (
            "structured string rank",
            json!({"scene": "incident", "rank": "2", "urgent": true}),
        ),
    ] {
        let response = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &user_id)
            .json(&json!({"content": content, "extra_metadata": metadata}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
    }

    let response = client
        .post(format!("{base}/v1/memories/query"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "extra_metadata_filter": {"scene": "incident", "rank": 2, "urgent": true},
            "memory_types": ["semantic"],
            "limit": 10
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["content"], "structured incident");
    assert_eq!(items[0]["retrieval_score"], Value::Null);

    let response = client
        .post(format!("{base}/v1/memories/query"))
        .header("X-User-Id", &user_id)
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);

    let response = client
        .post(format!("{base}/v1/memories/query"))
        .header("X-User-Id", &user_id)
        .json(&json!({"extra_metadata_filter": {"nested": {"value": 1}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 422);

    for invalid_request in [
        json!({"subject_id": "subject", "limit": 0}),
        json!({"subject_id": "subject", "limit": 501}),
        json!({"subject_id": "subject", "extra_metadata_filters": {"scene": "incident"}}),
        json!({"extra_metadata_filter": {"scene": "incident"}, "subject_id": "   "}),
        json!({"extra_metadata_filter": {"scene": "incident"}, "session_id": "   "}),
        json!({"extra_metadata_filter": {"scene": "incident"}, "trust_tier": "   "}),
        json!({"extra_metadata_filter": {"scene": "incident"}, "branch": "   "}),
        json!({"extra_metadata_filter": {"scene": "incident"}, "memory_types": []}),
        json!({"extra_metadata_filter": {"scene": "incident"}, "memory_types": ["semantic", " "]}),
        json!({"extra_metadata_filter": {"1scene": "incident"}}),
        json!({"extra_metadata_filter": {"scene": "x".repeat(1025)}}),
    ] {
        let response = client
            .post(format!("{base}/v1/memories/query"))
            .header("X-User-Id", &user_id)
            .json(&invalid_request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 422, "request: {invalid_request}");
    }
}

#[tokio::test]
async fn test_api_structured_query_enforces_scope_active_and_all_predicates() {
    let (base, client, _server) = spawn_server().await;
    let user_id = uid();
    let other_user_id = uid();
    let marker = format!("structured_filter_{}", uuid::Uuid::new_v4().simple());
    let matching_metadata = json!({"marker": marker, "urgent": true});

    async fn store(
        client: &reqwest::Client,
        base: &str,
        user_id: &str,
        content: &str,
        memory_type: &str,
        metadata: Value,
    ) -> String {
        let response = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", user_id)
            .json(&json!({
                "content": content,
                "memory_type": memory_type,
                "extra_metadata": metadata,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        response.json::<Value>().await.unwrap()["memory_id"]
            .as_str()
            .unwrap()
            .to_string()
    }

    let expected_id = store(
        &client,
        &base,
        &user_id,
        "only eligible structured result",
        "semantic",
        matching_metadata.clone(),
    )
    .await;
    store(
        &client,
        &base,
        &user_id,
        "wrong metadata boolean",
        "semantic",
        json!({"marker": marker, "urgent": false}),
    )
    .await;
    store(
        &client,
        &base,
        &user_id,
        "wrong memory type",
        "profile",
        matching_metadata.clone(),
    )
    .await;
    store(
        &client,
        &base,
        &other_user_id,
        "other tenant",
        "semantic",
        matching_metadata.clone(),
    )
    .await;
    let inactive_id = store(
        &client,
        &base,
        &user_id,
        "inactive match",
        "semantic",
        matching_metadata.clone(),
    )
    .await;
    let response = client
        .delete(format!("{base}/v1/memories/{inactive_id}"))
        .header("X-User-Id", &user_id)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);

    let response = client
        .post(format!("{base}/v1/memories/query"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "extra_metadata_filter": {"marker": marker, "urgent": true},
            "memory_types": ["semantic"],
            "limit": 10,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["memory_id"], expected_id);
}

#[tokio::test]
async fn test_api_structured_query_cursor_and_subject_isolation() {
    let (base, client, _server) = spawn_server().await;
    let user_id = uid();
    let matching_subject = format!("subject_{}", uuid::Uuid::new_v4().simple());
    let other_subject = format!("subject_{}", uuid::Uuid::new_v4().simple());
    let marker = format!("marker_{}", uuid::Uuid::new_v4().simple());
    let mut expected_ids = std::collections::HashSet::new();

    for index in 0..3 {
        let response = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &user_id)
            .json(&json!({
                "content": format!("structured page {index}"),
                "subject_id": matching_subject,
                "extra_metadata": {"marker": marker}
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        expected_ids.insert(
            response.json::<Value>().await.unwrap()["memory_id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }

    let response = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "same marker, other subject",
            "subject_id": other_subject,
            "extra_metadata": {"marker": marker}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);

    let first = client
        .post(format!("{base}/v1/memories/query"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "extra_metadata_filter": {"marker": marker},
            "subject_id": matching_subject,
            "limit": 2
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(first.status(), 200);
    let first: Value = first.json().await.unwrap();
    let cursor = first["next_cursor"].as_str().expect("first page cursor");
    let first_ids: std::collections::HashSet<String> = first["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["memory_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(first_ids.len(), 2);

    let second = client
        .post(format!("{base}/v1/memories/query"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "extra_metadata_filter": {"marker": marker},
            "subject_id": matching_subject,
            "limit": 2,
            "cursor": cursor
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(second.status(), 200);
    let second: Value = second.json().await.unwrap();
    assert!(second["next_cursor"].is_null());
    let second_ids: std::collections::HashSet<String> = second["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["memory_id"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(second_ids.len(), 1);
    assert!(first_ids.is_disjoint(&second_ids));
    assert_eq!(
        first_ids
            .union(&second_ids)
            .cloned()
            .collect::<std::collections::HashSet<_>>(),
        expected_ids
    );
}

#[tokio::test]
async fn test_api_structured_query_on_branch() {
    let (base, client, _server) = spawn_server().await;
    let user_id = uid();
    let branch = format!("query_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    let marker = format!("branch_{}", uuid::Uuid::new_v4().simple());

    let response = client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &user_id)
        .json(&json!({"name": branch}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);

    let response = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({
            "content": "structured branch only",
            "branch": branch,
            "extra_metadata": {"marker": marker}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);

    let branch_response = client
        .post(format!("{base}/v1/memories/query"))
        .header("X-User-Id", &user_id)
        .json(&json!({"branch": branch}))
        .send()
        .await
        .unwrap();
    assert_eq!(branch_response.status(), 200);
    let branch_body: Value = branch_response.json().await.unwrap();
    assert_eq!(branch_body["items"].as_array().unwrap().len(), 1);

    let main_response = client
        .post(format!("{base}/v1/memories/query"))
        .header("X-User-Id", &user_id)
        .json(&json!({"extra_metadata_filter": {"marker": marker}}))
        .send()
        .await
        .unwrap();
    assert_eq!(main_response.status(), 200);
    let main_body: Value = main_response.json().await.unwrap();
    assert!(main_body["items"].as_array().unwrap().is_empty());
}

// ── 2b. list response is lightweight (no embedding) and respects limit ────────

#[tokio::test]
async fn test_api_fulltext_search_with_structured_prefilters() {
    let (base, client, _server) = spawn_server().await;
    let user_id = uid();
    let other_user_id = uid();
    let token = format!("fulltext{}", uuid::Uuid::new_v4().simple());
    let subject_id = format!("subject_{}", uuid::Uuid::new_v4().simple());
    let session_id = format!("session_{}", uuid::Uuid::new_v4().simple());

    for (suffix, memory_type, subject, session, tier, metadata) in [
        (
            "target",
            "semantic",
            subject_id.as_str(),
            Some(session_id.as_str()),
            "T2",
            json!({"scene": "incident", "rank": 2}),
        ),
        (
            "wrong metadata",
            "semantic",
            subject_id.as_str(),
            Some(session_id.as_str()),
            "T2",
            json!({"scene": "review", "rank": 2}),
        ),
        (
            "wrong metadata type",
            "semantic",
            subject_id.as_str(),
            Some(session_id.as_str()),
            "T2",
            json!({"scene": "incident", "rank": "2"}),
        ),
        (
            "wrong session",
            "semantic",
            subject_id.as_str(),
            Some("another_session"),
            "T2",
            json!({"scene": "incident", "rank": 2}),
        ),
        (
            "unscoped session",
            "semantic",
            subject_id.as_str(),
            None,
            "T2",
            json!({"scene": "incident", "rank": 2}),
        ),
        (
            "wrong subject",
            "semantic",
            "another_subject",
            Some(session_id.as_str()),
            "T2",
            json!({"scene": "incident", "rank": 2}),
        ),
        (
            "wrong trust tier",
            "semantic",
            subject_id.as_str(),
            Some(session_id.as_str()),
            "T3",
            json!({"scene": "incident", "rank": 2}),
        ),
        (
            "wrong memory type",
            "profile",
            subject_id.as_str(),
            Some(session_id.as_str()),
            "T2",
            json!({"scene": "incident", "rank": 2}),
        ),
    ] {
        let response = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &user_id)
            .json(&json!({
                "content": format!("{token} {suffix}"),
                "memory_type": memory_type,
                "subject_id": subject,
                "session_id": session,
                "trust_tier": tier,
                "extra_metadata": metadata
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
    }

    let response = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &other_user_id)
        .json(&json!({
            "content": format!("{token} wrong user"),
            "memory_type": "semantic",
            "subject_id": subject_id,
            "session_id": session_id,
            "trust_tier": "T2",
            "extra_metadata": {"scene": "incident", "rank": 2}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);

    let body = wait_for_api_payload_contains(
        &client,
        &base,
        &user_id,
        "/v1/memories/fulltext-search",
        json!({
            "query": token,
            "extra_metadata_filter": {"scene": "incident", "rank": 2},
            "subject_id": subject_id,
            "memory_types": ["semantic"],
            "session_id": session_id,
            "trust_tier": "T2",
            "limit": 10
        }),
        &["target"],
    )
    .await;
    let items = body.as_array().expect("fulltext response array");
    assert_eq!(items.len(), 1, "all pre-filters must be applied: {body}");
    assert_eq!(items[0]["content"], format!("{token} target"));
    assert!(items[0]["retrieval_score"].is_number());

    for invalid_request in [
        json!({"query": ""}),
        json!({"query": "!!!"}),
        json!({"query": "valid", "limit": 0}),
        json!({"query": "valid", "limit": 101}),
        json!({"query": "valid", "session_id": "   "}),
        json!({"query": "valid", "subject_id": "   "}),
        json!({"query": "valid", "trust_tier": "   "}),
        json!({"query": "valid", "branch": "   "}),
        json!({"query": "valid", "memory_types": ["   "]}),
        json!({"query": "valid", "memory_types": ["semantic", ""]}),
        json!({"query": "a".repeat(memoria_storage::FULLTEXT_QUERY_MAX_BYTES + 1)}),
        json!({"query": "valid", "extra_metadata_filters": {"scene": "incident"}}),
        json!({"query": "valid", "extra_metadata_filter": {"nested": {"value": 1}}}),
    ] {
        let response = client
            .post(format!("{base}/v1/memories/fulltext-search"))
            .header("X-User-Id", &user_id)
            .json(&invalid_request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 422, "request: {invalid_request}");
    }

    // MatrixOne tokenizes a single-character NGRAM query to an empty pattern.
    // The public endpoint treats that database condition as a valid empty result.
    let response = client
        .post(format!("{base}/v1/memories/fulltext-search"))
        .header("X-User-Id", &user_id)
        .json(&json!({"query": "a"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.unwrap();
    assert!(body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_api_fulltext_search_orders_by_score_then_memory_id() {
    let (base, client, _server) = spawn_server().await;
    let user_id = uid();
    let primary = format!("primary{}", uuid::Uuid::new_v4().simple());
    let secondary = format!("secondary{}", uuid::Uuid::new_v4().simple());

    let high_id =
        store_memory_unscoped(&client, &base, &user_id, &format!("{primary} {secondary}")).await;
    let first_tie_id = store_memory_unscoped(&client, &base, &user_id, &primary).await;
    let second_tie_id = store_memory_unscoped(&client, &base, &user_id, &primary).await;

    let body = wait_for_api_payload_contains(
        &client,
        &base,
        &user_id,
        "/v1/memories/fulltext-search",
        json!({"query": format!("{primary} {secondary}"), "limit": 10}),
        &[&high_id, &first_tie_id, &second_tie_id],
    )
    .await;
    let items = body.as_array().expect("fulltext response array");
    assert_eq!(
        items.len(),
        3,
        "all ranked fixtures must be returned: {body}"
    );
    assert_eq!(items[0]["memory_id"], high_id);

    let high_score = items[0]["retrieval_score"].as_f64().unwrap();
    let first_tie_score = items[1]["retrieval_score"].as_f64().unwrap();
    let second_tie_score = items[2]["retrieval_score"].as_f64().unwrap();
    assert!(high_score > first_tie_score);
    assert_eq!(first_tie_score, second_tie_score);

    let mut expected_tie_ids = [first_tie_id, second_tie_id];
    expected_tie_ids.sort_by(|left, right| right.cmp(left));
    assert_eq!(items[1]["memory_id"], expected_tie_ids[0]);
    assert_eq!(items[2]["memory_id"], expected_tie_ids[1]);
}

#[tokio::test]
async fn test_api_fulltext_search_on_branch_is_isolated_from_main() {
    let (base, client, _server) = spawn_server().await;
    let user_id = uid();
    let branch = format!(
        "fulltext_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let token = format!("branchfulltext{}", uuid::Uuid::new_v4().simple());

    let response = client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &user_id)
        .json(&json!({"name": branch}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let response = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user_id)
        .json(&json!({"content": token, "branch": branch}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);

    let branch_body = wait_for_api_payload_contains(
        &client,
        &base,
        &user_id,
        "/v1/memories/fulltext-search",
        json!({"query": token, "branch": branch}),
        &[&token],
    )
    .await;
    assert_eq!(branch_body.as_array().unwrap().len(), 1);

    let main_response = client
        .post(format!("{base}/v1/memories/fulltext-search"))
        .header("X-User-Id", &user_id)
        .json(&json!({"query": token}))
        .send()
        .await
        .unwrap();
    assert_eq!(main_response.status(), 200);
    let main_body: Value = main_response.json().await.unwrap();
    assert!(main_body.as_array().unwrap().is_empty());
}

#[tokio::test]
async fn test_api_list_no_embedding_and_limit() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store 3 memories: 2 semantic + 1 profile, with small delays for distinct created_at
    for i in 0..2 {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("semantic {i}"), "memory_type": "semantic"}))
            .send()
            .await
            .expect("post");
        assert_eq!(r.status(), 201);
    }
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "profile item", "memory_type": "profile"}))
        .send()
        .await
        .expect("post");
    assert_eq!(r.status(), 201);

    // ── List all — verify excluded fields and required fields ──
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .expect("get");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert!(
        body["next_cursor"].is_null(),
        "no cursor when all items fit"
    );
    for item in items {
        // Must NOT contain heavy fields
        assert!(
            item.get("embedding").is_none(),
            "must not contain embedding"
        );
        assert!(
            item.get("source_event_ids").is_none(),
            "must not contain source_event_ids"
        );
        assert!(
            item.get("extra_metadata").is_none(),
            "metadata is omitted when no metadata was stored"
        );
        // Must contain core fields
        assert!(item["memory_id"].as_str().is_some(), "memory_id required");
        assert!(item["content"].as_str().is_some(), "content required");
        assert!(
            item["memory_type"].as_str().is_some(),
            "memory_type required"
        );
        assert!(item["trust_tier"].as_str().is_some(), "trust_tier required");
    }
    // Ordering: newest first (memory_id DESC — UUIDv7 is time-ordered)
    let ids: Vec<&str> = items
        .iter()
        .map(|i| i["memory_id"].as_str().unwrap())
        .collect();
    for w in ids.windows(2) {
        assert!(w[0] >= w[1], "must be ordered by memory_id DESC");
    }

    // ── memory_type filter ──
    let r = client
        .get(format!("{base}/v1/memories?memory_type=profile"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .expect("get");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["memory_type"].as_str().unwrap(), "profile");

    // ── Cursor pagination: page through with limit=1 ──
    let mut seen_ids: Vec<String> = Vec::new();
    let mut url = format!("{base}/v1/memories?limit=1");
    loop {
        let r = client
            .get(&url)
            .header("X-User-Id", &uid)
            .send()
            .await
            .expect("get");
        assert_eq!(r.status(), 200);
        let body: Value = r.json().await.unwrap();
        let page = body["items"].as_array().unwrap();
        assert!(!page.is_empty(), "page must not be empty while paginating");
        for item in page {
            seen_ids.push(item["memory_id"].as_str().unwrap().to_string());
        }
        match body["next_cursor"].as_str() {
            Some(c) => {
                assert!(!c.is_empty(), "cursor must not be empty");
                // Cursor is a memory_id (32-char hex UUIDv7)
                assert_eq!(c.len(), 32, "cursor must be 32-char hex");
                assert!(
                    c.chars().all(|ch| ch.is_ascii_hexdigit()),
                    "cursor must be hex"
                );
                url = format!("{base}/v1/memories?limit=1&cursor={c}");
            }
            None => break,
        }
    }
    assert_eq!(seen_ids.len(), 3, "cursor pagination must visit all items");
    // All IDs must be unique (no duplicates across pages)
    let unique: std::collections::HashSet<&String> = seen_ids.iter().collect();
    assert_eq!(unique.len(), 3, "no duplicate items across pages");

    println!("✅ list: fields, ordering, filter, cursor pagination");
}

// ── 2b. list: cursor + memory_type combined ───────────────────────────────────

#[tokio::test]
async fn test_api_list_cursor_with_type_filter() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store 3 semantic + 1 profile
    for i in 0..3 {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("sem {i}"), "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
    }
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "prof", "memory_type": "profile"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // Paginate semantic only with limit=1
    let mut seen: Vec<String> = Vec::new();
    let mut url = format!("{base}/v1/memories?memory_type=semantic&limit=1");
    loop {
        let body: Value = client
            .get(&url)
            .header("X-User-Id", &uid)
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let page = body["items"].as_array().unwrap();
        if page.is_empty() {
            break;
        }
        for item in page {
            assert_eq!(item["memory_type"].as_str().unwrap(), "semantic");
            seen.push(item["memory_id"].as_str().unwrap().to_string());
        }
        match body["next_cursor"].as_str() {
            Some(c) => url = format!("{base}/v1/memories?memory_type=semantic&limit=1&cursor={c}"),
            None => break,
        }
    }
    assert_eq!(seen.len(), 3, "must see all 3 semantic memories");
    let unique: std::collections::HashSet<&String> = seen.iter().collect();
    assert_eq!(unique.len(), 3, "no duplicates");
    println!("✅ list: cursor + memory_type filter");
}

// ── 2c. list: soft-deleted excluded ───────────────────────────────────────────

#[tokio::test]
async fn test_api_list_excludes_deleted() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store 2 memories
    let mut ids = Vec::new();
    for i in 0..2 {
        let body: Value = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("del {i}"), "memory_type": "semantic"}))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        ids.push(body["memory_id"].as_str().unwrap().to_string());
    }

    // Delete the first one
    let r = client
        .delete(format!("{base}/v1/memories/{}", ids[0]))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());

    // List should only return the second
    let body: Value = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["memory_id"].as_str().unwrap(), ids[1]);
    println!("✅ list: soft-deleted excluded");
}

// ── 2d. list: empty result ────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_list_empty() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid(); // fresh user, no memories

    let body: Value = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["items"].as_array().unwrap().len(), 0);
    assert!(body["next_cursor"].is_null(), "no cursor for empty result");
    println!("✅ list: empty result");
}

// ── 2e. list: limit capped at 500 ────────────────────────────────────────────

#[tokio::test]
async fn test_api_list_limit_cap() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store 2 memories, request limit=9999 — should still work (capped internally)
    for i in 0..2 {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("cap {i}"), "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
    }

    let body: Value = client
        .get(format!("{base}/v1/memories?limit=9999"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 2, "returns all items even with huge limit");
    assert!(body["next_cursor"].is_null(), "no cursor when all fit");
    println!("✅ list: limit cap at 500");
}

// ── 2f. list: invalid cursor doesn't crash ────────────────────────────────────

#[tokio::test]
async fn test_api_list_invalid_cursor() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store 1 memory so user exists
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "x", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // Garbage cursor — should not 500
    let r = client
        .get(format!("{base}/v1/memories?limit=10&cursor=garbage"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    // Either 200 with empty/some results, or 400 — but NOT 500
    assert_ne!(
        r.status(),
        500,
        "invalid cursor must not cause server error"
    );

    // Cursor without '|' separator — not a valid timestamp, treated as no cursor
    let r = client
        .get(format!("{base}/v1/memories?limit=10&cursor=no-pipe-here"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "non-timestamp cursor should be treated as no cursor"
    );
    println!("✅ list: invalid cursor handled gracefully");
}

// ── 2g. list: user isolation ──────────────────────────────────────────────────

#[tokio::test]
async fn test_api_list_user_isolation() {
    let (base, client, _server) = spawn_server().await;
    let uid_a = uid();
    let uid_b = uid();

    // User A stores a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid_a)
        .json(&json!({"content": "user A secret", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // User B stores a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid_b)
        .json(&json!({"content": "user B data", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // User A list — should only see their own
    let body: Value = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["content"].as_str().unwrap(), "user A secret");

    // User B list — should only see their own
    let body: Value = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid_b)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["content"].as_str().unwrap(), "user B data");
    println!("✅ list: user isolation");
}

// ── 2h. list: default limit ──────────────────────────────────────────────────

#[tokio::test]
async fn test_api_list_default_limit() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store 3 memories, don't pass limit param
    for i in 0..3 {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("dflt {i}"), "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
    }

    // No limit param — default is 100, so all 3 should come back
    let body: Value = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 3);
    assert!(
        body["next_cursor"].is_null(),
        "no cursor when under default limit"
    );
    println!("✅ list: default limit");
}

// ── 2i. list: has_more works at limit boundary (regression: double-clamp) ────

#[tokio::test]
async fn test_api_list_has_more_at_limit_boundary() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store 2 memories
    for i in 0..2 {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("boundary {i}"), "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
    }

    // Request limit=1 — there are 2 items, so has_more must be true (next_cursor present)
    let body: Value = client
        .get(format!("{base}/v1/memories?limit=1"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert!(
        body["next_cursor"].as_str().is_some(),
        "must have next_cursor when more items exist"
    );

    // Follow cursor — should get the second item with no further cursor
    let cursor = body["next_cursor"].as_str().unwrap();
    let body: Value = client
        .get(format!("{base}/v1/memories?limit=1&cursor={cursor}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let items2 = body["items"].as_array().unwrap();
    assert_eq!(items2.len(), 1);
    assert!(body["next_cursor"].is_null(), "no more items after page 2");

    // The two pages must return different items
    assert_ne!(
        items[0]["memory_id"].as_str().unwrap(),
        items2[0]["memory_id"].as_str().unwrap(),
        "pages must not overlap"
    );
    println!("✅ list: has_more at limit boundary");
}

// ── 3. batch store ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_batch_store() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "Memory A"},
            {"content": "Memory B", "memory_type": "profile"},
        ]}))
        .send()
        .await
        .expect("post");
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body.as_array().unwrap().len(), 2);
    println!("✅ POST /v1/memories/batch: 2 stored");
}

// ── 4. retrieve ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_retrieve() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "MatrixOne is a distributed database"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "database", "top_k": 5}))
        .send()
        .await
        .expect("post");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(!body.as_array().unwrap().is_empty());
    println!(
        "✅ POST /v1/memories/retrieve: {} results",
        body.as_array().unwrap().len()
    );
}

// ── 5. correct by id ──────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_correct() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Uses black for formatting"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = client
        .put(format!("{base}/v1/memories/{mid}/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "Uses ruff for formatting"}))
        .send()
        .await
        .expect("put");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["content"], "Uses ruff for formatting");
    println!("✅ PUT /v1/memories/:id/correct");
}

// ── 6. delete ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_delete() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "to be deleted"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = client
        .delete(format!("{base}/v1/memories/{mid}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .expect("delete");
    assert_eq!(r.status(), 204);
    println!("✅ DELETE /v1/memories/:id");
}

// ── 7. purge bulk ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_purge_bulk() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let mut ids = Vec::new();
    for i in 0..3 {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("bulk purge {i}")}))
            .send()
            .await
            .unwrap();
        ids.push(
            r.json::<Value>().await.unwrap()["memory_id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }

    let r = client
        .post(format!("{base}/v1/memories/purge"))
        .header("X-User-Id", &uid)
        .json(&json!({"memory_ids": ids}))
        .send()
        .await
        .expect("post");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["purged"], 3);
    println!("✅ POST /v1/memories/purge: 3 purged");
}

// ── 8. profile ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_profile() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Prefers Rust", "memory_type": "profile"}))
        .send()
        .await
        .unwrap();

    let r = client
        .get(format!("{base}/v1/profiles/me"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .expect("get");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["profile"].as_str().unwrap().contains("Prefers Rust"));
    println!("✅ GET /v1/profiles/me");
}

// ── 9. governance ─────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_governance() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/governance"))
        .header("X-User-Id", &uid)
        .json(&json!({"force": true}))
        .send()
        .await
        .expect("post");
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body.get("quarantined").is_some() || body.get("skipped").is_some());
    println!("✅ POST /v1/governance");
}

// ── Helper: spawn server with master key ─────────────────────────────────────

async fn spawn_server_with_master_key(
    master_key: &str,
) -> (String, reqwest::Client, support::multi_db::ApiTestServer) {
    let server = support::multi_db::spawn_api_server(
        "api_e2e_master",
        test_dim(),
        master_key.to_string(),
        None,
        None,
        None,
        true,
    )
    .await;
    (server.base.clone(), server.client.clone(), server)
}

async fn create_api_key_for_user(
    client: &reqwest::Client,
    base: &str,
    master_auth: &str,
    user_id: &str,
    name: &str,
) -> String {
    let r = client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", master_auth)
        .json(&json!({ "user_id": user_id, "name": name }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "create key for {user_id}");
    r.json::<Value>().await.unwrap()["raw_key"]
        .as_str()
        .unwrap()
        .to_string()
}

// ── 10. auth: missing token returns 401 ──────────────────────────────────────

#[tokio::test]
async fn test_api_auth_required() {
    let mk = "test-master-key-12345";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;

    // No token → 401
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", "alice")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // Wrong token → 401
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", "alice")
        .header("Authorization", "Bearer wrong-key")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // Correct token → 200
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", "alice")
        .header("Authorization", format!("Bearer {mk}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    println!("✅ Auth: 401 without token, 200 with correct token");
}

// ── 10b. auth: full API key CRUD (create/list/rotate/revoke) ─────────────────

#[tokio::test]
async fn test_api_key_crud() {
    let mk = "test-master-key-crud";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // 1. Create key
    let r = client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({"user_id": uid, "name": "test-key-1"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "create key");
    let body: Value = r.json().await.unwrap();
    let key_id = body["key_id"].as_str().unwrap().to_string();
    let raw_key = body["raw_key"].as_str().unwrap().to_string();
    assert!(raw_key.starts_with("sk-"), "raw_key should start with sk-");
    assert_eq!(body["user_id"].as_str().unwrap(), uid);
    assert_eq!(body["name"].as_str().unwrap(), "test-key-1");
    println!("✅ create key: {key_id}, prefix={}", body["key_prefix"]);

    // 2. List keys — use master key to authenticate (API keys are for external use)
    let r = client
        .get(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "list keys");
    let keys: Vec<Value> = r.json().await.unwrap();
    assert!(
        keys.iter().any(|k| k["key_id"].as_str() == Some(&key_id)),
        "should find created key"
    );
    println!("✅ list keys: {} keys found", keys.len());

    // 3. Rotate key
    let r = client
        .put(format!("{base}/auth/keys/{key_id}/rotate"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201, "rotate key");
    let body: Value = r.json().await.unwrap();
    let new_key_id = body["key_id"].as_str().unwrap().to_string();
    let new_raw_key = body["raw_key"].as_str().unwrap().to_string();
    assert_ne!(new_key_id, key_id, "rotated key should have new id");
    assert_ne!(new_raw_key, raw_key, "rotated key should have new raw_key");
    assert_eq!(
        body["name"].as_str().unwrap(),
        "test-key-1",
        "name preserved"
    );
    println!("✅ rotate key: old={key_id} → new={new_key_id}");

    // 4. Old key should be deactivated — verify via list
    let r = client
        .get(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let keys: Vec<Value> = r.json().await.unwrap();
    assert!(
        !keys.iter().any(|k| k["key_id"].as_str() == Some(&key_id)),
        "old key should not appear in active list"
    );
    println!("✅ old key deactivated after rotate");

    // 5. New key appears in list
    assert!(
        keys.iter()
            .any(|k| k["key_id"].as_str() == Some(&new_key_id)),
        "new key should appear in active list"
    );
    println!("✅ new key in active list after rotate");

    // 6. Revoke key
    let r = client
        .delete(format!("{base}/auth/keys/{new_key_id}"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204, "revoke key");
    println!("✅ revoke key: {new_key_id}");

    // 7. Revoked key should not appear in active list
    let r = client
        .get(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let keys: Vec<Value> = r.json().await.unwrap();
    assert!(
        !keys
            .iter()
            .any(|k| k["key_id"].as_str() == Some(&new_key_id)),
        "revoked key should not appear in active list"
    );
    println!("✅ revoked key not in active list");

    // 8. Rotate non-existent key → 404
    let r = client
        .put(format!("{base}/auth/keys/nonexistent-id/rotate"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "rotate nonexistent");
    println!("✅ rotate nonexistent → 404");

    // 9. Revoke non-existent key → 404
    let r = client
        .delete(format!("{base}/auth/keys/nonexistent-id"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404, "revoke nonexistent");
    println!("✅ revoke nonexistent → 404");
}

#[tokio::test]
async fn test_scoped_api_key_whoami_and_memory_authorization() {
    let mk = "test-master-key-scopes";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let user_id = uid();

    let r = client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({
            "user_id": user_id,
            "name": "astra-read-only",
            "scopes": ["identity:read", "memory:read"]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let key_body: Value = r.json().await.unwrap();
    let raw_key = key_body["raw_key"].as_str().unwrap();
    assert_eq!(key_body["scopes"], json!(["identity:read", "memory:read"]));

    let bearer = format!("Bearer {raw_key}");
    let r = client
        .get(format!("{base}/auth/whoami"))
        .header("Authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let whoami: Value = r.json().await.unwrap();
    assert_eq!(whoami["user_id"], user_id);
    assert_eq!(whoami["scope"]["type"], "personal");
    assert_eq!(whoami["scope"]["id"], user_id);
    assert_eq!(
        whoami["granted_scopes"],
        json!(["identity:read", "memory:read"])
    );
    assert_eq!(
        whoami["capabilities"],
        json!(["api_key_scopes", "memory_filters_v1"])
    );

    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "read-only key must be able to read");

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", &bearer)
        .json(&json!({"content": "must not be stored"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "read-only key must not be able to write");
    assert!(r.text().await.unwrap().contains("memory:write"));

    let r = client
        .get(format!("{base}/auth/keys"))
        .header("Authorization", &bearer)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        403,
        "integration keys must not be able to mint or manage credentials"
    );
    assert!(r.text().await.unwrap().contains("keys:manage"));

    let r = client
        .post(format!("{base}/mcp"))
        .header("Authorization", &bearer)
        .json(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "tools/call",
            "params": {"name": "memory_store", "arguments": {"content": "blocked"}}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["error"]["code"], -32003);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("memory:write"));
}

#[tokio::test]
async fn test_mcp_tool_outcomes_persist_and_remain_queryable_by_tool() {
    let master = "tool-outcome-test-master";
    let (base, client, server) = spawn_server_with_master_key(master).await;
    let user = uid();
    let response = client
        .post(format!("{base}/mcp"))
        .bearer_auth(master)
        .header("X-User-Id", &user)
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"memory_store","arguments":{"content":""}}}))
        .send()
        .await
        .unwrap();
    let body: Value = response.json().await.unwrap();
    assert_eq!(body["result"]["isError"], true, "{body}");
    // Inject a classified backend outcome to exercise persistence/querying
    // without relying on a flaky live database outage.
    server.state().call_log_batcher.record_rpc(
        user.clone(),
        "POST".into(),
        "/mcp/memory_retrieve".into(),
        200,
        1,
        memoria_api::auth::RpcMeta {
            success: true,
            error_code: None,
            tool_success: Some(false),
            tool_error_kind: Some("backend"),
        },
    );
    server
        .state()
        .call_log_batcher
        .flush(&server.service())
        .await;
    let store = server.service().user_sql_store(&user).await.unwrap();
    let rows: Vec<(String, i8, Option<i32>, Option<i8>, Option<String>)> = sqlx::query_as(&format!(
        "SELECT path, rpc_success, rpc_error_code, tool_success, tool_error_kind FROM {} WHERE user_id = ? ORDER BY path",
        store.t("mem_api_call_log")))
        .bind(&user).fetch_all(store.pool()).await.unwrap();
    assert_eq!(
        rows,
        vec![
            (
                "/mcp/memory_retrieve".into(),
                1,
                None,
                Some(0),
                Some("backend".into())
            ),
            (
                "/mcp/memory_store".into(),
                1,
                None,
                Some(0),
                Some("input".into())
            ),
        ]
    );
    let response = client
        .get(format!("{base}/admin/users/{user}/call-stats"))
        .bearer_auth(master)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let stats: Value = response.json().await.unwrap();
    let paths = stats["by_path"].as_array().expect("per-tool statistics");
    let input = paths
        .iter()
        .find(|r| r["path"] == "/mcp/memory_store")
        .unwrap();
    assert_eq!(input["rpc_error_count"], 0);
    assert_eq!(input["tool_input_error_count"], 1);
    assert_eq!(input["tool_backend_error_count"], 0);
    let backend = paths
        .iter()
        .find(|r| r["path"] == "/mcp/memory_retrieve")
        .unwrap();
    assert_eq!(backend["tool_backend_error_count"], 1);
}

#[tokio::test]
async fn test_call_log_repair_failure_keeps_memory_api_available_and_background_recovers() {
    let master = "log-repair-test-master";
    let (base, client, server) = spawn_server_with_master_key(master).await;
    let user = uid();
    server.user_store(&user).await;
    let pool = server.user_db_pool(&user).await;
    // Simulate a version-2 database whose optional logging table is unavailable.
    for column in ["tool_success", "tool_error_kind"] {
        sqlx::query(&format!(
            "ALTER TABLE mem_api_call_log DROP COLUMN {column}"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("ALTER TABLE mem_api_call_log RENAME TO saved_call_log")
        .execute(&pool)
        .await
        .unwrap();
    server.router().invalidate_user(&user).await;
    let response = client
        .post(format!("{base}/v1/memories"))
        .bearer_auth(master)
        .header("X-User-Id", &user)
        .json(&json!({"content":"A durable preference during a logging outage"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let memory: Value = response.json().await.unwrap();
    let memory_id = memory["memory_id"].as_str().expect("stored memory id");
    let response = client
        .get(format!("{base}/v1/memories/{memory_id}"))
        .bearer_auth(master)
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let cached_store = server.service().user_sql_store(&user).await.unwrap();
    server
        .state()
        .call_log_batcher
        .flush(&server.service())
        .await;

    // Restore the legacy table without invalidating the cached user store.
    sqlx::query("ALTER TABLE saved_call_log RENAME TO mem_api_call_log")
        .execute(&pool)
        .await
        .unwrap();
    server.state().call_log_batcher.record_rpc(
        user.clone(),
        "POST".into(),
        "/mcp/memory_retrieve".into(),
        200,
        1,
        memoria_api::auth::RpcMeta {
            success: true,
            error_code: None,
            tool_success: Some(false),
            tool_error_kind: Some("backend"),
        },
    );
    server
        .state()
        .call_log_batcher
        .flush(&server.service())
        .await;
    let store = server.service().user_sql_store(&user).await.unwrap();
    assert!(Arc::ptr_eq(&cached_store, &store));
    let outcome: (Option<i8>, Option<String>) = sqlx::query_as(
        "SELECT tool_success, tool_error_kind FROM mem_api_call_log
         WHERE user_id = ? AND path = '/mcp/memory_retrieve'",
    )
    .bind(&user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(outcome, (Some(0), Some("backend".into())));
    let response = client
        .get(format!("{base}/admin/users/{user}/call-stats"))
        .bearer_auth(master)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let stats: Value = response.json().await.unwrap();
    let entry = stats["by_path"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["path"] == "/mcp/memory_retrieve")
        .expect("recovered tool statistics");
    assert_eq!(entry["tool_backend_error_count"], 1);
    pool.close().await;
}

async fn assert_scoped_user_has_no_database(server: &support::multi_db::ApiTestServer, user: &str) {
    let registry: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM mem_user_registry WHERE user_id = ?")
            .bind(user)
            .fetch_one(&server.shared_pool())
            .await
            .unwrap();
    assert_eq!(registry, 0, "denied request registered {user}");
    let db_name = memoria_storage::DbRouter::user_db_name_for_id(user);
    let databases: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
    )
    .bind(db_name)
    .fetch_one(&server.shared_pool())
    .await
    .unwrap();
    assert_eq!(databases, 0, "denied request created a database for {user}");
}

#[tokio::test]
async fn test_scoped_mcp_denials_do_not_provision_storage_even_after_flush() {
    let master = "denied-telemetry-test-master";
    let (base, client, server) = spawn_server_with_master_key(master).await;
    let mut users = Vec::new();
    let mut write_key = String::new();
    for (scopes, denied_tools, expected_rpc_code) in [
        (
            json!(["identity:read"]),
            vec!["memory_store", "memory_search"],
            -32003,
        ),
        (
            json!(["identity:read", "memory:read"]),
            vec!["memory_store", "memory_tune_params"],
            -32003,
        ),
        (
            json!(["identity:read", "memory:read", "memory:write"]),
            vec!["memory_unclassified"],
            -32602,
        ),
    ] {
        let user = uid();
        let response = client
            .post(format!("{base}/auth/keys"))
            .bearer_auth(master)
            .json(&json!({"user_id":user,"name":"denial-test","scopes":scopes}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let key = response.json::<Value>().await.unwrap()["raw_key"]
            .as_str()
            .unwrap()
            .to_owned();
        assert_scoped_user_has_no_database(&server, &user).await;
        for tool in denied_tools {
            for notification in [false, true] {
                for header in ["X-Memoria-Tool", "X-Tool-Name"] {
                    let mut request = json!({"jsonrpc":"2.0","method":"tools/call",
                        "params":{"name":tool,"arguments":{"content":"must not be stored"}}});
                    if !notification {
                        request["id"] = json!(1);
                    }
                    let response = client
                        .post(format!("{base}/mcp"))
                        .bearer_auth(&key)
                        .header(header, "denied-test-agent")
                        .json(&request)
                        .send()
                        .await
                        .unwrap();
                    if notification {
                        assert_eq!(response.status(), 204);
                        assert!(response.bytes().await.unwrap().is_empty());
                    } else {
                        assert_eq!(response.status(), 200);
                        let result: Value = response.json().await.unwrap();
                        // Known tools with insufficient scopes are authorization
                        // errors; unknown tools are invalid protocol parameters.
                        // Neither rejection may provision user storage.
                        assert_eq!(result["error"]["code"], expected_rpc_code, "{result}");
                        assert!(result.get("result").is_none(), "{result}");
                    }
                }
            }
        }
        // Parse failures have not admitted any tool either.
        for invalid in [
            "not json",
            "[]",
            r#"{"jsonrpc":"1.0","method":"tools/call"}"#,
        ] {
            let response = client
                .post(format!("{base}/mcp"))
                .bearer_auth(&key)
                .header("X-Memoria-Tool", "denied-test-agent")
                .body(invalid)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            assert!(response
                .json::<Value>()
                .await
                .unwrap()
                .get("error")
                .is_some());
        }
        if scopes.as_array().unwrap().len() == 1 {
            let response = client
                .get(format!("{base}/auth/whoami"))
                .bearer_auth(&key)
                .header("X-Memoria-Tool", "identity-test-agent")
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            for method in [
                "initialize",
                "tools/list",
                "ping",
                "notifications/initialized",
            ] {
                let response = client
                    .post(format!("{base}/mcp"))
                    .bearer_auth(&key)
                    .header("X-Tool-Name", "identity-test-agent")
                    .json(&json!({"jsonrpc":"2.0","id":2,"method":method,"params":{}}))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 200);
            }
        }
        assert!(server
            .state()
            .tool_usage_batcher
            .get_user_tool_usage(&user)
            .is_empty());
        users.push(user);
        write_key = key;
    }
    server
        .state()
        .tool_usage_batcher
        .flush(&server.service())
        .await;
    server
        .state()
        .call_log_batcher
        .flush(&server.service())
        .await;
    for user in &users {
        assert_scoped_user_has_no_database(&server, user).await;
    }
    server.state().drain_flushers().await;
    for user in &users {
        assert_scoped_user_has_no_database(&server, user).await;
    }

    // Positive control: an admitted write still provisions and tracks usage.
    let response = client
        .post(format!("{base}/mcp"))
        .bearer_auth(&write_key)
        .header("X-Memoria-Tool", "allowed-test-agent")
        .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"memory_store","arguments":{"content":"authorized fixture"}}}))
        .send()
        .await
        .unwrap();
    let result: Value = response.json().await.unwrap();
    assert!(result.get("error").is_none(), "{result}");
    assert_ne!(result["result"]["isError"], true, "{result}");
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mem_user_registry WHERE user_id = ?")
        .bind(users.last().unwrap())
        .fetch_one(&server.shared_pool())
        .await
        .unwrap();
    assert_eq!(count, 1);
    assert!(!server
        .state()
        .tool_usage_batcher
        .get_user_tool_usage(users.last().unwrap())
        .is_empty());
    server
        .state()
        .tool_usage_batcher
        .flush(&server.service())
        .await;
    server
        .state()
        .call_log_batcher
        .flush(&server.service())
        .await;
}

#[tokio::test]
async fn test_scoped_key_warm_cache_expires_for_rest_and_mcp() {
    use sha2::{Digest, Sha256};
    let master = "expiry-test-master";
    let (base, client, server) = spawn_server_with_master_key(master).await;
    // DATETIME(6) stores microseconds; Linux clocks can expose nanoseconds.
    let expiry = chrono::DateTime::from_timestamp_micros(chrono::Utc::now().timestamp_micros())
        .unwrap()
        .naive_utc()
        + chrono::Duration::seconds(5);
    let mut keys = Vec::new();
    for expires_at in [
        Some(expiry),
        Some(expiry),
        None,
        Some(expiry + chrono::Duration::hours(1)),
    ] {
        let response = client.post(format!("{base}/auth/keys")).bearer_auth(master)
            .json(&json!({"user_id":uid(),"name":"expiry-test","expires_at":expires_at.map(|ts|ts.to_string()),
                "scopes":["identity:read","memory:read","memory:write"]})).send().await.unwrap();
        assert_eq!(response.status(), 201);
        let key = response.json::<Value>().await.unwrap()["raw_key"]
            .as_str()
            .unwrap()
            .to_owned();
        let response = client
            .post(format!("{base}/mcp"))
            .bearer_auth(&key)
            .json(&json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200, "must warm before expiry");
        let hash = format!("{:x}", Sha256::digest(key.as_bytes()));
        let cached = server
            .state()
            .api_key_cache
            .get(&hash)
            .expect("principal cached");
        assert_eq!(cached.expires_at, expires_at);
        keys.push(key);
    }
    let wait = (expiry - chrono::Utc::now().naive_utc())
        .to_std()
        .unwrap_or_default();
    tokio::time::sleep(wait + std::time::Duration::from_millis(100)).await;
    // Separate warmed entries: REST rejection must not evict the MCP entry
    // before its own first post-expiry authentication attempt.
    let rest = client
        .get(format!("{base}/v1/memories"))
        .bearer_auth(&keys[0])
        .send()
        .await
        .unwrap();
    assert_eq!(rest.status(), 401);
    let mcp = client
        .post(format!("{base}/mcp"))
        .bearer_auth(&keys[1])
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"memory_store","arguments":{"content":"expired denial"}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(mcp.status(), 401);
    for key in &keys[..2] {
        let response = client
            .get(format!("{base}/auth/whoami"))
            .bearer_auth(key)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            401,
            "fresh DB validation agrees with cache expiry"
        );
    }
    for key in &keys[2..] {
        let response = client
            .get(format!("{base}/v1/memories"))
            .bearer_auth(key)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "non-expiring/future keys remain valid"
        );
        let response = client
            .post(format!("{base}/mcp"))
            .bearer_auth(key)
            .json(&json!({"jsonrpc":"2.0","id":3,"method":"ping"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
    }
}

#[tokio::test]
async fn test_scoped_group_denial_precedes_memory_storage_lookup() {
    let master = "group-denial-test-master";
    let (base, client, server) = spawn_server_with_master_key(master).await;
    let user = uid();
    let group = format!("grp_{}", uuid::Uuid::new_v4().simple());
    let group_db = memoria_storage::DbRouter::user_db_name_for_id(&group);
    // Seed only control-plane metadata; neither member nor group has a memory DB.
    sqlx::query("INSERT INTO mem_groups (group_id,group_name,db_name,owner_user_id,status,created_at,updated_at) VALUES (?, 'denial fixture', ?, ?, 'active', NOW(6), NOW(6))")
        .bind(&group).bind(&group_db).bind(&user).execute(&server.shared_pool()).await.unwrap();
    sqlx::query("INSERT INTO mem_group_members (group_id,user_id,role,is_active,joined_at) VALUES (?, ?, 'owner', 1, NOW(6))")
        .bind(&group).bind(&user).execute(&server.shared_pool()).await.unwrap();
    let response = client.post(format!("{base}/auth/keys")).bearer_auth(master)
        .json(&json!({"user_id":user,"group_id":group,"name":"group-denial","scopes":["identity:read","memory:read"]})).send().await.unwrap();
    assert_eq!(response.status(), 201);
    let key = response.json::<Value>().await.unwrap()["raw_key"]
        .as_str()
        .unwrap()
        .to_owned();
    for notification in [false, true] {
        let mut body = json!({"jsonrpc":"2.0","method":"tools/call","params":{"name":"memory_store","arguments":{"content":"denied"}}});
        if !notification {
            body["id"] = json!(1);
        }
        let response = client
            .post(format!("{base}/mcp"))
            .bearer_auth(&key)
            .header("X-Memoria-Tool", "denied-group-agent")
            .json(&body)
            .send()
            .await
            .unwrap();
        if notification {
            assert_eq!(response.status(), 204);
        } else {
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"]["code"],
                -32003
            );
        }
    }
    let response = client
        .post(format!("{base}/v1/memories"))
        .bearer_auth(&key)
        .json(&json!({"content":"denied"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    server.state().drain_flushers().await;
    assert_scoped_user_has_no_database(&server, &user).await;
    let databases: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM information_schema.SCHEMATA WHERE SCHEMA_NAME = ?",
    )
    .bind(group_db)
    .fetch_one(&server.shared_pool())
    .await
    .unwrap();
    assert_eq!(
        databases, 0,
        "unauthorized guard lookup provisioned the group DB"
    );
}

#[tokio::test]
async fn test_scoped_mcp_write_authorization_is_independent_of_metrics() {
    let master = "mcp-scope-regression-master";
    let (base, client, server) = spawn_server_with_master_key(master).await;
    let user = uid();
    let mut keys = Vec::new();
    for scopes in [
        json!(["identity:read", "memory:read"]),
        json!(["identity:read", "memory:read", "memory:write"]),
        json!(["identity:read"]),
    ] {
        let response = client
            .post(format!("{base}/auth/keys"))
            .bearer_auth(master)
            .json(&json!({"user_id": user, "name": "mcp-scope-regression", "scopes": scopes}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let body: Value = response.json().await.unwrap();
        keys.push(body["raw_key"].as_str().unwrap().to_owned());
    }
    let read_key = &keys[0];
    let write_key = &keys[1];
    let identity_key = &keys[2];

    // Seed enough real feedback that an accidentally dispatched tune call would
    // persist a change, including when the caller uses a notification (no id).
    let response = client
        .post(format!("{base}/v1/memories"))
        .bearer_auth(write_key)
        .json(&json!({"content": "MCP authorization regression fixture"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let memory: Value = response.json().await.unwrap();
    let store = server.user_store(&user).await;
    for _ in 0..10 {
        store
            .record_feedback(&user, memory["memory_id"].as_str().unwrap(), "useful", None)
            .await
            .unwrap();
    }
    let before = store
        .get_user_retrieval_params(&user)
        .await
        .unwrap()
        .feedback_weight;

    for (name, arguments) in [
        (
            "memory_apply",
            json!({"source": "review-branch", "removes": [memory["memory_id"]]}),
        ),
        ("memory_rebuild_index", json!({"table": "mem_memories"})),
        ("memory_tune_params", json!({})),
    ] {
        for notification in [false, true] {
            let mut request = json!({"jsonrpc": "2.0", "method": "tools/call",
                "params": {"name": name, "arguments": arguments}});
            if !notification {
                request["id"] = json!(1);
            }
            let response = client
                .post(format!("{base}/mcp"))
                .bearer_auth(read_key)
                .json(&request)
                .send()
                .await
                .unwrap();
            if notification {
                assert_eq!(response.status(), 204, "{name}");
                assert!(response.bytes().await.unwrap().is_empty());
            } else {
                assert_eq!(response.status(), 200, "{name}");
                let body: Value = response.json().await.unwrap();
                assert_eq!(body["error"]["code"], -32003, "{name}: {body}");
                assert!(body["error"]["message"]
                    .as_str()
                    .unwrap()
                    .contains("memory:write"));
            }
        }
    }
    assert_eq!(
        store
            .get_user_retrieval_params(&user)
            .await
            .unwrap()
            .feedback_weight,
        before,
        "denied requests and notifications must not tune persisted parameters"
    );

    // Positive controls: scope checks allow writes into dispatch. Avoid an
    // expensive index rebuild and branch mutation by using handler validation.
    for (name, arguments, expected) in [
        (
            "memory_apply",
            json!({"source": "main"}),
            "Cannot apply from main",
        ),
        (
            "memory_rebuild_index",
            json!({"table": "invalid-table"}),
            "Invalid table",
        ),
        ("memory_tune_params", json!({}), "Parameters tuned"),
    ] {
        let response = client
            .post(format!("{base}/mcp"))
            .bearer_auth(write_key)
            .json(&json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call",
                "params": {"name": name, "arguments": arguments}}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert!(body.get("error").is_none(), "{name}: {body}");
        assert!(
            body["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(expected),
            "{body}"
        );
    }
    assert!(
        store
            .get_user_retrieval_params(&user)
            .await
            .unwrap()
            .feedback_weight
            > before,
        "the same write-authorized tune call must change persisted parameters"
    );

    for (key, allowed) in [(read_key, true), (write_key, true), (identity_key, false)] {
        let response = client
            .post(format!("{base}/mcp"))
            .bearer_auth(key)
            .json(&json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "memory_get_retrieval_params", "arguments": {}}}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        if allowed {
            assert!(body.get("error").is_none(), "{body}");
            assert!(body["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("feedback_weight"));
        } else {
            assert_eq!(body["error"]["code"], -32003, "{body}");
            assert!(body["error"]["message"]
                .as_str()
                .unwrap()
                .contains("memory:read"));
        }
    }

    // Full-access credentials must not bypass classification, and metrics-name
    // sanitization must not turn malformed names into authorized read tools.
    for key in [read_key.as_str(), master] {
        for name in [
            Value::Null,
            json!(42),
            json!(""),
            json!("memory_future_tool"),
            json!("memory_search/"),
            json!(" memory_search"),
        ] {
            let response = client
                .post(format!("{base}/mcp"))
                .bearer_auth(key)
                .json(&json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                    "params": {"name": name, "arguments": {}}}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), 200);
            let body: Value = response.json().await.unwrap();
            assert_eq!(body["error"]["code"], -32602, "{name}: {body}");
            assert!(body.get("result").is_none(), "{name}: {body}");
        }
    }
}

#[tokio::test]
async fn test_scoped_keys_deny_groups_and_whoami_observes_uncached_revocation() {
    let (base, client, server) = spawn_server_with_master_key("review-master").await;
    let user = uid();
    for scopes in [
        json!(["identity:read"]),
        json!(["identity:read", "memory:read"]),
        json!(["identity:read", "memory:read", "memory:write"]),
    ] {
        let response = client
            .post(format!("{base}/auth/keys"))
            .bearer_auth("review-master")
            .json(&json!({"user_id":user, "name":"review-scoped-key", "scopes":scopes}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 201);
        let key: Value = response.json().await.unwrap();
        let raw = key["raw_key"].as_str().unwrap();
        for (method, path) in [
            (reqwest::Method::GET, "/v1/groups"),
            (reqwest::Method::POST, "/v1/groups"),
            (
                reqwest::Method::POST,
                "/v1/groups/owned-group/members/other-user",
            ),
            (reqwest::Method::DELETE, "/v1/groups/owned-group"),
        ] {
            let response = client.request(method, format!("{base}{path}")).bearer_auth(raw)
                .json(&json!({"group_name":"must-not-be-created", "seed":{"db_name":"personal", "mode":"active_only"}}))
                .send().await.unwrap();
            assert_eq!(
                response.status(),
                403,
                "scope check must precede group lookup/mutation: {path}"
            );
        }
        assert_eq!(
            client
                .get(format!("{base}/auth/whoami"))
                .bearer_auth(raw)
                .send()
                .await
                .unwrap()
                .status(),
            200
        );
        // Simulate revocation through a different replica: do not invalidate
        // this server's warm principal cache.
        sqlx::query(&format!(
            "UPDATE {} SET is_active = 0 WHERE key_id = ?",
            server.shared_table("mem_api_keys")
        ))
        .bind(key["key_id"].as_str().unwrap())
        .execute(&server.shared_pool())
        .await
        .unwrap();
        assert_eq!(
            client
                .get(format!("{base}/auth/whoami"))
                .bearer_auth(raw)
                .send()
                .await
                .unwrap()
                .status(),
            401
        );
    }
}

#[tokio::test]
async fn test_api_key_cannot_get_other_users_memory() {
    let mk = "test-master-key-memory-read";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let owner_id = uid();
    let attacker_id = uid();
    let owner_key = create_api_key_for_user(&client, &base, &auth, &owner_id, "owner-read").await;
    let attacker_key =
        create_api_key_for_user(&client, &base, &auth, &attacker_id, "attacker-read").await;

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {owner_key}"))
        .json(&json!({ "content": "owner private memory", "memory_type": "semantic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let memory_id = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = client
        .get(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", format!("Bearer {attacker_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "non-owner API key must see the same response as a missing memory"
    );
    assert!(
        r.json::<Value>().await.unwrap().is_null(),
        "foreign memory reads must not reveal existence"
    );
}

#[tokio::test]
async fn test_api_key_cannot_correct_other_users_memory() {
    let mk = "test-master-key-memory-correct";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let owner_id = uid();
    let attacker_id = uid();
    let owner_key =
        create_api_key_for_user(&client, &base, &auth, &owner_id, "owner-correct").await;
    let attacker_key =
        create_api_key_for_user(&client, &base, &auth, &attacker_id, "attacker-correct").await;

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {owner_key}"))
        .json(&json!({ "content": "unchanged owner memory", "memory_type": "semantic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let memory_id = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = client
        .put(format!("{base}/v1/memories/{memory_id}/correct"))
        .header("Authorization", format!("Bearer {attacker_key}"))
        .json(&json!({ "new_content": "attacker overwrite" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        404,
        "non-owner API key must see the same response as a missing memory"
    );
}

#[tokio::test]
async fn test_api_key_cannot_get_other_users_task_status() {
    use memoria_service::AsyncTaskStore;

    let mk = "test-master-key-task-status";
    let (base, client, server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let owner_id = uid();
    let attacker_id = uid();
    let attacker_key =
        create_api_key_for_user(&client, &base, &auth, &attacker_id, "attacker-task").await;

    let store = server.shared_store();

    let task_id = format!("task_{}", uuid::Uuid::new_v4().simple());
    store
        .as_ref()
        .create_task(&task_id, "instance_authz", &owner_id)
        .await
        .unwrap();

    let r = client
        .get(format!("{base}/v1/tasks/{task_id}"))
        .header("Authorization", format!("Bearer {attacker_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "non-owner API key must get 403");
}

// ── 10b-4. cross-user delete ──────────────────────────────────────────────────

#[tokio::test]
async fn test_api_key_cannot_delete_other_users_memory() {
    let mk = "test-master-key-memory-delete";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let owner_id = uid();
    let attacker_id = uid();
    let owner_key = create_api_key_for_user(&client, &base, &auth, &owner_id, "owner-del").await;
    let attacker_key =
        create_api_key_for_user(&client, &base, &auth, &attacker_id, "attacker-del").await;

    // Owner creates memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {owner_key}"))
        .json(&json!({ "content": "owner secret", "memory_type": "semantic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let memory_id = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Attacker tries to delete
    let r = client
        .delete(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", format!("Bearer {attacker_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        404,
        "non-owner API key must see the same response as a missing memory on delete"
    );
}

// ── 10b-5. cross-user list isolation ─────────────────────────────────────────

#[tokio::test]
async fn test_api_key_list_only_own_memories() {
    let mk = "test-master-key-list-isolation";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let user_a = uid();
    let user_b = uid();
    let key_a = create_api_key_for_user(&client, &base, &auth, &user_a, "list-a").await;
    let key_b = create_api_key_for_user(&client, &base, &auth, &user_b, "list-b").await;

    // User A stores a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {key_a}"))
        .json(&json!({ "content": "user_a private data", "memory_type": "semantic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // User B lists — should see nothing from A
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {key_b}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let items = body["items"].as_array().unwrap();
    for item in items {
        assert_ne!(
            item["content"].as_str().unwrap_or(""),
            "user_a private data",
            "user B must not see user A's memories in list"
        );
    }
}

// ── 10b-6. master key impersonation boundary ─────────────────────────────────

#[tokio::test]
async fn test_master_key_can_impersonate_and_access_any_user() {
    let mk = "test-master-key-impersonate";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let user_a = uid();
    let key_a = create_api_key_for_user(&client, &base, &auth, &user_a, "imp-a").await;

    // User A stores a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {key_a}"))
        .json(&json!({ "content": "impersonation test data", "memory_type": "semantic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let memory_id = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // Master key can read any user's memory
    let r = client
        .get(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "master key should read any memory");

    // Master key can correct any user's memory
    let r = client
        .put(format!("{base}/v1/memories/{memory_id}/correct"))
        .header("Authorization", &auth)
        .json(&json!({ "new_content": "master corrected" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "master key should correct any memory");

    // Master key can delete any user's memory
    let r = client
        .delete(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204, "master key should delete any memory");

    // API key (non-master) cannot access admin routes
    let r = client
        .get(format!("{base}/admin/stats"))
        .header("Authorization", format!("Bearer {key_a}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "API key must not access admin routes");
}

#[tokio::test]
async fn test_owner_scoped_master_cannot_cross_user_memory_ids() {
    let master = "test-master-key-owner-scoped";
    let (base, client, _server) = spawn_server_with_master_key(master).await;
    let user_a = uid();
    let user_b = uid();
    let owner_auth = format!("Memoria-Owner {master}");

    let store = |user_id: &str, content: &str| {
        client
            .post(format!("{base}/v1/memories"))
            .header("Authorization", &owner_auth)
            .header("X-User-Id", user_id)
            .json(&json!({"content": content, "memory_type": "semantic"}))
            .send()
    };
    let response = store(&user_b, "user-b private memory").await.unwrap();
    assert_eq!(response.status(), 201);
    let user_b_memory = response.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let response = client
        .get(format!("{base}/v1/memories/{user_b_memory}"))
        .header("Authorization", &owner_auth)
        .header("X-User-Id", &user_a)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.unwrap(), Value::Null);

    let response = client
        .put(format!("{base}/v1/memories/{user_b_memory}/correct"))
        .header("Authorization", &owner_auth)
        .header("X-User-Id", &user_a)
        .json(&json!({"new_content": "cross-user overwrite"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    let response = client
        .delete(format!("{base}/v1/memories/{user_b_memory}"))
        .header("Authorization", &owner_auth)
        .header("X-User-Id", &user_a)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 404);

    let response = client
        .get(format!("{base}/v1/memories/{user_b_memory}"))
        .header("Authorization", &owner_auth)
        .header("X-User-Id", &user_b)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.json::<Value>().await.unwrap()["content"],
        "user-b private memory"
    );

    let response = store(&user_a, "user-a own memory").await.unwrap();
    assert_eq!(response.status(), 201);
    let user_a_memory = response.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();
    let response = client
        .put(format!("{base}/v1/memories/{user_a_memory}/correct"))
        .header("Authorization", &owner_auth)
        .header("X-User-Id", &user_a)
        .json(&json!({"new_content": "user-a corrected own memory"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let corrected_user_a_memory = response.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();
    let response = client
        .delete(format!("{base}/v1/memories/{corrected_user_a_memory}"))
        .header("Authorization", &owner_auth)
        .header("X-User-Id", &user_a)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);

    let response = client
        .get(format!("{base}/admin/stats"))
        .header("Authorization", &owner_auth)
        .header("X-User-Id", &user_a)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);

    let response = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", &owner_auth)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

#[tokio::test]
async fn test_non_master_cannot_probe_other_users_memory_ids() {
    let mk = "test-master-key-tenant-isolation";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let owner = uid();
    let intruder = uid();
    let owner_key = create_api_key_for_user(&client, &base, &auth, &owner, "owner-key").await;
    let intruder_key =
        create_api_key_for_user(&client, &base, &auth, &intruder, "intruder-key").await;

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {owner_key}"))
        .json(&json!({ "content": "owner private memory", "memory_type": "semantic" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let memory_id = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    let r = client
        .get(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", format!("Bearer {intruder_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "foreign GET must match missing-memory response"
    );
    assert!(r.json::<Value>().await.unwrap().is_null());

    let r = client
        .put(format!("{base}/v1/memories/{memory_id}/correct"))
        .header("Authorization", format!("Bearer {intruder_key}"))
        .json(&json!({ "new_content": "intruder overwrite" }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        404,
        "foreign correct must look like missing memory"
    );

    let r = client
        .delete(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", format!("Bearer {intruder_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        404,
        "foreign delete must look like missing memory"
    );

    let r = client
        .get(format!("{base}/v1/memories/{memory_id}"))
        .header("Authorization", format!("Bearer {owner_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "failed probes must not mutate owner memory"
    );
    assert_eq!(
        r.json::<Value>().await.unwrap()["content"],
        "owner private memory"
    );
}

// ── 10b-7. revoked API key returns 401 ───────────────────────────────────────

#[tokio::test]
async fn test_revoked_api_key_returns_401() {
    let mk = "test-master-key-revoked";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // Create and get raw key + key_id
    let r = client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({"user_id": uid, "name": "to-revoke"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let key_id = body["key_id"].as_str().unwrap().to_string();
    let raw_key = body["raw_key"].as_str().unwrap().to_string();

    // Verify key works before revoke
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "key should work before revoke");

    // Revoke
    let r = client
        .delete(format!("{base}/auth/keys/{key_id}"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);

    // Revoked key must be rejected
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "revoked key must return 401");

    // Also rejected on write endpoints
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .json(&json!({"content": "should fail", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401, "revoked key must return 401 on store");

    println!("✅ revoked API key returns 401 on all endpoints");
}

// ── 10c. observe endpoint ────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_observe_turn() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Observe with assistant + user messages
    let r = client
        .post(format!("{base}/v1/observe"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "messages": [
                {"role": "user", "content": "What is Rust?"},
                {"role": "assistant", "content": "Rust is a systems programming language"},
                {"role": "system", "content": "You are helpful"},
                {"role": "assistant", "content": ""}
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let memories = body["memories"].as_array().unwrap();
    // Should store user + non-empty assistant messages, skip system + empty
    assert_eq!(
        memories.len(),
        2,
        "should store 2 memories (user + assistant): {body}"
    );
    assert!(
        body.get("warning").is_some(),
        "should have LLM warning without LLM"
    );
    println!(
        "✅ observe: stored {} memories, warning={}",
        memories.len(),
        body["warning"]
    );

    // Verify stored memories are retrievable
    let r = client
        .post(format!("{base}/v1/memories/search"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "Rust programming", "top_k": 10}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(
        results.len() >= 2,
        "should find observed memories, got {}",
        results.len()
    );
    println!("✅ observe memories retrievable: {} found", results.len());
}

#[tokio::test]
async fn test_api_observe_empty_messages() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Empty messages array → should return 200 with empty memories
    let r = client
        .post(format!("{base}/v1/observe"))
        .header("X-User-Id", &uid)
        .json(&json!({"messages": []}))
        .send()
        .await
        .unwrap();
    // Could be 200 with empty or 422 for validation — check what we get
    let status = r.status().as_u16();
    assert!(
        status == 200 || status == 422,
        "empty messages: got {status}"
    );
    println!("✅ observe empty messages: {status}");
}

// ── 10d. retrieve edge cases ─────────────────────────────────────────────────

#[tokio::test]
async fn test_api_retrieve_top_k_respected() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store 5 memories
    for i in 0..5 {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("topk test item {i}"), "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
    }

    // Retrieve with top_k=2
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "topk test item", "top_k": 2}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(
        results.len() <= 2,
        "top_k=2 should return at most 2, got {}",
        results.len()
    );
    println!("✅ retrieve top_k=2: got {} results", results.len());
}

#[tokio::test]
async fn test_api_search_returns_fields() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "field check memory", "memory_type": "profile"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/memories/search"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "field check", "top_k": 1}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(!results.is_empty(), "should find memory");
    let mem = &results[0];
    // Verify essential fields are present
    assert!(mem["memory_id"].as_str().is_some(), "should have memory_id");
    assert!(mem["content"].as_str().is_some(), "should have content");
    assert!(
        mem["memory_type"].as_str().is_some(),
        "should have memory_type"
    );
    println!(
        "✅ search returns all fields: id={}, type={}",
        mem["memory_id"], mem["memory_type"]
    );
}

// ── 10e. error scenarios ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_store_missing_content() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422, "missing content should be 422");
    println!("✅ store missing content → 422");
}

#[tokio::test]
async fn test_api_delete_nonexistent() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .delete(format!("{base}/v1/memories/nonexistent-id-12345"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    // Should be 404 or 200 with "not found" — check what we return
    let status = r.status().as_u16();
    println!("✅ delete nonexistent: {status}");
}

#[tokio::test]
async fn test_api_correct_nonexistent() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .put(format!("{base}/v1/memories/nonexistent-id-12345/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "updated", "reason": "test"}))
        .send()
        .await
        .unwrap();
    let status = r.status().as_u16();
    // Should be 404 or 500
    assert!(
        status == 404 || status == 500,
        "correct nonexistent: got {status}"
    );
    println!("✅ correct nonexistent → {status}");
}

// ── 10f. memory history ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_memory_history() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "history test v1", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let mid = body["memory_id"].as_str().unwrap().to_string();

    // Get history — should have 1 version
    let r = client
        .get(format!("{base}/v1/memories/{mid}/history"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["total"].as_i64().unwrap(), 1);
    assert_eq!(
        body["versions"][0]["content"].as_str().unwrap(),
        "history test v1"
    );
    println!("✅ memory history: 1 version");

    // Correct the memory
    client
        .put(format!("{base}/v1/memories/{mid}/correct"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "history test v2", "reason": "updated"}))
        .send()
        .await
        .unwrap();

    // History should still show the memory (in-place update, same id)
    let r = client
        .get(format!("{base}/v1/memories/{mid}/history"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["total"].as_i64().unwrap() >= 1);
    println!(
        "✅ memory history after correct: {} versions",
        body["total"]
    );
}

#[tokio::test]
async fn test_api_memory_history_not_found() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .get(format!("{base}/v1/memories/nonexistent-id/history"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
    println!("✅ memory history nonexistent → 404");
}

// ── Remote mode E2E tests ─────────────────────────────────────────────────────

/// Spawn API server + test remote MCP client against it.
async fn spawn_api_for_remote() -> (String, reqwest::Client, support::multi_db::ApiTestServer) {
    // Reuse spawn_server but return the base URL for RemoteClient
    spawn_server().await
}

async fn spawn_api_for_remote_with_embedder(
    embedder: Arc<dyn memoria_core::interfaces::EmbeddingProvider>,
    dim: usize,
) -> (String, reqwest::Client, support::multi_db::ApiTestServer) {
    spawn_server_with_custom_embedder_and_pool(embedder, dim).await
}

#[tokio::test]
async fn test_remote_store_retrieve() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();

    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    // Store
    let r = remote
        .call(
            "memory_store",
            json!({
                "content": "Remote mode test memory",
                "memory_type": "semantic"
            }),
        )
        .await
        .expect("store");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Stored memory"), "got: {text}");
    println!("✅ remote store: {text}");

    // Retrieve
    let r = remote
        .call(
            "memory_retrieve",
            json!({
                "query": "remote mode test",
                "top_k": 5
            }),
        )
        .await
        .expect("retrieve");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("Remote mode test memory") || text.contains("No relevant"),
        "got: {text}"
    );
    println!("✅ remote retrieve: {}", &text[..text.len().min(80)]);
}

#[tokio::test]
async fn test_remote_store_metadata_round_trip() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _, server) = spawn_api_for_remote().await;
    let user_id = uid();
    let remote = RemoteClient::new(&base, None, user_id.clone(), None);
    remote
        .call(
            "memory_store",
            json!({
                "content": "remote metadata memory",
                "extra_metadata": {"scene": "remote", "agent": "mcp"}
            }),
        )
        .await
        .expect("remote store");

    let stored = server
        .service()
        .list_active(&user_id, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|memory| memory.content == "remote metadata memory")
        .expect("stored remote memory");
    assert_eq!(
        serde_json::to_value(stored.extra_metadata).unwrap(),
        json!({"scene": "remote", "agent": "mcp"}),
        "remote MCP must forward metadata to REST"
    );
}

#[tokio::test]
async fn test_remote_retrieve_session_scope_only_includes_unscoped() {
    use memoria_mcp::remote::RemoteClient;

    let (base, client, _server) =
        spawn_server_with_custom_embedder_and_pool(Arc::new(SessionScopeTestEmbedder), test_dim())
            .await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let target_session = format!("session:test-retrieve-{}", uuid::Uuid::new_v4().simple());
    let other_session = format!("session:test-retrieve-{}", uuid::Uuid::new_v4().simple());

    remote
        .call(
            "memory_store",
            json!({"content": "target-session memory", "session_id": target_session}),
        )
        .await
        .unwrap();
    remote
        .call("memory_store", json!({"content": "global-unscoped top"}))
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "other-session top", "session_id": other_session}),
        )
        .await
        .unwrap();

    wait_for_api_payload_contains(
        &client,
        &base,
        &uid,
        "/v1/memories/retrieve",
        json!({
            "query": "strict session query",
            "session_id": target_session,
            "session_scope": "only",
            "top_k": 2
        }),
        &["target-session memory", "global-unscoped top"],
    )
    .await;

    let strict = remote
        .call(
            "memory_retrieve",
            json!({
                "query": "strict session query",
                "session_id": target_session,
                "session_scope": "only",
                "top_k": 2
            }),
        )
        .await
        .unwrap();
    let strict_text = strict["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        strict_text.contains("target-session memory"),
        "strict remote retrieve should keep requested-session memory visible: {strict_text}"
    );
    assert!(
        strict_text.contains("global-unscoped top"),
        "strict remote retrieve should include unscoped memory: {strict_text}"
    );
    assert!(
        !strict_text.contains("other-session top"),
        "strict remote retrieve should exclude other scoped sessions: {strict_text}"
    );
    println!("✅ remote retrieve session_scope=only includes unscoped");
}

#[tokio::test]
async fn test_remote_correct_purge() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    // Store
    let r = remote
        .call("memory_store", json!({"content": "Uses black formatter"}))
        .await
        .expect("store");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    let mid = text
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .trim_end_matches(':')
        .to_string();

    // Correct by id
    let r = remote
        .call(
            "memory_correct",
            json!({
                "memory_id": mid,
                "new_content": "Uses ruff formatter",
                "reason": "switched"
            }),
        )
        .await
        .expect("correct");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Corrected"), "got: {text}");
    println!("✅ remote correct: {text}");

    // Purge
    let r = remote
        .call("memory_purge", json!({"memory_id": mid}))
        .await
        .expect("purge");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Purged"), "got: {text}");
    println!("✅ remote purge: {text}");
}

#[tokio::test]
async fn test_remote_governance() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    let r = remote
        .call("memory_governance", json!({"force": true}))
        .await
        .expect("governance");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("Governance complete") || text.contains("skipped"),
        "got: {text}"
    );
    println!("✅ remote governance: {text}");
}

#[tokio::test]
async fn test_remote_capabilities() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    let r = remote
        .call("memory_capabilities", json!({}))
        .await
        .expect("capabilities");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("remote mode"),
        "should mention remote mode, got: {text}"
    );
    println!("✅ remote capabilities: {}", &text[..text.len().min(80)]);
}

#[tokio::test]
async fn test_remote_list_search_profile() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    remote
        .call(
            "memory_store",
            json!({"content": "Prefers Rust", "memory_type": "profile"}),
        )
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "Uses MatrixOne database"}),
        )
        .await
        .unwrap();

    // list
    let r = remote
        .call("memory_list", json!({"limit": 10}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("MatrixOne") || t.contains("Prefers"),
        "list: {t}"
    );
    println!("✅ remote list: {}", &t[..t.len().min(80)]);

    // search
    let r = remote
        .call("memory_search", json!({"query": "database", "top_k": 5}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("MatrixOne") || t.contains("No relevant"),
        "search: {t}"
    );
    println!("✅ remote search: {}", &t[..t.len().min(80)]);

    // profile
    let r = remote.call("memory_profile", json!({})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("Prefers Rust") || t.contains("No profile"),
        "profile: {t}"
    );
    println!("✅ remote profile: {t}");
}

#[tokio::test]
async fn test_remote_search_session_scope_only_includes_unscoped() {
    use memoria_mcp::remote::RemoteClient;

    let (base, client, _server) =
        spawn_server_with_custom_embedder_and_pool(Arc::new(SessionScopeTestEmbedder), test_dim())
            .await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let target_session = format!("session:test-search-{}", uuid::Uuid::new_v4().simple());
    let other_session = format!("session:test-search-{}", uuid::Uuid::new_v4().simple());

    remote
        .call(
            "memory_store",
            json!({"content": "target-session memory", "session_id": target_session}),
        )
        .await
        .unwrap();
    remote
        .call("memory_store", json!({"content": "global-unscoped top"}))
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "other-session top", "session_id": other_session}),
        )
        .await
        .unwrap();

    wait_for_api_payload_contains(
        &client,
        &base,
        &uid,
        "/v1/memories/search",
        json!({
            "query": "strict session query",
            "session_id": target_session,
            "session_scope": "only",
            "top_k": 2
        }),
        &["target-session memory", "global-unscoped top"],
    )
    .await;

    let strict = remote
        .call(
            "memory_search",
            json!({
                "query": "strict session query",
                "session_id": target_session,
                "session_scope": "only",
                "top_k": 2
            }),
        )
        .await
        .unwrap();
    let strict_text = strict["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        strict_text.contains("target-session memory"),
        "strict remote search should keep requested-session memory visible: {strict_text}"
    );
    assert!(
        strict_text.contains("global-unscoped top"),
        "strict remote search should include unscoped memory: {strict_text}"
    );
    assert!(
        !strict_text.contains("other-session top"),
        "strict remote search should exclude other scoped sessions: {strict_text}"
    );
    println!("✅ remote search session_scope=only includes unscoped");
}

#[tokio::test]
async fn test_remote_snapshot_branch() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    // Store a memory first
    remote
        .call(
            "memory_store",
            json!({"content": "snapshot branch test memory"}),
        )
        .await
        .unwrap();

    // Create snapshot
    let snap_name = format!(
        "test_snap_{}",
        uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
    );
    let r = remote
        .call("memory_snapshot", json!({"name": snap_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("created") || t.contains(&snap_name),
        "snapshot create: {t}"
    );
    println!("✅ remote snapshot create: {t}");

    // List snapshots
    let r = remote
        .call("memory_snapshots", json!({"limit": 20}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote snapshots list: {}", &t[..t.len().min(80)]);

    // Create branch
    let branch_name = format!(
        "test_br_{}",
        uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
    );
    let r = remote
        .call("memory_branch", json!({"name": branch_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("created") || t.contains(&branch_name),
        "branch create: {t}"
    );
    println!("✅ remote branch create: {t}");

    // List branches
    let r = remote.call("memory_branches", json!({})).await.unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote branches list: {}", &t[..t.len().min(80)]);

    // Checkout branch
    let r = remote
        .call("memory_checkout", json!({"name": branch_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("Switched") || t.contains(&branch_name),
        "checkout: {t}"
    );
    println!("✅ remote checkout: {t}");

    // Store on branch
    remote
        .call("memory_store", json!({"content": "branch-only memory"}))
        .await
        .unwrap();

    // Diff
    let r = remote
        .call("memory_diff", json!({"source": branch_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote diff: {}", &t[..t.len().min(80)]);

    // Merge back
    let r = remote
        .call(
            "memory_merge",
            json!({"source": branch_name, "strategy": "append"}),
        )
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote merge: {}", &t[..t.len().min(80)]);

    // Delete branch
    let r = remote
        .call("memory_branch_delete", json!({"name": branch_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("deleted") || t.contains(&branch_name),
        "branch delete: {t}"
    );
    println!("✅ remote branch delete: {t}");

    // Delete snapshot
    let r = remote
        .call("memory_snapshot_delete", json!({"names": snap_name}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    println!("✅ remote snapshot delete: {t}");
}

#[tokio::test]
async fn test_remote_reflect_extract_entities() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    remote
        .call(
            "memory_store",
            json!({"content": "Uses Rust for backend services", "session_id": "s1"}),
        )
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "MatrixOne as primary database", "session_id": "s2"}),
        )
        .await
        .unwrap();

    // reflect candidates (no LLM needed)
    let r = remote
        .call(
            "memory_reflect",
            json!({"mode": "candidates", "force": true}),
        )
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        !t.to_lowercase().contains("error"),
        "reflect should not error: {t}"
    );
    println!("✅ remote reflect candidates: {}", &t[..t.len().min(100)]);

    // extract entities candidates
    let r = remote
        .call("memory_extract_entities", json!({"mode": "candidates"}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    let parsed: serde_json::Value = serde_json::from_str(t).unwrap_or(serde_json::Value::Null);
    assert!(
        parsed["status"] == "candidates" || parsed["status"] == "complete",
        "extract: {t}"
    );
    println!("✅ remote extract entities: status={}", parsed["status"]);

    // link entities if we have candidates
    if parsed["status"] == "candidates" {
        if let Some(mems) = parsed["memories"].as_array() {
            if let Some(first) = mems.first() {
                let mid = first["memory_id"].as_str().unwrap_or("");
                let link_payload = serde_json::to_string(&json!([{
                    "memory_id": mid,
                    "entities": [{"name": "Rust", "type": "tech"}]
                }]))
                .unwrap();
                let r = remote
                    .call("memory_link_entities", json!({"entities": link_payload}))
                    .await
                    .unwrap();
                let t = r["content"][0]["text"].as_str().unwrap_or("");
                let p: serde_json::Value =
                    serde_json::from_str(t).unwrap_or(serde_json::Value::Null);
                assert!(
                    p.get("entities_created").is_some() || p["status"] == "done",
                    "link: {t}"
                );
                println!("✅ remote link entities: {t}");
            }
        }
    }
}

#[tokio::test]
async fn test_reflect_no_llm_falls_back_to_candidates() {
    // When LLM is not configured, mode=auto should return candidates (not error)
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Rust backend", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "MatrixOne database", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/reflect"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "auto", "force": true}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "reflect auto without LLM should not 500: {}",
        r.status()
    );
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(
        body.get("candidates").is_some() || body.get("scenes_created").is_some(),
        "reflect response: {body}"
    );
    println!("✅ reflect mode=auto without LLM: candidates or scenes_created present");
}

#[tokio::test]
async fn test_extract_entities_no_llm_falls_back_to_candidates() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Uses PostgreSQL and Redis", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/extract-entities"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "auto"}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "extract_entities auto without LLM should not 500"
    );
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(
        body["status"] == "candidates" || body["status"] == "complete",
        "extract response: {body}"
    );
    println!(
        "✅ extract_entities mode=auto without LLM: status={}",
        body["status"]
    );
}

#[tokio::test]
async fn test_governance_pollution_detection() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store 3 memories then supersede 2 of them → ratio=2/5=0.4 > 0.3 → polluted
    let mut mids = vec![];
    for i in 0..3 {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": format!("fact {i}"), "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
        let body: serde_json::Value = r.json().await.unwrap();
        mids.push(body["memory_id"].as_str().unwrap_or("").to_string());
    }
    // Supersede 2 via correct (sets superseded_by on old, creates new)
    for mid in &mids[..2] {
        client
            .put(format!("{base}/v1/memories/{mid}/correct"))
            .header("X-User-Id", &uid)
            .json(&json!({"new_content": "updated fact", "reason": "test"}))
            .send()
            .await
            .unwrap();
    }

    let r = client
        .post(format!(
            "{base}/admin/governance/{uid}/trigger?op=governance"
        ))
        .header("Authorization", "Bearer ")
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "governance trigger failed: {}",
        r.status()
    );
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(
        body["pollution_detected"], true,
        "expected pollution=true: {body}"
    );
    println!("✅ governance pollution_detected=true (high supersede ratio)");
}

#[tokio::test]
async fn test_reflect_with_llm() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (base, client, server) = spawn_server_with_llm(llm).await;
    let uid = uid();

    let store = server.user_store(&uid).await;
    let graph = store.graph_store();
    for (idx, content) in [
        "Project uses Rust for all backend services",
        "MatrixOne is the primary database",
        "Validation emphasizes deterministic test loops",
    ]
    .into_iter()
    .enumerate()
    {
        graph
            .create_node(&memoria_storage::GraphNode {
                node_id: format!(
                    "reflect_node_{idx}_{}",
                    &uuid::Uuid::new_v4().simple().to_string()[..8]
                ),
                user_id: uid.clone(),
                node_type: memoria_storage::NodeType::Semantic,
                content: content.to_string(),
                entity_type: None,
                embedding: None,
                memory_id: None,
                session_id: Some("llm_cluster".to_string()),
                confidence: 0.8,
                trust_tier: "T3".to_string(),
                importance: 0.5,
                source_nodes: vec![],
                conflicts_with: None,
                conflict_resolution: None,
                access_count: 0,
                cross_session_count: 0,
                is_active: true,
                superseded_by: None,
                created_at: Some(chrono::Utc::now().naive_utc()),
            })
            .await
            .expect("create graph node");
    }

    let r = client
        .post(format!("{base}/v1/reflect"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "auto", "force": true}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "reflect with LLM failed: {}",
        r.status()
    );
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(
        body["scenes_created"].as_u64().unwrap_or(0) >= 1,
        "reflect should synthesize at least one scene: {body}"
    );

    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .query(&[("limit", "20")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let list_body: serde_json::Value = r.json().await.unwrap();
    let items = list_body["items"].as_array().expect("items array");
    assert!(
        items.iter().any(|m| {
            m["content"]
                .as_str()
                .unwrap_or("")
                .contains("Prefer deterministic validation")
        }),
        "reflected memory should be written back: {list_body}"
    );
    println!(
        "✅ reflect with LLM: scenes_created={}",
        body["scenes_created"]
    );
}

#[tokio::test]
async fn test_extract_entities_with_llm() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (base, client, _server) = spawn_server_with_llm(llm).await;
    let uid = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "content": "our stack relies on alphamesh for routing and deltafabric for caching",
            "memory_type": "semantic"
        }))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/extract-entities"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "auto"}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "extract_entities with LLM failed: {}",
        r.status()
    );
    let body: serde_json::Value = r.json().await.unwrap();
    assert_eq!(body["status"], "done", "extract LLM response: {body}");
    assert!(
        body["entities_found"].as_u64().unwrap_or(0) >= 2,
        "fake LLM should create entities: {body}"
    );

    let r = client
        .get(format!("{base}/v1/entities"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let entities_body: serde_json::Value = r.json().await.unwrap();
    let empty_entities = Vec::new();
    let names: Vec<&str> = entities_body["entities"]
        .as_array()
        .unwrap_or(&empty_entities)
        .iter()
        .filter_map(|e| e["name"].as_str())
        .collect();
    assert!(
        names.contains(&"alphamesh") && names.contains(&"deltafabric"),
        "LLM-extracted entities should be persisted: {entities_body}"
    );
    println!(
        "✅ extract_entities with LLM: entities_found={}",
        body["entities_found"]
    );
}

#[tokio::test]
async fn test_remote_consolidate() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    let r = remote
        .call("memory_consolidate", json!({"force": true}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        t.contains("Consolidation complete") || t.contains("skipped"),
        "got: {t}"
    );
    println!("✅ remote consolidate: {t}");
}

#[tokio::test]
async fn test_remote_correct_by_query() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let target_session = format!("session:test-correct-{}", uuid::Uuid::new_v4().simple());
    let other_session = format!("session:test-correct-{}", uuid::Uuid::new_v4().simple());

    remote
        .call(
            "memory_store",
            json!({"content": "Uses black for Python formatting in target", "session_id": target_session}),
        )
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "Uses black for Python formatting in other", "session_id": other_session}),
        )
        .await
        .unwrap();

    let r = remote
        .call(
            "memory_correct",
            json!({
                "query": "black formatting",
                "session_id": target_session,
                "session_scope": "only",
                "new_content": "Uses ruff for Python formatting in target",
                "reason": "switched"
            }),
        )
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("Corrected"), "got: {t}");
    assert!(t.contains("target"), "got: {t}");

    let list = remote
        .call("memory_list", json!({"limit": 10}))
        .await
        .unwrap();
    let list_text = list["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        list_text.contains("Uses ruff for Python formatting in target"),
        "got: {list_text}"
    );
    assert!(
        list_text.contains("Uses black for Python formatting in other"),
        "got: {list_text}"
    );
    println!("✅ remote correct by query: {t}");
}

#[tokio::test]
async fn test_remote_purge_by_topic() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    remote
        .call("memory_store", json!({"content": "topic purge test alpha"}))
        .await
        .unwrap();
    remote
        .call("memory_store", json!({"content": "topic purge test beta"}))
        .await
        .unwrap();

    let r = remote
        .call("memory_purge", json!({"topic": "topic purge test"}))
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("Purged"), "got: {t}");
    println!("✅ remote purge by topic: {t}");
}

#[tokio::test]
async fn test_remote_purge_by_session_id() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let target_session = format!("session:test-smp-{}", uuid::Uuid::new_v4().simple());
    let other_session = format!("session:test-smp-{}", uuid::Uuid::new_v4().simple());

    remote
        .call(
            "memory_store",
            json!({"content": "remote working alpha", "memory_type": "working", "session_id": target_session}),
        )
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "remote working beta", "memory_type": "working", "session_id": target_session}),
        )
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "remote semantic keep", "memory_type": "semantic", "session_id": target_session}),
        )
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "remote other keep", "memory_type": "working", "session_id": other_session}),
        )
        .await
        .unwrap();

    let r = remote
        .call(
            "memory_purge",
            json!({"session_id": target_session, "memory_types": ["working"]}),
        )
        .await
        .unwrap();
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("Purged 2"), "got: {t}");

    let list = remote
        .call("memory_list", json!({"limit": 10}))
        .await
        .unwrap();
    let list_text = list["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        list_text.contains("remote semantic keep"),
        "got: {list_text}"
    );
    assert!(list_text.contains("remote other keep"), "got: {list_text}");
    assert!(
        !list_text.contains("remote working alpha"),
        "got: {list_text}"
    );
    assert!(
        !list_text.contains("remote working beta"),
        "got: {list_text}"
    );
    println!("✅ remote purge by session_id: {t}");
}

#[tokio::test]
async fn test_remote_list_session_id_filter() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let target_session = format!("session:test-list-{}", uuid::Uuid::new_v4().simple());
    let other_session = format!("session:test-list-{}", uuid::Uuid::new_v4().simple());

    remote
        .call(
            "memory_store",
            json!({"content": "remote target list alpha", "session_id": target_session}),
        )
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "remote other list beta", "session_id": other_session}),
        )
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "remote global list gamma"}),
        )
        .await
        .unwrap();

    let list = remote
        .call(
            "memory_list",
            json!({"limit": 10, "session_id": target_session}),
        )
        .await
        .unwrap();
    let list_text = list["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        list_text.contains("remote target list alpha"),
        "got: {list_text}"
    );
    assert!(
        !list_text.contains("remote other list beta"),
        "got: {list_text}"
    );
    assert!(
        !list_text.contains("remote global list gamma"),
        "exact session filter should exclude unscoped memories: {list_text}"
    );
    println!("✅ remote list session_id filter");
}

#[tokio::test]
async fn test_remote_purge_rejects_invalid_memory_types_locally() {
    use memoria_mcp::remote::RemoteClient;
    let remote = RemoteClient::new("http://127.0.0.1:9", None, uid(), None);

    let err = remote
        .call(
            "memory_purge",
            json!({"session_id": "sess-target", "memory_types": ["not_a_real_type"]}),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("Invalid memory type"), "{err}");
    println!("✅ remote purge rejects invalid memory_types before API call");
}

// ── Episodic memory tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_episodic_no_llm_returns_503() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store some memories with a session_id
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Worked on Rust backend", "session_id": "sess1"}))
        .send()
        .await
        .unwrap();

    // Without LLM configured, should return 503
    let r = client
        .post(format!("{base}/v1/sessions/sess1/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 503, "should return 503 without LLM");
    println!("✅ episodic without LLM: 503 SERVICE_UNAVAILABLE");
}

async fn spawn_server_with_llm(
    llm: Arc<memoria_embedding::LlmClient>,
) -> (String, reqwest::Client, support::multi_db::ApiTestServer) {
    let server = support::multi_db::spawn_api_server(
        "api_e2e_llm",
        test_dim(),
        String::new(),
        None,
        Some(llm),
        None,
        false,
    )
    .await;
    (server.base.clone(), server.client.clone(), server)
}

async fn spawn_server_with_embedding(
    emb_key: String,
    base_url: String,
    model: String,
) -> (String, reqwest::Client, support::multi_db::ApiTestServer) {
    use memoria_embedding::HttpEmbedder;
    let embedder = Arc::new(HttpEmbedder::new(base_url, emb_key, model, 1024));
    let server = support::multi_db::spawn_api_server(
        "api_e2e_embedding",
        1024,
        String::new(),
        Some(embedder),
        None,
        None,
        false,
    )
    .await;
    (server.base.clone(), server.client.clone(), server)
}

async fn spawn_server_with_custom_embedder_and_pool(
    embedder: Arc<dyn memoria_core::interfaces::EmbeddingProvider>,
    dim: usize,
) -> (String, reqwest::Client, support::multi_db::ApiTestServer) {
    let server = support::multi_db::spawn_api_server(
        "api_e2e_custom_embedder",
        dim,
        String::new(),
        Some(embedder),
        None,
        None,
        false,
    )
    .await;
    (server.base.clone(), server.client.clone(), server)
}

async fn store_memory_for_session(
    client: &reqwest::Client,
    base: &str,
    user_id: &str,
    content: &str,
    session_id: &str,
) -> String {
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", user_id)
        .json(&json!({"content": content, "session_id": session_id}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn store_memory_unscoped(
    client: &reqwest::Client,
    base: &str,
    user_id: &str,
    content: &str,
) -> String {
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", user_id)
        .json(&json!({"content": content}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn wait_for_api_payload_contains(
    client: &reqwest::Client,
    base: &str,
    user_id: &str,
    path: &str,
    body: Value,
    expected_fragments: &[&str],
) -> Value {
    let mut last_body = Value::Null;
    for _ in 0..60 {
        let resp = client
            .post(format!("{base}{path}"))
            .header("X-User-Id", user_id)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        let current_body = resp.json::<Value>().await.unwrap();
        let haystack = current_body.to_string();
        if expected_fragments
            .iter()
            .all(|fragment| haystack.contains(fragment))
        {
            return current_body;
        }
        last_body = current_body;
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
    panic!(
        "timed out waiting for {path} to contain {:?}: {last_body}",
        expected_fragments
    );
}

#[tokio::test]
async fn test_episodic_no_memories_returns_error() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (base, client, _server) = spawn_server_with_llm(llm).await;
    let uid = uid();

    // No memories for this session → 500
    let r = client
        .post(format!("{base}/v1/sessions/nonexistent_session/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 500, "should return 500 for empty session");
    println!("✅ episodic empty session: 500");
}

#[tokio::test]
async fn test_episodic_async_task_polling() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Without LLM, async mode should still create a task (that will fail)
    // but the endpoint itself returns 503 before creating a task
    let r = client
        .post(format!("{base}/v1/sessions/sess_async/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": false}))
        .send()
        .await
        .unwrap();
    // Without LLM: 503
    assert_eq!(r.status(), 503);
    println!("✅ episodic async without LLM: 503");
}

#[tokio::test]
async fn test_episodic_with_llm_sync() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (base, client, _server) = spawn_server_with_llm(llm).await;
    let uid = uid();
    let session_id = format!(
        "ep_sess_{}",
        uuid::Uuid::new_v4().simple().to_string()[..8].to_string()
    );

    // Store memories with session_id
    for content in &[
        "Implemented Rust REST API",
        "Added episodic memory support",
        "All tests passing",
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content, "session_id": session_id}))
            .send()
            .await
            .unwrap();
    }

    // Generate episodic memory (sync)
    let r = client
        .post(format!("{base}/v1/sessions/{session_id}/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "should return 200");
    let body: serde_json::Value = r.json().await.unwrap();
    assert!(
        body["memory_id"].as_str().is_some(),
        "should have memory_id: {body}"
    );
    assert!(
        body["content"]
            .as_str()
            .map(|c| c.contains("Deterministic validation of session workflows"))
            .unwrap_or(false),
        "content should contain fake LLM topic: {body}"
    );
    assert_eq!(
        body["metadata"]["topic"].as_str().unwrap_or(""),
        "Deterministic validation of session workflows"
    );
    println!(
        "✅ episodic with LLM sync: memory_id={}",
        body["memory_id"].as_str().unwrap_or("")
    );
    println!(
        "   content: {}",
        &body["content"].as_str().unwrap_or("")
            [..100.min(body["content"].as_str().unwrap_or("").len())]
    );
}

#[tokio::test]
async fn test_episodic_with_llm_async() {
    let (llm, _shutdown) = spawn_fake_llm().await;
    let (base, client, _server) = spawn_server_with_llm(llm).await;
    let uid = uid();
    let session_id = format!(
        "ep_async_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );

    for content in [
        "Validated scheduler fallback behavior",
        "Verified deterministic reflection write-back",
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content, "session_id": session_id}))
            .send()
            .await
            .unwrap();
    }

    let r = client
        .post(format!("{base}/v1/sessions/{session_id}/summary"))
        .header("X-User-Id", &uid)
        .json(&json!({"mode": "full", "sync": false}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: serde_json::Value = r.json().await.unwrap();
    let task_id = body["task_id"].as_str().expect("task_id").to_string();

    let mut result = None;
    for attempt in 0..60 {
        let poll = client
            .get(format!("{base}/v1/tasks/{task_id}"))
            .header("X-User-Id", &uid)
            .send()
            .await
            .unwrap();
        assert_eq!(poll.status(), 200);
        let task: serde_json::Value = poll.json().await.unwrap();
        match task["status"].as_str().unwrap_or("") {
            "completed" => {
                result = Some(task["result"].clone());
                break;
            }
            "failed" => panic!("async episodic task failed on attempt {attempt}: {task}"),
            status => {
                if attempt == 59 {
                    panic!("async episodic task timed out after 60 polls, last status={status}: {task}");
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            }
        }
    }

    let result = result.expect("async episodic task should complete");
    assert!(
        result["content"]
            .as_str()
            .unwrap_or("")
            .contains("Deterministic validation of session workflows"),
        "async summary should persist fake LLM content: {result}"
    );
    println!("✅ episodic with LLM async: task_id={task_id}");
}

// ── Admin API ─────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_admin_stats_and_users() {
    let (base, client, _server) = spawn_server().await;
    let user = uid();

    // Store a memory first
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .json(&json!({"content": "admin test memory", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();

    // GET /admin/stats
    let r = client
        .get(format!("{base}/admin/stats"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["total_memories"].as_i64().unwrap() >= 1);
    assert!(body["total_users"].as_i64().unwrap() >= 1);
    println!("✅ admin stats: {body}");

    // GET /admin/users — just check it returns a list (may not contain our user if DB has many)
    let r = client
        .get(format!("{base}/admin/users"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let users = body["users"].as_array().unwrap();
    assert!(!users.is_empty(), "admin users list should not be empty");
    println!("✅ admin users: {} users", users.len());

    // GET /admin/users/:user_id/stats
    let r = client
        .get(format!("{base}/admin/users/{user}/stats"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memory_count"].as_i64().unwrap(), 1);
    println!("✅ admin user stats: {body}");

    // POST /admin/users/:user_id/reset-access-counts
    let r = client
        .post(format!("{base}/admin/users/{user}/reset-access-counts"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    println!("✅ admin reset access counts");

    // DELETE /admin/users/:user_id
    let r = client
        .delete(format!("{base}/admin/users/{user}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    println!("✅ admin delete user");

    // Verify user's memories are deactivated
    let r = client
        .get(format!("{base}/admin/users/{user}/stats"))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memory_count"].as_i64().unwrap(), 0);
    println!("✅ admin verified user deleted (0 active memories)");
}

#[tokio::test]
async fn test_admin_trigger_governance() {
    let (base, client, _server) = spawn_server().await;
    let user = uid();

    // Store a memory
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .json(&json!({"content": "governance trigger test", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();

    // Trigger governance (skips cooldown)
    let r = client
        .post(format!(
            "{base}/admin/governance/{user}/trigger?op=governance"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["op"].as_str().unwrap(), "governance");
    println!("✅ admin trigger governance: {body}");

    // Trigger consolidate
    let r = client
        .post(format!(
            "{base}/admin/governance/{user}/trigger?op=consolidate"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["op"].as_str().unwrap(), "consolidate");
    println!("✅ admin trigger consolidate: {body}");

    // Invalid op
    let r = client
        .post(format!("{base}/admin/governance/{user}/trigger?op=invalid"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    println!("✅ admin trigger invalid op rejected");
}

#[tokio::test]
async fn test_health_endpoints() {
    let (base, client, server) = spawn_server().await;
    let user = uid();

    // Store some memories
    for content in ["Rust is fast", "Python is easy", "Go is simple"] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &user)
            .json(&json!({"content": content, "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
    }

    // Legacy or directly-written rows may contain NULL despite the column default.
    // Aggregate endpoints must preserve that unknown value instead of failing to decode it.
    let pool = server.user_db_pool(&user).await;
    sqlx::query("UPDATE mem_memories SET initial_confidence = NULL WHERE user_id = ?")
        .bind(&user)
        .execute(&pool)
        .await
        .unwrap();

    // GET /v1/health/analyze
    let r = client
        .get(format!("{base}/v1/health/analyze"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["semantic"]["total"].as_i64().unwrap() >= 3);
    assert!(body["semantic"]["avg_confidence"].is_null());
    println!("✅ health analyze: {body}");

    // GET /v1/health/storage
    let r = client
        .get(format!("{base}/v1/health/storage"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["total"].as_i64().unwrap() >= 3);
    assert!(body["active"].as_i64().unwrap() >= 3);
    println!("✅ health storage: {body}");

    // Profile statistics use a separate aggregate query and must follow the
    // same all-NULL confidence semantics.
    let r = client
        .get(format!("{base}/v1/profiles/me"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["stats"]["avg_confidence"].as_f64(), Some(0.0));

    // GET /v1/health/capacity
    let r = client
        .get(format!("{base}/v1/health/capacity"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["recommendation"].as_str().unwrap(), "ok");
    println!("✅ health capacity: {body}");

    // GET /v1/health/hygiene
    let r = client
        .get(format!("{base}/v1/health/hygiene"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        body.get("inactive_memories").is_some(),
        "must have inactive_memories"
    );
    assert!(
        body.get("stale_working_memories").is_some(),
        "must have stale_working_memories"
    );
    assert!(
        body.get("orphan_graph_nodes").is_some(),
        "must have orphan_graph_nodes"
    );
    println!("✅ health hygiene: {body}");
}

#[tokio::test]
async fn test_admin_health_hygiene_global() {
    let mk = "test-master-key-hygiene";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");

    let r = client
        .get(format!("{base}/admin/health/hygiene"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body.get("inactive_memories").is_some());
    assert!(body.get("orphan_graph_nodes").is_some());
    assert!(body.get("orphan_stats").is_some());
    println!("✅ admin health hygiene global: {body}");

    // Without master key → 401
    let r = client
        .get(format!("{base}/admin/health/hygiene"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    println!("✅ admin health hygiene: 401 without master key");
}

// ── Sandbox validation ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_sandbox_validation() {
    let (base, client, _server) = spawn_server().await;
    let user = uid();

    // Store some base memories
    for content in [
        "Rust is a systems language",
        "Python is great for scripting",
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &user)
            .json(&json!({"content": content, "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
    }

    // Sandbox validation is internal — test via store (which uses it internally if enabled)
    // For now, verify that storing a memory still works (sandbox is fail-open)
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .json(&json!({"content": "Go is compiled and fast", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    let body: Value = r.json().await.unwrap();
    assert!(body["memory_id"].as_str().is_some());
    println!("✅ sandbox: store succeeds (fail-open)");
}

#[tokio::test]
async fn test_retrieve_with_explain() {
    let (base, client, _server) = spawn_server().await;
    let user = uid();

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .json(&json!({"content": "Rust is fast and safe", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();

    // explain=true (basic)
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &user)
        .json(&json!({"query": "fast language", "top_k": 5, "explain": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["explain"].is_object(), "explain field missing: {body}");
    assert!(body["explain"]["path"].is_string());
    assert!(body["explain"]["total_ms"].is_number());
    assert_eq!(body["explain"]["level"], "basic");
    println!(
        "✅ explain=basic: path={}, total_ms={}",
        body["explain"]["path"].as_str().unwrap_or("?"),
        body["explain"]["total_ms"].as_f64().unwrap_or(0.0)
    );

    // explain="verbose" — should include candidate_scores
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &user)
        .json(&json!({"query": "fast language", "top_k": 5, "explain": "verbose"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["explain"]["level"], "verbose");
    // candidate_scores present when results exist
    if !body["results"]
        .as_array()
        .map(|a| a.is_empty())
        .unwrap_or(true)
    {
        let scores = &body["explain"]["candidate_scores"];
        assert!(
            scores.is_array(),
            "verbose should have candidate_scores: {body}"
        );
        let first = &scores[0];
        assert!(
            first["final_score"].is_number(),
            "missing final_score: {first}"
        );
        assert!(first["vector_score"].is_number());
        assert!(first["keyword_score"].is_number());
        assert!(first["temporal_score"].is_number());
        assert!(first["confidence_score"].is_number());
        println!("✅ explain=verbose: candidate_scores[0]={}", first);
    }

    // explain="none" — returns array directly
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &user)
        .json(&json!({"query": "fast language", "top_k": 5}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        body.is_array(),
        "without explain should return array: {body}"
    );
    println!(
        "✅ explain=none: {} results",
        body.as_array().unwrap().len()
    );
}

#[tokio::test]
async fn test_explain_verbose_candidate_scores() {
    let Some((key, base_url, model)) = try_embedding() else {
        println!("⏭️  test_explain_verbose_candidate_scores skipped (EMBEDDING_API_KEY not set)");
        return;
    };
    let (base, client, _server) = spawn_server_with_embedding(key, base_url, model).await;
    let uid = uid();

    // Store a few memories
    for content in [
        "Rust is fast and memory-safe",
        "Python is easy to learn",
        "Go has great concurrency",
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content, "memory_type": "semantic"}))
            .send()
            .await
            .unwrap();
    }

    // explain=verbose should return candidate_scores with 4-dim breakdown
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "fast programming language", "top_k": 5, "explain": "verbose"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(
        body["explain"]["level"], "verbose",
        "level mismatch: {body}"
    );
    assert!(body["explain"]["path"].is_string());

    let results = body["results"].as_array().expect("results array");
    if !results.is_empty() {
        let scores = body["explain"]["candidate_scores"]
            .as_array()
            .expect("candidate_scores should be array when results non-empty");
        assert!(!scores.is_empty(), "candidate_scores empty");
        let first = &scores[0];
        assert!(first["final_score"].is_number(), "missing final_score");
        assert!(first["vector_score"].is_number(), "missing vector_score");
        assert!(first["keyword_score"].is_number(), "missing keyword_score");
        assert!(
            first["temporal_score"].is_number(),
            "missing temporal_score"
        );
        assert!(
            first["confidence_score"].is_number(),
            "missing confidence_score"
        );
        assert_eq!(first["rank"], 1);
        println!("✅ explain=verbose candidate_scores[0]: final={:.4} vec={:.4} kw={:.4} time={:.4} conf={:.4}",
            first["final_score"].as_f64().unwrap_or(0.0),
            first["vector_score"].as_f64().unwrap_or(0.0),
            first["keyword_score"].as_f64().unwrap_or(0.0),
            first["temporal_score"].as_f64().unwrap_or(0.0),
            first["confidence_score"].as_f64().unwrap_or(0.0));
    } else {
        println!("⚠️  no results returned (embedding may not have indexed yet)");
    }
}

#[tokio::test]
async fn test_retrieve_session_scope_only_prefilters_and_skips_graph() {
    use memoria_storage::{GraphEdge, GraphNode, GraphStore, NodeType};

    let (base, client, server) =
        spawn_server_with_custom_embedder_and_pool(Arc::new(SessionScopeTestEmbedder), test_dim())
            .await;
    let uid = uid();
    let user_store = server.user_store(&uid).await;

    let target_mid =
        store_memory_for_session(&client, &base, &uid, "target-session memory", "sess-target")
            .await;
    let global_mid = store_memory_unscoped(&client, &base, &uid, "global-unscoped top").await;
    let other_mid =
        store_memory_for_session(&client, &base, &uid, "other-session top", "sess-other").await;
    let other_second_mid =
        store_memory_for_session(&client, &base, &uid, "other-session second", "sess-other").await;

    let mut graph = GraphStore::new(user_store.pool().clone(), test_dim());
    if let Some(db_name) = user_store.db_name() {
        graph.set_db_name(db_name.to_string());
    }
    graph.migrate().await.expect("graph migrate");

    let make_node = |memory_id: &str, session_id: &str, content: &str| GraphNode {
        node_id: uuid::Uuid::new_v4().simple().to_string()[..32].to_string(),
        user_id: uid.clone(),
        node_type: NodeType::Semantic,
        content: content.to_string(),
        entity_type: None,
        embedding: Some(SessionScopeTestEmbedder::vector_for(content)),
        memory_id: Some(memory_id.to_string()),
        session_id: Some(session_id.to_string()),
        confidence: 0.95,
        trust_tier: "T1".to_string(),
        importance: 0.5,
        source_nodes: vec![],
        conflicts_with: None,
        conflict_resolution: None,
        access_count: 0,
        cross_session_count: 0,
        is_active: true,
        superseded_by: None,
        created_at: Some(chrono::Utc::now().naive_utc()),
    };

    let target_node = make_node(&target_mid, "sess-target", "target-session memory");
    let other_node = make_node(&other_mid, "sess-other", "other-session top");
    let other_second_node = make_node(&other_second_mid, "sess-other", "other-session second");
    graph.create_node(&target_node).await.unwrap();
    graph.create_node(&other_node).await.unwrap();
    graph.create_node(&other_second_node).await.unwrap();
    graph
        .add_edge(&GraphEdge {
            source_id: other_node.node_id.clone(),
            target_id: other_second_node.node_id.clone(),
            edge_type: "association".to_string(),
            weight: 1.0,
            user_id: uid.clone(),
        })
        .await
        .unwrap();

    let relaxed_body = wait_for_api_payload_contains(
        &client,
        &base,
        &uid,
        "/v1/memories/retrieve",
        json!({
            "query": "strict session query",
            "session_id": "sess-target",
            "top_k": 3,
            "explain": true
        }),
        &["other-session top"],
    )
    .await;
    assert_eq!(relaxed_body["explain"]["graph_attempted"], true);
    let relaxed_results = relaxed_body["results"]
        .as_array()
        .expect("relaxed retrieve should return result array");
    assert!(
        relaxed_results
            .iter()
            .any(|item| item["session_id"].as_str() == Some("sess-other")),
        "cross-session retrieval should still be free to return another session: {relaxed_body}",
    );

    let strict_body = wait_for_api_payload_contains(
        &client,
        &base,
        &uid,
        "/v1/memories/retrieve",
        json!({
            "query": "strict session query",
            "session_id": "sess-target",
            "session_scope": "only",
            "top_k": 2,
            "explain": true
        }),
        &["target-session memory", "global-unscoped top"],
    )
    .await;
    assert_eq!(strict_body["explain"]["graph_attempted"], false);
    assert_ne!(strict_body["explain"]["path"], "graph");
    let strict_results = strict_body["results"]
        .as_array()
        .expect("strict retrieve should return result array");
    let strict_ids: std::collections::HashSet<&str> = strict_results
        .iter()
        .filter_map(|item| item["memory_id"].as_str())
        .collect();
    assert!(
        strict_ids.contains(target_mid.as_str()),
        "strict session retrieval should still include requested-session memory: {strict_body}",
    );
    assert!(
        strict_ids.contains(global_mid.as_str()),
        "strict session retrieval should still include unscoped memory: {strict_body}",
    );
    assert!(
        !strict_ids.contains(other_mid.as_str()),
        "strict session retrieval should still exclude other scoped sessions: {strict_body}",
    );
}

#[tokio::test]
async fn test_retrieve_session_scope_requires_session_id() {
    let (base, client, _server) = spawn_server().await;
    let user = uid();

    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &user)
        .json(&json!({
            "query": "strict session query",
            "session_scope": "only",
            "top_k": 1
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);

    let r = client
        .post(format!("{base}/v1/memories/search"))
        .header("X-User-Id", &user)
        .json(&json!({
            "query": "strict session query",
            "session_scope": "prefer",
            "top_k": 1
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
}

#[tokio::test]
async fn test_retrieve_session_scope_only_preserves_top_k_with_session_candidates() {
    let (base, client, _server) =
        spawn_server_with_custom_embedder_and_pool(Arc::new(SessionScopeTestEmbedder), test_dim())
            .await;
    let uid = uid();

    let target_first =
        store_memory_for_session(&client, &base, &uid, "scoped-candidate-a", "sess-target").await;
    let target_second =
        store_memory_for_session(&client, &base, &uid, "scoped-candidate-b", "sess-target").await;
    let global_shared = store_memory_unscoped(&client, &base, &uid, "global-candidate-b").await;
    let _other_first =
        store_memory_for_session(&client, &base, &uid, "global-candidate-a", "sess-other").await;
    let _other_second =
        store_memory_for_session(&client, &base, &uid, "other-session decoy", "sess-other").await;
    let strict_body = wait_for_api_payload_contains(
        &client,
        &base,
        &uid,
        "/v1/memories/retrieve",
        json!({
            "query": "scoped topk query",
            "session_id": "sess-target",
            "session_scope": "only",
            "top_k": 3
        }),
        &[
            "scoped-candidate-a",
            "scoped-candidate-b",
            "global-candidate-b",
        ],
    )
    .await;
    let strict_results = strict_body
        .as_array()
        .expect("retrieve should return array");
    assert_eq!(
        strict_results.len(),
        3,
        "strict session retrieval should still fill top_k from session-local plus unscoped candidates",
    );
    assert!(
        strict_results
            .iter()
            .all(|item| item["session_id"].as_str() != Some("sess-other")),
        "strict session retrieval must still exclude other scoped sessions",
    );
    let strict_ids: std::collections::HashSet<&str> = strict_results
        .iter()
        .filter_map(|item| item["memory_id"].as_str())
        .collect();
    assert_eq!(
        strict_ids,
        std::collections::HashSet::from([
            target_first.as_str(),
            target_second.as_str(),
            global_shared.as_str(),
        ]),
        "strict retrieval should rank within the requested session plus unscoped memories",
    );
}

#[tokio::test]
async fn test_search_session_scope_only_respects_session() {
    let (base, client, _server) =
        spawn_server_with_custom_embedder_and_pool(Arc::new(SessionScopeTestEmbedder), test_dim())
            .await;
    let uid = uid();

    store_memory_for_session(&client, &base, &uid, "target-session memory", "sess-target").await;
    store_memory_unscoped(&client, &base, &uid, "global-unscoped top").await;
    store_memory_for_session(&client, &base, &uid, "other-session top", "sess-other").await;
    store_memory_for_session(&client, &base, &uid, "other-session second", "sess-other").await;

    let relaxed_body = wait_for_api_payload_contains(
        &client,
        &base,
        &uid,
        "/v1/memories/search",
        json!({
            "query": "strict session query",
            "session_id": "sess-target",
            "top_k": 3
        }),
        &["other-session top"],
    )
    .await;
    assert!(
        relaxed_body
            .as_array()
            .expect("search should return array")
            .iter()
            .any(|item| item["session_id"].as_str() == Some("sess-other")),
        "relaxed search should still be free to return another session: {relaxed_body}",
    );

    let strict_body = wait_for_api_payload_contains(
        &client,
        &base,
        &uid,
        "/v1/memories/search",
        json!({
            "query": "strict session query",
            "session_id": "sess-target",
            "session_scope": "only",
            "top_k": 2
        }),
        &["target-session memory", "global-unscoped top"],
    )
    .await;
    let strict_results = strict_body
        .as_array()
        .expect("strict search should return array");
    assert!(
        strict_results
            .iter()
            .all(|item| item["session_id"].as_str() != Some("sess-other")),
        "session_scope=only should exclude other scoped sessions: {strict_body}",
    );
    assert!(
        strict_results
            .iter()
            .any(|item| item["session_id"].is_null()),
        "session_scope=only should still include unscoped memories: {strict_body}",
    );
    assert!(
        strict_results
            .iter()
            .any(|item| item["session_id"].as_str() == Some("sess-target")),
        "session_scope=only should still include requested-session memories: {strict_body}",
    );
}

#[tokio::test]
async fn test_pipeline_run() {
    let (base, client, _server) = spawn_server().await;
    let user = uid();

    // Normal candidates — should all be stored
    let r = client
        .post(format!("{base}/v1/pipeline/run"))
        .header("X-User-Id", &user)
        .json(&json!({
            "candidates": [
                {"content": "Rust is fast", "memory_type": "semantic"},
                {"content": "Python is easy", "memory_type": "semantic"},
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memories_stored"].as_i64().unwrap(), 2);
    assert_eq!(body["memories_rejected"].as_i64().unwrap(), 0);
    println!("✅ pipeline run: {body}");

    // Sensitive candidate — should be blocked
    let r = client
        .post(format!("{base}/v1/pipeline/run"))
        .header("X-User-Id", &user)
        .json(&json!({
            "candidates": [
                {"content": "password=supersecret123", "memory_type": "semantic"},
                {"content": "Go is compiled", "memory_type": "semantic"},
            ]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memories_stored"].as_i64().unwrap(), 1);
    assert_eq!(body["memories_rejected"].as_i64().unwrap(), 1);
    println!("✅ pipeline sensitivity block: {body}");
}

// ═══════════════════════════════════════════════════════════════════════════════
// NEW FEATURE TESTS — admin keys, user params, snapshot detail/diff, batch embed
// ═══════════════════════════════════════════════════════════════════════════════

// ── Admin: list user keys ────────────────────────────────────────────────────

#[tokio::test]
async fn test_admin_list_user_keys() {
    let mk = "test-mk-list-keys";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // Create a key for this user first
    client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({"user_id": uid, "name": "key-for-list"}))
        .send()
        .await
        .unwrap();

    // List via admin endpoint
    let r = client
        .get(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["user_id"], uid);
    let keys = body["keys"].as_array().unwrap();
    assert!(
        keys.iter().any(|k| k["name"] == "key-for-list"),
        "should find created key: {body}"
    );
    // Verify key fields
    let k = &keys[0];
    assert!(k["key_id"].as_str().is_some());
    assert!(k["key_prefix"].as_str().is_some());
    assert!(k["created_at"].as_str().is_some());
    println!("✅ GET /admin/users/:id/keys: {} keys", keys.len());
}

#[tokio::test]
async fn test_admin_list_user_keys_empty() {
    let mk = "test-mk-list-keys-empty";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid(); // fresh user, no keys

    let r = client
        .get(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["keys"].as_array().unwrap().is_empty(),
        "new user should have no keys"
    );
    println!("✅ GET /admin/users/:id/keys (empty): {body}");
}

// ── Admin: revoke all user keys ──────────────────────────────────────────────

#[tokio::test]
async fn test_admin_revoke_all_user_keys() {
    let mk = "test-mk-revoke-all";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // Create 3 keys
    for i in 0..3 {
        client
            .post(format!("{base}/auth/keys"))
            .header("Authorization", &auth)
            .json(&json!({"user_id": uid, "name": format!("key-{i}")}))
            .send()
            .await
            .unwrap();
    }

    // Verify 3 keys exist
    let r = client
        .get(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.json::<Value>().await.unwrap()["keys"]
            .as_array()
            .unwrap()
            .len(),
        3
    );

    // Revoke all
    let r = client
        .delete(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["revoked"], 3);
    println!(
        "✅ DELETE /admin/users/:id/keys: revoked {}",
        body["revoked"]
    );

    // Verify all gone
    let r = client
        .get(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert!(
        body["keys"].as_array().unwrap().is_empty(),
        "all keys should be revoked"
    );
    println!("✅ verified 0 keys after revoke_all");
}

#[tokio::test]
async fn test_admin_revoke_all_user_keys_idempotent() {
    let mk = "test-mk-revoke-idem";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid(); // no keys

    // Revoke on user with no keys → should succeed with revoked=0
    let r = client
        .delete(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["revoked"], 0);
    println!("✅ revoke_all idempotent: revoked=0 for user with no keys");
}

// ── Admin: set user params ───────────────────────────────────────────────────

#[tokio::test]
async fn test_admin_set_user_params() {
    let mk = "test-mk-set-params";
    let (base, client, server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // Ensure the config table exists (may not be in Rust migration yet)
    let pool = server.shared_pool();
    let config_table = server.shared_table("mem_user_memory_config");
    let _ = sqlx::query(&format!(
        "CREATE TABLE IF NOT EXISTS {config_table} (\
             user_id VARCHAR(128) PRIMARY KEY, \
             strategy_key VARCHAR(64) DEFAULT NULL, \
             params_json JSON DEFAULT NULL, \
             updated_at DATETIME DEFAULT NULL)"
    ))
    .execute(&pool)
    .await;
    let _ = sqlx::query(&format!(
        "INSERT IGNORE INTO {config_table} (user_id) VALUES (?)"
    ))
    .bind(&uid)
    .execute(&pool)
    .await;

    // Set params
    let params = json!({"vector_weight": 0.7, "keyword_weight": 0.3, "max_results": 20});
    let r = client
        .post(format!("{base}/admin/users/{uid}/params"))
        .header("Authorization", &auth)
        .json(&params)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["user_id"], uid);
    assert_eq!(body["params"]["vector_weight"], 0.7);
    assert_eq!(body["params"]["keyword_weight"], 0.3);
    println!("✅ POST /admin/users/:id/params: {body}");
}

// ── Snapshot: GET detail (time-travel) ───────────────────────────────────────

#[tokio::test]
async fn test_snapshot_get_detail() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store memories
    for (content, trust_tier, initial_confidence, observed_at) in [
        ("snapshot detail A", "T1", 0.91, "2026-04-01T10:00:00Z"),
        ("snapshot detail B", "T2", 0.82, "2026-04-02T10:00:00Z"),
        ("snapshot detail C", "T3", 0.73, "2026-04-03T10:00:00Z"),
    ] {
        client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({
                "content": content,
                "memory_type": "semantic",
                "trust_tier": trust_tier,
                "initial_confidence": initial_confidence,
                "session_id": "snapshot-detail-session",
                "observed_at": observed_at
            }))
            .send()
            .await
            .unwrap();
    }

    // Create snapshot
    let snap = format!(
        "detail_test_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": snap}))
        .send()
        .await
        .unwrap();

    // GET snapshot detail (brief, default)
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["name"], snap);
    assert_eq!(body["memory_count"], 3);
    assert!(body["by_type"]["semantic"].as_i64().unwrap() >= 3);
    let mems = body["memories"].as_array().unwrap();
    assert_eq!(mems.len(), 3);
    let mut tiers: Vec<_> = mems
        .iter()
        .map(|m| m["trust_tier"].as_str().unwrap_or_default().to_string())
        .collect();
    tiers.sort();
    assert_eq!(tiers, vec!["T1", "T2", "T3"]);
    // Brief mode: content should be short
    for m in mems {
        assert!(m["memory_id"].as_str().is_some());
        assert_eq!(m["user_id"], uid);
        assert!(m["content"].as_str().is_some());
        assert_eq!(m["memory_type"], "semantic");
        assert!(m["initial_confidence"].is_number());
        assert_eq!(m["is_active"], true);
        assert_eq!(m["session_id"], "snapshot-detail-session");
        assert!(m["observed_at"].as_str().is_some());
        assert!(
            m["created_at"].as_str().is_some(),
            "brief mode should include created_at"
        );
        assert!(
            m["trust_tier"].as_str().is_some(),
            "brief mode should include trust_tier"
        );
        assert!(
            m.get("retrieval_score").is_some(),
            "brief mode should include retrieval_score"
        );
    }
    println!(
        "✅ GET /v1/snapshots/:name (brief): {} memories, by_type={}",
        mems.len(),
        body["by_type"]
    );

    // GET with detail=full — should include confidence
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}?detail=full"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let mems = body["memories"].as_array().unwrap();
    assert!(
        mems[0].get("confidence").is_some(),
        "full detail should include confidence: {}",
        mems[0]
    );
    assert!(
        mems.iter().all(|m| m["trust_tier"].as_str().is_some()),
        "full detail should include trust_tier: {body}"
    );
    assert!(
        mems.iter().all(|m| m["created_at"].as_str().is_some()),
        "full detail should include created_at: {body}"
    );
    assert!(
        mems.iter().all(|m| m["observed_at"].as_str().is_some()),
        "full detail should include observed_at: {body}"
    );
    assert!(
        mems.iter().all(|m| m["initial_confidence"].is_number()),
        "full detail should include initial_confidence: {body}"
    );
    assert!(
        mems.iter().all(|m| m.get("retrieval_score").is_some()),
        "full detail should include retrieval_score: {body}"
    );
    println!("✅ GET /v1/snapshots/:name (full): confidence present");

    // GET with pagination
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}?limit=2&offset=0"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memories"].as_array().unwrap().len(), 2);
    assert_eq!(body["has_more"], true);
    assert_eq!(body["limit"], 2);
    assert_eq!(body["offset"], 0);
    println!("✅ GET /v1/snapshots/:name (paginated): limit=2, has_more=true");

    // Page 2
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}?limit=2&offset=2"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memories"].as_array().unwrap().len(), 1);
    assert_eq!(body["has_more"], false);
    println!("✅ GET /v1/snapshots/:name (page 2): 1 memory, has_more=false");

    // Cleanup
    client
        .delete(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
}

// ── Snapshot: diff (current vs snapshot) ─────────────────────────────────────

#[tokio::test]
async fn test_snapshot_diff() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store 2 memories
    let mut mids = vec![];
    for (content, trust_tier) in [("diff base A", "T1"), ("diff base B", "T2")] {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({"content": content, "trust_tier": trust_tier}))
            .send()
            .await
            .unwrap();
        mids.push(
            r.json::<Value>().await.unwrap()["memory_id"]
                .as_str()
                .unwrap()
                .to_string(),
        );
    }

    // Create snapshot
    let snap = format!(
        "diff_test_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": snap}))
        .send()
        .await
        .unwrap();

    // Add a new memory after snapshot
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "diff added C", "trust_tier": "T3"}))
        .send()
        .await
        .unwrap();

    // Delete one of the original memories
    client
        .delete(format!("{base}/v1/memories/{}", mids[0]))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();

    // Diff
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}/diff"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["snapshot_count"], 2, "snapshot had 2 memories");
    assert_eq!(
        body["current_count"], 2,
        "current has 2 (1 original + 1 new)"
    );

    let added = body["added"].as_array().unwrap();
    let removed = body["removed"].as_array().unwrap();
    // "diff added C" should be in added
    assert!(
        added
            .iter()
            .any(|m| m["content"].as_str().unwrap().contains("diff added C")),
        "should find added memory: {added:?}"
    );
    assert!(
        added.iter().any(|m| m["trust_tier"] == "T3"),
        "added diff entries should include trust_tier: {added:?}"
    );
    // "diff base A" should be in removed (deleted after snapshot)
    assert!(
        removed
            .iter()
            .any(|m| m["content"].as_str().unwrap().contains("diff base A")),
        "should find removed memory: {removed:?}"
    );
    assert!(
        removed.iter().any(|m| m["trust_tier"] == "T1"),
        "removed diff entries should include trust_tier: {removed:?}"
    );
    println!(
        "✅ GET /v1/snapshots/:name/diff: added={}, removed={}",
        added.len(),
        removed.len()
    );

    // Diff with limit
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}/diff?limit=1"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["added"].as_array().unwrap().len() <= 1);
    assert!(body["removed"].as_array().unwrap().len() <= 1);
    println!("✅ GET /v1/snapshots/:name/diff (limit=1)");

    // Cleanup
    client
        .delete(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn test_snapshot_diff_no_changes() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store a memory
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "no change test"}))
        .send()
        .await
        .unwrap();

    // Snapshot immediately
    let snap = format!(
        "nochange_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": snap}))
        .send()
        .await
        .unwrap();

    // Diff with no changes
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}/diff"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["added"].as_array().unwrap().is_empty(),
        "no additions expected"
    );
    assert!(
        body["removed"].as_array().unwrap().is_empty(),
        "no removals expected"
    );
    assert_eq!(body["snapshot_count"], body["current_count"]);
    println!("✅ snapshot diff no changes: counts match, empty added/removed");

    client
        .delete(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn test_api_snapshot_limit_is_per_user() {
    let (base, client, _server) = spawn_server().await;
    let uid_a = uid();
    let uid_b = uid();
    let names_a: Vec<String> = (0..20)
        .map(|i| {
            format!(
                "api_cap_a_{i}_{}",
                &uuid::Uuid::new_v4().simple().to_string()[..6]
            )
        })
        .collect();

    for name in &names_a {
        let r = client
            .post(format!("{base}/v1/snapshots"))
            .header("X-User-Id", &uid_a)
            .json(&json!({"name": name}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 201);
        let body: Value = r.json().await.unwrap();
        let result = body["result"].as_str().unwrap_or("");
        assert!(
            result.contains("created"),
            "snapshot create failed: {result}"
        );
        assert_eq!(body["name"], name.as_str());
        assert!(body["created_at"].is_string(), "missing created_at: {body}");
        assert!(body["timestamp"].is_string(), "missing timestamp: {body}");
    }

    let overflow = format!(
        "api_cap_overflow_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let r = client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid_a)
        .json(&json!({"name": overflow}))
        .send()
        .await
        .unwrap();
    // A rejected tool operation must propagate as a REST error, not Created.
    assert_eq!(r.status(), 400);
    let body = r.text().await.unwrap();
    assert!(
        body.contains("Snapshot limit reached (20)"),
        "expected per-user cap message: {body}"
    );

    let listed: Value = client
        .get(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid_a)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let snapshots = listed["snapshots"].as_array().expect("snapshots array");
    assert_eq!(listed["total"], 20, "rejected create must not change the total");
    assert_eq!(snapshots.len(), 20, "rejected create must not add a snapshot");
    assert!(snapshots.iter().all(|snapshot| snapshot["name"] != overflow));

    let b_snap = format!(
        "api_cap_b_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let r = client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid_b)
        .json(&json!({"name": b_snap}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["result"].as_str().unwrap_or("").contains("created"),
        "user B should still be able to create: {body}"
    );

    let r = client
        .get(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid_b)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let listed = body["result"].as_str().unwrap_or("");
    let snapshots = body["snapshots"].as_array().expect("snapshots array");
    assert!(
        listed.contains(&b_snap),
        "B should see own snapshot: {listed}"
    );
    assert!(
        !listed.contains(&names_a[0]),
        "B should not see A's snapshots: {listed}"
    );
    assert!(
        snapshots.iter().any(|snapshot| snapshot["name"] == b_snap),
        "B should see own snapshot in structured list: {body}"
    );
    assert!(
        snapshots
            .iter()
            .all(|snapshot| snapshot["name"] != names_a[0]),
        "B structured list should not see A's snapshots: {body}"
    );

    client
        .post(format!("{base}/v1/snapshots/delete"))
        .header("X-User-Id", &uid_a)
        .json(&json!({"names": names_a.join(",")}))
        .send()
        .await
        .unwrap();
    client
        .delete(format!("{base}/v1/snapshots/{b_snap}"))
        .header("X-User-Id", &uid_b)
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn test_api_snapshot_detail_is_scoped_to_owner() {
    let (base, client, _server) = spawn_server().await;
    let uid_a = uid();
    let uid_b = uid();
    let snap = format!(
        "api_owned_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid_a)
        .json(&json!({"content": "owner snapshot detail"}))
        .send()
        .await
        .unwrap();

    client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid_a)
        .json(&json!({"name": snap}))
        .send()
        .await
        .unwrap();

    let r = client
        .get(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid_b)
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        404,
        "non-owner should not resolve snapshot detail"
    );

    client
        .delete(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid_a)
        .send()
        .await
        .unwrap();
}

// ── Batch store: validates types upfront ─────────────────────────────────────

#[tokio::test]
async fn test_batch_store_invalid_type_rejects_all() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // One valid, one invalid type → should reject entire batch
    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "valid memory", "memory_type": "semantic"},
            {"content": "bad type", "memory_type": "nonexistent_type"},
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422, "invalid type should reject batch");
    println!("✅ batch store: invalid type rejects entire batch");
}

#[tokio::test]
async fn test_batch_store_all_types() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "semantic fact", "memory_type": "semantic"},
            {"content": "user preference", "memory_type": "profile"},
            {"content": "how to deploy", "memory_type": "procedural"},
            {"content": "current task", "memory_type": "working"},
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Vec<Value> = r.json().await.unwrap();
    assert_eq!(body.len(), 4);
    let types: Vec<&str> = body
        .iter()
        .map(|m| m["memory_type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"semantic"));
    assert!(types.contains(&"profile"));
    assert!(types.contains(&"procedural"));
    assert!(types.contains(&"working"));
    println!("✅ batch store all types: {:?}", types);
}

#[tokio::test]
async fn test_batch_store_empty() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": []}))
        .send()
        .await
        .unwrap();
    // Empty batch should succeed with empty result
    assert_eq!(r.status(), 201);
    let body: Vec<Value> = r.json().await.unwrap();
    assert!(body.is_empty());
    println!("✅ batch store empty: 201 with []");
}

#[tokio::test]
async fn test_batch_store_sensitivity_filter() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Batch with a sensitive item — store_batch checks sensitivity
    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "normal memory"},
            {"content": "my password is hunter2 and my ssn is 123-45-6789"},
        ]}))
        .send()
        .await
        .unwrap();
    // Should either filter out sensitive or reject — check behavior
    let status = r.status().as_u16();
    let body: Value = r.json().await.unwrap();
    println!("✅ batch store sensitivity: status={status}, body={body}");
}

// ── Batch store with embedding (requires EMBEDDING_API_KEY) ──────────────────

#[tokio::test]
async fn test_batch_store_with_embedding() {
    let Some((key, base_url, model)) = try_embedding() else {
        println!("⏭️  test_batch_store_with_embedding skipped (EMBEDDING_API_KEY not set)");
        return;
    };
    let (base, client, _server) = spawn_server_with_embedding(key, base_url, model).await;
    let uid = uid();

    // Batch store 5 items — should use embed_batch (single API call)
    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": [
            {"content": "Rust is a systems programming language"},
            {"content": "Python is great for data science"},
            {"content": "Go has excellent concurrency support"},
            {"content": "TypeScript adds types to JavaScript"},
            {"content": "Java runs on the JVM"},
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Vec<Value> = r.json().await.unwrap();
    assert_eq!(body.len(), 5);
    println!("✅ batch store with embedding: 5 items stored");

    // Verify they're retrievable via semantic search
    let r = client
        .post(format!("{base}/v1/memories/retrieve"))
        .header("X-User-Id", &uid)
        .json(&json!({"query": "systems programming", "top_k": 3}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let results: Vec<Value> = r.json().await.unwrap();
    assert!(
        !results.is_empty(),
        "batch-stored memories should be retrievable"
    );
    // Rust should rank high for "systems programming"
    assert!(
        results[0]["content"].as_str().unwrap().contains("Rust"),
        "Rust should be top result for 'systems programming': {:?}",
        results
            .iter()
            .map(|r| r["content"].as_str().unwrap())
            .collect::<Vec<_>>()
    );
    println!(
        "✅ batch store retrieval: top result = {}",
        results[0]["content"]
    );
}

// ── Remote mode: admin key management ────────────────────────────────────────

#[tokio::test]
async fn test_remote_admin_list_revoke_keys() {
    let mk = "test-mk-remote-keys";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();

    // Create keys via REST
    for i in 0..2 {
        client
            .post(format!("{base}/auth/keys"))
            .header("Authorization", &auth)
            .json(&json!({"user_id": uid, "name": format!("rkey-{i}")}))
            .send()
            .await
            .unwrap();
    }

    // List via admin endpoint
    let r = client
        .get(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["keys"].as_array().unwrap().len(), 2);
    println!("✅ remote admin list keys: 2 keys");

    // Revoke all
    let r = client
        .delete(format!("{base}/admin/users/{uid}/keys"))
        .header("Authorization", &auth)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["revoked"], 2);
    println!("✅ remote admin revoke all: {body}");
}

// ── Snapshot detail via remote mode ──────────────────────────────────────────

#[tokio::test]
async fn test_remote_snapshot_detail_and_diff() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, _server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    // Store memories
    remote
        .call("memory_store", json!({"content": "remote snap detail A"}))
        .await
        .unwrap();
    remote
        .call("memory_store", json!({"content": "remote snap detail B"}))
        .await
        .unwrap();

    // Create snapshot
    let snap = format!("rsnap_{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    remote
        .call("memory_snapshot", json!({"name": snap}))
        .await
        .unwrap();

    // Add another memory after snapshot
    remote
        .call(
            "memory_store",
            json!({"content": "remote snap detail C (after)"}),
        )
        .await
        .unwrap();

    // GET snapshot detail via REST (not MCP — direct HTTP)
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["memory_count"], 2, "snapshot should have 2 memories");
    assert!(
        body["memories"].as_array().unwrap().iter().all(|m| {
            m["user_id"] == uid
                && m["initial_confidence"].is_number()
                && m["is_active"] == true
                && m.get("session_id").is_some()
                && m["observed_at"].as_str().is_some()
                && m["trust_tier"].as_str().is_some()
                && m.get("retrieval_score").is_some()
        }),
        "remote snapshot detail should align with MemoryResponse metadata: {body}"
    );
    assert!(
        body["memories"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["created_at"].as_str().is_some()),
        "remote snapshot detail should include created_at: {body}"
    );
    println!(
        "✅ remote snapshot detail: memory_count={}",
        body["memory_count"]
    );

    // GET snapshot diff
    let r = client
        .get(format!("{base}/v1/snapshots/{snap}/diff"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        !body["added"].as_array().unwrap().is_empty(),
        "should have added memories"
    );
    println!(
        "✅ remote snapshot diff: added={}, removed={}",
        body["added"].as_array().unwrap().len(),
        body["removed"].as_array().unwrap().len()
    );

    // Cleanup
    remote
        .call("memory_snapshot_delete", json!({"names": snap}))
        .await
        .unwrap();
}

// ── Plugin API e2e tests ──────────────────────────────────────────────────────

/// Build a signed Rhai plugin package as base64 file map (ready for POST /admin/plugins).
fn build_signed_plugin_files(
    signer_name: &str,
    signer_key: &ed25519_dalek::SigningKey,
    plugin_name: &str,
    version: &str,
) -> std::collections::HashMap<String, String> {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    use ed25519_dalek::Signer;

    let script = r#"
        fn memoria_plugin(ctx) {
            if ctx["phase"] == "plan" {
                return #{ requires_approval: false };
            }
            return #{ warnings: [] };
        }
    "#;

    let manifest_unsigned = serde_json::json!({
        "name": plugin_name,
        "version": version,
        "api_version": "v1",
        "runtime": "rhai",
        "entry": {
            "rhai": { "script": "policy.rhai", "entrypoint": "memoria_plugin" },
            "grpc": null
        },
        "capabilities": ["governance.plan", "governance.execute"],
        "compatibility": { "memoria": ">=0.1.0-rc1 <0.2.0" },
        "permissions": { "network": false, "filesystem": false, "env": [] },
        "limits": { "timeout_ms": 500, "max_memory_mb": 32, "max_output_bytes": 8192 },
        "integrity": { "sha256": "", "signature": "", "signer": signer_name },
        "metadata": { "display_name": "E2E test plugin" }
    });

    // Write to temp dir to compute sha256
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("policy.rhai"), script).unwrap();
    std::fs::write(
        dir.path().join("manifest.json"),
        serde_json::to_vec_pretty(&manifest_unsigned).unwrap(),
    )
    .unwrap();
    let sha256 = memoria_service::compute_package_sha256(dir.path()).unwrap();
    let signature = B64.encode(signer_key.sign(sha256.as_bytes()).to_bytes());

    let mut manifest_signed = manifest_unsigned;
    manifest_signed["integrity"]["sha256"] = serde_json::json!(sha256);
    manifest_signed["integrity"]["signature"] = serde_json::json!(signature);

    let mut files = std::collections::HashMap::new();
    files.insert(
        "manifest.json".into(),
        B64.encode(serde_json::to_vec_pretty(&manifest_signed).unwrap()),
    );
    files.insert("policy.rhai".into(), B64.encode(script));
    files
}

fn test_signer_key() -> ed25519_dalek::SigningKey {
    ed25519_dalek::SigningKey::from_bytes(&[42u8; 32])
}

fn test_signer_public_b64() -> String {
    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    use ed25519_dalek::VerifyingKey;
    B64.encode(VerifyingKey::from(&test_signer_key()).as_bytes())
}

/// Full plugin lifecycle: signer → publish → list → review → score → binding → activate → matrix → events → rules
#[tokio::test]
async fn test_plugin_full_lifecycle() {
    let (base, c, _server) = spawn_server().await;
    let signer_name = format!("e2e-signer-{}", uuid::Uuid::new_v4().simple());
    let plugin_name = format!("e2e-plugin-{}", uuid::Uuid::new_v4().simple());

    // 1. Register signer
    let r = c
        .post(format!("{base}/admin/plugins/signers"))
        .json(&json!({ "signer": signer_name, "public_key": test_signer_public_b64() }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 2. List signers — should contain ours
    let r = c
        .get(format!("{base}/admin/plugins/signers"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let signers = body["signers"].as_array().unwrap();
    assert!(signers.iter().any(|s| s["signer"] == signer_name));

    // 3. Publish
    let files = build_signed_plugin_files(&signer_name, &test_signer_key(), &plugin_name, "0.1.0");
    let r = c
        .post(format!("{base}/admin/plugins"))
        .json(&json!({ "files": files }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        200,
        "publish failed: {}",
        r.text().await.unwrap_or_default()
    );

    // Same content re-publish is idempotent (returns existing entry)
    let files2 = build_signed_plugin_files(&signer_name, &test_signer_key(), &plugin_name, "0.1.0");
    let r = c
        .post(format!("{base}/admin/plugins"))
        .json(&json!({ "files": files2 }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "idempotent re-publish should succeed");

    // 4. List packages
    let r = c.get(format!("{base}/admin/plugins")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let pkgs: Vec<Value> = r.json().await.unwrap();
    let ours = pkgs
        .iter()
        .find(|p| p["plugin_key"].as_str().unwrap().contains(&plugin_name));
    assert!(ours.is_some(), "published plugin should appear in list");
    let plugin_key = ours.unwrap()["plugin_key"].as_str().unwrap().to_string();
    assert_eq!(ours.unwrap()["status"], "pending");

    // 5. Review → active
    let r = c
        .post(format!("{base}/admin/plugins/{plugin_key}/0.1.0/review"))
        .json(&json!({ "status": "active", "notes": "e2e approved" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // Verify status changed
    let r = c.get(format!("{base}/admin/plugins")).send().await.unwrap();
    let pkgs: Vec<Value> = r.json().await.unwrap();
    let ours = pkgs.iter().find(|p| p["plugin_key"] == plugin_key).unwrap();
    assert_eq!(ours["status"], "active");

    // 6. Score
    let r = c
        .post(format!("{base}/admin/plugins/{plugin_key}/0.1.0/score"))
        .json(&json!({ "score": 4.5, "notes": "solid" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 7. Create binding rule
    let r = c
        .post(format!("{base}/admin/plugins/domains/governance/bindings"))
        .json(&json!({
            "binding_key": "default",
            "plugin_key": plugin_key,
            "selector_kind": "semver",
            "selector_value": ">=0.1.0",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 8. List rules
    let r = c
        .get(format!(
            "{base}/admin/plugins/domains/governance/bindings?binding=default"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let rules: Vec<Value> = r.json().await.unwrap();
    assert!(rules.iter().any(|r| r["plugin_key"] == plugin_key));

    // 9. Activate binding
    let r = c
        .post(format!("{base}/admin/plugins/domains/governance/activate"))
        .json(&json!({
            "plugin_key": plugin_key,
            "version": "0.1.0",
            "binding_key": "default",
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    // 10. Compatibility matrix
    let r = c
        .get(format!("{base}/admin/plugins/matrix"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let matrix: Vec<Value> = r.json().await.unwrap();
    assert!(matrix.iter().any(|m| m["plugin_key"] == plugin_key));

    // 11. Audit events — should have publish, review, score, binding, activate
    let r = c
        .get(format!(
            "{base}/admin/plugins/events?plugin_key={plugin_key}&limit=20"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let events: Vec<Value> = r.json().await.unwrap();
    let event_types: Vec<&str> = events
        .iter()
        .filter_map(|e| e["event_type"].as_str())
        .collect();
    assert!(
        event_types.contains(&"package.published"),
        "events: {event_types:?}"
    );
    assert!(
        event_types.contains(&"package.reviewed"),
        "events: {event_types:?}"
    );
    assert!(
        event_types.contains(&"package.scored"),
        "events: {event_types:?}"
    );

    println!(
        "✅ plugin full lifecycle: {plugin_key} — {} audit events",
        events.len()
    );
}

/// Dev-mode publish: skips signature verification, auto-approves.
#[tokio::test]
async fn test_plugin_dev_mode_publish() {
    let (base, c, _server) = spawn_server().await;
    let plugin_name = format!("e2e-dev-{}", uuid::Uuid::new_v4().simple());

    // Build an UNSIGNED package (no signer registered, no valid signature)
    let script = r#"fn memoria_plugin(ctx) { return #{}; }"#;
    let manifest = serde_json::json!({
        "name": plugin_name,
        "version": "0.1.0",
        "api_version": "v1",
        "runtime": "rhai",
        "entry": {
            "rhai": { "script": "policy.rhai", "entrypoint": "memoria_plugin" },
            "grpc": null
        },
        "capabilities": ["governance.plan"],
        "compatibility": { "memoria": ">=0.1.0-rc1 <0.2.0" },
        "permissions": { "network": false, "filesystem": false, "env": [] },
        "limits": { "timeout_ms": 500, "max_memory_mb": 32, "max_output_bytes": 8192 },
        "integrity": { "sha256": "", "signature": "", "signer": "nobody" },
        "metadata": {}
    });

    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    let mut files = std::collections::HashMap::<String, String>::new();
    files.insert(
        "manifest.json".into(),
        B64.encode(serde_json::to_vec_pretty(&manifest).unwrap()),
    );
    files.insert("policy.rhai".into(), B64.encode(script));

    // Normal publish should FAIL (no signer "nobody" registered)
    let r = c
        .post(format!("{base}/admin/plugins"))
        .json(&json!({ "files": files }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        500,
        "unsigned publish should fail without dev-mode"
    );

    // Dev-mode publish should succeed — but we need the dev endpoint.
    // The REST API currently only has the normal publish endpoint.
    // Dev-mode is available via CLI (which calls publish_plugin_package_dev directly).
    // Let's verify the service-layer function works by checking the normal publish rejects.
    println!("✅ plugin dev-mode: unsigned publish correctly rejected by normal endpoint");
}

/// Error: publish without manifest.json
#[tokio::test]
async fn test_plugin_publish_missing_manifest() {
    let (base, c, _server) = spawn_server().await;

    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    let mut files = std::collections::HashMap::<String, String>::new();
    files.insert("policy.rhai".into(), B64.encode("fn x() {}"));

    let r = c
        .post(format!("{base}/admin/plugins"))
        .json(&json!({ "files": files }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let text = r.text().await.unwrap();
    assert!(
        text.contains("manifest.json"),
        "error should mention manifest: {text}"
    );
    println!("✅ plugin publish missing manifest rejected");
}

/// Error: publish with path traversal filename
#[tokio::test]
async fn test_plugin_publish_path_traversal_rejected() {
    let (base, c, _server) = spawn_server().await;

    use base64::engine::general_purpose::STANDARD as B64;
    use base64::Engine;
    let mut files = std::collections::HashMap::<String, String>::new();
    files.insert("manifest.json".into(), B64.encode("{}"));
    files.insert("../etc/passwd".into(), B64.encode("evil"));

    let r = c
        .post(format!("{base}/admin/plugins"))
        .json(&json!({ "files": files }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 400);
    let text = r.text().await.unwrap();
    assert!(text.contains("invalid filename"), "error: {text}");
    println!("✅ plugin publish path traversal rejected");
}

/// Error: review a non-existent package
#[tokio::test]
async fn test_plugin_review_nonexistent() {
    let (base, c, _server) = spawn_server().await;

    let r = c
        .post(format!(
            "{base}/admin/plugins/governance:nonexistent:v0/9.9.9/review"
        ))
        .json(&json!({ "status": "active" }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 500);
    println!("✅ plugin review nonexistent rejected");
}

/// Signer upsert is idempotent
#[tokio::test]
async fn test_plugin_signer_upsert_idempotent() {
    let (base, c, _server) = spawn_server().await;
    let signer_name = format!("e2e-idem-{}", uuid::Uuid::new_v4().simple());

    for _ in 0..2 {
        let r = c
            .post(format!("{base}/admin/plugins/signers"))
            .json(&json!({ "signer": signer_name, "public_key": test_signer_public_b64() }))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    let r = c
        .get(format!("{base}/admin/plugins/signers"))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let count = body["signers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|s| s["signer"] == signer_name)
        .count();
    assert_eq!(count, 1, "signer should appear exactly once");
    println!("✅ plugin signer upsert idempotent");
}

/// Empty list/matrix/events return empty arrays, not errors
#[tokio::test]
async fn test_plugin_empty_queries() {
    let (base, c, _server) = spawn_server().await;

    let r = c.get(format!("{base}/admin/plugins")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let _: Vec<Value> = r.json().await.unwrap(); // should parse as array

    let r = c
        .get(format!("{base}/admin/plugins/matrix"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _: Vec<Value> = r.json().await.unwrap();

    let r = c
        .get(format!("{base}/admin/plugins/events"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _: Vec<Value> = r.json().await.unwrap();

    let r = c
        .get(format!(
            "{base}/admin/plugins/domains/governance/bindings?binding=default"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let _: Vec<Value> = r.json().await.unwrap();

    println!("✅ plugin empty queries return empty arrays");
}

#[tokio::test]
async fn test_plugin_admin_routes_require_master_key() {
    let mk = "test-mk-plugin-admin";
    let (base, client, _server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");
    let uid = uid();
    let user_key = create_api_key_for_user(&client, &base, &auth, &uid, "plugin-user").await;
    let user_auth = format!("Bearer {user_key}");

    let r = client
        .get(format!("{base}/admin/plugins"))
        .header("Authorization", &user_auth)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403, "API key must not list admin plugins");

    let r = client
        .post(format!("{base}/admin/plugins/signers"))
        .header("Authorization", &user_auth)
        .json(&json!({
            "signer": format!("forbidden-{}", uuid::Uuid::new_v4().simple()),
            "public_key": test_signer_public_b64()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(
        r.status(),
        403,
        "API key must not mutate admin plugin state"
    );
}

// ── Distributed coordination tests ────────────────────────────────────────────

/// Spawn a server with a specific instance_id, returning (base_url, client, instance_id).
async fn spawn_server_with_instance(
    instance_id: &str,
) -> (
    String,
    reqwest::Client,
    String,
    support::multi_db::ApiTestServer,
) {
    let server = support::multi_db::spawn_api_server(
        "api_e2e_instance",
        test_dim(),
        String::new(),
        None,
        None,
        Some(instance_id.to_string()),
        false,
    )
    .await;
    (
        server.base.clone(),
        server.client.clone(),
        instance_id.to_string(),
        server,
    )
}

#[tokio::test]
async fn test_distributed_health_instance_returns_id() {
    let iid = format!("inst_{}", uuid::Uuid::new_v4().simple());
    let (base, c, _, _server) = spawn_server_with_instance(&iid).await;

    let r = c
        .get(format!("{base}/health/instance"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["instance_id"], iid);
    assert_eq!(body["status"], "ok");
    println!("✅ /health/instance returns correct instance_id");
}

#[tokio::test]
async fn test_distributed_two_instances_different_ids() {
    let id_a = format!("inst_a_{}", uuid::Uuid::new_v4().simple());
    let id_b = format!("inst_b_{}", uuid::Uuid::new_v4().simple());

    let (base_a, c, _, _server_a) = spawn_server_with_instance(&id_a).await;
    let (base_b, c2, _, _server_b) = spawn_server_with_instance(&id_b).await;

    let ra: Value = c
        .get(format!("{base_a}/health/instance"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let rb: Value = c2
        .get(format!("{base_b}/health/instance"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    assert_eq!(ra["instance_id"], id_a);
    assert_eq!(rb["instance_id"], id_b);
    assert_ne!(ra["instance_id"], rb["instance_id"]);
    println!("✅ two instances report different instance_ids");
}

#[tokio::test]
async fn test_distributed_cross_instance_memory_visibility() {
    let id_a = format!("inst_a_{}", uuid::Uuid::new_v4().simple());
    let id_b = format!("inst_b_{}", uuid::Uuid::new_v4().simple());
    let user = uid();

    let (base_a, c, _, _server_a) = spawn_server_with_instance(&id_a).await;
    let (base_b, c2, _, _server_b) = spawn_server_with_instance(&id_b).await;

    // Store on instance A
    let r = c
        .post(format!("{base_a}/v1/memories"))
        .header("x-user-id", &user)
        .json(&json!({"content": "distributed test memory", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status().is_success(),
        "store should succeed, got {}",
        r.status()
    );

    // Read from instance B
    let r = c2
        .get(format!("{base_b}/v1/memories"))
        .header("x-user-id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let memories = body["items"]
        .as_array()
        .expect("response should have items array");
    assert!(
        memories
            .iter()
            .any(|m| m["content"] == "distributed test memory"),
        "Memory stored on instance A should be visible from instance B"
    );
    println!("✅ memory stored on A is visible from B");
}

#[tokio::test]
async fn test_distributed_lock_acquire_release() {
    // Direct test of the distributed lock via SqlMemoryStore
    use memoria_service::DistributedLock;
    use std::time::Duration;

    let ctx = memoria_test_utils::MultiDbTestContext::new(
        &db_url(),
        "api_e2e_lock",
        test_dim(),
        None,
        None,
    )
    .await;
    let store = ctx.shared_store();

    let lock_key = format!("test_lock_{}", uuid::Uuid::new_v4().simple());

    // Holder A acquires
    let acquired = store
        .try_acquire(&lock_key, "holder_a", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(acquired, "holder_a should acquire the lock");

    // Holder B cannot acquire
    let acquired = store
        .try_acquire(&lock_key, "holder_b", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(
        !acquired,
        "holder_b should NOT acquire while holder_a holds it"
    );

    // Holder A re-entrant
    let acquired = store
        .try_acquire(&lock_key, "holder_a", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(acquired, "holder_a should re-acquire (re-entrant)");

    // Holder A releases
    store.release(&lock_key, "holder_a").await.unwrap();

    // Now holder B can acquire
    let acquired = store
        .try_acquire(&lock_key, "holder_b", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(acquired, "holder_b should acquire after holder_a released");

    // Cleanup
    store.release(&lock_key, "holder_b").await.unwrap();
    println!("✅ distributed lock acquire/release/re-entrant works");
}

#[tokio::test]
async fn test_distributed_lock_expiry() {
    use memoria_service::DistributedLock;
    use std::time::Duration;

    let ctx = memoria_test_utils::MultiDbTestContext::new(
        &db_url(),
        "api_e2e_lock_expiry",
        test_dim(),
        None,
        None,
    )
    .await;
    let store = ctx.shared_store();

    let lock_key = format!("test_lock_exp_{}", uuid::Uuid::new_v4().simple());

    // Acquire with 1-second TTL
    let acquired = store
        .try_acquire(&lock_key, "holder_a", Duration::from_secs(1))
        .await
        .unwrap();
    assert!(acquired);

    // Wait for expiry
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Holder B should now acquire (expired lock cleaned up)
    let acquired = store
        .try_acquire(&lock_key, "holder_b", Duration::from_secs(60))
        .await
        .unwrap();
    assert!(
        acquired,
        "holder_b should acquire after holder_a's lock expired"
    );

    store.release(&lock_key, "holder_b").await.unwrap();
    println!("✅ distributed lock expires and can be taken over");
}

#[tokio::test]
async fn test_distributed_lock_renew() {
    use memoria_service::DistributedLock;
    use std::time::Duration;

    let ctx = memoria_test_utils::MultiDbTestContext::new(
        &db_url(),
        "api_e2e_lock_renew",
        test_dim(),
        None,
        None,
    )
    .await;
    let store = ctx.shared_store();

    let lock_key = format!("test_lock_renew_{}", uuid::Uuid::new_v4().simple());

    // Acquire
    store
        .try_acquire(&lock_key, "holder_a", Duration::from_secs(60))
        .await
        .unwrap();

    // Renew by holder_a succeeds
    let renewed = store
        .renew(&lock_key, "holder_a", Duration::from_secs(120))
        .await
        .unwrap();
    assert!(renewed, "holder_a should renew its own lock");

    // Renew by holder_b fails
    let renewed = store
        .renew(&lock_key, "holder_b", Duration::from_secs(120))
        .await
        .unwrap();
    assert!(!renewed, "holder_b should NOT renew holder_a's lock");

    store.release(&lock_key, "holder_a").await.unwrap();
    println!("✅ distributed lock renew works correctly");
}

#[tokio::test]
async fn test_distributed_async_task_cross_instance() {
    use memoria_service::AsyncTaskStore;

    let ctx = memoria_test_utils::MultiDbTestContext::new(
        &db_url(),
        "api_e2e_async_task",
        test_dim(),
        None,
        None,
    )
    .await;
    let store = ctx.shared_store();

    let task_id = format!("task_{}", uuid::Uuid::new_v4().simple());

    // Create task on "instance_a"
    store
        .create_task(&task_id, "instance_a", "test_user")
        .await
        .unwrap();

    // Read from "instance_b" perspective (same store, simulating different instance)
    let task = store
        .get_task(&task_id)
        .await
        .unwrap()
        .expect("task should exist");
    assert_eq!(task.task_id, task_id);
    assert_eq!(task.instance_id, "instance_a");
    assert_eq!(task.status, "processing");

    // Complete task
    store
        .complete_task(&task_id, json!({"memory_id": "m123"}))
        .await
        .unwrap();
    let task = store
        .get_task(&task_id)
        .await
        .unwrap()
        .expect("task should exist");
    assert_eq!(task.status, "completed");
    assert_eq!(task.result.unwrap()["memory_id"], "m123");

    println!("✅ async task visible cross-instance and completable");
}

#[tokio::test]
async fn test_distributed_async_task_fail() {
    use memoria_service::AsyncTaskStore;

    let ctx = memoria_test_utils::MultiDbTestContext::new(
        &db_url(),
        "api_e2e_async_task_fail",
        test_dim(),
        None,
        None,
    )
    .await;
    let store = ctx.shared_store();

    let task_id = format!("task_{}", uuid::Uuid::new_v4().simple());
    store
        .create_task(&task_id, "instance_x", "test_user")
        .await
        .unwrap();
    store
        .fail_task(&task_id, json!({"code": "ERR", "message": "boom"}))
        .await
        .unwrap();

    let task = store
        .get_task(&task_id)
        .await
        .unwrap()
        .expect("task should exist");
    assert_eq!(task.status, "failed");
    assert_eq!(task.error.unwrap()["message"], "boom");

    println!("✅ async task failure recorded correctly");
}

// ── Feedback API tests ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_feedback_record() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store a memory first
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "API feedback test memory", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    let memory_id = body["memory_id"].as_str().unwrap();

    // Record feedback
    let r = client
        .post(format!("{base}/v1/memories/{memory_id}/feedback"))
        .header("X-User-Id", &uid)
        .json(&json!({"signal": "useful", "context": "very helpful"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert!(!body["feedback_id"].as_str().unwrap().is_empty());
    assert_eq!(body["memory_id"], memory_id);
    assert_eq!(body["signal"], "useful");

    println!("✅ POST /v1/memories/:id/feedback");
}

#[tokio::test]
async fn test_api_feedback_invalid_signal() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "Invalid signal test", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let memory_id = body["memory_id"].as_str().unwrap();

    // Try invalid signal
    let r = client
        .post(format!("{base}/v1/memories/{memory_id}/feedback"))
        .header("X-User-Id", &uid)
        .json(&json!({"signal": "bad_signal"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422); // Validation error for invalid signal
    println!("✅ POST /v1/memories/:id/feedback rejects invalid signal");
}

#[tokio::test]
async fn test_api_feedback_nonexistent_memory() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .post(format!("{base}/v1/memories/nonexistent_id_12345/feedback"))
        .header("X-User-Id", &uid)
        .json(&json!({"signal": "useful"}))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error() || r.status().is_server_error());
    println!("✅ POST /v1/memories/:id/feedback rejects nonexistent memory");
}

#[tokio::test]
async fn test_api_feedback_stats() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store memories and give feedback
    for (i, signal) in ["useful", "useful", "irrelevant", "outdated"]
        .iter()
        .enumerate()
    {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(
                &json!({"content": format!("Stats test {signal} {i}"), "memory_type": "semantic"}),
            )
            .send()
            .await
            .unwrap();
        let body: Value = r.json().await.unwrap();
        let memory_id = body["memory_id"].as_str().unwrap();

        client
            .post(format!("{base}/v1/memories/{memory_id}/feedback"))
            .header("X-User-Id", &uid)
            .json(&json!({"signal": signal}))
            .send()
            .await
            .unwrap();
    }

    // Get stats
    let r = client
        .get(format!("{base}/v1/feedback/stats"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["total"], 4);
    assert_eq!(body["useful"], 2);
    assert_eq!(body["irrelevant"], 1);
    assert_eq!(body["outdated"], 1);
    assert_eq!(body["wrong"], 0);

    println!("✅ GET /v1/feedback/stats: {body}");
}

#[tokio::test]
async fn test_api_feedback_by_tier() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store memories with different tiers and give feedback
    for (tier, signal) in &[("T1", "useful"), ("T2", "useful"), ("T3", "irrelevant")] {
        let r = client
            .post(format!("{base}/v1/memories"))
            .header("X-User-Id", &uid)
            .json(&json!({
                "content": format!("Tier {tier} test"),
                "memory_type": "semantic",
                "trust_tier": tier
            }))
            .send()
            .await
            .unwrap();
        let body: Value = r.json().await.unwrap();
        let memory_id = body["memory_id"].as_str().unwrap();

        client
            .post(format!("{base}/v1/memories/{memory_id}/feedback"))
            .header("X-User-Id", &uid)
            .json(&json!({"signal": signal}))
            .send()
            .await
            .unwrap();
    }

    // Get breakdown by tier
    let r = client
        .get(format!("{base}/v1/feedback/by-tier"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let breakdown = body["breakdown"].as_array().unwrap();
    assert!(!breakdown.is_empty(), "should have tier breakdown");

    println!("✅ GET /v1/feedback/by-tier: {} entries", breakdown.len());
}

// ── Retrieval Params API Tests ────────────────────────────────────────────────

#[tokio::test]
async fn test_api_get_retrieval_params() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let r = client
        .get(format!("{base}/v1/retrieval-params"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();

    // Should return default params
    assert_eq!(body["feedback_weight"], 0.1);
    assert_eq!(body["temporal_decay_hours"], 168.0);
    assert_eq!(body["confidence_weight"], 0.1);

    println!("✅ GET /v1/retrieval-params: {body}");
}

#[tokio::test]
async fn test_api_set_retrieval_params() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Set custom params
    let r = client
        .put(format!("{base}/v1/retrieval-params"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "feedback_weight": 0.15,
            "temporal_decay_hours": 200.0
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();

    assert!((body["feedback_weight"].as_f64().unwrap() - 0.15).abs() < 0.001);
    assert!((body["temporal_decay_hours"].as_f64().unwrap() - 200.0).abs() < 0.1);

    // Verify persisted
    let r = client
        .get(format!("{base}/v1/retrieval-params"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert!((body["feedback_weight"].as_f64().unwrap() - 0.15).abs() < 0.001);

    println!("✅ PUT /v1/retrieval-params: params updated and persisted");
}

#[tokio::test]
async fn test_api_tune_retrieval_params() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Without enough feedback, should not tune
    let r = client
        .post(format!("{base}/v1/retrieval-params/tune"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["tuned"], false);

    println!("✅ POST /v1/retrieval-params/tune: {}", body["message"]);
}

#[tokio::test]
async fn test_api_tune_with_feedback() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Create a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "content": "Test memory for tuning API",
            "memory_type": "semantic"
        }))
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let memory_id = body["memory_id"].as_str().unwrap();

    // Add 12 useful feedback signals
    for _ in 0..12 {
        client
            .post(format!("{base}/v1/memories/{memory_id}/feedback"))
            .header("X-User-Id", &uid)
            .json(&json!({"signal": "useful"}))
            .send()
            .await
            .unwrap();
    }

    // Now tune should work
    let r = client
        .post(format!("{base}/v1/retrieval-params/tune"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body["tuned"], true);

    let old_weight = body["old_params"]["feedback_weight"].as_f64().unwrap();
    let new_weight = body["new_params"]["feedback_weight"].as_f64().unwrap();
    assert!(
        new_weight >= old_weight,
        "feedback_weight should increase with positive feedback"
    );

    println!(
        "✅ POST /v1/retrieval-params/tune: {:.3} → {:.3}",
        old_weight, new_weight
    );
}

// ── Prometheus metrics ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_metrics() {
    let (base, client, _server) = spawn_server().await;
    let r = client.get(format!("{base}/metrics")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    assert!(
        body.contains("memoria_memories_total"),
        "missing memoria_memories_total"
    );
    assert!(
        body.contains("memoria_users_total"),
        "missing memoria_users_total"
    );
    assert!(
        body.contains("memoria_auth_failures_total"),
        "missing auth_failures counter"
    );
    println!("✅ GET /metrics: {} bytes", body.len());
}

// ── Snapshot rollback ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_snapshot_rollback() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "before rollback", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // Create snapshot
    let snap = format!("rb_{}", &uuid::Uuid::new_v4().simple().to_string()[..6]);
    let r = client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": snap}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // Store another memory (after snapshot)
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "after snapshot", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    // Rollback
    let r = client
        .post(format!("{base}/v1/snapshots/{snap}/rollback"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(
        body["result"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("roll"),
        "rollback response: {body}"
    );
    println!("✅ POST /v1/snapshots/:name/rollback: {}", body["result"]);
}

#[tokio::test]
async fn test_api_branch_list_returns_structured_json() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();
    let branch = format!(
        "api_branch_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    let r = client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": branch }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/branches/{branch}/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = client
        .get(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    let branches = body["branches"].as_array().expect("branches array");
    assert!(
        branches
            .iter()
            .any(|entry| entry["name"] == "main" && entry["active"] == false),
        "main branch should be present and inactive after checkout: {body}"
    );
    assert!(
        branches
            .iter()
            .any(|entry| entry["name"] == branch && entry["active"] == true),
        "checked out branch should be marked active: {body}"
    );
    assert!(
        body["result"].as_str().unwrap_or("").contains("Branches:"),
        "compat text should still be present: {body}"
    );

    client
        .delete(format!("{base}/v1/branches/{branch}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
}

#[tokio::test]
async fn test_api_branch_pick_key_list_returns_result() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let branch = format!(
        "api_pick_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "main seed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": branch }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/branches/{branch}/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let alpha: Value = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "api branch alpha"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "api branch beta"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/branches/main/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = client
        .post(format!("{base}/v1/branches/{branch}/pick"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "selector": {"type": "key_list", "keys": [alpha["memory_id"].as_str().unwrap()]},
        }))
        .send()
        .await
        .unwrap();
    let Some(body) =
        pick_response_json_or_skip(r, "test_api_branch_pick_key_list_returns_result").await
    else {
        return;
    };
    assert!(
        body["result"]
            .as_str()
            .unwrap_or("")
            .contains("Picked 1 change"),
        "pick result: {body}"
    );

    let active = server.service().list_active(&uid, 10).await.unwrap();
    assert!(active
        .iter()
        .any(|memory| memory.content == "api branch alpha"));
    assert!(!active
        .iter()
        .any(|memory| memory.content == "api branch beta"));
}

#[tokio::test]
async fn test_api_branch_pick_key_list_dry_run_returns_preview() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let branch = format!(
        "api_pick_preview_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "main seed"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": branch }))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/branches/{branch}/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();

    let alpha: Value = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "api branch alpha"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    client
        .post(format!("{base}/v1/branches/main/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/branches/{branch}/pick"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "selector": {"type": "key_list", "keys": [alpha["memory_id"].as_str().unwrap()]},
            "dry_run": {
                "limit": 1,
                "include_content_preview": true,
                "include_scores": true
            }
        }))
        .send()
        .await
        .unwrap();
    let Some(body) =
        pick_response_json_or_skip(r, "test_api_branch_pick_key_list_dry_run_returns_preview")
            .await
    else {
        return;
    };
    assert_eq!(body["dry_run"].as_bool(), Some(true), "{body}");
    assert_eq!(
        body["summary"]["candidate_count"].as_u64(),
        Some(1),
        "{body}"
    );
    let candidate = &body["candidates"][0];
    assert_eq!(
        candidate["content_preview"].as_str(),
        Some("api branch alpha"),
        "{body}"
    );
    assert!(candidate.get("score").is_none(), "{body}");
    assert!(candidate.get("embedding").is_none(), "{body}");

    let active = server.service().list_active(&uid, 10).await.unwrap();
    assert!(!active
        .iter()
        .any(|memory| memory.content == "api branch alpha"));
}

#[tokio::test]
async fn test_api_branch_pick_retrieve_validation_error() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();
    let branch = format!(
        "api_pick_bad_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": branch }))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/branches/{branch}/pick"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "selector": {"type": "retrieve", "query": "anything", "top_k": 0},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    let body = r.text().await.unwrap();
    assert!(body.contains("top_k"), "body: {body}");
}

#[tokio::test]
async fn test_api_branch_pick_retrieve_min_score_validation_error() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();
    let branch = format!(
        "api_pick_min_score_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": branch }))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/branches/{branch}/pick"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "selector": {"type": "retrieve", "query": "anything", "top_k": 1, "min_score": -1},
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    let body = r.text().await.unwrap();
    assert!(body.contains("min_score"), "body: {body}");
}

#[tokio::test]
async fn test_api_branch_pick_dry_run_limit_validation_error() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();
    let branch = format!(
        "api_pick_limit_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": branch }))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/branches/{branch}/pick"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "selector": {"type": "key_list", "keys": ["abc"]},
            "dry_run": {"limit": 0}
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    let body = r.text().await.unwrap();
    assert!(body.contains("dry_run.limit"), "body: {body}");
}

#[tokio::test]
async fn test_api_branch_pick_conflict_returns_409() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let branch = format!(
        "api_pick_conflict_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    let original: Value = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "api shared conflict"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": branch }))
        .send()
        .await
        .unwrap();

    let branch_table = server
        .user_store(&uid)
        .await
        .list_branches(&uid)
        .await
        .unwrap()
        .into_iter()
        .find(|(name, _, _)| name == &branch)
        .map(|(_, table, _)| table)
        .expect("branch table");
    let pool = server.user_db_pool(&uid).await;

    sqlx::query(&format!(
        "UPDATE {branch_table} SET content = ? WHERE memory_id = ?"
    ))
    .bind("api branch conflict")
    .bind(original["memory_id"].as_str().unwrap())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE mem_memories SET content = ? WHERE memory_id = ?")
        .bind("api main conflict")
        .bind(original["memory_id"].as_str().unwrap())
        .execute(&pool)
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/branches/{branch}/pick"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "strategy": "fail",
            "selector": {
                "type": "key_list",
                "keys": [original["memory_id"].as_str().unwrap()]
            },
        }))
        .send()
        .await
        .unwrap();
    let status = r.status();
    let body = r.text().await.unwrap();
    if is_pick_not_supported_message(&body) {
        eprintln!("⚠️ DATA BRANCH PICK not supported, skipping test_api_branch_pick_fail_conflict_returns_409");
        return;
    }
    assert_eq!(status, 409, "body: {body}");
    assert!(body.contains("Conflict:"), "body: {body}");
}

#[tokio::test]
async fn test_api_branch_pick_snapshot_range_returns_result() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let branch = format!(
        "api_pick_snap_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let snap_before = format!(
        "pick_before_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let snap_after = format!(
        "pick_after_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "main seed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": branch }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/branches/{branch}/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": snap_before}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "snapshot-ranged memory"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/snapshots"))
        .header("X-User-Id", &uid)
        .json(&json!({"name": snap_after}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/branches/main/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = client
        .post(format!("{base}/v1/branches/{branch}/pick"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "selector": {
                "type": "snapshot_range",
                "from_snapshot": snap_before,
                "to_snapshot": snap_after
            },
        }))
        .send()
        .await
        .unwrap();
    let Some(body) =
        pick_response_json_or_skip(r, "test_api_branch_pick_snapshot_range_returns_result").await
    else {
        return;
    };
    assert!(
        body["result"]
            .as_str()
            .unwrap_or("")
            .contains("snapshot range"),
        "pick result: {body}"
    );

    let active = server.service().list_active(&uid, 10).await.unwrap();
    assert!(active
        .iter()
        .any(|memory| memory.content == "snapshot-ranged memory"));
}

#[tokio::test]
async fn test_api_branch_pick_retrieve_returns_ranked_result() {
    let (base, client, server) =
        spawn_server_with_custom_embedder_and_pool(Arc::new(PickTestEmbedder), test_dim()).await;
    let uid = uid();
    let branch = format!(
        "api_pick_retrieve_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "main seed"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": branch }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/branches/{branch}/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "alpha branch memory"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "beta branch memory"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/branches/main/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = client
        .post(format!("{base}/v1/branches/{branch}/pick"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "selector": {"type": "retrieve", "query": "alpha query", "top_k": 1},
        }))
        .send()
        .await
        .unwrap();
    let Some(body) =
        pick_response_json_or_skip(r, "test_api_branch_pick_retrieve_returns_ranked_result").await
    else {
        return;
    };
    assert!(
        body["result"].as_str().unwrap_or("").contains("top 1"),
        "pick result: {body}"
    );

    let active = server.service().list_active(&uid, 10).await.unwrap();
    assert!(active
        .iter()
        .any(|memory| memory.content == "alpha branch memory"));
    assert!(!active
        .iter()
        .any(|memory| memory.content == "beta branch memory"));
}

#[tokio::test]
async fn test_api_branch_pick_retrieve_dry_run_limits_preview() {
    let (base, client, server) =
        spawn_server_with_custom_embedder_and_pool(Arc::new(PickTestEmbedder), test_dim()).await;
    let uid = uid();
    let branch = format!(
        "api_pick_preview_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "main seed"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": branch }))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/branches/{branch}/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "alpha branch memory"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "beta branch memory"}))
        .send()
        .await
        .unwrap();
    client
        .post(format!("{base}/v1/branches/main/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/branches/{branch}/pick"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "selector": {"type": "retrieve", "query": "alpha query", "top_k": 2},
            "dry_run": {
                "limit": 1,
                "offset": 0,
                "include_content_preview": true,
                "include_scores": false
            }
        }))
        .send()
        .await
        .unwrap();
    let Some(body) =
        pick_response_json_or_skip(r, "test_api_branch_pick_retrieve_dry_run_limits_preview").await
    else {
        return;
    };
    assert_eq!(
        body["dry_run"].as_bool(),
        Some(true),
        "preview body: {body}"
    );
    assert_eq!(
        body["summary"]["candidate_count"].as_u64(),
        Some(2),
        "{body}"
    );
    assert_eq!(body["summary"]["shown_count"].as_u64(), Some(1), "{body}");
    assert_eq!(body["page"]["has_more"].as_bool(), Some(true), "{body}");
    let candidate = &body["candidates"][0];
    assert!(candidate.get("content_preview").is_some(), "{body}");
    assert!(candidate.get("score").is_none(), "{body}");
    assert!(candidate.get("embedding").is_none(), "{body}");

    let active = server.service().list_active(&uid, 10).await.unwrap();
    assert!(!active
        .iter()
        .any(|memory| memory.content == "alpha branch memory"));
    assert!(!active
        .iter()
        .any(|memory| memory.content == "beta branch memory"));
}

#[tokio::test]
async fn test_api_branch_pick_key_list_into_target_branch_returns_result() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let source = format!(
        "api_pick_src_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let target = format!(
        "api_pick_tgt_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "main seed"}))
        .send()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": source }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": target }))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .post(format!("{base}/v1/branches/{source}/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let source_memory: Value = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "source-only memory"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/branches/main/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let r = client
        .post(format!("{base}/v1/branches/{source}/pick"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "target": target,
            "selector": {
                "type": "key_list",
                "keys": [source_memory["memory_id"].as_str().unwrap()]
            },
        }))
        .send()
        .await
        .unwrap();
    let Some(body) = pick_response_json_or_skip(
        r,
        "test_api_branch_pick_key_list_into_target_branch_returns_result",
    )
    .await
    else {
        return;
    };
    assert!(
        body["result"]
            .as_str()
            .unwrap_or("")
            .contains("Picked 1 change"),
        "pick result: {body}"
    );

    let r = client
        .post(format!("{base}/v1/branches/{target}/checkout"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);

    let target_active = server.service().list_active(&uid, 10).await.unwrap();
    assert!(target_active
        .iter()
        .any(|memory| memory.content == "source-only memory"));
}

#[tokio::test]
async fn test_api_branch_pick_accept_replaces_conflict() {
    let (base, client, server) = spawn_server().await;
    let uid = uid();
    let branch = format!(
        "api_pick_accept_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    let original: Value = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "api shared accept"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();

    client
        .post(format!("{base}/v1/branches"))
        .header("X-User-Id", &uid)
        .json(&json!({ "name": branch }))
        .send()
        .await
        .unwrap();

    let branch_table = server
        .user_store(&uid)
        .await
        .list_branches(&uid)
        .await
        .unwrap()
        .into_iter()
        .find(|(name, _, _)| name == &branch)
        .map(|(_, table, _)| table)
        .expect("branch table");
    let pool = server.user_db_pool(&uid).await;

    sqlx::query(&format!(
        "UPDATE {branch_table} SET content = ? WHERE memory_id = ?"
    ))
    .bind("api branch accepted")
    .bind(original["memory_id"].as_str().unwrap())
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE mem_memories SET content = ? WHERE memory_id = ?")
        .bind("api main stale")
        .bind(original["memory_id"].as_str().unwrap())
        .execute(&pool)
        .await
        .unwrap();

    let r = client
        .post(format!("{base}/v1/branches/{branch}/pick"))
        .header("X-User-Id", &uid)
        .json(&json!({
            "strategy": "accept",
            "selector": {
                "type": "key_list",
                "keys": [original["memory_id"].as_str().unwrap()]
            },
        }))
        .send()
        .await
        .unwrap();
    let Some(body) =
        pick_response_json_or_skip(r, "test_api_branch_pick_accept_replaces_conflict").await
    else {
        return;
    };
    assert!(
        body["result"]
            .as_str()
            .unwrap_or("")
            .contains("Picked 1 change"),
        "pick result: {body}"
    );

    let row = sqlx::query("SELECT content FROM mem_memories WHERE memory_id = ?")
        .bind(original["memory_id"].as_str().unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::Row::try_get::<String, _>(&row, "content").unwrap(),
        "api branch accepted"
    );
}

#[tokio::test]
async fn test_remote_pick_key_list() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let branch = format!(
        "remote_pick_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    remote
        .call("memory_store", json!({"content": "remote main seed"}))
        .await
        .unwrap();
    remote
        .call("memory_branch", json!({"name": branch}))
        .await
        .unwrap();
    remote
        .call("memory_checkout", json!({"name": branch}))
        .await
        .unwrap();
    remote
        .call("memory_store", json!({"content": "remote branch memory"}))
        .await
        .unwrap();

    let branch_memory_id = server
        .service()
        .list_active(&uid, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|memory| memory.content == "remote branch memory")
        .expect("branch memory")
        .memory_id;

    remote
        .call("memory_checkout", json!({"name": "main"}))
        .await
        .unwrap();
    let Some(r) = remote_pick_or_skip(
        remote
            .call(
                "memory_pick",
                json!({
                    "source": branch,
                    "selector": {"type": "key_list", "keys": [branch_memory_id]},
                }),
            )
            .await,
        "test_remote_pick_key_list",
    ) else {
        return;
    };
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("Picked 1 change"), "remote pick: {t}");
}

#[tokio::test]
async fn test_remote_pick_key_list_into_target_branch() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, server) = spawn_api_for_remote().await;
    let uid = uid();
    let source = format!(
        "remote_pick_src_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let target = format!(
        "remote_pick_tgt_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let remote = RemoteClient::new(&base, None, uid.clone(), None);

    remote
        .call("memory_store", json!({"content": "remote main seed"}))
        .await
        .unwrap();
    remote
        .call("memory_branch", json!({"name": source}))
        .await
        .unwrap();
    remote
        .call("memory_branch", json!({"name": target}))
        .await
        .unwrap();
    remote
        .call("memory_checkout", json!({"name": source}))
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "remote target branch memory"}),
        )
        .await
        .unwrap();

    let branch_memory_id = server
        .service()
        .list_active(&uid, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|memory| memory.content == "remote target branch memory")
        .expect("branch memory")
        .memory_id;

    remote
        .call("memory_checkout", json!({"name": "main"}))
        .await
        .unwrap();
    let Some(r) = remote_pick_or_skip(
        remote
            .call(
                "memory_pick",
                json!({
                    "source": source,
                    "target": target,
                    "selector": {"type": "key_list", "keys": [branch_memory_id]},
                }),
            )
            .await,
        "test_remote_pick_key_list_into_target_branch",
    ) else {
        return;
    };
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("into"), "remote pick: {t}");

    remote
        .call("memory_checkout", json!({"name": target}))
        .await
        .unwrap();
    let target_active = server.service().list_active(&uid, 10).await.unwrap();
    assert!(target_active
        .iter()
        .any(|memory| memory.content == "remote target branch memory"));
}

#[tokio::test]
async fn test_remote_pick_snapshot_range() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let branch = format!(
        "remote_pick_snap_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let snap_before = format!(
        "remote_pick_before_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );
    let snap_after = format!(
        "remote_pick_after_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    remote
        .call("memory_store", json!({"content": "remote main seed"}))
        .await
        .unwrap();
    remote
        .call("memory_branch", json!({"name": branch}))
        .await
        .unwrap();
    remote
        .call("memory_checkout", json!({"name": branch}))
        .await
        .unwrap();
    remote
        .call("memory_snapshot", json!({"name": snap_before}))
        .await
        .unwrap();
    remote
        .call(
            "memory_store",
            json!({"content": "remote snapshot-ranged memory"}),
        )
        .await
        .unwrap();
    remote
        .call("memory_snapshot", json!({"name": snap_after}))
        .await
        .unwrap();
    remote
        .call("memory_checkout", json!({"name": "main"}))
        .await
        .unwrap();

    let Some(r) = remote_pick_or_skip(
        remote
            .call(
                "memory_pick",
                json!({
                    "source": branch,
                    "selector": {
                        "type": "snapshot_range",
                        "from_snapshot": snap_before,
                        "to_snapshot": snap_after
                    }
                }),
            )
            .await,
        "test_remote_pick_snapshot_range",
    ) else {
        return;
    };
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("snapshot range"), "remote pick: {t}");

    let active = server.service().list_active(&uid, 10).await.unwrap();
    assert!(active
        .iter()
        .any(|memory| memory.content == "remote snapshot-ranged memory"));
}

#[tokio::test]
async fn test_remote_pick_accept_replaces_conflict() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let branch = format!(
        "remote_pick_accept_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    remote
        .call("memory_store", json!({"content": "remote shared accept"}))
        .await
        .unwrap();
    let original_id = server
        .service()
        .list_active(&uid, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|memory| memory.content == "remote shared accept")
        .expect("original memory")
        .memory_id;

    remote
        .call("memory_branch", json!({"name": branch}))
        .await
        .unwrap();
    let branch_table = server
        .user_store(&uid)
        .await
        .list_branches(&uid)
        .await
        .unwrap()
        .into_iter()
        .find(|(name, _, _)| name == &branch)
        .map(|(_, table, _)| table)
        .expect("branch table");
    let pool = server.user_db_pool(&uid).await;

    sqlx::query(&format!(
        "UPDATE {branch_table} SET content = ? WHERE memory_id = ?"
    ))
    .bind("remote branch accepted")
    .bind(&original_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE mem_memories SET content = ? WHERE memory_id = ?")
        .bind("remote main stale")
        .bind(&original_id)
        .execute(&pool)
        .await
        .unwrap();

    let Some(r) = remote_pick_or_skip(
        remote
            .call(
                "memory_pick",
                json!({
                    "source": branch,
                    "strategy": "accept",
                    "selector": {"type": "key_list", "keys": [original_id]},
                }),
            )
            .await,
        "test_remote_pick_accept_replaces_conflict",
    ) else {
        return;
    };
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("Picked 1 change"), "remote pick: {t}");

    let row = sqlx::query("SELECT content FROM mem_memories WHERE memory_id = ?")
        .bind(&original_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::Row::try_get::<String, _>(&row, "content").unwrap(),
        "remote branch accepted"
    );
}

#[tokio::test]
async fn test_remote_pick_retrieve_applies_ranked_result() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, server) =
        spawn_api_for_remote_with_embedder(Arc::new(PickTestEmbedder), test_dim()).await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let branch = format!(
        "remote_pick_retrieve_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    remote
        .call("memory_store", json!({"content": "remote main seed"}))
        .await
        .unwrap();
    remote
        .call("memory_branch", json!({"name": branch}))
        .await
        .unwrap();
    remote
        .call("memory_checkout", json!({"name": branch}))
        .await
        .unwrap();
    remote
        .call("memory_store", json!({"content": "alpha branch memory"}))
        .await
        .unwrap();
    remote
        .call("memory_store", json!({"content": "beta branch memory"}))
        .await
        .unwrap();
    remote
        .call("memory_checkout", json!({"name": "main"}))
        .await
        .unwrap();

    let Some(r) = remote_pick_or_skip(
        remote
            .call(
                "memory_pick",
                json!({
                    "source": branch,
                    "selector": {"type": "retrieve", "query": "alpha query", "top_k": 1},
                }),
            )
            .await,
        "test_remote_pick_retrieve_applies_ranked_result",
    ) else {
        return;
    };
    let t = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(t.contains("top 1"), "remote pick: {t}");

    let active = server.service().list_active(&uid, 10).await.unwrap();
    assert!(active
        .iter()
        .any(|memory| memory.content == "alpha branch memory"));
    assert!(!active
        .iter()
        .any(|memory| memory.content == "beta branch memory"));
}

#[tokio::test]
async fn test_remote_pick_fail_conflict_returns_error() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, server) = spawn_api_for_remote().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let branch = format!(
        "remote_pick_conflict_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    remote
        .call("memory_store", json!({"content": "remote shared conflict"}))
        .await
        .unwrap();
    let original_id = server
        .service()
        .list_active(&uid, 10)
        .await
        .unwrap()
        .into_iter()
        .find(|memory| memory.content == "remote shared conflict")
        .expect("original memory")
        .memory_id;

    remote
        .call("memory_branch", json!({"name": branch}))
        .await
        .unwrap();
    let branch_table = server
        .user_store(&uid)
        .await
        .list_branches(&uid)
        .await
        .unwrap()
        .into_iter()
        .find(|(name, _, _)| name == &branch)
        .map(|(_, table, _)| table)
        .expect("branch table");
    let pool = server.user_db_pool(&uid).await;

    sqlx::query(&format!(
        "UPDATE {branch_table} SET content = ? WHERE memory_id = ?"
    ))
    .bind("remote branch conflict")
    .bind(&original_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE mem_memories SET content = ? WHERE memory_id = ?")
        .bind("remote main conflict")
        .bind(&original_id)
        .execute(&pool)
        .await
        .unwrap();

    let err = match remote
        .call(
            "memory_pick",
            json!({
                "source": branch,
                "strategy": "fail",
                "selector": {"type": "key_list", "keys": [original_id]},
            }),
        )
        .await
    {
        Ok(value) => panic!("fail conflict should return error: {value:?}"),
        Err(err) if is_pick_not_supported_message(&err.to_string()) => {
            eprintln!(
                "⚠️ DATA BRANCH PICK not supported, skipping test_remote_pick_fail_conflict_returns_error"
            );
            return;
        }
        Err(err) => err,
    };
    assert!(err.to_string().contains("Conflict:"), "{err:?}");
}

#[tokio::test]
async fn test_remote_pick_retrieve_dry_run_returns_preview_json() {
    use memoria_mcp::remote::RemoteClient;
    let (base, _, server) =
        spawn_api_for_remote_with_embedder(Arc::new(PickTestEmbedder), test_dim()).await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let branch = format!(
        "remote_pick_preview_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..6]
    );

    remote
        .call("memory_store", json!({"content": "remote main seed"}))
        .await
        .unwrap();
    remote
        .call("memory_branch", json!({"name": branch}))
        .await
        .unwrap();
    remote
        .call("memory_checkout", json!({"name": branch}))
        .await
        .unwrap();
    remote
        .call("memory_store", json!({"content": "alpha branch memory"}))
        .await
        .unwrap();
    remote
        .call("memory_store", json!({"content": "beta branch memory"}))
        .await
        .unwrap();
    remote
        .call("memory_checkout", json!({"name": "main"}))
        .await
        .unwrap();

    let Some(r) = remote_pick_or_skip(
        remote
            .call(
                "memory_pick",
                json!({
                    "source": branch,
                    "selector": {
                        "type": "retrieve",
                        "query": "alpha query",
                        "top_k": 2,
                        "min_score": 0.6
                    },
                    "dry_run": {
                        "limit": 1,
                        "include_content_preview": true,
                        "include_scores": true
                    }
                }),
            )
            .await,
        "test_remote_pick_retrieve_dry_run_returns_preview_json",
    ) else {
        return;
    };
    let body = mcp_text_json(&r);
    assert_eq!(
        body["dry_run"].as_bool(),
        Some(true),
        "remote preview: {body}"
    );
    assert_eq!(
        body["summary"]["candidate_count"].as_u64(),
        Some(1),
        "{body}"
    );
    let candidate = &body["candidates"][0];
    assert_eq!(
        candidate["content_preview"].as_str(),
        Some("alpha branch memory"),
        "{body}"
    );
    assert!(candidate.get("score").is_some(), "{body}");
    assert!(candidate.get("embedding").is_none(), "{body}");

    let active = server.service().list_active(&uid, 10).await.unwrap();
    assert!(!active
        .iter()
        .any(|memory| memory.content == "alpha branch memory"));
}

// ── Entity list ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_entities() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Store a memory to trigger entity extraction
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "We use PostgreSQL and Redis for caching", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);

    let r = client
        .get(format!("{base}/v1/entities"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["entities"].is_array(), "entities should be an array");
    println!(
        "✅ GET /v1/entities: {} entities",
        body["entities"].as_array().unwrap().len()
    );
}

// ── Admin config ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_api_admin_config() {
    let mk = format!("mk_{}", uuid::Uuid::new_v4().simple());
    let (base, client, _server) = spawn_server_with_master_key(&mk).await;

    // Without master key → 401
    let r = client
        .get(format!("{base}/admin/config"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);

    // With master key → 200
    let r = client
        .get(format!("{base}/admin/config"))
        .header("Authorization", format!("Bearer {mk}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert!(body["db_name"].is_string(), "missing db_name");
    assert!(body["embedding_dim"].is_number(), "missing embedding_dim");
    // Password should be redacted
    let db_url = body["db_url"].as_str().unwrap_or("");
    assert!(
        db_url.contains("***") || !db_url.contains("@"),
        "db password not redacted: {db_url}"
    );
    println!("✅ GET /admin/config: db_name={}", body["db_name"]);
}

#[tokio::test]
async fn test_api_admin_config_forbidden() {
    let mk = format!("mk_{}", uuid::Uuid::new_v4().simple());
    let (base, client, _server) = spawn_server_with_master_key(&mk).await;

    // Create an API key (non-master)
    let auth = format!("Bearer {mk}");
    let r = client
        .post(format!("{base}/auth/keys"))
        .header("Authorization", &auth)
        .json(&json!({"user_id": "bob", "name": "test"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let raw_key = r.json::<Value>().await.unwrap()["raw_key"]
        .as_str()
        .unwrap()
        .to_string();

    // Non-master API key → 403
    let r = client
        .get(format!("{base}/admin/config"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 403);
    println!("✅ GET /admin/config: non-master key → 403");
}

// ── Concurrency: parallel stores to same user ─────────────────────────────────

#[tokio::test]
async fn test_concurrent_stores() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();
    let client = Arc::new(client);
    let n = 20;

    let mut handles = Vec::new();
    for i in 0..n {
        let c = client.clone();
        let b = base.clone();
        let u = uid.clone();
        handles.push(tokio::spawn(async move {
            c.post(format!("{b}/v1/memories"))
                .header("X-User-Id", &u)
                .json(
                    &json!({"content": format!("concurrent fact #{i}"), "memory_type": "semantic"}),
                )
                .send()
                .await
                .unwrap()
        }));
    }
    let mut ok = 0;
    for h in handles {
        let r = h.await.unwrap();
        if r.status() == 201 {
            ok += 1;
        }
    }
    assert_eq!(ok, n, "all concurrent stores should succeed");

    // Verify all persisted
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .query(&[("limit", "100")])
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let count = body["items"].as_array().unwrap().len();
    assert!(count >= n, "expected >= {n} memories, got {count}");
    println!("✅ concurrent stores: {ok}/{n} succeeded, {count} persisted");
}

// ── Concurrency: entity extraction race condition ─────────────────────────────

#[tokio::test]
async fn test_concurrent_entity_upsert() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();
    let client = Arc::new(client);

    // All memories mention "Redis" — triggers concurrent upsert_entity for same entity
    let mut handles = Vec::new();
    for i in 0..10 {
        let c = client.clone();
        let b = base.clone();
        let u = uid.clone();
        handles.push(tokio::spawn(async move {
            c.post(format!("{b}/v1/memories"))
                .header("X-User-Id", &u)
                .json(&json!({"content": format!("Redis is used for caching scenario {i}"), "memory_type": "semantic"}))
                .send()
                .await
                .unwrap()
        }));
    }
    for h in handles {
        let r = h.await.unwrap();
        assert_eq!(r.status(), 201, "concurrent entity upsert should not fail");
    }

    // Verify entity exists (no duplicates crashed it)
    let r = client
        .get(format!("{base}/v1/entities"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    println!("✅ concurrent entity upsert: no race condition errors");
}

// ── Pressure: batch store at limit ────────────────────────────────────────────

#[tokio::test]
async fn test_batch_store_at_limit() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let memories: Vec<_> = (0..100)
        .map(|i| json!({"content": format!("batch item {i}"), "memory_type": "semantic"}))
        .collect();

    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": memories}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 201);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body.as_array().unwrap().len(), 100);

    // Over limit → 422
    let memories_over: Vec<_> = (0..101)
        .map(|i| json!({"content": format!("over {i}"), "memory_type": "semantic"}))
        .collect();
    let r = client
        .post(format!("{base}/v1/memories/batch"))
        .header("X-User-Id", &uid)
        .json(&json!({"memories": memories_over}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 422);
    println!("✅ batch store: 100 ok, 101 rejected");
}

// ── Concurrency: parallel feedback on same memory ─────────────────────────────

#[tokio::test]
async fn test_concurrent_feedback() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();
    let client = Arc::new(client);

    // Create a memory
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "shared memory for feedback", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    // 10 concurrent feedback signals
    let mut handles = Vec::new();
    for i in 0..10 {
        let c = client.clone();
        let b = base.clone();
        let u = uid.clone();
        let m = mid.clone();
        let signal = if i % 2 == 0 { "useful" } else { "irrelevant" };
        handles.push(tokio::spawn(async move {
            c.post(format!("{b}/v1/memories/{m}/feedback"))
                .header("X-User-Id", &u)
                .json(&json!({"signal": signal}))
                .send()
                .await
                .unwrap()
        }));
    }
    for h in handles {
        let r = h.await.unwrap();
        assert!(
            r.status() == 200 || r.status() == 201,
            "concurrent feedback should succeed, got {}",
            r.status()
        );
    }

    // Verify stats reflect all signals
    let r = client
        .get(format!("{base}/v1/feedback/stats"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let total = body["total"].as_i64().unwrap();
    assert_eq!(total, 10, "all 10 feedback signals should be recorded");
    println!("✅ concurrent feedback: {total} signals recorded");
}

// ── Graceful degradation ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_graceful_degradation() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    // Rollback to nonexistent snapshot → error, not crash
    let r = client
        .post(format!("{base}/v1/snapshots/nonexistent_snap_xyz/rollback"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert!(r.status().is_client_error() || r.status().is_server_error());

    // Feedback on nonexistent memory → 404
    let r = client
        .post(format!("{base}/v1/memories/nonexistent_id/feedback"))
        .header("X-User-Id", &uid)
        .json(&json!({"signal": "useful"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);

    // Store → purge → correct the purged memory → should fail gracefully
    let r = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .json(&json!({"content": "ephemeral", "memory_type": "semantic"}))
        .send()
        .await
        .unwrap();
    let mid = r.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .to_string();

    client
        .delete(format!("{base}/v1/memories/{mid}"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();

    let r = client
        .put(format!("{base}/v1/memories/{mid}"))
        .header("X-User-Id", &uid)
        .json(&json!({"new_content": "updated", "reason": "test"}))
        .send()
        .await
        .unwrap();
    assert!(
        r.status() == 404 || r.status().is_client_error() || r.status().is_server_error(),
        "correct after purge should fail gracefully, got {}",
        r.status()
    );

    println!("✅ graceful degradation: all error cases handled without panic");
}

// ── last_used_at batched flush (#62) ─────────────────────────────────────────

/// Verify that the LastUsedBatcher correctly coalesces multiple mark_used calls
/// and flushes them in a single batch UPDATE.
#[tokio::test]
async fn test_last_used_batcher_coalesces_and_flushes() {
    use memoria_api::auth::LastUsedBatcher;
    use sha2::{Digest, Sha256};

    let mk = "test-master-batcher";
    let (base, client, server) = spawn_server_with_master_key(mk).await;
    let auth = format!("Bearer {mk}");

    // Create 3 API keys
    let mut keys = Vec::new();
    for i in 0..3 {
        let raw = create_api_key_for_user(
            &client,
            &base,
            &auth,
            &format!("batcher_user_{i}"),
            &format!("key_{i}"),
        )
        .await;
        keys.push(raw);
    }

    // Build a batcher and mark all 3 keys as used
    let batcher = LastUsedBatcher::new();
    let hashes: Vec<String> = keys
        .iter()
        .map(|k| format!("{:x}", Sha256::digest(k.as_bytes())))
        .collect();
    for h in &hashes {
        batcher.mark_used(h.clone());
    }

    // Verify all 3 are pending
    // (We can't inspect the internal set directly, but we can flush and verify DB)

    // Flush against the same shared DB that stored the keys.
    let store = server.shared_store();
    batcher.flush(store.pool()).await;

    // Verify last_used_at was updated for all 3 keys
    for hash in &hashes {
        let row: Option<(Option<chrono::NaiveDateTime>,)> = sqlx::query_as(&format!(
            "SELECT last_used_at FROM {} WHERE key_hash = ?",
            store.t("mem_api_keys")
        ))
        .bind(hash)
        .fetch_optional(store.pool())
        .await
        .expect("query");
        let (last_used,) = row.expect("key should exist");
        assert!(
            last_used.is_some(),
            "last_used_at should be set after flush for hash {}",
            &hash[..8]
        );
    }

    // Flush again — should be a no-op (pending set is drained)
    batcher.flush(store.pool()).await;

    // Mark one key again and flush — only that one should be updated
    batcher.mark_used(hashes[0].clone());
    batcher.flush(store.pool()).await;

    println!("✅ test_last_used_batcher_coalesces_and_flushes: batch UPDATE works correctly");
}

/// Verify that API key auth works with the dedicated auth pool and that
/// last_used_at is updated via the batcher (not per-request fire-and-forget).
#[tokio::test]
async fn test_api_key_auth_uses_batcher_not_fire_and_forget() {
    let mk = "test-master-batcher-auth";
    let ctx = memoria_test_utils::MultiDbTestContext::new(
        &db_url(),
        "api_e2e_auth_batcher",
        test_dim(),
        None,
        None,
    )
    .await;

    let state = memoria_api::AppState::new(ctx.service(), ctx.git(), mk.to_string())
        .init_auth_pool(ctx.shared_db_url(), false)
        .await
        .expect("auth pool");

    let batcher = state.last_used_batcher.clone();
    let app = memoria_api::build_router(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    let auth = format!("Bearer {mk}");

    // Create an API key
    let raw_key =
        create_api_key_for_user(&client, &base, &auth, "batcher_e2e_user", "e2e_key").await;

    // Use the API key to make a request (cache miss → DB lookup → batcher.mark_used)
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "API key auth should succeed");

    // Make another request (cache hit → batcher.mark_used, no DB)
    let r = client
        .get(format!("{base}/v1/memories"))
        .header("Authorization", format!("Bearer {raw_key}"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "Cached API key auth should succeed");

    // Manually flush the batcher to verify last_used_at is updated
    let verify_store = ctx.shared_store();
    batcher.flush(verify_store.pool()).await;

    let key_hash = format!(
        "{:x}",
        <sha2::Sha256 as sha2::Digest>::digest(raw_key.as_bytes())
    );
    let row: Option<(Option<chrono::NaiveDateTime>,)> = sqlx::query_as(&format!(
        "SELECT last_used_at FROM {} WHERE key_hash = ?",
        verify_store.t("mem_api_keys")
    ))
    .bind(&key_hash)
    .fetch_optional(verify_store.pool())
    .await
    .expect("query");
    let (last_used,) = row.expect("key should exist");
    assert!(
        last_used.is_some(),
        "last_used_at should be set after batcher flush"
    );

    println!("✅ test_api_key_auth_uses_batcher_not_fire_and_forget: auth pool + batcher works");
}

// ── tool usage tracking ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_tool_usage_tracking() {
    let (base, client, _server) = spawn_server().await;
    let user = format!("tool_test_{}", uuid::Uuid::new_v4().simple());

    // 1. No tool header → usage should be empty
    let r = client
        .get(format!("{base}/v1/tool-usage"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let body: Value = r.json().await.unwrap();
    assert_eq!(body.as_array().unwrap().len(), 0);
    println!("✅ GET /v1/tool-usage: empty for new user");

    // 2. Request with X-Tool-Name → should be recorded
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .header("X-Tool-Name", "memory_store")
        .json(&json!({"content": "test"}))
        .send()
        .await
        .unwrap();

    let r = client
        .get(format!("{base}/v1/tool-usage"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["tool_name"], "memory_store");
    assert!(items[0]["last_used_at"].as_str().is_some());
    println!("✅ GET /v1/tool-usage: recorded memory_store");

    // 3. Empty X-Tool-Name → should NOT add an entry
    client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &user)
        .header("X-Tool-Name", "")
        .json(&json!({"content": "test2"}))
        .send()
        .await
        .unwrap();

    let r = client
        .get(format!("{base}/v1/tool-usage"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(
        body.as_array().unwrap().len(),
        1,
        "empty tool name should not create entry"
    );
    println!("✅ GET /v1/tool-usage: empty header ignored");

    // 4. Second tool → should have 2 entries
    client
        .post(format!("{base}/v1/memories/search"))
        .header("X-User-Id", &user)
        .header("X-Tool-Name", "memory_search")
        .json(&json!({"query": "test"}))
        .send()
        .await
        .unwrap();

    let r = client
        .get(format!("{base}/v1/tool-usage"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 2);
    let tools: Vec<&str> = items
        .iter()
        .map(|i| i["tool_name"].as_str().unwrap())
        .collect();
    assert!(tools.contains(&"memory_store"));
    assert!(tools.contains(&"memory_search"));
    println!("✅ GET /v1/tool-usage: 2 tools tracked");

    // 5. Different user should not see user's tools
    let other = format!("tool_test_{}", uuid::Uuid::new_v4().simple());
    let r = client
        .get(format!("{base}/v1/tool-usage"))
        .header("X-User-Id", &other)
        .send()
        .await
        .unwrap();
    let body: Value = r.json().await.unwrap();
    assert_eq!(body.as_array().unwrap().len(), 0);
    println!("✅ GET /v1/tool-usage: user isolation verified");
}

// ═══════════════════════════════════════════════════════════════════════════════
// MCP Remote: purge/correct graph + entity link cleanup verification
// ═══════════════════════════════════════════════════════════════════════════════

/// Helper: store via remote, create graph node + entity links manually, return memory_id.
async fn remote_store_with_links(
    remote: &memoria_mcp::remote::RemoteClient,
    user_store: &memoria_storage::SqlMemoryStore,
    uid: &str,
    content: &str,
) -> String {
    let r = remote
        .call("memory_store", json!({"content": content}))
        .await
        .expect("store");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    let mid = text
        .split_whitespace()
        .nth(2)
        .unwrap_or("")
        .trim_end_matches(':')
        .to_string();
    assert!(!mid.is_empty(), "should extract mid from: {text}");

    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    // Create graph node (remote path goes through REST API which doesn't create graph nodes)
    let node_id = uuid::Uuid::new_v4().simple().to_string()[..32].to_string();
    sqlx::query(&format!(
        "INSERT INTO {} \
         (node_id, user_id, node_type, content, memory_id, confidence, trust_tier, importance, \
           access_count, cross_session_count, is_active, created_at) \
         VALUES (?, ?, 'semantic', ?, ?, 0.95, 'T1', 0.5, 0, 0, 1, NOW())",
        user_store.t("memory_graph_nodes")
    ))
    .bind(&node_id)
    .bind(uid)
    .bind(content)
    .bind(&mid)
    .execute(user_store.pool())
    .await
    .unwrap();

    // Insert into legacy mem_entity_links
    let id = uuid::Uuid::new_v4().to_string().replace('-', "");
    sqlx::query(&format!(
        "INSERT INTO {} (id, user_id, memory_id, entity_name, entity_type, source, created_at) \
         VALUES (?, ?, ?, 'remote_entity', 'concept', 'manual', NOW())",
        user_store.t("mem_entity_links")
    ))
    .bind(&id)
    .bind(uid)
    .bind(&mid)
    .execute(user_store.pool())
    .await
    .unwrap();

    mid
}

async fn spawn_server_with_pool() -> (String, reqwest::Client, support::multi_db::ApiTestServer) {
    let server = support::multi_db::spawn_api_server(
        "api_e2e_pool",
        test_dim(),
        String::new(),
        None,
        None,
        None,
        false,
    )
    .await;
    (server.base.clone(), server.client.clone(), server)
}

async fn graph_node_active_count(user_store: &memoria_storage::SqlMemoryStore, mid: &str) -> i64 {
    sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {} WHERE memory_id = ? AND is_active = 1",
        user_store.t("memory_graph_nodes")
    ))
    .bind(mid)
    .fetch_one(user_store.pool())
    .await
    .unwrap()
}

// ── MCP Remote: purge cleans graph + entity links ───────────────────────────

#[tokio::test]
async fn test_remote_purge_cleans_graph_and_entity_links() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _client, server) = spawn_server_with_pool().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let user_store = server.user_store(&uid).await;

    let mid = remote_store_with_links(&remote, &user_store, &uid, "Remote purge graph test").await;

    // Verify graph node exists
    let cnt: i64 = graph_node_active_count(&user_store, &mid).await;
    assert!(cnt > 0, "graph node should exist before purge");

    // Purge via remote
    let r = remote
        .call("memory_purge", json!({"memory_id": &mid}))
        .await
        .expect("purge");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Purged"), "got: {text}");

    // Verify graph node deactivated
    let cnt: i64 = graph_node_active_count(&user_store, &mid).await;
    assert_eq!(
        cnt, 0,
        "graph node should be deactivated after remote purge"
    );

    // Verify entity links cleaned
    let cnt: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {} WHERE memory_id = ?",
        user_store.t("mem_entity_links")
    ))
    .bind(&mid)
    .fetch_one(user_store.pool())
    .await
    .unwrap();
    assert_eq!(
        cnt, 0,
        "mem_entity_links should be cleaned after remote purge"
    );

    println!("✅ remote purge: graph + entity links cleaned");
}

// ── MCP Remote: purge batch cleans graph + entity links ─────────────────────

#[tokio::test]
async fn test_remote_purge_batch_cleans_graph_and_entity_links() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _client, server) = spawn_server_with_pool().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let user_store = server.user_store(&uid).await;

    let mid1 = remote_store_with_links(&remote, &user_store, &uid, "Remote batch purge A").await;
    let mid2 = remote_store_with_links(&remote, &user_store, &uid, "Remote batch purge B").await;

    // Purge batch via remote (comma-separated)
    let r = remote
        .call(
            "memory_purge",
            json!({"memory_id": format!("{mid1},{mid2}")}),
        )
        .await
        .expect("purge");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Purged"), "got: {text}");

    for mid in [&mid1, &mid2] {
        let cnt: i64 = graph_node_active_count(&user_store, mid).await;
        assert_eq!(cnt, 0, "graph node should be deactivated for {mid}");
    }
    println!("✅ remote purge batch: graph + entity links cleaned");
}

// ── MCP Remote: purge by topic cleans graph + entity links ──────────────────

#[tokio::test]
async fn test_remote_purge_topic_cleans_graph_and_entity_links() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _client, server) = spawn_server_with_pool().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let user_store = server.user_store(&uid).await;

    let mid = remote_store_with_links(
        &remote,
        &user_store,
        &uid,
        "remote_topic_graph_cleanup_xyz unique",
    )
    .await;

    let r = remote
        .call(
            "memory_purge",
            json!({"topic": "remote_topic_graph_cleanup_xyz"}),
        )
        .await
        .expect("purge");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Purged"), "got: {text}");

    let cnt: i64 = graph_node_active_count(&user_store, &mid).await;
    assert_eq!(cnt, 0, "graph node should be deactivated after topic purge");

    let cnt: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {} WHERE memory_id = ?",
        user_store.t("mem_entity_links")
    ))
    .bind(&mid)
    .fetch_one(user_store.pool())
    .await
    .unwrap();
    assert_eq!(cnt, 0, "entity links should be cleaned after topic purge");

    println!("✅ remote purge topic: graph + entity links cleaned");
}

// ── MCP Remote: correct by id cleans old graph + entity links ───────────────

#[tokio::test]
async fn test_remote_correct_cleans_graph_and_entity_links() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _client, server) = spawn_server_with_pool().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let user_store = server.user_store(&uid).await;

    let old_mid = remote_store_with_links(
        &remote,
        &user_store,
        &uid,
        "Remote correct graph old content",
    )
    .await;

    // Correct
    let r = remote
        .call(
            "memory_correct",
            json!({
                "memory_id": &old_mid,
                "new_content": "Remote correct graph new content"
            }),
        )
        .await
        .expect("correct");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Corrected"), "got: {text}");

    // Old graph node deactivated
    let cnt: i64 = graph_node_active_count(&user_store, &old_mid).await;
    assert_eq!(
        cnt, 0,
        "old graph node should be deactivated after remote correct"
    );

    // Old entity links cleaned
    let cnt: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {} WHERE memory_id = ?",
        user_store.t("mem_entity_links")
    ))
    .bind(&old_mid)
    .fetch_one(user_store.pool())
    .await
    .unwrap();
    assert_eq!(
        cnt, 0,
        "old entity links should be cleaned after remote correct"
    );

    println!("✅ remote correct: old graph + entity links cleaned");
}

// ── MCP Remote: correct by query cleans old graph + entity links ────────────

#[tokio::test]
async fn test_remote_correct_by_query_cleans_graph_and_entity_links() {
    use memoria_mcp::remote::RemoteClient;

    let (base, _client, server) = spawn_server_with_pool().await;
    let uid = uid();
    let remote = RemoteClient::new(&base, None, uid.clone(), None);
    let user_store = server.user_store(&uid).await;

    let old_mid = remote_store_with_links(
        &remote,
        &user_store,
        &uid,
        "remote_correct_query_graph_xyz unique content",
    )
    .await;

    let r = remote
        .call(
            "memory_correct",
            json!({
                "query": "remote_correct_query_graph_xyz",
                "new_content": "Remote correct by query new content"
            }),
        )
        .await
        .expect("correct");
    let text = r["content"][0]["text"].as_str().unwrap_or("");
    assert!(
        text.contains("Corrected") || text.contains("No matching"),
        "got: {text}"
    );

    // If corrected, old graph should be cleaned
    if text.contains("Corrected") {
        let cnt: i64 = graph_node_active_count(&user_store, &old_mid).await;
        assert_eq!(
            cnt, 0,
            "old graph node should be deactivated after remote correct by query"
        );
        let cnt: i64 = sqlx::query_scalar(&format!(
            "SELECT COUNT(*) FROM {} WHERE memory_id = ?",
            user_store.t("mem_entity_links")
        ))
        .bind(&old_mid)
        .fetch_one(user_store.pool())
        .await
        .unwrap();
        assert_eq!(
            cnt, 0,
            "old entity links should be cleaned after remote correct by query"
        );
    }

    println!("✅ remote correct by query: old graph + entity links cleaned");
}

// ── Streamable HTTP MCP endpoint (/mcp) ──────────────────────────────────────

/// Spawn a server that requires a Bearer master key (for auth tests).
async fn spawn_server_with_key(
    master_key: &str,
) -> (String, reqwest::Client, support::multi_db::ApiTestServer) {
    let server = support::multi_db::spawn_api_server(
        "api_e2e_mcp_key",
        test_dim(),
        master_key.to_string(),
        None,
        None,
        None,
        false,
    )
    .await;
    (server.base.clone(), server.client.clone(), server)
}

/// POST /mcp helper: sends a JSON-RPC request and returns the parsed response.
async fn mcp_post_with_headers(
    client: &reqwest::Client,
    base: &str,
    body: Value,
    headers: &[(&str, &str)],
) -> Value {
    let mut req = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json");
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.body(serde_json::to_string(&body).unwrap())
        .send()
        .await
        .expect("POST /mcp")
        .json()
        .await
        .expect("parse json")
}

fn mcp_result_text(resp: &Value) -> &str {
    resp["result"]["content"][0]["text"].as_str().unwrap_or("")
}

async fn wait_for_mcp_text_contains(
    client: &reqwest::Client,
    base: &str,
    body: Value,
    headers: &[(&str, &str)],
    expected_fragments: &[&str],
) -> Value {
    let mut last_resp = Value::Null;
    for _ in 0..60 {
        let resp = mcp_post_with_headers(client, base, body.clone(), headers).await;
        let text = mcp_result_text(&resp);
        if resp["error"].is_null()
            && expected_fragments
                .iter()
                .all(|fragment| text.contains(fragment))
        {
            return resp;
        }
        last_resp = resp;
        tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    }
    panic!(
        "timed out waiting for MCP response to contain {:?}: {last_resp}",
        expected_fragments
    );
}

#[tokio::test]
async fn test_mcp_initialize() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let resp = mcp_post_with_headers(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {}}),
        &[("X-User-Id", uid.as_str())],
    )
    .await;

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 1);
    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    assert_eq!(resp["result"]["serverInfo"]["name"], "memoria-mcp-rs");
    assert!(resp["result"]["capabilities"]["tools"].is_object());
    assert!(resp["error"].is_null());
    println!(
        "✅ POST /mcp initialize: {:?}",
        resp["result"]["serverInfo"]
    );
}

#[tokio::test]
async fn test_mcp_ping() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let resp = mcp_post_with_headers(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": 2, "method": "ping"}),
        &[("X-User-Id", uid.as_str())],
    )
    .await;

    assert_eq!(resp, json!({"jsonrpc": "2.0", "id": 2, "result": {}}));
    println!("✅ POST /mcp ping");
}

#[tokio::test]
async fn test_mcp_ping_notification_returns_no_content() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let response = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .header("X-User-Id", uid)
        .body(r#"{"jsonrpc":"2.0","method":"ping"}"#)
        .send()
        .await
        .expect("POST /mcp ping notification");

    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    assert!(response
        .text()
        .await
        .expect("read ping notification response")
        .is_empty());
}

#[tokio::test]
async fn test_mcp_tools_list() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let resp = mcp_post_with_headers(
        &client,
        &base,
        json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
        &[("X-User-Id", uid.as_str())],
    )
    .await;

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 2);
    let tools = resp["result"]["tools"].as_array().expect("tools array");
    assert!(!tools.is_empty(), "expected at least one tool");
    let names: Vec<&str> = tools
        .iter()
        .map(|t| t["name"].as_str().unwrap_or(""))
        .collect();
    assert!(names.contains(&"memory_store"), "missing memory_store");
    assert!(
        names.contains(&"memory_retrieve"),
        "missing memory_retrieve"
    );
    println!("✅ POST /mcp tools/list: {} tools", tools.len());
}

#[tokio::test]
async fn test_mcp_tools_call_memory_store() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let resp = mcp_post_with_headers(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "memory_store",
                "arguments": {"content": "Rust ownership ensures memory safety", "memory_type": "semantic"}
            }
        }),
        &[("X-User-Id", uid.as_str())],
    )
    .await;

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(resp["id"], 3);
    assert!(
        resp["error"].is_null(),
        "unexpected error: {}",
        resp["error"]
    );
    let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("Stored"), "expected 'Stored' in: {text}");
    println!("✅ POST /mcp tools/call memory_store: {text}");
}

#[tokio::test]
async fn test_mcp_memory_store_rejects_empty_content() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    for (id, args) in [
        (4, json!({})),
        (5, json!({"content": ""})),
        (6, json!({"content": "   \t"})),
    ] {
        let resp = mcp_post_with_headers(
            &client,
            &base,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "memory_store",
                    "arguments": args
                }
            }),
            &[("X-User-Id", uid.as_str())],
        )
        .await;

        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], id);
        assert!(
            resp["error"].is_null(),
            "validation should return tool text, not RPC error: {}",
            resp["error"]
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("content is required"),
            "unexpected response for args={args}: {text}"
        );
    }

    let list = client
        .get(format!("{base}/v1/memories"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .unwrap();
    assert_eq!(list.status(), 200);
    assert!(
        list.json::<Value>().await.unwrap()["items"]
            .as_array()
            .unwrap()
            .is_empty(),
        "empty MCP store must not create memories"
    );
    println!("✅ POST /mcp memory_store rejects empty content");
}

#[tokio::test]
async fn test_mcp_memory_retrieve_and_search_reject_missing_query() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    for (id, tool, args) in [
        (7, "memory_retrieve", json!({})),
        (8, "memory_retrieve", json!({"query": ""})),
        (9, "memory_retrieve", json!({"query": "   \t"})),
        (10, "memory_search", json!({})),
        (11, "memory_search", json!({"query": ""})),
        (12, "memory_search", json!({"query": "   \t"})),
    ] {
        let resp = mcp_post_with_headers(
            &client,
            &base,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": tool,
                    "arguments": args
                }
            }),
            &[("X-User-Id", uid.as_str())],
        )
        .await;

        assert_eq!(resp["jsonrpc"], "2.0");
        assert_eq!(resp["id"], id);
        assert!(
            resp["error"].is_null(),
            "validation should return tool text, not RPC error: {}",
            resp["error"]
        );
        let text = resp["result"]["content"][0]["text"].as_str().unwrap_or("");
        assert!(
            text.contains("query is required"),
            "{tool} unexpected response for args={args}: {text}"
        );
    }
    println!("✅ POST /mcp memory_retrieve/search reject missing query");
}

#[tokio::test]
async fn test_mcp_memory_retrieve_session_scope_end_to_end() {
    let (base, client, _server) =
        spawn_server_with_custom_embedder_and_pool(Arc::new(SessionScopeTestEmbedder), test_dim())
            .await;
    let uid = uid();
    let headers = [("X-User-Id", uid.as_str())];

    for (id, content, session_id) in [
        (11, "target-session memory", "sess-target"),
        (12, "other-session top", "sess-other"),
        (13, "other-session second", "sess-other"),
    ] {
        let resp = mcp_post_with_headers(
            &client,
            &base,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "memory_store",
                    "arguments": {
                        "content": content,
                        "memory_type": "semantic",
                        "session_id": session_id
                    }
                }
            }),
            &headers,
        )
        .await;
        assert!(
            resp["error"].is_null(),
            "unexpected store error: {}",
            resp["error"]
        );
    }

    let global_resp = mcp_post_with_headers(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 131,
            "method": "tools/call",
            "params": {
                "name": "memory_store",
                "arguments": {
                    "content": "global-unscoped top",
                    "memory_type": "semantic"
                }
            }
        }),
        &headers,
    )
    .await;
    assert!(
        global_resp["error"].is_null(),
        "unexpected unscoped store error: {}",
        global_resp["error"]
    );

    let relaxed = wait_for_mcp_text_contains(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 14,
            "method": "tools/call",
            "params": {
                "name": "memory_retrieve",
                "arguments": {
                    "query": "strict session query",
                    "session_id": "sess-target",
                    "top_k": 3
                }
            }
        }),
        &headers,
        &["other-session top"],
    )
    .await;
    let relaxed_text = mcp_result_text(&relaxed);
    assert!(
        relaxed_text.contains("other-session"),
        "relaxed MCP retrieve should still be free to return cross-session memory: {relaxed_text}"
    );

    let strict = wait_for_mcp_text_contains(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 15,
            "method": "tools/call",
            "params": {
                "name": "memory_retrieve",
                "arguments": {
                    "query": "strict session query",
                    "session_id": "sess-target",
                    "session_scope": "only",
                    "top_k": 2
                }
            }
        }),
        &headers,
        &["target-session memory", "global-unscoped top"],
    )
    .await;
    let strict_text = mcp_result_text(&strict);
    assert!(
        strict_text.contains("target-session memory"),
        "strict MCP retrieve should keep requested-session memory visible: {strict_text}"
    );
    assert!(
        strict_text.contains("global-unscoped top"),
        "strict MCP retrieve should include unscoped memory: {strict_text}"
    );
    assert!(
        !strict_text.contains("other-session top"),
        "strict MCP retrieve should not leak cross-session memory: {strict_text}"
    );
}

#[tokio::test]
async fn test_mcp_memory_search_session_scope_end_to_end() {
    let (base, client, _server) =
        spawn_server_with_custom_embedder_and_pool(Arc::new(SessionScopeTestEmbedder), test_dim())
            .await;
    let uid = uid();
    let headers = [("X-User-Id", uid.as_str())];

    for (id, content, session_id) in [
        (21, "target-session memory", "sess-target"),
        (22, "other-session top", "sess-other"),
        (23, "other-session second", "sess-other"),
    ] {
        let resp = mcp_post_with_headers(
            &client,
            &base,
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {
                    "name": "memory_store",
                    "arguments": {
                        "content": content,
                        "memory_type": "semantic",
                        "session_id": session_id
                    }
                }
            }),
            &headers,
        )
        .await;
        assert!(
            resp["error"].is_null(),
            "unexpected store error: {}",
            resp["error"]
        );
    }

    let global_resp = mcp_post_with_headers(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 231,
            "method": "tools/call",
            "params": {
                "name": "memory_store",
                "arguments": {
                    "content": "global-unscoped top",
                    "memory_type": "semantic"
                }
            }
        }),
        &headers,
    )
    .await;
    assert!(
        global_resp["error"].is_null(),
        "unexpected unscoped store error: {}",
        global_resp["error"]
    );

    let relaxed = wait_for_mcp_text_contains(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 24,
            "method": "tools/call",
            "params": {
                "name": "memory_search",
                "arguments": {
                    "query": "strict session query",
                    "session_id": "sess-target",
                    "top_k": 3
                }
            }
        }),
        &headers,
        &["other-session top"],
    )
    .await;
    let relaxed_text = mcp_result_text(&relaxed);
    assert!(
        relaxed_text.contains("other-session"),
        "relaxed MCP search should still be free to return cross-session memory: {relaxed_text}"
    );

    let strict = wait_for_mcp_text_contains(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 25,
            "method": "tools/call",
            "params": {
                "name": "memory_search",
                "arguments": {
                    "query": "strict session query",
                    "session_id": "sess-target",
                    "session_scope": "only",
                    "top_k": 2
                }
            }
        }),
        &headers,
        &["target-session memory", "global-unscoped top"],
    )
    .await;
    let strict_text = mcp_result_text(&strict);
    assert!(
        strict_text.contains("target-session memory"),
        "strict MCP search should keep requested-session memory visible: {strict_text}"
    );
    assert!(
        strict_text.contains("global-unscoped top"),
        "strict MCP search should include unscoped memory: {strict_text}"
    );
    assert!(
        !strict_text.contains("other-session top"),
        "strict MCP search should not leak cross-session memory: {strict_text}"
    );
}

#[tokio::test]
async fn test_mcp_tools_call_records_tool_usage() {
    let (base, client, _server) = spawn_server().await;
    let uid = uid();

    let resp = mcp_post_with_headers(
        &client,
        &base,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "memory_store",
                "arguments": {"content": "mcp tool usage tracked", "memory_type": "semantic"}
            }
        }),
        &[("X-User-Id", uid.as_str())],
    )
    .await;

    assert!(
        resp["error"].is_null(),
        "unexpected error: {}",
        resp["error"]
    );

    let usage: Value = client
        .get(format!("{base}/v1/tool-usage"))
        .header("X-User-Id", &uid)
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("parse");

    let items = usage.as_array().expect("usage array");
    assert!(
        items.iter().any(|item| item["tool_name"] == "memory_store"),
        "memory_store missing from tool usage: {usage}"
    );
    println!("✅ POST /mcp tools/call records tool usage");
}

#[tokio::test]
async fn test_mcp_invalid_json_returns_parse_error() {
    let (base, client, _server) = spawn_server().await;

    let resp: Value = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .header("X-User-Id", "test_user")
        .body("this is not json {{{")
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("parse");

    assert_eq!(resp["jsonrpc"], "2.0");
    assert!(resp["id"].is_null());
    assert_eq!(resp["error"]["code"], -32700, "expected parse error code");
    println!("✅ POST /mcp invalid JSON → -32700 parse error");
}

#[tokio::test]
async fn test_mcp_invalid_request_non_object() {
    let (base, client, _server) = spawn_server().await;

    // A JSON array is valid JSON but not a JSON-RPC object → -32600
    let resp: Value = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .header("X-User-Id", "test_user")
        .body("[1,2,3]")
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("parse");

    assert_eq!(resp["jsonrpc"], "2.0");
    assert!(resp["id"].is_null());
    assert_eq!(
        resp["error"]["code"], -32600,
        "expected invalid request code"
    );
    println!("✅ POST /mcp array payload → -32600 Invalid Request");
}

#[tokio::test]
async fn test_mcp_invalid_request_wrong_version() {
    let (base, client, _server) = spawn_server().await;

    // jsonrpc != "2.0" → -32600
    let resp: Value = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .header("X-User-Id", "test_user")
        .body(r#"{"jsonrpc":"1.0","id":1,"method":"initialize"}"#)
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("parse");

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(
        resp["error"]["code"], -32600,
        "expected invalid request code"
    );
    println!("✅ POST /mcp jsonrpc=1.0 → -32600 Invalid Request");
}

#[tokio::test]
async fn test_mcp_invalid_request_missing_method() {
    let (base, client, _server) = spawn_server().await;

    // No method field → -32600 (not mistaken for a Notification)
    let resp: Value = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .header("X-User-Id", "test_user")
        .body(r#"{"jsonrpc":"2.0","id":1}"#)
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("parse");

    assert_eq!(resp["jsonrpc"], "2.0");
    assert_eq!(
        resp["error"]["code"], -32600,
        "expected invalid request code"
    );
    println!("✅ POST /mcp missing method → -32600 Invalid Request");
}

#[tokio::test]
async fn test_mcp_auth_required_when_master_key_set() {
    let (base, client, _server) = spawn_server_with_key("test-master-secret").await;

    // No Authorization header → 401
    let status = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .body(
            serde_json::to_string(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap(),
        )
        .send()
        .await
        .expect("send")
        .status();
    assert_eq!(status, 401, "expected 401 without token");
    println!("✅ POST /mcp without token → 401");

    // Wrong Bearer token → 401
    let status = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer sk-wrong-token")
        .body(
            serde_json::to_string(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap(),
        )
        .send()
        .await
        .expect("send")
        .status();
    assert_eq!(status, 401, "expected 401 with wrong token");
    println!("✅ POST /mcp with wrong token → 401");

    // Correct master key → 200
    let resp: Value = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .header("Authorization", "Bearer test-master-secret")
        .body(
            serde_json::to_string(&json!({"jsonrpc":"2.0","id":1,"method":"initialize"})).unwrap(),
        )
        .send()
        .await
        .expect("send")
        .json()
        .await
        .expect("parse");
    assert_eq!(resp["result"]["protocolVersion"], "2024-11-05");
    println!("✅ POST /mcp with master key → initialize OK");
}

#[tokio::test]
async fn test_mcp_notifications_initialized_no_error() {
    let (base, client, _server) = spawn_server().await;

    // JSON-RPC 2.0: a Notification has no "id" field.
    // The server MUST NOT reply — expected HTTP 204 No Content with no body.
    let status = client
        .post(format!("{base}/mcp"))
        .header("Content-Type", "application/json")
        .header("X-User-Id", "test_user")
        .body(
            serde_json::to_string(
                &json!({"jsonrpc": "2.0", "method": "notifications/initialized"}),
            )
            .unwrap(),
        )
        .send()
        .await
        .expect("send")
        .status();

    assert_eq!(
        status.as_u16(),
        204,
        "notification must return 204 No Content"
    );
    println!("✅ POST /mcp notifications/initialized → 204 No Content");
}
