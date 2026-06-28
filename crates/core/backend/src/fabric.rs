//! The buses the backend owns: which topics, queues and cache namespaces exist, and their
//! retention and TTLs. The registry is applied to the buses at startup.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use doc_cachebus::{
    CacheBus, MemoryCacheBus, Namespace, NamespaceSpec, NetworkCacheBus, PostgresCacheBus,
};
use doc_consensus::ClusterClient;
use doc_eventbus::{
    Event, EventBus, MemoryEventBus, NetworkEventBus, PostgresEventBus, Topic, TopicFilter,
    TopicSpec,
};
use doc_servicebus::{
    Address, MemoryServiceBus, NetworkServiceBus, PostgresServiceBus, QueueSpec, Request,
    ServiceBus, ServiceHandler,
};
use serde_json::{Value, json};
use sqlx::PgPool;

use crate::config::Config;

pub const SOURCE: &str = "core.backend";

/// How long a fresh fabric gets to find its feet before `cluster_as` gives up: long enough for
/// three nodes to resolve each other's names, restore a snapshot and elect a leader from cold.
const FABRIC_WAIT: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct Buses {
    pub events: Arc<dyn EventBus>,
    pub services: Arc<dyn ServiceBus>,
    pub cache: Arc<dyn CacheBus>,
    /// The cluster clients behind the buses, kept so `/api/v1/status` can read node metrics.
    /// Empty when the buses run in process.
    pub clusters: Vec<(String, ClusterClient)>,
}

impl Buses {
    /// The buses of a deployment running without the clusters, kept in the database every process
    /// of it already has, so the workers, the backend and anything else with the database share
    /// one Event Bus, one Service Bus and one Cache Bus. In process they would not: a bus held in
    /// one process reaches nothing outside it, and a schedule that came round in the workers
    /// would queue work the backend never heard of.
    pub fn shared(pool: &PgPool, name: &str) -> Self {
        Self {
            events: Arc::new(PostgresEventBus::new(pool.clone())),
            services: Arc::new(PostgresServiceBus::new(pool.clone(), name)),
            cache: Arc::new(PostgresCacheBus::new(pool.clone())),
            clusters: Vec::new(),
        }
    }

    /// Buses that live and die with this process, for tests and for anything with no database.
    pub fn in_memory() -> Self {
        Self {
            events: Arc::new(MemoryEventBus::new()),
            services: Arc::new(MemoryServiceBus::new()),
            cache: Arc::new(MemoryCacheBus::new()),
            clusters: Vec::new(),
        }
    }

    /// Clients for the three Raft clusters, using the certificates and per-bus tokens the
    /// bootstrap job wrote to the secrets volume.
    pub async fn cluster(config: &Config) -> Result<Self> {
        Self::cluster_as(config, "backend").await
    }

    /// Freshly started containers take a moment to resolve each other's names and elect a
    /// leader, which a restart of the whole stack hits every time, so this waits for the fabric
    /// the way `pool_with_retry` waits for Postgres rather than failing on the first attempt.
    pub async fn cluster_as(config: &Config, name: &str) -> Result<Self> {
        let started = std::time::Instant::now();
        let mut wait = Duration::from_millis(250);
        loop {
            match Self::connect(config, name).await {
                Ok(buses) => return Ok(buses),
                Err(err) if started.elapsed() + wait < FABRIC_WAIT => {
                    tracing::warn!(
                        error = %err,
                        retry_in_ms = wait.as_millis() as u64,
                        "waiting for the fabric"
                    );
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(Duration::from_secs(5));
                }
                Err(err) => return Err(err),
            }
        }
    }

    async fn connect(config: &Config, name: &str) -> Result<Self> {
        let secrets = &config.secrets.dir;
        let client = |bus: &str, nodes: &[String]| -> Result<ClusterClient> {
            let token = bus_token(secrets, bus)?;
            ClusterClient::new(bus, config.fabric.addresses(nodes), token, secrets.clone())
                .with_context(|| format!("connecting to the {bus} cluster"))
        };
        let events = client("eventbus", &config.fabric.event_bus)?;
        let queues = client("servicebus", &config.fabric.service_bus)?;
        let cache = client("cachebus", &config.fabric.cache_bus)?;
        let clusters = vec![
            ("eventbus".to_string(), events.clone()),
            ("servicebus".to_string(), queues.clone()),
            ("cachebus".to_string(), cache.clone()),
        ];
        let services = NetworkServiceBus::connect(queues, name)
            .await
            .context("starting the Service Bus client")?;
        Ok(Self {
            events: Arc::new(NetworkEventBus::new(events)),
            services: Arc::new(services),
            cache: Arc::new(NetworkCacheBus::new(cache)),
            clusters,
        })
    }
}

fn bus_token(secrets: &std::path::Path, bus: &str) -> Result<String> {
    let path = secrets.join(format!("tokens/buses/{bus}.token"));
    let token = std::fs::read_to_string(&path)
        .with_context(|| format!("reading the {bus} token from {}", path.display()))?;
    Ok(token.trim().to_string())
}

