//! The HTTP API: router, shared state and the health endpoints.

pub mod audit;
#[cfg(test)]
mod audited;
pub mod auth;
pub mod dashboard;
pub mod events;
pub mod health;
pub mod iam;
#[cfg(test)]
mod leaks;
pub mod navigation;
pub mod people;
pub mod plugin_admin;
pub mod plugin_settings;
pub mod plugins;
pub mod positions;
pub mod problem;
pub mod settings;
pub mod setup;
pub mod status;
pub mod tasks;
pub mod teams;
pub mod users;

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::routing::{any, delete, get, patch, post, put};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};

use crate::config::Config;
use crate::db::repositories::Repositories;
use crate::fabric::Buses;
use crate::permissions::PermissionSource;
use crate::plugins::Registry;
use doc_immediate_tasks::Pool as ImmediatePool;
use problem::Problem;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Synchronous plugin runs in flight at once, across every plugin.
const PLUGIN_RUNS: usize = 64;

#[derive(Clone)]
pub struct AppState {
    pub repos: Repositories,
    pub buses: Buses,
    pub config: Arc<Config>,
    pub started: Instant,
    /// `None` until the RBAC plugin can be asked, which leaves bootstrap permissions the only ones.
    pub permissions: Option<Arc<dyn PermissionSource>>,
    /// Work a request waits on, bounded so a slow answer cannot pile up unboundedly.
    pub immediate: ImmediatePool,
    /// Synchronous plugin runs, which get the plugins' own request deadline rather than the pool's.
    pub plugin_runs: ImmediatePool,
    /// The plugins the backend is holding a connection to (T20).
    pub plugins: Registry,
    pub limits: Arc<crate::limits::Limits>,
    /// The keys stored plugin secrets are encrypted under (ADR-0007). Empty until the bootstrap
    /// has made one, which is how a platform with no key says so rather than storing anything
    /// in the clear, and read again when a rotation writes one beside a serving backend.
    pub settings_keys: Arc<crate::secrets::SettingsKeyring>,
}

impl AppState {
    pub fn new(config: Config, repos: Repositories, buses: Buses) -> Self {
        let plugin_runs = ImmediatePool::new(PLUGIN_RUNS, config.plugins.request_deadline());
        let limits = crate::limits::Limits::new(&config.limits, &config.server.trusted_proxies);
        // A platform whose volume has no key yet keeps running: everything but storing a secret
        // works, and the Settings page says what is missing.
        let settings_keys = crate::secrets::SettingsKeyring::load(&config.secrets.dir)
            .unwrap_or_else(|err| {
                tracing::warn!(%err, "the settings key could not be read; secrets cannot be stored");
                crate::secrets::SettingsKeyring::empty()
            });
        Self {
            repos,
            buses,
            config: Arc::new(config),
            started: Instant::now(),
            permissions: None,
            immediate: ImmediatePool::default(),
            plugin_runs,
            plugins: Registry::default(),
            limits: Arc::new(limits),
            settings_keys: Arc::new(settings_keys),
        }
    }

    pub fn with_permissions(mut self, source: Arc<dyn PermissionSource>) -> Self {
        self.permissions = Some(source);
        self
    }

    /// A settings key that lives only in this process, for a test with no secrets volume.
    #[cfg(test)]
    pub fn with_settings_key(mut self) -> Self {
        self.settings_keys = Arc::new(crate::secrets::SettingsKeyring::in_memory());
        self
    }

    pub fn with_plugins(mut self, registry: Registry) -> Self {
        self.plugins = registry;
        self
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(health::healthz))
        .route("/readyz", get(health::readyz))
        .nest("/api/v1", api_v1())
        .fallback(not_found)
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(axum::middleware::from_fn(doc_telemetry::traced))
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .with_state(state)
}

