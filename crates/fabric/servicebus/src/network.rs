//! The networked Service Bus client: the T07 trait over the cluster. Requests carry a reply
//! address; one receive loop per client dispatches replies to the caller waiting on them.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use doc_consensus::{ClusterClient, ClusterError};
use parking_lot::Mutex;
use serde_json::{Value, json};
use tokio::sync::oneshot;
use tracing::Instrument;
use uuid::Uuid;

use crate::wire::{
    self, AckRequest, AddressRequest, DeadLettersResponse, DepthResponse, Empty, NackRequest,
    ReceiveRequest, ReceiveResponse, RegisterQueueRequest, SendRequest, SendResponse, WaitRequest,
    WaitResponse,
};
use crate::{
    Address, DeadLetter, Lease, Message, QueueSpec, Request, ServiceBus, ServiceBusError,
    ServiceHandler,
};

const CALL_DEADLINE: Duration = Duration::from_secs(10);
const REPLY_WAIT: Duration = Duration::from_secs(5);
const SUBJECT_REPLY: &str = "reply";

fn failed(error: &ClusterError) -> ServiceBusError {
    ServiceBusError::Unavailable(error.to_string())
}

struct Waiting {
    replies: Mutex<HashMap<Uuid, oneshot::Sender<Value>>>,
}

#[derive(Clone)]
pub struct NetworkServiceBus {
    client: ClusterClient,
    reply_to: Address,
    waiting: Arc<Waiting>,
}

impl NetworkServiceBus {
    /// Starts the client and its reply loop; `name` keeps reply addresses readable in logs.
    pub async fn connect(client: ClusterClient, name: &str) -> Result<Self, ServiceBusError> {
        let suffix = Uuid::now_v7().simple().to_string();
        let reply_to = Address::new(format!("core.reply-{name}-{}", &suffix[..12]))?;
        let bus = Self {
            client,
            reply_to: reply_to.clone(),
            waiting: Arc::new(Waiting { replies: Mutex::new(HashMap::new()) }),
        };
        bus.register_queue(QueueSpec::new(reply_to).with_attempts(1)).await?;
        tokio::spawn(bus.clone().dispatch_replies());
        Ok(bus)
    }

    pub fn reply_address(&self) -> &Address {
        &self.reply_to
    }

