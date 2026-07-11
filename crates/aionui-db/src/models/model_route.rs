use aionui_common::TimestampMs;

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ModelRouteRow {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub required_capabilities: String,
    pub fallback_after_ms: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ModelRouteMemberRow {
    pub id: String,
    pub route_id: String,
    pub provider_id: String,
    pub model: String,
    pub tier: String,
    pub weight: i64,
    pub max_concurrency: i64,
    pub rpm_limit: Option<i64>,
    pub tpm_limit: Option<i64>,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
    pub cooldown_until: Option<TimestampMs>,
    pub consecutive_5xx: i64,
    pub consecutive_429: i64,
    pub active_count: i64,
    pub request_window_started_at: Option<TimestampMs>,
    pub request_count: i64,
    pub token_count: i64,
    pub created_at: TimestampMs,
    pub updated_at: TimestampMs,
}

#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct ConversationModelAssignmentRow {
    pub conversation_id: String,
    pub route_id: String,
    pub member_id: String,
    pub provider_id: String,
    pub model: String,
    pub fallback_used: bool,
    pub assigned_at: TimestampMs,
    pub updated_at: TimestampMs,
}
