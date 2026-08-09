//! DOC's own copy of endoflife.date: every product it tracks, read in one request each night and
//! kept as it answered, so no page waits on it and a failed read loses nothing; and served again in
//! endoflife.date's own format, so teams' tools can read it from DOC.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};
use doc_plugin_sdk::telemetry::sent;
use doc_plugin_sdk::{Backend, DataRequest, PluginError, Query, Request, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::lifecycle::{self, cause, is_custom};
use crate::settings::Definitions;
use crate::store::{Product, SUMMARY};
use crate::{Refusal, faux, packages};

/// Where the copy stands, in the plugin's state.
const STATE: &str = "mirror";
const READING: Duration = Duration::from_secs(60);
const MAX_BYTES: usize = 32 * 1024 * 1024;
/// Products written in one transaction; the largest answers are over 100 KiB.
const WRITTEN_AT_ONCE: usize = 50;
/// A copy older than this is read again when the plugin loads, as when a night's read was missed.
const STALE_HOURS: i64 = 26;
/// What endoflife.date's API v1 answers are, where the copy does not say.
const SCHEMA: &str = "1.2.1";
/// Where DOC serves the copy, for the address endoflife.date gives each product.
const SERVED: &str = "/api/v1/plugins/eol/api/v1";

/// Where the copy stands: when endoflife.date last answered with every product, and when it was
/// last asked and why it did not answer.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Mirror {
    /// Where it was read from, with no trailing slash.
    pub base: String,
    pub read_at: Option<DateTime<Utc>>,
    /// endoflife.date's own stamps on that answer.
    pub generated_at: Option<String>,
    pub schema_version: Option<String>,
    pub etag: Option<String>,
    /// How many products it listed.
    pub products: usize,
    /// Their names, in the order it listed them, which is the order they are served in.
    pub order: Vec<String>,
    pub tried_at: Option<DateTime<Utc>>,
    pub problem: Option<String>,
}

pub async fn held(backend: &Backend) -> Mirror {
    match backend.state_get(STATE).await {
        Ok(Some(value)) => serde_json::from_value(value).unwrap_or_default(),
        _ => Mirror::default(),
    }
}

async fn keep(backend: &Backend, mirror: &Mirror) {
    if let Err(err) = backend.state_set(STATE, json!(mirror)).await {
        tracing::warn!(%err, "where the copy of endoflife.date stands was not kept");
    }
}

/// Whether to read the copy when the plugin loads: it never has been, it was read from somewhere
/// else, it is over a day old, or the last read failed.
pub async fn due_at_load(backend: &Backend) -> bool {
    let mirror = held(backend).await;
    let base = Definitions::read(&backend.settings()).base;
    let stale = Utc::now() - chrono::Duration::hours(STALE_HOURS);
    mirror.base != base || mirror.problem.is_some() || mirror.read_at.is_none_or(|at| at < stale)
}

// ---- reading ----------------------------------------------------------------------------------

enum Answer {
    Unchanged,
    Every(Value, Option<String>),
}

async fn fetched(url: &str, etag: Option<&str>) -> Result<Answer, String> {
    let http = lifecycle::client().map_err(|err| err.to_string())?;
    let mut asked = http.get(url).header("accept", "application/json").timeout(READING);
    if let Some(etag) = etag {
        asked = asked.header("if-none-match", etag);
    }
    let answer = asked.send().await;
    sent("endoflife", "products", &answer);
    let answer = answer.map_err(|err| format!("{url} could not be read: {}", cause(&err)))?;
    match answer.status().as_u16() {
        304 => return Ok(Answer::Unchanged),
        200 => {}
        status => return Err(format!("{url} answered {status}")),
    }
    let etag = answer.headers().get("etag").and_then(|tag| tag.to_str().ok()).map(str::to_string);
    let bytes =
        answer.bytes().await.map_err(|err| format!("{url} was cut short: {}", cause(&err)))?;
    if bytes.len() > MAX_BYTES {
        return Err(format!("{url} is over {} MiB", MAX_BYTES / 1024 / 1024));
    }
    let document: Value =
        serde_json::from_slice(&bytes).map_err(|_| format!("{url} is not JSON"))?;
    if !document["result"].is_array() {
        return Err(format!("{url} lists no products: is it endoflife.date's API v1?"));
    }
    Ok(Answer::Every(document, etag))
}

fn text(value: &Value) -> Option<String> {
    value.as_str().map(str::trim).filter(|text| !text.is_empty()).map(str::to_string)
}

