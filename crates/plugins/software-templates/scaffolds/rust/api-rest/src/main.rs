//! {{ values.name }} — {{ values.description }}

mod api;
mod platform;

use anyhow::{Context, Result};
use tokio::net::TcpListener;
use tokio::signal;

use crate::platform::{Config, Flags, Telemetry};

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load();
    let telemetry = Telemetry::start(&config).context("setting up telemetry")?;
    let flags = Flags::start(&config).await;

    let listener = TcpListener::bind(&config.address)
        .await
        .with_context(|| format!("taking {}", config.address))?;
    tracing::info!(address = %config.address, service = %config.service, "listening");

    let served = axum::serve(listener, api::routes(config.clone(), flags))
        .with_graceful_shutdown(stopping())
        .await;

    telemetry.shutdown();
    served.context("serving")
}

/// Ctrl-C, or the TERM a container runtime sends when it is stopping the pod.
async fn stopping() {
    let interrupt = async { signal::ctrl_c().await.ok() };
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate()).ok()?.recv().await
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<Option<()>>();

    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
    }
    tracing::info!("stopping");
}
