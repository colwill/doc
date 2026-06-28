//! The status model: what a probe reports, how it is stored and how the pieces combine. It lives
//! here rather than in the API because the probes in `crates/workers` produce it and the backend
//! only serves it, so both need the same shape.

pub mod plugins;

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use doc_cachebus::{CacheBus, Namespace};
use doc_consensus::BusMetrics;
use doc_eventbus::{Event, EventBus, Topic};
use serde::{Deserialize, Serialize};

/// Where the current answer for each component is kept, so the API answers without checking again.
pub const NAMESPACE: &str = "core.status";
/// Long enough that a missed run does not blank the page, short enough that a stopped worker shows.
pub const TTL: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Health {
    Up,
    Degraded,
    Down,
    Unknown,
}

impl Health {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Degraded => "degraded",
            Self::Down => "down",
            Self::Unknown => "unknown",
        }
    }

    /// How bad this is, so a set of components can be summed up by its worst member.
    pub fn severity(self) -> u8 {
        match self {
            Self::Up => 0,
            Self::Unknown => 1,
            Self::Degraded => 2,
            Self::Down => 3,
        }
    }
}

impl std::str::FromStr for Health {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "up" => Ok(Self::Up),
            "degraded" => Ok(Self::Degraded),
            "down" => Ok(Self::Down),
            "unknown" => Ok(Self::Unknown),
            other => Err(format!("{other} is not a health state")),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeStatus {
    pub address: String,
    pub state: Health,
    pub role: Option<String>,
    pub term: Option<u64>,
    pub leader: Option<u64>,
    pub last_applied: Option<u64>,
    pub replication_lag: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    pub kind: String,
    pub name: String,
    pub state: Health,
    pub detail: Option<String>,
    #[serde(default)]
    pub nodes: Vec<NodeStatus>,
    /// When this component was last checked, which is also how old this answer is.
    pub checked_at: DateTime<Utc>,
    /// How long the check itself took, which is what T34 charts.
    #[serde(default)]
    pub latency_ms: Option<u64>,
}

impl Component {
    pub fn new(kind: &str, name: &str, state: Health) -> Self {
        Self {
            kind: kind.to_string(),
            name: name.to_string(),
            state,
            detail: None,
            nodes: Vec::new(),
            checked_at: Utc::now(),
            latency_ms: None,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn with_nodes(mut self, nodes: Vec<NodeStatus>) -> Self {
        self.nodes = nodes;
        self
    }

    pub fn took(mut self, elapsed: Duration) -> Self {
        self.latency_ms = Some(elapsed.as_millis() as u64);
        self
    }
}

/// One check of one component. Each probe crate carries its own so they can diverge: a bus probe
/// answers about nodes, and a reachability probe about one URL.
#[async_trait]
pub trait Probe: Send + Sync {
    fn name(&self) -> &str;
    async fn check(&self) -> Component;
}

/// A bus is up only with every node healthy, and degraded while a quorum still answers.
pub fn bus_state(nodes: &[NodeStatus]) -> Health {
    if nodes.is_empty() {
        return Health::Unknown;
    }
    let up = nodes.iter().filter(|node| node.state == Health::Up).count();
    if up == nodes.len() {
        Health::Up
    } else if up > nodes.len() / 2 {
        Health::Degraded
    } else {
        Health::Down
    }
}

/// The sentence on a bus card: how many nodes answered, and which one leads.
pub fn bus_summary(nodes: &[NodeStatus]) -> String {
    let up = nodes.iter().filter(|node| node.state == Health::Up).count();
    let leader = nodes
        .iter()
        .find(|node| node.role.as_deref().is_some_and(|role| role.eq_ignore_ascii_case("leader")))
        .map(|node| node.address.split(':').next().unwrap_or(&node.address).to_string());
    let reach = format!("{up} of {} nodes answering", nodes.len());
    match leader {
        Some(leader) => format!("{reach}, leader {leader}"),
        None => format!("{reach}, no leader"),
    }
}

pub fn node_status(address: String, metrics: Result<BusMetrics, String>) -> NodeStatus {
    match metrics {
        Ok(metrics) => NodeStatus {
            address,
            state: if metrics.healthy { Health::Up } else { Health::Degraded },
            role: Some(metrics.state),
            term: Some(metrics.term),
            leader: metrics.leader,
            last_applied: metrics.last_applied,
            replication_lag: metrics.replication_lag.values().copied().max(),
            error: None,
        },
        Err(error) => NodeStatus {
            address,
            state: Health::Down,
            role: None,
            term: None,
            leader: None,
            last_applied: None,
            replication_lag: None,
            error: Some(error),
        },
    }
}

pub fn worst(components: &[Component]) -> Health {
    components
        .iter()
        .map(|component| component.state)
        .max_by_key(|state| state.severity())
        .unwrap_or(Health::Unknown)
}

fn namespace() -> Option<Namespace> {
    Namespace::new(NAMESPACE).ok()
}

/// Writes the current answer where the API reads it. A component that stops being written expires,
/// which is how a stopped worker eventually shows rather than leaving a stale answer forever.
pub async fn store(cache: &dyn CacheBus, component: &Component) -> Result<(), String> {
    let Some(namespace) = namespace() else { return Err("core.status is not a namespace".into()) };
    let value = serde_json::to_value(component).map_err(|err| err.to_string())?;
    cache
        .set(&namespace, &component.name, value, Some(TTL))
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

pub async fn load(cache: &dyn CacheBus, names: &[String]) -> Vec<Component> {
    let Some(namespace) = namespace() else { return Vec::new() };
    let mut found = Vec::new();
    for name in names {
        let Ok(Some(entry)) = cache.get(&namespace, name).await else { continue };
        if let Ok(component) = serde_json::from_value::<Component>(entry.value) {
            found.push(component);
        }
    }
    found
}

pub async fn announce(events: &dyn EventBus, component: &Component) -> Result<(), String> {
    let topic =
        Topic::new(format!("platform.status.{}", component.name)).map_err(|err| err.to_string())?;
    let payload = serde_json::to_value(component).map_err(|err| err.to_string())?;
    events
        .publish(Event::new(topic, "core.workers", payload))
        .await
        .map(|_| ())
        .map_err(|err| err.to_string())
}

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("{0}")]
    Other(String),
}

/// Every check is kept, so T34 can chart how latency and replication lag move. A cron task prunes it.
#[async_trait]
pub trait StatusHistory: Send + Sync {
    async fn record(&self, component: &Component) -> Result<(), HistoryError>;
    async fn prune(&self, before: DateTime<Utc>) -> Result<u64, HistoryError>;
    /// Every check since `since`, oldest first, each as the component it recorded.
    async fn since(&self, since: DateTime<Utc>) -> Result<Vec<Component>, HistoryError>;
    /// Every check from `from` up to but not including `to`, oldest first.
    async fn between(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<Component>, HistoryError>;
}
