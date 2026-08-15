//! Release data: each project's versions — Jira's releases — and the issues in them, exported to
//! the delivery roadmap. A read takes each project's versions in full, every issue of a version it
//! has not read before, and then only the issues changed since the read before, so an hour's read
//! of a quiet project is three small calls. A read cut short by its time carries on in a task of
//! its own from the project it reached.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

use chrono::{DateTime, Duration, NaiveDate, Utc};
use doc_plugin_sdk::{
    Backend, Collection, DataRequest, Declaration, Export, Field, ListOf, PluginError, Query,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::jira::{Cursor, Jira};
use crate::settings::Config;

/// How long one task reads for before it hands on to the next.
const BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
/// Versions whose issues are asked for in one search.
const IN_ONE: usize = 40;
const BATCH: usize = 100;
/// Issues changed this long before the last read are asked for again, for clocks that disagree.
const OVERLAP_MINUTES: i64 = 15;
const SYNCED: &str = "releases/synced";
const BEGUN: &str = "releases/begun";

pub fn declaration() -> Declaration {
    let exported = || Export::to(&["roadmap"]);
    Declaration::default()
        .collection(
            "projects",
            Collection::new()
                .field("key", Field::text().key().describe("Jira's key for it, such as PAY"))
                .field("id", Field::text().required())
                .field("name", Field::text().required())
                .field("url", Field::text())
                .export(exported()),
        )
        .collection(
            "versions",
            Collection::new()
                .field("id", Field::text().key().describe("Jira's ID for the version"))
                .field("project", Field::text().required())
                .field("name", Field::text().required())
                .field("description", Field::text().max(2_000.0))
                .field("released", Field::boolean().required().default(json!(false)))
                .field("archived", Field::boolean().required().default(json!(false)))
                .field("overdue", Field::boolean().required().default(json!(false)))
                .field("start_date", Field::date())
                .field("release_date", Field::date())
                .field("url", Field::text())
                .index(&["project"])
                .index(&["release_date"])
                .export(exported()),
        )
        .collection(
            "issues",
            Collection::new()
                .field("id", Field::text().key().describe("Jira's ID for the issue"))
                .field("key", Field::text().required().describe("Such as PAY-123"))
                .field("project", Field::text().required())
                .field("summary", Field::text().max(1_000.0))
                .field("type", Field::text())
                .field("status", Field::text())
                .field(
                    "category",
                    Field::text()
                        .required()
                        .one_of(&["new", "indeterminate", "done", "undefined"])
                        .describe("Its status's category: to do, in progress or done"),
                )
                .field("priority", Field::text())
                .field("versions", Field::list(ListOf::Text).describe("Its fix versions' IDs"))
                .field("components", Field::list(ListOf::Text))
                .field("labels", Field::list(ListOf::Text))
                .field("created", Field::timestamp())
                .field("updated", Field::timestamp())
                .field("resolved_at", Field::timestamp())
                .field("url", Field::text())
                .index(&["project"])
                .export(exported()),
        )
}

/// What a project's last read left: the versions whose issues have all been read, and when it
/// started, which the next read asks for changes since.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Kept {
    #[serde(default)]
    versions: BTreeSet<String>,
    #[serde(default)]
    at: Option<DateTime<Utc>>,
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) if !text.is_empty() => Some(text.clone()),
        Value::Number(number) => Some(number.to_string()),
        _ => None,
    }
}

fn date(value: &Value) -> Option<NaiveDate> {
    value.as_str().and_then(|text| NaiveDate::parse_from_str(text.get(..10)?, "%Y-%m-%d").ok())
}

/// Jira's times, `2026-09-01T10:00:00.000+0000`, which are not quite RFC 3339.
fn moment(value: &Value) -> Option<DateTime<Utc>> {
    let text = value.as_str()?;
    DateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S%.f%z")
        .or_else(|_| DateTime::parse_from_rfc3339(text))
        .ok()
        .map(|at| at.with_timezone(&Utc))
}

