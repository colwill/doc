//! Signed-in users. The session token lives in an `HttpOnly` cookie, and who it belongs to is cached
//! in the Cache Bus under a hash of the token, so a page need not ask the backend every time.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::extract::FromRequestParts;
use axum::response::{Html, IntoResponse, Redirect, Response};
use chrono::{DateTime, Utc};
use doc_cachebus::Namespace;
use doc_secret::Secret;
use http::header::{COOKIE, SET_COOKIE};
use http::request::Parts;
use http::{HeaderMap, HeaderValue, StatusCode};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::backend::{Access, BackendError};
use crate::config::FabricMode;
use crate::web::AppState;
use crate::web::cookie::Cookie;
use crate::web::error::WebError;

pub const SESSION_COOKIE: &str = "doc_session";
const NAMESPACE: &str = "core.sessions";
const ACCESS: &str = "core.access";
/// How long a revoked or disabled session can still open pages that have it cached.
const CACHE_TTL: Duration = Duration::from_secs(60);
/// How long the navigation is kept where nothing can say it changed. With the in-memory fabric
/// (`just dev`) the frontend's Event Bus is its own, so the backend's `platform.plugin.*.state`
/// never reaches `watch` and only expiry brings a plugin that started since into the bar.
const UNWATCHED_ACCESS_TTL: Duration = Duration::from_secs(3);

/// Counts each time the cached navigation is dropped, so an answer asked for before a drop is not
/// written back after it: a plugin that starts while a page is asking would otherwise be missing
/// from the bar until the cache expired, although the event saying it started had arrived.
static FORGOTTEN: AtomicU64 = AtomicU64::new(0);

pub fn cookie(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get_all(COOKIE)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(';'))
        .filter_map(|pair| pair.trim().split_once('='))
        .find(|(key, _)| *key == name)
        .map(|(_, value)| value.to_string())
        .filter(|value| !value.is_empty())
}

pub fn set_cookie(token: &str, expires_at: Option<DateTime<Utc>>) -> HeaderValue {
    let max_age = expires_at.map_or(12 * 3600, |at| (at - Utc::now()).num_seconds());
    Cookie::new(SESSION_COOKIE, token).lasting(max_age).header()
}

pub fn clear_cookie() -> HeaderValue {
    Cookie::cleared(SESSION_COOKIE).header()
}

fn key(token: &str) -> String {
    format!("frontend/{}", hex::encode(Sha256::digest(token.as_bytes())))
}

pub async fn lookup(state: &AppState, token: &str) -> Result<Value, BackendError> {
    let namespace = Namespace::new(NAMESPACE).ok();
    if let Some(namespace) = &namespace
        && let Ok(Some(entry)) = state.buses.cache.get(namespace, &key(token)).await
    {
        return Ok(entry.value);
    }
    let me = state.backend.me(token).await?;
    if let Some(namespace) = &namespace {
        let _ = state.buses.cache.set(namespace, &key(token), me.clone(), Some(CACHE_TTL)).await;
    }
    Ok(me)
}

/// What the navigation is built from, cached per user; the backend refusing leaves it empty.
pub async fn access(state: &AppState, signed: &Signed) -> Access {
    let namespace = Namespace::new(ACCESS).ok();
    let user = signed.me.get("id").and_then(Value::as_str).unwrap_or_default();
    let key = format!("frontend/{user}");
    if let Some(namespace) = &namespace
        && let Ok(Some(entry)) = state.buses.cache.get(namespace, &key).await
        && let Ok(access) = serde_json::from_value(entry.value)
    {
        return access;
    }
    let asked = FORGOTTEN.load(Ordering::Acquire);
    let access = match state.backend.access(signed.token()).await {
        Ok(access) => access,
        Err(err) => {
            tracing::warn!(%err, "the navigation is shown without plugins");
            return Access::default();
        }
    };
    let ttl = match state.config.fabric.mode {
        FabricMode::Memory => UNWATCHED_ACCESS_TTL,
        FabricMode::Cluster => CACHE_TTL,
    };
    if FORGOTTEN.load(Ordering::Acquire) == asked
        && let (Some(namespace), Ok(value)) = (&namespace, serde_json::to_value(&access))
    {
        let _ = state.buses.cache.set(namespace, &key, value, Some(ttl)).await;
    }
    access
}

