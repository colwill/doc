//! The plugin's routes: automations, their runs and templates as JSON, each automation's signed
//! webhook under `public/hooks/`, and the queues other plugins send to under `discovery/queues/`.

use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::{Backend, Caller, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use url::form_urlencoded::byte_serialize;
use uuid::Uuid;

use crate::actions::{setting, signature};
use crate::conditions::{context, first_failing};
use crate::model::{Action, Condition, Trigger, checked_actions, checked_conditions};
use crate::operations;
use crate::store::{Automation, Run, Store};
use crate::{Engine, Refusal, engine, templates};

type Answer = Result<(u16, Value), Refusal>;

pub const MAX_LISTED: i64 = 200;
const MAX_HOOK_BODY: usize = 1024 * 1024;

pub fn query(request: &Request, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn limit(request: &Request) -> i64 {
    query(request, "limit").and_then(|limit| limit.parse().ok()).unwrap_or(50).clamp(1, MAX_LISTED)
}

fn body<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T, Refusal> {
    let bytes = if request.body.is_empty() { &b"{}"[..] } else { &request.body[..] };
    serde_json::from_slice(bytes)
        .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))
}

pub fn id_of(text: &str) -> Result<Uuid, Refusal> {
    text.parse().map_err(|_| Refusal::missing("there is no such automation"))
}

/// `user:<id>` or `service:<id>`, for a person or service account; nobody else owns automations.
fn owner_of(backend: &Backend) -> Result<(String, String), Refusal> {
    match backend.caller() {
        Some(Caller { kind, id: Some(id), label, .. }) if kind == "user" || kind == "service" => {
            Ok((format!("{kind}:{id}"), label.clone().unwrap_or_else(|| id.clone())))
        }
        _ => Err(Refusal::forbidden("automations belong to a person or a service account")),
    }
}

pub fn is_owner(backend: &Backend, automation: &Automation) -> bool {
    owner_of(backend).is_ok_and(|(owner, _)| owner == automation.owner)
}

/// Whether the caller may change, run or delete it: its owner, since it acts as them, or an admin.
pub fn may_change(backend: &Backend, automation: &Automation) -> bool {
    is_owner(backend, automation) || backend.caller().is_some_and(|caller| caller.admin)
}

fn owned(backend: &Backend, automation: &Automation) -> Result<(), Refusal> {
    match may_change(backend, automation) {
        true => Ok(()),
        false => Err(Refusal::forbidden(format!(
            "only {}, who owns this automation, can change or run it",
            automation.owner_label
        ))),
    }
}

/// Where the backend's API is reached from outside, so a webhook's URL can be given in full.
pub fn hook_url(id: Uuid) -> String {
    let base = setting("DOC_API_PUBLIC_URL").unwrap_or_default();
    format!("{}/api/v1/plugins/automation/public/hooks/{id}", base.trim_end_matches('/'))
}

fn secret() -> Result<Secret<String>, Refusal> {
    let mut bytes = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut bytes)
        .map_err(|_| Refusal::unavailable("no randomness for a secret"))?;
    Ok(Secret::new(hex::encode(bytes)))
}

/// An action as anyone who can read the plugin sees it: header values and secrets are hidden.
pub fn masked(action: &Action) -> Value {
    let mut shown = json!(action);
    if let Some(headers) = shown["headers"].as_object_mut() {
        headers.values_mut().for_each(|value| *value = json!("(hidden)"));
    }
    if shown["secret"].is_string() {
        shown["secret"] = json!("(hidden)");
    }
    if let Some(fields) = shown.as_object_mut() {
        fields.retain(|_, value| {
            !value.is_null() && value.as_object().is_none_or(|map| !map.is_empty())
        });
    }
    shown
}

