//! Each plugin's own surface: `run`, `cancel`, and the `api`, `ui` and `public` routes it serves,
//! which are forwarded to it after core's checks (T22). Managing plugins is `plugin_admin`'s.

use axum::Json;
use axum::body::Body;
use axum::extract::{FromRequestParts, Path, Request, State};
use axum::response::{IntoResponse, Response};
use doc_permissions::{Access, CORE};
use doc_plugin_protocol::{Caller, PluginState};
use http::request::Parts;
use http::{HeaderName, HeaderValue, StatusCode};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::problem::Problem;
use crate::auth::Auth;
use crate::identity::{AuditEntry, Principal};
use crate::permissions::{self, Authorised};
use crate::plugins::client::{Answer, Forwarded};
use crate::plugins::handover::Pass;
use crate::plugins::routes::{self, ForwardError};
use crate::plugins::runs::{self, RunError, Started};
use crate::plugins::{self, Registered, TransitionError};

/// The most a forwarded request may carry; bulk data belongs in direct links (ADR-0001, rule 7).
const MAX_BODY: usize = 16 * 1024 * 1024;

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RunRequest {
    pub payload: Value,
    /// Async plugins only: how many times the background run is tried.
    pub max_attempts: Option<i32>,
}

/// Needs write access; synchronous plugins answer, async ones queue a task (202), others refuse.
pub async fn run(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, Problem> {
    let (mut parts, body) = request.into_parts();
    let authorised = admit(&state, &mut parts, &id, Access::Write).await?;
    let (entry, _pass) = taking_runs(&state, &id).await?;
    let body = axum::body::to_bytes(body, MAX_BODY).await.map_err(|_| too_large())?;
    let asked: RunRequest = if body.is_empty() {
        RunRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|err| Problem::bad_request(err.to_string()))?
    };
    let attempts = asked.max_attempts.unwrap_or(3).clamp(1, 10);
    let by = authorised.principal.clone();
    match runs::on_demand(&state, &entry, &authorised, asked.payload, attempts).await {
        Ok(Started::Finished(output)) => {
            plugins::audit(&state, &by, "plugin.run", &id, json!({ "task": null })).await;
            Ok(Json(json!({ "plugin": id, "output": output.payload })).into_response())
        }
        Ok(Started::Queued(task)) => {
            plugins::audit(&state, &by, "plugin.run", &id, json!({ "task": task.id })).await;
            Ok((StatusCode::ACCEPTED, Json(*task)).into_response())
        }
        Err(err) => Err(run_problem(&id, &err)),
    }
}

/// Needs write access to the plugin (T22) or to core (T25); resuming is the operator's alone.
pub async fn cancel(
    State(state): State<AppState>,
    Path(id): Path<String>,
    request: Request,
) -> Result<Response, Problem> {
    let (mut parts, _) = request.into_parts();
    let principal = Auth::from_request_parts(&mut parts, &state).await?.0;
    if !known(&state, &id).await? {
        return Err(Problem::not_found("plugin"));
    }
    let (authorised, allowed) = permissions::assess(&state, principal, &id, Access::Write).await;
    if !allowed && !permissions::holds(&state, &authorised.principal, CORE, Access::Write).await {
        let detail = format!("needs write access to {id}, or to core");
        return Err(Problem::forbidden(detail).with("plugin", id.as_str()));
    }
    if state.plugins.get(&id).await.is_none() {
        return Err(not_serving(&id, None));
    }
    if state.plugins.handing_over(&id) {
        return Err(refused(&id, &TransitionError::Busy(id.clone())));
    }
    plugins::cancel(&state, &id).await.map_err(|err| refused(&id, &err))?;
    let entry = AuditEntry::new("plugin.cancelled").by(&authorised.principal).subject(id.clone());
    if let Err(err) = state.repos.identity.record_audit(entry).await {
        tracing::warn!(%err, plugin = %id, "a cancellation was not written to the audit log");
    }
    Ok(Json(json!({ "plugin": id, "state": PluginState::Cancelled })).into_response())
}

/// `api` and `ui` after core's check, declared `public` routes for anyone, and never `internal`.
pub async fn forward(
    State(state): State<AppState>,
    Path((id, _)): Path<(String, String)>,
    request: Request,
) -> Response {
    relay(&state, &id, request).await.unwrap_or_else(IntoResponse::into_response)
}

async fn relay(state: &AppState, id: &str, request: Request) -> Result<Response, Problem> {
    let (mut parts, body) = request.into_parts();
    // As the caller sent it, still percent-encoded: everything after `/plugins/{id}/`.
    let route = parts.uri.path().splitn(4, '/').nth(3).unwrap_or_default().to_string();
    if !routes::clean(&route) {
        return Err(Problem::bad_request("a route may not contain `.` or `..` segments"));
    }
    let (entry, caller, principal, _pass) = match route.split('/').next().unwrap_or_default() {
        "api" | "ui" => {
            let manifest = state.plugins.get(id).await.map(|entry| entry.manifest);
            let access = routes::access_of(manifest.as_ref(), parts.method.as_str(), &route);
            let authorised = admit(state, &mut parts, id, access).await?;
            let (entry, pass) = serving(state, id).await?;
            (entry, runs::caller_of(&authorised), authorised.principal, pass)
        }
        "public" => {
            let client = state.limits.client(&parts);
            if let Err(wait) = state.limits.public_calls.hit(&format!("{id}|{client}")) {
                let detail = format!("too many calls on {id}'s public routes from this client");
                return Err(Problem::too_many(detail, wait));
            }
            let (entry, pass) = serving(state, id).await?;
            let below = route.strip_prefix("public/").unwrap_or_default();
            if !routes::is_public(&entry.manifest, below) {
                return Err(Problem::not_found("route"));
            }
            // Nobody signed in, so the plugin acts as itself.
            let caller = Caller { kind: "anonymous".into(), ..Caller::default() };
            (entry, caller, Principal::Plugin { id: id.to_string() }, pass)
        }
        _ => return Err(Problem::not_found("route")),
    };
    let body = axum::body::to_bytes(body, MAX_BODY).await.map_err(|_| too_large())?;
    let forwarded = Forwarded {
        method: parts.method.clone(),
        path: route,
        query: parts.uri.query().map(str::to_string),
        headers: routes::request_headers(&parts.headers),
        body,
    };
    let answer = routes::forward(state, &entry, &forwarded, &caller, principal)
        .await
        .map_err(|err| forward_problem(id, &err))?;
    Ok(respond(answer))
}

