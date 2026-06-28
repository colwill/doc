//! The QUIC + HTTP/3 endpoint plugins dial, using the backend's certificate from T05: registration,
//! liveness reports and §5's backend API, all behind the plugin's own registration token. Only a
//! plugin that is registered may call the API.

use std::net::SocketAddr;

use anyhow::{Context, Result};
use bytes::Bytes;
use doc_plugin_protocol::{Liveness, Problem, RegisterRequest, backend as paths, header};
use doc_transport::{
    EndpointConfig, H3ServerStream, bearer, endpoint, read_body, respond, respond_json, serve,
};
use http::StatusCode;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use tracing::Instrument;

use crate::api::AppState;
use crate::identity::Principal;
use crate::plugins::api::{self, Refusal};
use crate::plugins::{self, TransitionError};
use crate::secrets::TokenKind;

/// Binds the plugin endpoint and serves it until the process stops.
pub async fn start(state: AppState) -> Result<()> {
    let bind: SocketAddr = state
        .config
        .server
        .quic_addr
        .parse()
        .with_context(|| format!("parsing {}", state.config.server.quic_addr))?;
    let config = EndpointConfig::server(bind, state.config.secrets.dir.clone(), "backend");
    let (listener, report) = endpoint(&config).context("opening the plugin endpoint")?;
    tracing::info!(addr = %bind, ?report, "plugin host listening");

    tokio::spawn(serve(listener, move |request, stream, peer| {
        let state = state.clone();
        async move { dispatch(state, request, stream, peer).await }
    }));
    Ok(())
}

