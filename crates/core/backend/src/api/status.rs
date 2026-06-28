//! `GET /api/v1/status`: what the platform knows about itself — the database, each bus and its
//! nodes, and every plugin. The probes in `doc-workers` write the answers and this serves them;
//! with no worker running it falls back to checking live, so the page is never simply blank.

use std::collections::BTreeMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
use doc_plugin_protocol::{Classification, PluginState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::problem::Problem;
use super::{AppState, VERSION};

const MAX_HISTORY_HOURS: i64 = 48;
use crate::permissions::Authorised;
use crate::plugins::Registered;
use crate::status::plugins::{PluginStatus, health};
use crate::status::{Component, Health, TTL, bus_state, bus_summary, node_status, worst};

#[derive(Debug, Serialize)]
pub struct Status {
    pub state: Health,
    pub version: &'static str,
    pub uptime_s: u64,
    /// The newest check among the components, so a page can say how fresh the whole answer is.
    pub checked_at: DateTime<Utc>,
    /// `workers` when the probes wrote these, `live` when the backend checked for itself.
    pub source: &'static str,
    pub components: Vec<Component>,
    /// `workers` when the plugin probe wrote these, `live` when they come from the registry.
    pub plugins_source: &'static str,
    pub plugins: Vec<PluginEntry>,
}

#[derive(Debug, Serialize)]
pub struct PluginEntry {
    pub id: String,
    pub state: Health,
    /// §5's lifecycle state, or none when no process is registered for this plugin.
    pub lifecycle: Option<PluginState>,
    pub error: Option<String>,
    pub version: Option<String>,
    pub classification: Option<Classification>,
    /// When it entered its lifecycle state, or left the registry.
    pub since: Option<DateTime<Utc>>,
    pub registered_at: Option<DateTime<Utc>>,
    /// The most recent error, which outlives the plugin recovering from it.
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    pub checked_at: Option<DateTime<Utc>>,
}

impl PluginEntry {
    fn of(id: String, row: Option<PluginStatus>) -> Self {
        let Some(row) = row else {
            return Self {
                id,
                state: Health::Unknown,
                lifecycle: None,
                error: None,
                version: None,
                classification: None,
                since: None,
                registered_at: None,
                last_error: None,
                last_error_at: None,
                checked_at: None,
            };
        };
        Self {
            id,
            state: health(row.state),
            lifecycle: row.state,
            error: row.error,
            version: Some(row.version),
            classification: Some(row.classification),
            since: Some(row.since),
            registered_at: Some(row.registered_at),
            last_error: row.last_error,
            last_error_at: row.last_error_at,
            checked_at: Some(row.checked_at),
        }
    }
}

/// Every known plugin: as the plugin probe recorded it while it keeps up, else from the registry.
pub(super) async fn known_plugins(
    state: &AppState,
) -> (Vec<(String, Option<PluginStatus>)>, &'static str) {
    let known = state.repos.identity.list_plugins().await.unwrap_or_default();
    let recorded = state.repos.plugin_status.current().await.unwrap_or_default();
    let fresh = Utc::now() - chrono::Duration::from_std(TTL).unwrap_or_default();
    let (rows, source) = if recorded.iter().any(|row| row.checked_at > fresh) {
        (recorded, "workers")
    } else {
        (state.plugins.list().await.iter().map(Registered::status).collect(), "live")
    };
    let mut plugins: BTreeMap<String, Option<PluginStatus>> =
        known.into_iter().map(|id| (id, None)).collect();
    for row in rows {
        plugins.insert(row.plugin.clone(), Some(row));
    }
    (plugins.into_iter().collect(), source)
}

async fn plugins(state: &AppState) -> (Vec<PluginEntry>, &'static str) {
    let (known, source) = known_plugins(state).await;
    (known.into_iter().map(|(id, row)| PluginEntry::of(id, row)).collect(), source)
}

/// The components the probes are expected to write, which is also the order the page shows them in.
fn expected() -> Vec<String> {
    ["postgres", "eventbus", "servicebus", "cachebus", "backend", "frontend"]
        .map(str::to_string)
        .to_vec()
}

async fn database(state: &AppState) -> Component {
    let started = std::time::Instant::now();
    let component = match state.repos.health.ping().await {
        Ok(()) => Component::new("database", "postgres", Health::Up),
        Err(err) => Component::new("database", "postgres", Health::Down).detail(err.to_string()),
    };
    component.took(started.elapsed())
}

async fn buses(state: &AppState) -> Vec<Component> {
    if state.buses.clusters.is_empty() {
        return ["eventbus", "servicebus", "cachebus"]
            .into_iter()
            .map(|name| Component::new("bus", name, Health::Up).detail("running in process"))
            .collect();
    }
    let mut components = Vec::new();
    for (name, client) in &state.buses.clusters {
        let started = std::time::Instant::now();
        let nodes: Vec<_> = client
            .metrics()
            .await
            .into_iter()
            .map(|(address, result)| node_status(address, result.map_err(|err| err.to_string())))
            .collect();
        components.push(
            Component::new("bus", name, bus_state(&nodes))
                .detail(bus_summary(&nodes))
                .with_nodes(nodes)
                .took(started.elapsed()),
        );
    }
    components
}

async fn live(state: &AppState) -> Vec<Component> {
    let mut components = vec![database(state).await];
    components.extend(buses(state).await);
    components
}

#[derive(Debug, Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub hours: Option<i64>,
}

