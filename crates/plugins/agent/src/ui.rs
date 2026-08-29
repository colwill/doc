//! The pages: your jobs, the runbooks you can run, your reminders and how to connect your own
//! agent, each with the button that adds to it; a page per job with its runs, a page per runbook
//! with its approval and runs, and a page per run with its report and calls.

use askama::Template;
use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::json;
use uuid::Uuid;

use crate::jobs::{self, Asker, DELIVER, EVENTS, NewJob, SCOPES};
use crate::settings::Config;
use crate::store::{
    AGENT, DONE, ENVIRONMENTS, FAILED, Job, QUEUED, REMINDERS, REQUESTER, RUNNING, Run, Store,
};
use crate::{Refusal, claude, parameter, playbooks, runbooks, tools};

type Form = Vec<(String, String)>;
type Page = Result<String, Refusal>;
type Drawing<'a> = std::pin::Pin<Box<dyn Future<Output = Page> + Send + 'a>>;

fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> String {
    form.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .unwrap_or_default()
}

fn fields(form: &Form, name: &str) -> Vec<String> {
    form.iter().filter(|(key, _)| key == name).map(|(_, value)| value.trim().to_string()).collect()
}

fn id(text: &str) -> Result<Uuid, Refusal> {
    text.parse().map_err(|_| Refusal::bad("that is not an ID"))
}

fn when(at: DateTime<Utc>) -> String {
    at.format("%-d %b %Y, %H:%M UTC").to_string()
}

/// A report as a page shows it: Markdown, with raw HTML dropped and what is left cleaned, and its
/// headings moved three levels down so they sit under the page's own.
fn prose(markdown: &str) -> String {
    let mut options = comrak::Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.tasklist = true;
    options.extension.autolink = true;
    let mut html = ammonia::clean(&comrak::markdown_to_html(markdown, &options));
    // From the lowest level up, so no heading is moved twice.
    for level in (1..=6).rev() {
        let deeper = (level + 3).min(6);
        html = html.replace(&format!("<h{level}"), &format!("<h{deeper}"));
        html = html.replace(&format!("</h{level}>"), &format!("</h{deeper}>"));
    }
    html
}

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

