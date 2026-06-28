//! Plugin status (T24): what `platform.plugin.<id>.state` carries, what the plugin probe keeps of it
//! in `core.plugin_status`, and what the probe and the backend say to each other on `core.plugins`.

use async_trait::async_trait;
use chrono::{DateTime, SubsecRound, Utc};
use doc_plugin_protocol::{Classification, PluginState};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::Health;
use crate::db::repositories::RepositoryError;

/// `core.plugins`, which the backend answers for the platform alone.
pub const SERVICE: &str = "plugins";
pub const REGISTRY: &str = "registry";
pub const TIME_OUT: &str = "time-out";
pub const TIMED_OUT: &str = "timed out";

/// To the microsecond, as Postgres keeps it, so a timestamp read back equals the one sent.
pub fn now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

/// One registration's state and error as of `at`; no state means it has left the registry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginChange {
    pub plugin: String,
    pub version: String,
    pub classification: Classification,
    pub instance: Uuid,
    pub state: Option<PluginState>,
    pub error: Option<String>,
    /// When the registration entered this state; a later error on the same state keeps it.
    pub since: DateTime<Utc>,
    /// When the state or the error last changed.
    pub at: DateTime<Utc>,
    pub registered_at: DateTime<Utc>,
}

/// The registry as the backend held it at `at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Snapshot {
    pub at: DateTime<Utc>,
    pub plugins: Vec<PluginChange>,
}

/// The probe asking for one stay in `loading` or `unloading` to be put into `error`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimeOut {
    pub plugin: String,
    pub instance: Uuid,
    pub since: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Verdict {
    pub timed_out: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

impl Verdict {
    pub fn timed_out() -> Self {
        Self { timed_out: true, reason: None }
    }

    pub fn not(reason: impl Into<String>) -> Self {
        Self { timed_out: false, reason: Some(reason.into()) }
    }
}

/// One row of `core.plugin_status`: the probe's view of a plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginStatus {
    pub plugin: String,
    pub version: String,
    pub classification: Classification,
    pub instance: Uuid,
    pub state: Option<PluginState>,
    pub error: Option<String>,
    pub since: DateTime<Utc>,
    pub at: DateTime<Utc>,
    pub registered_at: DateTime<Utc>,
    /// The most recent error, kept after the plugin has recovered from it.
    pub last_error: Option<String>,
    pub last_error_at: Option<DateTime<Utc>>,
    /// When the probe last recorded a change or compared the row with the registry.
    pub checked_at: DateTime<Utc>,
}

impl PluginStatus {
    pub fn removed(&self, at: DateTime<Utc>) -> PluginChange {
        PluginChange {
            plugin: self.plugin.clone(),
            version: self.version.clone(),
            classification: self.classification,
            instance: self.instance,
            state: None,
            error: None,
            since: at,
            at,
            registered_at: self.registered_at,
        }
    }
}

/// One row of `core.plugin_status_history`, as a plugin's page lists it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusChange {
    pub version: String,
    pub instance: Uuid,
    /// §5's state, or `removed` once the plugin had left the registry.
    pub state: String,
    pub error: Option<String>,
    pub at: DateTime<Utc>,
    /// `event`, or `registry` when a comparison found the event missing.
    pub source: String,
}

/// Where a change was learned: its event, or a comparison that found the event missing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Event,
    Registry,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Event => "event",
            Self::Registry => "registry",
        }
    }
}

#[async_trait]
pub trait PluginStatuses: Send + Sync {
    async fn current(&self) -> Result<Vec<PluginStatus>, RepositoryError>;

    /// Into the history once however often it arrives, and the current row unless that is newer.
    async fn record(&self, change: &PluginChange, source: Source) -> Result<(), RepositoryError>;

    async fn checked(&self, at: DateTime<Utc>) -> Result<(), RepositoryError>;

    /// Newest first.
    async fn history(&self, plugin: &str, limit: u32)
    -> Result<Vec<StatusChange>, RepositoryError>;

    /// Every plugin's changes from `from` up to but not including `to`, oldest first.
    async fn changes(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, StatusChange)>, RepositoryError>;

    async fn prune(&self, before: DateTime<Utc>) -> Result<u64, RepositoryError>;
}

/// Only `running` is up; a plugin that has left the registry is unknown rather than down.
pub fn health(state: Option<PluginState>) -> Health {
    match state {
        Some(PluginState::Running) => Health::Up,
        Some(PluginState::Loading | PluginState::Cancelled | PluginState::Unloading) => {
            Health::Degraded
        }
        Some(PluginState::Error) => Health::Down,
        None => Health::Unknown,
    }
}
