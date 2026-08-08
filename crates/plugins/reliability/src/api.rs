//! The JSON routes: a scope's reliability or DOC's over a period, its outages, how each service is
//! watched and held to account, and what automations report — an outage monitoring saw, a backup
//! taken, a restore tried — through the operations the manifest declares (ADR-0012).

use chrono::{DateTime, NaiveDate, Utc};
use doc_plugin_sdk::{Backend, Query, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::metrics::{self, Period};
use crate::platform::{COMPONENTS, DATABASE};
use crate::scope::{self, Scope};
use crate::settings::Definitions;
use crate::store::{Outage, Subject};
use crate::ui::{self, MAX_DAYS};
use crate::{Refusal, faux, parameter, probe, record, view, who};

const MAX_LIMIT: usize = 1_000;

type Answer = Result<Value, Refusal>;

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let query = request.query.as_str();
    let answer = match (request.method.as_str(), segments.as_slice()) {
        (_, ["api", "mcp"]) => return crate::mcp::handle(backend, request).await,
        ("GET", ["api", "metrics"]) => figures(backend, query).await,
        ("GET", ["api", "doc"]) => doc(backend, query).await,
        ("GET", ["api", "outages"]) => outages(backend, query).await,
        ("GET", ["api", "readiness"]) => readiness(backend, query).await,
        ("GET", ["api", "services"]) => watched(backend).await,
        ("PUT", ["api", "services", name]) => match body::<Configured>(request) {
            Ok(asked) => configure(backend, name, asked).await.map(|subject| json!(subject)),
            Err(refusal) => Err(refusal),
        },
        ("DELETE", ["api", "services", name]) => {
            stop(backend, name).await.map(|subject| json!(subject))
        }
        ("POST", ["api", "services", name, "down"]) => down(backend, name, request).await,
        ("POST", ["api", "services", name, "up"]) => up(backend, name, request).await,
        ("POST", ["api", "backups"]) => backup(backend, request).await,
        ("POST", ["api", "restores"]) => restore(backend, request).await,
        _ => return Response::not_found(),
    };
    match answer {
        Ok(value) => Response::json(&value),
        Err(refusal) => refusal.response(),
    }
}

fn body<T: for<'a> Deserialize<'a> + Default>(request: &Request) -> Result<T, Refusal> {
    match request.body.is_empty() {
        true => Ok(T::default()),
        false => request.json().map_err(|err| Refusal::bad(err.to_string())),
    }
}

