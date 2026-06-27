//! The Service Bus service: queue routes over HTTP/3, plus the leader's expiry loop that
//! returns messages whose lease ran out.

pub mod machine;

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use doc_consensus::node::{BusService, is_leader, node_error};
use doc_consensus::{Node, NodeError};
use doc_transport::{H3ServerStream, read_body, respond_json};
use http::{Request, StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::wire::{
    self, AckRequest, AddressRequest, DeadLettersResponse, DepthResponse, LeasedMessage,
    NackRequest, ReceiveRequest, ReceiveResponse, RegisterQueueRequest, SendRequest, SendResponse,
    WaitRequest, WaitResponse,
};
use machine::{ServiceCommand, ServiceMachine, ServiceResponse};

const EXPIRY_INTERVAL: Duration = Duration::from_millis(250);
const POLL_INTERVAL: Duration = Duration::from_millis(25);
const MAX_WAIT: Duration = Duration::from_secs(30);

pub fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

pub struct ServiceBusService {
    node: Arc<Node<ServiceMachine>>,
}

impl ServiceBusService {
    pub fn new(node: Arc<Node<ServiceMachine>>) -> Arc<Self> {
        let service = Arc::new(Self { node });
        tokio::spawn(service.clone().expire_leases());
        service
    }

    async fn write(&self, command: ServiceCommand) -> Result<ServiceResponse, NodeError> {
        self.node.write(command).await.map(|applied| applied.response)
    }

    /// Only the leader proposes expiry, and only when a lease has actually run out.
    async fn expire_leases(self: Arc<Self>) {
        loop {
            tokio::time::sleep(EXPIRY_INTERVAL).await;
            let now = now_ms();
            let needed = self.node.state().read(|machine| machine.needs_expiry(now));
            if !needed {
                continue;
            }
            match self.write(ServiceCommand::Expire { now_ms: now }).await {
                Ok(ServiceResponse::Expired { requeued, dead }) if requeued + dead > 0 => {
                    tracing::info!(requeued, dead, "leases expired");
                }
                Ok(_) => {}
                Err(NodeError::NotLeader(_)) => {}
                Err(err) => tracing::warn!(%err, "expiring leases failed"),
            }
        }
    }

    /// Takes a message at once; a leader with none ready answers without writing to the log.
    async fn receive(&self, request: ReceiveRequest) -> Result<(StatusCode, Value)> {
        let leading = is_leader(self.node.raft().metrics().borrow().state);
        let ready = self.node.state().read(|machine| machine.has_ready(&request.address, now_ms()));
        if leading && !ready {
            return Ok((StatusCode::OK, serde_json::to_value(ReceiveResponse { lease: None })?));
        }
        let command = ServiceCommand::Receive {
            address: request.address,
            lease_ms: request.lease_ms,
            now_ms: now_ms(),
            lease: Uuid::now_v7(),
        };
        Ok(match self.write(command).await {
            Ok(ServiceResponse::Received { lease }) => {
                let lease = lease.map(|(id, message)| LeasedMessage { id, message: *message });
                (StatusCode::OK, serde_json::to_value(ReceiveResponse { lease })?)
            }
            Ok(_) => (StatusCode::OK, serde_json::to_value(ReceiveResponse { lease: None })?),
            Err(err) => node_error(err),
        })
    }

    /// Only reads, so a wait whose client has gone away cannot take a message nobody will handle.
    async fn wait(&self, request: WaitRequest) -> Result<(StatusCode, Value)> {
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(request.wait_ms).min(MAX_WAIT);
        loop {
            let ready =
                self.node.state().read(|machine| machine.has_ready(&request.address, now_ms()));
            if ready || tokio::time::Instant::now() >= deadline {
                return Ok((StatusCode::OK, serde_json::to_value(WaitResponse { ready })?));
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
    }

    async fn route(&self, path: &str, body: &[u8]) -> Result<(StatusCode, Value)> {
        Ok(match path {
            wire::REGISTER_QUEUE => {
                let request: RegisterQueueRequest = serde_json::from_slice(body)?;
                match self
                    .write(ServiceCommand::RegisterQueue {
                        address: request.address,
                        max_attempts: request.max_attempts,
                        lease_ms: request.lease_ms,
                        max_depth: request.max_depth,
                    })
                    .await
                {
                    Ok(_) => (StatusCode::OK, json!({})),
                    Err(err) => node_error(err),
                }
            }
            wire::SEND => {
                let request: SendRequest = serde_json::from_slice(body)?;
                match self.write(ServiceCommand::Send { message: Box::new(request.message) }).await
                {
                    Ok(ServiceResponse::Sent { id }) => {
                        (StatusCode::OK, serde_json::to_value(SendResponse { id })?)
                    }
                    Ok(_) => {
                        (StatusCode::SERVICE_UNAVAILABLE, json!({"error": "the queue is full"}))
                    }
                    Err(err) => node_error(err),
                }
            }
            wire::RECEIVE => {
                let request: ReceiveRequest = serde_json::from_slice(body)?;
                self.receive(request).await?
            }
            wire::WAIT => {
                let request: WaitRequest = serde_json::from_slice(body)?;
                self.wait(request).await?
            }
            wire::ACK => {
                let request: AckRequest = serde_json::from_slice(body)?;
                match self.write(ServiceCommand::Ack { lease: request.lease }).await {
                    Ok(ServiceResponse::Acked { found }) => {
                        (StatusCode::OK, json!({"found": found}))
                    }
                    Ok(_) => (StatusCode::OK, json!({"found": false})),
                    Err(err) => node_error(err),
                }
            }
            wire::NACK => {
                let request: NackRequest = serde_json::from_slice(body)?;
                let command = ServiceCommand::Nack {
                    lease: request.lease,
                    reason: request.reason,
                    now_ms: now_ms(),
                };
                match self.write(command).await {
                    Ok(ServiceResponse::Acked { found }) => {
                        (StatusCode::OK, json!({"found": found}))
                    }
                    Ok(_) => (StatusCode::OK, json!({"found": false})),
                    Err(err) => node_error(err),
                }
            }
            wire::DEAD_LETTERS => {
                let request: AddressRequest = serde_json::from_slice(body)?;
                let dead_letters =
                    self.node.state().read(|machine| machine.dead_letters(&request.address));
                (StatusCode::OK, serde_json::to_value(DeadLettersResponse { dead_letters })?)
            }
            wire::DEPTH => {
                let request: AddressRequest = serde_json::from_slice(body)?;
                let (ready, in_flight, dead) =
                    self.node.state().read(|machine| machine.depth(&request.address, now_ms()));
                (StatusCode::OK, serde_json::to_value(DepthResponse { ready, in_flight, dead })?)
            }
            _ => (StatusCode::NOT_FOUND, json!({"error": "unknown service bus route"})),
        })
    }
}

impl BusService for ServiceBusService {
    async fn handle(&self, request: Request<()>, mut stream: H3ServerStream) -> Result<()> {
        let path = request.uri().path().to_string();
        let body = read_body(&mut stream).await?;
        let (status, value) = self.route(&path, &body).await?;
        respond_json(&mut stream, status, &value).await
    }
}
