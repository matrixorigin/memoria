use serde_json::{json, Value};
use serial_test::serial;
use sqlx::Row;
use std::sync::Arc;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

fn admin_db_url() -> String {
    std::env::var("GROUP_TEST_ADMIN_DATABASE_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .unwrap_or_else(|_| "mysql://root:111@127.0.0.1:6666/memoria".to_string())
}

fn unique_shared_db_name() -> String {
    format!(
        "memoria_group_test_{}",
        &uuid::Uuid::new_v4().simple().to_string()[..12]
    )
}

fn replace_db_name(url: &str, db_name: &str) -> String {
    let (prefix, _) = url.rsplit_once('/').expect("db url with database name");
    format!("{prefix}/{db_name}")
}

async fn drop_database(pool: &sqlx::MySqlPool, db_name: &str) {
    let _ = sqlx::raw_sql(&format!(
        "DROP DATABASE IF EXISTS `{}`",
        db_name.replace('`', "``")
    ))
    .execute(pool)
    .await;
}

struct TestServer {
    base: String,
    client: reqwest::Client,
    shared_pool: sqlx::MySqlPool,
    shared_db_name: String,
}

impl TestServer {
    async fn cleanup(&self) {
        let rows = sqlx::query("SELECT db_name FROM mem_groups")
            .fetch_all(&self.shared_pool)
            .await
            .unwrap_or_default();
        for row in &rows {
            if let Ok(db_name) = row.try_get::<String, _>("db_name") {
                drop_database(&self.shared_pool, &db_name).await;
            }
        }
        drop_database(&self.shared_pool, &self.shared_db_name).await;
        self.shared_pool.close().await;
    }
}

async fn spawn_server() -> TestServer {
    use memoria_git::GitForDataService;
    use memoria_service::MemoryService;
    use memoria_storage::{DbRouter, SqlMemoryStore};

    std::env::set_var("DB_MAX_CONNECTIONS", "4");
    let admin_db = admin_db_url();
    memoria_test_utils::wait_for_mysql_ready(&admin_db, std::time::Duration::from_secs(30)).await;
    let admin_pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&admin_db)
        .await
        .expect("admin pool");

    let shared_db_name = unique_shared_db_name();
    sqlx::raw_sql(&format!(
        "CREATE DATABASE IF NOT EXISTS `{}`",
        shared_db_name.replace('`', "``")
    ))
    .execute(&admin_pool)
    .await
    .expect("create shared db");
    admin_pool.close().await;

    let shared_db_url = replace_db_name(&admin_db, &shared_db_name);
    let mut shared_store =
        SqlMemoryStore::connect(&shared_db_url, test_dim(), uuid::Uuid::new_v4().to_string())
            .await
            .expect("connect shared store");
    shared_store.migrate().await.expect("migrate shared store");

    let router = Arc::new(
        DbRouter::connect(&shared_db_url, test_dim(), uuid::Uuid::new_v4().to_string())
            .await
            .expect("connect router"),
    );
    let shared_pool = sqlx::mysql::MySqlPoolOptions::new()
        .max_connections(2)
        .connect(&shared_db_url)
        .await
        .expect("shared pool");
    let git = Arc::new(GitForDataService::new(shared_pool.clone(), &shared_db_name));
    shared_store.set_db_router(router.clone());
    let service = Arc::new(
        MemoryService::new_sql_with_llm_and_router(
            Arc::new(shared_store),
            Some(router),
            None,
            None,
        )
        .await,
    );
    let state = memoria_api::AppState::new(service, git, "master-group-test-key".to_string())
        .init_auth_pool(&shared_db_url, true)
        .await
        .expect("auth pool");
    let app = memoria_api::build_router(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();
    let base = format!("http://127.0.0.1:{port}");
    for _ in 0..20 {
        if client.get(format!("{base}/health")).send().await.is_ok() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    TestServer {
        base,
        client,
        shared_pool,
        shared_db_name,
    }
}

async fn create_group_with_payload(
    server: &TestServer,
    auth_header: &str,
    owner: Option<&str>,
    payload: Value,
) -> Value {
    let mut req = server
        .client
        .post(format!("{}/v1/groups", server.base))
        .header("Authorization", auth_header);
    if let Some(owner) = owner {
        req = req.header("X-User-Id", owner);
    }
    let resp = req.json(&payload).send().await.expect("create group");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 201, "create group failed: {body}");
    serde_json::from_str(&body).unwrap()
}

async fn create_group_with_members(server: &TestServer, owner: &str, members: &[&str]) -> Value {
    create_group_with_payload(
        server,
        "Bearer master-group-test-key",
        Some(owner),
        json!({
            "group_name": "team-a",
            "members": members
        }),
    )
    .await
}

async fn create_group(server: &TestServer, owner: &str) -> Value {
    create_group_with_members(server, owner, &[owner]).await
}

async fn register_user(server: &TestServer, user_id: &str) {
    let now = chrono::Utc::now().naive_utc();
    sqlx::query(
        "INSERT INTO mem_user_registry (user_id, db_name, status, created_at, updated_at) \
         VALUES (?, ?, 'active', ?, ?) \
         ON DUPLICATE KEY UPDATE status = 'active', updated_at = VALUES(updated_at)",
    )
    .bind(user_id)
    .bind(&server.shared_db_name)
    .bind(now)
    .bind(now)
    .execute(&server.shared_pool)
    .await
    .expect("register user in mem_user_registry");
}

async fn create_group_key(server: &TestServer, user_id: &str, group_id: &str) -> String {
    let resp = server
        .client
        .post(format!("{}/auth/keys", server.base))
        .header("Authorization", "Bearer master-group-test-key")
        .header("X-User-Id", "admin")
        .json(&json!({
            "user_id": user_id,
            "group_id": group_id,
            "name": "group-key"
        }))
        .send()
        .await
        .expect("create group key");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 201, "create group key failed: {body}");
    let body: Value = serde_json::from_str(&body).unwrap();
    body["raw_key"].as_str().unwrap().to_string()
}

