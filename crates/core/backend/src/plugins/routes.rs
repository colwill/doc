//! A caller's request on its way to a plugin's routes (§5, T22). Only headers a plugin has a use
//! for go in and only safe ones come out, public routes exist only where the manifest declares them,
//! and `internal/*` is reached over the Service Bus alone, and from core alone.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;
use doc_permissions::Access;
use doc_plugin_protocol::{Caller, Capability, Guard, Manifest};
use doc_servicebus::{Address, ServiceHandler};
use http::{HeaderMap, Method};
use serde_json::{Value, json};

use super::Registered;
use super::client::{Answer, CallError, Forwarded};
use crate::api::AppState;
use crate::identity::Principal;

#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    #[error("{0} did not answer in time")]
    Deadline(String),
    #[error("{0} could not be reached: {1}")]
    Unreachable(String, String),
    #[error("{0}")]
    Unavailable(String),
}

/// Credentials and cookies are never among these: a plugin acts through its context token.
fn forwardable(name: &str) -> bool {
    matches!(
        name,
        "accept"
            | "accept-language"
            | "content-type"
            | "if-match"
            | "if-modified-since"
            | "if-none-match"
            | "traceparent"
            | "user-agent"
            | "x-doc-signature"
            | "x-github-delivery"
            | "x-github-event"
            | "x-hub-signature-256"
            | "x-request-id"
    ) || name.starts_with("hx-")
        || name.starts_with("mcp-")
}

/// Only what a browser shows or caches by, and whose faux data a page shows; a plugin cannot set
/// cookies on the platform's origin.
fn returnable(name: &str) -> bool {
    matches!(
        name,
        "cache-control"
            | "content-disposition"
            | "content-language"
            | "content-type"
            | "etag"
            | "last-modified"
            | "location"
            | "vary"
            | "allow"
            | "x-doc-faux"
    ) || name.starts_with("hx-")
        || name.starts_with("mcp-")
}

pub fn request_headers(headers: &HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter(|(name, _)| forwardable(name.as_str()))
        .filter_map(|(name, value)| Some((name.as_str().to_string(), value.to_str().ok()?.into())))
        .collect()
}

/// Whether `{surface}/{path}` is one of `routes`, exactly or under a `/*` route.
fn declared(routes: &[String], surface: &str, path: &str) -> bool {
    routes.iter().any(|route| {
        let route = route.trim_start_matches('/');
        let route = route.strip_prefix(surface).and_then(|r| r.strip_prefix('/')).unwrap_or(route);
        match route.strip_suffix("/*") {
            Some(below) => path == below || path.starts_with(&format!("{below}/")),
            None => path == route,
        }
    })
}

/// Whether `public/{path}` is declared, exactly or under a `/*` route, by a plugin allowed any.
pub fn is_public(manifest: &Manifest, path: &str) -> bool {
    manifest.capabilities.contains(&Capability::PublicRoutes)
        && declared(&manifest.public_routes, "public", path)
}

/// The access `route` needs: reading for `GET`, `HEAD` and a `POST` to a declared read route,
/// which names a route under `api/`, or under `ui/` when it starts so.
pub fn access_of(manifest: Option<&Manifest>, method: &str, route: &str) -> Access {
    let reads = |manifest: &Manifest| {
        let (ui, api): (Vec<String>, Vec<String>) =
            manifest.read_routes.iter().cloned().partition(|declared| declared.starts_with("ui/"));
        match (route.strip_prefix("api/"), route.strip_prefix("ui/")) {
            (Some(path), _) => declared(&api, "api", path),
            (_, Some(path)) => declared(&ui, "ui", path),
            _ => false,
        }
    };
    match manifest {
        Some(manifest) if method == "POST" && reads(manifest) => Access::Read,
        _ => Access::of(method),
    }
}

/// Why a plugin cannot take a call relayed to it, in words its caller can pass on to a person.
fn not_serving(plugin: &str, entry: Option<&Registered>, off: bool) -> String {
    if off {
        return format!("{plugin} is turned off");
    }
    match entry {
        None => format!("{plugin} is not running"),
        Some(entry) => match &entry.error {
            Some(error) => {
                format!("{plugin} is not running: it is in {} ({error})", entry.state.as_str())
            }
            None => format!("{plugin} is not running: it is {}", entry.state.as_str()),
        },
    }
}

/// No `.` or `..` segments, so a route cannot be walked from `api/` into `internal/`.
pub fn clean(path: &str) -> bool {
    path.split('/').all(|segment| segment != "." && segment != "..")
}

