use std::time::{Duration, Instant};

use aionui_conversation::{
    ConversationError, ConversationModelLease, ConversationModelRequirements, ConversationModelRouteResolver,
};
use aionui_system::{ModelRequirements, ModelRouteService, SystemError};
use async_trait::async_trait;

pub struct AppModelRouteResolver {
    service: ModelRouteService,
}

impl AppModelRouteResolver {
    pub fn new(service: ModelRouteService) -> Self {
        Self { service }
    }
}

#[async_trait]
impl ConversationModelRouteResolver for AppModelRouteResolver {
    async fn acquire(
        &self,
        route_id: &str,
        conversation_id: &str,
        requirements: ConversationModelRequirements,
        primary_waited_ms: u64,
    ) -> Result<ConversationModelLease, ConversationError> {
        let started = Instant::now();
        let requirements = ModelRequirements {
            capabilities: [
                requirements.function_calling.then_some("function_calling".to_owned()),
                requirements.vision.then_some("vision".to_owned()),
            ]
            .into_iter()
            .flatten()
            .collect(),
            context_tokens: requirements.context_tokens,
            protocol: requirements.protocol,
            run_id: requirements.run_id,
            user_id: requirements.user_id,
        };
        if matches!(
            std::env::var("AIONUI_MODEL_ROUTES_ENABLED").as_deref(),
            Ok("0" | "false" | "off")
        ) {
            return self
                .service
                .direct_assignment(route_id, conversation_id)
                .await
                .map(to_conversation_lease)
                .map_err(map_route_error);
        }
        loop {
            let waited = primary_waited_ms.saturating_add(started.elapsed().as_millis() as u64);
            match self
                .service
                .acquire(route_id, conversation_id, &requirements, waited)
                .await
            {
                Ok(lease) => return Ok(to_conversation_lease(lease)),
                Err(SystemError::Conflict(reason)) if reason == "PROVIDER_POOL_EXHAUSTED" && waited < 15_000 => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(error) => return Err(map_route_error(error)),
            }
        }
    }

    async fn release(&self, lease: &ConversationModelLease, tokens: i64) {
        let lease = aionui_system::ModelMemberLease {
            assignment: aionui_api_types::EffectiveModelAssignmentResponse {
                route_id: lease.route_id.clone(),
                member_id: lease.member_id.clone(),
                provider_id: lease.provider_id.clone(),
                model: lease.model.clone(),
                fallback_used: lease.fallback_used,
            },
            accounted: lease.accounted,
            run_id: lease.run_id.clone(),
            user_id: lease.user_id.clone(),
            acquired_at: lease.acquired_at,
        };
        if let Err(error) = self.service.release(&lease, tokens).await {
            tracing::warn!(error = %error, "failed to release logical model member lease");
        }
    }

    async fn record_failure(&self, lease: &ConversationModelLease, message: &str) {
        let lower = message.to_ascii_lowercase();
        let http_status = [401_u16, 403, 429, 500, 502, 503, 504]
            .into_iter()
            .find(|status| lower.contains(&status.to_string()));
        let request = aionui_api_types::ModelRouteMemberHealthRequest {
            retry_after_ms: None,
            http_status,
            balance_exhausted: lower.contains("insufficient quota")
                || lower.contains("balance")
                || lower.contains("billing"),
        };
        let system_lease = aionui_system::ModelMemberLease {
            assignment: aionui_api_types::EffectiveModelAssignmentResponse {
                route_id: lease.route_id.clone(),
                member_id: lease.member_id.clone(),
                provider_id: lease.provider_id.clone(),
                model: lease.model.clone(),
                fallback_used: lease.fallback_used,
            },
            accounted: lease.accounted,
            run_id: lease.run_id.clone(),
            user_id: lease.user_id.clone(),
            acquired_at: lease.acquired_at,
        };
        if let Err(error) = self
            .service
            .record_lease_failure(&system_lease, request, Some("PROVIDER_FAILURE"))
            .await
        {
            tracing::warn!(error = %error, "failed to record logical model member failure");
        }
    }
}

fn to_conversation_lease(lease: aionui_system::ModelMemberLease) -> ConversationModelLease {
    ConversationModelLease {
        route_id: lease.assignment.route_id,
        member_id: lease.assignment.member_id,
        provider_id: lease.assignment.provider_id,
        model: lease.assignment.model,
        fallback_used: lease.assignment.fallback_used,
        accounted: lease.accounted,
        run_id: lease.run_id,
        user_id: lease.user_id,
        acquired_at: lease.acquired_at,
    }
}

fn map_route_error(error: SystemError) -> ConversationError {
    match error {
        SystemError::UnprocessableEntity(reason) if reason == "NO_COMPATIBLE_MODEL" => ConversationError::Capacity {
            code: "NO_COMPATIBLE_MODEL",
            reason: "No administrator-approved model satisfies this turn's capabilities".into(),
        },
        SystemError::Conflict(reason) if reason == "PROVIDER_POOL_EXHAUSTED" => ConversationError::Capacity {
            code: "PROVIDER_POOL_EXHAUSTED",
            reason: "All compatible provider accounts are currently unavailable".into(),
        },
        SystemError::NotFound(reason) => ConversationError::Capacity {
            code: "NO_COMPATIBLE_MODEL",
            reason,
        },
        other => ConversationError::Internal {
            reason: other.to_string(),
        },
    }
}
