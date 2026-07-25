//! Building a new automation from ready-made steps (ADR-0012 §4): a palette of triggers, conditions
//! and actions — every operation the person can call among them — put on a chain of **When**,
//! **If** and **Then** by dragging them there or with each one's **Add**. Every step's fields carry
//! a prefix of their own, so the whole chain is one form, and the order of its nodes is the order
//! its steps are saved in.

use std::collections::BTreeMap;

use askama::Template;
use doc_plugin_sdk::{Backend, Request};
use uuid::Uuid;

use super::{
    ACTION_KINDS, ActionFields, Form, OPS, TRIGGER_KINDS, TriggerFields, action_from,
    condition_from, field, op_text, render, trigger_from,
};
use crate::model::{Action, Condition, Op, Trigger};
use crate::{Refusal, graph, operations};

/// Conditions ready to put on the chain: a test, what the palette calls it, and what it is for.
const CONDITIONS: [(&str, &str, &str); 5] = [
    ("eq", "Something equals", "A field of what the trigger brought is exactly this"),
    ("contains", "Something contains", "Text that contains this, or a list holding it"),
    ("starts-with", "Something starts with", "Text that starts with this, such as release/"),
    ("exists", "Something is there", "A field the trigger brought at all"),
    ("gt", "A number is more than", "A count or a size above this"),
];

/// One ready-made step in the palette, and where it fetches its node and form from.
pub struct Item {
    pub label: String,
    pub hint: String,
    pub url: String,
}

/// The palette's steps for one column of the chain.
pub struct Group {
    pub zone: &'static str,
    pub heading: &'static str,
    pub items: Vec<Item>,
}

/// A step on the chain: its node in its column, and its form below the chain.
pub struct Step {
    pub zone: &'static str,
    pub prefix: String,
    pub heading: &'static str,
    pub title: String,
    /// A trigger's or an action's own fields, drawn by their fragment.
    pub fields: String,
    /// A condition's field, test and value.
    pub field: String,
    pub op: String,
    pub value: String,
    pub selected: bool,
}

impl Step {
    pub fn op_is(&self, op: &str) -> bool {
        self.op == op
    }
}

/// A step just added: its node goes where it was dropped, and its form joins the others.
#[derive(Template)]
#[template(path = "step.html")]
struct Fragment {
    step: Step,
    ops: &'static [(&'static str, &'static str)],
}

fn heading(zone: &str) -> &'static str {
    match zone {
        "when" => "When",
        "if" => "Only if",
        _ => "Then",
    }
}

/// A prefix no other step on the page has: `s` and eight hex digits from a v7 UUID's random bits.
fn prefix() -> String {
    let id = Uuid::now_v7().simple().to_string();
    format!("s{}-", &id[id.len() - 8..])
}

fn url(query: &[(&str, &str)]) -> String {
    let mut written = url::form_urlencoded::Serializer::new(String::new());
    for (name, value) in query {
        written.append_pair(name, value);
    }
    format!("/p/automation/new/step?{}", written.finish())
}

pub async fn palette(backend: &Backend) -> Result<Vec<Group>, Refusal> {
    let when = TRIGGER_KINDS
        .iter()
        .map(|(kind, label)| Item {
            label: (*label).to_string(),
            hint: String::new(),
            url: url(&[("zone", "when"), ("trigger", kind)]),
        })
        .collect();
    let conditions = CONDITIONS
        .iter()
        .map(|(op, label, hint)| Item {
            label: (*label).to_string(),
            hint: (*hint).to_string(),
            url: url(&[("zone", "if"), ("op", op)]),
        })
        .collect();
    let mut actions: Vec<Item> = ACTION_KINDS
        .iter()
        .filter(|(kind, _)| *kind != "operation")
        .map(|(kind, label)| Item {
            label: (*label).to_string(),
            hint: String::new(),
            url: url(&[("zone", "then"), ("action", kind)]),
        })
        .collect();
    // What the plugins the person can use offer, each a ready-made step of its own.
    for offered in operations::offered(backend).await? {
        let key = offered.key();
        actions.push(Item {
            label: format!("{} · {}", offered.plugin, offered.operation.label),
            hint: offered.operation.description.clone(),
            url: url(&[("zone", "then"), ("action", "operation"), ("operation", &key)]),
        });
    }
    Ok(vec![
        Group { zone: "when", heading: "When", items: when },
        Group { zone: "if", heading: "Only if", items: conditions },
        Group { zone: "then", heading: "Then", items: actions },
    ])
}

