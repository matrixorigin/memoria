/// Service layer unit tests using in-memory mock store.
use async_trait::async_trait;
use memoria_core::{
    interfaces::{EmbeddingProvider, MemoryStore},
    MemoriaError, Memory, MemoryType, TrustTier,
};
use memoria_service::MemoryService;
use memoria_storage::OwnedEditLogEntry;
use std::sync::{Arc, Mutex};

// ── Mock store ────────────────────────────────────────────────────────────────

#[derive(Default)]
struct MockStore {
    memories: Mutex<Vec<Memory>>,
}

#[async_trait]
impl MemoryStore for MockStore {
    async fn insert(&self, memory: &Memory) -> Result<(), MemoriaError> {
        self.memories.lock().unwrap().push(memory.clone());
        Ok(())
    }
    async fn get(&self, memory_id: &str) -> Result<Option<Memory>, MemoriaError> {
        Ok(self
            .memories
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.memory_id == memory_id && m.is_active)
            .cloned())
    }
    async fn get_including_inactive(
        &self,
        memory_id: &str,
    ) -> Result<Option<Memory>, MemoriaError> {
        Ok(self
            .memories
            .lock()
            .unwrap()
            .iter()
            .find(|m| m.memory_id == memory_id)
            .cloned())
    }
    async fn update(&self, memory: &Memory) -> Result<(), MemoriaError> {
        let mut store = self.memories.lock().unwrap();
        if let Some(m) = store.iter_mut().find(|m| m.memory_id == memory.memory_id) {
            *m = memory.clone();
        }
        Ok(())
    }
    async fn soft_delete(&self, memory_id: &str) -> Result<(), MemoriaError> {
        let mut store = self.memories.lock().unwrap();
        if let Some(m) = store.iter_mut().find(|m| m.memory_id == memory_id) {
            m.is_active = false;
        }
        Ok(())
    }
    async fn list_active(&self, user_id: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError> {
        Ok(self
            .memories
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.user_id == user_id && m.is_active)
            .take(limit as usize)
            .cloned()
            .collect())
    }
    async fn search_fulltext(
        &self,
        user_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        Ok(self
            .memories
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.user_id == user_id && m.is_active && m.content.contains(query))
            .take(limit as usize)
            .cloned()
            .collect())
    }
    async fn search_vector(
        &self,
        _user_id: &str,
        _embedding: &[f32],
        _limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError> {
        Ok(vec![]) // mock: no vector search, falls back to fulltext
    }
}

// ── Mock embedder ─────────────────────────────────────────────────────────────

struct MockEmbedder;

#[async_trait]
impl EmbeddingProvider for MockEmbedder {
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, MemoriaError> {
        Ok(vec![0.1, 0.2, 0.3, 0.4])
    }
    fn dimension(&self) -> usize {
        4
    }
}

fn make_service() -> MemoryService {
    MemoryService::new(
        Arc::new(MockStore::default()),
        Some(Arc::new(MockEmbedder)),
        None,
    )
}

#[tokio::test]
async fn test_observe_exclusions_keep_explicit_fact_and_capture_other_fact() {
    let (llm, _shutdown) = memoria_test_utils::spawn_fake_llm(vec![(
        "already_saved_memories",
        serde_json::json!([
            {"type": "profile", "content": "User likes rainy days", "confidence": 1.0},
            {"type": "profile", "content": "用户喝无糖咖啡", "confidence": 1.0}
        ]),
    )])
    .await;
    let mut svc = make_service();
    svc.llm = Some(llm);
    let saved = svc
        .store_memory(
            "u1",
            "User likes rainy days",
            MemoryType::Profile,
            Some("session".into()),
            None,
            None,
            None,
            None,
            Some("subject".into()),
        )
        .await
        .unwrap();
    let (captured, has_llm) = svc
        .observe_turn_excluding_on_branch(
            "u1",
            None,
            &[serde_json::json!({"role": "user", "content": "我喜欢下雨天，也喝无糖咖啡"})],
            Some("session".into()),
            Some("subject".into()),
            std::slice::from_ref(&saved.memory_id),
        )
        .await
        .unwrap();
    assert!(has_llm);
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].content, "用户喝无糖咖啡");
    assert_eq!(captured[0].trust_tier, TrustTier::T3Inferred);
    let original = svc.get(&saved.memory_id).await.unwrap().unwrap();
    assert_eq!(original.trust_tier, TrustTier::T1Verified);
    assert!(original.is_active);
    assert_eq!(svc.list_active("u1", 20).await.unwrap().len(), 2);
}

