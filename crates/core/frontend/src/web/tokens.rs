//! Personal access tokens: create one (its secret is shown once), list them and revoke them.

use askama::Template;
use axum::extract::{Extension, Form, Path, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use http::HeaderMap;
use serde::Deserialize;

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::pages::Chrome;
use crate::backend::{ScopedToken, TokenView};
use crate::session::Signed;

const MAX_DAYS: i64 = 365;

#[derive(Template)]
#[template(path = "tokens.html")]
pub struct TokensPage {
    pub chrome: Chrome,
    pub tokens: Vec<TokenView>,
    /// Scoped tokens in force, which are listed only to be seen and revoked.
    pub scoped: Vec<ScopedToken>,
    pub created: Option<String>,
}

#[derive(Template)]
#[template(path = "token_new.html")]
pub struct NewTokenPage {
    pub chrome: Chrome,
    pub values: NewToken,
    pub error: Option<String>,
}

impl TokensPage {
    pub fn when(at: &chrono::DateTime<chrono::Utc>) -> String {
        at.format("%-d %b %Y %H:%M UTC").to_string()
    }

    pub fn active(token: &TokenView) -> bool {
        token.revoked_at.is_none() && token.expires_at.is_none_or(|at| at > chrono::Utc::now())
    }
}

async fn render(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    created: Option<String>,
) -> Result<Html<String>, WebError> {
    let tokens = state.backend.tokens(signed.token()).await?;
    let scoped = state.backend.scoped_tokens(signed.token()).await.unwrap_or_default();
    let chrome = Chrome::new("Access tokens", "/tokens")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    Ok(Html(TokensPage { chrome, tokens, scoped, created }.render()?))
}

async fn new_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    values: NewToken,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let chrome = Chrome::new("Create a token", "/tokens")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    Ok(Html(NewTokenPage { chrome, values, error }.render()?))
}

pub async fn list(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    render(&state, &signed, &csrf, None).await
}

pub async fn new(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    new_page(&state, &signed, &csrf, NewToken::default(), None).await
}

#[derive(Debug, Default, Deserialize)]
pub struct NewToken {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub days: String,
}

pub async fn create(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Form(form): Form<NewToken>,
) -> Result<Html<String>, WebError> {
    let days = match form.days.trim() {
        "" => Ok(None),
        text => text
            .parse::<i64>()
            .ok()
            .filter(|days| (1..=MAX_DAYS).contains(days))
            .map(Some)
            .ok_or(()),
    };
    let name = form.name.trim();
    let problem = match (&days, name.is_empty()) {
        (Err(()), _) => {
            Some(format!("Expiry is a number of days from 1 to {MAX_DAYS}, or empty for never."))
        }
        (_, true) => Some("Give the token a name, so you can tell it apart later.".to_string()),
        _ => None,
    };
    if problem.is_some() {
        return new_page(&state, &signed, &csrf, form, problem).await;
    }
    let created = state.backend.create_token(signed.token(), name, days.unwrap_or(None)).await?;
    render(&state, &signed, &csrf, Some(created.token.expose().clone())).await
}

/// HTMX swaps the row for its revoked state; a plain form post comes back to the list.
pub async fn revoke(
    State(state): State<AppState>,
    signed: Signed,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    state.backend.revoke_token(signed.token(), &id).await?;
    if headers.contains_key("hx-request") {
        return Ok(
            Html("<strong class=\"doc-badge doc-badge--down\">Revoked</strong>").into_response()
        );
    }
    Ok(Redirect::to("/tokens").into_response())
}
