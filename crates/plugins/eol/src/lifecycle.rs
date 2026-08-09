//! Release cycles: endoflife.date's from DOC's copy of it, or a service's own file in its format,
//! kept for a day; judged — supported, security fixes only, ending soon, or past its end of life —
//! against today or against the day something is due to ship.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;
use std::time::Duration as Wait;

use chrono::{Duration, NaiveDate, Utc};
use doc_plugin_sdk::telemetry::sent;
use doc_plugin_sdk::{Backend, PluginError, Query};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};
use url::Url;

use crate::settings::Definitions;
use crate::store::{Product, Release, SUMMARY};
use crate::{Refusal, faux, mirror};

/// A product read in the last day is not read again when a page asks for it.
const FRESH_HOURS: i64 = 24;
/// One that could not be read is not asked for again sooner than this.
const RETRY_MINUTES: i64 = 60;
const TIMEOUT: Wait = Wait::from_secs(10);
/// Products read from endoflife.date at once.
const AT_ONCE: usize = 8;
/// The most a lifecycle file may be.
const MAX_BYTES: usize = 2 * 1024 * 1024;
const URL_PREFIX: &str = "url:";

static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

pub fn client() -> Result<&'static reqwest::Client, PluginError> {
    if let Some(http) = HTTP.get() {
        return Ok(http);
    }
    let built = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("doc-eol/", env!("CARGO_PKG_VERSION")))
        // A redirect could lead to a host the settings do not list.
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|err| {
            PluginError::Message(format!("the HTTP client could not be built: {err}"))
        })?;
    Ok(HTTP.get_or_init(|| built))
}

/// Where a service's own lifecycle data is, as a product's key.
pub fn custom(url: &str) -> String {
    format!("{URL_PREFIX}{url}")
}

pub fn is_custom(product: &str) -> bool {
    product.starts_with(URL_PREFIX)
}

