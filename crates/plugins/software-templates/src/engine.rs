//! Launching a template and taking its steps. A run is queued as whoever launched it, so every
//! step — the Catalogue entry, the call to another plugin — is made with their permissions and
//! shows up under their name. Each outcome is kept on the run, so a run that ran out of time
//! carries on from where it stopped rather than doing anything twice.

use std::time::{Duration, Instant};

use chrono::Utc;
use doc_plugin_sdk::{Backend, PluginError};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::files;
use crate::model::{Action, Definition, Step};
use crate::publish::{self, GitHub, Spec};
use crate::render;
use crate::store::{Outcome, Run, Store, Template};

/// How long one attempt spends on steps before it queues the rest as another task. The platform
/// gives a background run 30 seconds, and a step may be most of the way through one when the time
/// is up, so it stops well before that.
const BUDGET: Duration = Duration::from_secs(18);
/// Lines a publish step writes about the files it wrote before it stops naming them.
const NAMED_FILES: usize = 40;

/// The lines one step wrote, kept together so a step is one write to the log.
#[derive(Default)]
pub struct Log(Vec<(String, String, Option<String>)>);

impl Log {
    fn say(&mut self, level: &str, step: &str, text: impl Into<String>) {
        self.0.push((level.to_string(), text.into(), Some(step.to_string())));
    }
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

/// Who a run belongs to, as permissions and the Catalogue write them: `user:1234`.
pub fn requester(backend: &Backend) -> (String, String) {
    let caller = backend.caller();
    let reference = caller
        .and_then(|caller| Some(format!("{}:{}", caller.kind, caller.id.as_deref()?)))
        .unwrap_or_else(|| "anonymous".to_string());
    let label = caller.and_then(|caller| caller.label.clone()).unwrap_or_else(|| reference.clone());
    (reference, label)
}

/// Records a run and queues it as whoever asked for it.
pub async fn launch(
    backend: &Backend,
    template: &Template,
    definition: &Definition,
    answers: Map<String, Value>,
) -> Result<Run, Refusal> {
    let store = Store(backend);
    let (reference, label) = requester(backend);
    let delegation = backend
        .delegate(&format!(
            "the template {} ({}), launched by {label}",
            template.name, template.title
        ))
        .await?;
    let lifecycle = definition.lifecycle_of(&Value::Object(answers.clone()));
    let planned: Vec<Outcome> = definition
        .steps
        .iter()
        .map(|step| Outcome {
            id: step.id.clone(),
            title: step.label(),
            action: step.action.kind().to_string(),
            state: "pending".into(),
            ..Outcome::default()
        })
        .collect();
    let run = Run {
        id: Uuid::now_v7(),
        template: template.name.clone(),
        title: template.title.clone(),
        answers: Value::Object(answers),
        state: "queued".into(),
        lifecycle,
        steps: serde_json::to_value(&planned).unwrap_or_default(),
        cursor: 0,
        outputs: json!({}),
        links: json!([]),
        error: None,
        requester: reference,
        requester_label: label.clone(),
        delegation: Some(delegation),
        task: None,
        attempts: 0,
        cancelled: false,
        template_version: template.version,
        started_at: None,
        finished_at: None,
        created_at: String::new(),
    };
    let run = store.new_run(&run).await?;
    store
        .log(run.id, 0, &[("info".into(), format!("{label} launched {}", template.title), None)])
        .await?;
    match backend.task_as(delegation, json!({ "run": run.id }), Some(1)).await {
        Ok(task) => {
            let queued = store.set(run.id, json!({ "task": task })).await?;
            let _ = backend
                .publish(
                    "plugin.templates.run.started",
                    json!({ "run": run.id, "template": run.template, "by": run.requester }),
                )
                .await;
            Ok(queued.unwrap_or(run))
        }
        Err(err) => {
            let reason = format!("the run could not start as {label}: {}", err.detail());
            let _ = store
                .set(run.id, json!({ "state": "failed", "error": reason, "finished_at": now() }))
                .await;
            Err(Refusal::unavailable(reason))
        }
    }
}

/// What the templates in a run are rendered against: the answers, what the steps before it left
/// behind, and the platform's own stacks.
pub fn context(backend: &Backend, run: &Run, definition: &Definition) -> Value {
    let service = run.answers["name"].as_str().unwrap_or(&run.template).to_string();
    let lifecycle = definition.lifecycle_of(&run.answers);
    let mut context = json!({
        "values": run.answers,
        "steps": run.outputs,
        "run": { "id": run.id, "template": run.template, "title": run.title },
        "user": { "reference": run.requester, "name": run.requester_label },
        "template": { "name": definition.name, "title": definition.label(), "tags": definition.tags, "kind": definition.kind },
        "now": now(),
    });
    merge(&mut context, crate::platform::context(backend, &service, lifecycle));
    context
}

/// What a page or a dry run renders against, before there is a run to render against: the answers
/// as they have been given so far, and the platform's own values.
pub fn proposed(
    backend: &Backend,
    definition: &Definition,
    title: &str,
    values: &Map<String, Value>,
) -> Value {
    let (who, label) = requester(backend);
    let lifecycle = definition.lifecycle_of(&Value::Object(values.clone()));
    let mut context = json!({
        "values": values,
        "steps": {},
        "run": { "id": Uuid::nil(), "template": definition.name, "title": title },
        "user": { "reference": who, "name": label },
        "template": { "name": definition.name, "title": definition.label(), "tags": definition.tags, "kind": definition.kind },
        "now": now(),
    });
    let service =
        values.get("name").and_then(Value::as_str).unwrap_or(&definition.name).to_string();
    merge(&mut context, crate::platform::context(backend, &service, lifecycle));
    context
}

/// Adds the platform's own values to a context, which never overwrite what a run holds.
pub fn merge(context: &mut Value, extra: Value) {
    if let (Value::Object(context), Value::Object(extra)) = (context, extra) {
        for (key, value) in extra {
            context.entry(key).or_insert(value);
        }
    }
}

/// One attempt at a run: every step not yet taken, in order, until they are done or the time for
/// this attempt is up, when the rest is queued as another task.
pub async fn execute(backend: &Backend, id: Uuid) -> Result<Value, PluginError> {
    let store = Store(backend);
    let since = Instant::now();
    let mut run =
        store.run(id).await?.ok_or_else(|| PluginError::from(format!("there is no run {id}")))?;
    if !run.live() {
        return Ok(json!({ "run": id, "state": run.state }));
    }
    // Stopped between being queued and being picked up: nothing of it has happened, so nothing has
    // to be undone.
    if run.cancelled {
        store.set(id, json!({ "state": "cancelled", "finished_at": now() })).await?;
        return finished(backend, id, "cancelled", &run.template).await;
    }
    let template = store
        .template(&run.template)
        .await?
        .ok_or_else(|| PluginError::from(format!("the template {} is gone", run.template)))?;
    let definition = template.definition()?;
    let mut seq = store.next_line(run.id).await?;
    let mut outcomes = run.outcomes();

    run = store
        .set(
            id,
            json!({
                "state": "running",
                "attempts": run.attempts + 1,
                "started_at": run.started_at.clone().unwrap_or_else(now),
            }),
        )
        .await?
        .unwrap_or(run);

    while (run.cursor as usize) < definition.steps.len() {
        if store.run(id).await?.is_some_and(|latest| latest.cancelled) {
            let at = run.cursor as usize;
            for outcome in outcomes.iter_mut().skip(at) {
                outcome.state = "skipped".into();
                outcome.detail = Some("the run was cancelled".into());
            }
            store.log(id, seq, &[("warning".into(), "Cancelled".into(), None)]).await?;
            store
                .set(
                    id,
                    json!({
                        "state": "cancelled",
                        "steps": outcomes,
                        "finished_at": now(),
                    }),
                )
                .await?;
            return finished(backend, id, "cancelled", &run.template).await;
        }
        if since.elapsed() > BUDGET {
            store
                .log(id, seq, &[("muted".into(), "Carrying on in another task".into(), None)])
                .await?;
            let delegation = run
                .delegation
                .ok_or_else(|| PluginError::from("the run has no delegation to carry on with"))?;
            let task = backend.task_as(delegation, json!({ "run": id }), Some(1)).await?;
            store.set(id, json!({ "state": "queued", "task": task })).await?;
            return Ok(json!({ "run": id, "state": "queued", "continues": task }));
        }

        let at = run.cursor as usize;
        let step = &definition.steps[at];
        let context = context(backend, &run, &definition);
        let mut log = Log::default();
        let started = now();
        let outcome = match skipped(step, &context) {
            Some(why) => {
                log.say("muted", &step.id, format!("Skipped: {why}"));
                Outcome {
                    state: "skipped".into(),
                    detail: Some(why),
                    started_at: Some(started.clone()),
                    finished_at: Some(now()),
                    ..outcomes[at].clone()
                }
            }
            None => {
                log.say("command", &step.id, step.label());
                match perform(backend, step, &context, &definition, &mut log).await {
                    Ok(output) => {
                        log.say("success", &step.id, format!("{} is done", step.label()));
                        Outcome {
                            state: "done".into(),
                            output,
                            started_at: Some(started.clone()),
                            finished_at: Some(now()),
                            ..outcomes[at].clone()
                        }
                    }
                    Err(problem) => {
                        log.say("error", &step.id, problem.clone());
                        Outcome {
                            state: "failed".into(),
                            detail: Some(problem),
                            started_at: Some(started.clone()),
                            finished_at: Some(now()),
                            ..outcomes[at].clone()
                        }
                    }
                }
            }
        };
        let failed = outcome.state == "failed";
        let output = outcome.output.clone();
        outcomes[at] = outcome;
        seq = store.log(id, seq, &log.0).await?;

        let mut outputs = match run.outputs.clone() {
            Value::Object(outputs) => outputs,
            _ => Map::new(),
        };
        outputs.insert(step.id.clone(), output);
        let mut set = json!({
            "steps": outcomes,
            "outputs": outputs,
            "cursor": run.cursor + 1,
        });
        if failed {
            for later in outcomes.iter_mut().skip(at + 1) {
                later.state = "skipped".into();
                later.detail = Some("an earlier step failed".into());
            }
            set = json!({
                "steps": outcomes,
                "outputs": outputs,
                "cursor": definition.steps.len() as i64,
                "state": "failed",
                "error": outcomes[at].detail.clone(),
                "finished_at": now(),
            });
            store.set(id, set).await?;
            return finished(backend, id, "failed", &run.template).await;
        }
        run = store.set(id, set).await?.unwrap_or(run);
        let _ = backend.publish("plugin.templates.ui.run", json!({ "run": id })).await;
    }

    let context = context(backend, &run, &definition);
    let links: Vec<Value> = definition
        .links
        .iter()
        .filter_map(|link| {
            let url = render::line(&link.url, &context).ok()?;
            let title = render::line(&link.title, &context).unwrap_or_else(|_| link.title.clone());
            (!url.is_empty()).then(|| json!({ "title": title, "url": url }))
        })
        .collect();
    store.log(id, seq, &[("success".into(), format!("{} is ready", run.title), None)]).await?;
    store.set(id, json!({ "state": "succeeded", "links": links, "finished_at": now() })).await?;
    finished(backend, id, "succeeded", &run.template).await
}

/// Tells everyone a run ended, and answers what the task did.
async fn finished(
    backend: &Backend,
    id: Uuid,
    state: &str,
    template: &str,
) -> Result<Value, PluginError> {
    let _ = backend
        .publish(
            &format!("plugin.templates.run.{state}"),
            json!({ "run": id, "template": template, "state": state }),
        )
        .await;
    let _ = backend.publish("plugin.templates.ui.run", json!({ "run": id })).await;
    Ok(json!({ "run": id, "state": state }))
}

/// Why a step did not run, if it did not.
fn skipped(step: &Step, context: &Value) -> Option<String> {
    let when = step.when.as_ref()?;
    match render::line(when, context) {
        Ok(rendered) => {
            let value = Value::String(rendered);
            (!render::truthy(&value)).then(|| format!("`{when}` was not true"))
        }
        Err(err) => Some(format!("`{when}` could not be read: {err}")),
    }
}

/// A templated setting of a step, rendered, or what is wrong with it.
fn filled(source: &str, context: &Value, what: &str) -> Result<String, String> {
    render::line(source, context).map_err(|err| format!("{what}: {err}"))
}

/// Every string inside a JSON value, rendered.
fn filled_value(value: &Value, context: &Value) -> Value {
    match value {
        Value::String(text) => {
            Value::String(render::render(text, context).unwrap_or_else(|_| text.clone()))
        }
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| filled_value(item, context)).collect())
        }
        Value::Object(fields) => Value::Object(
            fields.iter().map(|(key, value)| (key.clone(), filled_value(value, context))).collect(),
        ),
        other => other.clone(),
    }
}