fn render<T: Template>(page: &T) -> Page {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

/// When a job runs, in words.
fn trigger(job: &Job) -> String {
    match job.trigger.as_str() {
        "schedule" => format!("On the schedule {}", job.cron),
        "event" => {
            let said: Vec<&str> = EVENTS
                .iter()
                .filter(|(topic, _)| job.events.iter().any(|event| event == topic))
                .map(|(_, label)| *label)
                .collect();
            format!("When {}", said.join(", or ").to_lowercase())
        }
        _ => "When asked".to_string(),
    }
}

fn badge(state: &str) -> (&'static str, &'static str) {
    match state {
        DONE => ("Done", "ready"),
        FAILED => ("Failed", "error"),
        RUNNING => ("Running", "loading"),
        QUEUED => ("Waiting", "loading"),
        _ => ("Stopped", "unknown"),
    }
}

pub struct JobRow {
    pub href: String,
    pub title: String,
    pub playbook: &'static str,
    pub when: String,
    pub owner: String,
    pub last: Option<(String, &'static str, &'static str)>,
    pub enabled: bool,
}

pub struct ReminderRow {
    pub id: Uuid,
    pub at: String,
    pub message: String,
    pub to: String,
}

pub struct Named {
    pub name: String,
    pub about: String,
}

pub struct RunbookRow {
    pub href: String,
    pub title: String,
    pub space: String,
    pub environment: String,
    /// Whether it may run by itself, in words, and whether the page still says what was approved.
    pub approval: Option<(String, bool)>,
}

/// A runbook's run, as the lists of them show it.
pub struct AskedRow {
    pub href: String,
    pub runbook: String,
    pub started: String,
    pub environment: String,
    pub who: String,
    pub state: &'static str,
    pub colour: &'static str,
}

/// The environments a runbook can be run against, one of them chosen.
pub struct Environment {
    pub value: &'static str,
    pub label: String,
    pub selected: bool,
}

fn environments(chosen: &str) -> Vec<Environment> {
    ENVIRONMENTS
        .iter()
        .map(|value| Environment {
            value,
            label: format!("{}{}", value[..1].to_uppercase(), &value[1..]),
            selected: *value == chosen,
        })
        .collect()
}

fn asked_row(run: &Run) -> AskedRow {
    let (state, colour) = badge(&run.state);
    AskedRow {
        href: run.href(),
        runbook: run.runbook_title.clone(),
        started: run.created_at.map(when).unwrap_or_default(),
        environment: run.environment.clone(),
        who: match run.access.as_str() {
            AGENT => format!("Agent Smith, for {}", run.owner_label),
            _ => run.owner_label.clone(),
        },
        state,
        colour,
    }
}

/// What a runbook approved to run by itself says of that, as a list or its page shows it.
fn approval_words(runbook: &serde_json::Value) -> Option<(String, bool)> {
    let approval = &runbook["approval"];
    if approval.is_null() {
        return None;
    }
    let at = approval["at"].as_str().and_then(|at| at.parse::<DateTime<Utc>>().ok());
    let said = format!(
        "Approved against {} by {}{}",
        approval["environment"].as_str().unwrap_or_default(),
        approval["by"].as_str().unwrap_or_default(),
        at.map(|at| format!(", {}", when(at))).unwrap_or_default(),
    );
    Some((said, approval["hash"] == runbook["hash"] || runbook["hash"].is_null()))
}

#[derive(Template)]
#[template(path = "home.html")]
struct Home {
    flash: Flash,
    section: &'static str,
    writes: bool,
    admin: bool,
    jobs: Vec<JobRow>,
    reminders: Vec<ReminderRow>,
    runbooks: Vec<RunbookRow>,
    asked: Vec<AskedRow>,
    url: String,
    tools: Vec<Named>,
    prompts: Vec<Named>,
    keyless: bool,
    /// Credentials the sources name that nobody has added, so `doc_fetch` cannot use them.
    unmet: Vec<String>,
}

/// Boxed, so every route awaiting it holds a pointer rather than the whole page's state.
fn home<'a>(
    backend: &'a Backend,
    asker: &'a Asker,
    section: &'static str,
    flash: Flash,
) -> Drawing<'a> {
    Box::pin(drawn_home(backend, asker, section, flash))
}

