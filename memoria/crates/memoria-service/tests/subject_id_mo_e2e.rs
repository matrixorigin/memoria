/// Service-layer E2E tests against MatrixOne for subject_id + memory_types.
///
/// Unlike `subject_id_filter.rs` (in-memory MapStore), these tests exercise the
/// full MemoryService → SqlMemoryStore → MatrixOne SQL path, including dedup.
///
/// Run:
///   DATABASE_URL=mysql://root:111@localhost:6001/memoria_test \
///   SQLX_OFFLINE=true cargo test -p memoria-service --test subject_id_mo_e2e -- --nocapture
use std::sync::Arc;

use async_trait::async_trait;
use memoria_core::{interfaces::EmbeddingProvider, interfaces::MemoryStore, MemoryType, MemoriaError};
use memoria_service::{
    ExplainLevel, ListActiveOptions, MemoryService, RetrieveOptions,
};
use memoria_storage::SqlMemoryStore;
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

fn db_url() -> String {
    std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string())
}

/// Deterministic embedder: maps text length to a unit vector axis for vector search.
struct HashEmbedder {
    dim: usize,
}

#[async_trait]
impl EmbeddingProvider for HashEmbedder {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoriaError> {
        let mut v = vec![0.0f32; self.dim];
        let idx = text.len() % self.dim;
        v[idx] = 1.0;
        Ok(v)
    }

    fn dimension(&self) -> usize {
        self.dim
    }
}

async fn setup_service() -> (MemoryService, String, Arc<SqlMemoryStore>) {
    let dim = test_dim();
    let instance_id = Uuid::new_v4().to_string();
    let store = SqlMemoryStore::connect(&db_url(), dim, instance_id)
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let store_arc = Arc::new(store);
    let embedder = Arc::new(HashEmbedder { dim });
    let service =
        MemoryService::new_sql_with_llm(store_arc.clone(), Some(embedder), None).await;
    let user_id = format!("svc_mo_{}", Uuid::new_v4().simple());
    (service, user_id, store_arc)
}

// ── List via service → list_active_lite SQL ──────────────────────────────────

#[tokio::test]
async fn test_service_list_subject_isolation() {
    let (svc, uid, _) = setup_service().await;
    let subject_a = "svc-alice";
    let subject_b = "svc-bob";

    svc.store_memory(
        uid.as_str(),
        "alice svc memory",
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        Some(subject_a.to_string()),
    )
    .await
    .unwrap();
    svc.store_memory(
        uid.as_str(),
        "bob svc memory",
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        Some(subject_b.to_string()),
    )
    .await
    .unwrap();

    let alice = svc
        .list_active_filtered_on_branch(
            &uid,
            None,
            ListActiveOptions {
                limit: 20,
                memory_type: None,
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id: Some(subject_a),
            },
        )
        .await
        .unwrap();
    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0].subject_id.as_deref(), Some(subject_a));
    assert_eq!(alice[0].content, "alice svc memory");

    let bob = svc
        .list_active_filtered_on_branch(
            &uid,
            None,
            ListActiveOptions {
                limit: 20,
                memory_type: None,
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id: Some(subject_b),
            },
        )
        .await
        .unwrap();
    assert_eq!(bob.len(), 1);
    assert_eq!(bob[0].content, "bob svc memory");
    println!("✅ service list subject isolation via SQL");
}

#[tokio::test]
async fn test_service_list_null_subject_excluded() {
    let (svc, uid, _) = setup_service().await;
    let subject = "svc-scoped";

    svc.store_memory(
        uid.as_str(),
        "unscoped svc memory",
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();
    svc.store_memory(
        uid.as_str(),
        "scoped svc memory",
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        Some(subject.to_string()),
    )
    .await
    .unwrap();

    let scoped = svc
        .list_active_filtered_on_branch(
            &uid,
            None,
            ListActiveOptions {
                limit: 20,
                memory_type: None,
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id: Some(subject),
            },
        )
        .await
        .unwrap();
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].content, "scoped svc memory");
    println!("✅ service list NULL subject excluded");
}

// ── Retrieve via service → hybrid SQL path ───────────────────────────────────

