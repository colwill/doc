//! Plugins' declared data (T62, DOC-SPEC §4.3 and §9.1): what registration checks, storage prepared
//! before `load` and trimmed once a handover completes, and `/plugin/v1/data`, whose requests are
//! checked here, once, before any store sees them.

pub mod core;
pub mod declaration;
pub mod memory;
pub mod postgres;
pub mod query;
pub mod store;
pub mod values;

#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use doc_eventbus::{Event, Topic};
use doc_plugin_protocol::Manifest;
use doc_plugin_protocol::data::{
    Change, Changed, Collection, DataRequest, Declaration, FieldType, MAX_BATCH, MAX_RECORD_BYTES,
    OnDelete, SYSTEM_FIELDS,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::api::AppState;
use crate::plugins::RegisterError;
use crate::plugins::api::Refusal;
use declaration::stored_kind;
use query::{Column, Record, Shape};
use store::DataError;

pub const DATA_TIMEOUT: Duration = Duration::from_secs(5);
const SOURCE: &str = "core.data";

impl From<DataError> for Refusal {
    fn from(err: DataError) -> Self {
        let (status, kind) = match &err {
            DataError::Invalid(_) => (400, "invalid-record"),
            DataError::Conflict(_) => (409, "version-conflict"),
            DataError::Duplicate(_) => (409, "duplicate-record"),
            DataError::BadQuery(_) => (400, "bad-query"),
            DataError::Forbidden(_) => (403, "forbidden"),
            DataError::NoCollection(_) => (404, "no-collection"),
            DataError::BadRequest(_) => (400, "bad-request"),
            DataError::Timeout => (504, "data-timeout"),
            DataError::Unavailable(_) => (503, "unavailable"),
        };
        Refusal::new(status, kind, err.to_string())
    }
}

/// Registration's checks: a well-formed declaration that can take over from the serving version's.
pub async fn admit(state: &AppState, manifest: &Manifest) -> Result<(), RegisterError> {
    declaration::validate(&manifest.data).map_err(RegisterError::BadData)?;
    let current = state
        .repos
        .data
        .declarations(&manifest.id)
        .await
        .map_err(|err| RegisterError::Storage(err.to_string()))?;
    if let Some(serving) = &current.serving {
        declaration::compatible(serving, &manifest.data)
            .map_err(RegisterError::IncompatibleData)?;
    }
    Ok(())
}

/// Before `load`: storage gains what the new version adds, and keeps what the serving one needs.
pub async fn prepare(
    state: &AppState,
    plugin: &str,
    instance: Uuid,
    declared: &Declaration,
) -> Result<(), String> {
    let mut prepared = state.plugins.prepared.lock().await;
    let current = state.repos.data.declarations(plugin).await.map_err(|err| err.to_string())?;
    let to = declaration::merged(&current.storage, declared);
    if to != current.storage {
        state.repos.data.apply(plugin, &to, None).await.map_err(|err| err.to_string())?;
    }
    prepared.insert(plugin.to_string(), instance);
    Ok(())
}

/// Once a version has taken over, it is the baseline and what it no longer declares goes.
pub async fn complete(state: &AppState, plugin: &str, instance: Uuid, declared: &Declaration) {
    let prepared = state.plugins.prepared.lock().await;
    if prepared.get(plugin) != Some(&instance) {
        tracing::debug!(plugin, "a newer registration was prepared, so storage is left for it");
        return;
    }
    if let Err(err) = state.repos.data.apply(plugin, declared, Some(declared)).await {
        tracing::warn!(plugin, %err, "what the running version no longer declares was not removed");
    }
}

fn system_columns() -> [(String, Column); 3] {
    let timestamp = Column { kind: FieldType::Timestamp, of: None };
    [
        ("_version".into(), Column { kind: FieldType::Integer, of: None }),
        ("_created_at".into(), timestamp),
        ("_updated_at".into(), timestamp),
    ]
}

/// A collection as its owner sees it: every field, the system fields, and what it can be sorted by.
pub fn shape_of(name: &str, collection: &Collection, declared: &Declaration) -> Shape {
    let mut columns: BTreeMap<String, Column> = collection
        .fields
        .iter()
        .map(|(field, declared_field)| {
            let kind = stored_kind(declared, declared_field);
            (field.clone(), Column { kind, of: declared_field.of })
        })
        .collect();
    columns.extend(system_columns());
    Shape {
        name: name.to_string(),
        key: collection.key().unwrap_or_default().to_string(),
        visible: columns.keys().cloned().collect(),
        columns,
        sortable: collection.indexes.iter().chain(&collection.unique).cloned().collect(),
        search: collection.search.clone(),
    }
}

fn core_shape(name: &str, collection: &Collection) -> Shape {
    Shape { name: format!("core.{name}"), ..shape_of(name, collection, &core::DECLARATION) }
        .without_system()
}

impl Shape {
    fn without_system(mut self) -> Self {
        for (field, _) in system_columns() {
            self.columns.remove(&field);
            self.visible.remove(&field);
        }
        self
    }
}

enum Source {
    Store { owner: String, shape: Shape },
    Core { name: String, shape: Shape },
}

/// Whose collection `name` is, and what of it `plugin` may read.
async fn readable(state: &AppState, plugin: &str, name: &str) -> Result<Source, DataError> {
    let (owner, collection) = name.split_once('.').unwrap_or((plugin, name));
    let missing = || DataError::NoCollection(format!("there is no collection {name}"));
    if owner == "core" {
        let declared = core::DECLARATION.collections.get(collection).ok_or_else(missing)?;
        let shape = core_shape(collection, declared);
        return Ok(Source::Core { name: collection.to_string(), shape });
    }
    let declarations = state.repos.data.declarations(owner).await?;
    let declared = declarations.storage.collections.get(collection).ok_or_else(missing)?;
    let mut shape = shape_of(collection, declared, &declarations.storage);
    if owner != plugin {
        let export = declarations
            .serving
            .as_ref()
            .and_then(|serving| serving.collections.get(collection))
            .and_then(|serving| serving.export.clone())
            .filter(|export| export.read.includes(plugin))
            .ok_or_else(|| DataError::Forbidden(format!("{name} is not exported to {plugin}")))?;
        if let Some(fields) = export.fields {
            shape.visible.retain(|field| field.starts_with('_') || fields.contains(field));
        }
        if shape.search.iter().any(|field| !shape.visible.contains(field)) {
            shape.search.clear();
        }
    }
    Ok(Source::Store { owner: owner.to_string(), shape })
}

/// `POST /plugin/v1/data`, as `plugin`, within the time a request may take.
pub async fn handle(
    state: &AppState,
    plugin: &str,
    request: DataRequest,
) -> Result<Value, Refusal> {
    let answered = tokio::time::timeout(DATA_TIMEOUT, answer(state, plugin, request)).await;
    let (answer, changes) = answered.map_err(|_| DataError::Timeout)??;
    announce(state, plugin, changes).await;
    Ok(answer)
}

async fn answer(
    state: &AppState,
    plugin: &str,
    request: DataRequest,
) -> Result<(Value, Vec<(String, Change)>), DataError> {
    if !request.is_write() {
        return Ok((read(state, plugin, request).await?, Vec::new()));
    }
    let (writes, batch) = match request {
        DataRequest::Batch { writes } => {
            if writes.is_empty() || writes.len() > MAX_BATCH {
                return Err(DataError::BadRequest(format!(
                    "a batch holds 1 to {MAX_BATCH} writes"
                )));
            }
            let nested = |write: &DataRequest| matches!(write, DataRequest::Batch { .. });
            if writes.iter().any(|write| !write.is_write() || nested(write)) {
                return Err(DataError::BadRequest(
                    "a batch holds only inserts, updates, upserts and deletes".into(),
                ));
            }
            (writes, true)
        }
        single => (vec![single], false),
    };
    let declarations = state.repos.data.declarations(plugin).await?;
    let writer = state.repos.data.begin(plugin).await?;
    let mut tx = Tx { state, plugin, declared: declarations.storage, writer, changes: Vec::new() };
    let mut results = Vec::with_capacity(writes.len());
    for write in writes {
        results.push(tx.write(write).await?);
    }
    let answer = match batch {
        true => json!({ "results": results }),
        false => results.pop().unwrap_or_default(),
    };
    tx.writer.commit().await?;
    Ok((answer, tx.changes))
}

async fn read(state: &AppState, plugin: &str, request: DataRequest) -> Result<Value, DataError> {
    let collection = request.collection().unwrap_or_default().to_string();
    match readable(state, plugin, &collection).await? {
        Source::Store { owner, shape } => read_store(state, &owner, &shape, request).await,
        Source::Core { name, shape } => {
            let filter = match &request {
                DataRequest::Query(query) => query.filter.clone(),
                DataRequest::Aggregate(aggregate) => aggregate.filter.clone(),
                _ => None,
            };
            let records = core::records(state, plugin, &name, filter.as_ref()).await?;
            read_records(&shape, records, request)
        }
    }
}

fn key_of(shape: &Shape, key: &Value) -> Result<Value, DataError> {
    values::scalar(shape.key_kind(), None, key)
        .map_err(|err| DataError::BadQuery(format!("the key {err}")))
}

async fn read_store(
    state: &AppState,
    owner: &str,
    shape: &Shape,
    request: DataRequest,
) -> Result<Value, DataError> {
    let data = &state.repos.data;
    match request {
        DataRequest::Get { key, .. } => {
            let record = data.get(owner, shape, &key_of(shape, &key)?).await?;
            Ok(json!({ "record": record.map(|record| shape.project(record, None)) }))
        }
        DataRequest::Query(query) => {
            let plan = query::plan(shape, &query).map_err(DataError::BadQuery)?;
            let (found, next) = data.query(owner, shape, &plan).await?;
            let found: Vec<Record> = found
                .into_iter()
                .map(|record| shape.project(record, plan.fields.as_deref()))
                .collect();
            Ok(json!({ "records": found, "next": next }))
        }
        DataRequest::Aggregate(aggregate) => {
            let plan = query::aggregate_plan(shape, &aggregate).map_err(DataError::BadQuery)?;
            Ok(json!({ "groups": data.aggregate(owner, shape, &plan).await? }))
        }
        _ => Err(DataError::BadRequest("not a read".into())),
    }
}

/// The same reads over records already in memory, which is how `core.*` is answered.
fn read_records(
    shape: &Shape,
    records: Vec<Record>,
    request: DataRequest,
) -> Result<Value, DataError> {
    match request {
        DataRequest::Get { key, .. } => {
            let key = key_of(shape, &key)?;
            let record = records.into_iter().find(|record| record.get(&shape.key) == Some(&key));
            Ok(json!({ "record": record.map(|record| shape.project(record, None)) }))
        }
        DataRequest::Query(query) => {
            let plan = query::plan(shape, &query).map_err(DataError::BadQuery)?;
            let (found, next) = query::page(shape, &plan, records);
            Ok(json!({ "records": found, "next": next }))
        }
        DataRequest::Aggregate(aggregate) => {
            let plan = query::aggregate_plan(shape, &aggregate).map_err(DataError::BadQuery)?;
            let groups = query::aggregate(&plan, &records).map_err(DataError::BadQuery)?;
            Ok(json!({ "groups": groups }))
        }
        _ => Err(DataError::BadRequest("not a read".into())),
    }
}

fn object(value: Value, what: &str) -> Result<Map<String, Value>, DataError> {
    match value {
        Value::Object(map) => Ok(map),
        _ => Err(DataError::BadRequest(format!("`{what}` is an object of field values"))),
    }
}

fn describe(key: &Value) -> String {
    key.as_str().map_or_else(|| key.to_string(), str::to_string)
}

struct Tx<'a> {
    state: &'a AppState,
    plugin: &'a str,
    declared: Declaration,
    writer: Box<dyn store::Writer>,
    changes: Vec<(String, Change)>,
}

