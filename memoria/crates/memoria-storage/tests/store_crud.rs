/// CRUD integration tests for SqlMemoryStore against real MatrixOne.
/// Each test uses a unique user_id — safe to run in parallel (default cargo test behavior).
///
/// Run: DATABASE_URL=mysql://root:111@localhost:6001/memoria_test \
///      SQLX_OFFLINE=true cargo test -p memoria-storage --test store_crud -- --nocapture
use chrono::Utc;
use memoria_core::{interfaces::MemoryStore, Memory, MemoryType, TrustTier};
use memoria_storage::SqlMemoryStore;
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

/// Make a test-dim-length vector with `val` at position `idx`, rest zeros.
fn dim_vec(idx: usize, val: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; test_dim()];
    v[idx] = val;
    v
}

async fn setup() -> (SqlMemoryStore, String) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string());
    let instance_id = uuid::Uuid::new_v4().to_string();
    let store = SqlMemoryStore::connect(&url, test_dim(), instance_id)
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    // Unique user_id per test — no cleanup needed, no interference between parallel tests
    let user_id = format!("test_{}", Uuid::new_v4().simple());
    (store, user_id)
}

fn make_memory(id: &str, content: &str, user_id: &str) -> Memory {
    Memory {
        memory_id: id.to_string(),
        user_id: user_id.to_string(),
        memory_type: MemoryType::Semantic,
        content: content.to_string(),
        initial_confidence: 0.8,
        embedding: Some(vec![0.1; test_dim()]),
        source_event_ids: vec!["evt-1".to_string()],
        superseded_by: None,
        is_active: true,
        access_count: 0,
        session_id: Some("sess-1".to_string()),
        observed_at: Some(Utc::now()),
        created_at: None,
        updated_at: None,
        extra_metadata: None,
        trust_tier: TrustTier::T3Inferred,
        retrieval_score: None,
        author_id: None,
        subject_id: None,
    }
}

#[tokio::test]
async fn test_insert_and_get() {
    let (store, uid) = setup().await;
    let id = format!("crud-1-{uid}");
    let m = make_memory(&id, "rust is a systems programming language", &uid);
    store.insert(&m).await.expect("insert");

    let got = store.get(&id).await.expect("get").expect("should exist");
    assert_eq!(got.memory_id, id);
    assert_eq!(got.content, m.content);
    assert_eq!(got.memory_type, MemoryType::Semantic);
    assert_eq!(got.trust_tier, TrustTier::T3Inferred);
    assert!(got.is_active);
    assert_eq!(got.source_event_ids, vec!["evt-1"]);
    let emb = got.embedding.unwrap();
    assert!((emb[0] - 0.1).abs() < 1e-4);
    println!("✅ insert_and_get");
}

#[tokio::test]
async fn test_get_nonexistent() {
    let (store, _) = setup().await;
    let got = store.get("no-such-id").await.expect("get");
    assert!(got.is_none());
    println!("✅ get_nonexistent");
}

#[tokio::test]
async fn test_update() {
    let (store, uid) = setup().await;
    let id = format!("crud-2-{uid}");
    let mut m = make_memory(&id, "original content", &uid);
    store.insert(&m).await.expect("insert");

    m.content = "updated content".to_string();
    m.trust_tier = TrustTier::T2Curated;
    store.update(&m).await.expect("update");

    let got = store.get(&id).await.expect("get").unwrap();
    assert_eq!(got.content, "updated content");
    assert_eq!(got.trust_tier, TrustTier::T2Curated);
    println!("✅ update");
}

#[tokio::test]
async fn test_soft_delete() {
    let (store, uid) = setup().await;
    let id = format!("crud-3-{uid}");
    let m = make_memory(&id, "to be deleted", &uid);
    store.insert(&m).await.expect("insert");

    store.soft_delete(&id).await.expect("soft_delete");

    let got = store.get(&id).await.expect("get");
    assert!(got.is_none());
    println!("✅ soft_delete");
}

#[tokio::test]
async fn test_list_active() {
    let (store, uid) = setup().await;
    for i in 0..3 {
        let m = make_memory(
            &format!("list-{i}-{uid}"),
            &format!("memory number {i}"),
            &uid,
        );
        store.insert(&m).await.expect("insert");
    }
    store
        .soft_delete(&format!("list-1-{uid}"))
        .await
        .expect("soft_delete");

    let results = store.list_active(&uid, 10).await.expect("list_active");
    assert_eq!(results.len(), 2);
    let ids: Vec<&str> = results.iter().map(|m| m.memory_id.as_str()).collect();
    assert!(!ids.iter().any(|id| id.contains("list-1")));
    println!("✅ list_active: {} results", results.len());
}

