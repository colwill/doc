//! Plugin management (T25): every plugin in one call and each one's history, the operator's reload,
//! unload and state changes, and registration tokens. These routes are core's, so reading needs
//! `core` read access and changing anything `core` write access; every change is audited.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Duration, Utc};
use doc_plugin_protocol::{Capability, Classification, Nav, PluginState};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::auth::TokenView;
use super::plugins::{known, refused};
use super::problem::Problem;
use super::status::known_plugins;
use crate::auth::forget;
use crate::db::repositories::NewToken;
use crate::identity::{Principal, TokenOwner, token_hash};
use crate::permissions::Authorised;
use crate::plugins::{self, Following, Registered, TransitionError, TurnedOff, handover};
use crate::secrets::{TokenKind, generate_token};
use crate::status::plugins::{PluginStatus, StatusChange};

/// How much of a plugin's history its page shows, newest first.
const HISTORY: u32 = 100;
const MAX_TOKEN_DAYS: i64 = 365;
const MAX_NAME: usize = 64;

#[derive(Debug, Serialize)]
pub struct PluginView {
    pub id: String,
    pub version: Option<String>,
    pub classification: Option<Classification>,
    /// §5's lifecycle state, or none while no process is registered.
    pub state: Option<PluginState>,
    pub error: Option<String>,
    /// The most recent error, which outlives the plugin recovering from it.
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    /// When it entered its state, or left the registry.
    pub since: Option<DateTime<Utc>>,
    pub registered_at: Option<DateTime<Utc>>,
    pub checked_at: Option<DateTime<Utc>>,
    /// What **Enable** would switch, for a running plugin whose features work from other plugins'
    /// and are not all on yet (FEAT-DORA).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enable: Option<Value>,
    /// Who turned it off and when, while it is off: held `cancelled` and offered to nobody.
    pub turned_off: Option<TurnedOff>,
    /// The flag it is turned on and off by, while it follows one.
    pub follows: Option<Following>,
    /// The category `[[plugins.categories]]` puts it in, if any.
    pub category: Option<String>,
}

impl PluginView {
    fn of(id: String, row: Option<PluginStatus>) -> Self {
        let Some(row) = row else {
            return Self {
                id,
                version: None,
                classification: None,
                state: None,
                error: None,
                last_error: None,
                last_error_at: None,
                since: None,
                registered_at: None,
                checked_at: None,
                enable: None,
                turned_off: None,
                follows: None,
                category: None,
            };
        };
        Self {
            id,
            version: Some(row.version),
            classification: Some(row.classification),
            state: row.state,
            error: row.error,
            last_error: row.last_error,
            last_error_at: row.last_error_at,
            since: Some(row.since),
            registered_at: Some(row.registered_at),
            checked_at: Some(row.checked_at),
            enable: None,
            turned_off: None,
            follows: None,
            category: None,
        }
    }
}

/// What only the live registry knows: the process the plugin is registered from right now.
#[derive(Debug, Serialize)]
pub struct Registration {
    pub instance: Uuid,
    pub address: String,
    pub last_seen: DateTime<Utc>,
    /// True while a hot reload or an operator's reload is under way.
    pub busy: bool,
    pub nav: Vec<Nav>,
    pub capabilities: Vec<Capability>,
}

#[derive(Debug, Serialize)]
pub struct PluginPage {
    #[serde(flatten)]
    pub plugin: PluginView,
    /// `workers` when the plugin probe recorded this, `live` when it came from the registry.
    pub source: &'static str,
    pub registration: Option<Registration>,
    pub history: Vec<StatusChange>,
}

#[derive(Debug, Deserialize)]
pub struct SetState {
    pub state: PluginState,
}