/// A day as `2026-09-01`, or a moment as RFC 3339.
pub fn moment(text: &str, name: &str) -> Result<DateTime<Utc>, Refusal> {
    if let Ok(at) = DateTime::parse_from_rfc3339(text.trim()) {
        return Ok(at.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d")
        .ok()
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .map(|at| at.and_utc())
        .ok_or_else(|| {
            Refusal::bad(format!("`{name}` is a day, such as 2026-09-01, or an RFC 3339 time"))
        })
}

/// A time given or now; never in the future, which nothing can have happened at yet.
fn when(value: &Option<Value>, name: &str) -> Result<DateTime<Utc>, Refusal> {
    let now = Utc::now();
    let at = match value {
        None | Some(Value::Null) => return Ok(now),
        Some(Value::String(text)) if text.trim().is_empty() => return Ok(now),
        Some(Value::String(text)) => moment(text, name)?,
        Some(_) => return Err(Refusal::bad(format!("`{name}` is an RFC 3339 time"))),
    };
    match at > now + chrono::Duration::minutes(5) {
        true => Err(Refusal::bad(format!("`{name}` is in the future"))),
        false => Ok(at.min(now)),
    }
}

/// A span as seconds, or as people write it: `90`, `15m`, `4h`, `1d`, `1h 30m`.
pub fn duration(text: &str) -> Result<f64, String> {
    let text = text.trim().to_ascii_lowercase();
    if let Ok(seconds) = text.parse::<f64>() {
        return (seconds >= 0.0).then_some(seconds).ok_or_else(|| "a time is not negative".into());
    }
    let mut total = 0.0;
    for part in text.split_whitespace() {
        let split = part.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(part.len());
        let (number, unit) = part.split_at(split);
        let number: f64 =
            number.parse().map_err(|_| format!("`{text}` is not a time, such as 4h or 30m"))?;
        total += number
            * match unit {
                "s" => 1.0,
                "m" | "min" => 60.0,
                "h" => 3_600.0,
                "d" => 86_400.0,
                _ => return Err(format!("`{text}` is not a time, such as 4h or 30m")),
            };
    }
    Ok(total)
}

/// An objective as the API or a form gives it: a number, text, or nothing for the default.
fn objective(value: &Option<Value>, name: &str, as_time: bool) -> Result<Option<f64>, Refusal> {
    let text = match value {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::Number(number)) => return Ok(number.as_f64()),
        Some(Value::String(text)) if text.trim().is_empty() => return Ok(None),
        Some(Value::String(text)) => text.trim().trim_end_matches('%').to_string(),
        Some(_) => return Err(Refusal::bad(format!("`{name}` is a number or text"))),
    };
    let parsed = match as_time {
        true => duration(&text).map_err(|problem| Refusal::bad(format!("`{name}`: {problem}")))?,
        false => {
            text.parse::<f64>().map_err(|_| Refusal::bad(format!("`{name}` is a percentage")))?
        }
    };
    if !as_time && !(0.0..=100.0).contains(&parsed) {
        return Err(Refusal::bad(format!("`{name}` is a percentage from 0 to 100")));
    }
    Ok(Some(parsed))
}

/// `from` and `to`, or the last `days`, or the last 30 days.
pub fn period(query: &str) -> Result<Period, Refusal> {
    let (from, to) = (parameter(query, "from"), parameter(query, "to"));
    if from.is_none() && to.is_none() {
        return Ok(Period::last(ui::days(query)?));
    }
    let to = match to {
        Some(to) => moment(&to, "to")?,
        None => Utc::now(),
    };
    let from = match from {
        Some(from) => moment(&from, "from")?,
        None => to - chrono::Duration::days(30),
    };
    if from >= to || to - from > chrono::Duration::days(2 * MAX_DAYS) {
        return Err(Refusal::bad(format!(
            "`from` must come before `to`, and at most {} days before it",
            2 * MAX_DAYS
        )));
    }
    Ok(Period { from, to })
}

async fn figures(backend: &Backend, query: &str) -> Answer {
    let scope = Scope::from_query(query)?;
    let period = period(query)?;
    let services = view::services(backend, &scope, &period).await?;
    Ok(json!({
        "scope": { "kind": scope.kind(), "name": scope.name() },
        "from": period.from,
        "to": period.to,
        "services": services.judged.iter().map(metrics::Judged::json).collect::<Vec<_>>(),
        "faux": faux::said(),
    }))
}

async fn doc(backend: &Backend, query: &str) -> Answer {
    let period = period(query)?;
    let doc = view::doc(backend, &period).await?;
    Ok(json!({
        "from": period.from,
        "to": period.to,
        "doc": doc.whole.json(),
        "parts": doc.parts.iter().map(metrics::Judged::json).collect::<Vec<_>>(),
        "plugins": doc.plugins.iter().map(metrics::Judged::json).collect::<Vec<_>>(),
        "faux": faux::said(),
    }))
}

fn limit(query: &str) -> Result<usize, Refusal> {
    match parameter(query, "limit") {
        None => Ok(100),
        Some(text) => text
            .parse::<usize>()
            .ok()
            .filter(|limit| (1..=MAX_LIMIT).contains(limit))
            .ok_or_else(|| Refusal::bad(format!("`limit` is a number from 1 to {MAX_LIMIT}"))),
    }
}

/// A scope's outages, or DOC's with `doc=1`: still going first, then the newest.
pub async fn listed_outages(
    backend: &Backend,
    query: &str,
    period: &Period,
) -> Result<Vec<Outage>, Refusal> {
    let held = match parameter(query, "doc").is_some_and(|value| value != "0" && value != "false") {
        true => view::doc(backend, period).await?.held,
        false => view::services(backend, &Scope::from_query(query)?, period).await?.held,
    };
    let mut found = held.outages;
    found.sort_by_key(|outage| (outage.ended_at.is_some(), std::cmp::Reverse(outage.started_at)));
    Ok(found)
}

async fn outages(backend: &Backend, query: &str) -> Answer {
    let period = period(query)?;
    let open_only = parameter(query, "open").is_some_and(|value| value != "0" && value != "false");
    let mut found = listed_outages(backend, query, &period).await?;
    found.retain(|outage| !open_only || outage.ended_at.is_none());
    found.truncate(limit(query)?);
    Ok(json!({ "outages": found, "faux": faux::said() }))
}

/// The services being watched that the caller can see, with how and what they are held to.
async fn watched(backend: &Backend) -> Answer {
    let visible: Vec<String> =
        scope::every(backend).await?.into_iter().map(|member| view::key(&member.name)).collect();
    let held: Vec<Subject> =
        backend.query_all(Query::new("subjects").filter(json!({ "kind": "service" }))).await?;
    let definitions = Definitions::read(&backend.settings());
    let listed: Vec<Value> = held
        .into_iter()
        .filter(|subject| visible.contains(&subject.subject))
        .map(|subject| {
            let targets = subject.targets(definitions.services);
            json!({ "service": subject.name, "url": subject.url, "state": subject.state,
                "checked_at": subject.checked_at, "objectives": targets })
        })
        .collect();
    Ok(json!({ "services": listed }))
}

/// How a service is to be watched and held to account. Anything left out is the default.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Configured {
    pub url: Option<String>,
    pub sla: Option<Value>,
    pub rto: Option<Value>,
    pub rpo: Option<Value>,
    pub mttr: Option<Value>,
}

