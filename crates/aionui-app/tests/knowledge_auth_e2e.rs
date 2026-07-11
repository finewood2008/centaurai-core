//! Knowledge APIs share Core's owner authentication boundary.

mod common;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use common::{body_json, build_app, get_request};

#[tokio::test]
async fn all_public_knowledge_route_groups_require_authentication() {
    let (app, _) = build_app().await;
    for uri in [
        "/api/knowledge/status",
        "/api/knowledge/spaces",
        "/api/knowledge/sources",
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