fn kind_label(kinds: &[(&str, &'static str)], kind: &str) -> String {
    kinds.iter().find(|(value, _)| *value == kind).map_or("A step", |(_, label)| label).to_string()
}

fn trigger_step(prefix: String, kind: &str, value: String) -> Result<Step, Refusal> {
    let fields = TriggerFields { value, ..TriggerFields::of(kind, None).prefixed(&prefix) };
    Ok(Step {
        zone: "when",
        heading: heading("when"),
        title: kind_label(&TRIGGER_KINDS, kind),
        fields: render(&fields)?,
        field: String::new(),
        op: String::new(),
        value: String::new(),
        selected: false,
        prefix,
    })
}

fn condition_step(prefix: String, condition: &Condition, title: String) -> Step {
    let op = serde_json::to_value(condition.op)
        .ok()
        .and_then(|op| op.as_str().map(str::to_string))
        .unwrap_or_else(|| "eq".into());
    Step {
        zone: "if",
        heading: heading("if"),
        title,
        fields: String::new(),
        field: condition.field.clone(),
        op,
        value: match &condition.value {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Null => String::new(),
            other => other.to_string(),
        },
        selected: false,
        prefix,
    }
}

async fn action_step(
    backend: &Backend,
    prefix: String,
    kind: &str,
    values: BTreeMap<String, String>,
) -> Result<Step, Refusal> {
    let fields = ActionFields::of(backend, kind, values, false).await?.prefixed(&prefix);
    let title = match fields.operations.iter().find(|(key, _, _)| *key == fields.chosen) {
        Some((_, plugin, label)) if kind == "operation" => format!("{plugin} · {label}"),
        _ => kind_label(&ACTION_KINDS, kind),
    };
    Ok(Step {
        zone: "then",
        heading: heading("then"),
        title,
        fields: render(&fields)?,
        field: String::new(),
        op: String::new(),
        value: String::new(),
        selected: false,
        prefix,
    })
}

/// `GET new/step`: a ready-made step, new on the chain, with its form open.
pub async fn fragment(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let query = |name: &str| super::query(request, name);
    let prefix = prefix();
    let mut step = match query("zone").as_deref() {
        Some("when") => {
            let kind = query("trigger").unwrap_or_else(|| "event".into());
            trigger_step(prefix, &kind, String::new())?
        }
        Some("if") => {
            let op = query("op").unwrap_or_else(|| "eq".into());
            let parsed: Op = serde_json::from_value(serde_json::json!(op))
                .map_err(|_| Refusal::bad(format!("`{op}` is not a test")))?;
            let title = CONDITIONS.iter().find(|(name, _, _)| *name == op).map_or_else(
                || format!("Something {}", op_text(parsed)),
                |(_, label, _)| (*label).to_string(),
            );
            let condition =
                Condition { field: String::new(), op: parsed, value: serde_json::Value::Null };
            condition_step(prefix, &condition, title)
        }
        Some("then") => {
            let kind = query("action").unwrap_or_else(|| "slack".into());
            let values = query("operation")
                .map(|chosen| BTreeMap::from([("operation".to_string(), chosen)]))
                .unwrap_or_default();
            action_step(backend, prefix, &kind, values).await?
        }
        _ => return Err(Refusal::bad("a step goes under When, If or Then")),
    };
    step.selected = true;
    render(&Fragment { step, ops: &OPS })
}

/// The prefixes of the steps in one column, in the order their nodes are in.
fn column<'a>(form: &'a Form, zone: &str) -> Vec<&'a str> {
    form.iter()
        .filter(|(name, _)| name == zone)
        .map(|(_, prefix)| prefix.as_str())
        .filter(|prefix| !prefix.is_empty())
        .collect()
}

/// One step's own fields, with its prefix taken off.
fn step_form(form: &Form, prefix: &str) -> Form {
    form.iter()
        .filter_map(|(name, value)| {
            name.strip_prefix(prefix).map(|name| (name.to_string(), value.clone()))
        })
        .collect()
}

