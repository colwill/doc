//! Health and metrics per node: leader, term, commit index and replication lag.

use std::collections::BTreeMap;
use std::sync::OnceLock;
use std::time::Duration;

use openraft::impls::BasicNode;
use openraft::{RaftMetrics, ServerState};
use opentelemetry::KeyValue;
use opentelemetry::metrics::{Histogram, ObservableGauge};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::types::NodeId;

/// How long a leader may go without a quorum acknowledgement before it counts as unhealthy.
/// A partitioned leader still reports `Leader` until its lease lapses (ADR-0002).
pub const QUORUM_ACK_LIMIT_MS: u64 = 3_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BusMetrics {
    pub bus: String,
    pub node_id: NodeId,
    pub state: String,
    pub term: u64,
    pub leader: Option<NodeId>,
    pub last_log_index: Option<u64>,
    pub last_applied: Option<u64>,
    pub snapshot: Option<u64>,
    pub purged: Option<u64>,
    pub voters: Vec<NodeId>,
    pub learners: Vec<NodeId>,
    pub replication_lag: BTreeMap<NodeId, u64>,
    pub millis_since_quorum_ack: Option<u64>,
    pub healthy: bool,
}

impl BusMetrics {
    pub fn from_raft(bus: &str, metrics: &RaftMetrics<NodeId, BasicNode>) -> Self {
        let last_log_index = metrics.last_log_index;
        let membership = metrics.membership_config.membership();
        let voters: Vec<NodeId> = membership.voter_ids().collect();
        let learners: Vec<NodeId> =
            membership.nodes().map(|(id, _)| *id).filter(|id| !voters.contains(id)).collect();
        let replication_lag = metrics
            .replication
            .as_ref()
            .map(|replication| {
                replication
                    .iter()
                    .map(|(id, matched)| {
                        let matched = matched.as_ref().map(|log| log.index).unwrap_or(0);
                        (*id, last_log_index.unwrap_or(0).saturating_sub(matched))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let state = format!("{:?}", metrics.state).to_lowercase();
        let healthy = match metrics.state {
            ServerState::Leader => {
                metrics.millis_since_quorum_ack.is_none_or(|millis| millis <= QUORUM_ACK_LIMIT_MS)
            }
            ServerState::Follower | ServerState::Learner => metrics.current_leader.is_some(),
            _ => false,
        };
        Self {
            bus: bus.to_string(),
            node_id: metrics.id,
            state,
            term: metrics.current_term,
            leader: metrics.current_leader,
            last_log_index,
            last_applied: metrics.last_applied.as_ref().map(|log| log.index),
            snapshot: metrics.snapshot.as_ref().map(|log| log.index),
            purged: metrics.purged.as_ref().map(|log| log.index),
            voters,
            learners,
            replication_lag,
            millis_since_quorum_ack: metrics.millis_since_quorum_ack,
            healthy,
        }
    }
}

type Watched = watch::Receiver<RaftMetrics<NodeId, BasicNode>>;

/// Exports a node's Raft health: term, leadership, health, replication lag and leader changes.
pub fn export(bus: &str, node: NodeId, metrics: Watched) -> Vec<ObservableGauge<u64>> {
    let meter = doc_telemetry::meter();
    let labels = [KeyValue::new("bus", bus.to_string()), KeyValue::new("node", node.to_string())];
    let reading = |name: &'static str, description: &'static str, read: fn(&BusMetrics) -> u64| {
        let (bus, metrics, labels) = (bus.to_string(), metrics.clone(), labels.clone());
        meter
            .u64_observable_gauge(name)
            .with_description(description)
            .with_callback(move |observer| {
                observer.observe(read(&BusMetrics::from_raft(&bus, &metrics.borrow())), &labels)
            })
            .build()
    };
    let lag = {
        let (bus, metrics) = (bus.to_string(), metrics.clone());
        meter
            .u64_observable_gauge("doc.raft.replication.lag")
            .with_unit("{entry}")
            .with_description("Log entries each follower is behind its leader")
            .with_callback(move |observer| {
                let now = BusMetrics::from_raft(&bus, &metrics.borrow());
                for (follower, lag) in now.replication_lag.iter().filter(|(id, _)| **id != node) {
                    let labels = [
                        KeyValue::new("bus", bus.clone()),
                        KeyValue::new("follower", follower.to_string()),
                    ];
                    observer.observe(*lag, &labels);
                }
            })
            .build()
    };
    let gauges = vec![
        reading("doc.raft.term", "The node's current Raft term", |now| now.term),
        reading("doc.raft.leader", "1 while the node leads its bus", |now| {
            u64::from(now.state == "leader")
        }),
        reading("doc.raft.healthy", "1 while the node has a working leader", |now| {
            u64::from(now.healthy)
        }),
        lag,
    ];
    // Every node counts, from zero, the changes it sees, so one that restarts loses none of them.
    let changes = meter
        .u64_counter("doc.raft.leader.changes")
        .with_description("Times a node saw its bus's leader change")
        .build();
    changes.add(0, &labels);
    let mut watching = metrics;
    tokio::spawn(async move {
        let mut known = None;
        while watching.changed().await.is_ok() {
            let Some(leader) = watching.borrow_and_update().current_leader else { continue };
            if known.is_some_and(|known| known != leader) {
                changes.add(1, &labels);
            }
            known = Some(leader);
        }
    });
    gauges
}

/// Records how long a write took to commit, as `doc.raft.commit.duration`.
pub fn committed(bus: &str, took: Duration) {
    static COMMIT: OnceLock<Histogram<f64>> = OnceLock::new();
    let histogram = COMMIT.get_or_init(|| {
        doc_telemetry::seconds(
            "doc.raft.commit.duration",
            "How long a write took to commit through Raft",
        )
    });
    histogram.record(took.as_secs_f64(), &[KeyValue::new("bus", bus.to_string())]);
}