impl Tx<'_> {
    /// The plugin's own collection `name`, which is the only kind it may write.
    fn own(&self, name: &str) -> Result<(Collection, Shape), DataError> {
        let plain = match name.split_once('.') {
            Some((owner, rest)) if owner == self.plugin => rest,
            Some(_) => {
                return Err(DataError::Forbidden(format!(
                    "{} can only write its own collections, not {name}",
                    self.plugin
                )));
            }
            None => name,
        };
        let collection = self
            .declared
            .collections
            .get(plain)
            .cloned()
            .ok_or_else(|| DataError::NoCollection(format!("there is no collection {name}")))?;
        let shape = shape_of(plain, &collection, &self.declared);
        Ok((collection, shape))
    }

    async fn write(&mut self, request: DataRequest) -> Result<Value, DataError> {
        match request {
            DataRequest::Insert { collection, values } => {
                let record = self.insert(&collection, object(values, "values")?).await?;
                Ok(json!({ "record": record }))
            }
            DataRequest::Update { collection, key, set, version } => {
                let record = self.update(&collection, &key, object(set, "set")?, version).await?;
                Ok(json!({ "record": record }))
            }
            DataRequest::Upsert { collection, on, values } => {
                let (record, created) =
                    self.upsert(&collection, &on, object(values, "values")?).await?;
                Ok(json!({ "record": record, "created": created }))
            }
            DataRequest::Delete { collection, key, version } => {
                let deleted = self.delete(&collection, &key, version).await?;
                Ok(json!({ "deleted": deleted }))
            }
            _ => Err(DataError::BadRequest("not a write".into())),
        }
    }

    fn checked(
        &self,
        name: &str,
        collection: &Collection,
        given: &Map<String, Value>,
    ) -> Result<Record, DataError> {
        let mut checked = Record::new();
        for (field_name, value) in given {
            if SYSTEM_FIELDS.contains(&field_name.as_str()) {
                return Err(DataError::Invalid(format!("`{field_name}` is kept by the backend")));
            }
            let field = collection.fields.get(field_name).ok_or_else(|| {
                DataError::Invalid(format!("`{field_name}` is not a field of {name}"))
            })?;
            let kind = stored_kind(&self.declared, field);
            let value = values::check(field, kind, value)
                .map_err(|err| DataError::Invalid(format!("`{field_name}` {err}")))?;
            if value.is_null() && field.required {
                return Err(DataError::Invalid(format!("`{field_name}` is required")));
            }
            checked.insert(field_name.clone(), value);
        }
        Ok(checked)
    }

    fn key(&self, shape: &Shape, key: &Value) -> Result<Value, DataError> {
        match values::scalar(shape.key_kind(), None, key) {
            Ok(Value::Null) | Err(_) => Err(DataError::Invalid(format!(
                "`{}` is the key of {} and is {}",
                shape.key,
                shape.name,
                if shape.key_kind() == FieldType::Uuid { "a UUID" } else { "text" }
            ))),
            Ok(key) => Ok(key),
        }
    }

    async fn insert(&mut self, name: &str, given: Map<String, Value>) -> Result<Record, DataError> {
        let (collection, shape) = self.own(name)?;
        let checked = self.checked(name, &collection, &given)?;
        let mut record = Record::new();
        for (field_name, field) in &collection.fields {
            let kind = stored_kind(&self.declared, field);
            let value = match checked.get(field_name) {
                Some(Value::Null) if !field.key => Value::Null,
                Some(value) if !value.is_null() => value.clone(),
                _ => values::default_for(field, kind)
                    .map_err(|err| DataError::Invalid(format!("`{field_name}`'s default {err}")))?,
            };
            if value.is_null() && field.key {
                return Err(DataError::Invalid(format!(
                    "`{field_name}` is the key of {name} and must be given"
                )));
            }
            if value.is_null() && field.required {
                return Err(DataError::Invalid(format!("`{field_name}` is required")));
            }
            record.insert(field_name.clone(), value);
        }
        let now = values::now();
        record.insert("_version".into(), json!(1));
        record.insert("_created_at".into(), json!(now));
        record.insert("_updated_at".into(), json!(now));
        let key = record.get(&shape.key).cloned().unwrap_or_default();
        if self.writer.lock(&shape, &key).await?.is_some() {
            return Err(DataError::Duplicate(format!(
                "{name} already has a record {}",
                describe(&key)
            )));
        }
        let every: BTreeSet<String> = collection.fields.keys().cloned().collect();
        self.check(&collection, &shape, &record, &every).await?;
        self.writer.insert(&shape, &record).await?;
        self.changed(&shape, "insert", &record);
        Ok(record)
    }

    async fn update(
        &mut self,
        name: &str,
        key: &Value,
        set: Map<String, Value>,
        version: Option<i64>,
    ) -> Result<Option<Record>, DataError> {
        let (collection, shape) = self.own(name)?;
        let key = self.key(&shape, key)?;
        let checked = self.checked(name, &collection, &set)?;
        if checked.get(&shape.key).is_some_and(|given| given != &key) {
            return Err(DataError::Invalid(format!(
                "`{}` is the key of {name} and cannot change",
                shape.key
            )));
        }
        let Some(current) = self.writer.lock(&shape, &key).await? else {
            return match version {
                Some(_) => {
                    Err(DataError::Conflict(format!("{name} {} no longer exists", describe(&key))))
                }
                None => Ok(None),
            };
        };
        let at = current.get("_version").and_then(Value::as_i64).unwrap_or_default();
        if version.is_some_and(|version| version != at) {
            return Err(DataError::Conflict(format!(
                "{name} {} is at version {at}, not {}",
                describe(&key),
                version.unwrap_or_default()
            )));
        }
        let mut record = current.clone();
        let changed: BTreeSet<String> = checked
            .iter()
            .filter(|(field, value)| current.get(*field) != Some(*value))
            .map(|(field, _)| field.clone())
            .collect();
        if changed.is_empty() {
            return Ok(Some(current));
        }
        record.extend(checked);
        record.insert("_version".into(), json!(at + 1));
        record.insert("_updated_at".into(), json!(values::now()));
        self.check(&collection, &shape, &record, &changed).await?;
        self.writer.update(&shape, &record).await?;
        self.changed(&shape, "update", &record);
        Ok(Some(record))
    }

    async fn upsert(
        &mut self,
        name: &str,
        on: &[String],
        mut given: Map<String, Value>,
    ) -> Result<(Record, bool), DataError> {
        let (collection, shape) = self.own(name)?;
        let wanted: BTreeSet<&String> = on.iter().collect();
        let keyed = on.len() == 1 && on[0] == shape.key;
        if !keyed
            && !collection
                .unique
                .iter()
                .any(|fields| fields.iter().collect::<BTreeSet<_>>() == wanted)
        {
            return Err(DataError::BadRequest(format!(
                "`on` must be {name}'s key or one of its unique constraints, not {}",
                on.join(", ")
            )));
        }
        let checked = self.checked(name, &collection, &given)?;
        let mut equal = Vec::with_capacity(on.len());
        for field in on {
            match checked.get(field) {
                Some(value) if !value.is_null() => equal.push((field.clone(), value.clone())),
                _ => {
                    return Err(DataError::Invalid(format!("`{field}` is needed to upsert on it")));
                }
            }
        }
        self.writer.guard(&shape, &equal).await?;
        let found = self.writer.find(&shape, &equal).await?;
        let Some(existing) = found.into_iter().next() else {
            return Ok((self.insert(name, given).await?, true));
        };
        let key = existing.get(&shape.key).cloned().unwrap_or_default();
        if checked.get(&shape.key).is_some_and(|given| given != &key) {
            return Err(DataError::Invalid(format!(
                "{name} {} already has these {} values",
                describe(&key),
                on.join(", ")
            )));
        }
        given.remove(&shape.key);
        let record = self.update(name, &key, given, None).await?;
        Ok((record.unwrap_or(existing), false))
    }

    /// Collections with a reference to `parent`: which field, and what deleting a parent does.
    fn referrers(&self, parent: &str) -> Vec<(Shape, String, OnDelete)> {
        let mut referrers = Vec::new();
        for (child, collection) in &self.declared.collections {
            for (field_name, field) in &collection.fields {
                if field.kind == FieldType::Ref && field.to.as_deref() == Some(parent) {
                    let on_delete = field.on_delete.unwrap_or(OnDelete::Restrict);
                    let child = shape_of(child, collection, &self.declared);
                    referrers.push((child, field_name.clone(), on_delete));
                }
            }
        }
        referrers
    }

    async fn delete(
        &mut self,
        name: &str,
        key: &Value,
        version: Option<i64>,
    ) -> Result<bool, DataError> {
        let (_, shape) = self.own(name)?;
        let key = self.key(&shape, key)?;
        let Some(current) = self.writer.lock(&shape, &key).await? else {
            return match version {
                Some(_) => {
                    Err(DataError::Conflict(format!("{name} {} no longer exists", describe(&key))))
                }
                None => Ok(false),
            };
        };
        let at = current.get("_version").and_then(Value::as_i64).unwrap_or_default();
        if version.is_some_and(|version| version != at) {
            return Err(DataError::Conflict(format!(
                "{name} {} is at version {at}, not {}",
                describe(&key),
                version.unwrap_or_default()
            )));
        }
        let mut seen = BTreeSet::from([(shape.name.clone(), describe(&key))]);
        let mut going = vec![(shape, current)];
        let mut next = 0;
        while let Some((parent, record)) = going.get(next).cloned() {
            next += 1;
            let parent_key = record.get(&parent.key).cloned().unwrap_or_default();
            for (child, field, on_delete) in self.referrers(&parent.name) {
                let equal = [(field.clone(), parent_key.clone())];
                for mut referrer in self.writer.find(&child, &equal).await? {
                    let referrer_key = referrer.get(&child.key).cloned().unwrap_or_default();
                    if !seen.insert((child.name.clone(), describe(&referrer_key))) {
                        continue;
                    }
                    match on_delete {
                        OnDelete::Restrict => {
                            return Err(DataError::Invalid(format!(
                                "{} {} is still referred to by {} {} through `{field}`",
                                parent.name,
                                describe(&parent_key),
                                child.name,
                                describe(&referrer_key)
                            )));
                        }
                        OnDelete::Cascade => going.push((child.clone(), referrer)),
                        OnDelete::Null => {
                            let at = referrer
                                .get("_version")
                                .and_then(Value::as_i64)
                                .unwrap_or_default();
                            referrer.insert(field.clone(), Value::Null);
                            referrer.insert("_version".into(), json!(at + 1));
                            referrer.insert("_updated_at".into(), json!(values::now()));
                            self.writer.update(&child, &referrer).await?;
                            self.changed(&child, "update", &referrer);
                        }
                    }
                }
            }
        }
        for (gone, record) in going {
            let key = record.get(&gone.key).cloned().unwrap_or_default();
            self.writer.delete(&gone, &key).await?;
            self.changed(&gone, "delete", &record);
        }
        Ok(true)
    }

    /// References, unique constraints and size, for the fields a write sets.
    async fn check(
        &mut self,
        collection: &Collection,
        shape: &Shape,
        record: &Record,
        changed: &BTreeSet<String>,
    ) -> Result<(), DataError> {
        for (field_name, field) in &collection.fields {
            let value = record.get(field_name).unwrap_or(&Value::Null);
            if field.kind != FieldType::Ref || value.is_null() || !changed.contains(field_name) {
                continue;
            }
            let to = field.to.clone().unwrap_or_default();
            if !self.refers(&to, value).await? {
                return Err(DataError::Invalid(format!(
                    "`{field_name}` refers to {} in {to}, which does not exist",
                    describe(value)
                )));
            }
        }
        for fields in &collection.unique {
            if !fields.iter().any(|field| changed.contains(field)) {
                continue;
            }
            let equal: Vec<(String, Value)> = fields
                .iter()
                .map(|field| (field.clone(), record.get(field).cloned().unwrap_or_default()))
                .collect();
            if equal.iter().any(|(_, value)| value.is_null()) {
                continue;
            }
            let key = record.get(&shape.key);
            let others = self.writer.find(shape, &equal).await?;
            if let Some(other) = others.iter().find(|other| other.get(&shape.key) != key) {
                return Err(DataError::Duplicate(format!(
                    "{} must be unique in {}, and {} already has these values",
                    fields.iter().map(|field| format!("`{field}`")).collect::<Vec<_>>().join(", "),
                    shape.name,
                    describe(other.get(&shape.key).unwrap_or(&Value::Null))
                )));
            }
        }
        let size = serde_json::to_vec(record).map_or(usize::MAX, |bytes| bytes.len());
        if size > MAX_RECORD_BYTES {
            return Err(DataError::Invalid(format!(
                "a record is at most {MAX_RECORD_BYTES} bytes"
            )));
        }
        Ok(())
    }

    async fn refers(&mut self, to: &str, value: &Value) -> Result<bool, DataError> {
        let unavailable =
            |err: crate::db::repositories::RepositoryError| DataError::Unavailable(err.to_string());
        let id = || value.as_str().and_then(|text| Uuid::parse_str(text).ok());
        let identity = &self.state.repos.identity;
        match to {
            "core.users" => match id() {
                Some(id) => Ok(identity.user_by_id(id).await.map_err(unavailable)?.is_some()),
                None => Ok(false),
            },
            "core.service-accounts" => match id() {
                Some(id) => {
                    Ok(identity.service_account_by_id(id).await.map_err(unavailable)?.is_some())
                }
                None => Ok(false),
            },
            "core.plugins" => {
                let known = self.state.repos.plugins.records().await.map_err(unavailable)?;
                Ok(known.iter().any(|plugin| Some(plugin.id.as_str()) == value.as_str()))
            }
            own => {
                let Some(target) = self.declared.collections.get(own).cloned() else {
                    return Ok(false);
                };
                let target = shape_of(own, &target, &self.declared);
                self.writer.exists(&target, value).await
            }
        }
    }

    fn changed(&mut self, shape: &Shape, op: &str, record: &Record) {
        let change = Change {
            op: op.to_string(),
            key: record.get(&shape.key).cloned().unwrap_or_default(),
            version: record.get("_version").and_then(Value::as_i64).unwrap_or_default(),
        };
        self.changes.push((shape.name.clone(), change));
    }
}

/// One event per collection a committed request changed, carrying keys and versions but no values.
async fn announce(state: &AppState, plugin: &str, changes: Vec<(String, Change)>) {
    let mut by_collection: BTreeMap<String, Vec<Change>> = BTreeMap::new();
    for (collection, change) in changes {
        by_collection.entry(collection).or_default().push(change);
    }
    for (collection, changes) in by_collection {
        let topic = format!("plugin.{plugin}.data.{collection}.changed");
        let Ok(topic) = Topic::new(&topic) else {
            tracing::warn!(plugin, %collection, "a collection's changes have no topic to go on");
            continue;
        };
        let payload = serde_json::to_value(Changed { collection: collection.clone(), changes })
            .unwrap_or_default();
        if let Err(err) = state.buses.events.publish(Event::new(topic, SOURCE, payload)).await {
            tracing::warn!(plugin, %collection, %err, "a data change was not announced");
        }
    }
}
