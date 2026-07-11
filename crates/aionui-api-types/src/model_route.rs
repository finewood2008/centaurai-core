use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelRouteResponse {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub required_capabilities: Vec<String>,
    pub fallback_after_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelRouteMemberResponse {
    pub id: String,
    pub route_id: String,
    pub provider_id: String,
    pub model: String,
    pub tier: String,
    pub weight: u32,
    pub max_concurrency: u32,
    pub rpm_limit: Option<u32>,
    pub tpm_limit: Option<u32>,
    pub enabled: bool,
    pub disabled_reason: Option<String>,
    pub cooldown_until: Option<i64>,
    pub active_count: u32,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CreateModelRouteRequest {
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    #[serde(default = "default_fallback_ms")]
    pub fallback_after_ms: u64,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct UpdateModelRouteRequest {
    pub name: Option<String>,
    pub enabled: Option<bool>,
    pub required_capabilities: Option<Vec<String>>,
    pub fallback_after_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct UpsertModelRouteMemberRequest {
    pub id: Option<String>,
    pub provider_id: String,
    pub model: String,
    pub tier: String,
    #[serde(default = "default_weight")]
    pub weight: u32,
    #[serde(default = "default_concurrency")]
    pub max_concurrency: u32,
    pub rpm_limit: Option<u32>,
    pub tpm_limit: Option<u32>,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModelRouteMemberHealthRequest {
    pub retry_after_ms: Option<u64>,
    pub http_status: Option<u16>,
    #[serde(default)]
    pub balance_exhausted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct EffectiveModelAssignmentResponse {
    pub route_id: String,
    pub member_id: String,
    pub provider_id: String,
    pub model: String,
    pub fallback_used: bool,
}

const fn default_true() -> bool {
    true
}
const fn default_weight() -> u32 {
    1
}
const fn default_concurrency() -> u32 {
    1
}
const fn default_fallback_ms() -> u64 {
    15_000
}
