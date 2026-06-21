//! The Event Bus service: publish and topic registration go through Raft, subscriptions are
//! served by the leader as a full-duplex HTTP/3 stream (deliveries out, acknowledgements in).

pub mod coordinator;
pub mod machine;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::Result;
use bytes::Bytes;
use doc_consensus::node::{BusService, node_error};
use doc_consensus::{Node, NodeError};
use doc_transport::{
    H3ServerRecv, H3ServerSend, H3ServerStream, Lines, lines, read_body, respond_json,
};
use http::{Request, Response, StatusCode};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Counter;
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::TopicFilter;
use crate::wire::{
    self, AckMessage, DeliveryMessage, PublishRequest, PublishResponse, RegisterTopicRequest,
    StreamMessage, SubscribeRequest, TopicStatus, TopicsResponse,
};
use coordinator::Coordinator;
use machine::{EventCommand, EventMachine, EventResponse};

static REDELIVERED: Counted = Counted::new(
    "doc.eventbus.redelivered",
    "Events delivered again after a lease ran out or a subscriber handed them back",
);
static DROPPED: Counted =
    Counted::new("doc.eventbus.dropped", "Events retention removed before a group had them");

struct Counted {
    name: &'static str,
    description: &'static str,
    counter: OnceLock<Counter<u64>>,
}

impl Counted {
    const fn new(name: &'static str, description: &'static str) -> Self {
        Self { name, description, counter: OnceLock::new() }
    }
}

/// Counts events for a consumer group, named without the filter its key carries.
fn counted(metric: &Counted, key: &str, events: u64) {
    let counter = metric.counter.get_or_init(|| {
        doc_telemetry::meter().u64_counter(metric.name).with_description(metric.description).build()
    });
    let group = key.split_once('|').map_or(key, |(group, _)| group);
    counter.add(events, &[KeyValue::new("group", group.to_string())]);
}

const COMMIT_INTERVAL: Duration = Duration::from_millis(200);
const IDLE_POLL: Duration = Duration::from_millis(50);
const MAX_LEASE: Duration = Duration::from_secs(300);

pub struct EventBusService {
    node: Arc<Node<EventMachine>>,
    coordinator: Coordinator,
    published: Notify,
}

impl EventBusService {
    pub fn new(node: Arc<Node<EventMachine>>) -> Arc<Self> {
        Arc::new(Self { node, coordinator: Coordinator::default(), published: Notify::new() })
    }

    async fn write(&self, command: EventCommand) -> Result<EventResponse, NodeError> {
        self.node.write(command).await.map(|applied| applied.response)
    }

    async fn route(&self, path: &str, body: &[u8]) -> Result<(StatusCode, Value)> {
        Ok(match path {
            wire::REGISTER_TOPIC => {
                let request: RegisterTopicRequest = serde_json::from_slice(body)?;
                match self
                    .write(EventCommand::RegisterTopic {
                        filter: request.filter,
                        max_events: request.max_events,
                        max_age_ms: request.max_age_ms,
                    })
                    .await
                {
                    Ok(_) => (StatusCode::OK, json!({})),
                    Err(err) => node_error(err),
                }
            }
            wire::PUBLISH => {
                let request: PublishRequest = serde_json::from_slice(body)?;
                match self.write(EventCommand::Publish { event: Box::new(request.event) }).await {
                    Ok(EventResponse::Published { id, offset, duplicate }) => {
                        self.published.notify_waiters();
                        (
                            StatusCode::OK,
                            serde_json::to_value(PublishResponse { id, offset, duplicate })?,
                        )
                    }
                    Ok(_) => {
                        (StatusCode::INTERNAL_SERVER_ERROR, json!({"error": "unexpected reply"}))
                    }
                    Err(err) => node_error(err),
                }
            }
            wire::TOPICS => {
                let topics = self.node.state().read(|machine| machine.topics());
                let response = TopicsResponse {
                    topics: topics
                        .into_iter()
                        .map(|(topic, (retained, next_offset))| {
                            (topic, TopicStatus { retained, next_offset })
                        })
                        .collect(),
                };
                (StatusCode::OK, serde_json::to_value(response)?)
            }
            _ => (StatusCode::NOT_FOUND, json!({"error": "unknown event bus route"})),
        })
    }

