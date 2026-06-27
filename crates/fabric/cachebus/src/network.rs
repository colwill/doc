//! The networked Cache Bus client: the T07 trait over the cluster. Reads are stale by default
//! and served by any node; `get_consistent` goes through the leader.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use doc_consensus::{ClusterClient, ClusterError};
use serde_json::Value;

use crate::wire::{
    self, ClearResponse, CompareAndSetRequest, DeleteResponse, Empty, EntryResponse, EntryWire,
    GetRequest, KeyRequest, NamespaceRequest, RegisterNamespaceRequest, SetRequest,
};
use crate::{CacheBus, CacheBusError, Entry, Namespace, NamespaceSpec};

const DEADLINE: Duration = Duration::from_secs(5);

fn failed(error: &ClusterError) -> CacheBusError {
    CacheBusError::Unavailable(error.to_string())
}

fn entry(wire: EntryWire) -> Entry {
    Entry {
        value: wire.value,
        version: wire.version,
        expires_at: wire.expires_at_ms.and_then(|at| Utc.timestamp_millis_opt(at as i64).single()),
    }
}

#[derive(Clone)]
pub struct NetworkCacheBus {
    client: ClusterClient,
}

impl NetworkCacheBus {
    pub fn new(client: ClusterClient) -> Self {
        Self { client }
    }

    /// A read that is guaranteed to see every write acknowledged before it, via the leader.
    pub async fn get_consistent(
        &self,
        namespace: &Namespace,
        key: &str,
    ) -> Result<Option<Entry>, CacheBusError> {
        let request = GetRequest {
            namespace: namespace.as_str().to_string(),
            key: key.to_string(),
            consistent: true,
        };
        let response: EntryResponse =
            self.client.call(wire::GET, &request, DEADLINE).await.map_err(|e| failed(&e))?;
        Ok(response.entry.map(entry))
    }
}

#[async_trait]
impl CacheBus for NetworkCacheBus {
    async fn register_namespace(&self, spec: NamespaceSpec) -> Result<(), CacheBusError> {
        let request = RegisterNamespaceRequest {
            namespace: spec.namespace.as_str().to_string(),
            default_ttl_ms: spec.default_ttl.map(|ttl| ttl.as_millis() as u64),
            max_entries: spec.max_entries as u64,
        };
        let _: Empty = self
            .client
            .call(wire::REGISTER_NAMESPACE, &request, DEADLINE)
            .await
            .map_err(|e| failed(&e))?;
        Ok(())
    }

    async fn get(&self, namespace: &Namespace, key: &str) -> Result<Option<Entry>, CacheBusError> {
        let request = GetRequest {
            namespace: namespace.as_str().to_string(),
            key: key.to_string(),
            consistent: false,
        };
        let response: EntryResponse =
            self.client.call_any(wire::GET, &request, DEADLINE).await.map_err(|e| failed(&e))?;
        Ok(response.entry.map(entry))
    }

    async fn set(
        &self,
        namespace: &Namespace,
        key: &str,
        value: Value,
        ttl: Option<Duration>,
    ) -> Result<Entry, CacheBusError> {
        let request = SetRequest {
            namespace: namespace.as_str().to_string(),
            key: key.to_string(),
            value,
            ttl_ms: ttl.map(|ttl| ttl.as_millis() as u64),
        };
        let response: EntryResponse =
            self.client.call(wire::SET, &request, DEADLINE).await.map_err(|e| failed(&e))?;
        response
            .entry
            .map(entry)
            .ok_or_else(|| CacheBusError::Other("the cache did not return the entry".into()))
    }

    async fn delete(&self, namespace: &Namespace, key: &str) -> Result<bool, CacheBusError> {
        let request =
            KeyRequest { namespace: namespace.as_str().to_string(), key: key.to_string() };
        let response: DeleteResponse =
            self.client.call(wire::DELETE, &request, DEADLINE).await.map_err(|e| failed(&e))?;
        Ok(response.existed)
    }

    async fn compare_and_set(
        &self,
        namespace: &Namespace,
        key: &str,
        expected: Option<u64>,
        value: Value,
        ttl: Option<Duration>,
    ) -> Result<Option<Entry>, CacheBusError> {
        let request = CompareAndSetRequest {
            namespace: namespace.as_str().to_string(),
            key: key.to_string(),
            expected,
            value,
            ttl_ms: ttl.map(|ttl| ttl.as_millis() as u64),
        };
        let response: EntryResponse = self
            .client
            .call(wire::COMPARE_AND_SET, &request, DEADLINE)
            .await
            .map_err(|e| failed(&e))?;
        Ok(response.entry.map(entry))
    }

    async fn clear(&self, namespace: &Namespace) -> Result<usize, CacheBusError> {
        let request = NamespaceRequest { namespace: namespace.as_str().to_string() };
        let response: ClearResponse =
            self.client.call(wire::CLEAR, &request, DEADLINE).await.map_err(|e| failed(&e))?;
        Ok(response.removed as usize)
    }
}
