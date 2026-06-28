//! Liveness (`/healthz`): the frontend is up whether or not the backend answers.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::{AppState, VERSION};

pub async fn healthz(State(state): State<AppState>) -> Response {
    Json(json!({
        "status": "ok",
        "version": VERSION,
        "uptime_s": state.started.elapsed().as_secs(),
    }))
    .into_response()
}