/// Endpoints are added to this router by later tasks; the platform serves it from the start.
fn api_v1() -> Router<AppState> {
    Router::new()
        .route("/status", get(status::status))
        .route("/status/history", get(status::history))
        .route("/status/certificates", get(status::certificates))
        .route("/me", get(auth::me))
        .route("/audit", get(audit::list))
        .route("/events/topics", get(events::topics))
        .route("/auth/providers", get(auth::providers))
        .route("/auth/logout", post(auth::logout))
        .route("/tokens", get(auth::list_personal_tokens).post(auth::create_personal_token))
        .route("/tokens/scoped", get(auth::list_scoped_tokens).post(auth::create_scoped_token))
        .route("/tokens/{id}", delete(auth::revoke_personal_token))
        .route("/service-accounts", get(iam::list_accounts).post(iam::create_account))
        .route("/service-accounts/{id}", get(iam::show_account).patch(iam::set_account_disabled))
        .route(
            "/service-accounts/{id}/tokens",
            get(iam::list_account_tokens).post(iam::create_account_token),
        )
        .route("/service-accounts/{id}/owner", put(iam::set_account_owner))
        .route("/service-accounts/{id}/tokens/{token}", delete(iam::revoke_account_token))
        .route(
            "/service-accounts/{id}/permissions",
            get(iam::account_permissions).post(iam::grant_account_permission),
        )
        .route(
            "/service-accounts/{id}/permissions/{permission}",
            delete(iam::revoke_account_permission),
        )
        .route("/users", get(users::list).post(users::create))
        .route("/people", post(people::create))
        .route("/users/{id}", get(users::show).patch(users::update))
        .route("/users/{id}/identities", post(users::attach))
        .route("/users/{id}/identities/{identity}", delete(users::detach))
        .route("/users/{id}/merge", post(users::merge))
        .route("/organisations", get(teams::list_organisations).post(teams::create_organisation))
        .route(
            "/organisations/{id}",
            get(teams::show_organisation)
                .patch(teams::update_organisation)
                .delete(teams::delete_organisation),
        )
        .route("/organisations/{id}/providers", put(teams::set_providers))
        .route("/organisations/{id}/domains", put(teams::set_domains))
        .route("/organisations/{id}/positions", get(positions::list_organisation_positions))
        .route(
            "/organisations/{id}/positions/{name}",
            put(positions::put_organisation_position)
                .delete(positions::delete_organisation_position),
        )
        .route("/teams", get(teams::list_teams).post(teams::create_team))
        .route(
            "/teams/{id}",
            get(teams::show_team).patch(teams::update_team).delete(teams::delete_team),
        )
        .route("/teams/{id}/members", post(teams::add_member))
        .route("/teams/{id}/members/{user}", delete(teams::remove_member))
        .route("/teams/{id}/members/{user}/position", put(positions::set_member_position))
        .route("/teams/{id}/lead", put(positions::set_lead))
        .route("/teams/{id}/positions", get(positions::list_team_positions))
        .route(
            "/teams/{id}/positions/{name}",
            put(positions::put_team_position).delete(positions::delete_team_position),
        )
        .route("/me/access", get(tasks::my_access))
        .route("/me/access-requests", get(dashboard::waiting))
        .route("/me/dashboard", get(dashboard::show).put(dashboard::set))
        .route("/me/identities", get(users::mine))
        .route("/me/identities/{identity}", delete(users::unlink_mine))
        .route("/me/links", post(users::start_link))
        .route("/navigation", get(navigation::show).put(navigation::set))
        .route("/settings", get(settings::show).put(settings::set))
        .route("/setup", get(setup::show).patch(setup::change))
        .route("/tasks", get(tasks::list_tasks).post(tasks::start_task))
        .route("/tasks/{id}", get(tasks::show_task))
        .route("/tasks/{id}/cancel", post(tasks::cancel_task))
        .route("/task-kinds/{kind}", patch(tasks::set_kind_paused))
        .route("/cron", get(tasks::list_cron))
        .route("/cron/{name}", patch(tasks::set_cron_paused))
        .route("/plugins", get(plugin_admin::list))
        .route("/plugins/{id}", get(plugin_admin::show))
        .route("/plugins/{id}/permissions", get(plugin_admin::permissions))
        .route("/plugins/{id}/state", post(plugin_admin::set_state))
        .route("/plugins/{id}/enabled", put(plugin_admin::set_enabled))
        .route("/plugins/{id}/flag", put(plugin_admin::set_flag))
        .route("/plugins/{id}/reload", post(plugin_admin::reload))
        .route("/plugins/{id}/unload", post(plugin_admin::unload))
        .route(
            "/plugins/{id}/tokens",
            get(plugin_admin::list_tokens).post(plugin_admin::create_token),
        )
        .route("/plugins/{id}/tokens/{token}", delete(plugin_admin::revoke_token))
        .route("/plugins/{id}/settings", get(plugin_settings::show).put(plugin_settings::save))
        .route("/plugins/{id}/settings/check", post(plugin_settings::check))
        .route("/plugins/{id}/settings/permissions", get(plugin_settings::permissions_tab))
        .route("/plugins/{id}/features", put(plugin_settings::set_features))
        .route("/plugins/{id}/enable", post(plugin_settings::enable))
        .route(
            "/plugins/{id}/access-requests/{request}/{verdict}",
            post(plugin_settings::decide_access),
        )
        .route("/plugins/{id}/run", post(plugins::run))
        .route("/plugins/{id}/cancel", post(plugins::cancel))
        .route("/plugins/{id}/{*route}", any(plugins::forward))
}

async fn not_found() -> Problem {
    Problem::not_found("endpoint")
}

#[cfg(test)]
mod tests {
    use crate::testing::{get, test_app};
    use http::StatusCode;

    #[tokio::test]
    async fn unknown_paths_return_problem_details() {
        let (app, _) = test_app();
        let (status, body, content_type) = get(&app, "/api/v1/nope").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(content_type.as_deref(), Some("application/problem+json"));
        assert_eq!(body["title"], "endpoint not found");
    }
}