fn shown(automation: &Automation, backend: &Backend) -> Value {
    json!({
        "id": automation.id,
        "name": automation.name,
        "resource": automation.resource,
        "owner": automation.owner_label,
        "yours": is_owner(backend, automation),
        "trigger": automation.trigger,
        "conditions": automation.conditions,
        "actions": automation.actions.iter().map(masked).collect::<Vec<_>>(),
        "enabled": automation.enabled,
        "template": automation.template,
        "webhook": matches!(automation.trigger, Trigger::Webhook).then(|| hook_url(automation.id)),
        "last_fired_at": automation.last_fired_at,
        "created_at": automation.created_at,
        "updated_at": automation.updated_at,
    })
}

/// `kind:name` with the kind in one spelling, without asking whether it exists.
pub fn normalised(text: &str) -> Result<String, Refusal> {
    match text.trim().split_once(':') {
        Some((kind, name)) if !name.trim().is_empty() => {
            let kind: String = kind
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase();
            match kind.is_empty() {
                true => Err(Refusal::bad(format!("write the resource as kind:name, not `{text}`"))),
                false => Ok(format!("{kind}:{}", name.trim())),
            }
        }
        _ => Err(Refusal::bad(format!(
            "write the resource as kind:name, such as service:card-gateway, not `{text}`"
        ))),
    }
}

