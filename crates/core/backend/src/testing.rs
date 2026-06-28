//! Endpoint test harness: a router built on the in-memory fakes, driven with `oneshot`.

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::response::Response;
use http::{Request, StatusCode};
use serde_json::Value;
use tower::ServiceExt;

use crate::api::{AppState, router};
use crate::config::Config;
use crate::data::memory::MemoryData;
use crate::db::memory::{FakeHealth, FakeIdentity, FakePluginStatus, FakePlugins};
use crate::db::repositories::Repositories;
use crate::fabric::Buses;
use crate::identity::TokenOwner;
use crate::secrets::TokenKind;
use doc_background_tasks::store::MemoryTasks;
use doc_cron_tasks::MemoryCron;

/// Every app in a test gets the in-memory task stores, so a handler that reaches for one works.
pub fn repositories(health: Arc<FakeHealth>, identity: Arc<FakeIdentity>) -> Repositories {
    let data = MemoryData::new();
    repositories_with(health, identity, FakePlugins::empty(), FakePluginStatus::empty(), data)
}

pub fn repositories_with(
    health: Arc<FakeHealth>,
    identity: Arc<FakeIdentity>,
    plugins: Arc<FakePlugins>,
    plugin_status: Arc<FakePluginStatus>,
    data: Arc<MemoryData>,
) -> Repositories {
    let (tasks, cron) = (MemoryTasks::new(), MemoryCron::new());
    let status_history = crate::db::memory::FakeStatusHistory::empty();
    let teams = identity.clone();
    Repositories {
        health,
        identity,
        teams,
        plugins,
        plugin_status,
        tasks,
        cron,
        status_history,
        data,
    }
}

pub fn test_app() -> (Router, Arc<FakeHealth>) {
    test_app_with(Config::default())
}

pub fn test_app_with(config: Config) -> (Router, Arc<FakeHealth>) {
    let database = FakeHealth::up();
    let repos = repositories(database.clone(), FakeIdentity::empty());
    (router(AppState::new(config, repos, Buses::in_memory())), database)
}

pub fn identity_app(config: Config, identity: Arc<FakeIdentity>) -> Router {
    let repos = repositories(FakeHealth::up(), identity);
    router(AppState::new(config, repos, Buses::in_memory()))
}

/// Core routes are guarded from T15, so a test that wants past one arrives as a bootstrap admin —
/// the only grant that applies while no permission source is configured.
pub const ADMIN: &str = "doc_ses_admin";

pub fn admin_app() -> (Router, Arc<FakeHealth>) {
    let (app, health, _) = admin_app_parts();
    (app, health)
}

/// Also hands back the buses, so a test can write what a probe would have written.
pub fn admin_app_parts() -> (Router, Arc<FakeHealth>, Buses) {
    let mut config = Config::default();
    config.bootstrap.admins = vec!["tester".into()];
    let database = FakeHealth::up();
    let identity = FakeIdentity::empty();
    let user = identity.add_user("tester");
    identity.give(ADMIN, TokenKind::Session, TokenOwner::User(user.id), None, false);
    let repos = repositories(database.clone(), identity);
    let buses = Buses::in_memory();
    (router(AppState::new(config, repos, buses.clone())), database, buses)
}

pub async fn get_as(app: &Router, path: &str, token: &str) -> (StatusCode, Value, Option<String>) {
    let request = Request::builder()
        .uri(path)
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request");
    send(app, request).await
}

pub async fn post_json(
    app: &Router,
    path: &str,
    token: Option<&str>,
    body: Value,
) -> (StatusCode, Value, Option<String>) {
    let mut builder = Request::builder()
        .method(http::Method::POST)
        .uri(path)
        .header(http::header::CONTENT_TYPE, "application/json");
    if let Some(token) = token {
        builder = builder.header(http::header::AUTHORIZATION, format!("Bearer {token}"));
    }
    let request = builder.body(Body::from(body.to_string())).expect("request");
    send(app, request).await
}

pub async fn patch_json(
    app: &Router,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value, Option<String>) {
    let request = Request::builder()
        .method(http::Method::PATCH)
        .uri(path)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .expect("request");
    send(app, request).await
}

pub async fn put_json(
    app: &Router,
    path: &str,
    token: &str,
    body: Value,
) -> (StatusCode, Value, Option<String>) {
    let request = Request::builder()
        .method(http::Method::PUT)
        .uri(path)
        .header(http::header::CONTENT_TYPE, "application/json")
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::from(body.to_string()))
        .expect("request");
    send(app, request).await
}

pub async fn delete_as(
    app: &Router,
    path: &str,
    token: &str,
) -> (StatusCode, Value, Option<String>) {
    let request = Request::builder()
        .method(http::Method::DELETE)
        .uri(path)
        .header(http::header::AUTHORIZATION, format!("Bearer {token}"))
        .body(Body::empty())
        .expect("request");
    send(app, request).await
}

