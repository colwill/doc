//! Admin → Settings: what the platform calls itself. The name is kept in core, so every frontend
//! and every page shows the same one; each service's configured name is the fallback until an
//! administrator sets one here.

use askama::Template;
use axum::Form;
use axum::extract::{Extension, State};
use axum::response::{Html, IntoResponse, Response};
use serde::Deserialize;

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::pages::Chrome;
use crate::backend::{BackendError, Settings};
use crate::session;
use crate::session::Signed;

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsPage {
    pub chrome: Chrome,
    pub values: Settings,
    pub notice: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Values {
    #[serde(default)]
    pub instance_name: String,
}

async fn page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    values: Settings,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let chrome =
        Chrome::new("Settings", "/settings").signed(signed, csrf).with_plugins(state, signed).await;
    Ok(Html(SettingsPage { chrome, values, notice, error }.render()?))
}

pub async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    let values = state.backend.settings(signed.token()).await?;
    page(&state, &signed, &csrf, values, None, None).await
}

pub async fn save(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Form(values): Form<Values>,
) -> Result<Response, WebError> {
    let wanted = Settings { instance_name: values.instance_name.trim().to_string() };
    match state.backend.set_settings(signed.token(), &wanted).await {
        Ok(saved) => {
            // Every page names the platform, so what each frontend cached about it goes now
            // rather than when it expires.
            session::forget_access(&state).await;
            super::pages::set_instance(&saved.instance_name);
            let notice = Some(match saved.instance_name.is_empty() {
                true => "Saved. Pages show the rundoc logo on its own.".to_string(),
                false => format!("Saved. Pages now read {}DOC.", saved.instance_name),
            });
            Ok(page(&state, &signed, &csrf, saved, notice, None).await?.into_response())
        }
        Err(err) if actionable(&err) => {
            Ok(page(&state, &signed, &csrf, wanted, None, Some(err.detail()))
                .await?
                .into_response())
        }
        Err(err) => Err(err.into()),
    }
}

fn actionable(err: &BackendError) -> bool {
    matches!(err.status(), Some(400 | 403 | 404 | 409))
}
