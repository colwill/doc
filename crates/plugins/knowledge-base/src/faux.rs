//! Faux data (FIX-FAUX-DATA): whether `faux-data` is providing the Knowledge Base's data — its
//! toggle on faux-data's Settings page, read from the `serving` record it exports here — and the
//! faux docs it provides: a space for each service's stand-in repository, with its pages, sources
//! and files, and a handbook linked to every service.
//!
//! The pages are rendered here with the code that renders real ones, and what people see is read
//! from them instead of the store: the store's reads ask `corpus` first. Only people and service
//! accounts are shown it; the plugin's own runs — syncs, imports, the repository sync — always see
//! the real Knowledge Base, which faux data never changes. While it is on, nothing can be changed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use doc_plugin_sdk::{Backend, Query, Response};
use serde_json::{Map, Value, json};

use crate::store::{Space, snippet};
use crate::{Refusal, imports, markdown};

pub const PROVIDER: &str = "faux-data";
const ID: &str = "kb";
/// A toggle is read again after this long, so turning one on or off shows within seconds.
const KEPT: Duration = Duration::from_secs(5);
/// The faux docs are made up again after this long, or when the services asked about change.
const CORPUS_KEPT: Duration = Duration::from_secs(120);
const ASKING: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Live,
    Faux,
    /// Its toggle is on, and the provider is not running.
    Stopped,
}

static MODE: Mutex<Option<(Instant, Mode)>> = Mutex::new(None);
static CORPUS: Mutex<Option<(Instant, String, Arc<Corpus>)>> = Mutex::new(None);

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

/// Reads whether faux data is on for this plugin, unless it was read in the last few seconds.
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

/// Whether what `backend` is asked for is faux: faux data is on, and a person or a service account
/// is asking rather than the plugin's own runs.
pub fn shown(backend: &Backend) -> bool {
    current() != Mode::Live
        && backend.caller().is_some_and(|caller| matches!(caller.kind.as_str(), "user" | "service"))
}

/// An answer marked with whose faux data it shows, for the frontend to say so at the top.
pub fn marked(backend: &Backend, response: Response) -> Response {
    match shown(backend) {
        true => response.faux(PROVIDER),
        false => response,
    }
}

/// The faux docs, when they are what this caller sees.
pub async fn corpus(backend: &Backend) -> Result<Option<Arc<Corpus>>, Refusal> {
    if !shown(backend) {
        return Ok(None);
    }
    if current() == Mode::Stopped {
        return Err(Refusal::unavailable(format!(
            "{PROVIDER}, which provides the Knowledge Base's faux data, is not running"
        )));
    }
    let services = catalogued(backend).await;
    let key = services.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>().join(",");
    if let Ok(held) = CORPUS.lock()
        && let Some((at, asked, corpus)) = held.as_ref()
        && *asked == key
        && at.elapsed() < CORPUS_KEPT
    {
        return Ok(Some(corpus.clone()));
    }
    let query = {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        for (name, title) in &services {
            query.append_pair("service", &format!("{name}={title}"));
        }
        query.finish()
    };
    let asked = backend.discovery(PROVIDER, "GET", "faux/docs", Some(&query), None);
    let answer = match tokio::time::timeout(ASKING, asked).await {
        Ok(Ok((200, body))) => body,
        Ok(Ok((status, body))) => {
            let said = body["detail"].as_str().unwrap_or("no reason given");
            return Err(Refusal::unavailable(format!("{PROVIDER} answered {status}: {said}")));
        }
        Ok(Err(err)) => {
            return Err(Refusal::unavailable(format!(
                "{PROVIDER} could not be asked: {}",
                err.detail()
            )));
        }
        Err(_) => return Err(Refusal::unavailable(format!("{PROVIDER} took too long"))),
    };
    let corpus = Arc::new(Corpus::of(&answer));
    if let Ok(mut held) = CORPUS.lock() {
        *held = Some((Instant::now(), key, corpus.clone()));
    }
    Ok(Some(corpus))
}

/// The Catalogue's services as the viewer sees them, by name and title; none where it has none,
/// and faux-data makes its own up.
async fn catalogued(backend: &Backend) -> Vec<(String, String)> {
    let asked = backend.ask("resources", "GET", "resources", Some("kind=service&limit=500"), None);
    let Ok((200, Value::Array(found))) = asked.await else { return Vec::new() };
    let mut services: Vec<(String, String)> = found
        .iter()
        .filter_map(|service| {
            let name = service["name"].as_str()?.to_string();
            let title =
                service["title"].as_str().filter(|title| !title.is_empty()).unwrap_or(&name);
            Some((name.clone(), title.to_string()))
        })
        .collect();
    services.sort();
    services
}

