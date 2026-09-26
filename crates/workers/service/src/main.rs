//! `doc-workers`: runs the status workers, the plugin probe and the TaskWorker cron and background
//! pools. It shares the backend's configuration and tables, so a replica needs no state of its own,
//! which is what lets several of them run at once, each claiming work the others then leave alone.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use chrono::Utc;
use doc_backend::config::{Config, FabricMode};
use doc_backend::db::postgres::{
    self, PostgresHealth, PostgresIdentity, PostgresPluginStatus, PostgresPlugins,
    PostgresStatusHistory,
};
use doc_backend::db::repositories::{HealthRepository, IdentityRepository, PluginRepository};
use doc_backend::fabric::Buses;
use doc_backend::status::plugins::PluginStatuses;
use doc_backend::status::{Probe, StatusHistory};
use doc_background_tasks::postgres::PostgresTasks;
use doc_background_tasks::{Actor, Cancel, NewTask, Task, TaskHandler};
use doc_cron_tasks::postgres::PostgresCron;
use doc_cron_tasks::{CronFamily, CronHandler, Schedule};
use doc_plugin_probe::PluginProbe;
use serde_json::{Value, json};

const DATABASE_WAIT: Duration = Duration::from_secs(60);
/// Every minute, which is the check interval a component has to go down within.
const PROBE_SCHEDULE: &str = "* * * * *";
/// DOC keeps a week of telemetry about any service or plugin, and no more. It is a platform for
/// seeing how things stand, not a time-series database, and a week is what answers "what happened
/// last night" without becoming the place an organisation's history lives.
const KEEP_TELEMETRY_FOR: chrono::Duration = chrono::Duration::days(7);
/// What it grows to instead once a plugin with the `telemetry-sink` capability is taking it
/// somewhere that keeps it, so what is here has stopped being the only copy.
const KEEP_HISTORY_FOR: chrono::Duration = chrono::Duration::days(30);
/// Tokens are only removed once nothing would show them any more, so a revocation stays visible
/// in the API for a while after it takes effect.
const KEEP_EXPIRED_FOR: chrono::Duration = chrono::Duration::days(7);

/// The first cron task: removing tokens that expired or were revoked long enough ago.
struct ExpireTokens(Arc<dyn IdentityRepository>);

#[async_trait]
impl CronHandler for ExpireTokens {
    async fn run(&self) -> Result<(), String> {
        let before = Utc::now() - KEEP_EXPIRED_FOR;
        let removed = self.0.purge_expired_tokens(before).await.map_err(|err| err.to_string())?;
        if removed > 0 {
            tracing::info!(removed, "expired tokens were removed");
        }
        Ok(())
    }
}

/// Waits, and gives up when asked to: what exercises this pool, now plugin runs are the backend's.
struct Sleep;