fn texts(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(text).collect()
}

fn digest(product: &Value) -> String {
    hex::encode(Sha256::digest(product.to_string().as_bytes()))
}

/// A product as kept: what judging reads, and endoflife.date's answer for it as it was.
fn record(name: &str, product: &Value, digest: &str, now: DateTime<Utc>) -> Value {
    let (label, link, category, releases, problem) = match lifecycle::parse(product, name) {
        Ok(parsed) => (parsed.label, parsed.link, parsed.category, parsed.releases, None),
        Err(problem) => (
            text(&product["label"]).unwrap_or_else(|| name.to_string()),
            text(&product["links"]["html"]),
            text(&product["category"]),
            Vec::new(),
            Some(format!("endoflife.date's answer for it could not be read: {problem}")),
        ),
    };
    json!({
        "product": name,
        "label": label,
        "link": link,
        "category": category.map(|category| category.chars().take(64).collect::<String>()),
        "releases": releases,
        "read_at": now,
        "tried_at": now,
        "problem": problem,
        "document": product,
        "digest": digest,
        "changed_at": now,
        "listed": true,
        "aliases": texts(&product["aliases"]),
        "tags": texts(&product["tags"]),
        "packages": packages::package_keys(product),
    })
}

/// The products listed, in order; how many of them changed; and how many are no longer listed.
struct Written {
    listed: Vec<String>,
    changed: usize,
    dropped: usize,
}

/// Keeps every product whose answer changed. One endoflife.date no longer lists is kept and marked
/// so, since a service may still run it; one only ever read on its own and not listed is removed.
async fn written(backend: &Backend, document: &Value) -> Result<Written, PluginError> {
    #[derive(Deserialize)]
    struct Held {
        product: String,
        #[serde(default)]
        digest: Option<String>,
        #[serde(default)]
        listed: Option<bool>,
    }
    let asked = Query::new("products").fields(&["product", "digest", "listed"]);
    let held: BTreeMap<String, Held> = backend
        .query_all::<Held>(asked)
        .await?
        .into_iter()
        .filter(|held| !is_custom(&held.product))
        .map(|held| (held.product.clone(), held))
        .collect();
    let now = Utc::now();
    let mut writes = Vec::new();
    let mut listed = Vec::new();
    let mut changed = 0;
    for product in document["result"].as_array().into_iter().flatten() {
        let Some(name) = text(&product["name"]).filter(|name| !is_custom(name)) else { continue };
        let digest = digest(product);
        match held.get(&name) {
            Some(kept) if kept.digest.as_deref() == Some(digest.as_str()) => {
                if kept.listed == Some(false) {
                    writes.push(DataRequest::update(
                        "products",
                        name.as_str(),
                        json!({ "listed": true }),
                    ));
                }
            }
            _ => {
                changed += 1;
                let values = record(&name, product, &digest, now);
                writes.push(DataRequest::upsert("products", &["product"], values));
            }
        }
        listed.push(name);
    }
    let mut dropped = 0;
    let named: BTreeSet<&String> = listed.iter().collect();
    for kept in held.values().filter(|kept| !named.contains(&kept.product)) {
        match (&kept.digest, kept.listed) {
            (None, _) => writes.push(DataRequest::delete("products", kept.product.as_str())),
            (Some(_), Some(false)) => {}
            (Some(_), _) => {
                dropped += 1;
                let unlisted = json!({ "listed": false });
                writes.push(DataRequest::update("products", kept.product.as_str(), unlisted));
            }
        }
    }
    for chunk in writes.chunks(WRITTEN_AT_ONCE) {
        backend.batch(chunk.to_vec()).await?;
    }
    Ok(Written { listed, changed, dropped })
}

