//! The replicated Event Bus state: one log per topic with retention, committed group offsets
//! and the idempotency index. Topic logs sit behind `Arc`s so snapshots stay cheap (ADR-0002).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use doc_consensus::BusStateMachine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{Event, TopicFilter};

const IDEMPOTENCY_KEYS: usize = 50_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EventCommand {
    RegisterTopic { filter: String, max_events: u64, max_age_ms: u64 },
    Publish { event: Box<Event> },
    CommitOffsets { group: String, offsets: BTreeMap<String, u64> },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum EventResponse {
    #[default]
    Empty,
    Published {
        id: Uuid,
        offset: u64,
        duplicate: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredSpec {
    pub filter: String,
    pub max_events: u64,
    pub max_age_ms: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TopicLog {
    pub next_offset: u64,
    pub events: VecDeque<(u64, Event)>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EventState {
    pub topics: BTreeMap<String, Arc<TopicLog>>,
    pub specs: Vec<StoredSpec>,
    /// Group key (`group|filter`) to the offset committed for each topic.
    pub groups: BTreeMap<String, BTreeMap<String, u64>>,
    keys: BTreeMap<String, Uuid>,
    key_order: VecDeque<String>,
}

#[derive(Default)]
pub struct EventMachine {
    state: EventState,
}

impl EventMachine {
    pub fn state(&self) -> &EventState {
        &self.state
    }

    /// Events for a filter at or after `cursor`, oldest first, for the delivery loop.
    pub fn events_after(
        &self,
        filter: &TopicFilter,
        cursors: &BTreeMap<String, u64>,
        limit: usize,
    ) -> Vec<(String, u64, Event)> {
        let mut out = Vec::new();
        for (topic, log) in &self.state.topics {
            let Ok(parsed) = crate::Topic::new(topic.clone()) else {
                continue;
            };
            if !filter.matches(&parsed) {
                continue;
            }
            let cursor = cursors.get(topic).copied().unwrap_or(0);
            for (offset, event) in &log.events {
                if *offset >= cursor {
                    out.push((topic.clone(), *offset, event.clone()));
                    if out.len() >= limit {
                        return out;
                    }
                }
            }
        }
        out
    }

    pub fn event_at(&self, topic: &str, offset: u64) -> Option<Event> {
        self.state
            .topics
            .get(topic)
            .and_then(|log| log.events.iter().find(|(at, _)| *at == offset))
            .map(|(_, event)| event.clone())
    }

    /// Where a group resumes: its committed offsets, or the end of each topic for new groups.
    pub fn group_cursors(&self, group_key: &str, from_start: bool) -> BTreeMap<String, u64> {
        if let Some(offsets) = self.state.groups.get(group_key) {
            return offsets.clone();
        }
        if from_start {
            return BTreeMap::new();
        }
        self.state.topics.iter().map(|(topic, log)| (topic.clone(), log.next_offset)).collect()
    }

    pub fn topics(&self) -> BTreeMap<String, (u64, u64)> {
        self.state
            .topics
            .iter()
            .map(|(topic, log)| (topic.clone(), (log.events.len() as u64, log.next_offset)))
            .collect()
    }

    fn retention(&self, topic: &str) -> (u64, u64) {
        let parsed = crate::Topic::new(topic.to_string()).ok();
        self.state
            .specs
            .iter()
            .find(|spec| {
                parsed.as_ref().is_some_and(|topic| {
                    TopicFilter::new(spec.filter.clone()).is_ok_and(|filter| filter.matches(topic))
                })
            })
            .map(|spec| (spec.max_events, spec.max_age_ms))
            .unwrap_or((10_000, 24 * 60 * 60 * 1_000))
    }

    fn remember_key(&mut self, key: String, id: Uuid) {
        self.state.keys.insert(key.clone(), id);
        self.state.key_order.push_back(key);
        while self.state.key_order.len() > IDEMPOTENCY_KEYS {
            if let Some(oldest) = self.state.key_order.pop_front() {
                self.state.keys.remove(&oldest);
            }
        }
    }
}

/// Retention by age uses the newest event's timestamp, so every replica trims identically.
fn trim(log: &mut TopicLog, max_events: u64, max_age_ms: u64, now: DateTime<Utc>) {
    let cutoff = now - chrono::Duration::milliseconds(max_age_ms as i64);
    while log.events.len() as u64 > max_events
        || log.events.front().is_some_and(|(_, event)| event.time < cutoff)
    {
        log.events.pop_front();
    }
}

impl BusStateMachine for EventMachine {
    type Command = EventCommand;
    type Response = EventResponse;
    type Snapshot = EventState;

    fn apply(&mut self, _log_index: u64, command: EventCommand) -> EventResponse {
        match command {
            EventCommand::RegisterTopic { filter, max_events, max_age_ms } => {
                self.state.specs.retain(|spec| spec.filter != filter);
                self.state.specs.push(StoredSpec { filter, max_events, max_age_ms });
                EventResponse::Empty
            }
            EventCommand::Publish { event } => {
                if let Some(key) = &event.idempotency_key
                    && let Some(id) = self.state.keys.get(key)
                {
                    return EventResponse::Published { id: *id, offset: 0, duplicate: true };
                }
                let topic = event.topic.as_str().to_string();
                let (max_events, max_age_ms) = self.retention(&topic);
                let log = Arc::make_mut(self.state.topics.entry(topic).or_default());
                let offset = log.next_offset;
                log.next_offset += 1;
                let time = event.time;
                let id = event.id;
                let key = event.idempotency_key.clone();
                log.events.push_back((offset, *event));
                trim(log, max_events, max_age_ms, time);
                if let Some(key) = key {
                    self.remember_key(key, id);
                }
                EventResponse::Published { id, offset, duplicate: false }
            }
            EventCommand::CommitOffsets { group, offsets } => {
                let committed = self.state.groups.entry(group).or_default();
                for (topic, offset) in offsets {
                    let entry = committed.entry(topic).or_default();
                    *entry = (*entry).max(offset);
                }
                EventResponse::Empty
            }
        }
    }

    fn snapshot(&self) -> EventState {
        self.state.clone()
    }

    fn restore(&mut self, snapshot: EventState) {
        self.state = snapshot;
    }
}

/// Offsets acknowledged out of order, so only the contiguous prefix is committed.
#[derive(Debug, Default)]
pub struct AckWindow {
    pub committed: BTreeMap<String, u64>,
    pending: BTreeMap<String, BTreeSet<u64>>,
}

impl AckWindow {
    pub fn start(committed: BTreeMap<String, u64>) -> Self {
        Self { committed, pending: BTreeMap::new() }
    }

    pub fn ack(&mut self, topic: &str, offset: u64) {
        let committed = self.committed.entry(topic.to_string()).or_default();
        let pending = self.pending.entry(topic.to_string()).or_default();
        pending.insert(offset);
        while pending.remove(committed) {
            *committed += 1;
        }
    }

    pub fn offsets(&self) -> BTreeMap<String, u64> {
        self.committed.clone()
    }
}
