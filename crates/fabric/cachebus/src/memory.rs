//! In-memory Cache Bus: TTL expiry and oldest-first eviction once a namespace is full.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::Value;

use crate::{CacheBus, CacheBusError, Entry, Namespace, NamespaceSpec};

#[derive(Default)]
struct State {
    entries: HashMap<Namespace, HashMap<String, (Entry, u64)>>,
    specs: HashMap<Namespace, NamespaceSpec>,
    writes: u64,
}

#[derive(Clone, Default)]
pub struct MemoryCacheBus {
    state: Arc<Mutex<State>>,
}

impl MemoryCacheBus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self, namespace: &Namespace) -> usize {
        self.state.lock().entries.get(namespace).map(HashMap::len).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.state.lock().entries.values().all(HashMap::is_empty)
    }
}

fn live(entry: &Entry) -> bool {
    entry.expires_at.is_none_or(|at| at > Utc::now())
}

impl State {
    fn write(
        &mut self,
        namespace: &Namespace,
        key: &str,
        value: Value,
        ttl: Option<Duration>,
    ) -> Result<Entry, CacheBusError> {
        let spec = self.specs.get(namespace).cloned();
        let ttl = ttl.or_else(|| spec.as_ref().and_then(|s| s.default_ttl));
        let expires_at = match ttl {
            Some(ttl) => Some(
                Utc::now()
                    + chrono::Duration::from_std(ttl)
                        .map_err(|e| CacheBusError::Other(e.to_string()))?,
            ),
            None => None,
        };
        self.writes += 1;
        let sequence = self.writes;
        let max_entries = spec.map(|s| s.max_entries).unwrap_or(100_000);
        let entries = self.entries.entry(namespace.clone()).or_default();
        let version = entries.get(key).map(|(entry, _)| entry.version + 1).unwrap_or(1);
        let entry = Entry { value, version, expires_at };
        entries.insert(key.to_string(), (entry.clone(), sequence));
        if entries.len() > max_entries {
            entries.retain(|_, (entry, _)| live(entry));
        }
        while entries.len() > max_entries {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (_, sequence))| *sequence)
                .map(|(key, _)| key.clone())
            {
                entries.remove(&oldest);
            }
        }
        Ok(entry)
    }

    fn read(&self, namespace: &Namespace, key: &str) -> Option<Entry> {
        self.entries
            .get(namespace)
            .and_then(|entries| entries.get(key))
            .map(|(entry, _)| entry.clone())
            .filter(live)
    }
}

#[async_trait]
impl CacheBus for MemoryCacheBus {
    async fn register_namespace(&self, spec: NamespaceSpec) -> Result<(), CacheBusError> {
        let mut state = self.state.lock();
        state.entries.entry(spec.namespace.clone()).or_default();
        state.specs.insert(spec.namespace.clone(), spec);
        Ok(())
    }

    async fn get(&self, namespace: &Namespace, key: &str) -> Result<Option<Entry>, CacheBusError> {
        Ok(self.state.lock().read(namespace, key))
    }

    async fn set(
        &self,
        namespace: &Namespace,
        key: &str,
        value: Value,
        ttl: Option<Duration>,
    ) -> Result<Entry, CacheBusError> {
        self.state.lock().write(namespace, key, value, ttl)
    }

    async fn delete(&self, namespace: &Namespace, key: &str) -> Result<bool, CacheBusError> {
        let mut state = self.state.lock();
        Ok(state
            .entries
            .get_mut(namespace)
            .and_then(|entries| entries.remove(key))
            .is_some_and(|(entry, _)| live(&entry)))
    }

    async fn compare_and_set(
        &self,
        namespace: &Namespace,
        key: &str,
        expected: Option<u64>,
        value: Value,
        ttl: Option<Duration>,
    ) -> Result<Option<Entry>, CacheBusError> {
        let mut state = self.state.lock();
        let current = state.read(namespace, key).map(|entry| entry.version);
        if current != expected {
            return Ok(None);
        }
        state.write(namespace, key, value, ttl).map(Some)
    }

    async fn clear(&self, namespace: &Namespace) -> Result<usize, CacheBusError> {
        let mut state = self.state.lock();
        Ok(state
            .entries
            .get_mut(namespace)
            .map(|entries| {
                let count = entries.len();
                entries.clear();
                count
            })
            .unwrap_or(0))
    }
}
