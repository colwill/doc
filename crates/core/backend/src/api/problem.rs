//! RFC 9457 problem details: the one error shape every endpoint returns.

use axum::Json;
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use serde_json::{Map, Value, json};

/// The detail and extensions are boxed and usually absent, which keeps `Result<_, Problem>` small
/// enough that every handler is not moving a large error on its success path.
#[derive(Debug, Clone, Default)]
struct Extra {
    detail: Option<String>,
    extensions: Map<String, Value>,
    retry_after: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Problem {
    pub status: StatusCode,
    pub kind: &'static str,
    pub title: String,
    extra: Option<Box<Extra>>,
}

impl Problem {
    pub fn new(status: StatusCode, kind: &'static str, title: impl Into<String>) -> Self {
        Self { status, kind, title: title.into(), extra: None }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.extra.get_or_insert_default().detail = Some(detail.into());
        self
    }

    pub fn detail_text(&self) -> Option<&str> {
        self.extra.as_ref()?.detail.as_deref()
    }

    pub fn with(mut self, key: &str, value: impl Into<Value>) -> Self {
        self.extra.get_or_insert_default().extensions.insert(key.to_string(), value.into());
        self
    }

    pub fn not_found(what: &str) -> Self {
        Self::new(StatusCode::NOT_FOUND, "not-found", format!("{what} not found"))
    }

    pub fn unauthorized() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "unauthorized", "Authentication required")
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", "Access denied").detail(detail)
    }

    pub fn bad_request(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "bad-request", "Invalid request").detail(detail)
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", "Conflict").detail(detail)
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::SERVICE_UNAVAILABLE, "unavailable", "Service unavailable")
            .detail(detail)
    }

    /// `429`, with `Retry-After` saying when to try again.
    pub fn too_many(detail: impl Into<String>, wait: std::time::Duration) -> Self {
        let mut problem =
            Self::new(StatusCode::TOO_MANY_REQUESTS, "too-many-requests", "Too many requests")
                .detail(detail);
        problem.extra.get_or_insert_default().retry_after = Some(wait.as_secs().max(1));
        problem
    }

    pub fn internal(detail: impl Into<String>) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "internal", "Internal error").detail(detail)
    }
}

/// The detail is logged, not returned: a storage error's text can name schemas and columns.
impl From<crate::db::repositories::RepositoryError> for Problem {
    fn from(err: crate::db::repositories::RepositoryError) -> Self {
        use crate::db::repositories::RepositoryError;
        if let RepositoryError::Conflict(detail) = err {
            return Self::conflict(detail);
        }
        tracing::error!(%err, "a repository call failed");
        match err {
            RepositoryError::Unavailable(_) => Self::unavailable("the database is unavailable"),
            _ => Self::internal("the request could not be completed"),
        }
    }
}

impl IntoResponse for Problem {
    fn into_response(self) -> Response {
        let mut body = json!({
            "type": format!("/problems/{}", self.kind),
            "title": self.title,
            "status": self.status.as_u16(),
        });
        let mut retry_after = None;
        if let (Some(map), Some(extra)) = (body.as_object_mut(), self.extra) {
            if let Some(detail) = extra.detail {
                map.insert("detail".into(), Value::String(detail));
            }
            map.extend(extra.extensions);
            retry_after = extra.retry_after;
        }
        let mut response = (self.status, Json(body)).into_response();
        if let Some(seconds) = retry_after {
            response
                .headers_mut()
                .insert(http::header::RETRY_AFTER, http::HeaderValue::from(seconds));
        }
        response.headers_mut().insert(
            http::header::CONTENT_TYPE,
            http::HeaderValue::from_static("application/problem+json"),
        );
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::body_json;

    #[tokio::test]
    async fn problems_follow_rfc_9457() {
        let response =
            Problem::forbidden("needs plugin:kb:user:rw").with("plugin", "kb").into_response();
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers().get(http::header::CONTENT_TYPE).expect("content type"),
            "application/problem+json"
        );
        let body = body_json(response).await;
        assert_eq!(body["type"], "/problems/forbidden");
        assert_eq!(body["title"], "Access denied");
        assert_eq!(body["status"], 403);
        assert_eq!(body["detail"], "needs plugin:kb:user:rw");
        assert_eq!(body["plugin"], "kb");
    }
}
