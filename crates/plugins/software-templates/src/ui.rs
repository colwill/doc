//! The pages at `/p/templates/…`: the templates to create from, the wizard that asks what one
//! needs and shows the files before anything is made, and every run with its steps and its log.

use askama::Template as Page;
use chrono::DateTime;
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::model::{self, Definition, Lifecycle, Parameter, ParameterKind};
use crate::store::{Line, Outcome, Run, Store};
use crate::{AUTHOR, Refusal, api, engine, files, seed};

/// The last lines of a log a page carries; the rest is a link to the API.
const TAIL: i64 = 300;
/// How much of a file the check page shows, so a preview of a whole repository stays a page.
const PREVIEW_LINES: usize = 60;
/// The page the wizard ends on.
const CHECK: &str = "Check your answers";

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    fn refused(detail: impl Into<String>) -> Self {
        Self { notice: None, error: Some(detail.into()) }
    }
}

pub struct Card {
    pub name: String,
    pub title: String,
    pub description: String,
    pub tags: Vec<String>,
    /// What the things it creates start out as, or that whoever creates one says.
    pub lifecycle: String,
    pub lifecycle_badge: &'static str,
    pub footer: String,
}

pub struct Asked {
    pub label: String,
    pub hint: Option<String>,
    pub takes: String,
    pub required: bool,
}

pub struct Doing {
    pub title: String,
    pub action: String,
    pub summary: String,
}

pub struct RunRow {
    pub id: Uuid,
    pub title: String,
    pub state: String,
    pub badge: &'static str,
    pub lifecycle: &'static str,
    pub lifecycle_badge: &'static str,
    pub by: String,
    pub when: String,
}

pub struct Field {
    pub name: String,
    pub id: String,
    pub label: String,
    pub hint: Option<String>,
    pub kind: String,
    pub value: String,
    pub checked: bool,
    pub options: Vec<(String, String)>,
    pub error: Option<String>,
    pub placeholder: String,
    pub kinds: String,
    pub required: bool,
}

pub struct Answer {
    pub label: String,
    pub value: String,
    pub page: String,
}

/// One file of a preview, as a row of the tree: its own name on the row, its whole path on the
/// code block so the frontend knows what language to colour it as.
pub struct Preview {
    pub path: String,
    pub name: String,
    pub size: String,
    pub body: String,
}

/// The files of one directory. A preview is drawn a directory at a time rather than as a tree of
/// every level, because a path read whole (`deploy/kubernetes`) says more than three folds do.
pub struct Folder {
    pub name: String,
    pub count: String,
    pub files: Vec<Preview>,
}

pub struct Marked {
    pub title: String,
    pub meta: String,
    pub marker: &'static str,
    pub detail: Option<String>,
}

pub struct Said {
    pub at: String,
    pub level: String,
    pub text: String,
}

#[derive(Page)]
#[template(path = "home.html")]
struct Home {
    flash: Flash,
    author: bool,
    q: String,
    cards: Vec<Card>,
}

#[derive(Page)]
#[template(path = "template.html")]
struct TemplatePage {
    flash: Flash,
    author: bool,
    writes: bool,
    name: String,
    title: String,
    description: String,
    kind: String,
    lifecycle: String,
    lifecycle_badge: &'static str,
    owner: String,
    origin: String,
    parameters: Vec<Asked>,
    steps: Vec<Doing>,
    runs: Vec<RunRow>,
}

#[derive(Page)]
#[template(path = "runs.html")]
struct RunsPage {
    flash: Flash,
    author: bool,
    mine: bool,
    runs: Vec<RunRow>,
}

#[derive(Page)]
#[template(path = "form.html")]
struct FormPage {
    flash: Flash,
    author: bool,
    name: String,
    title: String,
    page: String,
    number: usize,
    of: usize,
    back: Option<String>,
    hidden: Vec<(String, String)>,
    fields: Vec<Field>,
}

