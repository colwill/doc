//! Declared plugin data (DOC-SPEC §4.3) and the data API's requests (§9.1). A manifest declares its
//! collections; the backend validates, stores and evolves them, and a plugin reads and writes them
//! only through these requests.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const MAX_COLLECTIONS: usize = 64;
pub const MAX_FIELDS: usize = 64;
pub const MAX_INDEXES: usize = 16;
pub const MAX_BATCH: usize = 100;
pub const DEFAULT_LIMIT: u32 = 50;
pub const MAX_LIMIT: u32 = 1_000;
pub const MAX_GROUPS: usize = 1_000;
pub const MAX_GROUP_BY: usize = 3;
pub const MAX_RECORD_BYTES: usize = 1 << 20;
pub const SYSTEM_FIELDS: [&str; 3] = ["_version", "_created_at", "_updated_at"];
/// The collections every plugin can read, and none can write.
pub const CORE_COLLECTIONS: [&str; 6] = [
    "core.users",
    "core.service-accounts",
    "core.plugins",
    "core.plugin-status",
    "core.plugin-permissions",
    "core.tasks",
];

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Declaration {
    pub collections: BTreeMap<String, Collection>,
}

impl Declaration {
    pub fn is_empty(&self) -> bool {
        self.collections.is_empty()
    }

