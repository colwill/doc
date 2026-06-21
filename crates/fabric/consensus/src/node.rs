//! A bus node: Raft plus the HTTP/3 endpoint that serves Raft RPCs, cluster admin and the
//! bus's own routes.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use doc_secret::Secret;
use doc_transport::{EndpointConfig, H3ServerStream, read_body, respond_json};
use http::{Request, StatusCode};
use openraft::error::{CheckIsLeaderError, ClientWriteError, ForwardToLeader, RaftError};
use openraft::impls::BasicNode;
use openraft::raft::{AppendEntriesRequest, InstallSnapshotRequest, VoteRequest};
use openraft::{Config, ServerState, SnapshotPolicy};
use opentelemetry::metrics::ObservableGauge;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tracing::Instrument;

use crate::log::LogStore;
use crate::metrics::{self, BusMetrics};
use crate::network::{self, Network};
use crate::snapshot::StateMachine;
use crate::types::{BusStateMachine, BusTypes, NodeId, Raft};

/// Raft timings. ADR-0002 measured 1.8–1.9 s failover with these values.
#[derive(Debug, Clone)]
pub struct Tuning {
    pub heartbeat_ms: u64,
    pub election_min_ms: u64,
    pub election_max_ms: u64,
    pub snapshot_every: u64,
    pub keep_logs: u64,
    /// Event and Service fsync every append; the Cache Bus does not (ADR-0002).
    pub durable: bool,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            heartbeat_ms: 100,
            election_min_ms: 500,
            election_max_ms: 1_000,
            snapshot_every: 20_000,
            keep_logs: 1_000,
            durable: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub id: NodeId,
    pub bus: String,
    pub bind: SocketAddr,
    /// `host:port` other nodes and clients dial; the host is the certificate name.
    pub advertise: String,
    pub data_dir: PathBuf,
    pub secrets: PathBuf,
    pub certificate_name: String,
    pub token: Secret<String>,
    pub peers: Vec<(NodeId, String)>,
    pub tuning: Tuning,
}

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("not the leader")]
    NotLeader(Box<Redirect>),
    #[error("raft error: {0}")]
    Raft(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Redirect {
    pub leader_id: Option<NodeId>,
    pub leader_addr: Option<String>,
}

impl From<&ForwardToLeader<NodeId, BasicNode>> for Redirect {
    fn from(forward: &ForwardToLeader<NodeId, BasicNode>) -> Self {
        Self {
            leader_id: forward.leader_id,
            leader_addr: forward.leader_node.as_ref().map(|node| node.addr.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Applied<R> {
    pub index: u64,
    pub response: R,
}

/// The bus's own routes, served on the same endpoint as Raft.
pub trait BusService: Send + Sync + 'static {
    fn handle(
        &self,
        request: Request<()>,
        stream: H3ServerStream,
    ) -> impl Future<Output = Result<()>> + Send;
}

pub struct Node<M: BusStateMachine> {
    raft: Raft<M>,
    state: StateMachine<M>,
    config: NodeConfig,
    endpoint: doc_transport::Endpoint,
    _gauges: Vec<ObservableGauge<u64>>,
}

impl<M: BusStateMachine> Node<M> {
    pub async fn start(config: NodeConfig, machine: M) -> Result<Arc<Self>> {
        doc_transport::install_crypto();
        let endpoint_config = EndpointConfig::server(
            config.bind,
            config.secrets.clone(),
            config.certificate_name.clone(),
        );
        let (endpoint, report) = doc_transport::endpoint(&endpoint_config)
            .with_context(|| format!("starting the {} endpoint", config.bus))?;

        let raft_config = Config {
            cluster_name: config.bus.clone(),
            heartbeat_interval: config.tuning.heartbeat_ms,
            election_timeout_min: config.tuning.election_min_ms,
            election_timeout_max: config.tuning.election_max_ms,
            install_snapshot_timeout: 5_000,
            snapshot_policy: SnapshotPolicy::LogsSinceLast(config.tuning.snapshot_every),
            max_in_snapshot_log_to_keep: config.tuning.keep_logs,
            purge_batch_size: 256,
            ..Default::default()
        }
        .validate()
        .context("validating the Raft configuration")?;

        let log = LogStore::<M>::open(&config.data_dir.join("log"), config.tuning.durable)?;
        let state = StateMachine::open(&config.data_dir.join("state"), machine)?;
        let network = Network::new(endpoint.clone(), &config.token);
        let raft = Raft::<M>::new(config.id, Arc::new(raft_config), network, log, state.clone())
            .await
            .context("starting Raft")?;

        tracing::info!(
            bus = %config.bus,
            node = config.id,
            advertise = %config.advertise,
            durable = config.tuning.durable,
            recv_buffer = report.recv_buffer,
            "bus node listening"
        );
        let _gauges = metrics::export(&config.bus, config.id, raft.metrics());
        Ok(Arc::new(Self { raft, state, config, endpoint, _gauges }))
    }

    pub fn raft(&self) -> &Raft<M> {
        &self.raft
    }

    pub fn state(&self) -> &StateMachine<M> {
        &self.state
    }

    pub fn config(&self) -> &NodeConfig {
        &self.config
    }

    pub fn metrics(&self) -> BusMetrics {
        BusMetrics::from_raft(&self.config.bus, &self.raft.metrics().borrow())
    }

    /// Creates the cluster on first start; later starts find their state on disk.
    pub async fn ensure_cluster(&self) -> Result<bool> {
        if self.raft.is_initialized().await? {
            return Ok(false);
        }
        if self.config.peers.is_empty() {
            return Ok(false);
        }
        let members: BTreeMap<NodeId, BasicNode> = self
            .config
            .peers
            .iter()
            .map(|(id, addr)| (*id, BasicNode { addr: addr.clone() }))
            .collect();
        match self.raft.initialize(members).await {
            Ok(()) => {
                tracing::info!(bus = %self.config.bus, "cluster initialised");
                Ok(true)
            }
            // Another node won the race, which is the normal case when all three start together.
            Err(err) => {
                tracing::info!(bus = %self.config.bus, %err, "cluster already initialised elsewhere");
                Ok(false)
            }
        }
    }

    pub async fn write(&self, command: M::Command) -> Result<Applied<M::Response>, NodeError> {
        let bus = &self.config.bus;
        let span = match doc_telemetry::in_trace() {
            true => {
                tracing::info_span!(target: "doc", "raft.write", otel.name = %format!("{bus} raft write"), doc.bus = %bus, raft.log.index = tracing::field::Empty)
            }
            false => tracing::Span::none(),
        };
        let started = std::time::Instant::now();
        match self.raft.client_write(command).instrument(span.clone()).await {
            Ok(response) => {
                metrics::committed(bus, started.elapsed());
                span.record("raft.log.index", response.log_id.index);
                Ok(Applied { index: response.log_id.index, response: response.data })
            }
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => {
                Err(NodeError::NotLeader(Box::new(Redirect::from(&forward))))
            }
            Err(err) => Err(NodeError::Raft(err.to_string())),
        }
    }

    /// Confirms this node is still the leader, for reads that must not be stale.
    pub async fn ensure_linearizable(&self) -> Result<(), NodeError> {
        match self.raft.ensure_linearizable().await {
            Ok(_) => Ok(()),
            Err(RaftError::APIError(CheckIsLeaderError::ForwardToLeader(forward))) => {
                Err(NodeError::NotLeader(Box::new(Redirect::from(&forward))))
            }
            Err(err) => Err(NodeError::Raft(err.to_string())),
        }
    }

    pub async fn add_learner(&self, id: NodeId, addr: String) -> Result<u64, NodeError> {
        match self.raft.add_learner(id, BasicNode { addr }, true).await {
            Ok(response) => Ok(response.log_id.index),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => {
                Err(NodeError::NotLeader(Box::new(Redirect::from(&forward))))
            }
            Err(err) => Err(NodeError::Raft(err.to_string())),
        }
    }

    pub async fn change_membership(
        &self,
        members: BTreeSet<NodeId>,
        retain: bool,
    ) -> Result<u64, NodeError> {
        match self.raft.change_membership(members, retain).await {
            Ok(response) => Ok(response.log_id.index),
            Err(RaftError::APIError(ClientWriteError::ForwardToLeader(forward))) => {
                Err(NodeError::NotLeader(Box::new(Redirect::from(&forward))))
            }
            Err(err) => Err(NodeError::Raft(err.to_string())),
        }
    }

    pub async fn shutdown(&self) {
        let _ = self.raft.shutdown().await;
        self.endpoint.close().await;
    }

    /// Serves Raft, cluster admin and the bus's routes until the endpoint closes.
    pub async fn serve<S: BusService>(self: Arc<Self>, service: Arc<S>) {
        let endpoint = self.endpoint.clone();
        doc_transport::serve(endpoint, move |request, stream, _remote| {
            let node = self.clone();
            let service = service.clone();
            async move { node.dispatch(request, stream, service).await }
        })
        .await;
    }

    async fn dispatch<S: BusService>(
        &self,
        request: Request<()>,
        mut stream: H3ServerStream,
        service: Arc<S>,
    ) -> Result<()> {
        let presented = doc_transport::bearer(&request);
        if !presented.is_some_and(|presented| self.config.token.matches(presented)) {
            return respond_json(
                &mut stream,
                StatusCode::UNAUTHORIZED,
                &json!({"error": "invalid bus token"}),
            )
            .await;
        }
        let path = request.uri().path().to_string();
        if !path.starts_with("/raft/v1/") && !path.starts_with("/cluster/v1/") {
            let span = match doc_telemetry::header(request.headers()) {
                Some(parent) => {
                    let bus = &self.config.bus;
                    let span = tracing::info_span!(target: "doc", "bus.request", otel.name = %format!("{bus} {path}"), otel.kind = "server", doc.bus = %bus, doc.node = self.config.id);
                    doc_telemetry::adopt(&span, Some(parent));
                    span
                }
                None => tracing::Span::none(),
            };
            return service.handle(request, stream).instrument(span).await;
        }
        let body = read_body(&mut stream).await?;
        let (status, value) = self.cluster_route(&path, &body).await?;
        respond_json(&mut stream, status, &value).await
    }

    async fn cluster_route(&self, path: &str, body: &[u8]) -> Result<(StatusCode, Value)> {
        Ok(match path {
            network::APPEND => {
                let rpc: AppendEntriesRequest<BusTypes<M>> = serde_json::from_slice(body)?;
                (StatusCode::OK, serde_json::to_value(self.raft.append_entries(rpc).await)?)
            }
            network::VOTE => {
                let rpc: VoteRequest<NodeId> = serde_json::from_slice(body)?;
                (StatusCode::OK, serde_json::to_value(self.raft.vote(rpc).await)?)
            }
            network::SNAPSHOT => {
                let rpc: InstallSnapshotRequest<BusTypes<M>> = serde_json::from_slice(body)?;
                (StatusCode::OK, serde_json::to_value(self.raft.install_snapshot(rpc).await)?)
            }
            "/cluster/v1/metrics" => (StatusCode::OK, serde_json::to_value(self.metrics())?),
            "/cluster/v1/add-learner" => {
                #[derive(Deserialize)]
                struct AddLearner {
                    id: NodeId,
                    addr: String,
                }
                let request: AddLearner = serde_json::from_slice(body)?;
                match self.add_learner(request.id, request.addr).await {
                    Ok(index) => (StatusCode::OK, json!({"log_index": index})),
                    Err(err) => node_error(err),
                }
            }
            "/cluster/v1/membership" => {
                #[derive(Deserialize)]
                struct Membership {
                    members: BTreeSet<NodeId>,
                    #[serde(default)]
                    retain: bool,
                }
                let request: Membership = serde_json::from_slice(body)?;
                match self.change_membership(request.members, request.retain).await {
                    Ok(index) => (StatusCode::OK, json!({"log_index": index})),
                    Err(err) => node_error(err),
                }
            }
            "/cluster/v1/snapshot" => match self.raft.trigger().snapshot().await {
                Ok(()) => (StatusCode::OK, json!({"triggered": true})),
                Err(err) => (StatusCode::SERVICE_UNAVAILABLE, json!({"error": err.to_string()})),
            },
            _ => (StatusCode::NOT_FOUND, json!({"error": "unknown cluster route"})),
        })
    }
}

/// `421` carries the leader's address, which is how clients find the leader (ADR-0002).
pub fn node_error(error: NodeError) -> (StatusCode, Value) {
    match error {
        NodeError::NotLeader(redirect) => (
            StatusCode::MISDIRECTED_REQUEST,
            serde_json::to_value(*redirect).unwrap_or_else(|_| json!({})),
        ),
        NodeError::Raft(message) => (StatusCode::SERVICE_UNAVAILABLE, json!({"error": message})),
    }
}

pub fn is_leader(state: ServerState) -> bool {
    state == ServerState::Leader
}

pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(5);
