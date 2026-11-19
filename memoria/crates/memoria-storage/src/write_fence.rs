//! Durable range generations for suspended writes. No bodies or credentials.
use super::{db_err, SqlMemoryStore};
use memoria_core::{MemoriaError, Memory};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{MySqlConnection, Row};

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryWriteFence {
    pub subject_id: String,
    pub connector_id: String,
    pub workflow_id: String,
    pub generations: [String; 4],
}

fn range_key(owner: &str, kind: &str, identifiers: &[&str]) -> String {
    let encoded = serde_json::to_vec(&(owner, kind, identifiers)).expect("string range key");
    format!("{:x}", Sha256::digest(encoded))
}

impl MemoryWriteFence {
    fn keys(&self, owner: &str) -> [String; 4] {
        [
            range_key(owner, "group", &[]),
            range_key(owner, "subject", &[&self.subject_id]),
            range_key(
                owner,
                "subject_connector",
                &[&self.subject_id, &self.connector_id],
            ),
            range_key(owner, "workflow", &[&self.workflow_id]),
        ]
    }

    pub fn validate(&self) -> Result<(), MemoriaError> {
        for (value, limit) in [
            (&self.subject_id, 128),
            (&self.connector_id, 1024),
            (&self.workflow_id, 128),
        ] {
            if value.is_empty()
                || value.len() > limit
                || value.trim() != value
                || value.chars().any(char::is_control)
            {
                return Err(MemoriaError::Validation(
                    "invalid memory write fence scope".into(),
                ));
            }
        }
        if self
            .generations
            .iter()
            .any(|value| value.len() != 32 || uuid::Uuid::parse_str(value).is_err())
        {
            return Err(MemoriaError::Validation(
                "invalid memory write fence generation".into(),
            ));
        }
        Ok(())
    }
}

