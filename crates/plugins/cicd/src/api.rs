//! The JSON routes: the four figures for a scope over a period, for every stage and each on its
//! own, the workflows and runs behind them, and each time a default branch broke.

use std::collections::BTreeSet;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use doc_plugin_sdk::{Backend, Query, Request, Response};
use serde_json::{Value, json};

use crate::metrics::{self, Band, Counting, Held, Period};
use crate::scope::{self, Scope};
use crate::settings::{Definitions, METRICS, Stage};
use crate::store::Recovery;
use crate::ui::{self, MAX_DAYS};
use crate::{Refusal, faux, parameter};

const MAX_LIMIT: usize = 1_000;

type Answer = Result<Value, Refusal>;

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let query = request.query.as_str();
    let answer = match (request.method.as_str(), path.as_str()) {
        (_, "api/mcp") => return crate::mcp::handle(backend, request).await,
        ("GET", "api/metrics") => figures(backend, query).await,
        ("GET", "api/workflows") => workflows(backend, query).await,
        ("GET", "api/runs") => runs(backend, query).await,
        ("GET", "api/recoveries") => recoveries(backend, query).await,
        ("GET", "api/readiness") => readiness(backend, query).await,
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

/// A scope's repositories; `None` for everything.
type Repositories = Option<BTreeSet<String>>;

/// What a scoped read over a period comes to.
async fn read(
    backend: &Backend,
    query: &str,
) -> Result<(Scope, Period, Repositories, Held), Refusal> {
    let scope = Scope::from_query(query)?;
    let period = period(query)?;
    let repositories = scope::repositories(backend, &scope).await?;
    let held = metrics::held(backend, repositories.as_ref(), &period).await?;
    Ok((scope, period, repositories, held))
}

/// Every stage's figures, keyed by stage.
fn staged(held: &Held, period: &Period, definitions: &Definitions) -> Value {
    let counting = Counting::of(definitions);
    Stage::ALL
        .into_iter()
        .map(|stage| {
            let figures = metrics::figures(held, period, counting.stage(stage));
            (stage.key().to_string(), figures.json(&definitions.bands))
        })
        .collect::<serde_json::Map<String, Value>>()
        .into()
}

async fn figures(backend: &Backend, query: &str) -> Answer {
    let (scope, period, repositories, held) = read(backend, query).await?;
    let definitions = Definitions::read(&backend.settings());
    let before = metrics::held(backend, repositories.as_ref(), &period.before()).await?;
    let weekly = match parameter(query, "by").as_deref() {
        None => period.days() > 31.0,
        Some("week") => true,
        Some("day") => false,
        Some(_) => return Err(Refusal::bad("`by` is day or week")),
    };
    let counting = Counting::of(&definitions);
    Ok(json!({
        "scope": { "kind": scope.kind(), "name": scope.name() },
        "repositories": repositories,
        "from": period.from,
        "to": period.to,
        "branches": if definitions.default_only { "default" } else { "all" },
        "metrics": metrics::figures(&held, &period, counting).json(&definitions.bands),
        "stages": staged(&held, &period, &definitions),
        "previous": metrics::figures(&before, &period.before(), counting).json(&definitions.bands),
        "series": series(&held, &period, weekly, &definitions),
        "on": backend.feature(METRICS),
        "faux": faux::said(),
    }))
}

/// The figures in each day or week of the period, from its start.
fn series(held: &Held, period: &Period, weekly: bool, definitions: &Definitions) -> Value {
    let step = if weekly { Duration::weeks(1) } else { Duration::days(1) };
    let mut out = Vec::new();
    let mut start = period.from;
    while start < period.to {
        let end = (start + step).min(period.to);
        let part = Period { from: start, to: end };
        let theirs = Held {
            days: held.days.iter().filter(|day| part.holds_day(day.day)).cloned().collect(),
            recoveries: held
                .recoveries
                .iter()
                .filter(|spell| part.contains(spell.broke_at))
                .cloned()
                .collect(),
        };
        let counting = Counting::of(definitions);
        let mut figures = metrics::figures(&theirs, &part, counting).json(&definitions.bands);
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

async fn workflows(backend: &Backend, query: &str) -> Answer {
    let (_, period, _, held) = read(backend, query).await?;
    let definitions = Definitions::read(&backend.settings());
    let listed: Vec<Value> = metrics::workflows(&held, &period, &definitions)
        .into_iter()
        .take(limit(query)?)
        .map(|workflow| {
            json!({
                "repository": workflow.repository,
                "workflow": workflow.workflow,
                "stage": workflow.stage,
                "broken": workflow.broken,
                "metrics": workflow.figures.json(&definitions.bands),
            })
        })
        .collect();
    Ok(json!({ "workflows": listed, "faux": faux::said() }))
}

async fn runs(backend: &Backend, query: &str) -> Answer {
    let scope = Scope::from_query(query)?;
    let period = period(query)?;
    let failed_only =
        parameter(query, "failed").is_some_and(|value| value != "0" && value != "false");
    let repositories = scope::repositories(backend, &scope).await?;
    let found =
        ui::newest_runs(backend, repositories.as_ref(), &period, failed_only, limit(query)?)
            .await?;
    Ok(json!({ "runs": found, "faux": faux::said() }))
}

async fn recoveries(backend: &Backend, query: &str) -> Answer {
    let (_, _, _, held) = read(backend, query).await?;
    let open_only = parameter(query, "open").is_some_and(|value| value != "0" && value != "false");
    let mut spells = ui::ordered(held.recoveries);
    spells.retain(|spell| !open_only || spell.fixed_at.is_none());
    spells.truncate(limit(query)?);
    Ok(json!({ "recoveries": spells, "faux": faux::said() }))
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

/// The spells still broken on these repositories' default branches, however long ago they broke.
async fn still_broken(
    backend: &Backend,
    repositories: &BTreeSet<String>,
    held: &Held,
) -> Result<Vec<Recovery>, Refusal> {
    if faux::on(backend) {
        return Ok(held
            .recoveries
            .iter()
            .filter(|spell| spell.fixed_at.is_none())
            .cloned()
            .collect());
    }
    let names: Vec<&String> = repositories.iter().collect();
    let mut found = Vec::new();
    for chunk in names.chunks(200) {
        let filter = json!({ "repository": { "in": chunk }, "fixed_at": { "is_null": true } });
        found.extend(backend.query_all::<Recovery>(Query::new("recoveries").filter(filter)).await?);
    }
    found.sort_by_key(|spell| spell.broke_at);
    Ok(found)
}

/// Whether each service's pipelines are fit to release from: a default branch broken now blocks
/// it, and a success rate in the low band over the last 30 days warns. What the delivery roadmap
/// asks every plugin that can say (DOC-SPEC §9.2); `until` does not change the answer.
async fn readiness(backend: &Backend, query: &str) -> Answer {
    let names = asked(query)?;
    let definitions = Definitions::read(&backend.settings());
    let period = Period::last(30);
    let mut services = serde_json::Map::new();
    for name in names {
        let href = format!("/p/cicd/?{}", scope::encoded(&[("service", &name)]));
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
        let held = metrics::held(backend, Some(&repositories), &period).await?;
        let broken = still_broken(backend, &repositories, &held).await?;
        let figures = metrics::figures(&held, &period, Counting::of(&definitions));
        let band = figures.bands(&definitions.bands).success_rate;
        let rate = figures.success_rate.map(metrics::percent).unwrap_or_default();
        let (state, summary) = match (broken.first(), figures.success_rate, band) {
            (Some(spell), _, _) => (
                "blocked",
                format!(
                    "{} is broken on {}'s default branch since {}{}",
                    spell.workflow,
                    spell.repository,
                    spell.broke_at.format("%-d %b %Y, %H:%M UTC"),
                    match broken.len() {
                        1 => String::new(),
                        count => format!(", and {} more", metrics::plural(count - 1, "workflow")),
                    }
                ),
            ),
            (None, None, _) => ("unknown", "No workflow runs in the last 30 days".to_string()),
            (None, Some(_), Some(Band::Low)) => {
                ("warning", format!("Only {rate} of runs passed in the last 30 days, which is low"))
            }
            (None, Some(_), _) => (
                "ready",
                format!("{rate} of runs passed in the last 30 days, and nothing is broken"),
            ),
        };
        services.insert(name, json!({ "state": state, "summary": summary, "href": href }));
    }
    Ok(json!({ "title": "Pipelines", "services": services, "faux": faux::said() }))
}
