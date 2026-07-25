//! An automation drawn as a chain (ADR-0012): the trigger under **When**, each condition in its
//! own column so that each must pass before the next, and the actions under **Then** in the order
//! they run. The Service Map lays it out and draws it, as it does every graph on the platform.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::model::{Action, Condition, Trigger};
use crate::store::Automation;

/// A node of the graph, as the page's `node` query names it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Node {
    Trigger,
    Condition(usize),
    AddCondition,
    Action(usize),
    AddAction,
}

impl Node {
    pub fn named(text: &str) -> Option<Self> {
        let indexed = |prefix: &str| text.strip_prefix(prefix).and_then(|at| at.parse().ok());
        match text {
            "trigger" => Some(Self::Trigger),
            "add-condition" => Some(Self::AddCondition),
            "add-action" => Some(Self::AddAction),
            _ => indexed("condition-")
                .map(Self::Condition)
                .or_else(|| indexed("action-").map(Self::Action)),
        }
    }

    pub fn id(self) -> String {
        match self {
            Self::Trigger => "trigger".into(),
            Self::Condition(at) => format!("condition-{at}"),
            Self::AddCondition => "add-condition".into(),
            Self::Action(at) => format!("action-{at}"),
            Self::AddAction => "add-action".into(),
        }
    }
}

/// Where the page is, so a node can link back to it with itself chosen.
pub fn href(automation: &Automation, node: Node) -> String {
    format!("/p/automation/a/{}?node={}#node", automation.id, node.id())
}

pub fn trigger_label(trigger: &Trigger) -> (String, String) {
    match trigger {
        Trigger::Cron { cron } => {
            (format!("Every {cron}"), format!("On the schedule {cron}, in UTC"))
        }
        Trigger::Event { topic } => (topic.clone(), format!("When {topic} is published")),
        Trigger::Queue { queue } => {
            (format!("Queue {queue}"), format!("A message to the queue {queue}"))
        }
        Trigger::Webhook => ("A webhook".into(), "A signed POST to its own URL".into()),
    }
}

fn written(value: &Value) -> String {
    match value {
        Value::String(text) => format!("\"{text}\""),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

pub fn condition_label(condition: &Condition, op: &str) -> (String, String) {
    let last = condition.field.rsplit('.').next().unwrap_or(&condition.field);
    let value = written(&condition.value);
    let short = format!("{last} {op} {value}").trim_end().to_string();
    let full = format!("{} {op} {value}", condition.field).trim_end().to_string();
    (short, full)
}

/// What an action says on its node, and in full. `people` names the DOC users it may be for by
/// their IDs, so a node says who rather than an ID.
pub fn action_label(action: &Action, people: &BTreeMap<String, String>) -> (String, String) {
    match action {
        Action::Slack { channel, text } => (
            format!("Slack {}", channel.as_deref().unwrap_or("the team")),
            format!("Post to Slack: {text}"),
        ),
        Action::Email { to, subject, .. } => (
            format!("Email {}", to.as_deref().unwrap_or("the team")),
            format!("Send an email: {subject}"),
        ),
        Action::Webhook { url, .. } => {
            let host = url::Url::parse(url)
                .ok()
                .and_then(|parsed| parsed.host_str().map(str::to_string))
                .unwrap_or_else(|| url.clone());
            (format!("Webhook {host}"), format!("POST to {url}"))
        }
        Action::Event { topic, .. } => {
            (format!("Publish {topic}"), format!("Publish plugin.automation.{topic}"))
        }
        Action::Request { plugin, method, route, .. } => {
            (format!("{plugin} {method} {route}"), format!("{method} {plugin} api/{route}"))
        }
        Action::Operation { plugin, operation, params } => {
            let values: Vec<&str> = params.values().map(String::as_str).collect();
            let named: Vec<String> =
                params.iter().map(|(name, value)| format!("{name}: {value}")).collect();
            (
                format!("{plugin}.{operation}({})", values.join(", ")),
                format!("{plugin}.{operation}, with {}", named.join(", ")),
            )
        }
        Action::Notify { user, title, .. } => {
            let user = people.get(user).unwrap_or(user);
            (format!("Notify {user}"), format!("Notify {user}: {title}"))
        }
    }
}

/// What the Service Map's `discovery/draw` takes. `op` names a condition's test as the page does.
pub fn drawing(
    automation: &Automation,
    chosen: Option<Node>,
    editable: bool,
    op: impl Fn(&Condition) -> &'static str,
    people: &BTreeMap<String, String>,
) -> Value {
    let mut items = Vec::new();
    let mut links = Vec::new();
    let mut headings = serde_json::Map::new();
    let mut item = |node: Node, column: &str, (label, note): (String, String), order: u32| {
        let ghost = matches!(node, Node::AddCondition | Node::AddAction);
        items.push(json!({
            "id": node.id(),
            "column": column,
            "label": label,
            "note": note,
            "href": editable.then(|| href(automation, node)),
            "focus": chosen == Some(node),
            "order": order,
            "ghost": ghost,
        }));
    };

    headings.insert("when".into(), json!("When"));
    item(Node::Trigger, "when", trigger_label(&automation.trigger), 0);
    let mut last = Node::Trigger.id();
    for (at, condition) in automation.conditions.iter().enumerate() {
        let column = format!("if-{at}");
        headings.insert(column.clone(), json!(if at == 0 { "If" } else { "And" }));
        item(Node::Condition(at), &column, condition_label(condition, op(condition)), 0);
        let node = Node::Condition(at).id();
        links.push(json!({ "from": last, "to": node }));
        last = node;
    }
    if editable {
        let column = format!("if-{}", automation.conditions.len().saturating_sub(1));
        headings.entry(column.clone()).or_insert(json!("If"));
        let add = ("Add a condition".to_string(), "Only run when this is true as well".to_string());
        item(Node::AddCondition, &column, add, 1);
    }
    headings.insert("then".into(), json!("Then"));
    for (at, action) in automation.actions.iter().enumerate() {
        let (label, note) = action_label(action, people);
        item(Node::Action(at), "then", (format!("{}. {label}", at + 1), note), at as u32);
        links.push(json!({ "from": last, "to": Node::Action(at).id() }));
    }
    if editable && automation.actions.len() < crate::model::MAX_ACTIONS {
        let add =
            ("Add an action".to_string(), "Do something else as well, after these".to_string());
        item(Node::AddAction, "then", add, u32::MAX - 1);
    }

    let when = trigger_label(&automation.trigger).1;
    let tests: Vec<String> = automation
        .conditions
        .iter()
        .map(|condition| condition_label(condition, op(condition)).1)
        .collect();
    let dos: Vec<String> =
        automation.actions.iter().map(|action| action_label(action, people).1).collect();
    let description = match tests.is_empty() {
        true => format!("{when}: {}.", dos.join("; then ")),
        false => format!("{when}, if {}: {}.", tests.join(" and "), dos.join("; then ")),
    };
    let mut columns = vec![json!("when")];
    columns.extend((0..automation.conditions.len().max(1)).map(|at| json!(format!("if-{at}"))));
    columns.push(json!("then"));
    json!({
        "items": items,
        "links": links,
        "columns": columns,
        "headings": headings,
        "description": description,
    })
}
