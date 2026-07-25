//! Suggestions for the fields of an automation that can be known before it is saved: the fields a
//! condition can test and the values they have had, both read off samples of what the chosen
//! trigger really brought; the plugins a request can ask; and schedules people often want. Who a
//! notification goes to is the platform's own people picker. Each answers `doc-combo`.

use std::collections::BTreeMap;
use std::time::Duration;

use askama::Template;
use doc_plugin_sdk::{Backend, Request};
use serde_json::{Value, json};

use super::{render, topic_closeness};
use crate::Refusal;
use crate::api::{id_of, query};
use crate::model::{Trigger, topic_matches};
use crate::store::Store;

const SHOWN: usize = 20;
/// How many samples a trigger's fields are read from: a filter can cover many topics.
const SAMPLES: usize = 20;
/// How deep into a payload its fields are offered.
const DEPTH: usize = 6;

/// One option in a picker: what it puts in the field, what it says, and why it is offered.
pub struct Choice {
    pub value: String,
    pub label: String,
    pub kind: &'static str,
    pub title: String,
}

#[derive(Template)]
#[template(path = "choices.html")]
struct Choices {
    found: Vec<Choice>,
    more: bool,
    empty: String,
}

fn answer(mut found: Vec<(u32, Choice)>, empty: String) -> Result<String, Refusal> {
    found.sort_by(|(a_close, a), (b_close, b)| a_close.cmp(b_close).then(a.label.cmp(&b.label)));
    let more = found.len() > SHOWN;
    let found = found.into_iter().take(SHOWN).map(|(_, choice)| choice).collect();
    render(&Choices { found, more, empty })
}

/// What has been typed in the field the picker belongs to, which `for` names. (Not `field`, which
/// is the name of a condition's own field.)
fn typed(request: &Request) -> String {
    let named = query(request, "for").unwrap_or_default();
    query(request, &named).unwrap_or_default().trim().to_lowercase()
}

/// The trigger the suggestions are for: a saved automation's, or the one being chosen on the same
/// page, sent as that step's own fields (whatever their prefix).
async fn trigger(backend: &Backend, request: &Request) -> Option<(Trigger, Option<String>)> {
    if let Some(id) = query(request, "automation") {
        let automation = crate::api::found(backend, id_of(&id).ok()?).await.ok()?;
        return Some((automation.trigger, Some(automation.id.to_string())));
    }
    let sent: Vec<(String, String)> =
        url::form_urlencoded::parse(request.query.as_bytes()).into_owned().collect();
    let ending = |suffix: &str| {
        sent.iter()
            .find(|(name, value)| name.ends_with(suffix) && !value.trim().is_empty())
            .map(|(_, value)| value.trim().to_string())
    };
    let chosen = match ending("trigger")?.as_str() {
        "event" => Trigger::Event { topic: ending("topic")? },
        "queue" => Trigger::Queue { queue: ending("queue")? },
        "cron" => Trigger::Cron { cron: ending("cron").unwrap_or_default() },
        _ => Trigger::Webhook,
    };
    Some((chosen, None))
}

/// What the trigger has really brought, as `(where it came from, input)`.
async fn samples(
    backend: &Backend,
    trigger: &Trigger,
    automation: Option<&str>,
) -> Vec<(String, Value)> {
    let store = Store(backend);
    let found = match trigger {
        Trigger::Event { topic } => store
            .samples("event")
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|(name, _)| topic_matches(topic, name))
            .collect(),
        Trigger::Queue { queue } => store
            .samples("queue")
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|(name, _)| name == queue)
            .collect(),
        Trigger::Webhook => match automation {
            Some(id) => store
                .samples("webhook")
                .await
                .unwrap_or_default()
                .into_iter()
                .filter(|(name, _)| name == id)
                .map(|(_, input)| ("its webhook".to_string(), input))
                .collect(),
            None => Vec::new(),
        },
        Trigger::Cron { .. } => Vec::new(),
    };
    found.into_iter().take(SAMPLES).collect()
}

/// Every field of `value` down to `DEPTH`, each with what it held: a list is offered whole (for
/// `contains`) and by its first item.
fn flatten(value: &Value, path: &str, depth: usize, into: &mut BTreeMap<String, Vec<Value>>) {
    let joined = |key: &str| match path.is_empty() {
        true => key.to_string(),
        false => format!("{path}.{key}"),
    };
    match value {
        Value::Object(fields) if depth < DEPTH => {
            for (key, child) in fields {
                flatten(child, &joined(key), depth + 1, into);
            }
        }
        Value::Array(items) if depth < DEPTH => {
            into.entry(path.to_string()).or_default().push(value.clone());
            if let Some(first) = items.first() {
                flatten(first, &joined("0"), depth + 1, into);
            }
        }
        Value::Object(_) | Value::Array(_) => {}
        leaf => into.entry(path.to_string()).or_default().push(leaf.clone()),
    }
}

fn shown(value: &Value) -> String {
    let text = match value {
        Value::String(text) => format!("\"{text}\""),
        other => other.to_string(),
    };
    match text.chars().count() > 60 {
        true => format!("{}…", text.chars().take(59).collect::<String>()),
        false => text,
    }
}

