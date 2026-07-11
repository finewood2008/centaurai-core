#![warn(clippy::disallowed_types)]

mod error;
mod ports;
mod provider_executor;
mod repository;
mod routes;
mod service;

pub use error::DecisionError;
pub use ports::{
    BrainCatalogPort, BrainExecutionFailure, BrainExecutionFailureKind, BrainExecutionPort, BrainInvocation,
    BrainOpinion, DecisionKnowledgeFailure, DecisionKnowledgePort, DecisionKnowledgeRequest, NoopDecisionKnowledge,
};
pub use provider_executor::ProviderBrainRuntime;
pub use repository::DecisionRepository;
pub use routes::{DecisionRouterState, decision_routes};
pub use service::DecisionService;
