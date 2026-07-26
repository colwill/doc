//! {{ values.name }} — {{ values.description }}
//!
//! `{{ values.name }} crd` prints the CustomResourceDefinition, so the cluster and the types here
//! are always the same thing: `cargo run -- crd | kubectl apply -f -`.

mod controller;
mod platform;
mod types;

use std::sync::Arc;

use anyhow::{Context as _, Result};
use futures::StreamExt;
use kube::runtime::Controller;
use kube::runtime::watcher::Config as WatcherConfig;
use kube::{Api, Client, CustomResourceExt};

use crate::platform::{Config, Flags, Telemetry};
use crate::types::{{ scaffold.kind }};

#[tokio::main]
async fn main() -> Result<()> {
    if std::env::args().nth(1).as_deref() == Some("crd") {
        println!("{}", serde_yaml::to_string(&{{ scaffold.kind }}::crd())?);
        return Ok(());
    }

    let config = Config::load();
    let telemetry = Telemetry::start(&config).context("setting up telemetry")?;
    let flags = Flags::start(&config).await;

    let client = Client::try_default().await.context("reaching the cluster")?;
    let resources: Api<{{ scaffold.kind }}> = Api::all(client.clone());
    let context = Arc::new(controller::Context { client: client.clone(), config, flags });

    tracing::info!(kind = "{{ scaffold.kind }}", group = "{{ scaffold.group }}", "watching");
    Controller::new(resources, WatcherConfig::default())
        .shutdown_on_signal()
        .run(controller::reconcile, controller::on_error, context)
        .for_each(|outcome| async move {
            match outcome {
                Ok((resource, _)) => tracing::debug!(?resource, "reconciled"),
                Err(err) => tracing::warn!(%err, "a reconcile did not finish"),
            }
        })
        .await;

    telemetry.shutdown();
    Ok(())
}
