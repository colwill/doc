//! A field's JSON value, checked against its declaration and put in the one form storage keeps:
//! timestamps in UTC to the microsecond, decimals at their scale, UUIDs and base64 canonical. The
//! forms are chosen so that comparing two normalized strings orders them as their values.

use std::cmp::Ordering;

use base64::Engine;
use chrono::{DateTime, NaiveDate, SecondsFormat, Utc};
use doc_plugin_protocol::data::{Field, FieldType, ListOf};
use serde_json::{Number, Value};
use uuid::Uuid;

const BASE64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

pub fn now() -> String {
    timestamp(Utc::now())
}

pub fn timestamp(at: DateTime<Utc>) -> String {
    let micros = at.timestamp_micros();
    let at = DateTime::from_timestamp_micros(micros).unwrap_or(at);
    at.format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string()
}

pub fn parse_timestamp(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).ok().map(|at| at.with_timezone(&Utc))
}

/// The same instant as `timestamp` gives, in the shorter form people read.
pub fn readable(at: DateTime<Utc>) -> String {
    at.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// What `value` is as `kind`, where a reference is checked as the kind of key it points at.
pub fn check(field: &Field, kind: FieldType, value: &Value) -> Result<Value, String> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let normalized = scalar(kind, field.of, value)?;
    match kind {
        FieldType::Text => {
            let text = normalized.as_str().unwrap_or_default();
            if field.max.is_some_and(|max| text.chars().count() as f64 > max) {
                return Err(format!("is longer than {} characters", field.max.unwrap_or_default()));
            }
            if !field.one_of.is_empty() && !field.one_of.iter().any(|allowed| allowed == text) {
                return Err(format!("is not one of {}", field.one_of.join(", ")));
            }
        }
        FieldType::Integer | FieldType::Number => {
            let number = normalized.as_f64().unwrap_or_default();
            if let Some(min) = field.min.filter(|min| number < *min) {
                return Err(format!("is below the minimum of {min}"));
            }
            if let Some(max) = field.max.filter(|max| number > *max) {
                return Err(format!("is above the maximum of {max}"));
            }
        }
        FieldType::Decimal => {
            let text = normalized.as_str().unwrap_or_default();
            return decimal(text, field.scale).map(Value::String);
        }
        FieldType::Bytes => {
            let length =
                BASE64.decode(normalized.as_str().unwrap_or_default()).map_or(0, |b| b.len());
            if field.max.is_some_and(|max| length as f64 > max) {
                return Err(format!("is longer than {} bytes", field.max.unwrap_or_default()));
            }
        }
        _ => {}
    }
    Ok(normalized)
}

/// The type check alone, for values in filters and cursors as much as in records.
pub fn scalar(kind: FieldType, of: Option<ListOf>, value: &Value) -> Result<Value, String> {
    let wrong = || Err(format!("is not {}", article(kind)));
    match (kind, value) {
        (_, Value::Null) => Ok(Value::Null),
        (FieldType::Text, Value::String(_)) => Ok(value.clone()),
        (FieldType::Integer, Value::Number(number)) => match number.as_i64() {
            Some(integer) => Ok(Value::from(integer)),
            None => wrong(),
        },
        (FieldType::Number, Value::Number(number)) => match number.as_f64() {
            Some(float) => Ok(Number::from_f64(float).map_or(Value::Null, Value::Number)),
            None => wrong(),
        },
        (FieldType::Decimal, Value::String(text)) => decimal(text, None).map(Value::String),
        (FieldType::Decimal, Value::Number(number)) if number.is_i64() || number.is_u64() => {
            Ok(Value::String(number.to_string()))
        }
        (FieldType::Boolean, Value::Bool(_)) => Ok(value.clone()),
        (FieldType::Timestamp, Value::String(text)) => match parse_timestamp(text) {
            Some(at) => Ok(Value::String(timestamp(at))),
            None => Err("is not an RFC 3339 timestamp".into()),
        },
        (FieldType::Date, Value::String(text)) => {
            match NaiveDate::parse_from_str(text, "%Y-%m-%d") {
                Ok(date) => Ok(Value::String(date.format("%Y-%m-%d").to_string())),
                Err(_) => Err("is not a date as YYYY-MM-DD".into()),
            }
        }
        (FieldType::Uuid, Value::String(text)) => match Uuid::parse_str(text) {
            Ok(uuid) => Ok(Value::String(uuid.hyphenated().to_string())),
            Err(_) => wrong(),
        },
        (FieldType::Json, _) => Ok(value.clone()),
        (FieldType::Bytes, Value::String(text)) => match BASE64.decode(text) {
            Ok(bytes) => Ok(Value::String(BASE64.encode(bytes))),
            Err(_) => Err("is not base64".into()),
        },
        (FieldType::List, Value::Array(items)) => {
            let item = match of.unwrap_or(ListOf::Text) {
                ListOf::Text => FieldType::Text,
                ListOf::Integer => FieldType::Integer,
                ListOf::Uuid => FieldType::Uuid,
            };
            let mut normalized = Vec::with_capacity(items.len());
            for entry in items {
                if entry.is_null() {
                    return Err("holds a null".into());
                }
                normalized.push(
                    scalar(item, None, entry).map_err(|err| format!("holds a value that {err}"))?,
                );
            }
            Ok(Value::Array(normalized))
        }
        _ => wrong(),
    }
}

