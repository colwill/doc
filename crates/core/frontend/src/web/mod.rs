//! The UI: router, shared state, the pages and the error responses.

pub mod accounts;
pub mod admin;
pub mod assets;
pub mod audit;
pub mod auth;
pub mod categories;
pub mod client;
pub mod cookie;
pub mod csrf;
mod dashboard;
pub mod dev;
pub mod error;
pub mod health;
pub mod home;
mod landing;
pub mod me;
pub mod menu;
pub mod navigation;
pub mod pages;
pub mod plugin_settings;
pub mod plugins;
pub mod positions;
pub mod settings;
pub mod setup;
pub mod teams;
pub mod tokens;
pub mod users;

use std::sync::Arc;
use std::time::Instant;

use axum::Router;
use axum::middleware;
use axum::routing::{any, get, post};
use http::HeaderValue;
use http::header::{
    CONTENT_SECURITY_POLICY, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::set_header::SetResponseHeaderLayer;

use crate::backend::BackendClient;
use crate::config::Config;
use crate::fabric::Buses;
use csrf::CsrfKey;
use doc_immediate_tasks::Pool;
use error::WebError;

/// Only this site's own assets, nothing inline and nothing evaluated; HTMX 4's `hx-on` and `js:`
/// values need `unsafe-eval`, so they stay off.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; \
font-src 'self'; connect-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'";

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone)]
pub struct AppState {
    pub backend: BackendClient,
    pub buses: Buses,
    pub config: Arc<Config>,
    pub started: Instant,
    /// Backend calls a page waits on, bounded so a slow backend cannot pile requests up.
    pub immediate: Pool,
    pub csrf: CsrfKey,
    pub live: crate::live::Hub,
}

impl AppState {
    pub fn new(config: Config, backend: BackendClient, buses: Buses) -> Self {
        let csrf = CsrfKey::from_secret(config.backend.token.expose());
        let instance = std::env::var("HOSTNAME").unwrap_or_else(|_| "frontend".into());
        let live = crate::live::Hub::start(&buses.events, &instance);
        Self {
            backend,
            buses,
            config: Arc::new(config),
            started: Instant::now(),
            immediate: Pool::default(),
            csrf,
            live,
        }
    }
}

