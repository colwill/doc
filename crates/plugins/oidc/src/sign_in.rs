//! Signing in through the provider: away to its login page, back with a code, and the ID token
//! that comes of it says who arrived. Core is then told, exactly as any identity plugin tells it.

use chrono::{DateTime, TimeDelta, Utc};
use doc_plugin_sdk::protocol::calls::{IdentityRequest, LinkRequest, LinkStart, LinkStarted};
use doc_plugin_sdk::{Backend, Request, Response};
use doc_secret::Secret;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::provider::{Claims, Discovery, Provider};
use crate::settings::Settings;

/// How long someone has between leaving for the provider and coming back.
const MINUTES: i64 = 10;

/// What this sign-in was, kept while the person is away at the provider. The nonce ties the ID
/// token that comes back to the request that went out.
#[derive(Serialize, Deserialize)]
struct Pending {
    created_at: DateTime<Utc>,
    nonce: String,
    #[serde(default)]
    return_to: Option<String>,
    /// Core's ticket, when this is someone signed in adding the account to their own.
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

    fn unavailable(detail: impl std::fmt::Display) -> Self {
        Self::new(503, "unavailable", detail.to_string())
    }
}

fn param(request: &Request, name: &str) -> Option<String> {
    let query = request.path.split_once('?').map(|(_, query)| query).unwrap_or(&request.query);
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.into_owned())
}

fn key(state: &str) -> String {
    format!("sign-in/{state}")
}

fn random() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|err| format!("no randomness: {err}"))?;
    Ok(hex::encode(bytes))
}

/// Only a path on this platform, so a sign-in cannot end up on someone else's page.
fn local_path(path: String) -> Option<String> {
    (path.starts_with('/') && !path.starts_with("//") && !path.contains('\\')).then_some(path)
}

async fn away(
    backend: &Backend,
    provider: &Provider,
    settings: &Settings,
    pending: Pending,
) -> Result<String, Response> {
    let Some((issuer, client_id, _)) = settings.provider() else {
        return Err(Response::problem(503, "unavailable", "no identity provider is configured"));
    };
    let discovery = provider
        .discovery(issuer)
        .await
        .map_err(|err| Response::problem(503, "unavailable", &err.to_string()))?;
    let state = random().map_err(|err| Response::problem(500, "internal", &err))?;
    if let Err(err) = backend.state_set(&key(&state), json!(pending)).await {
        let detail = format!("the sign-in could not start: {err}");
        return Err(Response::problem(503, "unavailable", &detail));
    }
    let mut authorize = discovery.authorization_endpoint.clone();
    authorize
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", settings.redirect.as_str())
        .append_pair("scope", &settings.scopes)
        .append_pair("state", &state)
        .append_pair("nonce", &pending.nonce);
    Ok(authorize.to_string())
}

pub async fn start(
    backend: &Backend,
    provider: &Provider,
    settings: &Settings,
    request: &Request,
) -> Response {
    let nonce = match random() {
        Ok(nonce) => nonce,
        Err(err) => return Response::problem(500, "internal", &err),
    };
    let pending = Pending {
        created_at: Utc::now(),
        nonce,
        return_to: param(request, "return_to").and_then(local_path),
        link: None,
    };
    match away(backend, provider, settings, pending).await {
        Ok(location) => Response::new(302, "text/plain; charset=utf-8", Vec::new())
            .with_header("location", &location)
            .with_header("cache-control", "no-store"),
        Err(response) => response,
    }
}

/// Core starting a link for someone already signed in; only core reaches `internal/*`.
pub async fn link(
    backend: &Backend,
    provider: &Provider,
    settings: &Settings,
    request: &Request,
) -> Response {
    let asked: LinkStart = match request.json() {
        Ok(asked) => asked,
        Err(err) => return Response::problem(400, "bad-request", &err.to_string()),
    };
    let nonce = match random() {
        Ok(nonce) => nonce,
        Err(err) => return Response::problem(500, "internal", &err),
    };
    let pending = Pending {
        created_at: Utc::now(),
        nonce,
        return_to: asked.return_to.and_then(local_path),
        link: Some(asked.ticket.expose().clone()),
    };
    match away(backend, provider, settings, pending).await {
        Ok(location) => Response::json(&LinkStarted { location }),
        Err(response) => response,
    }
}

