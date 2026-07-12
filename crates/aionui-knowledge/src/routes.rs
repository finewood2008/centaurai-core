#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, CreateKnowledgeSourceResponse, CreateKnowledgeSpaceRequest, DeleteKnowledgeSourceResponse,
    KnowledgeJobResponse, KnowledgeSearchRequest, KnowledgeSourceResponse, KnowledgeSpaceResponse,
    KnowledgeStatusResponse, RetrievalBundle, UpdateKnowledgeSpaceRequest,
};
use aionui_common::ApiError;
use axum::body::Body;
use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, OriginalUri, Path, Query, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, IF_RANGE, RANGE};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

use crate::gateway::KnowledgeUploadIdempotency;
use crate::{KnowledgeError, KnowledgeGateway};

const MAX_IDEMPOTENT_UPLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;

impl From<KnowledgeError> for ApiError {
    fn from(error: KnowledgeError) -> Self {
        ApiError::coded(error.status_code(), error.error_code(), error.public_message(), None)
    }
}

#[derive(Clone)]
pub struct KnowledgeRouterState {
    pub gateway: Arc<KnowledgeGateway>,
}

#[derive(Clone)]
struct SubresourceState {
    gateway: Arc<KnowledgeGateway>,
    namespace: &'static str,
}

/// Public knowledge routes. The caller applies the same owner authentication
/// and CSRF middleware used by the rest of the Core API.
pub fn knowledge_routes(state: KnowledgeRouterState) -> Router {
    let upload = Router::new()
        .route("/api/knowledge/sources", post(create_source))
        .route_layer(DefaultBodyLimit::disable())
        .with_state(state.clone());

    Router::new()
        .route("/api/knowledge/status", get(status))
        .route("/api/knowledge/spaces", get(list_spaces).post(create_space))
        .route("/api/knowledge/spaces/{id}", patch(update_space))
        .route("/api/knowledge/sources", get(list_sources))
        .route("/api/knowledge/sources/{id}/content", get(source_content))
        .route("/api/knowledge/sources/{id}", delete(delete_source))
        .route("/api/knowledge/jobs/{id}", get(job))
        .route("/api/knowledge/search", post(search))
        .with_state(state.clone())
        .merge(upload)
        .nest(
            "/api/knowledge/wiki",
            read_write_subresources(SubresourceState {
                gateway: state.gateway.clone(),
                namespace: "wiki",
            }),
        )
        .nest(
            "/api/knowledge/memory",
            read_write_delete_subresources(SubresourceState {
                gateway: state.gateway.clone(),
                namespace: "memory",
            }),
        )
        .nest(
            "/api/knowledge/graph",
            read_only_subresources(SubresourceState {
                gateway: state.gateway,
                namespace: "graph",
            }),
        )
}

fn read_only_subresources(state: SubresourceState) -> Router {
    Router::new().route("/{*path}", get(subresource_get)).with_state(state)
}

fn read_write_subresources(state: SubresourceState) -> Router {
    Router::new()
        .route(
            "/{*path}",
            get(subresource_get).post(subresource_post).put(subresource_put),
        )
        .with_state(state)
}

fn read_write_delete_subresources(state: SubresourceState) -> Router {
    Router::new()
        .route(
            "/{*path}",
            get(subresource_get)
                .post(subresource_post)
                .put(subresource_put)
                .delete(subresource_delete),
        )
        .with_state(state)
}

async fn status(State(state): State<KnowledgeRouterState>) -> Json<ApiResponse<KnowledgeStatusResponse>> {
    Json(ApiResponse::ok(state.gateway.status().await))
}

async fn list_spaces(
    State(state): State<KnowledgeRouterState>,
) -> Result<Json<ApiResponse<Vec<KnowledgeSpaceResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.gateway.list_spaces().await?)))
}

async fn create_space(
    State(state): State<KnowledgeRouterState>,
    Json(request): Json<CreateKnowledgeSpaceRequest>,
) -> Result<(StatusCode, Json<ApiResponse<KnowledgeSpaceResponse>>), ApiError> {
    let space = state.gateway.create_space(request).await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(space))))
}

async fn update_space(
    State(state): State<KnowledgeRouterState>,
    Path(id): Path<String>,
    Json(request): Json<UpdateKnowledgeSpaceRequest>,
) -> Result<Json<ApiResponse<KnowledgeSpaceResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.gateway.update_space(&id, request).await?)))
}

