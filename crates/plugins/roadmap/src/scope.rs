//! What a page or a call is about — a service, a team, an organisation or everything — and which
//! Jira projects each of its services' releases are in: those the Settings page gives it under
//! **Projects tracked**, and those its metadata in the Catalogue names, asked as whoever is looking.
//! A service names them as Backstage's Jira plugin reads them: `jira/project-key: PAY`, narrowed
//! where a project holds several services by `jira/component` or `jira/label`.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use doc_plugin_sdk::Backend;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::settings::Definitions;
use crate::{Refusal, faux};

pub const PROJECT_KEY: &str = "jira/project-key";
pub const COMPONENT: &str = "jira/component";
pub const LABEL: &str = "jira/label";
/// The most services one map is drawn from.
const MAX_LISTED: usize = 500;
/// A map of the catalogue is kept for this long, and only while the catalogue is unchanged.
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
                "the roadmap is shown for services, teams and organisations, not a {other}"
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
            "which releases a service is in is read from the Catalogue as you, and it says: {said}"
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

/// A service, what it is called, and where its releases are planned.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub title: String,
    /// Jira project keys, upper case.
    pub projects: BTreeSet<String>,
    /// Components and labels that narrow a project to this service's issues; with neither, every
    /// release of its projects is its.
    #[serde(default)]
    pub components: BTreeSet<String>,
    #[serde(default)]
    pub labels: BTreeSet<String>,
    /// Projects the Settings page gives this service, upper case: every release in them is its,
    /// whatever its components and labels narrow its own projects to.
    #[serde(default)]
    pub tracked: BTreeSet<String>,
}

impl Member {
    /// Whether a release of `project` with issues in these components and labels is this
    /// service's.
    pub fn owns(
        &self,
        project: &str,
        components: &BTreeSet<String>,
        labels: &BTreeSet<String>,
    ) -> bool {
        if self.tracked.contains(project) {
            return true;
        }
        if !self.projects.contains(project) {
            return false;
        }
        if self.components.is_empty() && self.labels.is_empty() {
            return true;
        }
        self.components.iter().any(|component| components.contains(component))
            || self.labels.iter().any(|label| labels.contains(label))
    }
}

/// A metadata value as a set: a comma-separated string, or a list of strings.
fn listed(value: Option<&Value>, upper: bool) -> BTreeSet<String> {
    let items: Vec<String> = match value {
        Some(Value::String(text)) => text.split(',').map(str::to_string).collect(),
        Some(Value::Array(items)) => {
            items.iter().filter_map(Value::as_str).map(str::to_string).collect()
        }
        _ => Vec::new(),
    };
    items
        .into_iter()
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .map(|item| if upper { item.to_ascii_uppercase() } else { item })
        .collect()
}

fn member(name: String, title: String, metadata: &Map<String, Value>) -> Member {
    Member {
        name,
        title,
        projects: listed(metadata.get(PROJECT_KEY), true),
        components: listed(metadata.get(COMPONENT), false),
        labels: listed(metadata.get(LABEL), false),
        tracked: BTreeSet::new(),
    }
}

/// Every service in the Catalogue with what its metadata says, as whoever is looking.
pub async fn catalogued(backend: &Backend) -> Result<Vec<Member>, Refusal> {
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
            Some(member(name, title, resource["metadata"].as_object().unwrap_or(&Map::new())))
        })
        .collect();
    members.sort_by(|one, two| one.name.cmp(&two.name));
    Ok(members)
}

/// Every service with where its releases are planned: the Catalogue's map, and the projects the
/// Settings page gives each service. Faux data's services are its own, so nothing is given them.
pub async fn every(backend: &Backend) -> Result<Vec<Member>, Refusal> {
    let mut members = mapped(backend).await?;
    if !faux::on(backend) {
        let definitions = Definitions::read(&backend.settings());
        for (project, services) in &definitions.tracked {
            for member in members.iter_mut().filter(|member| services.contains(&member.name)) {
                member.tracked.insert(project.clone());
            }
        }
    }
    Ok(members)
}

/// Every service with where its metadata plans its releases, kept while the Catalogue is
/// unchanged. With faux data, each service's releases are in the project the estate gives it, and
/// `faux-data`'s own services stand in for a Catalogue with none.
async fn mapped(backend: &Backend) -> Result<Vec<Member>, Refusal> {
    let version = catalogue_version(backend).await?;
    let faux = faux::on(backend);
    let key = format!("map/{version}{}", if faux { "/faux" } else { "" });
    if let Ok(Some(kept)) = backend.cache_get(&key).await
        && let Ok(members) = serde_json::from_value::<Vec<Member>>(kept)
    {
        return Ok(members);
    }
    let mut members = catalogued(backend).await?;
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
    members.sort_by(|one, two| one.name.cmp(&two.name));
    if let Err(err) = backend.cache_set(&key, json!(members), Some(KEPT)).await {
        tracing::debug!(%err, "the catalogue's map was not kept");
    }
    Ok(members)
}

/// The services of a scope, and every service, since a release a scope's service is in may hold
/// others' too.
pub async fn members(
    backend: &Backend,
    scope: &Scope,
) -> Result<(Vec<Member>, Vec<Member>), Refusal> {
    let every = every(backend).await?;
    let Some(reference) = scope.reference() else { return Ok((every.clone(), every)) };
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
    let by_name: BTreeMap<&str, &Member> =
        every.iter().map(|member| (member.name.as_str(), member)).collect();
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
    Ok((found, every))
}