pub async fn body_json(response: Response) -> Value {
    let bytes = axum::body::to_bytes(response.into_body(), 4 * 1024 * 1024)
        .await
        .expect("reading the response body");
    if bytes.is_empty() {
        return Value::Null;
    }
    serde_json::from_slice(&bytes).expect("response body is JSON")
}

pub async fn get(app: &Router, path: &str) -> (StatusCode, Value, Option<String>) {
    let request = Request::builder().uri(path).body(Body::empty()).expect("request");
    send(app, request).await
}

pub async fn send(app: &Router, request: Request<Body>) -> (StatusCode, Value, Option<String>) {
    let response = app.clone().oneshot(request).await.expect("router responds");
    let status = response.status();
    let content_type = response
        .headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.split(';').next().unwrap_or(v).trim().to_string());
    (status, body_json(response).await, content_type)
}

/// Everything a plugin-host test drives: the state the registry lives in, the fake process it
/// registers, and the stores it and the plugin probe write to.
pub struct Host {
    pub state: AppState,
    pub app: Router,
    pub plugin: Arc<crate::plugins::client::FakePlugin>,
    pub connector: Arc<crate::plugins::client::FakeConnector>,
    pub store: Arc<FakePlugins>,
    pub identity: Arc<FakeIdentity>,
    pub status: Arc<FakePluginStatus>,
    pub data: Arc<MemoryData>,
    /// Stands in for the RBAC plugin, so a test can give somebody a permission and see it work.
    pub grants: Arc<FakeGrants>,
}

pub fn plugin_host() -> Host {
    let mut config = Config::default();
    config.bootstrap.admins = vec!["tester".into()];
    config.plugins.ids = vec!["hello".into(), "rbac".into()];
    config
        .plugins
        .capabilities
        .insert("rbac".into(), vec![doc_plugin_protocol::Capability::PermissionProvider]);
    plugin_host_with(config)
}

pub fn plugin_host_with(config: Config) -> Host {
    use crate::plugins::Registry;
    use crate::plugins::client::{FakeConnector, FakePlugin};

    let identity = FakeIdentity::empty();
    let user = identity.add_user("tester");
    identity.give(ADMIN, TokenKind::Session, TokenOwner::User(user.id), None, false);
    for id in &config.plugins.ids {
        let token = format!("doc_reg_{id}");
        identity.give(
            &token,
            TokenKind::PluginRegistration,
            TokenOwner::Plugin(id.clone()),
            None,
            false,
        );
    }

    let (store, status) = (FakePlugins::empty(), FakePluginStatus::empty());
    let plugin = FakePlugin::new();
    let connector = FakeConnector::new(plugin.clone());
    let data = MemoryData::new();
    let repos = repositories_with(
        FakeHealth::up(),
        identity.clone(),
        store.clone(),
        status.clone(),
        data.clone(),
    );
    let grants = Arc::new(FakeGrants::default());
    let state = AppState::new(config, repos, Buses::in_memory())
        .with_plugins(Registry::new(connector.clone()))
        .with_permissions(grants.clone())
        .with_settings_key();
    let app = router(state.clone());
    Host { state, app, plugin, connector, store, identity, status, data, grants }
}

impl Host {
    /// The same stores behind a fresh registry and buses, which is what a backend restart leaves.
    pub fn restarted(&self) -> Host {
        use crate::plugins::Registry;
        use crate::plugins::client::{FakeConnector, FakePlugin};

        let plugin = FakePlugin::new();
        let connector = FakeConnector::new(plugin.clone());
        let (store, identity, status, data) =
            (self.store.clone(), self.identity.clone(), self.status.clone(), self.data.clone());
        let repos = repositories_with(
            FakeHealth::up(),
            identity.clone(),
            store.clone(),
            status.clone(),
            data.clone(),
        );
        let config = (*self.state.config).clone();
        let grants = self.grants.clone();
        let state = AppState::new(config, repos, Buses::in_memory())
            .with_plugins(Registry::new(connector.clone()))
            .with_permissions(grants.clone())
            .with_settings_key();
        let app = router(state.clone());
        Host { state, app, plugin, connector, store, identity, status, data, grants }
    }

    /// The principal a plugin's registration token authenticates as.
    pub fn as_plugin(&self, id: &str) -> crate::identity::Principal {
        crate::identity::Principal::Plugin { id: id.to_string() }
    }