#[tokio::test]
async fn test_service_retrieve_subject_and_memory_types() {
    let (svc, uid, _) = setup_service().await;
    let subject = "svc-retrieve-subj";
    let token = "nebulaunique";

    svc.store_memory(
        uid.as_str(),
        &format!("{token} semantic for alice"),
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        Some(subject.to_string()),
    )
    .await
    .unwrap();
    svc.store_memory(
        uid.as_str(),
        &format!("{token} profile for alice"),
        MemoryType::Profile,
        None,
        None,
        None,
        None,
        None,
        Some(subject.to_string()),
    )
    .await
    .unwrap();
    svc.store_memory(
        uid.as_str(),
        &format!("{token} semantic for bob"),
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        Some("other-retrieve-subj".to_string()),
    )
    .await
    .unwrap();

    let opts = RetrieveOptions::from_session_scope(None, None)
        .with_subject_id(Some(subject.to_string()))
        .with_memory_types(Some(vec![MemoryType::Semantic]));

    let (results, explain) = svc
        .retrieve_explain_level_with_options_on_branch(
            &uid,
            None,
            token,
            10,
            ExplainLevel::Basic,
            &opts,
        )
        .await
        .unwrap();

    assert!(!results.is_empty(), "retrieve should find scoped semantic memory");
    for m in &results {
        assert_eq!(m.subject_id.as_deref(), Some(subject));
        assert_eq!(m.memory_type, MemoryType::Semantic);
    }
    assert!(
        explain.vector_attempted || explain.fulltext_attempted || explain.graph_attempted,
        "expected SQL retrieval path, got path={}",
        explain.path
    );
    println!(
        "✅ service retrieve subject + memory_types (path={})",
        explain.path
    );
}

#[tokio::test]
async fn test_service_subject_id_round_trip_via_sql() {
    let (svc, uid, store) = setup_service().await;
    let subject = "svc-roundtrip";

    let stored = svc
        .store_memory(
            uid.as_str(),
            "roundtrip via sql store",
            MemoryType::Semantic,
            None,
            None,
            None,
            None,
            None,
            Some(subject.to_string()),
        )
        .await
        .unwrap();
    assert_eq!(stored.subject_id.as_deref(), Some(subject));

    let fetched = store.get(&stored.memory_id).await.unwrap().unwrap();
    assert_eq!(fetched.subject_id.as_deref(), Some(subject));

    let listed = svc
        .list_active_filtered_on_branch(
            &uid,
            None,
            ListActiveOptions {
                limit: 10,
                memory_type: None,
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id: Some(subject),
            },
        )
        .await
        .unwrap();
    assert!(
        listed.iter().any(|m| m.memory_id == stored.memory_id),
        "stored memory must appear in subject-filtered list"
    );
    println!("✅ service subject_id round-trip via SQL");
}

#[tokio::test]
async fn test_service_batch_store_subject_ids() {
    let (svc, uid, _) = setup_service().await;
    let subject_a = "batch-alice";
    let subject_b = "batch-bob";

    let stored = svc
        .store_batch_with_metadata_on_branch(
            &uid,
            None,
            vec![
                (
                    "batch alice semantic".to_string(),
                    MemoryType::Semantic,
                    None,
                    None,
                    Some(subject_a.to_string()),
                    None,
                    None,
                ),
                (
                    "batch bob profile".to_string(),
                    MemoryType::Profile,
                    None,
                    None,
                    Some(subject_b.to_string()),
                    None,
                    None,
                ),
            ],
            None,
        )
        .await
        .unwrap();
    assert_eq!(stored.len(), 2);

    for m in &stored {
        assert!(
            m.subject_id.as_deref() == Some(subject_a) || m.subject_id.as_deref() == Some(subject_b)
        );
    }

    let alice_list = svc
        .list_active_filtered_on_branch(
            &uid,
            None,
            ListActiveOptions {
                limit: 10,
                memory_type: None,
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id: Some(subject_a),
            },
        )
        .await
        .unwrap();
    assert_eq!(alice_list.len(), 1);
    assert_eq!(alice_list[0].subject_id.as_deref(), Some(subject_a));
    println!("✅ service batch store preserves subject_id");
}

#[tokio::test]
async fn test_service_subject_id_length_validation() {
    let (svc, uid, _) = setup_service().await;

    let ok = "x".repeat(128);
    svc.store_memory(
        uid.as_str(),
        "ok length",
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        Some(ok),
    )
    .await
    .expect("128-char subject_id accepted");

    let too_long = "x".repeat(129);
    let err = svc
        .store_memory(
            uid.as_str(),
            "too long",
            MemoryType::Semantic,
            None,
            None,
            None,
            None,
            None,
            Some(too_long),
        )
        .await;
    assert!(err.is_err(), "129-char subject_id must be rejected at service layer");
    println!("✅ service subject_id length validation against SQL store");
}

// ── Dedup isolation by subject_id ─────────────────────────────────────────────

