use std::sync::Arc;

use aionui_knowledge::{KnowledgeGateway, KnowledgeRouterState, UnknownModelLocationResolver, knowledge_routes};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn json_body(response: axum::response::Response) -> serde_json::Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

fn json_request(method: &str, uri: &str, body: serde_json::Value) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn source_json() -> serde_json::Value {
    serde_json::json!({
        "id": "source_1",
        "space_id": "personal",
        "title": "Plan.pdf",
        "media_type": "pdf",
        "status": "ready",
        "created_at": "2026-07-12T00:00:00Z",
        "updated_at": "2026-07-12T00:00:00Z",
        "chunk_count": 3
    })
}

fn job_json() -> serde_json::Value {
    serde_json::json!({
        "id": "job_1",
        "source_id": "source_1",
        "kind": "index_source",
        "status": "completed",
        "progress": 1.0,
        "error_code": null,
        "retryable": false
    })
}

#[tokio::test]
async fn status_stays_machine_readable_when_worker_is_offline() {
    let app = knowledge_routes(KnowledgeRouterState {
        gateway: Arc::new(KnowledgeGateway::unavailable()),
    });
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/knowledge/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["success"], true);
    assert_eq!(body["data"]["available"], false);
    assert_eq!(body["data"]["state"], "offline");
}

#[tokio::test]
async fn worker_failures_map_to_sanitized_core_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/knowledge/search"))
        .respond_with(ResponseTemplate::new(503).set_body_string("private worker traceback"))
        .mount(&server)
        .await;
    let gateway =
        KnowledgeGateway::for_loopback_worker(&server.uri(), "private-token", Arc::new(UnknownModelLocationResolver))
            .unwrap();
    let app = knowledge_routes(KnowledgeRouterState {
        gateway: Arc::new(gateway),
    });
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/knowledge/search")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"query":"plan"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    let body = json_body(response).await;
    assert_eq!(body["code"], "KNOWLEDGE_WORKER_UNAVAILABLE");
    assert!(!body.to_string().contains("traceback"));
    assert!(!body.to_string().contains("private-token"));
}

#[tokio::test]
async fn graph_proxy_keeps_a_fixed_namespace_and_forwards_only_query_data() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/knowledge/graph/wiki"))
        .and(query_param("depth", "2"))
        .and(header("x-centaurai-internal-token", "private-token"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"nodes": []})))
        .expect(1)
        .mount(&server)
        .await;
    let gateway =
        KnowledgeGateway::for_loopback_worker(&server.uri(), "private-token", Arc::new(UnknownModelLocationResolver))
            .unwrap();
    let app = knowledge_routes(KnowledgeRouterState {
        gateway: Arc::new(gateway),
    });
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/knowledge/graph/wiki?depth=2")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["data"]["nodes"], serde_json::json!([]));
}

#[tokio::test]
async fn core_routes_cover_worker_crud_upload_and_mutating_subresources() {
    let server = MockServer::start().await;
    let space = serde_json::json!({
        "id": "personal",
        "name": "Personal",
        "description": "Private",
        "cloud_use": "allowed",
        "created_at": "2026-07-12T00:00:00Z",
        "updated_at": "2026-07-12T00:00:00Z"
    });
    for (worker_method, worker_path, response) in [
        (
            "GET",
            "/api/knowledge/status",
            serde_json::json!({
                "available": true,
                "state": "ready",
                "worker_version": "0.1.0",
                "pending_jobs": 0,
                "indexed_sources": 1,
                "contract_version": "1.0",
                "transport": "loopback"
            }),
        ),
        ("GET", "/api/knowledge/spaces", serde_json::json!([space.clone()])),
        ("POST", "/api/knowledge/spaces", space.clone()),
        ("PATCH", "/api/knowledge/spaces/personal", space.clone()),
        ("GET", "/api/knowledge/sources", serde_json::json!([source_json()])),
        (
            "POST",
            "/api/knowledge/sources",
            serde_json::json!({
                "source": source_json(), "job": job_json(), "job_id": "job_1"
            }),
        ),
        (
            "DELETE",
            "/api/knowledge/sources/source_1",
            serde_json::json!({"deleted": true}),
        ),
        ("GET", "/api/knowledge/jobs/job_1", job_json()),
        ("POST", "/api/knowledge/memory/search", serde_json::json!({"items": []})),
        (
            "PUT",
            "/api/knowledge/memory/files/USER.md",
            serde_json::json!({"updated": true}),
        ),
        (
            "DELETE",
            "/api/knowledge/memory/files/USER.md",
            serde_json::json!({"deleted": true}),
        ),
    ] {
        Mock::given(method(worker_method))
            .and(path(worker_path))
            .and(header("x-centaurai-internal-token", "private-token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(response))
            .expect(1)
            .mount(&server)
            .await;
    }
    let gateway =
        KnowledgeGateway::for_loopback_worker(&server.uri(), "private-token", Arc::new(UnknownModelLocationResolver))
            .unwrap();
    let app = knowledge_routes(KnowledgeRouterState {
        gateway: Arc::new(gateway),
    });

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/knowledge/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(response).await["data"]["state"], "ready");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/knowledge/spaces")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(response).await["data"][0]["cloud_use"], "allowed");
    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/knowledge/spaces",
            serde_json::json!({"name": "Personal", "description": "Private", "cloud_use": "allowed"}),
        ))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::CREATED);
    let response = app
        .clone()
        .oneshot(json_request(
            "PATCH",
            "/api/knowledge/spaces/personal",
            serde_json::json!({"cloud_use": "allowed"}),
        ))
        .await
        .unwrap();
    assert_eq!(json_body(response).await["data"]["id"], "personal");

    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/knowledge/sources?space_id=personal&limit=10&offset=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(response).await["data"][0]["id"], "source_1");
    let multipart = "--boundary\r\nContent-Disposition: form-data; name=\"space_id\"\r\n\r\npersonal\r\n--boundary\r\nContent-Disposition: form-data; name=\"file\"; filename=\"Plan.pdf\"\r\nContent-Type: application/pdf\r\n\r\npdf\r\n--boundary--\r\n";
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/knowledge/sources")
                .header("content-type", "multipart/form-data; boundary=boundary")
                .header("content-length", multipart.len().to_string())
                .body(Body::from(multipart))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::ACCEPTED);
    assert_eq!(json_body(response).await["data"]["job_id"], "job_1");
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/knowledge/sources/source_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(response).await["data"]["deleted"], true);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/knowledge/jobs/job_1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(response).await["data"]["status"], "completed");

    let response = app
        .clone()
        .oneshot(json_request(
            "POST",
            "/api/knowledge/memory/search",
            serde_json::json!({"query": "plan"}),
        ))
        .await
        .unwrap();
    assert_eq!(json_body(response).await["data"]["items"], serde_json::json!([]));
    let response = app
        .clone()
        .oneshot(json_request(
            "PUT",
            "/api/knowledge/memory/files/USER.md",
            serde_json::json!({"content": "memory"}),
        ))
        .await
        .unwrap();
    assert_eq!(json_body(response).await["data"]["updated"], true);
    let response = app
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/knowledge/memory/files/USER.md")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(json_body(response).await["data"]["deleted"], true);
}
