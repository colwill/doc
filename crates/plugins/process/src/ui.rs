//! The plugin's pages at `/p/process/...`: the caller's own processes and what they have to do,
//! every process they can see, process pages and forms, occurrence pages whose checklists are
//! ticked off one item at a time, and a panel for resource pages.

use std::collections::BTreeMap;

use askama::Template;
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use doc_plugin_sdk::{Backend, Request, Response};
use uuid::Uuid;

use crate::Refusal;
use crate::api::{self, Details, Sight, query};
use crate::cadence::{self, Cadence};
use crate::store::{Assignee, Occurrence, Process, Store};

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

fn zone_of(process: &Process) -> Tz {
    process.timezone.parse().unwrap_or(Tz::UTC)
}

fn instant(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).ok().map(|at| at.with_timezone(&Utc))
}

fn shown_at(text: &str, zone: Tz) -> String {
    instant(text).map_or_else(
        || text.to_string(),
        |at| at.with_timezone(&zone).format("%a %-d %b %Y, %H:%M %Z").to_string(),
    )
}

/// A resource's page in Resource Definitions.
fn resource_href(resource: &str) -> String {
    let (kind, name) = resource.split_once(':').unwrap_or(("", resource));
    format!("/p/resources/r/{kind}/{name}")
}

pub struct Badge {
    pub modifier: &'static str,
    pub text: &'static str,
}

fn badge(occurrence: &Occurrence) -> Badge {
    let overdue = instant(&occurrence.due_at).is_some_and(|due| due <= Utc::now());
    let (modifier, text) = match occurrence.status.as_str() {
        "done" if occurrence.missed_at.is_some() => ("degraded", "Done late"),
        "done" => ("ready", "Done"),
        "skipped" => ("unknown", "Skipped"),
        "missed" => ("error", "Missed"),
        _ if overdue => ("degraded", "Overdue"),
        _ => ("loading", "Pending"),
    };
    Badge { modifier, text }
}

/// A process in a table.
pub struct Row {
    pub href: String,
    pub title: String,
    pub resource: String,
    pub resource_href: String,
    pub schedule: String,
    pub assignees: String,
    pub next: String,
    pub next_href: String,
    pub next_badge: Badge,
    pub missed: usize,
}

fn rows(processes: &[Process], outlook: &BTreeMap<Uuid, (Option<Occurrence>, usize)>) -> Vec<Row> {
    processes
        .iter()
        .map(|process| {
            let (next, missed) = outlook.get(&process.id).cloned().unwrap_or_default();
            Row {
                href: format!("/p/process/processes/{}", process.id),
                title: process.title.clone(),
                resource: process.resource.clone(),
                resource_href: resource_href(&process.resource),
                schedule: api::rule_of(process).map(|rule| rule.describe()).unwrap_or_default(),
                assignees: api::assignees_text(process),
                next: next
                    .as_ref()
                    .map_or_else(String::new, |next| shown_at(&next.due_at, zone_of(process))),
                next_href: next
                    .as_ref()
                    .map_or_else(String::new, |next| format!("/p/process/occurrences/{}", next.id)),
                next_badge: next.as_ref().map_or(Badge { modifier: "unknown", text: "" }, badge),
                missed,
            }
        })
        .collect()
}

/// An occurrence in a table.
pub struct Line {
    pub href: String,
    pub title: String,
    pub resource: String,
    pub resource_href: String,
    pub when: String,
    pub badge: Badge,
    pub by: String,
}

fn line(occurrence: &Occurrence, process: &Process) -> Line {
    Line {
        href: format!("/p/process/occurrences/{}", occurrence.id),
        title: process.title.clone(),
        resource: process.resource.clone(),
        resource_href: resource_href(&process.resource),
        when: shown_at(&occurrence.due_at, zone_of(process)),
        badge: badge(occurrence),
        by: occurrence.finished_by.clone().unwrap_or_default(),
    }
}

