#![warn(clippy::disallowed_types)]

//! Private-worker boundary and public knowledge API for CentaurAI Core.
//!
//! The worker transport is selected only from trusted process configuration.
//! Public requests can choose spaces and retrieval policy, but can never
//! provide an endpoint, socket path, or internal bearer token.

mod client;
mod error;
mod gateway;
mod routes;
mod supervisor;

pub use error::{KnowledgeConfigError, KnowledgeError, KnowledgeLifecycleError};
pub use gateway::{
    KnowledgeGateway, ModelLocation, ModelLocationResolver, UnknownModelLocationResolver, augment_model_prompt,
};
pub use routes::{KnowledgeRouterState, knowledge_routes};
pub use supervisor::KnowledgeWorkerSupervisor;
