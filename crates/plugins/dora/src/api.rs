//! The JSON routes: the metrics for a scope over a period, the deployments behind them, and
//! counters, which an automation adds to through the `increment` operation (ADR-0012) — a
//! hotfix, an incident — and which count as failures when the settings say so.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use doc_plugin_sdk::{Backend, Order, Query, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::metrics::{self, Period};
use crate::scope::{self, Scope};
use crate::settings::{Definitions, METRICS};
use crate::store::Deployment;
use crate::ui::{self, MAX_DAYS};
use crate::{ID, Refusal, faux, parameter, who};

const MAX_LIMIT: usize = 1_000;
const MAX_BY: i64 = 1_000;

type Answer = Result<Value, Refusal>;

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let query = request.query.as_str();
    let answer = match (request.method.as_str(), segments.as_slice()) {
        (_, ["api", "mcp"]) => return crate::mcp::handle(backend, request).await,
        ("GET", ["api", "metrics"]) => figures(backend, query).await,
        ("GET", ["api", "deployments"]) => deployments(backend, query).await,
        ("GET", ["api", "readiness"]) => readiness(backend, query).await,
        ("GET", ["api", "counters"]) => counters(backend, query).await,
        ("POST", ["api", "counters", counter, "increment"]) => {
            increment(backend, counter, request).await
        }
        _ => return Response::not_found(),
    };
    match answer {
        Ok(value) => Response::json(&value),
        Err(refusal) => refusal.response(),
    }
}

/// A day as `2026-09-01`, or a moment as RFC 3339.
fn moment(text: &str, name: &str) -> Result<DateTime<Utc>, Refusal> {
    if let Ok(at) = DateTime::parse_from_rfc3339(text) {
        return Ok(at.with_timezone(&Utc));
    }
    NaiveDate::parse_from_str(text, "%Y-%m-%d")
        .ok()
        .and_then(|day| day.and_hms_opt(0, 0, 0))
        .map(|at| at.and_utc())
        .ok_or_else(|| {
            Refusal::bad(format!("`{name}` is a day, such as 2026-09-01, or an RFC 3339 time"))
        })
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
        None => to - Duration::days(30),
    };
    if from >= to || to - from > Duration::days(2 * MAX_DAYS) {
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
    let definitions = Definitions::read(&backend.settings());
    let repositories = scope::repositories(backend, &scope).await?;
    let now = metrics::deployments(backend, repositories.as_ref(), &period).await?;
    let before = metrics::deployments(backend, repositories.as_ref(), &period.before()).await?;
    let weekly = match parameter(query, "by").as_deref() {
        None => period.days() > 31.0,
        Some("week") => true,
        Some("day") => false,
        Some(_) => return Err(Refusal::bad("`by` is day or week")),
    };
    Ok(json!({
        "scope": { "kind": scope.kind(), "name": scope.name() },
        "repositories": repositories,
        "from": period.from,
        "to": period.to,
        "metrics": metrics::figures(&now, &period).json(&definitions.bands),
        "previous": metrics::figures(&before, &period.before()).json(&definitions.bands),
        "series": series(&now, &period, weekly, &definitions),
        "on": backend.feature(METRICS),
        "faux": faux::said(),
    }))
}

