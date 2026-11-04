//! GraphStore — CRUD for memory_graph_nodes and memory_graph_edges.
//! Mirrors Python's graph/graph_store.py core methods needed for consolidation.

use crate::graph::types::{edge_type, GraphEdge, GraphNode, NodeType};
use crate::store::{db_err, fulltext_rows_or_empty};
use memoria_core::{nullable_str, nullable_str_from_row, MemoriaError};
use sqlx::{MySqlPool, Row};
use uuid::Uuid;

fn new_id() -> String {
    Uuid::new_v4().simple().to_string()
}

/// Columns for GraphNode (excluding embedding for lightweight queries)
const NODE_COLS_NO_EMB: &str = "node_id, user_id, node_type, content, entity_type, memory_id, \
    session_id, confidence, trust_tier, importance, source_nodes, conflicts_with, \
    conflict_resolution, access_count, cross_session_count, is_active, superseded_by, created_at";

/// Columns for GraphNode (including embedding)
const NODE_COLS_WITH_EMB: &str =
    "node_id, user_id, node_type, content, entity_type, embedding, memory_id, \
    session_id, confidence, trust_tier, importance, source_nodes, conflicts_with, \
    conflict_resolution, access_count, cross_session_count, is_active, superseded_by, created_at";

pub struct GraphStore {
    pool: MySqlPool,
    /// When Some, table names are qualified with this database name.
    db_name: Option<String>,
    embedding_dim: usize,
    /// Cache: user_id → node count (TTL 2 min)
    node_count_cache: moka::sync::Cache<String, i64>,
}

impl GraphStore {
    pub fn new(pool: MySqlPool, embedding_dim: usize) -> Self {
        Self {
            pool,
            db_name: None,
            embedding_dim,
            node_count_cache: moka::sync::Cache::builder()
                .max_capacity(10_000)
                .time_to_live(std::time::Duration::from_secs(120))
                .build(),
        }
    }

    /// Create with a shared node-count cache (used by SqlMemoryStore to share
    /// the cache across multiple GraphStore instances).
    pub fn with_node_count_cache(
        pool: MySqlPool,
        embedding_dim: usize,
        node_count_cache: moka::sync::Cache<String, i64>,
    ) -> Self {
        Self {
            pool,
            db_name: None,
            embedding_dim,
            node_count_cache,
        }
    }

    pub fn pool(&self) -> &MySqlPool {
        &self.pool
    }

    pub fn set_db_name(&mut self, name: String) {
        self.db_name = Some(name);
    }

    /// Qualify a table name with the database prefix when `db_name` is set.
    pub fn t(&self, table: &str) -> String {
        if table.contains('.') || table.contains('`') {
            return table.to_string();
        }
        match &self.db_name {
            None => table.to_string(),
            Some(db) => format!("`{}`.{}", db.replace('`', "``"), table),
        }
    }

    // ── DDL ──────────────────────────────────────────────────────────────────

