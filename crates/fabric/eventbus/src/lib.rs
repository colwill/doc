//! Event Bus: the envelope, topic matching, the client trait and an in-memory implementation.
//! Delivery is at least once: an event stays in flight until it is acknowledged.

pub mod memory;
pub mod network;
pub mod postgres;
#[cfg(feature = "server")]
pub mod server;
pub mod wire;

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub use memory::MemoryEventBus;
pub use network::NetworkEventBus;
pub use postgres::PostgresEventBus;

#[derive(Debug, thiserror::Error)]
pub enum EventBusError {
    #[error("invalid topic: {0}")]
    InvalidTopic(String),
    #[error("event bus unavailable: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Other(String),
}

/// A dot-separated topic such as `platform.plugin.kb.state`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Topic(String);

impl Topic {
    pub fn new(topic: impl Into<String>) -> Result<Self, EventBusError> {
        let topic = topic.into();
        let valid = !topic.is_empty()
            && topic.split('.').all(|segment| {
                !segment.is_empty()
                    && segment
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            });
        if valid { Ok(Self(topic)) } else { Err(EventBusError::InvalidTopic(topic)) }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn starts_with(&self, prefix: &str) -> bool {
        self.0 == prefix || self.0.starts_with(&format!("{prefix}."))
    }
}

impl std::fmt::Display for Topic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Topic {
    type Error = EventBusError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Topic> for String {
    fn from(topic: Topic) -> Self {
        topic.0
    }
}

/// A subscription pattern: `*` matches one segment, `>` matches the rest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TopicFilter(String);

impl TopicFilter {
    pub fn new(filter: impl Into<String>) -> Result<Self, EventBusError> {
        let filter = filter.into();
        let segments: Vec<&str> = filter.split('.').collect();
        let valid = !filter.is_empty()
            && segments.iter().enumerate().all(|(i, segment)| match *segment {
                ">" => i + 1 == segments.len(),
                "*" => true,
                other => {
                    !other.is_empty()
                        && other
                            .chars()
                            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                }
            });
        if valid { Ok(Self(filter)) } else { Err(EventBusError::InvalidTopic(filter)) }
    }

    pub fn matches(&self, topic: &Topic) -> bool {
        let mut pattern = self.0.split('.');
        let mut actual = topic.as_str().split('.');
        loop {
            match (pattern.next(), actual.next()) {
                (Some(">"), Some(_)) => return true,
                (Some("*"), Some(_)) => continue,
                (Some(p), Some(a)) if p == a => continue,
                (None, None) => return true,
                _ => return false,
            }
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for TopicFilter {
    type Error = EventBusError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<TopicFilter> for String {
    fn from(filter: TopicFilter) -> Self {
        filter.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub topic: Topic,
    pub source: String,
    pub time: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub correlation_id: Option<Uuid>,
    pub schema_version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    pub payload: Value,
}

impl Event {
    pub fn new(topic: Topic, source: impl Into<String>, payload: Value) -> Self {
        Self {
            id: Uuid::now_v7(),
            topic,
            source: source.into(),
            time: Utc::now(),
            correlation_id: None,
            schema_version: 1,
            idempotency_key: None,
            payload,
        }
    }

    pub fn correlate(mut self, correlation_id: Uuid) -> Self {
        self.correlation_id = Some(correlation_id);
        self
    }

    pub fn idempotent(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }
}

/// A topic the bus has seen, for anything that asks what is published here: `retained` is how
/// many events it still holds, `published` how many have ever been published on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TopicReport {
    pub topic: Topic,
    pub retained: u64,
    pub published: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishAck {
    pub id: Uuid,
    pub offset: u64,
    /// True when an idempotency key matched an event that was already published.
    pub duplicate: bool,
}

/// What the backend registers for a topic pattern: how much history the bus keeps.
#[derive(Debug, Clone)]
pub struct TopicSpec {
    pub filter: TopicFilter,
    pub max_events: usize,
    pub max_age: Duration,
}

impl TopicSpec {
    pub fn new(filter: TopicFilter, max_events: usize, max_age: Duration) -> Self {
        Self { filter, max_events, max_age }
    }
}

#[derive(Debug, Clone)]
pub struct Delivery {
    pub id: Uuid,
    pub event: Event,
    pub attempt: u32,
}

/// A consumer group shares one cursor: each event goes to one member of the group.
#[derive(Debug, Clone)]
pub struct ConsumerGroup {
    pub name: String,
    pub filter: TopicFilter,
    pub lease: Duration,
    /// Start at the oldest retained event rather than at the next one published.
    pub from_start: bool,
}

impl ConsumerGroup {
    pub fn new(name: impl Into<String>, filter: TopicFilter) -> Self {
        Self { name: name.into(), filter, lease: Duration::from_secs(30), from_start: true }
    }
}

#[async_trait]
pub trait Subscription: Send + Sync {
    /// Waits for the next event, or returns `None` once the bus is closed.
    async fn next(&mut self) -> Option<Delivery>;
    async fn ack(&mut self, delivery: Uuid) -> Result<(), EventBusError>;
    /// Hands the event back for redelivery, optionally after a delay.
    async fn nack(&mut self, delivery: Uuid, after: Option<Duration>) -> Result<(), EventBusError>;
}

#[async_trait]
pub trait EventBus: Send + Sync {
    async fn register_topic(&self, spec: TopicSpec) -> Result<(), EventBusError>;
    async fn publish(&self, event: Event) -> Result<PublishAck, EventBusError>;
    async fn subscribe(&self, group: ConsumerGroup)
    -> Result<Box<dyn Subscription>, EventBusError>;

    /// Every topic published here, in topic order. Everything the platform and its plugins
    /// announce goes through the bus, so this is the list of what there is to subscribe to.
    async fn topics(&self) -> Result<Vec<TopicReport>, EventBusError>;
}
