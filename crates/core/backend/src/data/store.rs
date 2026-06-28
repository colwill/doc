//! What a data store does: keep each plugin's collections the way its declarations say, read them,
//! and give writes a transaction. The rules a write must keep are applied once, above any store.

use async_trait::async_trait;
use doc_plugin_protocol::data::Declaration;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::query::{AggregatePlan, Plan, Record, Shape};

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    /// The key, or a unique constraint's values, belong to another record already.
    #[error("{0}")]
    Duplicate(String),
    #[error("{0}")]
    BadQuery(String),
    #[error("{0}")]
    Forbidden(String),
    #[error("{0}")]
    NoCollection(String),
    #[error("{0}")]
    BadRequest(String),
    #[error("the request used up its 5 s of database time")]
    Timeout,
    #[error("storage unavailable: {0}")]
    Unavailable(String),
}

/// What the serving version declared, and what storage holds: during a handover, both versions'.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Declarations {
    pub serving: Option<Declaration>,
    pub storage: Declaration,
}

#[async_trait]
pub trait DataStore: Send + Sync {
    async fn declarations(&self, plugin: &str) -> Result<Declarations, DataError>;

    /// Makes storage hold exactly `to` and records it, with `serving` once a version has taken over.
    async fn apply(
        &self,
        plugin: &str,
        to: &Declaration,
        serving: Option<&Declaration>,
    ) -> Result<(), DataError>;

    async fn get(
        &self,
        plugin: &str,
        shape: &Shape,
        key: &Value,
    ) -> Result<Option<Record>, DataError>;

    /// A page of records, whole, and the cursor for the next page.
    async fn query(
        &self,
        plugin: &str,
        shape: &Shape,
        plan: &Plan,
    ) -> Result<(Vec<Record>, Option<String>), DataError>;

    async fn aggregate(
        &self,
        plugin: &str,
        shape: &Shape,
        plan: &AggregatePlan,
    ) -> Result<Vec<Record>, DataError>;

    async fn begin(&self, plugin: &str) -> Result<Box<dyn Writer>, DataError>;
}

/// One transaction's writes, undone unless committed.
#[async_trait]
pub trait Writer: Send {
    /// The record under `key`, which no other transaction can change until this one ends.
    async fn lock(&mut self, shape: &Shape, key: &Value) -> Result<Option<Record>, DataError>;

    async fn exists(&mut self, shape: &Shape, key: &Value) -> Result<bool, DataError>;

    /// Records whose fields equal these values, locked like `lock`.
    async fn find(
        &mut self,
        shape: &Shape,
        equal: &[(String, Value)],
    ) -> Result<Vec<Record>, DataError>;

    /// Makes transactions looking for the same values wait for this one, as before an upsert.
    async fn guard(&mut self, shape: &Shape, equal: &[(String, Value)]) -> Result<(), DataError>;

    async fn insert(&mut self, shape: &Shape, record: &Record) -> Result<(), DataError>;

    async fn update(&mut self, shape: &Shape, record: &Record) -> Result<(), DataError>;

    async fn delete(&mut self, shape: &Shape, key: &Value) -> Result<(), DataError>;

    async fn commit(self: Box<Self>) -> Result<(), DataError>;
}
