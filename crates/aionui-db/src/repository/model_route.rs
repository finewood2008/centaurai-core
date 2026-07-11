use crate::DbError;
use crate::models::{ConversationModelAssignmentRow, ModelRouteMemberRow, ModelRouteRow};

pub struct CreateModelRouteParams<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub enabled: bool,
    pub required_capabilities: &'a str,
    pub fallback_after_ms: i64,
}

#[derive(Default)]
pub struct UpdateModelRouteParams<'a> {
    pub name: Option<&'a str>,
    pub enabled: Option<bool>,
    pub required_capabilities: Option<&'a str>,
    pub fallback_after_ms: Option<i64>,
}

pub struct UpsertModelRouteMemberParams<'a> {
    pub id: &'a str,
    pub route_id: &'a str,
    pub provider_id: &'a str,
    pub model: &'a str,
    pub tier: &'a str,
    pub weight: i64,
    pub max_concurrency: i64,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub enabled: bool,
}

pub struct RecordModelRouteMetricParams<'a> {
    pub id: &'a str,
    pub run_id: &'a str,
    pub user_id: &'a str,
    pub route_id: &'a str,
    pub member_id: &'a str,
    pub tokens: i64,
    pub latency_ms: i64,
    pub success: bool,
    pub fallback_used: bool,
    pub error_code: Option<&'a str>,
    pub created_at: i64,
}

#[async_trait::async_trait]
pub trait IModelRouteRepository: Send + Sync {
    async fn list_routes(&self) -> Result<Vec<ModelRouteRow>, DbError>;
    async fn find_route(&self, id: &str) -> Result<Option<ModelRouteRow>, DbError>;
    async fn create_route(&self, params: &CreateModelRouteParams<'_>) -> Result<ModelRouteRow, DbError>;
    async fn update_route(&self, id: &str, params: &UpdateModelRouteParams<'_>) -> Result<ModelRouteRow, DbError>;
    async fn delete_route(&self, id: &str) -> Result<(), DbError>;
    async fn list_members(&self, route_id: &str) -> Result<Vec<ModelRouteMemberRow>, DbError>;
    async fn find_member(&self, id: &str) -> Result<Option<ModelRouteMemberRow>, DbError>;
    async fn upsert_member(&self, params: &UpsertModelRouteMemberParams<'_>) -> Result<ModelRouteMemberRow, DbError>;
    async fn delete_member(&self, id: &str) -> Result<(), DbError>;
    async fn find_assignment(&self, conversation_id: &str) -> Result<Option<ConversationModelAssignmentRow>, DbError>;
    async fn upsert_assignment(
        &self,
        conversation_id: &str,
        route_id: &str,
        member: &ModelRouteMemberRow,
        fallback_used: bool,
    ) -> Result<ConversationModelAssignmentRow, DbError>;
    async fn try_acquire_member(&self, member_id: &str, now: i64) -> Result<bool, DbError>;
    async fn release_member(&self, member_id: &str, tokens: i64, now: i64) -> Result<(), DbError>;
    async fn record_member_failure(
        &self,
        member_id: &str,
        http_status: Option<u16>,
        retry_after_ms: Option<u64>,
        balance_exhausted: bool,
        now: i64,
    ) -> Result<ModelRouteMemberRow, DbError>;
    async fn reset_member_breaker(&self, member_id: &str, now: i64) -> Result<ModelRouteMemberRow, DbError>;
    async fn record_metric(&self, params: &RecordModelRouteMetricParams<'_>) -> Result<(), DbError>;
}