pub struct Choice {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

fn choices(options: &[(String, String)], chosen: &str) -> Vec<Choice> {
    options
        .iter()
        .map(|(value, label)| Choice {
            value: value.clone(),
            label: label.clone(),
            selected: value == chosen,
        })
        .collect()
}

/// One checklist item on an occurrence page.
pub struct Tick {
    pub number: usize,
    pub text: String,
    pub done: bool,
    pub meta: String,
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    flash: Flash,
    writes: bool,
    to_do: Vec<Line>,
    processes: Vec<Row>,
}

#[derive(Template)]
#[template(path = "all.html")]
struct AllPage {
    flash: Flash,
    writes: bool,
    processes: Vec<Row>,
}

#[derive(Template)]
#[template(path = "process.html")]
struct ProcessPage {
    flash: Flash,
    process: Process,
    resource_href: String,
    schedule: String,
    assignees: String,
    reminder: String,
    grace: String,
    coming: Vec<Line>,
    recent: Vec<Line>,
    editable: bool,
}

#[derive(Template)]
#[template(path = "form.html")]
struct FormPage {
    flash: Flash,
    writes: bool,
    action: String,
    heading: String,
    new: bool,
    fields: BTreeMap<&'static str, String>,
    cadences: Vec<Choice>,
    weekdays: Vec<Choice>,
    months: Vec<Choice>,
}

#[derive(Template)]
#[template(path = "occurrence.html")]
struct OccurrencePage {
    flash: Flash,
    id: Uuid,
    process: Process,
    process_href: String,
    resource_href: String,
    when: String,
    deadline: String,
    badge: Badge,
    assignees: String,
    finished: String,
    note: String,
    event_href: String,
    ticks: Vec<Tick>,
    workable: bool,
}

#[derive(Template)]
#[template(path = "panel.html")]
struct PanelFragment {
    writes: bool,
    resource: String,
    processes: Vec<Row>,
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let fragment = matches!(path, ["panel"] | ["dashboard"]);
    match route(backend, request, path).await {
        Ok(html) => Response::html(html),
        Err(refusal) if fragment => {
            let text = format!(
                "<p class=\"doc-error-message\" role=\"alert\">{}</p>",
                escape(&refusal.detail)
            );
            Response::new(refusal.status, "text/html; charset=utf-8", text)
        }
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(refusal.detail.clone()) };
            let html = page.render().unwrap_or_else(|_| escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

async fn route(backend: &Backend, request: &Request, path: &[&str]) -> Result<String, Refusal> {
    let mut sight = Sight::new(backend);
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => home(backend, &mut sight, Flash::default()).await,
        ("GET", ["all"]) => all(backend, &mut sight).await,
        ("GET", ["processes", "new"]) => {
            let fields = defaults(backend, query(request, "resource"))?;
            form_page(backend, None, fields, Flash::default())
        }
        ("POST", ["processes"]) => {
            let form = form(request);
            let made = match asked(&form, true) {
                Ok(details) => api::create(backend, &mut sight, details).await,
                Err(refusal) => Err(refusal),
            };
            match made {
                Ok(process) => {
                    let notice =
                        "Saved. Its occurrences are planned and on the resource's calendar.";
                    process_page(backend, &mut sight, &process.id.to_string(), Flash::done(notice))
                        .await
                }
                Err(refusal) if refusal.status >= 500 => Err(refusal),
                Err(refusal) => {
                    form_page(backend, None, refilled(&form), Flash::refused(refusal.detail))
                }
            }
        }
        ("GET", ["processes", id]) => process_page(backend, &mut sight, id, Flash::default()).await,
        ("GET", ["processes", id, "edit"]) => {
            let process = api::found_process(&Store(backend), &mut sight, id).await?;
            form_page(backend, Some(&process), filled(&process), Flash::default())
        }
        ("POST", ["processes", id]) => {
            let form = form(request);
            let changed = match asked(&form, false) {
                Ok(details) => api::change(backend, &mut sight, id, details).await,
                Err(refusal) => Err(refusal),
            };
            match changed {
                Ok(_) => process_page(backend, &mut sight, id, Flash::done("Saved.")).await,
                Err(refusal) if refusal.status >= 500 || refusal.status == 404 => Err(refusal),
                Err(refusal) => {
                    let process = api::found_process(&Store(backend), &mut sight, id).await?;
                    form_page(
                        backend,
                        Some(&process),
                        refilled(&form),
                        Flash::refused(refusal.detail),
                    )
                }
            }
        }
        ("POST", ["processes", id, "delete"]) => match api::remove(backend, &mut sight, id).await {
            Ok(process) => {
                let notice = format!(
                    "Deleted {}, with its occurrences and their calendar events.",
                    process.title
                );
                home(backend, &mut sight, Flash::done(notice)).await
            }
            Err(refusal) => {
                process_page(backend, &mut sight, id, Flash::refused(refusal.detail)).await
            }
        },
        ("GET", ["occurrences", id]) => {
            occurrence_page(backend, &mut sight, id, Flash::default()).await
        }
        ("POST", ["occurrences", id, "tick"]) => {
            let form = form(request);
            let number = field(&form, "item").and_then(|number| number.parse().ok()).unwrap_or(0);
            let done = field(&form, "done").as_deref() != Some("false");
            let flash = match api::tick(backend, &mut sight, id, number, done).await {
                Ok(_) => Flash::default(),
                Err(refusal) => Flash::refused(refusal.detail),
            };
            occurrence_page(backend, &mut sight, id, flash).await
        }
        ("POST", ["occurrences", id, "complete"]) => {
            let form = form(request);
            let all = field(&form, "all").is_some();
            let flash =
                match api::complete(backend, &mut sight, id, all, field(&form, "note")).await {
                    Ok(_) => Flash::done("Marked done."),
                    Err(refusal) => Flash::refused(refusal.detail),
                };
            occurrence_page(backend, &mut sight, id, flash).await
        }
        ("POST", ["occurrences", id, "skip"]) => {
            let note = field(&form(request), "note").unwrap_or_default();
            let flash = match api::skip(backend, &mut sight, id, &note).await {
                Ok(_) => Flash::done("Skipped."),
                Err(refusal) => Flash::refused(refusal.detail),
            };
            occurrence_page(backend, &mut sight, id, flash).await
        }
        ("GET", ["panel"]) => panel(backend, &mut sight, request).await,
        ("GET", ["dashboard"]) => due(backend, &mut sight).await,
        _ => Err(Refusal::missing("no such page")),
    }
}

async fn home(backend: &Backend, sight: &mut Sight<'_>, flash: Flash) -> Result<String, Refusal> {
    let store = Store(backend);
    let mine = api::listed(backend, sight, None, true).await?;
    let outlook = api::outlook(&store, &mine).await?;
    let by_id: BTreeMap<Uuid, &Process> =
        mine.iter().map(|process| (process.id, process)).collect();
    let to_do = api::to_do(&store, &mine, 14)
        .await?
        .iter()
        .filter_map(|occurrence| {
            by_id.get(&occurrence.process).map(|process| line(occurrence, process))
        })
        .collect();
    render(&HomePage { flash, writes: backend.writes(), to_do, processes: rows(&mine, &outlook) })
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct DueFragment {
    to_do: Vec<Line>,
    more: usize,
}

/// What is due on a person's dashboard: the soonest of what their home page has to do — due in the
/// next two weeks, overdue or missed — on the processes they own or are assigned to.
async fn due(backend: &Backend, sight: &mut Sight<'_>) -> Result<String, Refusal> {
    let store = Store(backend);
    let mine = api::listed(backend, sight, None, true).await?;
    let by_id: BTreeMap<Uuid, &Process> =
        mine.iter().map(|process| (process.id, process)).collect();
    let lines: Vec<Line> = api::to_do(&store, &mine, 14)
        .await?
        .iter()
        .filter_map(|occurrence| {
            by_id.get(&occurrence.process).map(|process| line(occurrence, process))
        })
        .collect();
    let more = lines.len().saturating_sub(5);
    render(&DueFragment { to_do: lines.into_iter().take(5).collect(), more })
}

async fn all(backend: &Backend, sight: &mut Sight<'_>) -> Result<String, Refusal> {
    let found = api::listed(backend, sight, None, false).await?;
    let outlook = api::outlook(&Store(backend), &found).await?;
    render(&AllPage {
        flash: Flash::default(),
        writes: backend.writes(),
        processes: rows(&found, &outlook),
    })
}

fn plural(count: i64, unit: &str) -> String {
    match count {
        1 => format!("1 {unit}"),
        count => format!("{count} {unit}s"),
    }
}

/// Minutes as a person would say them: `90 minutes`, `2 hours`, `1 day`.
fn span(minutes: i64) -> String {
    match minutes {
        0 => "0 minutes".into(),
        minutes if minutes % (24 * 60) == 0 => plural(minutes / (24 * 60), "day"),
        minutes if minutes % 60 == 0 => plural(minutes / 60, "hour"),
        minutes => plural(minutes, "minute"),
    }
}

async fn process_page(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    flash: Flash,
) -> Result<String, Refusal> {
    let store = Store(backend);
    let process = api::found_process(&store, sight, id).await?;
    let now = Utc::now();
    let occurrences = store.occurrences_of(process.id).await?;
    let (past, future): (Vec<&Occurrence>, Vec<&Occurrence>) = occurrences
        .iter()
        .partition(|occurrence| instant(&occurrence.due_at).is_some_and(|due| due <= now));
    let coming = future.iter().take(5).map(|occurrence| line(occurrence, &process)).collect();
    let recent = past.iter().rev().take(12).map(|occurrence| line(occurrence, &process)).collect();
    let me = api::me(backend)?;
    let admin = backend.caller().is_some_and(|caller| caller.admin);
    let editable = backend.writes() && (admin || process.owner == me || process.created_by == me);
    render(&ProcessPage {
        flash,
        resource_href: resource_href(&process.resource),
        schedule: api::rule_of(&process).map(|rule| rule.describe()).unwrap_or_default(),
        assignees: api::assignees_text(&process),
        reminder: process.remind_minutes.map_or_else(
            || "None".to_string(),
            |minutes| format!("{} before it is due", span(minutes)),
        ),
        grace: format!("{} after it is due", span(process.grace_minutes)),
        coming,
        recent,
        editable,
        process,
    })
}

async fn occurrence_page(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    flash: Flash,
) -> Result<String, Refusal> {
    let store = Store(backend);
    let (occurrence, process) = api::found_occurrence(&store, sight, id).await?;
    let zone = zone_of(&process);
    let me = api::me(backend)?;
    let admin = backend.caller().is_some_and(|caller| caller.admin);
    let open = matches!(occurrence.status.as_str(), "pending" | "missed");
    let workable = backend.writes()
        && open
        && (admin || process.owner == me || sight.assigned(&process, &me).await);
    let deadline = instant(&occurrence.due_at).map_or_else(String::new, |due| {
        let at = due + Duration::minutes(process.grace_minutes);
        shown_at(&at.to_rfc3339(), zone)
    });
    let finished = match (&occurrence.finished_by, &occurrence.finished_at) {
        (Some(by), Some(at)) => format!("{by}, {}", shown_at(at, zone)),
        _ => String::new(),
    };
    let ticks = occurrence
        .checklist
        .iter()
        .enumerate()
        .map(|(index, item)| Tick {
            number: index + 1,
            text: item.text.clone(),
            done: item.done,
            meta: match (&item.by, &item.at) {
                (Some(by), Some(at)) => format!("{by} · {}", shown_at(at, zone)),
                _ => String::new(),
            },
        })
        .collect();
    render(&OccurrencePage {
        flash,
        id: occurrence.id,
        process_href: format!("/p/process/processes/{}", process.id),
        resource_href: resource_href(&process.resource),
        when: shown_at(&occurrence.due_at, zone),
        deadline,
        badge: badge(&occurrence),
        assignees: api::assignees_text(&process),
        finished,
        note: occurrence.note.clone(),
        event_href: occurrence
            .event
            .map_or_else(String::new, |event| format!("/p/calendar/events/{event}")),
        ticks,
        workable,
        process,
    })
}

async fn panel(
    backend: &Backend,
    sight: &mut Sight<'_>,
    request: &Request,
) -> Result<String, Refusal> {
    let asked = query(request, "resource").ok_or_else(|| Refusal::bad("name the resource"))?;
    let resource = api::normalised(&asked)?;
    let found = api::listed(backend, sight, Some(&resource), false).await?;
    let outlook = api::outlook(&Store(backend), &found).await?;
    render(&PanelFragment { writes: backend.writes(), resource, processes: rows(&found, &outlook) })
}

fn defaults(
    backend: &Backend,
    resource: Option<String>,
) -> Result<BTreeMap<&'static str, String>, Refusal> {
    Ok(BTreeMap::from([
        ("resource", resource.unwrap_or_default()),
        ("cadence", "weekly".into()),
        ("weekday", "1".into()),
        ("month", "1".into()),
        ("day", "1".into()),
        ("time", "09:00".into()),
        ("timezone", "UTC".into()),
        ("duration", "30".into()),
        ("reminder", "60".into()),
        ("grace", "1440".into()),
        ("owner", api::me(backend)?),
    ]))
}

fn filled(process: &Process) -> BTreeMap<&'static str, String> {
    let assignees: Vec<String> = process
        .assignees
        .iter()
        .map(|assignee| match assignee {
            Assignee::User(login) => login.clone(),
            Assignee::Team(team) => format!("team:{team}"),
        })
        .collect();
    BTreeMap::from([
        ("resource", process.resource.clone()),
        ("title", process.title.clone()),
        ("description", process.description.clone()),
        ("cadence", process.cadence.clone()),
        ("weekday", process.weekday.to_string()),
        ("month", process.month.to_string()),
        ("day", process.day.to_string()),
        ("time", process.at_time.clone()),
        ("timezone", process.timezone.clone()),
        ("duration", process.duration_minutes.to_string()),
        ("reminder", process.remind_minutes.map(|minutes| minutes.to_string()).unwrap_or_default()),
        ("grace", process.grace_minutes.to_string()),
        ("owner", process.owner.clone()),
        ("assignees", assignees.join(", ")),
        ("checklist", process.checklist.join("\n")),
    ])
}

const FIELDS: [&str; 15] = [
    "resource",
    "title",
    "description",
    "cadence",
    "weekday",
    "month",
    "day",
    "time",
    "timezone",
    "duration",
    "reminder",
    "grace",
    "owner",
    "assignees",
    "checklist",
];

/// The form as it was sent, to show again with what was refused.
fn refilled(form: &Form) -> BTreeMap<&'static str, String> {
    FIELDS.iter().map(|name| (*name, field(form, name).unwrap_or_default())).collect()
}

