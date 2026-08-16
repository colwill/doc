//! DNS: DOC as the name server for the domains it owns. It answers questions about those domains
//! from records kept on its page, and forwards questions about every other name to the upstream
//! DNS servers in its settings, for the networks allowed to ask.
//!
//! The server runs in this process while the plugin is loaded and its feature is on; changes to
//! records are published as `plugin.dns.record.added`, `.changed` and `.removed`, and audited.
//!
//! The same questions are answered over DOC's own HTTPS (RFC 8484) when **DNS over HTTPS** is on,
//! which needs no port and no certificate of its own: see `doh`.

mod api;
mod doh;
mod guide;
mod names;
mod proxied;
mod server;
mod services;
mod settings;
mod store;
mod ui;
mod zone;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Capability, Classification, Event, Manifest, Nav, Plugin, PluginError, Request,
    Response, RunInput, RunOutput, Schedule, Settings, SettingsVerdict,
};
use serde_json::Value;

use server::Server;

pub const ID: &str = "dns";

#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub detail: String,
}

impl Refusal {
    pub fn bad(detail: impl Into<String>) -> Self {
        Self { status: 400, detail: detail.into() }
    }

    pub fn missing(detail: impl Into<String>) -> Self {
        Self { status: 404, detail: detail.into() }
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self { status: 409, detail: detail.into() }
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self { status: 503, detail: detail.into() }
    }

    pub fn response(&self) -> Response {
        let kind = match self.status {
            400 => "bad-request",
            403 => "forbidden",
            404 => "not-found",
            409 => "conflict",
            _ => "unavailable",
        };
        Response::problem(self.status, kind, &self.detail)
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        match err {
            PluginError::Message(detail) => Self::bad(detail),
            err => match err.problem() {
                Some((status, _)) if status < 500 => Self { status, detail: err.detail() },
                _ => Self::unavailable(err.to_string()),
            },
        }
    }
}

#[derive(Default)]
struct Dns {
    server: Server,
}

#[async_trait]
impl Plugin for Dns {
    /// Starts the server, which a settings change does again: the default reload unloads and
    /// loads, so the new address, domains and upstreams all take effect together.
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        self.server.start(backend).await;
        // The developer guide says what these settings are, so it is written again after every
        // load, which every settings change brings; in a run of its own, since the Knowledge Base
        // may still be starting.
        if let Err(err) = backend.task(serde_json::json!({ "guide": "load" })).await {
            tracing::warn!(%err, "the developer guide was not queued");
        }
        tracing::info!(version = backend.version(), "dns loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        self.server.stop().await;
        Ok(None)
    }

    /// The developer guide, after a load or on its schedule: the server itself answers while the
    /// plugin is loaded, with nothing to run.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let asked = input.payload["guide"].is_string()
            || input.payload["schedule"].as_str() == Some(guide::SCHEDULE);
        if !asked {
            return Err(PluginError::from("dns runs nothing but its developer guide"));
        }
        let said = guide::publish(backend).await.map_err(PluginError::from)?;
        Ok(RunOutput { payload: serde_json::json!({ "guide": said }) })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let path = request.path.trim_end_matches('/').to_string();
        let segments: Vec<&str> = path.split('/').collect();
        match segments.as_slice() {
            // Signed in, so a name outside DOC's domains may be forwarded for them.
            ["api", "dns-query"] => doh::handle(&self.server, &request, true).await,
            // Nobody signed in: DOC's own domains only, never forwarded (see `doh`).
            ["public", "dns-query"] => doh::handle(&self.server, &request, false).await,
            ["ui", route @ ..] => ui::handle(backend, &self.server, &request, route).await,
            ["api", route @ ..] => api::handle(backend, &self.server, &request, route).await,
            _ => Refusal::missing("no such route").response(),
        }
    }

    /// Infra saying a resource is standing, or has gone: a service's name follows it.
    async fn on_event(&self, backend: &Backend, event: Event) -> Result<(), PluginError> {
        services::heard(backend, &self.server, &event).await;
        proxied::heard(backend, &self.server, &event).await;
        Ok(())
    }

    async fn settings_check(&self, _backend: &Backend, proposed: &Settings) -> SettingsVerdict {
        settings::check(proposed)
    }
}

doc_plugin_sdk::main!(
    Dns,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("DNS", "/")
                .described("The names DOC answers for, and where it sends the rest")
                .grouped("Admin"),
        ],
        settings: settings::declared(),
        features: settings::features(),
        schedules: vec![Schedule::new(
            guide::SCHEDULE,
            "41 * * * *",
            "Writes how developers use DOC's DNS into the Knowledge Base, when it says something new",
        )],
        data: store::declaration(),
        subscriptions: vec![
            services::ACTIVE.into(),
            services::DELETED.into(),
            proxied::HOSTED.into(),
            proxied::UNHOSTED.into(),
        ],
        // A question sent with POST only reads, so it is not a write to this plugin.
        read_routes: vec!["api/dns-query".into()],
        // The resolver anyone may use is declared only where the deployment asked for it: a
        // capability the configuration does not allow refuses registration outright, and a name
        // server that will not start is a worse thing than one without this route.
        capabilities: match settings::public_resolver() {
            true => vec![Capability::PublicRoutes],
            false => Vec::new(),
        },
        public_routes: match settings::public_resolver() {
            true => vec!["dns-query".into()],
            false => Vec::new(),
        },
        ..Manifest::default()
    }
);