#[tokio::test]
async fn test_observe_exclusions_never_fall_back_to_raw_storage() {
    for malformed_llm in [false, true] {
        let mut svc = make_service();
        let saved = svc
            .store_memory(
                "u1",
                "User likes rainy days",
                MemoryType::Profile,
                None,
                None,
                None,
                None,
                None,
                Some("subject".into()),
            )
            .await
            .unwrap();
        let mut shutdown = None;
        if malformed_llm {
            let (llm, handle) = memoria_test_utils::spawn_fake_llm(vec![(
                "already_saved_memories",
                serde_json::json!({"invalid": "not an array"}),
            )])
            .await;
            svc.llm = Some(llm);
            shutdown = Some(handle);
        }
        assert!(svc
            .observe_turn_excluding_on_branch(
                "u1",
                None,
                &[serde_json::json!({"role": "user", "content": "我喜欢下雨天"})],
                None,
                Some("subject".into()),
                &[saved.memory_id],
            )
            .await
            .is_err());
        assert_eq!(svc.list_active("u1", 20).await.unwrap().len(), 1);
        drop(shutdown);
    }
}

#[tokio::test]
async fn test_observe_exclusions_ignore_out_of_scope_ids_without_sending_private_content() {
    let (llm, _shutdown) = memoria_test_utils::spawn_fake_llm(vec![
        (
            "private preference",
            serde_json::json!([{"content": "scope leaked"}]),
        ),
        (
            "public other fact",
            serde_json::json!([{"content": "public other fact"}]),
        ),
    ])
    .await;
    let mut svc = make_service();
    svc.llm = Some(llm);
    let saved = svc
        .store_memory(
            "u1",
            "private preference",
            MemoryType::Profile,
            None,
            None,
            None,
            None,
            None,
            Some("alice".into()),
        )
        .await
        .unwrap();
    for (user, subject) in [("u2", "alice"), ("u1", "bob")] {
        let (captured, _) = svc
            .observe_turn_excluding_on_branch(
                user,
                None,
                &[serde_json::json!({"role": "user", "content": "public other fact"})],
                None,
                Some(subject.into()),
                std::slice::from_ref(&saved.memory_id),
            )
            .await
            .unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].content, "public other fact");
    }
    assert_eq!(
        svc.get(&saved.memory_id).await.unwrap().unwrap().content,
        "private preference"
    );
}

#[tokio::test]
async fn test_observe_exclusions_retain_inactive_facts_after_correct_or_purge() {
    for corrected in [true, false] {
        let mut items = vec![
            serde_json::json!({"content": "User likes rainy days"}),
            serde_json::json!({"content": "用户喝无糖咖啡"}),
        ];
        if corrected {
            items.push(serde_json::json!({"content": "User likes sunny days"}));
        }
        let (llm, _shutdown) = memoria_test_utils::spawn_fake_llm(vec![(
            "already_saved_memories",
            serde_json::json!(items),
        )])
        .await;
        let mut svc = make_service();
        svc.llm = Some(llm);
        let saved = svc
            .store_memory(
                "u1",
                "User likes rainy days",
                MemoryType::Profile,
                None,
                None,
                None,
                None,
                None,
                Some("subject".into()),
            )
            .await
            .unwrap();
        let mut excluded = vec![saved.memory_id.clone()];
        if corrected {
            let updated = svc
                .correct("u1", &saved.memory_id, "User likes sunny days")
                .await
                .unwrap();
            excluded.push(updated.memory_id);
        } else {
            svc.purge("u1", &saved.memory_id).await.unwrap();
        }
        assert!(svc.get(&saved.memory_id).await.unwrap().is_none());
        let (captured, _) = svc
            .observe_turn_excluding_on_branch(
                "u1",
                None,
                &[],
                None,
                Some("subject".into()),
                &excluded,
            )
            .await
            .unwrap();
        assert_eq!(captured.len(), 1);
        assert_eq!(captured[0].content, "用户喝无糖咖啡");
        assert!(svc.get(&saved.memory_id).await.unwrap().is_none());
    }
}

