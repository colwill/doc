//! Collections in Postgres: the `doc_plugins` database, a schema per plugin and a table per
//! collection, made and changed from declarations. Every name is quoted, and every value is a
//! parameter of the field's own type, so nothing a plugin sends is ever read as SQL.

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::time::Duration;

use anyhow::Context;
use async_trait::async_trait;
use base64::Engine;
use chrono::{DateTime, NaiveDate, Utc};
use doc_plugin_protocol::data::{
    Bucket, Collection, Declaration, Direction, Field, FieldType, ListOf, MAX_GROUPS, Measure,
};
use serde_json::{Number, Value};
use sha2::{Digest, Sha256};
use sqlx::postgres::{PgArguments, PgConnectOptions, PgPool, PgPoolOptions, PgRow};
use sqlx::{Arguments, AssertSqlSafe, Postgres, Row, Transaction};
use tokio::sync::OnceCell;
use uuid::Uuid;

use super::declaration::stored_kind;
use super::query::{AggregatePlan, Cmp, Column, Filter, Plan, Record, Shape, Test};
use super::store::{DataError, DataStore, Declarations, Writer};
use super::{DATA_TIMEOUT, shape_of, values};
use crate::db::postgres::quote_ident as quote;

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;
const SEARCH: &str = "_search";
const WEIGHTS: [&str; 4] = ["A", "B", "C", "D"];
/// Added to the rank of a record whose search fields hold the searched text as it was typed.
const LITERAL_BONUS: f32 = 0.1;

/// Text for a `LIKE` pattern, with its wildcards and escape character taken literally.
fn like(text: &str) -> String {
    text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_")
}

pub struct PostgresData {
    pool: PgPool,
    ready: OnceCell<()>,
    cached: parking_lot::RwLock<BTreeMap<String, Declarations>>,
}

fn failed(err: sqlx::Error) -> DataError {
    let sqlx::Error::Database(db) = err else { return DataError::Unavailable(err.to_string()) };
    let message = db.message().to_string();
    match db.code().as_deref() {
        Some("23505") => {
            DataError::Duplicate(format!("a unique constraint would be broken ({message})"))
        }
        Some("23503") => {
            DataError::Invalid(format!("a reference would point at nothing ({message})"))
        }
        Some("23502" | "22P02" | "22003" | "22007" | "22008") => DataError::Invalid(message),
        Some("57014") => DataError::Timeout,
        _ => DataError::Unavailable(message),
    }
}

fn schema(plugin: &str) -> String {
    format!("plugin_{}", plugin.replace('-', "_"))
}

fn table(plugin: &str, collection: &str) -> String {
    format!("{}.{}", quote(&schema(plugin)), quote(collection))
}

/// A short name for an index or constraint, unique within the plugin's schema.
fn named(prefix: &str, parts: &[&str]) -> String {
    let digest = Sha256::digest(parts.join("|").as_bytes());
    let hex: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    format!("{prefix}_{hex}")
}

impl PostgresData {
    /// Connects when first used, so the backend starts even while this database is unreachable.
    pub fn lazy(url: &str, max_connections: u32) -> anyhow::Result<Self> {
        let options = PgConnectOptions::from_str(url).context("parsing the plugin data URL")?;
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(5))
            .connect_lazy_with(options);
        Ok(Self { pool, ready: OnceCell::new(), cached: parking_lot::RwLock::new(BTreeMap::new()) })
    }

    /// The table recording each plugin's declarations, made the first time the database answers.
    async fn ready(&self) -> Result<(), DataError> {
        let bootstrap = || async {
            for statement in [
                "CREATE SCHEMA IF NOT EXISTS doc_data",
                "CREATE TABLE IF NOT EXISTS doc_data.declarations (\
                   plugin text PRIMARY KEY, serving jsonb, storage jsonb NOT NULL, \
                   updated_at timestamptz NOT NULL DEFAULT now())",
            ] {
                sqlx::query(statement).execute(&self.pool).await.map_err(failed)?;
            }
            Ok::<(), DataError>(())
        };
        self.ready.get_or_try_init(bootstrap).await.map(|_| ())
    }

    async fn stored(&self, plugin: &str) -> Result<Declarations, DataError> {
        let row: Option<(Option<Value>, Value)> =
            sqlx::query_as("SELECT serving, storage FROM doc_data.declarations WHERE plugin = $1")
                .bind(plugin)
                .fetch_optional(&self.pool)
                .await
                .map_err(failed)?;
        let Some((serving, storage)) = row else { return Ok(Declarations::default()) };
        let unreadable = |err: serde_json::Error| {
            DataError::Unavailable(format!("a stored declaration is unreadable: {err}"))
        };
        Ok(Declarations {
            serving: serving.map(serde_json::from_value).transpose().map_err(unreadable)?,
            storage: serde_json::from_value(storage).map_err(unreadable)?,
        })
    }
}

fn sql_type(column: Column) -> &'static str {
    match (column.kind, column.of) {
        (FieldType::Text | FieldType::Ref, _) => "text COLLATE \"C\"",
        (FieldType::Integer, _) => "bigint",
        (FieldType::Number, _) => "double precision",
        (FieldType::Decimal, _) => "numeric",
        (FieldType::Boolean, _) => "boolean",
        (FieldType::Timestamp, _) => "timestamptz",
        (FieldType::Date, _) => "date",
        (FieldType::Uuid, _) => "uuid",
        (FieldType::Json, _) => "jsonb",
        (FieldType::Bytes, _) => "bytea",
        (FieldType::List, Some(ListOf::Integer)) => "bigint[]",
        (FieldType::List, Some(ListOf::Uuid)) => "uuid[]",
        (FieldType::List, _) => "text[] COLLATE \"C\"",
    }
}

fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