#[derive(Debug, Deserialize)]
struct SourceListQuery {
    space_id: Option<String>,
    #[serde(default = "default_source_limit")]
    limit: u32,
    #[serde(default)]
    offset: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceContentQuery {
    download: Option<bool>,
}

fn default_source_limit() -> u32 {
    100
}

async fn list_sources(
    State(state): State<KnowledgeRouterState>,
    Query(query): Query<SourceListQuery>,
) -> Result<Json<ApiResponse<Vec<KnowledgeSourceResponse>>>, ApiError> {
    let sources = state
        .gateway
        .list_sources(query.space_id.as_deref(), query.limit, query.offset)
        .await?;
    Ok(Json(ApiResponse::ok(sources)))
}

async fn create_source(
    State(state): State<KnowledgeRouterState>,
    user: Option<axum::extract::Extension<aionui_auth::CurrentUser>>,
    headers: HeaderMap,
    body: Body,
) -> Result<(StatusCode, Json<ApiResponse<CreateKnowledgeSourceResponse>>), ApiError> {
    let content_type = headers
        .get(CONTENT_TYPE)
        .filter(|value| {
            value
                .to_str()
                .is_ok_and(|value| value.to_ascii_lowercase().starts_with("multipart/form-data;"))
        })
        .cloned()
        .ok_or(KnowledgeError::InvalidRequest)?;
    let operation_key = upload_operation_key(&headers)?;
    let (body, content_length, request_fingerprint, _spool_guard) = if operation_key.is_some() {
        let spooled = spool_idempotent_upload(&headers, &content_type, body).await?;
        (
            spooled.body,
            Some(spooled.content_length),
            Some(spooled.request_fingerprint),
            Some(spooled.guard),
        )
    } else {
        (body, headers.get(CONTENT_LENGTH).cloned(), None, None)
    };
    let idempotency = operation_key
        .zip(request_fingerprint.as_deref())
        .map(|(operation_key, request_fingerprint)| KnowledgeUploadIdempotency {
            operation_key,
            request_fingerprint,
        });
    let response = state
        .gateway
        .upload_source_idempotent(
            user.as_ref().map(|axum::extract::Extension(user)| user.id.as_str()),
            idempotency,
            content_type,
            content_length,
            body,
        )
        .await?;
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::ok(response))))
}

fn upload_operation_key(headers: &HeaderMap) -> Result<Option<&str>, KnowledgeError> {
    let canonical = header_text(headers, "idempotency-key")?;
    let client = header_text(headers, "x-client-operation-id")?;
    if let (Some(canonical), Some(client)) = (canonical, client)
        && canonical != client
    {
        return Err(KnowledgeError::Conflict);
    }
    let operation_key = canonical.or(client);
    if let Some(value) = operation_key
        && (!(8..=128).contains(&value.len())
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')))
    {
        return Err(KnowledgeError::InvalidRequest);
    }
    Ok(operation_key)
}

fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, KnowledgeError> {
    if headers.get_all(name).iter().count() > 1 {
        return Err(KnowledgeError::InvalidRequest);
    }
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .map(str::trim)
                .map_err(|_| KnowledgeError::InvalidRequest)
        })
        .transpose()
        .map(|value| value.filter(|value| !value.is_empty()))
}

struct SpooledUpload {
    body: Body,
    content_length: axum::http::HeaderValue,
    request_fingerprint: String,
    guard: tempfile::NamedTempFile,
}

