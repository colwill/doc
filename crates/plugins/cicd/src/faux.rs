//! Faux data (FIX-FAUX-DATA): whether `faux-data` is providing this plugin's data — its toggle on
//! faux-data's Settings page, read from the `serving` record it exports here — and asking it for
//! that data, which this plugin works out with the same code as live data. Nothing asked for is
//! stored and nothing is announced from it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Query, Response};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::compute;
use crate::metrics::{Held, Period};
use crate::scope::Member;
use crate::settings::Definitions;
use crate::store::Run;
use crate::{ID, Refusal};

pub const PROVIDER: &str = "faux-data";
/// A toggle is read again after this long, so turning one on or off shows within seconds.
const KEPT: Duration = Duration::from_secs(5);
/// How long the provider is given to answer.
const ASKING: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Live,
    Faux,
    /// Its toggle is on, and the provider is not running.
    Stopped,
}

static MODE: Mutex<Option<(Instant, Mode)>> = Mutex::new(None);

fn current() -> Mode {
    MODE.lock().ok().and_then(|held| held.map(|(_, mode)| mode)).unwrap_or(Mode::Live)
}

async fn running(backend: &Backend) -> bool {
    let asked = Query::new("core.plugins").filter(json!({ "id": PROVIDER })).fields(&["state"]);
    backend
        .query_all::<Value>(asked)
        .await
        .unwrap_or_default()
        .iter()
        .any(|plugin| matches!(plugin["state"].as_str(), Some("running" | "cancelled")))
}

/// Reads whether faux data is on for this plugin, unless it was read in the last few seconds:
/// done as each request comes in, so everything after it asks `on` without waiting.
pub async fn check(backend: &Backend) {
    if let Ok(held) = MODE.lock()
        && let Some((at, _)) = *held
        && at.elapsed() < KEPT
    {
        return;
    }
    let on = match backend.get::<Value>(&format!("{PROVIDER}.serving"), ID).await {
        Ok(Some(record)) => record["on"].as_bool().unwrap_or_default(),
        _ => false,
    };
    let mode = match on {
        false => Mode::Live,
        true if running(backend).await => Mode::Faux,
        true => Mode::Stopped,
    };
    if let Ok(mut held) = MODE.lock() {
        *held = Some((Instant::now(), mode));
    }
}

/// Whether this plugin shows faux data now.
pub fn on(_backend: &Backend) -> bool {
    current() != Mode::Live
}

/// An answer marked with whose faux data it shows, for the frontend to say so at the top.
pub fn marked(response: Response) -> Response {
    match current() {
        Mode::Live => response,
        _ => response.faux(PROVIDER),
    }
}

/// Where an API answer's data came from: `null` when it is live.
pub fn said() -> Value {
    match current() {
        Mode::Live => Value::Null,
        _ => json!({ "provider": PROVIDER }),
    }
}

/// What an agent is told first, so it never passes faux data on as measured.
pub fn prefix() -> String {
    match current() {
        Mode::Live => String::new(),
        _ => format!("FAUX DATA provided by {PROVIDER}, made up rather than measured. "),
    }
}

pub fn encoded(pairs: &[(&str, &str)]) -> String {
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        out.append_pair(key, value);
    }
    out.finish()
}

/// One of the provider's `discovery/faux/<route>` routes, answered.
pub async fn ask(backend: &Backend, route: &str, query: &str) -> Result<Value, Refusal> {
    if current() == Mode::Stopped {
        return Err(Refusal::unavailable(format!(
            "{PROVIDER}, which provides this page's faux data, is not running"
        )));
    }
    let path = format!("faux/{route}");
    let asked = backend.discovery(PROVIDER, "GET", &path, Some(query), None);
    match tokio::time::timeout(ASKING, asked).await {
        Ok(Ok((200, body))) => Ok(body),
        Ok(Ok((status, body))) => {
            let said = body["detail"].as_str().unwrap_or("no reason given");
            Err(Refusal::unavailable(format!("{PROVIDER} answered {status}: {said}")))
        }
        Ok(Err(err)) => Err(Refusal::unavailable(format!("{PROVIDER} could not be asked: {err}"))),
        Err(_) => Err(Refusal::unavailable(format!("{PROVIDER} did not answer in time"))),
    }
}

/// A service as the provider's estate has it: the stand-ins its faux data is made up for.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Estate {
    #[serde(default)]
    pub repositories: Vec<String>,
}

/// The estate of these services, and the services — name and title — made up where the
/// Catalogue has none.
pub async fn estate(
    backend: &Backend,
    services: &BTreeSet<String>,
) -> Result<(BTreeMap<String, Estate>, Vec<(String, String)>), Refusal> {
    let pairs: Vec<(&str, &str)> =
        services.iter().map(|service| ("service", service.as_str())).collect();
    let answer = ask(backend, "estate", &encoded(&pairs)).await?;
    let found: BTreeMap<String, Estate> =
        serde_json::from_value(answer["services"].clone()).unwrap_or_default();
    let defaults = answer["defaults"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|service| {
            let name = service["name"].as_str()?.to_string();
            let title = service["title"].as_str().unwrap_or(&name).to_string();
            Some((name, title))
        })
        .collect();
    Ok((found, defaults))
}

