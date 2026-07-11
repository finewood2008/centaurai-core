use std::sync::{Arc, Mutex};
use std::time::Duration;

use aionui_knowledge::{KnowledgeGateway, KnowledgeRouterState, UnknownModelLocationResolver, knowledge_routes};
use axum::Router;
use axum::body::{Body, Bytes};
use axum::http::{Request, StatusCode, header};
use axum::response::Response;
use axum::routing::get;
use futures_util::StreamExt;
use http_body_util::BodyExt;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::time::timeout;
use tower::ServiceExt;
use wiremock::matchers::{header as worker_header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct StreamDropSignal(Option<oneshot::Sender<()>>);

impl Drop for StreamDropSignal {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

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

fn get_request(uri: &str) -> Request<Body> {
    Request::builder().uri(uri).body(Body::empty()).unwrap()
}

fn failure_source_id(status: u16) -> &'static str {
    match status {
        401 => "000000000000000000000401",
        403 => "000000000000000000000403",
        500 => "000000000000000000000500",
        503 => "000000000000000000000503",
        _ => panic!("unexpected test status"),
    }
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
async fn source_content_streams_ranges_and_filters_both_header_directions() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/knowledge/sources/0123456789abcdef01234567/content"))
        .and(query_param("download", "true"))
        .and(worker_header("x-centaurai-internal-token", "private-token"))
        .and(worker_header("range", "bytes=2-5"))
        .and(worker_header("if-range", "\"revision-1\""))
        .respond_with(
            ResponseTemplate::new(206)
                .insert_header("content-type", "application/pdf")
                .insert_header("content-range", "bytes 2-5/10")
                .insert_header("accept-ranges", "bytes")
                .insert_header("etag", "\"revision-1\"")
                .insert_header("content-disposition", "inline; filename=plan.pdf")
                .insert_header("content-security-policy", "default-src *")
                .insert_header("x-worker-secret", "do-not-leak")
                .insert_header("set-cookie", "worker=secret")
                .set_body_bytes(b"2345"),
        )
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
                .uri("/api/knowledge/sources/0123456789abcdef01234567/content?download=true")
                .header(header::RANGE, "bytes=2-5")
                .header(header::IF_RANGE, "\"revision-1\"")
                .header(header::COOKIE, "centaurai-session=must-not-cross")
                .header(header::AUTHORIZATION, "Bearer must-not-cross")
                .header(header::FORWARDED, "for=203.0.113.5")
                .header("x-forwarded-host", "public.example")
                .header(header::HOST, "public.example")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(response.headers()[header::CONTENT_TYPE], "application/pdf");
    assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-5/10");
    assert_eq!(response.headers()[header::ETAG], "\"revision-1\"");
    assert_eq!(
        response.headers()[header::CONTENT_SECURITY_POLICY],
        "sandbox; default-src 'none'; base-uri 'none'; form-action 'none'"
    );
    assert!(!response.headers().contains_key("x-worker-secret"));
    assert!(!response.headers().contains_key(header::SET_COOKIE));
    assert_eq!(response.into_body().collect().await.unwrap().to_bytes(), "2345");

    let requests = server.received_requests().await.unwrap();
    let request = &requests[0];
    assert!(!request.headers.contains_key(header::COOKIE));
    assert!(!request.headers.contains_key(header::AUTHORIZATION));
    assert!(!request.headers.contains_key(header::FORWARDED));
    assert!(!request.headers.contains_key("x-forwarded-host"));
    assert_ne!(request.headers.get(header::HOST).unwrap(), "public.example");
}

#[tokio::test]
async fn source_content_forwards_head_and_preserves_unsatisfied_ranges_without_a_body() {
    let server = MockServer::start().await;
    Mock::given(method("HEAD"))
        .and(path("/api/knowledge/sources/0123456789abcdef01234567/content"))
        .and(worker_header("x-centaurai-internal-token", "private-token"))
        .respond_with(
            ResponseTemplate::new(200)
                .insert_header("content-type", "application/pdf")
                .insert_header("content-length", "10")
                .insert_header("last-modified", "Sun, 12 Jul 2026 00:00:00 GMT"),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/knowledge/sources/0123456789abcdef01234567/content"))
        .and(worker_header("range", "bytes=99-100"))
        .respond_with(
            ResponseTemplate::new(416)
                .insert_header("content-range", "bytes */10")
                .insert_header("x-worker-secret", "private range details")
                .set_body_string("private range parser output"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let gateway =
        KnowledgeGateway::for_loopback_worker(&server.uri(), "private-token", Arc::new(UnknownModelLocationResolver))
            .unwrap();
    let app = knowledge_routes(KnowledgeRouterState {
        gateway: Arc::new(gateway),
    });

    let head = app
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri("/api/knowledge/sources/0123456789abcdef01234567/content")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()[header::CONTENT_LENGTH], "10");
    assert!(head.into_body().collect().await.unwrap().to_bytes().is_empty());

    let unsatisfied = app
        .oneshot(
            Request::builder()
                .uri("/api/knowledge/sources/0123456789abcdef01234567/content")
                .header(header::RANGE, "bytes=99-100")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unsatisfied.status(), StatusCode::RANGE_NOT_SATISFIABLE);
    assert_eq!(unsatisfied.headers()[header::CONTENT_RANGE], "bytes */10");
    assert!(!unsatisfied.headers().contains_key("x-worker-secret"));
    assert!(unsatisfied.into_body().collect().await.unwrap().to_bytes().is_empty());
}

#[tokio::test]
async fn source_content_exposes_the_first_chunk_without_waiting_for_the_complete_file() {
    let (dropped_tx, dropped_rx) = oneshot::channel();
    let dropped_tx = Arc::new(Mutex::new(Some(dropped_tx)));
    let worker = Router::new().route(
        "/api/knowledge/sources/0123456789abcdef01234567/content",
        get(move || {
            let dropped_tx = dropped_tx.clone();
            async move {
                let signal = StreamDropSignal(dropped_tx.lock().unwrap().take());
                let first = futures_util::stream::once(async {
                    Ok::<Bytes, std::io::Error>(Bytes::from_static(b"first chunk"))
                });
                let never_finishes = futures_util::stream::unfold(signal, |signal| async move {
                    std::future::pending::<()>().await;
                    Some((Ok::<Bytes, std::io::Error>(Bytes::new()), signal))
                });
                let mut response = Response::new(Body::from_stream(first.chain(never_finishes)));
                response
                    .headers_mut()
                    .insert(header::CONTENT_TYPE, "application/octet-stream".parse().unwrap());
                response
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let worker_task = tokio::spawn(async move {
        axum::serve(listener, worker).await.unwrap();
    });
    let gateway = KnowledgeGateway::for_loopback_worker(
        &format!("http://{address}/"),
        "private-token",
        Arc::new(UnknownModelLocationResolver),
    )
    .unwrap();
    let app = knowledge_routes(KnowledgeRouterState {
        gateway: Arc::new(gateway),
    });

    let response = timeout(
        Duration::from_secs(2),
        app.oneshot(get_request("/api/knowledge/sources/0123456789abcdef01234567/content")),
    )
    .await
    .expect("Core must return after Worker response headers")
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let mut body = response.into_body();
    let frame = timeout(Duration::from_secs(2), body.frame())
        .await
        .expect("first Worker chunk must not wait for EOF")
        .unwrap()
        .unwrap();
    assert_eq!(frame.into_data().unwrap(), "first chunk");
    drop(body);
    timeout(Duration::from_secs(2), dropped_rx)
        .await
        .expect("dropping the client body must cancel the Worker response")
        .expect("Worker stream cancellation signal must be delivered");
    worker_task.abort();
}

#[tokio::test]
async fn source_content_rejects_unsafe_inputs_and_sanitizes_worker_failures() {
    let server = MockServer::start().await;
    for status in [401, 403, 500, 503] {
        Mock::given(method("GET"))
            .and(path(format!(
                "/api/knowledge/sources/{}/content",
                failure_source_id(status)
            )))
            .respond_with(ResponseTemplate::new(status).set_body_string(format!(
                "private traceback at http://127.0.0.1:8618/{status} token=secret"
            )))
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

    for uri in [
        "/api/knowledge/sources/bad.id/content",
        "/api/knowledge/sources/0123456789abcdef01234567/content?download=yes",
        "/api/knowledge/sources/0123456789abcdef01234567/content?unexpected=true",
    ] {
        let response = app.clone().oneshot(get_request(uri)).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{uri}");
    }
    for value in ["bytes=abc", "bytes=9-2", "bytes=1-2,4-5"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/knowledge/sources/0123456789abcdef01234567/content")
                    .header(header::RANGE, value)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{value}");
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/knowledge/sources/0123456789abcdef01234567/content")
                .header(header::IF_RANGE, "W/\"weak-not-allowed\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/knowledge/sources/0123456789abcdef01234567/content")
                .header(header::IF_RANGE, "\"revision-1\"")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    for (status, expected) in [
        (401, StatusCode::BAD_GATEWAY),
        (403, StatusCode::BAD_GATEWAY),
        (500, StatusCode::SERVICE_UNAVAILABLE),
        (503, StatusCode::SERVICE_UNAVAILABLE),
    ] {
        let response = app
            .clone()
            .oneshot(get_request(&format!(
                "/api/knowledge/sources/{}/content",
                failure_source_id(status)
            )))
            .await
            .unwrap();
        assert_eq!(response.status(), expected);
        let body = json_body(response).await.to_string();
        assert!(!body.contains("traceback"));
        assert!(!body.contains("127.0.0.1"));
        assert!(!body.contains("secret"));
    }
}

#[tokio::test]
async fn graph_proxy_keeps_a_fixed_namespace_and_forwards_only_query_data() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/knowledge/graph/wiki"))
        .and(query_param("depth", "2"))
        .and(worker_header("x-centaurai-internal-token", "private-token"))
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
            .and(worker_header("x-centaurai-internal-token", "private-token"))
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