async fn drawn_home(backend: &Backend, asker: &Asker, section: &'static str, flash: Flash) -> Page {
    let store = Store(backend);
    let config = Config::read(&backend.settings());
    let mut jobs = Vec::new();
    let mut reminders = Vec::new();
    let mut tools_offered = Vec::new();
    let mut prompts = Vec::new();
    let mut listed = Vec::new();
    let mut asked = Vec::new();
    match section {
        "runbooks" => {
            listed = runbooks::shown(backend)
                .await?
                .iter()
                .map(|runbook| RunbookRow {
                    href: runbooks::href(&runbooks::text(runbook, "key")),
                    title: runbooks::text(runbook, "title"),
                    space: runbooks::text(runbook, "space_name"),
                    environment: runbooks::text(runbook, "environment"),
                    approval: approval_words(runbook),
                })
                .collect();
            let mut filter = json!({ "runbook": { "is_null": false } });
            if !asker.admin {
                filter["owner"] = json!(asker.id);
            }
            asked = store.runs(filter, 50).await?.iter().map(asked_row).collect();
        }
        "reminders" => {
            let filter = json!({ "by": asker.id, "delivered_at": { "is_null": true } });
            reminders = store
                .reminders(filter)
                .await?
                .into_iter()
                .map(|reminder| ReminderRow {
                    id: reminder.id,
                    at: when(reminder.at),
                    message: reminder.message,
                    to: reminder.to_label,
                })
                .collect();
        }
        "connect" => {
            tools_offered = tools::available(backend, false)
                .await
                .iter()
                .map(|tool| Named {
                    name: tool["name"].as_str().unwrap_or_default().to_string(),
                    about: tool["description"].as_str().unwrap_or_default().to_string(),
                })
                .collect();
            prompts = playbooks::ALL
                .iter()
                .map(|playbook| Named {
                    name: playbook.name.to_string(),
                    about: playbook.about.to_string(),
                })
                .collect();
        }
        _ => {
            let owner = (!asker.admin).then_some(asker.id);
            let mut held = store.jobs(owner).await?;
            held.sort_by_key(|job| job.title.to_lowercase());
            for job in held {
                let last =
                    store.runs(json!({ "job": job.id }), 1).await?.into_iter().next().map(|run| {
                        let (word, colour) = badge(&run.state);
                        (run.created_at.map(when).unwrap_or_default(), word, colour)
                    });
                jobs.push(JobRow {
                    href: job.href(),
                    playbook: playbooks::named(&job.playbook)
                        .map_or("Custom", |playbook| playbook.title),
                    when: trigger(&job),
                    owner: job.owner_label.clone(),
                    title: job.title.clone(),
                    last,
                    enabled: job.enabled,
                });
            }
        }
    }
    render(&Home {
        flash,
        section,
        writes: asker.writes,
        admin: asker.admin,
        jobs,
        reminders,
        runbooks: listed,
        asked,
        url: format!("{}/api/v1/plugins/{}/api/mcp", config.api_url, crate::ID),
        tools: tools_offered,
        prompts,
        keyless: config.api_key.is_none(),
        unmet: config.unmet,
    })
}

pub struct Choice {
    pub value: String,
    pub label: String,
    pub about: String,
    pub selected: bool,
}

#[derive(Template)]
#[template(path = "job_form.html")]
struct JobForm {
    flash: Flash,
    editing: Option<Uuid>,
    title: String,
    playbooks: Vec<Choice>,
    brief: String,
    scope_kinds: Vec<Choice>,
    scope_name: String,
    trigger: String,
    cron: String,
    events: Vec<Choice>,
    deliver: Vec<Choice>,
    enabled: bool,
    /// The runbooks approved to run by itself, for somebody who may set one; empty for others.
    runbooks: Vec<Choice>,
}

/// The runbooks approved to run by themselves, offered to somebody holding `runbooks`.
async fn approved_choices(backend: &Backend, chosen: &str) -> Result<Vec<Choice>, Refusal> {
    if !backend.allows(runbooks::PERMISSION, true) {
        return Ok(Vec::new());
    }
    Ok(Store(backend)
        .approvals()
        .await?
        .into_iter()
        .map(|approval| Choice {
            selected: approval.runbook == chosen,
            label: format!("{} (against {})", approval.title, approval.environment),
            about: approval.runbook.clone(),
            value: approval.runbook,
        })
        .collect())
}

