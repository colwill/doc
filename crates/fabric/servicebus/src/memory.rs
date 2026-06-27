//! In-memory Service Bus: queues with leases, retries and dead letters, and direct request/reply.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::Notify;
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    Address, DeadLetter, Lease, Message, QueueSpec, Request, ServiceBus, ServiceBusError,
    ServiceHandler,
};

#[derive(Default)]
struct Queue {
    ready: VecDeque<Message>,
    inflight: HashMap<Uuid, (Message, Instant)>,
    dead: Vec<DeadLetter>,
}

#[derive(Default)]
struct State {
    queues: HashMap<Address, Queue>,
    specs: HashMap<Address, QueueSpec>,
}

/// How often a served address looks again for messages whose lease has run out.
const RECLAIM_EVERY: Duration = Duration::from_secs(1);

#[derive(Clone, Default)]
pub struct MemoryServiceBus {
    state: Arc<Mutex<State>>,
    handlers: Arc<Mutex<HashMap<Address, Arc<dyn ServiceHandler>>>>,
    /// Wakes a served address's consumer when a message is sent to it.
    arrivals: Arc<Mutex<HashMap<Address, Arc<Notify>>>>,
}

impl MemoryServiceBus {
    pub fn new() -> Self {
        Self::default()
    }

    fn spec(&self, address: &Address) -> QueueSpec {
        self.state
            .lock()
            .specs
            .get(address)
            .cloned()
            .unwrap_or_else(|| QueueSpec::new(address.clone()))
    }

    fn arrivals(&self, address: &Address) -> Arc<Notify> {
        self.arrivals.lock().entry(address.clone()).or_default().clone()
    }

    /// Hands a served address's queued messages to its handler, as the cluster's consumer does.
    fn consume(&self, address: Address) {
        let bus = self.clone();
        let arrived = self.arrivals(&address);
        tokio::spawn(async move {
            loop {
                let Some(handler) = bus.handlers.lock().get(&address).cloned() else { return };
                match bus.receive(&address).await {
                    Ok(Some(lease)) => {
                        let span = crate::handling(&lease.message);
                        let handled = handler.handle(Request::of(&lease.message));
                        let _ = match handled.instrument(span).await {
                            Ok(_) => bus.ack(lease.id).await,
                            Err(reason) => bus.nack(lease.id, &reason).await,
                        };
                    }
                    _ => {
                        let _ = tokio::time::timeout(RECLAIM_EVERY, arrived.notified()).await;
                    }
                }
            }
        });
    }

    /// Moves messages whose lease has expired back to the queue, dead-lettering exhausted ones.
    fn reclaim(&self, address: &Address) {
        let spec = self.spec(address);
        let mut state = self.state.lock();
        let Some(queue) = state.queues.get_mut(address) else {
            return;
        };
        let now = Instant::now();
        let expired: Vec<Uuid> = queue
            .inflight
            .iter()
            .filter(|(_, (_, deadline))| *deadline <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some((message, _)) = queue.inflight.remove(&id) {
                requeue(queue, message, &spec, "lease expired");
            }
        }
    }
}

fn requeue(queue: &mut Queue, mut message: Message, spec: &QueueSpec, reason: &str) {
    message.attempts += 1;
    if message.attempts >= spec.max_attempts {
        queue.dead.push(DeadLetter {
            message,
            reason: format!("{reason} after {} attempts", spec.max_attempts),
            at: Utc::now(),
        });
    } else {
        queue.ready.push_back(message);
    }
}

#[async_trait]
impl ServiceBus for MemoryServiceBus {
    async fn register_queue(&self, spec: QueueSpec) -> Result<(), ServiceBusError> {
        let mut state = self.state.lock();
        state.queues.entry(spec.address.clone()).or_default();
        state.specs.insert(spec.address.clone(), spec);
        Ok(())
    }

    async fn serve(
        &self,
        address: Address,
        handler: Arc<dyn ServiceHandler>,
    ) -> Result<(), ServiceBusError> {
        let first = self.handlers.lock().insert(address.clone(), handler).is_none();
        if first {
            self.consume(address);
        }
        Ok(())
    }

    async fn request_as(
        &self,
        address: &Address,
        subject: &str,
        payload: Value,
        deadline: Duration,
        principal: Option<&str>,
    ) -> Result<Value, ServiceBusError> {
        let handler = self.handlers.lock().get(address).cloned();
        let handler = handler.ok_or_else(|| ServiceBusError::NoHandler(address.clone()))?;
        let request = Request {
            subject: subject.to_string(),
            payload,
            principal: principal.map(str::to_string),
            correlation_id: None,
            trace: doc_telemetry::traceparent(),
        };
        match tokio::time::timeout(deadline, handler.handle(request)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(ServiceBusError::Remote { address: address.clone(), message }),
            Err(_) => Err(ServiceBusError::DeadlineExceeded(address.clone())),
        }
    }

    async fn send(&self, mut message: Message) -> Result<Uuid, ServiceBusError> {
        drop(crate::sending(&mut message));
        let spec = self.spec(&message.to);
        let mut state = self.state.lock();
        let queue = state.queues.entry(message.to.clone()).or_default();
        if queue.ready.len() >= spec.max_depth {
            return Err(ServiceBusError::Unavailable(format!("{} is full", message.to)));
        }
        let id = message.id;
        let to = message.to.clone();
        queue.ready.push_back(message);
        drop(state);
        if let Some(arrived) = self.arrivals.lock().get(&to) {
            arrived.notify_one();
        }
        Ok(id)
    }

    async fn receive(&self, address: &Address) -> Result<Option<Lease>, ServiceBusError> {
        self.reclaim(address);
        let spec = self.spec(address);
        let mut state = self.state.lock();
        let queue = state.queues.entry(address.clone()).or_default();
        let Some(message) = queue.ready.pop_front() else {
            return Ok(None);
        };
        let lease = Uuid::now_v7();
        queue.inflight.insert(lease, (message.clone(), Instant::now() + spec.lease));
        Ok(Some(Lease { id: lease, message }))
    }

    async fn ack(&self, lease: Uuid) -> Result<(), ServiceBusError> {
        let mut state = self.state.lock();
        for queue in state.queues.values_mut() {
            if queue.inflight.remove(&lease).is_some() {
                return Ok(());
            }
        }
        Ok(())
    }

    async fn nack(&self, lease: Uuid, reason: &str) -> Result<(), ServiceBusError> {
        let specs: HashMap<Address, QueueSpec> = self.state.lock().specs.clone();
        let mut state = self.state.lock();
        for (address, queue) in state.queues.iter_mut() {
            if let Some((message, _)) = queue.inflight.remove(&lease) {
                let spec =
                    specs.get(address).cloned().unwrap_or_else(|| QueueSpec::new(address.clone()));
                requeue(queue, message, &spec, reason);
                return Ok(());
            }
        }
        Ok(())
    }

    async fn dead_letters(&self, address: &Address) -> Result<Vec<DeadLetter>, ServiceBusError> {
        Ok(self.state.lock().queues.get(address).map(|q| q.dead.clone()).unwrap_or_default())
    }

    async fn depth(&self, address: &Address) -> Result<usize, ServiceBusError> {
        self.reclaim(address);
        Ok(self.state.lock().queues.get(address).map(|q| q.ready.len()).unwrap_or(0))
    }
}
