use aionui_api_types::{
    BrainDefinition, DecisionEvidenceInput, DecisionToolDefinition, RetrievalBundle, RoleDefinition,
};
use std::any::Any;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub struct BrainInvocation {
    pub decision_id: String,
    pub session_id: String,
    pub brain: BrainDefinition,
    pub question: String,
    pub role: RoleDefinition,
    pub tools: Vec<DecisionToolDefinition>,
    pub interjections: Vec<String>,
    pub evidence: Vec<DecisionEvidenceInput>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BrainOpinion {
    pub content: String,
    pub evidence: Vec<DecisionEvidenceInput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrainExecutionFailureKind {
    Timeout,
    RateLimited,
    Unauthorized,
    Unsupported,
    Unavailable,
    Cancelled,
}

impl BrainExecutionFailureKind {
    pub fn code(self) -> &'static str {
        match self {
            Self::Timeout => "BRAIN_TIMEOUT",
            Self::RateLimited => "BRAIN_RATE_LIMITED",
            Self::Unauthorized => "BRAIN_UNAUTHORIZED",
            Self::Unsupported => "BRAIN_UNSUPPORTED",
            Self::Unavailable => "BRAIN_UNAVAILABLE",
            Self::Cancelled => "BRAIN_CANCELLED",
        }
    }

    pub fn retryable(self) -> bool {
        matches!(self, Self::Timeout | Self::RateLimited | Self::Unavailable)
    }
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("{kind:?}: {message}")]
pub struct BrainExecutionFailure {
    pub kind: BrainExecutionFailureKind,
    pub message: String,
}

impl BrainExecutionFailure {
    pub fn new(kind: BrainExecutionFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

#[async_trait::async_trait]
pub trait BrainCatalogPort: Send + Sync {
    async fn available_brains(&self) -> Result<Vec<BrainDefinition>, BrainExecutionFailure>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrainLocation {
    Local,
    External,
    Unknown,
}

/// Immutable, executor-owned snapshot of the endpoint and credentials used by
/// one attempt. The service gates evidence against `location` from this same
/// snapshot, eliminating a provider-update gap between classification and I/O.
pub struct BrainExecutionPlan {
    pub brain: BrainDefinition,
    pub location: BrainLocation,
    payload: Arc<dyn Any + Send + Sync>,
}

impl BrainExecutionPlan {
    pub fn new<T>(brain: BrainDefinition, location: BrainLocation, payload: T) -> Self
    where
        T: Any + Send + Sync,
    {
        Self {
            brain,
            location,
            payload: Arc::new(payload),
        }
    }

    pub fn payload<T: Any + Send + Sync>(&self) -> Option<&T> {
        self.payload.downcast_ref()
    }
}

#[async_trait::async_trait]
pub trait BrainExecutionPort: Send + Sync {
    async fn prepare(&self, brain: &BrainDefinition) -> Result<BrainExecutionPlan, BrainExecutionFailure>;

    async fn execute(
        &self,
        plan: BrainExecutionPlan,
        invocation: BrainInvocation,
    ) -> Result<BrainOpinion, BrainExecutionFailure>;
}

#[derive(Debug, Clone)]
pub struct DecisionKnowledgeRequest {
    pub user_id: String,
    pub decision_id: Option<String>,
    pub question: String,
    pub policy: serde_json::Value,
}

#[derive(Debug, Clone, thiserror::Error)]
#[error("knowledge retrieval failed: {message}")]
pub struct DecisionKnowledgeFailure {
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct DecisionKnowledgeResult {
    pub evidence: Vec<DecisionEvidenceInput>,
    pub retrieval: Option<RetrievalBundle>,
}

impl DecisionKnowledgeFailure {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[async_trait::async_trait]
pub trait DecisionKnowledgePort: Send + Sync {
    async fn retrieve(
        &self,
        request: DecisionKnowledgeRequest,
    ) -> Result<DecisionKnowledgeResult, DecisionKnowledgeFailure>;
}

/// Safe default until the application composes the Knowledge Gateway adapter.
/// It never fabricates citations and therefore returns no hits.
pub struct NoopDecisionKnowledge;

#[async_trait::async_trait]
impl DecisionKnowledgePort for NoopDecisionKnowledge {
    async fn retrieve(
        &self,
        _request: DecisionKnowledgeRequest,
    ) -> Result<DecisionKnowledgeResult, DecisionKnowledgeFailure> {
        Ok(DecisionKnowledgeResult::default())
    }
}