/// The resource as the caller can see it in Resource Definitions.
async fn resource_of(backend: &Backend, text: &str) -> Result<String, Refusal> {
    let resource = normalised(text)?;
    let (kind, name) = resource.split_once(':').unwrap_or_default();
    let path: Vec<String> = name
        .split('/')
        .map(|part| byte_serialize(part.as_bytes()).collect::<String>().replace('+', "%20"))
        .collect();
    let route = format!("resources/{kind}/{}", path.join("/"));
    match backend.ask("resources", "GET", &route, None, None).await {
        Ok((200, _)) => Ok(resource),
        Ok((status @ (403 | 404), answer)) => Err(Refusal {
            status,
            detail: format!(
                "{resource} is not a resource you can see in Resource Definitions: {}",
                answer["detail"].as_str().unwrap_or("not found")
            ),
        }),
        Ok((status, _)) => {
            Err(Refusal::unavailable(format!("Resource Definitions answered {status}")))
        }
        Err(err) => {
            Err(Refusal::unavailable(format!("Resource Definitions could not be asked: {err}")))
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewAutomation {
    pub name: String,
    pub resource: String,
    pub trigger: Trigger,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    pub actions: Vec<Action>,
    #[serde(default = "yes")]
    pub enabled: bool,
}

fn yes() -> bool {
    true
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Change {
    pub name: Option<String>,
    pub trigger: Option<Trigger>,
    pub conditions: Option<Vec<Condition>>,
    pub actions: Option<Vec<Action>>,
    pub enabled: Option<bool>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
struct RunNow {
    payload: Value,
    force: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FromTemplate {
    pub resource: String,
    #[serde(default)]
    pub name: Option<String>,
    /// A Slack channel or email address other than the team's own.
    #[serde(default)]
    pub to: Option<String>,
}

fn checked_name(name: &str) -> Result<String, Refusal> {
    let name = name.trim();
    match name.is_empty() || name.chars().count() > 120 {
        true => Err(Refusal::bad("an automation's name is 1 to 120 characters")),
        false => Ok(name.to_string()),
    }
}

/// An automation, and its webhook's secret when that was just made.
pub struct Saved {
    pub automation: Automation,
    pub secret: Option<Secret<String>>,
}

impl Saved {
    fn shown(&self, backend: &Backend) -> Value {
        let mut shown = shown(&self.automation, backend);
        if let Some(secret) = &self.secret {
            shown["secret"] = json!(secret.expose());
        }
        shown
    }
}

pub async fn found(backend: &Backend, id: Uuid) -> Result<Automation, Refusal> {
    Store(backend)
        .automation(id)
        .await?
        .ok_or_else(|| Refusal::missing("there is no such automation"))
}

/// Makes an automation owned by the caller, who lets it run as them from now on.
pub async fn create(
    backend: &Backend,
    engine: &Engine,
    asked: NewAutomation,
    template: Option<&str>,
) -> Result<Saved, Refusal> {
    let (owner, owner_label) = owner_of(backend)?;
    let name = checked_name(&asked.name)?;
    asked.trigger.checked()?;
    checked_conditions(&asked.conditions)?;
    checked_actions(&asked.actions)?;
    operations::checked(backend, &asked.actions).await?;
    let resource = resource_of(backend, &asked.resource).await?;
    let id = Uuid::now_v7();
    let delegation = backend.delegate(&format!("automation {id} ({name}) on {resource}")).await?;
    let secret = matches!(asked.trigger, Trigger::Webhook).then(secret).transpose()?;
    let now = chrono::Utc::now().to_rfc3339();
    let automation = Automation {
        id,
        name,
        resource,
        owner,
        owner_label,
        delegation,
        trigger: asked.trigger,
        conditions: asked.conditions,
        actions: asked.actions,
        enabled: asked.enabled,
        secret: secret.clone(),
        template: template.map(str::to_string),
        last_fired_at: None,
        created_at: now.clone(),
        updated_at: now,
    };
    Store(backend).insert(&automation).await?;
    let subject = format!("automation:{id}");
    let _ =
        backend.audit("created", Some(&subject), json!({ "resource": automation.resource })).await;
    engine.changed();
    Ok(Saved { automation, secret })
}

pub async fn change(
    backend: &Backend,
    engine: &Engine,
    id: Uuid,
    change: Change,
) -> Result<Saved, Refusal> {
    let mut automation = found(backend, id).await?;
    owned(backend, &automation)?;
    if let Some(name) = change.name {
        automation.name = checked_name(&name)?;
    }
    if let Some(trigger) = change.trigger {
        trigger.checked()?;
        automation.trigger = trigger;
    }
    if let Some(conditions) = change.conditions {
        checked_conditions(&conditions)?;
        automation.conditions = conditions;
    }
    if let Some(actions) = change.actions {
        checked_actions(&actions)?;
        operations::checked(backend, &actions).await?;
        automation.actions = actions;
    }
    if let Some(enabled) = change.enabled {
        automation.enabled = enabled;
    }
    let fresh = match (&automation.trigger, &automation.secret) {
        (Trigger::Webhook, None) => Some(secret()?),
        _ => None,
    };
    if fresh.is_some() {
        automation.secret.clone_from(&fresh);
    }
    Store(backend).update(&automation).await?;
    engine.changed();
    Ok(Saved { automation, secret: fresh })
}

/// Deletes it and ends its leave to act as its owner.
pub async fn remove(backend: &Backend, engine: &Engine, id: Uuid) -> Result<Automation, Refusal> {
    let automation = found(backend, id).await?;
    owned(backend, &automation)?;
    backend.revoke(automation.delegation).await?;
    Store(backend).delete(automation.id).await?;
    let _ = backend.audit("deleted", Some(&format!("automation:{id}")), json!({})).await;
    engine.changed();
    Ok(automation)
}

/// A run asked for by hand: the conditions decide, unless `force` says to run anyway.
pub async fn run_now(
    backend: &Backend,
    id: Uuid,
    payload: Value,
    force: bool,
) -> Result<Result<Run, String>, Refusal> {
    let automation = found(backend, id).await?;
    owned(backend, &automation)?;
    let by = backend.caller().and_then(|caller| caller.label.clone()).unwrap_or_default();
    let input = json!({ "trigger": "manual", "by": by, "payload": payload });
    if let Some(failed) =
        first_failing(&automation.conditions, &context(&input, &automation)).filter(|_| !force)
    {
        let op = serde_json::to_value(failed.op).unwrap_or_default();
        let op = op.as_str().unwrap_or_default();
        return Ok(Err(format!(
            "the condition {} {op} {} did not hold",
            failed.field, failed.value
        )));
    }
    match engine::start(backend, &automation, None, input, true).await {
        Ok(run) => Ok(Ok(run)),
        Err((_, reason)) => Err(Refusal::forbidden(reason)),
    }
}

pub async fn rotate(backend: &Backend, id: Uuid) -> Result<Secret<String>, Refusal> {
    let mut automation = found(backend, id).await?;
    owned(backend, &automation)?;
    if !matches!(automation.trigger, Trigger::Webhook) {
        return Err(Refusal::bad("only an automation triggered by a webhook has a secret"));
    }
    let fresh = secret()?;
    automation.secret = Some(fresh.clone());
    Store(backend).update(&automation).await?;
    Ok(fresh)
}

pub async fn from_template(
    backend: &Backend,
    engine: &Engine,
    name: &str,
    asked: FromTemplate,
) -> Result<Saved, Refusal> {
    let template = templates::named(name)
        .ok_or_else(|| Refusal::missing(format!("there is no template {name}")))?;
    let resource = normalised(&asked.resource)?;
    let kind = resource.split(':').next().unwrap_or_default();
    if !template.kinds.contains(&kind) {
        let suits = template.kinds.join(", ");
        return Err(Refusal::bad(format!("{} suits {suits}, not {kind}", template.title)));
    }
    let to = asked.to.filter(|to| !to.trim().is_empty());
    let (trigger, conditions, actions) = template.parts(to);
    let name = asked
        .name
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| format!("{} for {resource}", template.title));
    let new = NewAutomation { name, resource, trigger, conditions, actions, enabled: true };
    create(backend, engine, new, Some(template.name)).await
}

pub async fn handle(backend: &Backend, engine: &Engine, request: Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let answer = match segments.as_slice() {
        ["public", "hooks", id] if request.method == "POST" => {
            hook(backend, engine, &request, id).await
        }
        ["discovery", "queues", name] if request.method == "POST" => {
            queue(backend, engine, &request, name).await
        }
        ["ui", route @ ..] => return crate::ui::handle(backend, engine, &request, route).await,
        ["api", route @ ..] => api(backend, engine, &request, route).await,
        _ => Err(Refusal::missing("no such route")),
    };
    match answer {
        Ok((204, _)) => Response::new(204, "application/json", Vec::new()),
        Ok((status, value)) => Response::new(
            status,
            "application/json",
            serde_json::to_vec(&value).unwrap_or_default(),
        ),
        Err(refusal) => refusal.response(),
    }
}

async fn api(backend: &Backend, engine: &Engine, request: &Request, path: &[&str]) -> Answer {
    let store = Store(backend);
    match (request.method.as_str(), path) {
        ("GET", ["automations"]) => {
            let resource =
                query(request, "resource").map(|resource| normalised(&resource)).transpose()?;
            let listed = store.automations(resource.as_deref()).await?;
            Ok((
                200,
                json!(
                    listed.iter().map(|automation| shown(automation, backend)).collect::<Vec<_>>()
                ),
            ))
        }
        ("POST", ["automations"]) => {
            Ok((201, create(backend, engine, body(request)?, None).await?.shown(backend)))
        }
        ("GET", ["automations", id]) => {
            Ok((200, shown(&found(backend, id_of(id)?).await?, backend)))
        }
        ("PATCH", ["automations", id]) => {
            Ok((200, change(backend, engine, id_of(id)?, body(request)?).await?.shown(backend)))
        }
        ("DELETE", ["automations", id]) => {
            remove(backend, engine, id_of(id)?).await?;
            Ok((204, Value::Null))
        }
        ("POST", ["automations", id, "run"]) => {
            let asked: RunNow = body(request)?;
            match run_now(backend, id_of(id)?, asked.payload, asked.force).await? {
                Ok(run) => Ok((202, json!({ "matched": true, "run": run.id, "task": run.task }))),
                Err(reason) => Ok((200, json!({ "matched": false, "reason": reason }))),
            }
        }
        ("POST", ["automations", id, "secret"]) => {
            let id = id_of(id)?;
            let fresh = rotate(backend, id).await?;
            Ok((200, json!({ "webhook": hook_url(id), "secret": fresh.expose() })))
        }
        ("GET", ["automations", id, "runs"]) => {
            Ok((200, json!(store.runs(id_of(id)?, limit(request)).await?)))
        }
        ("GET", ["automations", id, "triggers"]) => {
            Ok((200, json!(store.triggers(id_of(id)?, limit(request)).await?)))
        }
        ("GET", ["runs", id]) => {
            let id: Uuid = id.parse().map_err(|_| Refusal::missing("there is no such run"))?;
            let run =
                store.run(id).await?.ok_or_else(|| Refusal::missing("there is no such run"))?;
            Ok((200, json!(run)))
        }
        ("GET", ["templates"]) => {
            let listed: Vec<Value> =
                templates::ALL.iter().map(templates::Template::shown).collect();
            Ok((200, json!(listed)))
        }
        ("POST", ["templates", name]) => {
            Ok((201, from_template(backend, engine, name, body(request)?).await?.shown(backend)))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

/// A webhook's POST, taken only with the automation's own signature over the exact body.
async fn hook(backend: &Backend, engine: &Engine, request: &Request, id: &str) -> Answer {
    let unknown = || Refusal::missing("there is no such webhook");
    let id: Uuid = id.parse().map_err(|_| unknown())?;
    let automation = Store(backend).automation(id).await?.ok_or_else(unknown)?;
    let (Trigger::Webhook, Some(secret), true) =
        (&automation.trigger, &automation.secret, automation.enabled)
    else {
        return Err(unknown());
    };
    if request.body.len() > MAX_HOOK_BODY {
        return Err(Refusal::bad("a webhook's body is at most 1 MiB"));
    }
    let given = request
        .headers
        .get("x-doc-signature")
        .or_else(|| request.headers.get("x-hub-signature-256"))
        .ok_or_else(|| {
            Refusal::unauthorised(
                "sign the body with the webhook's secret, as x-doc-signature: sha256=<hex>",
            )
        })?;
    let expected = signature(secret.expose(), &request.body);
    if !bool::from(given.trim().as_bytes().ct_eq(expected.as_bytes())) {
        return Err(Refusal::unauthorised("the signature does not match the body"));
    }
    let payload = serde_json::from_slice::<Value>(&request.body)
        .unwrap_or_else(|_| json!({ "text": String::from_utf8_lossy(&request.body) }));
    let input = json!({ "trigger": "webhook", "payload": payload });
    crate::sample(backend, "webhook", &id.to_string(), &input).await;
    Store(backend).enqueue(&[id], "webhook", &input).await?;
    engine.wake();
    Ok((202, json!({ "accepted": true })))
}

/// A message another plugin sent to one of the queues automations listen on.
async fn queue(backend: &Backend, engine: &Engine, request: &Request, name: &str) -> Answer {
    let sender = match backend.caller() {
        Some(Caller { kind, id: Some(id), .. }) if kind == "plugin" => id.clone(),
        _ => {
            return Err(Refusal::forbidden(
                "queues take messages from plugins, over the Service Bus",
            ));
        }
    };
    let payload: Value = serde_json::from_slice(&request.body).unwrap_or(Value::Null);
    let listening: Vec<Uuid> = Store(backend)
        .triggered_by("queue")
        .await?
        .into_iter()
        .filter(|(_, queue)| queue == name)
        .map(|(id, _)| id)
        .collect();
    // Noted whether or not anything listens, so the queue can be chosen for a trigger later.
    let _ = Store(backend).heard(name, &sender).await;
    let input = json!({ "trigger": "queue", "queue": name, "sender": sender, "payload": payload });
    crate::sample(backend, "queue", name, &input).await;
    Store(backend).enqueue(&listening, "queue", &input).await?;
    engine.wake();
    Ok((200, json!({ "triggered": listening.len() })))
}
