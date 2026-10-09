/// Integration tests for subject_id isolation and memory_types filtering.
///
/// Tests cover:
/// 1. subject_id isolation — memories for subject A are not visible when querying subject B
/// 2. NULL semantics — memories with NULL subject_id are excluded when a subject filter is set
/// 3. memory_types filter — retrieve/list only returns requested types
/// 4. Backward compatibility — callers that don't pass subject_id still work correctly
/// 5. Composite filtering — subject_id + memory_types together
///
/// Run: cargo test -p memoria-service --test subject_id_filter -- --nocapture
use memoria_core::MemoryType;
use memoria_service::{ListActiveOptions, MemoryService, RetrieveOptions};
use std::sync::Arc;

/// ── helpers ──────────────────────────────────────────────────────────────────

mod mock_support {
    use memoria_core::{
        interfaces::{EmbeddingProvider, MemoryStore},
        Memory, MemoriaError,
    };
    use std::collections::HashMap;
    use std::sync::Mutex;

    /// Mock embedding: returns a stable fixed-dim vector derived from content length.
    pub struct StableEmbedder;
    #[async_trait::async_trait]
    impl EmbeddingProvider for StableEmbedder {
        async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoriaError> {
            let seed = text.len() as f32;
            Ok(vec![seed; 16])
        }
        fn dimension(&self) -> usize {
            16
        }
    }

    /// Simple in-memory store backed by a Mutex<HashMap>.
    #[derive(Default)]
    pub struct MapStore {
        data: Mutex<HashMap<String, Memory>>,
    }

    #[async_trait::async_trait]
    impl MemoryStore for MapStore {
        async fn insert(&self, m: &Memory) -> Result<(), MemoriaError> {
            self.data
                .lock()
                .unwrap()
                .insert(m.memory_id.clone(), m.clone());
            Ok(())
        }

        async fn get(&self, id: &str) -> Result<Option<Memory>, MemoriaError> {
            Ok(self.data.lock().unwrap().get(id).cloned())
        }

        async fn get_including_inactive(&self, id: &str) -> Result<Option<Memory>, MemoriaError> {
            Ok(self.data.lock().unwrap().get(id).cloned())
        }

        async fn update(&self, m: &Memory) -> Result<(), MemoriaError> {
            self.data
                .lock()
                .unwrap()
                .insert(m.memory_id.clone(), m.clone());
            Ok(())
        }

        async fn soft_delete(&self, id: &str) -> Result<(), MemoriaError> {
            if let Some(m) = self.data.lock().unwrap().get_mut(id) {
                m.is_active = false;
            }
            Ok(())
        }

        async fn list_active(
            &self,
            user_id: &str,
            limit: i64,
        ) -> Result<Vec<Memory>, MemoriaError> {
            let guard = self.data.lock().unwrap();
            let mut v: Vec<Memory> = guard
                .values()
                .filter(|m| m.user_id == user_id && m.is_active)
                .cloned()
                .collect();
            v.sort_by(|a, b| b.memory_id.cmp(&a.memory_id));
            v.truncate(limit as usize);
            Ok(v)
        }

        async fn search_fulltext(
            &self,
            user_id: &str,
            query: &str,
            limit: i64,
        ) -> Result<Vec<Memory>, MemoriaError> {
            let guard = self.data.lock().unwrap();
            let q = query.to_lowercase();
            let mut v: Vec<Memory> = guard
                .values()
                .filter(|m| {
                    m.user_id == user_id
                        && m.is_active
                        && m.content.to_lowercase().contains(&q)
                })
                .cloned()
                .collect();
            v.truncate(limit as usize);
            Ok(v)
        }

        async fn search_vector(
            &self,
            user_id: &str,
            _embedding: &[f32],
            limit: i64,
        ) -> Result<Vec<Memory>, MemoriaError> {
            // Simple mock: return all active memories for the user
            self.list_active(user_id, limit).await
        }
    }
}

use mock_support::{MapStore, StableEmbedder};

fn make_service() -> MemoryService {
    MemoryService::new(
        Arc::new(MapStore::default()),
        Some(Arc::new(StableEmbedder)),
        None,
    )
}

// ── Test 1: subject_id isolation ─────────────────────────────────────────────

