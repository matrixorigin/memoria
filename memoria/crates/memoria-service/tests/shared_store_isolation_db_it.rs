//! Scope checks must not depend on a DbRouter. SQL tests use a unique test database.
use async_trait::async_trait;
use memoria_core::{interfaces::MemoryStore, MemoriaError, Memory, MemoryType};
use memoria_service::MemoryService;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Default)]
struct SharedStore(Mutex<HashMap<String, Memory>>);

#[async_trait]
impl MemoryStore for SharedStore {
    async fn insert(&self, memory: &Memory) -> Result<(), MemoriaError> {
        self.0
            .lock()
            .unwrap()
            .insert(memory.memory_id.clone(), memory.clone());
        Ok(())
    }
    async fn get(&self, id: &str) -> Result<Option<Memory>, MemoriaError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .get(id)
            .filter(|m| m.is_active)
            .cloned())
    }
    async fn get_including_inactive(&self, id: &str) -> Result<Option<Memory>, MemoriaError> {
        Ok(self.0.lock().unwrap().get(id).cloned())
    }
    async fn update(&self, memory: &Memory) -> Result<(), MemoriaError> {
        self.insert(memory).await
    }
    async fn soft_delete(&self, id: &str) -> Result<(), MemoriaError> {
        if let Some(memory) = self.0.lock().unwrap().get_mut(id) {
            memory.is_active = false;
        }
        Ok(())
    }
    async fn list_active(&self, user: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .values()
            .filter(|m| m.user_id == user && m.is_active)
            .take(limit as usize)
            .cloned()
            .collect())
    }
    async fn search_fulltext(&self, _: &str, _: &str, _: i64) -> Result<Vec<Memory>, MemoriaError> {
        Ok(vec![])
    }
    async fn search_vector(&self, _: &str, _: &[f32], _: i64) -> Result<Vec<Memory>, MemoriaError> {
        Ok(vec![])
    }
}
fn service() -> MemoryService {
    MemoryService::new(Arc::new(SharedStore::default()), None, None)
}
async fn memory(svc: &MemoryService, owner: &str) -> Memory {
    svc.store_memory(
        owner,
        "private memory",
        MemoryType::Working,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn shared_store_read_rejects_other_user() {
    let svc = service();
    let alice = memory(&svc, "alice").await;
    assert!(svc
        .get_for_user("bob", &alice.memory_id)
        .await
        .unwrap()
        .is_none());
    assert!(svc
        .get_for_user("alice", &alice.memory_id)
        .await
        .unwrap()
        .is_some());
}
#[tokio::test]
async fn shared_store_correct_rejects_other_user() {
    let svc = service();
    let alice = memory(&svc, "alice").await;
    assert!(matches!(
        svc.correct("bob", &alice.memory_id, "changed").await,
        Err(MemoriaError::NotFound(_))
    ));
    assert_eq!(
        svc.get_for_user("alice", &alice.memory_id)
            .await
            .unwrap()
            .unwrap()
            .content,
        alice.content
    );
}
#[tokio::test]
async fn shared_store_purge_rejects_other_user() {
    let svc = service();
    let alice = memory(&svc, "alice").await;
    assert!(matches!(
        svc.purge("bob", &alice.memory_id).await,
        Err(MemoriaError::NotFound(_))
    ));
    assert!(svc
        .get_for_user("alice", &alice.memory_id)
        .await
        .unwrap()
        .is_some());
}
#[tokio::test]
async fn shared_store_batch_rejects_mixed_owners_without_mutation() {
    let svc = service();
    let alice = memory(&svc, "alice").await;
    let bob = memory(&svc, "bob").await;
    assert!(matches!(
        svc.purge_batch("bob", &[&bob.memory_id, &alice.memory_id])
            .await,
        Err(MemoriaError::NotFound(_))
    ));
    assert!(svc
        .get_for_user("alice", &alice.memory_id)
        .await
        .unwrap()
        .is_some());
    assert!(svc
        .get_for_user("bob", &bob.memory_id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn shared_sql_store_rejects_other_user_on_main_and_branch() {
    use memoria_git::GitForDataService;
    use memoria_storage::SqlMemoryStore;
    let base = std::env::var("DATABASE_URL").expect("DATABASE_URL must select a test MatrixOne");
    let url = memoria_test_utils::unique_db_url(&base, "scope_isolation");
    let db_name = url.rsplit('/').next().unwrap().to_string();
    let sql = Arc::new(
        SqlMemoryStore::connect(&url, 4, uuid::Uuid::new_v4().to_string())
            .await
            .unwrap(),
    );
    sql.migrate().await.unwrap();
    let svc = Arc::new(MemoryService::new_sql_with_llm(sql.clone(), None, None).await);
    let git = GitForDataService::new(sql.pool().clone(), &db_name);
    let alice = memory(&svc, "alice").await;
    let bob = memory(&svc, "bob").await;
    git.create_branch("scope_branch", "mem_memories")
        .await
        .unwrap();
    sql.register_branch("alice", "branch", "scope_branch")
        .await
        .unwrap();
    sql.register_branch("bob", "branch", "scope_branch")
        .await
        .unwrap();
    let tested = svc.clone();
    let tested_sql = sql.clone();
    let tested_git = GitForDataService::new(sql.pool().clone(), &db_name);
    // Catch assertion panics so cleanup still runs on a regression.
    let result = tokio::spawn(async move {
        for branch in [Some("branch"), Some("main"), None] {
            let read_rejected = tested.get_for_user_on_branch("bob", branch, &alice.memory_id).await.unwrap().is_none();
            let correction_rejected = matches!(tested.correct_on_branch("bob", branch, &alice.memory_id, "changed").await, Err(MemoriaError::NotFound(_)));
            let purge_rejected = matches!(tested.purge_on_branch("bob", branch, &alice.memory_id).await, Err(MemoriaError::NotFound(_)));
            let batch_rejected = matches!(tested.purge_batch_on_branch("bob", branch, &[&bob.memory_id, &alice.memory_id]).await, Err(MemoriaError::NotFound(_)));
            assert!(read_rejected && correction_rejected && purge_rejected && batch_rejected,
                "branch={branch:?}: read={read_rejected}, correct={correction_rejected}, purge={purge_rejected}, batch={batch_rejected}");
            assert_eq!(tested.get_for_user_on_branch("alice", branch, &alice.memory_id).await.unwrap().unwrap().content, alice.content);
            assert!(tested.get_for_user_on_branch("bob", branch, &bob.memory_id).await.unwrap().is_some());
        }
        assert!(tested_git.list_snapshots().await.unwrap().is_empty(), "rejected purges must not create snapshots");
        let corrected = tested.correct_on_branch("bob", Some("branch"), &bob.memory_id, "branch correction").await.unwrap();
        tested.purge_on_branch("bob", Some("branch"), &corrected.memory_id).await.unwrap();
        assert!(tested.get_for_user_on_branch("bob", Some("main"), &bob.memory_id).await.unwrap().is_some());
        let corrected = tested.correct("bob", &bob.memory_id, "owner correction").await.unwrap();
        assert_eq!(corrected.user_id, "bob");
        // Inactive foreign rows must not authorize cleanup of their metadata.
        sqlx::query("INSERT INTO mem_entity_links (id, user_id, memory_id, entity_name, entity_type, source, created_at) VALUES (?, 'bob', ?, 'private', 'concept', 'manual', NOW())")
            .bind(uuid::Uuid::new_v4().simple().to_string()).bind(&bob.memory_id).execute(tested_sql.pool()).await.unwrap();
        assert!(matches!(tested.purge("alice", &bob.memory_id).await, Err(MemoriaError::NotFound(_))));
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mem_entity_links WHERE memory_id = ?").bind(&bob.memory_id).fetch_one(tested_sql.pool()).await.unwrap();
        assert_eq!(count, 1);
        tested.purge("bob", &bob.memory_id).await.unwrap();
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM mem_entity_links WHERE memory_id = ?").bind(&bob.memory_id).fetch_one(tested_sql.pool()).await.unwrap();
        assert_eq!(count, 0);
        tested.purge("bob", &bob.memory_id).await.unwrap();
        assert!(matches!(tested.purge_batch("alice", &[&alice.memory_id, "missing"]).await, Err(MemoriaError::NotFound(_))));
        assert!(tested.get_for_user("alice", &alice.memory_id).await.unwrap().is_some());
        tested.purge_batch_unscoped_on_branch("admin", None, &[&alice.memory_id, "missing"]).await.unwrap();
        assert!(tested.get_for_user("alice", &alice.memory_id).await.unwrap().is_none());
    }).await;
    svc.flush_edit_log().await;
    let snapshots = git.list_snapshots().await.unwrap();
    for snapshot in snapshots {
        assert_eq!(snapshot.database_name.as_deref(), Some(db_name.as_str()));
        git.drop_snapshot(&snapshot.snapshot_name).await.unwrap();
    }
    sqlx::query(&format!("DROP DATABASE `{db_name}`"))
        .execute(sql.pool())
        .await
        .unwrap();
    result.unwrap();
}

#[tokio::test]
async fn shared_store_preserves_owner_and_group_scope_operations() {
    for owner in ["alice", "grp_shared"] {
        let svc = service();
        let original = memory(&svc, owner).await;
        let corrected = svc
            .correct(owner, &original.memory_id, "owner correction")
            .await
            .unwrap();
        assert_eq!(corrected.user_id, owner);
        assert!(svc
            .get_for_user(owner, &original.memory_id)
            .await
            .unwrap()
            .is_none());
        assert!(matches!(
            svc.purge("other", &original.memory_id).await,
            Err(MemoriaError::NotFound(_))
        ));
        svc.purge(owner, &original.memory_id).await.unwrap();
        svc.purge(owner, &original.memory_id).await.unwrap();
        svc.purge_batch(owner, &[&corrected.memory_id])
            .await
            .unwrap();
        assert!(svc
            .get_for_user(owner, &corrected.memory_id)
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn shared_store_rejects_missing_ids_before_mutation() {
    let svc = service();
    let alice = memory(&svc, "alice").await;
    assert!(svc
        .get_for_user("alice", "missing")
        .await
        .unwrap()
        .is_none());
    assert!(matches!(
        svc.correct("alice", "missing", "new").await,
        Err(MemoriaError::NotFound(_))
    ));
    assert!(matches!(
        svc.purge("alice", "missing").await,
        Err(MemoriaError::NotFound(_))
    ));
    assert!(matches!(
        svc.purge_batch("alice", &[&alice.memory_id, "missing"])
            .await,
        Err(MemoriaError::NotFound(_))
    ));
    assert!(svc
        .get_for_user("alice", &alice.memory_id)
        .await
        .unwrap()
        .is_some());
}

#[tokio::test]
async fn administrative_purge_preserves_unscoped_and_missing_id_behavior() {
    let svc = service();
    let alice = memory(&svc, "alice").await;
    svc.purge_batch_unscoped_on_branch("admin", None, &[&alice.memory_id, "missing"])
        .await
        .unwrap();
    assert!(svc
        .get_for_user("alice", &alice.memory_id)
        .await
        .unwrap()
        .is_none());
    svc.purge_batch_unscoped_on_branch("admin", None, &[&alice.memory_id])
        .await
        .unwrap();
}