#[tokio::test]
async fn test_search_fulltext() {
    let (store, uid) = setup().await;
    store
        .insert(&make_memory(
            &format!("ft-1-{uid}"),
            "rust programming language systems performance",
            &uid,
        ))
        .await
        .unwrap();
    store
        .insert(&make_memory(
            &format!("ft-2-{uid}"),
            "python memory service embedding vector search",
            &uid,
        ))
        .await
        .unwrap();

    let results = store
        .search_fulltext(&uid, "rust", 5)
        .await
        .expect("fulltext");
    assert!(!results.is_empty());
    assert!(results[0].content.contains("rust"));
    println!("✅ search_fulltext: top={}", results[0].memory_id);
}

#[tokio::test]
async fn test_fulltext_batches_merge_scores_limit_and_scopes() {
    let (store, uid) = setup().await;
    // Exercise escaping in the text-protocol candidate query and bind-based
    // hydration with the same owner, session, and subject scopes.
    let uid = format!("{uid}'\\owner");
    let session = "sess-'\\1";
    let subject = "batch-'\\subject";
    let terms: Vec<_> = (0..130).map(|i| format!("batchprobe{i:03}")).collect();
    let query = terms.join(" ");
    let all_matches = format!("{} {} {}", terms[0], terms[64], terms[129]);
    let mut overlap = make_memory(&format!("ft-overlap-{uid}"), &all_matches, &uid);
    overlap.session_id = Some(session.into());
    overlap.subject_id = Some(subject.into());
    overlap.extra_metadata = Some(std::collections::HashMap::from([(
        "source".into(),
        serde_json::json!("batch-test"),
    )]));
    let mut first = overlap.clone();
    first.memory_id = format!("ft-first-{uid}");
    first.content = format!("{} irrelevant padding", terms[0]);
    let mut tail = overlap.clone();
    tail.memory_id = format!("ft-tail-{uid}");
    tail.content = format!("{} irrelevant padding", terms[129]);
    // Session-scoped retrieval must also include global memories, and an IN
    // filter must accept every requested type.
    tail.session_id = None;
    tail.memory_type = MemoryType::Profile;
    for memory in [&overlap, &first, &tail] {
        store.insert(memory).await.expect("insert matching memory");
    }
    for excluded in [
        "owner",
        "session",
        "subject",
        "null-subject",
        "type",
        "inactive",
    ] {
        let mut memory = overlap.clone();
        memory.memory_id = format!("ft-x-{excluded}-{uid}");
        match excluded {
            "owner" => memory.user_id = format!("other-{uid}"),
            "session" => memory.session_id = Some("other-session".into()),
            "subject" => memory.subject_id = Some("other-subject".into()),
            "null-subject" => memory.subject_id = None,
            "type" => memory.memory_type = MemoryType::Working,
            "inactive" => {}
            _ => unreachable!(),
        }
        store.insert(&memory).await.expect("insert excluded memory");
        if excluded == "inactive" {
            store
                .soft_delete(&memory.memory_id)
                .await
                .expect("deactivate memory");
        }
    }

    let types = [MemoryType::Semantic, MemoryType::Profile];
    // Obtain actual DB scores from the individual batches. This avoids assuming
    // a particular full-text scoring algorithm or a fixed corpus-wide IDF.
    let mut expected = std::collections::HashMap::<String, f64>::new();
    let mut overlap_scores = Vec::new();
    for batch in terms.chunks(64) {
        let rows = store
            .search_fulltext_from_scoped(
                "mem_memories",
                &uid,
                &batch.join(" "),
                2,
                Some(session),
                Some(subject),
                Some(&types),
            )
            .await
            .expect("single batch");
        for memory in rows {
            let score = memory.retrieval_score.expect("lexical score");
            assert!(score > 0.0);
            if memory.memory_id == overlap.memory_id {
                overlap_scores.push(score);
            }
            *expected.entry(memory.memory_id).or_default() += score;
        }
    }
    assert_eq!(overlap_scores.len(), 3, "one memory must match all batches");
    assert_eq!(expected.len(), 3, "candidate union must exceed final limit");
    let mut expected: Vec<_> = expected.into_iter().collect();
    expected.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    expected.truncate(2);

    let results = store
        .search_fulltext_from_scoped(
            "mem_memories",
            &uid,
            &query,
            2,
            Some(session),
            Some(subject),
            Some(&types),
        )
        .await
        .expect("merged fulltext search");
    assert_eq!(
        results.len(),
        2,
        "limit applies after merging and deduplication"
    );
    assert_eq!(results[0].memory_id, overlap.memory_id);
    for (memory, (id, score)) in results.iter().zip(&expected) {
        assert_eq!(&memory.memory_id, id);
        assert!((memory.retrieval_score.unwrap() - score).abs() < 1e-6);
        assert_eq!(memory.user_id, uid);
        assert_eq!(memory.subject_id.as_deref(), Some(subject));
        assert!(memory.session_id.is_none() || memory.session_id.as_deref() == Some(session));
        assert!(types.contains(&memory.memory_type));
        assert!(memory.is_active);
        assert_eq!(memory.source_event_ids, overlap.source_event_ids);
        assert_eq!(memory.extra_metadata, overlap.extra_metadata);
        let embedding = memory
            .embedding
            .as_ref()
            .expect("final rows include embeddings");
        assert_eq!(embedding.len(), test_dim());
        assert!((embedding[0] - 0.1).abs() < 1e-4);
    }
    assert_eq!(results[0].content, all_matches);

    // Without optional filters, the multi-batch hydration must retain the
    // session/subject/type variants while still excluding other owners/deletes.
    let unscoped = store
        .search_fulltext(&uid, &query, 20)
        .await
        .expect("unscoped merge");
    assert_eq!(unscoped.len(), 7);
    assert!(unscoped.iter().all(|m| m.user_id == uid && m.is_active));
    assert!(store
        .search_fulltext(&uid, &query, 0)
        .await
        .unwrap()
        .is_empty());
    assert!(store
        .search_fulltext(&uid, "____ !@", 2)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn test_fulltext_batches_empty_owner_short_circuits() {
    let (store, uid) = setup().await;
    let table = format!("ft_empty_{}", Uuid::new_v4().simple());
    // Deliberately omit content and the full-text index: any attempted batch
    // query fails, so this verifies the early return rather than just no hits.
    sqlx::query(&format!(
        "CREATE TABLE {table} (user_id VARCHAR(128), is_active BOOLEAN)"
    ))
    .execute(store.pool())
    .await
    .unwrap();
    sqlx::query(&format!(
        "INSERT INTO {table} (user_id, is_active) VALUES (?, 0), (?, 1)"
    ))
    .bind(&uid)
    .bind(format!("other-{uid}"))
    .execute(store.pool())
    .await
    .unwrap();
    let query = (0..65)
        .map(|i| format!("emptyprobe{i}"))
        .collect::<Vec<_>>()
        .join(" ");
    let result = store.search_fulltext_from(&table, &uid, &query, 5).await;
    sqlx::query(&format!("DROP TABLE {table}"))
        .execute(store.pool())
        .await
        .unwrap();
    assert!(result
        .expect("empty owner must skip full-text SQL")
        .is_empty());
}

#[tokio::test]
async fn test_fulltext_case_insensitive_single_batch() {
    let (store, uid) = setup().await;
    let store = store
        .spawn_background_store(1)
        .await
        .expect("single-connection store");
    let memory = make_memory(&format!("ft-case-{uid}"), "MiXeDCaSePrObE", &uid);
    store.insert(&memory).await.unwrap();
    // Query MatrixOne directly as well as the public path: the latter lowercases
    // its input and alone cannot prove the database's own matching semantics.
    for query in ["mixedcaseprobe", "MIXEDCASEPROBE", "MiXeDCaSePrObE"] {
        let ids: Vec<String> = sqlx::query_scalar(&format!(
            "SELECT memory_id FROM mem_memories WHERE user_id = ? \
             AND MATCH(content) AGAINST('{query}' IN BOOLEAN MODE)"
        ))
        .bind(&uid)
        .fetch_all(store.pool())
        .await
        .expect("raw MATCH");
        assert_eq!(ids, vec![memory.memory_id.clone()]);
        let results = store.search_fulltext(&uid, query, 5).await.unwrap();
        assert_eq!(
            results.len(),
            1,
            "public search for {query}; raw IDs: {ids:?}"
        );
        assert_eq!(results[0].memory_id, memory.memory_id);
        assert!(results[0].retrieval_score.unwrap() > 0.0);
    }
}

#[tokio::test]
async fn test_search_vector() {
    let (store, uid) = setup().await;

    // 清理该用户的旧数据（避免维度不匹配）
    sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await
        .unwrap();

    let mut m1 = make_memory(&format!("vec-1-{uid}"), "close to query", &uid);
    m1.embedding = Some(dim_vec(0, 1.0));
    let mut m2 = make_memory(&format!("vec-2-{uid}"), "far from query", &uid);
    m2.embedding = Some(dim_vec(1, 1.0));

    store.insert(&m1).await.unwrap();
    store.insert(&m2).await.unwrap();

    let query = dim_vec(0, 1.0);
    let results = store
        .search_vector(&uid, &query, 2)
        .await
        .expect("vector search");

    assert!(!results.is_empty(), "Expected vector search results");
    assert!(results[0].memory_id.contains("vec-1"));
    println!("✅ search_vector: nearest={}", results[0].memory_id);
}

#[tokio::test]
async fn test_search_vector_sets_retrieval_score() {
    let (store, uid) = setup().await;

    sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await
        .unwrap();

    let mut near = make_memory(&format!("vec-score-1-{uid}"), "near query", &uid);
    near.embedding = Some(dim_vec(0, 1.0));
    let mut far = make_memory(&format!("vec-score-2-{uid}"), "far query", &uid);
    far.embedding = Some(dim_vec(1, 1.0));

    store.insert(&near).await.unwrap();
    store.insert(&far).await.unwrap();

    let results = store
        .search_vector(&uid, &dim_vec(0, 1.0), 2)
        .await
        .expect("vector search");

    assert!(!results.is_empty(), "Expected vector search results");
    let top_score = results[0]
        .retrieval_score
        .expect("vector search should propagate similarity score");
    assert!(
        top_score > 0.0,
        "retrieval_score should reflect vector similarity, got {top_score}"
    );
    println!("✅ search_vector_sets_retrieval_score: top_score={top_score:.4}");
}

#[tokio::test]
async fn test_search_vector_from_filtered_scoped_prefilters_by_session() {
    let (store, uid) = setup().await;

    sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await
        .unwrap();

    let mk_vec = |x: f32, y: f32, z: f32| {
        let mut v = vec![0.0; test_dim()];
        v[0] = x;
        v[1] = y;
        v[2] = z;
        v
    };

    let mut target = make_memory(&format!("vec-scope-1-{uid}"), "target session", &uid);
    target.session_id = Some("sess-target".to_string());
    target.embedding = Some(mk_vec(0.0, 1.0, 0.0));

    let mut other = make_memory(&format!("vec-scope-2-{uid}"), "other session", &uid);
    other.session_id = Some("sess-other".to_string());
    other.embedding = Some(mk_vec(1.0, 0.0, 0.0));

    let mut global_mem = make_memory(&format!("vec-scope-3-{uid}"), "global session", &uid);
    global_mem.session_id = None;
    global_mem.embedding = Some(mk_vec(0.7, 0.0, 0.7));

    store.insert(&target).await.unwrap();
    store.insert(&other).await.unwrap();
    store.insert(&global_mem).await.unwrap();

    let global_results = store
        .search_vector_from_filtered_scoped(
            "mem_memories",
            &uid,
            &mk_vec(1.0, 0.0, 0.0),
            1,
            None,
            None,
            None,
            None,
        )
        .await
        .expect("global vector search");
    assert_eq!(
        global_results.first().map(|m| m.memory_id.as_str()),
        Some(other.memory_id.as_str()),
        "global search should return the nearest cross-session memory",
    );

    let scoped = store
        .search_vector_from_filtered_scoped(
            "mem_memories",
            &uid,
            &mk_vec(1.0, 0.0, 0.0),
            1,
            None,
            Some("sess-target"),
            None,
            None,
        )
        .await
        .expect("session-scoped vector search");
    assert_eq!(
        scoped.len(),
        1,
        "strict session search should still return a candidate"
    );
    assert_eq!(
        scoped.first().map(|m| m.memory_id.as_str()),
        Some(global_mem.memory_id.as_str()),
        "strict session search should rank within the target-session plus unscoped candidate set",
    );
}

#[tokio::test]
async fn test_search_vector_from_filtered_scoped_fills_limit_with_session_candidates() {
    let (store, uid) = setup().await;

    let mk_vec = |x: f32, y: f32, z: f32| {
        let mut v = vec![0.0; test_dim()];
        v[0] = x;
        v[1] = y;
        v[2] = z;
        v
    };

    let mut target_a = make_memory(&format!("vec-scope-fill-1-{uid}"), "scoped a", &uid);
    target_a.session_id = Some("sess-target".to_string());
    target_a.embedding = Some(mk_vec(0.95, 0.2, 0.0));

    let mut target_b = make_memory(&format!("vec-scope-fill-2-{uid}"), "scoped b", &uid);
    target_b.session_id = Some("sess-target".to_string());
    target_b.embedding = Some(mk_vec(0.7, 0.7, 0.0));

    let mut other_a = make_memory(&format!("vec-scope-fill-3-{uid}"), "global a", &uid);
    other_a.session_id = Some("sess-other".to_string());
    other_a.embedding = Some(mk_vec(0.0, 1.0, 0.0));

    let mut other_b = make_memory(&format!("vec-scope-fill-4-{uid}"), "global b", &uid);
    other_b.session_id = Some("sess-other".to_string());
    other_b.embedding = Some(mk_vec(0.0, 0.0, 1.0));

    let mut global = make_memory(&format!("vec-scope-fill-5-{uid}"), "unscoped shared", &uid);
    global.session_id = None;
    global.embedding = Some(mk_vec(0.6, 0.0, 0.8));

    store.insert(&target_a).await.unwrap();
    store.insert(&target_b).await.unwrap();
    store.insert(&other_a).await.unwrap();
    store.insert(&other_b).await.unwrap();
    store.insert(&global).await.unwrap();

    let scoped = store
        .search_vector_from_filtered_scoped(
            "mem_memories",
            &uid,
            &mk_vec(1.0, 0.0, 0.0),
            3,
            None,
            Some("sess-target"),
            None,
            None,
        )
        .await
        .expect("session-scoped vector search");
    assert_eq!(
        scoped.len(),
        3,
        "strict session search should fill top_k from the target-session plus unscoped candidate set",
    );
    let scoped_ids: std::collections::HashSet<&str> =
        scoped.iter().map(|m| m.memory_id.as_str()).collect();
    assert_eq!(
        scoped_ids,
        std::collections::HashSet::from([
            target_a.memory_id.as_str(),
            target_b.memory_id.as_str(),
            global.memory_id.as_str(),
        ]),
        "strict session vector search should return session-local plus unscoped candidates",
    );
}

#[tokio::test]
async fn test_search_hybrid_access_count_stays_tiebreaker() {
    let (store, uid) = setup().await;

    let mut relevant = make_memory(&format!("hybrid-relevant-{uid}"), "fresh relevant", &uid);
    relevant.embedding = Some(dim_vec(0, 1.0));

    let mut popular = make_memory(&format!("hybrid-popular-{uid}"), "stale popular", &uid);
    popular.embedding = Some(dim_vec(0, 0.8));

    store.insert(&relevant).await.unwrap();
    store.insert(&popular).await.unwrap();

    sqlx::query("INSERT INTO mem_memories_stats (memory_id, access_count) VALUES (?, 40)")
        .bind(&popular.memory_id)
        .execute(store.pool())
        .await
        .unwrap();

    let (results, _) = store
        .search_hybrid_from_scored("mem_memories", &uid, &dim_vec(0, 1.0), "zzzzzz", 2, 0.0)
        .await
        .expect("hybrid search");

    assert_eq!(
        results.first().map(|m| m.memory_id.as_str()),
        Some(relevant.memory_id.as_str()),
        "access_count should not outrank the stronger semantic match"
    );
}

// ── Field round-trip: every column written and read back ─────────────────────

#[tokio::test]
async fn test_all_fields_round_trip() {
    let (store, uid) = setup().await;
    let id = format!("rt-{uid}");
    let now = Utc::now();

    let mut meta = std::collections::HashMap::new();
    meta.insert("key".to_string(), serde_json::json!("value"));
    meta.insert("num".to_string(), serde_json::json!(42));

    let m = Memory {
        memory_id: id.clone(),
        user_id: uid.clone(),
        memory_type: MemoryType::Profile,
        content: "full field test".to_string(),
        initial_confidence: 0.91,
        embedding: Some(vec![0.1; test_dim()]),
        source_event_ids: vec!["e1".to_string(), "e2".to_string()],
        superseded_by: Some("other-id".to_string()),
        is_active: true,
        access_count: 0,
        session_id: Some("sess-xyz".to_string()),
        observed_at: Some(now),
        created_at: None,
        updated_at: None,
        extra_metadata: Some(meta),
        trust_tier: TrustTier::T1Verified,
        retrieval_score: None,
        author_id: None,
        subject_id: None,
    };
    store.insert(&m).await.expect("insert");

    let got = store.get(&id).await.expect("get").expect("exists");

    assert_eq!(got.memory_type, MemoryType::Profile);
    assert_eq!(got.trust_tier, TrustTier::T1Verified);
    assert!(
        (got.initial_confidence - 0.91).abs() < 1e-4,
        "initial_confidence mismatch"
    );
    assert_eq!(got.session_id.as_deref(), Some("sess-xyz"));
    assert_eq!(got.superseded_by.as_deref(), Some("other-id"));
    assert_eq!(got.source_event_ids, vec!["e1", "e2"]);

    let emb = got.embedding.as_ref().expect("embedding");
    assert!((emb[0] - 0.1).abs() < 1e-4);
    assert!((emb[test_dim() - 1] - 0.1).abs() < 1e-4);

    let meta = got.extra_metadata.as_ref().expect("extra_metadata");
    assert_eq!(meta["key"], serde_json::json!("value"));
    assert_eq!(meta["num"], serde_json::json!(42));

    // observed_at round-trips within 1 second
    let got_ts = got.observed_at.expect("observed_at").timestamp();
    assert!(
        (got_ts - now.timestamp()).abs() <= 1,
        "observed_at mismatch"
    );

    // created_at is set by DB
    assert!(got.created_at.is_some(), "created_at should be set by DB");

    println!("✅ all_fields_round_trip: all columns verified");
}

// ── All MemoryType variants ───────────────────────────────────────────────────

#[tokio::test]
async fn test_all_memory_types() {
    let (store, uid) = setup().await;
    let types = [
        MemoryType::Semantic,
        MemoryType::Profile,
        MemoryType::Procedural,
        MemoryType::Working,
        MemoryType::ToolResult,
        MemoryType::Episodic,
    ];
    for mt in &types {
        let id = format!("mt-{mt:?}-{uid}");
        let mut m = make_memory(&id, &format!("type test {mt:?}"), &uid);
        m.memory_type = mt.clone();
        store.insert(&m).await.expect("insert");
        let got = store.get(&id).await.expect("get").expect("exists");
        assert_eq!(&got.memory_type, mt, "memory_type mismatch for {mt:?}");
    }
    println!("✅ all_memory_types: {} variants", types.len());
}

// ── All TrustTier variants ────────────────────────────────────────────────────

#[tokio::test]
async fn test_all_trust_tiers() {
    let (store, uid) = setup().await;
    let tiers = [
        TrustTier::T1Verified,
        TrustTier::T2Curated,
        TrustTier::T3Inferred,
        TrustTier::T4Unverified,
    ];
    for tier in &tiers {
        let id = format!("tt-{tier:?}-{uid}");
        let mut m = make_memory(&id, &format!("tier test {tier:?}"), &uid);
        m.trust_tier = tier.clone();
        store.insert(&m).await.expect("insert");
        let got = store.get(&id).await.expect("get").expect("exists");
        assert_eq!(&got.trust_tier, tier, "trust_tier mismatch for {tier:?}");
    }
    println!("✅ all_trust_tiers: {} variants", tiers.len());
}

// ── NULL optional fields ──────────────────────────────────────────────────────

#[tokio::test]
async fn test_null_optional_fields() {
    let (store, uid) = setup().await;
    let id = format!("null-{uid}");
    let m = Memory {
        memory_id: id.clone(),
        user_id: uid.clone(),
        memory_type: MemoryType::Semantic,
        content: "null fields test".to_string(),
        initial_confidence: 0.5,
        embedding: None, // NULL vecf32
        source_event_ids: vec![],
        superseded_by: None,
        is_active: true,
        access_count: 0,
        session_id: None,
        observed_at: None,
        created_at: None,
        updated_at: None,
        extra_metadata: None, // NULL JSON
        trust_tier: TrustTier::T3Inferred,
        retrieval_score: None,
        author_id: None,
        subject_id: None,
    };
    store.insert(&m).await.expect("insert with nulls");

    let got = store.get(&id).await.expect("get").expect("exists");
    assert!(got.embedding.is_none(), "embedding should be NULL");
    assert!(got.session_id.is_none(), "session_id should be NULL");
    assert!(got.superseded_by.is_none(), "superseded_by should be NULL");
    assert!(
        got.extra_metadata.is_none(),
        "extra_metadata should be NULL"
    );
    assert_eq!(got.source_event_ids, Vec::<String>::new());
    println!("✅ null_optional_fields: all NULLs round-trip correctly");
}

#[tokio::test]
async fn test_insert_normalizes_empty_nullable_strings() {
    let (store, uid) = setup().await;
    let id = format!("empty-nullable-{uid}");
    let mut memory = make_memory(&id, "empty nullable strings", &uid);
    memory.session_id = Some(String::new());
    memory.superseded_by = Some(String::new());

    store
        .insert(&memory)
        .await
        .expect("insert empty nullable strings");

    let stored: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT session_id, superseded_by FROM mem_memories WHERE memory_id = ?",
    )
    .bind(&id)
    .fetch_one(store.pool())
    .await
    .expect("read raw nullable strings");
    assert_eq!(stored, (None, None), "empty strings must be stored as NULL");
}

#[tokio::test]
async fn test_session_id_recovers_after_null_insert_on_same_connection() {
    let (store, uid) = setup().await;
    // Force all three INSERTs through one physical connection. MatrixOne
    // matrixorigin/matrixone#26874 retained a prepared parameter's NULL state
    // on statement reuse.
    let store = store
        .spawn_background_store(1)
        .await
        .expect("single-connection store");

    let before_id = format!("session-before-{uid}");
    let null_id = format!("session-null-{uid}");
    let after_id = format!("session-after-{uid}");

    let mut before = make_memory(&before_id, "session before null", &uid);
    before.session_id = Some("sess-before".to_string());
    store.insert(&before).await.expect("insert before NULL");

    let mut unscoped = make_memory(&null_id, "unscoped memory", &uid);
    unscoped.session_id = None;
    store.insert(&unscoped).await.expect("insert NULL session");

    let mut after = make_memory(&after_id, "session after null", &uid);
    after.session_id = Some("sess-after".to_string());
    store.insert(&after).await.expect("insert after NULL");

    let before = store.get(&before_id).await.expect("get before").unwrap();
    let unscoped = store.get(&null_id).await.expect("get unscoped").unwrap();
    let after = store.get(&after_id).await.expect("get after").unwrap();

    assert_eq!(before.session_id.as_deref(), Some("sess-before"));
    assert!(unscoped.session_id.is_none());
    assert_eq!(after.session_id.as_deref(), Some("sess-after"));
}

#[tokio::test]
async fn test_batch_insert_normalizes_empty_nullable_strings() {
    let (store, uid) = setup().await;
    let id = format!("batch-empty-nullable-{uid}");
    let mut memory = make_memory(&id, "batch empty nullable strings", &uid);
    memory.session_id = Some(String::new());
    memory.superseded_by = Some(String::new());

    store
        .batch_insert_into("mem_memories", &[&memory])
        .await
        .expect("batch insert empty nullable strings");

    let stored: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT session_id, superseded_by FROM mem_memories WHERE memory_id = ?",
    )
    .bind(&id)
    .fetch_one(store.pool())
    .await
    .expect("read raw batch nullable strings");
    assert_eq!(stored, (None, None), "empty strings must be stored as NULL");
}

