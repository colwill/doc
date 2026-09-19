//! eventbus probe: for each node, whether it answers, which one leads, whether a quorum is left and
//! how far replication has fallen behind. It is its own crate so it can diverge from the other bus
//! probes as each bus grows checks of its own.

use std::time::Instant;

use async_trait::async_trait;
use doc_backend::status::{Component, NodeStatus, Probe, bus_state, bus_summary, node_status};
use doc_consensus::ClusterClient;

pub const NAME: &str = "eventbus";

pub struct EventbusProbe {
    client: ClusterClient,
}

impl EventbusProbe {
    pub fn new(client: ClusterClient) -> Self {
        Self { client }
    }
}

#[async_trait]
impl Probe for EventbusProbe {
    fn name(&self) -> &str {
        NAME
    }

    async fn check(&self) -> Component {
        let started = Instant::now();
        let nodes: Vec<NodeStatus> = self
            .client
            .metrics()
            .await
            .into_iter()
            .map(|(address, result)| node_status(address, result.map_err(|err| err.to_string())))
            .collect();
        Component::new("bus", NAME, bus_state(&nodes))
            .detail(bus_summary(&nodes))
            .with_nodes(nodes)
            .took(started.elapsed())
    }
}
