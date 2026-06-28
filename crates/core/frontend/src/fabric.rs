//! The buses the frontend uses: the Event Bus for live updates and the Cache Bus for sessions
//! and access summaries. The backend owns the topics and namespaces, so none are registered here.
//!
//! Without the clusters the backend and the workers share their buses through the database, and
//! the frontend deliberately does not join them: it has no database and is not given one, since
//! everything it shows it asks the backend for. It keeps its own buses instead, and `session.rs`
//! holds what it caches for a short while rather than until something says otherwise, because
//! nothing will.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use doc_cachebus::{CacheBus, MemoryCacheBus, NetworkCacheBus};
use doc_consensus::ClusterClient;
use doc_eventbus::{EventBus, MemoryEventBus, NetworkEventBus};

use crate::config::{Config, FabricMode};

#[derive(Clone)]
pub struct Buses {
    pub events: Arc<dyn EventBus>,
    pub cache: Arc<dyn CacheBus>,
}

impl Buses {
    pub fn in_memory() -> Self {
        Self { events: Arc::new(MemoryEventBus::new()), cache: Arc::new(MemoryCacheBus::new()) }
    }

    pub fn cluster(config: &Config) -> Result<Self> {
        let secrets = &config.secrets.dir;
        let client = |bus: &str, nodes: &[String]| -> Result<ClusterClient> {
            let token = bus_token(secrets, bus)?;
            ClusterClient::new(bus, config.fabric.addresses(nodes), token, secrets.clone())
                .with_context(|| format!("connecting to the {bus} cluster"))
        };
        Ok(Self {
            events: Arc::new(NetworkEventBus::new(client("eventbus", &config.fabric.event_bus)?)),
            cache: Arc::new(NetworkCacheBus::new(client("cachebus", &config.fabric.cache_bus)?)),
        })
    }
}

fn bus_token(secrets: &Path, bus: &str) -> Result<String> {
    let path = secrets.join(format!("tokens/buses/{bus}.token"));
    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("reading the {bus} token from {}", path.display()))?;
    Ok(token.trim().to_string())
}

pub fn connect(config: &Config) -> Result<Buses> {
    match config.fabric.mode {
        FabricMode::Memory => Ok(Buses::in_memory()),
        FabricMode::Cluster => Buses::cluster(config),
    }
}