#[derive(Page)]
#[template(path = "review.html")]
struct ReviewPage {
    flash: Flash,
    author: bool,
    name: String,
    title: String,
    page: String,
    number: usize,
    of: usize,
    back: Option<String>,
    hidden: Vec<(String, String)>,
    answers: Vec<Answer>,
    directories: Vec<Folder>,
    loose: Vec<Preview>,
    total: String,
    lifecycle: &'static str,
    lifecycle_badge: &'static str,
    /// The page to go back to to change it, or nothing when the template settles it itself.
    lifecycle_page: Option<String>,
    steps: Vec<Doing>,
    preview_problem: Option<String>,
}

#[derive(Page)]
#[template(path = "run.html")]
struct RunPage {
    flash: Flash,
    author: bool,
    writes: bool,
    id: Uuid,
    short: String,
    template: String,
    title: String,
    state: String,
    badge: &'static str,
    lifecycle: &'static str,
    lifecycle_badge: &'static str,
    lifecycle_about: &'static str,
    live: bool,
    by: String,
    started: String,
    finished: Option<String>,
    error: Option<String>,
    answers: Vec<Answer>,
    links: Vec<Link>,
    /// What it made can be downloaded, by this viewer, now.
    downloads: bool,
    /// The template has been edited since the run, so a download is made from it as it is now.
    changed: bool,
    steps: Vec<Marked>,
    lines: Vec<Said>,
    more: i64,
}

pub struct Link {
    pub title: String,
    pub url: String,
}

#[derive(Page)]
#[template(path = "steps.html")]
struct StepsFragment {
    id: Uuid,
    live: bool,
    steps: Vec<Marked>,
}

#[derive(Page)]
#[template(path = "log.html")]
struct LogFragment {
    id: Uuid,
    live: bool,
    template: String,
    short: String,
    state: String,
    badge: &'static str,
    lines: Vec<Said>,
    more: i64,
}

#[derive(Page)]
#[template(path = "definition.html")]
struct DefinitionPage {
    flash: Flash,
    author: bool,
    editing: bool,
    name: String,
    title: String,
    definition: String,
    error: Option<String>,
}

fn drawn<T: Page>(page: &T) -> Result<Response, Refusal> {
    page.render()
        .map(Response::html)
        .map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

type Form = Vec<(String, String)>;

fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> Option<String> {
    form.iter().find(|(key, _)| key == name).map(|(_, value)| value.trim().to_string())
}

fn when(text: &str) -> String {
    DateTime::parse_from_rfc3339(text)
        .map_or_else(|_| text.to_string(), |at| at.format("%-d %b %Y, %H:%M").to_string())
}

fn clock(text: &str) -> String {
    DateTime::parse_from_rfc3339(text)
        .map_or_else(|_| text.to_string(), |at| at.format("%H:%M:%S").to_string())
}

fn badge(state: &str) -> &'static str {
    match state {
        "succeeded" => "ready",
        "queued" | "running" => "loading",
        "failed" => "error",
        _ => "unknown",
    }
}

fn marker(state: &str) -> &'static str {
    match state {
        "done" => "done",
        "running" => "running",
        "failed" => "failed",
        "skipped" => "skipped",
        _ => "pending",
    }
}

/// What a parameter takes, as the template's own page says it.
fn takes(parameter: &Parameter) -> String {
    match parameter.kind {
        ParameterKind::Select => parameter
            .options
            .iter()
            .map(|choice| choice.title().to_string())
            .collect::<Vec<_>>()
            .join(", "),
        ParameterKind::Boolean => "Yes or no".into(),
        ParameterKind::Number => "A number".into(),
        ParameterKind::Team => "A team".into(),
        ParameterKind::Resource => match parameter.kinds.is_empty() {
            true => "Something in the Catalogue".into(),
            false => format!("A {} from the Catalogue", parameter.kinds.join(" or ")),
        },
        ParameterKind::List => "One to a line".into(),
        ParameterKind::Textarea => "Some words".into(),
        ParameterKind::Text => "Text".into(),
    }
}

