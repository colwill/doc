//! Collections held in memory, for the endpoint tests: one lock per plugin makes each write
//! transaction run alone, on a copy that replaces the original only when it commits.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use doc_plugin_protocol::data::Declaration;
use serde_json::Value;
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::declaration::stored_kind;
use super::query::{self, AggregatePlan, Plan, Record, Shape};
use super::store::{DataError, DataStore, Declarations, Writer};
use super::values;

#[derive(Debug, Clone, Default)]
struct Plugin {
    declarations: Declarations,
    tables: BTreeMap<String, BTreeMap<String, Record>>,
}

#[derive(Default)]
pub struct MemoryData {
    plugins: parking_lot::Mutex<BTreeMap<String, Arc<Mutex<Plugin>>>>,
    /// Makes `apply` fail, so a test can drive what happens when storage cannot be prepared.
    broken: std::sync::atomic::AtomicBool,
}

pub fn key_text(key: &Value) -> String {
    key.as_str().map_or_else(|| key.to_string(), str::to_string)
}

impl MemoryData {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn set_broken(&self, broken: bool) {
        self.broken.store(broken, std::sync::atomic::Ordering::SeqCst);
    }

    fn plugin(&self, id: &str) -> Arc<Mutex<Plugin>> {
        self.plugins.lock().entry(id.to_string()).or_default().clone()
    }

    async fn records(&self, plugin: &str, shape: &Shape) -> Vec<Record> {
        let plugin = self.plugin(plugin);
        let held = plugin.lock().await;
        held.tables
            .get(&shape.name)
            .map(|table| table.values().cloned().collect())
            .unwrap_or_default()
    }
}

/// Existing records get a new field's default, and lose a field that is going.
fn reshape(plugin: &mut Plugin, to: &Declaration) -> Result<(), DataError> {
    let from = plugin.declarations.storage.clone();
    plugin.tables.retain(|name, _| to.collections.contains_key(name));
    for (name, collection) in &to.collections {
        let table = plugin.tables.entry(name.clone()).or_default();
        let before = from.collections.get(name);
        for (field_name, field) in &collection.fields {
            let kind = stored_kind(to, field);
            let unchanged = before
                .and_then(|before| before.fields.get(field_name))
                .is_some_and(|old| old.kind == field.kind && stored_kind(&from, old) == kind);
            if unchanged {
                continue;
            }
            for record in table.values_mut() {
                let value = values::default_for(field, kind).map_err(DataError::Invalid)?;
                record.insert(field_name.clone(), value);
            }
        }
        for record in table.values_mut() {
            record
                .retain(|field, _| field.starts_with('_') || collection.fields.contains_key(field));
        }
        for fields in &collection.unique {
            let mut seen = std::collections::BTreeSet::new();
            for record in table.values() {
                let values: Vec<&Value> =
                    fields.iter().map(|field| record.get(field).unwrap_or(&Value::Null)).collect();
                if values.iter().any(|value| value.is_null()) {
                    continue;
                }
                if !seen.insert(serde_json::to_string(&values).unwrap_or_default()) {
                    return Err(DataError::Invalid(format!(
                        "{name} has records that break its new unique constraint on {}",
                        fields.join(", ")
                    )));
                }
            }
        }
    }
    Ok(())
}

#[async_trait]
impl DataStore for MemoryData {
    async fn declarations(&self, plugin: &str) -> Result<Declarations, DataError> {
        Ok(self.plugin(plugin).lock().await.declarations.clone())
    }

    async fn apply(
        &self,
        plugin: &str,
        to: &Declaration,
        serving: Option<&Declaration>,
    ) -> Result<(), DataError> {
        if self.broken.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(DataError::Unavailable("the fake store refuses to change".into()));
        }
        let plugin = self.plugin(plugin);
        let mut held = plugin.lock().await;
        let mut changed = held.clone();
        reshape(&mut changed, to)?;
        changed.declarations.storage = to.clone();
        if let Some(serving) = serving {
            changed.declarations.serving = Some(serving.clone());
        }
        *held = changed;
        Ok(())
    }

    async fn get(
        &self,
        plugin: &str,
        shape: &Shape,
        key: &Value,
    ) -> Result<Option<Record>, DataError> {
        let plugin = self.plugin(plugin);
        let held = plugin.lock().await;
        Ok(held.tables.get(&shape.name).and_then(|table| table.get(&key_text(key)).cloned()))
    }

    async fn query(
        &self,
        plugin: &str,
        shape: &Shape,
        plan: &Plan,
    ) -> Result<(Vec<Record>, Option<String>), DataError> {
        let all = Shape { visible: shape.columns.keys().cloned().collect(), ..shape.clone() };
        let plan = Plan { fields: None, ..plan.clone() };
        Ok(query::page(&all, &plan, self.records(plugin, shape).await))
    }

    async fn aggregate(
        &self,
        plugin: &str,
        shape: &Shape,
        plan: &AggregatePlan,
    ) -> Result<Vec<Record>, DataError> {
        query::aggregate(plan, &self.records(plugin, shape).await).map_err(DataError::BadQuery)
    }

    async fn begin(&self, plugin: &str) -> Result<Box<dyn Writer>, DataError> {
        let held = self.plugin(plugin).lock_owned().await;
        let working = held.clone();
        Ok(Box::new(MemoryWriter { held, working }))
    }
}

struct MemoryWriter {
    held: OwnedMutexGuard<Plugin>,
    working: Plugin,
}

impl MemoryWriter {
    fn table(&mut self, shape: &Shape) -> &mut BTreeMap<String, Record> {
        self.working.tables.entry(shape.name.clone()).or_default()
    }
}

#[async_trait]
impl Writer for MemoryWriter {
    async fn lock(&mut self, shape: &Shape, key: &Value) -> Result<Option<Record>, DataError> {
        Ok(self.table(shape).get(&key_text(key)).cloned())
    }

    async fn exists(&mut self, shape: &Shape, key: &Value) -> Result<bool, DataError> {
        Ok(self.table(shape).contains_key(&key_text(key)))
    }

    async fn find(
        &mut self,
        shape: &Shape,
        equal: &[(String, Value)],
    ) -> Result<Vec<Record>, DataError> {
        Ok(self
            .table(shape)
            .values()
            .filter(|record| equal.iter().all(|(field, value)| record.get(field) == Some(value)))
            .cloned()
            .collect())
    }

    async fn guard(&mut self, _shape: &Shape, _equal: &[(String, Value)]) -> Result<(), DataError> {
        Ok(())
    }

    async fn insert(&mut self, shape: &Shape, record: &Record) -> Result<(), DataError> {
        let key = key_text(record.get(&shape.key).unwrap_or(&Value::Null));
        self.table(shape).insert(key, record.clone());
        Ok(())
    }

    async fn update(&mut self, shape: &Shape, record: &Record) -> Result<(), DataError> {
        self.insert(shape, record).await
    }

    async fn delete(&mut self, shape: &Shape, key: &Value) -> Result<(), DataError> {
        self.table(shape).remove(&key_text(key));
        Ok(())
    }

    async fn commit(mut self: Box<Self>) -> Result<(), DataError> {
        *self.held = std::mem::take(&mut self.working);
        Ok(())
    }
}
