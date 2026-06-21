//! What every bus binary needs: arguments, start-up, cluster creation and graceful shutdown.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use doc_secret::Secret;

use crate::node::{BusService, NodeConfig, Tuning};
use crate::types::{BusStateMachine, NodeId, server_name_of};
use crate::{ClusterClient, Node};

#[derive(Debug, Clone, clap::Args)]
pub struct BusArgs {
    #[arg(long, env = "DOC_NODE_ID")]
    pub id: NodeId,
    #[arg(long, env = "DOC_BIND", default_value = "0.0.0.0:4433")]
    pub bind: SocketAddr,
    /// `host:port` that peers and clients dial; its host must match this node's certificate.
    #[arg(long, env = "DOC_ADVERTISE")]
    pub advertise: String,
    #[arg(long, env = "DOC_DATA_DIR", default_value = "/data")]
    pub data: PathBuf,
    #[arg(long, env = "DOC_SECRETS_DIR", default_value = "/secrets")]
    pub secrets: PathBuf,
    /// Defaults to the host part of `--advertise`.
    #[arg(long, env = "DOC_CERTIFICATE")]
    pub certificate: Option<String>,
    #[arg(long, env = "DOC_BUS_TOKEN", value_parser = secret)]
    pub token: Option<Secret<String>>,
    /// Defaults to `<secrets>/tokens/buses/<bus>.token`.
    #[arg(long, env = "DOC_BUS_TOKEN_FILE")]
    pub token_file: Option<PathBuf>,
    /// `id=host:port`, comma separated; used only when the cluster is first created.
    #[arg(long, env = "DOC_PEERS", default_value = "")]
    pub peers: String,
    #[arg(long, env = "DOC_SNAPSHOT_EVERY", default_value_t = 20_000)]
    pub snapshot_every: u64,
}

fn secret(value: &str) -> Result<Secret<String>, String> {
    Ok(Secret::new(value.to_string()))
}

impl BusArgs {
    pub fn token(&self, bus: &str) -> Result<Secret<String>> {
        if let Some(token) = &self.token {
            return Ok(token.clone());
        }
        let path = self
            .token_file
            .clone()
            .unwrap_or_else(|| self.secrets.join(format!("tokens/buses/{bus}.token")));
        let token = std::fs::read_to_string(&path)
            .with_context(|| format!("reading the {bus} token from {}", path.display()))?;
        Ok(Secret::new(token.trim().to_string()))
    }

    pub fn peers(&self) -> Result<Vec<(NodeId, String)>> {
        self.peers
            .split(',')
            .filter(|peer| !peer.trim().is_empty())
            .map(|peer| {
                let (id, addr) = peer
                    .split_once('=')
                    .with_context(|| format!("expected id=host:port, got {peer}"))?;
                Ok((id.trim().parse()?, addr.trim().to_string()))
            })
            .collect()
    }

    pub fn node_config(&self, bus: &str, durable: bool) -> Result<NodeConfig> {
        Ok(NodeConfig {
            id: self.id,
            bus: bus.to_string(),
            bind: self.bind,
            advertise: self.advertise.clone(),
            data_dir: self.data.clone(),
            secrets: self.secrets.clone(),
            certificate_name: self
                .certificate
                .clone()
                .unwrap_or_else(|| server_name_of(&self.advertise).to_string()),
            token: self.token(bus)?,
            peers: self.peers()?,
            tuning: Tuning { snapshot_every: self.snapshot_every, durable, ..Tuning::default() },
        })
    }

    /// A client for this cluster, which the bus's own tools and status checks use.
    pub fn cluster_client(&self, bus: &str) -> Result<ClusterClient> {
        let nodes = self.peers()?.into_iter().map(|(_, addr)| addr).collect();
        ClusterClient::new(bus, nodes, self.token(bus)?, self.secrets.clone())
    }
}

/// Starts the node, serves it, creates the cluster on first run and waits for shutdown.
pub async fn run<M, S, F>(
    bus: &str,
    args: BusArgs,
    durable: bool,
    machine: M,
    service: F,
) -> Result<()>
where
    M: BusStateMachine,
    S: BusService,
    F: FnOnce(Arc<Node<M>>) -> Arc<S>,
{
    let node = Node::start(args.node_config(bus, durable)?, machine).await?;
    let serving = tokio::spawn(node.clone().serve(service(node.clone())));

    tokio::time::sleep(Duration::from_millis(500)).await;
    if let Err(err) = node.ensure_cluster().await {
        tracing::warn!(%err, "could not create the cluster");
    }

    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = terminate.recv() => {}
        result = serving => {
            result?;
            return Ok(());
        }
    }
    tracing::info!(bus, node = args.id, "shutting down");
    node.shutdown().await;
    Ok(())
}

/// JSON logs with the usual noisy targets turned down, and OTLP export as `doc-<bus>` when configured.
pub fn init_tracing(bus: &str) -> doc_telemetry::Telemetry {
    let filter = "info,openraft=warn,quiche=warn,tokio_quiche=warn";
    doc_telemetry::init(&format!("doc-{bus}"), env!("CARGO_PKG_VERSION"), filter)
}