#[async_trait]
impl TaskHandler for Sleep {
    async fn run(&self, task: &Task, cancel: Cancel) -> Result<Value, String> {
        let seconds = task.payload["seconds"].as_f64().unwrap_or(1.0).clamp(0.0, 3_600.0);
        let until = tokio::time::Instant::now() + Duration::from_secs_f64(seconds);
        while tokio::time::Instant::now() < until {
            if cancel.is_cancelled() {
                return Err("cancelled".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(json!({ "slept": seconds }))
    }
}

/// Runs one probe and puts the answer everywhere it is read: Postgres for the history T34 charts
/// and DOC's reliability is judged from, the Cache Bus for the API, and a `platform.status.*` event
/// for anything watching. The history comes first, so an outage of the Cache Bus is on it too.
struct StatusCheck {
    probe: Arc<dyn Probe>,
    buses: Buses,
    history: Arc<dyn StatusHistory>,
}

#[async_trait]
impl CronHandler for StatusCheck {
    async fn run(&self) -> Result<(), String> {
        let component = self.probe.check().await;
        if let Err(err) = self.history.record(&component).await {
            tracing::warn!(%err, component = %component.name, "a status check was not recorded");
        }
        let stored = doc_backend::status::store(self.buses.cache.as_ref(), &component).await;
        if let Err(err) =
            doc_backend::status::announce(self.buses.events.as_ref(), &component).await
        {
            tracing::warn!(%err, component = %component.name, "a status event was not published");
        }
        stored
    }
}

struct PruneStatus {
    history: Arc<dyn StatusHistory>,
    plugins: Arc<dyn PluginStatuses>,
    registered: Arc<dyn PluginRepository>,
}

impl PruneStatus {
    /// Whether anything registered is taking DOC's telemetry somewhere that keeps it. Asked each
    /// time rather than at startup, so onboarding one lengthens the history without a restart —
    /// and removing one shortens it again at the next prune.
    async fn exported(&self) -> bool {
        match self.registered.records().await {
            Ok(records) => records.iter().any(|plugin| {
                plugin.registered_at.is_some()
                    && plugin.manifest["capabilities"]
                        .as_array()
                        .is_some_and(|held| held.iter().any(|one| one == "telemetry-sink"))
            }),
            Err(err) => {
                tracing::warn!(%err, "could not tell whether telemetry is exported; keeping a week");
                false
            }
        }
    }
}

#[async_trait]
impl CronHandler for PruneStatus {
    async fn run(&self) -> Result<(), String> {
        let keep = match self.exported().await {
            true => KEEP_HISTORY_FOR,
            false => KEEP_TELEMETRY_FOR,
        };
        let before = Utc::now() - keep;
        let removed = self.history.prune(before).await.map_err(|err| err.to_string())?;
        let changes = self.plugins.prune(before).await.map_err(|err| err.to_string())?;
        if removed + changes > 0 {
            tracing::info!(removed, changes, "old status history was pruned");
        }
        Ok(())
    }
}

/// Each plugin's own schedule, `plugin.<id>.<name>`: a background run of that plugin, as itself.
struct PluginSchedules {
    tasks: Arc<PostgresTasks>,
    buses: Buses,
}

#[async_trait]
impl CronFamily for PluginSchedules {
    async fn run(&self, name: &str) -> Result<(), String> {
        let (plugin, schedule) = name
            .strip_prefix("plugin.")
            .and_then(|rest| rest.split_once('.'))
            .ok_or_else(|| format!("{name} names no plugin schedule"))?;
        let by = Some(plugin.to_string());
        let new = NewTask {
            kind: format!("plugin.{plugin}.run"),
            payload: json!({ "schedule": schedule }),
            max_attempts: 1,
            started_by: Actor { kind: "plugin".into(), id: by.clone(), label: by },
            chain: None,
        };
        let task =
            doc_background_tasks::start(self.tasks.as_ref(), self.buses.services.as_ref(), new)
                .await
                .map_err(|err| err.to_string())?;
        tracing::info!(plugin, schedule, task = %task.id, "a plugin's schedule came round");
        Ok(())
    }
}

async fn probe_plugins(probe: Option<&PluginProbe>) -> Result<()> {
    match probe {
        Some(probe) => probe.run().await.map_err(anyhow::Error::msg),
        None => std::future::pending().await,
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // The probes' HTTP clients are built without a TLS provider of their own, and telemetry only
    // installs one when it is exporting; without this, a deployment with no OTLP endpoint panics
    // the moment the first probe is made.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let filter = "info,openraft=warn,quiche=warn,tokio_quiche=warn";
    let _telemetry = doc_telemetry::init("doc-workers", env!("CARGO_PKG_VERSION"), filter);

    let config = Config::load(None)?;
    let worker = std::env::var("DOC_WORKER_NAME").unwrap_or_else(|_| "workers".into());

    let pool = postgres::pool_with_retry(&config.database, DATABASE_WAIT).await?;
    let identity: Arc<dyn IdentityRepository> = Arc::new(PostgresIdentity::new(pool.clone()));
    let tasks = Arc::new(PostgresTasks::new(pool.clone()));
    let cron_store = Arc::new(PostgresCron::new(pool.clone()));

    // Without the clusters the buses are kept in the database, so this process shares them with
    // the backend. In process they would not be shared at all, and a schedule coming round here
    // would queue work nothing ever took.
    let buses = match config.fabric.mode {
        FabricMode::Memory => Buses::shared(&pool, &worker),
        FabricMode::Cluster => Buses::cluster_as(&config, &worker)
            .await
            .context("connecting doc-workers to the fabric")?,
    };
    tracing::info!(mode = ?config.fabric.mode, %worker, "doc-workers starting");

    let history: Arc<dyn StatusHistory> = Arc::new(PostgresStatusHistory::new(pool.clone()));
    let health: Arc<dyn HealthRepository> = Arc::new(PostgresHealth::new(pool.clone()));
    let plugin_status: Arc<dyn PluginStatuses> = Arc::new(PostgresPluginStatus::new(pool.clone()));
    let registered_plugins: Arc<dyn PluginRepository> =
        Arc::new(PostgresPlugins::new(pool.clone()));

    let mut probes: Vec<Arc<dyn Probe>> =
        vec![Arc::new(doc_persistence_probe::PersistenceProbe::new(health))];
    for (bus, client) in &buses.clusters {
        let probe: Arc<dyn Probe> = match bus.as_str() {
            "eventbus" => Arc::new(doc_eventbus_probe::EventbusProbe::new(client.clone())),
            "servicebus" => Arc::new(doc_servicebus_probe::ServicebusProbe::new(client.clone())),
            "cachebus" => Arc::new(doc_cachebus_probe::CachebusProbe::new(client.clone())),
            other => {
                tracing::warn!(bus = other, "no probe for this bus");
                continue;
            }
        };
        probes.push(probe);
    }
    let backend_url =
        std::env::var("DOC_BACKEND_URL").unwrap_or_else(|_| "http://backend:8080".into());
    let frontend_url =
        std::env::var("DOC_FRONTEND_URL").unwrap_or_else(|_| "http://frontend:8081".into());
    probes.push(Arc::new(doc_backend_probe::BackendProbe::new(&backend_url)?));
    probes.push(Arc::new(doc_frontend_probe::FrontendProbe::new(&frontend_url)?));

    let schedules = PluginSchedules { tasks: tasks.clone(), buses: buses.clone() };
    let mut cron =
        doc_cron_tasks::Pool::new(cron_store, &worker).family("plugin.", Arc::new(schedules));
    for probe in probes {
        let name = format!("core.status.{}", probe.name());
        let description = format!("Checks {} and publishes the result", probe.name());
        cron.register(Schedule::new(
            &name,
            PROBE_SCHEDULE,
            &description,
            Arc::new(StatusCheck { probe, buses: buses.clone(), history: history.clone() }),
        ))
        .await
        .with_context(|| format!("registering {name}"))?;
    }
    cron.register(Schedule::new(
        "core.prune-status",
        "17 3 * * *",
        "Removes status history older than 30 days",
        Arc::new(PruneStatus {
            history: history.clone(),
            plugins: plugin_status.clone(),
            registered: registered_plugins.clone(),
        }),
    ))
    .await
    .context("registering core.prune-status")?;
    cron.register(Schedule::new(
        "core.expire-tokens",
        "*/15 * * * *",
        "Removes tokens that expired or were revoked over a week ago",
        Arc::new(ExpireTokens(identity.clone())),
    ))
    .await
    .context("registering core.expire-tokens")?;

    let background = doc_background_tasks::Pool::new(tasks, buses.services.clone())
        .handling("core.sleep", Arc::new(Sleep))
        .announcing(Arc::new(doc_backend::api::tasks::Announcer(buses.events.clone())));

    // The registry it compares with is the backend's, which in-process buses cannot reach.
    let plugin_probe = match config.fabric.mode {
        FabricMode::Cluster => Some(PluginProbe::new(
            buses.events.clone(),
            buses.services.clone(),
            plugin_status,
            config.plugins.stuck_after(),
        )),
        FabricMode::Memory => {
            tracing::info!("the plugin probe needs the cluster fabric, so it is not running");
            None
        }
    };

    tracing::info!(cron = ?cron.names(), background = ?background.kinds(), "task pools ready");

    tokio::select! {
        result = cron.run() => result.context("the cron pool stopped")?,
        result = background.run() => result.context("the background pool stopped")?,
        result = probe_plugins(plugin_probe.as_ref()) => result.context("the plugin probe stopped")?,
        _ = shutdown() => tracing::info!("doc-workers stopping"),
    }
    pool.close().await;
    Ok(())
}

async fn shutdown() {
    let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
        Ok(term) => term,
        Err(err) => {
            tracing::error!(%err, "cannot listen for SIGTERM");
            return;
        }
    };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}
