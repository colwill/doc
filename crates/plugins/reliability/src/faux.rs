//! Faux data (FIX-FAUX-DATA): whether `faux-data` is providing this plugin's data — its toggle on
//! faux-data's Settings page, read from the `serving` record it exports here — and asking it for
//! that data, which this plugin works out with the same code as live data. Nothing asked for is
//! stored and nothing is announced from it.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use doc_plugin_sdk::{Backend, Query, Response};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

use crate::metrics::{Held, Period};
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

/// The services, name and title, made up where the Catalogue has none.
pub async fn defaults(backend: &Backend) -> Result<Vec<(String, String)>, Refusal> {
    let answer = ask(backend, "estate", "").await?;
    Ok(answer["defaults"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|service| {
            let name = service["name"].as_str()?.to_string();
            let title = service["title"].as_str().unwrap_or(&name).to_string();
            Some((name, title))
        })
        .collect())
}

// ---- Reliability's own -------------------------------------------------------------------------

fn part<T: DeserializeOwned>(answer: &Value, name: &str) -> Result<T, Refusal> {
    serde_json::from_value(answer[name].clone()).map_err(|err| {
        Refusal::unavailable(format!("{PROVIDER}'s {name} could not be read: {err}"))
    })
}

/// Everything these subjects would have kept for the period — services, DOC's parts and its
/// plugins — as `faux-data` makes it up, to be judged by the same code as live records.
pub async fn held(backend: &Backend, keys: &[String], period: &Period) -> Result<Held, Refusal> {
    let (from, to) = (period.from.to_rfc3339(), period.to.to_rfc3339());
    let mut pairs: Vec<(&str, &str)> = keys.iter().map(|key| ("subject", key.as_str())).collect();
    pairs.extend([("from", from.as_str()), ("to", to.as_str())]);
    let answer = ask(backend, "reliability", &encoded(&pairs)).await?;
    Ok(Held {
        subjects: part(&answer, "subjects")?,
        outages: part(&answer, "outages")?,
        backups: part(&answer, "backups")?,
        restores: part(&answer, "restores")?,
        coverage: part(&answer, "coverage")?,
        // Faux data is made up a day at a time, so a made-up hour is read from the day it falls
        // in, spread evenly, exactly as a live one was before telemetry was kept.
        samples: Vec::new(),
        sampled: false,
    })
}

/// The services made up where the Catalogue has none.
pub async fn members(backend: &Backend) -> Result<Vec<Member>, Refusal> {
    Ok(defaults(backend).await?.into_iter().map(|(name, title)| Member { name, title }).collect())
}