pub async fn forward(
    state: &AppState,
    entry: &Registered,
    request: &Forwarded,
    caller: &Caller,
    principal: Principal,
) -> Result<Answer, ForwardError> {
    let deadline = state.config.plugins.request_deadline();
    let context = state
        .plugins
        .contexts
        .issue(&entry.id, principal, deadline)
        .map_err(ForwardError::Unavailable)?;
    if caller.via.is_some() || caller.guard.is_some() {
        state.plugins.contexts.relayed(context.token(), caller.via.as_deref(), caller.guard);
    }
    let call = entry.client.request(request, caller, context.token(), deadline);
    let answered = tokio::time::timeout(deadline, call).await;
    drop(context);
    match answered {
        Ok(Ok(mut answer)) => {
            if answer.status.is_server_error() {
                tracing::warn!(
                    plugin = %entry.id,
                    path = %request.path,
                    status = answer.status.as_u16(),
                    detail = %String::from_utf8_lossy(&answer.body),
                    "a plugin route failed"
                );
            }
            answer.headers.retain(|(name, _)| returnable(&name.to_ascii_lowercase()));
            Ok(answer)
        }
        Ok(Err(CallError::Deadline(_))) | Err(_) => Err(ForwardError::Deadline(entry.id.clone())),
        Ok(Err(CallError::NotReady(_))) => {
            Err(ForwardError::Unavailable(format!("{} is not ready", entry.id)))
        }
        Ok(Err(err)) => {
            tracing::warn!(plugin = %entry.id, path = %request.path, %err, "a forwarded call failed");
            Err(ForwardError::Unreachable(entry.id.clone(), err.to_string()))
        }
    }
}

/// Answers `plugin.<id>` on the Service Bus, once per plugin, looking it up on every request.
pub async fn serve_internal(state: &AppState, id: &str) {
    if !state.plugins.serving.lock().insert(id.to_string()) {
        return;
    }
    let Ok(address) = Address::plugin(id) else { return };
    let handler = Arc::new(Internal { state: state.clone(), plugin: id.to_string() });
    if let Err(err) = state.buses.services.serve(address, handler).await {
        tracing::warn!(plugin = %id, %err, "the plugin's internal routes are not being served");
        state.plugins.serving.lock().remove(id);
    }
}

struct Internal {
    state: AppState,
    plugin: String,
}

impl Internal {
    /// Another plugin asking `api/*` for whoever it acts for, who is checked as over HTTP.
    async fn relay(
        &self,
        reference: &str,
        request: doc_servicebus::Request,
    ) -> Result<Value, String> {
        let plugin = &self.plugin;
        let Some(route) = request.subject.strip_prefix("api/").filter(|route| clean(route)) else {
            return Err(format!("{plugin}'s internal routes are for core alone"));
        };
        let payload = &request.payload;
        let method = Method::from_bytes(payload["method"].as_str().unwrap_or("GET").as_bytes())
            .map_err(|_| "not an HTTP method".to_string())?;
        let principal = crate::permissions::principal_of(&self.state, reference).await?;
        let manifest = self.state.plugins.get(plugin).await.map(|entry| entry.manifest);
        let access = access_of(manifest.as_ref(), method.as_str(), &request.subject);
        // What the asking plugin limited itself to, which core wrote into the payload itself.
        let guard: Option<Guard> = serde_json::from_value(payload["guard"].clone()).unwrap_or(None);
        if guard == Some(Guard::ReadOnly) && access == Access::Write {
            let detail = format!(
                "{plugin} was not asked to change anything: this call may only read, as a run \
                 against production does"
            );
            return Ok(json!({ "status": 403, "body": { "detail": detail } }));
        }
        let authorised =
            match crate::permissions::authorise(&self.state, principal, plugin, access).await {
                Ok(authorised) => authorised,
                Err(problem) => {
                    let detail = problem.detail_text().unwrap_or(&problem.title).to_string();
                    return Ok(
                        json!({ "status": problem.status.as_u16(), "body": { "detail": detail } }),
                    );
                }
            };
        let entry = match self.state.plugins.get(plugin).await {
            Some(entry) if self.state.plugins.offers(&entry) => entry,
            other => {
                return Err(not_serving(plugin, other.as_ref(), self.state.plugins.is_off(plugin)));
            }
        };
        let patience = self.state.config.plugins.handover_timeout();
        let _pass =
            self.state.plugins.gate(plugin).pass(patience).await.ok_or_else(|| {
                format!("{plugin} is being reloaded or handed over to a new version")
            })?;
        let body = match &payload["body"] {
            Value::Null => Bytes::new(),
            body => Bytes::from(body.to_string()),
        };
        let forwarded = Forwarded {
            method,
            path: format!("api/{route}"),
            query: payload["query"].as_str().map(str::to_string),
            headers: vec![("content-type".into(), "application/json".into())],
            body,
        };
        let mut caller = super::runs::caller_of(&authorised);
        caller.via = payload["via"].as_str().map(str::to_string);
        if guard.is_some() {
            caller.guard = guard;
            caller.production = self.state.config.environments.production.clone();
        }
        let answer = forward(&self.state, &entry, &forwarded, &caller, authorised.principal)
            .await
            .map_err(|err| err.to_string())?;
        let body = serde_json::from_slice(&answer.body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&answer.body).into_owned()));
        Ok(json!({ "status": answer.status.as_u16(), "body": body }))
    }
}

