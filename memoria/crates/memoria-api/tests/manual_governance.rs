//! Real MatrixOne coverage for operator-controlled memory lifecycle.
mod support;

use reqwest::{Method, RequestBuilder};
use serde_json::{json, Value};
use support::multi_db::ApiTestServer;

const MASTER: &str = "synthetic-governance-test-master";

fn request(server: &ApiTestServer, owner: &str, method: Method, path: &str) -> RequestBuilder {
    server
        .client
        .request(method, format!("{}{path}", server.base))
        .header("Authorization", format!("Memoria-Owner {MASTER}"))
        .header("X-User-Id", owner)
}

async fn server() -> ApiTestServer {
    support::multi_db::spawn_api_server(
        "manual_governance",
        4,
        MASTER.into(),
        None,
        None,
        None,
        false,
    )
    .await
}

async fn store(server: &ApiTestServer, owner: &str, key: &str) -> String {
    let response = request(server, owner, Method::POST, "/v1/memories/idempotent")
        .json(&json!({"content":format!("Synthetic project fact {key}"),
            "memory_type":"semantic", "subject_id":"subject-a", "branch":"main",
            "idempotency_key":key}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    response.json::<Value>().await.unwrap()["memory_id"]
        .as_str()
        .unwrap()
        .into()
}

#[tokio::test]
async fn disabled_memory_survives_cleanup_and_preserves_subject_fence() {
    let server = server().await;
    let owner = format!("governance_{}", uuid::Uuid::new_v4().simple());
    let id = store(&server, &owner, "cleanup").await;
    let path = format!("/v1/memories/{id}/disable");
    assert_eq!(
        request(&server, &owner, Method::POST, &path)
            .json(&json!({"branch":"main","subject_id":"other"}))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let response = request(&server, &owner, Method::POST, &path)
        .json(&json!({"branch":"main","subject_id":"subject-a","reason":"operator rejection"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let disabled: Value = response.json().await.unwrap();
    assert_eq!(disabled["is_active"], false);
    assert!(disabled["extra_metadata"]
        .get("_memoria_disabled")
        .is_none());

    let sql = server.user_store(&owner).await;
    let table = sql.t("mem_memories");
    // Age only this synthetic test row beyond the stale-cleanup grace period.
    sqlx::query(&format!(
        "UPDATE {table} SET updated_at = DATE_SUB(NOW(), INTERVAL 48 HOUR) WHERE memory_id = ?"
    ))
    .bind(&id)
    .execute(sql.pool())
    .await
    .unwrap();
    assert_eq!(sql.cleanup_stale(&owner).await.unwrap(), 0);
    let response = request(&server, &owner, Method::GET, "/v1/memories/disabled")
        .query(&[("subject_id", "subject-a"), ("branch", "main")])
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let page: Value = response.json().await.unwrap();
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    assert_eq!(page["items"][0]["disabled_reason"], "operator rejection");
    assert!(page["items"][0]["extra_metadata"]
        .get("_memoria_disabled")
        .is_none());
    let restored = request(
        &server,
        &owner,
        Method::POST,
        &format!("/v1/memories/{id}/restore"),
    )
    .json(&json!({"branch":"main","subject_id":"subject-a"}))
    .send()
    .await
    .unwrap();
    assert_eq!(restored.status(), 200);
    assert_eq!(restored.json::<Value>().await.unwrap()["is_active"], true);
    assert_eq!(
        request(
            &server,
            &owner,
            Method::POST,
            &format!("/v1/memories/{id}/restore")
        )
        .json(&json!({"branch":"main"}))
        .send()
        .await
        .unwrap()
        .status(),
        409
    );
}

#[tokio::test]
async fn disabled_pagination_is_stable_and_rejects_malformed_cursors() {
    let server = server().await;
    let owner = format!("governance_page_{}", uuid::Uuid::new_v4().simple());
    for key in ["a", "b", "c"] {
        let id = store(&server, &owner, key).await;
        assert_eq!(
            request(
                &server,
                &owner,
                Method::POST,
                &format!("/v1/memories/{id}/disable")
            )
            .json(&json!({"branch":"main"}))
            .send()
            .await
            .unwrap()
            .status(),
            200
        );
    }
    // Equal timestamps exercise the memory_id tie breaker.
    let sql = server.user_store(&owner).await;
    sqlx::query(&format!(
        "UPDATE {} SET updated_at = '2026-01-01 00:00:00.000000' WHERE user_id = ?",
        sql.t("mem_memories")
    ))
    .bind(&owner)
    .execute(sql.pool())
    .await
    .unwrap();
    let first: Value = request(&server, &owner, Method::GET, "/v1/memories/disabled")
        .query(&[("limit", "2"), ("branch", "main")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(first["items"].as_array().unwrap().len(), 2);
    let cursor = first["next_cursor"].as_str().unwrap();
    let second: Value = request(&server, &owner, Method::GET, "/v1/memories/disabled")
        .query(&[("limit", "2"), ("branch", "main"), ("cursor", cursor)])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_eq!(second["next_cursor"], Value::Null);
    assert_ne!(
        first["items"][0]["memory_id"],
        second["items"][0]["memory_id"]
    );
    assert_ne!(
        first["items"][1]["memory_id"],
        second["items"][0]["memory_id"]
    );
    for (name, value) in [("cursor", "not-json"), ("limit", "501"), ("limit", "0")] {
        assert_eq!(
            request(&server, &owner, Method::GET, "/v1/memories/disabled")
                .query(&[(name, value)])
                .send()
                .await
                .unwrap()
                .status(),
            422
        );
    }
}

#[tokio::test]
async fn erase_compare_and_swap_preserves_a_concurrently_superseded_revision() {
    let server = server().await;
    let owner = format!("governance_race_{}", uuid::Uuid::new_v4().simple());
    let id = store(&server, &owner, "race").await;
    let sql = server.user_store(&owner).await;
    let table = sql.table_for_branch(&owner, Some("main")).await.unwrap();
    // Deterministically interleave the erase read with a completed correction.
    let expected = sql
        .get_owned_from(&table, &id, &owner)
        .await
        .unwrap()
        .unwrap();
    server
        .service()
        .correct_on_branch(&owner, Some("main"), &id, "Synthetic corrected fact")
        .await
        .unwrap();
    assert_eq!(sql.erase_from(&table, &expected).await.unwrap(), 0);
    let old = sql
        .get_owned_from(&table, &id, &owner)
        .await
        .unwrap()
        .unwrap();
    assert!(old.superseded_by.is_some());
    assert_eq!(
        request(
            &server,
            &owner,
            Method::POST,
            &format!("/v1/memories/{id}/erase")
        )
        .json(&json!({"branch":"main"}))
        .send()
        .await
        .unwrap()
        .status(),
        409
    );
}

#[tokio::test]
async fn cloned_branch_governance_preserves_main_graph_state() {
    let server = server().await;
    let owner = format!("governance_branch_{}", uuid::Uuid::new_v4().simple());
    let id = store(&server, &owner, "branch").await;
    let sql = server.user_store(&owner).await;
    let graph = sql.graph_store();
    let node_id = uuid::Uuid::new_v4().simple().to_string();
    graph
        .create_node(&memoria_storage::GraphNode {
            node_id: node_id.clone(),
            user_id: owner.clone(),
            node_type: memoria_storage::NodeType::Semantic,
            content: "Synthetic main graph fact".into(),
            entity_type: None,
            embedding: None,
            memory_id: Some(id.clone()),
            session_id: None,
            confidence: 0.8,
            trust_tier: "T3".into(),
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
        .unwrap();
    assert_eq!(
        request(&server, &owner, Method::POST, "/v1/branches")
            .json(&json!({"name":"review"}))
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    for action in ["disable", "restore", "disable", "erase"] {
        let response = request(
            &server,
            &owner,
            Method::POST,
            &format!("/v1/memories/{id}/{action}"),
        )
        .json(&json!({"branch":"review","subject_id":"subject-a"}))
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), if action == "erase" { 204 } else { 200 });
        assert!(graph.get_node(&node_id).await.unwrap().unwrap().is_active);
        let main = sql.table_for_branch(&owner, Some("main")).await.unwrap();
        assert!(
            sql.get_owned_from(&main, &id, &owner)
                .await
                .unwrap()
                .unwrap()
                .is_active
        );
    }
    sqlx::query(&format!(
        "UPDATE {} SET updated_at = '2026-01-01 00:00:00' WHERE memory_id = ?",
        sql.t("mem_memories")
    ))
    .bind(&id)
    .execute(sql.pool())
    .await
    .unwrap();
    let disabled = server
        .service()
        .disable_on_branch(&owner, Some("main"), Some("subject-a"), &id, "graph test")
        .await
        .unwrap();
    assert!(!graph.get_node(&node_id).await.unwrap().unwrap().is_active);
    let stored = sql
        .get_owned_from(&sql.t("mem_memories"), &id, &owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(disabled.updated_at, stored.updated_at);
    assert!(
        disabled.updated_at.unwrap()
            > chrono::DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap()
    );
    let restored = server
        .service()
        .restore_on_branch(&owner, Some("main"), Some("subject-a"), &id)
        .await
        .unwrap();
    assert!(graph.get_node(&node_id).await.unwrap().unwrap().is_active);
    let stored = sql
        .get_owned_from(&sql.t("mem_memories"), &id, &owner)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(restored.updated_at, stored.updated_at);
    let (nodes,): (i64,) = sqlx::query_as(&format!(
        "SELECT COUNT(*) FROM {} WHERE memory_id = ?",
        sql.t("memory_graph_nodes")
    ))
    .bind(&id)
    .fetch_one(sql.pool())
    .await
    .unwrap();
    assert_eq!(nodes, 1);
    server
        .service()
        .disable_on_branch(
            &owner,
            Some("main"),
            Some("subject-a"),
            &id,
            "disable again",
        )
        .await
        .unwrap();
    // A delayed graph step from an earlier restore cannot activate a memory
    // that has already been disabled again.
    graph.reactivate_by_memory_id(&id).await.unwrap();
    assert!(!graph.get_node(&node_id).await.unwrap().unwrap().is_active);
    server
        .service()
        .restore_on_branch(&owner, Some("main"), Some("subject-a"), &id)
        .await
        .unwrap();
    assert!(graph.get_node(&node_id).await.unwrap().unwrap().is_active);
}

#[tokio::test]
async fn selective_apply_never_merges_or_reverses_operator_lifecycle_state() {
    let server = server().await;
    let owner = format!("governance_apply_{}", uuid::Uuid::new_v4().simple());
    let branch_disabled = store(&server, &owner, "branch-disabled").await;
    let main_disabled = store(&server, &owner, "main-disabled").await;
    let main_erased = store(&server, &owner, "main-erased").await;
    assert_eq!(
        request(&server, &owner, Method::POST, "/v1/branches")
            .json(&json!({"name":"review"}))
            .send()
            .await
            .unwrap()
            .status(),
        201
    );
    for (id, branch, action) in [
        (&branch_disabled, "review", "disable"),
        (&main_disabled, "main", "disable"),
        (&main_erased, "main", "erase"),
    ] {
        assert_eq!(
            request(
                &server,
                &owner,
                Method::POST,
                &format!("/v1/memories/{id}/{action}")
            )
            .json(&json!({"branch":branch}))
            .send()
            .await
            .unwrap()
            .status(),
            if action == "erase" { 204 } else { 200 }
        );
    }
    let response = request(&server, &owner, Method::POST, "/v1/branches/review/apply")
        .json(&json!({"removes":[branch_disabled], "adds":[main_disabled,main_erased]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let result: Value = response.json().await.unwrap();
    assert_eq!(result["skipped_removes"], json!([branch_disabled]));
    assert_eq!(result["skipped_adds"], json!([main_disabled, main_erased]));
    let sql = server.user_store(&owner).await;
    let main = sql.t("mem_memories");
    assert!(
        sql.get_owned_from(&main, &branch_disabled, &owner)
            .await
            .unwrap()
            .unwrap()
            .is_active
    );
    assert_eq!(
        sql.list_disabled_from(&main, &owner, None, 10, None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(sql
        .get_owned_from(&main, &main_erased, &owner)
        .await
        .unwrap()
        .unwrap()
        .content
        .is_empty());
}

#[tokio::test]
async fn blank_subject_and_ignored_reason_are_rejected_without_mutation() {
    let server = server().await;
    let owner = format!("governance_contract_{}", uuid::Uuid::new_v4().simple());
    let id = store(&server, &owner, "contract").await;
    for subject in ["", "   "] {
        for action in ["disable", "restore", "erase"] {
            let response = request(
                &server,
                &owner,
                Method::POST,
                &format!("/v1/memories/{id}/{action}"),
            )
            .json(&json!({"subject_id":subject, "branch":"main"}))
            .send()
            .await
            .unwrap();
            assert_eq!(response.status(), 422, "{action} accepted a blank subject");
        }
        let response = request(&server, &owner, Method::GET, "/v1/memories/disabled")
            .query(&[("subject_id", subject)])
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 422);
    }
    for action in ["restore", "erase"] {
        let response = request(
            &server,
            &owner,
            Method::POST,
            &format!("/v1/memories/{id}/{action}"),
        )
        .json(&json!({"reason":"must not be silently discarded"}))
        .send()
        .await
        .unwrap();
        assert_eq!(response.status(), 422);
    }
    // A JSON object, even when empty, is part of the documented request contract.
    assert_eq!(
        request(
            &server,
            &owner,
            Method::POST,
            &format!("/v1/memories/{id}/disable")
        )
        .send()
        .await
        .unwrap()
        .status(),
        415
    );
    assert!(
        server
            .service()
            .get_for_user_on_branch(&owner, Some("main"), &id)
            .await
            .unwrap()
            .unwrap()
            .is_active
    );
}

#[tokio::test]
async fn erased_idempotent_content_cannot_be_resurrected_by_replay_or_cleanup() {
    let server = server().await;
    let owner = format!("governance_receipt_{}", uuid::Uuid::new_v4().simple());
    let key = "durable-erasure";
    let id = store(&server, &owner, key).await;
    let payload = json!({"content":format!("Synthetic project fact {key}"), "memory_type":"semantic",
        "subject_id":"subject-a", "branch":"main", "idempotency_key":key});
    assert_eq!(
        request(
            &server,
            &owner,
            Method::POST,
            &format!("/v1/memories/{id}/disable")
        )
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    assert_eq!(
        request(&server, &owner, Method::POST, "/v1/memories/idempotent")
            .json(&payload)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert_eq!(
        request(
            &server,
            &owner,
            Method::POST,
            &format!("/v1/memories/{id}/erase")
        )
        .json(&json!({"subject_id":"subject-a"}))
        .send()
        .await
        .unwrap()
        .status(),
        204
    );
    let sql = server.user_store(&owner).await;
    let table = sql.t("mem_memories");
    let tombstone = sql
        .get_owned_from(&table, &id, &owner)
        .await
        .unwrap()
        .unwrap();
    assert!(!tombstone.is_active);
    assert_eq!(tombstone.content, "");
    assert!(tombstone.embedding.is_none());
    assert!(
        tombstone.subject_id.is_none()
            && tombstone.author_id.is_none()
            && tombstone.session_id.is_none()
    );
    let metadata = tombstone.extra_metadata.unwrap();
    assert_eq!(metadata.len(), 2);
    assert!(metadata.contains_key(memoria_core::ERASED_FIELD));
    sqlx::query(&format!("UPDATE {table} SET updated_at = DATE_SUB(NOW(), INTERVAL 48 HOUR), memory_type = 'tool_result', observed_at = DATE_SUB(NOW(), INTERVAL 96 HOUR) WHERE memory_id = ?"))
        .bind(&id).execute(sql.pool()).await.unwrap();
    assert_eq!(sql.cleanup_stale(&owner).await.unwrap(), 0);
    sql.cleanup_tool_results(72).await.unwrap();
    assert_eq!(
        sql.health_hygiene(&owner).await.unwrap()["inactive_memories"],
        0
    );
    for action in ["restore", "erase"] {
        assert_eq!(
            request(
                &server,
                &owner,
                Method::POST,
                &format!("/v1/memories/{id}/{action}")
            )
            .json(&json!({}))
            .send()
            .await
            .unwrap()
            .status(),
            404
        );
    }
    assert_eq!(
        request(&server, &owner, Method::POST, "/v1/memories/idempotent")
            .json(&payload)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
}

#[tokio::test]
async fn disabled_tool_result_survives_ttl_and_is_not_reported_cleanable() {
    let server = server().await;
    let owner = format!("governance_ttl_{}", uuid::Uuid::new_v4().simple());
    let id = store(&server, &owner, "tool").await;
    let sql = server.user_store(&owner).await;
    let table = sql.t("mem_memories");
    sqlx::query(&format!("UPDATE {table} SET memory_type = 'tool_result', observed_at = DATE_SUB(NOW(), INTERVAL 96 HOUR) WHERE memory_id = ?"))
        .bind(&id).execute(sql.pool()).await.unwrap();
    assert_eq!(
        request(
            &server,
            &owner,
            Method::POST,
            &format!("/v1/memories/{id}/disable")
        )
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    sqlx::query(&format!(
        "UPDATE {table} SET updated_at = DATE_SUB(NOW(), INTERVAL 48 HOUR) WHERE memory_id = ?"
    ))
    .bind(&id)
    .execute(sql.pool())
    .await
    .unwrap();
    assert_eq!(
        sql.health_hygiene(&owner).await.unwrap()["inactive_memories"],
        0
    );
    assert_eq!(
        sql.health_hygiene_global().await.unwrap()["inactive_memories"],
        0
    );
    sql.cleanup_tool_results(72).await.unwrap();
    assert_eq!(
        request(
            &server,
            &owner,
            Method::POST,
            &format!("/v1/memories/{id}/restore")
        )
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    // Restore does not reset observed_at: normal active-memory TTL applies again.
    assert_eq!(sql.cleanup_tool_results(72).await.unwrap(), 1);
}

struct CorrectionGate {
    entered: tokio::sync::Notify,
    released: tokio::sync::Notify,
}

#[tokio::test]
async fn correction_insert_failure_rolls_back_retirement() {
    let server = server().await;
    let owner = format!("governance_rollback_{}", uuid::Uuid::new_v4().simple());
    let id = store(&server, &owner, "original").await;
    let collision = store(&server, &owner, "collision").await;
    let sql = server.user_store(&owner).await;
    let table = sql.t("mem_memories");
    let original = sql
        .get_owned_from(&table, &id, &owner)
        .await
        .unwrap()
        .unwrap();
    let mut replacement = original.clone();
    replacement.memory_id = collision;
    assert!(sql
        .replace_active_revision(&table, &original, &replacement)
        .await
        .is_err());
    let preserved = sql
        .get_owned_from(&table, &id, &owner)
        .await
        .unwrap()
        .unwrap();
    assert!(preserved.is_active);
    assert!(preserved.superseded_by.is_none());
}

#[async_trait::async_trait]
impl memoria_core::interfaces::EmbeddingProvider for CorrectionGate {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, memoria_core::MemoriaError> {
        if text == "Synthetic correction blocked in embedding" {
            self.entered.notify_one();
            self.released.notified().await;
        }
        Ok(vec![1.0, 0.0, 0.0, 0.0])
    }
    fn dimension(&self) -> usize {
        4
    }
}

#[tokio::test]
async fn concurrent_disable_prevents_correction_from_publishing_a_replacement() {
    let gate = std::sync::Arc::new(CorrectionGate {
        entered: Default::default(),
        released: Default::default(),
    });
    let server = support::multi_db::spawn_api_server(
        "governance_correction",
        4,
        MASTER.into(),
        Some(gate.clone()),
        None,
        None,
        false,
    )
    .await;
    let owner = format!("governance_correct_{}", uuid::Uuid::new_v4().simple());
    let id = store(&server, &owner, "correct").await;
    let correction = request(
        &server,
        &owner,
        Method::PUT,
        &format!("/v1/memories/{id}/correct"),
    )
    .json(&json!({"branch":"main", "new_content":"Synthetic correction blocked in embedding"}));
    let task = tokio::spawn(async move { correction.send().await.unwrap() });
    tokio::time::timeout(std::time::Duration::from_secs(10), gate.entered.notified())
        .await
        .unwrap();
    assert_eq!(
        request(
            &server,
            &owner,
            Method::POST,
            &format!("/v1/memories/{id}/disable")
        )
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
    gate.released.notify_one();
    assert_eq!(task.await.unwrap().status(), 409);
    let sql = server.user_store(&owner).await;
    let table = sql.t("mem_memories");
    let (count,): (i64,) =
        sqlx::query_as(&format!("SELECT COUNT(*) FROM {table} WHERE user_id = ?"))
            .bind(&owner)
            .fetch_one(sql.pool())
            .await
            .unwrap();
    assert_eq!(
        count, 1,
        "a losing correction published an orphan replacement"
    );
    assert_eq!(
        sql.list_disabled_from(&table, &owner, Some("subject-a"), 10, None)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(
        request(
            &server,
            &owner,
            Method::POST,
            &format!("/v1/memories/{id}/restore")
        )
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .status(),
        200
    );
}