/// Takes one step, answering what it leaves behind for the steps after it.
async fn perform(
    backend: &Backend,
    step: &Step,
    context: &Value,
    definition: &Definition,
    log: &mut Log,
) -> Result<Value, String> {
    match &step.action {
        Action::Publish { owner, repository, description, visibility, branch, message } => {
            let settings = backend.settings();
            let github = GitHub::new(&settings)?;
            let named = match owner {
                Some(owner) => filled(owner, context, "the owner")?,
                None => String::new(),
            };
            let owner = match named.is_empty() {
                true => settings.some_text(publish::OWNER).unwrap_or_default(),
                false => named,
            };
            if owner.is_empty() {
                return Err(
                    "no GitHub owner was named, and the plugin has no default one on its Settings page".into(),
                );
            }
            let repository = filled(repository, context, "the repository's name")?;
            if repository.is_empty() {
                return Err("the repository's name came out empty".into());
            }
            let visibility = match visibility {
                Some(visibility) => filled(visibility, context, "the visibility")?,
                None => "private".to_string(),
            };
            let spec = Spec {
                owner: owner.clone(),
                repository: repository.clone(),
                description: match description {
                    Some(description) => filled(description, context, "the description")?,
                    None => definition.description.clone(),
                },
                private: visibility != "public",
                branch: match branch {
                    Some(branch) => Some(filled(branch, context, "the branch")?),
                    None => None,
                },
                message: match message {
                    Some(message) => filled(message, context, "the commit message")?,
                    None => format!("Created from the {} template in DOC", definition.label()),
                },
            };
            let files = files::assemble(backend, definition, context).await?;
            log.say("info", &step.id, format!("{} files to commit", files.len()));
            for file in files.iter().take(NAMED_FILES) {
                log.say("muted", &step.id, format!("  {} ({} bytes)", file.path, file.size()));
            }
            if files.len() > NAMED_FILES {
                log.say("muted", &step.id, format!("  … and {} more", files.len() - NAMED_FILES));
            }
            log.say("info", &step.id, format!("Creating {owner}/{repository}"));
            let output = publish::publish(&github, &spec, &files).await?;
            if let Some(url) = output["url"].as_str() {
                log.say("info", &step.id, format!("Pushed to {url}"));
            }
            Ok(output)
        }
        Action::Register { kind, name, title, description, owner, connections, metadata } => {
            let kind = filled(kind, context, "the kind")?;
            let name = filled(name, context, "the name")?;
            let mut document = json!({ "kind": kind, "name": name });
            if let Some(title) = title {
                document["title"] = json!(filled(title, context, "the title")?);
            }
            if let Some(description) = description {
                document["description"] = json!(filled(description, context, "the description")?);
            }
            if let Some(owner) = owner {
                let owner = filled(owner, context, "the owner")?;
                // A team chosen from the Catalogue comes back as `Team:payments`, and a document
                // names the team itself.
                document["owner"] = json!(
                    owner.split_once(':').map_or(owner.clone(), |(_, name)| name.to_string())
                );
            }
            if !connections.is_empty() {
                let refs: Result<Vec<String>, String> = connections
                    .iter()
                    .map(|connection| filled(connection, context, "a connection"))
                    .collect();
                document["connections"] =
                    json!(refs?.into_iter().filter(|one| !one.is_empty()).collect::<Vec<_>>());
            }
            if !metadata.is_empty() {
                document["metadata"] = filled_value(&Value::Object(metadata.clone()), context);
            }
            log.say("info", &step.id, format!("Applying {kind}:{name} to the Catalogue"));
            let (status, answer) = backend
                .ask("resources", "POST", "apply", None, Some(json!([document])))
                .await
                .map_err(|err| format!("the Catalogue could not be asked: {}", err.detail()))?;
            if !(200..300).contains(&status) {
                let detail = answer["detail"].as_str().unwrap_or("it gave no reason");
                return Err(match status {
                    403 => format!("the Catalogue refused to add {kind}:{name} as you: {detail}"),
                    _ => format!("the Catalogue answered {status}: {detail}"),
                });
            }
            let slug = kind.to_lowercase();
            Ok(json!({
                "kind": kind,
                "name": name,
                "reference": format!("{kind}:{name}"),
                "url": format!("/p/resources/r/{slug}/{name}"),
                "applied": answer,
            }))
        }
        Action::Request { plugin, method, route, query, body } => {
            let route = filled(route, context, "the route")?;
            let query = match query {
                Some(query) => Some(filled(query, context, "the query")?),
                None => None,
            };
            let body = body.as_ref().map(|body| filled_value(body, context));
            log.say("info", &step.id, format!("{} {route} on {plugin}", method.to_uppercase()));
            let (status, answer) = backend
                .ask(plugin, &method.to_uppercase(), &route, query.as_deref(), body)
                .await
                .map_err(|err| format!("{plugin} could not be asked: {}", err.detail()))?;
            if !(200..300).contains(&status) {
                let detail = answer["detail"].as_str().unwrap_or("it gave no reason");
                return Err(format!("{plugin} answered {status}: {detail}"));
            }
            Ok(json!({ "status": status, "body": answer }))
        }
        Action::Infra { template, team, service, name, region, size, lifetime } => {
            let template = filled(template, context, "the Infra template")?;
            // A resource parameter answers `Team:payments-core`; Infra takes the name alone, the
            // same way a register step does.
            let named = |text: String| {
                text.split_once(':').map_or(text.clone(), |(_, name)| name.to_string())
            };
            let team = named(filled(team, context, "the team")?);
            let service = match service {
                Some(service) => Some(named(filled(service, context, "the service")?)),
                None => None,
            };
            let name = match name {
                Some(name) => filled(name, context, "the name")?,
                // A resource with no name of its own is named after what it is for.
                None => service
                    .clone()
                    .ok_or("an infra step needs a name, or a service to take one from")?,
            };
            let mut body = json!({ "template": template, "team": team, "name": name });
            for (key, value) in [
                ("service", service.as_ref()),
                ("region", region.as_ref()),
                ("size", size.as_ref()),
                ("lifetime", lifetime.as_ref()),
            ] {
                if let Some(value) = value {
                    body[key] = json!(filled(value, context, key)?);
                }
            }
            log.say("info", &step.id, format!("{template} for {team}, called {name}"));
            let (status, answer) = backend
                .ask("infra", "POST", "requests", None, Some(body))
                .await
                .map_err(|err| format!("the Infra plugin could not be asked: {}", err.detail()))?;
            if !(200..300).contains(&status) {
                let detail = answer["detail"].as_str().unwrap_or("it gave no reason");
                return Err(format!("the Infra plugin answered {status}: {detail}"));
            }
            // Infra answers before the vendor has made anything, so this is as much as the run
            // ever knows: whatever it stood up says so itself when it is ready, and the service's
            // name in DNS follows that rather than this.
            log.say(
                "info",
                &step.id,
                "Infra is standing it up; its name in DNS follows when it answers".to_string(),
            );
            Ok(json!({
                "request": answer["id"],
                "status": answer["status"],
                "resource": answer["resource"],
                "url": answer["url"],
            }))
        }
        Action::Dns { name, value, kind, ttl, note } => {
            let name = filled(name, context, "the name")?;
            let value = filled(value, context, "what it points at")?;
            // An address is an A or AAAA; anything else is a name, so a CNAME. Said this way so a
            // template that points at whatever `infra` stood up does not have to know which.
            let kind = match kind {
                Some(kind) => kind.trim().to_ascii_uppercase(),
                None => match value.parse::<std::net::IpAddr>() {
                    Ok(std::net::IpAddr::V4(_)) => "A".to_string(),
                    Ok(std::net::IpAddr::V6(_)) => "AAAA".to_string(),
                    Err(_) => "CNAME".to_string(),
                },
            };
            let mut body = json!({ "name": name, "type": kind, "value": value });
            if let Some(ttl) = ttl {
                body["ttl"] = json!(ttl);
            }
            if let Some(note) = note {
                body["note"] = json!(filled(note, context, "the note")?);
            }
            log.say("info", &step.id, format!("{kind} {name} to {value}"));
            let (status, answer) = backend
                .ask("dns", "POST", "records", None, Some(body))
                .await
                .map_err(|err| format!("the DNS plugin could not be asked: {}", err.detail()))?;
            if !(200..300).contains(&status) {
                let detail = answer["detail"].as_str().unwrap_or("it gave no reason");
                return Err(format!("the DNS plugin answered {status}: {detail}"));
            }
            Ok(json!({ "name": name, "type": kind, "value": value, "record": answer["id"] }))
        }
        Action::Notify { user, title, body, url } => {
            let who = match user {
                Some(user) => filled(user, context, "the person to tell")?,
                None => context["user"]["reference"].as_str().unwrap_or_default().to_string(),
            };
            let who = who.strip_prefix("user:").unwrap_or(&who).to_string();
            if who.is_empty() || who == "anonymous" {
                return Err("there is nobody to tell".into());
            }
            let title = filled(title, context, "the title")?;
            let body = match body {
                Some(body) => filled(body, context, "the message")?,
                None => String::new(),
            };
            let url = match url {
                Some(url) => Some(filled(url, context, "the link")?),
                None => None,
            };
            backend
                .notify(&who, &title, &body, url.as_deref())
                .await
                .map_err(|err| format!("the notification could not be left: {}", err.detail()))?;
            Ok(json!({ "user": who, "title": title }))
        }
        Action::Event { topic, payload } => {
            let topic = match topic.starts_with("plugin.templates.") {
                true => topic.clone(),
                false => format!("plugin.templates.{topic}"),
            };
            let payload = match payload {
                Some(payload) => filled_value(payload, context),
                None => context.clone(),
            };
            let event = backend
                .publish(&topic, payload)
                .await
                .map_err(|err| format!("the event could not be published: {}", err.detail()))?;
            Ok(json!({ "topic": topic, "event": event }))
        }
    }
}
