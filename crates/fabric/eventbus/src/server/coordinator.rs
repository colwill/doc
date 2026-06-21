//! Per-group delivery state on the leader: cursors, leases and the acknowledged prefix.
//! Offsets are committed through Raft, so a new leader resumes from the last acknowledged event.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use uuid::Uuid;

use super::machine::AckWindow;

pub struct InFlight {
    pub topic: String,
    pub offset: u64,
    pub attempt: u32,
    pub expires: Instant,
}

pub struct GroupRuntime {
    /// Next offset to hand out per topic; ahead of the committed offset while events are in flight.
    pub cursors: BTreeMap<String, u64>,
    pub window: AckWindow,
    pub inflight: HashMap<Uuid, InFlight>,
    pub redeliver: VecDeque<(String, u64, u32)>,
    pub dirty: bool,
}

impl GroupRuntime {
    pub fn new(cursors: BTreeMap<String, u64>) -> Self {
        Self {
            window: AckWindow::start(cursors.clone()),
            cursors,
            inflight: HashMap::new(),
            redeliver: VecDeque::new(),
            dirty: false,
        }
    }

    /// Hands expired leases back for redelivery, which is what makes delivery at least once.
    pub fn expire(&mut self) {
        let now = Instant::now();
        let expired: Vec<Uuid> = self
            .inflight
            .iter()
            .filter(|(_, flight)| flight.expires <= now)
            .map(|(id, _)| *id)
            .collect();
        for id in expired {
            if let Some(flight) = self.inflight.remove(&id) {
                self.redeliver.push_back((flight.topic, flight.offset, flight.attempt));
            }
        }
    }

    pub fn take_redelivery(&mut self) -> Option<(String, u64, u32)> {
        self.redeliver.pop_front()
    }

    pub fn lease(&mut self, topic: String, offset: u64, attempt: u32, lease: Duration) -> Uuid {
        let id = Uuid::now_v7();
        let cursor = self.cursors.entry(topic.clone()).or_default();
        *cursor = (*cursor).max(offset + 1);
        self.inflight
            .insert(id, InFlight { topic, offset, attempt, expires: Instant::now() + lease });
        id
    }

    pub fn ack(&mut self, delivery: Uuid) -> bool {
        match self.inflight.remove(&delivery) {
            Some(flight) => {
                self.window.ack(&flight.topic, flight.offset);
                self.dirty = true;
                true
            }
            None => false,
        }
    }

    pub fn nack(&mut self, delivery: Uuid, after: Option<Duration>) {
        let Some(flight) = self.inflight.remove(&delivery) else {
            return;
        };
        match after {
            Some(delay) => {
                self.inflight
                    .insert(delivery, InFlight { expires: Instant::now() + delay, ..flight });
            }
            None => self.redeliver.push_back((flight.topic, flight.offset, flight.attempt)),
        }
    }

    pub fn take_offsets(&mut self) -> Option<BTreeMap<String, u64>> {
        if !self.dirty {
            return None;
        }
        self.dirty = false;
        Some(self.window.offsets())
    }
}

/// Every subscriber of a group on this node shares one runtime, so each event goes to one member.
#[derive(Default)]
pub struct Coordinator {
    groups: Mutex<HashMap<String, Arc<Mutex<GroupRuntime>>>>,
}

impl Coordinator {
    pub fn runtime(
        &self,
        key: &str,
        initial: impl FnOnce() -> BTreeMap<String, u64>,
    ) -> Arc<Mutex<GroupRuntime>> {
        let mut groups = self.groups.lock();
        groups
            .entry(key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(GroupRuntime::new(initial()))))
            .clone()
    }

    /// Drops every group's runtime, so the next subscription starts from the committed offsets.
    /// Used when this node stops being the leader.
    pub fn reset(&self) {
        self.groups.lock().clear();
    }
}
