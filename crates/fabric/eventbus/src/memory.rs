//! In-memory Event Bus: one log per topic with retention, consumer-group cursors and leases.
//! Endpoint unit tests use it, and the platform runs on it until the Raft clusters exist.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use tokio::sync::Notify;
use uuid::Uuid;

use crate::{
    ConsumerGroup, Delivery, Event, EventBus, EventBusError, PublishAck, Subscription, Topic,
    TopicSpec,
};

const DEFAULT_MAX_EVENTS: usize = 10_000;
const DEFAULT_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Default)]
struct TopicLog {
    next_offset: u64,
    events: VecDeque<(u64, Event)>,
}

struct InFlight {
    topic: Topic,
    offset: u64,
    attempt: u32,
    expires: Instant,
}

#[derive(Default)]
struct GroupState {
    cursor: BTreeMap<Topic, u64>,
    inflight: HashMap<Uuid, InFlight>,
    redeliver: VecDeque<(Topic, u64, u32)>,
    started: bool,
}

#[derive(Default)]
struct State {
    topics: BTreeMap<Topic, TopicLog>,
    specs: Vec<TopicSpec>,
    groups: HashMap<String, GroupState>,
    idempotency: HashMap<String, Uuid>,
}

impl State {
    fn retention(&self, topic: &Topic) -> (usize, Duration) {
        self.specs
            .iter()
            .find(|spec| spec.filter.matches(topic))
            .map(|spec| (spec.max_events, spec.max_age))
            .unwrap_or((DEFAULT_MAX_EVENTS, DEFAULT_MAX_AGE))
    }

    fn event_at(&self, topic: &Topic, offset: u64) -> Option<Event> {
        let log = self.topics.get(topic)?;
        log.events.iter().find(|(o, _)| *o == offset).map(|(_, e)| e.clone())
    }
}

#[derive(Clone, Default)]
pub struct MemoryEventBus {
    state: Arc<Mutex<State>>,
    published: Arc<Notify>,
}

impl MemoryEventBus {
    pub fn new() -> Self {
        Self::default()
    }

    /// Events currently retained for a topic, for status pages and manual checks.
    pub fn len(&self, topic: &Topic) -> usize {
        self.state.lock().topics.get(topic).map(|log| log.events.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.state.lock().topics.values().all(|log| log.events.is_empty())
    }

    fn group_key(group: &ConsumerGroup) -> String {
        format!("{}|{}", group.name, group.filter.as_str())
    }
}

#[async_trait]
impl EventBus for MemoryEventBus {
    async fn register_topic(&self, spec: TopicSpec) -> Result<(), EventBusError> {
        let mut state = self.state.lock();
        state.specs.retain(|existing| existing.filter != spec.filter);
        state.specs.push(spec);
        Ok(())
    }

    async fn publish(&self, event: Event) -> Result<PublishAck, EventBusError> {
        let mut state = self.state.lock();
        if let Some(key) = &event.idempotency_key
            && let Some(id) = state.idempotency.get(key)
        {
            return Ok(PublishAck { id: *id, offset: 0, duplicate: true });
        }
        let (max_events, max_age) = state.retention(&event.topic);
        let log = state.topics.entry(event.topic.clone()).or_default();
        let offset = log.next_offset;
        log.next_offset += 1;
        log.events.push_back((offset, event.clone()));
        let cutoff = chrono::Duration::from_std(max_age).ok().map(|age| Utc::now() - age);
        while log.events.len() > max_events
            || cutoff.is_some_and(|cutoff| log.events.front().is_some_and(|(_, e)| e.time < cutoff))
        {
            log.events.pop_front();
        }
        if let Some(key) = &event.idempotency_key {
            state.idempotency.insert(key.clone(), event.id);
        }
        drop(state);
        self.published.notify_waiters();
        Ok(PublishAck { id: event.id, offset, duplicate: false })
    }

    async fn topics(&self) -> Result<Vec<crate::TopicReport>, EventBusError> {
        let state = self.state.lock();
        Ok(state
            .topics
            .iter()
            .map(|(topic, log)| crate::TopicReport {
                topic: topic.clone(),
                retained: log.events.len() as u64,
                published: log.next_offset,
            })
            .collect())
    }