/// The job's form, filled from what was sent, or from the job, or blank.
async fn job_form(backend: &Backend, editing: Option<&Job>, given: &Form, flash: Flash) -> Page {
    let sent = !given.is_empty();
    let from = |name: &str, held: String| if sent { field(given, name) } else { held };
    let playbook = from(
        "playbook",
        editing.map(|job| job.playbook.clone()).unwrap_or_else(|| "health".into()),
    );
    let (kind, name) = match editing.and_then(|job| job.scope.split_once(':')) {
        Some((kind, name)) if !sent => (kind.to_string(), name.to_string()),
        _ => (field(given, "scope_kind"), field(given, "scope_name")),
    };
    let events: Vec<String> = match (sent, editing) {
        (true, _) => fields(given, "events"),
        (false, Some(job)) => job.events.clone(),
        _ => Vec::new(),
    };
    let deliver: Vec<String> = match (sent, editing) {
        (true, _) => fields(given, "deliver"),
        (false, Some(job)) => job.deliver.clone(),
        _ => vec!["notify".to_string()],
    };
    let runbook = from("runbook", editing.map(|job| job.runbook.clone()).unwrap_or_default());
    let runbooks = approved_choices(backend, &runbook).await?;
    render(&JobForm {
        flash,
        editing: editing.map(|job| job.id),
        title: from("title", editing.map(|job| job.title.clone()).unwrap_or_default()),
        playbooks: playbooks::ALL
            .iter()
            // A runbook's job runs by itself, which only somebody holding `runbooks` sets.
            .filter(|choice| choice.name != playbooks::RUNBOOK || !runbooks.is_empty())
            .map(|choice| Choice {
                selected: choice.name == playbook,
                value: choice.name.into(),
                label: choice.title.into(),
                about: choice.about.into(),
            })
            .collect(),
        brief: from("brief", editing.map(|job| job.brief.clone()).unwrap_or_default()),
        scope_kinds: SCOPES
            .iter()
            .map(|scope| Choice {
                selected: *scope == kind,
                value: scope.to_string(),
                label: scope.to_string(),
                about: String::new(),
            })
            .collect(),
        scope_name: name,
        trigger: from(
            "trigger",
            editing.map(|job| job.trigger.clone()).unwrap_or_else(|| "manual".into()),
        ),
        cron: from(
            "cron",
            editing.map(|job| job.cron.clone()).unwrap_or_else(|| "0 8 * * 1".into()),
        ),
        events: EVENTS
            .iter()
            .map(|(topic, label)| Choice {
                selected: events.iter().any(|event| event == topic),
                value: topic.to_string(),
                label: label.to_string(),
                about: topic.to_string(),
            })
            .collect(),
        deliver: DELIVER
            .iter()
            .map(|(how, label)| Choice {
                selected: deliver.iter().any(|held| held == how),
                value: how.to_string(),
                label: label.to_string(),
                about: String::new(),
            })
            .collect(),
        enabled: match (sent, editing) {
            (true, _) => field(given, "enabled") == "on",
            (false, Some(job)) => job.enabled,
            _ => true,
        },
        runbooks,
    })
}

fn new_job(given: &Form, editing: bool) -> NewJob {
    let (kind, name) = (field(given, "scope_kind"), field(given, "scope_name"));
    NewJob {
        title: field(given, "title"),
        playbook: field(given, "playbook"),
        brief: field(given, "brief"),
        scope: match (kind.is_empty(), name.is_empty()) {
            (false, false) => format!("{kind}:{name}"),
            _ => String::new(),
        },
        trigger: field(given, "trigger"),
        cron: field(given, "cron"),
        events: fields(given, "events"),
        deliver: fields(given, "deliver"),
        enabled: !editing || field(given, "enabled") == "on",
        runbook: field(given, "runbook"),
    }
}

pub struct RunRow {
    pub href: String,
    pub started: String,
    pub why: String,
    pub state: &'static str,
    pub colour: &'static str,
    pub turns: i64,
    pub tokens: String,
}

#[derive(Template)]
#[template(path = "job.html")]
struct JobPage {
    flash: Flash,
    section: &'static str,
    writes: bool,
    job: Job,
    playbook: &'static str,
    when: String,
    deliver: Vec<&'static str>,
    runs: Vec<RunRow>,
    /// A runbook's job: the runbook's page in Agent Smith, and its title.
    runbook: Option<(String, String)>,
}

fn tokens(run: &Run) -> String {
    let total = run.input_tokens + run.output_tokens;
    match total {
        0 => String::new(),
        total => format!("{total} ({} in, {} out)", run.input_tokens, run.output_tokens),
    }
}