/// Reads every product endoflife.date tracks in one request, and keeps those whose answer changed.
/// One that cannot be read keeps the copy as it was, says why, and fails, so the run is retried.
pub async fn read(backend: &Backend) -> Result<Value, PluginError> {
    let base = Definitions::read(&backend.settings()).base;
    let mut mirror = held(backend).await;
    let url = format!("{base}/api/v1/products/full");
    // An answer from somewhere else, or a copy never wholly written, is read whole.
    let whole = mirror.base == base && mirror.read_at.is_some() && !mirror.order.is_empty();
    let etag = mirror.etag.clone().filter(|_| whole);
    mirror.tried_at = Some(Utc::now());
    let (document, etag) = match fetched(&url, etag.as_deref()).await {
        Ok(Answer::Unchanged) => {
            mirror.problem = None;
            keep(backend, &mirror).await;
            return Ok(json!({ "products": mirror.products, "changed": 0, "unchanged": true }));
        }
        Ok(Answer::Every(document, etag)) => (document, etag),
        Err(problem) => {
            mirror.problem = Some(problem.chars().take(500).collect());
            keep(backend, &mirror).await;
            return Err(PluginError::from(problem));
        }
    };
    let written = match written(backend, &document).await {
        Ok(written) => written,
        Err(err) => {
            mirror.problem = Some(format!("what endoflife.date answered was not kept: {err}"));
            keep(backend, &mirror).await;
            return Err(err);
        }
    };
    packages::forget(backend).await;
    let mirror = Mirror {
        base,
        read_at: Some(Utc::now()),
        generated_at: text(&document["generated_at"]),
        schema_version: text(&document["schema_version"]),
        etag,
        products: written.listed.len(),
        order: written.listed,
        tried_at: mirror.tried_at,
        problem: None,
    };
    keep(backend, &mirror).await;
    tracing::info!(
        products = mirror.products,
        changed = written.changed,
        dropped = written.dropped,
        "the copy of endoflife.date was read"
    );
    Ok(json!({
        "products": mirror.products,
        "changed": written.changed,
        "dropped": written.dropped,
    }))
}

/// Every product in the copy, endoflife.date's own answers left out; or `faux-data`'s.
pub async fn every(backend: &Backend) -> Result<Vec<Product>, Refusal> {
    let mut every: Vec<Product> = match faux::on(backend) {
        true => faux::catalogue(backend)
            .await?
            .iter()
            .filter_map(|document| {
                let name = text(&document["name"])?;
                let mut product = lifecycle::parse(document, &name).ok()?;
                product.product = name;
                Some(product)
            })
            .collect(),
        false => backend
            .query_all::<Product>(Query::new("products").fields(&SUMMARY))
            .await?
            .into_iter()
            .filter(|product| !is_custom(&product.product))
            .collect(),
    };
    every.sort_by_cached_key(|product| product.label.to_lowercase());
    Ok(every)
}

// ---- serving ----------------------------------------------------------------------------------

/// The stamps on every answer, as endoflife.date puts them.
struct Stamps {
    schema_version: String,
    generated_at: Option<String>,
}

impl Stamps {
    fn listed(&self, result: &[Value]) -> Value {
        json!({
            "schema_version": self.schema_version,
            "generated_at": self.generated_at,
            "total": result.len(),
            "result": result,
        })
    }

    fn product(&self, result: &Value, changed_at: Option<DateTime<Utc>>) -> Value {
        json!({
            "schema_version": self.schema_version,
            "generated_at": self.generated_at,
            "last_modified": changed_at.map(|at| at.to_rfc3339_opts(SecondsFormat::Secs, false)),
            "result": result,
        })
    }

    fn release(&self, result: &Value) -> Value {
        json!({
            "schema_version": self.schema_version,
            "generated_at": self.generated_at,
            "result": result,
        })
    }
}

async fn stamps(backend: &Backend) -> Result<Stamps, Refusal> {
    if faux::on(backend) {
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, false);
        return Ok(Stamps { schema_version: SCHEMA.into(), generated_at: Some(now) });
    }
    let mirror = held(backend).await;
    if mirror.read_at.is_none() {
        return Err(Refusal::unavailable(match mirror.problem {
            Some(problem) => {
                format!("DOC's copy of endoflife.date has not been read yet: {problem}")
            }
            None => "DOC's copy of endoflife.date has not been read yet: it is read when End of \
                     life loads, and each night"
                .into(),
        }));
    }
    Ok(Stamps {
        schema_version: mirror.schema_version.unwrap_or_else(|| SCHEMA.into()),
        generated_at: mirror.generated_at,
    })
}

/// A product as endoflife.date's list of products gives it.
fn summary(document: &Value) -> Value {
    let name = document["name"].as_str().unwrap_or_default();
    json!({
        "name": name,
        "aliases": texts(&document["aliases"]),
        "label": document["label"],
        "category": document["category"],
        "tags": texts(&document["tags"]),
        "uri": format!("{SERVED}/products/{name}"),
    })
}

