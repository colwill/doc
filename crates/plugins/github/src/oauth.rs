//! Sign-in and linking. `oauth/start` sends the browser to GitHub with a one-use state value kept
//! through `backend.state()`, and `oauth/callback` turns GitHub's answer into a DOC session. Core
//! starts a link through `internal/link` with a ticket, which the callback hands back to link the
//! account to whoever asked rather than signing anyone in.

use chrono::{DateTime, TimeDelta, Utc};
use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::protocol::calls::{IdentityRequest, LinkRequest, LinkStart, LinkStarted};
use doc_plugin_sdk::{Backend, PluginError, Request, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::github::GitHub;
use crate::settings::{OAuth, Settings};

const STATE_MINUTES: i64 = 10;

#[derive(Serialize, Deserialize)]
struct Pending {
    created_at: DateTime<Utc>,
    #[serde(default)]
    return_to: Option<String>,
    /// Core's ticket when this is linking an account rather than signing in.
    #[serde(default)]
    link: Option<String>,
}

struct Failure {
    status: u16,
    kind: &'static str,
    detail: String,
}

impl Failure {
    fn new(status: u16, kind: &'static str, detail: impl Into<String>) -> Self {
        Self { status, kind, detail: detail.into() }
    }

    fn github(detail: String) -> Self {
        Self::new(502, "github", detail)
    }

    fn unavailable(err: &PluginError) -> Self {
        Self::new(503, "unavailable", format!("the backend did not answer: {err}"))
    }
}

fn param(request: &Request, name: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

fn key(state: &str) -> String {
    format!("oauth/{state}")
}

fn fresh_state() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| format!("no randomness for a state value: {err}"))?;
    Ok(hex::encode(bytes))
}

/// Only a path on this site may be returned to, so the callback cannot be used as an open redirect.
fn local_path(path: String) -> Option<String> {
    (path.starts_with('/') && !path.starts_with("//") && !path.contains('\\')).then_some(path)
}

/// Where to send the browser to sign in to GitHub, with a new state value kept for the callback.
async fn authorize(
    backend: &Backend,
    settings: &Settings,
    oauth: &OAuth,
    pending: Pending,
) -> Result<String, Response> {
    let state = fresh_state().map_err(|err| Response::problem(500, "internal", &err))?;
    if let Err(err) = backend.state_set(&key(&state), json!(pending)).await {
        let detail = format!("the sign-in could not start: {err}");
        return Err(Response::problem(503, "unavailable", &detail));
    }
    let Ok(mut authorize) = settings.web.join("login/oauth/authorize") else {
        return Err(Response::problem(500, "internal", "the GitHub URL cannot be joined"));
    };
    authorize
        .query_pairs_mut()
        .append_pair("client_id", &oauth.client_id)
        .append_pair("redirect_uri", oauth.redirect.as_str())
        .append_pair("scope", &oauth.scopes)
        .append_pair("state", &state)
        .append_pair("allow_signup", "false");
    Ok(authorize.to_string())
}

pub async fn start(
    backend: &Backend,
    settings: &Settings,
    oauth: &OAuth,
    request: &Request,
) -> Response {
    let pending = Pending {
        created_at: Utc::now(),
        return_to: param(request, "return_to").and_then(local_path),
        link: None,
    };
    match authorize(backend, settings, oauth, pending).await {
        Ok(location) => Response::new(302, "text/plain; charset=utf-8", Vec::new())
            .with_header("location", &location)
            .with_header("cache-control", "no-store"),
        Err(response) => response,
    }
}

/// Core starting a link for someone signed in: the same sign-in, with core's ticket kept for the
/// callback. Only core reaches `internal/*`.
pub async fn link(
    backend: &Backend,
    settings: &Settings,
    oauth: &OAuth,
    request: &Request,
) -> Response {
    let asked: LinkStart = match request.json() {
        Ok(asked) => asked,
        Err(err) => return Response::problem(400, "bad-request", &err.to_string()),
    };
    let pending = Pending {
        created_at: Utc::now(),
        return_to: asked.return_to.and_then(local_path),
        link: Some(asked.ticket.expose().clone()),
    };
    match authorize(backend, settings, oauth, pending).await {
        Ok(location) => Response::json(&LinkStarted { location }),
        Err(response) => response,
    }
}

