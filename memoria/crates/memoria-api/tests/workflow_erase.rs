//! Real HTTP, API-key authorization and isolated MatrixOne databases.
mod support;
use reqwest::Method;
use serde_json::{json, Value};
use support::multi_db::ApiTestServer;

async fn call(server: &ApiTestServer, key: &str, method: Method, path: &str, body: Value) -> Value {
    let response = server
        .client
        .request(method, format!("{}{path}", server.base))
        .bearer_auth(key)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "HTTP {}: {}",
        response.status(),
        response.text().await.unwrap()
    );
    response.json().await.unwrap()
}

async fn key(server: &ApiTestServer, master: &str, owner: &str, write: bool) -> String {
    let scopes = if write {
        vec!["identity:read", "memory:read", "memory:write"]
    } else {
        vec!["identity:read", "memory:read"]
    };
    call(
        server,
        master,
        Method::POST,
        "/auth/keys",
        json!({"user_id":owner,"name":"synthetic-workflow-erase","scopes":scopes}),
    )
    .await["raw_key"]
        .as_str()
        .unwrap()
        .to_owned()
}

#[tokio::test]
async fn suspended_write_fences_and_revision_cas_use_real_group_http_contracts() {
    let master = "synthetic-revision-fence-master";
    let server = support::multi_db::spawn_api_server(
        "revision_fence",
        4,
        master.into(),
        None,
        None,
        None,
        true,
    )
    .await;
    let owner = format!("fence_{}", uuid::Uuid::new_v4().simple());
    let personal = key(&server, master, &owner, true).await;
    store(&server, &personal, "register-owner", None, "control").await;
    let response = server
        .client
        .post(format!("{}/v1/groups", server.base))
        .bearer_auth(master)
        .header("X-User-Id", &owner)
        .json(&json!({"group_name":"synthetic-fence-group", "members":[owner]}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 201);
    let group: Value = response.json().await.unwrap();
    let group_id = group["group_id"].as_str().unwrap();
    let mut scoped_keys = vec![];
    for scopes in [
        json!(["identity:read", "memory:read", "memory:write"]),
        json!(["identity:read", "memory:read"]),
    ] {
        let created = call(
            &server,
            master,
            Method::POST,
            "/auth/keys",
            json!({"user_id":owner, "group_id":group_id, "name":"synthetic", "scopes":scopes}),
        )
        .await;
        scoped_keys.push(created["raw_key"].as_str().unwrap().to_owned());
    }
    let (writer, reader) = (&scoped_keys[0], &scoped_keys[1]);
    let scope =
        json!({"subject_id":"subject", "connector_id":"connector", "workflow_id":"workflow"});
    let fence = call(
        &server,
        writer,
        Method::POST,
        "/v1/memories/write-fence",
        scope.clone(),
    )
    .await;
    let denied = server
        .client
        .post(format!("{}/v1/memories/write-fence", server.base))
        .bearer_auth(reader)
        .json(&scope)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    let memory = json!({"idempotency_key":"suspended-original", "content":"synthetic old body", "memory_type":"semantic", "subject_id":"subject", "session_id":"session-a", "branch":"main",
        "initial_confidence":0.42, "extra_metadata":{"connector_id":"connector", "workflow_id":"workflow", "source_job_id":"run-a", "source_evidence":{"content_sha256":"old-body", "items":[{"quote":"old source"}]}}});
    let original = call(
        &server,
        writer,
        Method::POST,
        "/v1/memories/fenced",
        json!({"memory":memory, "fence":fence}),
    )
    .await;
    let id = original["memory_id"].as_str().unwrap();
    let old = call(
        &server,
        writer,
        Method::GET,
        &format!("/v1/memories/{id}?branch=main"),
        json!(null),
    )
    .await;
    assert_eq!(old["revision"].as_str().unwrap().len(), 64);
    let patch = json!({"subject_id":"subject", "session_id":"session-a", "expected_updated_at":old["updated_at"], "expected_revision":old["revision"], "extra_metadata":{"result_freshness":"superseded"}});
    let changed = call(
        &server,
        writer,
        Method::PUT,
        &format!("/v1/memories/{id}/metadata"),
        patch.clone(),
    )
    .await;
    assert_ne!(changed["revision"], old["revision"]);
    assert_eq!(changed["content"], old["content"]);
    assert_eq!(changed["initial_confidence"], old["initial_confidence"]);
    let stale = server
        .client
        .put(format!("{}/v1/memories/{id}/metadata", server.base))
        .bearer_auth(writer)
        .json(&patch)
        .send()
        .await
        .unwrap();
    assert_eq!(stale.status(), 409);
    let forbidden = server
        .client
        .put(format!("{}/v1/memories/{id}/metadata", server.base))
        .bearer_auth(reader)
        .json(&patch)
        .send()
        .await
        .unwrap();
    assert_eq!(forbidden.status(), 403);
    let correction = json!({"new_content":"synthetic new body", "subject_id":"subject", "session_id":"session-b", "branch":"main", "expected_updated_at":changed["updated_at"], "expected_revision":changed["revision"], "initial_confidence":0.82,
        "extra_metadata_patch":{"source_job_id":"run-b", "source_evidence":{"content_sha256":"new-body", "items":[{"quote":"new source"}]}, "source_feedback":null}});
    let corrected = call(
        &server,
        writer,
        Method::PUT,
        &format!("/v1/memories/{id}/correct-attributed"),
        correction,
    )
    .await;
    let corrected_id = corrected["memory_id"].as_str().unwrap();
    let persisted = call(
        &server,
        writer,
        Method::GET,
        &format!("/v1/memories/{corrected_id}?branch=main"),
        json!(null),
    )
    .await;
    assert!((persisted["initial_confidence"].as_f64().unwrap() - 0.82).abs() < 1e-6);
    assert_eq!(
        persisted["extra_metadata"]["source_evidence"]["content_sha256"],
        "new-body"
    );
    assert_eq!(persisted["session_id"], "session-b");
    let deleted = call(
        &server,
        writer,
        Method::POST,
        "/v1/memories/erase",
        json!({"mode":"workflow", "workflow_id":"workflow"}),
    )
    .await;
    assert_eq!(deleted["memories"], 2);
    let refused = server
        .client
        .post(format!("{}/v1/memories/fenced", server.base))
        .bearer_auth(writer)
        .json(&json!({"memory":memory, "fence":fence}))
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), 409);
    assert_eq!(
        refused.json::<Value>().await.unwrap()["code"],
        "memory_write_fence_stale"
    );
    let absent = call(
        &server,
        writer,
        Method::GET,
        &format!("/v1/memories/{id}?branch=main"),
        json!(null),
    )
    .await;
    assert!(absent.is_null());
    let fresh = call(
        &server,
        writer,
        Method::POST,
        "/v1/memories/write-fence",
        scope,
    )
    .await;
    let mut new_memory = memory.clone();
    new_memory["idempotency_key"] = json!("new-explicit-request");
    call(
        &server,
        writer,
        Method::POST,
        "/v1/memories/fenced",
        json!({"memory":new_memory,"fence":fresh}),
    )
    .await;
    let retained = call(
        &server,
        &personal,
        Method::POST,
        "/v1/memories/query",
        json!({"subject_id":"control", "branch":"main", "limit":10}),
    )
    .await;
    assert_eq!(retained["items"].as_array().unwrap().len(), 1);
}

