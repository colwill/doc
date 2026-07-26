//! {{ values.name }}'s HTTP API. Every request is traced and counted through the platform's own
//! telemetry stack, and what it answers follows the flags DOC holds for this service.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use serde_json::json;
use tower_http::trace::TraceLayer;
use tracing::instrument;

use crate::platform::{Config, Flags};

#[derive(Clone)]
pub struct Api {
    pub config: Config,
    pub flags: Flags,
    pub requests: Counter<u64>,
}

/// Everything {{ values.name }} answers.
pub fn routes(config: Config, flags: Flags) -> Router {
    let meter = opentelemetry::global::meter(config.service.clone());
    let requests = meter
        .u64_counter("http.server.requests")
        .with_description("Requests this service answered")
        .build();

    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/v1/hello", get(hello))
        .layer(TraceLayer::new_for_http())
        .with_state(Api { config, flags, requests })
}

/// Liveness: the process is up. Kubernetes restarts it when this stops answering.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, Json(json!({ "status": "ok" })))
}

/// Readiness: it is up and willing to take traffic, which a flag can withdraw.
async fn readyz(State(api): State<Api>) -> impl IntoResponse {
    match api.flags.bool("accepting-traffic", true) {
        true => (StatusCode::OK, Json(json!({ "status": "ready" }))),
        false => (StatusCode::SERVICE_UNAVAILABLE, Json(json!({ "status": "draining" }))),
    }
}

#[instrument(skip_all)]
async fn hello(State(api): State<Api>) -> impl IntoResponse {
    api.requests.add(1, &[KeyValue::new("http.route", "/api/v1/hello")]);

    // What the service says is decided in DOC, with a fallback it keeps working on.
    let greeting = api.flags.string("greeting", "Hello");
    Json(json!({
        "message": greeting,
        "service": api.config.service,
        "environment": api.config.environment,
    }))
}
