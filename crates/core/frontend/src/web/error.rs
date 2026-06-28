//! What a failed request returns: a status and the placeholder page, never a backend problem
//! body verbatim.

use axum::response::{IntoResponse, Response};
use http::StatusCode;

use super::pages::Page;
use crate::backend::BackendError;

#[derive(Debug, thiserror::Error)]
pub enum WebError {
    #[error("Page not found")]
    NotFound,
    #[error("The backend could not be reached")]
    Backend(#[from] BackendError),
    #[error("The page could not be rendered")]
    Render(#[from] askama::Error),
}

impl WebError {
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Backend(BackendError::Problem(problem)) => {
                StatusCode::from_u16(problem.status).unwrap_or(StatusCode::BAD_GATEWAY)
            }
            Self::Backend(_) => StatusCode::BAD_GATEWAY,
            Self::Render(_) => StatusCode::INTERNAL_SERVER_ERROR,
        }
    }
}

impl IntoResponse for WebError {
    fn into_response(self) -> Response {
        let status = self.status();
        tracing::warn!(status = status.as_u16(), error = ?self, "request failed");
        let title = status.canonical_reason().unwrap_or("Error");
        let message = match &self {
            Self::Backend(err @ BackendError::Problem(_)) => err.detail(),
            other => other.to_string(),
        };
        match Page::new(title, message).render_html() {
            Ok(html) => (status, html).into_response(),
            Err(err) => {
                tracing::error!(%err, "the error page itself could not be rendered");
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal error").into_response()
            }
        }
    }
}