/// A product's release by its name, or its newest for `latest`, as endoflife.date answers.
fn release_of(document: &Value, wanted: &str) -> Result<Value, Refusal> {
    let releases = document["releases"].as_array().map(Vec::as_slice).unwrap_or_default();
    let found = match wanted {
        "latest" => releases.first(),
        wanted => releases.iter().find(|release| release["name"] == wanted).or_else(|| {
            releases.iter().find(|release| {
                release["name"].as_str().is_some_and(|name| name.eq_ignore_ascii_case(wanted))
            })
        }),
    };
    let label = document["label"].as_str().or(document["name"].as_str()).unwrap_or("it");
    found.cloned().ok_or_else(|| Refusal::missing(format!("{label} has no release {wanted}")))
}

/// Products in the order endoflife.date listed them, any it did not name last, by name.
fn in_order<T>(items: &mut [T], order: &[String], name: impl Fn(&T) -> &str) {
    let place: BTreeMap<&str, usize> =
        order.iter().enumerate().map(|(at, name)| (name.as_str(), at)).collect();
    items.sort_by(|one, two| {
        let (one, two) = (name(one), name(two));
        (place.get(one).unwrap_or(&usize::MAX), one)
            .cmp(&(place.get(two).unwrap_or(&usize::MAX), two))
    });
}

async fn summaries(backend: &Backend) -> Result<Vec<Value>, Refusal> {
    let documents: Vec<Value> = if faux::on(backend) {
        faux::catalogue(backend).await?
    } else {
        let asked = Query::new("products")
            .filter(json!({ "listed": true }))
            .fields(&["product", "label", "category", "aliases", "tags"]);
        backend
            .query_all::<Product>(asked)
            .await?
            .into_iter()
            .map(|product| {
                json!({
                    "name": product.product,
                    "aliases": product.aliases,
                    "label": product.label,
                    "category": product.category,
                    "tags": product.tags,
                })
            })
            .collect()
    };
    let mut summaries: Vec<Value> = documents.iter().map(summary).collect();
    let order = held(backend).await.order;
    in_order(&mut summaries, &order, |summary| summary["name"].as_str().unwrap_or_default());
    Ok(summaries)
}

async fn documents(backend: &Backend) -> Result<Vec<Value>, Refusal> {
    #[derive(Deserialize)]
    struct Held {
        product: String,
        #[serde(default)]
        document: Option<Value>,
    }
    if faux::on(backend) {
        return faux::catalogue(backend).await;
    }
    let asked =
        Query::new("products").filter(json!({ "listed": true })).fields(&["product", "document"]);
    let mut kept: Vec<Held> = backend.query_all(asked).await?;
    let order = held(backend).await.order;
    in_order(&mut kept, &order, |kept| kept.product.as_str());
    Ok(kept.into_iter().filter_map(|kept| kept.document).collect())
}

/// One product's answer, and when it last changed; one endoflife.date no longer lists is still
/// served by its name, as it was when it last did.
async fn one(backend: &Backend, name: &str) -> Result<(Value, Option<DateTime<Utc>>), Refusal> {
    #[derive(Deserialize)]
    struct Held {
        #[serde(default)]
        document: Option<Value>,
        #[serde(default)]
        changed_at: Option<DateTime<Utc>>,
    }
    let name = name.trim().to_ascii_lowercase();
    let missing = || Refusal::missing(format!("endoflife.date has no product called {name}"));
    if name.is_empty() || is_custom(&name) {
        return Err(missing());
    }
    if faux::on(backend) {
        let made_up = faux::catalogue(backend).await?;
        let found = made_up.into_iter().find(|document| document["name"] == name.as_str());
        return Ok((found.ok_or_else(missing)?, None));
    }
    let held = backend.get::<Held>("products", name.as_str()).await?.ok_or_else(missing)?;
    Ok((held.document.ok_or_else(missing)?, held.changed_at))
}

async fn answered(backend: &Backend, route: &str) -> Result<Value, Refusal> {
    let segments: Vec<String> = route.split('/').map(crate::api::decoded).collect();
    let segments: Vec<&str> = segments.iter().map(String::as_str).collect();
    let stamps = stamps(backend).await?;
    let mut answer = match segments.as_slice() {
        ["products"] => stamps.listed(&summaries(backend).await?),
        ["products", "full"] => stamps.listed(&documents(backend).await?),
        ["products", name] => {
            let (document, changed_at) = one(backend, name).await?;
            stamps.product(&document, changed_at)
        }
        ["products", name, "releases", release] => {
            let (document, _) = one(backend, name).await?;
            stamps.release(&release_of(&document, release)?)
        }
        _ => {
            return Err(Refusal::missing(
                "DOC serves endoflife.date's products, products/full, products/<product> and \
                 products/<product>/releases/<release or latest>",
            ));
        }
    };
    if faux::on(backend) {
        answer["faux"] = faux::said();
    }
    Ok(answer)
}