async fn store(
    server: &ApiTestServer,
    key: &str,
    tag: &str,
    workflow: Option<&str>,
    subject: &str,
) -> Value {
    let mut metadata = json!({"connector_id":"same-source", "source_job_id":tag});
    if let Some(workflow) = workflow {
        metadata["workflow_id"] = json!(workflow);
    }
    call(server, key, Method::POST, "/v1/memories/idempotent", json!({
        "content":format!("Synthetic fact {tag}"),"memory_type":"semantic","subject_id":subject,
        "session_id":"same-session","branch":"main","idempotency_key":tag,"extra_metadata":metadata
    })).await
}

#[tokio::test]
async fn workflow_erase_preserves_other_workflows_scopes_and_corrected_provenance() {
    let master = "synthetic-workflow-erase-master";
    let server = support::multi_db::spawn_api_server(
        "workflow_erase",
        4,
        master.into(),
        None,
        None,
        None,
        true,
    )
    .await;
    let owner = format!("erase_{}", uuid::Uuid::new_v4().simple());
    let other_owner = format!("other_{}", uuid::Uuid::new_v4().simple());
    let writer = key(&server, master, &owner, true).await;
    let reader = key(&server, master, &owner, false).await;
    let other_key = key(&server, master, &other_owner, true).await;
    let a = store(&server, &writer, "a", Some("workflow-a"), "subject-a").await;
    let inactive = store(
        &server,
        &writer,
        "inactive",
        Some("workflow-a"),
        "subject-b",
    )
    .await;
    let b = store(&server, &writer, "b", Some("workflow-b"), "subject-a").await;
    let manual = store(&server, &writer, "manual", None, "subject-a").await;
    let foreign = store(
        &server,
        &other_key,
        "foreign",
        Some("workflow-a"),
        "subject-a",
    )
    .await;
    let a_id = a["memory_id"].as_str().unwrap();
    let a_revision = call(
        &server,
        &writer,
        Method::GET,
        &format!("/v1/memories/{a_id}?branch=main"),
        json!(null),
    )
    .await;
    let corrected = call(
        &server,
        &writer,
        Method::PUT,
        &format!("/v1/memories/{a_id}/correct-attributed"),
        json!({
            "new_content":"Synthetic updated fact from workflow B", "session_id":"session-b",
            "subject_id":"subject-a", "branch":"main", "expected_updated_at":a_revision["updated_at"], "expected_revision":a_revision["revision"],
            "extra_metadata_patch":{"workflow_id":"workflow-b","source_job_id":"run-b"}
        }),
    )
    .await;
    assert_eq!(corrected["extra_metadata"]["workflow_id"], "workflow-b");
    assert_eq!(corrected["extra_metadata"]["connector_id"], "same-source");
    assert_eq!(corrected["session_id"], "session-b");
    let inactive_id = inactive["memory_id"].as_str().unwrap();
    call(
        &server,
        &writer,
        Method::POST,
        &format!("/v1/memories/{inactive_id}/disable"),
        json!({"subject_id":"subject-b","branch":"main","reason":"synthetic"}),
    )
    .await;

    // A rejected source patch must leave the existing revision active.
    let b_id = b["memory_id"].as_str().unwrap();
    let b_revision = call(
        &server,
        &writer,
        Method::GET,
        &format!("/v1/memories/{b_id}?branch=main"),
        json!(null),
    )
    .await;
    let denied_patch = server.client.put(format!("{}/v1/memories/{b_id}/correct-attributed",server.base)).bearer_auth(&writer)
        .json(&json!({"new_content":"must not be written","session_id":"same-session","subject_id":"subject-a", "expected_updated_at":b_revision["updated_at"], "expected_revision":b_revision["revision"], "extra_metadata_patch":{"_memoria_store_request_sha256":"forged"}})).send().await.unwrap();
    assert_eq!(denied_patch.status(), 422);
    assert_eq!(
        call(
            &server,
            &writer,
            Method::GET,
            &format!("/v1/memories/{b_id}"),
            json!(null)
        )
        .await["content"],
        b["content"]
    );

    let erase = json!({"mode":"workflow","workflow_id":"workflow-a"});
    let denied = server
        .client
        .post(format!("{}/v1/memories/erase", server.base))
        .bearer_auth(&reader)
        .json(&erase)
        .send()
        .await
        .unwrap();
    assert_eq!(denied.status(), 403);
    for invalid in [
        json!({"mode":"workflow"}),
        json!({"mode":"group","workflow_id":"workflow-a"}),
        json!({"mode":"workflow","workflow_id":"workflow-a","subject_id":"subject-a"}),
    ] {
        assert_eq!(
            server
                .client
                .post(format!("{}/v1/memories/erase", server.base))
                .bearer_auth(&writer)
                .json(&invalid)
                .send()
                .await
                .unwrap()
                .status(),
            422
        );
    }
    let empty = call(
        &server,
        &writer,
        Method::POST,
        "/v1/memories/erase",
        json!({"mode":"workflow","workflow_id":"workflow"}),
    )
    .await;
    assert_eq!(empty["memories"], 0);
    let result = call(
        &server,
        &writer,
        Method::POST,
        "/v1/memories/erase",
        erase.clone(),
    )
    .await;
    assert_eq!(result["memories"], 2); // superseded A revision + explicitly disabled A
    assert_eq!(result["snapshots_dropped"], 0);
    let sql = server.user_store(&owner).await;
    let table = sql.table_for_branch(&owner, Some("main")).await.unwrap();
    assert!(sql
        .get_owned_from(&table, a_id, &owner)
        .await
        .unwrap()
        .is_none());
    assert!(sql
        .get_owned_from(&table, inactive_id, &owner)
        .await
        .unwrap()
        .is_none());
    for kept in [&b, &manual, &corrected] {
        let id = kept["memory_id"].as_str().unwrap();
        assert!(sql
            .get_owned_from(&table, id, &owner)
            .await
            .unwrap()
            .is_some());
    }
    let foreign_id = foreign["memory_id"].as_str().unwrap();
    assert_eq!(
        call(
            &server,
            &other_key,
            Method::GET,
            &format!("/v1/memories/{foreign_id}"),
            json!(null)
        )
        .await["memory_id"],
        foreign_id
    );
    assert_eq!(
        call(&server, &writer, Method::POST, "/v1/memories/erase", erase).await["memories"],
        0
    );
}
