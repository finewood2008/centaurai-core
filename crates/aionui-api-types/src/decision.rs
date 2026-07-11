use serde::{Deserialize, Serialize};
use serde_json::Value;

fn default_brain_count() -> usize {
    3
}

/// A concrete model/provider or ACP agent participating in a decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrainKind {
    ProviderModel,
    AcpAgent,
}

fn default_brain_kind() -> BrainKind {
    BrainKind::ProviderModel
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrainDefinition {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default = "default_brain_kind")]
    pub kind: BrainKind,
    pub provider_id: String,
    pub model: String,
    pub role_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default)]
    pub tool_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoleDefinition {
    pub id: String,
    pub name: String,
    pub instructions: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionToolDefinition {
    pub id: String,
    pub name: String,
    pub kind: String,
    #[serde(default)]
    pub config: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateDecisionRequest {
    pub question: String,
    #[serde(default = "default_brain_count")]
    pub brain_count: usize,
    #[serde(default)]
    pub brains: Vec<BrainDefinition>,
    #[serde(default)]
    pub roles: Vec<RoleDefinition>,
    #[serde(default)]
    pub tools: Vec<DecisionToolDefinition>,
    #[serde(default)]
    pub knowledge: Value,
    #[serde(default)]
    pub evidence: Vec<DecisionEvidenceInput>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct UpdateDecisionRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub question: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<DecisionStatus>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterjectDecisionRequest {
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SelectDecisionCandidateRequest {
    pub candidate_id: String,
    #[serde(default)]
    pub action_items: Vec<DecisionActionItemInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionActionItemInput {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RefreshDecisionKnowledgeRequest {
    #[serde(default)]
    pub evidence: Vec<DecisionEvidenceInput>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionEvidenceInput {
    pub source_id: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
    pub media_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timestamp_ms: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionStatus {
    Draft,
    Running,
    Paused,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionBrainState {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionBrainResponse {
    pub id: String,
    pub provider_id: String,
    pub model: String,
    pub role_id: String,
    pub state: DecisionBrainState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default)]
    pub tool_ids: Vec<String>,
    pub kind: BrainKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionSessionResponse {
    pub id: String,
    pub status: DecisionStatus,
    pub revision: i64,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionTurnResponse {
    pub id: String,
    pub session_id: String,
    pub brain_id: Option<String>,
    pub kind: String,
    pub content: String,
    pub status: String,
    pub attempt: i64,
    pub error_code: Option<String>,
    pub provider_id: Option<String>,
    pub model: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionEvidenceLocator {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_seconds: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionEvidenceResponse {
    pub id: String,
    pub source_id: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
    pub media_type: String,
    pub page: Option<i64>,
    pub chapter: Option<String>,
    pub timestamp_ms: Option<i64>,
    pub turn_id: Option<String>,
    pub locator: DecisionEvidenceLocator,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionCandidateResponse {
    pub id: String,
    pub brain_id: Option<String>,
    pub title: String,
    pub content: String,
    pub rank: i64,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionActionItemResponse {
    pub id: String,
    pub title: String,
    pub owner: Option<String>,
    pub due_at: Option<String>,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionResolutionResponse {
    pub id: String,
    pub summary: String,
    pub status: String,
    pub partial: bool,
    pub action_items: Vec<DecisionActionItemResponse>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DecisionResponse {
    pub id: String,
    pub question: String,
    pub status: DecisionStatus,
    pub brains: Vec<DecisionBrainResponse>,
    pub conclusion: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub session: Option<DecisionSessionResponse>,
    #[serde(default)]
    pub turns: Vec<DecisionTurnResponse>,
    #[serde(default)]
    pub evidence: Vec<DecisionEvidenceResponse>,
    #[serde(default)]
    pub candidates: Vec<DecisionCandidateResponse>,
    pub resolution: Option<DecisionResolutionResponse>,
}