/// Sets a service's health URL and objectives, for someone who can see it in the Catalogue.
pub async fn configure(
    backend: &Backend,
    name: &str,
    asked: Configured,
) -> Result<Subject, Refusal> {
    scope::visible(backend, name).await?;
    let definitions = Definitions::read(&backend.settings());
    let url = match asked.url.as_deref().map(str::trim).filter(|url| !url.is_empty()) {
        None => None,
        Some(url) => Some(
            probe::allowed(url, &definitions)
                .await
                .map_err(|problem| {
                    Refusal::bad(format!("that health URL cannot be checked: {problem}"))
                })?
                .to_string(),
        ),
    };
    let key = view::key(name);
    let mut subject = backend
        .get::<Subject>("subjects", key.as_str())
        .await?
        .unwrap_or_else(|| Subject::service(name));
    if subject.url != url {
        subject.state = "unknown".into();
        subject.failing = 0;
        subject.failing_since = None;
    }
    subject.url = url;
    subject.sla = objective(&asked.sla, "sla", false)?;
    subject.rto = objective(&asked.rto, "rto", true)?;
    subject.rpo = objective(&asked.rpo, "rpo", true)?;
    subject.mttr = objective(&asked.mttr, "mttr", true)?;
    subject.updated_by = Some(who(backend));
    backend.upsert::<Value>("subjects", &["subject"], json!(subject)).await?;
    Ok(subject)
}

/// Stops checking a service and forgets its own objectives; what happened to it is kept.
pub async fn stop(backend: &Backend, name: &str) -> Result<Subject, Refusal> {
    configure(backend, name, Configured::default()).await
}

/// A report about a service: when, and a note, which automations send as text.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Reported {
    at: Option<Value>,
    note: Option<String>,
}

/// The service's record, made the first time something is reported about it: reporting is a way
/// of watching it, from then on.
async fn reported(backend: &Backend, name: &str, at: DateTime<Utc>) -> Result<String, Refusal> {
    scope::visible(backend, name).await?;
    let key = view::key(name);
    let held = backend.get::<Subject>("subjects", key.as_str()).await?;
    if held.as_ref().is_none_or(|held| held.since.is_none()) {
        let mut subject = held.unwrap_or_else(|| Subject::service(name));
        subject.since = Some(at);
        backend.upsert::<Value>("subjects", &["subject"], json!(subject)).await?;
    }
    Ok(key)
}

