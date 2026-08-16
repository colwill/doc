//! Faux data (FIX-FAUX-DATA): made up in one place for every plugin that shows data from outside
//! DOC — what GitHub, Jira, endoflife.date, health checks and reports would give DORA metrics,
//! CI/CD/CT metrics, reliability, end of life and the delivery roadmap — for one estate of
//! services, each with one story that holds in all of them. A toggle on the Settings page turns
//! it on for each plugin; that plugin then asks here instead of its sources, works the answer out
//! with its own code, and the platform says at the top of each page that the data is faux.

mod delivery;
mod dice;
mod docs;
mod estate;
mod lifecycles;
mod pipelines;
mod png;
mod releases;
mod reliability;
mod routes;
mod settings;
mod ui;

use async_trait::async_trait;
use chrono::Utc;
use doc_plugin_sdk::{
    Backend, Classification, Collection, DataRequest, Declaration, Export, Field, Manifest, Nav,
    Plugin, PluginError, Request, Response, RunInput, RunOutput,
};
use serde_json::{Value, json};

use settings::{CONSUMERS, Config};

pub const ID: &str = "faux-data";

/// Whether faux data is provided to each plugin, exported to them all: how each learns its
/// toggle, with a data read rather than a call to a plugin that might not be running.
fn declaration() -> Declaration {
    let consumers: Vec<&str> = CONSUMERS.iter().map(|(id, ..)| *id).collect();
    Declaration::default().collection(
        "serving",
        Collection::new()
            .field("plugin", Field::text().key().describe("The plugin it may be provided to"))
            .field("on", Field::boolean().required().default(json!(false)))
            .field("changed_at", Field::timestamp())
            .export(Export::to(&consumers)),
    )
}

/// Writes each toggle where its plugin reads it. A settings change reloads the plugin, so this
/// runs whenever one changes.
async fn announce_toggles(backend: &Backend, config: &Config) -> Result<(), PluginError> {
    let now = Utc::now();
    let writes = CONSUMERS
        .iter()
        .map(|(id, ..)| {
            let on = config.serving.contains(*id);
            DataRequest::upsert(
                "serving",
                &["plugin"],
                json!({ "plugin": id, "on": on, "changed_at": now }),
            )
        })
        .collect();
    backend.batch(writes).await?;
    Ok(())
}

#[derive(Default)]
struct FauxData;

#[async_trait]
impl Plugin for FauxData {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let config = Config::read(&backend.settings());
        announce_toggles(backend, &config).await?;
        tracing::info!(
            version = backend.version(),
            serving = ?config.serving,
            "faux-data loaded"
        );
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    /// Nothing runs in the background: faux data is made up as it is asked for.
    async fn run(&self, _backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        Err(PluginError::from("faux-data has no background work"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let config = Config::read(&backend.settings());
        match request.path.starts_with("discovery/faux/") {
            true => routes::handle(backend, &request, &config).await,
            false if request.is_ui() => ui::handle(backend, &request, &config).await,
            false => Response::not_found(),
        }
    }
}

doc_plugin_sdk::main!(
    FauxData,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("Faux data", "/")
                .described(
                    "Made-up data for DORA, CI/CD/CT, reliability, end of life and the roadmap, \
                     to see how they look before anything real is read",
                )
                .grouped("Admin"),
        ],
        settings: settings::declared(),
        data: declaration(),
        ..Manifest::default()
    }
);
