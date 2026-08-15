//! Faux data (FIX-FAUX-DATA): whether `faux-data` is providing this plugin's data — its toggle on
//! faux-data's Settings page, read from the `serving` record it exports here — and asking it for
//! that data, which this plugin works out with the same code as live data. Nothing asked for is
//! stored and nothing is announced from it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use doc_plugin_sdk::{Backend, Query, Response};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::plan::{Held, Issue, Project, Version};
use crate::scope::Member;
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
    pub projects: Vec<String>,
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

// ---- The roadmap's own -------------------------------------------------------------------------

/// These services, each with the Jira project the estate puts its releases in, called what the
/// Catalogue calls it.
pub async fn named(
    backend: &Backend,
    titles: &BTreeMap<String, String>,
) -> Result<Vec<Member>, Refusal> {
    let names: BTreeSet<String> = titles.keys().cloned().collect();
    let (found, _) = estate(backend, &names).await?;
    Ok(titles
        .iter()
        .map(|(name, title)| Member {
            name: name.clone(),
            title: title.clone(),
            projects: found
                .get(name)
                .map(|estate| estate.projects.iter().cloned().collect())
                .unwrap_or_default(),
            components: BTreeSet::new(),
            labels: BTreeSet::new(),
            tracked: BTreeSet::new(),
        })
        .collect())
}

/// The services made up where the Catalogue has none, each with its project.
pub async fn members(backend: &Backend) -> Result<Vec<Member>, Refusal> {
    let (_, defaults) = estate(backend, &BTreeSet::new()).await?;
    named(backend, &defaults.into_iter().collect()).await
}

fn part<T: DeserializeOwned>(answer: &Value, name: &str) -> Result<Vec<T>, Refusal> {
    serde_json::from_value(answer[name].clone()).map_err(|err| {
        Refusal::unavailable(format!("{PROVIDER}'s {name} could not be read: {err}"))
    })
}

/// The projects, releases and issues of these services, as Jira's release data would hold
/// them, from `faux-data` as their source.
pub async fn held(backend: &Backend, members: &[Member]) -> Result<Held, Refusal> {
    let pairs: Vec<(&str, &str)> =
        members.iter().map(|member| ("service", member.name.as_str())).collect();
    let answer = ask(backend, "releases", &encoded(&pairs)).await?;
    let source = PROVIDER.to_string();
    let mut held = Held::default();
    for project in part::<Project>(&answer, "projects")? {
        held.projects.insert((source.clone(), project.key.clone()), project);
    }
    held.versions =
        part::<Version>(&answer, "versions")?.into_iter().map(|v| (source.clone(), v)).collect();
    held.issues =
        part::<Issue>(&answer, "issues")?.into_iter().map(|i| (source.clone(), i)).collect();
    Ok(held)
}
