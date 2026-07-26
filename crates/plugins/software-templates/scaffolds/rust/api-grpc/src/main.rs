//! {{ values.name }} — {{ values.description }}

mod platform;
mod rpc;

use anyhow::{Context, Result};
use tokio::signal;
use tonic::transport::Server;

use crate::platform::{Config, Flags, Telemetry};
use crate::rpc::proto::{{ scaffold.package }}_service_server::{{ scaffold.pascal }}ServiceServer;

#[tokio::main]
async fn main() -> Result<()> {
    let mut config = Config::load();
    if config.address.ends_with(":8080") {
        config.address = config.address.replace(":8080", ":9090");
    }
    let telemetry = Telemetry::start(&config).context("setting up telemetry")?;
    let flags = Flags::start(&config).await;

    let address = config.address.parse().with_context(|| format!("{} is not an address", config.address))?;

    // Health and reflection are what everything else expects a gRPC service to answer.
    let (checks, health) = tonic_health::server::health_reporter();
    checks.set_serving::<{{ scaffold.pascal }}ServiceServer<rpc::Service>>().await;
    let reflection = tonic_reflection::server::Builder::configure()
        .register_encoded_file_descriptor_set(rpc::DESCRIPTOR)
        .build_v1()?;

    tracing::info!(address = %config.address, service = %config.service, "listening");
    let served = Server::builder()
        .trace_fn(|request| tracing::info_span!("grpc", path = %request.uri().path()))
        .add_service(health)
        .add_service(reflection)
        .add_service({{ scaffold.pascal }}ServiceServer::new(rpc::Service::new(config, flags)))
        .serve_with_shutdown(address, stopping())
        .await;

    telemetry.shutdown();
    served.context("serving")
}

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
