//! The JSON API at `/api/v1/plugins/templates/api/…`: the templates, a dry run of one, launching
//! one, and the runs that came of it.

use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::model::Lifecycle;
use crate::store::{Run, Store, Template};
use crate::{AUTHOR, Refusal, archive, engine, files, model, scaffolds, seed};

pub fn query(request: &Request, name: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn ok(value: &Value) -> Result<Response, Refusal> {
    Ok(Response::json(value))
}

/// The answers a caller sent: `{"values": {…}}`, or the object itself.
fn answers(request: &Request) -> Result<Map<String, Value>, Refusal> {
    if request.body.is_empty() {
        return Ok(Map::new());
    }
    let body: Value = request.json().map_err(|err| Refusal::bad(err.to_string()))?;
    let values = match body {
        Value::Object(mut body) => match body.remove("values") {
            Some(Value::Object(values)) => values,
            Some(_) => return Err(Refusal::bad("`values` is an object of answers")),
            None => body,
        },
        _ => return Err(Refusal::bad("send an object of answers")),
    };
    Ok(values)
}

/// A template as the API describes it. The whole definition is there, since a template is meant to
/// be read, copied and written again.
pub fn shown(template: &Template) -> Value {
    json!({
        "name": template.name,
        "title": template.title,
        "description": template.description,
        "kind": template.kind,
        "tags": template.tags,
        "owner": template.owner,
        "lifecycle": template.definition["lifecycle"],
        "builtin": template.builtin,
        "author": template.author,
        "definition": template.definition,
    })
}

pub fn run_shown(run: &Run) -> Value {
    json!({
        "id": run.id,
        "template": run.template,
        "title": run.title,
        "state": run.state,
        "lifecycle": run.lifecycle.name(),
        "lifecycle_title": run.lifecycle.title(),
        "values": run.answers,
        "steps": run.steps,
        "outputs": run.outputs,
        "links": run.links,
        "error": run.error,
        "requester": run.requester,
        "requested_by": run.requester_label,
        "task": run.task,
        "created_at": run.created_at,
        "started_at": run.started_at,
        "finished_at": run.finished_at,
    })
}

pub async fn handle(backend: &Backend, request: Request) -> Response {
    match route(backend, &request).await {
        Ok(response) => response,
        Err(refusal) => refusal.response(),
    }
}

async fn route(backend: &Backend, request: &Request) -> Result<Response, Refusal> {
    let store = Store(backend);
    let path = request.path.trim_start_matches("api/").trim_end_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    match (request.method.as_str(), parts.as_slice()) {
        ("GET", ["templates"]) => {
            let templates = store.templates(query(request, "q").as_deref()).await?;
            ok(&json!({ "templates": templates.iter().map(shown).collect::<Vec<_>>() }))
        }
        ("POST", ["templates"]) => {
            backend.require(AUTHOR, true)?;
            let text = std::str::from_utf8(&request.body)
                .map_err(|_| Refusal::bad("the template is not UTF-8 text"))?;
            let definition = model::definition(text).map_err(Refusal::bad)?;
            let (who, _) = engine::requester(backend);
            let template = store.save(&definition, &who, false).await?;
            backend.audit("template.saved", Some(&template.name), json!({ "by": who })).await.ok();
            ok(&shown(&template))
        }
        ("GET", ["templates", name]) => {
            let template = found(&store, name).await?;
            ok(&shown(&template))
        }
        ("DELETE", ["templates", name]) => {
            backend.require(AUTHOR, true)?;
            if !store.remove(name).await? {
                return Err(Refusal::missing(format!("there is no template called `{name}`")));
            }
            let (who, _) = engine::requester(backend);
            backend.audit("template.removed", Some(name), json!({ "by": who })).await.ok();
            Ok(Response::new(204, "application/json", Vec::new()))
        }
        // A dry run: everything the template would make, and what each step would do, without
        // creating anything anywhere.
        ("POST", ["templates", name, "preview"]) => {
            let template = found(&store, name).await?;
            let definition = template.definition()?;
            let values = model::values(&definition.asked(), &answers(request)?)
                .map_err(|problems| Refusal::from_problems(&problems))?;
            let context = engine::proposed(backend, &definition, &template.title, &values);
            let made =
                files::assemble(backend, &definition, &context).await.map_err(Refusal::bad)?;
            ok(&json!({
                "template": definition.name,
                "values": values,
                "lifecycle": definition.lifecycle_of(&Value::Object(values.clone())).name(),
                "files": made
                    .iter()
                    .map(|file| json!({
                        "path": file.path,
                        "bytes": file.size(),
                        "text": file.text,
                    }))
                    .collect::<Vec<_>>(),
                "steps": definition
                    .steps
                    .iter()
                    .map(|step| json!({
                        "id": step.id,
                        "title": step.label(),
                        "action": step.action.kind(),
                        "summary": step.action.summary(Some(&context)),
                    }))
                    .collect::<Vec<_>>(),
            }))
        }
        ("POST", ["templates", name, "launch"]) => {
            if !backend.writes() {
                return Err(Refusal::forbidden(
                    "launching a template needs plugin:templates:user:rw",
                ));
            }
            let template = found(&store, name).await?;
            let definition = template.definition()?;
            let values = model::values(&definition.asked(), &answers(request)?)
                .map_err(|problems| Refusal::from_problems(&problems))?;
            let run = engine::launch(backend, &template, &definition, values).await?;
            Ok(Response::new(
                202,
                "application/json",
                json!({
                    "run": run.id,
                    "task": run.task,
                    "state": run.state,
                    "lifecycle": run.lifecycle.name(),
                })
                .to_string(),
            ))
        }
        ("GET", ["runs"]) => {
            let template = query(request, "template");
            let mine = query(request, "mine").is_some();
            let requester = mine.then(|| engine::requester(backend).0);
            let lifecycle = asked_lifecycle(request)?;
            let runs =
                store.runs(template.as_deref(), requester.as_deref(), lifecycle, 100).await?;
            ok(&json!({ "runs": runs.iter().map(run_shown).collect::<Vec<_>>() }))
        }
        ("GET", ["runs", id]) => {
            let run = run_found(&store, id).await?;
            ok(&run_shown(&run))
        }
        ("GET", ["runs", id, "log"]) => {
            let run = run_found(&store, id).await?;
            let from = query(request, "from").and_then(|from| from.parse().ok()).unwrap_or(0);
            let lines = store.lines(run.id, from, 1_000).await?;
            ok(&json!({
                "run": run.id,
                "state": run.state,
                "lines": lines
                    .iter()
                    .map(|line| json!({
                        "seq": line.seq,
                        "at": line.at,
                        "step": line.step,
                        "level": line.level,
                        "text": line.text,
                    }))
                    .collect::<Vec<_>>(),
            }))
        }
        ("GET", ["runs", id, "download", format]) => download(backend, id, format).await,
        ("POST", ["runs", id, "cancel"]) => {
            if !backend.writes() {
                return Err(Refusal::forbidden("cancelling a run needs plugin:templates:user:rw"));
            }
            let run = run_found(&store, id).await?;
            let cancelled = store
                .cancel(run.id)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such run"))?;
            ok(
                &json!({ "run": cancelled.id, "state": cancelled.state, "cancelled": cancelled.cancelled }),
            )
        }
        // Every language and application type a template can scaffold, for anything building a
        // form of its own — and for reading what a pair actually writes.
        ("GET", ["scaffolds"]) => {
            let files = |language: &str, app: &str| {
                scaffolds::files(language, app)
                    .into_iter()
                    .map(|(path, content)| json!({ "path": path, "bytes": content.len() }))
                    .collect::<Vec<_>>()
            };
            let wanted = (query(request, "language"), query(request, "app"));
            if let (Some(language), Some(app)) = wanted {
                if !scaffolds::holds(&language, &app) {
                    return Err(Refusal::missing(format!(
                        "there is no {app} scaffold for {language}"
                    )));
                }
                return ok(&json!({
                    "language": language,
                    "app": app,
                    "files": files(&language, &app),
                }));
            }
            ok(&json!({
                "languages": scaffolds::LANGUAGES
                    .iter()
                    .map(|language| json!({
                        "name": language.name,
                        "title": language.title,
                        "about": language.about,
                    }))
                    .collect::<Vec<_>>(),
                "applications": scaffolds::APPS
                    .iter()
                    .map(|app| json!({
                        "name": app.name,
                        "title": app.title,
                        "about": app.about,
                        "languages": match app.languages.is_empty() {
                            true => scaffolds::LANGUAGES.iter().map(|language| language.name).collect(),
                            false => app.languages.to_vec(),
                        },
                    }))
                    .collect::<Vec<_>>(),
            }))
        }
        // How far along a creation can be, which every template says of what it makes.
        ("GET", ["lifecycles"]) => ok(&json!({
            "lifecycles": Lifecycle::ALL
                .iter()
                .map(|lifecycle| json!({
                    "name": lifecycle.name(),
                    "title": lifecycle.title(),
                    "about": lifecycle.about(),
                }))
                .collect::<Vec<_>>(),
        })),
        // What the plugin starts with, for an author to copy rather than start from nothing.
        ("GET", ["examples"]) => ok(&json!({ "examples": seed::BUILT_IN })),
        _ => Ok(Response::not_found()),
    }
}

/// `?lifecycle=experiment`, refused rather than ignored when it names one there is not.
fn asked_lifecycle(request: &Request) -> Result<Option<Lifecycle>, Refusal> {
    match query(request, "lifecycle") {
        None => Ok(None),
        Some(asked) => Lifecycle::parse(&asked).map(Some).ok_or_else(|| {
            Refusal::bad(format!(
                "`{asked}` is not a lifecycle: they are {}",
                Lifecycle::ALL.map(Lifecycle::name).join(", ")
            ))
        }),
    }
}

pub async fn found(store: &Store<'_>, name: &str) -> Result<Template, Refusal> {
    store
        .template(name)
        .await?
        .ok_or_else(|| Refusal::missing(format!("there is no template called `{name}`")))
}

/// Whether the caller may download what a run made: whoever asked for it, or anyone who may create
/// from templates themselves and so could have made the same.
pub fn downloads(backend: &Backend, run: &Run) -> bool {
    backend.writes() || engine::requester(backend).0 == run.requester
}

/// What a finished run made, packed as a `.zip` or a `.tar.gz` to download. The files are made
/// again from the run's answers and what its steps left behind, against the template as it is now.
pub async fn download(backend: &Backend, id: &str, format: &str) -> Result<Response, Refusal> {
    let format = archive::Format::named(format)
        .ok_or_else(|| Refusal::missing("a run's files download as zip or tar.gz"))?;
    let store = Store(backend);
    let run = run_found(&store, id).await?;
    if !downloads(backend, &run) {
        return Err(Refusal::forbidden(
            "only whoever asked for this run, or somebody who may create from templates, can \
             download what it made",
        ));
    }
    if run.state != "succeeded" {
        return Err(Refusal {
            status: 409,
            detail: format!(
                "the run is {}; what it made can be downloaded once it succeeds",
                run.state
            ),
        });
    }
    let template = store.template(&run.template).await?.ok_or_else(|| {
        Refusal::missing(format!(
            "the template {} is gone, so its files cannot be made again",
            run.template
        ))
    })?;
    let definition = template.definition()?;
    if !definition.makes_files() {
        return Err(Refusal::missing(format!("{} makes no files", run.template)));
    }
    let context = engine::context(backend, &run, &definition);
    let made =
        files::assemble(backend, &definition, &context).await.map_err(Refusal::unavailable)?;
    let root = archive::root(run.answers["name"].as_str().unwrap_or(&run.template));
    let at = run
        .finished_at
        .as_deref()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
        .map_or_else(chrono::Utc::now, |at| at.with_timezone(&chrono::Utc));
    let packed = archive::pack(format, &root, &made, at);
    let name = format!("{root}.{}", format.extension());
    Ok(Response::new(200, format.content_type(), packed)
        .with_header("content-disposition", &format!("attachment; filename=\"{name}\""))
        .with_header("cache-control", "no-store"))
}

pub async fn run_found(store: &Store<'_>, id: &str) -> Result<Run, Refusal> {
    let id = Uuid::parse_str(id).map_err(|_| Refusal::bad("a run is named by its id"))?;
    store.run(id).await?.ok_or_else(|| Refusal::missing("there is no such run"))
}
