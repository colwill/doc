//! The delivery roadmap: every release planned in Jira — each version of a project, with its
//! start and release dates and the issues in it — for every service, team and organisation in the
//! Catalogue, and whether the services in each are ready to ship: their pipelines, reliability,
//! end of life and deployments, asked of the plugins that measure them as whoever is looking.

mod api;
mod faux;
mod given;
mod mcp;
mod plan;
mod readiness;
mod scope;
mod settings;
mod timeline;
mod ui;
mod view;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, DashboardItem, Insight, Manifest, Nav, Plugin, PluginError, Request,
    ResourcePanel, Response, RunInput, RunOutput,
};
use serde_json::Value;

use settings::ROADMAP;

pub const ID: &str = "roadmap";

#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub detail: String,
}

impl Refusal {
    pub fn bad(detail: impl Into<String>) -> Self {
        Self { status: 400, detail: detail.into() }
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self { status: 403, detail: detail.into() }
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
            403 => "forbidden",
            404 => "not-found",
            _ => "unavailable",
        };
        Response::problem(self.status, kind, &self.detail)
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        match err {
            PluginError::Forbidden(permission) => {
                Self::forbidden(format!("that needs {permission}:rw"))
            }
            PluginError::Message(detail) => Self::bad(detail),
            err => match err.problem() {
                Some((status, _)) if status < 500 => Self { status, detail: err.detail() },
                _ => Self::unavailable(err.to_string()),
            },
        }
    }
}

/// A parameter of a query string.
pub fn parameter(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Default)]
struct Roadmap;

#[async_trait]
impl Plugin for Roadmap {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(
            version = backend.version(),
            on = backend.feature(ROADMAP),
            "roadmap loaded"
        );
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    /// Nothing runs in the background: releases are read from the source plugins, and readiness
    /// from the others, whenever somebody looks.
    async fn run(&self, _backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        Err(PluginError::from("the roadmap has no background work"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        faux::check(backend).await;
        let response = match request.is_ui() {
            true => ui::handle(backend, &request).await,
            false => api::handle(backend, &request).await,
        };
        faux::marked(response)
    }
}

doc_plugin_sdk::main!(
    Roadmap,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("Delivery roadmap", "/")
                .described(
                    "Every release planned in Jira, how far along it is, and whether the services \
                     in it are ready: their pipelines, reliability and end of life",
                )
                .grouped("Platform"),
        ],
        // Its panel with nothing named is every release the viewer may see.
        dashboard: vec![DashboardItem::new("releases", "Releases coming up", "/panel").described(
            "The next releases of the services you can see, and whether each is ready."
        ),],
        resource_panels: ["service", "team", "organisation"]
            .into_iter()
            .map(|kind| ResourcePanel::new(kind, "Roadmap", "/panel"))
            .collect(),
        // What is going wrong with what is coming up, on its own.
        insights: ui::INSIGHTS
            .iter()
            .flat_map(|(id, label)| {
                ["service", "team", "organisation"].into_iter().map(move |kind| {
                    Insight::new(kind, id, label, &format!("/insight/{id}"))
                        .described("Of the releases coming up, as the Roadmap judges them")
                })
            })
            .collect(),
        settings: settings::declared(),
        features: settings::features(),
        // An agent asks over MCP with a POST that only reads.
        read_routes: vec!["mcp".into()],
        ..Manifest::default()
    }
);