async fn dispatch(
    state: AppState,
    request: http::Request<()>,
    mut stream: H3ServerStream,
    peer: SocketAddr,
) -> Result<()> {
    let path = request.uri().path().to_string();
    if !path.starts_with(paths::PREFIX) {
        return respond(&mut stream, StatusCode::NOT_FOUND, Bytes::new()).await;
    }
    let Some(token) = bearer(&request).map(str::to_string) else {
        return refuse(&mut stream, 401, "unauthorized", "a bearer token is required").await;
    };
    if TokenKind::of(&token) != Some(TokenKind::PluginRegistration) {
        return refuse(&mut stream, 401, "unauthorized", "only a registration token is taken here")
            .await;
    }
    // Counted apart from HTTP callers, so a plugin with a stale token cannot shut its host out.
    let client = format!("plugin-host|{}", peer.ip());
    let principal = match crate::auth::authenticate_from(&state, &token, &client).await {
        Ok(principal) => principal,
        Err(problem) if problem.status == StatusCode::TOO_MANY_REQUESTS => {
            let detail = problem.detail_text().unwrap_or_default().to_string();
            return refuse(&mut stream, 429, "too-many-requests", &detail).await;
        }
        Err(_) => {
            return refuse(&mut stream, 401, "unauthorized", "this token is not usable").await;
        }
    };
    // Only a plugin's own registration token reaches this endpoint; nothing else has business
    // driving a plugin's lifecycle, whatever permissions it holds elsewhere.
    let Principal::Plugin { id } = &principal else {
        return refuse(&mut stream, 403, "forbidden", "this is not a plugin token").await;
    };
    let id = id.clone();
    let context = request
        .headers()
        .get(header::CONTEXT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = read_body(&mut stream).await?;

    match path.as_str() {
        paths::REGISTER => register(&state, &principal, body, peer, &mut stream).await,
        paths::LIVENESS => liveness(&state, &principal, body, &mut stream).await,
        _ if state.plugins.get(&id).await.is_none() => {
            refuse(&mut stream, 404, "not-registered", "register before calling the backend API")
                .await
        }
        _ => {
            let span = traced(&id, &path, doc_telemetry::header(request.headers()));
            let called = call(&state, &id, &path, context.as_deref(), &body);
            match called.instrument(span.clone()).await {
                Ok(answer) => respond_json(&mut stream, StatusCode::OK, &answer).await,
                Err(refusal) => {
                    tracing::debug!(plugin = %id, %path, status = refusal.status, detail = %refusal.detail, "refused");
                    span.record("http.response.status_code", refusal.status);
                    if refusal.status >= 500 {
                        span.record("otel.status_description", refusal.detail.as_str());
                    }
                    refuse(&mut stream, refusal.status, refusal.kind, &refusal.detail).await
                }
            }
        }
    }
}

/// A span for a plugin's call on the backend API, when the call continues a trace.
fn traced(plugin: &str, path: &str, parent: Option<&str>) -> tracing::Span {
    let Some(parent) = parent else { return tracing::Span::none() };
    let function = path.trim_start_matches(paths::PREFIX).trim_start_matches('/');
    let span = tracing::info_span!(
        target: "doc",
        "plugin.api",
        otel.name = %format!("{plugin} {function}"),
        otel.kind = "server",
        otel.status_description = tracing::field::Empty,
        doc.plugin.id = %plugin,
        doc.plugin.function = %function,
        http.response.status_code = tracing::field::Empty,
    );
    doc_telemetry::adopt(&span, Some(parent));
    span
}

fn parse<T: DeserializeOwned>(body: &[u8]) -> Result<T, Refusal> {
    serde_json::from_slice(body).map_err(|err| Refusal::bad(format!("malformed request: {err}")))
}

fn answer<T: Serialize>(value: T) -> Result<Value, Refusal> {
    serde_json::to_value(value).map_err(|err| Refusal::unavailable(err.to_string()))
}

/// §5's backend API. The plugin comes from the registration token, never from the request.
async fn call(
    state: &AppState,
    plugin: &str,
    path: &str,
    context: Option<&str>,
    body: &[u8],
) -> Result<Value, Refusal> {
    match path {
        paths::DATA => answer(api::data(state, plugin, parse(body)?).await?),
        paths::EVENTS => answer(api::publish(state, plugin, parse(body)?).await?),
        paths::SERVICES => answer(api::services(state, plugin, context, parse(body)?).await?),
        paths::CACHE => answer(api::cache(state, plugin, parse(body)?).await?),
        paths::TASKS => answer(api::tasks(state, plugin, context, parse(body)?).await?),
        paths::DELEGATIONS => answer(api::delegations(state, plugin, context, parse(body)?).await?),
        paths::STATE => answer(api::plugin_state(state, plugin, parse(body)?).await?),
        paths::SETTINGS => answer(api::settings(state, plugin).await?),
        paths::AUDIT => answer(api::audit(state, plugin, context, parse(body)?).await?),
        paths::STATUS => answer(api::status(state, plugin, parse(body)?).await?),
        paths::IDENTITY => answer(api::identity(state, plugin, parse(body)?).await?),
        paths::IDENTITY_LINK => answer(api::link(state, plugin, parse(body)?).await?),
        paths::USERS => answer(api::users(state, plugin, parse(body)?).await?),
        paths::TEAMS => answer(api::teams(state, plugin, parse(body)?).await?),
        paths::ORGANISATION_WRITE => {
            answer(api::write_organisation(state, plugin, context, parse(body)?).await?)
        }
        paths::TEAM_WRITE => answer(api::write_team(state, plugin, context, parse(body)?).await?),
        paths::TEAM_MEMBERS => answer(api::team_members(state, plugin, parse(body)?).await?),
        paths::TEAM_REMOVE => answer(api::team_remove(state, plugin, parse(body)?).await?),
        paths::DEPROVISION => answer(api::deprovision(state, plugin, parse(body)?).await?),
        paths::OFFBOARD => answer(api::offboard(state, plugin, parse(body)?).await?),
        paths::ACCESS_REQUESTS => answer(super::access::ask(state, plugin, parse(body)?).await?),
        paths::SCOPED_TOKENS => {
            answer(api::scoped_token(state, plugin, context, parse(body)?).await?)
        }
        paths::SCOPED_TOKEN_REVOKE => {
            answer(api::revoke_scoped_token(state, plugin, parse(body)?).await?)
        }
        paths::SEAL => answer(api::seal(state, plugin, parse(body)?).await?),
        paths::OPEN => answer(api::open(state, plugin, parse(body)?).await?),
        paths::SECRETS_CHANGED => answer(api::secrets_changed(state, plugin, parse(body)?).await?),
        paths::PEOPLE => answer(api::add_person(state, plugin, context, parse(body)?).await?),
        _ => Err(Refusal::new(404, "not-found", "no such call")),
    }
}

async fn register(
    state: &AppState,
    principal: &Principal,
    body: Bytes,
    peer: SocketAddr,
    stream: &mut H3ServerStream,
) -> Result<()> {
    let mut request: RegisterRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(err) => return refuse(stream, 400, "bad-request", &err.to_string()).await,
    };
    if request.address.trim().is_empty() {
        // A plugin that cannot name itself is still reachable at the address it dialled from,
        // which is better than refusing it outright.
        request.address = peer.to_string();
    }
    tracing::info!(
        plugin = %request.manifest.id,
        version = %request.manifest.version,
        address = %request.address,
        %peer,
        "a plugin is registering"
    );
    match plugins::register(state, principal, request).await {
        Ok(response) => respond_json(stream, StatusCode::OK, &response).await,
        Err(err) => {
            tracing::warn!(error = %err, kind = err.kind(), "a registration was refused");
            let detail = err.to_string();
            refuse(stream, err.status(), err.kind(), &detail).await
        }
    }
}

async fn liveness(
    state: &AppState,
    principal: &Principal,
    body: Bytes,
    stream: &mut H3ServerStream,
) -> Result<()> {
    let report: Liveness = match serde_json::from_slice(&body) {
        Ok(report) => report,
        Err(err) => return refuse(stream, 400, "bad-request", &err.to_string()).await,
    };
    match plugins::liveness(state, principal, report).await {
        Ok(()) => respond(stream, StatusCode::NO_CONTENT, Bytes::new()).await,
        // `404` sends the plugin to register again; `410` tells a replaced process to exit.
        Err(err @ TransitionError::Unknown(_)) => {
            refuse(stream, 404, "not-registered", &err.to_string()).await
        }
        Err(err @ TransitionError::Superseded(_)) => {
            refuse(stream, 410, "superseded", &err.to_string()).await
        }
        Err(err) => refuse(stream, err.status(), "illegal-transition", &err.to_string()).await,
    }
}

async fn refuse(stream: &mut H3ServerStream, status: u16, kind: &str, detail: &str) -> Result<()> {
    let problem = Problem::new(status, kind, kind).detail(detail);
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    respond_json(stream, code, &problem).await
}
