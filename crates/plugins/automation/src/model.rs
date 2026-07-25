//! What an automation is: the resource it belongs to, one trigger, conditions on what the trigger
//! brought, and the actions it takes. Everything is checked when it is saved.

use std::collections::BTreeMap;

use doc_plugin_sdk::protocol::Secret;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::Refusal;

pub const MAX_ACTIONS: usize = 10;
pub const MAX_CONDITIONS: usize = 20;
pub const OWN_TOPICS: &str = "plugin.automation.";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Trigger {
    /// Five fields, in UTC.
    Cron { cron: String },
    /// A topic filter: `*` stands for one segment and a final `>` for the rest.
    Event { topic: String },
    /// Messages another plugin sends to `plugin.automation`, subject `discovery/queues/<queue>`.
    Queue { queue: String },
    /// Signed POSTs to the automation's own URL.
    Webhook,
}

impl Trigger {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Cron { .. } => "cron",
            Self::Event { .. } => "event",
            Self::Queue { .. } => "queue",
            Self::Webhook => "webhook",
        }
    }

    /// What the trigger is found by: its schedule, topic filter or queue.
    pub fn key(&self) -> Option<&str> {
        match self {
            Self::Cron { cron } => Some(cron),
            Self::Event { topic } => Some(topic),
            Self::Queue { queue } => Some(queue),
            Self::Webhook => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Op {
    Eq,
    Ne,
    Contains,
    In,
    StartsWith,
    Exists,
    Missing,
    Gt,
    Gte,
    Lt,
    Lte,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Condition {
    /// A dotted path into what the trigger brought, such as `payload.team`.
    pub field: String,
    pub op: Op,
    #[serde(default)]
    pub value: Value,
}

fn get() -> String {
    "GET".into()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Action {
    /// POSTs JSON, retrying a failure that may pass; `secret` signs the body as `x-doc-signature`.
    Webhook {
        url: String,
        #[serde(default)]
        headers: BTreeMap<String, String>,
        #[serde(default)]
        body: Option<Value>,
        #[serde(default, serialize_with = "doc_secret::exposed_option")]
        secret: Option<Secret<String>>,
    },
    /// To `channel`, or else the resource's team's channel.
    Slack {
        #[serde(default)]
        channel: Option<String>,
        text: String,
    },
    /// To `to`, or else the resource's team's address.
    Email {
        #[serde(default)]
        to: Option<String>,
        subject: String,
        body: String,
    },
    /// Published as `plugin.automation.<topic>`.
    Event {
        topic: String,
        #[serde(default)]
        payload: Option<Value>,
    },
    /// Another plugin's `api/<route>`, asked as the owner.
    Request {
        plugin: String,
        #[serde(default = "get")]
        method: String,
        route: String,
        #[serde(default)]
        query: Option<String>,
        #[serde(default)]
        body: Option<Value>,
    },
    /// An operation another plugin offers by name (ADR-0012), such as `dora`'s `increment`, asked
    /// as the owner. Each parameter is a template. What it calls is looked up when it runs, so a
    /// plugin may move the route behind it.
    Operation {
        plugin: String,
        operation: String,
        #[serde(default)]
        params: BTreeMap<String, String>,
    },
    /// Puts a notification in `user`'s inbox (the header bell and `/p/notifications/`). Unlike
    /// Slack and Email, there is no team to fall back to: a notification is always for someone
    /// specific, so `user` is required.
    Notify {
        user: String,
        title: String,
        body: String,
        #[serde(default)]
        url: Option<String>,
    },
}

impl Action {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Webhook { .. } => "webhook",
            Self::Slack { .. } => "slack",
            Self::Email { .. } => "email",
            Self::Event { .. } => "event",
            Self::Request { .. } => "request",
            Self::Operation { .. } => "operation",
            Self::Notify { .. } => "notify",
        }
    }
}

fn segment(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// An Event Bus topic filter, as the manifest's subscriptions are written.
pub fn topic_filter(filter: &str) -> Result<(), String> {
    let parts: Vec<&str> = filter.split('.').collect();
    for (at, part) in parts.iter().enumerate() {
        let last = at + 1 == parts.len();
        if !(segment(part) || *part == "*" || (*part == ">" && last)) {
            return Err(format!(
                "`{filter}` is not a topic filter: segments are a-z, 0-9 and -, `*`, or a final `>`"
            ));
        }
    }
    if filter.starts_with(OWN_TOPICS) {
        return Err("an automation cannot be triggered by the events automations publish".into());
    }
    Ok(())
}

/// Whether `topic` is one `filter` covers.
pub fn topic_matches(filter: &str, topic: &str) -> bool {
    let mut wanted = filter.split('.');
    let mut given = topic.split('.');
    loop {
        match (wanted.next(), given.next()) {
            (Some(">"), Some(_)) => return true,
            (Some("*"), Some(_)) => {}
            (Some(part), Some(got)) if part == got => {}
            (None, None) => return true,
            _ => return false,
        }
    }
}

pub fn published_topic(topic: &str) -> String {
    match topic.strip_prefix(OWN_TOPICS) {
        Some(_) => topic.to_string(),
        None => format!("{OWN_TOPICS}{topic}"),
    }
}

fn address(text: &str) -> bool {
    let text = text.trim();
    text.contains('@') && !text.contains(['\r', '\n', ' ', ',', '<', '>'])
}

impl Trigger {
    pub fn checked(&self) -> Result<(), Refusal> {
        match self {
            Self::Cron { cron } => cron
                .parse::<croner::Cron>()
                .map(|_| ())
                .map_err(|err| Refusal::bad(format!("`{cron}` is not a cron schedule: {err}"))),
            Self::Event { topic } => topic_filter(topic).map_err(Refusal::bad),
            Self::Queue { queue } if segment(queue) && queue.len() <= 64 => Ok(()),
            Self::Queue { queue } => Err(Refusal::bad(format!(
                "`{queue}` is not a queue name: up to 64 of a-z, 0-9 and -"
            ))),
            Self::Webhook => Ok(()),
        }
    }
}

impl Action {
    pub fn checked(&self) -> Result<(), Refusal> {
        let refused = |why: String| Err(Refusal::bad(format!("a {} action {why}", self.kind())));
        match self {
            Self::Webhook { url, headers, .. } => {
                match url::Url::parse(url) {
                    Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => {}
                    _ => return refused(format!("needs an http or https URL, not `{url}`")),
                }
                if let Some(bad) = headers.keys().find(|name| {
                    name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
                }) {
                    return refused(format!("has a header name `{bad}` that is not one"));
                }
                if headers.values().any(|value| value.contains(['\r', '\n'])) {
                    return refused("has a header value with a line break".into());
                }
                Ok(())
            }
            Self::Slack { text, .. } if text.trim().is_empty() || text.chars().count() > 4000 => {
                refused("needs text, up to 4000 characters".into())
            }
            Self::Slack { .. } => Ok(()),
            Self::Email { to, subject, body } => {
                if to.as_deref().is_some_and(|to| !address(to)) {
                    return refused(format!(
                        "sends to one address, not `{}`",
                        to.as_deref().unwrap_or_default()
                    ));
                }
                if subject.trim().is_empty()
                    || subject.contains(['\r', '\n'])
                    || subject.chars().count() > 200
                {
                    return refused("needs a subject of one line, up to 200 characters".into());
                }
                if body.trim().is_empty() {
                    return refused("needs a body".into());
                }
                Ok(())
            }
            Self::Event { topic, .. } => {
                let full = published_topic(topic);
                match full[OWN_TOPICS.len()..].split('.').all(segment) {
                    true => Ok(()),
                    false => refused(format!(
                        "publishes under plugin.automation., and `{topic}` is not a topic"
                    )),
                }
            }
            Self::Request { plugin, method, route, .. } => {
                if !segment(plugin) {
                    return refused(format!("names a plugin by its ID, not `{plugin}`"));
                }
                if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
                    return refused(format!(
                        "uses GET, POST, PUT, PATCH or DELETE, not `{method}`"
                    ));
                }
                let clean = !route.is_empty()
                    && !route.starts_with('/')
                    && route.split('/').all(|part| part != "." && part != "..");
                match clean {
                    true => Ok(()),
                    false => {
                        refused(format!("asks for a route under the plugin's api/, not `{route}`"))
                    }
                }
            }
            Self::Operation { plugin, operation, .. } => {
                match segment(plugin) && segment(operation) {
                    true => Ok(()),
                    false => refused(format!(
                        "names a plugin and one of its operations, not `{plugin}.{operation}`"
                    )),
                }
            }
            Self::Notify { user, title, body, .. } => {
                if user.trim().is_empty() {
                    return refused("names who it is for".into());
                }
                if title.trim().is_empty() || title.chars().count() > 200 {
                    return refused("needs a title, up to 200 characters".into());
                }
                if body.trim().is_empty() {
                    return refused("needs a body".into());
                }
                Ok(())
            }
        }
    }
}

pub fn checked_conditions(conditions: &[Condition]) -> Result<(), Refusal> {
    if conditions.len() > MAX_CONDITIONS {
        return Err(Refusal::bad(format!("an automation has at most {MAX_CONDITIONS} conditions")));
    }
    match conditions.iter().find(|condition| condition.field.trim().is_empty()) {
        Some(_) => Err(Refusal::bad("a condition names the field it tests, such as payload.team")),
        None => Ok(()),
    }
}

pub fn checked_actions(actions: &[Action]) -> Result<(), Refusal> {
    if actions.is_empty() || actions.len() > MAX_ACTIONS {
        return Err(Refusal::bad(format!("an automation takes 1 to {MAX_ACTIONS} actions")));
    }
    actions.iter().try_for_each(Action::checked)
}
