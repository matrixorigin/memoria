//! Exercise both extraction paths through the authenticated HTTP endpoint.
use serde_json::{json, Value};

mod support;

#[tokio::test]
async fn complete_observation_preserves_the_correction_or_rejects_before_storage() {
    let key = uuid::Uuid::new_v4().to_string();
    let tail = "Final correction: use Python.";
    let dialogue = format!("{}{}", "界".repeat(500), tail);
    let (model, _shutdown) = memoria_test_utils::spawn_fake_llm(vec![(
        "Final correction: use Python.",
        json!([{"type":"profile","content":"The user prefers Python."}]),
    )])
    .await;
    for (llm, expected) in [
        (Some(model), "The user prefers Python."),
        (None, dialogue.as_str()),
    ] {
        let server = support::multi_db::spawn_api_server(
            "observe_complete",
            4,
            key.clone(),
            None,
            llm,
            None,
            false,
        )
        .await;
        let owner = uuid::Uuid::new_v4().simple().to_string();
        let post = |text: &str| {
            server
                .client
                .post(format!("{}/v1/observe/complete", server.base))
                .header("Authorization", format!("Memoria-Owner {key}"))
                .header("X-User-Id", &owner)
                .json(&json!({"messages":[{"role":"user","content":text}]}))
                .send()
        };
        let malformed = json!({"messages":[{"role":"tool","content":"private tool result"}]});
        let invalid = server
            .client
            .post(format!("{}/v1/observe/complete", server.base))
            .header("Authorization", format!("Memoria-Owner {key}"))
            .header("X-User-Id", &owner)
            .json(&malformed)
            .send()
            .await
            .unwrap();
        assert_eq!(invalid.status().as_u16(), 422);
        let oversize = "界".repeat(2000);
        assert_eq!(post(&oversize).await.unwrap().status().as_u16(), 422);
        assert!(server
            .service()
            .list_active(&owner, 10)
            .await
            .unwrap()
            .is_empty());
        let response = post(&dialogue).await.unwrap();
        assert!(response.status().is_success(), "{}", response.status());
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["memories"][0]["content"], expected);
        let stored = server.service().list_active(&owner, 10).await.unwrap();
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].content, expected);
    }
}