    pub async fn migrate(&self) -> Result<(), MemoriaError> {
        // memory_graph_nodes
        let sql = format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                node_id             VARCHAR(32)  NOT NULL,
                user_id             VARCHAR(64)  NOT NULL,
                node_type           VARCHAR(10)  NOT NULL,
                content             TEXT         NOT NULL,
                entity_type         VARCHAR(20)  DEFAULT NULL,
                embedding           vecf32({dim}) DEFAULT NULL,
                memory_id           VARCHAR(64)  DEFAULT NULL,
                session_id          VARCHAR(64)  DEFAULT NULL,
                confidence          FLOAT        DEFAULT 0.75,
                trust_tier          VARCHAR(4)   DEFAULT 'T3',
                importance          FLOAT        NOT NULL DEFAULT 0.0,
                source_nodes        TEXT         DEFAULT NULL,
                conflicts_with      VARCHAR(32)  DEFAULT NULL,
                conflict_resolution VARCHAR(10)  DEFAULT NULL,
                access_count        INT          DEFAULT 0,
                cross_session_count INT          DEFAULT 0,
                is_active           SMALLINT     NOT NULL DEFAULT 1,
                superseded_by       VARCHAR(32)  DEFAULT NULL,
                created_at          DATETIME(6)  NOT NULL,
                PRIMARY KEY (node_id),
                KEY idx_graph_memory (memory_id),
                KEY idx_graph_conflicts (user_id, conflicts_with),
                KEY idx_graph_user_active (user_id, is_active, node_type),
                FULLTEXT ft_graph_content (content) WITH PARSER ngram
            )"#,
            self.t("memory_graph_nodes"),
            dim = self.embedding_dim
        );
        sqlx::query(&sql)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;

        // memory_graph_edges
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                source_id  VARCHAR(32)  NOT NULL,
                target_id  VARCHAR(32)  NOT NULL,
                edge_type  VARCHAR(15)  NOT NULL,
                weight     FLOAT        NOT NULL,
                user_id    VARCHAR(64)  NOT NULL,
                PRIMARY KEY (source_id, target_id, edge_type),
                KEY idx_edge_user (user_id),
                KEY idx_edge_target (target_id)
            )"#,
            self.t("memory_graph_edges"),
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        // mem_entities
        let sql2 = format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                entity_id    VARCHAR(32)  NOT NULL,
                user_id      VARCHAR(64)  NOT NULL,
                name         VARCHAR(200) NOT NULL,
                display_name VARCHAR(200) DEFAULT NULL,
                entity_type  VARCHAR(20)  NOT NULL DEFAULT 'concept',
                embedding    vecf32({dim}) DEFAULT NULL,
                created_at   DATETIME(6)  NOT NULL DEFAULT NOW(),
                PRIMARY KEY (entity_id),
                UNIQUE KEY uidx_entity_user_name (user_id, name),
                KEY idx_entity_user (user_id)
            )"#,
            self.t("mem_entities"),
            dim = self.embedding_dim
        );
        sqlx::query(&sql2)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;

        // mem_memory_entity_links
        sqlx::query(&format!(
            r#"CREATE TABLE IF NOT EXISTS {} (
                memory_id  VARCHAR(64) NOT NULL,
                entity_id  VARCHAR(32) NOT NULL,
                user_id    VARCHAR(64) NOT NULL,
                source     VARCHAR(10) NOT NULL DEFAULT 'regex',
                weight     FLOAT       NOT NULL DEFAULT 0.8,
                created_at DATETIME(6) NOT NULL DEFAULT NOW(),
                PRIMARY KEY (memory_id, entity_id),
                KEY idx_link_user_entity (user_id, entity_id),
                KEY idx_link_entity_user (entity_id, user_id)
            )"#,
            self.t("mem_memory_entity_links"),
        ))
        .execute(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(())
    }

    // ── Node reads ───────────────────────────────────────────────────────────

    pub async fn get_node(&self, node_id: &str) -> Result<Option<GraphNode>, MemoriaError> {
        let row = sqlx::query(&format!(
            "SELECT {NODE_COLS_WITH_EMB} FROM {} WHERE node_id = ?",
            self.t("memory_graph_nodes"),
        ))
        .bind(node_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.map(|r| row_to_node(&r)))
    }

    pub async fn get_nodes_by_ids(&self, ids: &[String]) -> Result<Vec<GraphNode>, MemoriaError> {
        if ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT {NODE_COLS_NO_EMB} FROM {} WHERE node_id IN ({placeholders})",
            self.t("memory_graph_nodes"),
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows.iter().map(row_to_node_no_emb).collect())
    }

    pub async fn get_node_by_memory_id(
        &self,
        memory_id: &str,
    ) -> Result<Option<GraphNode>, MemoriaError> {
        let row = sqlx::query(&format!(
            "SELECT {NODE_COLS_NO_EMB} FROM {} WHERE memory_id = ? AND is_active = 1 LIMIT 1",
            self.t("memory_graph_nodes"),
        ))
        .bind(memory_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.map(|r| row_to_node_no_emb(&r)))
    }

    /// Get all active nodes of a given type for a user (no embedding loaded).
    pub async fn get_user_nodes(
        &self,
        user_id: &str,
        node_type: &NodeType,
        active_only: bool,
    ) -> Result<Vec<GraphNode>, MemoriaError> {
        let active_clause = if active_only {
            " AND is_active = 1"
        } else {
            ""
        };
        let sql = format!(
            "SELECT node_id, user_id, node_type, content, entity_type, \
             memory_id, session_id, confidence, trust_tier, importance, \
             source_nodes, conflicts_with, conflict_resolution, \
             access_count, cross_session_count, is_active, superseded_by, created_at \
             FROM {} \
             WHERE user_id = ? AND node_type = ?{active_clause}",
            self.t("memory_graph_nodes"),
        );
        let rows = sqlx::query(&sql)
            .bind(user_id)
            .bind(node_type.as_str())
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(rows.iter().map(row_to_node_no_emb).collect())
    }

    // ── Node writes ──────────────────────────────────────────────────────────

    /// Get all active nodes for a user, including embeddings.
    pub async fn get_user_nodes_with_embeddings(
        &self,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<GraphNode>, MemoriaError> {
        // Must alias embedding to force MO to return vecf32 as text
        let sql = format!(
            "SELECT node_id, user_id, node_type, content, entity_type, \
             embedding AS embedding, memory_id, session_id, confidence, trust_tier, importance, \
             source_nodes, conflicts_with, conflict_resolution, \
             access_count, cross_session_count, is_active, superseded_by, created_at \
             FROM {} \
             WHERE user_id = ? AND is_active = 1 \
               AND embedding IS NOT NULL AND vector_dims(embedding) > 0 \
             LIMIT ?",
            self.t("memory_graph_nodes"),
        );
        let rows = sqlx::query(&sql)
            .bind(user_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(rows.iter().map(row_to_node).collect())
    }

    pub async fn create_node(&self, node: &GraphNode) -> Result<(), MemoriaError> {
        let source_nodes_str = if node.source_nodes.is_empty() {
            None
        } else {
            Some(node.source_nodes.join(","))
        };
        let now = node
            .created_at
            .unwrap_or_else(|| chrono::Utc::now().naive_utc());
        let emb_lit = node.embedding.as_ref().map(|v| {
            format!(
                "[{}]",
                v.iter()
                    .map(|f| f.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            )
        });
        let sql = if let Some(emb) = &emb_lit {
            format!(
                "INSERT INTO {} \
                 (node_id, user_id, node_type, content, entity_type, embedding, \
                  memory_id, session_id, confidence, trust_tier, importance, \
                  source_nodes, conflicts_with, conflict_resolution, \
                  access_count, cross_session_count, is_active, superseded_by, created_at) \
                 VALUES (?,?,?,?,?,'{}',?,?,?,?,?,?,?,?,?,?,?,?,?)",
                self.t("memory_graph_nodes"),
                emb
            )
        } else {
            format!(
                "INSERT INTO {} \
                 (node_id, user_id, node_type, content, entity_type, \
                  memory_id, session_id, confidence, trust_tier, importance, \
                  source_nodes, conflicts_with, conflict_resolution, \
                  access_count, cross_session_count, is_active, superseded_by, created_at) \
                 VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
                self.t("memory_graph_nodes"),
            )
        };
        sqlx::query(&sql)
            .bind(&node.node_id)
            .bind(&node.user_id)
            .bind(node.node_type.as_str())
            .bind(&node.content)
            .bind(nullable_str(&node.entity_type))
            .bind(nullable_str(&node.memory_id))
            .bind(nullable_str(&node.session_id))
            .bind(node.confidence)
            .bind(&node.trust_tier)
            .bind(node.importance)
            .bind(source_nodes_str)
            .bind(nullable_str(&node.conflicts_with))
            .bind(nullable_str(&node.conflict_resolution))
            .bind(node.access_count)
            .bind(node.cross_session_count)
            .bind(if node.is_active { 1i8 } else { 0i8 })
            .bind(nullable_str(&node.superseded_by))
            .bind(now)
            .execute(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(())
    }

    pub async fn deactivate_node(&self, node_id: &str) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            "UPDATE {} SET is_active = 0 WHERE node_id = ?",
            self.t("memory_graph_nodes")
        ))
        .bind(node_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Deactivate all graph nodes linked to a memory_id.
    pub async fn deactivate_by_memory_id(&self, memory_id: &str) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            "UPDATE {} SET is_active = 0 WHERE memory_id = ?",
            self.t("memory_graph_nodes")
        ))
        .bind(memory_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Restore existing nodes instead of creating duplicate active nodes during backfill.
    pub async fn reactivate_by_memory_id(&self, memory_id: &str) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            "UPDATE {} SET is_active = 1 WHERE memory_id = ? \
             AND (superseded_by IS NULL OR superseded_by = '') \
             AND EXISTS (SELECT 1 FROM {} WHERE memory_id = ? AND is_active = 1)",
            self.t("memory_graph_nodes"),
            self.t("mem_memories")
        ))
        .bind(memory_id)
        .bind(memory_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Update content (and optionally confidence) of a graph node by memory_id.
    pub async fn update_content_by_memory_id(
        &self,
        memory_id: &str,
        new_content: &str,
    ) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            "UPDATE {} SET content = ? WHERE memory_id = ? AND is_active = 1",
            self.t("memory_graph_nodes"),
        ))
        .bind(new_content)
        .bind(memory_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn update_confidence_and_tier(
        &self,
        node_id: &str,
        confidence: f32,
        tier: &str,
    ) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            "UPDATE {} SET confidence = ?, trust_tier = ? WHERE node_id = ?",
            self.t("memory_graph_nodes"),
        ))
        .bind(confidence)
        .bind(tier)
        .bind(node_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    pub async fn mark_conflict(
        &self,
        older_id: &str,
        newer_id: &str,
        confidence_factor: f32,
        old_confidence: f32,
    ) -> Result<(), MemoriaError> {
        // Mark older as conflicting with newer, reduce confidence
        sqlx::query(&format!(
            "UPDATE {} \
             SET conflicts_with = ?, confidence = ? \
             WHERE node_id = ?",
            self.t("memory_graph_nodes"),
        ))
        .bind(newer_id)
        .bind(old_confidence * confidence_factor)
        .bind(older_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    // ── Edge reads ───────────────────────────────────────────────────────────

    /// Get association edges with current cosine similarity between node embeddings.
    /// Returns (source_id, target_id, edge_weight, current_cosine_sim).
    /// Only returns pairs where both nodes have embeddings and cosine sim dropped below threshold.
    pub async fn get_association_edges_with_current_sim(
        &self,
        user_id: &str,
        min_edge_weight: f32,
        max_current_sim: f32,
    ) -> Result<Vec<(String, String, f32, f32)>, MemoriaError> {
        // MatrixOne doesn't support HAVING on computed aliases — use subquery
        let rows = sqlx::query(&format!(
            "SELECT source_id, target_id, edge_weight, cur_sim FROM ( \
               SELECT e.source_id, e.target_id, e.weight as edge_weight, \
               cosine_similarity(n1.embedding, n2.embedding) as cur_sim \
               FROM {} e \
               JOIN {} n1 ON e.source_id = n1.node_id \
               JOIN {} n2 ON e.target_id = n2.node_id \
               WHERE e.user_id = ? AND e.edge_type = ? AND e.weight >= ? \
               AND n1.embedding IS NOT NULL AND n2.embedding IS NOT NULL \
               AND vector_dims(n1.embedding) > 0 AND vector_dims(n2.embedding) > 0 \
             ) t WHERE t.cur_sim <= ?",
            self.t("memory_graph_edges"),
            self.t("memory_graph_nodes"),
            self.t("memory_graph_nodes"),
        ))
        .bind(user_id)
        .bind(edge_type::ASSOCIATION)
        .bind(min_edge_weight)
        .bind(max_current_sim)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;

        Ok(rows
            .iter()
            .filter_map(|r| {
                let src: String = r.try_get("source_id").ok()?;
                let tgt: String = r.try_get("target_id").ok()?;
                let ew: f32 = r.try_get("edge_weight").ok()?;
                let cs: f32 = r.try_get("cur_sim").ok()?;
                Some((src, tgt, ew, cs))
            })
            .collect::<Vec<_>>())
    }

    // ── Edge writes ──────────────────────────────────────────────────────────

    pub async fn add_edge(&self, edge: &GraphEdge) -> Result<(), MemoriaError> {
        sqlx::query(&format!(
            "INSERT INTO {} (source_id, target_id, edge_type, weight, user_id) \
             VALUES (?,?,?,?,?) \
             ON DUPLICATE KEY UPDATE weight = VALUES(weight)",
            self.t("memory_graph_edges"),
        ))
        .bind(&edge.source_id)
        .bind(&edge.target_id)
        .bind(&edge.edge_type)
        .bind(edge.weight)
        .bind(&edge.user_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    // ── Entity / link writes ─────────────────────────────────────────────────

    /// Upsert entity, return (entity_id, is_new).
    pub async fn upsert_entity(
        &self,
        user_id: &str,
        name: &str,
        display_name: &str,
        entity_type: &str,
    ) -> Result<(String, bool), MemoriaError> {
        // Try INSERT first; on duplicate (user_id, name) catch the error and SELECT.
        let entity_id = new_id();
        let now = chrono::Utc::now().naive_utc();
        let res = sqlx::query(&format!(
            "INSERT INTO {} (entity_id, user_id, name, display_name, entity_type, created_at) \
             VALUES (?,?,?,?,?,?)",
            self.t("mem_entities"),
        ))
        .bind(&entity_id)
        .bind(user_id)
        .bind(name)
        .bind(display_name)
        .bind(entity_type)
        .bind(now)
        .execute(&self.pool)
        .await;

        match res {
            Ok(_) => Ok((entity_id, true)),
            Err(sqlx::Error::Database(e)) if e.message().contains("Duplicate entry") => {
                // Duplicate on unique key — fetch existing
                let row = sqlx::query(&format!(
                    "SELECT entity_id FROM {} WHERE user_id = ? AND name = ?",
                    self.t("mem_entities"),
                ))
                .bind(user_id)
                .bind(name)
                .fetch_one(&self.pool)
                .await
                .map_err(db_err)?;
                let id: String = row.try_get("entity_id").map_err(db_err)?;
                Ok((id, false))
            }
            Err(e) => Err(db_err(e)),
        }
    }

    /// Batch-upsert entities, return vec of (name, entity_id).
    /// Uses multi-row INSERT IGNORE + single SELECT to resolve IDs in 2 round-trips.
    /// Duplicate names in the input are handled gracefully: INSERT IGNORE skips the
    /// second row (wasting only a generated UUID), and the SELECT returns one row per
    /// unique (user_id, name). Callers use the returned name→id map, so duplicates
    /// resolve to the same entity_id automatically.
    const UPSERT_CHUNK_SIZE: usize = 50;

    pub async fn batch_upsert_entities(
        &self,
        user_id: &str,
        entities: &[(&str, &str, &str)], // (name, display_name, entity_type)
    ) -> Result<Vec<(String, String)>, MemoriaError> {
        if entities.is_empty() {
            return Ok(Vec::new());
        }
        let now = chrono::Utc::now().naive_utc();

        // 1. Bulk INSERT IGNORE — inserts new, silently skips duplicates
        for chunk in entities.chunks(Self::UPSERT_CHUNK_SIZE) {
            let placeholders = chunk
                .iter()
                .map(|_| "(?,?,?,?,?,?)")
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT IGNORE INTO {} \
                 (entity_id, user_id, name, display_name, entity_type, created_at) \
                 VALUES {placeholders}",
                self.t("mem_entities"),
            );
            let mut q = sqlx::query(&sql);
            for (name, display, etype) in chunk {
                let eid = new_id();
                q = q
                    .bind(eid)
                    .bind(user_id)
                    .bind(*name)
                    .bind(*display)
                    .bind(*etype)
                    .bind(now);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }

        // 2. SELECT to resolve all entity_ids (deduplicate names, chunk to avoid
        //    overly long IN clauses on very large batches)
        let mut seen = std::collections::HashSet::new();
        let names: Vec<&str> = entities
            .iter()
            .map(|(n, _, _)| *n)
            .filter(|n| seen.insert(*n))
            .collect();

        let mut result = Vec::with_capacity(names.len());
        for chunk in names.chunks(Self::UPSERT_CHUNK_SIZE) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let sql = format!(
                "SELECT name, entity_id FROM {} WHERE user_id = ? AND name IN ({placeholders})",
                self.t("mem_entities"),
            );
            let mut q = sqlx::query(&sql).bind(user_id);
            for name in chunk {
                q = q.bind(*name);
            }
            let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
            for r in rows {
                let name: String = r.try_get("name").unwrap_or_default();
                let eid: String = r.try_get("entity_id").unwrap_or_default();
                result.push((name, eid));
            }
        }
        Ok(result)
    }

    pub async fn upsert_memory_entity_link(
        &self,
        memory_id: &str,
        entity_id: &str,
        user_id: &str,
        source: &str,
    ) -> Result<(), MemoriaError> {
        let now = chrono::Utc::now().naive_utc();
        // Weight by extraction source — higher = more confident
        let weight: f32 = match source {
            "manual" => 1.0,
            "llm" => 0.9,
            _ => 0.8, // regex, unknown
        };
        sqlx::query(&format!(
            "INSERT INTO {} \
             (memory_id, entity_id, user_id, source, weight, created_at) \
             VALUES (?,?,?,?,?,?) \
             ON DUPLICATE KEY UPDATE source = VALUES(source), weight = VALUES(weight)",
            self.t("mem_memory_entity_links"),
        ))
        .bind(memory_id)
        .bind(entity_id)
        .bind(user_id)
        .bind(source)
        .bind(weight)
        .bind(now)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Batch-upsert multiple memory↔entity links in a single multi-row INSERT.
    /// Each entry: (memory_id, entity_id, source).
    /// `user_id` is shared across all entries.
    pub async fn batch_upsert_memory_entity_links(
        &self,
        user_id: &str,
        links: &[(&str, &str, &str)], // (memory_id, entity_id, source)
    ) -> Result<(), MemoriaError> {
        if links.is_empty() {
            return Ok(());
        }
        let now = chrono::Utc::now().naive_utc();
        for chunk in links.chunks(Self::UPSERT_CHUNK_SIZE) {
            let placeholders = chunk
                .iter()
                .map(|_| "(?,?,?,?,?,?)")
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "INSERT INTO {} \
                 (memory_id, entity_id, user_id, source, weight, created_at) \
                 VALUES {placeholders} \
                 ON DUPLICATE KEY UPDATE source = VALUES(source), weight = VALUES(weight)",
                self.t("mem_memory_entity_links"),
            );
            let mut q = sqlx::query(&sql);
            for (memory_id, entity_id, source) in chunk {
                let weight: f32 = match *source {
                    "manual" => 1.0,
                    "llm" => 0.9,
                    _ => 0.8,
                };
                q = q
                    .bind(*memory_id)
                    .bind(*entity_id)
                    .bind(user_id)
                    .bind(*source)
                    .bind(weight)
                    .bind(now);
            }
            q.execute(&self.pool).await.map_err(db_err)?;
        }
        Ok(())
    }

    /// Get entity candidates: memories without entity links (for extract_entities candidates mode).
    pub async fn get_unlinked_memories(
        &self,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<(String, String)>, MemoriaError> {
        // Memories that have no entry in mem_memory_entity_links
        let rows = sqlx::query(&format!(
            "SELECT m.memory_id, m.content FROM {} m \
             WHERE m.user_id = ? AND m.is_active = 1 \
             AND NOT EXISTS (SELECT 1 FROM {} l \
                             WHERE l.memory_id = m.memory_id AND l.user_id = m.user_id) \
             ORDER BY m.created_at DESC LIMIT ?",
            self.t("mem_memories"),
            self.t("mem_memory_entity_links"),
        ))
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let mid: String = r.try_get("memory_id").ok()?;
                let content: String = r.try_get("content").ok()?;
                Some((mid, content))
            })
            .collect::<Vec<_>>())
    }

    pub async fn get_user_entities(
        &self,
        user_id: &str,
    ) -> Result<Vec<(String, String)>, MemoriaError> {
        let rows = sqlx::query(&format!(
            "SELECT name, entity_type FROM {} WHERE user_id = ? ORDER BY name",
            self.t("mem_entities"),
        ))
        .bind(user_id)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let name: String = r.try_get("name").ok()?;
                let etype: String = r.try_get("entity_type").ok()?;
                Some((name, etype))
            })
            .collect::<Vec<_>>())
    }

    // ── Graph retrieval queries ─────────────────────────────────────────

    /// Vector similarity search on graph nodes (cosine via l2_distance).
    pub async fn search_nodes_vector(
        &self,
        user_id: &str,
        embedding: &[f32],
        top_k: i64,
    ) -> Result<Vec<(GraphNode, f32)>, MemoriaError> {
        let vec_lit = format!(
            "[{}]",
            embedding
                .iter()
                .map(|f| f.to_string())
                .collect::<Vec<_>>()
                .join(",")
        );
        let sql = format!(
            "SELECT {NODE_COLS_NO_EMB}, l2_distance(embedding, '{vec_lit}') AS l2_dist \
             FROM {} \
             WHERE user_id = ? AND is_active = 1 \
               AND embedding IS NOT NULL AND vector_dims(embedding) > 0 \
             ORDER BY l2_dist ASC \
             LIMIT ? by rank with option 'mode=post'",
            self.t("memory_graph_nodes"),
        );
        let rows = sqlx::query(&sql)
            .bind(user_id)
            .bind(top_k)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .map(|r| {
                // Convert L2 distance to cosine similarity for normalized vectors:
                // cos = 1 - L2² / 2
                let l2: f32 = r
                    .try_get::<f32, _>("l2_dist")
                    .or_else(|_| r.try_get::<f64, _>("l2_dist").map(|v| v as f32))
                    .unwrap_or(f32::MAX);
                let sim = (1.0 - l2 * l2 / 2.0).max(0.0);
                (row_to_node(r), sim)
            })
            .collect())
    }

    /// Fulltext (BM25) search on graph nodes.
    pub async fn search_nodes_fulltext(
        &self,
        user_id: &str,
        query: &str,
        top_k: i64,
    ) -> Result<Vec<(GraphNode, f32)>, MemoriaError> {
        if query.trim().is_empty() {
            return Ok(vec![]);
        }
        // Sanitize query for MATCH AGAINST — strip boolean-mode operators and SQL chars
        let safe: String = query
            .chars()
            .filter(|c| *c != '\0')
            .map(|c| if "+-<>()~*\"@'\\".contains(c) { ' ' } else { c })
            .collect();
        let safe: String = safe.split_whitespace().collect::<Vec<_>>().join(" ");
        if safe.is_empty() {
            return Ok(vec![]);
        }
        let sql = format!(
            "SELECT {NODE_COLS_NO_EMB}, MATCH(content) AGAINST('{safe}' IN BOOLEAN MODE) AS ft_score \
             FROM {} \
             WHERE user_id = ? AND is_active = 1 \
             AND MATCH(content) AGAINST('{safe}' IN BOOLEAN MODE) \
             ORDER BY ft_score DESC LIMIT ?",
            self.t("memory_graph_nodes"),
        );
        let rows = fulltext_rows_or_empty(
            sqlx::query(&sql)
                .bind(user_id)
                .bind(top_k)
                .fetch_all(&self.pool)
                .await,
        )?;
        Ok(rows
            .iter()
            .map(|r| {
                let score: f32 = r.try_get("ft_score").unwrap_or(0.0);
                (row_to_node_no_emb(r), score)
            })
            .collect())
    }

    /// Find entity_id by exact name match.
    pub async fn find_entity_by_name(
        &self,
        user_id: &str,
        name: &str,
    ) -> Result<Option<String>, MemoriaError> {
        let row = sqlx::query(&format!(
            "SELECT entity_id FROM {} WHERE user_id = ? AND name = ?",
            self.t("mem_entities")
        ))
        .bind(user_id)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(row.and_then(|r| r.try_get("entity_id").ok()))
    }

    /// Batch find entity_ids by names. Returns name → entity_id map.
    pub async fn find_entities_by_names(
        &self,
        user_id: &str,
        names: &[&str],
    ) -> Result<std::collections::HashMap<String, String>, MemoriaError> {
        let mut map = std::collections::HashMap::new();
        if names.is_empty() {
            return Ok(map);
        }
        for chunk in names.chunks(100) {
            let placeholders = chunk.iter().map(|_| "?").collect::<Vec<_>>().join(",");
            let sql = format!(
                "SELECT name, entity_id FROM {} WHERE user_id = ? AND name IN ({placeholders})",
                self.t("mem_entities"),
            );
            let mut q = sqlx::query(&sql).bind(user_id);
            for name in chunk {
                q = q.bind(*name);
            }
            let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
            for r in &rows {
                let n: String = r.try_get("name").unwrap_or_default();
                let eid: String = r.try_get("entity_id").unwrap_or_default();
                map.insert(n, eid);
            }
        }
        Ok(map)
    }

    /// Batch reverse lookup: multiple entity_ids → memory_ids with weights.
    /// `limit_per_entity` caps results per entity; total LIMIT = limit_per_entity × entity count.
    pub async fn get_memories_by_entities(
        &self,
        entity_ids: &[&str],
        user_id: &str,
        limit_per_entity: i64,
    ) -> Result<Vec<(String, f32)>, MemoriaError> {
        if entity_ids.is_empty() {
            return Ok(vec![]);
        }
        let total_limit = limit_per_entity * entity_ids.len() as i64;
        let placeholders = entity_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT l.memory_id, l.weight \
             FROM {} l \
             JOIN {} m ON l.memory_id = m.memory_id \
             WHERE l.entity_id IN ({placeholders}) AND l.user_id = ? AND m.is_active = 1 \
             ORDER BY l.weight DESC, m.created_at DESC \
             LIMIT ?",
            self.t("mem_memory_entity_links"),
            self.t("mem_memories"),
            placeholders = placeholders
        );
        let mut q = sqlx::query(&sql);
        for eid in entity_ids {
            q = q.bind(*eid);
        }
        let rows = q
            .bind(user_id)
            .bind(total_limit)
            .fetch_all(&self.pool)
            .await
            .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let mid: String = r.try_get("memory_id").ok()?;
                let w: f32 = r.try_get("weight").ok()?;
                Some((mid, w))
            })
            .collect())
    }

    /// Reverse lookup: entity → memory_ids with weights.
    pub async fn get_memories_by_entity(
        &self,
        entity_id: &str,
        user_id: &str,
        limit: i64,
    ) -> Result<Vec<(String, f32)>, MemoriaError> {
        let rows = sqlx::query(&format!(
            "SELECT l.memory_id, l.weight \
             FROM {} l \
             JOIN {} m ON l.memory_id = m.memory_id \
             WHERE l.entity_id = ? AND l.user_id = ? AND m.is_active = 1 \
             ORDER BY l.weight DESC, m.created_at DESC \
             LIMIT ?",
            self.t("mem_memory_entity_links"),
            self.t("mem_memories"),
        ))
        .bind(entity_id)
        .bind(user_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let mid: String = r.try_get("memory_id").ok()?;
                let w: f32 = r.try_get("weight").ok()?;
                Some((mid, w))
            })
            .collect())
    }

    /// Bidirectional edge fetch for a set of node IDs.
    /// Returns (incoming, outgoing) maps: node_id → Vec<(peer_id, edge_type, weight)>.
    pub async fn get_edges_bidirectional(
        &self,
        node_ids: &[String],
    ) -> Result<
        (
            std::collections::HashMap<String, Vec<(String, String, f32)>>,
            std::collections::HashMap<String, Vec<(String, String, f32)>>,
        ),
        MemoriaError,
    > {
        if node_ids.is_empty() {
            return Ok((Default::default(), Default::default()));
        }
        let ph = node_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        // Outgoing: source_id IN node_ids, peer = target_id
        // Incoming: target_id IN node_ids, peer = source_id
        let sql = format!(
            "SELECT e.source_id, e.target_id, e.edge_type, e.weight, 0 AS direction \
             FROM {} e \
             JOIN {} n ON n.node_id = e.target_id AND n.is_active = 1 \
             WHERE e.source_id IN ({ph}) \
             UNION ALL \
             SELECT e.source_id, e.target_id, e.edge_type, e.weight, 1 AS direction \
             FROM {} e \
             JOIN {} n ON n.node_id = e.source_id AND n.is_active = 1 \
             WHERE e.target_id IN ({ph})",
            self.t("memory_graph_edges"),
            self.t("memory_graph_nodes"),
            self.t("memory_graph_edges"),
            self.t("memory_graph_nodes"),
        );
        let mut q = sqlx::query(&sql);
        for id in node_ids {
            q = q.bind(id);
        }
        for id in node_ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;

        let mut incoming: std::collections::HashMap<String, Vec<(String, String, f32)>> =
            Default::default();
        let mut outgoing: std::collections::HashMap<String, Vec<(String, String, f32)>> =
            Default::default();
        for r in &rows {
            let src: String = r.try_get("source_id").unwrap_or_default();
            let tgt: String = r.try_get("target_id").unwrap_or_default();
            let etype: String = r.try_get("edge_type").unwrap_or_default();
            let w: f32 = r.try_get("weight").unwrap_or(1.0);
            let dir: i32 = r.try_get("direction").unwrap_or(0);
            if dir == 0 {
                // outgoing: anchor=source_id, peer=target_id
                outgoing.entry(src).or_default().push((tgt, etype, w));
            } else {
                // incoming: anchor=target_id, peer=source_id
                incoming.entry(tgt).or_default().push((src, etype, w));
            }
        }
        Ok((incoming, outgoing))
    }

    /// Deactivate entity links in `mem_memory_entity_links` for a memory_id.
    pub async fn delete_memory_entity_links(&self, memory_id: &str) -> Result<i64, MemoriaError> {
        let r = sqlx::query(&format!(
            "DELETE FROM {} WHERE memory_id = ?",
            self.t("mem_memory_entity_links")
        ))
        .bind(memory_id)
        .execute(&self.pool)
        .await
        .map_err(db_err)?;
        Ok(r.rows_affected() as i64)
    }

    /// Remove orphaned rows from `mem_memory_entity_links` whose memory_id
    /// no longer exists or is inactive in `mem_memories`. Idempotent, batch-safe.
    pub async fn cleanup_orphan_memory_entity_links(&self) -> Result<i64, MemoriaError> {
        // Two-step: find orphan memory_ids, then delete by primary key.
        let orphans: Vec<(String, String)> = sqlx::query_as(&format!(
            "SELECT l.memory_id, l.entity_id FROM {} l \
             LEFT JOIN {} m ON l.memory_id = m.memory_id AND m.is_active = 1 \
             WHERE m.memory_id IS NULL LIMIT 5000",
            self.t("mem_memory_entity_links"),
            self.t("mem_memories"),
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        if orphans.is_empty() {
            return Ok(0);
        }
        let mut total = 0i64;
        for chunk in orphans.chunks(100) {
            let conds = chunk
                .iter()
                .map(|_| "(memory_id = ? AND entity_id = ?)")
                .collect::<Vec<_>>()
                .join(" OR ");
            let sql = format!(
                "DELETE FROM {} WHERE {conds}",
                self.t("mem_memory_entity_links")
            );
            let mut q = sqlx::query(&sql);
            for (mid, eid) in chunk {
                q = q.bind(mid).bind(eid);
            }
            let r = q.execute(&self.pool).await.map_err(db_err)?;
            total += r.rows_affected() as i64;
        }
        Ok(total)
    }

    /// Deactivate graph nodes whose memory_id is inactive in `mem_memories`.
    /// Idempotent fallback for crash recovery.
    pub async fn cleanup_orphan_graph_nodes(&self) -> Result<i64, MemoriaError> {
        // Two-step: find orphan node_ids, then update by primary key.
        let orphans: Vec<(String,)> = sqlx::query_as(&format!(
            "SELECT g.node_id FROM {} g \
             LEFT JOIN {} m ON g.memory_id = m.memory_id \
             WHERE g.is_active = 1 AND g.memory_id IS NOT NULL \
               AND (m.is_active = 0 OR m.memory_id IS NULL) \
             LIMIT 5000",
            self.t("memory_graph_nodes"),
            self.t("mem_memories"),
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(db_err)?;
        if orphans.is_empty() {
            return Ok(0);
        }
        let placeholders = orphans.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "UPDATE {} SET is_active = 0 WHERE node_id IN ({placeholders})",
            self.t("memory_graph_nodes"),
        );
        let mut q = sqlx::query(&sql);
        for (nid,) in &orphans {
            q = q.bind(nid);
        }
        let r = q.execute(&self.pool).await.map_err(db_err)?;
        Ok(r.rows_affected() as i64)
    }

    /// Count active nodes for a user (cached, TTL 2 min).
    pub async fn count_user_nodes(&self, user_id: &str) -> Result<i64, MemoriaError> {
        if let Some(cached) = self.node_count_cache.get(user_id) {
            return Ok(cached);
        }
        let row = sqlx::query(&format!(
            "SELECT COUNT(*) as cnt FROM {} WHERE user_id = ? AND is_active = 1",
            self.t("memory_graph_nodes"),
        ))
        .bind(user_id)
        .fetch_one(&self.pool)
        .await
        .map_err(db_err)?;
        let cnt: i64 = row.try_get("cnt").unwrap_or(0);
        self.node_count_cache.insert(user_id.to_string(), cnt);
        Ok(cnt)
    }

    /// Get edges between a set of node IDs (for connected components).
    pub async fn get_edges_for_nodes(
        &self,
        node_ids: &[String],
    ) -> Result<Vec<(String, String)>, MemoriaError> {
        if node_ids.is_empty() {
            return Ok(vec![]);
        }
        let placeholders = node_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT source_id, target_id FROM {} \
             WHERE source_id IN ({placeholders}) OR target_id IN ({placeholders})",
            self.t("memory_graph_edges"),
        );
        let mut q = sqlx::query(&sql);
        for id in node_ids {
            q = q.bind(id);
        }
        for id in node_ids {
            q = q.bind(id);
        }
        let rows = q.fetch_all(&self.pool).await.map_err(db_err)?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                let src: String = r.try_get("source_id").ok()?;
                let tgt: String = r.try_get("target_id").ok()?;
                Some((src, tgt))
            })
            .collect::<Vec<_>>())
    }
}