/// Pages, sessions and plugin routes are added to this router by T31 and T32.
pub fn router(state: AppState) -> Router {
    dev::set_enabled(state.config.dev.reload);
    pages::set_configured_instance(&state.config.instance.name);
    let mut router = Router::new()
        .route("/healthz", get(health::healthz))
        .route("/", get(home::home))
        .route("/dashboard", get(dashboard::choose).post(dashboard::save))
        .route("/dashboard/access-requests", get(dashboard::access_requests))
        .route("/sections", get(dashboard::sections))
        .route("/design", get(pages::design))
        .route("/search", get(home::search))
        .route("/search/options", get(home::search_options))
        .route("/people/options", get(users::options))
        .route("/status", get(pages::status))
        .route("/status/panel", get(pages::status_panel))
        .route("/status/charts", get(pages::status_charts))
        .route("/events", get(crate::live::events))
        .route("/sign-in", get(auth::sign_in))
        .route("/sign-in/{provider}", post(auth::password_sign_in))
        .route("/sign-in/{provider}/password", post(auth::choose_password))
        .route("/sign-out", post(auth::sign_out))
        .route("/auth/{provider}/start", get(auth::oauth_start))
        .route("/auth/{provider}/callback", get(auth::oauth_callback))
        .route("/account", get(me::show))
        .route("/account/accounts", get(me::accounts))
        .route("/account/links", get(me::links))
        .route("/account/links/{provider}", post(me::link))
        .route("/account/identities/{id}/unlink", post(me::unlink))
        .route("/welcome", get(me::welcome))
        .route("/users", get(users::list).post(users::create))
        .route("/users/new", get(users::new))
        .route("/users/{id}", get(users::show))
        .route("/users/{id}/identities", get(users::identities).post(users::attach))
        .route("/users/{id}/identities/new", get(users::link_form))
        .route("/users/{id}/teams", get(users::teams))
        .route("/users/{id}/identities/{identity}/unlink", post(users::detach))
        .route("/users/{id}/merge", get(users::merge_form).post(users::merge))
        .route("/users/{id}/disabled", post(users::set_disabled))
        .route("/users/{id}/organisation", get(users::move_form).post(users::move_to))
        .route("/tokens", get(tokens::list).post(tokens::create))
        .route("/tokens/new", get(tokens::new))
        .route("/tokens/{id}/revoke", post(tokens::revoke))
        .route("/teams", get(teams::list).post(teams::create_team))
        .route("/teams/new", get(teams::new_team))
        .route("/teams/{id}", get(teams::team).post(teams::update_team))
        .route("/teams/{id}/edit", get(teams::edit_team))
        .route("/teams/{id}/delete", post(teams::delete_team))
        .route("/teams/{id}/members", get(teams::team_people).post(teams::add_member))
        .route("/teams/{id}/members/new", get(teams::new_member))
        .route("/teams/{id}/people", post(teams::add_person))
        .route("/teams/{id}/teams", get(teams::team_teams))
        .route("/teams/{id}/service-accounts", get(teams::team_accounts))
        .route("/teams/{id}/members/{user}/remove", post(teams::remove_member))
        .route("/teams/{id}/members/{user}/position", post(positions::set_member_position))
        .route("/teams/{id}/lead", get(teams::lead_form).post(positions::set_lead))
        .route(
            "/teams/{id}/positions",
            get(teams::team_positions).post(positions::create_team_position),
        )
        .route("/teams/{id}/positions/new", get(positions::new_team_position))
        .route(
            "/teams/{id}/positions/{name}",
            get(positions::edit_team_position).post(positions::update_team_position),
        )
        .route("/teams/{id}/positions/{name}/revert", post(positions::revert_team_position))
        .route("/organisations", get(teams::organisations).post(teams::create_organisation))
        .route("/organisations/new", get(teams::new_organisation))
        .route("/organisations/{id}", get(teams::organisation).post(teams::update_organisation))
        .route("/organisations/{id}/edit", get(teams::edit_organisation))
        .route("/organisations/{id}/delete", post(teams::delete_organisation))
        .route(
            "/organisations/{id}/providers",
            get(teams::providers_form).post(teams::set_providers),
        )
        .route("/organisations/{id}/sign-in", get(teams::organisation_signing_in))
        .route("/organisations/{id}/teams", get(teams::organisation_teams))
        .route(
            "/organisations/{id}/positions",
            get(teams::organisation_positions).post(positions::create_organisation_position),
        )
        .route("/organisations/{id}/positions/new", get(positions::new_organisation_position))
        .route(
            "/organisations/{id}/positions/{name}",
            get(positions::edit_organisation_position)
                .post(positions::update_organisation_position),
        )
        .route(
            "/organisations/{id}/positions/{name}/delete",
            post(positions::delete_organisation_position),
        )
        .route("/service-accounts", get(accounts::list).post(accounts::create))
        .route("/service-accounts/new", get(accounts::new))
        .route("/service-accounts/{id}/owner", get(accounts::owner_form).post(accounts::set_owner))
        .route("/service-accounts/{id}", get(accounts::show))
        .route("/service-accounts/{id}/disabled", post(accounts::set_disabled))
        .route("/service-accounts/{id}/tokens", get(accounts::tokens).post(accounts::create_token))
        .route("/service-accounts/{id}/tokens/new", get(accounts::token_form))
        .route("/service-accounts/{id}/tokens/{token}/revoke", post(accounts::revoke_token))
        .route(
            "/service-accounts/{id}/permissions",
            get(accounts::permissions).post(accounts::grant),
        )
        .route("/service-accounts/{id}/permissions/new", get(accounts::permission_form))
        .route("/service-accounts/{id}/permissions/revoke", post(accounts::revoke))
        .route("/audit", get(audit::show))
        .route("/navigation", get(navigation::show).post(navigation::save))
        .route("/settings", get(settings::show).post(settings::save))
        .route("/setup", get(setup::overview))
        .route("/setup/{step}", get(setup::show).post(setup::save))
        .route("/menu/{slug}", get(menu::show))
        .route("/landing", get(landing::show).post(landing::save))
        .route("/plugins", get(admin::list))
        .route("/plugins/panel", get(admin::list_panel))
        .route("/plugins/{id}", get(admin::show))
        .route("/plugins/{id}/settings", get(plugin_settings::show).post(plugin_settings::save))
        .route("/plugins/{id}/settings/test", post(plugin_settings::test))
        .route("/plugins/{id}/settings/requests/{request}/{verdict}", post(plugin_settings::decide))
        .route(
            "/plugins/{id}/features",
            get(plugin_settings::features).post(plugin_settings::set_features),
        )
        .route("/plugins/{id}/permissions", get(plugin_settings::permissions))
        .route("/plugins/{id}/tokens", get(admin::tokens).post(admin::create_token))
        .route("/plugins/{id}/tokens/new", get(admin::new_token))
        .route("/plugins/{id}/tokens/{token}/revoke", post(admin::revoke_token))
        .route("/plugins/{id}/flag", get(admin::follow_page).post(admin::follow))
        .route("/plugins/{id}/flag/options", get(admin::flag_options))
        .route("/plugins/{id}/{action}", post(admin::act))
        .route("/p/{plugin}", any(plugins::root))
        .route("/p/{plugin}/", any(plugins::root))
        .route("/p/{plugin}/{*path}", any(plugins::below))
        .route("/assets/{*path}", get(assets::asset));
    if state.config.dev.reload {
        tracing::warn!("development reload is on: /dev/boot is served and pages poll it");
        router = router.route("/dev/boot", get(dev::boot_id));
    }
    // Overriding, so no handler or forwarded answer can weaken them.
    let header =
        |name, value| SetResponseHeaderLayer::overriding(name, HeaderValue::from_static(value));
    router
        .fallback(not_found)
        .layer(middleware::from_fn_with_state(state.clone(), csrf::guard))
        .layer(header(CONTENT_SECURITY_POLICY, CSP))
        .layer(header(X_FRAME_OPTIONS, "DENY"))
        .layer(header(X_CONTENT_TYPE_OPTIONS, "nosniff"))
        .layer(header(REFERRER_POLICY, "same-origin"))
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(middleware::from_fn(client::forwarded))
        // Outside the CSRF guard and the routes: a plugin's own name is answered before anything
        // else looks at the request, whatever the method or the path.
        .layer(middleware::from_fn(plugins::by_host))
        .layer(middleware::from_fn(doc_telemetry::traced))
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
        .with_state(state)
}

async fn not_found() -> WebError {
    WebError::NotFound
}