/// The metrics in each day or week of the period, from its start.
fn series(
    deployments: &[Deployment],
    period: &Period,
    weekly: bool,
    definitions: &Definitions,
) -> Value {
    let step = if weekly { Duration::weeks(1) } else { Duration::days(1) };
    let mut out = Vec::new();
    let mut start = period.from;
    while start < period.to {
        let end = (start + step).min(period.to);
        let part = Period { from: start, to: end };
        let theirs: Vec<Deployment> =
            deployments.iter().filter(|d| part.contains(d.deployed_at)).cloned().collect();
        let mut figures = metrics::figures(&theirs, &part).json(&definitions.bands);
        figures["from"] = json!(start);
        out.push(figures);
        start = end;
    }
    Value::Array(out)
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

async fn deployments(backend: &Backend, query: &str) -> Answer {
    let scope = Scope::from_query(query)?;
    let period = period(query)?;
    let failed_only =
        parameter(query, "failed").is_some_and(|value| value != "0" && value != "false");
    let repositories = scope::repositories(backend, &scope).await?;
    let found =
        ui::newest(backend, repositories.as_ref(), &period, failed_only, limit(query)?).await?;
    Ok(json!({ "deployments": found, "faux": faux::said() }))
}

async fn counters(backend: &Backend, query: &str) -> Answer {
    let period = period(query)?;
    if faux::on(backend) {
        let wanted = parameter(query, "counter").map(|counter| counter.to_ascii_lowercase());
        let scope = Scope::from_query(query)?;
        let repositories = scope::repositories(backend, &scope).await?.unwrap_or_default();
        let mut counted = faux::counted(backend, &repositories, &period).await?;
        counted.retain(|count| wanted.as_ref().is_none_or(|wanted| &count.counter == wanted));
        counted.sort_by_key(|count| std::cmp::Reverse(count.at));
        return Ok(json!({ "counters": counted, "faux": faux::said() }));
    }
    let mut filter = json!({ "at": { "gte": period.from, "lt": period.to } });
    if let Some(counter) = parameter(query, "counter") {
        filter["counter"] = json!(counter.to_ascii_lowercase());
    }
    let counted: Vec<Value> =
        backend.query_all(Query::new("counters").filter(filter).order(Order::desc("at"))).await?;
    Ok(json!({ "counters": counted }))
}

/// What `increment` is called with. Automations send every value as text, so `by` may be either.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Increment {
    service: Option<String>,
    repository: Option<String>,
    by: Option<Value>,
    note: Option<String>,
}