/// Every check in the last few hours (6 unless asked, at most 48), which the dashboard charts.
pub async fn history(
    _: Authorised,
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Value>, Problem> {
    let hours = query.hours.unwrap_or(6).clamp(1, MAX_HISTORY_HOURS);
    let since = Utc::now() - chrono::Duration::hours(hours);
    let checks = state.repos.status_history.since(since).await.map_err(|err| {
        tracing::warn!(%err, "the status history could not be read");
        Problem::unavailable("the status history could not be read")
    })?;
    Ok(Json(json!({ "since": since, "checks": checks })))
}

/// Each certificate in the secrets volume and when it expires; `renew` means the next bootstrap
/// run replaces it, and `expired` that a service using it can no longer be reached.
pub async fn certificates(_: Authorised, State(state): State<AppState>) -> Json<Value> {
    let dir = &state.config.secrets.dir;
    let mut found = vec![("ca".to_string(), dir.join("ca/ca.pem"))];
    if let Ok(entries) = std::fs::read_dir(dir.join("certs")) {
        let mut certs: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|extension| extension == "pem"))
            .collect();
        certs.sort();
        found.extend(
            certs
                .into_iter()
                .filter_map(|path| Some((path.file_stem()?.to_string_lossy().into_owned(), path))),
        );
    }
    let now = Utc::now();
    let renew = crate::bootstrap::RENEW_WITHIN_DAYS;
    let certificates: Vec<Value> = found
        .into_iter()
        .filter(|(_, path)| path.exists())
        .map(|(name, path)| match crate::bootstrap::expiry(&path) {
            Ok(expires_at) => {
                let days_left = (expires_at - now).num_days();
                let state = match days_left {
                    left if expires_at <= now || left < 0 => "expired",
                    left if left < renew => "renew",
                    _ => "ok",
                };
                json!({ "name": name, "expires_at": expires_at, "days_left": days_left, "state": state })
            }
            Err(err) => json!({ "name": name, "state": "unreadable", "error": err.to_string() }),
        })
        .collect();
    Json(json!({ "renew_within_days": renew, "certificates": certificates }))
}