#[derive(Debug, Deserialize)]
pub struct SetEnabled {
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct SetFlag {
    pub flag: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct TokenRequest {
    pub name: Option<String>,
    pub expires_in_days: Option<i64>,
}

/// 404 for a plugin the platform does not know of, and 409 for one with no process registered.
async fn registered(state: &AppState, id: &str) -> Result<Registered, Problem> {
    if let Some(entry) = state.plugins.get(id).await {
        return Ok(entry);
    }
    if known(state, id).await? {
        let detail = format!("{id} has no process registered; starting it registers it");
        return Err(Problem::conflict(detail).with("plugin", id));
    }
    Err(Problem::not_found("plugin"))
}

async fn exists(state: &AppState, id: &str) -> Result<(), Problem> {
    if known(state, id).await? { Ok(()) } else { Err(Problem::not_found("plugin")) }
}

pub async fn list(_: Authorised, State(state): State<AppState>) -> Response {
    let (known, source) = known_plugins(&state).await;
    let mut plugins: Vec<PluginView> =
        known.into_iter().map(|(id, row)| PluginView::of(id, row)).collect();
    for plugin in &mut plugins {
        plugin.enable = enabling(&state, &plugin.id).await;
        plugin.turned_off = state.plugins.turned_off(&plugin.id);
        plugin.follows = state.plugins.following(&plugin.id);
        plugin.category = state.config.plugins.category(&plugin.id).map(str::to_string);
    }
    Json(json!({ "source": source, "plugins": plugins })).into_response()
}

/// Only a running plugin that declares a feature working from another plugin's is asked, so the
/// list costs nothing more for the plugins that declare none.
async fn enabling(state: &AppState, id: &str) -> Option<Value> {
    let entry = state.plugins.get(id).await.filter(|entry| entry.state.serves_requests())?;
    if entry.manifest.features.iter().all(|feature| feature.needs.is_empty()) {
        return None;
    }
    let offer = plugins::settings::enabling(state, id, &entry.manifest).await?;
    let with: Vec<Value> = offer
        .with
        .iter()
        .map(|(plugin, feature)| json!({ "plugin": plugin, "feature": feature }))
        .collect();
    Some(json!({ "features": offer.features, "with": with, "blocked": offer.blocked }))
}

pub async fn show(
    _: Authorised,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<PluginPage>, Problem> {
    let (known, source) = known_plugins(&state).await;
    let Some((id, row)) = known.into_iter().find(|(known, _)| *known == id) else {
        return Err(Problem::not_found("plugin"));
    };
    let registration = state.plugins.get(&id).await.map(|entry| Registration {
        instance: entry.instance,
        address: entry.address.clone(),
        last_seen: entry.last_seen,
        busy: state.plugins.handing_over(&id),
        nav: entry.manifest.nav.clone(),
        capabilities: entry.manifest.capabilities.clone(),
    });
    let history = state.repos.plugin_status.history(&id, HISTORY).await?;
    let mut plugin = PluginView::of(id, row);
    plugin.enable = enabling(&state, &plugin.id).await;
    plugin.turned_off = state.plugins.turned_off(&plugin.id);
    plugin.follows = state.plugins.following(&plugin.id);
    plugin.category = state.config.plugins.category(&plugin.id).map(str::to_string);
    Ok(Json(PluginPage { plugin, source, registration, history }))
}

/// Turns a plugin off, or on again. `{"enabled": false}` holds it `cancelled` and offers it to
/// nobody — no navigation, panels, pages or API — across restarts and new versions, until
/// `{"enabled": true}` resumes it. A plugin need not be registered to be turned off.
pub async fn set_enabled(
    caller: Authorised,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetEnabled>,
) -> Result<Json<Value>, Problem> {
    exists(&state, &id).await?;
    let now = plugins::switches::switch(&state, &id, body.enabled, &caller.principal)
        .await
        .map_err(|err| refused(&id, &err))?;
    Ok(Json(json!({
        "plugin": id,
        "enabled": body.enabled,
        "state": now,
        "turned_off": state.plugins.turned_off(&id),
    })))
}

/// Has a plugin follow a flag — `{"flag": "enable_maturity"}` — so the platform turns it on and off
/// as that flag says, read from the flags plugin as the service `doc`; `{"flag": null}` goes back
/// to turning it on and off by hand, leaving it as it is.
pub async fn set_flag(
    caller: Authorised,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetFlag>,
) -> Result<Json<Value>, Problem> {
    exists(&state, &id).await?;
    let follows = plugins::switches::follow(&state, &id, body.flag.as_deref(), &caller.principal)
        .await
        .map_err(|err| refused(&id, &err))?;
    Ok(Json(json!({ "plugin": id, "follows": follows })))
}

pub async fn permissions(
    _: Authorised,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Problem> {
    let declared = state.repos.plugins.permissions(&id).await?;
    let named: Vec<Value> = declared
        .iter()
        .map(|permission| match permission.name.as_str() {
            "" => json!(format!("plugin:{id}:{}", permission.kind)),
            name => json!(format!("plugin:{id}:{}:{name}", permission.kind)),
        })
        .collect();
    Ok(Json(json!({ "plugin": id, "permissions": named })))
}

/// Unloads in a task of its own, so a caller hanging up cannot leave a plugin half unloaded.
async fn unload_detached(state: &AppState, id: &str, by: &Principal) -> Result<bool, Problem> {
    let (state, plugin, by) = (state.clone(), id.to_string(), by.clone());
    let task = tokio::spawn(async move {
        let outcome = plugins::unload(&state, &plugin).await;
        let detail = match &outcome {
            Ok(carried) => json!({ "carried": carried.is_some() }),
            Err(err) => json!({ "error": err.to_string() }),
        };
        plugins::audit(&state, &by, "plugin.unloaded", &plugin, detail).await;
        outcome.map(|carried| carried.is_some())
    });
    let outcome = task.await.map_err(|err| Problem::internal(err.to_string()))?;
    outcome.map_err(|err| refused(id, &err))
}

pub async fn reload(
    caller: Authorised,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Response, Problem> {
    registered(&state, &id).await?;
    let first = handover::reload(&state, &id).await.map_err(|err| refused(&id, &err))?;
    plugins::audit(&state, &caller.principal, "plugin.reload", &id, json!({ "first": first }))
        .await;
    Ok((StatusCode::ACCEPTED, Json(json!({ "plugin": id, "state": first }))).into_response())
}

pub async fn unload(
    caller: Authorised,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, Problem> {
    registered(&state, &id).await?;
    let carried = unload_detached(&state, &id, &caller.principal).await?;
    Ok(Json(json!({ "plugin": id, "unloaded": true, "carried": carried })))
}

/// §5's state machine, driven by an operator: `loading` reloads and `unloading` unloads.
pub async fn set_state(
    caller: Authorised,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<SetState>,
) -> Result<Response, Problem> {
    registered(&state, &id).await?;
    if state.plugins.handing_over(&id) {
        return Err(refused(&id, &TransitionError::Busy(id.clone())));
    }
    if body.state == PluginState::Running && state.plugins.is_off(&id) {
        return Err(refused(&id, &TransitionError::Off(id.clone())));
    }
    let by = &caller.principal;
    let (action, outcome) = match body.state {
        PluginState::Unloading => {
            let carried = unload_detached(&state, &id, by).await?;
            return Ok(Json(json!({ "plugin": id, "state": body.state, "carried": carried }))
                .into_response());
        }
        PluginState::Loading => ("plugin.reload", handover::reload(&state, &id).await),
        PluginState::Cancelled => {
            ("plugin.cancelled", plugins::cancel(&state, &id).await.map(|()| body.state))
        }
        PluginState::Running => {
            ("plugin.resumed", plugins::transition(&state, &id, body.state, None).await)
        }
        PluginState::Error => {
            ("plugin.marked-error", plugins::transition(&state, &id, body.state, None).await)
        }
    };
    let now = outcome.map_err(|err| refused(&id, &err))?;
    plugins::audit(&state, by, action, &id, json!({ "state": now })).await;
    Ok(Json(json!({ "plugin": id, "state": now })).into_response())
}

pub async fn list_tokens(
    _: Authorised,
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Vec<TokenView>>, Problem> {
    exists(&state, &id).await?;
    let owner = TokenOwner::Plugin(id);
    let tokens = state.repos.identity.list_tokens(&owner, TokenKind::PluginRegistration).await?;
    Ok(Json(tokens.into_iter().map(TokenView::from).collect()))
}

/// The secret is shown this once; the plugin's process needs it in its secrets to register.
pub async fn create_token(
    caller: Authorised,
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<TokenRequest>,
) -> Result<Response, Problem> {
    exists(&state, &id).await?;
    let expires_at = match body.expires_in_days {
        Some(days) if !(1..=MAX_TOKEN_DAYS).contains(&days) => {
            return Err(Problem::bad_request(format!(
                "expires_in_days must be between 1 and {MAX_TOKEN_DAYS}"
            )));
        }
        Some(days) => Some(Utc::now() + Duration::days(days)),
        None => None,
    };
    let name = body.name.as_deref().map(str::trim).filter(|name| !name.is_empty());
    if name.is_some_and(|name| name.chars().count() > MAX_NAME) {
        return Err(Problem::bad_request(format!("a name is at most {MAX_NAME} characters")));
    }
    let secret = generate_token(TokenKind::PluginRegistration)
        .map_err(|err| Problem::internal(format!("issuing a token: {err}")))?;
    let token = state
        .repos
        .identity
        .issue_token(NewToken {
            kind: TokenKind::PluginRegistration,
            token_hash: token_hash(secret.expose()),
            name: Some(name.map_or_else(|| format!("registration token for {id}"), str::to_string)),
            owner: TokenOwner::Plugin(id.clone()),
            expires_at,
            scopes: None,
            issued_by: None,
        })
        .await?;
    let detail = json!({ "token": token.id, "expires_at": token.expires_at });
    plugins::audit(&state, &caller.principal, "plugin.token.created", &id, detail).await;
    let created = TokenView::from(token);
    Ok((StatusCode::CREATED, Json(json!({ "token": secret.expose(), "created": created })))
        .into_response())
}

/// Takes effect at once, so the plugin's own calls stop working and it goes unreachable in turn.
pub async fn revoke_token(
    caller: Authorised,
    State(state): State<AppState>,
    Path((id, token)): Path<(String, Uuid)>,
) -> Result<StatusCode, Problem> {
    exists(&state, &id).await?;
    let owner = TokenOwner::Plugin(id.clone());
    let Some(hash) = state.repos.identity.revoke_token(token, &owner).await? else {
        return Err(Problem::not_found("token"));
    };
    forget(&state, &hash).await;
    plugins::audit(
        &state,
        &caller.principal,
        "plugin.token.revoked",
        &id,
        json!({ "token": token }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::Duration as StdDuration;

    use async_trait::async_trait;
    use doc_permissions::Grants;
    use doc_plugin_protocol::{Manifest, RegisterRequest};
    use http::Method;

    use super::*;
    use crate::api::router;
    use crate::db::repositories::{IdentityRepository, PluginRepository};
    use crate::permissions::PermissionSource;
    use crate::status::plugins::{PluginChange, PluginStatuses, Source};
    use crate::testing::{Host, delete_as, get, get_as, plugin_host, post_json, put_json, send};

    /// A service account holding `plugin:core:service:rw`, which is what an operator's CI has.
    const CI: &str = "doc_svc_ci";
    /// A service account holding `plugin:core:service:ro`.
    const READER: &str = "doc_svc_reader";
    /// A user holding `plugin:hello:user:rw` and nothing of core's.
    const ADA: &str = "doc_ses_ada";

    /// Answers by label, so one test can hold callers with different access.
    struct Held(BTreeMap<String, Value>);

    #[async_trait]
    impl PermissionSource for Held {
        async fn grants(&self, principal: &Principal, _teams: &[Uuid]) -> Result<Grants, String> {
            let held = self.0.get(&principal.label()).cloned().unwrap_or_else(|| json!({}));
            serde_json::from_value(held).map_err(|err| err.to_string())
        }
    }

    async fn host() -> Host {
        let mut host = plugin_host();
        for id in ["hello", "rbac"] {
            host.identity.register_plugin(id).await.expect("known");
        }
        for (name, token) in [("ci", CI), ("reader", READER)] {
            let account = host.identity.add_service_account(name);
            let owner = TokenOwner::ServiceAccount(account.id);
            host.identity.give(token, TokenKind::Service, owner, None, false);
        }
        let ada = host.identity.add_user("ada");
        host.identity.give(ADA, TokenKind::Session, TokenOwner::User(ada.id), None, false);
        let held = Held(BTreeMap::from([
            ("ci".to_string(), json!({ "permissions": ["plugin:core:service:rw"] })),
            ("reader".to_string(), json!({ "permissions": ["plugin:core:service:ro"] })),
            ("ada".to_string(), json!({ "permissions": ["plugin:hello:user:rw"] })),
        ]));
        host.state = host.state.clone().with_permissions(Arc::new(held));
        host.app = router(host.state.clone());
        host
    }

    fn hello() -> RegisterRequest {
        let manifest = Manifest {
            id: "hello".into(),
            version: "1.0.0".into(),
            nav: vec![Nav::new("Hello", "/")],
            ..Manifest::default()
        };
        RegisterRequest {
            manifest,
            address: "plugin-hello:4440".into(),
            binary_sha256: "a".repeat(64),
            started_at: None,
        }
    }

    async fn running(host: &Host) -> Uuid {
        let principal = host.as_plugin("hello");
        let response =
            plugins::register(&host.state, &principal, hello()).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        response.instance.expect("an instance")
    }

    async fn eventually<F: Fn() -> Fut, Fut: Future<Output = bool>>(what: &str, check: F) {
        for _ in 0..200 {
            if check().await {
                return;
            }
            tokio::time::sleep(StdDuration::from_millis(10)).await;
        }
        panic!("{what} did not happen within 2s");
    }

    async fn settled(host: &Host) {
        eventually("the reload finishing", || async {
            !host.state.plugins.handing_over("hello")
                && host.state_of("hello").await == Some(PluginState::Running)
        })
        .await;
    }

    /// What the plugin probe would record for hello at `at`.
    fn change(state: Option<PluginState>, error: Option<&str>, at: DateTime<Utc>) -> PluginChange {
        PluginChange {
            plugin: "hello".into(),
            version: "2.0.0".into(),
            classification: Classification::Async,
            instance: Uuid::nil(),
            state,
            error: error.map(str::to_string),
            since: at,
            at,
            registered_at: at - Duration::minutes(5),
        }
    }

    fn audited(host: &Host, action: &str) -> Vec<crate::identity::AuditEntry> {
        host.identity.audit_entries().into_iter().filter(|entry| entry.action == action).collect()
    }

    async fn call(host: &Host, method: Method, path: &str, token: &str) -> StatusCode {
        let request = http::Request::builder()
            .method(method)
            .uri(path)
            .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(json!({ "state": "cancelled" }).to_string()))
            .expect("request");
        send(&host.app, request).await.0
    }

    #[tokio::test]
    async fn every_plugin_is_listed_in_one_call_as_the_probe_recorded_it() {
        let host = host().await;
        running(&host).await;
        let at = crate::status::plugins::now();
        let stuck = change(Some(PluginState::Error), Some("timed out"), at);
        host.status.record(&stuck, Source::Event).await.expect("recorded");

        let (status, body, _) = get_as(&host.app, "/api/v1/plugins", READER).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["source"], "workers");
        let plugins = body["plugins"].as_array().expect("plugins");
        let ids: Vec<&str> = plugins.iter().filter_map(|plugin| plugin["id"].as_str()).collect();
        assert_eq!(ids, ["hello", "rbac"]);
        let hello = &plugins[0];
        assert_eq!(hello["version"], "2.0.0");
        assert_eq!(
            (hello["classification"].clone(), hello["state"].clone()),
            ("async".into(), "error".into())
        );
        assert_eq!(
            (hello["error"].clone(), hello["last_error"].clone()),
            ("timed out".into(), "timed out".into())
        );
        assert_eq!(hello["registered_at"], json!(stuck.registered_at));
        assert!(
            plugins[1]["state"].is_null() && plugins[1]["version"].is_null(),
            "never registered"
        );
    }

    #[tokio::test]
    async fn without_the_plugin_probe_the_list_comes_from_the_registry() {
        let host = host().await;
        running(&host).await;
        let (_, body, _) = get_as(&host.app, "/api/v1/plugins", READER).await;
        assert_eq!(body["source"], "live");
        let hello = &body["plugins"][0];
        assert_eq!(
            (hello["state"].clone(), hello["version"].clone()),
            ("running".into(), "1.0.0".into())
        );
    }

    #[tokio::test]
    async fn a_plugins_page_adds_its_history_and_its_registration() {
        let host = host().await;
        let instance = running(&host).await;
        let started = crate::status::plugins::now();
        let steps = [
            (0, Some(PluginState::Loading), None),
            (1, Some(PluginState::Running), None),
            (2, Some(PluginState::Error), Some("unreachable")),
            (3, None, None),
        ];
        for (seconds, state, error) in steps {
            let at = started + Duration::seconds(seconds);
            host.status.record(&change(state, error, at), Source::Event).await.expect("recorded");
        }

        let (status, body, _) = get_as(&host.app, "/api/v1/plugins/hello", READER).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["state"].is_null(), "it has left the registry, as the probe last saw it");
        assert_eq!(body["last_error"], "unreachable");
        let states: Vec<&str> = body["history"]
            .as_array()
            .expect("history")
            .iter()
            .filter_map(|row| row["state"].as_str())
            .collect();
        assert_eq!(states, ["removed", "error", "running", "loading"], "newest first");
        assert_eq!(body["history"][1]["error"], "unreachable");
        let registration = &body["registration"];
        assert_eq!(registration["instance"], json!(instance), "what the registry holds now");
        assert_eq!(
            (registration["busy"].clone(), registration["nav"][0]["label"].clone()),
            (json!(false), "Hello".into())
        );

        let (status, body, _) = get_as(&host.app, "/api/v1/plugins/rbac", READER).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["registration"].is_null());
        assert_eq!(body["history"], json!([]));
        let (status, _, _) = get_as(&host.app, "/api/v1/plugins/ghost", READER).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn reading_plugins_needs_core_read_access() {
        let host = host().await;
        running(&host).await;
        for path in ["/api/v1/plugins", "/api/v1/plugins/hello", "/api/v1/plugins/hello/tokens"] {
            assert_eq!(get(&host.app, path).await.0, StatusCode::UNAUTHORIZED, "{path}");
            let (status, _, _) = get_as(&host.app, path, ADA).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}: access to hello is not core's");
            assert_eq!(get_as(&host.app, path, READER).await.0, StatusCode::OK, "{path}");
        }
    }

    #[tokio::test]
    async fn managing_plugins_needs_core_write_access() {
        let host = host().await;
        running(&host).await;
        let token = Uuid::now_v7();
        let changes = [
            (Method::POST, "/api/v1/plugins/hello/reload".to_string()),
            (Method::POST, "/api/v1/plugins/hello/unload".to_string()),
            (Method::POST, "/api/v1/plugins/hello/state".to_string()),
            (Method::POST, "/api/v1/plugins/hello/tokens".to_string()),
            (Method::DELETE, format!("/api/v1/plugins/hello/tokens/{token}")),
        ];
        for caller in [READER, ADA] {
            for (method, path) in &changes {
                let status = call(&host, method.clone(), path, caller).await;
                assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path} as {caller}");
            }
        }
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running), "nothing moved");
        assert_eq!(
            host.identity.audit_actions().iter().filter(|a| a.starts_with("plugin.token")).count(),
            0
        );

        let status = call(&host, Method::POST, "/api/v1/plugins/hello/state", CI).await;
        assert_eq!(status, StatusCode::OK, "core write access may drive any plugin");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Cancelled));
        let cancelled = audited(&host, "plugin.cancelled");
        assert_eq!(cancelled[0].actor_label.as_deref(), Some("ci"));
    }

