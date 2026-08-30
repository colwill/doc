//! The Architecture map (ADR-0017): components declared by whoever builds them, relationships
//! drawn as claims, and the platform checking the claims against what it can observe.
//!
//! What is here so far is the half the editor stands on: the components, the claims, the views and
//! their owners. Derivation (§3), the three link states (§4) and the canvas (§5) are next, and
//! the ADR's own order says the read-only view earns its keep before the canvas is attempted.

mod directory;
mod model;
mod ops;
mod store;
mod sync;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, Insight, Manifest, Nav, Plugin, PluginError, Request, ResourcePanel,
    Response, RunInput, RunOutput, Schedule,
};
use serde_json::{Value, json};

use model::Refusal;

pub const ID: &str = "architecture";

/// The schedule that tells the Catalogue about every component again, so one it missed arrives.
const CATALOGUE: &str = "catalogue";

#[derive(Default)]
struct Architecture;

#[async_trait]
impl Plugin for Architecture {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), "architecture loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        match input.payload["schedule"].as_str() {
            Some(CATALOGUE) => {
                let told = sync::everything(backend).await?;
                Ok(RunOutput { payload: json!({ "components": told }) })
            }
            _ => Err(PluginError::from("architecture runs only its own schedule")),
        }
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let path: Vec<&str> =
            request.path.trim_matches('/').split('/').filter(|part| !part.is_empty()).collect();
        match path.split_first() {
            Some((&"ui", rest)) => ui::handle(backend, &request, rest).await,
            _ => Refusal::missing("no such route").response(),
        }
    }
}

doc_plugin_sdk::main!(
    Architecture,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("Architecture", "/")
                .described("How services are built, and what the platform can verify of it")
                .grouped("Technology")
        ],
        // The view a service is the primary component of, drawn as that view opens, on the
        // service's Catalogue page: as a part of the page of its own, or pinned to its top below
        // whatever figures are pinned there.
        resource_panels: vec![ResourcePanel::new("service", "Architecture", "/panel")],
        insights: vec![
            Insight::new("service", "diagram", ui::DIAGRAM, "/insight/diagram")
                .described("The view of the architecture centred on this service")
                .wide(),
        ],
        // A component is told to the Catalogue as it is declared, and again on this, so one
        // declared while the Catalogue was not listening still arrives.
        schedules: vec![Schedule::new(
            CATALOGUE,
            "*/5 * * * *",
            "Tells the Catalogue about every component declared here"
        )],
        data: store::declaration(),
        ..Manifest::default()
    }
);