/// What a chain says: its trigger, its conditions and its actions in order, each action with the
/// prefix of its step.
pub type Chain = (Trigger, Vec<Condition>, Vec<(Action, String)>);

/// Why a chain was refused, and the prefix of the step that was, when it was one.
pub type Refused = (Refusal, Option<String>);

/// What the chain says, or what is wrong with it and which step it is.
pub fn parse(form: &Form) -> Result<Chain, Refused> {
    let named = |step: &str, prefix: &str| {
        let step = step.to_string();
        let prefix = prefix.to_string();
        move |refusal: Refusal| {
            (Refusal { detail: format!("{step}: {}", refusal.detail), ..refusal }, Some(prefix))
        }
    };
    let trigger = match column(form, "when").as_slice() {
        [prefix] => {
            let trigger = trigger_from(&step_form(form, prefix)).map_err(named("When", prefix))?;
            trigger.checked().map_err(named("When", prefix))?;
            trigger
        }
        [] => return Err((Refusal::bad("put a trigger under When"), None)),
        _ => return Err((Refusal::bad("an automation has one trigger"), None)),
    };
    let mut conditions = Vec::new();
    for (at, prefix) in column(form, "if").into_iter().enumerate() {
        let step = format!("Only if, step {}", at + 1);
        let condition = condition_from(&step_form(form, prefix))
            .map_err(named(&step, prefix))?
            .ok_or_else(|| named(&step, prefix)(Refusal::bad("name the field it tests")))?;
        conditions.push(condition);
    }
    let mut actions = Vec::new();
    for (at, prefix) in column(form, "then").into_iter().enumerate() {
        let step = format!("Then, step {}", at + 1);
        let action = action_from(&step_form(form, prefix)).map_err(named(&step, prefix))?;
        action.checked().map_err(named(&step, prefix))?;
        actions.push((action, prefix.to_string()));
    }
    if actions.is_empty() {
        return Err((Refusal::bad("put at least one action under Then"), None));
    }
    Ok((trigger, conditions, actions))
}

/// Checks the chain's operations as the person saving it, naming the step that is refused.
pub async fn operations_checked(
    backend: &Backend,
    actions: &[(Action, String)],
) -> Result<(), Refused> {
    for (at, (action, prefix)) in actions.iter().enumerate() {
        operations::checked(backend, std::slice::from_ref(action)).await.map_err(|refusal| {
            let detail = format!("Then, step {}: {}", at + 1, refusal.detail);
            (Refusal { detail, ..refusal }, Some(prefix.clone()))
        })?;
    }
    Ok(())
}

/// The chain as it was sent, to draw again when it was refused: every step with what it held.
pub async fn steps(
    backend: &Backend,
    form: &Form,
    failed: Option<&str>,
) -> Result<(Vec<Step>, Vec<Step>, Vec<Step>), Refusal> {
    let mut when = Vec::new();
    for prefix in column(form, "when").into_iter().take(1) {
        let own = step_form(form, prefix);
        let kind = field(&own, "trigger").unwrap_or_else(|| "event".into());
        let value = ["cron", "topic", "queue"]
            .iter()
            .find_map(|name| field(&own, name))
            .unwrap_or_default();
        when.push(trigger_step(prefix.to_string(), &kind, value)?);
    }
    let mut ifs = Vec::new();
    for prefix in column(form, "if") {
        let own = step_form(form, prefix);
        let condition = condition_from(&own).ok().flatten().unwrap_or(Condition {
            field: String::new(),
            op: Op::Eq,
            value: serde_json::Value::Null,
        });
        let title = match condition.field.is_empty() {
            true => "A condition".to_string(),
            false => graph::condition_label(&condition, op_text(condition.op)).0,
        };
        ifs.push(condition_step(prefix.to_string(), &condition, title));
    }
    let mut thens = Vec::new();
    for prefix in column(form, "then") {
        let own = step_form(form, prefix);
        let kind = field(&own, "action").unwrap_or_else(|| "slack".into());
        let values: BTreeMap<String, String> = own.into_iter().collect();
        thens.push(action_step(backend, prefix.to_string(), &kind, values).await?);
    }
    for step in when.iter_mut().chain(ifs.iter_mut()).chain(thens.iter_mut()) {
        step.selected = failed == Some(step.prefix.as_str());
    }
    Ok((when, ifs, thens))
}
