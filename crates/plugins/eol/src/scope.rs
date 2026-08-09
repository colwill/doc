//! What a page or a call is about — a service, a team, an organisation or everything — and what
//! each of its services runs, asked as whoever is looking.
//!
//! It is known in two ways, and both are read. The **repositories connected to it** say what they
//! are built on — the packages Repository Insights found in their lockfiles, and, where they are
//! read, their own files — which `repositories` keeps on a schedule; and its metadata in the
//! Catalogue can name products as Backstage's end-of-life plugin reads them —
//! `endoflife.date/products: nodejs@20,postgresql@15`, and its own lifecycle file with
//! `endoflife.date/url-location`. The first needs nobody to keep a list up to date, so it is
//! usually the truer of the two; where they disagree, both are shown, each saying where it came
//! from.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use doc_plugin_sdk::Backend;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::lifecycle;
use crate::{Refusal, faux};

pub const PRODUCTS: &str = "endoflife.date/products";
/// What a page calls the service's own metadata, where something was said there and in a
/// repository both.
pub const OWN: &str = "Its metadata";
pub const URL_LOCATION: &str = "endoflife.date/url-location";
/// The most services one map is drawn from.
const MAX_LISTED: usize = 500;
/// A map of the catalogue is kept for this long, and only while neither the catalogue nor what its
/// repositories were found to use has changed.
const KEPT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    All,
    Service(String),
    Team(String),
    Organisation(String),
}

fn parameter(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn encoded(pairs: &[(&str, &str)]) -> String {
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        out.append_pair(key, value);
    }
    out.finish()
}

impl Scope {
    /// From `service=`, `team=` or `organisation=`, or a panel's `resource=service:name`.
    pub fn from_query(query: &str) -> Result<Self, Refusal> {
        if let Some(resource) = parameter(query, "resource") {
            let (kind, name) = resource
                .split_once(':')
                .ok_or_else(|| Refusal::bad("a resource is written kind:name"))?;
            return Self::named(&kind.to_ascii_lowercase(), name.trim());
        }
        for kind in ["service", "team", "organisation"] {
            if let Some(name) = parameter(query, kind) {
                return Self::named(kind, &name);
            }
        }
        Ok(Self::All)
    }

    pub fn named(kind: &str, name: &str) -> Result<Self, Refusal> {
        if name.is_empty() {
            return Err(Refusal::bad(format!("name the {kind}")));
        }
        match kind {
            "service" => Ok(Self::Service(name.to_string())),
            "team" => Ok(Self::Team(name.to_string())),
            "organisation" | "organization" => Ok(Self::Organisation(name.to_string())),
            other => Err(Refusal::bad(format!(
                "end of life is shown for services, teams and organisations, not a {other}"
            ))),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::All => "everything",
            Self::Service(_) => "service",
            Self::Team(_) => "team",
            Self::Organisation(_) => "organisation",
        }
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Self::All => None,
            Self::Service(name) | Self::Team(name) | Self::Organisation(name) => Some(name),
        }
    }

    /// What it is called on a page.
    pub fn label(&self) -> String {
        match self {
            Self::All => "every service".to_string(),
            Self::Service(name) => name.clone(),
            Self::Team(name) => format!("team {name}"),
            Self::Organisation(name) => format!("organisation {name}"),
        }
    }

    /// Its part of a link's query string, empty for everything.
    pub fn query(&self) -> String {
        match self.name() {
            Some(name) => encoded(&[(self.kind(), name)]),
            None => String::new(),
        }
    }

    /// Its page in the Catalogue, linked as the Catalogue links it.
    pub fn catalogue_href(&self) -> Option<String> {
        Some(format!("/p/resources/r/{}/{}", self.kind(), self.name()?))
    }

    fn reference(&self) -> Option<String> {
        self.name().map(|name| format!("{}:{name}", self.kind()))
    }
}

/// One product a service runs: endoflife.date's name for it, or `url:<address>` for its own
/// file, and the version it runs, where it says.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Used {
    pub product: String,
    #[serde(default)]
    pub version: Option<String>,
    /// Where it was said, in the words a page shows: empty for the service's own metadata, and
    /// otherwise one line per repository file it was read from. A product said in both places
    /// carries both, so nobody has to guess why DOC thinks a service runs something.
    #[serde(default)]
    pub from: Vec<String>,
}

impl Used {
    /// Whether it came from a repository rather than the service's own metadata.
    pub fn discovered(&self) -> bool {
        !self.from.is_empty()
    }
}

