#[derive(Debug, thiserror::Error)]
pub enum DecisionError {
    #[error("Decision not found")]
    NotFound,
    #[error("Invalid decision request: {0}")]
    InvalidRequest(String),
    #[error("Decision state conflict: {0}")]
    Conflict(String),
    #[error("Decision provider unavailable: {0}")]
    ProviderUnavailable(String),
    #[error("Decision knowledge unavailable: {0}")]
    KnowledgeUnavailable(String),
    #[error("Database error: {0}")]
    Database(#[from] sqlx::Error),
    #[error("Serialization error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("Internal decision error: {0}")]
    Internal(String),
}