fn respond(answer: Answer) -> Response {
    let mut response = Response::new(Body::from(answer.body));
    *response.status_mut() = answer.status;
    for (name, value) in answer.headers {
        if let (Ok(name), Ok(value)) = (HeaderName::try_from(name), HeaderValue::try_from(value)) {
            response.headers_mut().append(name, value);
        }
    }
    response
}

/// A plugin the platform has a record of, whether or not it is running now.
pub(super) async fn known(state: &AppState, id: &str) -> Result<bool, Problem> {
    if state.plugins.get(id).await.is_some() {
        return Ok(true);
    }
    if state.repos.identity.list_plugins().await?.iter().any(|known| known == id) {
        return Ok(true);
    }
    Ok(state.repos.plugins.records().await?.iter().any(|record| record.id == id))
}

/// 401, then 404, then 403: which plugins exist is no secret, but the rest is.
async fn admit(
    state: &AppState,
    parts: &mut Parts,
    id: &str,
    access: Access,
) -> Result<Authorised, Problem> {
    let principal = Auth::from_request_parts(parts, state).await?.0;
    if !known(state, id).await? {
        return Err(Problem::not_found("plugin"));
    }
    permissions::authorise(state, principal, id, access).await
}

/// Waits out a handover's pause, up to its timeout, and counts the request in for it to drain.
async fn through_gate(state: &AppState, id: &str) -> Result<Pass, Problem> {
    if state.plugins.get(id).await.is_none() {
        return Err(if known(state, id).await? {
            not_serving(id, None)
        } else {
            Problem::not_found("plugin")
        });
    }
    let patience = state.config.plugins.handover_timeout();
    match state.plugins.gate(id).pass(patience).await {
        Some(pass) => Ok(pass),
        None => Err(not_serving(id, state.plugins.get(id).await.as_ref())),
    }
}

async fn serving(state: &AppState, id: &str) -> Result<(Registered, Pass), Problem> {
    let pass = through_gate(state, id).await?;
    match state.plugins.get(id).await {
        Some(entry) if state.plugins.offers(&entry) => Ok((entry, pass)),
        _ if state.plugins.is_off(id) => Err(turned_off(id)),
        entry => Err(not_serving(id, entry.as_ref())),
    }
}

async fn taking_runs(state: &AppState, id: &str) -> Result<(Registered, Pass), Problem> {
    let pass = through_gate(state, id).await?;
    match state.plugins.get(id).await {
        _ if state.plugins.is_off(id) => Err(turned_off(id)),
        Some(entry) if entry.state.accepts_run() => Ok((entry, pass)),
        entry => Err(not_serving(id, entry.as_ref())),
    }
}

/// `503` for a plugin somebody turned off, which it stays until somebody turns it on.
fn turned_off(id: &str) -> Problem {
    Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "plugin-turned-off",
        format!("{id} is not available"),
    )
    .detail(format!("{id} is turned off; an administrator turns it on from Plugins"))
    .with("plugin", id)
    .with("state", "turned off")
}

/// `503` with the plugin's state in the body, so a caller can tell loading from broken.
fn not_serving(id: &str, entry: Option<&Registered>) -> Problem {
    let state = entry.map_or("not running", |entry| entry.state.as_str());
    let says = if state == PluginState::Error.as_str() { "in error" } else { state };
    let mut problem = Problem::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "plugin-unavailable",
        format!("{id} is not available"),
    )
    .detail(format!("{id} is {says}"))
    .with("plugin", id)
    .with("state", state);
    if let Some(error) = entry.and_then(|entry| entry.error.clone()) {
        problem = problem.with("error", error);
    }
    problem
}

fn too_large() -> Problem {
    Problem::new(StatusCode::PAYLOAD_TOO_LARGE, "too-large", "Request too large")
        .detail(format!("a plugin request carries at most {MAX_BODY} bytes"))
}

fn deadline(id: &str) -> Problem {
    Problem::new(StatusCode::GATEWAY_TIMEOUT, "deadline", format!("{id} did not answer in time"))
        .with("plugin", id)
}

fn forward_problem(id: &str, err: &ForwardError) -> Problem {
    match err {
        ForwardError::Deadline(_) => deadline(id),
        ForwardError::Unreachable(..) => Problem::new(
            StatusCode::BAD_GATEWAY,
            "plugin-unreachable",
            format!("{id} could not be reached"),
        )
        .with("plugin", id),
        ForwardError::Unavailable(detail) => {
            Problem::unavailable(detail.clone()).with("plugin", id)
        }
    }
}

/// A failed run is the plugin's answer, so a `502` carrying what it said; its state is untouched.
fn run_problem(id: &str, err: &RunError) -> Problem {
    match err {
        RunError::NotOnDemand(..) => Problem::conflict(err.to_string()).with("plugin", id),
        RunError::Deadline(_) => deadline(id),
        RunError::Busy | RunError::Unavailable(_) => {
            Problem::unavailable(err.to_string()).with("plugin", id)
        }
        RunError::Failed(detail) => {
            Problem::new(StatusCode::BAD_GATEWAY, "run-failed", format!("{id}'s run failed"))
                .detail(detail.clone())
                .with("plugin", id)
        }
    }
}

