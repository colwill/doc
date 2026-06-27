//! Cache Bus: namespaces with TTLs, get/set/delete and compare-and-set.

pub mod memory;
pub mod network;
pub mod postgres;
#[cfg(feature = "server")]
pub mod server;
pub mod wire;

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use memory::MemoryCacheBus;
pub use network::NetworkCacheBus;
pub use postgres::PostgresCacheBus;

#[derive(Debug, thiserror::Error)]
pub enum CacheBusError {
    #[error("invalid namespace: {0}")]
    InvalidNamespace(String),
    #[error("cache bus unavailable: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Other(String),
}

/// A cache namespace such as `core.sessions` or `plugin.kb`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Namespace(String);

impl Namespace {
    pub fn new(namespace: impl Into<String>) -> Result<Self, CacheBusError> {
        let namespace = namespace.into();
        let valid = !namespace.is_empty()
            && namespace.split('.').all(|segment| {
                !segment.is_empty()
                    && segment
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            });
        if valid { Ok(Self(namespace)) } else { Err(CacheBusError::InvalidNamespace(namespace)) }
    }

    pub fn plugin(id: &str) -> Result<Self, CacheBusError> {
        Self::new(format!("plugin.{id}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Namespace {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for Namespace {
    type Error = CacheBusError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<Namespace> for String {
    fn from(namespace: Namespace) -> Self {
        namespace.0
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub value: Value,
    /// Changes on every write; compare-and-set uses it to detect a lost update.
    pub version: u64,
    pub expires_at: Option<DateTime<Utc>>,
}

/// What the backend registers for a namespace: its default TTL and how much it may hold.
#[derive(Debug, Clone)]
pub struct NamespaceSpec {
    pub namespace: Namespace,
    pub default_ttl: Option<Duration>,
    pub max_entries: usize,
}

impl NamespaceSpec {
    pub fn new(namespace: Namespace, default_ttl: Option<Duration>) -> Self {
        Self { namespace, default_ttl, max_entries: 100_000 }
    }

    pub fn with_max_entries(mut self, max_entries: usize) -> Self {
        self.max_entries = max_entries;
        self
    }
}

#[async_trait]
pub trait CacheBus: Send + Sync {
    async fn register_namespace(&self, spec: NamespaceSpec) -> Result<(), CacheBusError>;

    async fn get(&self, namespace: &Namespace, key: &str) -> Result<Option<Entry>, CacheBusError>;

    /// `ttl` of `None` uses the namespace's default.
    async fn set(
        &self,
        namespace: &Namespace,
        key: &str,
        value: Value,
        ttl: Option<Duration>,
    ) -> Result<Entry, CacheBusError>;

    async fn delete(&self, namespace: &Namespace, key: &str) -> Result<bool, CacheBusError>;

    /// Writes only if the current version matches; `None` means the key must be absent.
    async fn compare_and_set(
        &self,
        namespace: &Namespace,
        key: &str,
        expected: Option<u64>,
        value: Value,
        ttl: Option<Duration>,
    ) -> Result<Option<Entry>, CacheBusError>;

    /// Drops every key in a namespace, used when a permission or token change invalidates it.
    async fn clear(&self, namespace: &Namespace) -> Result<usize, CacheBusError>;
}
