//! Cluster client: finds the leader, follows `421` redirects and skips nodes that just failed.
//! ADR-0002 measured that these rules, not the Raft settings, decide how long a client stalls
//! during a failover.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use doc_secret::Secret;
use doc_transport::{EndpointConfig, H3Client, H3ClientRecv, H3ClientSend, TransportError};
use http::{Method, StatusCode};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::metrics::BusMetrics;
use crate::network::resolve;
use crate::node::Redirect;
use crate::types::server_name_of;

const UNHEALTHY_FOR: Duration = Duration::from_millis(1_000);
const ATTEMPT_TIMEOUT: Duration = Duration::from_millis(1_000);
/// A plain failure or timeout waits this long before the next node is tried: with no other node
/// to prefer, `pick` hands back one already marked unhealthy rather than stall, and a caller who
/// keeps this client for as long as a subscription lives would otherwise dial fresh sockets as
/// fast as they fail, all day, for however long the cluster stays unreachable.
const RETRY_BACKOFF: Duration = Duration::from_millis(250);

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("no node in {bus} accepted the request within the deadline: {last}")]
    Unavailable { bus: String, last: String },
    #[error("{bus} refused the request: {status} {body}")]
    Refused { bus: String, status: StatusCode, body: String },
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("malformed answer from {bus}: {source}")]
    Malformed { bus: String, source: serde_json::Error },
}

struct Inner {
    bus: String,
    token: Secret<String>,
    nodes: Vec<String>,
    endpoint: doc_transport::Endpoint,
    clients: Mutex<HashMap<String, H3Client>>,
    leader: RwLock<Option<String>>,
    next: Mutex<usize>,
    unhealthy: Mutex<HashMap<String, Instant>>,
}

#[derive(Clone)]
pub struct ClusterClient(Arc<Inner>);

impl ClusterClient {
    pub fn new(
        bus: impl Into<String>,
        nodes: Vec<String>,
        token: impl Into<Secret<String>>,
        secrets: impl Into<std::path::PathBuf>,
    ) -> anyhow::Result<Self> {
        doc_transport::install_crypto();
        let (endpoint, _) = doc_transport::endpoint(&EndpointConfig::client(secrets))?;
        Ok(Self(Arc::new(Inner {
            bus: bus.into(),
            token: token.into(),
            nodes,
            endpoint,
            clients: Mutex::default(),
            leader: RwLock::default(),
            next: Mutex::default(),
            unhealthy: Mutex::default(),
        })))
    }

    pub fn bus(&self) -> &str {
        &self.0.bus
    }

    pub fn nodes(&self) -> &[String] {
        &self.0.nodes
    }

    async fn client(&self, addr: &str) -> Result<H3Client, TransportError> {
        if let Some(client) = self.0.clients.lock().get(addr) {
            return Ok(client.clone());
        }
        let resolved = resolve(addr).await?;
        let client = H3Client::new(self.0.endpoint.clone(), resolved, server_name_of(addr));
        self.0.clients.lock().insert(addr.to_string(), client.clone());
        Ok(client)
    }

    fn unhealthy(&self, addr: &str) -> bool {
        self.0.unhealthy.lock().get(addr).is_some_and(|at| at.elapsed() < UNHEALTHY_FOR)
    }

    fn mark_unhealthy(&self, addr: &str) {
        self.0.unhealthy.lock().insert(addr.to_string(), Instant::now());
        self.0.clients.lock().remove(addr);
        let mut leader = self.0.leader.write();
        if leader.as_deref() == Some(addr) {
            *leader = None;
        }
    }

    /// A leader hint is only followed when it names a node this client was configured with:
    /// a cluster may advertise addresses that its clients cannot reach (for example container
    /// names seen from outside Docker), and rotating finds the leader anyway.
    fn usable_hint(&self, hint: &str, target: &str) -> bool {
        if hint == target || self.unhealthy(hint) {
            return false;
        }
        if self.0.nodes.iter().any(|node| node == hint) {
            return true;
        }
        tracing::debug!(hint, "ignoring a leader hint that is not a configured node");
        false
    }

