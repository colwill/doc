//! The messages the Service Bus client and service exchange over HTTP/3.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{DeadLetter, Message};

pub const REGISTER_QUEUE: &str = "/services/v1/register-queue";
pub const SEND: &str = "/services/v1/send";
pub const RECEIVE: &str = "/services/v1/receive";
pub const WAIT: &str = "/services/v1/wait";
pub const ACK: &str = "/services/v1/ack";
pub const NACK: &str = "/services/v1/nack";
pub const DEAD_LETTERS: &str = "/services/v1/dead-letters";
pub const DEPTH: &str = "/services/v1/depth";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterQueueRequest {
    pub address: String,
    pub max_attempts: u32,
    pub lease_ms: u64,
    pub max_depth: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendRequest {
    pub message: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendResponse {
    pub id: Uuid,
}

/// Takes a message at once; waiting for one is `WaitRequest`'s job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiveRequest {
    pub address: String,
    pub lease_ms: u64,
}

/// Held by the leader until a message is ready or `wait_ms` passes; waiting never takes one.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitRequest {
    pub address: String,
    pub wait_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitResponse {
    pub ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReceiveResponse {
    pub lease: Option<LeasedMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LeasedMessage {
    pub id: Uuid,
    pub message: Message,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AckRequest {
    pub lease: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NackRequest {
    pub lease: Uuid,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AddressRequest {
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLettersResponse {
    pub dead_letters: Vec<DeadLetter>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DepthResponse {
    pub ready: u64,
    pub in_flight: u64,
    pub dead: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Empty {}