/// Boxed, so every route awaiting it holds a pointer rather than the whole page's state.
fn job_page<'a>(
    backend: &'a Backend,
    asker: &'a Asker,
    job: Uuid,
    section: &'static str,
    flash: Flash,
) -> Drawing<'a> {
    Box::pin(drawn_job_page(backend, asker, job, section, flash))
}

async fn drawn_job_page(
    backend: &Backend,
    asker: &Asker,
    job: Uuid,
    section: &'static str,
    flash: Flash,
) -> Page {
    let store = Store(backend);
    let job = store.job(job).await?;
    if !asker.sees(&job) {
        return Err(Refusal::forbidden("only its owner sees a job"));
    }
    let runs = match section {
        "runs" => store
            .runs(json!({ "job": job.id }), 100)
            .await?
            .iter()
            .map(|run| {
                let (state, colour) = badge(&run.state);
                RunRow {
                    href: run.href(),
                    started: run.created_at.map(when).unwrap_or_default(),
                    why: run.why.clone(),
                    state,
                    colour,
                    turns: run.turns,
                    tokens: tokens(run),
                }
            })
            .collect(),
        _ => Vec::new(),
    };
    let runbook = match job.playbook.as_str() {
        playbooks::RUNBOOK => {
            let title = store.approval(&job.runbook).await?.map(|approval| approval.title);
            Some((runbooks::href(&job.runbook), title.unwrap_or_else(|| job.runbook.clone())))
        }
        _ => None,
    };
    render(&JobPage {
        flash,
        section,
        runbook,
        writes: asker.writes,
        playbook: playbooks::named(&job.playbook).map_or("Custom", |playbook| playbook.title),
        when: trigger(&job),
        deliver: DELIVER
            .iter()
            .filter(|(how, _)| job.deliver.iter().any(|held| held == how))
            .map(|(_, label)| *label)
            .collect(),
        runs,
        job,
    })
}

pub struct CallRow {
    pub turn: i64,
    pub tool: String,
    pub input: String,
    pub result: String,
    pub failed: bool,
    pub done: bool,
}

#[derive(Template)]
#[template(path = "run.html")]
struct RunPage {
    flash: Flash,
    run: Run,
    job: Option<Job>,
    /// What it is called: its job's title, or its runbook's.
    title: String,
    /// Whose access its calls were made with, in words.
    access: String,
    state: &'static str,
    colour: &'static str,
    started: String,
    finished: String,
    tokens: String,
    report: String,
    events: Vec<String>,
    calls: Vec<CallRow>,
    going: bool,
}

/// Boxed, so every route awaiting it holds a pointer rather than the whole page's state.
fn run_page<'a>(backend: &'a Backend, asker: &'a Asker, run: Uuid, flash: Flash) -> Drawing<'a> {
    Box::pin(drawn_run_page(backend, asker, run, flash))
}

async fn drawn_run_page(backend: &Backend, asker: &Asker, run: Uuid, flash: Flash) -> Page {
    let store = Store(backend);
    let run = store.run(run).await?;
    let job = match run.job {
        Some(job) => Some(store.job(job).await?),
        None => None,
    };
    if !asker.sees_run(&run, job.as_ref()) {
        return Err(Refusal::forbidden("only whoever asked for a run sees it"));
    }
    let title = run.title(job.as_ref());
    let access = match (run.access.as_str(), &job) {
        (AGENT, _) => format!("Agent Smith's own, as {}", crate::ACCOUNT),
        (REQUESTER, _) => run.owner_label.clone(),
        (_, Some(job)) => job.owner_label.clone(),
        (_, None) => String::new(),
    };
    let calls = store
        .calls(run.id)
        .await?
        .into_iter()
        .map(|call| CallRow {
            turn: call.turn,
            input: serde_json::to_string_pretty(&call.input)
                .unwrap_or_default()
                .chars()
                .take(4_000)
                .collect(),
            result: call.result.chars().take(4_000).collect(),
            tool: call.tool,
            failed: call.failed,
            done: call.done,
        })
        .collect();
    let events = run
        .events
        .as_array()
        .into_iter()
        .flatten()
        .map(|event| {
            format!(
                "{} at {}",
                event["topic"].as_str().unwrap_or_default(),
                event["at"].as_str().unwrap_or_default()
            )
        })
        .collect();
    let (state, colour) = badge(&run.state);
    render(&RunPage {
        flash,
        title,
        access,
        state,
        colour,
        started: run.started_at.map(when).unwrap_or_default(),
        finished: run.finished_at.map(when).unwrap_or_default(),
        tokens: tokens(&run),
        report: prose(&run.report),
        events,
        calls,
        going: run.state == QUEUED || run.state == RUNNING,
        run,
        job,
    })
}

