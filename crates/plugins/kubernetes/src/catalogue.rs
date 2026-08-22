//! Which repository each service is, from the Catalogue, for the rollouts DORA and CI/CD/CT count
//! by repository. The Catalogue answers only as a person, so somebody who can read it lets the
//! plugin do so as them (a delegation, DOC-SPEC §9.5) on the Kubernetes page. A run started as them
//! then reads every service and its repositories into `services`, again every few hours, and
//! only while they can still read the Catalogue themselves.
//!
//! A workload that names its repository in a `rundoc.sh/repository` annotation needs none of this.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration as Wait;

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, DataRequest, PluginError, Query};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ID;
use crate::store::{Mapped, Rollout};

const STATE: &str = "catalogue";
/// How often the map is read again.
const EVERY_HOURS: i64 = 6;
/// A read asked for and not yet done is not asked for again until this long has passed.
const ASKED_MINUTES: i64 = 10;
const MAX_SERVICES: usize = 500;
const AT_ONCE: usize = 8;
const BATCH: usize = 100;

/// Whose leave the plugin reads the Catalogue with, and how the last read went.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Reading {
    pub delegation: Option<Uuid>,
    pub by: Option<String>,
    pub granted_at: Option<DateTime<Utc>>,
    pub asked_at: Option<DateTime<Utc>>,
    pub read_at: Option<DateTime<Utc>>,
    pub services: usize,
    pub problem: Option<String>,
}

pub async fn reading(backend: &Backend) -> Reading {
    match backend.state_get(STATE).await {
        Ok(Some(value)) => serde_json::from_value(value).unwrap_or_default(),
        _ => Reading::default(),
    }
}

async fn keep(backend: &Backend, reading: &Reading) -> Result<(), PluginError> {
    backend.state_set(STATE, json!(reading)).await
}

/// Lets the plugin read the Catalogue as whoever is asking, instead of whoever did before, and
/// has it read at once.
pub async fn grant(backend: &Backend, who: &str) -> Result<Reading, PluginError> {
    let delegation = backend
        .delegate("kubernetes: reading which repositories each service has, for DORA and CI/CD/CT")
        .await?;
    let before = reading(backend).await;
    if let Some(old) = before.delegation
        && let Err(err) = backend.revoke(old).await
    {
        tracing::warn!(%err, "the earlier leave to read the Catalogue could not be revoked");
    }
    let now = Utc::now();
    let reading = Reading {
        delegation: Some(delegation),
        by: Some(who.to_string()),
        granted_at: Some(now),
        asked_at: Some(now),
        ..before
    };
    keep(backend, &reading).await?;
    backend.task_as(delegation, json!({ "catalogue": true }), Some(1)).await?;
    Ok(reading)
}

/// Stops reading the Catalogue; the map read last is kept.
pub async fn stop(backend: &Backend) -> Result<(), PluginError> {
    let mut reading = reading(backend).await;
    if let Some(delegation) = reading.delegation.take() {
        backend.revoke(delegation).await?;
    }
    reading.by = None;
    reading.granted_at = None;
    keep(backend, &reading).await
}

/// Asks for a read as whoever gave leave, when the last one is old enough.
pub async fn when_due(backend: &Backend) {
    let mut reading = reading(backend).await;
    let Some(delegation) = reading.delegation else { return };
    let now = Utc::now();
    let stale = reading.read_at.is_none_or(|at| now - at > Duration::hours(EVERY_HOURS));
    let asked = reading.asked_at.is_some_and(|at| now - at < Duration::minutes(ASKED_MINUTES));
    if !stale || asked {
        return;
    }
    reading.asked_at = Some(now);
    if let Err(err) = keep(backend, &reading).await {
        tracing::warn!(%err, "when the Catalogue was asked for could not be kept");
        return;
    }
    if let Err(err) = backend.task_as(delegation, json!({ "catalogue": true }), Some(1)).await {
        tracing::warn!(%err, "the Catalogue could not be asked for");
        reading.problem = Some(format!(
            "the Catalogue could not be read as {}: {}",
            reading.by.as_deref().unwrap_or("whoever gave leave"),
            err.detail()
        ));
        let _ = keep(backend, &reading).await;
    }
}

/// Each service's repositories, as last read.
pub async fn mapping(backend: &Backend) -> BTreeMap<String, Vec<String>> {
    match backend.query_all::<Mapped>(Query::new("services")).await {
        Ok(mapped) => mapped.into_iter().map(|mapped| (mapped.id, mapped.repositories)).collect(),
        Err(err) => {
            tracing::warn!(%err, "the map of services to repositories could not be read");
            BTreeMap::new()
        }
    }
}

