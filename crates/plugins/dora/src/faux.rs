//! Faux data (FIX-FAUX-DATA): whether `faux-data` is providing this plugin's data — its toggle on
//! faux-data's Settings page, read from the `serving` record it exports here — and asking it for
//! that data, which this plugin works out with the same code as live data. Nothing asked for is
//! stored and nothing is announced from it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::{DateTime, Duration as Days, Utc};
use doc_plugin_sdk::{Backend, Query, Response};
use futures::StreamExt;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::compute;
use crate::metrics::Period;
use crate::scope::Member;
use crate::settings::Definitions;
use crate::store::{Counted, Deployment, Merged, Shipped};
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

// ---- DORA's own --------------------------------------------------------------------------------

/// How many days after a period ends are asked for too, so a failure near its end recovers as it
/// would in a longer period.
const AFTER_DAYS: i64 = 3;
/// Repositories asked about at once.
const AT_ONCE: usize = 4;

/// What `faux-data` made up for one repository, as GitHub's delivery data and `dora`'s own
/// counters would hold it.
#[derive(Debug, Default, Deserialize)]
struct Delivered {
    #[serde(default)]
    deployments: Vec<Shipped>,
    #[serde(default)]
    pull_requests: Vec<Merged>,
    #[serde(default)]
    incidents: Vec<Counted>,
}

async fn delivered(
    backend: &Backend,
    repository: String,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<(String, Delivered), Refusal> {
    let (from, to) = (from.to_rfc3339(), to.to_rfc3339());
    let query = encoded(&[("repository", &repository), ("from", &from), ("to", &to)]);
    let answer = ask(backend, "delivery", &query).await?;
    let delivered = serde_json::from_value(answer).map_err(|err| {
        Refusal::unavailable(format!("{PROVIDER}'s delivery data could not be read: {err}"))
    })?;
    Ok((repository, delivered))
}

async fn every(
    backend: &Backend,
    repositories: &BTreeSet<String>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<(String, Delivered)>, Refusal> {
    let owned: Vec<String> = repositories.iter().cloned().collect();
    let answers: Vec<Result<(String, Delivered), Refusal>> = futures::stream::iter(owned)
        .map(|repository| delivered(backend, repository, from, to))
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    answers.into_iter().collect()
}

/// The deployments of these repositories in the period, worked out from faux delivery data
/// exactly as live ones are, so the failure signals and definitions in the settings apply.
pub async fn deployments(
    backend: &Backend,
    repositories: &BTreeSet<String>,
    period: &Period,
    definitions: &Definitions,
) -> Result<Vec<Deployment>, Refusal> {
    let from = period.from - definitions.window() - Days::days(2);
    let to = period.to + Days::days(AFTER_DAYS);
    let mut found: Vec<Deployment> = every(backend, repositories, from, to)
        .await?
        .into_iter()
        .flat_map(|(repository, delivered)| {
            compute::derive(
                definitions,
                PROVIDER,
                &repository,
                &delivered.deployments,
                &delivered.pull_requests,
                &delivered.incidents,
            )
        })
        .filter(|deployment| period.contains(deployment.deployed_at))
        .collect();
    found.sort_by_key(|deployment| deployment.deployed_at);
    Ok(found)
}

/// The incidents counted for these repositories in the period, as faux data has them.
pub async fn counted(
    backend: &Backend,
    repositories: &BTreeSet<String>,
    period: &Period,
) -> Result<Vec<Counted>, Refusal> {
    Ok(every(backend, repositories, period.from, period.to)
        .await?
        .into_iter()
        .flat_map(|(_, delivered)| delivered.incidents)
        .filter(|count| period.contains(count.at))
        .collect())
}

/// Each service's stand-in repositories.
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
