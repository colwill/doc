//! What a page or a call is about — a service, a team, an organisation or everything — and the
//! services that come to, asked of the Catalogue as whoever is looking. So anybody sees the
//! reliability of what they can see there and nothing else.

use std::collections::BTreeMap;
use std::time::Duration;

use doc_plugin_sdk::Backend;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Refusal, faux};

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
                "reliability is judged for services, teams and organisations, not a {other}"
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

fn refused_by_catalogue(status: u16, body: &Value) -> Refusal {
    let said = body["detail"].as_str().unwrap_or("it refused");
    match status {
        401 | 403 => Refusal::forbidden(format!(
            "which services there are is read from the Catalogue as you, and it says: {said}"
        )),
        404 => Refusal::missing(format!("the Catalogue says: {said}")),
        _ => Refusal::unavailable(format!("the Catalogue answered {status}: {said}")),
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

/// One service, and what it is called.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Member {
    pub name: String,
    pub title: String,
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

/// Every service in the Catalogue the viewer can see, kept while the Catalogue is unchanged. With
/// faux data and none there, the ones `faux-data` makes up.
pub async fn every(backend: &Backend) -> Result<Vec<Member>, Refusal> {
    let version = catalogue_version(backend).await?;
    let faux = faux::on(backend);
    let key = format!("services/{version}{}", if faux { "/faux" } else { "" });
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
    let titles: BTreeMap<String, String> = listed
        .iter()
        .filter_map(|resource| {
            let name = resource["name"].as_str()?.to_string();
            let title =
                resource["title"].as_str().filter(|title| !title.is_empty()).unwrap_or(&name);
            Some((name.clone(), title.to_string()))
        })
        .collect();
    let mut members: Vec<Member> =
        titles.into_iter().map(|(name, title)| Member { name, title }).collect();
    if faux && members.is_empty() {
        members = faux::members(backend).await?;
    }
    if let Err(err) = backend.cache_set(&key, json!(members), Some(KEPT)).await {
        tracing::debug!(%err, "the catalogue's services were not kept");
    }
    Ok(members)
}

/// The services a scope comes to: the one named, or those connected to a team or organisation.
/// A service the viewer cannot see in the Catalogue is refused as missing.
pub async fn services(backend: &Backend, scope: &Scope) -> Result<Vec<Member>, Refusal> {
    let faux = faux::on(backend);
    let Some(reference) = scope.reference() else { return every(backend).await };
    let found = match neighbours(backend, &reference).await {
        Ok(found) => found,
        // One of the services faux data makes up where the Catalogue has none.
        Err(refusal) if faux && refusal.status == 404 => {
            return match scope {
                Scope::Service(name) => faux::members(backend)
                    .await?
                    .into_iter()
                    .find(|member| &member.name == name)
                    .map(|member| vec![member])
                    .ok_or(refusal),
                _ => Err(refusal),
            };
        }
        Err(refusal) => return Err(refusal),
    };
    let mut members: Vec<Member> = match scope {
        Scope::Service(name) => vec![Member { name: name.clone(), title: name.clone() }],
        _ => found
            .into_iter()
            .filter(|(kind, _)| kind == "service")
            .map(|(_, name)| Member { title: name.clone(), name })
            .collect(),
    };
    members.sort();
    members.dedup();
    Ok(members)
}

/// Whether the viewer can see a service in the Catalogue, which changing or reporting on it needs.
pub async fn visible(backend: &Backend, name: &str) -> Result<(), Refusal> {
    services(backend, &Scope::Service(name.to_string())).await.map(|_| ())
}
