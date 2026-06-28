//! Reading collections: `where` parsed into conditions on the fields a reader may see, sorting and
//! cursors, projections and aggregates, plus an evaluator of all of them over records in memory,
//! which the memory store and the `core.*` collections use.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use base64::Engine;
use chrono::{Datelike, Duration, TimeZone, Timelike, Utc};
use doc_plugin_protocol::data::{
    Aggregate, Bucket, DEFAULT_LIMIT, Direction, FieldType, GroupBy, ListOf, MAX_GROUP_BY,
    MAX_GROUPS, MAX_LIMIT, Measure, Query,
};
use serde_json::{Map, Number, Value, json};

use super::values;

pub type Record = Map<String, Value>;

const MAX_LIST: usize = 1_000;
const CURSOR: base64::engine::GeneralPurpose = base64::engine::general_purpose::URL_SAFE_NO_PAD;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Column {
    pub kind: FieldType,
    pub of: Option<ListOf>,
}

/// A collection as one reader sees it: its stored columns, and those the reader may name.
#[derive(Debug, Clone)]
pub struct Shape {
    pub name: String,
    pub key: String,
    pub columns: BTreeMap<String, Column>,
    pub visible: BTreeSet<String>,
    pub sortable: Vec<Vec<String>>,
    pub search: Vec<String>,
}

impl Shape {
    fn column(&self, field: &str) -> Result<Column, String> {
        match self.columns.get(field) {
            Some(column) if self.visible.contains(field) => Ok(*column),
            _ => Err(format!("{} has no field `{field}`", self.name)),
        }
    }

    pub fn key_kind(&self) -> FieldType {
        self.columns.get(&self.key).map_or(FieldType::Text, |column| column.kind)
    }

