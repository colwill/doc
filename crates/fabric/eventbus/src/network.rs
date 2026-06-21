//! The networked Event Bus client: the T07 trait over the cluster, with leader redirects,
//! retries and a streamed subscription.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use doc_consensus::{ClusterClient, ClusterError};
use doc_transport::{H3ClientRecv, H3ClientSend, Lines, lines};
use tokio::sync::{Mutex, mpsc};
use uuid::Uuid;

use crate::wire::{
    self, AckMessage, Empty, PublishRequest, PublishResponse, RegisterTopicRequest, StreamMessage,
    SubscribeRequest, TopicsResponse,
};
use crate::{
    ConsumerGroup, Delivery, Event, EventBus, EventBusError, PublishAck, Subscription, Topic,
    TopicReport, TopicSpec,
};

const DEADLINE: Duration = Duration::from_secs(10);

fn failed(error: &ClusterError) -> EventBusError {
    EventBusError::Unavailable(error.to_string())
}

#[derive(Clone)]
pub struct NetworkEventBus {
    client: ClusterClient,
}

impl NetworkEventBus {
    pub fn new(client: ClusterClient) -> Self {
        Self { client }
    }

    pub fn client(&self) -> &ClusterClient {
        &self.client
    }
}

#[async_trait]
impl EventBus for NetworkEventBus {
    async fn register_topic(&self, spec: TopicSpec) -> Result<(), EventBusError> {
        let request = RegisterTopicRequest {
            filter: spec.filter.as_str().to_string(),
            max_events: spec.max_events as u64,
            max_age_ms: spec.max_age.as_millis() as u64,
        };
        let _: Empty = self
            .client
            .call(wire::REGISTER_TOPIC, &request, DEADLINE)
            .await
            .map_err(|e| failed(&e))?;
        Ok(())
    }

    async fn publish(&self, event: Event) -> Result<PublishAck, EventBusError> {
        let response: PublishResponse = self
            .client
            .call(wire::PUBLISH, &PublishRequest { event }, DEADLINE)
            .await
            .map_err(|e| failed(&e))?;
        Ok(PublishAck { id: response.id, offset: response.offset, duplicate: response.duplicate })
    }

    async fn topics(&self) -> Result<Vec<TopicReport>, EventBusError> {
        let response: TopicsResponse =
            self.client.call(wire::TOPICS, &Empty {}, DEADLINE).await.map_err(|e| failed(&e))?;
        Ok(response
            .topics
            .into_iter()
            .filter_map(|(topic, status)| {
                Some(TopicReport {
                    topic: Topic::new(topic).ok()?,
                    retained: status.retained,
                    published: status.next_offset,
                })
            })
            .collect())
    }

    async fn subscribe(
        &self,
        group: ConsumerGroup,
    ) -> Result<Box<dyn Subscription>, EventBusError> {
        let request = SubscribeRequest {
            group: group.name.clone(),
            filter: group.filter.as_str().to_string(),
            lease_ms: group.lease.as_millis() as u64,
            from_start: group.from_start,
        };
        let first = lines::encode(&request).map_err(|e| EventBusError::Other(e.to_string()))?;
        // Fail fast if the cluster is unreachable now; after that the subscription reconnects.
        let (send, recv) = self
            .client
            .open_stream(wire::SUBSCRIBE, first.clone(), DEADLINE)
            .await
            .map_err(|e| failed(&e))?;
        Ok(Box::new(NetworkSubscription::start(self.client.clone(), first, (send, recv))))
    }
}

/// One task pumps the stream in both directions and reconnects when a leader changes, so
/// callers see one long-lived subscription. Events acknowledged on a stream that has gone are
/// simply redelivered.
pub struct NetworkSubscription {
    deliveries: mpsc::Receiver<Delivery>,
    acks: mpsc::Sender<AckMessage>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl NetworkSubscription {
    fn start(
        client: ClusterClient,
        first: bytes::Bytes,
        initial: (H3ClientSend, H3ClientRecv),
    ) -> Self {
        let (delivery_tx, deliveries) = mpsc::channel(64);
        let (acks, ack_rx) = mpsc::channel::<AckMessage>(64);
        let ack_rx = Arc::new(Mutex::new(ack_rx));

        let task = tokio::spawn(async move {
            let mut stream = Some(initial);
            loop {
                let (send, recv) = match stream.take() {
                    Some(pair) => pair,
                    None => {
                        match client.open_stream(wire::SUBSCRIBE, first.clone(), DEADLINE).await {
                            Ok(pair) => {
                                tracing::info!("subscription reconnected");
                                pair
                            }
                            Err(err) => {
                                tracing::warn!(%err, "cannot reach the event bus; retrying");
                                tokio::time::sleep(Duration::from_millis(250)).await;
                                continue;
                            }
                        }
                    }
                };
                if !pump(send, recv, &delivery_tx, &ack_rx).await {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });

        Self { deliveries, acks, task: Some(task) }
    }
}

/// Returns false once the caller has dropped the subscription.
async fn pump(
    mut send: H3ClientSend,
    mut recv: H3ClientRecv,
    deliveries: &mpsc::Sender<Delivery>,
    acks: &Arc<Mutex<mpsc::Receiver<AckMessage>>>,
) -> bool {
    let mut buffer = Lines::new();
    loop {
        let mut ack_rx = acks.lock().await;
        tokio::select! {
            chunk = recv.recv_data() => {
                drop(ack_rx);
                match chunk {
                    Ok(Some(chunk)) => {
                        buffer.push(chunk);
                        while let Some(message) = buffer.next_message::<StreamMessage>() {
                            match message {
                                Ok(StreamMessage::Delivery(delivery)) => {
                                    if deliveries.send(delivery.into()).await.is_err() {
                                        return false;
                                    }
                                }
                                Ok(StreamMessage::Ready { node }) => {
                                    tracing::debug!(node, "subscription ready")
                                }
                                Ok(StreamMessage::Closing { reason }) => {
                                    tracing::warn!(%reason, "the bus closed the subscription");
                                    return true;
                                }
                                Err(err) => tracing::debug!(%err, "malformed stream message"),
                            }
                        }
                    }
                    Ok(None) | Err(_) => return true,
                }
            }
            message = async { ack_rx.recv().await } => {
                drop(ack_rx);
                match message {
                    Some(message) => {
                        let Ok(bytes) = lines::encode(&message) else { continue };
                        if send.send_data(bytes).await.is_err() {
                            return true;
                        }
                    }
                    None => return false,
                }
            }
        }
    }
}

impl Drop for NetworkSubscription {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[async_trait]
impl Subscription for NetworkSubscription {
    async fn next(&mut self) -> Option<Delivery> {
        self.deliveries.recv().await
    }

    async fn ack(&mut self, delivery: Uuid) -> Result<(), EventBusError> {
        self.acks
            .send(AckMessage::Ack { delivery })
            .await
            .map_err(|_| EventBusError::Unavailable("the subscription closed".into()))
    }

    async fn nack(&mut self, delivery: Uuid, after: Option<Duration>) -> Result<(), EventBusError> {
        self.acks
            .send(AckMessage::Nack { delivery, after_ms: after.map(|d| d.as_millis() as u64) })
            .await
            .map_err(|_| EventBusError::Unavailable("the subscription closed".into()))
    }
}
