//! Faux data (FIX-FAUX-DATA): whether `faux-data` is providing this plugin's data — its toggle on
//! faux-data's Settings page, read from the `serving` record it exports here — and asking it for
//! that data, which this plugin works out with the same code as live data. Nothing asked for is
//! stored and nothing is announced from it.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use doc_plugin_sdk::{Backend, Query, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::lifecycle;
use crate::scope::{Member, Used};
use crate::store::Product;
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
    pub products: Vec<String>,
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

// ---- End of life's own -------------------------------------------------------------------------

/// `nodejs@20`, or a product with no version.
fn parsed(written: &str) -> Used {
    match written.split_once('@') {
        Some((product, version)) => Used {
            product: product.to_string(),
            version: Some(version.to_string()),
            from: Vec::new(),
        },
        None => Used { product: written.to_string(), version: None, from: Vec::new() },
    }
}

/// These services, each with what the estate says it runs, called what the Catalogue calls it.
pub async fn named(
    backend: &Backend,
    titles: &BTreeMap<String, String>,
) -> Result<Vec<Member>, Refusal> {
    let names: BTreeSet<String> = titles.keys().cloned().collect();
    let (found, _) = estate(backend, &names).await?;
    Ok(titles
        .iter()
        .map(|(name, title)| Member {
            repositories: Vec::new(),
            name: name.clone(),
            title: title.clone(),
            used: found
                .get(name)
                .map(|estate| estate.products.iter().map(|written| parsed(written)).collect())
                .unwrap_or_default(),
            problems: Vec::new(),
        })
        .collect())
}

/// The services made up where the Catalogue has none, with what each runs.
pub async fn members(backend: &Backend) -> Result<Vec<Member>, Refusal> {
    let (_, defaults) = estate(backend, &BTreeSet::new()).await?;
    named(backend, &defaults.into_iter().collect()).await
}

/// Every product `faux-data` makes up, each as endoflife.date's list of every product gives it.
pub async fn catalogue(backend: &Backend) -> Result<Vec<Value>, Refusal> {
    let answer = ask(backend, "lifecycles", "").await?;
    Ok(answer["products"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(_, document)| document.get("result").cloned())
        .collect())
}

/// Each product's release cycles, as `faux-data` makes them up, read as endoflife.date's are;
/// one it does not make up is left out, and shows as not known.
pub async fn products(
    backend: &Backend,
    wanted: &BTreeSet<String>,
) -> Result<BTreeMap<String, Product>, Refusal> {
    let pairs: Vec<(&str, &str)> = wanted.iter().map(|key| ("product", key.as_str())).collect();
    let answer = ask(backend, "lifecycles", &encoded(&pairs)).await?;
    Ok(answer["products"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(key, document)| {
            let mut product = lifecycle::parse(document, key).ok()?;
            product.product = key.clone();
            Some((key.clone(), product))
        })
        .collect())
}