pub(super) fn refused(id: &str, err: &TransitionError) -> Problem {
    match err {
        TransitionError::Unknown(_) => Problem::not_found("plugin"),
        other => Problem::new(
            StatusCode::from_u16(other.status()).unwrap_or(StatusCode::CONFLICT),
            "plugin-state",
            format!("{id}: {other}"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use axum::Router;
    use bytes::Bytes;
    use doc_background_tasks::{Task, TaskFilter, TaskState};
    use doc_eventbus::{ConsumerGroup, TopicFilter};
    use doc_permissions::Grants;
    use doc_plugin_protocol::calls::ServiceRequest;
    use doc_plugin_protocol::{
        Capability, Classification, Guard, Manifest, Nav, OwnAccount, RegisterRequest,
    };
    use doc_servicebus::Address;
    use http::{HeaderMap, Method};
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::api::router;
    use crate::config::Config;
    use crate::identity::TokenOwner;
    use crate::permissions::PermissionSource;
    use crate::plugins::api as plugin_api;
    use crate::secrets::TokenKind;
    use crate::testing::{ADMIN, Host, plugin_host_with};

    /// `hello:user:rw`, the plugin's own `greetings` permission and an attribute.
    const ADA: &str = "doc_ses_ada";
    /// `hello:user:ro` and nothing else.
    const BOB: &str = "doc_ses_bob";
    /// Nothing at all.
    const EVE: &str = "doc_ses_eve";

    /// Answers by login, so one test can hold callers with different access.
    struct Source(BTreeMap<String, Value>);

    #[async_trait]
    impl PermissionSource for Source {
        async fn grants(&self, principal: &Principal, _teams: &[Uuid]) -> Result<Grants, String> {
            let held = self.0.get(&principal.label()).cloned().unwrap_or_else(|| json!({}));
            serde_json::from_value(held).map_err(|err| err.to_string())
        }
    }

    fn config() -> Config {
        let mut config = Config::default();
        config.bootstrap.admins = vec!["tester".into()];
        config.plugins.ids = vec!["hello".into(), "github".into()];
        config.plugins.capabilities.insert("github".into(), vec![Capability::PublicRoutes]);
        config
    }

    fn host() -> Host {
        host_with(config())
    }

    fn host_with(config: Config) -> Host {
        let mut host = plugin_host_with(config);
        for (login, token) in [("ada", ADA), ("bob", BOB), ("eve", EVE)] {
            let user = host.identity.add_user(login);
            host.identity.give(token, TokenKind::Session, TokenOwner::User(user.id), None, false);
        }
        let source = Source(BTreeMap::from([
            (
                "ada".to_string(),
                json!({
                    "permissions": ["plugin:hello:user:rw", "plugin:hello:pluginuser:greetings:rw"],
                    "attributes": { "team": "payments" },
                }),
            ),
            ("bob".to_string(), json!({ "permissions": ["plugin:hello:user:ro"] })),
            ("agent-smith".to_string(), json!({ "permissions": ["plugin:hello:service:ro"] })),
        ]));
        host.state = host.state.clone().with_permissions(Arc::new(source));
        host.app = router(host.state.clone());
        host.plugin.resolve_with("hello", host.state.plugins.contexts.clone());
        host
    }

    async fn register_as(host: &Host, manifest: Manifest) -> PluginState {
        let id = manifest.id.clone();
        let request = RegisterRequest {
            manifest,
            address: format!("plugin-{id}:4440"),
            binary_sha256: "a".repeat(64),
            started_at: None,
        };
        plugins::register(&host.state, &host.as_plugin(&id), request).await.expect("registered");
        host.settle(&id).await.expect("still registered")
    }

    async fn running(host: &Host, classification: Classification) {
        let manifest = Manifest {
            id: "hello".into(),
            version: "1.0.0".into(),
            classification,
            ..Manifest::default()
        };
        assert_eq!(register_as(host, manifest).await, PluginState::Running);
    }

    async fn call(
        app: &Router,
        method: Method,
        path: &str,
        token: Option<&str>,
        headers: &[(&str, &str)],
        body: Value,
    ) -> (StatusCode, HeaderMap, Bytes) {
        let mut request = http::Request::builder().method(method).uri(path);
        if let Some(token) = token {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let body = if body.is_null() {
            Body::empty()
        } else {
            request = request.header("content-type", "application/json");
            Body::from(body.to_string())
        };
        let response =
            app.clone().oneshot(request.body(body).expect("request")).await.expect("answered");
        let (status, headers) = (response.status(), response.headers().clone());
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX).await.expect("body");
        (status, headers, bytes)
    }

    async fn get(app: &Router, path: &str, token: Option<&str>) -> (StatusCode, Value) {
        let (status, _, body) = call(app, Method::GET, path, token, &[], Value::Null).await;
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn post(
        app: &Router,
        path: &str,
        token: Option<&str>,
        body: Value,
    ) -> (StatusCode, Value) {
        let (status, _, body) = call(app, Method::POST, path, token, &[], body).await;
        (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
    }

    async fn eventually<F: Fn() -> Fut, Fut: Future<Output = bool>>(what: &str, check: F) {
        for _ in 0..100 {
            if check().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        panic!("{what} did not happen within 5s");
    }

    async fn task(host: &Host, id: Uuid) -> Task {
        host.state.repos.tasks.get(id).await.expect("read").expect("task")
    }

    async fn finished(host: &Host, id: Uuid) -> Task {
        eventually("the task finishing", || async { task(host, id).await.state.finished() }).await;
        task(host, id).await
    }

    #[tokio::test]
    async fn an_unknown_plugin_is_not_found_whoever_asks() {
        let host = host();
        for token in [ADMIN, EVE] {
            let (status, body) =
                get(&host.app, "/api/v1/plugins/nope/api/thing", Some(token)).await;
            assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
            assert_eq!(body["title"], "plugin not found");
        }
    }

    #[tokio::test]
    async fn a_caller_needs_read_access_to_read_and_write_access_to_write() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        let path = "/api/v1/plugins/hello/api/greetings";
        let (status, body) = get(&host.app, path, Some(EVE)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["detail"], "needs plugin:hello:user:ro");
        assert_eq!(get(&host.app, path, Some(BOB)).await.0, StatusCode::OK);
        let (status, body) = post(&host.app, path, Some(BOB), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["detail"], "needs plugin:hello:user:rw");
        assert_eq!(post(&host.app, path, Some(ADA), json!({})).await.0, StatusCode::OK);
        assert_eq!(get(&host.app, path, None).await.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_post_to_a_declared_read_route_needs_only_read_access() {
        let host = host();
        let manifest = Manifest {
            id: "hello".into(),
            version: "1.0.0".into(),
            read_routes: vec!["search".into(), "mcp/*".into(), "ui/rsvp".into()],
            ..Manifest::default()
        };
        assert_eq!(register_as(&host, manifest).await, PluginState::Running);
        for route in ["api/search", "api/mcp", "api/mcp/messages", "ui/rsvp"] {
            let path = format!("/api/v1/plugins/hello/{route}");
            let (status, body) = post(&host.app, &path, Some(BOB), json!({})).await;
            assert_eq!(status, StatusCode::OK, "{route}: {body}");
            assert_eq!(body["caller"]["scope"], "ro");
            let (status, body) = post(&host.app, &path, Some(EVE), json!({})).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{route}");
            assert_eq!(body["detail"], "needs plugin:hello:user:ro");
        }
        for route in ["api/greetings", "api/searches", "ui/search", "api/rsvp"] {
            let path = format!("/api/v1/plugins/hello/{route}");
            let (status, body) = post(&host.app, &path, Some(BOB), json!({})).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{route} is a write");
            assert_eq!(body["detail"], "needs plugin:hello:user:rw");
        }
        let (status, _, _) = call(
            &host.app,
            Method::DELETE,
            "/api/v1/plugins/hello/api/search",
            Some(BOB),
            &[],
            Value::Null,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "only a POST is read");

        let bus = host.state.buses.services.clone();
        let address = Address::plugin("hello").expect("address");
        let bob = reference(&host, "bob").await;
        let asked = json!({ "method": "POST", "body": { "q": "doc" } });
        let answer = bus
            .request_as(&address, "api/search", asked.clone(), Duration::from_secs(2), Some(&bob))
            .await
            .expect("relayed");
        assert_eq!(answer["status"], 200, "the relay checks the same way: {answer}");
        let answer = bus
            .request_as(&address, "api/greetings", asked, Duration::from_secs(2), Some(&bob))
            .await
            .expect("answered");
        assert_eq!(answer["status"], 403);
    }

    #[tokio::test]
    async fn api_and_ui_requests_carry_the_callers_custom_permissions_attributes_and_context() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        let headers = [("hx-request", "true"), ("cookie", "doc_session=secret")];
        let path = "/api/v1/plugins/hello/ui/greetings?page=2";
        let (status, _, _) =
            call(&host.app, Method::GET, path, Some(ADA), &headers, Value::Null).await;
        assert_eq!(status, StatusCode::OK);

        let (forwarded, caller) = host.plugin.requests().pop().expect("forwarded");
        assert_eq!(forwarded.path, "ui/greetings");
        assert_eq!(forwarded.query.as_deref(), Some("page=2"));
        assert_eq!((caller.kind.as_str(), caller.label.as_deref()), ("user", Some("ada")));
        assert_eq!(caller.custom.get("greetings").map(String::as_str), Some("rw"));
        assert_eq!(caller.attributes.get("team").map(String::as_str), Some("payments"));
        assert!(!caller.admin);
        assert_eq!(caller.scope.as_deref(), Some("rw"));
        assert!(caller.writes());
        let names: Vec<&str> = forwarded.headers.iter().map(|(name, _)| name.as_str()).collect();
        assert!(names.contains(&"hx-request"));
        assert!(!names.contains(&"authorization"), "a caller's token never reaches a plugin");
        assert!(!names.contains(&"cookie"), "nor do their cookies");
        let (_, during) = host.plugin.contexts().pop().expect("a context for the request");
        assert!(during.is_some_and(|who| who.starts_with("user:")), "the plugin acts as ada");
    }

    #[tokio::test]
    async fn a_reader_is_told_they_only_read_so_a_page_leaves_out_its_editing() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        get(&host.app, "/api/v1/plugins/hello/ui/greetings", Some(BOB)).await;
        let (_, caller) = host.plugin.requests().pop().expect("forwarded");
        assert_eq!(caller.scope.as_deref(), Some("ro"));
        assert!(!caller.writes());
        get(&host.app, "/api/v1/plugins/hello/ui/greetings", Some(ADMIN)).await;
        let (_, caller) = host.plugin.requests().pop().expect("forwarded");
        assert!(caller.writes(), "an admin writes everywhere");
    }

    #[tokio::test]
    async fn a_platform_admin_is_told_so_and_passes_the_plugins_own_checks() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        get(&host.app, "/api/v1/plugins/hello/api/greetings", Some(ADMIN)).await;
        let (_, caller) = host.plugin.requests().pop().expect("forwarded");
        assert!(caller.admin);
        assert!(caller.allows("greetings", true));
    }

    #[tokio::test]
    async fn the_plugins_answer_comes_back_but_never_its_cookies() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        host.plugin.answer_with(Answer {
            status: StatusCode::CREATED,
            headers: vec![
                ("content-type".into(), "text/html; charset=utf-8".into()),
                ("set-cookie".into(), "doc_session=stolen".into()),
                ("content-security-policy".into(), "default-src *".into()),
                ("x-frame-options".into(), "ALLOWALL".into()),
                ("hx-trigger".into(), "greeted".into()),
                ("x-doc-faux".into(), "faux-data".into()),
            ],
            body: Bytes::from_static(b"<p>hello</p>"),
        });
        let path = "/api/v1/plugins/hello/ui/greet";
        let (status, headers, body) =
            call(&host.app, Method::POST, path, Some(ADA), &[], json!({})).await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(headers["content-type"], "text/html; charset=utf-8");
        assert_eq!(headers["hx-trigger"], "greeted");
        assert_eq!(headers["x-doc-faux"], "faux-data", "the frontend says whose faux data it is");
        assert!(headers.get("set-cookie").is_none());
        assert!(headers.get("content-security-policy").is_none(), "only the frontend sets the CSP");
        assert!(headers.get("x-frame-options").is_none());
        assert_eq!(&body[..], b"<p>hello</p>");
    }

    #[tokio::test]
    async fn a_route_that_fails_fails_only_that_request() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        let panicked =
            json!({ "type": "/problems/panicked", "status": 500, "detail": "panicked: boom" });
        host.plugin.answer_with(Answer::json(StatusCode::INTERNAL_SERVER_ERROR, &panicked));
        let (status, body) = get(&host.app, "/api/v1/plugins/hello/api/panic", Some(ADA)).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body["detail"], "panicked: boom");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
    }

    #[tokio::test]
    async fn a_plugin_that_is_not_serving_answers_503_with_its_state() {
        let host = host();
        host.plugin.set_failing(Some("load broke"));
        let manifest =
            Manifest { id: "hello".into(), version: "1.0.0".into(), ..Manifest::default() };
        assert_eq!(register_as(&host, manifest).await, PluginState::Error);
        let (status, body) = get(&host.app, "/api/v1/plugins/hello/api/greetings", Some(ADA)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["state"], "error");
        assert!(body["error"].as_str().is_some_and(|error| error.contains("load broke")), "{body}");

        host.plugin.set_failing(None);
        plugins::unload(&host.state, "hello").await.expect("unloaded");
        let (status, body) = get(&host.app, "/api/v1/plugins/hello/ui/", Some(ADA)).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "known, but no process");
        assert_eq!(body["state"], "not running");
    }

    #[tokio::test]
    async fn a_request_that_outlives_its_deadline_is_504() {
        let mut config = config();
        config.plugins.request_deadline_s = 1;
        let host = host_with(config);
        running(&host, Classification::Synchronous).await;
        host.plugin.slow_down(Some(Duration::from_millis(1500)));
        let (status, body) = get(&host.app, "/api/v1/plugins/hello/api/slow", Some(ADA)).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(body["plugin"], "hello");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
    }

    #[tokio::test]
    async fn a_route_cannot_walk_out_of_its_surface() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        let (status, _) =
            get(&host.app, "/api/v1/plugins/hello/api/../internal/x", Some(ADA)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(host.plugin.requests().is_empty());
    }

    #[tokio::test]
    async fn public_routes_need_the_capability_and_a_declared_path() {
        let host = host();
        let github = Manifest {
            id: "github".into(),
            version: "1.0.0".into(),
            capabilities: vec![Capability::PublicRoutes],
            public_routes: vec!["callback".into(), "assets/*".into()],
            ..Manifest::default()
        };
        assert_eq!(register_as(&host, github).await, PluginState::Running);
        running(&host, Classification::Synchronous).await;

        for open in ["callback", "assets/app.css"] {
            let path = format!("/api/v1/plugins/github/public/{open}");
            let (status, body) = get(&host.app, &path, None).await;
            assert_eq!(status, StatusCode::OK, "{path}: {body}");
            assert_eq!(body["caller"]["kind"], "anonymous");
        }
        let (status, _) = get(&host.app, "/api/v1/plugins/github/public/settings", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "not declared");
        let (status, _) = get(&host.app, "/api/v1/plugins/hello/public/callback", None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "hello may not have public routes");
    }

    #[tokio::test]
    async fn calls_on_public_routes_are_limited_per_client() {
        use axum::extract::ConnectInfo;
        let mut config = config();
        config.limits.public_calls_per_minute = 2;
        let host = host_with(config);
        let github = Manifest {
            id: "github".into(),
            version: "1.0.0".into(),
            capabilities: vec![Capability::PublicRoutes],
            public_routes: vec!["hooks/*".into()],
            ..Manifest::default()
        };
        assert_eq!(register_as(&host, github).await, PluginState::Running);
        let call = |peer: &'static str| {
            let app = host.app.clone();
            async move {
                let mut request = http::Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/plugins/github/public/hooks/push")
                    .body(Body::from("{}"))
                    .expect("request");
                let peer: std::net::SocketAddr = format!("{peer}:443").parse().expect("address");
                request.extensions_mut().insert(ConnectInfo(peer));
                let response = app.oneshot(request).await.expect("answered");
                (response.status(), response.headers().contains_key(http::header::RETRY_AFTER))
            }
        };
        for _ in 0..2 {
            assert_eq!(call("192.0.2.10").await, (StatusCode::OK, false));
        }
        assert_eq!(call("192.0.2.10").await, (StatusCode::TOO_MANY_REQUESTS, true));
        assert_eq!(
            call("192.0.2.11").await,
            (StatusCode::OK, false),
            "another sender is unaffected"
        );
    }

    #[tokio::test]
    async fn internal_routes_answer_core_over_the_service_bus_and_nobody_else() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        let (status, _) =
            get(&host.app, "/api/v1/plugins/hello/internal/permissions", Some(ADMIN)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "not reachable over HTTP, even for an admin");

        let bus = host.state.buses.services.clone();
        let address = Address::plugin("hello").expect("address");
        let deadline = Duration::from_secs(2);
        let answer = bus
            .request(&address, "permissions", json!({ "principal": "user:1" }), deadline)
            .await
            .expect("core is answered");
        assert_eq!(answer["path"], "internal/permissions");
        assert_eq!(answer["caller"]["kind"], "platform");
        let (forwarded, _) = host.plugin.requests().pop().expect("forwarded");
        assert_eq!(forwarded.body, Bytes::from(json!({ "principal": "user:1" }).to_string()));

        let relayed = bus
            .request_as(&address, "permissions", json!({}), deadline, Some("plugin:kb"))
            .await
            .expect_err("a request relayed for someone is not core's");
        assert!(relayed.to_string().contains("core alone"), "{relayed}");
    }

    async fn reference(host: &Host, login: &str) -> String {
        use crate::db::repositories::IdentityRepository;
        let held = host.identity.identity("local", login).await.expect("looked up");
        format!("user:{}", held.expect("known").user_id)
    }

    #[tokio::test]
    async fn a_plugin_asks_another_s_api_as_whoever_it_acts_for() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        let bus = host.state.buses.services.clone();
        let address = Address::plugin("hello").expect("address");
        let deadline = Duration::from_secs(2);
        let asked = json!({ "method": "GET", "query": "page=2" });

        let ada = reference(&host, "ada").await;
        let answer = bus
            .request_as(&address, "api/greetings", asked.clone(), deadline, Some(&ada))
            .await
            .expect("relayed");
        assert_eq!(answer["status"], 200, "{answer}");
        assert_eq!(answer["body"]["path"], "api/greetings");
        assert_eq!(answer["body"]["caller"]["label"], "ada");
        assert_eq!(answer["body"]["caller"]["custom"]["greetings"], "rw");
        let (forwarded, _) = host.plugin.requests().pop().expect("forwarded");
        assert_eq!(forwarded.query.as_deref(), Some("page=2"));

        let eve = reference(&host, "eve").await;
        let refused = bus
            .request_as(&address, "api/greetings", asked, deadline, Some(&eve))
            .await
            .expect("answered");
        assert_eq!(refused["status"], 403);
        assert_eq!(refused["body"]["detail"], "needs plugin:hello:user:ro");
        let bob = reference(&host, "bob").await;
        let write = json!({ "method": "POST", "body": { "name": "doc" } });
        let refused = bus
            .request_as(&address, "api/greetings", write, deadline, Some(&bob))
            .await
            .expect("answered");
        assert_eq!(refused["status"], 403, "bob only reads hello");
        assert_eq!(host.plugin.requests().len(), 1, "neither refusal reached the plugin");

        let walked = bus
            .request_as(&address, "api/../internal/x", json!({}), deadline, Some(&ada))
            .await
            .expect_err("no way out of api/*");
        assert!(walked.to_string().contains("core alone"), "{walked}");
    }

    const DAY: Duration = Duration::from_secs(86_400);

    /// What a plugin asks another's `api/` through core, as `doc_plugin_sdk`'s `ask` sends it.
    fn asking(route: &str, method: &str, guard: Option<Guard>) -> ServiceRequest {
        ServiceRequest {
            address: "plugin.hello".into(),
            subject: format!("api/{route}"),
            payload: json!({ "method": method }),
            deadline_ms: Some(2_000),
            queue: false,
            guard,
        }
    }

    #[tokio::test]
    async fn a_plugin_asking_as_itself_is_its_own_service_account_when_it_has_one() {
        let mut config = config();
        config.plugins.ids.extend(["agent".to_string(), "vacuum".to_string()]);
        config.plugins.capabilities.insert("agent".into(), vec![Capability::ServiceAccount]);
        let host = host_with(config);
        running(&host, Classification::Synchronous).await;
        let mut agent = Manifest {
            id: "agent".into(),
            version: "1.0.0".into(),
            capabilities: vec![Capability::ServiceAccount],
            service_account: Some(OwnAccount::new("agent-smith", "Agent Smith, by itself")),
            ..Manifest::default()
        };
        assert_eq!(register_as(&host, agent.clone()).await, PluginState::Running);
        let identity = &host.state.repos.identity;
        let account = identity.plugin_service_account("agent").await.unwrap().expect("made");
        assert_eq!((account.name.as_str(), account.owner_id), ("agent-smith", None));
        assert_eq!(account.owner_team_id, None, "the platform owns it");

        let contexts = &host.state.plugins.contexts;
        let itself = contexts.issue("agent", host.as_plugin("agent"), DAY).expect("issued");
        let asked = asking("greetings", "GET", None);
        let answer = plugin_api::services(&host.state, "agent", Some(itself.token()), asked)
            .await
            .expect("answered");
        assert_eq!(answer.payload["status"], 200, "{}", answer.payload);
        let caller = &answer.payload["body"]["caller"];
        assert_eq!(
            (caller["kind"].as_str(), caller["label"].as_str()),
            (Some("service"), Some("agent-smith"))
        );
        assert_eq!(caller["via"], "agent");
        let write = asking("greetings", "POST", None);
        let refused = plugin_api::services(&host.state, "agent", Some(itself.token()), write)
            .await
            .expect("answered");
        assert_eq!(refused.payload["status"], 403, "it holds only what it was granted");

        // Registering again keeps the account; one without the capability holds nothing.
        agent.version = "1.0.1".into();
        register_as(&host, agent).await;
        let again = identity.plugin_service_account("agent").await.unwrap().expect("kept");
        assert_eq!(again.id, account.id);
        let other = contexts.issue("vacuum", host.as_plugin("vacuum"), DAY).expect("issued");
        let asked = asking("greetings", "GET", None);
        let refused = plugin_api::services(&host.state, "vacuum", Some(other.token()), asked)
            .await
            .expect("answered");
        assert_eq!(refused.payload["status"], 403, "a plugin without one is nobody");

        // A disabled account acts for nobody.
        identity.set_service_account_disabled(account.id, true).await.unwrap();
        permissions::forget_all(&host.state).await;
        let asked = asking("greetings", "GET", None);
        plugin_api::services(&host.state, "agent", Some(itself.token()), asked)
            .await
            .expect_err("disabled");
    }

    #[tokio::test]
    async fn a_plugin_cannot_claim_a_service_account_somebody_else_made() {
        let mut config = config();
        config.plugins.ids.push("agent".into());
        config.plugins.capabilities.insert("agent".into(), vec![Capability::ServiceAccount]);
        let host = host_with(config);
        host.identity.add_service_account("deployer");
        let manifest = Manifest {
            id: "agent".into(),
            version: "1.0.0".into(),
            capabilities: vec![Capability::ServiceAccount],
            service_account: Some(OwnAccount::new("deployer", "")),
            ..Manifest::default()
        };
        assert_eq!(register_as(&host, manifest).await, PluginState::Running);
        let held = host.state.repos.identity.plugin_service_account("agent").await.unwrap();
        assert!(held.is_none(), "the deployer's access is not the plugin's to take");
    }

    #[tokio::test]
    async fn a_relayed_call_says_who_relayed_it_and_its_guard_holds_all_the_way_down() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        let ada = reference(&host, "ada").await;
        let ada = permissions::principal_of(&host.state, &ada).await.expect("known");
        let contexts = &host.state.plugins.contexts;
        let for_ada = contexts.issue("agent", ada.clone(), DAY).expect("issued");
        let ask =
            |request| plugin_api::services(&host.state, "agent", Some(for_ada.token()), request);

        let answer = ask(asking("greetings", "GET", Some(Guard::NotProduction))).await.unwrap();
        let caller = &answer.payload["body"]["caller"];
        assert_eq!(
            (caller["label"].as_str(), caller["via"].as_str()),
            (Some("ada"), Some("agent"))
        );
        assert_eq!(caller["guard"], "not-production");
        assert_eq!(caller["production"], json!(["production", "prod", "live"]));

        let forwarded = host.plugin.requests().len();
        let answer = ask(asking("greetings", "POST", Some(Guard::ReadOnly))).await.unwrap();
        assert_eq!(answer.payload["status"], 403, "ada may write, but this call only reads");
        assert_eq!(host.plugin.requests().len(), forwarded, "it never reached hello");
        let answer = ask(asking("greetings", "GET", Some(Guard::ReadOnly))).await.unwrap();
        assert_eq!(answer.payload["status"], 200, "reading is fine");

        // Whatever hello asks while it handles a guarded call is guarded too, and cannot loosen it.
        let handling = contexts.issue("hello", ada, DAY).expect("issued");
        contexts.relayed(handling.token(), Some("agent"), Some(Guard::ReadOnly));
        for loosened in [None, Some(Guard::NotProduction)] {
            let write = asking("greetings", "POST", loosened);
            let answer = plugin_api::services(&host.state, "hello", Some(handling.token()), write)
                .await
                .unwrap();
            assert_eq!(answer.payload["status"], 403, "{loosened:?}");
        }

        // And what hello audits while handling it says who relayed it.
        let written = doc_plugin_protocol::calls::AuditRequest {
            action: "greeted".into(),
            subject: None,
            detail: json!({}),
        };
        plugin_api::audit(&host.state, "hello", Some(handling.token()), written).await.unwrap();
        let entry = host.identity.audit_entries().pop().expect("an entry");
        assert_eq!(entry.detail["via"], "plugin:agent");
        assert!(entry.detail["on_behalf_of"].as_str().is_some_and(|who| who.starts_with("user:")));
    }

    #[tokio::test]
    async fn a_plugin_asking_as_itself_reaches_discovery_routes_and_nobody_else_does() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        let bus = host.state.buses.services.clone();
        let address = Address::plugin("hello").expect("address");
        let deadline = Duration::from_secs(2);
        let asked = json!({ "method": "POST", "body": { "repository": "acme/card-gateway" } });
        let answer = bus
            .request_as(
                &address,
                "discovery/archive-links",
                asked.clone(),
                deadline,
                Some("plugin:kb"),
            )
            .await
            .expect("answered");
        assert_eq!(answer["status"], 200, "{answer}");
        assert_eq!(answer["body"]["path"], "discovery/archive-links");
        assert_eq!(answer["body"]["caller"]["kind"], "plugin");
        assert_eq!(answer["body"]["caller"]["id"], "kb");
        let (forwarded, _) = host.plugin.requests().pop().expect("forwarded");
        assert_eq!(forwarded.method, Method::POST);
        assert_eq!(
            forwarded.body,
            Bytes::from(json!({ "repository": "acme/card-gateway" }).to_string())
        );
        let (_, during) = host.plugin.contexts().pop().expect("a context for the request");
        assert_eq!(during.as_deref(), Some("plugin:kb"), "hello acts as kb if it asks onward");

        let ada = reference(&host, "ada").await;
        let refused = bus
            .request_as(&address, "discovery/archive-links", asked, deadline, Some(&ada))
            .await
            .expect_err("people go through api/*");
        assert!(refused.to_string().contains("other plugins alone"), "{refused}");
        let walked = bus
            .request_as(&address, "discovery/../internal/x", json!({}), deadline, Some("plugin:kb"))
            .await
            .expect_err("no way out of discovery/*");
        assert!(walked.to_string().contains("not a route"), "{walked}");
        let (status, _) =
            get(&host.app, "/api/v1/plugins/hello/discovery/archive-links", Some(ADMIN)).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "not reachable over HTTP");
    }

    #[tokio::test]
    async fn core_access_answers_for_whoever_a_plugin_acts_for() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        let address = Address::core("access").expect("address");
        let service = Arc::new(crate::api::tasks::AccessService(host.state.clone()));
        host.state.buses.services.serve(address.clone(), service).await.expect("served");
        let bus = host.state.buses.services.clone();
        let deadline = Duration::from_secs(2);

        let ada = reference(&host, "ada").await;
        let access =
            bus.request_as(&address, "", json!({}), deadline, Some(&ada)).await.expect("answered");
        assert_eq!(access["plugins"]["hello"]["write"], true);
        assert_eq!(access["plugins"]["hello"]["custom"]["greetings"], "rw");
        let bob = reference(&host, "bob").await;
        let access =
            bus.request_as(&address, "", json!({}), deadline, Some(&bob)).await.expect("answered");
        assert_eq!(access["plugins"]["hello"]["read"], true);
        assert_eq!(access["plugins"]["hello"]["write"], false);

        let nobody =
            bus.request(&address, "", json!({}), deadline).await.expect_err("nobody to answer for");
        assert!(nobody.to_string().contains("acts for"), "{nobody}");
        let stranger = format!("user:{}", Uuid::now_v7());
        let unknown = bus
            .request_as(&address, "", json!({}), deadline, Some(&stranger))
            .await
            .expect_err("unknown");
        assert!(!unknown.to_string().is_empty());
    }

    #[tokio::test]
    async fn a_synchronous_run_answers_with_its_output() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        let asked = json!({ "payload": { "name": "doc" } });
        let (status, body) = post(&host.app, "/api/v1/plugins/hello/run", Some(ADA), asked).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["output"], json!({ "ran": { "name": "doc" } }));
        let (input, caller) = host.plugin.runs().pop().expect("run");
        assert_eq!(input.task, None);
        assert_eq!(caller.custom.get("greetings").map(String::as_str), Some("rw"));
        let (_, during) = host.plugin.contexts().pop().expect("a context for the run");
        assert!(during.is_some_and(|who| who.starts_with("user:")));
    }

    #[tokio::test]
    async fn running_a_plugin_needs_write_access_to_it() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        let (status, body) =
            post(&host.app, "/api/v1/plugins/hello/run", Some(BOB), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["detail"], "needs plugin:hello:user:rw");
        let (status, _) =
            post(&host.app, "/api/v1/plugins/hello/cancel", Some(BOB), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(host.plugin.runs().is_empty());
    }

    #[tokio::test]
    async fn a_run_that_fails_is_a_502_and_leaves_the_plugin_running() {
        let host = host();
        running(&host, Classification::Synchronous).await;
        host.plugin.fail_runs(Some("the greeting went wrong"));
        let (status, body) =
            post(&host.app, "/api/v1/plugins/hello/run", Some(ADA), json!({})).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(body["detail"], "the greeting went wrong");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
    }

    #[tokio::test]
    async fn a_synchronous_run_that_outlives_its_deadline_is_504() {
        let mut config = config();
        config.plugins.request_deadline_s = 1;
        let host = host_with(config);
        running(&host, Classification::Synchronous).await;
        host.plugin.slow_down(Some(Duration::from_millis(1500)));
        let (status, _) = post(&host.app, "/api/v1/plugins/hello/run", Some(ADA), json!({})).await;
        assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
    }

    #[tokio::test]
    async fn an_async_run_is_queued_and_its_completion_announced() {
        let host = host();
        running(&host, Classification::Async).await;
        let filter = TopicFilter::new("plugin.hello.run.completed").expect("filter");
        let mut completed = host
            .state
            .buses
            .events
            .subscribe(ConsumerGroup::new("test", filter))
            .await
            .expect("subscribed");
        runs::start_pool(&host.state);

        let asked = json!({ "payload": { "n": 1 } });
        let (status, body) = post(&host.app, "/api/v1/plugins/hello/run", Some(ADA), asked).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["kind"], "plugin.hello.run");
        let id: Uuid = serde_json::from_value(body["id"].clone()).expect("a task ID");

        let done = finished(&host, id).await;
        assert_eq!(done.state, TaskState::Succeeded);
        assert_eq!(done.result, Some(json!({ "ran": { "n": 1 } })));
        let (input, caller) = host.plugin.runs().pop().expect("run");
        assert_eq!(input.task, Some(id), "the plugin can report against its task");
        assert_eq!(caller.label.as_deref(), Some("ada"), "run for whoever started it");
        assert_eq!(caller.custom.get("greetings").map(String::as_str), Some("rw"));

        let delivery = tokio::time::timeout(Duration::from_secs(2), completed.next())
            .await
            .expect("announced")
            .expect("delivered");
        assert_eq!(delivery.event.payload["task"], json!(id));
        assert_eq!(delivery.event.payload["state"], "succeeded");
    }

    #[tokio::test]
    async fn a_failing_background_run_is_retried_then_fails_with_its_error() {
        let host = host();
        running(&host, Classification::Async).await;
        runs::start_pool(&host.state);
        host.plugin.fail_runs(Some("no network"));
        let asked = json!({ "payload": {}, "max_attempts": 2 });
        let (_, body) = post(&host.app, "/api/v1/plugins/hello/run", Some(ADA), asked).await;
        let id: Uuid = serde_json::from_value(body["id"].clone()).expect("a task ID");

        let done = finished(&host, id).await;
        assert_eq!(done.state, TaskState::Failed);
        assert_eq!(done.attempts, 2, "tried as many times as asked, and no more");
        assert_eq!(done.error.as_deref(), Some("no network"));
        assert_eq!(host.plugin.runs().len(), 2);
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
    }

    #[tokio::test]
    async fn a_one_shot_plugin_runs_once_after_each_load_and_not_on_demand() {
        let host = host();
        runs::start_pool(&host.state);
        running(&host, Classification::OneShot).await;
        eventually("the one-shot run", || async { host.plugin.runs().len() == 1 }).await;
        let filter = TaskFilter { kind: Some("plugin.hello.run".into()), ..TaskFilter::default() };
        let queued = host.state.repos.tasks.list(filter).await.expect("tasks");
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].started_by.kind, "platform");
        assert_eq!(finished(&host, queued[0].id).await.state, TaskState::Succeeded);

        let (status, body) =
            post(&host.app, "/api/v1/plugins/hello/run", Some(ADA), json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
    }

    #[tokio::test]
    async fn a_long_running_run_lasts_until_cancelled_and_starts_again_on_resume() {
        let host = host();
        host.plugin.hold_runs(true);
        running(&host, Classification::LongRunning).await;
        eventually("the run starting", || async { host.plugin.runs().len() == 1 }).await;
        let (status, _) = post(&host.app, "/api/v1/plugins/hello/run", Some(ADA), json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "the platform runs it, not a caller");

        let (status, body) =
            post(&host.app, "/api/v1/plugins/hello/cancel", Some(ADA), json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            host.state_of("hello").await,
            Some(PluginState::Cancelled),
            "ending on cancel is no error"
        );

        let resume = json!({ "state": "running" });
        let (status, _) = post(&host.app, "/api/v1/plugins/hello/state", Some(ADMIN), resume).await;
        assert_eq!(status, StatusCode::OK);
        eventually("the run starting again", || async { host.plugin.runs().len() == 2 }).await;
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
    }

    #[tokio::test]
    async fn a_long_running_run_that_ends_by_itself_puts_the_plugin_in_error() {
        let host = host();
        let manifest = Manifest {
            id: "hello".into(),
            version: "1.0.0".into(),
            classification: Classification::LongRunning,
            ..Manifest::default()
        };
        register_as(&host, manifest).await;
        eventually("the plugin going into error", || async {
            host.state_of("hello").await == Some(PluginState::Error)
        })
        .await;
        let error = host.error_of("hello").await.unwrap_or_default();
        assert!(error.contains("the long-running run returned"), "{error}");
    }

    #[tokio::test]
    async fn a_long_running_run_that_loses_its_connection_starts_again_once_the_plugin_reports() {
        let host = host();
        host.plugin.hold_runs(true);
        host.plugin.cut_runs(1);
        let manifest = Manifest {
            id: "hello".into(),
            version: "1.0.0".into(),
            classification: Classification::LongRunning,
            ..Manifest::default()
        };
        register_as(&host, manifest).await;
        eventually("the connection being lost", || async {
            host.state_of("hello").await == Some(PluginState::Error)
        })
        .await;
        let error = host.error_of("hello").await.unwrap_or_default();
        assert!(error.starts_with(plugins::UNREACHABLE), "{error}");

        let report = doc_plugin_protocol::Liveness {
            id: "hello".into(),
            state: PluginState::Running,
            error: None,
            instance: None,
        };
        plugins::liveness(&host.state, &host.as_plugin("hello"), report).await.expect("reported");
        eventually("the run starting again", || async { host.plugin.runs().len() == 2 }).await;
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
        assert_eq!(host.error_of("hello").await, None);
    }

    #[tokio::test]
    async fn cancelling_stops_the_plugins_queued_work_and_its_runs_until_resumed() {
        let host = host();
        running(&host, Classification::Async).await;
        let mut queued = Vec::new();
        for _ in 0..2 {
            let (_, body) =
                post(&host.app, "/api/v1/plugins/hello/run", Some(ADA), json!({})).await;
            queued.push(serde_json::from_value::<Uuid>(body["id"].clone()).expect("a task ID"));
        }
        let (status, _) =
            post(&host.app, "/api/v1/plugins/hello/cancel", Some(ADA), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(host.plugin.cancelled.load(std::sync::atomic::Ordering::SeqCst), 1);
        for id in queued {
            assert_eq!(task(&host, id).await.state, TaskState::Cancelled);
        }
        let (status, body) =
            post(&host.app, "/api/v1/plugins/hello/run", Some(ADA), json!({})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["state"], "cancelled");
    }

    #[tokio::test]
    async fn navigation_and_panels_are_offered_only_for_plugins_the_caller_can_read() {
        let host = host();
        let panel =
            doc_plugin_protocol::ResourcePanel::new("service", "Greetings", "/panels/service");
        let manifest = Manifest {
            id: "hello".into(),
            version: "1.0.0".into(),
            nav: vec![Nav::new("Hello", "/")],
            resource_panels: vec![panel],
            ..Manifest::default()
        };
        register_as(&host, manifest).await;
        let (_, body, _) = crate::testing::get_as(&host.app, "/api/v1/me/access", BOB).await;
        assert_eq!(body["plugins"]["hello"]["nav"], json!([{ "label": "Hello", "path": "/" }]));
        let offered =
            json!([{ "resource": "service", "label": "Greetings", "path": "/panels/service" }]);
        assert_eq!(body["plugins"]["hello"]["panels"], offered);
        let (_, body, _) = crate::testing::get_as(&host.app, "/api/v1/me/access", EVE).await;
        assert_eq!(body["plugins"]["hello"]["nav"], json!([]), "eve cannot read hello");
        assert_eq!(body["plugins"]["hello"]["panels"], json!([]));
    }

    #[tokio::test]
    async fn an_unloaded_plugin_offers_no_navigation_or_panels() {
        let host = host();
        let manifest = Manifest {
            id: "hello".into(),
            version: "1.0.0".into(),
            nav: vec![Nav::new("Hello", "/")],
            ..Manifest::default()
        };
        assert_eq!(register_as(&host, manifest).await, PluginState::Running);
        let (_, body, _) = crate::testing::get_as(&host.app, "/api/v1/me/access", BOB).await;
        assert_eq!(body["plugins"]["hello"]["running"], true);

        plugins::unload(&host.state, "hello").await.expect("unloaded");
        let (_, body, _) = crate::testing::get_as(&host.app, "/api/v1/me/access", BOB).await;
        let hello = &body["plugins"]["hello"];
        assert_ne!(hello["running"], true, "{hello}");
        assert!(hello["nav"].as_array().is_none_or(Vec::is_empty), "its pages cannot be opened");
    }
}