#[tokio::test]
async fn test_observe_missing_exclusions_still_forbid_raw_fallback() {
    let (llm, _shutdown) = memoria_test_utils::spawn_fake_llm(vec![(
        "new fact",
        serde_json::json!({"invalid": "not an array"}),
    )])
    .await;
    let mut svc = make_service();
    svc.llm = Some(llm);
    let error = svc
        .observe_turn_excluding_on_branch(
            "u1",
            None,
            &[serde_json::json!({"role": "user", "content": "new fact"})],
            None,
            None,
            &["physically-purged-id".into()],
        )
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        MemoriaError::ObserveExtractionUnavailable(_)
    ));
    assert!(svc.list_active("u1", 20).await.unwrap().is_empty());
}

#[tokio::test]
async fn test_observe_exclusions_empty_extraction_does_not_store_raw_chinese_messages() {
    let (llm, _shutdown) =
        memoria_test_utils::spawn_fake_llm(vec![("already_saved_memories", serde_json::json!([]))])
            .await;
    let mut svc = make_service();
    svc.llm = Some(llm);
    let saved = svc
        .store_memory(
            "u1",
            "User likes rainy days",
            MemoryType::Profile,
            None,
            None,
            None,
            None,
            None,
            Some("subject".into()),
        )
        .await
        .unwrap();
    let messages = vec![serde_json::json!({"role": "user", "content": "雨".repeat(500)}); 20];
    let (captured, _) = svc
        .observe_turn_excluding_on_branch(
            "u1",
            None,
            &messages,
            None,
            Some("subject".into()),
            &[saved.memory_id],
        )
        .await
        .unwrap();
    assert!(captured.is_empty());
    assert_eq!(svc.list_active("u1", 20).await.unwrap().len(), 1);
}

#[tokio::test]
async fn test_observe_exclusions_reject_invalid_or_excessive_ids_before_writes() {
    let svc = make_service();
    for ids in [vec!["../bad".into()], vec!["a".repeat(32); 101]] {
        assert!(svc
            .observe_turn_excluding_on_branch("u1", None, &[], None, None, &ids,)
            .await
            .is_err());
    }
    assert!(svc.list_active("u1", 20).await.unwrap().is_empty());
}

#[tokio::test]
async fn test_observe_without_exclusions_preserves_existing_raw_fallback() {
    let svc = make_service();
    let (captured, has_llm) = svc
        .observe_turn_on_branch(
            "u1",
            None,
            &[serde_json::json!({"role": "user", "content": "I like rainy days"})],
            None,
            Some("subject".into()),
        )
        .await
        .unwrap();
    assert!(!has_llm);
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].content, "I like rainy days");
}

fn make_service_with_entries() -> (MemoryService, Arc<Mutex<Vec<OwnedEditLogEntry>>>) {
    MemoryService::new_with_test_entries(
        Arc::new(MockStore::default()),
        Some(Arc::new(MockEmbedder)),
    )
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_store_and_retrieve() {
    let svc = make_service();
    let m = svc
        .store_memory(
            "u1",
            "rust is fast",
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
    assert!(!m.memory_id.is_empty());
    assert_eq!(m.content, "rust is fast");
    assert!(m.embedding.is_some());

    // retrieve falls back to fulltext (mock vector returns empty)
    let results = svc.retrieve("u1", "rust", 5).await.unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0].content, "rust is fast");
    println!("✅ store_and_retrieve");
}