/// A declared default as SQL, from a value `values::check` has already normalized.
fn default_sql(field: &Field, column: Column) -> Result<Option<String>, DataError> {
    let Some(default) = &field.default else { return Ok(None) };
    match (column.kind, default) {
        (FieldType::Timestamp, Value::String(now)) if now == "now" => {
            return Ok(Some("now()".into()));
        }
        (FieldType::Uuid, Value::String(uuid)) if uuid == "uuid" => {
            return Ok(Some("gen_random_uuid()".into()));
        }
        _ => {}
    }
    let value = values::check(field, column.kind, default).map_err(DataError::Invalid)?;
    let text = |value: &Value| value.as_str().map_or_else(|| value.to_string(), str::to_string);
    Ok(Some(match (column.kind, &value) {
        (_, Value::Null) => return Ok(None),
        (FieldType::Integer | FieldType::Number | FieldType::Boolean, value) => value.to_string(),
        (FieldType::Json, value) => format!("{}::jsonb", literal(&value.to_string())),
        (FieldType::Bytes, value) => format!("decode({}, 'base64')", literal(&text(value))),
        (FieldType::List, Value::Array(items)) => {
            let items: Vec<String> = items.iter().map(|item| literal(&text(item))).collect();
            format!(
                "ARRAY[{}]::{}",
                items.join(", "),
                sql_type(column).split(' ').next().unwrap_or("text[]")
            )
        }
        (_, value) => format!(
            "{}::{}",
            literal(&text(value)),
            sql_type(column).split(' ').next().unwrap_or("text")
        ),
    }))
}

fn column_sql(name: &str, field: &Field, column: Column) -> Result<String, DataError> {
    let mut sql = format!("{} {}", quote(name), sql_type(column));
    if field.required || field.key {
        sql.push_str(" NOT NULL");
    }
    if let Some(default) = default_sql(field, column)? {
        sql.push_str(&format!(" DEFAULT {default}"));
    }
    Ok(sql)
}

fn search_sql(collection: &Collection) -> String {
    let parts: Vec<String> = collection
        .search
        .iter()
        .enumerate()
        .map(|(at, field)| {
            let weight = WEIGHTS[at.min(WEIGHTS.len() - 1)];
            format!("setweight(to_tsvector('english', coalesce({}, '')), '{weight}')", quote(field))
        })
        .collect();
    parts.join(" || ")
}

