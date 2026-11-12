//! Snapshot pagination is validated before SQL and handles out-of-range pages.
use serde_json::{json, Value};
use std::time::Duration;

mod support;

#[tokio::test]
async fn snapshot_pagination_boundaries() {
    let server = support::multi_db::spawn_api_server(
        "snapshot_page",
        4,
        String::new(),
        None,
        None,
        None,
        false,
    )
    .await;
    let base = &server.base;
    let client = &server.client;
    let user = format!("page_{}", uuid::Uuid::new_v4().simple());
    let name = format!("page_{}", uuid::Uuid::new_v4().simple());
    let outcome: anyhow::Result<()> = async {
        for content in ["snapshot page A", "snapshot page B"] {
            client
                .post(format!("{base}/v1/memories"))
                .header("X-User-Id", &user)
                .json(&json!({"content": content, "memory_type": "working"}))
                .send()
                .await?
                .error_for_status()?;
        }
        client
            .post(format!("{base}/v1/snapshots"))
            .header("X-User-Id", &user)
            .json(&json!({"name": name}))
            .send()
            .await?
            .error_for_status()?;
        let mut failures = Vec::new();
        for (suffix, expected) in [
            ("?limit=-1", 422),
            ("?offset=-1", 422),
            ("?limit=-9223372036854775808", 422),
            ("?offset=-9223372036854775808", 422),
            ("/diff?limit=-1", 422),
            ("/diff?limit=-9223372036854775808", 422),
            ("?limit=1&offset=9223372036854775807", 200),
        ] {
            match client
                .get(format!("{base}/v1/snapshots/{name}{suffix}"))
                .header("X-User-Id", &user)
                .timeout(Duration::from_secs(15))
                .send()
                .await
            {
                Ok(response) => {
                    let status = response.status().as_u16();
                    let body = response.text().await?;
                    if status != expected {
                        failures.push(format!(
                            "{suffix}: expected {expected}, got {status}: {body}"
                        ));
                    } else if expected == 200 {
                        let body: Value = serde_json::from_str(&body)?;
                        anyhow::ensure!(
                            body["memories"] == json!([]) && body["has_more"] == false,
                            "unexpected out-of-range page: {body}"
                        );
                    }
                }
                Err(error) => failures.push(format!("{suffix}: {error}")),
            }
        }
        anyhow::ensure!(failures.is_empty(), "{}", failures.join("\n"));
        for (query, expected_limit, expected_len, has_more) in [
            ("", 50, 2, false),
            ("?limit=0", 0, 0, true),
            ("?limit=1", 1, 1, true),
            ("?limit=1&offset=1", 1, 1, false),
            ("?limit=1&offset=2", 1, 0, false),
            ("?limit=9223372036854775807", 500, 2, false),
        ] {
            let body: Value = client
                .get(format!("{base}/v1/snapshots/{name}{query}"))
                .header("X-User-Id", &user)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::ensure!(
                body["limit"] == expected_limit && body["memory_count"] == 2,
                "{query}: {body}"
            );
            anyhow::ensure!(
                body["memories"].as_array().unwrap().len() == expected_len,
                "{query}: {body}"
            );
            anyhow::ensure!(body["has_more"] == has_more, "{query}: {body}");
        }
        for query in ["", "?limit=0", "?limit=9223372036854775807"] {
            let body: Value = client
                .get(format!("{base}/v1/snapshots/{name}/diff{query}"))
                .header("X-User-Id", &user)
                .send()
                .await?
                .error_for_status()?
                .json()
                .await?;
            anyhow::ensure!(body["added"] == json!([]) && body["removed"] == json!([]));
        }
        Ok(())
    }
    .await;
    // Release only this test's snapshot, including after a regression failure.
    let cleanup = client
        .delete(format!("{base}/v1/snapshots/{name}"))
        .header("X-User-Id", &user)
        .send()
        .await
        .unwrap();
    assert_eq!(
        cleanup.status(),
        204,
        "snapshot cleanup failed; test: {outcome:?}"
    );
    outcome.unwrap();
}

#[tokio::test]
async fn invalid_snapshot_pagination_precedes_snapshot_lookup() {
    let server = support::multi_db::spawn_api_server(
        "snapshot_page_input",
        4,
        String::new(),
        None,
        None,
        None,
        false,
    )
    .await;
    let user = format!("page_{}", uuid::Uuid::new_v4().simple());
    for suffix in ["?limit=-1", "?offset=-1", "/diff?limit=-1"] {
        let response = server
            .client
            .get(format!("{}/v1/snapshots/missing{suffix}", server.base))
            .header("X-User-Id", &user)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 422, "{suffix}");
    }
}