#[tokio::test]
async fn test_subject_id_store_and_list_isolation() {
    let svc = make_service();
    let uid = "iso-user";
    let subject_a = "subject-alice";
    let subject_b = "subject-bob";

    // Store memories for two different subjects under the same user
    svc.store_memory(
        uid,
        "alice memory 1",
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
        uid,
        "alice memory 2",
        MemoryType::Profile,
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
        uid,
        "bob memory 1",
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

    // List with subject_a filter: should only see alice's memories
    let alice_list = svc
        .list_active_filtered_on_branch(
            uid,
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

    assert_eq!(
        alice_list.len(),
        2,
        "subject_a filter should return 2 memories, got: {:?}",
        alice_list.iter().map(|m| &m.content).collect::<Vec<_>>()
    );
    for m in &alice_list {
        assert_eq!(
            m.subject_id.as_deref(),
            Some(subject_a),
            "all memories must belong to subject_a"
        );
    }

    // List with subject_b filter: should only see bob's memory
    let bob_list = svc
        .list_active_filtered_on_branch(
            uid,
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

    assert_eq!(bob_list.len(), 1, "subject_b filter should return 1 memory");
    assert_eq!(bob_list[0].content, "bob memory 1");
    println!("✅ subject_id isolation: list");
}

// ── Test 2: NULL semantics — unscoped memories are excluded by subject filter ─

#[tokio::test]
async fn test_subject_id_null_excluded_when_filtering() {
    let svc = make_service();
    let uid = "null-user";
    let subject = "subject-x";

    // Memory with no subject_id (NULL)
    svc.store_memory(
        uid,
        "unscoped memory",
        MemoryType::Semantic,
        None,
        None,
        None,
        None,
        None,
        None, // subject_id = NULL
    )
    .await
    .unwrap();

    // Memory belonging to subject-x
    svc.store_memory(
        uid,
        "scoped memory",
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

    // Filter by subject-x: only "scoped memory" should appear
    let list = svc
        .list_active_filtered_on_branch(
            uid,
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

    assert_eq!(list.len(), 1, "NULL-subject memories must NOT appear in subject filter");
    assert_eq!(list[0].content, "scoped memory");
    assert_eq!(list[0].subject_id.as_deref(), Some(subject));
    println!("✅ subject_id NULL semantics: unscoped excluded");
}

// ── Test 3: No subject filter returns all memories ───────────────────────────

#[tokio::test]
async fn test_no_subject_filter_returns_all() {
    let svc = make_service();
    let uid = "all-user";

    svc.store_memory(uid, "mem null", MemoryType::Semantic, None, None, None, None, None, None)
        .await
        .unwrap();
    svc.store_memory(uid, "mem subj", MemoryType::Semantic, None, None, None, None, None, Some("s1".to_string()))
        .await
        .unwrap();

    // No subject filter → all memories returned
    let list = svc
        .list_active_filtered_on_branch(
            uid,
            None,
            ListActiveOptions {
                limit: 20,
                memory_type: None,
                session_id: None,
                trust_tier: None,
                cursor: None,
                subject_id: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(list.len(), 2, "no subject filter must return all 2 memories");
    println!("✅ no subject_id filter returns all memories");
}

// ── Test 4: memory_types filter in RetrieveOptions ───────────────────────────

#[tokio::test]
async fn test_memory_types_filter_in_retrieve() {
    let svc = make_service();
    let uid = "types-user";

    svc.store_memory(uid, "semantic content", MemoryType::Semantic, None, None, None, None, None, None)
        .await
        .unwrap();
    svc.store_memory(uid, "profile content", MemoryType::Profile, None, None, None, None, None, None)
        .await
        .unwrap();
    svc.store_memory(uid, "working content", MemoryType::Working, None, None, None, None, None, None)
        .await
        .unwrap();

    // Retrieve with only Semantic + Profile
    let opts = RetrieveOptions::from_session_scope(None, None)
        .with_memory_types(Some(vec![MemoryType::Semantic, MemoryType::Profile]));

    let results = svc
        .retrieve_with_options_on_branch(uid, None, "content", 10, &opts)
        .await
        .unwrap();

    // All returned memories must be Semantic or Profile
    for m in &results {
        assert!(
            matches!(m.memory_type, MemoryType::Semantic | MemoryType::Profile),
            "unexpected memory_type: {:?} for content: {}",
            m.memory_type,
            m.content
        );
    }
    assert!(
        !results.is_empty(),
        "should return at least some matching memories"
    );
    println!("✅ memory_types filter in retrieve");
}

// ── Test 5: subject_id + memory_types composite filter ───────────────────────

#[tokio::test]
async fn test_subject_and_memory_types_combined() {
    let svc = make_service();
    let uid = "combo-user";
    let subject = "user-99";

    svc.store_memory(uid, "subject semantic", MemoryType::Semantic, None, None, None, None, None, Some(subject.to_string()))
        .await
        .unwrap();
    svc.store_memory(uid, "subject profile", MemoryType::Profile, None, None, None, None, None, Some(subject.to_string()))
        .await
        .unwrap();
    svc.store_memory(uid, "subject working", MemoryType::Working, None, None, None, None, None, Some(subject.to_string()))
        .await
        .unwrap();
    // Another subject's memory with the same type — should NOT appear
    svc.store_memory(uid, "other subject semantic", MemoryType::Semantic, None, None, None, None, None, Some("other-subject".to_string()))
        .await
        .unwrap();

    let opts = RetrieveOptions::from_session_scope(None, None)
        .with_subject_id(Some(subject.to_string()))
        .with_memory_types(Some(vec![MemoryType::Semantic]));

    let results = svc
        .retrieve_with_options_on_branch(uid, None, "subject", 10, &opts)
        .await
        .unwrap();

    for m in &results {
        assert_eq!(
            m.subject_id.as_deref(),
            Some(subject),
            "must belong to correct subject"
        );
        assert_eq!(m.memory_type, MemoryType::Semantic, "must be Semantic type");
    }
    println!("✅ subject_id + memory_types composite filter");
}

// ── Test 6: subject_id is persisted and returned in response ─────────────────

#[tokio::test]
async fn test_subject_id_round_trip() {
    let svc = make_service();
    let uid = "rt-user";
    let subject = "subject-roundtrip";

    let m = svc
        .store_memory(
            uid,
            "round trip content",
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

    assert_eq!(
        m.subject_id.as_deref(),
        Some(subject),
        "subject_id must be returned in the stored memory"
    );

    let fetched = svc.get(&m.memory_id).await.unwrap().unwrap();
    assert_eq!(
        fetched.subject_id.as_deref(),
        Some(subject),
        "subject_id must be retrievable by memory_id"
    );
    println!("✅ subject_id round-trip: store and get");
}

// ── Test 7: backward compatibility — callers without subject_id still work ────

#[tokio::test]
async fn test_backward_compat_no_subject_id() {
    let svc = make_service();
    let uid = "compat-user";

    // Use the old-style call without subject_id
    let m = svc
        .store_memory(
            uid,
            "compat content",
            MemoryType::Semantic,
            None,
            None,
            None,
            None,
            None,
            None, // subject_id = None (backward compat)
        )
        .await
        .unwrap();

    assert!(m.subject_id.is_none(), "subject_id must be None");

    // Retrieve without any subject filter — must find the memory
    let results = svc
        .retrieve_with_options_on_branch(uid, None, "compat", 5, &RetrieveOptions::default())
        .await
        .unwrap();

    let found = results.iter().any(|r| r.memory_id == m.memory_id);
    assert!(found, "backward-compat memory must be retrievable without subject filter");
    println!("✅ backward compatibility: no subject_id");
}

// ── Test 8: subject_id length validation (≤128) ──────────────────────────────

#[tokio::test]
async fn test_subject_id_length_validation() {
    let svc = make_service();
    let uid = "len-user";

    // Exactly 128 chars — must succeed
    let ok_id = "x".repeat(128);
    svc.store_memory(uid, "ok len", MemoryType::Semantic, None, None, None, None, None, Some(ok_id))
        .await
        .expect("subject_id of 128 chars should be accepted");

    // 129 chars — must be rejected
    let too_long = "x".repeat(129);
    let err = svc
        .store_memory(uid, "too long", MemoryType::Semantic, None, None, None, None, None, Some(too_long))
        .await;
    assert!(err.is_err(), "subject_id of 129 chars must be rejected");
    println!("✅ subject_id length validation");
}

// ── Test 9: memory_types post-filter excludes unmatched types ─────────────────

#[tokio::test]
async fn test_memory_types_excludes_other_types() {
    let svc = make_service();
    let uid = "excl-user";

    svc.store_memory(uid, "alpha semantic", MemoryType::Semantic, None, None, None, None, None, None)
        .await
        .unwrap();
    svc.store_memory(uid, "alpha episodic", MemoryType::Episodic, None, None, None, None, None, None)
        .await
        .unwrap();
    svc.store_memory(uid, "alpha procedural", MemoryType::Procedural, None, None, None, None, None, None)
        .await
        .unwrap();

    // Filter: only Episodic
    let opts = RetrieveOptions::from_session_scope(None, None)
        .with_memory_types(Some(vec![MemoryType::Episodic]));

    let results = svc
        .retrieve_with_options_on_branch(uid, None, "alpha", 10, &opts)
        .await
        .unwrap();

    assert!(!results.is_empty(), "should find at least one Episodic memory");
    for m in &results {
        assert_eq!(
            m.memory_type,
            MemoryType::Episodic,
            "only Episodic should be returned, got {:?}",
            m.memory_type
        );
    }
    println!("✅ memory_types excludes other types");
}