    /// Only the fields this reader may see.
    pub fn project(&self, mut record: Record, fields: Option<&[String]>) -> Record {
        record.retain(|name, _| {
            self.visible.contains(name) && fields.is_none_or(|fields| fields.contains(name))
        });
        record
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Test {
    Compare(Cmp, Value),
    In(Vec<Value>),
    NotIn(Vec<Value>),
    Prefix(String),
    Contains(Value),
    IsNull(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    All(Vec<Filter>),
    Any(Vec<Filter>),
    Field { field: String, kind: FieldType, test: Test },
}

pub fn filter(shape: &Shape, value: &Value) -> Result<Filter, String> {
    let Value::Object(conditions) = value else {
        return Err("`where` is an object of conditions".into());
    };
    let mut all = Vec::new();
    for (name, condition) in conditions {
        match (name.as_str(), condition) {
            ("any" | "all", Value::Array(items)) => {
                let parsed =
                    items.iter().map(|item| filter(shape, item)).collect::<Result<_, _>>()?;
                all.push(if name == "any" { Filter::Any(parsed) } else { Filter::All(parsed) });
            }
            _ => all.extend(field_filters(shape, name, condition)?),
        }
    }
    Ok(Filter::All(all))
}

fn field_filters(shape: &Shape, name: &str, condition: &Value) -> Result<Vec<Filter>, String> {
    let column = shape.column(name)?;
    let operators = match condition {
        Value::Object(operators) => operators.clone(),
        Value::Null => Map::from_iter([("is_null".to_string(), Value::Bool(true))]),
        other => Map::from_iter([("eq".to_string(), other.clone())]),
    };
    let mut filters = Vec::new();
    for (operator, operand) in &operators {
        let test = test(column, operator, operand).map_err(|err| format!("`{name}` {err}"))?;
        filters.push(Filter::Field { field: name.to_string(), kind: column.kind, test });
    }
    Ok(filters)
}

fn operand(column: Column, value: &Value) -> Result<Value, String> {
    if value.is_null() {
        return Err("is compared with null; use is_null".into());
    }
    values::scalar(column.kind, column.of, value)
}

fn test(column: Column, operator: &str, value: &Value) -> Result<Test, String> {
    let kind = column.kind;
    let unordered = matches!(kind, FieldType::Json | FieldType::Bytes | FieldType::List);
    let comparison = |cmp| match unordered {
        true => Err(format!("cannot be compared with {operator}")),
        false => Ok(Test::Compare(cmp, operand(column, value)?)),
    };
    let list = |value: &Value| -> Result<Vec<Value>, String> {
        let Value::Array(items) = value else { return Err(format!("{operator} takes a list")) };
        if unordered || items.len() > MAX_LIST {
            return Err(format!("{operator} takes up to {MAX_LIST} values of a comparable field"));
        }
        items.iter().map(|item| operand(column, item)).collect()
    };
    match operator {
        "eq" => comparison(Cmp::Eq),
        "ne" => comparison(Cmp::Ne),
        "lt" => comparison(Cmp::Lt),
        "lte" => comparison(Cmp::Lte),
        "gt" => comparison(Cmp::Gt),
        "gte" => comparison(Cmp::Gte),
        "in" => Ok(Test::In(list(value)?)),
        "not_in" => Ok(Test::NotIn(list(value)?)),
        "prefix" => match (kind, value) {
            (FieldType::Text, Value::String(prefix)) => Ok(Test::Prefix(prefix.clone())),
            _ => Err("takes prefix only as text on a text field".into()),
        },
        "contains" => {
            let item = match column.of {
                Some(ListOf::Integer) => FieldType::Integer,
                Some(ListOf::Uuid) => FieldType::Uuid,
                _ => FieldType::Text,
            };
            match kind {
                FieldType::List if !value.is_null() => {
                    Ok(Test::Contains(values::scalar(item, None, value)?))
                }
                _ => Err("takes contains only on a list field".into()),
            }
        }
        "is_null" => match value {
            Value::Bool(null) => Ok(Test::IsNull(*null)),
            _ => Err("takes is_null as true or false".into()),
        },
        other => Err(format!("has no operator `{other}`")),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub filter: Option<Filter>,
    pub search: Option<String>,
    /// What records are sorted by, ending with the key; empty when ordered by relevance.
    pub order: Vec<(String, FieldType, Direction)>,
    pub after: Option<Vec<Value>>,
    pub limit: usize,
    pub fields: Option<Vec<String>>,
    pub key: String,
}

impl Plan {
    pub fn by_relevance(&self) -> bool {
        self.order.is_empty()
    }

    fn spec(&self) -> String {
        match self.by_relevance() {
            true => "~relevance".into(),
            false => self
                .order
                .iter()
                .map(|(field, _, dir)| format!("{field}:{}", direction(*dir)))
                .collect::<Vec<_>>()
                .join(","),
        }
    }

    /// Where the next page starts: the last record's sort values, and its relevance if it has one.
    pub fn cursor(&self, last: &Record, relevance: Option<f64>) -> String {
        let value = |field: &str| last.get(field).cloned().unwrap_or_default();
        let values: Vec<Value> = match self.by_relevance() {
            true => vec![
                relevance.and_then(Number::from_f64).map_or(Value::Null, Value::Number),
                value(&self.key),
            ],
            false => self.order.iter().map(|(field, ..)| value(field)).collect(),
        };
        CURSOR.encode(json!([self.spec(), values]).to_string())
    }
}

fn direction(dir: Direction) -> &'static str {
    match dir {
        Direction::Asc => "asc",
        Direction::Desc => "desc",
    }
}

const SYSTEM_SORTS: [&str; 2] = ["_created_at", "_updated_at"];

pub fn plan(shape: &Shape, query: &Query) -> Result<Plan, String> {
    let filter = query.filter.as_ref().map(|value| filter(shape, value)).transpose()?;
    let search = query.search.as_deref().map(str::trim).filter(|text| !text.is_empty());
    if search.is_some() && shape.search.iter().all(|field| !shape.visible.contains(field)) {
        return Err(format!("{} has no search fields", shape.name));
    }
    let named: Vec<&str> = query.order.iter().map(|order| order.field.as_str()).collect();
    let single_system =
        named.len() == 1 && (named[0] == shape.key || SYSTEM_SORTS.contains(&named[0]));
    let indexed = shape.sortable.iter().any(|fields| {
        fields.len() >= named.len() && fields.iter().zip(&named).all(|(field, name)| field == name)
    });
    if !named.is_empty() && !single_system && !indexed {
        return Err(format!(
            "{} cannot be sorted by {}: sort by the key, _created_at, _updated_at or the leading fields of an index",
            shape.name,
            named.join(", ")
        ));
    }
    let mut order = Vec::new();
    for requested in &query.order {
        order.push((requested.field.clone(), shape.column(&requested.field)?.kind, requested.dir));
    }
    let relevance = search.is_some() && order.is_empty();
    if !relevance && !order.iter().any(|(field, ..)| field == &shape.key) {
        order.push((shape.key.clone(), shape.key_kind(), Direction::Asc));
    }
    if relevance {
        order.clear();
    }
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if limit == 0 || limit > MAX_LIMIT {
        return Err(format!("limit is 1 to {MAX_LIMIT}"));
    }
    if let Some(fields) = &query.fields {
        for field in fields {
            shape.column(field)?;
        }
    }
    let mut plan = Plan {
        filter,
        search: search.map(str::to_string),
        order,
        after: None,
        limit: limit as usize,
        fields: query.fields.clone(),
        key: shape.key.clone(),
    };
    if let Some(cursor) = &query.after {
        plan.after = Some(after(shape, &plan, cursor)?);
    }
    Ok(plan)
}

fn after(shape: &Shape, plan: &Plan, cursor: &str) -> Result<Vec<Value>, String> {
    let foreign = || "the cursor does not belong to this query".to_string();
    let decoded = CURSOR.decode(cursor).map_err(|_| foreign())?;
    let Ok(Value::Array(parts)) = serde_json::from_slice::<Value>(&decoded) else {
        return Err(foreign());
    };
    let [Value::String(spec), Value::Array(values)] = parts.as_slice() else {
        return Err(foreign());
    };
    if spec != &plan.spec() {
        return Err(foreign());
    }
    let mut kinds: Vec<FieldType> = plan.order.iter().map(|(_, kind, _)| *kind).collect();
    if plan.by_relevance() {
        kinds = vec![FieldType::Number, shape.key_kind()];
    }
    if values.len() != kinds.len() {
        return Err(foreign());
    }
    kinds
        .iter()
        .zip(values)
        .map(|(kind, value)| values::scalar(*kind, None, value).map_err(|_| foreign()))
        .collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Grouping {
    pub field: String,
    pub kind: FieldType,
    pub bucket: Option<Bucket>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Measured {
    pub name: String,
    pub measure: Measure,
    /// The measured field's kind, or `None` for `count` of `*`.
    pub kind: Option<FieldType>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AggregatePlan {
    pub filter: Option<Filter>,
    pub groups: Vec<Grouping>,
    pub measures: Vec<Measured>,
}

pub fn aggregate_plan(shape: &Shape, aggregate: &Aggregate) -> Result<AggregatePlan, String> {
    let filter = aggregate.filter.as_ref().map(|value| filter(shape, value)).transpose()?;
    if aggregate.group_by.len() > MAX_GROUP_BY {
        return Err(format!("group_by takes up to {MAX_GROUP_BY} fields"));
    }
    let mut groups = Vec::new();
    for group in &aggregate.group_by {
        let column = shape.column(group.field())?;
        if matches!(column.kind, FieldType::Json | FieldType::Bytes | FieldType::List) {
            return Err(format!("`{}` cannot be grouped by", group.field()));
        }
        let bucket = match group {
            GroupBy::Bucket { bucket, .. } if column.kind == FieldType::Timestamp => Some(*bucket),
            GroupBy::Bucket { field, .. } => {
                return Err(format!("`{field}` is not a timestamp to bucket"));
            }
            GroupBy::Field(_) => None,
        };
        groups.push(Grouping { field: group.field().to_string(), kind: column.kind, bucket });
    }
    if aggregate.measures.is_empty() {
        return Err("an aggregate needs at least one measure".into());
    }
    let mut measures = Vec::new();
    for (name, measure) in &aggregate.measures {
        if !super::declaration::valid_field_name(name)
            || groups.iter().any(|group| &group.field == name)
        {
            return Err(format!("`{name}` cannot name a measure"));
        }
        let kind = match (measure, measure.field()) {
            (Measure::Count(_), "*") => None,
            (_, field) => Some(shape.column(field)?.kind),
        };
        let numeric =
            matches!(kind, Some(FieldType::Integer | FieldType::Number | FieldType::Decimal));
        let ordered = numeric
            || matches!(kind, Some(FieldType::Timestamp | FieldType::Date | FieldType::Text));
        let fits = match measure {
            Measure::Count(_) => true,
            Measure::Sum(_) | Measure::Avg(_) => numeric,
            Measure::Min(_) | Measure::Max(_) => ordered,
        };
        if !fits {
            return Err(format!("`{name}` measures a field it cannot be taken of"));
        }
        measures.push(Measured { name: name.clone(), measure: measure.clone(), kind });
    }
    Ok(AggregatePlan { filter, groups, measures })
}

/// The start of the bucket `value` falls in, as `YYYY-MM-DD`, or an hour as a timestamp.
pub fn bucket(value: &Value, bucket: Bucket) -> Value {
    let Some(at) = value.as_str().and_then(values::parse_timestamp) else { return Value::Null };
    let day = at.date_naive();
    let start = match bucket {
        Bucket::Hour => {
            let hour = Utc.from_utc_datetime(&day.and_hms_opt(at.hour(), 0, 0).unwrap_or_default());
            return Value::String(values::readable(hour));
        }
        Bucket::Day => day,
        Bucket::Week => day - Duration::days(i64::from(day.weekday().num_days_from_monday())),
        Bucket::Month => day.with_day(1).unwrap_or(day),
    };
    Value::String(start.format("%Y-%m-%d").to_string())
}

pub fn matches(filter: &Filter, record: &Record) -> bool {
    match filter {
        Filter::All(filters) => filters.iter().all(|filter| matches(filter, record)),
        Filter::Any(filters) => filters.iter().any(|filter| matches(filter, record)),
        Filter::Field { field, kind, test } => {
            let value = record.get(field).unwrap_or(&Value::Null);
            passes(*kind, value, test)
        }
    }
}

fn passes(kind: FieldType, value: &Value, test: &Test) -> bool {
    let compare = |other: &Value| values::compare(kind, value, other);
    match test {
        Test::IsNull(null) => value.is_null() == *null,
        Test::Compare(Cmp::Ne, other) => compare(other) != Ordering::Equal,
        Test::NotIn(others) => others.iter().all(|other| compare(other) != Ordering::Equal),
        _ if value.is_null() => false,
        Test::Compare(cmp, other) => {
            let order = compare(other);
            match cmp {
                Cmp::Eq => order == Ordering::Equal,
                Cmp::Lt => order == Ordering::Less,
                Cmp::Lte => order != Ordering::Greater,
                Cmp::Gt => order == Ordering::Greater,
                Cmp::Gte => order != Ordering::Less,
                Cmp::Ne => unreachable!("handled above"),
            }
        }
        Test::In(others) => others.iter().any(|other| compare(other) == Ordering::Equal),
        Test::Prefix(prefix) => {
            value.as_str().is_some_and(|text| text.starts_with(prefix.as_str()))
        }
        Test::Contains(item) => value.as_array().is_some_and(|items| items.contains(item)),
    }
}

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// How well a record matches every word searched for, or `None` if it misses one.
pub fn relevance(shape: &Shape, record: &Record, search: &str) -> Option<f64> {
    let text: Vec<String> = shape
        .search
        .iter()
        .filter_map(|field| record.get(field).and_then(Value::as_str))
        .flat_map(words)
        .collect();
    let mut score = 0.0;
    for term in words(search) {
        let hits = text.iter().filter(|word| word.starts_with(&term)).count();
        if hits == 0 {
            return None;
        }
        score += hits as f64 / text.len().max(1) as f64;
    }
    Some(score)
}

fn sort_order(plan: &Plan, a: &Record, b: &Record) -> Ordering {
    for (field, kind, dir) in &plan.order {
        let null = Value::Null;
        let order =
            values::compare(*kind, a.get(field).unwrap_or(&null), b.get(field).unwrap_or(&null));
        let order = if *dir == Direction::Desc { order.reverse() } else { order };
        if order != Ordering::Equal {
            return order;
        }
    }
    Ordering::Equal
}

/// One page of `records`, and the cursor for the next when there is more.
pub fn page(shape: &Shape, plan: &Plan, records: Vec<Record>) -> (Vec<Record>, Option<String>) {
    let mut found: Vec<(Record, Option<f64>)> = records
        .into_iter()
        .filter(|record| plan.filter.as_ref().is_none_or(|filter| matches(filter, record)))
        .filter_map(|record| match &plan.search {
            Some(search) => relevance(shape, &record, search).map(|rank| (record, Some(rank))),
            None => Some((record, None)),
        })
        .collect();
    let key_kind = shape.key_kind();
    let by_relevance = |(a, ra): &(Record, Option<f64>), (b, rb): &(Record, Option<f64>)| {
        rb.partial_cmp(ra).unwrap_or(Ordering::Equal).then_with(|| {
            let null = Value::Null;
            values::compare(
                key_kind,
                a.get(&shape.key).unwrap_or(&null),
                b.get(&shape.key).unwrap_or(&null),
            )
        })
    };
    match plan.by_relevance() {
        true => found.sort_by(by_relevance),
        false => found.sort_by(|(a, _), (b, _)| sort_order(plan, a, b)),
    }
    if let Some(after) = &plan.after {
        found.retain(|(record, rank)| match plan.by_relevance() {
            true => {
                let cursor =
                    (Map::from_iter([(shape.key.clone(), after[1].clone())]), after[0].as_f64());
                by_relevance(&(record.clone(), *rank), &cursor) == Ordering::Greater
            }
            false => {
                let cursor: Record = plan
                    .order
                    .iter()
                    .zip(after)
                    .map(|((field, ..), value)| (field.clone(), value.clone()))
                    .collect();
                sort_order(plan, record, &cursor) == Ordering::Greater
            }
        });
    }
    let more = found.len() > plan.limit;
    found.truncate(plan.limit);
    let next = match (more, found.last()) {
        (true, Some((last, rank))) => Some(plan.cursor(last, *rank)),
        _ => None,
    };
    let records = found
        .into_iter()
        .map(|(record, _)| shape.project(record, plan.fields.as_deref()))
        .collect();
    (records, next)
}

#[derive(Default)]
struct Tally {
    count: i64,
    integer: i128,
    number: f64,
    decimals: Vec<String>,
    least: Option<Value>,
    most: Option<Value>,
}

pub fn aggregate(plan: &AggregatePlan, records: &[Record]) -> Result<Vec<Record>, String> {
    let mut groups: Vec<(Vec<Value>, Vec<Tally>)> = Vec::new();
    for record in records
        .iter()
        .filter(|record| plan.filter.as_ref().is_none_or(|filter| matches(filter, record)))
    {
        let key: Vec<Value> = plan
            .groups
            .iter()
            .map(|group| {
                let value = record.get(&group.field).cloned().unwrap_or_default();
                group.bucket.map_or(value.clone(), |size| bucket(&value, size))
            })
            .collect();
        let at = match groups.iter().position(|(existing, _)| existing == &key) {
            Some(at) => at,
            None => {
                groups.push((key, plan.measures.iter().map(|_| Tally::default()).collect()));
                groups.len() - 1
            }
        };
        for (measured, tally) in plan.measures.iter().zip(groups[at].1.iter_mut()) {
            let value = match measured.measure.field() {
                "*" => Value::Bool(true),
                field => record.get(field).cloned().unwrap_or_default(),
            };
            if value.is_null() {
                continue;
            }
            let kind = measured.kind.unwrap_or(FieldType::Text);
            tally.count += 1;
            tally.integer += i128::from(value.as_i64().unwrap_or_default());
            tally.number += value.as_f64().unwrap_or_default();
            if kind == FieldType::Decimal {
                tally.decimals.push(value.as_str().unwrap_or("0").to_string());
            }
            if tally
                .least
                .as_ref()
                .is_none_or(|least| values::compare(kind, &value, least) == Ordering::Less)
            {
                tally.least = Some(value.clone());
            }
            if tally
                .most
                .as_ref()
                .is_none_or(|most| values::compare(kind, &value, most) == Ordering::Greater)
            {
                tally.most = Some(value);
            }
        }
    }
    groups.sort_by(|(a, _), (b, _)| {
        plan.groups
            .iter()
            .zip(a.iter().zip(b))
            .map(|(group, (x, y))| {
                values::compare(
                    if group.bucket.is_some() { FieldType::Text } else { group.kind },
                    x,
                    y,
                )
            })
            .find(|order| *order != Ordering::Equal)
            .unwrap_or(Ordering::Equal)
    });
    groups.truncate(MAX_GROUPS);
    let mut answer = Vec::with_capacity(groups.len());
    for (key, tallies) in groups {
        let mut row: Record =
            plan.groups.iter().map(|group| group.field.clone()).zip(key).collect();
        for (measured, tally) in plan.measures.iter().zip(tallies) {
            row.insert(measured.name.clone(), result(measured, tally)?);
        }
        answer.push(row);
    }
    Ok(answer)
}

fn result(measured: &Measured, tally: Tally) -> Result<Value, String> {
    if let Measure::Count(_) = measured.measure {
        return Ok(Value::from(tally.count));
    }
    if tally.count == 0 {
        return Ok(Value::Null);
    }
    let kind = measured.kind.unwrap_or(FieldType::Number);
    let decimal = || -> Result<(i128, usize), String> {
        let scale = tally
            .decimals
            .iter()
            .map(|d| d.split_once('.').map_or(0, |(_, f)| f.len()))
            .max()
            .unwrap_or(0);
        let mut sum: i128 = 0;
        for value in &tally.decimals {
            let scaled = values::scaled(value, scale).ok_or("a decimal is too large to add up")?;
            sum = sum.checked_add(scaled).ok_or("a sum of decimals is too large")?;
        }
        Ok((sum, scale))
    };
    let float = |value: f64| Number::from_f64(value).map_or(Value::Null, Value::Number);
    Ok(match (&measured.measure, kind) {
        (Measure::Min(_), _) => tally.least.unwrap_or_default(),
        (Measure::Max(_), _) => tally.most.unwrap_or_default(),
        (Measure::Sum(_), FieldType::Integer) => i64::try_from(tally.integer)
            .map(Value::from)
            .map_err(|_| "a sum is too large".to_string())?,
        (Measure::Sum(_), FieldType::Decimal) => {
            let (sum, scale) = decimal()?;
            Value::String(values::unscaled(sum, scale))
        }
        (Measure::Sum(_), _) => float(tally.number),
        (Measure::Avg(_), FieldType::Decimal) => {
            let (sum, scale) = decimal()?;
            let widened = sum.checked_mul(10_000).ok_or("a sum of decimals is too large")?;
            Value::String(values::unscaled(widened / i128::from(tally.count), scale + 4))
        }
        (Measure::Avg(_), FieldType::Integer) => float(tally.integer as f64 / tally.count as f64),
        (Measure::Avg(_), _) => float(tally.number / tally.count as f64),
        (Measure::Count(_), _) => unreachable!("counted above"),
    })
}