impl SqlMemoryStore {
    pub(super) async fn ensure_write_fence_row(
        &self,
        owner: &str,
        key: &str,
    ) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            "INSERT IGNORE INTO {} (range_key, user_id, generation, erasing) VALUES (?, ?, ?, 0)",
            self.t("mem_write_fences")
        ))
        .bind(key)
        .bind(owner)
        .bind(uuid::Uuid::new_v4().simple().to_string())
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn lock_write_fences(
        &self,
        connection: &mut MySqlConnection,
        owner: &str,
        fence: &MemoryWriteFence,
        compare: bool,
    ) -> Result<[String; 4], MemoriaError> {
        let mut generations: [String; 4] = Default::default();
        for (index, key) in fence.keys(owner).iter().enumerate() {
            let row = sqlx::query(&format!(
                "SELECT generation, erasing FROM {} WHERE range_key = ? FOR UPDATE",
                self.t("mem_write_fences")
            ))
            .bind(key)
            .fetch_optional(&mut *connection)
            .await
            .map_err(db_err)?
            .ok_or_else(|| MemoriaError::Conflict("memory_write_fence_stale".into()))?;
            let generation: String = row.try_get("generation").map_err(db_err)?;
            let erasing: i8 = row.try_get("erasing").map_err(db_err)?;
            if erasing != 0 || (compare && generation != fence.generations[index]) {
                return Err(MemoriaError::Conflict("memory_write_fence_stale".into()));
            }
            generations[index] = generation;
        }
        Ok(generations)
    }

    pub async fn capture_write_fence(
        &self,
        owner: &str,
        subject: &str,
        connector: &str,
        workflow: &str,
    ) -> Result<MemoryWriteFence, MemoriaError> {
        let mut fence = MemoryWriteFence {
            subject_id: subject.into(),
            connector_id: connector.into(),
            workflow_id: workflow.into(),
            generations: std::array::from_fn(|_| uuid::Uuid::new_v4().simple().to_string()),
        };
        fence.validate()?;
        for key in fence.keys(owner) {
            self.ensure_write_fence_row(owner, &key).await?;
        }
        let mut transaction = self.pool.begin().await.map_err(db_err)?;
        fence.generations = self
            .lock_write_fences(&mut transaction, owner, &fence, false)
            .await?;
        transaction.commit().await.map_err(db_err)?;
        Ok(fence)
    }

    /// Complete the already-issued transaction even if the HTTP caller leaves.
    /// The database lock, not a dropped Future or lease TTL, orders it with erase.
    pub async fn insert_with_write_fence(
        &self,
        table: &str,
        memory: &Memory,
        fence: &MemoryWriteFence,
    ) -> Result<Option<Memory>, MemoriaError> {
        fence.validate()?;
        let metadata = memory.extra_metadata.as_ref();
        if memory.subject_id.as_deref() != Some(&fence.subject_id)
            || metadata
                .and_then(|m| m.get("connector_id"))
                .and_then(|v| v.as_str())
                != Some(&fence.connector_id)
            || metadata
                .and_then(|m| m.get("workflow_id"))
                .and_then(|v| v.as_str())
                != Some(&fence.workflow_id)
        {
            return Err(MemoriaError::Validation(
                "memory write fence does not match payload".into(),
            ));
        }
        let store = self.clone();
        let (table, memory, fence) = (table.to_owned(), memory.clone(), fence.clone());
        tokio::spawn(async move {
            let _permit = store
                .erase_barrier()
                .try_enter(&memory.user_id)
                .ok_or_else(|| {
                    MemoriaError::Blocked("memory erase is already running for this scope".into())
                })?;
            let mut transaction = store.pool.begin().await.map_err(db_err)?;
            store
                .lock_write_fences(&mut transaction, &memory.user_id, &fence, true)
                .await?;
            let row = sqlx::query(&format!(
                "SELECT {} FROM {table} WHERE memory_id = ? AND user_id = ?",
                super::FULLTEXT_MEMORY_COLUMNS
            ))
            .bind(&memory.memory_id)
            .bind(&memory.user_id)
            .fetch_optional(&mut *transaction)
            .await
            .map_err(db_err)?;
            let prior = row.as_ref().map(super::row_to_memory).transpose()?;
            if prior.is_none() {
                Self::insert_on_connection(&mut transaction, &table, &memory).await?;
            }
            transaction.commit().await.map_err(db_err)?;
            Ok(prior)
        })
        .await
        .map_err(|_| MemoriaError::Internal("memory fenced write task failed".into()))?
    }

    pub(super) fn write_fence_erase_key(
        owner: &str,
        selector: &super::MemoryEraseSelector,
    ) -> Result<String, MemoriaError> {
        let key = match (
            &selector.subject_id,
            &selector.connector_id,
            &selector.workflow_id,
        ) {
            (None, None, None) => range_key(owner, "group", &[]),
            (Some(subject), None, None) => range_key(owner, "subject", &[subject]),
            (Some(subject), Some(connector), None) => {
                range_key(owner, "subject_connector", &[subject, connector])
            }
            (None, None, Some(workflow)) => range_key(owner, "workflow", &[workflow]),
            _ => return Err(MemoriaError::Validation("invalid erase range".into())),
        };
        Ok(key)
    }

    pub(super) async fn begin_write_fence_erase(
        &self,
        key: &str,
        generation: &str,
    ) -> Result<u64, MemoriaError> {
        // UPDATE serializes with the exact same row locked by a fenced INSERT.
        let result = sqlx::query(&format!(
            "UPDATE {} SET generation = ?, erasing = 1 WHERE range_key = ?",
            self.t("mem_write_fences")
        ))
        .bind(generation)
        .bind(key)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if result.rows_affected() != 1 {
            return Err(MemoriaError::Internal(
                "memory erase lost its lock before completion (write fence)".into(),
            ));
        }
        Ok(1)
    }

    pub(super) async fn finish_write_fence_erase(
        &self,
        key: &str,
        generation: &str,
    ) -> Result<u64, MemoriaError> {
        let result = sqlx::query(&format!(
            "UPDATE {} SET erasing = 0 WHERE range_key = ? AND generation = ? AND erasing = 1",
            self.t("mem_write_fences")
        ))
        .bind(key)
        .bind(generation)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        if result.rows_affected() != 1 {
            return Err(MemoriaError::Internal(
                "memory erase lost its lock before completion (write fence)".into(),
            ));
        }
        Ok(1)
    }
}