    async fn subscribe(
        &self,
        group: ConsumerGroup,
    ) -> Result<Box<dyn Subscription>, EventBusError> {
        let key = Self::group_key(&group);
        {
            let mut state = self.state.lock();
            let started = state.groups.get(&key).is_some_and(|group| group.started);
            if !started {
                let cursors: Vec<(Topic, u64)> = if group.from_start {
                    Vec::new()
                } else {
                    state
                        .topics
                        .iter()
                        .map(|(topic, log)| (topic.clone(), log.next_offset))
                        .collect()
                };
                let entry = state.groups.entry(key.clone()).or_default();
                entry.started = true;
                entry.cursor.extend(cursors);
            }
        }
        Ok(Box::new(MemorySubscription { bus: self.clone(), key, group }))
    }
}

struct MemorySubscription {
    bus: MemoryEventBus,
    key: String,
    group: ConsumerGroup,
}

impl MemorySubscription {
    fn take_next(&self) -> Option<Delivery> {
        let mut state = self.bus.state.lock();
        let now = Instant::now();

        let expired: Vec<(Uuid, Topic, u64, u32)> = state
            .groups
            .get(&self.key)
            .map(|group| {
                group
                    .inflight
                    .iter()
                    .filter(|(_, flight)| flight.expires <= now)
                    .map(|(id, flight)| (*id, flight.topic.clone(), flight.offset, flight.attempt))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(group) = state.groups.get_mut(&self.key) {
            for (id, topic, offset, attempt) in expired {
                group.inflight.remove(&id);
                group.redeliver.push_back((topic, offset, attempt));
            }
        }

        let redelivery = state.groups.get_mut(&self.key).and_then(|g| g.redeliver.pop_front());
        let (topic, offset, attempt) = match redelivery {
            Some((topic, offset, attempt)) => (topic, offset, attempt + 1),
            None => {
                let mut found = None;
                let topics: Vec<Topic> = state
                    .topics
                    .keys()
                    .filter(|topic| self.group.filter.matches(topic))
                    .cloned()
                    .collect();
                for topic in topics {
                    let cursor = state
                        .groups
                        .get(&self.key)
                        .and_then(|g| g.cursor.get(&topic).copied())
                        .unwrap_or(0);
                    let next = state.topics.get(&topic).and_then(|log| {
                        log.events.iter().find(|(o, _)| *o >= cursor).map(|(o, _)| *o)
                    });
                    if let Some(offset) = next {
                        found = Some((topic, offset, 1));
                        break;
                    }
                }
                found?
            }
        };

        let event = state.event_at(&topic, offset)?;
        let delivery = Uuid::now_v7();
        let lease = self.group.lease;
        let group = state.groups.entry(self.key.clone()).or_default();
        group.cursor.insert(topic.clone(), offset + 1);
        group.inflight.insert(delivery, InFlight { topic, offset, attempt, expires: now + lease });
        Some(Delivery { id: delivery, event, attempt })
    }
}

#[async_trait]
impl Subscription for MemorySubscription {
    async fn next(&mut self) -> Option<Delivery> {
        loop {
            if let Some(delivery) = self.take_next() {
                return Some(delivery);
            }
            let published = self.bus.published.notified();
            tokio::select! {
                _ = published => {}
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
        }
    }

    async fn ack(&mut self, delivery: Uuid) -> Result<(), EventBusError> {
        let mut state = self.bus.state.lock();
        if let Some(group) = state.groups.get_mut(&self.key) {
            group.inflight.remove(&delivery);
        }
        Ok(())
    }

    async fn nack(&mut self, delivery: Uuid, after: Option<Duration>) -> Result<(), EventBusError> {
        let mut state = self.bus.state.lock();
        let Some(group) = state.groups.get_mut(&self.key) else {
            return Ok(());
        };
        if let Some(flight) = group.inflight.remove(&delivery) {
            match after {
                Some(delay) => {
                    group
                        .inflight
                        .insert(delivery, InFlight { expires: Instant::now() + delay, ..flight });
                }
                None => group.redeliver.push_back((flight.topic, flight.offset, flight.attempt)),
            }
        }
        Ok(())
    }
}
