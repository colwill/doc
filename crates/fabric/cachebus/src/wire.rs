//! The messages the Cache Bus client and service exchange over HTTP/3.

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const REGISTER_NAMESPACE: &str = "/cache/v1/register-namespace";
pub const GET: &str = "/cache/v1/get";
pub const SET: &str = "/cache/v1/set";
pub const DELETE: &str = "/cache/v1/delete";
pub const COMPARE_AND_SET: &str = "/cache/v1/compare-and-set";
pub const CLEAR: &str = "/cache/v1/clear";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterNamespaceRequest {
    pub namespace: String,
    pub default_ttl_ms: Option<u64>,
    pub max_entries: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GetRequest {
    pub namespace: String,
    pub key: String,
    /// Consistent reads go through the leader; stale reads are answered by any node.
    pub consistent: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SetRequest {
    pub namespace: String,
    pub key: String,
    pub value: Value,
    pub ttl_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompareAndSetRequest {
    pub namespace: String,
    pub key: String,
    pub expected: Option<u64>,
    pub value: Value,
    pub ttl_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyRequest {
    pub namespace: String,
    pub key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceRequest {
    pub namespace: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryWire {
    pub value: Value,
    pub version: u64,
    pub expires_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntryResponse {
    pub entry: Option<EntryWire>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteResponse {
    pub existed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClearResponse {
    pub removed: u64,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Empty {}