async fn spool_idempotent_upload(
    headers: &HeaderMap,
    content_type: &axum::http::HeaderValue,
    body: Body,
) -> Result<SpooledUpload, KnowledgeError> {
    if let Some(declared) = header_text(headers, "content-length")? {
        let declared = declared.parse::<u64>().map_err(|_| KnowledgeError::InvalidRequest)?;
        if declared > MAX_IDEMPOTENT_UPLOAD_BYTES {
            return Err(KnowledgeError::PayloadTooLarge);
        }
    }
    let guard = tempfile::NamedTempFile::new().map_err(|error| {
        tracing::error!(error = %error, "failed to create idempotent upload spool");
        KnowledgeError::Unavailable
    })?;
    let writer = guard.reopen().map_err(|error| {
        tracing::error!(error = %error, "failed to open idempotent upload spool");
        KnowledgeError::Unavailable
    })?;
    let mut file = tokio::fs::File::from_std(writer);
    let mut stream = body.into_data_stream();
    let mut actual_length = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| KnowledgeError::InvalidRequest)?;
        actual_length = actual_length
            .checked_add(chunk.len() as u64)
            .ok_or(KnowledgeError::PayloadTooLarge)?;
        if actual_length > MAX_IDEMPOTENT_UPLOAD_BYTES {
            return Err(KnowledgeError::PayloadTooLarge);
        }
        file.write_all(&chunk).await.map_err(|error| {
            tracing::error!(error = %error, "failed to write idempotent upload spool");
            KnowledgeError::Unavailable
        })?;
    }
    file.flush().await.map_err(|error| {
        tracing::error!(error = %error, "failed to flush idempotent upload spool");
        KnowledgeError::Unavailable
    })?;
    if let Some(declared) = header_text(headers, "content-length")?
        && declared.parse::<u64>().map_err(|_| KnowledgeError::InvalidRequest)? != actual_length
    {
        return Err(KnowledgeError::InvalidRequest);
    }
    let content_type_text = content_type.to_str().map_err(|_| KnowledgeError::InvalidRequest)?;
    let body_digest = multipart_content_digest(&guard, content_type_text).await?;
    if let Some(declared_digest) = header_text(headers, "x-content-sha256")?
        && (declared_digest.len() != 64
            || !declared_digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            || !declared_digest.eq_ignore_ascii_case(&body_digest))
    {
        return Err(KnowledgeError::InvalidRequest);
    }
    let media_type = content_type_text
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    let request_fingerprint = hex::encode(Sha256::digest(format!("{media_type}\0{body_digest}")));
    file.seek(std::io::SeekFrom::Start(0)).await.map_err(|error| {
        tracing::error!(error = %error, "failed to rewind idempotent upload spool");
        KnowledgeError::Unavailable
    })?;
    let body_stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut buffer = vec![0u8; 64 * 1024];
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        buffer.truncate(read);
        Ok(Some((Bytes::from(buffer), file)))
    });
    Ok(SpooledUpload {
        body: Body::from_stream(body_stream),
        content_length: actual_length
            .to_string()
            .parse()
            .map_err(|_| KnowledgeError::InvalidRequest)?,
        request_fingerprint,
        guard,
    })
}

async fn multipart_content_digest(
    guard: &tempfile::NamedTempFile,
    content_type: &str,
) -> Result<String, KnowledgeError> {
    let boundary = multer::parse_boundary(content_type).map_err(|_| KnowledgeError::InvalidRequest)?;
    let reader = guard.reopen().map_err(|error| {
        tracing::error!(error = %error, "failed to reopen idempotent upload spool");
        KnowledgeError::Unavailable
    })?;
    let file = tokio::fs::File::from_std(reader);
    let stream = futures_util::stream::try_unfold(file, |mut file| async move {
        let mut buffer = vec![0u8; 64 * 1024];
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            return Ok::<_, std::io::Error>(None);
        }
        buffer.truncate(read);
        Ok(Some((Bytes::from(buffer), file)))
    });
    let mut multipart = multer::Multipart::new(stream, boundary);
    let mut digest = Sha256::new();
    let mut field_count = 0u64;
    while let Some(mut field) = multipart
        .next_field()
        .await
        .map_err(|_| KnowledgeError::InvalidRequest)?
    {
        field_count += 1;
        hash_text_component(&mut digest, field.name().unwrap_or_default());
        hash_text_component(&mut digest, field.file_name().unwrap_or_default());
        hash_text_component(
            &mut digest,
            field
                .content_type()
                .map(ToString::to_string)
                .as_deref()
                .unwrap_or_default(),
        );
        let mut content_digest = Sha256::new();
        let mut content_length = 0u64;
        while let Some(chunk) = field.chunk().await.map_err(|_| KnowledgeError::InvalidRequest)? {
            content_length = content_length
                .checked_add(chunk.len() as u64)
                .ok_or(KnowledgeError::PayloadTooLarge)?;
            content_digest.update(&chunk);
        }
        digest.update(content_length.to_be_bytes());
        digest.update(content_digest.finalize());
    }
    digest.update(field_count.to_be_bytes());
    Ok(hex::encode(digest.finalize()))
}

fn hash_text_component(digest: &mut Sha256, value: &str) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value.as_bytes());
}

async fn delete_source(
    State(state): State<KnowledgeRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DeleteKnowledgeSourceResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.gateway.delete_source(&id).await?)))
}