/// The run started as whoever gave leave: every service and its repositories, as they see them.
pub async fn read(backend: &Backend) -> Result<Value, PluginError> {
    let mut reading = reading(backend).await;
    let found = services(backend).await;
    let found = match found {
        Ok(found) => found,
        Err(problem) => {
            reading.problem = Some(problem.clone());
            keep(backend, &reading).await?;
            return Err(PluginError::Message(problem));
        }
    };
    let held = mapping(backend).await;
    let mut writes: Vec<DataRequest> = found
        .iter()
        .filter(|(service, repositories)| held.get(*service) != Some(repositories))
        .map(|(service, repositories)| {
            DataRequest::upsert(
                "services",
                &["id"],
                json!({ "id": service, "repositories": repositories }),
            )
        })
        .collect();
    writes.extend(
        held.keys()
            .filter(|service| !found.contains_key(*service))
            .map(|service| DataRequest::delete("services", service.as_str())),
    );
    for chunk in writes.chunks(BATCH) {
        backend.batch(chunk.to_vec()).await?;
    }
    let placed = place(backend, &found).await?;
    reading.read_at = Some(Utc::now());
    reading.services = found.len();
    reading.problem = None;
    keep(backend, &reading).await?;
    Ok(json!({ "services": found.len(), "rollouts_placed": placed }))
}

async fn services(backend: &Backend) -> Result<BTreeMap<String, Vec<String>>, String> {
    let query = format!("kind=service&limit={MAX_SERVICES}");
    let listed = match backend.ask("resources", "GET", "resources", Some(&query), None).await {
        Ok((200, Value::Array(listed))) => listed,
        Ok((status, body)) => {
            let said = body["detail"].as_str().unwrap_or("it refused").to_string();
            return Err(format!("the Catalogue answered {status}: {said}"));
        }
        Err(err) => return Err(format!("the Catalogue could not be asked: {}", err.detail())),
    };
    let names: Vec<String> = listed
        .iter()
        .filter_map(|resource| resource["name"].as_str().map(str::to_string))
        .collect();
    let answers: Vec<(String, Result<Vec<String>, String>)> = futures::stream::iter(names)
        .map(|name| async move {
            let repositories = repositories(backend, &name).await;
            (name, repositories)
        })
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    let mut found = BTreeMap::new();
    for (name, repositories) in answers {
        found.insert(name, repositories?);
    }
    Ok(found)
}

async fn repositories(backend: &Backend, service: &str) -> Result<Vec<String>, String> {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("of", &format!("service:{service}"))
        .finish();
    let asked = backend.ask("resources", "GET", "neighbours", Some(&query), None);
    let body = match tokio::time::timeout(Wait::from_secs(10), asked).await {
        Ok(Ok((200, body))) => body,
        Ok(Ok((status, body))) => {
            let said = body["detail"].as_str().unwrap_or("it refused").to_string();
            return Err(format!("the Catalogue answered {status} about {service}: {said}"));
        }
        Ok(Err(err)) => return Err(format!("the Catalogue could not be asked: {}", err.detail())),
        Err(_) => return Err(format!("the Catalogue took too long about {service}")),
    };
    let repositories: BTreeSet<String> = body["neighbours"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|neighbour| {
            neighbour["kind"].as_str().is_some_and(|kind| kind.eq_ignore_ascii_case("repository"))
        })
        .filter_map(|neighbour| neighbour["name"].as_str().map(str::to_ascii_lowercase))
        .collect();
    Ok(repositories.into_iter().collect())
}

/// Gives rollouts already kept for a service with one repository that repository, where they had
/// none, and has DORA work those repositories out again.
async fn place(
    backend: &Backend,
    found: &BTreeMap<String, Vec<String>>,
) -> Result<usize, PluginError> {
    let mut placed = 0;
    for (service, repositories) in found {
        let [repository] = repositories.as_slice() else { continue };
        let rollouts: Vec<Rollout> =
            backend.query_all(Query::new("rollouts").filter(json!({ "service": service }))).await?;
        let unplaced: Vec<&Rollout> =
            rollouts.iter().filter(|rollout| rollout.repository.is_none()).collect();
        if unplaced.is_empty() {
            continue;
        }
        let writes: Vec<DataRequest> = unplaced
            .iter()
            .map(|rollout| {
                DataRequest::update(
                    "rollouts",
                    rollout.id.as_str(),
                    json!({ "repository": repository }),
                )
            })
            .collect();
        for chunk in writes.chunks(BATCH) {
            backend.batch(chunk.to_vec()).await?;
        }
        placed += unplaced.len();
        // DORA counts only production rollouts; the rest wait for CI/CD/CT's next read.
        let since = unplaced
            .iter()
            .filter(|rollout| rollout.production)
            .map(|rollout| rollout.started_at)
            .min();
        let Some(since) = since else { continue };
        let payload = json!({ "repository": repository, "since": since });
        if let Err(err) = backend.publish(&format!("plugin.{ID}.delivery.synced"), payload).await {
            tracing::warn!(%err, repository, "DORA was not told of rollouts given a repository");
        }
    }
    Ok(placed)
}