pub async fn callback(
    backend: &Backend,
    provider: &Provider,
    settings: &Settings,
    request: &Request,
) -> Response {
    match finish(backend, provider, settings, request).await {
        Ok(session) => Response::json(&session).with_header("cache-control", "no-store"),
        Err(failure) => Response::problem(failure.status, failure.kind, &failure.detail),
    }
}

async fn taken(backend: &Backend, state: &str) -> Result<Pending, Failure> {
    let pending: Option<Pending> = backend
        .state_get(&key(state))
        .await
        .map_err(Failure::unavailable)?
        .and_then(|value| serde_json::from_value(value).ok());
    let Some(pending) = pending else {
        return Err(Failure::new(
            403,
            "wrong-state",
            "this sign-in was not started here, or was already used",
        ));
    };
    backend.state_delete(&key(state)).await.map_err(Failure::unavailable)?;
    if Utc::now() - pending.created_at > TimeDelta::minutes(MINUTES) {
        return Err(Failure::new(403, "wrong-state", "this sign-in took too long; start again"));
    }
    Ok(pending)
}

async fn finish(
    backend: &Backend,
    provider: &Provider,
    settings: &Settings,
    request: &Request,
) -> Result<Value, Failure> {
    if let Some(error) = param(request, "error") {
        let detail = param(request, "error_description").unwrap_or(error);
        return Err(Failure::new(403, "refused", format!("the provider refused: {detail}")));
    }
    let (Some(code), Some(state)) = (param(request, "code"), param(request, "state")) else {
        return Err(Failure::new(400, "bad-request", "a callback needs a code and a state"));
    };
    let Some((issuer, client_id, client_secret)) = settings.provider() else {
        return Err(Failure::new(503, "unavailable", "no identity provider is configured"));
    };
    let pending = taken(backend, &state).await?;
    let discovery: Discovery = provider.discovery(issuer).await.map_err(Failure::unavailable)?;
    let tokens = provider
        .exchange(&discovery, client_id, client_secret, &code, &settings.redirect)
        .await
        .map_err(|err| Failure::new(403, "refused", err.to_string()))?;
    let Some(id_token) = tokens["id_token"].as_str() else {
        return Err(Failure::new(
            502,
            "unavailable",
            "the provider returned no ID token, so it is not an OpenID Connect provider",
        ));
    };
    let mut claims = provider
        .claims(&discovery, id_token, client_id, &pending.nonce)
        .await
        .map_err(|err| Failure::new(403, "refused", err.to_string()))?;
    provider.fill_in(&discovery, tokens["access_token"].as_str(), &mut claims).await;
    let login = claims.login.clone().unwrap_or_else(|| claims.subject.clone());

    if let Some(ticket) = pending.link {
        let linking = LinkRequest {
            ticket: Secret::new(ticket),
            provider: backend.id().to_string(),
            external_id: claims.subject.clone(),
            login: login.clone(),
            name: claims.name.clone(),
            email: claims.email.clone(),
        };
        let linked = backend.link(linking).await.map_err(|err| match err.problem() {
            Some((status @ (403 | 404 | 409), detail)) => Failure::new(status, "refused", detail),
            _ => Failure::unavailable(err),
        })?;
        tracing::info!(%login, user = %linked.user_id, "linked an account from the provider");
        return Ok(json!({ "linked": true, "login": login, "return_to": pending.return_to }));
    }

    let session = backend.identity(identity(backend, &claims, &login)).await.map_err(|err| {
        match err.problem() {
            Some((403, detail)) => Failure::new(403, "forbidden", detail),
            _ => Failure::unavailable(err),
        }
    })?;
    tracing::info!(%login, "signed in through the identity provider");
    Ok(json!({
        "token": session.session_token.expose(),
        "expires_at": session.expires_at,
        "user_id": session.user_id,
        "login": login,
        "first": session.first,
        "return_to": pending.return_to,
    }))
}

/// What core is told, all of it from the token's claims.
fn identity(backend: &Backend, claims: &Claims, login: &str) -> IdentityRequest {
    IdentityRequest {
        provider: backend.id().to_string(),
        external_id: claims.subject.clone(),
        login: login.to_string(),
        name: claims.name.clone(),
        email: claims.email.clone(),
        first_name: claims.first_name.clone(),
        surname: claims.surname.clone(),
        ..IdentityRequest::default()
    }
}