async fn create_branch(server: &TestServer, auth: &str, name: &str) {
    let resp = server
        .client
        .post(format!("{}/v1/branches", server.base))
        .header("Authorization", auth)
        .json(&json!({ "name": name }))
        .send()
        .await
        .expect("create branch");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 201, "create branch failed: {body}");
}

async fn checkout_branch(server: &TestServer, auth: &str, name: &str) {
    let resp = server
        .client
        .post(format!("{}/v1/branches/{name}/checkout", server.base))
        .header("Authorization", auth)
        .send()
        .await
        .expect("checkout branch");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "checkout branch failed: {body}");
}

async fn store_memory(server: &TestServer, auth: &str, content: &str) -> Value {
    let resp = server
        .client
        .post(format!("{}/v1/memories", server.base))
        .header("Authorization", auth)
        .json(&json!({ "content": content, "memory_type": "semantic" }))
        .send()
        .await
        .expect("store memory");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 201, "store memory failed: {body}");
    serde_json::from_str(&body).unwrap()
}

async fn correct_memory(
    server: &TestServer,
    auth: &str,
    memory_id: &str,
    new_content: &str,
) -> Value {
    let resp = server
        .client
        .put(format!("{}/v1/memories/{memory_id}/correct", server.base))
        .header("Authorization", auth)
        .json(&json!({ "new_content": new_content }))
        .send()
        .await
        .expect("correct memory");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "correct memory failed: {body}");
    serde_json::from_str(&body).unwrap()
}

async fn delete_memory(server: &TestServer, auth: &str, memory_id: &str) {
    let resp = server
        .client
        .delete(format!("{}/v1/memories/{memory_id}", server.base))
        .header("Authorization", auth)
        .send()
        .await
        .expect("delete memory");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 204, "delete memory failed: {body}");
}

async fn diff_items(server: &TestServer, auth: &str, branch: &str) -> Value {
    let resp = server
        .client
        .get(format!("{}/v1/branches/{branch}/diff-items", server.base))
        .header("Authorization", auth)
        .send()
        .await
        .expect("diff items");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "diff-items failed: {body}");
    serde_json::from_str(&body).unwrap()
}

async fn apply_branch(server: &TestServer, auth: &str, branch: &str, payload: Value) -> Value {
    let resp = server
        .client
        .post(format!("{}/v1/branches/{branch}/apply", server.base))
        .header("Authorization", auth)
        .json(&payload)
        .send()
        .await
        .expect("apply branch");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "apply branch failed: {body}");
    serde_json::from_str(&body).unwrap()
}

async fn list_memories(server: &TestServer, auth: &str) -> Value {
    let resp = server
        .client
        .get(format!("{}/v1/memories", server.base))
        .header("Authorization", auth)
        .send()
        .await
        .expect("list memories");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "list memories failed: {body}");
    serde_json::from_str(&body).unwrap()
}

