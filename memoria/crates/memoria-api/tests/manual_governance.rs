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
}