    pub fn collection(mut self, name: &str, collection: Collection) -> Self {
        self.collections.insert(name.to_string(), collection);
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Collection {
    pub fields: BTreeMap<String, Field>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub indexes: Vec<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub unique: Vec<Vec<String>>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub search: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub export: Option<Export>,
    #[serde(skip_serializing_if = "is_false")]
    pub deprecated: bool,
}

impl Collection {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn field(mut self, name: &str, field: Field) -> Self {
        self.fields.insert(name.to_string(), field);
        self
    }

    pub fn index(mut self, fields: &[&str]) -> Self {
        self.indexes.push(fields.iter().map(|field| field.to_string()).collect());
        self
    }

    pub fn unique(mut self, fields: &[&str]) -> Self {
        self.unique.push(fields.iter().map(|field| field.to_string()).collect());
        self
    }

    pub fn search(mut self, fields: &[&str]) -> Self {
        self.search.extend(fields.iter().map(|field| field.to_string()));
        self
    }

    pub fn export(mut self, export: Export) -> Self {
        self.export = Some(export);
        self
    }

    pub fn deprecated(mut self) -> Self {
        self.deprecated = true;
        self
    }

    /// The key field's name, when exactly one field is the key.
    pub fn key(&self) -> Option<&str> {
        let mut keys = self.fields.iter().filter(|(_, field)| field.key);
        match (keys.next(), keys.next()) {
            (Some((name, _)), None) => Some(name),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    Text,
    Integer,
    Number,
    Decimal,
    Boolean,
    Timestamp,
    Date,
    Uuid,
    Json,
    Bytes,
    Ref,
    List,
}

impl FieldType {
    pub fn name(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Integer => "integer",
            Self::Number => "number",
            Self::Decimal => "decimal",
            Self::Boolean => "boolean",
            Self::Timestamp => "timestamp",
            Self::Date => "date",
            Self::Uuid => "uuid",
            Self::Json => "json",
            Self::Bytes => "bytes",
            Self::Ref => "ref",
            Self::List => "list",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnDelete {
    Restrict,
    Cascade,
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ListOf {
    Text,
    Integer,
    Uuid,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field {
    #[serde(rename = "type")]
    pub kind: FieldType,
    #[serde(default, skip_serializing_if = "is_false")]
    pub key: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub one_of: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_delete: Option<OnDelete>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub of: Option<ListOf>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub deprecated: bool,
}

impl Field {
    pub fn of_type(kind: FieldType) -> Self {
        Self {
            kind,
            key: false,
            required: false,
            default: None,
            description: String::new(),
            max: None,
            min: None,
            one_of: Vec::new(),
            scale: None,
            to: None,
            on_delete: None,
            of: None,
            deprecated: false,
        }
    }

    pub fn text() -> Self {
        Self::of_type(FieldType::Text)
    }

    pub fn integer() -> Self {
        Self::of_type(FieldType::Integer)
    }

    pub fn number() -> Self {
        Self::of_type(FieldType::Number)
    }

    pub fn decimal(scale: u32) -> Self {
        Self { scale: Some(scale), ..Self::of_type(FieldType::Decimal) }
    }

    pub fn boolean() -> Self {
        Self::of_type(FieldType::Boolean)
    }

    pub fn timestamp() -> Self {
        Self::of_type(FieldType::Timestamp)
    }

    pub fn date() -> Self {
        Self::of_type(FieldType::Date)
    }

    pub fn uuid() -> Self {
        Self::of_type(FieldType::Uuid)
    }

    pub fn json() -> Self {
        Self::of_type(FieldType::Json)
    }

    pub fn bytes() -> Self {
        Self::of_type(FieldType::Bytes)
    }

    /// A reference to a record of `to`: one of this plugin's collections, or `core.users`,
    /// `core.service-accounts` or `core.plugins`.
    pub fn reference(to: &str) -> Self {
        Self { to: Some(to.to_string()), ..Self::of_type(FieldType::Ref) }
    }

    pub fn list(of: ListOf) -> Self {
        Self { of: Some(of), ..Self::of_type(FieldType::List) }
    }

    pub fn key(mut self) -> Self {
        self.key = true;
        self
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn default(mut self, value: Value) -> Self {
        self.default = Some(value);
        self
    }

    pub fn max(mut self, max: f64) -> Self {
        self.max = Some(max);
        self
    }

    pub fn min(mut self, min: f64) -> Self {
        self.min = Some(min);
        self
    }

    pub fn one_of(mut self, values: &[&str]) -> Self {
        self.one_of = values.iter().map(|value| value.to_string()).collect();
        self
    }

    pub fn on_delete(mut self, on_delete: OnDelete) -> Self {
        self.on_delete = Some(on_delete);
        self
    }

    pub fn describe(mut self, description: &str) -> Self {
        self.description = description.to_string();
        self
    }

    pub fn deprecated(mut self) -> Self {
        self.deprecated = true;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Readers {
    /// `"all"`: every plugin.
    All(AllPlugins),
    Plugins(Vec<String>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AllPlugins {
    All,
}

impl Readers {
    pub fn includes(&self, plugin: &str) -> bool {
        match self {
            Self::All(_) => true,
            Self::Plugins(plugins) => plugins.iter().any(|reader| reader == plugin),
        }
    }
}

/// Who else may read a collection, and which of its fields they see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Export {
    pub read: Readers,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
}

impl Export {
    pub fn to(plugins: &[&str]) -> Self {
        Self {
            read: Readers::Plugins(plugins.iter().map(|plugin| plugin.to_string()).collect()),
            fields: None,
        }
    }

    pub fn to_all() -> Self {
        Self { read: Readers::All(AllPlugins::All), fields: None }
    }

    pub fn fields(mut self, fields: &[&str]) -> Self {
        self.fields = Some(fields.iter().map(|field| field.to_string()).collect());
        self
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    #[default]
    Asc,
    Desc,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Order {
    pub field: String,
    #[serde(default)]
    pub dir: Direction,
}

impl Order {
    pub fn asc(field: &str) -> Self {
        Self { field: field.to_string(), dir: Direction::Asc }
    }

    pub fn desc(field: &str) -> Self {
        Self { field: field.to_string(), dir: Direction::Desc }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Query {
    pub collection: String,
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub order: Vec<Order>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fields: Option<Vec<String>>,
}

impl Query {
    pub fn new(collection: &str) -> Self {
        Self { collection: collection.to_string(), ..Self::default() }
    }

    /// Conditions as DOC-SPEC §9.1 writes them, such as `json!({"team": "payments"})`.
    pub fn filter(mut self, filter: Value) -> Self {
        self.filter = Some(filter);
        self
    }

    pub fn search(mut self, text: &str) -> Self {
        self.search = Some(text.to_string());
        self
    }

    pub fn order(mut self, order: Order) -> Self {
        self.order.push(order);
        self
    }

    pub fn limit(mut self, limit: u32) -> Self {
        self.limit = Some(limit);
        self
    }

    pub fn after(mut self, cursor: Option<String>) -> Self {
        self.after = cursor;
        self
    }

    pub fn fields(mut self, fields: &[&str]) -> Self {
        self.fields = Some(fields.iter().map(|field| field.to_string()).collect());
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Bucket {
    Hour,
    Day,
    Week,
    Month,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum GroupBy {
    Field(String),
    Bucket { field: String, bucket: Bucket },
}

impl GroupBy {
    pub fn field(&self) -> &str {
        match self {
            Self::Field(field) | Self::Bucket { field, .. } => field,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Measure {
    /// Of `"*"` for every record, or of a field for those where it is set.
    Count(String),
    Sum(String),
    Min(String),
    Max(String),
    Avg(String),
}

impl Measure {
    pub fn field(&self) -> &str {
        match self {
            Self::Count(field)
            | Self::Sum(field)
            | Self::Min(field)
            | Self::Max(field)
            | Self::Avg(field) => field,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Aggregate {
    pub collection: String,
    #[serde(default, rename = "where", skip_serializing_if = "Option::is_none")]
    pub filter: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_by: Vec<GroupBy>,
    pub measures: BTreeMap<String, Measure>,
}

impl Aggregate {
    pub fn new(collection: &str) -> Self {
        Self { collection: collection.to_string(), ..Self::default() }
    }

    pub fn filter(mut self, filter: Value) -> Self {
        self.filter = Some(filter);
        self
    }

    pub fn group_by(mut self, field: &str) -> Self {
        self.group_by.push(GroupBy::Field(field.to_string()));
        self
    }

    pub fn bucket(mut self, field: &str, bucket: Bucket) -> Self {
        self.group_by.push(GroupBy::Bucket { field: field.to_string(), bucket });
        self
    }

    pub fn measure(mut self, name: &str, measure: Measure) -> Self {
        self.measures.insert(name.to_string(), measure);
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "lowercase")]
pub enum DataRequest {
    Get {
        collection: String,
        key: Value,
    },
    Query(Query),
    Aggregate(Aggregate),
    Insert {
        collection: String,
        values: Value,
    },
    Update {
        collection: String,
        key: Value,
        set: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<i64>,
    },
    Upsert {
        collection: String,
        on: Vec<String>,
        values: Value,
    },
    Delete {
        collection: String,
        key: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        version: Option<i64>,
    },
    Batch {
        writes: Vec<DataRequest>,
    },
}

impl DataRequest {
    pub fn get(collection: &str, key: impl Into<Value>) -> Self {
        Self::Get { collection: collection.to_string(), key: key.into() }
    }

    pub fn insert(collection: &str, values: Value) -> Self {
        Self::Insert { collection: collection.to_string(), values }
    }

    pub fn update(collection: &str, key: impl Into<Value>, set: Value) -> Self {
        Self::Update { collection: collection.to_string(), key: key.into(), set, version: None }
    }

    pub fn upsert(collection: &str, on: &[&str], values: Value) -> Self {
        let on = on.iter().map(|field| field.to_string()).collect();
        Self::Upsert { collection: collection.to_string(), on, values }
    }

    pub fn delete(collection: &str, key: impl Into<Value>) -> Self {
        Self::Delete { collection: collection.to_string(), key: key.into(), version: None }
    }

    /// Only applies if the record is still at `version`, for an `update` or a `delete`.
    pub fn at_version(mut self, at: i64) -> Self {
        if let Self::Update { version, .. } | Self::Delete { version, .. } = &mut self {
            *version = Some(at);
        }
        self
    }

    pub fn is_write(&self) -> bool {
        matches!(
            self,
            Self::Insert { .. }
                | Self::Update { .. }
                | Self::Upsert { .. }
                | Self::Delete { .. }
                | Self::Batch { .. }
        )
    }

    /// The collection named, or none for a batch.
    pub fn collection(&self) -> Option<&str> {
        match self {
            Self::Get { collection, .. }
            | Self::Insert { collection, .. }
            | Self::Update { collection, .. }
            | Self::Upsert { collection, .. }
            | Self::Delete { collection, .. } => Some(collection),
            Self::Query(query) => Some(&query.collection),
            Self::Aggregate(aggregate) => Some(&aggregate.collection),
            Self::Batch { .. } => None,
        }
    }
}

/// Every answer's fields in one shape; each operation fills in its own (DOC-SPEC §9.1).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DataAnswer {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<Map<String, Value>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub records: Option<Vec<Map<String, Value>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub groups: Option<Vec<Map<String, Value>>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub results: Option<Vec<DataAnswer>>,
}

/// One committed change, as `plugin.<id>.data.<collection>.changed` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Change {
    pub op: String,
    pub key: Value,
    pub version: i64,
}

/// The payload of `plugin.<id>.data.<collection>.changed`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Changed {
    pub collection: String,
    pub changes: Vec<Change>,
}