/// The versions worth reading: every unreleased one, and those released in the last `days`.
fn kept(version: &Value, days: i64, today: NaiveDate) -> bool {
    if version["archived"].as_bool().unwrap_or_default() {
        return false;
    }
    match version["released"].as_bool().unwrap_or_default() {
        false => true,
        true => {
            date(&version["releaseDate"]).is_some_and(|day| day >= today - Duration::days(days))
        }
    }
}

fn version_record(version: &Value, project: &str, base: &str) -> Option<Value> {
    let id = text(&version["id"])?;
    Some(json!({
        "id": id,
        "project": project,
        "name": text(&version["name"]).unwrap_or_else(|| id.clone()),
        "description": version["description"].as_str().map(|d| d.chars().take(2_000).collect::<String>()),
        "released": version["released"].as_bool().unwrap_or_default(),
        "archived": version["archived"].as_bool().unwrap_or_default(),
        "overdue": version["overdue"].as_bool().unwrap_or_default(),
        "start_date": date(&version["startDate"]),
        "release_date": date(&version["releaseDate"]),
        "url": format!("{base}/projects/{project}/versions/{id}"),
    }))
}

fn names(list: &Value, field: &str) -> Vec<String> {
    list.as_array()
        .into_iter()
        .flatten()
        .filter_map(|item| match field {
            "" => text(item),
            field => text(&item[field]),
        })
        .collect()
}

/// An issue as it is exported, with the fix versions among `wanted`; `None` where it is in none.
fn issue_record(issue: &Value, base: &str, wanted: &BTreeSet<String>) -> (String, Option<Value>) {
    let id = text(&issue["id"]).unwrap_or_default();
    let fields = &issue["fields"];
    let versions: Vec<String> =
        names(&fields["fixVersions"], "id").into_iter().filter(|id| wanted.contains(id)).collect();
    if versions.is_empty() || id.is_empty() {
        return (id, None);
    }
    let key = text(&issue["key"]).unwrap_or_else(|| id.clone());
    let project = key.split('-').next().unwrap_or_default().to_string();
    let category = fields["status"]["statusCategory"]["key"]
        .as_str()
        .filter(|category| matches!(*category, "new" | "indeterminate" | "done"))
        .unwrap_or("undefined");
    let record = json!({
        "id": id,
        "key": key,
        "project": project,
        "summary": fields["summary"].as_str().map(|s| s.chars().take(1_000).collect::<String>()),
        "type": fields["issuetype"]["name"].as_str(),
        "status": fields["status"]["name"].as_str(),
        "category": category,
        "priority": fields["priority"]["name"].as_str(),
        "versions": versions,
        "components": names(&fields["components"], "name"),
        "labels": names(&fields["labels"], ""),
        "created": moment(&fields["created"]),
        "updated": moment(&fields["updated"]),
        "resolved_at": moment(&fields["resolutiondate"]),
        "url": format!("{base}/browse/{key}"),
    });
    (id, Some(record))
}

async fn written(backend: &Backend, writes: Vec<DataRequest>) -> Result<usize, PluginError> {
    let count = writes.len();
    for chunk in writes.chunks(BATCH) {
        backend.batch(chunk.to_vec()).await?;
    }
    Ok(count)
}

/// Every page a search finds, kept as it goes: issues in a wanted version are written, and any
/// that is in none any more is forgotten. `false` when the time ran out first.
async fn searched(
    backend: &Backend,
    jira: &Jira,
    jql: &str,
    wanted: &BTreeSet<String>,
    started: Instant,
    counts: &mut Counts,
) -> Result<bool, PluginError> {
    let mut cursor: Option<Cursor> = None;
    loop {
        if started.elapsed() > BUDGET {
            return Ok(false);
        }
        let (issues, next) = jira
            .search(jql, cursor.as_ref())
            .await
            .map_err(|refused| PluginError::from(refused.detail.as_str()))?;
        let mut writes = Vec::new();
        for issue in &issues {
            match issue_record(issue, jira.base(), wanted) {
                (_, Some(record)) => {
                    writes.push(DataRequest::upsert("issues", &["id"], record));
                    counts.issues += 1;
                }
                (id, None) if !id.is_empty() => writes.push(DataRequest::delete("issues", id)),
                _ => {}
            }
        }
        written(backend, writes).await?;
        match next {
            Some(next) => cursor = Some(next),
            None => return Ok(true),
        }
    }
}