/// What each step will do. With the answers to hand, the summaries are filled in with them, so the
/// check page says `Put Service:payments-api in the Catalogue` rather than repeating the template.
fn doing(definition: &Definition, context: Option<&Value>) -> Vec<Doing> {
    definition
        .steps
        .iter()
        .map(|step| Doing {
            title: step.label(),
            action: step.action.kind().to_string(),
            summary: step.action.summary(context),
        })
        .collect()
}

fn rows(runs: &[Run]) -> Vec<RunRow> {
    runs.iter()
        .map(|run| RunRow {
            id: run.id,
            title: run.title.clone(),
            state: run.state.clone(),
            badge: badge(&run.state),
            lifecycle: run.lifecycle.title(),
            lifecycle_badge: run.lifecycle.badge(),
            by: run.requester_label.clone(),
            when: when(&run.created_at),
        })
        .collect()
}

/// An answer as a page shows it.
fn shown_value(parameter: &Parameter, value: &Value) -> String {
    if let Value::Array(items) = value {
        let items: Vec<String> = items.iter().map(written).collect();
        return match items.is_empty() {
            true => "Not given".into(),
            false => items.join(", "),
        };
    }
    match (parameter.kind, value) {
        (ParameterKind::Boolean, Value::Bool(yes)) => match yes {
            true => "Yes".into(),
            false => "No".into(),
        },
        (_, Value::Null) => "Not given".into(),
        (_, Value::String(text)) if text.is_empty() => "Not given".into(),
        (_, Value::String(text)) => text.clone(),
        (_, other) => other.to_string(),
    }
}

/// The answers as a page lists them. The lifecycle is left out: it is shown as what the creation
/// is, in its own row, rather than as one answer among the rest.
fn answers_of(definition: &Definition, values: &Value) -> Vec<Answer> {
    definition
        .asked()
        .iter()
        .filter(|parameter| parameter.name != model::LIFECYCLE)
        .map(|parameter| Answer {
            label: parameter.label(),
            value: shown_value(parameter, &values[&parameter.name]),
            page: parameter.page(),
        })
        .collect()
}

