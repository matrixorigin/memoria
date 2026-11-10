//! Real HTTP/MatrixOne correction receipts and transaction rollback.
mod support;
use serde_json::{json, Value};

#[tokio::test]
async fn correction_receipt_is_atomic_scoped_and_replayable() {
    let master = "synthetic-correction-master";
    let server = support::multi_db::spawn_api_server(
        "atomic_correct",
        4,
        master.into(),
        None,
        None,
        None,
        false,
    )
    .await;
    let owner = format!("correct_{}", uuid::Uuid::new_v4().simple());
    let request = |method, path: &str| {
        server
            .client
            .request(method, format!("{}{path}", server.base))
            .header("Authorization", format!("Memoria-Owner {master}"))
            .header("X-User-Id", &owner)
    };
    let stored: Value = request(reqwest::Method::POST, "/v1/memories/idempotent")
        .json(&json!({"content":"synthetic original preference", "memory_type":"semantic", "subject_id":"consumer-a", "branch":"main", "idempotency_key":"original"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let id = stored["memory_id"].as_str().unwrap();
    let path = format!("/v1/memories/{id}/correct-idempotent");
    let body = json!({"new_content":"synthetic corrected preference", "subject_id":"consumer-a", "branch":"main", "idempotency_key":"correction-1"});
    let response = request(reqwest::Method::PUT, &path)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
    let corrected: Value = request(reqwest::Method::PUT, &path)
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_ne!(corrected["memory_id"], stored["memory_id"]);
    assert_eq!(corrected["content"], body["new_content"]);
    assert!(!corrected
        .to_string()
        .contains(memoria_core::STORE_REQUEST_HASH_FIELD));
    let replay: Value = request(reqwest::Method::PUT, &path)
        .json(&body)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(replay["memory_id"], corrected["memory_id"]);
    let mut conflict = body.clone();
    conflict["new_content"] = json!("different payload");
    assert_eq!(
        request(reqwest::Method::PUT, &path)
            .json(&conflict)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    let mut wrong_subject = body.clone();
    wrong_subject["subject_id"] = json!("consumer-b");
    assert_eq!(
        request(reqwest::Method::PUT, &path)
            .json(&wrong_subject)
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    let sql = server.user_store(&owner).await;
    let table = sql.table_for_branch(&owner, Some("main")).await.unwrap();
    assert!(sql.get_from(&table, id).await.unwrap().is_none());
    assert_eq!(
        sql.list_active_from(&table, &owner, 100)
            .await
            .unwrap()
            .len(),
        1
    );
    request(
        reqwest::Method::DELETE,
        &format!(
            "/v1/memories/{}?branch=main",
            corrected["memory_id"].as_str().unwrap()
        ),
    )
    .send()
    .await
    .unwrap()
    .error_for_status()
    .unwrap();
    assert_eq!(
        request(reqwest::Method::PUT, &path)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        409
    );
    assert!(sql
        .list_active_from(&table, &owner, 100)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn failed_replacement_insert_rolls_back_original_retirement() {
    let master = "synthetic-rollback-master";
    let server = support::multi_db::spawn_api_server(
        "correct_rollback",
        4,
        master.into(),
        None,
        None,
        None,
        false,
    )
    .await;
    let owner = format!("rollback_{}", uuid::Uuid::new_v4().simple());
    let store = |text: &str| {
        server.client.post(format!("{}/v1/memories/idempotent", server.base))
        .header("Authorization", format!("Memoria-Owner {master}")).header("X-User-Id", &owner)
        .json(&json!({"content":text,"memory_type":"episodic","subject_id":"consumer-a","branch":"main","idempotency_key":text}))
    };
    let original: Value = store("original")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let collision: Value = store("collision")
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    let sql = server.user_store(&owner).await;
    let table = sql.table_for_branch(&owner, Some("main")).await.unwrap();
    let old = sql
        .get_from(&table, original["memory_id"].as_str().unwrap())
        .await
        .unwrap()
        .unwrap();
    let mut replacement = old.clone();
    replacement.memory_id = collision["memory_id"].as_str().unwrap().into();
    replacement.content = "must not commit".into();
    assert!(sql
        .replace_memory_atomically(&table, &old, &replacement)
        .await
        .is_err());
    let preserved = sql.get_from(&table, &old.memory_id).await.unwrap().unwrap();
    assert_eq!(preserved.content, old.content);
    assert!(preserved.is_active);
    assert!(preserved.superseded_by.is_none());
    assert_eq!(
        sql.list_active_from(&table, &owner, 100)
            .await
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn concurrent_corrections_publish_only_one_replacement() {
    let master = "synthetic-concurrent-correction";
    let server = support::multi_db::spawn_api_server(
        "correct_race",
        4,
        master.into(),
        None,
        None,
        None,
        false,
    )
    .await;
    let owner = format!("race_{}", uuid::Uuid::new_v4().simple());
    let stored: Value = server.client.post(format!("{}/v1/memories/idempotent", server.base))
        .header("Authorization", format!("Memoria-Owner {master}")).header("X-User-Id", &owner)
        .json(&json!({"content":"original race fact", "memory_type":"semantic", "subject_id":"consumer", "branch":"main", "idempotency_key":"original"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let id = stored["memory_id"].as_str().unwrap();
    let mut pending = tokio::task::JoinSet::new();
    for index in 0..6 {
        let request = server.client.put(format!("{}/v1/memories/{id}/correct-idempotent", server.base))
            .header("Authorization", format!("Memoria-Owner {master}")).header("X-User-Id", &owner)
            .json(&json!({"new_content":format!("replacement {index}"),"subject_id":"consumer","branch":"main","idempotency_key":format!("operation-{index}")}));
        pending.spawn(async move { request.send().await.unwrap() });
    }
    let mut successful = 0;
    while let Some(response) = pending.join_next().await {
        let response = response.unwrap();
        let status = response.status();
        if status == 200 {
            successful += 1;
        }
        assert!(
            matches!(status.as_u16(), 200 | 404 | 409),
            "unexpected correction status={status}: {}",
            response.text().await.unwrap()
        );
    }
    assert_eq!(
        successful, 1,
        "distinct concurrent requests must not create multiple replacement facts"
    );
    let sql = server.user_store(&owner).await;
    let table = sql.table_for_branch(&owner, Some("main")).await.unwrap();
    assert_eq!(
        sql.list_active_from(&table, &owner, 100)
            .await
            .unwrap()
            .len(),
        1
    );
    let total: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table} WHERE user_id = ?"))
        .bind(&owner)
        .fetch_one(sql.pool())
        .await
        .unwrap();
    assert_eq!(
        total.0, 2,
        "losing transactions must not leave inactive orphan replacements"
    );
}

#[tokio::test]
async fn concurrent_identical_corrections_share_one_receipt() {
    let master = "synthetic-identical-correction";
    let server = support::multi_db::spawn_api_server(
        "correct_replay_race",
        4,
        master.into(),
        None,
        None,
        None,
        false,
    )
    .await;
    let owner = format!("replay_{}", uuid::Uuid::new_v4().simple());
    let stored: Value = server.client.post(format!("{}/v1/memories/idempotent", server.base))
        .header("Authorization", format!("Memoria-Owner {master}")).header("X-User-Id", &owner)
        .json(&json!({"content":"original replay fact", "memory_type":"semantic", "subject_id":"consumer", "branch":"main", "idempotency_key":"original"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let id = stored["memory_id"].as_str().unwrap();
    let mut pending = tokio::task::JoinSet::new();
    for _ in 0..6 {
        let request = server.client.put(format!("{}/v1/memories/{id}/correct-idempotent", server.base))
            .header("Authorization", format!("Memoria-Owner {master}")).header("X-User-Id", &owner)
            .json(&json!({"new_content":"one corrected fact","subject_id":"consumer","branch":"main","idempotency_key":"same-operation"}));
        pending.spawn(async move { request.send().await.unwrap() });
    }
    let mut returned = std::collections::HashSet::new();
    while let Some(response) = pending.join_next().await {
        let response = response.unwrap();
        assert_eq!(response.status(), 200, "{}", response.text().await.unwrap());
        let memory: Value = response.json().await.unwrap();
        returned.insert(memory["memory_id"].as_str().unwrap().to_owned());
    }
    assert_eq!(returned.len(), 1);
    let sql = server.user_store(&owner).await;
    let table = sql.table_for_branch(&owner, Some("main")).await.unwrap();
    let total: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table} WHERE user_id = ?"))
        .bind(&owner)
        .fetch_one(sql.pool())
        .await
        .unwrap();
    assert_eq!(total.0, 2);
    assert_eq!(
        sql.list_active_from(&table, &owner, 100)
            .await
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn correction_requires_write_scope_and_cannot_route_to_another_owner() {
    let master = "synthetic-correction-scope";
    let server = support::multi_db::spawn_api_server(
        "correct_scope",
        4,
        master.into(),
        None,
        None,
        None,
        true,
    )
    .await;
    let owner = format!("scope_{}", uuid::Uuid::new_v4().simple());
    let stored: Value = server.client.post(format!("{}/v1/memories/idempotent", server.base))
        .bearer_auth(master).header("X-User-Id", &owner)
        .json(&json!({"content":"protected original fact", "memory_type":"semantic", "subject_id":"consumer", "branch":"main", "idempotency_key":"original"}))
        .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
    let id = stored["memory_id"].as_str().unwrap();
    let body = json!({"new_content":"must not be written","subject_id":"consumer","branch":"main","idempotency_key":"forbidden"});
    for (key_owner, scopes, expected) in [
        (owner.as_str(), vec!["identity:read", "memory:read"], 403),
        (
            "another-correction-owner",
            vec!["identity:read", "memory:read", "memory:write"],
            404,
        ),
    ] {
        let key: Value = server
            .client
            .post(format!("{}/auth/keys", server.base))
            .bearer_auth(master)
            .json(&json!({"user_id":key_owner,"name":"correction-boundary","scopes":scopes}))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap();
        let response = server
            .client
            .put(format!(
                "{}/v1/memories/{id}/correct-idempotent",
                server.base
            ))
            .bearer_auth(key["raw_key"].as_str().unwrap())
            .header("X-User-Id", &owner)
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            expected,
            "{}",
            response.text().await.unwrap()
        );
    }
    let sql = server.user_store(&owner).await;
    let table = sql.table_for_branch(&owner, Some("main")).await.unwrap();
    let unchanged = sql.get_from(&table, id).await.unwrap().unwrap();
    assert_eq!(unchanged.content, "protected original fact");
    assert_eq!(
        sql.list_active_from(&table, &owner, 100)
            .await
            .unwrap()
            .len(),
        1
    );
}