// ---- CI/CD/CT's own ----------------------------------------------------------------------------

/// How far before a period runs are asked for, so a branch already broken when it starts is read
/// from the run that broke it.
const BEFORE_DAYS: i64 = 8;
/// Repositories asked about at once.
const AT_ONCE: usize = 4;

/// Every faux run of one repository that finished from `from` to `to`, a page at a time.
async fn runs_of(
    backend: &Backend,
    repository: &str,
    (from, to): (DateTime<Utc>, DateTime<Utc>),
    default_only: bool,
) -> Result<Vec<Run>, Refusal> {
    let mut found = Vec::new();
    let mut after: Option<u64> = None;
    loop {
        let (from, to) = (from.to_rfc3339(), to.to_rfc3339());
        let after_text = after.map(|after| after.to_string()).unwrap_or_default();
        let mut pairs = vec![("repository", repository), ("from", &from), ("to", &to)];
        if after.is_some() {
            pairs.push(("after", &after_text));
        }
        if default_only {
            pairs.push(("default_branch", "true"));
        }
        let answer = ask(backend, "pipelines", &encoded(&pairs)).await?;
        let runs: Vec<Run> = serde_json::from_value(answer["runs"].clone()).map_err(|err| {
            Refusal::unavailable(format!("{PROVIDER}'s workflow runs could not be read: {err}"))
        })?;
        found.extend(runs);
        match answer["next"].as_u64() {
            Some(next) => after = Some(next),
            None => return Ok(found),
        }
    }
}

/// The faux runs of these repositories that finished in the period, oldest first; only the
/// default branch's where that is all that counts, since a year of every branch's is a lot to
/// carry.
pub async fn runs(
    backend: &Backend,
    repositories: &BTreeSet<String>,
    (from, to): (DateTime<Utc>, DateTime<Utc>),
    default_only: bool,
) -> Result<Vec<Run>, Refusal> {
    let owned: Vec<String> = repositories.iter().cloned().collect();
    let answers: Vec<Result<Vec<Run>, Refusal>> =
        futures::stream::iter(owned)
            .map(|repository| async move {
                runs_of(backend, &repository, (from, to), default_only).await
            })
            .buffer_unordered(AT_ONCE)
            .collect()
            .await;
    let mut found = Vec::new();
    for answer in answers {
        found.extend(answer?);
    }
    found.sort_by_key(|run| run.finished_at);
    Ok(found)
}

/// The daily figures and broken spells of these repositories in the period, worked out from
/// faux runs exactly as live ones are, so the stages in the settings apply.
pub async fn held(
    backend: &Backend,
    repositories: &BTreeSet<String>,
    period: &Period,
    definitions: &Definitions,
) -> Result<Held, Refusal> {
    let first_day = period.first_day();
    let from = first_day - chrono::Duration::days(BEFORE_DAYS);
    let every = runs(backend, repositories, (from, period.to), definitions.default_only).await?;
    let mut held = Held::default();
    for repository in repositories {
        let theirs: Vec<Run> =
            every.iter().filter(|run| &run.repository == repository).cloned().collect();
        let (days, recoveries) =
            compute::derive(definitions, PROVIDER, repository, &theirs, first_day);
        held.days.extend(days.into_iter().filter(|day| period.holds_day(day.day)));
        held.recoveries
            .extend(recoveries.into_iter().filter(|spell| period.contains(spell.broke_at)));
    }
    held.recoveries.sort_by_key(|spell| spell.broke_at);
    Ok(held)
}

/// Each service's stand-in repositories; with none asked about, the services made up where the
/// Catalogue has none, each with its own.
pub async fn repositories(
    backend: &Backend,
    services: &BTreeSet<String>,
) -> Result<BTreeSet<String>, Refusal> {
    let (found, _) = estate(backend, services).await?;
    Ok(found.into_values().flat_map(|estate| estate.repositories).collect())
}

/// The services made up where the Catalogue has none, with their stand-in repositories.
pub async fn members(backend: &Backend) -> Result<Vec<Member>, Refusal> {
    let (_, defaults) = estate(backend, &BTreeSet::new()).await?;
    let names: BTreeSet<String> = defaults.iter().map(|(name, _)| name.clone()).collect();
    let (found, _) = estate(backend, &names).await?;
    Ok(defaults
        .into_iter()
        .map(|(name, title)| {
            let repositories = found
                .get(&name)
                .map(|estate| estate.repositories.iter().cloned().collect())
                .unwrap_or_default();
            Member { name, title, repositories }
        })
        .collect())
}
