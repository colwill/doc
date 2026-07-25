//! The plugin's pages at `/p/automation/...`: every automation, forms for triggers, conditions and
//! actions with a test run, each run's history, templates, and a panel for resource pages.

mod builder;
mod suggest;

use std::collections::BTreeMap;

use askama::Template;
use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};
use url::form_urlencoded::byte_serialize;
use uuid::Uuid;

use crate::api::{
    self, Change, FromTemplate, NewAutomation, Saved, id_of, may_change, normalised, query,
};
use crate::graph::{self, Node};
use crate::model::{Action, Condition, Op, Trigger};
use crate::store::{Automation, Run, Store};
use crate::{Engine, Refusal, operations, templates};

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    fn refused(refusal: &Refusal) -> Self {
        Self { notice: None, error: Some(refusal.detail.clone()) }
    }
}

type Form = Vec<(String, String)>;

fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> Option<String> {
    form.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn when(text: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|at| at.format("%-d %B %Y %H:%M:%S").to_string())
        .unwrap_or_else(|_| text.to_string())
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap_or_default()
}

/// The resource's page in Resource Definitions.
fn resource_href(resource: &str) -> String {
    let (kind, name) = resource.split_once(':').unwrap_or(("service", resource));
    let name: Vec<String> = name
        .split('/')
        .map(|part| byte_serialize(part.as_bytes()).collect::<String>().replace('+', "%20"))
        .collect();
    format!("/p/resources/r/{kind}/{}", name.join("/"))
}

pub fn trigger_text(trigger: &Trigger) -> String {
    match trigger {
        Trigger::Cron { cron } => format!("Cron {cron}"),
        Trigger::Event { topic } => format!("Event {topic}"),
        Trigger::Queue { queue } => format!("Queue {queue}"),
        Trigger::Webhook => "Webhook".into(),
    }
}

fn badge(state: &str) -> &'static str {
    match state {
        "succeeded" | "done" => "doc-badge--up",
        "failed" => "doc-badge--error",
        "running" | "retrying" | "claimed" => "doc-badge--loading",
        _ => "doc-badge--unknown",
    }
}

const OPS: [(&str, &str); 11] = [
    ("eq", "equals"),
    ("ne", "does not equal"),
    ("contains", "contains"),
    ("in", "is one of"),
    ("starts-with", "starts with"),
    ("exists", "is there"),
    ("missing", "is not there"),
    ("gt", "is more than"),
    ("gte", "is at least"),
    ("lt", "is less than"),
    ("lte", "is at most"),
];

fn op_text(op: Op) -> &'static str {
    let written = serde_json::to_value(op).unwrap_or_default();
    OPS.iter().find(|(name, _)| written == *name).map_or("?", |(_, text)| text)
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

struct Row {
    id: Uuid,
    name: String,
    resource: String,
    resource_href: String,
    trigger: String,
    owner: String,
    enabled: bool,
    last: Option<(String, &'static str, String)>,
}

#[derive(Template)]
#[template(path = "list.html")]
struct ListPage {
    flash: Flash,
    writes: bool,
    resource: String,
    rows: Vec<Row>,
}

#[derive(Template)]
#[template(path = "panel.html")]
struct PanelFragment {
    writes: bool,
    resource: String,
    rows: Vec<Row>,
}

#[derive(Template)]
#[template(path = "new.html")]
struct NewPage {
    flash: Flash,
    writes: bool,
    name: String,
    resource: String,
    palette: Vec<builder::Group>,
    when: Vec<builder::Step>,
    ifs: Vec<builder::Step>,
    thens: Vec<builder::Step>,
    ops: &'static [(&'static str, &'static str)],
}

impl NewPage {
    /// Every step on the chain, for their forms below it.
    fn steps(&self) -> impl Iterator<Item = &builder::Step> {
        self.when.iter().chain(&self.ifs).chain(&self.thens)
    }
}

const TRIGGER_KINDS: [(&str, &str); 4] = [
    ("event", "An Event Bus event"),
    ("cron", "A schedule"),
    ("queue", "A Service Bus queue"),
    ("webhook", "A webhook"),
];

const ACTION_KINDS: [(&str, &str); 7] = [
    ("slack", "Post to Slack"),
    ("email", "Send an email"),
    ("webhook", "Call a webhook"),
    ("event", "Publish an event"),
    ("operation", "Use another plugin"),
    ("request", "Ask another plugin, by route"),
    ("notify", "Notify someone in DOC"),
];

#[derive(Template)]
#[template(path = "trigger_fields.html")]
struct TriggerFields {
    kinds: &'static [(&'static str, &'static str)],
    /// What every field's name starts with, so one form can hold several steps (the builder).
    prefix: String,
    kind: String,
    /// Its schedule, topic or queue, when it is one being changed.
    value: String,
}

impl TriggerFields {
    fn of(kind: &str, trigger: Option<&Trigger>) -> Self {
        let value = trigger
            .filter(|trigger| trigger.kind() == kind)
            .and_then(Trigger::key)
            .unwrap_or_default()
            .to_string();
        Self { kinds: &TRIGGER_KINDS, prefix: String::new(), kind: kind.to_string(), value }
    }

    fn prefixed(mut self, prefix: &str) -> Self {
        self.prefix = prefix.to_string();
        self
    }

