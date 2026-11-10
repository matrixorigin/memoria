//! Production HTTP/authentication + real MatrixOne. The embedding fixture is
//! deterministic; these tests do not claim retrieval-quality or SLA acceptance.
mod support;
use hmac::{Hmac, Mac};
use memoria_storage::readonly_sql::MemorySqlGateway;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{ConnectOptions, Connection, Executor};
use std::{
    io::Write,
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
struct SqlLogs(Arc<Mutex<Vec<u8>>>);

impl Write for SqlLogs {
    fn write(&mut self, value: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(value);
        Ok(value.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct FixedSqlEmbedding;

#[async_trait::async_trait]
impl memoria_core::interfaces::EmbeddingProvider for FixedSqlEmbedding {
    async fn embed(&self, _: &str) -> Result<Vec<f32>, memoria_core::MemoriaError> {
        Ok(vec![1.0, 0.0, 0.0, 0.0])
    }
    fn dimension(&self) -> usize {
        4
    }
}

async fn key(
    server: &support::multi_db::ApiTestServer,
    master: &str,
    owner: &str,
    sql: bool,
) -> String {
    let scopes = if sql {
        vec!["identity:read", "memory:read", "memory:sql"]
    } else {
        vec!["identity:read", "memory:read", "keys:manage"]
    };
    let result: Value = server
        .client
        .post(format!("{}/auth/keys", server.base))
        .bearer_auth(master)
        .json(&json!({"user_id":owner,"name":"synthetic-sql-key","scopes":scopes}))
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    result["raw_key"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn disabled_sql_has_no_reader_objects_or_query_execution() {
    let master = "synthetic-disabled-sql";
    let server = support::multi_db::spawn_api_server(
        "sql_disabled",
        4,
        master.into(),
        Some(std::sync::Arc::new(FixedSqlEmbedding)),
        None,
        None,
        true,
    )
    .await;
    let key = key(&server, master, "disabled-sql-owner", true).await;
    let response = server
        .client
        .post(format!("{}/v1/memories/sql-query", server.base))
        .bearer_auth(key)
        .json(
            &json!({"sql":"SELECT content FROM memories LIMIT 1","parameters":[],"branch":"main"}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 503);
    let failure: Value = response.json().await.unwrap();
    assert_eq!(failure["code"], "memory_sql_disabled");
}

#[tokio::test]
async fn sql_reads_only_authenticated_scope_and_rejects_unsafe_queries() {
    let logs = SqlLogs::default();
    let writer = logs.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter("memoria_storage::readonly_sql=info,sqlx=debug")
        .with_ansi(false)
        .with_writer(move || writer.clone())
        .finish();
    let _subscriber = tracing::subscriber::set_default(subscriber);
    let master = "synthetic-scoped-sql";
    let gateway = MemorySqlGateway::new(vec![19; 32]).unwrap();
    let server = support::multi_db::spawn_api_server_with_sql(
        "sql_scoped",
        4,
        master.into(),
        Some(std::sync::Arc::new(FixedSqlEmbedding)),
        None,
        None,
        true,
        Some(gateway),
    )
    .await;
    let owners = [
        format!("sql_a_{}", uuid::Uuid::new_v4().simple()),
        format!("sql_b_{}", uuid::Uuid::new_v4().simple()),
    ];
    let mut ids = Vec::new();
    for owner in &owners {
        let result: Value = server.client.post(format!("{}/v1/memories/idempotent", server.base))
            .bearer_auth(master).header("X-User-Id", owner)
            .json(&json!({"content":"synthetic SQL fact","subject_id":"consumer","memory_type":"semantic","branch":"main","idempotency_key":"original","extra_metadata":{"scope":{"agent_id":"synthetic-agent"}}}))
            .send().await.unwrap().error_for_status().unwrap().json().await.unwrap();
        ids.push(result["memory_id"].as_str().unwrap().to_owned());
    }
    let sql_key = key(&server, master, &owners[0], true).await;
    let readonly = key(&server, master, &owners[0], false).await;
    let body = json!({"sql":"SELECT memory_id, content, JSON_UNQUOTE(JSON_EXTRACT(extra_metadata, '$.scope.agent_id')) AS agent_id FROM memories WHERE subject_id = ? LIMIT 10","parameters":["consumer"],"branch":"main"});
    let request = |body: &Value| {
        server
            .client
            .post(format!("{}/v1/memories/sql-query", server.base))
            .bearer_auth(&sql_key)
            .header("X-User-Id", &owners[1])
            .json(body)
    };
    let response = request(&body).send().await.unwrap();
    let status = response.status();
    let result: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{result}");
    assert_eq!(result["rows"].as_array().unwrap().len(), 1);
    assert_eq!(result["rows"][0][0], ids[0]);
    assert_ne!(result["rows"][0][0], ids[1]);
    assert_eq!(result["rows"][0][2], "synthetic-agent");
    let private_query = json!({"sql":"SELECT content FROM memories WHERE content = 'synthetic-private-sql-literal' AND subject_id = ? LIMIT 1", "parameters":["synthetic-private-sql-parameter"],"branch":"main"});
    request(&private_query)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap();
    assert!(result["fingerprint"]
        .as_str()
        .is_some_and(|s| s.len() == 64));
    assert!(!result
        .to_string()
        .contains(memoria_core::STORE_REQUEST_HASH_FIELD));
    assert_eq!(
        server
            .client
            .post(format!("{}/v1/memories/sql-query", server.base))
            .bearer_auth(readonly)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    // Key-management permission is not permission to mint a SQL capability.
    let ordinary_manager = key(&server, master, &owners[0], false).await;
    assert_eq!(
        server
            .client
            .post(format!("{}/auth/keys", server.base))
            .bearer_auth(ordinary_manager)
            .json(
                &json!({"user_id":owners[0], "name":"rejected-escalation", "scopes":["identity:read","memory:read","memory:sql"]})
            )
            .send()
            .await
            .unwrap()
            .status(),
        403
    );
    assert_eq!(
        server
            .client
            .post(format!("{}/v1/memories/sql-query", server.base))
            .bearer_auth(master)
            .json(&body)
            .send()
            .await
            .unwrap()
            .status(),
        403
    );

    let mut injected = body.clone();
    injected["parameters"] = json!(["consumer' OR 1=1 --"]);
    let empty: Value = request(&injected)
        .send()
        .await
        .unwrap()
        .error_for_status()
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(empty["rows"].as_array().unwrap().is_empty());
    assert_eq!(
        empty["columns"], result["columns"],
        "Empty results must preserve their schema"
    );
    let vector = json!({"sql":"SELECT memory_id, l2_distance(embedding, CAST(? AS vecf32(4))) AS distance FROM memories ORDER BY distance LIMIT 1", "parameters":["[0,0,0,0]"],"branch":"main"});
    let response = request(&vector).send().await.unwrap();
    let status = response.status();
    let vector_result: Value = response.json().await.unwrap();
    assert_eq!(status, 200, "{vector_result}");
    assert_eq!(vector_result["rows"][0][0], ids[0]);
    assert!(vector_result["rows"][0][1].is_number());
    for sql in [
        "DELETE FROM memories",
        "SELECT content FROM memories; SELECT content FROM memories",
        "SELECT SLEEP(20) FROM memories LIMIT 1",
        "SELECT content FROM mo_catalog.mem_api_keys LIMIT 1",
        "SELECT content FROM mem_memories LIMIT 1",
        "SELECT (SELECT content FROM memories) FROM memories LIMIT 1",
        "SELECT content FROM memories LIMIT 101",
    ] {
        let invalid = json!({"sql":sql,"parameters":[],"branch":"main"});
        let response = request(&invalid).send().await.unwrap();
        assert!(matches!(response.status().as_u16(), 400 | 422));
        let error: Value = response.json().await.unwrap();
        assert!(!error.to_string().contains(sql));
    }
    let sql = server.user_store(&owners[0]).await;
    let table = sql
        .table_for_branch(&owners[0], Some("main"))
        .await
        .unwrap();
    assert_eq!(
        sql.list_active_from(&table, &owners[0], 10)
            .await
            .unwrap()
            .len(),
        1
    );
    // Independently verify the database identity, not only the AST allowlist.
    // The credentials are derived from a synthetic fixture seed, never a live key.
    let options = sql.pool().connect_options();
    let database = sql.database_name().or(options.get_database()).unwrap();
    let digest = format!(
        "{:x}",
        Sha256::digest(format!("{database}\0{}", owners[0]).as_bytes())
    );
    let user = format!("memq_{}", &digest[..32]);
    let role = format!("memqr_{}", &digest[..32]);
    let view = format!("`{database}`.`memoria_sql_{}`", &digest[..32]);
    let mut mac = Hmac::<Sha256>::new_from_slice(&[19; 32]).unwrap();
    mac.update(b"memoria.sql.reader.v1\0");
    mac.update(digest.as_bytes());
    let password = format!("{:x}", mac.finalize().into_bytes());
    let username = match options.get_username().split_once(':') {
        Some((account, _)) => format!("{account}:{user}:{role}"),
        None => user.clone(),
    };
    let readonly_options = options
        .as_ref()
        .clone()
        .username(&username)
        .password(&password)
        .disable_statement_logging();
    let mut reader = sqlx::MySqlConnection::connect_with(&readonly_options)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>(&format!("SELECT memory_id FROM {view} LIMIT 1"))
            .fetch_one(&mut reader)
            .await
            .unwrap(),
        ids[0]
    );
    for forbidden in [
        format!("SELECT content FROM {} LIMIT 1", sql.t("mem_memories")),
        format!("DELETE FROM {view}"),
        format!("UPDATE {view} SET content = 'forbidden'"),
        format!(
            "SELECT content FROM `{}`.mem_memories LIMIT 1",
            server.user_db_name(&owners[1]).await
        ),
    ] {
        assert!(
            reader.execute(forbidden.as_str()).await.is_err(),
            "Restricted database identity unexpectedly admitted an operation"
        );
    }
    reader.close().await.unwrap();
    // Exact fixture-owned identities are removed, including global RBAC facts.
    let mut admin =
        sqlx::MySqlConnection::connect_with(&options.as_ref().clone().disable_statement_logging())
            .await
            .unwrap();
    admin
        .execute(format!("DROP USER `{user}`").as_str())
        .await
        .unwrap();
    admin
        .execute(format!("DROP ROLE `{role}`").as_str())
        .await
        .unwrap();
    admin.close().await.unwrap();
    let captured = String::from_utf8(logs.0.lock().unwrap().clone()).unwrap();
    assert!(
        captured.contains("memory SQL query completed"),
        "SQL audit log was not actually captured"
    );
    for private in [
        "synthetic-private-sql-literal",
        "synthetic-private-sql-parameter",
        sql_key.as_str(),
        password.as_str(),
    ] {
        assert!(
            !captured.contains(private),
            "SQL logs exposed private query or authentication material"
        );
    }
}