#[derive(Template)]
#[template(path = "runbook.html")]
struct RunbookPage {
    flash: Flash,
    key: String,
    space_key: String,
    path: String,
    title: String,
    href: String,
    space: String,
    environments: Vec<Environment>,
    writes: bool,
    /// Whether the viewer holds `runbooks`, and so approves it and withdraws approval.
    approves: bool,
    approval: Option<(String, bool)>,
    runs: Vec<AskedRow>,
}

/// Boxed, so every route awaiting it holds a pointer rather than the whole page's state.
fn runbook_page<'a>(
    backend: &'a Backend,
    asker: &'a Asker,
    key: String,
    flash: Flash,
) -> Drawing<'a> {
    Box::pin(drawn_runbook_page(backend, asker, key, flash))
}

async fn drawn_runbook_page(backend: &Backend, asker: &Asker, key: String, flash: Flash) -> Page {
    let store = Store(backend);
    let mut runbook = runbooks::read(backend, &key).await?;
    runbook["approval"] = match store.approval(&key).await? {
        Some(approval) => json!({
            "environment": approval.environment, "hash": approval.hash,
            "by": approval.approved_by_label, "at": approval.approved_at,
        }),
        None => serde_json::Value::Null,
    };
    let mut filter = json!({ "runbook": key });
    if !asker.admin {
        filter["owner"] = json!(asker.id);
    }
    let runs = store.runs(filter, 50).await?.iter().map(asked_row).collect();
    render(&RunbookPage {
        flash,
        space_key: runbooks::text(&runbook, "space"),
        path: runbooks::text(&runbook, "path"),
        title: runbooks::text(&runbook, "title"),
        href: runbooks::text(&runbook, "href"),
        space: runbooks::text(&runbook, "space_name"),
        environments: environments(&runbooks::text(&runbook, "environment")),
        writes: asker.writes,
        approves: backend.allows(runbooks::PERMISSION, true),
        approval: approval_words(&runbook),
        runs,
        key,
    })
}

/// A run somebody asked for, as their dashboard lists it.
pub struct DashboardRun {
    pub href: String,
    pub title: String,
    pub when: String,
    pub state: &'static str,
    pub colour: &'static str,
}

/// What Agent Smith did for a person lately, and their reminders still to come.
#[derive(Template)]
#[template(path = "dashboard.html")]
struct DashboardFragment {
    runs: Vec<DashboardRun>,
    reminders: Vec<ReminderRow>,
}

#[derive(Template)]
#[template(path = "reminder_form.html")]
struct ReminderForm {
    flash: Flash,
    at: String,
    message: String,
    to: String,
}