async fn source_content(
    State(state): State<KnowledgeRouterState>,
    Path(id): Path<String>,
    Query(query): Query<SourceContentQuery>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if headers.get_all(RANGE).iter().count() > 1 || headers.get_all(IF_RANGE).iter().count() > 1 {
        return Err(KnowledgeError::InvalidRequest.into());
    }
    let content = state
        .gateway
        .source_content(
            &id,
            query.download,
            method,
            headers.get(RANGE).cloned(),
            headers.get(IF_RANGE).cloned(),
        )
        .await?;
    let mut response = Response::new(content.body);
    *response.status_mut() = content.status;
    *response.headers_mut() = content.headers;
    response.headers_mut().insert(
        CONTENT_SECURITY_POLICY,
        "sandbox; default-src 'none'; base-uri 'none'; form-action 'none'"
            .parse()
            .expect("static knowledge content policy is valid"),
    );
    Ok(response)
}

async fn job(
    State(state): State<KnowledgeRouterState>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<KnowledgeJobResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.gateway.job(&id).await?)))
}

async fn search(
    State(state): State<KnowledgeRouterState>,
    Json(request): Json<KnowledgeSearchRequest>,
) -> Result<Json<ApiResponse<RetrievalBundle>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.gateway.search(request).await?)))
}

async fn subresource_get(
    State(state): State<SubresourceState>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    let path = subresource_path(&state, &path)?;
    Ok(Json(ApiResponse::ok(
        state.gateway.proxy_get(&path, uri.query()).await?,
    )))
}

async fn subresource_post(
    State(state): State<SubresourceState>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    Json(body): Json<Value>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    subresource_write(state, path, uri.query(), Method::POST, body).await
}

async fn subresource_put(
    State(state): State<SubresourceState>,
    Path(path): Path<String>,
    OriginalUri(uri): OriginalUri,
    Json(body): Json<Value>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    subresource_write(state, path, uri.query(), Method::PUT, body).await
}

async fn subresource_write(
    state: SubresourceState,
    path: String,
    query: Option<&str>,
    method: Method,
    body: Value,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    let path = subresource_path(&state, &path)?;
    Ok(Json(ApiResponse::ok(
        state.gateway.proxy_json(method, &path, query, &body).await?,
    )))
}

async fn subresource_delete(
    State(state): State<SubresourceState>,
    Path(path): Path<String>,
) -> Result<Json<ApiResponse<Value>>, ApiError> {
    let path = subresource_path(&state, &path)?;
    Ok(Json(ApiResponse::ok(state.gateway.proxy_delete(&path).await?)))
}