/// The statements taking a plugin's schema from `from` to `to`, drops first and constraints last.
fn changes(plugin: &str, from: &Declaration, to: &Declaration) -> Result<Vec<String>, DataError> {
    let mut drops = Vec::new();
    let mut builds = Vec::new();
    let mut links = Vec::new();
    for name in from.collections.keys().filter(|name| !to.collections.contains_key(*name)) {
        drops.push(format!("DROP TABLE IF EXISTS {} CASCADE", table(plugin, name)));
    }
    let kept_table = |name: &String, collection: &Collection| {
        from.collections.get(name).filter(|before| {
            before.key() == collection.key()
                && before.key().is_some_and(|key| {
                    stored_kind(from, &before.fields[key])
                        == stored_kind(to, &collection.fields[key])
                })
        })
    };
    let fresh: BTreeSet<&String> = to
        .collections
        .iter()
        .filter(|(name, collection)| kept_table(name, collection).is_none())
        .map(|(name, _)| name)
        .collect();
    for (name, collection) in &to.collections {
        let at = table(plugin, name);
        let shape = shape_of(name, collection, to);
        let before = kept_table(name, collection);
        if before.is_none() && from.collections.contains_key(name) {
            drops.push(format!("DROP TABLE IF EXISTS {at} CASCADE"));
        }
        let mut added: BTreeSet<&String> = BTreeSet::new();
        match before {
            None => {
                let mut columns = Vec::new();
                for (field_name, field) in &collection.fields {
                    columns.push(column_sql(field_name, field, shape.columns[field_name])?);
                    added.insert(field_name);
                }
                columns.push("\"_version\" bigint NOT NULL DEFAULT 1".into());
                columns.push("\"_created_at\" timestamptz NOT NULL DEFAULT now()".into());
                columns.push("\"_updated_at\" timestamptz NOT NULL DEFAULT now()".into());
                columns.push(format!("PRIMARY KEY ({})", quote(&shape.key)));
                builds.push(format!("CREATE TABLE {at} ({})", columns.join(", ")));
            }
            Some(before) => {
                let old = shape_of(name, before, from);
                for field_name in
                    before.fields.keys().filter(|field| !collection.fields.contains_key(*field))
                {
                    drops.push(format!(
                        "ALTER TABLE {at} DROP COLUMN IF EXISTS {} CASCADE",
                        quote(field_name)
                    ));
                }
                for (field_name, field) in &collection.fields {
                    let column = shape.columns[field_name];
                    let Some(was) = before.fields.get(field_name) else {
                        builds.push(format!(
                            "ALTER TABLE {at} ADD COLUMN {}",
                            column_sql(field_name, field, column)?
                        ));
                        added.insert(field_name);
                        continue;
                    };
                    if old.columns[field_name] != column {
                        drops.push(format!(
                            "ALTER TABLE {at} DROP COLUMN IF EXISTS {} CASCADE",
                            quote(field_name)
                        ));
                        builds.push(format!(
                            "ALTER TABLE {at} ADD COLUMN {}",
                            column_sql(field_name, field, column)?
                        ));
                        added.insert(field_name);
                        continue;
                    }
                    let quoted = quote(field_name);
                    if was.default != field.default {
                        match default_sql(field, column)? {
                            Some(default) => builds.push(format!(
                                "ALTER TABLE {at} ALTER COLUMN {quoted} SET DEFAULT {default}"
                            )),
                            None => builds.push(format!(
                                "ALTER TABLE {at} ALTER COLUMN {quoted} DROP DEFAULT"
                            )),
                        }
                    }
                    let (was_required, required) =
                        (was.required || was.key, field.required || field.key);
                    if was_required && !required {
                        builds
                            .push(format!("ALTER TABLE {at} ALTER COLUMN {quoted} DROP NOT NULL"));
                    }
                    if required && !was_required {
                        if let Some(default) = default_sql(field, column)? {
                            builds.push(format!(
                                "UPDATE {at} SET {quoted} = {default} WHERE {quoted} IS NULL"
                            ));
                        }
                        builds.push(format!("ALTER TABLE {at} ALTER COLUMN {quoted} SET NOT NULL"));
                    }
                }
            }
        }
        let kept = |lists: &Vec<Vec<String>>, fields: &Vec<String>| {
            lists.contains(fields) && fields.iter().all(|field| !added.contains(field))
        };
        let (old_indexes, old_unique) = match before {
            Some(before) => (before.indexes.clone(), before.unique.clone()),
            None => (Vec::new(), Vec::new()),
        };
        for (prefix, wanted, had) in
            [("i", &collection.indexes, &old_indexes), ("u", &collection.unique, &old_unique)]
        {
            for fields in had.iter().filter(|fields| !kept(wanted, fields)) {
                let index = named(prefix, &[name, &fields.join(",")]);
                drops.push(format!(
                    "DROP INDEX IF EXISTS {}.{}",
                    quote(&schema(plugin)),
                    quote(&index)
                ));
            }
            for fields in wanted.iter().filter(|fields| !kept(had, fields)) {
                let index = named(prefix, &[name, &fields.join(",")]);
                let columns: Vec<String> = fields.iter().map(|field| quote(field)).collect();
                let unique = if prefix == "u" { "UNIQUE " } else { "" };
                links.push(format!(
                    "CREATE {unique}INDEX IF NOT EXISTS {} ON {at} ({})",
                    quote(&index),
                    columns.join(", ")
                ));
            }
        }
        let searched_before = before.map(|before| before.search.clone()).unwrap_or_default();
        let dropped_searched = searched_before
            .iter()
            .any(|field| !collection.fields.contains_key(field) || added.contains(field));
        if before.is_none() || searched_before != collection.search || dropped_searched {
            builds.push(format!("ALTER TABLE {at} DROP COLUMN IF EXISTS {SEARCH}"));
            if !collection.search.is_empty() {
                builds.push(format!(
                    "ALTER TABLE {at} ADD COLUMN {SEARCH} tsvector GENERATED ALWAYS AS ({}) STORED",
                    search_sql(collection)
                ));
                let index = named("s", &[name]);
                links.push(format!(
                    "CREATE INDEX IF NOT EXISTS {} ON {at} USING gin ({SEARCH})",
                    quote(&index)
                ));
            }
        }
        // A field that stopped pointing at another collection, or points somewhere else now,
        // loses the constraint it had: the column stays, and what it holds is the plugin's to
        // rewrite.
        for (field_name, was) in before.iter().flat_map(|before| &before.fields) {
            let still = collection
                .fields
                .get(field_name)
                .is_some_and(|field| field.kind == FieldType::Ref && field.to == was.to);
            if was.kind == FieldType::Ref && !still {
                let constraint = named("f", &[name, field_name]);
                drops.push(format!(
                    "ALTER TABLE {at} DROP CONSTRAINT IF EXISTS {}",
                    quote(&constraint)
                ));
            }
        }
        for (field_name, field) in &collection.fields {
            let (FieldType::Ref, Some(to_name)) = (field.kind, &field.to) else { continue };
            let Some(target) = to.collections.get(to_name) else { continue };
            let refers_before = before
                .and_then(|before| before.fields.get(field_name))
                .is_some_and(|was| was.to == field.to);
            if refers_before && !added.contains(field_name) && !fresh.contains(to_name) {
                continue;
            }
            let constraint = named("f", &[name, field_name]);
            links
                .push(format!("ALTER TABLE {at} DROP CONSTRAINT IF EXISTS {}", quote(&constraint)));
            links.push(format!(
                "ALTER TABLE {at} ADD CONSTRAINT {} FOREIGN KEY ({}) REFERENCES {} ({}) DEFERRABLE INITIALLY DEFERRED",
                quote(&constraint),
                quote(field_name),
                table(plugin, to_name),
                quote(target.key().unwrap_or_default())
            ));
        }
    }
    Ok(drops.into_iter().chain(builds).chain(links).collect())
}

/// The value for `column`, bound as its own type; the placeholder casts it where Postgres needs to.
fn bind(args: &mut PgArguments, column: Column, value: &Value) -> Result<String, DataError> {
    let text = value.as_str().map(str::to_string);
    let bad = |err: sqlx::error::BoxDynError| DataError::Invalid(err.to_string());
    let mut cast = "";
    match (column.kind, column.of) {
        (FieldType::Text | FieldType::Ref, _) => args.add(text).map_err(bad)?,
        (FieldType::Integer, _) => args.add(value.as_i64()).map_err(bad)?,
        (FieldType::Number, _) => args.add(value.as_f64()).map_err(bad)?,
        (FieldType::Decimal, _) => {
            cast = "::numeric";
            args.add(text).map_err(bad)?
        }
        (FieldType::Boolean, _) => args.add(value.as_bool()).map_err(bad)?,
        (FieldType::Timestamp, _) => {
            args.add(text.as_deref().and_then(values::parse_timestamp)).map_err(bad)?
        }
        (FieldType::Date, _) => args
            .add(text.as_deref().and_then(|date| NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()))
            .map_err(bad)?,
        (FieldType::Uuid, _) => {
            args.add(text.as_deref().and_then(|id| Uuid::parse_str(id).ok())).map_err(bad)?
        }
        (FieldType::Json, _) => {
            let json = (!value.is_null()).then(|| sqlx::types::Json(value.clone()));
            args.add(json).map_err(bad)?
        }
        (FieldType::Bytes, _) => {
            args.add(text.as_deref().and_then(|bytes| BASE64.decode(bytes).ok())).map_err(bad)?
        }
        (FieldType::List, of) => {
            let items = value.as_array();
            match of {
                Some(ListOf::Integer) => args
                    .add(
                        items.map(|items| {
                            items.iter().filter_map(Value::as_i64).collect::<Vec<_>>()
                        }),
                    )
                    .map_err(bad)?,
                Some(ListOf::Uuid) => args
                    .add(items.map(|items| {
                        items
                            .iter()
                            .filter_map(|item| Uuid::parse_str(item.as_str()?).ok())
                            .collect::<Vec<_>>()
                    }))
                    .map_err(bad)?,
                _ => args
                    .add(items.map(|items| {
                        items
                            .iter()
                            .filter_map(|item| Some(item.as_str()?.to_string()))
                            .collect::<Vec<_>>()
                    }))
                    .map_err(bad)?,
            }
        }
    }
    Ok(format!("${}{cast}", args.len()))
}