    fn is(&self, kind: &str) -> bool {
        self.kind == kind
    }
}

/// A parameter of the chosen operation, as its field.
struct ParamField {
    name: String,
    label: String,
    hint: String,
    required: bool,
    field: String,
}

#[derive(Template)]
#[template(path = "action_fields.html")]
struct ActionFields {
    kinds: &'static [(&'static str, &'static str)],
    /// What every field's name starts with, so one form can hold several steps (the builder).
    prefix: String,
    kind: String,
    /// Whether it is an action being changed, rather than one being added.
    editing: bool,
    /// What its fields hold now, by field name.
    values: BTreeMap<String, String>,
    /// For an operation: every one on offer, as `(plugin.name, plugin, label)`, the chosen one, what
    /// it says it does, and its parameters.
    operations: Vec<(String, String, String)>,
    chosen: String,
    about: String,
    params: Vec<ParamField>,
    /// For a notification to somebody already chosen: their DOC user ID, and the login the field
    /// shows in its place, so an ID is never what anyone reads.
    user_chosen: String,
    user_shown: String,
}

impl ActionFields {
    fn get(&self, name: &str) -> &str {
        self.values.get(name).map_or("", String::as_str)
    }

    fn is(&self, kind: &str) -> bool {
        self.kind == kind
    }

    fn picked(&self, operation: &str) -> bool {
        self.chosen == operation
    }

    fn prefixed(mut self, prefix: &str) -> Self {
        self.prefix = prefix.to_string();
        self
    }

    /// The fields for `kind`, filled with `values`; an operation's list what the viewer may call.
    async fn of(
        backend: &Backend,
        kind: &str,
        values: BTreeMap<String, String>,
        editing: bool,
    ) -> Result<Self, Refusal> {
        let mut fields = Self {
            kinds: &ACTION_KINDS,
            prefix: String::new(),
            kind: kind.to_string(),
            editing,
            values,
            operations: Vec::new(),
            chosen: String::new(),
            about: String::new(),
            params: Vec::new(),
            user_chosen: String::new(),
            user_shown: String::new(),
        };
        if kind == "notify" {
            let user = fields.get("user").trim().to_string();
            if Uuid::parse_str(&user).is_ok() {
                let query = doc_plugin_sdk::Query::new("core.users").filter(json!({ "id": user }));
                let found = Store(backend).core(query).await.unwrap_or_default();
                if let Some(login) = found.first().and_then(|person| person["login"].as_str()) {
                    fields.user_shown = login.to_string();
                    fields.user_chosen = user;
                }
            }
            return Ok(fields);
        }
        if kind != "operation" {
            return Ok(fields);
        }
        let offered = operations::offered(backend).await?;
        let wanted = fields.get("operation").to_string();
        let chosen = offered.iter().find(|one| one.key() == wanted).or(offered.first());
        if let Some(chosen) = chosen {
            fields.chosen = chosen.key();
            fields.about = chosen.operation.description.clone();
            fields.params = chosen
                .operation
                .params
                .iter()
                .map(|param| ParamField {
                    name: param.name.clone(),
                    label: param.label.clone(),
                    hint: param.hint.clone(),
                    required: param.required,
                    field: format!("param-{}", param.name),
                })
                .collect();
        }
        fields.operations = offered
            .iter()
            .map(|one| (one.key(), one.plugin.clone(), one.operation.label.clone()))
            .collect();
        Ok(fields)
    }
}

/// What an action's fields hold, so it can be changed where it is.
fn action_values(action: &Action) -> BTreeMap<String, String> {
    let json_text = |value: &Option<Value>| value.as_ref().map(pretty).unwrap_or_default();
    let mut values: BTreeMap<String, String> = BTreeMap::new();
    let mut put = |name: &str, value: String| {
        values.insert(name.to_string(), value);
    };
    match action {
        Action::Webhook { url, headers, body, .. } => {
            put("url", url.clone());
            put("body", json_text(body));
            if let Some(name) = headers.keys().next() {
                put("header_name", name.clone());
            }
        }
        Action::Slack { channel, text } => {
            put("channel", channel.clone().unwrap_or_default());
            put("text", text.clone());
        }
        Action::Email { to, subject, body } => {
            put("to", to.clone().unwrap_or_default());
            put("subject", subject.clone());
            put("body", body.clone());
        }
        Action::Event { topic, payload } => {
            put("topic", topic.clone());
            put("payload", json_text(payload));
        }
        Action::Request { plugin, method, route, query, body } => {
            put("plugin", plugin.clone());
            put("method", method.clone());
            put("route", route.clone());
            put("query", query.clone().unwrap_or_default());
            put("request_body", json_text(body));
        }
        Action::Operation { plugin, operation, params } => {
            put("operation", format!("{plugin}.{operation}"));
            for (name, value) in params {
                put(&format!("param-{name}"), value.clone());
            }
        }
        Action::Notify { user, title, body, url } => {
            put("user", user.clone());
            put("title", title.clone());
            put("body", body.clone());
            put("url", url.clone().unwrap_or_default());
        }
    }
    values
}

struct ActionRow {
    kind: &'static str,
    shown: String,
}

struct ConditionRow {
    field: String,
    op: &'static str,
    value: String,
}

struct RunRow {
    id: Uuid,
    created: String,
    trigger: String,
    state: String,
    badge: &'static str,
    attempts: String,
    error: String,
}

struct TriggerRow {
    received: String,
    kind: String,
    state: String,
    badge: &'static str,
    detail: String,
}

#[derive(Template)]
#[template(path = "automation.html")]
struct AutomationPage {
    flash: Flash,
    /// The section open, each a page of its own: `steps`, `test` or `runs`.
    section: &'static str,
    mine: bool,
    automation: Automation,
    resource_href: String,
    trigger: String,
    webhook: Option<String>,
    conditions: Vec<ConditionRow>,
    actions: Vec<ActionRow>,
    runs: Vec<RunRow>,
    triggers: Vec<TriggerRow>,
    /// The automation drawn as a chain, or why it could not be.
    graph: Result<String, String>,
    /// The chosen node's form.
    editor: Option<Editor>,
}

impl AutomationPage {
    fn url(&self) -> String {
        format!("/p/automation/a/{}", self.automation.id)
    }

