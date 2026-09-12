//! TaskWorker cron tasks pool. Schedules live in Postgres so several `doc-workers` replicas share
//! them: a run is claimed under a lease, and only the replica that wins the claim runs it.

pub mod postgres;
pub mod store;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use croner::Cron;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tracing::Instrument;

pub use store::{CronError, CronStore, MemoryCron};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CronTask {
    pub name: String,
    pub schedule: String,
    pub description: Option<String>,
    pub paused: bool,
    pub next_run_at: Option<DateTime<Utc>>,
    pub last_run_at: Option<DateTime<Utc>>,
    pub last_state: Option<String>,
    pub last_error: Option<String>,
}

/// When a schedule next comes round. Five fields, in UTC, as the table stores them.
pub fn next_after(schedule: &str, after: DateTime<Utc>) -> Result<DateTime<Utc>, CronError> {
    let cron = Cron::from_str(schedule)
        .map_err(|err| CronError::Schedule(format!("{schedule}: {err}")))?;
    cron.find_next_occurrence(&after, false)
        .map_err(|err| CronError::Schedule(format!("{schedule}: {err}")))
}

#[async_trait]
pub trait CronHandler: Send + Sync {
    async fn run(&self) -> Result<(), String>;
}

/// Runs every schedule under one prefix, such as plugins' own, which come and go at run time.
#[async_trait]
pub trait CronFamily: Send + Sync {
    async fn run(&self, name: &str) -> Result<(), String>;
}

/// A schedule and what it runs, registered together so a schedule with no handler is never claimed.
pub struct Schedule {
    pub name: String,
    pub expression: String,
    pub description: Option<String>,
    pub handler: Arc<dyn CronHandler>,
}

impl Schedule {
    pub fn new(
        name: &str,
        expression: &str,
        description: &str,
        handler: Arc<dyn CronHandler>,
    ) -> Self {
        Self {
            name: name.to_string(),
            expression: expression.to_string(),
            description: Some(description.to_string()),
            handler,
        }
    }
}

pub struct Pool {
    store: Arc<dyn CronStore>,
    handlers: BTreeMap<String, Arc<dyn CronHandler>>,
    families: BTreeMap<String, Arc<dyn CronFamily>>,
    worker: String,
    lease: Duration,
    tick: Duration,
}

impl Pool {
    pub fn new(store: Arc<dyn CronStore>, worker: &str) -> Self {
        Self {
            store,
            handlers: BTreeMap::new(),
            families: BTreeMap::new(),
            worker: worker.to_string(),
            lease: Duration::from_secs(300),
            tick: Duration::from_secs(5),
        }
    }

    pub fn with_tick(mut self, tick: Duration) -> Self {
        self.tick = tick;
        self
    }

    pub fn names(&self) -> Vec<&str> {
        self.handlers.keys().chain(self.families.keys()).map(String::as_str).collect()
    }

    /// Claims every schedule whose name starts with `prefix`, whoever recorded it.
    pub fn family(mut self, prefix: &str, handler: Arc<dyn CronFamily>) -> Self {
        self.families.insert(prefix.to_string(), handler);
        self
    }

    /// Records the schedule if it is new, leaving a paused one paused and an existing next run
    /// where it is, so a restart does not bring every schedule forward.
    pub async fn register(&mut self, schedule: Schedule) -> Result<(), CronError> {
        let next = next_after(&schedule.expression, Utc::now())?;
        self.store
            .upsert(&schedule.name, &schedule.expression, schedule.description.as_deref(), next)
            .await?;
        self.handlers.insert(schedule.name.clone(), schedule.handler);
        Ok(())
    }

    pub async fn run(&self) -> Result<(), CronError> {
        loop {
            match self.claim_and_run().await {
                Ok(true) => continue,
                Ok(false) => tokio::time::sleep(self.tick).await,
                Err(err) => {
                    tracing::warn!(%err, "the cron schedules could not be read");
                    tokio::time::sleep(self.tick).await;
                }
            }
        }
    }

    /// Runs at most one due schedule, and says whether there may be another waiting.
    pub async fn claim_and_run(&self) -> Result<bool, CronError> {
        let known: Vec<String> = self.handlers.keys().cloned().collect();
        let prefixes: Vec<String> = self.families.keys().cloned().collect();
        let claimed = self.store.claim_due(&self.worker, self.lease, &known, &prefixes).await?;
        let Some(task) = claimed else {
            return Ok(false);
        };
        let started = Utc::now();
        let span = tracing::info_span!(
            target: "doc",
            "cron.run",
            otel.name = %format!("cron {}", task.name),
            otel.status_description = tracing::field::Empty,
            doc.cron.name = %task.name,
            doc.cron.schedule = %task.schedule,
        );
        let outcome = match self.handlers.get(&task.name) {
            Some(handler) => handler.run().instrument(span.clone()).await,
            None => match self.families.iter().find(|(prefix, _)| task.name.starts_with(*prefix)) {
                Some((_, family)) => family.run(&task.name).instrument(span.clone()).await,
                None => return Ok(false),
            },
        };
        if let Err(error) = &outcome {
            span.record("otel.status_description", error.as_str());
        }
        let next = next_after(&task.schedule, Utc::now())?;
        match &outcome {
            Ok(()) => {
                tracing::info!(
                    task = %task.name,
                    ms = (Utc::now() - started).num_milliseconds(),
                    "a cron task finished"
                );
                self.store.finish(&task.name, next, "succeeded", None).await?;
            }
            Err(error) => {
                tracing::warn!(task = %task.name, %error, "a cron task failed");
                self.store.finish(&task.name, next, "failed", Some(error)).await?;
            }
        }
        Ok(true)
    }
}
