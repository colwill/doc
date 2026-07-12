//! Resource Definitions (T37): the catalogue of organisations, services, teams and what other
//! plugins publish, the connections between them, and the definitions that say how to drill
//! through them. Organisations and teams are the platform's, read from `core.*` and written there
//! (T69); everything else is this plugin's own.

mod api;
mod apply;
mod definitions;
mod discovery;
mod graph;
mod kinds;
mod model;
mod ops;
mod rbac;
mod store;
mod sync;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Capability, Classification, DashboardItem, Event, Manifest, Nav, Plugin, PluginError,
    Request, Response, RunInput, RunOutput,
};
use serde_json::Value;

#[derive(Default)]
struct Resources;

#[async_trait]
impl Plugin for Resources {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        store::seed(backend).await?;
        tracing::info!(version = backend.version(), "resources loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, _backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        Err(PluginError::from("resources has nothing to run: apply documents through its API"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        api::handle(backend, request).await
    }

    async fn on_event(&self, backend: &Backend, event: Event) -> Result<(), PluginError> {
        sync::on_event(backend, event).await
    }
}

doc_plugin_sdk::main!(
    Resources,
    Manifest {
        id: "resources".into(),
        classification: Classification::Synchronous,
        nav: vec![
            // The Catalogue itself, first in the Platform menu, before the plugins that show
            // parts of what it holds.
            Nav::new("Catalogue", "/")
                .described("Every organisation, service and team, and how they relate")
                .grouped("Platform"),
        ],
        dashboard: vec![
            DashboardItem::new("services", "Your teams' services", "/dashboard").described(
                "The services your teams own or work on, worst first by what maturity, delivery, \
                 pipelines, reliability and end of life say of each.",
            ),
        ],
        // Organisations and teams are core's, and the catalogue shows and writes them (T69).
        capabilities: vec![Capability::TeamWriter],
        subscriptions: sync::TOPICS.iter().map(|topic| (*topic).to_string()).collect(),
        // Which plugins may read the catalogue as themselves, for work nobody started.
        settings: discovery::declared(),
        data: store::declaration(),
        ..Manifest::default()
    }
);
