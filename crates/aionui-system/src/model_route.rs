use std::collections::HashSet;
use std::sync::Arc;

use aionui_api_types::{
    CreateModelRouteRequest, EffectiveModelAssignmentResponse, ModelRouteMemberHealthRequest, ModelRouteMemberResponse,
    ModelRouteResponse, UpdateModelRouteRequest, UpsertModelRouteMemberRequest,
};
use aionui_common::{generate_prefixed_id, now_ms};
use aionui_db::{
    CreateModelRouteParams, IModelRouteRepository, IProviderRepository, ModelRouteMemberRow, ModelRouteRow,
    RecordModelRouteMetricParams, UpdateModelRouteParams, UpsertModelRouteMemberParams,
};

use crate::SystemError;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelRequirements {
    pub capabilities: Vec<String>,
    pub context_tokens: i64,
    pub protocol: Option<String>,
    pub run_id: String,
    pub user_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelMemberLease {
    pub assignment: EffectiveModelAssignmentResponse,
    pub accounted: bool,
    pub run_id: String,
    pub user_id: String,
    pub acquired_at: i64,
}

#[derive(Clone)]
pub struct ModelRouteService {
    routes: Arc<dyn IModelRouteRepository>,
    providers: Arc<dyn IProviderRepository>,
}

impl ModelRouteService {
    pub fn new(routes: Arc<dyn IModelRouteRepository>, providers: Arc<dyn IProviderRepository>) -> Self {
        Self { routes, providers }
    }

    pub async fn list_routes(&self) -> Result<Vec<ModelRouteResponse>, SystemError> {
        self.routes
            .list_routes()
            .await?
            .into_iter()
            .map(route_response)
            .collect()
    }

    pub async fn create_route(&self, request: CreateModelRouteRequest) -> Result<ModelRouteResponse, SystemError> {
        if request.name.trim().is_empty() {
            return Err(SystemError::BadRequest("route name is required".into()));
        }
        let id = generate_prefixed_id("route");
        let capabilities = serde_json::to_string(&request.required_capabilities)
            .map_err(|error| SystemError::BadRequest(error.to_string()))?;
        route_response(
            self.routes
                .create_route(&CreateModelRouteParams {
                    id: &id,
                    name: request.name.trim(),
                    enabled: request.enabled,
                    required_capabilities: &capabilities,
                    fallback_after_ms: request.fallback_after_ms.clamp(1_000, 300_000) as i64,
                })
                .await?,
        )
    }

    pub async fn update_route(
        &self,
        id: &str,
        request: UpdateModelRouteRequest,
    ) -> Result<ModelRouteResponse, SystemError> {
        let capabilities = request
            .required_capabilities
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(|error| SystemError::BadRequest(error.to_string()))?;
        route_response(
            self.routes
                .update_route(
                    id,
                    &UpdateModelRouteParams {
                        name: request.name.as_deref(),
                        enabled: request.enabled,
                        required_capabilities: capabilities.as_deref(),
                        fallback_after_ms: request
                            .fallback_after_ms
                            .map(|value| value.clamp(1_000, 300_000) as i64),
                    },
                )
                .await?,
        )
    }

    pub async fn delete_route(&self, id: &str) -> Result<(), SystemError> {
        self.routes.delete_route(id).await?;
        Ok(())
    }

    pub async fn list_members(&self, route_id: &str) -> Result<Vec<ModelRouteMemberResponse>, SystemError> {
        Ok(self
            .routes
            .list_members(route_id)
            .await?
            .into_iter()
            .map(member_response)
            .collect())
    }

    pub async fn upsert_member(
        &self,
        route_id: &str,
        request: UpsertModelRouteMemberRequest,
    ) -> Result<ModelRouteMemberResponse, SystemError> {
        if !matches!(request.tier.as_str(), "primary" | "fallback") {
            return Err(SystemError::BadRequest(
                "member tier must be primary or fallback".into(),
            ));
        }
        if self.providers.find_by_id(&request.provider_id).await?.is_none() {
            return Err(SystemError::BadRequest("physical provider does not exist".into()));
        }
        if self.routes.find_route(route_id).await?.is_none() {
            return Err(SystemError::NotFound("model route not found".into()));
        }
        let id = request.id.unwrap_or_else(|| generate_prefixed_id("route_member"));
        Ok(member_response(
            self.routes
                .upsert_member(&UpsertModelRouteMemberParams {
                    id: &id,
                    route_id,
                    provider_id: &request.provider_id,
                    model: &request.model,
                    tier: &request.tier,
                    weight: request.weight.max(1) as i64,
                    max_concurrency: request.max_concurrency.max(1) as i64,
                    rpm_limit: request.rpm_limit.map(i64::from),
                    tpm_limit: request.tpm_limit.map(i64::from),
                    enabled: request.enabled,
                })
                .await?,
        ))
    }

    pub async fn delete_member(&self, member_id: &str) -> Result<(), SystemError> {
        self.routes.delete_member(member_id).await?;
        Ok(())
    }

    pub async fn reset_member(&self, member_id: &str) -> Result<ModelRouteMemberResponse, SystemError> {
        Ok(member_response(
            self.routes.reset_member_breaker(member_id, now_ms()).await?,
        ))
    }

    pub async fn record_member_failure(
        &self,
        member_id: &str,
        request: ModelRouteMemberHealthRequest,
    ) -> Result<ModelRouteMemberResponse, SystemError> {
        Ok(member_response(
            self.routes
                .record_member_failure(
                    member_id,
                    request.http_status,
                    request.retry_after_ms,
                    request.balance_exhausted,
                    now_ms(),
                )
                .await?,
        ))
    }

    pub async fn acquire(
        &self,
        route_id: &str,
        conversation_id: &str,
        requirements: &ModelRequirements,
        primary_waited_ms: u64,
    ) -> Result<ModelMemberLease, SystemError> {
        let route = self
            .routes
            .find_route(route_id)
            .await?
            .filter(|route| route.enabled)
            .ok_or_else(|| SystemError::NotFound("model route is disabled or missing".into()))?;
        let mut required: HashSet<String> = serde_json::from_str::<Vec<String>>(&route.required_capabilities)
            .unwrap_or_default()
            .into_iter()
            .collect();
        required.extend(requirements.capabilities.iter().cloned());
        let members = self.routes.list_members(route_id).await?;

        if let Some(assignment) = self.routes.find_assignment(conversation_id).await?
            && assignment.route_id == route_id
            && let Some(member) = members.iter().find(|member| member.id == assignment.member_id)
            && self.member_compatible(member, &required, requirements).await?
            && self.routes.try_acquire_member(&member.id, now_ms()).await?
        {
            return Ok(ModelMemberLease {
                assignment: EffectiveModelAssignmentResponse {
                    route_id: route_id.to_owned(),
                    member_id: member.id.clone(),
                    provider_id: member.provider_id.clone(),
                    model: member.model.clone(),
                    fallback_used: assignment.fallback_used,
                },
                accounted: true,
                run_id: requirements.run_id.clone(),
                user_id: requirements.user_id.clone(),
                acquired_at: now_ms(),
            });
        }

        let mut compatible = Vec::new();
        for member in &members {
            if self.member_compatible(member, &required, requirements).await? {
                compatible.push(member.clone());
            }
        }
        if compatible.is_empty() {
            return Err(SystemError::UnprocessableEntity("NO_COMPATIBLE_MODEL".into()));
        }
        let now = now_ms();
        let mut primary = compatible
            .iter()
            .filter(|member| member.tier == "primary" && member_available(member, now))
            .cloned()
            .collect::<Vec<_>>();
        sort_by_normalized_load(&mut primary);
        let allow_fallback = primary_waited_ms >= route.fallback_after_ms.max(0) as u64;
        let candidates = if primary.is_empty() && allow_fallback {
            let mut fallback = compatible
                .into_iter()
                .filter(|member| member.tier == "fallback" && member_available(member, now))
                .collect::<Vec<_>>();
            sort_by_normalized_load(&mut fallback);
            fallback
        } else {
            primary
        };
        let fallback_used = candidates.first().is_some_and(|member| member.tier == "fallback");
        for member in candidates {
            if self.routes.try_acquire_member(&member.id, now).await? {
                let assignment = self
                    .routes
                    .upsert_assignment(conversation_id, route_id, &member, fallback_used)
                    .await?;
                return Ok(ModelMemberLease {
                    assignment: EffectiveModelAssignmentResponse {
                        route_id: assignment.route_id,
                        member_id: assignment.member_id,
                        provider_id: assignment.provider_id,
                        model: assignment.model,
                        fallback_used: assignment.fallback_used,
                    },
                    accounted: true,
                    run_id: requirements.run_id.clone(),
                    user_id: requirements.user_id.clone(),
                    acquired_at: now,
                });
            }
        }
        Err(SystemError::Conflict("PROVIDER_POOL_EXHAUSTED".into()))
    }

    pub async fn release(&self, lease: &ModelMemberLease, tokens: i64) -> Result<(), SystemError> {
        if !lease.accounted {
            return Ok(());
        }
        self.routes
            .release_member(&lease.assignment.member_id, tokens, now_ms())
            .await?;
        self.record_lease_metric(lease, tokens, true, None).await?;
        Ok(())
    }

    pub async fn record_lease_failure(
        &self,
        lease: &ModelMemberLease,
        request: ModelRouteMemberHealthRequest,
        error_code: Option<&str>,
    ) -> Result<(), SystemError> {
        self.routes
            .record_member_failure(
                &lease.assignment.member_id,
                request.http_status,
                request.retry_after_ms,
                request.balance_exhausted,
                now_ms(),
            )
            .await?;
        self.record_lease_metric(lease, 0, false, error_code).await
    }

    async fn record_lease_metric(
        &self,
        lease: &ModelMemberLease,
        tokens: i64,
        success: bool,
        error_code: Option<&str>,
    ) -> Result<(), SystemError> {
        if lease.run_id.is_empty() || lease.user_id.is_empty() {
            return Ok(());
        }
        let created_at = now_ms();
        self.routes
            .record_metric(&RecordModelRouteMetricParams {
                id: &generate_prefixed_id("route_metric"),
                run_id: &lease.run_id,
                user_id: &lease.user_id,
                route_id: &lease.assignment.route_id,
                member_id: &lease.assignment.member_id,
                tokens,
                latency_ms: created_at.saturating_sub(lease.acquired_at),
                success,
                fallback_used: lease.assignment.fallback_used,
                error_code,
                created_at,
            })
            .await?;
        Ok(())
    }

    /// Resolve a previously sticky physical assignment without enforcing the
    /// route pool. Used only by the emergency model-route rollback switch.
    pub async fn direct_assignment(
        &self,
        route_id: &str,
        conversation_id: &str,
    ) -> Result<ModelMemberLease, SystemError> {
        let assignment = self
            .routes
            .find_assignment(conversation_id)
            .await?
            .filter(|assignment| assignment.route_id == route_id)
            .ok_or_else(|| SystemError::Conflict("MODEL_ROUTE_DISABLED_WITHOUT_ASSIGNMENT".into()))?;
        let member = self
            .routes
            .find_member(&assignment.member_id)
            .await?
            .filter(|member| member.enabled)
            .ok_or_else(|| SystemError::Conflict("MODEL_ROUTE_ASSIGNMENT_UNAVAILABLE".into()))?;
        let provider = self
            .providers
            .find_by_id(&member.provider_id)
            .await?
            .filter(|provider| provider.enabled)
            .ok_or_else(|| SystemError::Conflict("MODEL_ROUTE_PROVIDER_UNAVAILABLE".into()))?;
        Ok(ModelMemberLease {
            assignment: EffectiveModelAssignmentResponse {
                route_id: assignment.route_id,
                member_id: member.id,
                provider_id: provider.id,
                model: assignment.model,
                fallback_used: assignment.fallback_used,
            },
            accounted: false,
            run_id: String::new(),
            user_id: String::new(),
            acquired_at: now_ms(),
        })
    }

    pub fn may_retry(visible_output: bool, tool_executed: bool) -> bool {
        !visible_output && !tool_executed
    }

    async fn member_compatible(
        &self,
        member: &ModelRouteMemberRow,
        required: &HashSet<String>,
        requirements: &ModelRequirements,
    ) -> Result<bool, SystemError> {
        if !member.enabled || member.disabled_reason.is_some() {
            return Ok(false);
        }
        let Some(provider) = self.providers.find_by_id(&member.provider_id).await? else {
            return Ok(false);
        };
        if !provider.enabled
            || provider
                .context_limit
                .is_some_and(|limit| limit < requirements.context_tokens)
        {
            return Ok(false);
        }
        let capabilities: serde_json::Value = serde_json::from_str(&provider.capabilities).unwrap_or_default();
        if required
            .iter()
            .any(|required| !json_has_capability(&capabilities, required))
        {
            return Ok(false);
        }
        if let Some(protocol) = requirements.protocol.as_deref() {
            let protocols: serde_json::Value = provider
                .model_protocols
                .as_deref()
                .and_then(|value| serde_json::from_str(value).ok())
                .unwrap_or_default();
            if protocols.get(&member.model).and_then(|value| value.as_str()) != Some(protocol) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

fn route_response(row: ModelRouteRow) -> Result<ModelRouteResponse, SystemError> {
    Ok(ModelRouteResponse {
        id: row.id,
        name: row.name,
        enabled: row.enabled,
        required_capabilities: serde_json::from_str(&row.required_capabilities)
            .map_err(|error| SystemError::Internal(error.to_string()))?,
        fallback_after_ms: row.fallback_after_ms.max(0) as u64,
    })
}

fn member_response(row: ModelRouteMemberRow) -> ModelRouteMemberResponse {
    ModelRouteMemberResponse {
        id: row.id,
        route_id: row.route_id,
        provider_id: row.provider_id,
        model: row.model,
        tier: row.tier,
        weight: row.weight.max(1) as u32,
        max_concurrency: row.max_concurrency.max(1) as u32,
        rpm_limit: row.rpm_limit.map(|value| value.max(0) as u32),
        tpm_limit: row.tpm_limit.map(|value| value.max(0) as u32),
        enabled: row.enabled,
        disabled_reason: row.disabled_reason,
        cooldown_until: row.cooldown_until,
        active_count: row.active_count.max(0) as u32,
    }
}

fn member_available(member: &ModelRouteMemberRow, now: i64) -> bool {
    member.enabled
        && member.disabled_reason.is_none()
        && member.cooldown_until.is_none_or(|until| until <= now)
        && member.active_count < member.max_concurrency
        && (member.rpm_limit.is_none()
            || member
                .request_window_started_at
                .is_none_or(|start| start <= now - 60_000)
            || member.request_count < member.rpm_limit.unwrap_or_default())
        && (member.tpm_limit.is_none()
            || member
                .request_window_started_at
                .is_none_or(|start| start <= now - 60_000)
            || member.token_count < member.tpm_limit.unwrap_or_default())
}

fn sort_by_normalized_load(members: &mut [ModelRouteMemberRow]) {
    members.sort_by(|left, right| {
        let left_load =
            (left.active_count.max(0) + 1) as f64 / left.max_concurrency.max(1) as f64 / left.weight.max(1) as f64;
        let right_load =
            (right.active_count.max(0) + 1) as f64 / right.max_concurrency.max(1) as f64 / right.weight.max(1) as f64;
        left_load.total_cmp(&right_load).then_with(|| left.id.cmp(&right.id))
    });
}

fn json_has_capability(value: &serde_json::Value, capability: &str) -> bool {
    match value {
        serde_json::Value::String(value) => value.eq_ignore_ascii_case(capability),
        serde_json::Value::Array(items) => items.iter().any(|item| json_has_capability(item, capability)),
        serde_json::Value::Object(map) => {
            map.get(capability).and_then(|value| value.as_bool()) == Some(true)
                || map.values().any(|value| json_has_capability(value, capability))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::{CreateModelRouteRequest, UpsertModelRouteMemberRequest};
    use aionui_db::{
        CreateProviderParams, IConversationRepository, IProviderRepository, IUserRepository,
        SqliteConversationRepository, SqliteModelRouteRepository, SqliteProviderRepository, SqliteUserRepository,
        init_database_memory, models::ConversationRow,
    };

    fn member(id: &str, active: i64, max: i64, weight: i64) -> ModelRouteMemberRow {
        ModelRouteMemberRow {
            id: id.into(),
            route_id: "r".into(),
            provider_id: "p".into(),
            model: "m".into(),
            tier: "primary".into(),
            weight,
            max_concurrency: max,
            rpm_limit: None,
            tpm_limit: None,
            enabled: true,
            disabled_reason: None,
            cooldown_until: None,
            consecutive_5xx: 0,
            consecutive_429: 0,
            active_count: active,
            request_window_started_at: None,
            request_count: 0,
            token_count: 0,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn weighted_normalized_load_prefers_capacity_and_weight() {
        let mut members = vec![member("loaded", 1, 2, 1), member("weighted", 0, 1, 4)];
        sort_by_normalized_load(&mut members);
        assert_eq!(members[0].id, "weighted");
    }

    #[test]
    fn retry_is_forbidden_after_visible_output_or_tool_side_effect() {
        assert!(ModelRouteService::may_retry(false, false));
        assert!(!ModelRouteService::may_retry(true, false));
        assert!(!ModelRouteService::may_retry(false, true));
    }

    #[tokio::test]
    async fn primary_capacity_falls_back_and_assignment_stays_sticky() {
        let database = init_database_memory().await.unwrap();
        let users = SqliteUserRepository::new(database.pool().clone());
        let user = users.create_user("route-user", "hash").await.unwrap();
        let conversations = SqliteConversationRepository::new(database.pool().clone());
        for id in ["route-conv-1", "route-conv-2"] {
            conversations
                .create(&ConversationRow {
                    id: id.into(),
                    user_id: user.id.clone(),
                    name: id.into(),
                    r#type: "aionrs".into(),
                    extra: "{}".into(),
                    model: None,
                    status: Some("finished".into()),
                    source: None,
                    channel_chat_id: None,
                    pinned: false,
                    pinned_at: None,
                    created_at: now_ms(),
                    updated_at: now_ms(),
                })
                .await
                .unwrap();
        }
        let provider_repo = Arc::new(SqliteProviderRepository::new(database.pool().clone()));
        for id in ["provider-primary", "provider-fallback"] {
            provider_repo
                .create(CreateProviderParams {
                    id: Some(id),
                    platform: "openai",
                    name: id,
                    base_url: "https://example.invalid/v1",
                    api_key_encrypted: "encrypted",
                    models: r#"["model"]"#,
                    enabled: true,
                    capabilities: r#"["function_calling","vision"]"#,
                    context_limit: Some(128_000),
                    model_protocols: None,
                    model_enabled: None,
                    model_health: None,
                    bedrock_config: None,
                    is_full_url: false,
                })
                .await
                .unwrap();
        }
        let service = ModelRouteService::new(
            Arc::new(SqliteModelRouteRepository::new(database.pool().clone())),
            provider_repo,
        );
        let route = service
            .create_route(CreateModelRouteRequest {
                name: "Fast".into(),
                enabled: true,
                required_capabilities: vec!["function_calling".into()],
                fallback_after_ms: 1_000,
            })
            .await
            .unwrap();
        for (id, provider_id, tier) in [
            ("primary", "provider-primary", "primary"),
            ("fallback", "provider-fallback", "fallback"),
        ] {
            service
                .upsert_member(
                    &route.id,
                    UpsertModelRouteMemberRequest {
                        id: Some(id.into()),
                        provider_id: provider_id.into(),
                        model: "model".into(),
                        tier: tier.into(),
                        weight: 1,
                        max_concurrency: 1,
                        rpm_limit: None,
                        tpm_limit: None,
                        enabled: true,
                    },
                )
                .await
                .unwrap();
        }
        let requirements = ModelRequirements {
            capabilities: vec!["function_calling".into()],
            ..Default::default()
        };
        let primary = service
            .acquire(&route.id, "route-conv-1", &requirements, 0)
            .await
            .unwrap();
        assert_eq!(primary.assignment.member_id, "primary");

        let fallback = service
            .acquire(&route.id, "route-conv-2", &requirements, 1_000)
            .await
            .unwrap();
        assert_eq!(fallback.assignment.member_id, "fallback");
        assert!(fallback.assignment.fallback_used);

        service.release(&primary, 0).await.unwrap();
        let sticky = service
            .acquire(&route.id, "route-conv-1", &requirements, 1_000)
            .await
            .unwrap();
        assert_eq!(sticky.assignment.member_id, "primary");

        let failed = service
            .record_member_failure(
                "fallback",
                ModelRouteMemberHealthRequest {
                    retry_after_ms: Some(60_000),
                    http_status: Some(429),
                    balance_exhausted: false,
                },
            )
            .await
            .unwrap();
        assert!(failed.cooldown_until.is_some_and(|until| until > now_ms()));
    }
}
