//! The audit log viewer, for anyone who can read the RBAC plugin or the platform, newest first.

use askama::Template;
use axum::extract::{Extension, Query, State};
use axum::response::Html;
use serde::Deserialize;

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::pages::Chrome;
use crate::backend::AuditRow;
use crate::session::Signed;

const PAGE: usize = 50;

#[derive(Debug, Default, Deserialize)]
pub struct Filter {
    #[serde(default)]
    pub action: String,
    #[serde(default)]
    pub actor: String,
    #[serde(default)]
    pub before: String,
}

#[derive(Template)]
#[template(path = "audit.html")]
pub struct AuditPage {
    pub chrome: Chrome,
    pub filter: Filter,
    pub entries: Vec<AuditRow>,
    /// The query for the next page back, when this one was full.
    pub older: Option<String>,
}

impl AuditPage {
    fn when(at: &chrono::DateTime<chrono::Utc>) -> String {
        at.format("%-d %b %Y %H:%M:%S UTC").to_string()
    }

    fn detail(detail: &serde_json::Value) -> String {
        match detail {
            serde_json::Value::Object(map) if map.is_empty() => String::new(),
            other => other.to_string(),
        }
    }
}

pub async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Query(filter): Query<Filter>,
) -> Result<Html<String>, WebError> {
    let query = {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("limit", &PAGE.to_string());
        let wanted =
            [("action", &filter.action), ("actor", &filter.actor), ("before", &filter.before)];
        for (key, value) in wanted {
            if !value.trim().is_empty() {
                query.append_pair(key, value.trim());
            }
        }
        query.finish()
    };
    let entries = state.backend.audit(signed.token(), &query).await?.entries;
    let older = (entries.len() == PAGE).then(|| {
        let last = entries.last().map(|entry| entry.at.to_rfc3339()).unwrap_or_default();
        url::form_urlencoded::Serializer::new(String::new())
            .append_pair("action", &filter.action)
            .append_pair("actor", &filter.actor)
            .append_pair("before", &last)
            .finish()
    });
    let chrome = Chrome::new("Audit log", "/audit")
        .signed(&signed, &csrf)
        .with_plugins(&state, &signed)
        .await;
    Ok(Html(AuditPage { chrome, filter, entries, older }.render()?))
}