// ── insert_entity_links batch optimization tests ─────────────────────────────

#[tokio::test]
async fn test_insert_entity_links_empty() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    let (created, reused) = store.insert_entity_links(&uid, &mid, &[]).await.unwrap();
    assert_eq!(created, 0);
    assert_eq!(reused, 0);
    println!("✅ insert_entity_links: empty input returns (0, 0)");
}

#[tokio::test]
async fn test_insert_entity_links_batch() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    let entities = vec![
        ("Rust".to_string(), "tech".to_string()),
        ("MatrixOne".to_string(), "tech".to_string()),
        ("Memoria".to_string(), "project".to_string()),
    ];
    let (created, reused) = store
        .insert_entity_links(&uid, &mid, &entities)
        .await
        .unwrap();
    assert_eq!(created, 3);
    assert_eq!(reused, 0);

    // Verify all 3 exist in DB
    let rows = sqlx::query("SELECT entity_name FROM mem_entity_links WHERE user_id = ? AND memory_id = ? ORDER BY entity_name")
        .bind(&uid).bind(&mid)
        .fetch_all(store.pool()).await.unwrap();
    let names: Vec<String> = rows
        .iter()
        .map(|r| sqlx::Row::get::<String, _>(r, "entity_name"))
        .collect();
    assert_eq!(names, vec!["matrixone", "memoria", "rust"]);
    println!("✅ insert_entity_links: batch of 3 inserted in single statement");
}