fn subresource_path(state: &SubresourceState, path: &str) -> Result<String, KnowledgeError> {
    if path.is_empty()
        || path.len() > 2_000
        || path
            .chars()
            .any(|character| matches!(character, '\\' | '?' | '#' | '\0' | '\r' | '\n'))
        || path.split('/').any(|segment| matches!(segment, "." | ".."))
    {
        return Err(KnowledgeError::InvalidRequest);
    }
    Ok(format!("/api/knowledge/{}/{}", state.namespace, path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use aionui_auth::CurrentUser;
    use aionui_db::init_database_memory;
    use axum::Extension;
    use axum::http::Request;
    use tower::ServiceExt;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn worker_upload_response() -> serde_json::Value {
        serde_json::json!({
            "source": {
                "id": "source_1", "space_id": "personal", "title": "Plan.pdf",
                "media_type": "pdf", "status": "ready",
                "created_at": "2026-07-12T00:00:00Z", "updated_at": "2026-07-12T00:00:00Z"
            },
            "job": {
                "id": "job_1", "source_id": "source_1", "kind": "index_source",
                "status": "completed", "progress": 1.0
            },
            "job_id": "job_1"
        })
    }

    fn idempotent_upload_request(operation_key: &str) -> Request<Body> {
        let body =
            "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"Plan.pdf\"\r\n\r\npdf\r\n--b--\r\n";
        Request::builder()
            .method("POST")
            .uri("/api/knowledge/sources")
            .header("content-type", "multipart/form-data; boundary=b")
            .header("content-length", body.len().to_string())
            .header("idempotency-key", operation_key)
            .body(Body::from(body))
            .unwrap()
    }

    #[test]
    fn subresource_path_cannot_escape_namespace() {
        let state = SubresourceState {
            gateway: Arc::new(KnowledgeGateway::unavailable()),
            namespace: "wiki",
        };
        assert_eq!(
            subresource_path(&state, "pages/Plan.md").unwrap(),
            "/api/knowledge/wiki/pages/Plan.md"
        );
        assert!(subresource_path(&state, "../status").is_err());
        assert!(subresource_path(&state, "pages\\status").is_err());
    }

    #[test]
    fn upload_operation_headers_are_canonical_and_conflict_safe() {
        let mut headers = HeaderMap::new();
        headers.insert("idempotency-key", "upload-op-0001".parse().unwrap());
        headers.insert("x-client-operation-id", "upload-op-0001".parse().unwrap());
        assert_eq!(upload_operation_key(&headers).unwrap(), Some("upload-op-0001"));
        headers.insert("x-client-operation-id", "different-op-0002".parse().unwrap());
        assert!(matches!(upload_operation_key(&headers), Err(KnowledgeError::Conflict)));
        headers.clear();
        headers.insert("idempotency-key", "short".parse().unwrap());
        assert!(matches!(
            upload_operation_key(&headers),
            Err(KnowledgeError::InvalidRequest)
        ));
    }

    #[tokio::test]
    async fn source_upload_idempotency_replays_original_and_is_user_scoped() {
        let worker = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/knowledge/sources"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "source": {
                    "id": "source_1", "space_id": "personal", "title": "Plan.pdf",
                    "media_type": "pdf", "status": "ready",
                    "created_at": "2026-07-12T00:00:00Z", "updated_at": "2026-07-12T00:00:00Z"
                },
                "job": {
                    "id": "job_1", "source_id": "source_1", "kind": "index_source",
                    "status": "completed", "progress": 1.0
                },
                "job_id": "job_1"
            })))
            .expect(3)
            .mount(&worker)
            .await;
        let database = init_database_memory().await.unwrap();
        let now = aionui_common::now_ms();
        sqlx::query("INSERT INTO users (id, username, password_hash, created_at, updated_at) VALUES (?, ?, '', ?, ?)")
            .bind("other-user")
            .bind("other-user")
            .bind(now)
            .bind(now)
            .execute(database.pool())
            .await
            .unwrap();
        let gateway = KnowledgeGateway::for_loopback_worker(
            &worker.uri(),
            "internal-token",
            Arc::new(crate::UnknownModelLocationResolver),
        )
        .unwrap()
        .with_idempotency_pool(database.pool().clone());
        let state = KnowledgeRouterState {
            gateway: Arc::new(gateway),
        };
        let owner = knowledge_routes(state.clone()).layer(Extension(CurrentUser {
            id: "system_default_user".into(),
            username: "owner".into(),
            is_admin: true,
        }));
        let body =
            "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"Plan.pdf\"\r\n\r\npdf\r\n--b--\r\n";
        let request = || {
            Request::builder()
                .method("POST")
                .uri("/api/knowledge/sources")
                .header("content-type", "multipart/form-data; boundary=b")
                .header("content-length", body.len().to_string())
                .header("idempotency-key", "upload-op-0001")
                .body(Body::from(body))
                .unwrap()
        };
        let first = owner.clone().oneshot(request()).await.unwrap();
        let same_content_new_boundary = body.replace("--b", "--different");
        let retry = owner
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/knowledge/sources")
                    .header("content-type", "multipart/form-data; boundary=different")
                    .header("content-length", same_content_new_boundary.len().to_string())
                    .header("idempotency-key", "upload-op-0001")
                    .body(Body::from(same_content_new_boundary))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::ACCEPTED);
        assert_eq!(retry.status(), StatusCode::ACCEPTED);

        let different_body = body.replace("pdf", "doc");
        assert_eq!(different_body.len(), body.len());
        let conflict = owner
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/knowledge/sources")
                    .header("content-type", "multipart/form-data; boundary=b")
                    .header("content-length", different_body.len().to_string())
                    .header("idempotency-key", "upload-op-0001")
                    .body(Body::from(different_body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(conflict.status(), StatusCode::CONFLICT);

        let forged_digest = owner
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/knowledge/sources")
                    .header("content-type", "multipart/form-data; boundary=b")
                    .header("content-length", body.len().to_string())
                    .header("idempotency-key", "forged-digest-op")
                    .header("x-content-sha256", "0".repeat(64))
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(forged_digest.status(), StatusCode::BAD_REQUEST);

        let concurrent_request = || {
            Request::builder()
                .method("POST")
                .uri("/api/knowledge/sources")
                .header("content-type", "multipart/form-data; boundary=b")
                .header("content-length", body.len().to_string())
                .header("idempotency-key", "concurrent-op-0001")
                .body(Body::from(body))
                .unwrap()
        };
        let (left, right) = tokio::join!(
            owner.clone().oneshot(concurrent_request()),
            owner.oneshot(concurrent_request())
        );
        assert_eq!(left.unwrap().status(), StatusCode::ACCEPTED);
        assert_eq!(right.unwrap().status(), StatusCode::ACCEPTED);

        let other = knowledge_routes(state).layer(Extension(CurrentUser {
            id: "other-user".into(),
            username: "other".into(),
            is_admin: true,
        }));
        assert_eq!(other.oneshot(request()).await.unwrap().status(), StatusCode::ACCEPTED);
        worker.verify().await;
        let worker_requests = worker.received_requests().await.unwrap();
        assert_eq!(worker_requests.len(), 3);
        for request in worker_requests {
            let operation_key = request.headers.get("idempotency-key").unwrap().to_str().unwrap();
            assert!(operation_key.starts_with("core-"));
            assert_eq!(operation_key.len(), 69);
            assert_eq!(
                request
                    .headers
                    .get("x-centaurai-request-fingerprint")
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .len(),
                64
            );
        }
    }

    #[tokio::test]
    async fn source_upload_idempotency_coordinates_separate_core_instances() {
        let worker = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/knowledge/sources"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_delay(std::time::Duration::from_millis(80))
                    .set_body_json(worker_upload_response()),
            )
            .expect(1)
            .mount(&worker)
            .await;
        let database = init_database_memory().await.unwrap();
        let build = || {
            let gateway = KnowledgeGateway::for_loopback_worker(
                &worker.uri(),
                "internal-token",
                Arc::new(crate::UnknownModelLocationResolver),
            )
            .unwrap()
            .with_idempotency_pool(database.pool().clone());
            knowledge_routes(KnowledgeRouterState {
                gateway: Arc::new(gateway),
            })
            .layer(Extension(CurrentUser {
                id: "system_default_user".into(),
                username: "owner".into(),
                is_admin: true,
            }))
        };
        let (left, right) = tokio::join!(
            build().oneshot(idempotent_upload_request("multi-core-op-0001")),
            build().oneshot(idempotent_upload_request("multi-core-op-0001")),
        );
        assert_eq!(left.unwrap().status(), StatusCode::ACCEPTED);
        assert_eq!(right.unwrap().status(), StatusCode::ACCEPTED);
        worker.verify().await;
    }

    #[tokio::test]
    async fn source_upload_idempotency_recovers_an_expired_durable_lease() {
        let worker = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/knowledge/sources"))
            .respond_with(ResponseTemplate::new(200).set_body_json(worker_upload_response()))
            .expect(1)
            .mount(&worker)
            .await;
        let database = init_database_memory().await.unwrap();
        let body =
            "--b\r\nContent-Disposition: form-data; name=\"file\"; filename=\"Plan.pdf\"\r\n\r\npdf\r\n--b--\r\n";
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, body.len().to_string().parse().unwrap());
        let content_type: axum::http::HeaderValue = "multipart/form-data; boundary=b".parse().unwrap();
        let fingerprint = spool_idempotent_upload(&headers, &content_type, Body::from(body))
            .await
            .unwrap()
            .request_fingerprint;
        sqlx::query(
            "INSERT INTO idempotency_records (id, user_id, operation_scope, operation_key, request_fingerprint, \
             resource_id, state, lease_owner, lease_expires_at, created_at) \
             VALUES ('expired-upload', 'system_default_user', 'knowledge.source.upload', \
             'expired-lease-op', ?, '', 'pending', 'dead-core', 0, ?)",
        )
        .bind(&fingerprint)
        .bind(aionui_common::now_ms())
        .execute(database.pool())
        .await
        .unwrap();

        let gateway = KnowledgeGateway::for_loopback_worker(
            &worker.uri(),
            "internal-token",
            Arc::new(crate::UnknownModelLocationResolver),
        )
        .unwrap()
        .with_idempotency_pool(database.pool().clone());
        let app = knowledge_routes(KnowledgeRouterState {
            gateway: Arc::new(gateway),
        })
        .layer(Extension(CurrentUser {
            id: "system_default_user".into(),
            username: "owner".into(),
            is_admin: true,
        }));
        let response = app
            .oneshot(idempotent_upload_request("expired-lease-op"))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let state: String = sqlx::query_scalar("SELECT state FROM idempotency_records WHERE id = 'expired-upload'")
            .fetch_one(database.pool())
            .await
            .unwrap();
        assert_eq!(state, "completed");
        worker.verify().await;
    }
}