#[tokio::test]
#[serial]
async fn test_group_key_main_is_read_only() {
    let server = spawn_server().await;
    register_user(&server, "bob").await;
    let group = create_group_with_members(&server, "alice", &["alice", "bob"]).await;
    let group_id = group["group_id"].as_str().unwrap();
    let raw_key = create_group_key(&server, "alice", group_id).await;

    let resp = server
        .client
        .post(format!("{}/v1/memories", server.base))
        .header("Authorization", format!("Bearer {raw_key}"))
        .json(&json!({"content": "should fail on protected main"}))
        .send()
        .await
        .expect("group store");
    assert_eq!(resp.status(), 403);

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_solo_owner_group_key_can_write_on_main() {
    let server = spawn_server().await;
    let group = create_group(&server, "alice").await;
    let group_id = group["group_id"].as_str().unwrap();
    let raw_key = create_group_key(&server, "alice", group_id).await;
    let auth = format!("Bearer {raw_key}");

    let created = store_memory(&server, &auth, "solo owner main write").await;
    assert_eq!(created["content"], "solo owner main write");

    let body = list_memories(&server, &auth).await;
    assert!(body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["content"] == "solo owner main write"));

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_group_governance_honors_explicit_target_in_both_checkout_states() {
    let server = spawn_server().await;
    register_user(&server, "bob").await;
    let group = create_group_with_members(&server, "alice", &["alice", "bob"]).await;
    let group_id = group["group_id"].as_str().unwrap();
    let key = create_group_key(&server, "alice", group_id).await;
    let auth = format!("Bearer {key}");
    create_branch(&server, &auth, "seed").await;
    checkout_branch(&server, &auth, "seed").await;
    let memory = store_memory(&server, &auth, "Synthetic shared governance fact").await;
    let id = memory["memory_id"].as_str().unwrap();
    apply_branch(&server, &auth, "seed", json!({"adds":[id]})).await;
    checkout_branch(&server, &auth, "main").await;
    create_branch(&server, &auth, "review").await;
    // On main, an explicit branch is writable without changing checkout.
    for action in ["disable", "restore"] {
        let response = server
            .client
            .post(format!("{}/v1/memories/{id}/{action}", server.base))
            .header("Authorization", &auth)
            .json(&json!({"branch":"review"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            200,
            "explicit review {action} was rejected"
        );
    }
    checkout_branch(&server, &auth, "review").await;
    // A non-main checkout must not authorize an explicit shared-main mutation.
    for action in ["disable", "restore", "erase"] {
        let response = server
            .client
            .post(format!("{}/v1/memories/{id}/{action}", server.base))
            .header("Authorization", &auth)
            .json(&json!({"branch":"main"}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            403,
            "non-main checkout bypassed {action} protection"
        );
    }
    checkout_branch(&server, &auth, "main").await;
    let response = server
        .client
        .post(format!("{}/v1/memories/{id}/erase", server.base))
        .header("Authorization", &auth)
        .json(&json!({"branch":"review"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 204);
    let main = list_memories(&server, &auth).await;
    assert!(main["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["memory_id"] == id));
    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_owner_scoped_master_cannot_enter_group_database() {
    let server = spawn_server().await;
    let group = create_group(&server, "alice").await;
    let group_id = group["group_id"].as_str().unwrap();
    let raw_key = create_group_key(&server, "alice", group_id).await;
    let group_auth = format!("Bearer {raw_key}");
    let created = store_memory(&server, &group_auth, "group private memory").await;
    let memory_id = created["memory_id"].as_str().unwrap();
    let owner_auth = "Memoria-Owner master-group-test-key";

    let response = server
        .client
        .get(format!("{}/v1/memories", server.base))
        .header("Authorization", owner_auth)
        .header("X-User-Id", group_id)
        .send()
        .await
        .expect("owner-scoped group list attempt");
    assert_eq!(response.status(), 400);

    let response = server
        .client
        .get(format!("{}/v1/memories/{memory_id}", server.base))
        .header("Authorization", owner_auth)
        .header("X-User-Id", group_id)
        .send()
        .await
        .expect("owner-scoped group read attempt");
    assert_eq!(response.status(), 400);

    let response = server
        .client
        .post(format!("{}/v1/memories", server.base))
        .header("Authorization", owner_auth)
        .header("X-User-Id", group_id)
        .json(&json!({"content": "unauthorized group write", "memory_type": "semantic"}))
        .send()
        .await
        .expect("owner-scoped group write attempt");
    assert_eq!(response.status(), 400);

    let response = server
        .client
        .put(format!("{}/v1/memories/{memory_id}/correct", server.base))
        .header("Authorization", owner_auth)
        .header("X-User-Id", group_id)
        .json(&json!({"new_content": "unauthorized overwrite"}))
        .send()
        .await
        .expect("owner-scoped group correction attempt");
    assert_eq!(response.status(), 400);

    let response = server
        .client
        .delete(format!("{}/v1/memories/{memory_id}", server.base))
        .header("Authorization", owner_auth)
        .header("X-User-Id", group_id)
        .send()
        .await
        .expect("owner-scoped group delete attempt");
    assert_eq!(response.status(), 400);

    let memories = list_memories(&server, &group_auth).await;
    let items = memories["items"].as_array().unwrap();
    assert!(items.iter().any(|item| {
        item["memory_id"] == memory_id && item["content"] == "group private memory"
    }));
    assert!(!items
        .iter()
        .any(|item| item["content"] == "unauthorized group write"));

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_create_group_can_seed_from_personal_db() {
    let server = spawn_server().await;

    let resp = server
        .client
        .post(format!("{}/v1/memories", server.base))
        .header("Authorization", "Bearer master-group-test-key")
        .header("X-User-Id", "alice")
        .json(&json!({"content": "personal seed memory", "memory_type": "semantic"}))
        .send()
        .await
        .expect("store personal memory");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 201, "store personal memory failed: {body}");

    let personal_db_name: String =
        sqlx::query_scalar("SELECT db_name FROM mem_user_registry WHERE user_id = ?")
            .bind("alice")
            .fetch_one(&server.shared_pool)
            .await
            .expect("alice personal db");

    let group = create_group_with_payload(
        &server,
        "Bearer master-group-test-key",
        Some("alice"),
        json!({
            "group_name": "seeded-team",
            "members": ["alice"],
            "seed": {
                "db_name": personal_db_name,
                "mode": "active_only"
            }
        }),
    )
    .await;
    let group_id = group["group_id"].as_str().unwrap();
    let raw_key = create_group_key(&server, "alice", group_id).await;
    let auth = format!("Bearer {raw_key}");

    let body = list_memories(&server, &auth).await;
    let item = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["content"] == "personal seed memory")
        .expect("seeded personal memory visible");
    assert_eq!(item["author_id"], "alice");

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_create_group_can_seed_from_current_group_db() {
    let server = spawn_server().await;
    let source_group = create_group(&server, "alice").await;
    let source_group_id = source_group["group_id"].as_str().unwrap();
    let source_group_db = source_group["db_name"].as_str().unwrap().to_string();
    let source_key = create_group_key(&server, "alice", source_group_id).await;
    let source_auth = format!("Bearer {source_key}");

    create_branch(&server, &source_auth, "seed").await;
    checkout_branch(&server, &source_auth, "seed").await;
    let created = store_memory(&server, &source_auth, "group seed memory").await;
    let memory_id = created["memory_id"].as_str().unwrap().to_string();
    let _ = apply_branch(&server, &source_auth, "seed", json!({"adds": [memory_id]})).await;
    checkout_branch(&server, &source_auth, "main").await;

    let seeded_group = create_group_with_payload(
        &server,
        &source_auth,
        None,
        json!({
            "group_name": "seeded-from-group",
            "members": ["alice"],
            "seed": {
                "db_name": source_group_db,
                "mode": "active_only"
            }
        }),
    )
    .await;
    let seeded_group_id = seeded_group["group_id"].as_str().unwrap();
    let seeded_key = create_group_key(&server, "alice", seeded_group_id).await;
    let seeded_auth = format!("Bearer {seeded_key}");

    let body = list_memories(&server, &seeded_auth).await;
    assert!(body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["content"] == "group seed memory"));

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_group_branch_diff_and_apply_flow() {
    let server = spawn_server().await;
    let group = create_group(&server, "alice").await;
    let group_id = group["group_id"].as_str().unwrap();
    let raw_key = create_group_key(&server, "alice", group_id).await;
    let auth = format!("Bearer {raw_key}");

    let resp = server
        .client
        .post(format!("{}/v1/branches", server.base))
        .header("Authorization", &auth)
        .json(&json!({"name": "exp1"}))
        .send()
        .await
        .expect("create branch");
    assert_eq!(resp.status(), 201);

    let resp = server
        .client
        .post(format!("{}/v1/branches/exp1/checkout", server.base))
        .header("Authorization", &auth)
        .send()
        .await
        .expect("checkout branch");
    assert_eq!(resp.status(), 200);

    let resp = server
        .client
        .post(format!("{}/v1/memories", server.base))
        .header("Authorization", &auth)
        .json(&json!({"content": "group branch memory", "memory_type": "semantic"}))
        .send()
        .await
        .expect("store on branch");
    assert_eq!(resp.status(), 201);
    let created: Value = resp.json().await.unwrap();
    let memory_id = created["memory_id"].as_str().unwrap().to_string();

    let group_db_name = group["db_name"].as_str().unwrap();
    let group_db_url = replace_db_name(&admin_db_url(), group_db_name);
    let group_pool = sqlx::MySqlPool::connect(&group_db_url)
        .await
        .expect("group pool");
    for _ in 0..60 {
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM mem_edit_log WHERE user_id = ? AND operation = 'inject'",
        )
        .bind(group_id)
        .fetch_one(&group_pool)
        .await
        .unwrap();
        if count > 0 {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
    let edit_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_edit_log WHERE user_id = ? AND operation = 'inject'",
    )
    .bind(group_id)
    .fetch_one(&group_pool)
    .await
    .unwrap();
    assert!(edit_count > 0, "group db should contain store audit log");

    let resp = server
        .client
        .get(format!("{}/v1/branches/exp1/diff-items", server.base))
        .header("Authorization", &auth)
        .send()
        .await
        .expect("diff items");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "diff-items failed: {body}");
    let diff: Value = serde_json::from_str(&body).unwrap();
    assert!(diff["added"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["memory_id"] == memory_id));

    let resp = server
        .client
        .post(format!("{}/v1/branches/exp1/apply", server.base))
        .header("Authorization", &auth)
        .json(&json!({"adds": [memory_id]}))
        .send()
        .await
        .expect("apply branch");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "apply failed: {body}");
    let applied: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(applied["applied_adds"].as_array().unwrap().len(), 1);

    let resp = server
        .client
        .post(format!("{}/v1/branches/main/checkout", server.base))
        .header("Authorization", &auth)
        .send()
        .await
        .expect("checkout main");
    assert_eq!(resp.status(), 200);

    let resp = server
        .client
        .get(format!("{}/v1/memories", server.base))
        .header("Authorization", &auth)
        .send()
        .await
        .expect("list memories");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    assert!(body["items"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["content"] == "group branch memory"));

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_group_conflict_accept_branch_apply_flow() {
    let server = spawn_server().await;
    register_user(&server, "bob").await;
    let group = create_group_with_members(&server, "alice", &["alice", "bob"]).await;
    let group_id = group["group_id"].as_str().unwrap();
    let alice_key = create_group_key(&server, "alice", group_id).await;
    let bob_key = create_group_key(&server, "bob", group_id).await;
    let alice_auth = format!("Bearer {alice_key}");
    let bob_auth = format!("Bearer {bob_key}");

    // Bootstrap one shared memory onto main.
    let resp = server
        .client
        .post(format!("{}/v1/branches", server.base))
        .header("Authorization", &alice_auth)
        .json(&json!({"name": "seed"}))
        .send()
        .await
        .expect("create seed branch");
    assert_eq!(resp.status(), 201);
    let resp = server
        .client
        .post(format!("{}/v1/branches/seed/checkout", server.base))
        .header("Authorization", &alice_auth)
        .send()
        .await
        .expect("checkout seed");
    assert_eq!(resp.status(), 200);
    let resp = server
        .client
        .post(format!("{}/v1/memories", server.base))
        .header("Authorization", &alice_auth)
        .json(&json!({"content": "shared base memory", "memory_type": "semantic"}))
        .send()
        .await
        .expect("store base memory");
    assert_eq!(resp.status(), 201);
    let created: Value = resp.json().await.unwrap();
    let base_id = created["memory_id"].as_str().unwrap().to_string();
    let resp = server
        .client
        .post(format!("{}/v1/branches/seed/apply", server.base))
        .header("Authorization", &alice_auth)
        .json(&json!({"adds": [base_id.clone()]}))
        .send()
        .await
        .expect("apply seed branch");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "seed apply failed: {body}");
    let resp = server
        .client
        .post(format!("{}/v1/branches/main/checkout", server.base))
        .header("Authorization", &alice_auth)
        .send()
        .await
        .expect("checkout alice main");
    assert_eq!(resp.status(), 200);

    // Alice creates a stale branch from the original main.
    let resp = server
        .client
        .post(format!("{}/v1/branches", server.base))
        .header("Authorization", &alice_auth)
        .json(&json!({"name": "alice-exp"}))
        .send()
        .await
        .expect("create alice branch");
    assert_eq!(resp.status(), 201);
    let resp = server
        .client
        .post(format!("{}/v1/branches/alice-exp/checkout", server.base))
        .header("Authorization", &alice_auth)
        .send()
        .await
        .expect("checkout alice branch");
    assert_eq!(resp.status(), 200);

    // Bob corrects the same memory and applies to main first.
    let resp = server
        .client
        .post(format!("{}/v1/branches", server.base))
        .header("Authorization", &bob_auth)
        .json(&json!({"name": "bob-exp"}))
        .send()
        .await
        .expect("create bob branch");
    assert_eq!(resp.status(), 201);
    let resp = server
        .client
        .post(format!("{}/v1/branches/bob-exp/checkout", server.base))
        .header("Authorization", &bob_auth)
        .send()
        .await
        .expect("checkout bob branch");
    assert_eq!(resp.status(), 200);
    let resp = server
        .client
        .put(format!("{}/v1/memories/{}/correct", server.base, base_id))
        .header("Authorization", &bob_auth)
        .json(&json!({"new_content": "bob wins on main first"}))
        .send()
        .await
        .expect("bob correct");
    assert_eq!(resp.status(), 200);
    let bob_corrected: Value = resp.json().await.unwrap();
    let bob_new_id = bob_corrected["memory_id"].as_str().unwrap().to_string();
    let resp = server
        .client
        .post(format!("{}/v1/branches/bob-exp/apply", server.base))
        .header("Authorization", &bob_auth)
        .json(&json!({
            "updates": [{"old_id": base_id, "new_id": bob_new_id}]
        }))
        .send()
        .await
        .expect("apply bob branch");
    assert_eq!(resp.status(), 200);

    // Alice makes a different correction on her stale branch.
    let resp = server
        .client
        .put(format!("{}/v1/memories/{}/correct", server.base, base_id))
        .header("Authorization", &alice_auth)
        .json(&json!({"new_content": "alice accept_branch target"}))
        .send()
        .await
        .expect("alice correct");
    assert_eq!(resp.status(), 200);
    let alice_corrected: Value = resp.json().await.unwrap();
    let alice_new_id = alice_corrected["memory_id"].as_str().unwrap().to_string();

    let resp = server
        .client
        .get(format!("{}/v1/branches/alice-exp/diff-items", server.base))
        .header("Authorization", &alice_auth)
        .send()
        .await
        .expect("alice diff items");
    assert_eq!(resp.status(), 200);
    let diff: Value = resp.json().await.unwrap();
    assert!(diff["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["memory_id"] == base_id));

    let resp = server
        .client
        .post(format!("{}/v1/branches/alice-exp/apply", server.base))
        .header("Authorization", &alice_auth)
        .json(&json!({
            "accept_branch_conflicts": [base_id]
        }))
        .send()
        .await
        .expect("apply alice conflict");
    let status = resp.status();
    let body = resp.text().await.unwrap();
    assert_eq!(status, 200, "accept_branch apply failed: {body}");
    let applied: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(applied["applied_conflicts"].as_array().unwrap().len(), 1);

    let resp = server
        .client
        .post(format!("{}/v1/branches/main/checkout", server.base))
        .header("Authorization", &alice_auth)
        .send()
        .await
        .expect("checkout alice main after apply");
    assert_eq!(resp.status(), 200);
    let resp = server
        .client
        .get(format!("{}/v1/memories", server.base))
        .header("Authorization", &alice_auth)
        .send()
        .await
        .expect("list memories after conflict apply");
    assert_eq!(resp.status(), 200);
    let body: Value = resp.json().await.unwrap();
    let contents: Vec<String> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["content"].as_str().map(ToString::to_string))
        .collect();
    assert!(contents
        .iter()
        .any(|content| content == "alice accept_branch target"));
    assert!(!contents
        .iter()
        .any(|content| content == "bob wins on main first"));

    let group_db_name = group["db_name"].as_str().unwrap();
    let group_db_url = replace_db_name(&admin_db_url(), group_db_name);
    let group_pool = sqlx::MySqlPool::connect(&group_db_url)
        .await
        .expect("group pool");
    let bob_active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_memories WHERE memory_id = ? AND is_active = 1",
    )
    .bind(&bob_new_id)
    .fetch_one(&group_pool)
    .await
    .unwrap();
    let alice_active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM mem_memories WHERE memory_id = ? AND is_active = 1",
    )
    .bind(&alice_new_id)
    .fetch_one(&group_pool)
    .await
    .unwrap();
    assert_eq!(
        bob_active_count, 0,
        "bob replacement should be removed from main"
    );
    assert_eq!(
        alice_active_count, 1,
        "alice replacement should be active on main"
    );

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_group_branch_diff_and_apply_mixed_flow() {
    let server = spawn_server().await;
    let group = create_group(&server, "alice").await;
    let group_id = group["group_id"].as_str().unwrap();
    let raw_key = create_group_key(&server, "alice", group_id).await;
    let auth = format!("Bearer {raw_key}");

    create_branch(&server, &auth, "seed").await;
    checkout_branch(&server, &auth, "seed").await;
    let update_seed = store_memory(&server, &auth, "base update target").await;
    let update_seed_id = update_seed["memory_id"].as_str().unwrap().to_string();
    let remove_seed = store_memory(&server, &auth, "base remove target").await;
    let remove_seed_id = remove_seed["memory_id"].as_str().unwrap().to_string();
    let applied = apply_branch(
        &server,
        &auth,
        "seed",
        json!({ "adds": [update_seed_id.clone(), remove_seed_id.clone()] }),
    )
    .await;
    assert_eq!(applied["applied_adds"].as_array().unwrap().len(), 2);
    checkout_branch(&server, &auth, "main").await;

    create_branch(&server, &auth, "work").await;
    checkout_branch(&server, &auth, "work").await;
    let added = store_memory(&server, &auth, "branch added memory").await;
    let added_id = added["memory_id"].as_str().unwrap().to_string();
    let corrected = correct_memory(&server, &auth, &update_seed_id, "branch updated memory").await;
    let corrected_id = corrected["memory_id"].as_str().unwrap().to_string();
    delete_memory(&server, &auth, &remove_seed_id).await;

    let diff = diff_items(&server, &auth, "work").await;
    assert!(diff["added"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["memory_id"] == added_id));
    assert!(diff["updated"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["memory_id"] == corrected_id && item["old_memory_id"] == update_seed_id));
    assert!(diff["removed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["memory_id"] == remove_seed_id));

    let applied = apply_branch(
        &server,
        &auth,
        "work",
        json!({
            "adds": [added_id.clone()],
            "updates": [{ "old_id": update_seed_id, "new_id": corrected_id }],
            "removes": [remove_seed_id.clone()]
        }),
    )
    .await;
    assert_eq!(applied["applied_adds"].as_array().unwrap().len(), 1);
    assert_eq!(applied["applied_updates"].as_array().unwrap().len(), 1);
    assert_eq!(applied["applied_removes"].as_array().unwrap().len(), 1);

    checkout_branch(&server, &auth, "main").await;
    let body = list_memories(&server, &auth).await;
    let contents: Vec<String> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["content"].as_str().map(ToString::to_string))
        .collect();
    assert!(contents
        .iter()
        .any(|content| content == "branch added memory"));
    assert!(contents
        .iter()
        .any(|content| content == "branch updated memory"));
    assert!(!contents
        .iter()
        .any(|content| content == "base update target"));
    assert!(!contents
        .iter()
        .any(|content| content == "base remove target"));

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_group_apply_skips_unchecked_conflict_and_keeps_main() {
    let server = spawn_server().await;
    register_user(&server, "bob").await;
    let group = create_group_with_members(&server, "alice", &["alice", "bob"]).await;
    let group_id = group["group_id"].as_str().unwrap();
    let alice_key = create_group_key(&server, "alice", group_id).await;
    let bob_key = create_group_key(&server, "bob", group_id).await;
    let alice_auth = format!("Bearer {alice_key}");
    let bob_auth = format!("Bearer {bob_key}");

    create_branch(&server, &alice_auth, "seed").await;
    checkout_branch(&server, &alice_auth, "seed").await;
    let created = store_memory(&server, &alice_auth, "shared base memory").await;
    let base_id = created["memory_id"].as_str().unwrap().to_string();
    let applied = apply_branch(
        &server,
        &alice_auth,
        "seed",
        json!({ "adds": [base_id.clone()] }),
    )
    .await;
    assert_eq!(applied["applied_adds"].as_array().unwrap().len(), 1);
    checkout_branch(&server, &alice_auth, "main").await;

    create_branch(&server, &alice_auth, "alice-exp").await;
    checkout_branch(&server, &alice_auth, "alice-exp").await;
    let alice_add = store_memory(
        &server,
        &alice_auth,
        "alice add while conflict stays on branch",
    )
    .await;
    let alice_add_id = alice_add["memory_id"].as_str().unwrap().to_string();

    create_branch(&server, &bob_auth, "bob-exp").await;
    checkout_branch(&server, &bob_auth, "bob-exp").await;
    let bob_corrected = correct_memory(&server, &bob_auth, &base_id, "bob keeps main").await;
    let bob_new_id = bob_corrected["memory_id"].as_str().unwrap().to_string();
    let applied = apply_branch(
        &server,
        &bob_auth,
        "bob-exp",
        json!({
            "updates": [{ "old_id": base_id, "new_id": bob_new_id }]
        }),
    )
    .await;
    assert_eq!(applied["applied_updates"].as_array().unwrap().len(), 1);

    let alice_corrected = correct_memory(
        &server,
        &alice_auth,
        &base_id,
        "alice conflict stays on branch",
    )
    .await;
    let alice_new_id = alice_corrected["memory_id"].as_str().unwrap().to_string();

    let diff = diff_items(&server, &alice_auth, "alice-exp").await;
    assert!(diff["conflicts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["memory_id"] == base_id));
    assert!(diff["added"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["memory_id"] == alice_add_id));

    let applied = apply_branch(
        &server,
        &alice_auth,
        "alice-exp",
        json!({
            "adds": [alice_add_id.clone()]
        }),
    )
    .await;
    assert_eq!(applied["applied_adds"].as_array().unwrap().len(), 1);
    assert!(applied["applied_conflicts"].as_array().unwrap().is_empty());

    checkout_branch(&server, &alice_auth, "main").await;
    let body = list_memories(&server, &alice_auth).await;
    let contents: Vec<String> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["content"].as_str().map(ToString::to_string))
        .collect();
    let ids: Vec<String> = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|item| item["memory_id"].as_str().map(ToString::to_string))
        .collect();
    assert!(contents
        .iter()
        .any(|content| content == "alice add while conflict stays on branch"));
    assert!(contents.iter().any(|content| content == "bob keeps main"));
    assert!(!contents
        .iter()
        .any(|content| content == "alice conflict stays on branch"));
    assert!(ids.iter().any(|id| id == &bob_new_id));
    assert!(!ids.iter().any(|id| id == &alice_new_id));

    server.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_group_diff_reports_behind_main_items() {
    let server = spawn_server().await;
    register_user(&server, "bob").await;
    let group = create_group_with_members(&server, "alice", &["alice", "bob"]).await;
    let group_id = group["group_id"].as_str().unwrap();
    let alice_key = create_group_key(&server, "alice", group_id).await;
    let bob_key = create_group_key(&server, "bob", group_id).await;
    let alice_auth = format!("Bearer {alice_key}");
    let bob_auth = format!("Bearer {bob_key}");

    create_branch(&server, &alice_auth, "alice-exp").await;
    checkout_branch(&server, &alice_auth, "main").await;

    create_branch(&server, &bob_auth, "bob-exp").await;
    checkout_branch(&server, &bob_auth, "bob-exp").await;
    let bob_added = store_memory(&server, &bob_auth, "bob added after alice branch").await;
    let bob_added_id = bob_added["memory_id"].as_str().unwrap().to_string();
    let applied = apply_branch(
        &server,
        &bob_auth,
        "bob-exp",
        json!({ "adds": [bob_added_id.clone()] }),
    )
    .await;
    assert_eq!(applied["applied_adds"].as_array().unwrap().len(), 1);

    let diff = diff_items(&server, &alice_auth, "alice-exp").await;
    assert!(diff["behind_main"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["memory_id"] == bob_added_id
            && item["content"] == "bob added after alice branch"));

    server.cleanup().await;
}