fn article(kind: FieldType) -> String {
    match kind {
        FieldType::Integer => "an integer".into(),
        FieldType::Uuid => "a UUID".into(),
        FieldType::List => "a list".into(),
        FieldType::Json => "JSON".into(),
        FieldType::Bytes => "base64".into(),
        other => format!("a {}", other.name()),
    }
}

/// `-?digits(.digits)?`, without leading zeros, at exactly `scale` places when there is one.
fn decimal(text: &str, scale: Option<u32>) -> Result<String, String> {
    let (negative, digits) = match text.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, text),
    };
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let numeric = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    if whole.is_empty()
        || !numeric(whole)
        || !numeric(fraction)
        || (digits.contains('.') && fraction.is_empty())
    {
        return Err("is not a decimal such as \"12.50\"".into());
    }
    let mut fraction = fraction.to_string();
    if let Some(scale) = scale.map(|scale| scale as usize) {
        if fraction.len() > scale {
            return Err(format!("has more than {scale} decimal places"));
        }
        fraction.extend(std::iter::repeat_n('0', scale - fraction.len()));
    }
    let whole = whole.trim_start_matches('0');
    let whole = if whole.is_empty() { "0" } else { whole };
    let zero = whole == "0" && fraction.chars().all(|c| c == '0');
    let sign = if negative && !zero { "-" } else { "" };
    match fraction.is_empty() {
        true => Ok(format!("{sign}{whole}")),
        false => Ok(format!("{sign}{whole}.{fraction}")),
    }
}

/// Orders two normalized decimals by value.
pub fn compare_decimals(a: &str, b: &str) -> Ordering {
    let parts = |text: &str| {
        let (negative, digits) = text.strip_prefix('-').map_or((false, text), |rest| (true, rest));
        let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
        (negative, whole.to_string(), fraction.trim_end_matches('0').to_string())
    };
    let (a_negative, a_whole, a_fraction) = parts(a);
    let (b_negative, b_whole, b_fraction) = parts(b);
    let magnitude = a_whole
        .len()
        .cmp(&b_whole.len())
        .then_with(|| a_whole.cmp(&b_whole))
        .then_with(|| a_fraction.cmp(&b_fraction));
    match (a_negative, b_negative) {
        (false, false) => magnitude,
        (true, true) => magnitude.reverse(),
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
    }
}

/// A total order on normalized values of one kind, with null before everything else.
pub fn compare(kind: FieldType, a: &Value, b: &Value) -> Ordering {
    match (a, b) {
        (Value::Null, Value::Null) => Ordering::Equal,
        (Value::Null, _) => Ordering::Less,
        (_, Value::Null) => Ordering::Greater,
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(x), Some(y)) if kind != FieldType::Number => x.cmp(&y),
            _ => x.as_f64().partial_cmp(&y.as_f64()).unwrap_or(Ordering::Equal),
        },
        (Value::String(x), Value::String(y)) if kind == FieldType::Decimal => {
            compare_decimals(x, y)
        }
        (Value::String(x), Value::String(y)) => x.cmp(y),
        (Value::Bool(x), Value::Bool(y)) => x.cmp(y),
        (x, y) => x.to_string().cmp(&y.to_string()),
    }
}

/// A decimal as an integer count of `10^-scale`, for adding up; `None` when it does not fit.
pub fn scaled(text: &str, scale: usize) -> Option<i128> {
    let (negative, digits) = text.strip_prefix('-').map_or((false, text), |rest| (true, rest));
    let (whole, fraction) = digits.split_once('.').unwrap_or((digits, ""));
    let mut fraction = fraction.to_string();
    if fraction.len() > scale {
        return None;
    }
    fraction.extend(std::iter::repeat_n('0', scale - fraction.len()));
    let magnitude: i128 = format!("{whole}{fraction}").parse().ok()?;
    Some(if negative { -magnitude } else { magnitude })
}

pub fn unscaled(value: i128, scale: usize) -> String {
    let negative = value < 0;
    let digits = value.unsigned_abs().to_string();
    let digits = format!("{digits:0>width$}", width = scale + 1);
    let (whole, fraction) = digits.split_at(digits.len() - scale);
    let sign = if negative { "-" } else { "" };
    match scale {
        0 => format!("{sign}{whole}"),
        _ => format!("{sign}{whole}.{fraction}"),
    }
}

/// The value a field starts with when an insert leaves it out.
pub fn default_for(field: &Field, kind: FieldType) -> Result<Value, String> {
    match (&field.default, field.kind) {
        (Some(Value::String(special)), FieldType::Timestamp) if special == "now" => {
            Ok(Value::String(now()))
        }
        (Some(Value::String(special)), FieldType::Uuid) if special == "uuid" => {
            Ok(Value::String(Uuid::now_v7().to_string()))
        }
        (Some(value), _) => check(field, kind, value),
        (None, FieldType::Uuid) if field.key => Ok(Value::String(Uuid::now_v7().to_string())),
        (None, _) => Ok(Value::Null),
    }
}
