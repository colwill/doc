//! The replicated Cache Bus state: namespaces of versioned entries with expiry times.
//! Expiry and eviction are proposed by the leader, so every node drops the same entries.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use doc_consensus::BusStateMachine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CacheCommand {
    RegisterNamespace {
        namespace: String,
        default_ttl_ms: Option<u64>,
        max_entries: u64,
    },
    Set {
        namespace: String,
        key: String,
        value: Value,
        ttl_ms: Option<u64>,
        now_ms: u64,
    },
    Delete {
        namespace: String,
        key: String,
        now_ms: u64,
    },
    CompareAndSet {
        namespace: String,
        key: String,
        expected: Option<u64>,
        value: Value,
        ttl_ms: Option<u64>,
        now_ms: u64,
    },
    Clear {
        namespace: String,
    },
    /// Removes expired entries and trims namespaces that are over their limit.
    Evict {
        now_ms: u64,
    },
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub enum CacheResponse {
    #[default]
    Empty,
    Entry {
        entry: Option<StoredEntry>,
    },
    Written {
        entry: StoredEntry,
    },
    Deleted {
        existed: bool,
    },
    Rejected,
    Cleared {
        removed: u64,
    },
    Evicted {
        expired: u64,
        evicted: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredEntry {
    pub value: Value,
    pub version: u64,
    pub expires_at_ms: Option<u64>,
}

impl StoredEntry {
    pub fn live(&self, now_ms: u64) -> bool {
        self.expires_at_ms.is_none_or(|at| at > now_ms)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceSpecStored {
    pub default_ttl_ms: Option<u64>,
    pub max_entries: u64,
}

impl Default for NamespaceSpecStored {
    fn default() -> Self {
        Self { default_ttl_ms: None, max_entries: 100_000 }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NamespaceData {
    pub entries: BTreeMap<String, StoredEntry>,
    /// Write order, which is what eviction uses: reads are local, so read recency cannot be
    /// replicated without a write per read.
    pub written: VecDeque<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CacheState {
    pub namespaces: BTreeMap<String, Arc<NamespaceData>>,
    pub specs: BTreeMap<String, NamespaceSpecStored>,
}

#[derive(Default)]
pub struct CacheMachine {
    state: CacheState,
}

impl CacheMachine {
    pub fn spec(&self, namespace: &str) -> NamespaceSpecStored {
        self.state.specs.get(namespace).cloned().unwrap_or_default()
    }

    /// Expired entries are invisible everywhere at once, before the leader evicts them.
    pub fn get(&self, namespace: &str, key: &str, now_ms: u64) -> Option<StoredEntry> {
        self.state
            .namespaces
            .get(namespace)
            .and_then(|data| data.entries.get(key))
            .filter(|entry| entry.live(now_ms))
            .cloned()
    }

    pub fn len(&self, namespace: &str, now_ms: u64) -> usize {
        self.state
            .namespaces
            .get(namespace)
            .map(|data| data.entries.values().filter(|entry| entry.live(now_ms)).count())
            .unwrap_or(0)
    }

    pub fn needs_eviction(&self, now_ms: u64) -> bool {
        self.state.namespaces.iter().any(|(namespace, data)| {
            data.entries.values().any(|entry| !entry.live(now_ms))
                || data.entries.len() as u64 > self.spec(namespace).max_entries
        })
    }

    fn write(
        &mut self,
        namespace: String,
        key: String,
        value: Value,
        ttl_ms: Option<u64>,
        now_ms: u64,
    ) -> StoredEntry {
        let spec = self.spec(&namespace);
        let ttl = ttl_ms.or(spec.default_ttl_ms);
        let expires_at_ms = ttl.map(|ttl| now_ms + ttl);
        let data = Arc::make_mut(self.state.namespaces.entry(namespace).or_default());
        let version = data.entries.get(&key).map(|entry| entry.version + 1).unwrap_or(1);
        let entry = StoredEntry { value, version, expires_at_ms };
        data.entries.insert(key.clone(), entry.clone());
        data.written.retain(|written| written != &key);
        data.written.push_back(key);
        entry
    }
}

impl BusStateMachine for CacheMachine {
    type Command = CacheCommand;
    type Response = CacheResponse;
    type Snapshot = CacheState;

    fn apply(&mut self, _log_index: u64, command: CacheCommand) -> CacheResponse {
        match command {
            CacheCommand::RegisterNamespace { namespace, default_ttl_ms, max_entries } => {
                self.state
                    .specs
                    .insert(namespace.clone(), NamespaceSpecStored { default_ttl_ms, max_entries });
                self.state.namespaces.entry(namespace).or_default();
                CacheResponse::Empty
            }
            CacheCommand::Set { namespace, key, value, ttl_ms, now_ms } => {
                CacheResponse::Written { entry: self.write(namespace, key, value, ttl_ms, now_ms) }
            }
            CacheCommand::Delete { namespace, key, now_ms } => {
                let Some(entry) = self.state.namespaces.get_mut(&namespace) else {
                    return CacheResponse::Deleted { existed: false };
                };
                let data = Arc::make_mut(entry);
                let existed = data.entries.remove(&key).is_some_and(|entry| entry.live(now_ms));
                data.written.retain(|written| written != &key);
                CacheResponse::Deleted { existed }
            }
            CacheCommand::CompareAndSet { namespace, key, expected, value, ttl_ms, now_ms } => {
                let current = self.get(&namespace, &key, now_ms).map(|entry| entry.version);
                if current != expected {
                    return CacheResponse::Rejected;
                }
                CacheResponse::Written { entry: self.write(namespace, key, value, ttl_ms, now_ms) }
            }
            CacheCommand::Clear { namespace } => {
                let Some(entry) = self.state.namespaces.get_mut(&namespace) else {
                    return CacheResponse::Cleared { removed: 0 };
                };
                let data = Arc::make_mut(entry);
                let removed = data.entries.len() as u64;
                data.entries.clear();
                data.written.clear();
                CacheResponse::Cleared { removed }
            }
            CacheCommand::Evict { now_ms } => {
                let specs = self.state.specs.clone();
                let (mut expired, mut evicted) = (0, 0);
                for (namespace, entry) in self.state.namespaces.iter_mut() {
                    let spec = specs.get(namespace).cloned().unwrap_or_default();
                    let stale: Vec<String> = entry
                        .entries
                        .iter()
                        .filter(|(_, entry)| !entry.live(now_ms))
                        .map(|(key, _)| key.clone())
                        .collect();
                    let over = entry.entries.len().saturating_sub(stale.len()) as u64;
                    if stale.is_empty() && over <= spec.max_entries {
                        continue;
                    }
                    let data = Arc::make_mut(entry);
                    for key in stale {
                        data.entries.remove(&key);
                        data.written.retain(|written| written != &key);
                        expired += 1;
                    }
                    while data.entries.len() as u64 > spec.max_entries {
                        let Some(oldest) = data.written.pop_front() else {
                            break;
                        };
                        if data.entries.remove(&oldest).is_some() {
                            evicted += 1;
                        }
                    }
                }
                CacheResponse::Evicted { expired, evicted }
            }
        }
    }

    fn snapshot(&self) -> CacheState {
        self.state.clone()
    }

    fn restore(&mut self, snapshot: CacheState) {
        self.state = snapshot;
    }
}