pub async fn callback(
    backend: &Backend,
    github: &GitHub,
    settings: &Settings,
    oauth: &OAuth,
    request: &Request,
) -> Response {
    match finish(backend, github, settings, oauth, request).await {
        Ok(session) => Response::json(&session).with_header("cache-control", "no-store"),
        Err(failure) => Response::problem(failure.status, failure.kind, &failure.detail),
    }
}

async fn finish(
    backend: &Backend,
    github: &GitHub,
    settings: &Settings,
    oauth: &OAuth,
    request: &Request,
) -> Result<Value, Failure> {
    if let Some(error) = param(request, "error") {
        return Err(Failure::new(
            403,
            "refused",
            format!("GitHub did not sign the user in: {error}"),
        ));
    }
    let (Some(code), Some(state)) = (param(request, "code"), param(request, "state")) else {
        return Err(Failure::new(400, "bad-request", "a callback needs a code and a state"));
    };
    let pending: Option<Pending> = backend
        .state_get(&key(&state))
        .await
        .map_err(|err| Failure::unavailable(&err))?
        .and_then(|value| serde_json::from_value(value).ok());
    let Some(pending) = pending else {
        return Err(Failure::new(
            403,
            "wrong-state",
            "this sign-in was not started here, or was already used",
        ));
    };
    backend.state_delete(&key(&state)).await.map_err(|err| Failure::unavailable(&err))?;
    if Utc::now() - pending.created_at > TimeDelta::minutes(STATE_MINUTES) {
        return Err(Failure::new(403, "wrong-state", "this sign-in took too long; start again"));
    }
    let token = github.exchange(settings, oauth, &code).await.map_err(Failure::github)?;
    let profile = github.profile(settings, &token).await.map_err(Failure::github)?;
    if !settings.allows(&profile.organisations) {
        tracing::info!(
            login = %profile.user.login,
            reported = ?profile.organisations,
            allowed = ?settings.organisations,
            "refused a user outside the allowed organisations"
        );
        return Err(Failure::new(
            403,
            "not-a-member",
            format!("{} is not in an organisation allowed to sign in", profile.user.login),
        ));
    }
    let login = profile.user.login.clone();
    if let Some(ticket) = pending.link {
        let linking = LinkRequest {
            ticket: Secret::new(ticket),
            provider: backend.id().to_string(),
            external_id: profile.user.id.to_string(),
            login: profile.user.login,
            name: profile.user.name,
            email: profile.email,
        };
        let linked = backend.link(linking).await.map_err(|err| match err.problem() {
            Some((status @ (403 | 404 | 409), detail)) => Failure::new(status, "refused", detail),
            _ => Failure::unavailable(&err),
        })?;
        tracing::info!(%login, user = %linked.user_id, "linked a GitHub account");
        return Ok(json!({ "linked": true, "login": login, "return_to": pending.return_to }));
    }
    let identity = IdentityRequest {
        provider: backend.id().to_string(),
        external_id: profile.user.id.to_string(),
        login: profile.user.login,
        name: profile.user.name,
        email: profile.email,
        organisations: profile.organisations,
        teams: profile.teams,
        ..IdentityRequest::default()
    };
    let session = backend.identity(identity).await.map_err(|err| match err.problem() {
        Some((403, detail)) => Failure::new(403, "forbidden", detail),
        _ => Failure::unavailable(&err),
    })?;
    tracing::info!(%login, "signed in with GitHub");
    Ok(json!({
        "token": session.session_token.expose(),
        "expires_at": session.expires_at,
        "user_id": session.user_id,
        "login": login,
        "first": session.first,
        "return_to": pending.return_to,
    }))
}
