use std::sync::Arc;

use aionui_api_types::{
    KnowledgeSearchMode, KnowledgeSearchRequest, SendMessageKnowledge, SendMessageKnowledgeMode, SendMessageRequest,
};
use aionui_knowledge::{KnowledgeError, KnowledgeGateway, ModelLocation, ModelLocationResolver};
use async_trait::async_trait;
use wiremock::matchers::{body_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct FixedLocation(ModelLocation);

#[async_trait]
impl ModelLocationResolver for FixedLocation {
    async fn resolve(&self, _user_id: &str, _conversation_id: &str) -> ModelLocation {
        self.0
    }
}

fn search_request(cloud_use: bool) -> KnowledgeSearchRequest {
    KnowledgeSearchRequest {
        query: "  launch plan  ".into(),
        mode: KnowledgeSearchMode::Hybrid,
        space_ids: vec!["personal".into()],
        max_hits: 8,
        cloud_use,
        media_type: None,
    }
}

fn valid_search_response() -> serde_json::Value {
    serde_json::json!({
        "query": "launch plan",
        "hits": [{
            "source_id": "source_1",
            "title": "Launch plan.pdf",
            "snippet": "The first milestone is a private beta.",
            "score": 0.91,
            "media_type": "pdf",
            "locator": {"page": 3, "uri": "contextofme://knowledge/sources/source_1"}
        }],
        "token_budget": 0,
        "cloud_authorized": false,
        "space_ids": ["personal"]
    })
}

async fn gateway(server: &MockServer, location: ModelLocation) -> KnowledgeGateway {
    KnowledgeGateway::for_loopback_worker(&server.uri(), "internal-test-token", Arc::new(FixedLocation(location)))
        .unwrap()
}

#[tokio::test]
async fn search_uses_fixed_worker_origin_private_token_and_validates_hits() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/knowledge/search"))
        .and(header("x-centaurai-internal-token", "internal-test-token"))
        .and(body_json(serde_json::json!({
            "query": "launch plan",
            "mode": "hybrid",
            "space_ids": ["personal"],
            "max_hits": 8,
            "cloud_use": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(valid_search_response()))
        .expect(1)
        .mount(&server)
        .await;

    let bundle = gateway(&server, ModelLocation::Local)
        .await
        .search(search_request(false))
        .await
        .unwrap();

    assert_eq!(bundle.hits[0].source_id, "source_1");
    assert_eq!(bundle.hits[0].locator.page, Some(3));
    assert!(bundle.token_budget > 0);
    assert!(!bundle.cloud_authorized);
}

#[tokio::test]
async fn cloud_search_requires_every_selected_space_to_be_preapproved() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/knowledge/spaces"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!([{
            "id": "personal",
            "name": "Personal",
            "description": "",
            "cloud_use": "ask",
            "created_at": "2026-07-12T00:00:00Z",
            "updated_at": "2026-07-12T00:00:00Z"
        }])))
        .expect(1)
        .mount(&server)
        .await;

    let error = gateway(&server, ModelLocation::External)
        .await
        .search(search_request(true))
        .await
        .unwrap_err();
    assert!(matches!(error, KnowledgeError::CloudConsentRequired));
}

#[tokio::test]
async fn auto_enrichment_degrades_without_cloud_permission_but_required_does_not() {
    let server = MockServer::start().await;
    let gateway = gateway(&server, ModelLocation::External).await;
    let mut automatic = SendMessageRequest {
        content: "launch plan".into(),
        files: vec![],
        inject_skills: vec![],
        hidden: false,
        knowledge: Some(SendMessageKnowledge {
            mode: SendMessageKnowledgeMode::Auto,
            space_ids: vec!["personal".into()],
            max_hits: 8,
            cloud_use: false,
        }),
        retrieval: None,
    };
    gateway
        .enrich_message("owner", "conversation", &mut automatic)
        .await
        .unwrap();
    assert!(automatic.retrieval.is_none());

    automatic.knowledge.as_mut().unwrap().mode = SendMessageKnowledgeMode::Required;
    let error = gateway
        .enrich_message("owner", "conversation", &mut automatic)
        .await
        .unwrap_err();
    assert!(matches!(error, KnowledgeError::CloudConsentRequired));
}

#[tokio::test]
async fn worker_failure_is_sanitized_and_required_empty_search_is_explicit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/knowledge/search"))
        .respond_with(ResponseTemplate::new(503).set_body_string("secret internal failure"))
        .expect(1)
        .mount(&server)
        .await;
    let error = gateway(&server, ModelLocation::Local)
        .await
        .search(search_request(false))
        .await
        .unwrap_err();
    assert!(matches!(error, KnowledgeError::Unavailable));
    assert_eq!(error.to_string(), "knowledge worker is unavailable");

    let empty_server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/knowledge/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "query": "launch plan",
            "hits": [],
            "token_budget": 0,
            "cloud_authorized": false,
            "space_ids": ["personal"]
        })))
        .mount(&empty_server)
        .await;
    let gateway = gateway(&empty_server, ModelLocation::Local).await;
    let mut request = SendMessageRequest {
        content: "launch plan".into(),
        files: vec![],
        inject_skills: vec![],
        hidden: false,
        knowledge: Some(SendMessageKnowledge {
            mode: SendMessageKnowledgeMode::Required,
            space_ids: vec!["personal".into()],
            max_hits: 8,
            cloud_use: false,
        }),
        retrieval: None,
    };
    let error = gateway
        .enrich_message("owner", "conversation", &mut request)
        .await
        .unwrap_err();
    assert!(matches!(error, KnowledgeError::NoResults));
}

#[tokio::test]
async fn malformed_worker_dto_is_rejected_at_runtime() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/knowledge/search"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "query": "launch plan",
            "hits": [{"source_id": "source_1", "snippet": "missing fields"}],
            "token_budget": 0,
            "cloud_authorized": false,
            "space_ids": ["personal"]
        })))
        .mount(&server)
        .await;
    let error = gateway(&server, ModelLocation::Local)
        .await
        .search(search_request(false))
        .await
        .unwrap_err();
    assert!(matches!(error, KnowledgeError::InvalidResponse));
}
