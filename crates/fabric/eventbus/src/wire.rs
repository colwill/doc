//! The messages the Event Bus client and service exchange over HTTP/3.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Delivery, Event};

pub const REGISTER_TOPIC: &str = "/events/v1/register-topic";
pub const PUBLISH: &str = "/events/v1/publish";
pub const SUBSCRIBE: &str = "/events/v1/subscribe";
pub const TOPICS: &str = "/events/v1/topics";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterTopicRequest {
    pub filter: String,
    pub max_events: u64,
    pub max_age_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishRequest {
    pub event: Event,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublishResponse {
    pub id: Uuid,
    pub offset: u64,
    pub duplicate: bool,
}

/// The first line of a subscription's request body; the rest of the body carries acknowledgements.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeRequest {
    pub group: String,
    pub filter: String,
    pub lease_ms: u64,
    pub from_start: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum AckMessage {
    Ack { delivery: Uuid },
    Nack { delivery: Uuid, after_ms: Option<u64> },
}

/// Lines of the subscription's response body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum StreamMessage {
    Ready { node: u64 },
    Delivery(DeliveryMessage),
    Closing { reason: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeliveryMessage {
    pub id: Uuid,
    pub event: Event,
    pub attempt: u32,
}

impl From<DeliveryMessage> for Delivery {
    fn from(message: DeliveryMessage) -> Self {
        Self { id: message.id, event: message.event, attempt: message.attempt }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TopicsResponse {
    pub topics: BTreeMap<String, TopicStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TopicStatus {
    pub retained: u64,
    pub next_offset: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Empty {}
