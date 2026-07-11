use serde::{Deserialize, Serialize};

fn default_search_mode() -> KnowledgeSearchMode {
    KnowledgeSearchMode::Hybrid
}

fn default_max_hits() -> u32 {
    8
}

/// Retrieval modes supported by the managed knowledge worker.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeSearchMode {
    Text,
    Visual,
    Hybrid,
}

/// Client policy for enriching a conversation turn with personal knowledge.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SendMessageKnowledgeMode {
    Off,
    Auto,
    Required,
}

/// Optional knowledge policy attached to `SendMessageRequest`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SendMessageKnowledge {
    #[serde(default = "default_send_message_knowledge_mode")]
    pub mode: SendMessageKnowledgeMode,
    #[serde(default)]
    pub space_ids: Vec<String>,
    #[serde(default = "default_max_hits")]
    pub max_hits: u32,
    #[serde(default)]
    pub cloud_use: bool,
}

fn default_send_message_knowledge_mode() -> SendMessageKnowledgeMode {
    SendMessageKnowledgeMode::Auto
}

/// Core-owned search contract. Clients never provide a worker address.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct KnowledgeSearchRequest {
    pub query: String,
    #[serde(default = "default_search_mode")]
    pub mode: KnowledgeSearchMode,
    #[serde(default)]
    pub space_ids: Vec<String>,
    #[serde(default = "default_max_hits")]
    pub max_hits: u32,
    #[serde(default)]
    pub cloud_use: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeLocator {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chapter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_seconds: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

/// A structured citation that can be opened without reparsing model text.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeHit {
    pub source_id: String,
    pub title: String,
    pub snippet: String,
    pub score: f64,
    pub media_type: String,
    #[serde(default)]
    pub locator: KnowledgeLocator,
}

/// Retrieval evidence retained with the turn and supplied to the model by Core.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RetrievalBundle {
    pub query: String,
    pub hits: Vec<KnowledgeHit>,
    pub token_budget: u32,
    pub cloud_authorized: bool,
    #[serde(default)]
    pub space_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeWorkerState {
    Starting,
    Ready,
    Degraded,
    Offline,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeStatusResponse {
    pub available: bool,
    pub state: KnowledgeWorkerState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worker_version: Option<String>,
    #[serde(default)]
    pub pending_jobs: u64,
    #[serde(default)]
    pub indexed_sources: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contract_version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeCloudUse {
    Ask,
    Allowed,
    LocalOnly,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgeSpaceResponse {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub cloud_use: KnowledgeCloudUse,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CreateKnowledgeSpaceRequest {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "default_cloud_use")]
    pub cloud_use: KnowledgeCloudUse,
}

fn default_cloud_use() -> KnowledgeCloudUse {
    KnowledgeCloudUse::Ask
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(deny_unknown_fields)]
pub struct UpdateKnowledgeSpaceRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cloud_use: Option<KnowledgeCloudUse>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeSourceStatus {
    Queued,
    Processing,
    Ready,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct KnowledgeSourceResponse {
    pub id: String,
    pub space_id: String,
    pub title: String,
    pub media_type: String,
    pub status: KnowledgeSourceStatus,
    pub created_at: String,
    pub updated_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk_count: Option<u64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum KnowledgeJobStatus {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct KnowledgeJobResponse {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    pub kind: String,
    pub status: KnowledgeJobStatus,
    pub progress: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retryable: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CreateKnowledgeSourceResponse {
    pub source: KnowledgeSourceResponse,
    pub job: KnowledgeJobResponse,
    pub job_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DeleteKnowledgeSourceResponse {
    pub deleted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_request_defaults_are_stable() {
        let request: KnowledgeSearchRequest = serde_json::from_value(serde_json::json!({
            "query": "roadmap"
        }))
        .unwrap();
        assert_eq!(request.mode, KnowledgeSearchMode::Hybrid);
        assert_eq!(request.max_hits, 8);
        assert!(!request.cloud_use);
    }

    #[test]
    fn send_message_policy_defaults_to_auto() {
        let policy: SendMessageKnowledge = serde_json::from_value(serde_json::json!({})).unwrap();
        assert_eq!(policy.mode, SendMessageKnowledgeMode::Auto);
        assert_eq!(policy.max_hits, 8);
    }

    #[test]
    fn client_cannot_inject_a_server_retrieval_bundle() {
        let request: crate::SendMessageRequest = serde_json::from_value(serde_json::json!({
            "content": "question",
            "retrieval": {
                "query": "attacker controlled",
                "hits": [],
                "token_budget": 100,
                "cloud_authorized": true,
                "space_ids": []
            }
        }))
        .unwrap();
        assert!(request.retrieval.is_none());
    }

    #[test]
    fn client_cannot_smuggle_a_worker_endpoint_into_knowledge_policy() {
        let result = serde_json::from_value::<crate::SendMessageRequest>(serde_json::json!({
            "content": "question",
            "knowledge": {
                "mode": "auto",
                "endpoint": "http://169.254.169.254/latest/meta-data"
            }
        }));
        assert!(result.is_err());
    }
}