#[tokio::test]
async fn test_correct() {
    let svc = make_service();
    let m = svc
        .store_memory(
            "u1",
            "old content",
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
    let corrected = svc
        .correct("u1", &m.memory_id, "new content")
        .await
        .unwrap();
    assert_eq!(corrected.content, "new content");
    assert!(corrected.embedding.is_some());
    println!("✅ correct");
}

#[tokio::test]
async fn test_purge() {
    let svc = make_service();
    let m = svc
        .store_memory(
            "u1",
            "to delete",
            MemoryType::Working,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    svc.purge("u1", &m.memory_id).await.unwrap();
    let got = svc.get(&m.memory_id).await.unwrap();
    assert!(got.is_none());
    println!("✅ purge");
}

#[tokio::test]
async fn test_list_active_excludes_deleted() {
    let svc = make_service();
    svc.store_memory(
        "u1",
        "keep this",
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
    let del = svc
        .store_memory(
            "u1",
            "delete this",
            MemoryType::Working,
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    svc.purge("u1", &del.memory_id).await.unwrap();

    let list = svc.list_active("u1", 10).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].content, "keep this");
    println!("✅ list_active_excludes_deleted");
}

#[tokio::test]
async fn test_purge_by_session_id_filters_memory_type() {
    let svc = make_service();
    for (content, memory_type, session_id) in [
        (
            "remove working a",
            MemoryType::Working,
            Some("sess-target".to_string()),
        ),
        (
            "remove working b",
            MemoryType::Working,
            Some("sess-target".to_string()),
        ),
        (
            "keep semantic",
            MemoryType::Semantic,
            Some("sess-target".to_string()),
        ),
        (
            "keep other session",
            MemoryType::Working,
            Some("sess-other".to_string()),
        ),
    ] {
        svc.store_memory(
            "u1",
            content,
            memory_type,
            session_id,
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }

    let memory_types = [MemoryType::Working];
    let result = svc
        .purge_by_session_id("u1", "sess-target", Some(&memory_types))
        .await
        .unwrap();
    assert_eq!(result.purged, 2);

    let list = svc.list_active("u1", 10).await.unwrap();
    let contents: Vec<&str> = list.iter().map(|m| m.content.as_str()).collect();
    assert_eq!(list.len(), 2);
    assert!(contents.contains(&"keep semantic"));
    assert!(contents.contains(&"keep other session"));
    println!("✅ purge_by_session_id filters working memories only");
}

#[tokio::test]
async fn test_purge_by_session_id_fallback_is_not_capped() {
    let svc = make_service();
    let target_count = 10_005usize;
    for index in 0..target_count {
        svc.store_memory(
            "u1",
            &format!("target working {index}"),
            MemoryType::Working,
            Some("sess-target".to_string()),
            None,
            None,
            None,
            None,
            None,
        )
        .await
        .unwrap();
    }
    svc.store_memory(
        "u1",
        "keep semantic",
        MemoryType::Semantic,
        Some("sess-target".to_string()),
        None,
        None,
        None,
        None,
        None,
    )
    .await
    .unwrap();

    let memory_types = [MemoryType::Working];
    let result = svc
        .purge_by_session_id("u1", "sess-target", Some(&memory_types))
        .await
        .unwrap();
    assert_eq!(result.purged, target_count);

    let list = svc.list_active("u1", i64::MAX).await.unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0].content, "keep semantic");
    println!("✅ purge_by_session_id fallback scans full active set");
}

#[tokio::test]
async fn test_memory_types() {
    let svc = make_service();
    for mt in [
        MemoryType::Semantic,
        MemoryType::Profile,
        MemoryType::Procedural,
        MemoryType::Working,
        MemoryType::ToolResult,
        MemoryType::Episodic,
    ] {
        let m = svc
            .store_memory("u1", "content", mt.clone(), None, None, None, None, None, None)
            .await
            .unwrap();
        assert_eq!(m.memory_type, mt);
    }
    println!("✅ all 6 memory types");
}

#[tokio::test]
async fn test_trust_tiers() {
    let svc = make_service();
    for (tier, expected_conf) in [
        (TrustTier::T1Verified, 0.95f64),
        (TrustTier::T2Curated, 0.85),
        (TrustTier::T3Inferred, 0.65),
        (TrustTier::T4Unverified, 0.40),
    ] {
        let m = svc
            .store_memory(
                "u1",
                "content",
                MemoryType::Semantic,
                None,
                Some(tier),
                None,
                None,
                None,
                None,
            )
            .await
            .unwrap();
        assert!((m.initial_confidence - expected_conf).abs() < 1e-6);
    }
    println!("✅ all 4 trust tiers");
}

#[tokio::test]
async fn test_no_embedder_still_works() {
    let svc = MemoryService::new(Arc::new(MockStore::default()), None, None);
    let m = svc
        .store_memory(
            "u1",
            "no embedding",
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
    assert!(m.embedding.is_none());
    println!("✅ no_embedder_still_works");
}

#[tokio::test]
async fn test_flush_edit_log_drains_in_memory_buffer() {
    let (svc, entries) = make_service_with_entries();
    svc.send_edit_log("u1", "inject", Some("m1"), Some("{}"), "store_memory", None);

    assert!(
        entries.lock().unwrap().is_empty(),
        "entries should remain buffered until an explicit flush in this test"
    );

    svc.flush_edit_log().await;

    let drained = entries.lock().unwrap();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].user_id, "u1");
    assert_eq!(drained[0].operation, "inject");
    assert_eq!(drained[0].reason, "store_memory");
    println!("✅ flush_edit_log_drains_in_memory_buffer");
}