#[tokio::test]
async fn test_insert_entity_links_dedup_existing() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    let entities = vec![
        ("Rust".to_string(), "tech".to_string()),
        ("Go".to_string(), "tech".to_string()),
    ];
    let (created, _) = store
        .insert_entity_links(&uid, &mid, &entities)
        .await
        .unwrap();
    assert_eq!(created, 2);

    // Insert again with overlap + new
    let entities2 = vec![
        ("Rust".to_string(), "tech".to_string()),   // existing
        ("Python".to_string(), "tech".to_string()), // new
    ];
    let (created2, reused2) = store
        .insert_entity_links(&uid, &mid, &entities2)
        .await
        .unwrap();
    assert_eq!(created2, 1, "only Python should be new");
    assert_eq!(reused2, 1, "Rust should be reused");

    let rows = sqlx::query(
        "SELECT COUNT(*) as cnt FROM mem_entity_links WHERE user_id = ? AND memory_id = ?",
    )
    .bind(&uid)
    .bind(&mid)
    .fetch_one(store.pool())
    .await
    .unwrap();
    let cnt: i64 = sqlx::Row::get(&rows, "cnt");
    assert_eq!(cnt, 3, "total should be 3 (rust, go, python)");
    println!("✅ insert_entity_links: dedup existing entities correctly");
}

