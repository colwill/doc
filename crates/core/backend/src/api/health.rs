//! Liveness (`/healthz`) and readiness (`/readyz`), which reports each dependency it checks.

use axum::Json;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use serde_json::json;

use super::problem::Problem;
use super::{AppState, VERSION};

pub async fn healthz(State(state): State<AppState>) -> Response {
    Json(json!({
        "status": "ok",
        "version": VERSION,
        "uptime_s": state.started.elapsed().as_secs(),
    }))
    .into_response()
}

pub async fn readyz(State(state): State<AppState>) -> Response {
    let database = match state.repos.health.ping().await {
        Ok(()) => json!({"status": "up"}),
        Err(err) => json!({"status": "down", "error": err.to_string()}),
    };
    let ready = database["status"] == "up";
    let checks = json!({"database": database});
    if ready {
        Json(json!({"status": "ready", "checks": checks})).into_response()
    } else {
        Problem::unavailable("a dependency is not available").with("checks", checks).into_response()
    }
}

#[cfg(test)]
mod tests {
    use crate::testing::{get, test_app};
    use http::StatusCode;

    #[tokio::test]
    async fn healthz_is_ok_without_a_database() {
        let (app, database) = test_app();
        database.set_down(true);
        let (status, body, _) = get(&app, "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ok");
        assert_eq!(body["version"], super::VERSION);
    }

    #[tokio::test]
    async fn readyz_is_ready_when_the_database_answers() {
        let (app, _) = test_app();
        let (status, body, _) = get(&app, "/readyz").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "ready");
        assert_eq!(body["checks"]["database"]["status"], "up");
    }

    #[tokio::test]
    async fn readyz_reports_a_stopped_database() {
        let (app, database) = test_app();
        database.set_down(true);
        let (status, body, content_type) = get(&app, "/readyz").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(content_type.as_deref(), Some("application/problem+json"));
        assert_eq!(body["checks"]["database"]["status"], "down");
        assert_eq!(body["checks"]["database"]["error"], "storage unavailable: connection refused");

        database.set_down(false);
        let (status, _, _) = get(&app, "/readyz").await;
        assert_eq!(status, StatusCode::OK);
    }
}