fn valid_counter(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Counts something against a service or a repository. When the counter is one the settings call a
/// failure, the most recent deployment of those repositories within the failure window is marked
/// as having caused one, at once; recomputing later comes to the same, since the count keeps the
/// repository it was put down to.
async fn increment(backend: &Backend, counter: &str, request: &Request) -> Answer {
    let counter = counter.trim().to_ascii_lowercase();
    if !valid_counter(&counter) {
        return Err(Refusal::bad("a counter is named with up to 64 of a-z, 0-9 and -"));
    }
    let asked: Increment = match request.body.is_empty() {
        true => Increment::default(),
        false => request.json().map_err(|err| Refusal::bad(err.to_string()))?,
    };
    let text = |value: Option<String>| {
        value.map(|value| value.trim().to_string()).filter(|v| !v.is_empty())
    };
    let (service, repository, note) =
        (text(asked.service), text(asked.repository), text(asked.note));
    let by = match &asked.by {
        None | Some(Value::Null) => 1,
        Some(Value::Number(number)) => number.as_i64().unwrap_or(0),
        Some(Value::String(text)) if text.trim().is_empty() => 1,
        Some(Value::String(text)) => text.trim().parse::<i64>().unwrap_or(0),
        Some(_) => 0,
    };
    if !(1..=MAX_BY).contains(&by) {
        return Err(Refusal::bad(format!("`by` is a whole number from 1 to {MAX_BY}")));
    }

    let mut candidates: BTreeSet<String> = BTreeSet::new();
    let mut unmapped = None;
    if let Some(service) = &service {
        match scope::repositories(backend, &Scope::Service(service.clone())).await {
            Ok(repositories) => candidates.extend(repositories.unwrap_or_default()),
            Err(refusal) => unmapped = Some(refusal.detail),
        }
    }
    if let Some(repository) = &repository {
        candidates.insert(repository.to_ascii_lowercase());
    }

    let definitions = Definitions::read(&backend.settings());
    let now = Utc::now();
    let mut failed = None;
    if backend.feature(METRICS) && definitions.counters.contains(&counter) && !candidates.is_empty()
    {
        let names: Vec<&String> = candidates.iter().collect();
        let latest = backend
            .query::<Deployment>(
                Query::new("deployments")
                    .filter(json!({
                        "repository": { "in": names },
                        "deployed_at": { "gte": now - definitions.window(), "lte": now },
                    }))
                    .order(Order::desc("deployed_at"))
                    .limit(1),
            )
            .await?
            .records
            .into_iter()
            .next();
        if let Some(deployment) = latest {
            if !deployment.failed {
                let set = json!({ "failed": true, "failure": counter, "failure_at": now });
                backend.update::<Value>("deployments", deployment.id.as_str(), set, None).await?;
            }
            failed = Some(deployment);
        }
    }
    // Put down to the repository it failed, or to the one repository named, so a recompute finds it.
    let put_down = failed
        .as_ref()
        .map(|deployment| deployment.repository.clone())
        .or_else(|| (candidates.len() == 1).then(|| candidates.iter().next().cloned()).flatten());
    let record = json!({
        "counter": counter,
        "amount": by,
        "service": service,
        "repository": put_down,
        "at": now,
        "by": who(backend),
        "note": note,
    });
    let counted: Value = backend.insert("counters", record).await?;
    let deployment = failed.as_ref().map(|deployment| deployment.id.clone());
    backend
        .publish(
            &format!("plugin.{ID}.counter.incremented"),
            json!({
                "counter": counter,
                "amount": by,
                "service": service,
                "repository": put_down,
                "deployment": deployment,
            }),
        )
        .await?;
    Ok(json!({
        "counted": counted,
        "failure_of": deployment,
        "catalogue": unmapped,
    }))
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

/// Whether each service's deliveries make it fit to release: a failed deployment not yet
/// recovered from blocks it, and a change fail rate in the low band over the last 30 days warns.
/// What the delivery roadmap asks every plugin that can say (DOC-SPEC §9.2); `until` does not
/// change the answer.
async fn readiness(backend: &Backend, query: &str) -> Answer {
    let names = asked(query)?;
    let definitions = Definitions::read(&backend.settings());
    let period = Period::last(30);
    let mut services = serde_json::Map::new();
    for name in names {
        let href = format!("/p/dora/?{}", scope::encoded(&[("service", &name)]));
        let scope = Scope::Service(name.clone());
        let repositories = match scope::repositories(backend, &scope).await {
            Ok(Some(repositories)) => repositories,
            Ok(None) => BTreeSet::new(),
            Err(refusal) if refusal.status == 404 => {
                let said = "The Catalogue has no such service, or you cannot see it";
                services.insert(name, json!({ "state": "unknown", "summary": said }));
                continue;
            }
            Err(refusal) => return Err(refusal),
        };
        if repositories.is_empty() {
            let said = "No repository is connected to it in the Catalogue";
            services.insert(name, json!({ "state": "unknown", "summary": said, "href": href }));
            continue;
        }
        let deployments = metrics::deployments(backend, Some(&repositories), &period).await?;
        let figures = metrics::figures(&deployments, &period);
        let band = figures.bands(&definitions.bands).change_fail_rate;
        let unrecovered = deployments
            .iter()
            .filter(|deployment| deployment.failed && deployment.recovered_at.is_none())
            .max_by_key(|deployment| deployment.deployed_at);
        let failing = match figures.change_fail_rate {
            Some(rate) => format!("{} of them failed", metrics::percent(rate)),
            None => String::new(),
        };
        let (state, summary) = match (unrecovered, figures.deployments, band) {
            (Some(deployment), _, _) => (
                "blocked",
                format!(
                    "The deployment to {} on {} failed and has not recovered",
                    deployment.environment,
                    deployment.deployed_at.format("%-d %b %Y, %H:%M UTC")
                ),
            ),
            (None, 0, _) => ("unknown", "No deployments in the last 30 days".to_string()),
            (None, count, Some(metrics::Band::Low)) => (
                "warning",
                format!("{count} deployments in the last 30 days, and {failing}: the low band"),
            ),
            (None, count, _) => (
                "ready",
                format!(
                    "{} in the last 30 days{}",
                    metrics::plural(count, "deployment"),
                    if failing.is_empty() { String::new() } else { format!(", {failing}") }
                ),
            ),
        };
        services.insert(name, json!({ "state": state, "summary": summary, "href": href }));
    }
    Ok(json!({ "title": "Delivery", "services": services, "faux": faux::said() }))
}