fn form_page(
    backend: &Backend,
    process: Option<&Process>,
    fields: BTreeMap<&'static str, String>,
    flash: Flash,
) -> Result<String, Refusal> {
    let value = |name: &str| fields.get(name).cloned().unwrap_or_default();
    let cadences: Vec<(String, String)> = Cadence::ALL
        .iter()
        .map(|cadence| {
            let name = cadence.name();
            (name.to_string(), format!("{}{}", name[..1].to_ascii_uppercase(), &name[1..]))
        })
        .collect();
    let numbered = |names: &[&str]| -> Vec<(String, String)> {
        names
            .iter()
            .enumerate()
            .map(|(index, name)| ((index + 1).to_string(), (*name).to_string()))
            .collect()
    };
    let (action, heading) = match process {
        Some(process) => {
            (format!("/p/process/processes/{}", process.id), format!("Edit {}", process.title))
        }
        None => ("/p/process/processes".to_string(), "New process".to_string()),
    };
    render(&FormPage {
        flash,
        writes: backend.writes(),
        action,
        heading,
        new: process.is_none(),
        cadences: choices(&cadences, &value("cadence")),
        weekdays: choices(&numbered(&cadence::WEEKDAYS), &value("weekday")),
        months: choices(&numbered(&cadence::MONTHS), &value("month")),
        fields,
    })
}

