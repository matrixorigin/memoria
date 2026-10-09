use crate::{MemoriaError, Memory};
use async_trait::async_trait;

/// Core storage trait — implemented by memoria-storage.
#[async_trait]
pub trait MemoryStore: Send + Sync {
    async fn insert(&self, memory: &Memory) -> Result<(), MemoriaError>;
    async fn get(&self, memory_id: &str) -> Result<Option<Memory>, MemoriaError>;
    /// Internal history lookup, including soft-deleted/superseded records.
    /// The SQL implementation reads only the main `mem_memories` table, not
    /// branch tables. Use branch-aware store methods for branch history.
    /// Callers must enforce user/subject scope before using the returned content.
    async fn get_including_inactive(&self, memory_id: &str)
        -> Result<Option<Memory>, MemoriaError>;
    async fn update(&self, memory: &Memory) -> Result<(), MemoriaError>;
    async fn soft_delete(&self, memory_id: &str) -> Result<(), MemoriaError>;
    async fn list_active(&self, user_id: &str, limit: i64) -> Result<Vec<Memory>, MemoriaError>;
    async fn search_fulltext(
        &self,
        user_id: &str,
        query: &str,
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError>;
    async fn search_vector(
        &self,
        user_id: &str,
        embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<Memory>, MemoriaError>;
}

/// Embedding provider trait — implemented by memoria-embedding.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    async fn embed(&self, text: &str) -> Result<Vec<f32>, MemoriaError>;

    /// Embed multiple texts in one call. Default: sequential fallback.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, MemoriaError> {
        let mut results = Vec::with_capacity(texts.len());
        for t in texts {
            results.push(self.embed(t).await?);
        }
        Ok(results)
    }

    fn dimension(&self) -> usize;
}