    fn pick(&self, prefer_leader: bool) -> String {
        if prefer_leader && let Some(leader) = self.0.leader.read().clone() {
            return leader;
        }
        let mut next = self.0.next.lock();
        for _ in 0..self.0.nodes.len() {
            let current = *next;
            *next = (current + 1) % self.0.nodes.len();
            if !self.unhealthy(&self.0.nodes[current]) {
                return self.0.nodes[current].clone();
            }
        }
        let current = *next;
        *next = (current + 1) % self.0.nodes.len();
        self.0.nodes[current].clone()
    }

    /// Sends to the leader, following redirects; use for writes and consistent reads.
    pub async fn call<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        deadline: Duration,
    ) -> Result<R, ClusterError> {
        self.send(path, body, deadline, true, ATTEMPT_TIMEOUT).await
    }

    /// A long poll is held open by the node on purpose, so the caller says how long an attempt may
    /// take. With the default a receive that waits for work is read as a dead node on every call.
    pub async fn call_waiting<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        deadline: Duration,
        attempt: Duration,
    ) -> Result<R, ClusterError> {
        self.send(path, body, deadline, true, attempt.max(ATTEMPT_TIMEOUT)).await
    }

    /// Sends to any node; use for stale reads, which work even without a quorum.
    pub async fn call_any<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        deadline: Duration,
    ) -> Result<R, ClusterError> {
        self.send(path, body, deadline, false, ATTEMPT_TIMEOUT).await
    }

    async fn send<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        deadline: Duration,
        prefer_leader: bool,
        attempt_timeout: Duration,
    ) -> Result<R, ClusterError> {
        let payload = Bytes::from(
            serde_json::to_vec(body)
                .map_err(|e| ClusterError::Malformed { bus: self.0.bus.clone(), source: e })?,
        );
        let authorization = format!("Bearer {}", self.0.token.expose());
        let parent = doc_telemetry::traceparent();
        let mut headers =
            vec![("authorization", authorization.as_str()), ("content-type", "application/json")];
        if let Some(parent) = parent.as_deref() {
            headers.push((doc_telemetry::TRACEPARENT, parent));
        }
        let started = Instant::now();
        let mut last = String::from("no attempt made");
        while started.elapsed() < deadline {
            let target = self.pick(prefer_leader);
            let attempt = tokio::time::timeout(attempt_timeout, async {
                let client = self.client(&target).await?;
                client.call(Method::POST, path, &headers, payload.clone()).await
            })
            .await;
            match attempt {
                Ok(Ok((StatusCode::OK, bytes))) => {
                    if prefer_leader {
                        *self.0.leader.write() = Some(target);
                    }
                    return serde_json::from_slice(&bytes).map_err(|e| ClusterError::Malformed {
                        bus: self.0.bus.clone(),
                        source: e,
                    });
                }
                Ok(Ok((StatusCode::MISDIRECTED_REQUEST, bytes))) => {
                    let redirect: Redirect = serde_json::from_slice(&bytes)
                        .unwrap_or(Redirect { leader_id: None, leader_addr: None });
                    last = format!("{target} is not the leader");
                    let hint = redirect.leader_addr.filter(|addr| self.usable_hint(addr, &target));
                    let known = hint.is_some();
                    *self.0.leader.write() = hint;
                    if !known {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
                Ok(Ok((status, bytes))) => {
                    return Err(ClusterError::Refused {
                        bus: self.0.bus.clone(),
                        status,
                        body: String::from_utf8_lossy(&bytes).into_owned(),
                    });
                }
                Ok(Err(err)) => {
                    last = err.to_string();
                    self.mark_unhealthy(&target);
                    tokio::time::sleep(RETRY_BACKOFF).await;
                }
                Err(_) => {
                    last = format!("{target} did not answer within {ATTEMPT_TIMEOUT:?}");
                    self.mark_unhealthy(&target);
                    tokio::time::sleep(RETRY_BACKOFF).await;
                }
            }
        }
        Err(ClusterError::Unavailable { bus: self.0.bus.clone(), last })
    }

    /// Opens a long-lived stream on the leader, sending `first` as its opening line. The stream
    /// stays open in both directions: subscriptions read deliveries and write acknowledgements.
    pub async fn open_stream(
        &self,
        path: &str,
        first: Bytes,
        deadline: Duration,
    ) -> Result<(H3ClientSend, H3ClientRecv), ClusterError> {
        let started = Instant::now();
        let mut last = String::from("no attempt made");
        while started.elapsed() < deadline {
            let target = self.pick(true);
            let authorization = format!("Bearer {}", self.0.token.expose());
            let attempt = tokio::time::timeout(ATTEMPT_TIMEOUT, async {
                let client = self.client(&target).await?;
                let request = http::Request::builder()
                    .method(Method::POST)
                    .uri(client.uri(path))
                    .header("authorization", authorization)
                    .header("content-type", "application/x-ndjson")
                    .body(())?;
                let mut stream = client.open(request).await?;
                stream
                    .send_data(first.clone())
                    .await
                    .map_err(|e| anyhow::anyhow!(e.to_string()))?;
                let (send, mut recv) = stream.split();
                let response =
                    recv.recv_response().await.map_err(|e| anyhow::anyhow!(e.to_string()))?;
                let mut body = Vec::new();
                if response.status() != StatusCode::OK {
                    while let Ok(Some(mut chunk)) = recv.recv_data().await {
                        use bytes::Buf;
                        body.extend_from_slice(&chunk.copy_to_bytes(chunk.remaining()));
                    }
                }
                Ok::<_, anyhow::Error>((response.status(), body, send, recv))
            })
            .await;
            match attempt {
                Ok(Ok((StatusCode::OK, _, send, recv))) => {
                    *self.0.leader.write() = Some(target);
                    return Ok((send, recv));
                }
                Ok(Ok((StatusCode::MISDIRECTED_REQUEST, body, _, _))) => {
                    let redirect: Redirect = serde_json::from_slice(&body)
                        .unwrap_or(Redirect { leader_id: None, leader_addr: None });
                    last = format!("{target} is not the leader");
                    let hint = redirect.leader_addr.filter(|addr| self.usable_hint(addr, &target));
                    let known = hint.is_some();
                    *self.0.leader.write() = hint;
                    if !known {
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                }
                Ok(Ok((status, body, _, _))) => {
                    return Err(ClusterError::Refused {
                        bus: self.0.bus.clone(),
                        status,
                        body: String::from_utf8_lossy(&body).into_owned(),
                    });
                }
                Ok(Err(err)) => {
                    last = err.to_string();
                    self.mark_unhealthy(&target);
                    tokio::time::sleep(RETRY_BACKOFF).await;
                }
                Err(_) => {
                    last = format!("{target} did not answer within {ATTEMPT_TIMEOUT:?}");
                    self.mark_unhealthy(&target);
                    tokio::time::sleep(RETRY_BACKOFF).await;
                }
            }
        }
        Err(ClusterError::Unavailable { bus: self.0.bus.clone(), last })
    }

    /// Asks every node for its metrics, for the status workers and the dashboard.
    pub async fn metrics(&self) -> Vec<(String, Result<BusMetrics, ClusterError>)> {
        let mut out = Vec::new();
        for addr in self.0.nodes.clone() {
            let authorization = format!("Bearer {}", self.0.token.expose());
            let result = async {
                let client = self.client(&addr).await?;
                let headers = [("authorization", authorization.as_str())];
                let call = client.call(Method::POST, "/cluster/v1/metrics", &headers, Bytes::new());
                let (status, bytes) =
                    tokio::time::timeout(ATTEMPT_TIMEOUT, call).await.map_err(|_| {
                        ClusterError::Unavailable {
                            bus: self.0.bus.clone(),
                            last: format!("{addr} timed out"),
                        }
                    })??;
                if status != StatusCode::OK {
                    return Err(ClusterError::Refused {
                        bus: self.0.bus.clone(),
                        status,
                        body: String::from_utf8_lossy(&bytes).into_owned(),
                    });
                }
                serde_json::from_slice::<BusMetrics>(&bytes)
                    .map_err(|e| ClusterError::Malformed { bus: self.0.bus.clone(), source: e })
            }
            .await;
            if result.is_err() {
                self.mark_unhealthy(&addr);
            }
            out.push((addr, result));
        }
        out
    }
}