/// An error with everything that caused it: reqwest's own says only "error sending request".
pub fn cause(err: &(dyn std::error::Error + 'static)) -> String {
    let mut said = err.to_string();
    let mut source = err.source();
    while let Some(next) = source {
        let next_said = next.to_string();
        if !said.contains(&next_said) {
            said.push_str(": ");
            said.push_str(&next_said);
        }
        source = next.source();
    }
    said
}

// ---- reading ----------------------------------------------------------------------------------

fn date(value: &Value) -> Option<NaiveDate> {
    value.as_str().and_then(|text| NaiveDate::parse_from_str(text.get(..10)?, "%Y-%m-%d").ok())
}

/// endoflife.date's older fields are a date, or `true` or `false` where there is none.
fn date_or_flag(value: &Value) -> (Option<NaiveDate>, bool) {
    match value {
        Value::Bool(flag) => (None, *flag),
        other => (date(other), false),
    }
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.trim().is_empty() => Some(text.trim().to_string()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

/// A release as endoflife.date's API v1 gives it.
fn release_v1(release: &Value) -> Option<Release> {
    Some(Release {
        name: text(&release["name"])?,
        label: text(&release["label"]),
        released: date(&release["releaseDate"]),
        lts: release["isLts"].as_bool().unwrap_or_default(),
        support_ends: date(&release["eoasFrom"]),
        support_ended: release["isEoas"].as_bool().unwrap_or_default(),
        eol: date(&release["eolFrom"]),
        ended: release["isEol"].as_bool().unwrap_or_default(),
        extended_ends: date(&release["eoesFrom"]),
        latest: text(&release["latest"]["name"]),
        latest_on: date(&release["latest"]["date"]),
        link: text(&release["latest"]["link"]),
    })
}

/// A cycle in endoflife.date's older format, which files of a service's own often still use.
fn release_legacy(cycle: &Value) -> Option<Release> {
    let (eol, ended) = date_or_flag(&cycle["eol"]);
    let (support_ends, support_ended) = date_or_flag(&cycle["support"]);
    let (extended_ends, _) = date_or_flag(&cycle["extendedSupport"]);
    let name = text(&cycle["cycle"])?;
    Some(Release {
        label: text(&cycle["codename"]).map(|codename| format!("{name} ({codename})")),
        name,
        released: date(&cycle["releaseDate"]),
        lts: matches!(&cycle["lts"], Value::Bool(true) | Value::String(_)),
        support_ends,
        support_ended,
        eol,
        ended,
        extended_ends,
        latest: text(&cycle["latest"]),
        latest_on: date(&cycle["latestReleaseDate"]),
        link: text(&cycle["link"]),
    })
}

/// What a lifecycle document holds: its label, link, category and releases, newest first. It may
/// be API v1's answer, its `result`, or the older list of cycles.
pub fn parse(document: &Value, fallback: &str) -> Result<Product, String> {
    let body = document.get("result").unwrap_or(document);
    let (releases, label, link, category) = match body {
        Value::Array(cycles) => {
            (cycles.iter().filter_map(release_legacy).collect::<Vec<_>>(), None, None, None)
        }
        Value::Object(_) => {
            let listed = body["releases"]
                .as_array()
                .ok_or("it has no releases: is it endoflife.date's format?")?;
            let releases = match listed.iter().any(|release| release.get("cycle").is_some()) {
                true => listed.iter().filter_map(release_legacy).collect(),
                false => listed.iter().filter_map(release_v1).collect(),
            };
            (
                releases,
                text(&body["label"]),
                text(&body["links"]["html"]).or_else(|| text(&body["link"])),
                text(&body["category"]),
            )
        }
        _ => return Err("it is not endoflife.date's format".into()),
    };
    let mut releases: Vec<Release> = releases;
    if releases.is_empty() {
        return Err("it lists no releases".into());
    }
    releases.sort_by_key(|release| std::cmp::Reverse(release.released));
    Ok(Product {
        product: String::new(),
        label: label.unwrap_or_else(|| fallback.to_string()),
        link,
        category,
        releases,
        read_at: Some(Utc::now()),
        ..Product::default()
    })
}

/// Whether a service's own lifecycle file may be read: from a listed host only.
fn permitted(address: &str, definitions: &Definitions) -> Result<Url, String> {
    let url = Url::parse(address).map_err(|err| format!("{address} is not a URL: {err}"))?;
    if !matches!(url.scheme(), "https" | "http") {
        return Err(format!("{address} is not an http or https address"));
    }
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if !definitions.custom_hosts.contains(&host) {
        return Err(format!(
            "{host} is not one of the hosts a service's own lifecycle data may come from; an \
             administrator can list it on the Settings page"
        ));
    }
    Ok(url)
}

async fn fetched(key: &str, definitions: &Definitions) -> Result<Product, String> {
    let http = client().map_err(|err| err.to_string())?;
    let (url, fallback) = match key.strip_prefix(URL_PREFIX) {
        Some(address) => {
            let url = permitted(address, definitions)?;
            let named = url.path_segments().and_then(|mut path| path.next_back()).unwrap_or("");
            let named = named.trim_end_matches(".json").to_string();
            (url, if named.is_empty() { address.to_string() } else { named })
        }
        None => {
            let url = format!("{}/api/v1/products/{}", definitions.base, key);
            let url = Url::parse(&url).map_err(|err| format!("{url} is not a URL: {err}"))?;
            (url, key.to_string())
        }
    };
    let answer = http.get(url.clone()).header("accept", "application/json").send().await;
    sent("endoflife", if is_custom(key) { "custom" } else { "product" }, &answer);
    let answer =
        answer.map_err(|err| format!("{} could not be read: {}", url.as_str(), cause(&err)))?;
    match answer.status().as_u16() {
        200 => {}
        404 if !is_custom(key) => {
            return Err(format!("endoflife.date has no product called {key}"));
        }
        status => return Err(format!("{} answered {status}", url.as_str())),
    }
    let bytes = answer
        .bytes()
        .await
        .map_err(|err| format!("{} was cut short: {}", url.as_str(), cause(&err)))?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("{} is over {} MiB", url.as_str(), MAX_BYTES / 1024 / 1024));
    }
    let document: Value =
        serde_json::from_slice(&bytes).map_err(|_| format!("{} is not JSON", url.as_str()))?;
    let mut product = parse(&document, &fallback)?;
    product.product = key.to_string();
    Ok(product)
}

/// Reads a product again and keeps it; one that cannot be read keeps what it had, and when it was
/// read, with why it could not be read again.
pub async fn refreshed(
    backend: &Backend,
    key: &str,
    kept: Option<Product>,
    definitions: &Definitions,
) -> Product {
    let mut product = match fetched(key, definitions).await {
        Ok(product) => product,
        Err(problem) => {
            let mut product = kept.unwrap_or_else(|| Product {
                product: key.to_string(),
                label: key.strip_prefix(URL_PREFIX).unwrap_or(key).to_string(),
                ..Product::default()
            });
            product.problem = Some(problem.chars().take(500).collect());
            product
        }
    };
    product.tried_at = Some(Utc::now());
    let values = json!({
        "product": product.product,
        "label": product.label,
        "link": product.link,
        "category": product.category,
        "releases": product.releases,
        "read_at": product.read_at,
        "tried_at": product.tried_at,
        "problem": product.problem,
    });
    if let Err(err) = backend.upsert::<Value>("products", &["product"], values).await {
        tracing::warn!(%err, product = key, "a product's release cycles were not kept");
    }
    product
}

/// Whether a product read before is read again: over a day ago, and not tried in the last hour.
fn due_again(kept: &Product) -> bool {
    let now = Utc::now();
    kept.read_at.is_none_or(|at| at < now - Duration::hours(FRESH_HOURS))
        && kept.tried_at.is_none_or(|at| at < now - Duration::minutes(RETRY_MINUTES))
}

/// The products kept by these names, without endoflife.date's own answers.
async fn kept(
    backend: &Backend,
    wanted: &BTreeSet<String>,
) -> Result<BTreeMap<String, Product>, PluginError> {
    let mut found = BTreeMap::new();
    let names: Vec<&String> = wanted.iter().collect();
    for chunk in names.chunks(200) {
        let asked =
            Query::new("products").filter(json!({ "product": { "in": chunk } })).fields(&SUMMARY);
        for product in backend.query_all::<Product>(asked).await? {
            found.insert(product.product.clone(), product);
        }
    }
    Ok(found)
}

/// Each product's release cycles: endoflife.date's from the copy, asked for alone only before it is
/// first read; a service's own file read again after a day; with faux data, `faux-data`'s.
pub async fn products(
    backend: &Backend,
    wanted: &BTreeSet<String>,
) -> Result<BTreeMap<String, Product>, Refusal> {
    if faux::on(backend) {
        return faux::products(backend, wanted).await;
    }
    let mut found = kept(backend, wanted).await?;
    let copied = mirror::held(backend).await.read_at.is_some();
    let mut due: Vec<(String, Option<Product>)> = Vec::new();
    for key in wanted {
        match found.get(key) {
            _ if copied && !is_custom(key) => {}
            Some(kept) if !due_again(kept) => {}
            kept => due.push((key.clone(), kept.cloned())),
        }
    }
    if copied {
        let untracked: Vec<&String> =
            wanted.iter().filter(|key| !is_custom(key) && !found.contains_key(*key)).collect();
        for key in untracked {
            let unknown = Product {
                product: key.clone(),
                label: key.clone(),
                problem: Some(format!("endoflife.date has no product called {key}")),
                ..Product::default()
            };
            found.insert(key.clone(), unknown);
        }
    }
    if due.is_empty() {
        return Ok(found);
    }
    let definitions = Definitions::read(&backend.settings());
    let read: Vec<Product> = futures::stream::iter(due)
        .map(|(key, kept)| {
            let definitions = definitions.clone();
            async move { refreshed(backend, &key, kept, &definitions).await }
        })
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    for product in read {
        found.insert(product.product.clone(), product);
    }
    Ok(found)
}

/// The daily schedule: DOC's copy of endoflife.date in one read, then each service's own file. A
/// copy that could not be read fails the run, after the files are read, so it is tried again.
pub async fn refresh_all(backend: &Backend) -> Result<Value, PluginError> {
    let copied = mirror::read(backend).await;
    let files = refresh_files(backend).await?;
    match copied {
        Ok(copied) => Ok(json!({ "copy": copied, "files": files })),
        Err(err) => Err(err),
    }
}

/// Reads every service's own lifecycle file kept again.
async fn refresh_files(backend: &Backend) -> Result<Value, PluginError> {
    let asked = Query::new("products").filter(json!({ "product": { "prefix": URL_PREFIX } }));
    let kept: Vec<Product> = backend
        .query_all::<Product>(asked.fields(&SUMMARY))
        .await?
        .into_iter()
        .filter(|product| is_custom(&product.product))
        .collect();
    let definitions = Definitions::read(&backend.settings());
    let count = kept.len();
    let read: Vec<Product> = futures::stream::iter(kept)
        .map(|product| {
            let definitions = definitions.clone();
            async move {
                let key = product.product.clone();
                refreshed(backend, &key, Some(product), &definitions).await
            }
        })
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    let problems = read.iter().filter(|product| product.problem.is_some()).count();
    Ok(json!({ "read": count, "problems": problems }))
}

// ---- judging ----------------------------------------------------------------------------------

/// Where a release stands, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Ended,
    Ending,
    Security,
    Supported,
    Unknown,
}