async fn down(backend: &Backend, name: &str, request: &Request) -> Answer {
    let asked: Reported = body(request)?;
    let at = when(&asked.at, "at")?;
    let key = reported(backend, name, at).await?;
    let note = asked.note.map(|note| note.trim().to_string()).filter(|note| !note.is_empty());
    let opened = record::open(backend, &key, at, "report", note, Some(who(backend))).await?;
    let open = record::still_open(backend, &key).await?;
    Ok(json!({ "opened": opened, "open": open }))
}

async fn up(backend: &Backend, name: &str, request: &Request) -> Answer {
    let asked: Reported = body(request)?;
    let at = when(&asked.at, "at")?;
    let key = reported(backend, name, at).await?;
    let closed = record::close(backend, &key, at, None).await?;
    Ok(json!({ "closed": closed }))
}

/// What a backup or restore is about: a service the caller can see, or — for somebody who
/// administers the platform — a part of DOC.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct About {
    service: Option<String>,
    component: Option<String>,
    at: Option<Value>,
    kind: Option<String>,
    note: Option<String>,
    started_at: Option<Value>,
    finished_at: Option<Value>,
    succeeded: Option<Value>,
}

async fn subject_of(backend: &Backend, about: &About) -> Result<String, Refusal> {
    let text = |value: &Option<String>| {
        value.as_deref().map(str::trim).filter(|v| !v.is_empty()).map(str::to_string)
    };
    match (text(&about.service), text(&about.component)) {
        (Some(service), None) => {
            scope::visible(backend, &service).await?;
            Ok(view::key(&service))
        }
        (None, Some(component)) => {
            if !backend.caller().is_some_and(|caller| caller.admin) {
                return Err(Refusal::forbidden(
                    "only somebody who administers DOC records its own backups",
                ));
            }
            let component = component.to_ascii_lowercase();
            match COMPONENTS.iter().any(|(name, _)| *name == component) {
                true => Ok(format!("doc:{component}")),
                false => Err(Refusal::bad(format!(
                    "DOC's parts are {}; its database, whose backups matter, is postgres",
                    COMPONENTS.map(|(name, _)| name).join(", ")
                ))),
            }
        }
        _ => Err(Refusal::bad("name a `service`, or a DOC `component` such as postgres")),
    }
}

fn note(text: &Option<String>) -> Option<String> {
    text.as_deref()
        .map(str::trim)
        .filter(|note| !note.is_empty())
        .map(|note| note.chars().take(500).collect())
}

async fn backup(backend: &Backend, request: &Request) -> Answer {
    let about: About = body(request)?;
    let subject = subject_of(backend, &about).await?;
    let record = json!({
        "id": Uuid::new_v4(),
        "subject": subject,
        "at": when(&about.at, "at")?,
        "kind": note(&about.kind).map(|kind| kind.chars().take(64).collect::<String>()),
        "note": note(&about.note),
        "by": who(backend),
    });
    let recorded: Value = backend.insert("backups", record).await?;
    Ok(json!({ "backup": recorded, "database": subject == DATABASE }))
}

async fn restore(backend: &Backend, request: &Request) -> Answer {
    let about: About = body(request)?;
    let subject = subject_of(backend, &about).await?;
    if about.started_at.as_ref().is_none_or(Value::is_null) {
        return Err(Refusal::bad("`started_at` is when the restore began"));
    }
    let started_at = when(&about.started_at, "started_at")?;
    let finished_at = when(&about.finished_at, "finished_at")?;
    if finished_at < started_at {
        return Err(Refusal::bad("a restore finishes after it starts"));
    }
    let succeeded = match &about.succeeded {
        None | Some(Value::Null) => true,
        Some(Value::Bool(said)) => *said,
        Some(Value::String(text)) => {
            !matches!(text.trim().to_ascii_lowercase().as_str(), "false" | "no" | "0")
        }
        Some(_) => return Err(Refusal::bad("`succeeded` is true or false")),
    };
    let record = json!({
        "id": Uuid::new_v4(),
        "subject": subject,
        "started_at": started_at,
        "finished_at": finished_at,
        "seconds": (finished_at - started_at).num_seconds() as f64,
        "succeeded": succeeded,
        "note": note(&about.note),
        "by": who(backend),
    });
    let recorded: Value = backend.insert("restores", record).await?;
    Ok(json!({ "restore": recorded }))
}