    /// Waits for the `load` that registration spawns to have finished one way or the other.
    pub async fn settle(&self, id: &str) -> Option<doc_plugin_protocol::PluginState> {
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            match self.state.plugins.get(id).await {
                Some(entry) if entry.state != doc_plugin_protocol::PluginState::Loading => {
                    return Some(entry.state);
                }
                Some(_) => continue,
                None => return None,
            }
        }
        self.state.plugins.get(id).await.map(|entry| entry.state)
    }

    pub async fn state_of(&self, id: &str) -> Option<doc_plugin_protocol::PluginState> {
        self.state.plugins.get(id).await.map(|entry| entry.state)
    }

    pub async fn error_of(&self, id: &str) -> Option<String> {
        self.state.plugins.get(id).await.and_then(|entry| entry.error)
    }

    /// Registers a manifest and waits for it to be running, which is what a test that is about
    /// something else — settings, say — needs a plugin for.
    pub async fn register(&self, manifest: doc_plugin_protocol::Manifest) {
        let id = manifest.id.clone();
        let request = doc_plugin_protocol::RegisterRequest {
            manifest,
            address: format!("plugin-{id}:4440"),
            binary_sha256: "a".repeat(64),
            started_at: None,
        };
        crate::plugins::register(&self.state, &self.as_plugin(&id), request)
            .await
            .expect("registered");
        assert_eq!(
            self.settle(&id).await,
            Some(doc_plugin_protocol::PluginState::Running),
            "{id} did not come up"
        );
    }

    /// Somebody who holds exactly these permissions, and the session token they arrive with.
    pub async fn user_holding(&self, login: &str, permissions: &[&str]) -> String {
        let user = self.identity.add_user(login);
        let token = format!("doc_ses_{login}");
        self.identity.give(&token, TokenKind::Session, TokenOwner::User(user.id), None, false);
        self.grants.give(&user.id.to_string(), permissions);
        crate::permissions::forget_all(&self.state).await;
        token
    }

    /// The cron schedules recorded for a plugin, which is how a feature being off is visible.
    pub async fn schedules(&self) -> Vec<String> {
        let recorded = self.state.repos.cron.list().await.expect("listed");
        recorded
            .into_iter()
            .map(|task| task.name)
            .filter(|name| name.starts_with("plugin."))
            .collect()
    }
}

/// A manifest with nothing in it but an ID and a version, for a test about something else.
pub fn manifest(id: &str, version: &str) -> doc_plugin_protocol::Manifest {
    doc_plugin_protocol::Manifest {
        id: id.into(),
        version: version.into(),
        ..doc_plugin_protocol::Manifest::default()
    }
}

/// What a test hands out in place of the RBAC plugin: permissions by principal, as written.
#[derive(Default)]
pub struct FakeGrants(parking_lot::Mutex<std::collections::BTreeMap<String, Vec<String>>>);

impl FakeGrants {
    pub fn give(&self, principal_id: &str, permissions: &[&str]) {
        let held = permissions.iter().map(|held| (*held).to_string()).collect();
        self.0.lock().insert(principal_id.to_string(), held);
    }
}

#[async_trait::async_trait]
impl crate::permissions::PermissionSource for FakeGrants {
    async fn grants(
        &self,
        principal: &crate::identity::Principal,
        _teams: &[uuid::Uuid],
    ) -> Result<doc_permissions::Grants, String> {
        let id = match principal {
            crate::identity::Principal::User(user) => user.id.to_string(),
            crate::identity::Principal::ServiceAccount(account) => account.id.to_string(),
            crate::identity::Principal::Plugin { id } => id.clone(),
        };
        let held = self.0.lock().get(&id).cloned().unwrap_or_default();
        serde_json::from_value(serde_json::json!({ "permissions": held }))
            .map_err(|err| err.to_string())
    }
}

/// Registers `id` as a running identity provider that the fake's organisation signs in with. The
/// host's configuration must allow it the capability.
pub async fn identity_provider(host: &Host, id: &str) {
    use crate::db::repositories::TeamRepository;
    use doc_plugin_protocol::{Capability, Manifest, PluginState, RegisterRequest};
    let manifest = Manifest {
        id: id.into(),
        version: "1.0.0".into(),
        capabilities: vec![Capability::IdentityProvider],
        ..Manifest::default()
    };
    let request = RegisterRequest {
        manifest,
        address: format!("plugin-{id}:4440"),
        binary_sha256: "a".repeat(64),
        started_at: None,
    };
    crate::plugins::register(&host.state, &host.as_plugin(id), request).await.expect("registered");
    assert_eq!(host.settle(id).await, Some(PluginState::Running));
    let organisation = host.identity.organisation();
    let mut chosen: Vec<String> = host
        .identity
        .providers()
        .await
        .expect("providers")
        .into_iter()
        .filter(|(_, by)| *by == organisation)
        .map(|(provider, _)| provider)
        .collect();
    chosen.push(id.to_string());
    host.identity.set_providers(organisation, &chosen).await.expect("chosen");
}

/// Signs someone in through an identity provider, as its sign-in callback would.
pub async fn sign_in(
    host: &Host,
    provider: &str,
    external_id: &str,
    login: &str,
) -> doc_plugin_protocol::calls::IdentityResponse {
    let request = doc_plugin_protocol::calls::IdentityRequest {
        provider: provider.into(),
        external_id: external_id.into(),
        login: login.into(),
        ..Default::default()
    };
    crate::plugins::api::identity(&host.state, provider, request).await.expect("signed in")
}