/// An answer with an ETag of what it says, so a tool asking again for what has not changed is
/// told so rather than sent it again.
fn tagged(request: &Request, answer: &Value) -> Response {
    let body = answer.to_string();
    let etag = format!("\"{}\"", &hex::encode(Sha256::digest(body.as_bytes()))[..32]);
    let known = request
        .headers
        .get("if-none-match")
        .is_some_and(|held| held.split(',').any(|tag| tag.trim().trim_start_matches("W/") == etag));
    if known {
        return Response::new(304, "application/json", Vec::new()).with_header("etag", &etag);
    }
    Response::new(200, "application/json", body)
        .with_header("etag", &etag)
        .with_header("cache-control", "no-cache")
}

/// endoflife.date's API v1, answered from DOC's copy for teams' own tools: what follows
/// `api/v1/` in the route.
pub async fn serve(backend: &Backend, request: &Request, route: &str) -> Response {
    match answered(backend, route).await {
        Ok(answer) => tagged(request, &answer),
        Err(refusal) => refusal.response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nodejs() -> Value {
        json!({
            "name": "nodejs",
            "aliases": ["node"],
            "label": "Node.js",
            "category": "framework",
            "tags": ["javascript-runtime"],
            "identifiers": [{ "type": "purl", "id": "pkg:generic/nodejs" }],
            "links": { "html": "https://endoflife.date/nodejs" },
            "releases": [
                { "name": "24", "label": "24 (LTS)", "releaseDate": "2025-05-06", "isLts": true,
                  "isEol": false, "eolFrom": "2028-04-30", "latest": { "name": "24.9.0" } },
                { "name": "22", "label": "22 (LTS)", "releaseDate": "2024-04-24", "isLts": true,
                  "isEol": false, "eolFrom": "2027-04-30", "latest": { "name": "22.20.0" } },
            ],
        })
    }

    fn stamps() -> Stamps {
        Stamps {
            schema_version: "1.2.1".into(),
            generated_at: Some("2026-10-01T00:08:50+00:00".into()),
        }
    }

    #[test]
    fn a_product_is_served_as_endoflife_date_answered_it() {
        let changed = DateTime::parse_from_rfc3339("2026-09-04T00:07:10Z").unwrap().to_utc();
        let answer = stamps().product(&nodejs(), Some(changed));
        assert_eq!(answer["result"], nodejs());
        assert_eq!(answer["schema_version"], "1.2.1");
        assert_eq!(answer["generated_at"], "2026-10-01T00:08:50+00:00");
        assert_eq!(answer["last_modified"], "2026-09-04T00:07:10+00:00");
    }

    #[test]
    fn the_list_names_each_product_with_where_doc_serves_it() {
        let answer = stamps().listed(&[summary(&nodejs())]);
        assert_eq!(answer["total"], 1);
        assert_eq!(
            answer["result"][0],
            json!({
                "name": "nodejs",
                "aliases": ["node"],
                "label": "Node.js",
                "category": "framework",
                "tags": ["javascript-runtime"],
                "uri": "/api/v1/plugins/eol/api/v1/products/nodejs",
            })
        );
    }

    #[test]
    fn a_release_is_found_by_its_name_and_latest_is_the_newest() {
        assert_eq!(release_of(&nodejs(), "latest").unwrap()["name"], "24");
        assert_eq!(release_of(&nodejs(), "22").unwrap()["latest"]["name"], "22.20.0");
        let missing = release_of(&nodejs(), "18").unwrap_err();
        assert_eq!((missing.status, missing.detail.as_str()), (404, "Node.js has no release 18"));
    }

    #[test]
    fn a_kept_product_is_what_judging_reads_and_the_answer_as_it_was() {
        let now = Utc::now();
        let kept = record("nodejs", &nodejs(), "abc", now);
        assert_eq!(kept["document"], nodejs());
        assert_eq!(kept["label"], "Node.js");
        assert_eq!(kept["releases"][0]["eol"], "2028-04-30");
        assert_eq!(kept["packages"], json!(["generic/nodejs"]));
        assert_eq!(kept["listed"], true);
        assert_eq!(kept["problem"], Value::Null);
    }
}