    /// A plugin somebody turns off is held `cancelled` and offered to nobody — out of everyone's
    /// navigation, its pages refused — stays off when it registers again, cannot be resumed
    /// while it is, and runs again once it is turned on.
    #[tokio::test]
    async fn a_plugin_turned_off_is_offered_to_nobody_until_it_is_turned_on() {
        let host = host().await;
        running(&host).await;
        let path = "/api/v1/plugins/hello/enabled";
        let off = json!({ "enabled": false });
        let refused = put_json(&host.app, path, READER, off.clone()).await.0;
        assert_eq!(refused, StatusCode::FORBIDDEN, "core readers cannot");
        let (status, body, _) = put_json(&host.app, path, CI, off).await;
        assert_eq!((status, body["state"].clone()), (StatusCode::OK, json!("cancelled")));
        assert_eq!(body["turned_off"]["by"], "ci");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Cancelled));
        assert_eq!(audited(&host, "plugin.turned-off")[0].actor_label.as_deref(), Some("ci"));

        let (_, access, _) = get_as(&host.app, "/api/v1/me/access", ADA).await;
        let nav = access["plugins"]["hello"]["nav"].as_array().cloned().unwrap_or_default();
        assert!(nav.is_empty(), "nobody is offered its navigation: {nav:?}");
        let (status, page, _) = get_as(&host.app, "/api/v1/plugins/hello/ui/", ADA).await;
        assert_eq!(
            (status, page["state"].clone()),
            (StatusCode::SERVICE_UNAVAILABLE, json!("turned off"))
        );
        let resume = json!({ "state": "running" });
        let (status, body, _) =
            post_json(&host.app, "/api/v1/plugins/hello/state", Some(CI), resume).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        plugins::register(&host.state, &host.as_plugin("hello"), hello())
            .await
            .expect("registered again");
        assert_eq!(host.settle("hello").await, Some(PluginState::Cancelled), "it comes back off");
        let (_, listed, _) = get_as(&host.app, "/api/v1/plugins", READER).await;
        let hello = listed["plugins"]
            .as_array()
            .and_then(|plugins| plugins.iter().find(|plugin| plugin["id"] == "hello").cloned())
            .expect("listed");
        assert_eq!(hello["turned_off"]["by"], "ci");

        let (status, body, _) = put_json(&host.app, path, CI, json!({ "enabled": true })).await;
        assert_eq!((status, body["state"].clone()), (StatusCode::OK, json!("running")));
        let (_, access, _) = get_as(&host.app, "/api/v1/me/access", ADA).await;
        assert_eq!(access["plugins"]["hello"]["nav"][0]["label"], "Hello");
        let switches = host.state.repos.plugins.plugin_switches().await.expect("read");
        assert!(switches.is_empty(), "turning it on forgets it was off");
    }

    /// A plugin that follows a flag is turned off and on as the flags plugin says the flag is for
    /// the platform, is the flag's alone to switch meanwhile, and is left as it is whenever the
    /// flag cannot be read.
    #[tokio::test]
    async fn a_plugin_that_follows_a_flag_is_turned_on_and_off_by_it() {
        use crate::plugins::client::Answer;
        use crate::plugins::switches::follow_flags;

        let host = host().await;
        host.identity.register_plugin("flags").await.expect("known");
        running(&host).await;
        let path = "/api/v1/plugins/hello/flag";
        let (status, body, _) =
            put_json(&host.app, path, CI, json!({ "flag": "Enable_Hello" })).await;
        assert_eq!(
            (status, body["follows"]["flag"].clone()),
            (StatusCode::OK, json!("enable_hello"))
        );
        assert_eq!(audited(&host, "plugin.follows-flag")[0].actor_label.as_deref(), Some("ci"));
        let off = json!({ "enabled": false });
        let (status, body, _) = put_json(&host.app, "/api/v1/plugins/hello/enabled", CI, off).await;
        assert_eq!(status, StatusCode::CONFLICT, "the flag decides: {body}");

        follow_flags(&host.state).await;
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running), "no flags plugin");
        let problem = host.state.plugins.following("hello").and_then(|held| held.problem);
        assert!(problem.unwrap_or_default().contains("could not be asked"));

        let mut flags = hello();
        flags.manifest.id = "flags".into();
        plugins::register(&host.state, &host.as_plugin("flags"), flags).await.expect("registered");
        assert_eq!(host.settle("flags").await, Some(PluginState::Running));
        let says =
            |on: Value| Answer::json(StatusCode::OK, &json!({ "flags": { "enable_hello": on } }));
        host.plugin.answer_with(says(json!(false)));
        follow_flags(&host.state).await;
        assert_eq!(host.state_of("hello").await, Some(PluginState::Cancelled));
        let by = host.state.plugins.turned_off("hello").and_then(|off| off.by);
        assert_eq!(by.as_deref(), Some("the flag enable_hello"));
        let (asked, caller) = host.plugin.requests().pop().expect("the flags plugin was asked");
        assert_eq!((asked.path.as_str(), caller.kind.as_str()), ("internal/platform", "platform"));

        host.plugin.answer_with(says(json!(true)));
        follow_flags(&host.state).await;
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
        host.plugin.answer_with(says(json!("yes")));
        follow_flags(&host.state).await;
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running), "not a switch");
        let problem = host.state.plugins.following("hello").and_then(|held| held.problem);
        assert!(problem.unwrap_or_default().contains("not true or false"));

        let (status, _, _) = put_json(&host.app, path, CI, json!({ "flag": null })).await;
        assert_eq!(status, StatusCode::OK);
        let off = json!({ "enabled": false });
        let status = put_json(&host.app, "/api/v1/plugins/hello/enabled", CI, off).await.0;
        assert_eq!(status, StatusCode::OK, "switched by hand again");
    }

    #[tokio::test]
    async fn a_plugin_that_decides_permissions_cannot_be_turned_off() {
        let host = host().await;
        let mut rbac = hello();
        rbac.manifest.id = "rbac".into();
        rbac.manifest.capabilities = vec![Capability::PermissionProvider];
        plugins::register(&host.state, &host.as_plugin("rbac"), rbac).await.expect("registered");
        assert_eq!(host.settle("rbac").await, Some(PluginState::Running));
        let off = json!({ "enabled": false });
        let (status, body, _) = put_json(&host.app, "/api/v1/plugins/rbac/enabled", CI, off).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(
            body["title"].as_str().unwrap_or_default().contains("decides permissions"),
            "{body}"
        );
        assert_eq!(host.state_of("rbac").await, Some(PluginState::Running));
    }

    /// T22 lets a plugin's own writers cancel it; core's writers may cancel any plugin.
    #[tokio::test]
    async fn core_write_access_can_cancel_any_plugin() {
        let host = host().await;
        running(&host).await;
        let path = "/api/v1/plugins/hello/cancel";
        assert_eq!(
            post_json(&host.app, path, Some(READER), json!({})).await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(post_json(&host.app, path, Some(CI), json!({})).await.0, StatusCode::OK);
        assert_eq!(host.state_of("hello").await, Some(PluginState::Cancelled));
        assert_eq!(audited(&host, "plugin.cancelled")[0].actor_label.as_deref(), Some("ci"));
    }

    #[tokio::test]
    async fn a_running_plugin_reloads_in_place_with_its_state() {
        let host = host().await;
        let instance = running(&host).await;
        host.plugin.set_carries(Some(json!({ "greeted": 3 })));

        let (status, body, _) =
            post_json(&host.app, "/api/v1/plugins/hello/reload", Some(CI), json!({})).await;
        assert_eq!((status, body["state"].clone()), (StatusCode::ACCEPTED, "unloading".into()));
        settled(&host).await;

        let entry = host.state.plugins.get("hello").await.expect("registered");
        assert_eq!(entry.instance, instance, "the same registration, loaded again");
        assert_eq!(host.plugin.unloaded.load(Ordering::SeqCst), 1);
        assert_eq!(host.plugin.loaded.load(Ordering::SeqCst), 2);
        assert_eq!(host.plugin.saw_previous(), Some(json!({ "greeted": 3 })));
        assert_eq!(host.plugin.exited.load(Ordering::SeqCst), 0, "a reload keeps its process");
        assert_eq!(audited(&host, "plugin.reload")[0].actor_label.as_deref(), Some("ci"));
    }

    #[tokio::test]
    async fn a_plugin_in_error_is_loaded_again_from_its_last_handover() {
        let host = host().await;
        running(&host).await;
        host.store.save_handover("hello", Some(&json!({ "greeted": 9 }))).await.expect("saved");
        plugins::transition(&host.state, "hello", PluginState::Error, Some("it fell over"))
            .await
            .expect("in error");

        let (status, body, _) =
            post_json(&host.app, "/api/v1/plugins/hello/reload", Some(CI), json!({})).await;
        assert_eq!((status, body["state"].clone()), (StatusCode::ACCEPTED, "loading".into()));
        settled(&host).await;
        assert_eq!(host.plugin.unloaded.load(Ordering::SeqCst), 0, "nothing to unload in error");
        assert_eq!(host.plugin.saw_previous(), Some(json!({ "greeted": 9 })));
        assert_eq!(host.error_of("hello").await, None);
    }

    #[tokio::test]
    async fn a_reload_needs_a_process_that_is_not_already_moving() {
        let host = host().await;
        let path = "/api/v1/plugins/rbac/reload";
        let (status, body, _) = post_json(&host.app, path, Some(CI), json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body["detail"].as_str().unwrap_or_default().contains("no process registered"));
        let (status, _, _) =
            post_json(&host.app, "/api/v1/plugins/ghost/reload", Some(CI), json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        host.plugin.slow_down(Some(StdDuration::from_millis(300)));
        plugins::register(&host.state, &host.as_plugin("hello"), hello())
            .await
            .expect("registered");
        let (status, body, _) =
            post_json(&host.app, "/api/v1/plugins/hello/reload", Some(CI), json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body["title"].as_str().unwrap_or_default().contains("already loading"), "{body}");
        assert!(audited(&host, "plugin.reload").is_empty(), "a refused reload did nothing");
    }

    #[tokio::test]
    async fn unloading_keeps_what_the_plugin_handed_over_and_is_audited() {
        let host = host().await;
        running(&host).await;
        host.plugin.set_carries(Some(json!({ "greeted": 4 })));
        let path = "/api/v1/plugins/hello/unload";

        let (status, body, _) = post_json(&host.app, path, Some(CI), json!({})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!((body["unloaded"].clone(), body["carried"].clone()), (json!(true), json!(true)));
        assert!(host.state_of("hello").await.is_none(), "it has left the registry");
        let saved = host.store.handover("hello").await.expect("read");
        assert_eq!(saved, Some(json!({ "greeted": 4 })));
        assert_eq!(audited(&host, "plugin.unloaded")[0].actor_label.as_deref(), Some("ci"));

        let (status, _, _) = post_json(&host.app, path, Some(CI), json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "nothing is registered any more");
    }

    async fn issue(host: &Host, body: Value) -> (StatusCode, Value) {
        let (status, body, _) =
            post_json(&host.app, "/api/v1/plugins/hello/tokens", Some(CI), body).await;
        (status, body)
    }

    #[tokio::test]
    async fn a_registration_token_is_shown_once_and_registers_the_plugin() {
        let host = host().await;
        let (status, body) =
            issue(&host, json!({ "name": "ci deploy", "expires_in_days": 30 })).await;
        assert_eq!(status, StatusCode::CREATED);
        let secret = body["token"].as_str().expect("the secret").to_string();
        assert!(secret.starts_with("doc_reg_"), "{secret}");
        let id = body["created"]["id"].clone();
        assert_eq!(body["created"]["name"], "ci deploy");
        assert!(body["created"]["expires_at"].is_string());

        let (status, listed, _) = get_as(&host.app, "/api/v1/plugins/hello/tokens", READER).await;
        assert_eq!(status, StatusCode::OK);
        assert!(listed.as_array().expect("tokens").iter().any(|token| token["id"] == id));
        assert!(!listed.to_string().contains(&secret), "a secret is never shown again");

        let principal = crate::auth::authenticate(&host.state, &secret).await.expect("it signs in");
        assert_eq!(principal.reference(), "plugin:hello");
        plugins::register(&host.state, &principal, hello()).await.expect("and registers hello");
        let created = audited(&host, "plugin.token.created");
        assert_eq!(
            (created[0].subject.as_deref(), created[0].actor_label.as_deref()),
            (Some("hello"), Some("ci"))
        );
    }

    #[tokio::test]
    async fn a_revoked_registration_token_stops_working_at_once() {
        let host = host().await;
        let (_, body) = issue(&host, json!({})).await;
        let secret = body["token"].as_str().expect("the secret").to_string();
        let id = body["created"]["id"].as_str().expect("an id").to_string();
        crate::auth::authenticate(&host.state, &secret).await.expect("it works, and is cached");

        let path = format!("/api/v1/plugins/hello/tokens/{id}");
        assert_eq!(delete_as(&host.app, &path, CI).await.0, StatusCode::NO_CONTENT);
        assert!(crate::auth::authenticate(&host.state, &secret).await.is_err(), "revoked at once");
        let (_, listed, _) = get_as(&host.app, "/api/v1/plugins/hello/tokens", READER).await;
        assert!(!listed.to_string().contains(&id), "a revoked token is not listed");
        assert_eq!(
            delete_as(&host.app, &path, CI).await.0,
            StatusCode::NOT_FOUND,
            "already revoked"
        );
        assert_eq!(audited(&host, "plugin.token.revoked").len(), 1);
    }

    #[tokio::test]
    async fn a_registration_token_belongs_to_one_known_plugin() {
        let host = host().await;
        let (status, _, _) =
            post_json(&host.app, "/api/v1/plugins/ghost/tokens", Some(CI), json!({})).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        let (_, body) = issue(&host, json!({})).await;
        assert_eq!(body["created"]["name"], "registration token for hello", "named by default");
        let id = body["created"]["id"].as_str().expect("an id").to_string();
        let path = format!("/api/v1/plugins/rbac/tokens/{id}");
        assert_eq!(delete_as(&host.app, &path, CI).await.0, StatusCode::NOT_FOUND, "not rbac's");

        for days in [0, MAX_TOKEN_DAYS + 1] {
            let (status, _) = issue(&host, json!({ "expires_in_days": days })).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{days} days");
        }
        let (status, _) = issue(&host, json!({ "name": "x".repeat(MAX_NAME + 1) })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
}