/// The faux Knowledge Base, rendered, and answering what the store would.
pub struct Corpus {
    pub services: Vec<(String, String)>,
    spaces: Vec<Space>,
    /// Every page as the store keeps it.
    documents: Vec<Map<String, Value>>,
    sources: Vec<Value>,
    /// Each source's kind, by its ID.
    kinds: BTreeMap<String, String>,
    files: BTreeMap<(String, String, String), (String, Vec<u8>)>,
    links: BTreeMap<String, Vec<String>>,
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

impl Corpus {
    fn of(answer: &Value) -> Self {
        let services = answer["services"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|service| (text(service, "name"), text(service, "title")))
            .collect();
        let (mut spaces, mut documents, mut sources, mut files) =
            (Vec::new(), Vec::new(), Vec::new(), BTreeMap::new());
        let now = chrono::Utc::now().to_rfc3339();
        for held in answer["spaces"].as_array().into_iter().flatten() {
            let pages: Vec<&Value> = held["pages"].as_array().into_iter().flatten().collect();
            let space = Space {
                key: text(held, "key"),
                name: text(held, "name"),
                resource: held["resource"].as_str().map(str::to_string),
                // Made-up spaces are never archived: there is nothing to restore them from.
                archived_at: None,
                archived_by: None,
                owners: held["owners"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                documents: pages.len() as i64,
            };
            let attached: Vec<(String, String)> = held["files"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|file| {
                    let bytes = base64::engine::general_purpose::STANDARD
                        .decode(text(file, "base64"))
                        .unwrap_or_default();
                    let (page, name) = (text(file, "page"), text(file, "name"));
                    files.insert(
                        (space.key.clone(), page.clone(), name.clone()),
                        (text(file, "content_type"), bytes),
                    );
                    (page, name)
                })
                .collect();
            for (position, page) in pages.iter().enumerate() {
                let path = text(page, "path");
                let (front, body) = markdown::front_matter(page["markdown"].as_str().unwrap_or(""));
                let rendered = markdown::render(body, &|url| {
                    let target = markdown::relative(&path, url)?;
                    let file = target.rsplit('/').next().unwrap_or(&target);
                    match attached.iter().any(|(on, name)| *on == path && name == file) {
                        true => Some(format!(
                            "/p/kb/attachments/{}/{}/{}",
                            space.key,
                            crate::storage::percent(&path),
                            crate::storage::percent(file)
                        )),
                        false => markdown::page_link(&space.key, &path, url),
                    }
                });
                let mut document = Map::new();
                document.insert("id".into(), json!(format!("{}/{path}", space.key)));
                document.insert("space".into(), json!(space.key));
                document.insert("path".into(), json!(path));
                document.insert("title".into(), page["title"].clone());
                document.insert("position".into(), json!(position));
                document.insert("html".into(), json!(rendered.html));
                document.insert("plain".into(), json!(rendered.text));
                document.insert("resources".into(), json!(imports::resources_of(&front, &space)));
                document.insert("front_matter".into(), Value::Object(front));
                document.insert("tags".into(), json!([]));
                document.insert("source".into(), page["source"].clone());
                document.insert("source_url".into(), page["source_url"].clone());
                document.insert("updated_at".into(), json!(now));
                documents.push(document);
            }
            sources.extend(held["sources"].as_array().into_iter().flatten().cloned());
            spaces.push(space);
        }
        spaces.sort_by(|a, b| a.name.cmp(&b.name));
        let kinds =
            sources.iter().map(|source| (text(source, "id"), text(source, "kind"))).collect();
        let links = answer["links"]
            .as_object()
            .into_iter()
            .flatten()
            .map(|(service, spaces)| {
                let spaces = spaces
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect();
                (service.clone(), spaces)
            })
            .collect();
        Self { services, spaces, documents, sources, kinds, files, links }
    }

    fn kind(&self, document: &Map<String, Value>) -> String {
        let source = document.get("source").and_then(Value::as_str).unwrap_or_default();
        self.kinds.get(source).cloned().unwrap_or_else(|| "upload".into())
    }

    fn get(document: &Map<String, Value>, key: &str) -> Value {
        document.get(key).cloned().unwrap_or(Value::Null)
    }

    pub fn spaces(&self) -> Vec<Space> {
        self.spaces.clone()
    }

    pub fn space(&self, key: &str) -> Option<Space> {
        self.spaces.iter().find(|space| space.key == key).cloned()
    }

    fn of_space(&self, space: &str) -> impl Iterator<Item = &Map<String, Value>> {
        self.documents.iter().filter(move |document| document["space"] == json!(space))
    }

    pub fn pages_of(&self, space: &str) -> Vec<Map<String, Value>> {
        self.of_space(space)
            .map(|document| {
                ["id", "path", "title", "resources", "source"]
                    .iter()
                    .map(|key| (key.to_string(), Self::get(document, key)))
                    .collect()
            })
            .collect()
    }

    pub fn every_page(&self) -> Vec<Value> {
        self.documents
            .iter()
            .map(|document| {
                json!({
                    "space": document["space"], "path": document["path"],
                    "title": document["title"], "front_matter": document["front_matter"],
                    "tags": document["tags"], "resources": document["resources"],
                })
            })
            .collect()
    }

    pub fn tree(&self, space: &str) -> Vec<Value> {
        self.of_space(space)
            .map(|document| {
                json!({
                    "path": document["path"], "title": document["title"],
                    "position": document["position"], "updated_at": document["updated_at"],
                })
            })
            .collect()
    }

    pub fn document(&self, space: &str, path: &str) -> Option<Value> {
        let document = self.of_space(space).find(|document| document["path"] == json!(path))?;
        let mut shown = json!({});
        for key in [
            "space",
            "path",
            "title",
            "html",
            "front_matter",
            "resources",
            "tags",
            "source_url",
            "updated_at",
        ] {
            shown[key] = Self::get(document, key);
        }
        shown["source"] = json!(self.kind(document));
        Some(shown)
    }

    pub fn documenting_any(&self, resources: &[String], spaces: &[String]) -> Vec<Value> {
        let names = |document: &Map<String, Value>| {
            document["resources"].as_array().is_some_and(|listed| {
                listed.iter().any(|resource| {
                    resource
                        .as_str()
                        .is_some_and(|resource| resources.iter().any(|r| r == resource))
                })
            })
        };
        let mut found: Vec<&Map<String, Value>> = self
            .documents
            .iter()
            .filter(|document| {
                names(document) || spaces.iter().any(|space| document["space"] == json!(space))
            })
            .collect();
        found.sort_by_key(|document| {
            (
                document["space"].as_str().unwrap_or_default().to_string(),
                document["position"].as_i64().unwrap_or_default(),
            )
        });
        found
            .into_iter()
            .map(|document| {
                json!({
                    "space": document["space"], "path": document["path"],
                    "title": document["title"], "source": self.kind(document),
                })
            })
            .collect()
    }

    pub fn documented(
        &self,
        links: &BTreeMap<String, Vec<String>>,
    ) -> BTreeMap<String, BTreeMap<String, i64>> {
        let mut linked: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (service, spaces) in links {
            for space in spaces {
                linked.entry(space.as_str()).or_default().push(format!("service:{service}"));
            }
        }
        let mut counted: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
        for document in &self.documents {
            let mut named: BTreeSet<String> = document["resources"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            let space = document["space"].as_str().unwrap_or_default();
            named.extend(linked.get(space).into_iter().flatten().cloned());
            let kind = self.kind(document);
            for resource in named {
                *counted.entry(resource).or_default().entry(kind.clone()).or_default() += 1;
            }
        }
        counted
    }

    /// Pages holding every word asked for, the more times the better.
    pub fn search(&self, asked: &str, space: Option<&str>, limit: i64) -> Vec<Value> {
        let words: Vec<String> = asked
            .split_whitespace()
            .map(str::to_lowercase)
            .filter(|word| !word.is_empty())
            .collect();
        let mut hits: Vec<(usize, &Map<String, Value>)> = self
            .documents
            .iter()
            .filter(|document| space.is_none_or(|space| document["space"] == json!(space)))
            .filter_map(|document| {
                let haystack = format!(
                    "{} {}",
                    document["title"].as_str().unwrap_or_default(),
                    document["plain"].as_str().unwrap_or_default()
                )
                .to_lowercase();
                let score: Option<Vec<usize>> = words
                    .iter()
                    .map(|word| Some(haystack.matches(word.as_str()).count()).filter(|n| *n > 0))
                    .collect();
                score.map(|counts| (counts.iter().sum(), document))
            })
            .collect();
        hits.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
        hits.truncate(limit.clamp(1, 1_000) as usize);
        let count = hits.len();
        hits.into_iter()
            .enumerate()
            .map(|(at, (_, document))| {
                json!({
                    "space": document["space"], "path": document["path"], "title": document["title"],
                    "rank": (count - at) as f64 / count as f64,
                    "snippet": snippet(document["plain"].as_str().unwrap_or_default(), asked),
                })
            })
            .collect()
    }

    pub fn sources(&self) -> Vec<Value> {
        self.sources.clone()
    }

    pub fn attachment(&self, space: &str, page: &str, name: &str) -> Option<(String, Vec<u8>)> {
        self.files.get(&(space.to_string(), page.to_string(), name.to_string())).cloned()
    }

    pub fn links(&self) -> BTreeMap<String, Vec<String>> {
        self.links.clone()
    }

    /// A made-up service as the Catalogue would list it.
    pub fn service(&self, name: &str) -> Option<Value> {
        self.services.iter().find(|(held, _)| held == name).map(|(name, title)| {
            json!({ "name": name, "title": title, "description": "", "owner": null })
        })
    }

    pub fn listed(&self) -> Vec<Value> {
        self.services.iter().filter_map(|(name, _)| self.service(name)).collect()
    }
}
