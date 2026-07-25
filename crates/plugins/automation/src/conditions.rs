//! Conditions on what a trigger brought, and `{{ path }}` templates filled from it. Both read the
//! same context: the trigger's input, with `resource` and `automation` beside it.

use serde_json::{Value, json};

use crate::model::{Condition, Op};

/// The value at a dotted path, such as `payload.team` or `payload.tags.0`.
pub fn lookup<'a>(context: &'a Value, path: &str) -> Option<&'a Value> {
    path.trim().split('.').try_fold(context, |value, part| match value {
        Value::Array(items) => items.get(part.parse::<usize>().ok()?),
        Value::Object(map) => map.get(part),
        _ => None,
    })
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// `text` with each `{{ path }}` replaced by what the context holds there, or nothing.
pub fn render(template: &str, context: &Value) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        let Some(end) = rest[start + 2..].find("}}") else { break };
        out.push_str(&rest[..start]);
        let path = &rest[start + 2..start + 2 + end];
        out.push_str(&lookup(context, path).map(text).unwrap_or_default());
        rest = &rest[start + 2 + end + 2..];
    }
    out.push_str(rest);
    out
}

/// A JSON template: strings are rendered, and a string that is only `{{ path }}` becomes the value.
pub fn render_value(template: &Value, context: &Value) -> Value {
    match template {
        Value::String(text) => {
            let trimmed = text.trim();
            let whole = trimmed.strip_prefix("{{").and_then(|inner| inner.strip_suffix("}}"));
            match whole.filter(|inner| !inner.contains("{{") && !inner.contains("}}")) {
                Some(path) => lookup(context, path).cloned().unwrap_or(Value::Null),
                None => Value::String(render(text, context)),
            }
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| render_value(item, context)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter().map(|(key, value)| (key.clone(), render_value(value, context))).collect(),
        ),
        other => other.clone(),
    }
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn equal(found: &Value, wanted: &Value) -> bool {
    match (found, wanted) {
        (Value::String(a), Value::String(b)) => a == b,
        (Value::Number(_), _) | (_, Value::Number(_)) => {
            number(found).zip(number(wanted)).is_some_and(|(a, b)| (a - b).abs() < f64::EPSILON)
        }
        _ => found == wanted || text(found) == text(wanted),
    }
}

/// Whether the value at the condition's field passes it; its own value may be a template.
pub fn passes(condition: &Condition, context: &Value) -> bool {
    let found = lookup(context, &condition.field);
    let wanted = render_value(&condition.value, context);
    let compare = |test: fn(f64, f64) -> bool| {
        found.and_then(number).zip(number(&wanted)).is_some_and(|(a, b)| test(a, b))
    };
    match condition.op {
        Op::Exists => found.is_some_and(|value| !value.is_null()),
        Op::Missing => found.is_none_or(Value::is_null),
        Op::Eq => found.is_some_and(|value| equal(value, &wanted)),
        Op::Ne => !found.is_some_and(|value| equal(value, &wanted)),
        Op::Contains => match found {
            Some(Value::Array(items)) => items.iter().any(|item| equal(item, &wanted)),
            Some(Value::String(text)) => text.contains(&self::text(&wanted)),
            _ => false,
        },
        Op::In => match &wanted {
            Value::Array(items) => {
                found.is_some_and(|value| items.iter().any(|item| equal(value, item)))
            }
            _ => false,
        },
        Op::StartsWith => found.is_some_and(|value| text(value).starts_with(&text(&wanted))),
        Op::Gt => compare(|a, b| a > b),
        Op::Gte => compare(|a, b| a >= b),
        Op::Lt => compare(|a, b| a < b),
        Op::Lte => compare(|a, b| a <= b),
    }
}

/// The first condition that fails, if any.
pub fn first_failing<'a>(conditions: &'a [Condition], context: &Value) -> Option<&'a Condition> {
    conditions.iter().find(|condition| !passes(condition, context))
}

/// What conditions and templates read: the trigger's input, and what the automation belongs to.
pub fn context(input: &Value, automation: &crate::store::Automation) -> Value {
    let mut context = match input {
        Value::Object(_) => input.clone(),
        other => json!({ "payload": other }),
    };
    let (kind, name) = automation.resource.split_once(':').unwrap_or(("", &automation.resource));
    context["resource"] = json!({ "ref": automation.resource, "kind": kind, "name": name });
    context["automation"] = json!({ "id": automation.id, "name": automation.name });
    context
}