/// A list for `= ANY(...)`, bound as an array of the column's type.
fn bind_list(args: &mut PgArguments, column: Column, items: &[Value]) -> Result<String, DataError> {
    let bad = |err: sqlx::error::BoxDynError| DataError::Invalid(err.to_string());
    let texts =
        || items.iter().filter_map(|item| item.as_str().map(str::to_string)).collect::<Vec<_>>();
    let mut cast = "";
    match column.kind {
        FieldType::Integer => {
            args.add(items.iter().filter_map(Value::as_i64).collect::<Vec<_>>()).map_err(bad)?
        }
        FieldType::Number => {
            args.add(items.iter().filter_map(Value::as_f64).collect::<Vec<_>>()).map_err(bad)?
        }
        FieldType::Boolean => {
            args.add(items.iter().filter_map(Value::as_bool).collect::<Vec<_>>()).map_err(bad)?
        }
        FieldType::Decimal => {
            cast = "::numeric[]";
            args.add(texts()).map_err(bad)?
        }
        FieldType::Timestamp => args
            .add(
                items
                    .iter()
                    .filter_map(|item| values::parse_timestamp(item.as_str()?))
                    .collect::<Vec<_>>(),
            )
            .map_err(bad)?,
        FieldType::Date => args
            .add(
                items
                    .iter()
                    .filter_map(|item| NaiveDate::parse_from_str(item.as_str()?, "%Y-%m-%d").ok())
                    .collect::<Vec<_>>(),
            )
            .map_err(bad)?,
        FieldType::Uuid => args
            .add(
                items
                    .iter()
                    .filter_map(|item| Uuid::parse_str(item.as_str()?).ok())
                    .collect::<Vec<_>>(),
            )
            .map_err(bad)?,
        _ => args.add(texts()).map_err(bad)?,
    }
    Ok(format!("${}{cast}", args.len()))
}

