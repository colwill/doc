//! What a cron schedule needs from storage, and an in-memory implementation for endpoint tests.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use crate::CronTask;

#[derive(Debug, thiserror::Error)]
pub enum CronError {
    #[error("storage unavailable: {0}")]
    Unavailable(String),
    #[error("{0} is not a cron schedule")]
    Schedule(String),
    #[error("{0}")]
    Other(String),
}

#[async_trait]
pub trait CronStore: Send + Sync {
    /// Records a schedule the first time it is seen. A schedule already there keeps its paused flag
    /// and its next run, so restarting a worker does not bring every schedule forward.
    async fn upsert(
        &self,
        name: &str,
        schedule: &str,
        description: Option<&str>,
        next_run_at: DateTime<Utc>,
    ) -> Result<CronTask, CronError>;

    async fn list(&self) -> Result<Vec<CronTask>, CronError>;

    async fn get(&self, name: &str) -> Result<Option<CronTask>, CronError>;

    async fn set_paused(&self, name: &str, paused: bool) -> Result<bool, CronError>;

    /// Takes the next due, unclaimed schedule this worker can run: named in `known`, or under a prefix.
    async fn claim_due(
        &self,
        worker: &str,
        lease: Duration,
        known: &[String],
        prefixes: &[String],
    ) -> Result<Option<CronTask>, CronError>;

    /// Removes the schedules under `prefix` that are not in `keep`, as when a plugin drops one.
    async fn prune(&self, prefix: &str, keep: &[String]) -> Result<u64, CronError>;

    async fn finish(
        &self,
        name: &str,
        next_run_at: DateTime<Utc>,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), CronError>;
}

#[derive(Default)]
pub struct MemoryCron {
    held: Mutex<Vec<(CronTask, Option<DateTime<Utc>>)>>,
}

impl MemoryCron {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Brings a schedule forward so a test does not have to wait for its next occurrence.
    pub fn make_due(&self, name: &str) {
        let mut held = self.held.lock();
        if let Some((task, claimed)) = held.iter_mut().find(|(task, _)| task.name == name) {
            task.next_run_at = Some(Utc::now() - chrono::Duration::seconds(1));
            *claimed = None;
        }
    }
}

#[async_trait]
impl CronStore for MemoryCron {
    async fn upsert(
        &self,
        name: &str,
        schedule: &str,
        description: Option<&str>,
        next_run_at: DateTime<Utc>,
    ) -> Result<CronTask, CronError> {
        let mut held = self.held.lock();
        if let Some((task, _)) = held.iter_mut().find(|(task, _)| task.name == name) {
            task.schedule = schedule.to_string();
            task.description = description.map(str::to_string);
            return Ok(task.clone());
        }
        let task = CronTask {
            name: name.to_string(),
            schedule: schedule.to_string(),
            description: description.map(str::to_string),
            paused: false,
            next_run_at: Some(next_run_at),
            last_run_at: None,
            last_state: None,
            last_error: None,
        };
        held.push((task.clone(), None));
        Ok(task)
    }

    async fn list(&self) -> Result<Vec<CronTask>, CronError> {
        let mut found: Vec<CronTask> =
            self.held.lock().iter().map(|(task, _)| task.clone()).collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }

    async fn get(&self, name: &str) -> Result<Option<CronTask>, CronError> {
        Ok(self.held.lock().iter().find(|(task, _)| task.name == name).map(|(t, _)| t.clone()))
    }

    async fn set_paused(&self, name: &str, paused: bool) -> Result<bool, CronError> {
        let mut held = self.held.lock();
        let Some((task, _)) = held.iter_mut().find(|(task, _)| task.name == name) else {
            return Ok(false);
        };
        task.paused = paused;
        Ok(true)
    }

    async fn claim_due(
        &self,
        _worker: &str,
        lease: Duration,
        known: &[String],
        prefixes: &[String],
    ) -> Result<Option<CronTask>, CronError> {
        let now = Utc::now();
        let mut held = self.held.lock();
        let found = held.iter_mut().find(|(task, claimed)| {
            !task.paused
                && (known.contains(&task.name)
                    || prefixes.iter().any(|prefix| task.name.starts_with(prefix)))
                && task.next_run_at.is_some_and(|next| next <= now)
                && claimed.is_none_or(|until| until < now)
        });
        let Some((task, claimed)) = found else { return Ok(None) };
        *claimed = Some(now + chrono::Duration::from_std(lease).unwrap_or_default());
        Ok(Some(task.clone()))
    }

    async fn prune(&self, prefix: &str, keep: &[String]) -> Result<u64, CronError> {
        let mut held = self.held.lock();
        let before = held.len();
        held.retain(|(task, _)| !task.name.starts_with(prefix) || keep.contains(&task.name));
        Ok(u64::try_from(before - held.len()).unwrap_or_default())
    }

    async fn finish(
        &self,
        name: &str,
        next_run_at: DateTime<Utc>,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), CronError> {
        let mut held = self.held.lock();
        let Some((task, claimed)) = held.iter_mut().find(|(task, _)| task.name == name) else {
            return Ok(());
        };
        task.last_run_at = Some(Utc::now());
        task.last_state = Some(state.to_string());
        task.last_error = error.map(str::to_string);
        task.next_run_at = Some(next_run_at);
        *claimed = None;
        Ok(())
    }
}