#[cfg(test)]
mod tests {
    use super::super::{EraseStatementHold, EraseStepFuture, EraseStepGate, MemoryEraseSelector};
    use super::*;
    use futures::FutureExt;
    use memoria_core::{MemoryType, TrustTier};
    use sqlx::mysql::{MySqlConnectOptions, MySqlPoolOptions};
    use std::{future::Future, time::Duration};

    struct Direct;
    impl EraseStatementHold for Direct {
        fn run_step<F>(&self, action: F) -> EraseStepFuture
        where
            F: Future<Output = Result<u64, MemoriaError>> + Send + 'static,
        {
            Box::pin(action)
        }
    }

    fn candidate(owner: &str, fence: &MemoryWriteFence, id: &str) -> Memory {
        Memory {
            memory_id: id.into(),
            user_id: owner.into(),
            content: "synthetic fenced candidate".into(),
            memory_type: MemoryType::Semantic,
            trust_tier: TrustTier::T1Verified,
            initial_confidence: 0.73,
            embedding: None,
            session_id: Some("synthetic-session".into()),
            source_event_ids: vec![],
            extra_metadata: Some(std::collections::HashMap::from([
                ("connector_id".into(), serde_json::json!(fence.connector_id)),
                ("workflow_id".into(), serde_json::json!(fence.workflow_id)),
                (
                    "business_number".into(),
                    serde_json::json!(9007199254740993_u64),
                ),
            ])),
            observed_at: None,
            created_at: None,
            updated_at: None,
            superseded_by: None,
            is_active: true,
            access_count: 0,
            retrieval_score: None,
            author_id: None,
            subject_id: Some(fence.subject_id.clone()),
        }
    }