#[derive(Default)]
struct Counts {
    projects: usize,
    versions: usize,
    issues: usize,
}

/// One project read: its versions, the issues of any version not read before, and what changed
/// since the last read. `false` when the time ran out first; what was read is kept either way.
async fn project(
    backend: &Backend,
    jira: &Jira,
    config: &Config,
    key: &str,
    started: Instant,
    counts: &mut Counts,
) -> Result<bool, PluginError> {
    let begun = Utc::now();
    let today = begun.date_naive();
    let listed =
        jira.versions(key).await.map_err(|refused| PluginError::from(refused.detail.as_str()))?;
    let records: Vec<Value> = listed
        .iter()
        .filter(|version| kept(version, config.days, today))
        .filter_map(|version| version_record(version, key, jira.base()))
        .collect();
    let wanted: BTreeSet<String> =
        records.iter().filter_map(|record| record["id"].as_str().map(str::to_string)).collect();
    let held: Vec<Value> = backend
        .query_all(Query::new("versions").filter(json!({ "project": key })).fields(&["id"]))
        .await?;
    let mut writes: Vec<DataRequest> = records
        .into_iter()
        .map(|record| DataRequest::upsert("versions", &["id"], record))
        .collect();
    counts.versions += writes.len();
    writes.extend(
        held.iter()
            .filter_map(|record| record["id"].as_str())
            .filter(|id| !wanted.contains(*id))
            .map(|id| DataRequest::delete("versions", id)),
    );
    written(backend, writes).await?;

    let state = format!("read/{key}");
    let mut kept_state: Kept = backend
        .state_get(&state)
        .await?
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    kept_state.versions.retain(|id| wanted.contains(id));
    let unread: Vec<&String> =
        wanted.iter().filter(|id| !kept_state.versions.contains(*id)).collect();
    for chunk in unread.chunks(IN_ONE) {
        let ids: Vec<&str> = chunk.iter().map(|id| id.as_str()).collect();
        let jql =
            format!("project = \"{key}\" AND fixVersion in ({}) ORDER BY key", ids.join(", "));
        if !searched(backend, jira, &jql, &wanted, started, counts).await? {
            backend.state_set(&state, json!(kept_state)).await?;
            return Ok(false);
        }
        kept_state.versions.extend(chunk.iter().map(|id| (*id).clone()));
        backend.state_set(&state, json!(kept_state)).await?;
    }
    if let Some(at) = kept_state.at {
        let minutes = (begun - at).num_minutes().max(0) + OVERLAP_MINUTES;
        let jql = format!("project = \"{key}\" AND updated >= \"-{minutes}m\" ORDER BY updated");
        if !searched(backend, jira, &jql, &wanted, started, counts).await? {
            return Ok(false);
        }
    }
    // Issues of versions no longer read — released too long ago, archived or deleted.
    let issues: Vec<Value> = backend
        .query_all(
            Query::new("issues").filter(json!({ "project": key })).fields(&["id", "versions"]),
        )
        .await?;
    let stale: Vec<DataRequest> = issues
        .iter()
        .filter(|issue| {
            !issue["versions"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .any(|id| wanted.contains(id))
        })
        .filter_map(|issue| issue["id"].as_str())
        .map(|id| DataRequest::delete("issues", id))
        .collect();
    written(backend, stale).await?;
    kept_state.at = Some(begun);
    backend.state_set(&state, json!(kept_state)).await?;
    counts.projects += 1;
    Ok(true)
}

/// A read of every project, from the one after `after`; one cut short queues the rest.
pub async fn read(
    backend: &Backend,
    config: &Config,
    dc: bool,
    after: Option<String>,
) -> Result<Value, PluginError> {
    if let Some(problem) = config.problem() {
        return Ok(json!({ "read": false, "why": problem }));
    }
    let jira = Jira::new(config, dc).map_err(|problem| PluginError::from(problem.as_str()))?;
    let started = Instant::now();
    let listed = jira
        .projects(&config.projects)
        .await
        .map_err(|refused| PluginError::from(refused.detail.as_str()))?;
    let mut projects: BTreeMap<String, Value> = BTreeMap::new();
    for found in listed {
        let Some(key) = text(&found["key"]) else { continue };
        let record = json!({
            "key": key,
            "id": text(&found["id"]).unwrap_or_default(),
            "name": text(&found["name"]).unwrap_or_else(|| key.clone()),
            "url": format!("{}/browse/{key}", jira.base()),
        });
        projects.insert(key, record);
    }
    if after.is_none() {
        let mut writes: Vec<DataRequest> = projects
            .values()
            .map(|record| DataRequest::upsert("projects", &["key"], record.clone()))
            .collect();
        let held: Vec<Value> = backend.query_all(Query::new("projects").fields(&["key"])).await?;
        for gone in held.iter().filter_map(|record| record["key"].as_str()) {
            if !projects.contains_key(gone) {
                writes.push(DataRequest::delete("projects", gone));
                backend.delete_where("versions", "id", json!({ "project": gone })).await?;
                backend.delete_where("issues", "id", json!({ "project": gone })).await?;
                backend.state_delete(&format!("read/{gone}")).await?;
            }
        }
        written(backend, writes).await?;
    }
    let mut counts = Counts::default();
    let mut previous = after.clone();
    for key in projects.keys().filter(|key| after.as_ref().is_none_or(|after| *key > after)) {
        if started.elapsed() > BUDGET
            || !project(backend, &jira, config, key, started, &mut counts).await?
        {
            let resume = json!({ "releases": { "after": previous } });
            let task = backend.task(resume).await?;
            tracing::info!(%task, project = %key, "the read of releases carries on in another task");
            if counts.issues + counts.versions > 0 {
                announce(backend, &counts).await;
            }
            return Ok(json!({ "read": counts.projects, "carries_on": task, "from": key }));
        }
        previous = Some(key.clone());
    }
    backend.state_set(SYNCED, json!({ "at": Utc::now(), "projects": projects.len() })).await?;
    announce(backend, &counts).await;
    Ok(json!({ "read": counts.projects, "versions": counts.versions, "issues": counts.issues }))
}

async fn announce(backend: &Backend, counts: &Counts) {
    let payload = json!({ "projects": counts.projects, "versions": counts.versions, "issues": counts.issues });
    if let Err(err) =
        backend.publish(&format!("plugin.{}.releases.synced", crate::ID), payload).await
    {
        tracing::warn!(%err, "the read of releases was not announced");
    }
}

/// How release data stands, for the roadmap's page: nothing secret.
pub async fn status(backend: &Backend, config: &Config) -> Value {
    let synced = backend.state_get(SYNCED).await.ok().flatten();
    json!({ "on": config.on, "problem": config.problem(), "synced": synced })
}

/// The first read after the feature comes on, rather than up to an hour later at the schedule.
pub async fn begin(backend: &Backend, config: &Config) {
    if !config.on || config.problem().is_some() {
        return;
    }
    let begun = backend.state_get(BEGUN).await.ok().flatten();
    let synced = backend.state_get(SYNCED).await.ok().flatten();
    if begun.is_some() || synced.is_some() {
        return;
    }
    match backend.task(json!({ "releases": {} })).await {
        Ok(task) => {
            let _ = backend.state_set(BEGUN, json!({ "at": Utc::now(), "task": task })).await;
            tracing::info!(%task, "the first read of releases is queued");
        }
        Err(err) => tracing::warn!(%err, "the first read of releases could not be queued"),
    }
}
