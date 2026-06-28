//! Signing in and out. Identity providers' OAuth routes pass through to the backend, and whichever
//! way a session starts, its token goes into the session cookie and nowhere else.

use askama::Template;
use axum::extract::{Extension, Form, Path, Query, RawQuery, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use doc_secret::Secret;
use http::StatusCode;
use http::header::SET_COOKIE;
use serde::Deserialize;

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::pages::Chrome;
use crate::backend::{BackendError, Offered, Session, SignInOrganisation};
use crate::session::{self, SESSION_COOKIE, Signed, clear_cookie, set_cookie};

/// Only a path on this site may be returned to, so a sign-in cannot end on someone else's page.
pub fn local_path(path: Option<&str>) -> String {
    match path {
        Some(path) if path.starts_with('/') && !path.starts_with("//") && !path.contains('\\') => {
            path.to_string()
        }
        _ => "/".into(),
    }
}

pub fn provider_name(id: &str) -> String {
    match id {
        "github" => "GitHub".into(),
        "ghe" => "GitHub Enterprise".into(),
        "local" => "DOC account".into(),
        "oidc" => "Single sign-on".into(),
        other => other.to_string(),
    }
}

#[derive(Template)]
#[template(path = "sign_in.html")]
pub struct SignInPage {
    pub chrome: Chrome,
    /// Each organisation with a sign-in running, and its providers.
    pub organisations: Vec<SignInOrganisation>,
    /// Only the redirect providers (GitHub, single sign-on, …): the default view.
    pub redirect_organisations: Vec<SignInOrganisation>,
    /// Only the password providers (DOC accounts, …): behind "Other ways of signing in".
    pub password_organisations: Vec<SignInOrganisation>,
    /// Shown instead of the redirect providers: asked for, or there is nothing else to offer.
    pub show_password_step: bool,
    /// Whether there is a redirect view to go back to from the password step.
    pub can_go_back: bool,
    /// Whether the redirect view can offer a way to the password step.
    pub can_show_other: bool,
    pub return_to: String,
    /// `return_to` as a query value, for the providers' start links.
    pub return_query: String,
    pub error: Option<String>,
    /// A sign-in link somebody was emailed when they were added (FEAT-PEOPLE): the password
    /// provider, their username and the one-time password, filled in for them.
    pub link: Option<SignInLink>,
}

pub struct SignInLink {
    pub provider: String,
    pub username: String,
    pub code: String,
}

/// Each organisation's providers of one kind, dropping an organisation left with none.
fn split(organisations: &[SignInOrganisation], password: bool) -> Vec<SignInOrganisation> {
    organisations
        .iter()
        .filter_map(|organisation| {
            let providers: Vec<Offered> = organisation
                .providers
                .iter()
                .filter(|provider| provider.is_password() == password)
                .cloned()
                .collect();
            (!providers.is_empty()).then(|| SignInOrganisation {
                id: organisation.id.clone(),
                name: organisation.name.clone(),
                title: organisation.title.clone(),
                providers,
            })
        })
        .collect()
}

#[derive(Template)]
#[template(path = "choose_password.html")]
pub struct ChoosePasswordPage {
    pub chrome: Chrome,
    pub provider: String,
    pub username: String,
    pub ticket: String,
    pub return_to: String,
    pub error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SignInQuery {
    #[serde(default)]
    pub return_to: Option<String>,
    /// Show the password step rather than the redirect providers.
    #[serde(default)]
    pub other: bool,
    /// From an emailed sign-in link: the password provider, the username and the one-time password.
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
}

impl SignInQuery {
    /// The link's sign-in, when it is one: a provider named plainly and a username and code given.
    fn link(&self) -> Option<SignInLink> {
        let plain = |text: &str| {
            !text.is_empty()
                && text.len() <= 64
                && text.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
        };
        let provider = self.provider.clone().filter(|provider| plain(provider))?;
        let username = self.username.clone().filter(|username| plain(username))?;
        let code = self.code.clone().filter(|code| plain(code))?;
        Some(SignInLink { provider, username, code })
    }
}

async fn page(
    state: &AppState,
    csrf: &Csrf,
    return_to: String,
    other: bool,
    error: Option<String>,
) -> Result<SignInPage, WebError> {
    let organisations = state.backend.providers().await.unwrap_or_default().organisations;
    let redirect_organisations = split(&organisations, false);
    let password_organisations = split(&organisations, true);
    // With nothing to redirect to, there is nothing to step past.
    let show_password_step =
        redirect_organisations.is_empty() || (other && !password_organisations.is_empty());
    let can_go_back = show_password_step && !redirect_organisations.is_empty();
    let can_show_other = !show_password_step && !password_organisations.is_empty();
    let chrome = Chrome::new("Sign in", "/sign-in").with_csrf(csrf);
    let return_query = url::form_urlencoded::byte_serialize(return_to.as_bytes()).collect();
    Ok(SignInPage {
        link: None,
        chrome,
        organisations,
        redirect_organisations,
        password_organisations,
        show_password_step,
        can_go_back,
        can_show_other,
        return_to,
        return_query,
        error,
    })
}

fn render(status: StatusCode, page: &SignInPage) -> Response {
    match page.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(err) => WebError::from(err).into_response(),
    }
}

pub async fn sign_in(
    State(state): State<AppState>,
    Extension(csrf): Extension<Csrf>,
    Query(query): Query<SignInQuery>,
    headers: http::HeaderMap,
) -> Response {
    let return_to = local_path(query.return_to.as_deref());
    if let Some(token) = session::cookie(&headers, SESSION_COOKIE)
        && session::lookup(&state, &token).await.is_ok()
    {
        return Redirect::to(&return_to).into_response();
    }
    match page(&state, &csrf, return_to, query.other, None).await {
        Ok(mut page) => {
            page.link = query.link();
            render(StatusCode::OK, &page)
        }
        Err(err) => err.into_response(),
    }
}

/// Where a sign-in goes on to: after someone's first, the welcome page, which offers the accounts
/// plugins need; otherwise wherever they were going.
fn onward(session: &Session, return_to: &str) -> String {
    match session.first {
        true => {
            let back: String = url::form_urlencoded::byte_serialize(return_to.as_bytes()).collect();
            format!("/welcome?return_to={back}")
        }
        false => return_to.to_string(),
    }
}

/// A new session's cookie, and the way on to where the person was going.
fn started(session: &Session, return_to: &str) -> Response {
    let Some(token) = &session.token else {
        let err = BackendError::Decode("the sign-in did not start a session".into());
        return WebError::Backend(err).into_response();
    };
    let mut response = Redirect::to(&onward(session, return_to)).into_response();
    response.headers_mut().insert(SET_COOKIE, set_cookie(token.expose(), session.expires_at));
    response
}

/// `other` shows the password step again, for a failure at the password providers rather than a
/// redirect provider's callback.
async fn failed(
    state: &AppState,
    csrf: &Csrf,
    return_to: String,
    other: bool,
    err: &BackendError,
) -> Response {
    tracing::info!(%err, "a sign-in was refused");
    let status = err
        .status()
        .filter(|status| (400..500).contains(status))
        .and_then(|status| StatusCode::from_u16(status).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    let message = match err.status() {
        Some(401) => "The username or password is not right.".to_string(),
        _ => err.detail(),
    };
    match page(state, csrf, return_to, other, Some(message)).await {
        Ok(page) => render(status, &page),
        Err(err) => err.into_response(),
    }
}

pub async fn oauth_start(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<SignInQuery>,
) -> Result<Response, WebError> {
    let return_to = local_path(query.return_to.as_deref());
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("return_to", &return_to)
        .finish();
    let location = state.backend.oauth_start(&provider, &query).await?;
    if !(location.starts_with("https://") || location.starts_with("http://")) {
        return Err(WebError::NotFound);
    }
    Ok(Redirect::to(&location).into_response())
}

/// Ends a sign-in, or a link started from someone's account page, which leaves their session as it
/// is. A link that is refused is shown where it was started, to whoever is signed in.
pub async fn oauth_callback(
    State(state): State<AppState>,
    Extension(csrf): Extension<Csrf>,
    Path(provider): Path<String>,
    RawQuery(query): RawQuery,
    headers: http::HeaderMap,
) -> Response {
    match state.backend.oauth_callback(&provider, query.as_deref().unwrap_or_default()).await {
        Ok(session) if session.linked => {
            session::forget_access(&state).await;
            let back =
                session.return_to.as_deref().map_or("/account".into(), |to| local_path(Some(to)));
            Redirect::to(&back).into_response()
        }
        Ok(session) => started(&session, &local_path(session.return_to.as_deref())),
        Err(err) => match session::signed_in(&state, &headers).await {
            Some(signed) => super::me::refused(&state, &signed, &csrf, &err).await,
            None => failed(&state, &csrf, "/".into(), false, &err).await,
        },
    }
}

#[derive(Debug, Deserialize)]
pub struct PasswordSignIn {
    pub username: String,
    pub password: Secret<String>,
    #[serde(default)]
    pub return_to: Option<String>,
}

/// A sign-in with a username and a password, at a provider that takes them. A one-time password
/// leads on to choosing their own, before any session starts.
pub async fn password_sign_in(
    State(state): State<AppState>,
    Extension(csrf): Extension<Csrf>,
    Path(provider): Path<String>,
    Form(form): Form<PasswordSignIn>,
) -> Response {
    let return_to = local_path(form.return_to.as_deref());
    let username = form.username.trim();
    let signed = state
        .backend
        .password_sign_in(&provider, username, form.password.expose(), &return_to)
        .await;
    match signed {
        Ok(session) if session.change_password => {
            let Some(ticket) = session.ticket else {
                let err = BackendError::Decode("a one-time password came with no ticket".into());
                return failed(&state, &csrf, return_to, true, &err).await;
            };
            let page = ChoosePasswordPage {
                chrome: Chrome::new("Choose your password", "/sign-in").with_csrf(&csrf),
                provider,
                username: session.username.unwrap_or_else(|| username.to_string()),
                ticket: ticket.expose().clone(),
                return_to,
                error: None,
            };
            choosing(StatusCode::OK, &page)
        }
        Ok(session) => started(&session, &return_to),
        Err(err) => failed(&state, &csrf, return_to, true, &err).await,
    }
}

fn choosing(status: StatusCode, page: &ChoosePasswordPage) -> Response {
    match page.render() {
        Ok(html) => (status, Html(html)).into_response(),
        Err(err) => WebError::from(err).into_response(),
    }
}

#[derive(Debug, Deserialize)]
pub struct Chosen {
    pub ticket: Secret<String>,
    pub username: String,
    pub password: Secret<String>,
    pub again: Secret<String>,
    #[serde(default)]
    pub return_to: Option<String>,
}

/// The password someone chose after a one-time one, which signs them in.
pub async fn choose_password(
    State(state): State<AppState>,
    Extension(csrf): Extension<Csrf>,
    Path(provider): Path<String>,
    Form(form): Form<Chosen>,
) -> Response {
    let return_to = local_path(form.return_to.as_deref());
    let again = |error: String| ChoosePasswordPage {
        chrome: Chrome::new("Choose your password", "/sign-in").with_csrf(&csrf),
        provider: provider.clone(),
        username: form.username.clone(),
        ticket: form.ticket.expose().clone(),
        return_to: return_to.clone(),
        error: Some(error),
    };
    if form.password.expose() != form.again.expose() {
        let page = again("The two passwords are not the same.".into());
        return choosing(StatusCode::BAD_REQUEST, &page);
    }
    let chosen = state
        .backend
        .choose_password(&provider, form.ticket.expose(), form.password.expose(), &return_to)
        .await;
    match chosen {
        Ok(session) => started(&session, &return_to),
        // A password that is too short can be tried again with the same ticket; anything else
        // starts again from signing in.
        Err(err) if err.status() == Some(400) => {
            choosing(StatusCode::BAD_REQUEST, &again(err.detail()))
        }
        Err(err) => failed(&state, &csrf, return_to, true, &err).await,
    }
}

/// The session is revoked, not just forgotten, so a copied cookie stops working too.
pub async fn sign_out(State(state): State<AppState>, signed: Signed) -> Response {
    if let Err(err) = state.backend.logout(signed.token()).await {
        tracing::warn!(%err, "the backend did not revoke a session being signed out");
    }
    session::forget(&state, signed.token()).await;
    let mut response = Redirect::to("/sign-in").into_response();
    response.headers_mut().insert(SET_COOKIE, clear_cookie());
    response
}
