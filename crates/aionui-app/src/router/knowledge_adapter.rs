use std::net::IpAddr;
use std::sync::Arc;

use aionui_api_types::{DecisionEvidenceInput, KnowledgeHit, KnowledgeSearchMode, KnowledgeSearchRequest};
use aionui_conversation::ConversationService;
use aionui_db::IProviderRepository;
use aionui_decision::{
    DecisionKnowledgeFailure, DecisionKnowledgePort, DecisionKnowledgeRequest, DecisionKnowledgeResult,
};
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
    ) -> Result<DecisionKnowledgeResult, DecisionKnowledgeFailure> {
        let policy = if request.policy.is_null() {
            DecisionKnowledgePolicy::default()
        } else {
            serde_json::from_value(request.policy)
                .map_err(|_| DecisionKnowledgeFailure::new("invalid decision knowledge policy"))?
        };
        let _ = policy.mode;

        if policy.mode.as_deref() == Some("off") {
            return Ok(DecisionKnowledgeResult::default());
        }

        // Retrieve locally first. Cloud authorization is evaluated separately
        // and persisted on the bundle; the Decision service gates that bundle
        // again for each concrete Brain/fallback execution location.
        let mut bundle = self
            .gateway
            .search(KnowledgeSearchRequest {
                query: request.question,
                mode: KnowledgeSearchMode::Hybrid,
                space_ids: policy.space_ids.clone(),
                max_hits: policy.max_hits,
                cloud_use: false,
                media_type: None,
            })
            .await
            .map_err(|error| DecisionKnowledgeFailure::new(error.public_message()))?;
        bundle.cloud_authorized = if policy.cloud_use {
            match self.gateway.cloud_authorized(&policy.space_ids).await {
                Ok(authorized) => authorized,
                Err(error) => {
                    tracing::warn!(
                        error = %error,
                        "decision cloud-consent check failed closed; local retrieval remains available"
                    );
                    false
                }
            }
        } else {
            false
        };
        let evidence = bundle.hits.iter().cloned().map(hit_to_decision_evidence).collect();
        Ok(DecisionKnowledgeResult {
            evidence,
            retrieval: Some(bundle),
        })
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
        end_seconds: hit.locator.end_seconds,
        uri: hit.locator.uri,
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
    url.host_str()
        .and_then(|host| {
            host.trim_matches(|character| matches!(character, '[' | ']'))
                .parse::<IpAddr>()
                .ok()
        })
        .is_some_and(|address| address.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_api_types::KnowledgeLocator;
    use wiremock::matchers::{body_json, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn only_explicit_local_provider_targets_are_local() {
        assert!(is_local_provider("ollama", "http://127.0.0.1:11434"));
        assert!(!is_local_provider("local", "http://localhost:11434/v1"));
        assert!(!is_local_provider("ollama", "http://example.com"));
        assert!(!is_local_provider("openai", "http://127.0.0.1:11434/v1"));
        assert!(!is_local_provider("openai", "https://api.openai.com/v1"));
        assert!(!is_local_provider("openai", "not-a-url"));
    }

    #[tokio::test]
    async fn decision_retrieval_remains_available_locally_without_cloud_authorization() {
        let worker = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/knowledge/search"))
            .and(body_json(serde_json::json!({
                "query": "private question",
                "mode": "hybrid",
                "space_ids": ["personal"],
                "max_hits": 8,
                "cloud_use": false
            })))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query": "private question",
                "hits": [{
                    "source_id": "source-one", "title": "Private", "snippet": "local evidence",
                    "score": 0.9, "media_type": "text", "locator": {}
                }],
                "token_budget": 0,
                "cloud_authorized": false,
                "space_ids": ["personal"]
            })))
            .expect(1)
            .mount(&worker)
            .await;
        let gateway = KnowledgeGateway::for_loopback_worker(
            &worker.uri(),
            "internal-token",
            Arc::new(aionui_knowledge::UnknownModelLocationResolver),
        )
        .unwrap();
        let adapter = AppDecisionKnowledge::new(Arc::new(gateway));
        let evidence = adapter
            .retrieve(DecisionKnowledgeRequest {
                user_id: "owner".into(),
                decision_id: None,
                question: "private question".into(),
                policy: serde_json::json!({
                    "mode": "auto", "cloud_use": false, "space_ids": ["personal"]
                }),
            })
            .await
            .unwrap();
        assert_eq!(evidence.evidence[0].source_id, "source-one");
        assert!(!evidence.retrieval.unwrap().cloud_authorized);
    }

    #[tokio::test]
    async fn cloud_consent_lookup_failure_does_not_suppress_local_decision_evidence() {
        let worker = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/knowledge/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "query": "private question",
                "hits": [{
                    "source_id": "source-one", "title": "Private", "snippet": "local evidence",
                    "score": 0.9, "media_type": "text", "locator": {}
                }],
                "token_budget": 0,
                "cloud_authorized": false,
                "space_ids": ["personal"]
            })))
            .expect(1)
            .mount(&worker)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/knowledge/spaces"))
            .respond_with(ResponseTemplate::new(503).set_body_string("private worker error"))
            .expect(1)
            .mount(&worker)
            .await;
        let gateway = KnowledgeGateway::for_loopback_worker(
            &worker.uri(),
            "internal-token",
            Arc::new(aionui_knowledge::UnknownModelLocationResolver),
        )
        .unwrap();
        let result = AppDecisionKnowledge::new(Arc::new(gateway))
            .retrieve(DecisionKnowledgeRequest {
                user_id: "owner".into(),
                decision_id: None,
                question: "private question".into(),
                policy: serde_json::json!({
                    "mode": "auto", "cloud_use": true, "space_ids": ["personal"]
                }),
            })
            .await
            .unwrap();
        assert_eq!(result.evidence[0].source_id, "source-one");
        assert!(!result.retrieval.unwrap().cloud_authorized);
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
                end_seconds: Some(18.25),
                uri: Some("contextofme://knowledge/wiki/plan/risk".into()),
            },
        });
        assert_eq!(evidence.page, Some(4));
        assert_eq!(evidence.chapter.as_deref(), Some("Risk"));
        assert_eq!(evidence.timestamp_ms, Some(12_345));
        assert_eq!(evidence.end_seconds, Some(18.25));
        assert_eq!(evidence.uri.as_deref(), Some("contextofme://knowledge/wiki/plan/risk"));
    }
}
