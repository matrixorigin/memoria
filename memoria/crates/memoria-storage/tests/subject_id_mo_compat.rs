/// MatrixOne SQL compatibility tests for `subject_id` and `memory_types` filters.
///
/// Exercises the actual SQL paths added for MOI memory subject isolation:
/// - `list_active_lite` with `subject_id` / `memory_type`
/// - `search_fulltext_from_scoped` (bound `memory_type IN (?, ?)`)
/// - `search_vector_from_filtered_scoped` (inlined subject_id + memory_types)
/// - `search_hybrid_from_scored_scoped` (combined vector + fulltext)
///
/// Run:
///   DATABASE_URL=mysql://root:111@localhost:6001/memoria_test \
///   SQLX_OFFLINE=true cargo test -p memoria-storage --test subject_id_mo_compat -- --nocapture
use chrono::Utc;
use memoria_core::{interfaces::MemoryStore, Memory, MemoryType, TrustTier};
use memoria_storage::SqlMemoryStore;
use sqlx::Row;
use uuid::Uuid;

fn test_dim() -> usize {
    std::env::var("EMBEDDING_DIM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1024)
}

fn dim_vec(idx: usize, val: f32) -> Vec<f32> {
    let mut v = vec![0.0f32; test_dim()];
    if idx < v.len() {
        v[idx] = val;
    }
    v
}

async fn setup() -> (SqlMemoryStore, String) {
    let url = std::env::var("DATABASE_URL")
        .unwrap_or_else(|_| "mysql://root:111@localhost:6001/memoria_test".to_string());
    let instance_id = Uuid::new_v4().to_string();
    let store = SqlMemoryStore::connect(&url, test_dim(), instance_id)
        .await
        .expect("connect");
    store.migrate().await.expect("migrate");
    let user_id = format!("subj_mo_{}", Uuid::new_v4().simple());
    (store, user_id)
}

fn make_memory(id: &str, content: &str, user_id: &str, subject_id: Option<&str>) -> Memory {
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
        subject_id: subject_id.map(|s| s.to_string()),
    }
}

async fn column_exists(pool: &sqlx::MySqlPool, table: &str, column: &str) -> bool {
    let row = sqlx::query(
        "SELECT COUNT(*) AS cnt FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? AND COLUMN_NAME = ?",
    )
    .bind(table)
    .bind(column)
    .fetch_one(pool)
    .await
    .expect("information_schema query");
    row.try_get::<i64, _>("cnt").unwrap_or(0) > 0
}

async fn index_exists(pool: &sqlx::MySqlPool, table: &str, index: &str) -> bool {
    let row = sqlx::query(
        "SELECT COUNT(*) AS cnt FROM information_schema.STATISTICS \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_NAME = ? AND INDEX_NAME = ?",
    )
    .bind(table)
    .bind(index)
    .fetch_one(pool)
    .await
    .expect("information_schema index query");
    row.try_get::<i64, _>("cnt").unwrap_or(0) > 0
}

// ── Schema / migration ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_migrate_adds_subject_id_column_and_index() {
    let (store, _) = setup().await;
    let pool = store.pool();

    assert!(
        column_exists(pool, "mem_memories", "subject_id").await,
        "mem_memories must have subject_id column after migrate"
    );
    assert!(
        index_exists(pool, "mem_memories", "idx_scope_subject_active").await,
        "mem_memories must have idx_scope_subject_active after migrate"
    );
    println!("✅ migrate adds subject_id column and composite index");
}

#[tokio::test]
async fn test_migrate_subject_id_idempotent() {
    let (store, _) = setup().await;
    store.migrate().await.expect("second migrate must succeed");
    store.migrate().await.expect("third migrate must succeed");
    println!("✅ subject_id migration is idempotent");
}

// ── CRUD persistence ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_insert_persists_subject_id() {
    let (store, uid) = setup().await;
    let id = format!("subj-persist-{uid}");
    let subject = "patient-42";
    let m = make_memory(&id, "subject persistence test", &uid, Some(subject));
    store.insert(&m).await.expect("insert");

    let got = store.get(&id).await.expect("get").expect("exists");
    assert_eq!(got.subject_id.as_deref(), Some(subject));
    println!("✅ insert persists subject_id");
}

#[tokio::test]
async fn test_subject_id_max_length_128_persisted() {
    let (store, uid) = setup().await;
    let id = format!("subj-max-{uid}");
    let subject = "s".repeat(128);
    let m = make_memory(&id, "max length subject", &uid, Some(&subject));
    store.insert(&m).await.expect("insert 128-char subject_id");

    let got = store.get(&id).await.expect("get").unwrap();
    assert_eq!(got.subject_id.as_deref(), Some(subject.as_str()));
    println!("✅ 128-char subject_id persisted");
}