impl Internal {
    /// A plugin asking as itself: core checks nothing, and the route decides whom it answers.
    async fn discovery(
        &self,
        reference: &str,
        request: doc_servicebus::Request,
    ) -> Result<Value, String> {
        let plugin = &self.plugin;
        let Some(route) = request.subject.strip_prefix("discovery/").filter(|route| clean(route))
        else {
            return Err(format!("`{}` is not a route", request.subject));
        };
        let Some(asking) = reference.strip_prefix("plugin:") else {
            return Err(format!("{plugin}'s discovery routes answer other plugins alone"));
        };
        let payload = &request.payload;
        let method = Method::from_bytes(payload["method"].as_str().unwrap_or("GET").as_bytes())
            .map_err(|_| "not an HTTP method".to_string())?;
        let entry = match self.state.plugins.get(plugin).await {
            Some(entry) if self.state.plugins.offers(&entry) => entry,
            other => {
                return Err(not_serving(plugin, other.as_ref(), self.state.plugins.is_off(plugin)));
            }
        };
        let patience = self.state.config.plugins.handover_timeout();
        let _pass =
            self.state.plugins.gate(plugin).pass(patience).await.ok_or_else(|| {
                format!("{plugin} is being reloaded or handed over to a new version")
            })?;
        let body = match &payload["body"] {
            Value::Null => Bytes::new(),
            body => Bytes::from(body.to_string()),
        };
        let forwarded = Forwarded {
            method,
            path: format!("discovery/{route}"),
            query: payload["query"].as_str().map(str::to_string),
            headers: vec![("content-type".into(), "application/json".into())],
            body,
        };
        let caller = Caller {
            kind: "plugin".into(),
            id: Some(asking.to_string()),
            label: Some(asking.to_string()),
            ..Caller::default()
        };
        let principal = Principal::Plugin { id: asking.to_string() };
        let answer = forward(&self.state, &entry, &forwarded, &caller, principal)
            .await
            .map_err(|err| err.to_string())?;
        let body = serde_json::from_slice(&answer.body)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&answer.body).into_owned()));
        Ok(json!({ "status": answer.status.as_u16(), "body": body }))
    }
}

#[async_trait]
impl ServiceHandler for Internal {
    async fn handle(&self, request: doc_servicebus::Request) -> Result<Value, String> {
        let plugin = &self.plugin;
        // A relayed request always carries a principal, so only the platform's own arrive without.
        if let Some(reference) = request.principal.clone() {
            return match request.subject.starts_with("discovery/") {
                true => self.discovery(&reference, request).await,
                false => self.relay(&reference, request).await,
            };
        }
        ask_internal(&self.state, plugin, &request.subject, &request.payload).await
    }
}

/// One of a plugin's `internal/*` routes, asked by the platform itself over the connection this
/// backend holds, so what it carries goes nowhere else.
pub async fn ask_internal(
    state: &AppState,
    plugin: &str,
    route: &str,
    payload: &Value,
) -> Result<Value, String> {
    let route = route.trim_start_matches('/');
    if !clean(route) {
        return Err(format!("`{route}` is not a route"));
    }
    if state.plugins.get(plugin).await.is_none() {
        return Err(format!("{plugin} is not running"));
    }
    let patience = state.config.plugins.handover_timeout();
    let _pass = state
        .plugins
        .gate(plugin)
        .pass(patience)
        .await
        .ok_or_else(|| format!("{plugin} is being reloaded or handed over to a new version"))?;
    let entry = match state.plugins.get(plugin).await {
        Some(entry) if state.plugins.offers(&entry) => entry,
        other => return Err(not_serving(plugin, other.as_ref(), state.plugins.is_off(plugin))),
    };
    let forwarded = Forwarded {
        method: Method::POST,
        path: format!("internal/{route}"),
        query: None,
        headers: vec![("content-type".into(), "application/json".into())],
        body: Bytes::from(payload.to_string()),
    };
    let caller = Caller { kind: "platform".into(), ..Caller::default() };
    let principal = Principal::Plugin { id: plugin.to_string() };
    let answer = forward(state, &entry, &forwarded, &caller, principal)
        .await
        .map_err(|err| err.to_string())?;
    if !answer.status.is_success() {
        let refused = CallError::Refused(
            plugin.to_string(),
            answer.status.as_u16(),
            String::from_utf8_lossy(&answer.body).into_owned(),
        );
        return Err(format!("{plugin} answered {}: {}", answer.status.as_u16(), refused.detail()));
    }
    if answer.body.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_slice(&answer.body)
        .map_err(|err| format!("{plugin} answered with something that is not JSON: {err}"))
}