#[tokio::test]
async fn test_insert_entity_links_dedup_within_batch() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    // Same entity name twice in one batch (different case)
    let entities = vec![
        ("Rust".to_string(), "tech".to_string()),
        ("rust".to_string(), "tech".to_string()),
        ("RUST".to_string(), "tech".to_string()),
    ];
    let (created, reused) = store
        .insert_entity_links(&uid, &mid, &entities)
        .await
        .unwrap();
    assert_eq!(created, 1, "only one 'rust' should be inserted");
    assert_eq!(reused, 2, "two duplicates within batch");
    println!("✅ insert_entity_links: dedup within batch (case-insensitive)");
}

#[tokio::test]
async fn test_insert_entity_links_case_insensitive() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    let entities = vec![("MatrixOne".to_string(), "tech".to_string())];
    store
        .insert_entity_links(&uid, &mid, &entities)
        .await
        .unwrap();

    // Query back — should be lowercased
    let rows =
        sqlx::query("SELECT entity_name FROM mem_entity_links WHERE user_id = ? AND memory_id = ?")
            .bind(&uid)
            .bind(&mid)
            .fetch_all(store.pool())
            .await
            .unwrap();
    let name: String = sqlx::Row::get(&rows[0], "entity_name");
    assert_eq!(name, "matrixone", "entity name should be lowercased");
    println!("✅ insert_entity_links: names stored as lowercase");
}