/// Regression: identical content under different subjects must not be treated as
/// an exact duplicate — each subject gets its own active row.
#[tokio::test]
async fn test_service_dedup_exact_content_isolated_by_subject() {
    let (svc, uid, _) = setup_service().await;
    let subject_a = "dedup-exact-a";
    let subject_b = "dedup-exact-b";
    let content = "identical semantic memory for cross-subject dedup isolation";

    svc.store_memory(
        uid.as_str(),
        content,
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        Some(subject_a.to_string()),
    )
    .await
    .expect("store for subject_a");
    svc.store_memory(
        uid.as_str(),
        content,
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        Some(subject_b.to_string()),
    )
    .await
    .expect("store for subject_b");

    let alice = svc
        .list_active_filtered_on_branch(
            &uid,
            None,
            ListActiveOptions {
                limit: 10,
                memory_type: Some("semantic"),
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id: Some(subject_a),
            },
        )
        .await
        .unwrap();
    let bob = svc
        .list_active_filtered_on_branch(
            &uid,
            None,
            ListActiveOptions {
                limit: 10,
                memory_type: Some("semantic"),
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id: Some(subject_b),
            },
        )
        .await
        .unwrap();

    assert_eq!(alice.len(), 1, "subject_a must retain its own memory");
    assert_eq!(bob.len(), 1, "subject_b must retain its own memory");
    assert_eq!(alice[0].content, content);
    assert_eq!(bob[0].content, content);
    assert_ne!(
        alice[0].memory_id, bob[0].memory_id,
        "each subject must have a distinct memory_id"
    );
    assert!(alice[0].is_active && bob[0].is_active);
    println!("✅ dedup: identical content isolated across subjects");
}

/// Regression: near-duplicate embeddings under different subjects must not
/// supersede each other. HashEmbedder maps equal-length text to the same vector,
/// so this exercises the near-dup branch (different content, L2 dist = 0).
#[tokio::test]
async fn test_service_dedup_near_duplicate_isolated_by_subject() {
    let (svc, uid, _) = setup_service().await;
    let subject_a = "dedup-near-a";
    let subject_b = "dedup-near-b";
    // Same length → identical embedding under HashEmbedder, different text → near-dup path.
    let content_a = "subject-a-near-dup-isolation-test!!";
    let content_b = "subject-b-near-dup-isolation-test!!";
    assert_eq!(
        content_a.len(),
        content_b.len(),
        "test precondition: embedder must produce identical vectors"
    );

    let first = svc
        .store_memory(
            uid.as_str(),
            content_a,
            MemoryType::Semantic,
            None,
            None,
            None,
            None,
            None,
            Some(subject_a.to_string()),
        )
        .await
        .expect("store for subject_a");

    svc.store_memory(
        uid.as_str(),
        content_b,
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        Some(subject_b.to_string()),
    )
    .await
    .expect("store for subject_b");

    let alice = svc
        .list_active_filtered_on_branch(
            &uid,
            None,
            ListActiveOptions {
                limit: 10,
                memory_type: Some("semantic"),
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id: Some(subject_a),
            },
        )
        .await
        .unwrap();
    assert_eq!(alice.len(), 1);
    assert_eq!(alice[0].memory_id, first.memory_id);
    assert!(alice[0].is_active, "subject_a memory must not be superseded by subject_b write");

    let bob = svc
        .list_active_filtered_on_branch(
            &uid,
            None,
            ListActiveOptions {
                limit: 10,
                memory_type: Some("semantic"),
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id: Some(subject_b),
            },
        )
        .await
        .unwrap();
    assert_eq!(bob.len(), 1);
    assert!(bob[0].is_active);
    assert_ne!(bob[0].memory_id, first.memory_id);
    println!("✅ dedup: near-duplicate embeddings isolated across subjects");
}

/// Within the same subject, near-duplicate content should still supersede (existing behaviour).
#[tokio::test]
async fn test_service_dedup_near_duplicate_supersedes_within_subject() {
    let (svc, uid, _) = setup_service().await;
    let subject = "dedup-within-subject";
    let content_v1 = "within-subject-near-dup-test-v1!!";
    let content_v2 = "within-subject-near-dup-test-v2!!";
    assert_eq!(content_v1.len(), content_v2.len());

    let first = svc
        .store_memory(
            uid.as_str(),
            content_v1,
            MemoryType::Semantic,
            None,
            None,
            None,
            None,
            None,
            Some(subject.to_string()),
        )
        .await
        .expect("store v1");

    svc.store_memory(
        uid.as_str(),
        content_v2,
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        Some(subject.to_string()),
    )
    .await
    .expect("store v2");

    let active = svc
        .list_active_filtered_on_branch(
            &uid,
            None,
            ListActiveOptions {
                limit: 10,
                memory_type: Some("semantic"),
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id: Some(subject),
            },
        )
        .await
        .unwrap();
    assert_eq!(
        active.len(),
        1,
        "within-subject near-dup should leave only one active memory"
    );
    assert_ne!(
        active[0].memory_id, first.memory_id,
        "v2 should have superseded v1"
    );
    assert_eq!(active[0].content, content_v2);
    println!("✅ dedup: within-subject near-duplicate still supersedes");
}