pub struct Registry {
    pub topics: Vec<TopicSpec>,
    pub queues: Vec<QueueSpec>,
    pub namespaces: Vec<NamespaceSpec>,
}

const HOUR: Duration = Duration::from_secs(3600);
const DAY: Duration = Duration::from_secs(24 * 3600);

fn topic_spec(filter: &str, max_events: usize, max_age: Duration) -> Result<TopicSpec> {
    Ok(TopicSpec::new(
        TopicFilter::new(filter).with_context(|| format!("topic filter {filter}"))?,
        max_events,
        max_age,
    ))
}

fn namespace_spec(name: &str, ttl: Duration) -> Result<NamespaceSpec> {
    Ok(NamespaceSpec::new(
        Namespace::new(name).with_context(|| format!("cache namespace {name}"))?,
        Some(ttl),
    ))
}

/// The platform's own topics, queues and namespaces. Plugins add theirs when they register.
pub fn platform_registry() -> Result<Registry> {
    Ok(Registry {
        topics: vec![
            topic_spec("platform.backend.>", 1_000, HOUR)?,
            topic_spec("platform.status.>", 10_000, DAY)?,
            topic_spec("platform.plugin.>", 10_000, 7 * DAY)?,
            topic_spec("platform.task.>", 10_000, DAY)?,
            topic_spec("platform.iam.>", 10_000, 30 * DAY)?,
            topic_spec("platform.organisation.>", 10_000, 30 * DAY)?,
            topic_spec("platform.team.>", 10_000, 30 * DAY)?,
            topic_spec("plugin.>", 50_000, DAY)?,
        ],
        queues: vec![
            QueueSpec::new(Address::core(doc_background_tasks::QUEUE)?).with_attempts(5),
            // Held as long as a run may take, so a slow run is never handed out twice.
            QueueSpec::new(Address::core(doc_background_tasks::PLUGIN_RUNS)?)
                .with_attempts(5)
                .with_lease(crate::config::PLUGIN_RUN_LEASE),
            QueueSpec::new(Address::core("events")?).with_attempts(10),
        ],
        namespaces: vec![
            namespace_spec("core.sessions", 12 * HOUR)?,
            namespace_spec("core.tokens", Duration::from_secs(300))?,
            namespace_spec("core.permissions", Duration::from_secs(60))?,
            namespace_spec("core.access", Duration::from_secs(60))?,
            // Link tickets (ADR-0005), each good for one link and gone once used.
            namespace_spec("core.links", Duration::from_secs(600))?,
            namespace_spec("core.status", Duration::from_secs(120))?,
        ],
    })
}

impl Registry {
    pub async fn apply(&self, buses: &Buses) -> Result<()> {
        for spec in &self.topics {
            buses.events.register_topic(spec.clone()).await.context("registering a topic")?;
        }
        for spec in &self.queues {
            buses.services.register_queue(spec.clone()).await.context("registering a queue")?;
        }
        for spec in &self.namespaces {
            buses
                .cache
                .register_namespace(spec.clone())
                .await
                .context("registering a cache namespace")?;
        }
        tracing::info!(
            topics = self.topics.len(),
            queues = self.queues.len(),
            namespaces = self.namespaces.len(),
            "fabric registry applied"
        );
        Ok(())
    }
}

struct Ping;

#[async_trait]
impl ServiceHandler for Ping {
    async fn handle(&self, request: Request) -> Result<Value, String> {
        Ok(json!({
            "pong": true,
            "subject": request.subject,
            "source": SOURCE,
            "principal": request.principal,
        }))
    }
}

/// Registers `core.ping`, announces the backend and round-trips a cache value, so the logs show
/// all three buses working before anything depends on them.
pub async fn start(buses: &Buses) -> Result<()> {
    platform_registry()?.apply(buses).await?;

    let ping = Address::core("ping")?;
    buses.services.serve(ping.clone(), Arc::new(Ping)).await.context("serving core.ping")?;

    let started = Event::new(
        Topic::new("platform.backend.started")?,
        SOURCE,
        json!({"version": crate::api::VERSION}),
    );
    let ack = buses.events.publish(started).await.context("publishing platform.backend.started")?;
    tracing::info!(event = %ack.id, topic = "platform.backend.started", "event bus ready");

    let reply = buses
        .services
        .request(&ping, "ping", json!({"from": SOURCE}), Duration::from_secs(5))
        .await
        .context("calling core.ping")?;
    tracing::info!(address = %ping, reply = %reply, "service bus ready");

    let namespace = Namespace::new("core.status")?;
    let written = buses
        .cache
        .set(&namespace, "backend", json!({"state": "starting"}), None)
        .await
        .context("writing a cache value")?;
    let read = buses.cache.get(&namespace, "backend").await.context("reading a cache value")?;
    let round_tripped = read.as_ref().is_some_and(|entry| entry.value == written.value);
    tracing::info!(
        namespace = %namespace,
        version = written.version,
        round_tripped,
        "cache bus ready"
    );
    if !round_tripped {
        anyhow::bail!("the cache did not return the value that was written");
    }
    Ok(())
}