pub async fn status(_: Authorised, State(state): State<AppState>) -> Response {
    let stored = crate::status::load(state.buses.cache.as_ref(), &expected()).await;
    let (components, source) =
        if stored.is_empty() { (live(&state).await, "live") } else { (stored, "workers") };
    let checked_at =
        components.iter().map(|component| component.checked_at).max().unwrap_or_else(Utc::now);
    let (plugins, plugins_source) = plugins(&state).await;
    Json(Status {
        state: worst(&components),
        version: VERSION,
        uptime_s: state.started.elapsed().as_secs(),
        checked_at,
        source,
        components,
        plugins_source,
        plugins,
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::IdentityRepository;
    use crate::identity::Principal;
    use crate::status::plugins::{PluginChange, PluginStatuses, Source};
    use crate::status::store;
    use crate::testing::{ADMIN, Host, admin_app, admin_app_parts, get, get_as, plugin_host};
    use doc_plugin_protocol::{Manifest, RegisterRequest};
    use http::StatusCode;
    use serde_json::Value;
    use uuid::Uuid;

    #[tokio::test]
    async fn status_reports_the_database_and_every_bus() {
        let (app, _) = admin_app();
        let (status, body, _) = get_as(&app, "/api/v1/status", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "up");
        assert_eq!(body["source"], "live", "no worker has written anything yet");
        let components = body["components"].as_array().expect("components");
        assert_eq!(components.len(), 4);
        assert_eq!(components[0]["kind"], "database");
        assert_eq!(components[0]["state"], "up");
        let buses: Vec<&str> =
            components[1..].iter().map(|c| c["name"].as_str().expect("name")).collect();
        assert_eq!(buses, ["eventbus", "servicebus", "cachebus"]);
    }

    #[tokio::test]
    async fn the_history_the_dashboard_charts_is_for_core_readers() {
        use crate::db::memory::{FakeHealth, FakeIdentity, FakeStatusHistory};
        use crate::identity::TokenOwner;
        use crate::secrets::TokenKind;
        use crate::status::StatusHistory;
        let mut config = crate::config::Config::default();
        config.bootstrap.admins = vec!["tester".into()];
        let identity = FakeIdentity::empty();
        let tester = identity.add_user("tester");
        identity.give(ADMIN, TokenKind::Session, TokenOwner::User(tester.id), None, false);
        let plain = identity.add_user("plain");
        identity.give("doc_ses_plain", TokenKind::Session, TokenOwner::User(plain.id), None, false);
        let mut repos = crate::testing::repositories(FakeHealth::up(), identity);
        let history = FakeStatusHistory::empty();
        repos.status_history = history.clone();
        let buses = crate::fabric::Buses::in_memory();
        let app = crate::api::router(AppState::new(config, repos, buses));
        let check = |hours_ago: i64, latency: u64| Component {
            kind: "database".into(),
            name: "postgres".into(),
            state: Health::Up,
            detail: None,
            nodes: Vec::new(),
            checked_at: Utc::now() - chrono::Duration::hours(hours_ago),
            latency_ms: Some(latency),
        };
        history.record(&check(10, 9)).await.expect("recorded");
        history.record(&check(1, 3)).await.expect("recorded");

        let (status, body, _) = get_as(&app, "/api/v1/status/history", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        let checks = body["checks"].as_array().expect("checks");
        assert_eq!(checks.len(), 1, "six hours unless asked");
        assert_eq!(checks[0]["latency_ms"], 3);
        let (_, body, _) = get_as(&app, "/api/v1/status/history?hours=12", ADMIN).await;
        assert_eq!(body["checks"].as_array().expect("checks").len(), 2);
        let (status, _, _) = get_as(&app, "/api/v1/status/history", "doc_ses_plain").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn status_reports_a_stopped_database_without_failing() {
        let (app, database) = admin_app();
        database.set_down(true);
        let (status, body, _) = get_as(&app, "/api/v1/status", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["state"], "down");
        assert_eq!(body["components"][0]["state"], "down");
        assert_eq!(body["components"][0]["detail"], "storage unavailable: connection refused");
    }

    async fn hello_running(host: &Host) {
        let manifest =
            Manifest { id: "hello".into(), version: "1.0.0".into(), ..Manifest::default() };
        let request = RegisterRequest {
            manifest,
            address: "127.0.0.1:4440".into(),
            binary_sha256: "a".repeat(64),
            started_at: None,
        };
        let hello = Principal::Plugin { id: "hello".into() };
        crate::plugins::register(&host.state, &hello, request).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
    }

    async fn known(ids: &[&str]) -> Host {
        let host = plugin_host();
        for id in ids {
            host.identity.register_plugin(id).await.expect("known");
        }
        host
    }

    fn listed(body: &Value, id: &str) -> Value {
        let plugins = body["plugins"].as_array().expect("plugins").clone();
        plugins.into_iter().find(|plugin| plugin["id"] == id).expect(id)
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
            registered_at: at - chrono::Duration::minutes(5),
        }
    }

    /// What the registry holds, not what the bootstrap listed: a plugin with no process is unknown,
    /// a running one is up, and one in error is down with its reason.
    #[tokio::test]
    async fn each_plugin_shows_its_live_state() {
        let host = known(&["hello", "rbac"]).await;
        hello_running(&host).await;

        let (_, body, _) = get_as(&host.app, "/api/v1/status", ADMIN).await;
        assert_eq!(body["plugins_source"], "live", "no plugin probe has recorded anything");
        let hello = listed(&body, "hello");
        assert_eq!(
            (hello["state"].clone(), hello["lifecycle"].clone()),
            ("up".into(), "running".into())
        );
        assert_eq!(
            (hello["version"].clone(), hello["classification"].clone()),
            ("1.0.0".into(), "synchronous".into())
        );
        assert!(hello["since"].is_string() && hello["registered_at"].is_string());
        assert_eq!(listed(&body, "rbac")["state"], "unknown");
        assert!(listed(&body, "rbac")["lifecycle"].is_null(), "no process has registered");

        crate::plugins::transition(&host.state, "hello", PluginState::Error, Some("boom"))
            .await
            .unwrap();
        let (_, body, _) = get_as(&host.app, "/api/v1/status", ADMIN).await;
        assert_eq!(listed(&body, "hello")["state"], "down");
        assert_eq!(listed(&body, "hello")["error"], "boom");
    }

    /// Like a component, a plugin is served as its probe last recorded it, ahead of a live look.
    #[tokio::test]
    async fn plugins_are_served_as_the_plugin_probe_recorded_them() {
        let host = known(&["hello", "rbac"]).await;
        hello_running(&host).await;
        let at = crate::status::plugins::now();
        let stuck = change(Some(PluginState::Error), Some("timed out"), at);
        host.status.record(&stuck, Source::Event).await.expect("recorded");

        let (status, body, _) = get_as(&host.app, "/api/v1/status", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["plugins_source"], "workers");
        let hello = listed(&body, "hello");
        assert_eq!(
            (hello["state"].clone(), hello["lifecycle"].clone()),
            ("down".into(), "error".into())
        );
        assert_eq!(hello["error"], "timed out");
        assert_eq!(
            (hello["version"].clone(), hello["classification"].clone()),
            ("2.0.0".into(), "async".into())
        );
        assert_eq!(hello["since"], serde_json::to_value(at).expect("a time"));
        assert_eq!(
            hello["registered_at"],
            serde_json::to_value(stuck.registered_at).expect("a time")
        );
        assert_eq!(hello["last_error"], "timed out");
        assert!(hello["checked_at"].is_string());
        let rbac = listed(&body, "rbac");
        assert_eq!(
            (rbac["state"].clone(), rbac["lifecycle"].clone()),
            ("unknown".into(), Value::Null)
        );
        assert!(rbac["version"].is_null(), "the probe has never seen rbac");
    }

    #[tokio::test]
    async fn a_plugin_probe_that_has_stopped_leaves_the_registry_to_answer() {
        let host = known(&["hello"]).await;
        hello_running(&host).await;
        let stale =
            change(Some(PluginState::Error), Some("long ago"), crate::status::plugins::now());
        host.status.record(&stale, Source::Event).await.expect("recorded");
        host.status.checked(Utc::now() - chrono::Duration::minutes(10)).await.expect("checked");

        let (_, body, _) = get_as(&host.app, "/api/v1/status", ADMIN).await;
        assert_eq!(body["plugins_source"], "live");
        let hello = listed(&body, "hello");
        assert_eq!(
            (hello["state"].clone(), hello["lifecycle"].clone()),
            ("up".into(), "running".into())
        );
        assert_eq!(hello["version"], "1.0.0", "the registry's version, not the stale row's");
    }

    #[tokio::test]
    async fn the_last_error_outlives_recovery_and_leaving_the_registry() {
        let host = known(&["hello"]).await;
        let failed = crate::status::plugins::now() - chrono::Duration::minutes(3);
        let (recovered, removed) =
            (failed + chrono::Duration::minutes(1), failed + chrono::Duration::minutes(2));
        for recorded in [
            change(Some(PluginState::Error), Some("load failed: boom"), failed),
            change(Some(PluginState::Running), None, recovered),
        ] {
            host.status.record(&recorded, Source::Event).await.expect("recorded");
        }
        let (_, body, _) = get_as(&host.app, "/api/v1/status", ADMIN).await;
        let hello = listed(&body, "hello");
        assert_eq!((hello["state"].clone(), hello["error"].clone()), ("up".into(), Value::Null));
        assert_eq!(hello["last_error"], "load failed: boom");
        assert_eq!(hello["last_error_at"], serde_json::to_value(failed).expect("a time"));

        host.status.record(&change(None, None, removed), Source::Event).await.expect("recorded");
        let (_, body, _) = get_as(&host.app, "/api/v1/status", ADMIN).await;
        let hello = listed(&body, "hello");
        assert_eq!(
            (hello["state"].clone(), hello["lifecycle"].clone()),
            ("unknown".into(), Value::Null)
        );
        assert_eq!(hello["since"], serde_json::to_value(removed).expect("a time"), "when it left");
        assert_eq!(
            (hello["version"].clone(), hello["last_error"].clone()),
            ("2.0.0".into(), "load failed: boom".into())
        );
    }

    /// Events can arrive twice or late, so a change older than the one recorded is history only.
    #[tokio::test]
    async fn a_late_change_does_not_replace_a_newer_one() {
        let host = known(&["hello"]).await;
        let loading = crate::status::plugins::now();
        let running =
            change(Some(PluginState::Running), None, loading + chrono::Duration::seconds(1));
        host.status.record(&running, Source::Event).await.expect("recorded");
        let late = change(Some(PluginState::Loading), None, loading);
        for _ in 0..2 {
            host.status.record(&late, Source::Event).await.expect("recorded");
        }

        let (_, body, _) = get_as(&host.app, "/api/v1/status", ADMIN).await;
        assert_eq!(listed(&body, "hello")["lifecycle"], "running");
        let kept: Vec<Option<PluginState>> =
            host.status.history().iter().map(|(recorded, _)| recorded.state).collect();
        assert_eq!(kept, [Some(PluginState::Running), Some(PluginState::Loading)], "each once");
    }

    #[tokio::test]
    async fn status_lists_every_registered_plugin() {
        let (app, _) = admin_app();
        let (_, body, _) = get_as(&app, "/api/v1/status", ADMIN).await;
        assert_eq!(body["plugins"].as_array().expect("plugins").len(), 0);
    }

    /// What the probes write is what the API serves, without checking anything itself.
    #[tokio::test]
    async fn a_stored_check_is_served_in_place_of_a_live_one() {
        let (app, database, buses) = admin_app_parts();
        database.set_down(true);
        let component = Component::new("database", "postgres", Health::Up).detail("probed");
        store(buses.cache.as_ref(), &component).await.expect("stored");

        let (status, body, _) = get_as(&app, "/api/v1/status", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["source"], "workers");
        assert_eq!(body["components"].as_array().expect("components").len(), 1);
        assert_eq!(body["components"][0]["detail"], "probed");
        assert_eq!(body["state"], "up", "the stored answer decides, not a live ping");
    }

    #[tokio::test]
    async fn the_overall_state_is_the_worst_component() {
        let (app, _, buses) = admin_app_parts();
        for (name, health) in
            [("postgres", Health::Up), ("eventbus", Health::Degraded), ("servicebus", Health::Up)]
        {
            let kind = if name == "postgres" { "database" } else { "bus" };
            store(buses.cache.as_ref(), &Component::new(kind, name, health)).await.expect("stored");
        }
        let (_, body, _) = get_as(&app, "/api/v1/status", ADMIN).await;
        assert_eq!(body["state"], "degraded");
    }

    #[tokio::test]
    async fn status_is_guarded_by_the_platform_read_permission() {
        let (app, _) = admin_app();
        let (status, _, _) = get(&app, "/api/v1/status").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "no token at all");
        let (status, body, _) = get_as(&app, "/api/v1/status", "doc_ses_nosuch").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "an unknown token");
        assert_eq!(body["title"], "Authentication required");
    }

    #[tokio::test]
    async fn certificates_near_their_expiry_are_marked_for_renewal() {
        use chrono::Datelike;
        let dir = tempfile::tempdir().expect("secrets dir");
        let mut config = crate::config::Config::default();
        config.bootstrap.admins = vec!["tester".into()];
        config.secrets.dir = dir.path().to_path_buf();
        crate::bootstrap::run(&config, dir.path()).expect("bootstrap");

        let soon = Utc::now() + chrono::Duration::days(10);
        let key = rcgen::KeyPair::generate().expect("key");
        let mut params =
            rcgen::CertificateParams::new(vec!["frontend".to_string()]).expect("params");
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(soon.year(), soon.month() as u8, soon.day() as u8);
        let cert = params.self_signed(&key).expect("certificate");
        std::fs::write(dir.path().join("certs/frontend.pem"), cert.pem()).expect("written");

        let identity = crate::db::memory::FakeIdentity::empty();
        let user = identity.add_user("tester");
        let owner = crate::identity::TokenOwner::User(user.id);
        identity.give(ADMIN, crate::secrets::TokenKind::Session, owner, None, false);
        let app = crate::testing::identity_app(config, identity);
        let (status, body, _) = get_as(&app, "/api/v1/status/certificates", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        let state_of = |name: &str| {
            body["certificates"]
                .as_array()
                .and_then(|all| all.iter().find(|cert| cert["name"] == name))
                .map(|cert| cert["state"].clone())
        };
        assert_eq!(state_of("frontend"), Some(json!("renew")), "{body}");
        assert_eq!(state_of("backend"), Some(json!("ok")));
        assert_eq!(state_of("ca"), Some(json!("ok")));
        let (status, _, _) = get(&app, "/api/v1/status/certificates").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