/// Every user's cached navigation, dropped when access, the layout or a plugin's state changes.
pub async fn forget_access(state: &AppState) {
    FORGOTTEN.fetch_add(1, Ordering::AcqRel);
    if let Ok(namespace) = Namespace::new(ACCESS) {
        let _ = state.buses.cache.clear(&namespace).await;
    }
}

/// Drops the cached navigation whenever the RBAC plugin says an assignment moved, an administrator
/// arranges the navigation again, a plugin starts or stops serving its pages, or someone's linked
/// accounts change, which the navigation carries.
pub fn watch(state: &AppState) {
    forget_on(state, "plugin.rbac.changed", "frontend.access");
    forget_on(state, "platform.navigation.changed", "frontend.navigation");
    forget_on(state, "platform.settings.changed", "frontend.settings");
    forget_on(state, "platform.plugin.*.state", "frontend.plugin-states");
    forget_on(state, "platform.iam.user.identity.*", "frontend.identities");
    forget_on(state, "platform.iam.user.merged", "frontend.merges");
}

fn forget_on(state: &AppState, topic: &'static str, consumers: &'static str) {
    let state = state.clone();
    tokio::spawn(async move {
        let Ok(filter) = doc_eventbus::TopicFilter::new(topic) else { return };
        let group = doc_eventbus::ConsumerGroup::new(consumers, filter);
        let mut subscription = match state.buses.events.subscribe(group).await {
            Ok(subscription) => subscription,
            Err(err) => {
                tracing::warn!(%err, topic, "the navigation will only notice this as its cache expires");
                return;
            }
        };
        while let Some(delivery) = subscription.next().await {
            forget_access(&state).await;
            let _ = subscription.ack(delivery.id).await;
        }
    });
}

/// Whoever the request's session cookie belongs to, for a page that works either way.
pub async fn signed_in(state: &AppState, headers: &HeaderMap) -> Option<Signed> {
    let token = cookie(headers, SESSION_COOKIE)?;
    let me = lookup(state, &token).await.ok()?;
    Some(Signed { token: Secret::new(token), me, boosted: false })
}

pub async fn forget(state: &AppState, token: &str) {
    if let Ok(namespace) = Namespace::new(NAMESPACE) {
        let _ = state.buses.cache.delete(&namespace, &key(token)).await;
    }
}

/// A signed-in caller; a page that asks for one sends anyone else to sign in and back afterwards.
pub struct Signed {
    token: Secret<String>,
    pub me: Value,
    /// Whether HTMX asked for this page as a boosted navigation, in which case the answer is the
    /// page's own part alone: the chrome around it is already in the browser.
    pub boosted: bool,
}

impl Signed {
    /// The session token, for the backend calls made as this user.
    pub fn token(&self) -> &str {
        self.token.expose()
    }

    pub fn label(&self) -> String {
        ["login", "name"]
            .iter()
            .find_map(|field| self.me.get(field).and_then(Value::as_str))
            .unwrap_or("signed in")
            .to_string()
    }
}

pub fn to_sign_in(parts: &Parts) -> Response {
    let here = parts.uri.path_and_query().map_or("/", |path| path.as_str());
    let target = format!(
        "/sign-in?return_to={}",
        url::form_urlencoded::byte_serialize(here.as_bytes()).collect::<String>()
    );
    let mut response = match parts.headers.contains_key("hx-request") {
        true => (
            StatusCode::UNAUTHORIZED,
            Html(format!(
                "<p class=\"doc-error-message\">Your session has ended. <a href=\"{target}\">Sign in again</a>.</p>"
            )),
        )
            .into_response(),
        false => Redirect::to(&target).into_response(),
    };
    response.headers_mut().insert(SET_COOKIE, clear_cookie());
    response
}

impl FromRequestParts<AppState> for Signed {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let Some(token) = cookie(&parts.headers, SESSION_COOKIE) else {
            return Err(to_sign_in(parts));
        };
        let boosted = parts
            .headers
            .get("hx-boosted")
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| value.eq_ignore_ascii_case("true"));
        match lookup(state, &token).await {
            Ok(me) => Ok(Self { token: Secret::new(token), me, boosted }),
            Err(err) if err.status() == Some(401) => Err(to_sign_in(parts)),
            Err(err) => Err(WebError::Backend(err).into_response()),
        }
    }
}