/// The products a service's own metadata names and the ones its repositories were found to use,
/// as one list: the same product and version said twice is one entry naming both places, and a
/// product named with no version is dropped once something says which version it is.
fn merged(own: Vec<Used>, found: &[(String, crate::manifests::Found)]) -> Vec<Used> {
    let mut all: BTreeMap<(String, Option<String>), Vec<String>> = BTreeMap::new();
    let mut named: BTreeSet<(String, Option<String>)> = BTreeSet::new();
    for one in own {
        named.insert((one.product.clone(), one.version.clone()));
        all.entry((one.product, one.version)).or_default();
    }
    for (repository, one) in found {
        let key = (one.product.clone(), one.version.clone());
        let where_from = all.entry(key.clone()).or_default();
        // Said in both places: the metadata is named too, so the row does not read as though
        // somebody's own entry in the Catalogue had been ignored.
        if where_from.is_empty() && named.contains(&key) {
            where_from.push(OWN.to_string());
        }
        where_from.push(format!("{repository}: {}", one.file));
    }
    let versioned: BTreeSet<String> = all
        .keys()
        .filter(|(_, version)| version.is_some())
        .map(|(product, _)| product.clone())
        .collect();
    all.into_iter()
        .filter(|((product, version), _)| version.is_some() || !versioned.contains(product))
        .map(|((product, version), mut from)| {
            from.sort();
            from.dedup();
            Used { product, version, from }
        })
        .collect()
}

/// A service, what it is called, and what it runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub title: String,
    pub used: Vec<Used>,
    /// What in its metadata could not be read, said as it is.
    #[serde(default)]
    pub problems: Vec<String>,
    /// The repositories connected to it that were read, as the Catalogue names them.
    #[serde(default)]
    pub repositories: Vec<String>,
}

impl Member {
    /// Whether anything it runs was worked out from a repository rather than named by hand.
    pub fn discovered(&self) -> bool {
        self.used.iter().any(Used::discovered)
    }
}

/// A metadata value as a list: a comma-separated string, or a list of strings.
fn listed(value: &Value) -> Vec<String> {
    let items: Vec<String> = match value {
        Value::String(text) => text.split([',', '\n']).map(str::to_string).collect(),
        Value::Array(items) => items.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => Vec::new(),
    };
    items.into_iter().map(|item| item.trim().to_string()).filter(|item| !item.is_empty()).collect()
}

/// `name@version`, or a product with no version; a URL's version follows its last `@`, where
/// nothing after it is a path.
fn split(written: &str) -> (String, Option<String>) {
    match written.rsplit_once('@') {
        Some((name, version)) if !version.contains('/') && !name.is_empty() => {
            (name.trim().to_string(), Some(version.trim().to_string()).filter(|v| !v.is_empty()))
        }
        _ => (written.trim().to_string(), None),
    }
}

/// What a service's metadata says it runs.
pub fn used(metadata: &Map<String, Value>) -> (Vec<Used>, Vec<String>) {
    let mut used = BTreeSet::new();
    let mut problems = Vec::new();
    if let Some(value) = metadata.get(PRODUCTS) {
        for written in listed(value) {
            let (name, version) = split(&written);
            let name = name.to_ascii_lowercase();
            if name.is_empty()
                || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'))
            {
                problems.push(format!("{written} is not a product endoflife.date could name"));
                continue;
            }
            used.insert(Used { product: name, version, from: Vec::new() });
        }
    }
    if let Some(value) = metadata.get(URL_LOCATION) {
        for written in listed(value) {
            let (address, version) = split(&written);
            match url::Url::parse(&address) {
                Ok(url) if matches!(url.scheme(), "https" | "http") => {
                    used.insert(Used {
                        product: lifecycle::custom(url.as_str()),
                        version,
                        from: Vec::new(),
                    });
                }
                _ => problems.push(format!("{written} is not an http or https address")),
            }
        }
    }
    (used.into_iter().collect(), problems)
}

/// A resource's neighbours in the Catalogue, as `(kind, name)`, asked as whoever is looking.
async fn neighbours(backend: &Backend, reference: &str) -> Result<Vec<(String, String)>, Refusal> {
    let query = encoded(&[("of", reference)]);
    let answer = backend.ask("resources", "GET", "neighbours", Some(&query), None).await;
    let body = match answer {
        Ok((200, body)) => body,
        Ok((status, body)) => return Err(refused_by_catalogue(status, &body)),
        Err(err) => {
            return Err(Refusal::unavailable(format!("the Catalogue could not be asked: {err}")));
        }
    };
    Ok(body["neighbours"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|neighbour| {
            let kind = neighbour["kind"].as_str()?.to_ascii_lowercase();
            Some((kind, neighbour["name"].as_str()?.to_string()))
        })
        .collect())
}

fn refused_by_catalogue(status: u16, body: &Value) -> Refusal {
    let said = body["detail"].as_str().unwrap_or("it refused");
    match status {
        401 | 403 => Refusal::forbidden(format!(
            "what a service runs is read from the Catalogue as you, and it says: {said}"
        )),
        404 => Refusal::missing(format!("the Catalogue says: {said}")),
        _ => Refusal::unavailable(format!("the Catalogue answered {status}: {said}")),
    }
}

