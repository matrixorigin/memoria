//! Router-less deployments share one store for every scope, so a `grp_*`
//! admin delete must not run its table-wide statement there.

use memoria_git::GitForDataService;
use memoria_service::MemoryService;
use memoria_storage::SqlMemoryStore;
use serde_json::json;
use sqlx::MySqlPool;
use std::sync::Arc;
use uuid::Uuid;

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

fn db_name(url: &str) -> String {
    let end = url.find(['?', '#']).unwrap_or(url.len());
    url[..end]
        .rsplit_once('/')
        .map(|(_, name)| name.to_string())
        .expect("database url must include db name")
}

#[tokio::test]
async fn routerless_group_delete_keeps_other_users_rows() {
    let url = db_url();
    let store = SqlMemoryStore::connect(&url, test_dim(), Uuid::new_v4().to_string())
        .await
        .expect("connect store");
    store.migrate().await.expect("migrate store");
    let store = Arc::new(store);
    let table = store.t("mem_memories");
    let pool = MySqlPool::connect(&url).await.expect("connect pool");
    let git = Arc::new(GitForDataService::new(pool.clone(), &db_name(&url)));
    let service = Arc::new(MemoryService::new_sql_with_llm(store, None, None).await);
    assert!(service.db_router.is_none(), "fixture must be router-less");
    let app = memoria_api::build_router(memoria_api::AppState::new(service, git, String::new()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let client = reqwest::Client::builder().no_proxy().build().unwrap();

    let other = format!("other-{}", Uuid::new_v4().simple());
    let stored = client
        .post(format!("{base}/v1/memories"))
        .header("X-User-Id", &other)
        .json(&json!({"content": "another owner's active memory"}))
        .send()
        .await
        .unwrap();
    assert!(stored.status().is_success(), "store: {}", stored.status());

    let group = format!("grp_{}", Uuid::new_v4().simple());
    let deleted = client
        .delete(format!("{base}/admin/users/{group}"))
        .send()
        .await
        .unwrap();
    assert_eq!(
        deleted.status(),
        409,
        "router-less group delete must be rejected"
    );

    let active: i64 = sqlx::query_scalar(&format!(
        "SELECT COUNT(*) FROM {table} WHERE user_id = ? AND is_active > 0"
    ))
    .bind(&other)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active, 1, "another owner's row must stay active");

    // Ordinary per-user deletion is unaffected in router-less mode.
    let own = client
        .delete(format!("{base}/admin/users/{other}"))
        .send()
        .await
        .unwrap();
    assert_eq!(own.status(), 200);
    server.abort();
}