// ── list_active_lite SQL ─────────────────────────────────────────────────────

#[tokio::test]
async fn test_list_active_lite_filters_by_subject_id() {
    let (store, uid) = setup().await;
    let subject_a = "subject-alice-mo";
    let subject_b = "subject-bob-mo";

    store
        .insert(&make_memory(
            &format!("list-a1-{uid}"),
            "alice memory one",
            &uid,
            Some(subject_a),
        ))
        .await
        .unwrap();
    store
        .insert(&make_memory(
            &format!("list-a2-{uid}"),
            "alice memory two",
            &uid,
            Some(subject_a),
        ))
        .await
        .unwrap();
    store
        .insert(&make_memory(
            &format!("list-b1-{uid}"),
            "bob memory one",
            &uid,
            Some(subject_b),
        ))
        .await
        .unwrap();

    let alice = store
        .list_active_lite(
            "mem_memories",
            &uid,
            20,
            None,
            None,
            None,
            None,
            Some(subject_a),
        )
        .await
        .expect("list alice");
    assert_eq!(alice.len(), 2);
    for m in &alice {
        assert_eq!(m.subject_id.as_deref(), Some(subject_a));
    }

    let bob = store
        .list_active_lite(
            "mem_memories",
            &uid,
            20,
            None,
            None,
            None,
            None,
            Some(subject_b),
        )
        .await
        .expect("list bob");
    assert_eq!(bob.len(), 1);
    assert_eq!(bob[0].content, "bob memory one");
    println!("✅ list_active_lite filters by subject_id");
}

#[tokio::test]
async fn test_list_active_lite_null_subject_excluded_when_filtering() {
    let (store, uid) = setup().await;
    let subject = "scoped-only";

    store
        .insert(&make_memory(
            &format!("null-1-{uid}"),
            "unscoped memory",
            &uid,
            None,
        ))
        .await
        .unwrap();
    store
        .insert(&make_memory(
            &format!("null-2-{uid}"),
            "scoped memory",
            &uid,
            Some(subject),
        ))
        .await
        .unwrap();

    let scoped = store
        .list_active_lite(
            "mem_memories",
            &uid,
            20,
            None,
            None,
            None,
            None,
            Some(subject),
        )
        .await
        .expect("list scoped");
    assert_eq!(scoped.len(), 1);
    assert_eq!(scoped[0].content, "scoped memory");
    println!("✅ NULL subject_id excluded when filtering");
}

#[tokio::test]
async fn test_list_active_lite_subject_and_memory_type_composite() {
    let (store, uid) = setup().await;
    let subject = "combo-subject";

    let mut semantic = make_memory(
        &format!("combo-sem-{uid}"),
        "combo semantic",
        &uid,
        Some(subject),
    );
    semantic.memory_type = MemoryType::Semantic;
    let mut profile = make_memory(
        &format!("combo-pro-{uid}"),
        "combo profile",
        &uid,
        Some(subject),
    );
    profile.memory_type = MemoryType::Profile;
    let mut other_subject = make_memory(
        &format!("combo-other-{uid}"),
        "combo semantic other",
        &uid,
        Some("other-subject"),
    );
    other_subject.memory_type = MemoryType::Semantic;

    store.insert(&semantic).await.unwrap();
    store.insert(&profile).await.unwrap();
    store.insert(&other_subject).await.unwrap();

    let results = store
        .list_active_lite(
            "mem_memories",
            &uid,
            20,
            Some("semantic"),
            None,
            None,
            None,
            Some(subject),
        )
        .await
        .expect("composite list");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].memory_type, MemoryType::Semantic);
    assert_eq!(results[0].subject_id.as_deref(), Some(subject));
    println!("✅ list_active_lite composite subject_id + memory_type");
}

// ── Fulltext scoped SQL (bound memory_types IN clause) ───────────────────────

