//! Service Bus: addresses, queued messages with leases and retries, and request/reply.

pub mod memory;
pub mod network;
pub mod postgres;
#[cfg(feature = "server")]
pub mod server;
pub mod wire;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub use memory::MemoryServiceBus;
pub use network::NetworkServiceBus;
pub use postgres::PostgresServiceBus;

#[derive(Debug, thiserror::Error)]
pub enum ServiceBusError {
    #[error("invalid address: {0}")]
    InvalidAddress(String),
    #[error("nothing is serving {0}")]
    NoHandler(Address),
    #[error("deadline exceeded calling {0}")]
    DeadlineExceeded(Address),
    #[error("service bus unavailable: {0}")]
    Unavailable(String),
    #[error("{address} failed: {message}")]
    Remote { address: Address, message: String },
}

/// `core.<service>` for platform services, `plugin.<id>` for plugins.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Address(String);

impl Address {
    pub fn new(address: impl Into<String>) -> Result<Self, ServiceBusError> {
        let address = address.into();
        let segments: Vec<&str> = address.split('.').collect();
        let prefix_ok = matches!(segments.first(), Some(&"core") | Some(&"plugin"));
        let segments_ok = segments.len() >= 2
            && segments.iter().all(|segment| {
                !segment.is_empty()
                    && segment
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            });
        if prefix_ok && segments_ok {
            Ok(Self(address))
        } else {
            Err(ServiceBusError::InvalidAddress(address))
        }
    }

    pub fn core(service: &str) -> Result<Self, ServiceBusError> {
        Self::new(format!("core.{service}"))
    }

    pub fn plugin(id: &str) -> Result<Self, ServiceBusError> {
        Self::new(format!("plugin.{id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Address {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Address {
    type Error = ServiceBusError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Address> for String {
    fn from(address: Address) -> Self {
        address.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub to: Address,
    pub subject: String,
    pub payload: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<Uuid>,
    /// Where the answer goes; request/reply sets it to the caller's own reply queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<Address>,
    /// The principal the work runs as; plugins may only act for the caller they were given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deadline: Option<DateTime<Utc>>,
    pub attempts: u32,
    /// The sender's `traceparent`, set on sending, so handling the message continues its trace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trace: Option<String>,
}

impl Message {
    pub fn new(to: Address, subject: impl Into<String>, payload: Value) -> Self {
        Self {
            id: Uuid::now_v7(),
            to,
            subject: subject.into(),
            payload,
            correlation_id: None,
            reply_to: None,
            principal: None,
            deadline: None,
            attempts: 0,
            trace: None,
        }
    }

    pub fn as_principal(mut self, principal: impl Into<String>) -> Self {
        self.principal = Some(principal.into());
        self
    }

    pub fn correlate(mut self, correlation_id: Uuid) -> Self {
        self.correlation_id = Some(correlation_id);
        self
    }
}

/// A message handed to a consumer until its lease expires.
#[derive(Debug, Clone)]
pub struct Lease {
    pub id: Uuid,
    pub message: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetter {
    pub message: Message,
    pub reason: String,
    pub at: DateTime<Utc>,
}

/// What the backend registers for a queue: retries, lease length and dead-lettering.
#[derive(Debug, Clone)]
pub struct QueueSpec {
    pub address: Address,
    pub max_attempts: u32,
    pub lease: Duration,
    pub max_depth: usize,
}

impl QueueSpec {
    pub fn new(address: Address) -> Self {
        Self { address, max_attempts: 5, lease: Duration::from_secs(30), max_depth: 10_000 }
    }

    pub fn with_attempts(mut self, max_attempts: u32) -> Self {
        self.max_attempts = max_attempts;
        self
    }

    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }
}

#[derive(Debug, Clone)]
pub struct Request {
    pub subject: String,
    pub payload: Value,
    pub principal: Option<String>,
    pub correlation_id: Option<Uuid>,
    pub trace: Option<String>,
}

impl Request {
    fn of(message: &Message) -> Self {
        Self {
            subject: message.subject.clone(),
            payload: message.payload.clone(),
            principal: message.principal.clone(),
            correlation_id: message.correlation_id,
            trace: message.trace.clone(),
        }
    }
}

/// The span sending runs in; the message carries it, unless it already carries a trace.
fn sending(message: &mut Message) -> tracing::Span {
    let span = match doc_telemetry::in_trace() {
        true => tracing::info_span!(
            target: "doc",
            "servicebus.send",
            otel.name = %format!("{} {} send", message.to, message.subject),
            otel.kind = "producer",
            messaging.system = "doc-servicebus",
            messaging.destination.name = %message.to,
            messaging.message.id = %message.id,
        ),
        false => tracing::Span::none(),
    };
    if message.trace.is_none() {
        message.trace = span.in_scope(doc_telemetry::traceparent);
    }
    span
}

/// The span a handler runs in, continuing the trace of whoever sent the message.
pub fn handling(message: &Message) -> tracing::Span {
    let span = match message.trace {
        Some(_) => tracing::info_span!(
            target: "doc",
            "servicebus.process",
            otel.name = %format!("{} {} process", message.to, message.subject),
            otel.kind = "consumer",
            messaging.system = "doc-servicebus",
            messaging.destination.name = %message.to,
            messaging.message.id = %message.id,
            messaging.delivery.attempts = message.attempts,
        ),
        None => tracing::Span::none(),
    };
    doc_telemetry::adopt(&span, message.trace.as_deref());
    span
}

#[async_trait]
pub trait ServiceHandler: Send + Sync {
    async fn handle(&self, request: Request) -> Result<Value, String>;
}

#[async_trait]
pub trait ServiceBus: Send + Sync {
    async fn register_queue(&self, spec: QueueSpec) -> Result<(), ServiceBusError>;

    /// Answers requests sent to this address until the bus is dropped.
    async fn serve(
        &self,
        address: Address,
        handler: Arc<dyn ServiceHandler>,
    ) -> Result<(), ServiceBusError>;

    async fn request(
        &self,
        address: &Address,
        subject: &str,
        payload: Value,
        deadline: Duration,
    ) -> Result<Value, ServiceBusError> {
        self.request_as(address, subject, payload, deadline, None).await
    }

    /// A request made on someone's behalf. The handler sees `principal`, so a service checks the
    /// caller a plugin is acting for rather than the plugin that relayed it.
    async fn request_as(
        &self,
        address: &Address,
        subject: &str,
        payload: Value,
        deadline: Duration,
        principal: Option<&str>,
    ) -> Result<Value, ServiceBusError>;

    async fn send(&self, message: Message) -> Result<Uuid, ServiceBusError>;

    /// Takes the next message, leased for the queue's visibility timeout.
    async fn receive(&self, address: &Address) -> Result<Option<Lease>, ServiceBusError>;

    async fn ack(&self, lease: Uuid) -> Result<(), ServiceBusError>;

    /// Returns the message to the queue, dead-lettering it once attempts run out.
    async fn nack(&self, lease: Uuid, reason: &str) -> Result<(), ServiceBusError>;

    async fn dead_letters(&self, address: &Address) -> Result<Vec<DeadLetter>, ServiceBusError>;

    async fn depth(&self, address: &Address) -> Result<usize, ServiceBusError>;
}
