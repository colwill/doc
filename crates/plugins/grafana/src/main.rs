//! Grafana: dashboards drawn in DOC from their own queries. Grafana is reached through a proxied
//! vendor account in Secret Storage, as this plugin itself, so the plugin holds no credential
//! and Secret Storage records every call it makes. A dashboard given services shows on theirs.

mod api;
mod dashboard;
mod frames;
mod geomap;
mod grafana;
mod settings;
mod ui;
mod view;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, Manifest, Nav, Plugin, PluginError, Request, ResourcePanel, Response,
    RunInput, RunOutput, Settings, SettingsVerdict,
};
use serde_json::Value;

use grafana::Grafana;
use settings::{ACCOUNT, Config};

pub const ID: &str = "grafana";

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

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self { status: 503, detail: detail.into() }
    }

    pub fn response(&self) -> Response {
        let kind = match self.status {
            400 => "bad-request",
            401 => "unauthorized",
            403 => "forbidden",
            404 => "not-found",
            502 => "bad-gateway",
            _ => "unavailable",
        };
        Response::problem(self.status, kind, &self.detail)
    }
}

/// One query parameter, decoded.
pub fn parameter(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Default)]
struct GrafanaPlugin;

#[async_trait]
impl Plugin for GrafanaPlugin {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let config = Config::read(&backend.settings());
        tracing::info!(
            version = backend.version(),
            connected = config.account.is_some(),
            dashboards = config.dashboards.len(),
            "grafana loaded"
        );
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, _backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        Err(PluginError::from(
            "grafana has nothing to run: it draws dashboards when they are opened",
        ))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    /// Tries the vendor account before it is stored, so a refusal lands on its field.
    async fn settings_check(&self, backend: &Backend, proposed: &Settings) -> SettingsVerdict {
        let config = Config::read(proposed);
        let Some(account) = config.account.as_deref() else { return SettingsVerdict::ok() };
        match Grafana::new(backend, account).reaches().await {
            Ok(()) => SettingsVerdict::saying(&format!(
                "Reached Grafana through the {account} account in Secret Storage"
            )),
            Err(refusal) => SettingsVerdict::wrong(ACCOUNT, &refusal.detail),
        }
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let path = request.path.trim_end_matches('/').to_string();
        let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        match segments.as_slice() {
            ["ui", route @ ..] => ui::handle(backend, &request, route).await,
            ["api", route @ ..] => api::handle(backend, &request, route).await,
            _ => Refusal::missing("no such route").response(),
        }
    }
}

doc_plugin_sdk::main!(
    GrafanaPlugin,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("Grafana", "/")
                .described(
                    "Grafana dashboards drawn in DOC from their own queries, through a vendor \
                     account in Secret Storage",
                )
                .grouped("Platform"),
        ],
        resource_panels: vec![ResourcePanel::new("service", "Grafana dashboards", "/panel")],
        settings: settings::declared(),
        ..Manifest::default()
    }
);
