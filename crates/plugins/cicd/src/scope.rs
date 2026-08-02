//! What a page or a call is about — a service, a team, an organisation or everything — and the
//! repositories that come to, asked of the Catalogue as whoever is looking. So anybody sees the
//! metrics of what they can see there and nothing else, and a connection made in the Catalogue
//! counts at once, with nothing here to bring up to date.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use doc_plugin_sdk::Backend;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Refusal, faux};

/// The most services or teams one map is drawn from, and how many are asked about at once.
const MAX_LISTED: usize = 500;
const AT_ONCE: usize = 8;
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
                "CI/CD/CT metrics are for services, teams and organisations, not a {other}"
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
            "which repositories make up a service is read from the Catalogue as you, and it says: {said}"
        )),
        404 => Refusal::missing(format!("the Catalogue says: {said}")),
        _ => Refusal::unavailable(format!("the Catalogue answered {status}: {said}")),
    }
}

fn of_kind(found: &[(String, String)], kind: &str) -> BTreeSet<String> {
    found
        .iter()
        .filter(|(held, _)| held == kind)
        .map(|(_, name)| name.to_ascii_lowercase())
        .collect()
}

/// A service's repositories: those connected to it in the Catalogue, or with faux data, the
/// stand-ins `faux-data` makes runs up for.
async fn service_repositories(backend: &Backend, name: &str) -> Result<BTreeSet<String>, Refusal> {
    if faux::on(backend) {
        return faux::repositories(backend, &BTreeSet::from([name.to_string()])).await;
    }
    Ok(of_kind(&neighbours(backend, &format!("service:{name}")).await?, "repository"))
}

/// The repositories of each of these services, asked a few at a time.
async fn of_services(
    backend: &Backend,
    services: BTreeSet<String>,
) -> Result<BTreeSet<String>, Refusal> {
    if faux::on(backend) {
        return faux::repositories(backend, &services).await;
    }
    let answers: Vec<Result<BTreeSet<String>, Refusal>> = futures::stream::iter(services)
        .map(|service| async move { service_repositories(backend, &service).await })
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    let mut repositories = BTreeSet::new();
    for answer in answers {
        repositories.extend(answer?);
    }
    Ok(repositories)
}

/// The repositories a scope comes to; `None` for everything, once the Catalogue has said the
/// viewer may read it. With faux data, every scope comes to its services' stand-ins, so
/// everything is every service's.
pub async fn repositories(
    backend: &Backend,
    scope: &Scope,
) -> Result<Option<BTreeSet<String>>, Refusal> {
    let Some(reference) = scope.reference() else {
        catalogue_version(backend).await?;
        if !faux::on(backend) {
            return Ok(None);
        }
        let members = every(backend, "service").await?;
        return Ok(Some(members.into_iter().flat_map(|member| member.repositories).collect()));
    };
    named(backend, scope, &reference).await.map(Some)
}

/// A named service, team or organisation's repositories, and its services' for the last two.
async fn named(
    backend: &Backend,
    scope: &Scope,
    reference: &str,
) -> Result<BTreeSet<String>, Refusal> {
    let faux = faux::on(backend);
    let found = match neighbours(backend, reference).await {
        Ok(found) => found,
        // One of the services faux data makes up where the Catalogue has none.
        Err(refusal) if faux && refusal.status == 404 => match scope {
            Scope::Service(name)
                if faux::members(backend).await?.iter().any(|m| &m.name == name) =>
            {
                return faux::repositories(backend, &BTreeSet::from([name.clone()])).await;
            }
            _ => return Err(refusal),
        },
        Err(refusal) => return Err(refusal),
    };
    if faux {
        let services = match scope {
            Scope::Service(name) => BTreeSet::from([name.clone()]),
            _ => of_kind(&found, "service"),
        };
        return faux::repositories(backend, &services).await;
    }
    let mut repositories = of_kind(&found, "repository");
    if matches!(scope, Scope::Team(_) | Scope::Organisation(_)) {
        repositories.extend(of_services(backend, of_kind(&found, "service")).await?);
    }
    Ok(repositories)
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

/// One service or team, what it is called, and the repositories it comes to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub title: String,
    pub repositories: BTreeSet<String>,
}

/// Every service, or every team, with its repositories: what the overview's table and the Teams
/// tab are drawn from. Kept while the Catalogue is unchanged, since asking about every service
/// one by one is the slow part of the page, and anybody who may read the Catalogue sees all of it.
pub async fn every(backend: &Backend, kind: &str) -> Result<Vec<Member>, Refusal> {
    let version = catalogue_version(backend).await?;
    let faux = faux::on(backend);
    let key = format!("map/{kind}/{version}{}", if faux { "/faux" } else { "" });
    if let Ok(Some(kept)) = backend.cache_get(&key).await
        && let Ok(members) = serde_json::from_value::<Vec<Member>>(kept)
    {
        return Ok(members);
    }
    let query = encoded(&[("kind", kind), ("limit", &MAX_LISTED.to_string())]);
    let listed = match backend.ask("resources", "GET", "resources", Some(&query), None).await {
        Ok((200, Value::Array(listed))) => listed,
        Ok((status, body)) => return Err(refused_by_catalogue(status, &body)),
        Err(err) => {
            return Err(Refusal::unavailable(format!("the Catalogue could not be asked: {err}")));
        }
    };
    let titles: BTreeMap<String, String> = listed
        .iter()
        .filter_map(|resource| {
            let name = resource["name"].as_str()?.to_string();
            let title =
                resource["title"].as_str().filter(|title| !title.is_empty()).unwrap_or(&name);
            Some((name.clone(), title.to_string()))
        })
        .collect();
    let scoped = |name: &str| match kind {
        "team" => Scope::Team(name.to_string()),
        _ => Scope::Service(name.to_string()),
    };
    let answers: Vec<Result<Member, Refusal>> = futures::stream::iter(titles)
        .map(|(name, title)| {
            let scope = scoped(&name);
            async move {
                let reference = scope.reference().unwrap_or_default();
                let repositories = named(backend, &scope, &reference).await?;
                Ok(Member { name, title, repositories })
            }
        })
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    let mut members = answers.into_iter().collect::<Result<Vec<_>, _>>()?;
    if faux && members.is_empty() && kind == "service" {
        members = faux::members(backend).await?;
    }
    members.sort_by(|one, two| one.name.cmp(&two.name));
    if let Err(err) = backend.cache_set(&key, json!(members), Some(KEPT)).await {
        tracing::debug!(%err, "the catalogue's map was not kept");
    }
    Ok(members)
}
