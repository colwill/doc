//! The Cache Bus service: writes go through Raft, reads are local unless the caller asks for a
//! consistent one, and the leader evicts expired entries on a timer.

pub mod machine;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use doc_consensus::node::{BusService, node_error};
use doc_consensus::{Node, NodeError};
use doc_transport::{H3ServerStream, read_body, respond_json};
use http::{Request, StatusCode};
use serde_json::{Value, json};

use crate::wire::{
    self, ClearResponse, CompareAndSetRequest, DeleteResponse, EntryResponse, EntryWire,
    GetRequest, KeyRequest, NamespaceRequest, RegisterNamespaceRequest, SetRequest,
};
use machine::{CacheCommand, CacheMachine, CacheResponse, StoredEntry};

const EVICT_INTERVAL: Duration = Duration::from_secs(1);

pub fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

fn wire_entry(entry: StoredEntry) -> EntryWire {
    EntryWire { value: entry.value, version: entry.version, expires_at_ms: entry.expires_at_ms }
}

pub struct CacheBusService {
    node: Arc<Node<CacheMachine>>,
}

impl CacheBusService {
    pub fn new(node: Arc<Node<CacheMachine>>) -> Arc<Self> {
        let service = Arc::new(Self { node });
        tokio::spawn(service.clone().evict());
        service
    }

    async fn write(&self, command: CacheCommand) -> Result<CacheResponse, NodeError> {
        self.node.write(command).await.map(|applied| applied.response)
    }

    async fn evict(self: Arc<Self>) {
        loop {
            tokio::time::sleep(EVICT_INTERVAL).await;
            let now = now_ms();
            if !self.node.state().read(|machine| machine.needs_eviction(now)) {
                continue;
            }
            match self.write(CacheCommand::Evict { now_ms: now }).await {
                Ok(CacheResponse::Evicted { expired, evicted }) if expired + evicted > 0 => {
                    tracing::debug!(expired, evicted, "cache entries removed");
                }
                Ok(_) | Err(NodeError::NotLeader(_)) => {}
                Err(err) => tracing::warn!(%err, "evicting cache entries failed"),
            }
        }
    }

    async fn route(&self, path: &str, body: &[u8]) -> Result<(StatusCode, Value)> {
        Ok(match path {
            wire::REGISTER_NAMESPACE => {
                let request: RegisterNamespaceRequest = serde_json::from_slice(body)?;
                match self
                    .write(CacheCommand::RegisterNamespace {
                        namespace: request.namespace,
                        default_ttl_ms: request.default_ttl_ms,
                        max_entries: request.max_entries,
                    })
                    .await
                {
                    Ok(_) => (StatusCode::OK, json!({})),
                    Err(err) => node_error(err),
                }
            }
            wire::GET => {
                let request: GetRequest = serde_json::from_slice(body)?;
                if request.consistent
                    && let Err(err) = self.node.ensure_linearizable().await
                {
                    return Ok(node_error(err));
                }
                let entry = self
                    .node
                    .state()
                    .read(|machine| machine.get(&request.namespace, &request.key, now_ms()));
                (
                    StatusCode::OK,
                    serde_json::to_value(EntryResponse { entry: entry.map(wire_entry) })?,
                )
            }
            wire::SET => {
                let request: SetRequest = serde_json::from_slice(body)?;
                let command = CacheCommand::Set {
                    namespace: request.namespace,
                    key: request.key,
                    value: request.value,
                    ttl_ms: request.ttl_ms,
                    now_ms: now_ms(),
                };
                match self.write(command).await {
                    Ok(CacheResponse::Written { entry }) => (
                        StatusCode::OK,
                        serde_json::to_value(EntryResponse { entry: Some(wire_entry(entry)) })?,
                    ),
                    Ok(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "unexpected reply"}))
                    }
                    Err(err) => node_error(err),
                }
            }
            wire::DELETE => {
                let request: KeyRequest = serde_json::from_slice(body)?;
                let command = CacheCommand::Delete {
                    namespace: request.namespace,
                    key: request.key,
                    now_ms: now_ms(),
                };
                match self.write(command).await {
                    Ok(CacheResponse::Deleted { existed }) => {
                        (StatusCode::OK, serde_json::to_value(DeleteResponse { existed })?)
                    }
                    Ok(_) => {
                        (StatusCode::OK, serde_json::to_value(DeleteResponse { existed: false })?)
                    }
                    Err(err) => node_error(err),
                }
            }
            wire::COMPARE_AND_SET => {
                let request: CompareAndSetRequest = serde_json::from_slice(body)?;
                let command = CacheCommand::CompareAndSet {
                    namespace: request.namespace,
                    key: request.key,
                    expected: request.expected,
                    value: request.value,
                    ttl_ms: request.ttl_ms,
                    now_ms: now_ms(),
                };
                match self.write(command).await {
                    Ok(CacheResponse::Written { entry }) => (
                        StatusCode::OK,
                        serde_json::to_value(EntryResponse { entry: Some(wire_entry(entry)) })?,
                    ),
                    Ok(CacheResponse::Rejected) => {
                        (StatusCode::OK, serde_json::to_value(EntryResponse { entry: None })?)
                    }
                    Ok(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "unexpected reply"}))
                    }
                    Err(err) => node_error(err),
                }
            }
            wire::CLEAR => {
                let request: NamespaceRequest = serde_json::from_slice(body)?;
                match self.write(CacheCommand::Clear { namespace: request.namespace }).await {
                    Ok(CacheResponse::Cleared { removed }) => {
                        (StatusCode::OK, serde_json::to_value(ClearResponse { removed })?)
                    }
                    Ok(_) => (StatusCode::OK, serde_json::to_value(ClearResponse { removed: 0 })?),
                    Err(err) => node_error(err),
                }
            }
            _ => (StatusCode::NOT_FOUND, json!({"error": "unknown cache bus route"})),
        })
    }
}

impl BusService for CacheBusService {
    async fn handle(&self, request: Request<()>, mut stream: H3ServerStream) -> Result<()> {
        let path = request.uri().path().to_string();
        let body = read_body(&mut stream).await?;
        let (status, value) = self.route(&path, &body).await?;
        respond_json(&mut stream, status, &value).await
    }
}
