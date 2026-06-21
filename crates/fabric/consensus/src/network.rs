//! Raft RPCs over HTTP/3: one authenticated connection per peer, redialled when it drops.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use doc_secret::Secret;
use doc_transport::{H3Client, TransportError};
use http::{Method, StatusCode};
use openraft::error::{
    InstallSnapshotError, NetworkError, RPCError, RaftError, RemoteError, Unreachable,
};
use openraft::impls::BasicNode;
use openraft::network::{RPCOption, RaftNetwork, RaftNetworkFactory};
use openraft::raft::{
    AppendEntriesRequest, AppendEntriesResponse, InstallSnapshotRequest, InstallSnapshotResponse,
    VoteRequest, VoteResponse,
};
use parking_lot::Mutex;
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::types::{BusStateMachine, BusTypes, NodeId, server_name_of};

pub const APPEND: &str = "/raft/v1/append";
pub const VOTE: &str = "/raft/v1/vote";
pub const SNAPSHOT: &str = "/raft/v1/snapshot";

pub struct Network {
    endpoint: doc_transport::Endpoint,
    token: Secret<Arc<str>>,
    clients: Arc<Mutex<HashMap<String, H3Client>>>,
}

impl Clone for Network {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            token: self.token.clone(),
            clients: self.clients.clone(),
        }
    }
}

impl Network {
    pub fn new(endpoint: doc_transport::Endpoint, token: &Secret<String>) -> Self {
        let token = Secret::new(Arc::from(token.expose().as_str()));
        Self { endpoint, token, clients: Arc::default() }
    }

    pub async fn client(&self, addr: &str) -> Result<H3Client, TransportError> {
        if let Some(client) = self.clients.lock().get(addr) {
            return Ok(client.clone());
        }
        let resolved = resolve(addr).await?;
        let client = H3Client::with_pool(self.endpoint.clone(), resolved, server_name_of(addr), 1);
        self.clients.lock().insert(addr.to_string(), client.clone());
        Ok(client)
    }

    pub fn forget(&self, addr: &str) {
        self.clients.lock().remove(addr);
    }
}

pub async fn resolve(addr: &str) -> Result<std::net::SocketAddr, TransportError> {
    let addr = addr.to_string();
    tokio::net::lookup_host(&addr)
        .await
        .map_err(|e| TransportError::Unreachable { peer: addr.clone(), source: e.into() })?
        .find(|candidate| candidate.is_ipv4())
        .ok_or_else(|| TransportError::Unreachable {
            peer: addr.clone(),
            source: anyhow::anyhow!("no IPv4 address"),
        })
}

pub struct Connection {
    network: Network,
    target: NodeId,
    addr: String,
}

impl<M: BusStateMachine> RaftNetworkFactory<BusTypes<M>> for Network {
    type Network = Connection;

    async fn new_client(&mut self, target: NodeId, node: &BasicNode) -> Self::Network {
        Connection { network: self.clone(), target, addr: node.addr.clone() }
    }
}

impl Connection {
    async fn rpc<Req, Resp, E>(
        &self,
        path: &str,
        request: &Req,
        option: &RPCOption,
    ) -> Result<Resp, RPCError<NodeId, BasicNode, RaftError<NodeId, E>>>
    where
        Req: Serialize,
        Resp: DeserializeOwned,
        E: std::error::Error + DeserializeOwned,
    {
        let unreachable = |message: String| {
            RPCError::Unreachable(Unreachable::new(&std::io::Error::other(message)))
        };
        let client =
            self.network.client(&self.addr).await.map_err(|e| unreachable(e.to_string()))?;
        let body = Bytes::from(
            serde_json::to_vec(request).map_err(|e| RPCError::Network(NetworkError::new(&e)))?,
        );
        let authorization = bearer(&self.network);
        let headers = [("authorization", authorization.as_str())];
        let call = client.call(Method::POST, path, &headers, body);
        let (status, bytes) = match tokio::time::timeout(option.hard_ttl(), call).await {
            Ok(Ok(response)) => response,
            Ok(Err(err)) => {
                self.network.forget(&self.addr);
                return Err(unreachable(err.to_string()));
            }
            Err(_) => return Err(unreachable("the RPC timed out".to_string())),
        };
        if status != StatusCode::OK {
            return Err(RPCError::Network(NetworkError::new(&std::io::Error::other(format!(
                "{path} answered {status}"
            )))));
        }
        let result: Result<Resp, RaftError<NodeId, E>> =
            serde_json::from_slice(&bytes).map_err(|e| RPCError::Network(NetworkError::new(&e)))?;
        result.map_err(|e| RPCError::RemoteError(RemoteError::new(self.target, e)))
    }
}

fn bearer(network: &Network) -> String {
    format!("Bearer {}", network.token.expose())
}

impl<M: BusStateMachine> RaftNetwork<BusTypes<M>> for Connection {
    async fn append_entries(
        &mut self,
        rpc: AppendEntriesRequest<BusTypes<M>>,
        option: RPCOption,
    ) -> Result<AppendEntriesResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        self.rpc(APPEND, &rpc, &option).await
    }

    async fn install_snapshot(
        &mut self,
        rpc: InstallSnapshotRequest<BusTypes<M>>,
        option: RPCOption,
    ) -> Result<
        InstallSnapshotResponse<NodeId>,
        RPCError<NodeId, BasicNode, RaftError<NodeId, InstallSnapshotError>>,
    > {
        self.rpc(SNAPSHOT, &rpc, &option).await
    }

    async fn vote(
        &mut self,
        rpc: VoteRequest<NodeId>,
        option: RPCOption,
    ) -> Result<VoteResponse<NodeId>, RPCError<NodeId, BasicNode, RaftError<NodeId>>> {
        self.rpc(VOTE, &rpc, &option).await
    }

    fn backoff(&self) -> openraft::network::Backoff {
        openraft::network::Backoff::new(std::iter::repeat(Duration::from_millis(200)))
    }
}