#[tokio::test]
async fn test_search_fulltext_scoped_subject_and_memory_types() {
    let (store, uid) = setup().await;
    let subject = "ft-subject-mo";
    let token = "zephyrunique";

    let mut semantic = make_memory(
        &format!("ft-sem-{uid}"),
        &format!("{token} semantic scoped"),
        &uid,
        Some(subject),
    );
    semantic.memory_type = MemoryType::Semantic;
    let mut profile = make_memory(
        &format!("ft-pro-{uid}"),
        &format!("{token} profile scoped"),
        &uid,
        Some(subject),
    );
    profile.memory_type = MemoryType::Profile;
    let mut other_subject = make_memory(
        &format!("ft-other-{uid}"),
        &format!("{token} semantic other subject"),
        &uid,
        Some("other-ft-subject"),
    );
    other_subject.memory_type = MemoryType::Semantic;

    store.insert(&semantic).await.unwrap();
    store.insert(&profile).await.unwrap();
    store.insert(&other_subject).await.unwrap();

    let results = store
        .search_fulltext_from_scoped(
            "mem_memories",
            &uid,
            token,
            10,
            None,
            Some(subject),
            Some(&[MemoryType::Semantic]),
            &memoria_core::MemoryFilter::default(),
        )
        .await
        .expect("fulltext scoped");

    assert!(
        !results.is_empty(),
        "fulltext should match scoped semantic memory"
    );
    for m in &results {
        assert_eq!(m.subject_id.as_deref(), Some(subject));
        assert_eq!(m.memory_type, MemoryType::Semantic);
        assert!(
            m.content.contains(token),
            "result must contain search token: {}",
            m.content
        );
    }
    assert!(
        !results.iter().any(|m| m.content.contains("profile")),
        "Profile type must be excluded by memory_types filter"
    );
    println!("✅ search_fulltext_from_scoped subject + memory_types");
}

// ── Vector scoped SQL (inlined subject_id + memory_types) ─────────────────────

#[tokio::test]
async fn test_search_vector_scoped_subject_and_memory_types() {
    let (store, uid) = setup().await;
    let subject = "vec-subject-mo";

    sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await
        .unwrap();

    let mut target = make_memory(
        &format!("vec-target-{uid}"),
        "vector target scoped",
        &uid,
        Some(subject),
    );
    target.memory_type = MemoryType::Semantic;
    target.embedding = Some(dim_vec(0, 1.0));

    let mut same_subject_profile = make_memory(
        &format!("vec-profile-{uid}"),
        "vector profile scoped",
        &uid,
        Some(subject),
    );
    same_subject_profile.memory_type = MemoryType::Profile;
    same_subject_profile.embedding = Some(dim_vec(0, 0.9));

    let mut other_subject = make_memory(
        &format!("vec-other-{uid}"),
        "vector other subject",
        &uid,
        Some("other-vec-subject"),
    );
    other_subject.memory_type = MemoryType::Semantic;
    other_subject.embedding = Some(dim_vec(0, 1.0));

    store.insert(&target).await.unwrap();
    store.insert(&same_subject_profile).await.unwrap();
    store.insert(&other_subject).await.unwrap();

    let query_emb = dim_vec(0, 1.0);
    let results = store
        .search_vector_from_filtered_scoped(
            "mem_memories",
            &uid,
            &query_emb,
            5,
            None,
            None,
            Some(subject),
            Some(&[MemoryType::Semantic]),
            &memoria_core::MemoryFilter::default(),
        )
        .await
        .expect("vector scoped");

    assert!(!results.is_empty());
    assert_eq!(results[0].memory_id, target.memory_id);
    for m in &results {
        assert_eq!(m.subject_id.as_deref(), Some(subject));
        assert_eq!(m.memory_type, MemoryType::Semantic);
    }
    println!("✅ search_vector_from_filtered_scoped subject + memory_types");
}

// ── Hybrid scoped SQL ────────────────────────────────────────────────────────

#[tokio::test]
async fn test_search_hybrid_scoped_subject_isolation() {
    let (store, uid) = setup().await;
    let subject = "hybrid-subject-mo";
    let token = "quasarunique";

    sqlx::query("DELETE FROM mem_memories WHERE user_id = ?")
        .bind(&uid)
        .execute(store.pool())
        .await
        .unwrap();

    let mut scoped = make_memory(
        &format!("hyb-scoped-{uid}"),
        &format!("{token} hybrid scoped memory"),
        &uid,
        Some(subject),
    );
    scoped.embedding = Some(dim_vec(1, 1.0));

    let mut other = make_memory(
        &format!("hyb-other-{uid}"),
        &format!("{token} hybrid other subject"),
        &uid,
        Some("other-hybrid-subject"),
    );
    other.embedding = Some(dim_vec(1, 1.0));

    store.insert(&scoped).await.unwrap();
    store.insert(&other).await.unwrap();

    let query_emb = dim_vec(1, 1.0);
    let (results, _scores) = store
        .search_hybrid_from_scored_scoped(
            "mem_memories",
            &uid,
            &query_emb,
            token,
            5,
            0.0,
            None,
            Some(subject),
            None,
            &memoria_core::MemoryFilter::default(),
        )
        .await
        .expect("hybrid scoped");

    assert!(!results.is_empty());
    for m in &results {
        assert_eq!(m.subject_id.as_deref(), Some(subject));
    }
    assert!(
        !results.iter().any(|m| m.memory_id.contains("hyb-other")),
        "other subject must not appear in hybrid results"
    );
    println!("✅ search_hybrid_from_scored_scoped subject isolation");
}