impl Status {
    pub const ALL: [Self; 5] =
        [Self::Ended, Self::Ending, Self::Security, Self::Supported, Self::Unknown];

    pub fn word(self) -> &'static str {
        match self {
            Self::Ended => "End of life",
            Self::Ending => "Ending soon",
            Self::Security => "Security fixes only",
            Self::Supported => "Supported",
            Self::Unknown => "Not known",
        }
    }

    /// The badge it is shown with: its meaning, since colour alone says nothing.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Ended => "error",
            Self::Ending => "degraded",
            Self::Security => "loading",
            Self::Supported => "ready",
            Self::Unknown => "unknown",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Self::Ended => "ended",
            Self::Ending => "ending",
            Self::Security => "security",
            Self::Supported => "supported",
            Self::Unknown => "unknown",
        }
    }

    /// Whether it is worse than supported: what a page counts as needing attention.
    pub fn concerning(self) -> bool {
        matches!(self, Self::Ended | Self::Ending)
    }
}

impl Release {
    /// Its end of life has come by `day`.
    pub fn ended_by(&self, day: NaiveDate) -> bool {
        match self.eol {
            Some(eol) => eol <= day,
            None => self.ended,
        }
    }

    pub fn status(&self, today: NaiveDate, warn_days: i64) -> Status {
        if self.ended_by(today) {
            return Status::Ended;
        }
        if self.eol.is_some_and(|eol| eol <= today + Duration::days(warn_days)) {
            return Status::Ending;
        }
        let support_over = match self.support_ends {
            Some(ends) => ends <= today,
            None => self.support_ended,
        };
        if support_over { Status::Security } else { Status::Supported }
    }

    pub fn title(&self) -> String {
        self.label.clone().unwrap_or_else(|| self.name.clone())
    }
}

/// The release a version belongs to: `20.11.1` is Node.js 20, `3.12.4` Python 3.12, and a name
/// matches whatever its case.
pub fn matched<'a>(product: &'a Product, version: &str) -> Option<&'a Release> {
    let mut wanted = version.trim().trim_start_matches(['v', 'V']).to_ascii_lowercase();
    loop {
        if let Some(found) =
            product.releases.iter().find(|release| release.name.to_ascii_lowercase() == wanted)
        {
            return Some(found);
        }
        let (shorter, _) = wanted.rsplit_once(['.', '-'])?;
        wanted = shorter.to_string();
    }
}

pub fn day(date: NaiveDate) -> String {
    date.format("%-d %b %Y").to_string()
}

pub fn today() -> NaiveDate {
    Utc::now().date_naive()
}
