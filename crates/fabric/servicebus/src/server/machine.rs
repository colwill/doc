//! The replicated Service Bus state: queues with delayed retries, leases, and dead letters.
//! Every change goes through Raft, and the leader supplies the clock so replicas agree.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use chrono::{TimeZone, Utc};
use doc_consensus::BusStateMachine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{DeadLetter, Message};

const MAX_DEAD_LETTERS: usize = 1_000;
const BASE_BACKOFF_MS: u64 = 200;
const MAX_BACKOFF_MS: u64 = 30_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ServiceCommand {
    RegisterQueue {
        address: String,
        max_attempts: u32,
        lease_ms: u64,
        max_depth: u64,
    },
    Send {
        message: Box<Message>,
    },
    Receive {
        address: String,
        lease_ms: u64,
        now_ms: u64,
        /// Chosen before the command is proposed, so every node records the lease the client holds.
        /// Entries written before it existed get a fresh one each, as they always did.
        #[serde(default = "Uuid::now_v7")]
        lease: Uuid,
    },
    Ack {
        lease: Uuid,
    },
    Nack {
        lease: Uuid,
        reason: String,
        now_ms: u64,
    },
    /// Requeues expired leases and dead-letters messages that ran out of attempts.
    Expire {
        now_ms: u64,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum ServiceResponse {
    #[default]
    Empty,
    Sent {
        id: Uuid,
    },
    Received {
        lease: Option<(Uuid, Box<Message>)>,
    },
    Acked {
        found: bool,
    },
    Expired {
        requeued: u64,
        dead: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueSpecStored {
    pub max_attempts: u32,
    pub lease_ms: u64,
    pub max_depth: u64,
}

impl Default for QueueSpecStored {
    fn default() -> Self {
        Self { max_attempts: 5, lease_ms: 30_000, max_depth: 10_000 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub message: Message,
    /// Retries wait for their backoff before becoming visible again.
    pub visible_at_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Queue {
    pub ready: VecDeque<Pending>,
    pub inflight: BTreeMap<Uuid, (Message, u64)>,
    pub dead: VecDeque<DeadLetter>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServiceState {
    pub queues: BTreeMap<String, Arc<Queue>>,
    pub specs: BTreeMap<String, QueueSpecStored>,
}

#[derive(Default)]
pub struct ServiceMachine {
    state: ServiceState,
}

fn backoff_ms(attempts: u32) -> u64 {
    BASE_BACKOFF_MS.saturating_mul(1 << attempts.min(8)).min(MAX_BACKOFF_MS)
}

impl ServiceMachine {
    pub fn spec(&self, address: &str) -> QueueSpecStored {
        self.state.specs.get(address).cloned().unwrap_or_default()
    }

    pub fn depth(&self, address: &str, now_ms: u64) -> (u64, u64, u64) {
        self.state
            .queues
            .get(address)
            .map(|queue| {
                let ready =
                    queue.ready.iter().filter(|pending| pending.visible_at_ms <= now_ms).count();
                (ready as u64, queue.inflight.len() as u64, queue.dead.len() as u64)
            })
            .unwrap_or((0, 0, 0))
    }

    pub fn dead_letters(&self, address: &str) -> Vec<DeadLetter> {
        self.state
            .queues
            .get(address)
            .map(|queue| queue.dead.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// True when a message could be taken now; one whose lease ran out counts once it is requeued.
    pub fn has_ready(&self, address: &str, now_ms: u64) -> bool {
        self.state
            .queues
            .get(address)
            .is_some_and(|queue| queue.ready.iter().any(|pending| pending.visible_at_ms <= now_ms))
    }

    pub fn needs_expiry(&self, now_ms: u64) -> bool {
        self.state
            .queues
            .values()
            .any(|queue| queue.inflight.values().any(|(_, expires)| *expires <= now_ms))
    }

    pub fn addresses(&self) -> Vec<String> {
        self.state.queues.keys().cloned().collect()
    }

    fn requeue(
        queue: &mut Queue,
        mut message: Message,
        spec: &QueueSpecStored,
        reason: &str,
        now_ms: u64,
    ) -> bool {
        message.attempts += 1;
        if message.attempts >= spec.max_attempts {
            queue.dead.push_back(DeadLetter {
                message,
                reason: format!("{reason} after {} attempts", spec.max_attempts),
                at: Utc.timestamp_millis_opt(now_ms as i64).single().unwrap_or_else(Utc::now),
            });
            while queue.dead.len() > MAX_DEAD_LETTERS {
                queue.dead.pop_front();
            }
            return true;
        }
        let visible_at_ms = now_ms + backoff_ms(message.attempts);
        queue.ready.push_back(Pending { message, visible_at_ms });
        false
    }
}

impl BusStateMachine for ServiceMachine {
    type Command = ServiceCommand;
    type Response = ServiceResponse;
    type Snapshot = ServiceState;

    fn apply(&mut self, _log_index: u64, command: ServiceCommand) -> ServiceResponse {
        match command {
            ServiceCommand::RegisterQueue { address, max_attempts, lease_ms, max_depth } => {
                self.state
                    .specs
                    .insert(address.clone(), QueueSpecStored { max_attempts, lease_ms, max_depth });
                self.state.queues.entry(address).or_default();
                ServiceResponse::Empty
            }
            ServiceCommand::Send { message } => {
                let address = message.to.as_str().to_string();
                let spec = self.spec(&address);
                let id = message.id;
                let queue = Arc::make_mut(self.state.queues.entry(address).or_default());
                if queue.ready.len() as u64 >= spec.max_depth {
                    return ServiceResponse::Empty;
                }
                queue.ready.push_back(Pending { message: *message, visible_at_ms: 0 });
                ServiceResponse::Sent { id }
            }
            ServiceCommand::Receive { address, lease_ms, now_ms, lease } => {
                let spec = self.spec(&address);
                let lease_ms = if lease_ms == 0 { spec.lease_ms } else { lease_ms };
                let Some(entry) = self.state.queues.get_mut(&address) else {
                    return ServiceResponse::Received { lease: None };
                };
                let queue = Arc::make_mut(entry);
                let position =
                    queue.ready.iter().position(|pending| pending.visible_at_ms <= now_ms);
                let Some(position) = position else {
                    return ServiceResponse::Received { lease: None };
                };
                let Some(pending) = queue.ready.remove(position) else {
                    return ServiceResponse::Received { lease: None };
                };
                queue.inflight.insert(lease, (pending.message.clone(), now_ms + lease_ms));
                ServiceResponse::Received { lease: Some((lease, Box::new(pending.message))) }
            }
            ServiceCommand::Ack { lease } => {
                for entry in self.state.queues.values_mut() {
                    if entry.inflight.contains_key(&lease) {
                        Arc::make_mut(entry).inflight.remove(&lease);
                        return ServiceResponse::Acked { found: true };
                    }
                }
                ServiceResponse::Acked { found: false }
            }
            ServiceCommand::Nack { lease, reason, now_ms } => {
                let specs = self.state.specs.clone();
                for (address, entry) in self.state.queues.iter_mut() {
                    if entry.inflight.contains_key(&lease) {
                        let spec = specs.get(address).cloned().unwrap_or_default();
                        let queue = Arc::make_mut(entry);
                        if let Some((message, _)) = queue.inflight.remove(&lease) {
                            Self::requeue(queue, message, &spec, &reason, now_ms);
                        }
                        return ServiceResponse::Acked { found: true };
                    }
                }
                ServiceResponse::Acked { found: false }
            }
            ServiceCommand::Expire { now_ms } => {
                let specs = self.state.specs.clone();
                let (mut requeued, mut dead) = (0, 0);
                for (address, entry) in self.state.queues.iter_mut() {
                    let expired: Vec<Uuid> = entry
                        .inflight
                        .iter()
                        .filter(|(_, (_, expires))| *expires <= now_ms)
                        .map(|(lease, _)| *lease)
                        .collect();
                    if expired.is_empty() {
                        continue;
                    }
                    let spec = specs.get(address).cloned().unwrap_or_default();
                    let queue = Arc::make_mut(entry);
                    for lease in expired {
                        if let Some((message, _)) = queue.inflight.remove(&lease) {
                            if Self::requeue(queue, message, &spec, "lease expired", now_ms) {
                                dead += 1;
                            } else {
                                requeued += 1;
                            }
                        }
                    }
                }
                ServiceResponse::Expired { requeued, dead }
            }
        }
    }

    fn snapshot(&self) -> ServiceState {
        self.state.clone()
    }

    fn restore(&mut self, snapshot: ServiceState) {
        self.state = snapshot;
    }
}