#[tokio::test]
async fn test_insert_entity_links_large_batch_chunking() {
    let (store, uid) = setup().await;
    let mid = Uuid::new_v4().simple().to_string();
    // 120 entities — should be split into 3 chunks (50+50+20)
    let entities: Vec<(String, String)> = (0..120)
        .map(|i| (format!("entity_{i}"), "concept".to_string()))
        .collect();
    let (created, reused) = store
        .insert_entity_links(&uid, &mid, &entities)
        .await
        .unwrap();
    assert_eq!(created, 120);
    assert_eq!(reused, 0);

    let rows = sqlx::query(
        "SELECT COUNT(*) as cnt FROM mem_entity_links WHERE user_id = ? AND memory_id = ?",
    )
    .bind(&uid)
    .bind(&mid)
    .fetch_one(store.pool())
    .await
    .unwrap();
    let cnt: i64 = sqlx::Row::get(&rows, "cnt");
    assert_eq!(cnt, 120);
    println!("✅ insert_entity_links: 120 entities chunked correctly");
}

#[tokio::test]
async fn test_list_active_lite() {
    let (store, uid) = setup().await;
    // Insert 3 memories with embeddings
    for i in 0..3 {
        let mut m = make_memory(
            &format!("lite-{i}-{uid}"),
            &format!("lite memory {i}"),
            &uid,
        );
        if i == 0 {
            m.extra_metadata = Some(std::collections::HashMap::from([(
                "mapper".to_string(),
                serde_json::json!("lite"),
            )]));
        }
        store.insert(&m).await.expect("insert");
    }
    store
        .soft_delete(&format!("lite-1-{uid}"))
        .await
        .expect("soft_delete");

    let results = store
        .list_active_lite("mem_memories", &uid, 10, None, None, None, None, None)
        .await
        .expect("list_active_lite");
    assert_eq!(results.len(), 2, "should exclude soft-deleted");
    // lite results must NOT carry embedding or source_event_ids
    for m in &results {
        assert!(m.embedding.is_none(), "lite should skip embedding");
        assert!(
            m.source_event_ids.is_empty(),
            "lite should skip source_event_ids"
        );
        assert!(!m.content.is_empty(), "content must be present");
    }
    let metadata_memory = results
        .iter()
        .find(|m| m.memory_id == format!("lite-0-{uid}"))
        .expect("metadata memory");
    assert_eq!(
        metadata_memory
            .extra_metadata
            .as_ref()
            .and_then(|metadata| metadata.get("mapper")),
        Some(&serde_json::json!("lite")),
        "lite mapper must preserve extra_metadata"
    );
    // Verify ordering: newest first
    assert!(results[0].created_at >= results[1].created_at);
    println!(
        "✅ list_active_lite: {} results, no embedding",
        results.len()
    );
}

#[tokio::test]
async fn test_list_active_lite_limit_cap() {
    let (store, uid) = setup().await;
    for i in 0..5 {
        let m = make_memory(&format!("cap-{i}-{uid}"), &format!("cap memory {i}"), &uid);
        store.insert(&m).await.expect("insert");
    }
    // Request limit=2, should only get 2
    let results = store
        .list_active_lite("mem_memories", &uid, 2, None, None, None, None, None)
        .await
        .expect("list_active_lite");
    assert_eq!(results.len(), 2, "should respect limit");

    // Request absurdly large limit — capped at 500 internally
    let results = store
        .list_active_lite("mem_memories", &uid, 999999, None, None, None, None, None)
        .await
        .expect("list_active_lite");
    assert!(results.len() <= 501, "should cap at 501");
    println!("✅ list_active_lite_limit_cap");
}