/// People and teams, written `alice, team:payments-core`.
fn assignees(text: &str) -> Vec<Assignee> {
    text.split([',', '\n'])
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| match part.strip_prefix("team:").or_else(|| part.strip_prefix("team ")) {
            Some(team) => Assignee::Team(team.trim().to_string()),
            None => Assignee::User(part.trim_start_matches('@').to_string()),
        })
        .collect()
}

/// What the process form asks for.
fn asked(form: &Form, new: bool) -> Result<Details, Refusal> {
    let number = |name: &str, what: &str| -> Result<Option<i64>, Refusal> {
        field(form, name)
            .map(|text| {
                text.parse::<i64>()
                    .map_err(|_| Refusal::bad(format!("{what} is a whole number, not `{text}`")))
            })
            .transpose()
    };
    let checklist = field(form, "checklist").unwrap_or_default();
    Ok(Details {
        resource: if new { field(form, "resource") } else { None },
        title: Some(field(form, "title").unwrap_or_default()),
        description: Some(field(form, "description").unwrap_or_default()),
        cadence: field(form, "cadence"),
        weekday: field(form, "weekday").map(serde_json::Value::String),
        month: field(form, "month").map(serde_json::Value::String),
        day: number("day", "the day of the month")?,
        time: field(form, "time"),
        timezone: field(form, "timezone"),
        duration_minutes: number("duration", "how long it takes")?,
        remind_minutes: Some(number("reminder", "the reminder")?),
        grace_minutes: number("grace", "when it is missed")?,
        owner: field(form, "owner"),
        assignees: Some(assignees(&field(form, "assignees").unwrap_or_default())),
        checklist: Some(checklist.lines().map(str::to_string).collect()),
    })
}