fn reminder_form(given: &Form, flash: Flash) -> Page {
    render(&ReminderForm {
        flash,
        at: field(given, "at"),
        message: field(given, "message"),
        to: match field(given, "to") {
            none if none.is_empty() => "me".into(),
            some => some,
        },
    })
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let mut moved = None;
    match Box::pin(route(backend, request, path, &mut moved)).await {
        Ok(html) => match moved {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(&refusal) };
            let html = page.render().unwrap_or_else(|_| refusal.detail.clone());
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

async fn route(
    backend: &Backend,
    request: &Request,
    path: &[&str],
    moved: &mut Option<String>,
) -> Page {
    let asker = Asker::of(backend)?;
    let given = form(request);
    let mut went = |url: String| *moved = Some(url);
    let store = Store(backend);
    match (request.method.as_str(), path) {
        ("GET", []) => home(backend, &asker, "jobs", Flash::default()).await,
        ("GET", ["reminders"]) => home(backend, &asker, "reminders", Flash::default()).await,
        ("GET", ["dashboard"]) => {
            let mut rows = Vec::new();
            for run in store.runs(json!({ "owner": asker.id }), 5).await? {
                let job = match run.job {
                    Some(job) => store.job(job).await.ok(),
                    None => None,
                };
                let (state, colour) = badge(&run.state);
                rows.push(DashboardRun {
                    href: run.href(),
                    title: run.title(job.as_ref()),
                    when: run.created_at.map(when).unwrap_or_default(),
                    state,
                    colour,
                });
            }
            let filter = json!({ "by": asker.id, "delivered_at": { "is_null": true } });
            let reminders = store
                .reminders(filter)
                .await?
                .into_iter()
                .take(3)
                .map(|reminder| ReminderRow {
                    id: reminder.id,
                    at: when(reminder.at),
                    message: reminder.message,
                    to: reminder.to_label,
                })
                .collect();
            render(&DashboardFragment { runs: rows, reminders })
        }
        ("GET", ["runbooks"]) => home(backend, &asker, "runbooks", Flash::default()).await,
        ("POST", ["runbooks", "run"]) => {
            let key = runbooks::key(&field(&given, "space"), &field(&given, "path"));
            let run =
                runbooks::requested(backend, &asker, &key, &field(&given, "environment"), None)
                    .await?;
            went(run.href());
            let said = "It is running with your access. This page shows what it does as it goes; \
                        reload it to see more. Its report comes to your inbox too.";
            run_page(backend, &asker, run.id, Flash::done(said)).await
        }
        ("POST", ["runbooks", change @ ("approve" | "withdraw")]) => {
            let key = field(&given, "runbook");
            let flash = match *change {
                "approve" => {
                    match runbooks::approve(backend, &asker, &key, &field(&given, "environment"))
                        .await
                    {
                        Ok(approval) => Flash::done(format!(
                            "Approved as it reads now, against {}. Schedules, events and \
                             automations may run it by itself until it changes.",
                            approval.environment
                        )),
                        Err(refusal) => Flash::refused(&refusal),
                    }
                }
                _ => match runbooks::withdraw(backend, &key).await {
                    Ok(()) => Flash::done("Withdrawn. Nothing runs it by itself any more."),
                    Err(refusal) => Flash::refused(&refusal),
                },
            };
            runbook_page(backend, &asker, key, flash).await
        }
        ("GET", ["runbooks", rest @ ..]) => {
            let key = rest.iter().map(|part| runbooks::decoded(part)).collect::<Vec<_>>().join("/");
            runbook_page(backend, &asker, key, Flash::default()).await
        }
        ("GET", ["connect"]) => home(backend, &asker, "connect", Flash::default()).await,
        ("GET", ["jobs", "new"]) => job_form(backend, None, &given, Flash::default()).await,
        ("POST", ["jobs", "new"]) => {
            match jobs::create(backend, &asker, new_job(&given, false)).await {
                Ok(job) => {
                    went(job.href());
                    let said = match job.trigger.as_str() {
                        "manual" => "The job is set. Run it now, or whenever you want its report.",
                        _ => "The job is set, and runs by itself from now, as you.",
                    };
                    job_page(backend, &asker, job.id, "overview", Flash::done(said)).await
                }
                Err(refusal) => job_form(backend, None, &given, Flash::refused(&refusal)).await,
            }
        }
        ("GET", ["jobs", job]) => {
            let section = match parameter(&request.query, "section").as_deref() {
                Some("runs") => "runs",
                _ => "overview",
            };
            job_page(backend, &asker, id(job)?, section, Flash::default()).await
        }
        ("GET", ["jobs", job, "edit"]) => {
            let held = store.job(id(job)?).await?;
            if !asker.sees(&held) {
                return Err(Refusal::forbidden("only its owner changes a job"));
            }
            job_form(backend, Some(&held), &Form::new(), Flash::default()).await
        }
        ("POST", ["jobs", job, "edit"]) => {
            let job = id(job)?;
            match jobs::change(backend, &asker, job, new_job(&given, true)).await {
                Ok(changed) => {
                    went(changed.href());
                    job_page(
                        backend,
                        &asker,
                        job,
                        "overview",
                        Flash::done("Changed. It runs as you from now."),
                    )
                    .await
                }
                Err(refusal) => {
                    let held = store.job(job).await?;
                    job_form(backend, Some(&held), &given, Flash::refused(&refusal)).await
                }
            }
        }
        ("POST", ["jobs", job, "run"]) => {
            let job = store.job(id(job)?).await?;
            if !asker.sees(&job) || !asker.writes {
                return Err(Refusal::forbidden("only its owner runs a job"));
            }
            // Pressing Run on a runbook's job runs it with your access, as on the runbook itself.
            let run = match job.playbook.as_str() {
                playbooks::RUNBOOK => {
                    runbooks::requested(backend, &asker, &job.runbook, "", Some(&job)).await?
                }
                _ => jobs::queue(backend, &job, &format!("{} asking", asker.login), None).await?,
            };
            went(run.href());
            run_page(backend, &asker, run.id, Flash::done("It is running. This page shows what it does as it goes; reload it to see more.")).await
        }
        ("POST", ["jobs", job, "delete"]) => {
            let gone = jobs::delete(backend, &asker, id(job)?).await?;
            went("/p/agent/".into());
            home(
                backend,
                &asker,
                "jobs",
                Flash::done(format!(
                    "{} is deleted, and its leave to act as its owner given back.",
                    gone.title
                )),
            )
            .await
        }
        ("GET", ["runs", run]) => run_page(backend, &asker, id(run)?, Flash::default()).await,
        ("POST", ["runs", run, "stop"]) => {
            let held = store.run(id(run)?).await?;
            let job = match held.job {
                Some(job) => Some(store.job(job).await?),
                None => None,
            };
            if !asker.sees_run(&held, job.as_ref()) {
                return Err(Refusal::forbidden("only whoever asked for a run stops it"));
            }
            let flash = match claude::stop(backend, &held).await {
                Ok(()) => Flash::done("Stopped. It ends before its next turn."),
                Err(refusal) => Flash::refused(&refusal),
            };
            run_page(backend, &asker, held.id, flash).await
        }
        ("GET", ["reminders", "new"]) => reminder_form(&given, Flash::default()),
        ("POST", ["reminders", "new"]) => {
            if !asker.writes {
                return Err(Refusal::forbidden("that needs plugin:agent:user:rw"));
            }
            let input = json!({ "at": field(&given, "at"), "message": field(&given, "message"), "to": field(&given, "to") });
            let outcome = tools::call(backend, "doc_remind", &input).await;
            match outcome.failed {
                false => {
                    went("/p/agent/reminders".into());
                    home(backend, &asker, "reminders", Flash::done(outcome.text)).await
                }
                true => reminder_form(&given, Flash::refused(&Refusal::bad(outcome.text))),
            }
        }
        ("POST", ["reminders", reminder, "cancel"]) => {
            let reminder = store.reminder(id(reminder)?).await?;
            if reminder.by != asker.id {
                return Err(Refusal::forbidden("only whoever set a reminder cancels it"));
            }
            store.delete(REMINDERS, reminder.id).await?;
            home(backend, &asker, "reminders", Flash::done("Cancelled.")).await
        }
        _ => Err(Refusal::missing("no such page")),
    }
}
