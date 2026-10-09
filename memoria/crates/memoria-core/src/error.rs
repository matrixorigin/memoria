use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum MemoriaError {
    #[error("Invalid memory type: {0}")]
    InvalidMemoryType(String),

    #[error("Invalid trust tier: {0}")]
    InvalidTrustTier(String),

    #[error("Memory not found: {0}")]
    NotFound(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Embedding error: {0}")]
    Embedding(String),

    #[error("Internal error: {0}")]
    Internal(String),

    /// Observe extraction failed before any memory persistence was attempted.
    #[error("Observe extraction unavailable: {0}")]
    ObserveExtractionUnavailable(String),

    #[error("Validation error: {0}")]
    Validation(String),

    #[error("Blocked: {0}")]
    Blocked(String),
}

impl From<serde_json::Error> for MemoriaError {
    fn from(e: serde_json::Error) -> Self {
        MemoriaError::Serialization(e.to_string())
    }
}