    /// The sections: what it does, a test run for whoever may change it, and what it has done.
    fn sections(&self) -> Vec<(&'static str, &'static str, String)> {
        let url = self.url();
        let mut sections = vec![("steps", "What it does", url.clone())];
        if self.mine {
            sections.push(("test", "Test run", format!("{url}/test")));
        }
        sections.push(("runs", "Runs", format!("{url}/runs")));
        sections
    }
}

impl Editor {
    fn op_is(&self, op: &str) -> bool {
        self.op == op
    }
}

/// The form under the graph for the node that was chosen: the trigger, a condition or an action,
/// changed where it is, or one being added.
struct Editor {
    heading: String,
    /// `trigger`, `condition` or `action`, which decides the fields.
    what: &'static str,
    save: String,
    submit: &'static str,
    remove: Option<String>,
    /// A trigger's or an action's own fields, drawn by their fragment.
    fields: String,
    /// A condition's field, test and value.
    field: String,
    op: String,
    value: String,
    ops: &'static [(&'static str, &'static str)],
}

struct StepRow {
    number: usize,
    kind: String,
    state: String,
    badge: &'static str,
    detail: String,
}

#[derive(Template)]
#[template(path = "run.html")]
struct RunPage {
    flash: Flash,
    run: Run,
    automation: String,
    badge: &'static str,
    input: String,
    steps: Vec<StepRow>,
    created: String,
    finished: String,
}

#[derive(Template)]
#[template(path = "templates.html")]
struct TemplatesPage {
    flash: Flash,
    writes: bool,
    resource: String,
    templates: &'static [templates::Template],
}

pub async fn handle(
    backend: &Backend,
    engine: &Engine,
    request: &Request,
    path: &[&str],
) -> Response {
    let fragment = matches!(path, ["panel" | "fields" | "suggest" | "topics" | "queues", ..]);
    let page = route(backend, engine, request, path).await;
    match page {
        Ok(html) => Response::html(html),
        Err(refusal) if fragment => {
            let text = format!(
                "<p class=\"doc-error-message\" role=\"alert\">{}</p>",
                escape(&refusal.detail)
            );
            Response::new(refusal.status, "text/html; charset=utf-8", text)
        }
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(&refusal) };
            let html = page.render().unwrap_or_else(|_| escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

async fn route(
    backend: &Backend,
    engine: &Engine,
    request: &Request,
    path: &[&str],
) -> Result<String, Refusal> {
    let method = request.method.as_str();
    match (method, path) {
        ("GET", [] | [""]) => list(backend, query(request, "resource"), Flash::default()).await,
        ("GET", ["panel"]) => panel(backend, request).await,
        // What there is to subscribe to, for the topic picker: core reads it off the Event Bus.
        ("GET", ["topics"]) => topics(backend, request).await,
        // What else can be known before an automation is saved, for the fields' pickers.
        ("GET", ["suggest", "paths"]) => suggest::paths(backend, request).await,
        ("GET", ["suggest", "values"]) => suggest::values(backend, request).await,
        ("GET", ["suggest", "plugins"]) => suggest::plugins(backend, request).await,
        ("GET", ["suggest", "schedules"]) => suggest::schedules(request),
        // The Service Bus queues there are to listen on, for the queue picker.
        ("GET", ["queues"]) => queues(backend, request).await,
        // A kind chosen in a step's dropdown sends the dropdown's own value, under its prefix.
        ("GET", ["fields", "trigger"]) => {
            let prefix = step_prefix(request)?;
            let kind = query(request, &format!("{prefix}trigger")).unwrap_or_else(|| "cron".into());
            render(&TriggerFields::of(&kind, None).prefixed(&prefix))
        }
        ("GET", ["fields", "action"]) => {
            let prefix = step_prefix(request)?;
            let kind = query(request, &format!("{prefix}action"))
                .or_else(|| query(request, "action"))
                .unwrap_or_else(|| "slack".into());
            let values = query(request, &format!("{prefix}operation"))
                .map(|chosen| BTreeMap::from([("operation".to_string(), chosen)]))
                .unwrap_or_default();
            render(&ActionFields::of(backend, &kind, values, false).await?.prefixed(&prefix))
        }
        ("GET", ["new"]) => {
            new_page(backend, query(request, "resource").unwrap_or_default(), Flash::default())
                .await
        }
        ("POST", ["new"]) => create(backend, engine, &form(request)).await,
        // A ready-made step put on the chain: its node, and its form out of band.
        ("GET", ["new", "step"]) => builder::fragment(backend, request).await,
        ("GET", ["templates"]) => templates_page(
            backend,
            query(request, "resource").unwrap_or_default(),
            Flash::default(),
        ),
        ("POST", ["templates", name]) => apply(backend, engine, name, &form(request)).await,
        ("GET", ["a", id]) => {
            let node = query(request, "node").and_then(|node| Node::named(&node));
            detail_at(backend, id_of(id)?, "steps", node, Flash::default()).await
        }
        ("GET", ["a", id, section @ ("test" | "runs")]) => {
            let section = if *section == "test" { "test" } else { "runs" };
            detail_at(backend, id_of(id)?, section, None, Flash::default()).await
        }
        ("POST", ["a", id, change]) => {
            changed(backend, engine, id_of(id)?, change, None, &form(request)).await
        }
        // A condition or an action changed where it is, from its node's form.
        ("POST", ["a", id, list, index]) => {
            let index: usize = index.parse().map_err(|_| Refusal::missing("no such item"))?;
            changed(backend, engine, id_of(id)?, list, Some((index, false)), &form(request)).await
        }
        ("POST", ["a", id, list, index, "remove"]) => {
            let index: usize = index.parse().map_err(|_| Refusal::missing("no such item"))?;
            changed(backend, engine, id_of(id)?, list, Some((index, true)), &form(request)).await
        }
        ("GET", ["runs", id]) => run_page(backend, id).await,
        _ => Err(Refusal::missing("no such page")),
    }
}

/// The prefix a builder step's fields carry: a few of a-z and 0-9, or none.
fn step_prefix(request: &Request) -> Result<String, Refusal> {
    let prefix = query(request, "prefix").unwrap_or_default();
    match prefix.len() <= 16
        && prefix.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        true => Ok(prefix),
        false => Err(Refusal::bad("that is not a step")),
    }
}

/// One choice in the topic picker.
struct TopicChoice {
    value: String,
    kind: String,
    title: String,
}

#[derive(Template)]
#[template(path = "topics.html")]
struct TopicsFragment {
    text: String,
    found: Vec<TopicChoice>,
    more: bool,
}

#[derive(Template)]
#[template(path = "queues.html")]
struct QueuesFragment {
    text: String,
    found: Vec<TopicChoice>,
    more: bool,
}

fn times(count: i64) -> String {
    match count {
        1 => "once".to_string(),
        2 => "twice".to_string(),
        many => format!("{many} times"),
    }
}

/// The queue picker's options: the queues plugins have sent to over the Service Bus and those
/// automations listen on, which are the only ones there are, since the bus keeps no list.
async fn queues(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    const SHOWN: usize = 20;
    let named = query(request, "field").unwrap_or_else(|| "queue".to_string());
    let text = query(request, "q")
        .or_else(|| query(request, &named))
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    let store = Store(backend);
    let mut known: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (queue, sender, received) in store.queues().await? {
        known.entry(queue).or_default().push(format!("sent {}, last by {sender}", times(received)));
    }
    let mut listening: BTreeMap<String, usize> = BTreeMap::new();
    for (_, queue) in store.triggered_by("queue").await? {
        *listening.entry(queue).or_default() += 1;
    }
    for (queue, count) in listening {
        let said = match count {
            1 => "an automation listens on it".to_string(),
            many => format!("{many} automations listen on it"),
        };
        known.entry(queue).or_default().push(said);
    }
    let shaped = |queue: &str| {
        !queue.is_empty()
            && queue.len() <= 64
            && queue.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };
    let mut found: Vec<(u32, TopicChoice)> = known
        .into_iter()
        .filter(|(queue, _)| shaped(queue))
        .filter_map(|(queue, said)| {
            let close = topic_closeness(&queue, &text)?;
            Some((
                close,
                TopicChoice { value: queue, kind: "queue".into(), title: said.join("; ") },
            ))
        })
        .collect();
    found.sort_by(|(a_close, a), (b_close, b)| a_close.cmp(b_close).then(a.value.cmp(&b.value)));
    let more = found.len() > SHOWN;
    let found = found.into_iter().take(SHOWN).map(|(_, choice)| choice).collect();
    render(&QueuesFragment { text, found, more })
}

/// How well a topic answers what has been typed, lower being better: what it starts with, then a
/// segment of it, then anywhere in it. Dots are ignored, so `kbdoc` finds `plugin.kb.document.*`.
fn topic_closeness(topic: &str, typed: &str) -> Option<u32> {
    if typed.is_empty() {
        return Some(50);
    }
    let bare = |text: &str| text.replace(['.', '-', '*', '>'], "");
    match () {
        _ if topic == typed => Some(0),
        _ if topic.starts_with(typed) => Some(10),
        _ if topic.split('.').any(|segment| segment.starts_with(typed)) => Some(20),
        _ if topic.contains(typed) => Some(30),
        _ if bare(topic).contains(&bare(typed)) => Some(40),
        _ => None,
    }
}

/// The topic picker's options: the topics this platform has published that match what has been
/// typed, and a `>` filter for each group of them, since a trigger may take either.
async fn topics(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    const SHOWN: usize = 20;
    let named = query(request, "field").unwrap_or_else(|| "topic".to_string());
    let text = query(request, "q")
        .or_else(|| query(request, &named))
        .unwrap_or_default()
        .trim()
        .to_lowercase();
    // A field that names part of a topic, such as an event action's, says what it sits under;
    // what it offers is then what is published there, without the part it does not carry.
    let under = query(request, "under").unwrap_or_default();
    let prefix = match under.is_empty() {
        true => String::new(),
        false => format!("{under}."),
    };
    let published = Store(backend).topics().await?;
    let mut found: Vec<(u32, TopicChoice)> = Vec::new();
    let mut groups: BTreeMap<String, usize> = BTreeMap::new();
    for (topic, count) in &published {
        let Some(topic) = topic.strip_prefix(&prefix).map(str::to_string) else { continue };
        let topic = &topic;
        let Some(close) = topic_closeness(topic, &text) else { continue };
        if !prefix.is_empty() {
            let title = match count {
                0 => "never published".to_string(),
                1 => "published once".to_string(),
                many => format!("published {many} times"),
            };
            found.push((close, TopicChoice { value: topic.clone(), kind: "topic".into(), title }));
            continue;
        }
        // `plugin.kb.document.imported` belongs to the group `plugin.kb`, which is the plugin
        // that publishes it; the platform's own topics group under `platform`.
        let segments: Vec<&str> = topic.split('.').collect();
        let group = match segments.as_slice() {
            ["plugin", plugin, ..] => format!("plugin.{plugin}"),
            [first, ..] => (*first).to_string(),
            [] => continue,
        };
        *groups.entry(group).or_default() += 1;
        let title = match count {
            0 => "never published".to_string(),
            1 => "published once".to_string(),
            many => format!("published {many} times"),
        };
        found.push((close, TopicChoice { value: topic.clone(), kind: "topic".into(), title }));
    }
    for group in groups.into_iter().filter(|(_, matched)| *matched > 1).map(|(group, _)| group) {
        let filter = format!("{group}.>");
        let close = topic_closeness(&filter, &text).unwrap_or(45);
        let title = format!("everything under {group}, including topics not published yet");
        found.push((close + 5, TopicChoice { value: filter, kind: "filter".into(), title }));
    }
    found.sort_by(|(a_close, a), (b_close, b)| a_close.cmp(b_close).then(a.value.cmp(&b.value)));
    let more = found.len() > SHOWN;
    let found = found.into_iter().take(SHOWN).map(|(_, choice)| choice).collect();
    render(&TopicsFragment { text, found, more })
}

fn row(automation: &Automation, latest: &BTreeMap<Uuid, (String, String)>) -> Row {
    Row {
        id: automation.id,
        name: automation.name.clone(),
        resource: automation.resource.clone(),
        resource_href: resource_href(&automation.resource),
        trigger: trigger_text(&automation.trigger),
        owner: automation.owner_label.clone(),
        enabled: automation.enabled,
        last: latest.get(&automation.id).map(|(state, at)| (state.clone(), badge(state), when(at))),
    }
}

async fn rows(backend: &Backend, resource: Option<&str>) -> Result<Vec<Row>, Refusal> {
    let store = Store(backend);
    let latest = store.latest_runs().await?;
    Ok(store
        .automations(resource)
        .await?
        .iter()
        .map(|automation| row(automation, &latest))
        .collect())
}

async fn list(
    backend: &Backend,
    resource: Option<String>,
    flash: Flash,
) -> Result<String, Refusal> {
    let resource = resource.map(|resource| normalised(&resource)).transpose()?;
    let rows = rows(backend, resource.as_deref()).await?;
    render(&ListPage {
        flash,
        writes: backend.writes(),
        resource: resource.unwrap_or_default(),
        rows,
    })
}

async fn panel(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let resource = query(request, "resource").ok_or_else(|| Refusal::bad("name the resource"))?;
    let resource = normalised(&resource)?;
    let rows = rows(backend, Some(&resource)).await?;
    render(&PanelFragment { writes: backend.writes(), resource, rows })
}

async fn new_page(backend: &Backend, resource: String, flash: Flash) -> Result<String, Refusal> {
    render(&NewPage {
        flash,
        writes: backend.writes(),
        name: String::new(),
        resource,
        palette: builder::palette(backend).await?,
        when: Vec::new(),
        ifs: Vec::new(),
        thens: Vec::new(),
        ops: &OPS,
    })
}

fn json_field(form: &Form, name: &str, what: &str) -> Result<Option<Value>, Refusal> {
    field(form, name)
        .map(|text| {
            serde_json::from_str(&text)
                .map_err(|err| Refusal::bad(format!("the {what} is not JSON: {err}")))
        })
        .transpose()
}

fn trigger_from(form: &Form) -> Result<Trigger, Refusal> {
    let value = |name: &str, what: &str| {
        field(form, name).ok_or_else(|| Refusal::bad(format!("this trigger needs {what}")))
    };
    match field(form, "trigger").as_deref() {
        Some("cron") => Ok(Trigger::Cron { cron: value("cron", "a cron schedule")? }),
        Some("event") => Ok(Trigger::Event { topic: value("topic", "a topic")? }),
        Some("queue") => Ok(Trigger::Queue { queue: value("queue", "a queue name")? }),
        Some("webhook") => Ok(Trigger::Webhook),
        _ => Err(Refusal::bad("choose a trigger: cron, event, queue or webhook")),
    }
}

fn action_from(form: &Form) -> Result<Action, Refusal> {
    let text = |name: &str| field(form, name).unwrap_or_default();
    match field(form, "action").as_deref() {
        Some("webhook") => {
            let headers = match (field(form, "header_name"), field(form, "header_value")) {
                (Some(name), Some(value)) => [(name, value)].into_iter().collect(),
                _ => Default::default(),
            };
            Ok(Action::Webhook {
                url: text("url"),
                headers,
                body: json_field(form, "body", "body")?,
                secret: field(form, "secret").map(Secret::new),
            })
        }
        Some("slack") => Ok(Action::Slack { channel: field(form, "channel"), text: text("text") }),
        Some("email") => Ok(Action::Email {
            to: field(form, "to"),
            subject: text("subject"),
            body: text("body"),
        }),
        Some("event") => Ok(Action::Event {
            topic: text("topic"),
            payload: json_field(form, "payload", "payload")?,
        }),
        Some("request") => Ok(Action::Request {
            plugin: text("plugin"),
            method: field(form, "method").unwrap_or_else(|| "GET".into()),
            route: text("route"),
            query: field(form, "query"),
            body: json_field(form, "request_body", "body")?,
        }),
        Some("notify") => Ok(Action::Notify {
            user: text("user"),
            title: text("title"),
            body: text("body"),
            url: field(form, "url"),
        }),
        Some("operation") => {
            let chosen = text("operation");
            let (plugin, operation) =
                chosen.split_once('.').ok_or_else(|| Refusal::bad("choose an operation"))?;
            let params = form
                .iter()
                .filter_map(|(name, value)| {
                    let name = name.strip_prefix("param-")?;
                    let value = value.trim();
                    (!value.is_empty()).then(|| (name.to_string(), value.to_string()))
                })
                .collect();
            Ok(Action::Operation { plugin: plugin.into(), operation: operation.into(), params })
        }
        _ => Err(Refusal::bad(
            "choose an action: webhook, slack, email, event, operation, request or notify",
        )),
    }
}

fn condition_from(form: &Form) -> Result<Option<Condition>, Refusal> {
    let Some(on) = field(form, "field") else { return Ok(None) };
    let op = field(form, "op").unwrap_or_else(|| "eq".into());
    let op: Op = serde_json::from_value(json!(op))
        .map_err(|_| Refusal::bad(format!("`{op}` is not a test")))?;
    let value = field(form, "value")
        .map(|text| serde_json::from_str(&text).unwrap_or(Value::String(text)))
        .unwrap_or(Value::Null);
    Ok(Some(Condition { field: on, op, value }))
}

fn made(saved: &Saved, what: &str) -> Flash {
    match &saved.secret {
        Some(secret) => Flash::done(format!(
            "{what} Its webhook is {} and its secret, shown only now, is {}. Sign each body with it as x-doc-signature: sha256=<HMAC-SHA256 in hex>.",
            api::hook_url(saved.automation.id),
            secret.expose()
        )),
        None => Flash::done(what),
    }
}

async fn create(backend: &Backend, engine: &Engine, form: &Form) -> Result<String, Refusal> {
    let name = field(form, "name").unwrap_or_default();
    let resource = field(form, "resource").unwrap_or_default();
    let parsed = match builder::parse(form) {
        Ok((trigger, conditions, actions)) => {
            builder::operations_checked(backend, &actions).await.map(|()| {
                (trigger, conditions, actions.into_iter().map(|(action, _)| action).collect())
            })
        }
        Err(refused) => Err(refused),
    };
    let saved = match parsed {
        Ok((trigger, conditions, actions)) => {
            let asked = NewAutomation {
                name: name.clone(),
                resource: resource.clone(),
                trigger,
                conditions,
                actions,
                enabled: true,
            };
            api::create(backend, engine, asked, None).await.map_err(|refusal| (refusal, None))
        }
        Err(refused) => Err(refused),
    };
    match saved {
        Ok(saved) => detail(backend, saved.automation.id, made(&saved, "Created.")).await,
        // The chain comes back as it was sent, with the step that was refused open.
        Err((refusal, failed)) => {
            let (when, ifs, thens) = builder::steps(backend, form, failed.as_deref()).await?;
            render(&NewPage {
                flash: Flash::refused(&refusal),
                writes: backend.writes(),
                name,
                resource,
                palette: builder::palette(backend).await?,
                when,
                ifs,
                thens,
                ops: &OPS,
            })
        }
    }
}

async fn apply(
    backend: &Backend,
    engine: &Engine,
    name: &str,
    form: &Form,
) -> Result<String, Refusal> {
    let asked = FromTemplate {
        resource: field(form, "resource").unwrap_or_default(),
        name: field(form, "name"),
        to: field(form, "to"),
    };
    let resource = asked.resource.clone();
    match api::from_template(backend, engine, name, asked).await {
        Ok(saved) => {
            detail(backend, saved.automation.id, made(&saved, "Made from the template.")).await
        }
        Err(refusal) => templates_page(backend, resource, Flash::refused(&refusal)),
    }
}

/// One change from a form on the automation's page, answered with the page as it now is.
async fn changed(
    backend: &Backend,
    engine: &Engine,
    id: Uuid,
    change: &str,
    index: Option<(usize, bool)>,
    form: &Form,
) -> Result<String, Refusal> {
    let automation = api::found(backend, id).await?;
    if change == "delete" {
        api::remove(backend, engine, id).await?;
        let notice = format!(
            "Deleted {}; it can no longer act as {}.",
            automation.name, automation.owner_label
        );
        return list(backend, None, Flash::done(notice)).await;
    }
    let outcome: Result<Flash, Refusal> = async {
        match (change, index) {
            ("conditions", None) => {
                let condition = condition_from(form)?.ok_or_else(|| Refusal::bad("name the field the condition tests"))?;
                let mut conditions = automation.conditions.clone();
                conditions.push(condition);
                let asked = Change { conditions: Some(conditions), ..Change::default() };
                api::change(backend, engine, id, asked).await.map(|_| Flash::done("Condition added."))
            }
            ("conditions", Some((index, true))) => {
                let mut conditions = automation.conditions.clone();
                if index < conditions.len() {
                    conditions.remove(index);
                }
                let asked = Change { conditions: Some(conditions), ..Change::default() };
                api::change(backend, engine, id, asked).await.map(|_| Flash::done("Condition removed."))
            }
            ("conditions", Some((index, false))) => {
                let condition = condition_from(form)?.ok_or_else(|| Refusal::bad("name the field the condition tests"))?;
                let mut conditions = automation.conditions.clone();
                let held = conditions.get_mut(index).ok_or_else(|| Refusal::missing("there is no such condition"))?;
                *held = condition;
                let asked = Change { conditions: Some(conditions), ..Change::default() };
                api::change(backend, engine, id, asked).await.map(|_| Flash::done("Condition changed."))
            }
            ("actions", None) => {
                let mut actions = automation.actions.clone();
                actions.push(action_from(form)?);
                let asked = Change { actions: Some(actions), ..Change::default() };
                api::change(backend, engine, id, asked).await.map(|_| Flash::done("Action added."))
            }
            ("actions", Some((index, true))) => {
                let mut actions = automation.actions.clone();
                if index < actions.len() {
                    actions.remove(index);
                }
                let asked = Change { actions: Some(actions), ..Change::default() };
                api::change(backend, engine, id, asked).await.map(|_| Flash::done("Action removed."))
            }
            ("actions", Some((index, false))) => {
                let mut action = action_from(form)?;
                let mut actions = automation.actions.clone();
                let held = actions.get_mut(index).ok_or_else(|| Refusal::missing("there is no such action"))?;
                // A webhook's secret and header value are never shown, so leaving them empty keeps
                // what it had.
                if let (
                    Action::Webhook { secret, headers, .. },
                    Action::Webhook { secret: kept_secret, headers: kept_headers, .. },
                ) = (&mut action, &*held)
                {
                    if secret.is_none() {
                        secret.clone_from(kept_secret);
                    }
                    if let (true, Some(name), None) =
                        (headers.is_empty(), field(form, "header_name"), field(form, "header_value"))
                        && let Some(value) = kept_headers.get(&name)
                    {
                        headers.insert(name, value.clone());
                    }
                }
                *held = action;
                let asked = Change { actions: Some(actions), ..Change::default() };
                api::change(backend, engine, id, asked).await.map(|_| Flash::done("Action changed."))
            }
            ("trigger", None) => {
                let asked = Change { trigger: Some(trigger_from(form)?), ..Change::default() };
                let saved = api::change(backend, engine, id, asked).await?;
                Ok(made(&saved, "Trigger changed."))
            }
            ("enabled", None) => {
                let on = field(form, "enabled").as_deref() == Some("true");
                let asked = Change { enabled: Some(on), ..Change::default() };
                let notice = if on { "Turned on." } else { "Turned off: nothing triggers it now." };
                api::change(backend, engine, id, asked).await.map(|_| Flash::done(notice))
            }
            ("run", None) => {
                let payload = json_field(form, "payload", "payload")?.unwrap_or_else(|| json!({}));
                let force = field(form, "force").is_some();
                match api::run_now(backend, id, payload, force).await? {
                    Ok(run) => Ok(Flash::done(format!("Test run {} queued; its outcome is in the history below.", run.id))),
                    Err(reason) => Ok(Flash {
                        notice: None,
                        error: Some(format!("It would not run: {reason}. Tick \"run anyway\" to ignore the conditions.")),
                    }),
                }
            }
            ("secret", None) => {
                let fresh = api::rotate(backend, id).await?;
                Ok(Flash::done(format!("New secret, shown only now: {}. The old one no longer works.", fresh.expose())))
            }
            _ => Err(Refusal::missing("no such change")),
        }
    }
    .await;
    // A test run is followed in the runs, or tried again where it was asked for.
    let section = match (change, &outcome) {
        ("run", Ok(Flash { error: None, .. })) => "runs",
        ("run", _) => "test",
        _ => "steps",
    };
    let flash = outcome.unwrap_or_else(|refusal| Flash::refused(&refusal));
    detail_at(backend, id, section, None, flash).await
}

fn action_row(action: &Action, people: &BTreeMap<String, String>) -> ActionRow {
    let mut shown = api::masked(action);
    if let Some(object) = shown.as_object_mut() {
        object.remove("type");
        if let Some(login) =
            object.get("user").and_then(Value::as_str).and_then(|id| people.get(id))
        {
            object.insert("user".into(), json!(login));
        }
    }
    ActionRow { kind: action.kind(), shown: pretty(&shown) }
}

async fn detail(backend: &Backend, id: Uuid, flash: Flash) -> Result<String, Refusal> {
    detail_at(backend, id, "steps", None, flash).await
}

/// The logins of the DOC users the actions notify, by ID, so nothing shows an ID where a person
/// is meant. A user that is a template, or unknown, is left as it is.
async fn people(backend: &Backend, actions: &[Action]) -> BTreeMap<String, String> {
    let mut people = BTreeMap::new();
    for action in actions {
        let Action::Notify { user, .. } = action else { continue };
        if people.contains_key(user) || Uuid::parse_str(user).is_err() {
            continue;
        }
        let query = doc_plugin_sdk::Query::new("core.users").filter(json!({ "id": user }));
        let found = Store(backend).core(query).await.unwrap_or_default();
        if let Some(login) = found.first().and_then(|person| person["login"].as_str()) {
            people.insert(user.clone(), login.to_string());
        }
    }
    people
}

/// The automation as the Service Map draws it, or why it could not be.
async fn drawn(
    backend: &Backend,
    automation: &Automation,
    node: Option<Node>,
    mine: bool,
    people: &BTreeMap<String, String>,
) -> Result<String, String> {
    let drawing = graph::drawing(automation, node, mine, |condition| op_text(condition.op), people);
    match backend.discovery("service-map", "POST", "draw", None, Some(drawing)).await {
        Ok((200, answer)) => answer["svg"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| "the Service Map answered without a drawing".to_string()),
        Ok((status, answer)) => Err(format!(
            "the Service Map answered {status}: {}",
            answer["detail"].as_str().unwrap_or("it did not say why")
        )),
        Err(err) => Err(format!("the Service Map could not be asked: {err}")),
    }
}

async fn editor(
    backend: &Backend,
    automation: &Automation,
    node: Node,
) -> Result<Option<Editor>, Refusal> {
    let base = format!("/p/automation/a/{}", automation.id);
    let blank = |heading: String, what: &'static str, save: String, submit: &'static str| Editor {
        heading,
        what,
        save,
        submit,
        remove: None,
        fields: String::new(),
        field: String::new(),
        op: "eq".into(),
        value: String::new(),
        ops: &OPS,
    };
    let editor = match node {
        Node::Trigger => {
            let kind = automation.trigger.kind();
            Editor {
                fields: render(&TriggerFields::of(kind, Some(&automation.trigger)))?,
                ..blank("When".into(), "trigger", format!("{base}/trigger"), "Change the trigger")
            }
        }
        Node::Condition(at) => {
            let Some(condition) = automation.conditions.get(at) else { return Ok(None) };
            let op = serde_json::to_value(condition.op)
                .ok()
                .and_then(|op| op.as_str().map(str::to_string));
            Editor {
                remove: Some(format!("{base}/conditions/{at}/remove")),
                field: condition.field.clone(),
                op: op.unwrap_or_else(|| "eq".into()),
                value: match &condition.value {
                    Value::String(text) => text.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                },
                ..blank(
                    if at == 0 { "If".into() } else { "And if".into() },
                    "condition",
                    format!("{base}/conditions/{at}"),
                    "Change the condition",
                )
            }
        }
        Node::AddCondition => blank(
            "Add a condition".into(),
            "condition",
            format!("{base}/conditions"),
            "Add the condition",
        ),
        Node::Action(at) => {
            let Some(action) = automation.actions.get(at) else { return Ok(None) };
            let fields =
                ActionFields::of(backend, action.kind(), action_values(action), true).await?;
            Editor {
                remove: (automation.actions.len() > 1)
                    .then(|| format!("{base}/actions/{at}/remove")),
                fields: render(&fields)?,
                ..blank(
                    format!("Then, step {}", at + 1),
                    "action",
                    format!("{base}/actions/{at}"),
                    "Change the action",
                )
            }
        }
        Node::AddAction => {
            let fields = ActionFields::of(backend, "slack", BTreeMap::new(), false).await?;
            Editor {
                fields: render(&fields)?,
                ..blank(
                    "Add an action".into(),
                    "action",
                    format!("{base}/actions"),
                    "Add the action",
                )
            }
        }
    };
    Ok(Some(editor))
}

async fn detail_at(
    backend: &Backend,
    id: Uuid,
    section: &'static str,
    node: Option<Node>,
    flash: Flash,
) -> Result<String, Refusal> {
    let automation = api::found(backend, id).await?;
    let mine = may_change(backend, &automation);
    let node = node.filter(|_| mine);
    let store = Store(backend);
    let runs = store
        .runs(id, 20)
        .await?
        .into_iter()
        .map(|run| RunRow {
            id: run.id,
            created: when(&run.created_at),
            trigger: run.input["trigger"].as_str().unwrap_or("?").to_string(),
            badge: badge(&run.state),
            state: run.state.clone(),
            attempts: format!("{} of {}", run.attempts, run.max_attempts),
            error: run.error.clone().unwrap_or_default(),
        })
        .collect();
    let triggers = store
        .triggers(id, 10)
        .await?
        .into_iter()
        .map(|trigger| {
            let state = trigger["state"].as_str().unwrap_or_default().to_string();
            TriggerRow {
                received: when(trigger["received_at"].as_str().unwrap_or_default()),
                kind: trigger["kind"].as_str().unwrap_or_default().to_string(),
                badge: badge(&state),
                state,
                detail: trigger["detail"].as_str().unwrap_or_default().to_string(),
            }
        })
        .collect();
    let editor = match node {
        Some(node) => editor(backend, &automation, node).await?,
        None => None,
    };
    let people = people(backend, &automation.actions).await;
    render(&AutomationPage {
        flash,
        section,
        mine,
        resource_href: resource_href(&automation.resource),
        trigger: trigger_text(&automation.trigger),
        webhook: matches!(automation.trigger, Trigger::Webhook)
            .then(|| api::hook_url(automation.id)),
        conditions: automation
            .conditions
            .iter()
            .map(|condition| ConditionRow {
                field: condition.field.clone(),
                op: op_text(condition.op),
                value: match &condition.value {
                    Value::String(text) => text.clone(),
                    Value::Null => String::new(),
                    other => other.to_string(),
                },
            })
            .collect(),
        actions: automation.actions.iter().map(|action| action_row(action, &people)).collect(),
        runs,
        triggers,
        graph: drawn(backend, &automation, node, mine, &people).await,
        editor,
        automation,
    })
}

async fn run_page(backend: &Backend, id: &str) -> Result<String, Refusal> {
    let id: Uuid = id.parse().map_err(|_| Refusal::missing("there is no such run"))?;
    let run =
        Store(backend).run(id).await?.ok_or_else(|| Refusal::missing("there is no such run"))?;
    let automation = Store(backend)
        .automation(run.automation)
        .await?
        .map(|automation| automation.name)
        .unwrap_or_default();
    let steps = run
        .steps
        .iter()
        .enumerate()
        .map(|(index, step)| {
            let state = step["state"].as_str().unwrap_or("not run").to_string();
            let detail = match step.get("error").and_then(Value::as_str) {
                Some(error) => error.to_string(),
                None => pretty(&step["output"]),
            };
            StepRow {
                number: index + 1,
                kind: step["action"].as_str().unwrap_or("?").to_string(),
                badge: badge(&state),
                state,
                detail,
            }
        })
        .collect();
    render(&RunPage {
        flash: Flash::default(),
        badge: badge(&run.state),
        input: pretty(&run.input),
        steps,
        created: when(&run.created_at),
        finished: run.finished_at.as_deref().map(when).unwrap_or_default(),
        automation,
        run,
    })
}

fn templates_page(backend: &Backend, resource: String, flash: Flash) -> Result<String, Refusal> {
    render(&TemplatesPage { flash, writes: backend.writes(), resource, templates: &templates::ALL })
}
