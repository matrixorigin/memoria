use std::io::{self, Write};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use tracing::instrument::WithSubscriber;

use memoria_core::{interfaces::EmbeddingProvider, MemoriaError};
use serde_json::{json, Value};

mod support;

#[derive(Default)]
struct FixedEmbedding(AtomicUsize);

#[derive(Clone, Default)]
struct TraceCapture(Arc<Mutex<Vec<u8>>>);

impl Write for TraceCapture {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl EmbeddingProvider for FixedEmbedding {
    async fn embed(&self, _: &str) -> Result<Vec<f32>, MemoriaError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        let mut vector = vec![0.0; 16];
        vector[0] = 1.0;
        Ok(vector)
    }

    fn dimension(&self) -> usize {
        16
    }
}

/// The distractors have identical vectors/text and stronger initial confidence.
/// A post-TopK filter cannot recover the target; both candidate paths must filter.
#[tokio::test]
async fn filters_apply_before_topk_and_do_not_broaden_empty_results() {
    let master = "synthetic-memory-filter-test-master";
    let embedder = Arc::new(FixedEmbedding::default());
    let server = support::multi_db::spawn_api_server(
        "memory_filter",
        16,
        master.into(),
        Some(embedder.clone()),
        None,
        None,
        false,
    )
    .await;
    let user = format!("filter_{}", uuid::Uuid::new_v4().simple());
    let mut target = String::new();
    for (name, subject, scene, tags, confidence, observed) in [
        (
            "target",
            "subject-a",
            "support",
            json!(["follow_up"]),
            0.8,
            "2026-11-03T04:00:00Z",
        ),
        (
            "wrong-scene",
            "subject-a",
            "other",
            json!(["follow_up"]),
            1.0,
            "2026-11-03T04:00:00Z",
        ),
        (
            "wrong-subject",
            "subject-b",
            "support",
            json!(["follow_up"]),
            1.0,
            "2026-11-03T04:00:00Z",
        ),
        (
            "scalar-not-array",
            "subject-a",
            "support",
            json!("follow_up"),
            1.0,
            "2026-11-03T04:00:00Z",
        ),
        (
            "low-confidence",
            "subject-a",
            "support",
            json!(["follow_up"]),
            0.1,
            "2026-11-03T04:00:00Z",
        ),
        (
            "old-source",
            "subject-a",
            "support",
            json!(["follow_up"]),
            1.0,
            "2020-01-01T00:00:00Z",
        ),
    ] {
        let response = server.client.post(format!("{}/v1/memories", server.base))
            .bearer_auth(master).header("X-User-Id", &user)
            .json(&json!({
                "content": "quartz shared fact", "subject_id": subject,
                "initial_confidence": confidence, "observed_at": observed,
                "extra_metadata": {"test_case": name, "labels": {"scene": scene, "scene_tags": tags}}
            })).send().await.unwrap();
        assert!(
            response.status().is_success(),
            "seed {name} failed with {}",
            response.status()
        );
        let memory: Value = response.json().await.unwrap();
        if name == "target" {
            target = memory["memory_id"].as_str().unwrap().into();
        }
    }
    assert!(!target.is_empty());
    let filter = json!({
        "metadata": [
            {"path": "labels.scene", "operator": "equal", "value": "support"},
            {"path": "labels.scene_tags", "operator": "contains", "value": "follow_up"}
        ],
        "observed_at_gte": "2026-11-03T12:00:00+08:00",
        "observed_at_lte": "2026-11-03T04:00:00Z",
        "min_initial_confidence": 0.8
    });
    for endpoint in ["retrieve", "search", "fulltext-search", "query", "retrieve"] {
        let request = if matches!(endpoint, "query" | "fulltext-search") {
            json!({"query": "quartz", "limit": 1, "subject_id": "subject-a", "filter": filter})
        } else {
            json!({"query": "quartz", "top_k": 1, "subject_id": "subject-a", "filter": filter})
        };
        let mut request = request;
        if endpoint == "query" {
            request.as_object_mut().unwrap().remove("query");
        }
        let response = server
            .client
            .post(format!("{}/v1/memories/{endpoint}", server.base))
            .bearer_auth(master)
            .header("X-User-Id", &user)
            .json(&request)
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{endpoint} failed with {}",
            response.status()
        );
        let result: Value = response.json().await.unwrap();
        let memories = if endpoint == "query" {
            result["items"].as_array().unwrap()
        } else {
            result.as_array().unwrap()
        };
        let ids: Vec<_> = memories
            .iter()
            .map(|memory| memory["memory_id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [target.as_str()],
            "{endpoint} returned an ineligible candidate"
        );
    }
    for endpoint in ["retrieve", "search"] {
        for value in ["missing-scene", "x' OR 1=1 --"] {
            let response = server
                .client
                .post(format!("{}/v1/memories/{endpoint}", server.base))
                .bearer_auth(master)
                .header("X-User-Id", &user)
                .json(
                    &json!({"query": "quartz", "top_k": 1, "subject_id": "subject-a", "filter": {
                        "metadata": [{"path": "labels.scene", "operator": "equal", "value": value}]
                    }}),
                )
                .send()
                .await
                .unwrap();
            assert!(response.status().is_success());
            assert_eq!(response.json::<Value>().await.unwrap(), json!([]));
        }
    }
    let calls = embedder.0.load(Ordering::SeqCst);
    let mut vector = vec![0.0; 16];
    vector[0] = 1.0;
    let response = server.client.post(format!("{}/v1/memories/vector-search", server.base))
        .bearer_auth(master).header("X-User-Id", &user)
        .json(&json!({"query_vector": vector, "top_k": 1, "subject_id": "subject-a", "filter": filter}))
        .send().await.unwrap();
    assert!(
        response.status().is_success(),
        "vector search failed with {}",
        response.status()
    );
    let memories: Vec<Value> = response.json().await.unwrap();
    assert_eq!(
        memories
            .iter()
            .map(|memory| memory["memory_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [target.as_str()]
    );
    assert_eq!(
        embedder.0.load(Ordering::SeqCst),
        calls,
        "query-vector search must not invoke the embedding provider"
    );

    let capture = TraceCapture::default();
    let writer = capture.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_span_events(tracing_subscriber::fmt::format::FmtSpan::NEW)
        .with_writer(move || writer.clone())
        .finish();
    let options = memoria_service::RetrieveOptions::default().with_filter(
        serde_json::from_value(json!({"metadata": [{"path": "labels.scene", "operator": "equal", "value": "private-filter-probe"}]})).unwrap());
    async {
        server
            .service()
            .retrieve_with_options_on_branch(&user, None, "private-query-probe", 1, &options)
            .await
            .unwrap();
    }
    .with_subscriber(subscriber)
    .await;
    let logs = String::from_utf8(capture.0.lock().unwrap().clone()).unwrap();
    assert!(
        logs.contains("retrieve_inner"),
        "privacy assertion needs an actual production span"
    );
    assert!(
        !logs.contains("private-query-probe"),
        "query text escaped to spans"
    );
    assert!(
        !logs.contains("private-filter-probe"),
        "metadata operands escaped to spans"
    );
}

#[tokio::test]
async fn unsupported_or_invalid_filters_are_not_silently_ignored() {
    let master = "synthetic-memory-filter-validation-master";
    let server = support::multi_db::spawn_api_server(
        "filter_validation",
        16,
        master.into(),
        Some(Arc::new(FixedEmbedding::default())),
        None,
        None,
        false,
    )
    .await;
    for filter in [
        json!({"metadata": [{"path": "labels[*]", "operator": "equal", "value": "x"}]}),
        json!({"metadata": [{"path": "labels.scene", "operator": "equal", "value": {"x": 1}}]}),
        json!({"min_initial_confidence": 1.1}),
        json!({"observed_at_gte": "2026-11-03T04:00:00"}),
        json!({"unknown_selector": true}),
    ] {
        let response = server
            .client
            .post(format!("{}/v1/memories/retrieve", server.base))
            .bearer_auth(master)
            .header("X-User-Id", "filter-validation-user")
            .json(&json!({"query": "quartz", "filter": filter}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 422, "invalid filter was not rejected");
    }
    for request in [
        json!({"query_vector": [], "top_k": 1}),
        json!({"query_vector": [1, 0], "top_k": 1}),
        json!({"query_vector": vec![0.0; 16], "top_k": 101}),
        json!({"query_vector": vec![0.0; 16], "top_k": 0}),
        json!({"query_vector": vec![0.0; 16], "top_k": 1, "unexpected": true}),
    ] {
        let response = server
            .client
            .post(format!("{}/v1/memories/vector-search", server.base))
            .bearer_auth(master)
            .header("X-User-Id", "filter-validation-user")
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            422,
            "invalid query vector was not rejected"
        );
        let error: Value = response.json().await.unwrap();
        assert_eq!(error["code"], "memory_query_invalid");
        assert_eq!(error["message"], "The memory query is invalid.");
    }
}

#[tokio::test]
async fn vector_search_honors_read_scope_and_authenticated_user_isolation() {
    let master = "synthetic-vector-scope-test-master";
    let server = support::multi_db::spawn_api_server(
        "vector_scope",
        16,
        master.into(),
        Some(Arc::new(FixedEmbedding::default())),
        None,
        None,
        true,
    )
    .await;
    let mut ids = Vec::new();
    for user in ["vector-owner-a", "vector-owner-b"] {
        let response = server.client.post(format!("{}/v1/memories", server.base))
            .bearer_auth(master).header("X-User-Id", user)
            .json(&json!({"content": "identical scoped vector fixture", "subject_id": "same-subject"}))
            .send().await.unwrap();
        assert_eq!(response.status(), 201);
        ids.push(
            response.json::<Value>().await.unwrap()["memory_id"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    let response = server.client.post(format!("{}/auth/keys", server.base)).bearer_auth(master)
        .json(&json!({"user_id": "vector-owner-a", "name": "read-only-vector", "scopes": ["identity:read", "memory:read"]}))
        .send().await.unwrap();
    assert_eq!(response.status(), 201);
    let key = response.json::<Value>().await.unwrap();
    let key = key["raw_key"].as_str().unwrap();
    let mut vector = vec![0.0; 16];
    vector[0] = 1.0;
    let request = json!({"query_vector": vector, "top_k": 100, "subject_id": "same-subject"});
    // An untrusted routing header must not replace the authenticated key owner.
    let response = server
        .client
        .post(format!("{}/v1/memories/vector-search", server.base))
        .bearer_auth(key)
        .header("X-User-Id", "vector-owner-b")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let found: Vec<Value> = response.json().await.unwrap();
    assert_eq!(
        found
            .iter()
            .map(|memory| memory["memory_id"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [ids[0].as_str()]
    );
    let response = server
        .client
        .post(format!("{}/v1/memories", server.base))
        .bearer_auth(key)
        .json(&json!({"content": "must not be stored"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    let response = server
        .client
        .get(format!("{}/v1/memories", server.base))
        .bearer_auth(key)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let page: Value = response.json().await.unwrap();
    assert_eq!(
        page["items"].as_array().unwrap().len(),
        1,
        "denied write changed storage"
    );
}