fn select_list(shape: &Shape) -> String {
    shape
        .columns
        .iter()
        .map(|(name, column)| match column.kind {
            FieldType::Decimal => format!("{0}::text AS {0}", quote(name)),
            _ => quote(name),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn decode(row: &PgRow, shape: &Shape) -> Result<Record, DataError> {
    let mut record = Record::new();
    for (index, (name, column)) in shape.columns.iter().enumerate() {
        record.insert(name.clone(), cell(row, index, *column).map_err(failed)?);
    }
    Ok(record)
}

fn cell(row: &PgRow, index: usize, column: Column) -> Result<Value, sqlx::Error> {
    let string = |value: Option<String>| value.map_or(Value::Null, Value::String);
    Ok(match (column.kind, column.of) {
        (FieldType::Text | FieldType::Ref | FieldType::Decimal, _) => string(row.try_get(index)?),
        (FieldType::Integer, _) => {
            row.try_get::<Option<i64>, _>(index)?.map_or(Value::Null, Value::from)
        }
        (FieldType::Number, _) => row
            .try_get::<Option<f64>, _>(index)?
            .and_then(Number::from_f64)
            .map_or(Value::Null, Value::Number),
        (FieldType::Boolean, _) => {
            row.try_get::<Option<bool>, _>(index)?.map_or(Value::Null, Value::Bool)
        }
        (FieldType::Timestamp, _) => {
            string(row.try_get::<Option<DateTime<Utc>>, _>(index)?.map(values::timestamp))
        }
        (FieldType::Date, _) => string(
            row.try_get::<Option<NaiveDate>, _>(index)?
                .map(|date| date.format("%Y-%m-%d").to_string()),
        ),
        (FieldType::Uuid, _) => {
            string(row.try_get::<Option<Uuid>, _>(index)?.map(|id| id.to_string()))
        }
        (FieldType::Json, _) => row
            .try_get::<Option<sqlx::types::Json<Value>>, _>(index)?
            .map_or(Value::Null, |json| json.0),
        (FieldType::Bytes, _) => {
            string(row.try_get::<Option<Vec<u8>>, _>(index)?.map(|bytes| BASE64.encode(bytes)))
        }
        (FieldType::List, Some(ListOf::Integer)) => {
            row.try_get::<Option<Vec<i64>>, _>(index)?.map_or(Value::Null, Value::from)
        }
        (FieldType::List, Some(ListOf::Uuid)) => {
            row.try_get::<Option<Vec<Uuid>>, _>(index)?.map_or(Value::Null, |items| {
                items.iter().map(|id| Value::String(id.to_string())).collect()
            })
        }
        (FieldType::List, _) => row
            .try_get::<Option<Vec<String>>, _>(index)?
            .map_or(Value::Null, |items| items.into_iter().map(Value::String).collect()),
    })
}

/// A filter as a SQL condition, with its values bound into `args`.
fn condition(shape: &Shape, filter: &Filter, args: &mut PgArguments) -> Result<String, DataError> {
    Ok(match filter {
        Filter::All(filters) | Filter::Any(filters) if filters.is_empty() => {
            if matches!(filter, Filter::All(_)) { "TRUE".into() } else { "FALSE".into() }
        }
        Filter::All(filters) => {
            let parts: Result<Vec<_>, _> =
                filters.iter().map(|filter| condition(shape, filter, args)).collect();
            format!("({})", parts?.join(" AND "))
        }
        Filter::Any(filters) => {
            let parts: Result<Vec<_>, _> =
                filters.iter().map(|filter| condition(shape, filter, args)).collect();
            format!("({})", parts?.join(" OR "))
        }
        Filter::Field { field, test, .. } => {
            let column = shape
                .columns
                .get(field)
                .copied()
                .ok_or_else(|| DataError::BadQuery(format!("no field `{field}`")))?;
            let name = quote(field);
            match test {
                Test::IsNull(true) => format!("{name} IS NULL"),
                Test::IsNull(false) => format!("{name} IS NOT NULL"),
                Test::Compare(cmp, value) => {
                    let at = bind(args, column, value)?;
                    match cmp {
                        Cmp::Eq => format!("{name} = {at}"),
                        Cmp::Ne => format!("{name} IS DISTINCT FROM {at}"),
                        Cmp::Lt => format!("{name} < {at}"),
                        Cmp::Lte => format!("{name} <= {at}"),
                        Cmp::Gt => format!("{name} > {at}"),
                        Cmp::Gte => format!("{name} >= {at}"),
                    }
                }
                Test::In(items) if items.is_empty() => "FALSE".into(),
                Test::In(items) => format!("{name} = ANY({})", bind_list(args, column, items)?),
                Test::NotIn(items) if items.is_empty() => "TRUE".into(),
                Test::NotIn(items) => {
                    format!(
                        "({name} IS NULL OR NOT ({name} = ANY({})))",
                        bind_list(args, column, items)?
                    )
                }
                Test::Prefix(prefix) => {
                    let pattern =
                        prefix.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_");
                    let text = Column { kind: FieldType::Text, of: None };
                    format!(
                        "{name} LIKE {} ESCAPE '\\'",
                        bind(args, text, &Value::String(format!("{pattern}%")))?
                    )
                }
                Test::Contains(item) => {
                    let kind = match column.of {
                        Some(ListOf::Integer) => FieldType::Integer,
                        Some(ListOf::Uuid) => FieldType::Uuid,
                        _ => FieldType::Text,
                    };
                    format!("{} = ANY({name})", bind(args, Column { kind, of: None }, item)?)
                }
            }
        }
    })
}

/// Records sorting after the cursor's: nulls come first going up, and last coming down.
fn keyset(
    shape: &Shape,
    plan: &Plan,
    after: &[Value],
    args: &mut PgArguments,
) -> Result<String, DataError> {
    let mut alternatives = Vec::new();
    for (at, (field, _, dir)) in plan.order.iter().enumerate() {
        let mut parts = Vec::new();
        for (earlier, _, _) in &plan.order[..at] {
            let value = &after
                [plan.order.iter().position(|(name, ..)| name == earlier).unwrap_or_default()];
            let column = shape.columns[earlier];
            parts.push(match value.is_null() {
                true => format!("{} IS NULL", quote(earlier)),
                false => format!("{} = {}", quote(earlier), bind(args, column, value)?),
            });
        }
        let value = &after[at];
        let name = quote(field);
        let column = shape.columns[field];
        parts.push(match (dir, value.is_null()) {
            (Direction::Asc, true) => format!("{name} IS NOT NULL"),
            (Direction::Asc, false) => format!("{name} > {}", bind(args, column, value)?),
            (Direction::Desc, true) => "FALSE".into(),
            (Direction::Desc, false) => {
                format!("({name} < {} OR {name} IS NULL)", bind(args, column, value)?)
            }
        });
        alternatives.push(format!("({})", parts.join(" AND ")));
    }
    Ok(format!("({})", alternatives.join(" OR ")))
}

fn order_sql(plan: &Plan) -> String {
    plan.order
        .iter()
        .map(|(field, _, dir)| match dir {
            Direction::Asc => format!("{} ASC NULLS FIRST", quote(field)),
            Direction::Desc => format!("{} DESC NULLS LAST", quote(field)),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn bucket_sql(field: &str, bucket: Bucket) -> String {
    let unit = match bucket {
        Bucket::Hour => "hour",
        Bucket::Day => "day",
        Bucket::Week => "week",
        Bucket::Month => "month",
    };
    match bucket {
        Bucket::Hour => format!(
            "to_char(date_trunc('hour', {} AT TIME ZONE 'UTC'), 'YYYY-MM-DD\"T\"HH24:00:00\"Z\"')",
            quote(field)
        ),
        _ => format!(
            "to_char(date_trunc('{unit}', {} AT TIME ZONE 'UTC'), 'YYYY-MM-DD')",
            quote(field)
        ),
    }
}

#[async_trait]
impl DataStore for PostgresData {
    async fn declarations(&self, plugin: &str) -> Result<Declarations, DataError> {
        if let Some(known) = self.cached.read().get(plugin) {
            return Ok(known.clone());
        }
        self.ready().await?;
        let stored = self.stored(plugin).await?;
        self.cached.write().insert(plugin.to_string(), stored.clone());
        Ok(stored)
    }

    async fn apply(
        &self,
        plugin: &str,
        to: &Declaration,
        serving: Option<&Declaration>,
    ) -> Result<(), DataError> {
        self.ready().await?;
        let mut tx = self.pool.begin().await.map_err(failed)?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("doc_data:{plugin}"))
            .execute(&mut *tx)
            .await
            .map_err(failed)?;
        let row: Option<(Option<Value>, Value)> = sqlx::query_as(
            "SELECT serving, storage FROM doc_data.declarations WHERE plugin = $1 FOR UPDATE",
        )
        .bind(plugin)
        .fetch_optional(&mut *tx)
        .await
        .map_err(failed)?;
        let (was_serving, from) = match row {
            Some((serving, storage)) => {
                (serving, serde_json::from_value(storage).unwrap_or_default())
            }
            None => (None, Declaration::default()),
        };
        let create = format!("CREATE SCHEMA IF NOT EXISTS {}", quote(&schema(plugin)));
        sqlx::query(AssertSqlSafe(create)).execute(&mut *tx).await.map_err(failed)?;
        for statement in changes(plugin, &from, to)? {
            tracing::debug!(plugin, %statement, "changing plugin storage");
            sqlx::query(AssertSqlSafe(statement.clone())).execute(&mut *tx).await.map_err(
                |err| DataError::Invalid(format!("{} (while running `{statement}`)", failed(err))),
            )?;
        }
        let serving = match serving {
            Some(serving) => Some(serde_json::to_value(serving).unwrap_or_default()),
            None => was_serving,
        };
        sqlx::query(
            "INSERT INTO doc_data.declarations (plugin, serving, storage) VALUES ($1, $2, $3) \
             ON CONFLICT (plugin) DO UPDATE SET serving = EXCLUDED.serving, storage = EXCLUDED.storage, updated_at = now()",
        )
        .bind(plugin)
        .bind(serving.clone())
        .bind(serde_json::to_value(to).unwrap_or_default())
        .execute(&mut *tx)
        .await
        .map_err(failed)?;
        tx.commit().await.map_err(failed)?;
        let recorded = Declarations {
            serving: serving.and_then(|serving| serde_json::from_value(serving).ok()),
            storage: to.clone(),
        };
        self.cached.write().insert(plugin.to_string(), recorded);
        Ok(())
    }

    async fn get(
        &self,
        plugin: &str,
        shape: &Shape,
        key: &Value,
    ) -> Result<Option<Record>, DataError> {
        let mut args = PgArguments::default();
        let at = bind(&mut args, shape.columns[&shape.key], key)?;
        let sql = format!(
            "SELECT {} FROM {} WHERE {} = {at}",
            select_list(shape),
            table(plugin, &shape.name),
            quote(&shape.key)
        );
        let row = sqlx::query_with(AssertSqlSafe(sql), args)
            .fetch_optional(&self.pool)
            .await
            .map_err(failed)?;
        row.map(|row| decode(&row, shape)).transpose()
    }

    async fn query(
        &self,
        plugin: &str,
        shape: &Shape,
        plan: &Plan,
    ) -> Result<(Vec<Record>, Option<String>), DataError> {
        let mut args = PgArguments::default();
        let mut wheres = Vec::new();
        if let Some(filter) = &plan.filter {
            wheres.push(condition(shape, filter, &mut args)?);
        }
        let mut select = select_list(shape);
        let mut rank = String::new();
        if let Some(search) = &plan.search {
            let text = Column { kind: FieldType::Text, of: None };
            let asked = bind(&mut args, text, &Value::String(search.clone()))?;
            // English full text drops stop words, so "how" alone asks for nothing, and matches
            // whole words only, so a word half typed misses. A record whose search fields hold
            // the text as it was typed matches too, and ranks a little higher for it.
            let pattern = bind(&mut args, text, &Value::String(format!("%{}%", like(search))))?;
            let literal: Vec<String> = shape
                .search
                .iter()
                .map(|field| format!("coalesce({}, '') ILIKE {pattern}", quote(field)))
                .collect();
            let literal = format!("({})", literal.join(" OR "));
            let words = format!("{SEARCH} @@ websearch_to_tsquery('english', {asked})");
            wheres.push(format!("({words} OR {literal})"));
            rank = format!(
                "(ts_rank({SEARCH}, websearch_to_tsquery('english', {asked})) \
                 + CASE WHEN {literal} THEN {LITERAL_BONUS}::real ELSE 0::real END)"
            );
            select.push_str(&format!(", {rank} AS \"_rank\""));
        }
        let order = match plan.by_relevance() {
            true => format!("\"_rank\" DESC, {} ASC", quote(&shape.key)),
            false => order_sql(plan),
        };
        if let Some(after) = &plan.after {
            match plan.by_relevance() {
                true => {
                    let score = Column { kind: FieldType::Number, of: None };
                    let (at_rank, at_key) = (
                        bind(&mut args, score, &after[0])?,
                        bind(&mut args, shape.columns[&shape.key], &after[1])?,
                    );
                    wheres.push(format!(
                        "({rank} < {at_rank}::real OR ({rank} = {at_rank}::real AND {} > {at_key}))",
                        quote(&shape.key)
                    ));
                }
                false => wheres.push(keyset(shape, plan, after, &mut args)?),
            }
        }
        let wheres = if wheres.is_empty() { "TRUE".to_string() } else { wheres.join(" AND ") };
        let sql = format!(
            "SELECT {select} FROM {} WHERE {wheres} ORDER BY {order} LIMIT {}",
            table(plugin, &shape.name),
            plan.limit + 1
        );
        let mut tx = self.pool.begin().await.map_err(failed)?;
        sqlx::query(AssertSqlSafe(format!(
            "SET LOCAL statement_timeout = {}",
            DATA_TIMEOUT.as_millis()
        )))
        .execute(&mut *tx)
        .await
        .map_err(failed)?;
        let rows =
            sqlx::query_with(AssertSqlSafe(sql), args).fetch_all(&mut *tx).await.map_err(failed)?;
        tx.commit().await.map_err(failed)?;
        let mut found = Vec::with_capacity(rows.len());
        for row in &rows {
            let relevance = match plan.search.is_some() {
                true => row.try_get::<f32, _>("_rank").ok().map(f64::from),
                false => None,
            };
            found.push((decode(row, shape)?, relevance));
        }
        let more = found.len() > plan.limit;
        found.truncate(plan.limit);
        let next = match (more, found.last()) {
            (true, Some((last, relevance))) => Some(plan.cursor(last, *relevance)),
            _ => None,
        };
        Ok((found.into_iter().map(|(record, _)| record).collect(), next))
    }

    async fn aggregate(
        &self,
        plugin: &str,
        shape: &Shape,
        plan: &AggregatePlan,
    ) -> Result<Vec<Record>, DataError> {
        let mut args = PgArguments::default();
        let wheres = match &plan.filter {
            Some(filter) => condition(shape, filter, &mut args)?,
            None => "TRUE".into(),
        };
        let mut select = Vec::new();
        let mut groups = Vec::new();
        for (at, group) in plan.groups.iter().enumerate() {
            let expression = match group.bucket {
                Some(bucket) => bucket_sql(&group.field, bucket),
                None => match group.kind {
                    FieldType::Decimal => format!("{}::text", quote(&group.field)),
                    _ => quote(&group.field),
                },
            };
            select.push(format!("{expression} AS \"g{at}\""));
            groups.push(format!("\"g{at}\""));
        }
        for (at, measured) in plan.measures.iter().enumerate() {
            let field = measured.measure.field();
            let target = if field == "*" { "*".to_string() } else { quote(field) };
            let decimal = measured.kind == Some(FieldType::Decimal);
            let expression = match &measured.measure {
                Measure::Count(_) => format!("count({target})"),
                Measure::Sum(_) if decimal => format!("sum({target})::text"),
                Measure::Sum(_) if measured.kind == Some(FieldType::Integer) => {
                    format!("sum({target})::bigint")
                }
                Measure::Sum(_) => format!("sum({target})::double precision"),
                Measure::Avg(_) if decimal => format!("avg({target})::text"),
                Measure::Avg(_) => format!("avg({target})::double precision"),
                Measure::Min(_) if decimal => format!("min({target})::text"),
                Measure::Max(_) if decimal => format!("max({target})::text"),
                Measure::Min(_) => format!("min({target})"),
                Measure::Max(_) => format!("max({target})"),
            };
            select.push(format!("{expression} AS \"m{at}\""));
        }
        let grouping = match groups.is_empty() {
            true => String::new(),
            false => format!(" GROUP BY {0} ORDER BY {0}", groups.join(", ")),
        };
        let sql = format!(
            "SELECT {} FROM {} WHERE {wheres}{grouping} LIMIT {MAX_GROUPS}",
            select.join(", "),
            table(plugin, &shape.name)
        );
        let mut tx = self.pool.begin().await.map_err(failed)?;
        sqlx::query(AssertSqlSafe(format!(
            "SET LOCAL statement_timeout = {}",
            DATA_TIMEOUT.as_millis()
        )))
        .execute(&mut *tx)
        .await
        .map_err(failed)?;
        let rows = sqlx::query_with(AssertSqlSafe(sql), args).fetch_all(&mut *tx).await.map_err(
            |err| match failed(err) {
                DataError::Invalid(detail) => DataError::BadQuery(detail),
                other => other,
            },
        )?;
        tx.commit().await.map_err(failed)?;
        let mut answer = Vec::with_capacity(rows.len());
        for row in &rows {
            let mut record = Record::new();
            for (at, group) in plan.groups.iter().enumerate() {
                let column = match (group.bucket, group.kind) {
                    (Some(_), _) | (None, FieldType::Decimal) => {
                        Column { kind: FieldType::Text, of: None }
                    }
                    (None, kind) => shape
                        .columns
                        .get(&group.field)
                        .copied()
                        .unwrap_or(Column { kind, of: None }),
                };
                record.insert(group.field.clone(), cell(row, at, column).map_err(failed)?);
            }
            for (at, measured) in plan.measures.iter().enumerate() {
                let index = plan.groups.len() + at;
                let kind = match (&measured.measure, measured.kind) {
                    (Measure::Count(_), _) => FieldType::Integer,
                    (_, Some(FieldType::Decimal)) => FieldType::Text,
                    (Measure::Avg(_), _) => FieldType::Number,
                    (Measure::Sum(_), Some(FieldType::Integer)) => FieldType::Integer,
                    (Measure::Sum(_), _) => FieldType::Number,
                    (_, kind) => kind.unwrap_or(FieldType::Text),
                };
                record.insert(
                    measured.name.clone(),
                    cell(row, index, Column { kind, of: None }).map_err(failed)?,
                );
            }
            answer.push(record);
        }
        Ok(answer)
    }

    async fn begin(&self, plugin: &str) -> Result<Box<dyn Writer>, DataError> {
        let mut tx = self.pool.begin().await.map_err(failed)?;
        sqlx::query(AssertSqlSafe(format!(
            "SET LOCAL statement_timeout = {}",
            DATA_TIMEOUT.as_millis()
        )))
        .execute(&mut *tx)
        .await
        .map_err(failed)?;
        Ok(Box::new(PostgresWriter { plugin: plugin.to_string(), tx }))
    }
}

struct PostgresWriter {
    plugin: String,
    tx: Transaction<'static, Postgres>,
}

impl PostgresWriter {
    async fn rows(
        &mut self,
        sql: String,
        args: PgArguments,
        shape: &Shape,
    ) -> Result<Vec<Record>, DataError> {
        let rows = sqlx::query_with(AssertSqlSafe(sql), args)
            .fetch_all(&mut *self.tx)
            .await
            .map_err(failed)?;
        rows.iter().map(|row| decode(row, shape)).collect()
    }

    async fn run(&mut self, sql: String, args: PgArguments) -> Result<(), DataError> {
        sqlx::query_with(AssertSqlSafe(sql), args).execute(&mut *self.tx).await.map_err(failed)?;
        Ok(())
    }
}

#[async_trait]
impl Writer for PostgresWriter {
    async fn lock(&mut self, shape: &Shape, key: &Value) -> Result<Option<Record>, DataError> {
        let mut args = PgArguments::default();
        let at = bind(&mut args, shape.columns[&shape.key], key)?;
        let sql = format!(
            "SELECT {} FROM {} WHERE {} = {at} FOR UPDATE",
            select_list(shape),
            table(&self.plugin, &shape.name),
            quote(&shape.key)
        );
        Ok(self.rows(sql, args, shape).await?.into_iter().next())
    }

    async fn exists(&mut self, shape: &Shape, key: &Value) -> Result<bool, DataError> {
        let mut args = PgArguments::default();
        let at = bind(&mut args, shape.columns[&shape.key], key)?;
        let sql = format!(
            "SELECT 1 FROM {} WHERE {} = {at} FOR KEY SHARE",
            table(&self.plugin, &shape.name),
            quote(&shape.key)
        );
        let row = sqlx::query_with(AssertSqlSafe(sql), args)
            .fetch_optional(&mut *self.tx)
            .await
            .map_err(failed)?;
        Ok(row.is_some())
    }

    async fn find(
        &mut self,
        shape: &Shape,
        equal: &[(String, Value)],
    ) -> Result<Vec<Record>, DataError> {
        let mut args = PgArguments::default();
        let mut wheres = Vec::new();
        for (field, value) in equal {
            let column = shape.columns[field];
            wheres.push(format!("{} = {}", quote(field), bind(&mut args, column, value)?));
        }
        let sql = format!(
            "SELECT {} FROM {} WHERE {} FOR UPDATE",
            select_list(shape),
            table(&self.plugin, &shape.name),
            wheres.join(" AND ")
        );
        self.rows(sql, args, shape).await
    }

    async fn guard(&mut self, shape: &Shape, equal: &[(String, Value)]) -> Result<(), DataError> {
        let held = format!(
            "{}:{}:{}",
            self.plugin,
            shape.name,
            serde_json::to_string(equal).unwrap_or_default()
        );
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(held)
            .execute(&mut *self.tx)
            .await
            .map_err(failed)?;
        Ok(())
    }

    async fn insert(&mut self, shape: &Shape, record: &Record) -> Result<(), DataError> {
        let mut args = PgArguments::default();
        let mut names = Vec::new();
        let mut placeholders = Vec::new();
        for (name, column) in &shape.columns {
            names.push(quote(name));
            placeholders.push(bind(&mut args, *column, record.get(name).unwrap_or(&Value::Null))?);
        }
        let sql = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            table(&self.plugin, &shape.name),
            names.join(", "),
            placeholders.join(", ")
        );
        self.run(sql, args).await
    }

    async fn update(&mut self, shape: &Shape, record: &Record) -> Result<(), DataError> {
        let mut args = PgArguments::default();
        let mut sets = Vec::new();
        for (name, column) in shape.columns.iter().filter(|(name, _)| **name != shape.key) {
            sets.push(format!(
                "{} = {}",
                quote(name),
                bind(&mut args, *column, record.get(name).unwrap_or(&Value::Null))?
            ));
        }
        let key = bind(
            &mut args,
            shape.columns[&shape.key],
            record.get(&shape.key).unwrap_or(&Value::Null),
        )?;
        let sql = format!(
            "UPDATE {} SET {} WHERE {} = {key}",
            table(&self.plugin, &shape.name),
            sets.join(", "),
            quote(&shape.key)
        );
        self.run(sql, args).await
    }

    async fn delete(&mut self, shape: &Shape, key: &Value) -> Result<(), DataError> {
        let mut args = PgArguments::default();
        let at = bind(&mut args, shape.columns[&shape.key], key)?;
        let sql = format!(
            "DELETE FROM {} WHERE {} = {at}",
            table(&self.plugin, &shape.name),
            quote(&shape.key)
        );
        self.run(sql, args).await
    }

    async fn commit(self: Box<Self>) -> Result<(), DataError> {
        self.tx.commit().await.map_err(failed)
    }
}

#[cfg(test)]
mod tests {
    use doc_plugin_protocol::data::{Collection, Declaration, Field};

    use super::{changes, like};

    #[test]
    fn like_patterns_take_wildcards_literally() {
        assert_eq!(like("how"), "how");
        assert_eq!(like("50%_off\\now"), "50\\%\\_off\\\\now");
    }

    /// A field that stops pointing at another collection keeps its column and what is in it, but
    /// not the constraint, so the plugin can rewrite the values to something kept elsewhere (T69).
    #[test]
    fn a_reference_that_becomes_a_plain_field_loses_its_constraint() {
        let with = |owner: Field| {
            Declaration::default().collection(
                "resources",
                Collection::new().field("id", Field::uuid().key()).field("owner", owner),
            )
        };
        let (before, after) = (with(Field::reference("resources")), with(Field::uuid()));
        let statements = changes("resources", &before, &after).expect("the changes");
        assert!(
            statements.iter().any(|statement| statement.contains("DROP CONSTRAINT IF EXISTS")),
            "{statements:?}"
        );
        assert!(
            !statements.iter().any(|statement| statement.contains("DROP COLUMN")),
            "the column and what it holds stay: {statements:?}"
        );
        assert!(
            !statements.iter().any(|statement| statement.contains("ADD CONSTRAINT")),
            "{statements:?}"
        );
    }
}