    // Explicit local integration entry; never silently skips missing credentials.
    #[tokio::test]
    #[ignore = "requires the dedicated local MatrixOne probe wrapper"]
    async fn write_fence_and_metadata_cas_against_matrixone() {
        let url = std::env::var("MEMORIA_REVIEW_DATABASE_URL").expect("local probe URL required");
        let options: MySqlConnectOptions = url.parse().expect("local probe URL");
        let admin = MySqlPoolOptions::new()
            .max_connections(1)
            .connect_with(options.clone())
            .await
            .expect("admin connection");
        let database = format!("memory_fence_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE `{database}`"))
            .execute(&admin)
            .await
            .expect("isolated database");
        let pool = MySqlPoolOptions::new()
            .max_connections(6)
            .connect_with(options.database(&database))
            .await
            .expect("isolated pool");
        let store = SqlMemoryStore::new(pool.clone(), 4, "local-fence-probe".into());
        store.migrate_user().await.expect("production user schema");
        let owner = uuid::Uuid::new_v4().simple().to_string();
        let result = std::panic::AssertUnwindSafe(async {
            // Retain existing memories through the version-2 upgrade; do not
            // replace a historical database with fresh schema or reset its data.
            let synthetic = MemoryWriteFence {
                subject_id: "upgrade-control".into(),
                connector_id: "connector".into(),
                workflow_id: "workflow".into(),
                generations: std::array::from_fn(|_| "1".repeat(32)),
            };
            let retained = candidate(&owner, &synthetic, "retained-upgrade-row");
            store.insert_into("mem_memories", &retained).await?;
            sqlx::query("DROP TABLE mem_write_fences")
                .execute(&pool)
                .await
                .map_err(db_err)?;
            sqlx::query(
                "UPDATE mem_schema_meta SET schema_version = 2 WHERE schema_key = 'user_schema'",
            )
            .execute(&pool)
            .await
            .map_err(db_err)?;
            store.migrate_user().await?;
            assert_eq!(
                store
                    .get_from("mem_memories", &retained.memory_id)
                    .await?
                    .unwrap()
                    .content,
                retained.content
            );
            // Each narrower erase preserves a control outside precisely that range.
            for (index, selector, control) in [
                (
                    0,
                    MemoryEraseSelector {
                        subject_id: Some("subject".into()),
                        connector_id: Some("connector".into()),
                        workflow_id: None,
                    },
                    ("subject", "other-connector", "other-workflow"),
                ),
                (
                    1,
                    MemoryEraseSelector {
                        subject_id: Some("subject".into()),
                        ..Default::default()
                    },
                    ("other-subject", "connector", "workflow"),
                ),
                (
                    2,
                    MemoryEraseSelector {
                        workflow_id: Some("workflow".into()),
                        ..Default::default()
                    },
                    ("subject", "connector", "other-workflow"),
                ),
                (
                    3,
                    MemoryEraseSelector::default(),
                    ("other-subject", "other-connector", "other-workflow"),
                ),
            ] {
                let fence = store
                    .capture_write_fence(&owner, "subject", "connector", "workflow")
                    .await?;
                let unaffected = store
                    .capture_write_fence(&owner, control.0, control.1, control.2)
                    .await?;
                let old = candidate(&owner, &fence, &format!("old-{index}"));
                assert!(store
                    .insert_with_write_fence("mem_memories", &old, &fence)
                    .await?
                    .is_none());
                assert!(
                    store
                        .insert_with_write_fence("mem_memories", &old, &fence)
                        .await?
                        .is_some(),
                    "same receipt replay"
                );
                store
                    .erase_memories(&owner, &selector, &mut EraseStepGate::new(Direct))
                    .await?;
                assert!(store
                    .get_from("mem_memories", &old.memory_id)
                    .await?
                    .is_none());
                assert!(
                    matches!(store.insert_with_write_fence("mem_memories", &old, &fence).await,
                    Err(MemoriaError::Conflict(message)) if message == "memory_write_fence_stale")
                );
                let control_row = candidate(&owner, &unaffected, &format!("control-{index}"));
                let kept = store
                    .insert_with_write_fence("mem_memories", &control_row, &unaffected)
                    .await;
                if index == 3 {
                    assert!(matches!(kept, Err(MemoriaError::Conflict(_))));
                } else {
                    assert!(
                        kept?.is_none(),
                        "narrow erase must not reject unrelated ranges"
                    );
                }
                let fresh = store
                    .capture_write_fence(&owner, "subject", "connector", "workflow")
                    .await?;
                assert!(store
                    .insert_with_write_fence(
                        "mem_memories",
                        &candidate(&owner, &fresh, &format!("fresh-{index}")),
                        &fresh
                    )
                    .await?
                    .is_none());
            }
            // An actual SELECT FOR UPDATE blocks the epoch UPDATE, independent of
            // in-process barriers. Do not drop either database operation to cancel it.
            let fence = store
                .capture_write_fence(&owner, "subject", "connector", "workflow")
                .await?;
            let key =
                SqlMemoryStore::write_fence_erase_key(&owner, &MemoryEraseSelector::default())?;
            let mut held = pool.begin().await.map_err(db_err)?;
            store
                .lock_write_fences(&mut held, &owner, &fence, true)
                .await?;
            let (other, epoch) = (store.clone(), uuid::Uuid::new_v4().simple().to_string());
            let mut erase =
                tokio::spawn(async move { other.begin_write_fence_erase(&key, &epoch).await });
            assert!(
                tokio::time::timeout(Duration::from_millis(150), &mut erase)
                    .await
                    .is_err(),
                "epoch UPDATE bypassed locked row"
            );
            let inflight = candidate(&owner, &fence, "inflight-before-erase");
            SqlMemoryStore::insert_on_connection(&mut held, "mem_memories", &inflight).await?;
            held.commit().await.map_err(db_err)?;
            tokio::time::timeout(Duration::from_secs(10), erase)
                .await
                .expect("epoch wait finished")
                .expect("epoch task")?;
            store
                .erase_memories(
                    &owner,
                    &MemoryEraseSelector::default(),
                    &mut EraseStepGate::new(Direct),
                )
                .await?;
            assert!(store
                .get_from("mem_memories", &inflight.memory_id)
                .await?
                .is_none());
            assert!(matches!(
                store
                    .insert_with_write_fence("mem_memories", &inflight, &fence)
                    .await,
                Err(MemoriaError::Conflict(_))
            ));

            // Caller cancellation cannot drop the independent write transaction.
            let fence = store
                .capture_write_fence(&owner, "subject", "connector", "workflow")
                .await?;
            let mut held = pool.begin().await.map_err(db_err)?;
            store
                .lock_write_fences(&mut held, &owner, &fence, true)
                .await?;
            let memory = candidate(&owner, &fence, "cancelled-caller");
            let (other, saved) = (store.clone(), memory.clone());
            let caller = tokio::spawn(async move {
                other
                    .insert_with_write_fence("mem_memories", &saved, &fence)
                    .await
            });
            tokio::time::timeout(Duration::from_secs(5), async {
                while pool.size() as usize - pool.num_idle() < 2 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("detached write acquired its connection");
            caller.abort();
            assert!(caller.await.expect_err("caller aborted").is_cancelled());
            held.commit().await.map_err(db_err)?;
            let persisted = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    if let Some(row) = store.get_from("mem_memories", &memory.memory_id).await? {
                        break Ok::<_, MemoriaError>(row);
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            })
            .await
            .expect("detached write finished")?;
            let patch = std::collections::HashMap::from([(
                "result_freshness".into(),
                serde_json::json!("superseded"),
            )]);
            let changed = store
                .patch_metadata_atomically("mem_memories", &persisted, &patch)
                .await?;
            assert_eq!(changed.content, persisted.content);
            assert_eq!(changed.initial_confidence, persisted.initial_confidence);
            assert!(changed.updated_at > persisted.updated_at);
            assert_eq!(
                changed.extra_metadata.as_ref().unwrap()["business_number"],
                serde_json::json!(9007199254740993_u64)
            );
            assert!(matches!(
                store
                    .patch_metadata_atomically("mem_memories", &persisted, &patch)
                    .await,
                Err(MemoriaError::Conflict(_))
            ));
            let mut concurrent_metadata = changed.extra_metadata.clone().unwrap();
            concurrent_metadata.insert(
                "source_job_id".into(),
                serde_json::json!("concurrent-owner"),
            );
            sqlx::query("UPDATE mem_memories SET extra_metadata = ? WHERE memory_id = ?")
                .bind(serde_json::to_string(&concurrent_metadata)?)
                .bind(&changed.memory_id)
                .execute(&pool)
                .await
                .map_err(db_err)?;
            let concurrent = store
                .get_from("mem_memories", &changed.memory_id)
                .await?
                .unwrap();
            assert_eq!(
                concurrent.updated_at, changed.updated_at,
                "simulate an old timestamp-colliding writer"
            );
            assert_ne!(
                super::super::memory_revision(&concurrent),
                super::super::memory_revision(&changed)
            );
            assert!(matches!(
                store
                    .patch_metadata_atomically("mem_memories", &changed, &patch)
                    .await,
                Err(MemoriaError::Conflict(_))
            ));
            let mut absent = changed.clone();
            absent.memory_id = "absent".into();
            assert!(matches!(
                store
                    .patch_metadata_atomically("mem_memories", &absent, &patch)
                    .await,
                Err(MemoriaError::NotFound(_))
            ));
            Ok::<_, MemoriaError>(())
        })
        .catch_unwind()
        .await;
        pool.close().await;
        sqlx::query(&format!("DROP DATABASE `{database}`"))
            .execute(&admin)
            .await
            .expect("drop only isolated scratch database");
        admin.close().await;
        result
            .expect("local fence/CAS assertions")
            .expect("local fence/CAS contract");
    }
    #[test]
    fn range_keys_preserve_exact_boundaries() {
        assert_ne!(
            range_key("a", "subject_connector", &["a:b", "c"]),
            range_key("a", "subject_connector", &["a", "b:c"])
        );
        assert_ne!(range_key("a", "group", &[]), range_key("b", "group", &[]));
        assert_ne!(
            range_key("a", "subject", &["w"]),
            range_key("a", "workflow", &["w"])
        );
    }
}