/// The context every automation's conditions can read, whatever its trigger brought.
fn context(trigger: &Trigger) -> Vec<(&'static str, &'static str)> {
    let mut paths = vec![
        ("resource.name", "the name of what the automation belongs to"),
        ("resource.kind", "what kind of resource it belongs to"),
        ("resource.ref", "the resource as kind:name"),
        ("automation.name", "this automation's name"),
    ];
    paths.extend(match trigger {
        Trigger::Event { .. } => {
            vec![("topic", "the topic the event was published on"), ("source", "who published it")]
        }
        Trigger::Queue { .. } => {
            vec![("queue", "the queue it came to"), ("sender", "the plugin that sent it")]
        }
        Trigger::Webhook => vec![("payload", "the body that was posted")],
        Trigger::Cron { .. } => vec![],
    });
    paths
}

/// `GET suggest/paths`: the fields a condition can test.
pub async fn paths(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let text = typed(request);
    let Some((trigger, automation)) = trigger(backend, request).await else {
        let empty = "Choose the trigger first: what it brings is what a condition can test.";
        return answer(Vec::new(), empty.into());
    };
    let samples = samples(backend, &trigger, automation.as_deref()).await;
    let mut fields: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for (_, input) in &samples {
        if let Some(payload) = input.get("payload") {
            flatten(payload, "payload", 1, &mut fields);
        }
    }
    let mut found: Vec<(u32, Choice)> = fields
        .into_iter()
        .filter_map(|(path, seen)| {
            let close = topic_closeness(&path.to_lowercase(), &text)?;
            let title = format!("such as {}", seen.first().map(shown).unwrap_or_default());
            Some((close, Choice { label: path.clone(), value: path, kind: "field", title }))
        })
        .collect();
    for (path, what) in context(&trigger) {
        if let Some(close) = topic_closeness(path, &text) {
            let choice = Choice {
                value: path.into(),
                label: path.into(),
                kind: "context",
                title: what.into(),
            };
            found.push((close + 5, choice));
        }
    }
    let empty = match samples.is_empty() {
        true => "Nothing has come through this trigger yet, so its fields are not known; a path such as payload.title can still be typed.".to_string(),
        false => format!("No field matches “{text}”."),
    };
    answer(found, empty)
}

/// `GET suggest/values`: what a field has held, for a condition's value. `path` names the field
/// holding the path.
pub async fn values(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let text = typed(request);
    let path = query(request, "path").and_then(|named| query(request, &named)).unwrap_or_default();
    let Some((trigger, automation)) = trigger(backend, request).await else {
        return answer(Vec::new(), "Choose the trigger and the field first.".into());
    };
    if path.is_empty() {
        return answer(
            Vec::new(),
            "Name the field first, and what it has held is offered here.".into(),
        );
    }
    let mut seen: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (source, input) in samples(backend, &trigger, automation.as_deref()).await {
        let mut fields = BTreeMap::new();
        flatten(&input, "", 0, &mut fields);
        for value in fields.remove(&path).unwrap_or_default() {
            let written = match value {
                Value::String(text) => text,
                Value::Array(_) | Value::Object(_) => continue,
                other => other.to_string(),
            };
            seen.entry(written).or_default().push(source.clone());
        }
    }
    let found = seen
        .into_iter()
        .filter_map(|(value, sources)| {
            let close = topic_closeness(&value.to_lowercase(), &text)?;
            let title = format!("seen in {}", sources.join(", "));
            Some((close, Choice { label: value.clone(), value, kind: "seen", title }))
        })
        .collect();
    answer(found, format!("No value seen at {path} matches; any value can still be typed."))
}

/// `GET suggest/plugins`: the plugins a request can ask, which are those the person can use.
pub async fn plugins(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let text = typed(request);
    let access = backend
        .request("core.access", "access", json!({}), Duration::from_secs(10))
        .await
        .map_err(|err| {
            Refusal::unavailable(format!("core could not say what you can use: {err}"))
        })?;
    let found = access["plugins"]
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(_, held)| held["read"] == true && held["running"] == true)
        .filter_map(|(plugin, held)| {
            let close = topic_closeness(plugin, &text)?;
            let title = held["nav"][0]["label"].as_str().unwrap_or_default().to_string();
            Some((
                close,
                Choice { value: plugin.clone(), label: plugin.clone(), kind: "plugin", title },
            ))
        })
        .collect();
    answer(found, format!("No plugin you can use matches “{text}”."))
}

/// Schedules people often want, in UTC.
const SCHEDULES: [(&str, &str); 8] = [
    ("*/15 * * * *", "every 15 minutes"),
    ("0 * * * *", "every hour, on the hour"),
    ("0 9 * * *", "every day at 09:00"),
    ("0 9 * * 1-5", "every weekday at 09:00"),
    ("0 17 * * 5", "every Friday at 17:00"),
    ("0 9 * * 1", "every Monday at 09:00"),
    ("0 0 1 * *", "the first of every month, at midnight"),
    ("0 3 * * *", "every night at 03:00"),
];

/// `GET suggest/schedules`: common cron schedules, found by the words for them too.
pub fn schedules(request: &Request) -> Result<String, Refusal> {
    let text = typed(request);
    let found = SCHEDULES
        .iter()
        .filter(|(cron, said)| text.is_empty() || cron.contains(&text) || said.contains(&text))
        .enumerate()
        .map(|(at, (cron, said))| {
            let choice = Choice {
                value: (*cron).to_string(),
                label: (*cron).to_string(),
                kind: "schedule",
                title: format!("{said}, UTC"),
            };
            (at as u32, choice)
        })
        .collect();
    answer(found, "Write five cron fields, such as 30 8 * * 1-5.".into())
}
