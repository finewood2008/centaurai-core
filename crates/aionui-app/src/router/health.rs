//! Health check endpoint.

use axum::Json;
use serde::Serialize;

#[derive(Serialize)]
pub(super) struct HealthResponse {
    status: &'static str,
    service: &'static str,
    version: &'static str,
    build_time: &'static str,
}

pub(super) async fn health_check() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "centaurai-core",
        version: env!("CARGO_PKG_VERSION"),
        build_time: env!("BUILD_TIME"),
    })
}
