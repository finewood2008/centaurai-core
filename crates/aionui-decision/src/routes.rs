#![allow(clippy::disallowed_types)]

use std::sync::Arc;

use aionui_api_types::{
    ApiResponse, CreateDecisionRequest, DecisionResponse, InterjectDecisionRequest, RefreshDecisionKnowledgeRequest,
    SelectDecisionCandidateRequest, UpdateDecisionRequest,
};
use aionui_auth::CurrentUser;
use aionui_common::ApiError;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Extension, Json, Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::{get, post};

use crate::{DecisionError, DecisionService};

#[derive(Clone)]
pub struct DecisionRouterState {
    pub service: Arc<DecisionService>,
}

pub fn decision_routes(state: DecisionRouterState) -> Router {
    Router::new()
        .route("/api/decisions", get(list_decisions).post(create_decision))
        .route("/api/decisions/{id}", get(get_decision).patch(update_decision))
        .route("/api/decisions/{id}/start", post(start_decision))
        .route("/api/decisions/{id}/interject", post(interject_decision))
        .route("/api/decisions/{id}/continue", post(continue_decision))
        .route("/api/decisions/{id}/cancel", post(cancel_decision))
        .route("/api/decisions/{id}/select", post(select_candidate))
        .route("/api/decisions/{id}/refresh-knowledge", post(refresh_knowledge))
        .with_state(state)
}

async fn create_decision(
    State(state): State<DecisionRouterState>,
    Extension(user): Extension<CurrentUser>,
    headers: HeaderMap,
    body: Result<Json<CreateDecisionRequest>, JsonRejection>,
) -> Result<(StatusCode, Json<ApiResponse<DecisionResponse>>), ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    let operation_id = header_value(&headers, "idempotency-key")?;
    let response = state
        .service
        .create_with_idempotency(&user.id, request, operation_id)
        .await?;
    Ok((StatusCode::CREATED, Json(ApiResponse::ok(response))))
}

fn header_value<'a>(headers: &'a HeaderMap, name: &str) -> Result<Option<&'a str>, ApiError> {
    if headers.get_all(name).iter().count() > 1 {
        return Err(ApiError::BadRequest(format!("{name} must be supplied once")));
    }
    headers
        .get(name)
        .map(|value| {
            value
                .to_str()
                .map_err(|_| ApiError::BadRequest(format!("{name} must contain visible ASCII")))
        })
        .transpose()
}

async fn list_decisions(
    State(state): State<DecisionRouterState>,
    Extension(user): Extension<CurrentUser>,
) -> Result<Json<ApiResponse<Vec<DecisionResponse>>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.list(&user.id).await?)))
}

async fn get_decision(
    State(state): State<DecisionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DecisionResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.get(&user.id, &id).await?)))
}

async fn update_decision(
    State(state): State<DecisionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<UpdateDecisionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<DecisionResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.update(&user.id, &id, request).await?,
    )))
}

async fn start_decision(
    State(state): State<DecisionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DecisionResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.start(&user.id, &id).await?)))
}

async fn interject_decision(
    State(state): State<DecisionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<InterjectDecisionRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<DecisionResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.interject(&user.id, &id, request).await?,
    )))
}

async fn continue_decision(
    State(state): State<DecisionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DecisionResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(
        state.service.continue_decision(&user.id, &id).await?,
    )))
}

async fn cancel_decision(
    State(state): State<DecisionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
) -> Result<Json<ApiResponse<DecisionResponse>>, ApiError> {
    Ok(Json(ApiResponse::ok(state.service.cancel(&user.id, &id).await?)))
}

async fn select_candidate(
    State(state): State<DecisionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<SelectDecisionCandidateRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<DecisionResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.select(&user.id, &id, request).await?,
    )))
}

async fn refresh_knowledge(
    State(state): State<DecisionRouterState>,
    Extension(user): Extension<CurrentUser>,
    Path(id): Path<String>,
    body: Result<Json<RefreshDecisionKnowledgeRequest>, JsonRejection>,
) -> Result<Json<ApiResponse<DecisionResponse>>, ApiError> {
    let Json(request) = body.map_err(ApiError::from)?;
    Ok(Json(ApiResponse::ok(
        state.service.refresh_knowledge(&user.id, &id, request).await?,
    )))
}

impl From<DecisionError> for ApiError {
    fn from(error: DecisionError) -> Self {
        match error {
            DecisionError::NotFound => ApiError::NotFound("Decision not found".into()),
            DecisionError::InvalidRequest(message) => ApiError::BadRequest(message),
            DecisionError::Conflict(message) => ApiError::Conflict(message),
            DecisionError::ProviderUnavailable(message) => ApiError::coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "DECISION_PROVIDER_UNAVAILABLE",
                message,
                None,
            ),
            DecisionError::KnowledgeUnavailable(message) => ApiError::coded(
                StatusCode::SERVICE_UNAVAILABLE,
                "DECISION_KNOWLEDGE_UNAVAILABLE",
                message,
                None,
            ),
            DecisionError::Database(error) => {
                tracing::error!(error = %error, "decision database operation failed");
                ApiError::Internal("decision persistence failed".into())
            }
            DecisionError::Json(error) => {
                tracing::error!(error = %error, "decision serialization failed");
                ApiError::Internal("decision serialization failed".into())
            }
            DecisionError::Internal(message) => {
                tracing::error!(error = %message, "internal decision operation failed");
                ApiError::Internal("decision operation failed".into())
            }
        }
    }
}