    async fn subscribe(&self, stream: H3ServerStream) -> Result<()> {
        let (mut send, mut recv) = stream.split();
        let mut buffer = Lines::new();
        let request: SubscribeRequest = loop {
            match recv.recv_data().await? {
                Some(chunk) => {
                    buffer.push(chunk);
                    if let Some(message) = buffer.next_message::<SubscribeRequest>() {
                        break message?;
                    }
                }
                None => return Ok(()),
            }
        };

        if let Err(err) = self.node.ensure_linearizable().await {
            let (status, body) = node_error(err);
            send.send_response(Response::builder().status(status).body(())?).await?;
            send.send_data(Bytes::from(serde_json::to_vec(&body)?)).await?;
            send.finish().await?;
            return Ok(());
        }

        let filter = TopicFilter::new(request.filter.clone())?;
        let key = format!("{}|{}", request.group, filter.as_str());
        let lease = Duration::from_millis(request.lease_ms.max(1_000)).min(MAX_LEASE);
        let from_start = request.from_start;
        let runtime = {
            let node = self.node.clone();
            let key_for_init = key.clone();
            self.coordinator.runtime(&key, move || {
                node.state().read(|machine| machine.group_cursors(&key_for_init, from_start))
            })
        };

        send.send_response(
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "application/x-ndjson")
                .body(())?,
        )
        .await?;
        send.send_data(lines::encode(&StreamMessage::Ready { node: self.node.config().id })?)
            .await?;

        let acks = tokio::spawn(read_acks(recv, runtime.clone()));
        let result = self.deliver(&mut send, &key, &filter, lease, runtime).await;
        acks.abort();
        let _ = send.finish().await;
        result
    }

    async fn deliver(
        &self,
        send: &mut H3ServerSend,
        key: &str,
        filter: &TopicFilter,
        lease: Duration,
        runtime: Arc<parking_lot::Mutex<coordinator::GroupRuntime>>,
    ) -> Result<()> {
        let mut last_commit = tokio::time::Instant::now();
        loop {
            let next = {
                let mut group = runtime.lock();
                group.expire();
                match group.take_redelivery() {
                    Some((topic, offset, attempt)) => Some((topic, offset, attempt + 1)),
                    None => {
                        let cursors = group.cursors.clone();
                        drop(group);
                        let found = self
                            .node
                            .state()
                            .read(|machine| machine.events_after(filter, &cursors, 1))
                            .into_iter()
                            .next();
                        found.map(|(topic, offset, _)| {
                            // Retention trimmed whatever lies between the cursor and what is left.
                            if let Some(cursor) =
                                cursors.get(&topic).filter(|cursor| **cursor < offset)
                            {
                                counted(&DROPPED, key, offset - cursor);
                            }
                            (topic, offset, 1)
                        })
                    }
                }
            };

            if let Some((topic, offset, attempt)) = next {
                let event = self.node.state().read(|machine| machine.event_at(&topic, offset));
                match event {
                    Some(event) => {
                        if attempt > 1 {
                            counted(&REDELIVERED, key, 1);
                        }
                        let id = runtime.lock().lease(topic, offset, attempt, lease);
                        let message =
                            StreamMessage::Delivery(DeliveryMessage { id, event, attempt });
                        send.send_data(lines::encode(&message)?).await?;
                    }
                    // Trimmed by retention while it waited to be delivered again.
                    None => counted(&DROPPED, key, 1),
                }
            } else {
                tokio::select! {
                    _ = self.published.notified() => {}
                    _ = tokio::time::sleep(IDLE_POLL) => {}
                }
            }

            if last_commit.elapsed() >= COMMIT_INTERVAL {
                last_commit = tokio::time::Instant::now();
                let offsets = runtime.lock().take_offsets();
                if let Some(offsets) = offsets
                    && let Err(err) = self
                        .write(EventCommand::CommitOffsets { group: key.to_string(), offsets })
                        .await
                {
                    let reason = format!("offsets could not be committed: {err}");
                    self.coordinator.reset();
                    send.send_data(lines::encode(&StreamMessage::Closing { reason })?).await?;
                    return Ok(());
                }
            }
        }
    }
}

async fn read_acks(
    mut recv: H3ServerRecv,
    runtime: Arc<parking_lot::Mutex<coordinator::GroupRuntime>>,
) {
    let mut buffer = Lines::new();
    loop {
        match recv.recv_data().await {
            Ok(Some(chunk)) => {
                buffer.push(chunk);
                while let Some(message) = buffer.next_message::<AckMessage>() {
                    match message {
                        Ok(AckMessage::Ack { delivery }) => {
                            runtime.lock().ack(delivery);
                        }
                        Ok(AckMessage::Nack { delivery, after_ms }) => {
                            runtime.lock().nack(delivery, after_ms.map(Duration::from_millis));
                        }
                        Err(err) => tracing::debug!(%err, "malformed acknowledgement"),
                    }
                }
            }
            Ok(None) | Err(_) => return,
        }
    }
}

impl BusService for EventBusService {
    async fn handle(&self, request: Request<()>, mut stream: H3ServerStream) -> Result<()> {
        let path = request.uri().path().to_string();
        if path == wire::SUBSCRIBE {
            // The caller keeps the request body open for acknowledgements, so it is not read here.
            return self.subscribe(stream).await;
        }
        let body = read_body(&mut stream).await?;
        let (status, value) = self.route(&path, &body).await?;
        respond_json(&mut stream, status, &value).await
    }
}