    async fn dispatch_replies(self) {
        loop {
            match self.receive_with_wait(&self.reply_to.clone(), REPLY_WAIT).await {
                Ok(Some(lease)) => {
                    let correlation = lease.message.correlation_id;
                    let _ = self.ack(lease.id).await;
                    if let Some(correlation) = correlation
                        && let Some(sender) = self.waiting.replies.lock().remove(&correlation)
                    {
                        let _ = sender.send(lease.message.payload);
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::debug!(%err, "waiting for replies failed");
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            }
        }
    }

    /// Waits without taking, then takes, so a wait left behind by a dead client strands nothing.
    async fn receive_with_wait(
        &self,
        address: &Address,
        wait: Duration,
    ) -> Result<Option<Lease>, ServiceBusError> {
        let until = tokio::time::Instant::now() + wait;
        loop {
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            if !left.is_zero() && !self.wait_for(address, left).await? {
                return Ok(None);
            }
            if let Some(lease) = self.take(address).await? {
                return Ok(Some(lease));
            }
            if tokio::time::Instant::now() >= until {
                return Ok(None);
            }
        }
    }

    async fn take(&self, address: &Address) -> Result<Option<Lease>, ServiceBusError> {
        let request = ReceiveRequest { address: address.as_str().to_string(), lease_ms: 0 };
        let response: ReceiveResponse = self
            .client
            .call(wire::RECEIVE, &request, CALL_DEADLINE)
            .await
            .map_err(|e| failed(&e))?;
        Ok(response.lease.map(|lease| Lease { id: lease.id, message: lease.message }))
    }

    async fn wait_for(&self, address: &Address, wait: Duration) -> Result<bool, ServiceBusError> {
        let request =
            WaitRequest { address: address.as_str().to_string(), wait_ms: wait.as_millis() as u64 };
        let deadline = wait + CALL_DEADLINE;
        let response: WaitResponse = self
            .client
            .call_waiting(wire::WAIT, &request, deadline, deadline)
            .await
            .map_err(|e| failed(&e))?;
        Ok(response.ready)
    }

    /// Answers requests sent to `address` until the returned task is dropped.
    pub fn consume(
        &self,
        address: Address,
        handler: Arc<dyn ServiceHandler>,
    ) -> tokio::task::JoinHandle<()> {
        let bus = self.clone();
        tokio::spawn(async move {
            loop {
                match bus.receive_with_wait(&address, REPLY_WAIT).await {
                    Ok(Some(lease)) => {
                        let span = crate::handling(&lease.message);
                        bus.answer(lease, handler.as_ref()).instrument(span).await;
                    }
                    Ok(None) => {}
                    Err(err) => {
                        tracing::debug!(%err, address = %address, "receiving failed");
                        tokio::time::sleep(Duration::from_millis(250)).await;
                    }
                }
            }
        })
    }
}

impl NetworkServiceBus {
    async fn answer(&self, lease: Lease, handler: &dyn ServiceHandler) {
        let answer = handler.handle(Request::of(&lease.message)).await;
        if let Some(reply_to) = lease.message.reply_to.clone() {
            let payload = match &answer {
                Ok(value) => json!({"ok": value}),
                Err(message) => json!({"error": message}),
            };
            let mut reply = Message::new(reply_to, SUBJECT_REPLY, payload);
            reply.correlation_id = lease.message.correlation_id;
            let _ = self.send(reply).await;
        }
        let _ = match answer {
            Ok(_) => self.ack(lease.id).await,
            Err(reason) => self.nack(lease.id, &reason).await,
        };
    }
}

#[async_trait]
impl ServiceBus for NetworkServiceBus {
    async fn register_queue(&self, spec: QueueSpec) -> Result<(), ServiceBusError> {
        let request = RegisterQueueRequest {
            address: spec.address.as_str().to_string(),
            max_attempts: spec.max_attempts,
            lease_ms: spec.lease.as_millis() as u64,
            max_depth: spec.max_depth as u64,
        };
        let _: Empty = self
            .client
            .call(wire::REGISTER_QUEUE, &request, CALL_DEADLINE)
            .await
            .map_err(|e| failed(&e))?;
        Ok(())
    }

    async fn serve(
        &self,
        address: Address,
        handler: Arc<dyn ServiceHandler>,
    ) -> Result<(), ServiceBusError> {
        self.register_queue(QueueSpec::new(address.clone())).await?;
        self.consume(address, handler);
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
        let correlation = Uuid::now_v7();
        let (sender, receiver) = oneshot::channel();
        self.waiting.replies.lock().insert(correlation, sender);

        let mut message = Message::new(address.clone(), subject, payload);
        message.principal = principal.map(str::to_string);
        message.correlation_id = Some(correlation);
        message.reply_to = Some(self.reply_to.clone());
        message.deadline =
            Some(chrono::Utc::now() + chrono::Duration::from_std(deadline).unwrap_or_default());
        if let Err(err) = self.send(message).await {
            self.waiting.replies.lock().remove(&correlation);
            return Err(err);
        }

        match tokio::time::timeout(deadline, receiver).await {
            Ok(Ok(value)) => match value.get("error") {
                Some(Value::String(message)) => Err(ServiceBusError::Remote {
                    address: address.clone(),
                    message: message.clone(),
                }),
                _ => Ok(value.get("ok").cloned().unwrap_or(value)),
            },
            Ok(Err(_)) => Err(ServiceBusError::Unavailable("the reply loop stopped".into())),
            Err(_) => {
                self.waiting.replies.lock().remove(&correlation);
                Err(ServiceBusError::DeadlineExceeded(address.clone()))
            }
        }
    }

    async fn send(&self, mut message: Message) -> Result<Uuid, ServiceBusError> {
        let span = crate::sending(&mut message);
        let response: SendResponse = self
            .client
            .call(wire::SEND, &SendRequest { message }, CALL_DEADLINE)
            .instrument(span)
            .await
            .map_err(|e| failed(&e))?;
        Ok(response.id)
    }

    async fn receive(&self, address: &Address) -> Result<Option<Lease>, ServiceBusError> {
        self.receive_with_wait(address, Duration::ZERO).await
    }

    async fn ack(&self, lease: Uuid) -> Result<(), ServiceBusError> {
        let _: Value = self
            .client
            .call(wire::ACK, &AckRequest { lease }, CALL_DEADLINE)
            .await
            .map_err(|e| failed(&e))?;
        Ok(())
    }

    async fn nack(&self, lease: Uuid, reason: &str) -> Result<(), ServiceBusError> {
        let request = NackRequest { lease, reason: reason.to_string() };
        let _: Value =
            self.client.call(wire::NACK, &request, CALL_DEADLINE).await.map_err(|e| failed(&e))?;
        Ok(())
    }

    async fn dead_letters(&self, address: &Address) -> Result<Vec<DeadLetter>, ServiceBusError> {
        let request = AddressRequest { address: address.as_str().to_string() };
        // Diagnostics read from the leader: a stale node can still be missing the last change.
        let response: DeadLettersResponse = self
            .client
            .call(wire::DEAD_LETTERS, &request, CALL_DEADLINE)
            .await
            .map_err(|e| failed(&e))?;
        Ok(response.dead_letters)
    }

    async fn depth(&self, address: &Address) -> Result<usize, ServiceBusError> {
        let request = AddressRequest { address: address.as_str().to_string() };
        let response: DepthResponse =
            self.client.call(wire::DEPTH, &request, CALL_DEADLINE).await.map_err(|e| failed(&e))?;
        Ok(response.ready as usize)
    }
}
