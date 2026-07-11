use std::net::IpAddr;
use std::sync::Arc;

use aionui_api_types::{DecisionEvidenceInput, KnowledgeHit, KnowledgeSearchMode, KnowledgeSearchRequest};
use aionui_conversation::ConversationService;
use aionui_db::IProviderRepository;
use aionui_decision::{DecisionKnowledgeFailure, DecisionKnowledgePort, DecisionKnowledgeRequest};
use aionui_knowledge::{KnowledgeGateway, ModelLocation, ModelLocationResolver};
use async_trait::async_trait;
use serde::Deserialize;

pub(crate) struct AppModelLocationResolver {
    conversations: ConversationService,
    providers: Arc<dyn IProviderRepository>,
}

impl AppModelLocationResolver {
    pub(crate) fn new(conversations: ConversationService, providers: Arc<dyn IProviderRepository>) -> Self {
        Self {
            conversations,
            providers,
        }
    }
}

#[async_trait]
impl ModelLocationResolver for AppModelLocationResolver {
    async fn resolve(&self, user_id: &str, conversation_id: &str) -> ModelLocation {
        let Ok(conversation) = self.conversations.get(user_id, conversation_id).await else {
            return ModelLocation::Unknown;
        };
        let Some(model) = conversation.model else {
            return ModelLocation::Unknown;
        };
        let Ok(Some(provider)) = self.providers.find_by_id(&model.provider_id).await else {
            return ModelLocation::Unknown;
        };
        if is_local_provider(&provider.platform, &provider.base_url) {
            ModelLocation::Local
        } else {
            ModelLocation::External
        }
    }
}

/// The only production path from a decision into personal knowledge. It
/// accepts policy fields, never transport details, and maps already-validated
/// Gateway hits into evidence owned by the decision repository.
pub(crate) struct AppDecisionKnowledge {
    gateway: Arc<KnowledgeGateway>,
}

impl AppDecisionKnowledge {
    pub(crate) fn new(gateway: Arc<KnowledgeGateway>) -> Self {
        Self { gateway }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct DecisionKnowledgePolicy {
    mode: Option<String>,
    space_ids: Vec<String>,
    #[serde(default = "default_max_hits")]
    max_hits: u32,
    cloud_use: bool,
}

fn default_max_hits() -> u32 {
    8
}

#[async_trait]
impl DecisionKnowledgePort for AppDecisionKnowledge {
    async fn retrieve(
        &self,
        request: DecisionKnowledgeRequest,
    ) -> Result<Vec<DecisionEvidenceInput>, DecisionKnowledgeFailure> {
        let policy = if request.policy.is_null() {
            DecisionKnowledgePolicy::default()
        } else {
            serde_json::from_value(request.policy)
                .map_err(|_| DecisionKnowledgeFailure::new("invalid decision knowledge policy"))?
        };
        let _ = policy.mode;

        // Decisions may fan evidence out to multiple provider brains. Until
        // every configured brain can be proven local, explicit cloud consent
        // is required before retrieval so local passages cannot leak through
        // a later provider invocation.
        if !policy.cloud_use {
            return Ok(Vec::new());
        }

        let bundle = self
            .gateway
            .search(KnowledgeSearchRequest {
                query: request.question,
                mode: KnowledgeSearchMode::Hybrid,
                space_ids: policy.space_ids,
                max_hits: policy.max_hits,
                cloud_use: true,
                media_type: None,
            })
            .await
            .map_err(|error| DecisionKnowledgeFailure::new(error.public_message()))?;
        Ok(bundle.hits.into_iter().map(hit_to_decision_evidence).collect())
    }
}

fn hit_to_decision_evidence(hit: KnowledgeHit) -> DecisionEvidenceInput {
    DecisionEvidenceInput {
        source_id: hit.source_id,
        title: hit.title,
        snippet: hit.snippet,
        score: hit.score,
        media_type: hit.media_type,
        page: hit.locator.page.map(i64::from),
        chapter: hit.locator.chapter,
        timestamp_ms: hit
            .locator
            .start_seconds
            .map(|seconds| (seconds * 1_000.0).round() as i64),
    }
}

fn is_local_provider(platform: &str, base_url: &str) -> bool {
    let explicitly_local = matches!(
        platform.trim().to_ascii_lowercase().as_str(),
        "ollama" | "local" | "llama.cpp" | "llamacpp"
    );
    if !explicitly_local {
        return false;
    }
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    match url.host_str() {
        Some("localhost") => true,
        Some(host) => host.parse::<IpAddr>().is_ok_and(|address| address.is_loopback()),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::KnowledgeLocator;

    #[test]
    fn only_explicit_local_provider_targets_are_local() {
        assert!(is_local_provider("ollama", "http://127.0.0.1:11434"));
        assert!(is_local_provider("local", "http://localhost:11434/v1"));
        assert!(!is_local_provider("ollama", "http://example.com"));
        assert!(!is_local_provider("openai", "http://127.0.0.1:11434/v1"));
        assert!(!is_local_provider("openai", "https://api.openai.com/v1"));
        assert!(!is_local_provider("openai", "not-a-url"));
    }

    #[tokio::test]
    async fn decision_retrieval_requires_explicit_cloud_authorization() {
        let adapter = AppDecisionKnowledge::new(Arc::new(KnowledgeGateway::unavailable()));
        let evidence = adapter
            .retrieve(DecisionKnowledgeRequest {
                user_id: "owner".into(),
                decision_id: None,
                question: "private question".into(),
                policy: serde_json::json!({"mode": "auto", "cloud_use": false}),
            })
            .await
            .unwrap();
        assert!(evidence.is_empty());
    }

    #[tokio::test]
    async fn decision_policy_rejects_worker_transport_fields() {
        let adapter = AppDecisionKnowledge::new(Arc::new(KnowledgeGateway::unavailable()));
        let error = adapter
            .retrieve(DecisionKnowledgeRequest {
                user_id: "owner".into(),
                decision_id: None,
                question: "private question".into(),
                policy: serde_json::json!({
                    "mode": "auto",
                    "cloud_use": false,
                    "endpoint": "http://169.254.169.254/latest/meta-data"
                }),
            })
            .await
            .unwrap_err();
        assert_eq!(error.message, "invalid decision knowledge policy");
    }

    #[test]
    fn gateway_locator_is_preserved_as_decision_evidence() {
        let evidence = hit_to_decision_evidence(KnowledgeHit {
            source_id: "source-one".into(),
            title: "Plan".into(),
            snippet: "Evidence".into(),
            score: 0.91,
            media_type: "video".into(),
            locator: KnowledgeLocator {
                page: Some(4),
                chapter: Some("Risk".into()),
                start_seconds: Some(12.345),
                end_seconds: None,
                uri: None,
            },
        });
        assert_eq!(evidence.page, Some(4));
        assert_eq!(evidence.chapter.as_deref(), Some("Risk"));
        assert_eq!(evidence.timestamp_ms, Some(12_345));
    }
}
