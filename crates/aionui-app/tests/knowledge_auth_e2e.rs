//! Knowledge APIs share Core's owner authentication boundary.

mod common;

use std::sync::Arc;

use aionui_api_types::{CreateDevicePairingRequest, RedeemDevicePairingRequest};
use aionui_app::{AppConfig, AppServices, build_module_states, create_router_with_states};
use aionui_auth::TokenPayload;
use aionui_knowledge::{KnowledgeGateway, KnowledgeRouterState, UnknownModelLocationResolver};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use jsonwebtoken::{EncodingKey, Header, encode};
use tower::ServiceExt;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use common::{body_json, build_app, get_request};

#[tokio::test]
async fn all_public_knowledge_route_groups_require_authentication() {
    let (app, _) = build_app().await;
    for uri in [
        "/api/knowledge/status",
        "/api/knowledge/spaces",
        "/api/knowledge/sources",
        "/api/knowledge/sources/0123456789abcdef01234567/content",
        "/api/knowledge/jobs/job_1",
        "/api/knowledge/wiki/stats",
        "/api/knowledge/memory/files",
        "/api/knowledge/graph/wiki",
    ] {
        let response = app.clone().oneshot(get_request(uri)).await.unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED, "{uri}");
        assert_eq!(body_json(response).await["code"], "UNAUTHORIZED", "{uri}");
    }

    let request = Request::builder()
        .method("POST")
        .uri("/api/knowledge/search")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"query":"private plan"}"#))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(body_json(response).await["code"], "UNAUTHORIZED");
}

async fn build_app_with_worker(server: &MockServer) -> (axum::Router, AppServices) {
    let db = aionui_db::init_database_memory().await.unwrap();
    let services = AppServices::from_config(db, &AppConfig::default()).await.unwrap();
    let (mut states, _) = build_module_states(&services).await.expect("build module states");
    states.knowledge = KnowledgeRouterState {
        gateway: Arc::new(
            KnowledgeGateway::for_loopback_worker(
                &server.uri(),
                "private-token",
                Arc::new(UnknownModelLocationResolver),
            )
            .unwrap(),
        ),
    };
    let app = create_router_with_states(&services, states);
    (app, services)
}

fn content_request(token: &str) -> Request<Body> {
    Request::builder()
        .uri("/api/knowledge/sources/0123456789abcdef01234567/content")
        .header("authorization", format!("Bearer {token}"))
        .body(Body::empty())
        .unwrap()
}

fn expired_token(services: &AppServices) -> String {
    encode(
        &Header::default(),
        &TokenPayload {
            user_id: "system_default_user".into(),
            username: "admin".into(),
            iat: 1,
            exp: 2,
            iss: "aionui".into(),
            aud: "aionui-webui".into(),
        },
        &EncodingKey::from_secret(services.jwt_secret_raw.as_bytes()),
    )
    .unwrap()
}

#[tokio::test]
async fn source_content_accepts_owner_and_device_but_rejects_expired_and_revoked_credentials() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/knowledge/sources/0123456789abcdef01234567/content"))
        .and(header("x-centaurai-internal-token", "private-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/plain")
                .set_body_string("owner document"),
        )
        .expect(2)
        .mount(&server)
        .await;
    let (app, services) = build_app_with_worker(&server).await;

    let expired = app
        .clone()
        .oneshot(content_request(&expired_token(&services)))
        .await
        .unwrap();
    assert_eq!(expired.status(), StatusCode::UNAUTHORIZED);

    let owner_token = services.jwt_service.sign("system_default_user", "admin").unwrap();
    let owner = app.clone().oneshot(content_request(&owner_token)).await.unwrap();
    assert_eq!(owner.status(), StatusCode::OK);
    assert_eq!(owner.into_body().collect().await.unwrap().to_bytes(), "owner document");

    let pairing = services
        .device_service
        .create_pairing(
            "system_default_user",
            CreateDevicePairingRequest {
                server_url: "http://192.168.1.20:25808".into(),
            },
        )
        .await
        .unwrap();
    let code = pairing.pairing_uri.rsplit("code=").next().unwrap().to_owned();
    let credential = services
        .device_service
        .redeem_pairing(RedeemDevicePairingRequest {
            code,
            name: "Phone".into(),
            platform: "ios".into(),
        })
        .await
        .unwrap();
    let device = app
        .clone()
        .oneshot(content_request(&credential.device_token))
        .await
        .unwrap();
    assert_eq!(device.status(), StatusCode::OK);
    assert_eq!(device.into_body().collect().await.unwrap().to_bytes(), "owner document");

    services.jwt_service.blacklist_token(&owner_token);
    let revoked_owner = app.clone().oneshot(content_request(&owner_token)).await.unwrap();
    assert_eq!(revoked_owner.status(), StatusCode::UNAUTHORIZED);

    services
        .device_service
        .revoke_device("system_default_user", &credential.device.id)
        .await
        .unwrap();
    let revoked_device = app.oneshot(content_request(&credential.device_token)).await.unwrap();
    assert_eq!(revoked_device.status(), StatusCode::UNAUTHORIZED);
}