/// The services a readiness question names, each once.
fn asked(query: &str) -> Result<Vec<String>, Refusal> {
    let mut names: Vec<String> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _)| key == "service")
        .flat_map(|(_, value)| {
            value.split(',').map(|name| name.trim().to_string()).collect::<Vec<_>>()
        })
        .filter(|name| !name.is_empty())
        .collect();
    names.sort();
    names.dedup();
    match names.len() {
        0 => Err(Refusal::bad("name at least one service with `service`")),
        count if count > 100 => Err(Refusal::bad("ask about at most 100 services at once")),
        _ => Ok(names),
    }
}

/// `availability objective`, or `availability, recovery time and recovery point objectives`.
fn listed(objectives: &[String]) -> String {
    match objectives {
        [one] => format!("{one} objective"),
        [first @ .., last] => format!("{} and {last} objectives", first.join(", ")),
        [] => String::new(),
    }
}

/// Whether each service is fit to release to: down now blocks it, and an objective missed over
/// the last 30 days — its error budget spent — warns. What the delivery roadmap asks every plugin
/// that can say (DOC-SPEC §9.2); `until` does not change the answer.
async fn readiness(backend: &Backend, query: &str) -> Answer {
    let names = asked(query)?;
    let definitions = Definitions::read(&backend.settings());
    let period = Period::last(30);
    let mut services = serde_json::Map::new();
    let mut visible = Vec::new();
    for name in names {
        match scope::services(backend, &Scope::Service(name.clone())).await {
            Ok(_) => visible.push(name),
            Err(refusal) if refusal.status == 404 => {
                let said = "The Catalogue has no such service, or you cannot see it";
                services.insert(name, json!({ "state": "unknown", "summary": said }));
            }
            Err(refusal) => return Err(refusal),
        }
    }
    let keys: Vec<String> = visible.iter().map(|name| view::key(name)).collect();
    let held = metrics::held(backend, &keys, &period).await?;
    for name in visible {
        let key = view::key(&name);
        let subject = held.subjects.get(&key);
        let targets =
            subject.map_or(definitions.services, |held| held.targets(definitions.services));
        let judged = metrics::judge(&key, &name, targets, &held, &period, definitions.at_risk);
        let href = format!("/p/reliability/?{}", scope::encoded(&[("service", &name)]));
        let down_since = held
            .outages
            .iter()
            .filter(|outage| outage.subject == key && outage.ended_at.is_none())
            .map(|outage| outage.started_at)
            .min();
        let missed: Vec<String> = [
            (judged.sla, "availability"),
            (judged.recovery, "mean time to recover"),
            (judged.rto, "recovery time"),
            (judged.rpo, "recovery point"),
        ]
        .into_iter()
        .filter(|(verdict, _)| *verdict == metrics::Verdict::Missed)
        .map(|(_, objective)| objective.to_string())
        .collect();
        let available = judged
            .availability
            .map(|availability| format!("{} available", metrics::percent(availability)))
            .unwrap_or_default();
        let (state, summary) = match (down_since, subject, missed.is_empty()) {
            (Some(since), _, _) => {
                ("blocked", format!("Down now, since {}", since.format("%-d %b %Y, %H:%M UTC")))
            }
            (None, None, _) if judged.outages == 0 && judged.backups == 0 => (
                "unknown",
                "Not watched: it has no health URL and nothing has been reported".to_string(),
            ),
            (None, _, false) => (
                "warning",
                format!(
                    "Missed its {} over the last 30 days{}",
                    listed(&missed),
                    match available.is_empty() {
                        true => String::new(),
                        false => format!(", {available}"),
                    }
                ),
            ),
            (None, _, true) => (
                "ready",
                match available.is_empty() {
                    true => "Kept its objectives over the last 30 days".to_string(),
                    false => format!("Kept its objectives over the last 30 days, {available}"),
                },
            ),
        };
        services.insert(name, json!({ "state": state, "summary": summary, "href": href }));
    }
    Ok(json!({ "title": "Reliability", "services": services, "faux": faux::said() }))
}
