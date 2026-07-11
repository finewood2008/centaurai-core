//! Static client-negotiation contract.

use std::collections::BTreeMap;

use aionui_api_types::{ApiResponse, CoreCapabilitiesResponse, CoreContractVersions, CoreWebSocketCapabilities};
use axum::Json;

const WEBSOCKET_EVENTS: &[&str] = &[
    "channel.pairing-requested",
    "channel.plugin-status-changed",
    "channel.user-authorized",
    "confirmation.remove",
    "conversation.artifact",
    "conversation.effectiveModelSelected",
    "conversation.fallbackSelected",
    "conversation.listChanged",
    "conversation.queueUpdated",
    "conversation.runQueued",
    "conversation.runStarted",
    "conversation.runTimedOut",
    "cron.job-created",
    "cron.job-executed",
    "cron.job-removed",
    "cron.job-updated",
    "decision.completed",
    "decision.evidenceAdded",
    "decision.sessionChanged",
    "decision.turnDelta",
    "excel-preview.status",
    "extensions.lifecycle",
    "extensions.state-changed",
    "fileStream.contentUpdate",
    "fileWatch.fileChanged",
    "hub.state-changed",
    "message.stream",
    "message.userCreated",
    "ping",
    "ppt-preview.status",
    "runtime.statusChanged",
    "show-open-request",
    "team.agentRemoved",
    "team.agentRenamed",
    "team.agentRuntimeStatusChanged",
    "team.agentSpawned",
    "team.agentStatusChanged",
    "team.childTurnCancelled",
    "team.childTurnCompleted",
    "team.childTurnStarted",
    "team.created",
    "team.listChanged",
    "team.removed",
    "team.renamed",
    "team.runAccepted",
    "team.runCancelled",
    "team.runCompleted",
    "team.runFailed",
    "team.runStarted",
    "team.runUpdated",
    "team.sessionChanged",
    "team.sessionStatusChanged",
    "team.taskChanged",
    "team.teammateMessage",
    "turn.completed",
    "word-preview.status",
    "workspaceOfficeWatch.fileAdded",
];

pub(super) async fn get_capabilities() -> Json<ApiResponse<CoreCapabilitiesResponse>> {
    Json(ApiResponse::ok(capabilities()))
}

fn capabilities() -> CoreCapabilitiesResponse {
    CoreCapabilitiesResponse {
        contract: CoreContractVersions {
            rest: "1".into(),
            websocket: "1".into(),
            startup: "2".into(),
        },
        feature_version: "1".into(),
        features: BTreeMap::from([
            ("agent_management".into(), true),
            ("agent_management_refresh".into(), true),
            ("centaurai_auth_cookies".into(), true),
            ("centaurai_environment_aliases".into(), true),
            ("centaurai_proxy_identity_headers".into(), true),
            ("decisions".into(), true),
            ("device_pairing".into(), true),
            ("device_token_auth".into(), true),
            ("logical_model_routes".into(), true),
            ("knowledge_gateway".into(), true),
            ("knowledge_message_retrieval".into(), true),
            ("knowledge_source_content".into(), true),
            ("knowledge_worker_supervision".into(), true),
            ("mcp".into(), true),
            ("provider_secret_redaction".into(), true),
            ("teams".into(), true),
        ]),
        websocket: CoreWebSocketCapabilities {
            version: "1".into(),
            events: WEBSOCKET_EVENTS.iter().map(|event| (*event).into()).collect(),
        },
    }
}

#[cfg(test)]
mod tests {
    use axum::Router;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    use super::*;

    #[tokio::test]
    async fn endpoint_returns_the_versioned_public_contract() {
        let app = Router::new().route("/api/capabilities", get(get_capabilities));
        let response = app
            .oneshot(Request::builder().uri("/api/capabilities").body(Body::empty()).unwrap())
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["success"], true);
        assert_eq!(body["data"]["contract"]["rest"], "1");
        assert_eq!(body["data"]["features"]["agent_management_refresh"], true);
        assert_eq!(body["data"]["features"]["decisions"], true);
        assert_eq!(body["data"]["features"]["device_pairing"], true);
        assert_eq!(body["data"]["features"]["knowledge_gateway"], true);
        assert_eq!(body["data"]["features"]["knowledge_source_content"], true);
        assert_eq!(body["data"]["websocket"]["version"], "1");
        assert!(
            body["data"]["websocket"]["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event == "team.agentStatusChanged")
        );
        assert!(
            body["data"]["websocket"]["events"]
                .as_array()
                .unwrap()
                .iter()
                .any(|event| event == "decision.completed")
        );
    }
}
