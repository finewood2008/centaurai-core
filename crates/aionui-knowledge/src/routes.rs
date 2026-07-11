#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, CreateKnowledgeSourceResponse, CreateKnowledgeSpaceRequest, DeleteKnowledgeSourceResponse,
    KnowledgeJobResponse, KnowledgeSearchRequest, KnowledgeSourceResponse, KnowledgeSpaceResponse,
    KnowledgeStatusResponse, RetrievalBundle, UpdateKnowledgeSpaceRequest,
};
use aionui_common::ApiError;
use axum::body::Body;
use axum::extract::{DefaultBodyLimit, OriginalUri, Path, Query, State};
use axum::http::header::{CONTENT_LENGTH, CONTENT_SECURITY_POLICY, CONTENT_TYPE, IF_RANGE, RANGE};
use axum::http::{HeaderMap, Method, StatusCode};
use axum::response::Response;
use axum::routing::{delete, get, patch, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::Value;

use crate::{KnowledgeError, KnowledgeGateway};

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
    let response = state
        .gateway
        .upload_source(content_type, headers.get(CONTENT_LENGTH).cloned(), body)
        .await?;
    Ok((StatusCode::ACCEPTED, Json(ApiResponse::ok(response))))
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
}