// ── Row helpers ──────────────────────────────────────────────────────────────

fn row_to_node(r: &sqlx::mysql::MySqlRow) -> GraphNode {
    let emb_str: Option<String> = r.try_get("embedding").ok().flatten();
    let embedding = emb_str.map(|s| mo_to_vec(&s));
    row_to_node_inner(r, embedding)
}

fn row_to_node_no_emb(r: &sqlx::mysql::MySqlRow) -> GraphNode {
    row_to_node_inner(r, None)
}

fn row_to_node_inner(r: &sqlx::mysql::MySqlRow, embedding: Option<Vec<f32>>) -> GraphNode {
    let node_type_str: String = r.try_get("node_type").unwrap_or_default();
    let source_nodes_str: Option<String> = r.try_get("source_nodes").ok().flatten();
    let source_nodes = source_nodes_str
        .map(|s| s.split(',').map(String::from).collect())
        .unwrap_or_default();
    GraphNode {
        node_id: r.try_get("node_id").unwrap_or_default(),
        user_id: r.try_get("user_id").unwrap_or_default(),
        node_type: node_type_str.parse().unwrap(),
        content: r.try_get("content").unwrap_or_default(),
        entity_type: nullable_str_from_row(
            r.try_get::<Option<String>, _>("entity_type").ok().flatten(),
        ),
        embedding,
        memory_id: nullable_str_from_row(
            r.try_get::<Option<String>, _>("memory_id").ok().flatten(),
        ),
        session_id: nullable_str_from_row(
            r.try_get::<Option<String>, _>("session_id").ok().flatten(),
        ),
        confidence: r.try_get::<f32, _>("confidence").unwrap_or(0.75),
        trust_tier: r.try_get("trust_tier").unwrap_or_else(|_| "T3".to_string()),
        importance: r.try_get::<f32, _>("importance").unwrap_or(0.0),
        source_nodes,
        conflicts_with: nullable_str_from_row(
            r.try_get::<Option<String>, _>("conflicts_with")
                .ok()
                .flatten(),
        ),
        conflict_resolution: nullable_str_from_row(
            r.try_get::<Option<String>, _>("conflict_resolution")
                .ok()
                .flatten(),
        ),
        access_count: r.try_get::<i32, _>("access_count").unwrap_or(0),
        cross_session_count: r.try_get::<i32, _>("cross_session_count").unwrap_or(0),
        is_active: r.try_get::<i16, _>("is_active").unwrap_or(1) != 0,
        superseded_by: nullable_str_from_row(
            r.try_get::<Option<String>, _>("superseded_by")
                .ok()
                .flatten(),
        ),
        created_at: r.try_get("created_at").ok(),
    }
}

fn mo_to_vec(s: &str) -> Vec<f32> {
    let s = s.trim_start_matches('[').trim_end_matches(']');
    s.split(',').filter_map(|x| x.trim().parse().ok()).collect()
}