/// The Catalogue's version, which changes whenever anything in it does. Asking it as the viewer
/// is also how a page learns they may read the Catalogue at all.
async fn catalogue_version(backend: &Backend) -> Result<i64, Refusal> {
    match backend.ask("resources", "GET", "version", None, None).await {
        Ok((200, body)) => Ok(body["version"].as_i64().unwrap_or_default()),
        Ok((status, body)) => Err(refused_by_catalogue(status, &body)),
        Err(err) => Err(Refusal::unavailable(format!("the Catalogue could not be asked: {err}"))),
    }
}

/// Every service with what it runs, kept while the Catalogue is unchanged. With faux data, what
/// each runs is the estate's, and `faux-data`'s own services stand in for a Catalogue with none.
pub async fn every(backend: &Backend) -> Result<Vec<Member>, Refusal> {
    let version = catalogue_version(backend).await?;
    let faux = faux::on(backend);
    let files = backend.feature(crate::settings::REPOSITORIES);
    let read = crate::repositories::generation(backend).await;
    let key = format!(
        "map/{version}/{read}{}{}",
        if faux { "/faux" } else { "" },
        if files { "/files" } else { "" }
    );
    if let Ok(Some(kept)) = backend.cache_get(&key).await
        && let Ok(members) = serde_json::from_value::<Vec<Member>>(kept)
    {
        return Ok(members);
    }
    let query = encoded(&[("kind", "service"), ("limit", &MAX_LISTED.to_string())]);
    let listed = match backend.ask("resources", "GET", "resources", Some(&query), None).await {
        Ok((200, Value::Array(listed))) => listed,
        Ok((status, body)) => return Err(refused_by_catalogue(status, &body)),
        Err(err) => {
            return Err(Refusal::unavailable(format!("the Catalogue could not be asked: {err}")));
        }
    };
    let mut members: Vec<Member> = listed
        .iter()
        .filter_map(|resource| {
            let name = resource["name"].as_str()?.to_string();
            let title = resource["title"]
                .as_str()
                .filter(|title| !title.is_empty())
                .unwrap_or(&name)
                .to_string();
            let (used, problems) = used(resource["metadata"].as_object().unwrap_or(&Map::new()));
            Some(Member { name, title, used, problems, repositories: Vec::new() })
        })
        .collect();
    if faux {
        members = match members.is_empty() {
            true => faux::members(backend).await?,
            false => {
                let titles =
                    members.into_iter().map(|member| (member.name, member.title)).collect();
                faux::named(backend, &titles).await?
            }
        };
    }
    // What the repositories connected to each service were last found to be built on, folded in
    // beside what the service itself names. It is read from what the schedule kept rather than
    // fetched here, so drawing a page never waits on a repository. Faux data makes up what each
    // service runs, and real repositories are left out of it.
    let found = match faux {
        true => BTreeMap::new(),
        false => crate::repositories::by_service(backend, files).await,
    };
    for member in &mut members {
        let Some(theirs) = found.get(&member.name) else { continue };
        member.repositories = theirs
            .iter()
            .map(|(repository, _)| repository.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        member.used = merged(std::mem::take(&mut member.used), theirs);
    }
    members.sort_by(|one, two| one.name.cmp(&two.name));
    if let Err(err) = backend.cache_set(&key, json!(members), Some(KEPT)).await {
        tracing::debug!(%err, "the catalogue's map was not kept");
    }
    Ok(members)
}

/// The services of a scope, with what each runs.
pub async fn members(backend: &Backend, scope: &Scope) -> Result<Vec<Member>, Refusal> {
    let every = every(backend).await?;
    let Some(reference) = scope.reference() else { return Ok(every) };
    let by_name: BTreeMap<&str, &Member> =
        every.iter().map(|member| (member.name.as_str(), member)).collect();
    let wanted: BTreeSet<String> = match scope {
        Scope::Service(name) => BTreeSet::from([name.clone()]),
        _ => match neighbours(backend, &reference).await {
            Ok(found) => found
                .into_iter()
                .filter(|(kind, _)| kind == "service")
                .map(|(_, name)| name)
                .collect(),
            Err(refusal) if faux::on(backend) && refusal.status == 404 => BTreeSet::new(),
            Err(refusal) => return Err(refusal),
        },
    };
    let mut found = Vec::new();
    let mut unknown = BTreeMap::new();
    for name in &wanted {
        match by_name.get(name.as_str()) {
            Some(member) => found.push((*member).clone()),
            // Faux data is made up for any service asked about.
            None if faux::on(backend) => {
                unknown.insert(name.clone(), name.clone());
            }
            None if matches!(scope, Scope::Service(_)) => {
                return Err(Refusal::missing(format!(
                    "the Catalogue has no service called {name}"
                )));
            }
            None => {}
        }
    }
    if !unknown.is_empty() {
        found.extend(faux::named(backend, &unknown).await?);
    }
    Ok(found)
}