fn links_of(run: &Run) -> Vec<Link> {
    run.links
        .as_array()
        .map(|links| {
            links
                .iter()
                .filter_map(|link| {
                    Some(Link {
                        title: link["title"].as_str()?.to_string(),
                        url: link["url"].as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn marked(outcomes: &[Outcome]) -> Vec<Marked> {
    outcomes
        .iter()
        .map(|outcome| Marked {
            title: outcome.title.clone(),
            meta: match (&outcome.state, &outcome.finished_at, &outcome.started_at) {
                (state, Some(at), _) if state == "done" => format!("Done {}", clock(at)),
                (state, Some(at), _) if state == "failed" => format!("Failed {}", clock(at)),
                (state, _, Some(at)) if state == "running" => format!("Started {}", clock(at)),
                (state, _, _) if state == "skipped" => "Skipped".into(),
                _ => "Waiting".into(),
            },
            marker: marker(&outcome.state),
            detail: outcome.detail.clone(),
        })
        .collect()
}

fn said(lines: &[Line]) -> Vec<Said> {
    lines
        .iter()
        .map(|line| Said {
            at: clock(&line.at),
            level: line.level.clone(),
            text: line.text.clone(),
        })
        .collect()
}

pub async fn handle(backend: &Backend, request: Request) -> Response {
    match route(backend, &request).await {
        Ok(response) => response,
        Err(refusal) => refusal.response(),
    }
}

async fn route(backend: &Backend, request: &Request) -> Result<Response, Refusal> {
    let store = Store(backend);
    let author = backend.allows(AUTHOR, true);
    let writes = backend.writes();
    let path = request.path.trim_start_matches("ui").trim_matches('/');
    let parts: Vec<&str> = match path.is_empty() {
        true => Vec::new(),
        false => path.split('/').collect(),
    };
    match (request.method.as_str(), parts.as_slice()) {
        ("GET", []) => home(backend, api::query(request, "q"), Flash::default()).await,
        ("GET", ["t", name]) => template_page(backend, name, Flash::default()).await,
        ("GET", ["t", name, "new"]) => {
            let template = api::found(&store, name).await?;
            let definition = template.definition()?;
            let answers = Map::new();
            wizard(backend, &definition, &template.title, first(&definition), &answers, &[], author)
        }
        ("POST", ["t", name, "new"]) => launched(backend, name, request, author).await,
        ("GET", ["new"]) => {
            must_author(author)?;
            drawn(&DefinitionPage {
                flash: Flash::default(),
                author,
                editing: false,
                name: String::new(),
                title: String::new(),
                definition: seed::BUILT_IN[1].trim_start().to_string(),
                error: None,
            })
        }
        ("POST", ["new"]) => saved(backend, request, author, None).await,
        ("GET", ["t", name, "edit"]) => {
            must_author(author)?;
            let template = api::found(&store, name).await?;
            let definition = template.definition()?;
            drawn(&DefinitionPage {
                flash: Flash::default(),
                author,
                editing: true,
                name: template.name.clone(),
                title: template.title.clone(),
                definition: serde_yaml_ng::to_string(&definition).unwrap_or_default(),
                error: None,
            })
        }
        ("POST", ["t", name, "edit"]) => saved(backend, request, author, Some(name)).await,
        ("DELETE", ["t", name]) => {
            must_author(author)?;
            let removed = store.remove(name).await?;
            let flash = match removed {
                true => Flash::done(format!("{name} is gone.")),
                false => Flash::refused(format!("there is no template called {name}")),
            };
            home(backend, None, flash).await
        }
        ("GET", ["runs"]) => {
            let mine = api::query(request, "mine").is_some();
            let requester = mine.then(|| engine::requester(backend).0);
            let runs = store.runs(None, requester.as_deref(), None, 100).await?;
            drawn(&RunsPage { flash: Flash::default(), author, mine, runs: rows(&runs) })
        }
        ("GET", ["runs", id]) => run_page(backend, id, Flash::default()).await,
        ("GET", ["runs", id, "steps"]) => {
            let run = api::run_found(&store, id).await?;
            drawn(&StepsFragment { id: run.id, live: run.live(), steps: marked(&run.outcomes()) })
        }
        ("GET", ["runs", id, "log"]) => {
            let run = api::run_found(&store, id).await?;
            let (lines, more) = tail(&store, &run).await?;
            drawn(&LogFragment {
                id: run.id,
                live: run.live(),
                template: run.template.clone(),
                short: short(run.id),
                state: run.state.clone(),
                badge: badge(&run.state),
                lines: said(&lines),
                more,
            })
        }
        ("GET", ["runs", id, "download", format]) => api::download(backend, id, format).await,
        ("POST", ["runs", id, "cancel"]) => {
            if !writes {
                return Err(Refusal::forbidden("stopping a run needs plugin:templates:user:rw"));
            }
            let run = api::run_found(&store, id).await?;
            store.cancel(run.id).await?;
            run_page(backend, id, Flash::done("It will stop at its next step.")).await
        }
        _ => Ok(Response::not_found()),
    }
}

fn must_author(author: bool) -> Result<(), Refusal> {
    match author {
        true => Ok(()),
        false => {
            Err(Refusal::forbidden("writing templates needs plugin:templates:pluginuser:author:rw"))
        }
    }
}

async fn home(backend: &Backend, q: Option<String>, flash: Flash) -> Result<Response, Refusal> {
    let store = Store(backend);
    let templates = store.templates(q.as_deref()).await?;
    let cards = templates
        .iter()
        .map(|template| {
            let definition = template.definition().ok();
            let steps = definition.as_ref().map_or(0, |definition| definition.steps.len());
            let asks = definition.as_ref().map_or(0, |definition| definition.asked().len());
            let settled = definition.as_ref().and_then(|definition| definition.lifecycle);
            Card {
                name: template.name.clone(),
                title: template.title.clone(),
                description: template.description.clone(),
                tags: template.tags.clone(),
                lifecycle: settled
                    .map_or_else(|| "You choose".to_string(), |one| one.title().to_string()),
                lifecycle_badge: settled.map_or("unknown", Lifecycle::badge),
                footer: format!(
                    "{} · {asks} questions · {steps} steps",
                    template.kind.clone().unwrap_or_else(|| "Anything".into())
                ),
            }
        })
        .collect();
    drawn(&Home { flash, author: backend.allows(AUTHOR, true), q: q.unwrap_or_default(), cards })
}

async fn template_page(backend: &Backend, name: &str, flash: Flash) -> Result<Response, Refusal> {
    let store = Store(backend);
    let template = api::found(&store, name).await?;
    let definition = template.definition()?;
    let runs = store.runs(Some(&template.name), None, None, 10).await?;
    let origin = match (&definition.scaffold, &definition.source, definition.files.len()) {
        (Some(scaffold), _, _) => {
            match (scaffold.language.contains("{{"), scaffold.app.contains("{{")) {
                (false, false) => format!("The {} {} scaffold", scaffold.language, scaffold.app),
                (true, false) => format!("A {} scaffold, in the language you pick", scaffold.app),
                (false, true) => format!("A {} scaffold, of the kind you pick", scaffold.language),
                (true, true) => "A scaffold, of the language and kind you pick".to_string(),
            }
        }
        (None, Some(source), 0) => format!("From {}", source.repository),
        (None, Some(source), carried) => {
            format!("From {} and {carried} of its own", source.repository)
        }
        (None, None, 0) => "It writes none".to_string(),
        (None, None, carried) => format!("{carried} it carries itself"),
    };
    drawn(&TemplatePage {
        flash,
        author: backend.allows(AUTHOR, true),
        writes: backend.writes(),
        name: template.name.clone(),
        title: template.title.clone(),
        description: template.description.clone(),
        kind: template.kind.clone().unwrap_or_else(|| "Anything".into()),
        lifecycle: definition
            .lifecycle
            .map_or_else(|| "Whoever creates one says".to_string(), |one| one.title().to_string()),
        lifecycle_badge: definition.lifecycle.map_or("unknown", Lifecycle::badge),
        owner: template.owner.clone().unwrap_or_else(|| "Nobody in particular".into()),
        origin,
        parameters: definition
            .asked()
            .iter()
            .map(|parameter| Asked {
                label: parameter.label(),
                hint: parameter.hint.clone(),
                takes: takes(parameter),
                required: parameter.required,
            })
            .collect(),
        steps: doing(&definition, None),
        runs: rows(&runs),
    })
}

async fn run_page(backend: &Backend, id: &str, flash: Flash) -> Result<Response, Refusal> {
    let store = Store(backend);
    let run = api::run_found(&store, id).await?;
    let template = store.template(&run.template).await?;
    let changed = template.as_ref().is_some_and(|template| {
        run.template_version != 0 && template.version != run.template_version
    });
    let definition = template.and_then(|template| template.definition().ok());
    let downloads = run.state == "succeeded"
        && definition.as_ref().is_some_and(|definition| definition.makes_files())
        && api::downloads(backend, &run);
    let answers = definition
        .as_ref()
        .map(|definition| answers_of(definition, &run.answers))
        .unwrap_or_default();
    let (lines, more) = tail(&store, &run).await?;
    drawn(&RunPage {
        flash,
        author: backend.allows(AUTHOR, true),
        writes: backend.writes(),
        id: run.id,
        short: short(run.id),
        template: run.template.clone(),
        title: run.title.clone(),
        state: run.state.clone(),
        badge: badge(&run.state),
        lifecycle: run.lifecycle.title(),
        lifecycle_badge: run.lifecycle.badge(),
        lifecycle_about: run.lifecycle.about(),
        live: run.live(),
        by: run.requester_label.clone(),
        started: when(run.started_at.as_deref().unwrap_or(&run.created_at)),
        finished: run.finished_at.as_deref().map(when),
        error: run.error.clone(),
        answers,
        links: links_of(&run),
        downloads,
        changed,
        steps: marked(&run.outcomes()),
        lines: said(&lines),
        more,
    })
}

/// A size as somebody reads it rather than as it is counted.
fn sized(bytes: usize) -> String {
    match bytes {
        0 => "empty".to_string(),
        1 => "1 byte".to_string(),
        bytes if bytes < 1_000 => format!("{bytes} bytes"),
        bytes if bytes < 1_000_000 => format!("{:.1} kB", bytes as f64 / 1_000.0),
        bytes => format!("{:.1} MB", bytes as f64 / 1_000_000.0),
    }
}

fn counted(files: usize) -> String {
    match files {
        1 => "1 file".to_string(),
        files => format!("{files} files"),
    }
}

/// The rendered files as the tree draws them: what each directory holds, then what sits loose in
/// the root. Directories come first, as a repository lists them.
fn foldered(made: &[files::Rendered]) -> (Vec<Folder>, Vec<Preview>, String) {
    let shown = |file: &files::Rendered, name: &str| Preview {
        path: file.path.clone(),
        name: name.to_string(),
        size: sized(file.size()),
        body: file.preview(PREVIEW_LINES),
    };
    let mut directories: Vec<Folder> = Vec::new();
    let mut loose: Vec<Preview> = Vec::new();
    for file in made {
        match file.path.rsplit_once('/') {
            None => loose.push(shown(file, &file.path)),
            Some((directory, name)) => {
                let held = shown(file, name);
                match directories.iter_mut().find(|folder| folder.name == directory) {
                    Some(folder) => folder.files.push(held),
                    None => directories.push(Folder {
                        name: directory.to_string(),
                        count: String::new(),
                        files: vec![held],
                    }),
                }
            }
        }
    }
    directories.sort_by(|one, two| one.name.cmp(&two.name));
    for folder in &mut directories {
        folder.count = counted(folder.files.len());
    }
    let bytes: usize = made.iter().map(files::Rendered::size).sum();
    (directories, loose, format!("{} · {}", counted(made.len()), sized(bytes)))
}

fn short(id: Uuid) -> String {
    id.to_string().chars().take(8).collect()
}

/// The end of a run's log, and how many lines are only in the API.
async fn tail(store: &Store<'_>, run: &Run) -> Result<(Vec<Line>, i64), Refusal> {
    let next = store.next_line(run.id).await?;
    let from = (next - TAIL).max(0);
    let lines = store.lines(run.id, from, TAIL as u32).await?;
    Ok((lines, from))
}

/// Saves what an author wrote, keeping the page open with the problem when it is not a template.
async fn saved(
    backend: &Backend,
    request: &Request,
    author: bool,
    editing: Option<&str>,
) -> Result<Response, Refusal> {
    must_author(author)?;
    let form = form(request);
    let written = field(&form, "definition").unwrap_or_default();
    let store = Store(backend);
    match model::definition(&written) {
        Err(problem) => drawn(&DefinitionPage {
            flash: Flash::default(),
            author,
            editing: editing.is_some(),
            name: editing.unwrap_or_default().to_string(),
            title: editing.unwrap_or_default().to_string(),
            definition: written,
            error: Some(problem),
        }),
        Ok(definition) => {
            if let Some(name) = editing
                && name != definition.name
            {
                return drawn(&DefinitionPage {
                    flash: Flash::default(),
                    author,
                    editing: true,
                    name: name.to_string(),
                    title: name.to_string(),
                    definition: written,
                    error: Some(format!(
                        "this is {name}: to make a template of another name, write a new one"
                    )),
                });
            }
            let (who, _) = engine::requester(backend);
            let template = store.save(&definition, &who, false).await?;
            backend.audit("template.saved", Some(&template.name), json!({ "by": who })).await.ok();
            template_page(
                backend,
                &template.name,
                Flash::done(format!("{} is saved.", template.title)),
            )
            .await
        }
    }
}

/// The first page of the wizard, which is the first group its parameters name.
fn first(definition: &Definition) -> String {
    definition.pages().first().cloned().unwrap_or_else(|| CHECK.to_string())
}

/// Every page of the wizard, ending with the check.
fn pages(definition: &Definition) -> Vec<String> {
    let mut pages = definition.pages();
    pages.push(CHECK.to_string());
    pages
}

/// One page of the wizard, with what has been answered so far carried in hidden fields.
fn wizard(
    backend: &Backend,
    definition: &Definition,
    title: &str,
    page: String,
    answers: &Map<String, Value>,
    problems: &[model::Problem],
    author: bool,
) -> Result<Response, Refusal> {
    let all = pages(definition);
    let at = all.iter().position(|one| *one == page).unwrap_or(0);
    let back = (at > 0).then(|| all[at - 1].clone());
    let on_page = definition.on_page(&page);
    let hidden: Vec<(String, String)> = definition
        .asked()
        .iter()
        .filter(|parameter| !on_page.iter().any(|shown| shown.name == parameter.name))
        .map(|parameter| (parameter.name.clone(), raw(answers, parameter)))
        .collect();
    let fields = on_page
        .iter()
        .map(|parameter| {
            let kind = match parameter.kind {
                ParameterKind::Textarea | ParameterKind::List => "textarea",
                ParameterKind::Select => "select",
                ParameterKind::Boolean => "boolean",
                ParameterKind::Number => "number",
                ParameterKind::Team | ParameterKind::Resource => "resource",
                ParameterKind::Text => "text",
            };
            let value = raw(answers, parameter);
            let kinds = match parameter.kind {
                ParameterKind::Team => "Team".to_string(),
                _ => parameter.kinds.join(","),
            };
            Field {
                name: parameter.name.clone(),
                id: format!("field-{}", parameter.name),
                label: parameter.label(),
                hint: parameter.hint.clone(),
                kind: kind.to_string(),
                checked: matches!(value.as_str(), "on" | "true" | "yes" | "1"),
                value,
                options: parameter
                    .options
                    .iter()
                    .map(|choice| (choice.value().to_string(), choice.title().to_string()))
                    .collect(),
                error: problems
                    .iter()
                    .find(|problem| problem.parameter == parameter.name)
                    .map(|problem| problem.detail.clone()),
                placeholder: parameter.placeholder.clone().unwrap_or_default(),
                kinds,
                required: parameter.required,
            }
        })
        .collect();
    let _ = backend;
    drawn(&FormPage {
        flash: Flash::default(),
        author,
        name: definition.name.clone(),
        title: title.to_string(),
        page,
        number: at + 1,
        of: all.len(),
        back,
        hidden,
        fields,
    })
}

/// A value as a page writes it, which for a list is its items.
fn written(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// What a field holds now: what was answered, or the parameter's default.
fn raw(answers: &Map<String, Value>, parameter: &Parameter) -> String {
    match answers.get(&parameter.name) {
        Some(Value::Array(items)) => items.iter().map(written).collect::<Vec<_>>().join("\n"),
        Some(Value::String(text)) => text.clone(),
        Some(Value::Bool(yes)) => match yes {
            true => "on".into(),
            false => String::new(),
        },
        Some(Value::Null) | None => match &parameter.default {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Bool(true)) => "on".into(),
            Some(Value::Null) | Some(Value::Bool(false)) | None => String::new(),
            Some(other) => other.to_string(),
        },
        Some(other) => other.to_string(),
    }
}

/// A page of the wizard was submitted: go back, go on, or create it.
async fn launched(
    backend: &Backend,
    name: &str,
    request: &Request,
    author: bool,
) -> Result<Response, Refusal> {
    let store = Store(backend);
    let template = api::found(&store, name).await?;
    let definition = template.definition()?;
    let submitted = form(request);
    let mut answers = Map::new();
    for parameter in &definition.asked() {
        if let Some(value) = field(&submitted, &parameter.name) {
            answers.insert(parameter.name.clone(), Value::String(value));
        } else if parameter.kind == ParameterKind::Boolean {
            answers.insert(parameter.name.clone(), Value::String(String::new()));
        }
    }
    let page = field(&submitted, "page").unwrap_or_else(|| first(&definition));
    let go = field(&submitted, "go").unwrap_or_else(|| "next".to_string());
    let all = pages(&definition);

    // Going back, or to a page the check offered to change: nothing is checked on the way.
    if go != "next" && go != "launch" && all.contains(&go) {
        return wizard(backend, &definition, &template.title, go, &answers, &[], author);
    }

    if go == "launch" {
        if !backend.writes() {
            return Err(Refusal::forbidden(
                "creating from a template needs plugin:templates:user:rw",
            ));
        }
        return match model::values(&definition.asked(), &answers) {
            Err(problems) => {
                let first_page = problems
                    .first()
                    .and_then(|problem| definition.parameter(&problem.parameter))
                    .map(|parameter| parameter.page())
                    .unwrap_or_else(|| first(&definition));
                wizard(
                    backend,
                    &definition,
                    &template.title,
                    first_page,
                    &answers,
                    &problems,
                    author,
                )
            }
            Ok(values) => {
                let run = engine::launch(backend, &template, &definition, values).await?;
                let page = run_page(
                    backend,
                    &run.id.to_string(),
                    Flash::done(format!("{} is being created.", template.title)),
                )
                .await?;
                Ok(page.with_header("hx-push-url", &format!("/p/templates/runs/{}", run.id)))
            }
        };
    }

    // Going on: only what this page asked is checked, so somebody is not stopped by a later page.
    let on_page = definition.on_page(&page);
    if let Err(problems) = model::values(&on_page, &answers) {
        return wizard(backend, &definition, &template.title, page, &answers, &problems, author);
    }
    let at = all.iter().position(|one| *one == page).unwrap_or(0);
    let next = all.get(at + 1).cloned().unwrap_or_else(|| CHECK.to_string());
    match next == CHECK {
        false => wizard(backend, &definition, &template.title, next, &answers, &[], author),
        true => review(backend, &definition, &template.title, &answers, author).await,
    }
}

/// The last page: every answer, what each step will do, and the files as they will be committed.
async fn review(
    backend: &Backend,
    definition: &Definition,
    title: &str,
    answers: &Map<String, Value>,
    author: bool,
) -> Result<Response, Refusal> {
    let all = pages(definition);
    let checked = model::values(&definition.asked(), answers);
    let values = match checked {
        Ok(values) => values,
        Err(problems) => {
            let page = problems
                .first()
                .and_then(|problem| definition.parameter(&problem.parameter))
                .map(|parameter| parameter.page())
                .unwrap_or_else(|| first(definition));
            return wizard(backend, definition, title, page, answers, &problems, author);
        }
    };
    let lifecycle = definition.lifecycle_of(&Value::Object(values.clone()));
    let context = engine::proposed(backend, definition, title, &values);
    let (made, preview_problem) = match files::assemble(backend, definition, &context).await {
        Ok(made) => (made, None),
        Err(problem) => (Vec::new(), Some(problem)),
    };
    let (directories, loose, total) = foldered(&made);
    let hidden: Vec<(String, String)> = definition
        .asked()
        .iter()
        .map(|parameter| (parameter.name.clone(), raw(answers, parameter)))
        .collect();
    drawn(&ReviewPage {
        flash: Flash::default(),
        author,
        name: definition.name.clone(),
        title: title.to_string(),
        page: CHECK.to_string(),
        number: all.len(),
        of: all.len(),
        back: (all.len() > 1).then(|| all[all.len() - 2].clone()),
        hidden,
        answers: answers_of(definition, &Value::Object(values)),
        directories,
        loose,
        total,
        lifecycle: lifecycle.title(),
        lifecycle_badge: lifecycle.badge(),
        lifecycle_page: definition.lifecycle.is_none().then(|| Lifecycle::question().page()),
        steps: doing(definition, Some(&context)),
        preview_problem,
    })
}
