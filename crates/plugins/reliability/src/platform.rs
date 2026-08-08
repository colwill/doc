//! DOC's own reliability, from the status history core keeps: each part of DOC — its database,
//! buses, backend and frontend — is down from the first of the settings' failed checks in a row
//! until one passes, and each plugin while it is in error or has left the registry. Read from
//! where the last read stopped, a day at a time, so the first read goes back over the 30 days core
//! keeps and every read after it costs a few minutes of checks.

use std::collections::BTreeMap;
use std::time::{Duration as Wait, Instant};

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, DataRequest, Order, PluginError, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::record::{self, Watching};
use crate::settings::Definitions;
use crate::store::Subject;

/// DOC's parts as its status probes name them, and what a page calls them. DOC is up while all of
/// them are.
pub const COMPONENTS: [(&str, &str); 6] = [
    ("postgres", "Database"),
    ("eventbus", "Event Bus"),
    ("servicebus", "Service Bus"),
    ("cachebus", "Cache Bus"),
    ("backend", "Backend"),
    ("frontend", "Frontend"),
];
/// What DOC's own backups are recorded against.
pub const DATABASE: &str = "doc:postgres";
/// How long one read goes on before it hands the rest on, inside the 30 s a run gets.
const BUDGET: Wait = Wait::from_secs(20);
/// How far back the first read goes: what core keeps.
const BACK_DAYS: i64 = 30;
/// A check a minute; more than this between two is a gap in watching.
const GAP_SECONDS: i64 = 180;
const BATCH: usize = 100;

/// What a component is called on a page.
pub fn titled(name: &str) -> String {
    COMPONENTS
        .iter()
        .find(|(component, _)| *component == name)
        .map_or_else(|| name.to_string(), |(_, title)| (*title).to_string())
}

/// Where the reads have got to in each history.
#[derive(Default, Serialize, Deserialize)]
struct Kept {
    #[serde(default)]
    components_to: Option<DateTime<Utc>>,
    #[serde(default)]
    plugins_to: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct Check {
    at: DateTime<Utc>,
    name: String,
    state: String,
    #[serde(default)]
    detail: Option<String>,
}

#[derive(Deserialize)]
struct Change {
    at: DateTime<Utc>,
    plugin: String,
    state: String,
    #[serde(default)]
    error: Option<String>,
}

fn subject_of<'a>(
    held: &'a mut BTreeMap<String, Subject>,
    key: String,
    kind: &str,
    name: &str,
) -> &'a mut Subject {
    held.entry(key.clone()).or_insert_with(|| Subject {
        subject: key,
        kind: kind.to_string(),
        name: name.to_string(),
        state: "unknown".into(),
        ..Subject::default()
    })
}

/// Reads DOC's status history on from where the last read stopped, until it has caught up or its
/// time is spent, when it queues the rest.
pub async fn read(backend: &Backend, definitions: &Definitions) -> Result<Value, PluginError> {
    let until = Instant::now() + BUDGET;
    let now = Utc::now();
    let mut kept: Kept = backend
        .state_get("platform/read")
        .await?
        .and_then(|held| serde_json::from_value(held).ok())
        .unwrap_or_default();
    let mut subjects: BTreeMap<String, Subject> = backend
        .query_all::<Subject>(
            Query::new("subjects").filter(json!({ "kind": { "in": ["component", "plugin"] } })),
        )
        .await?
        .into_iter()
        .map(|subject| (subject.subject.clone(), subject))
        .collect();
    let mut watching = Watching::default();
    let (mut checks, mut changes) = (0, 0);

    let mut from = kept.components_to.unwrap_or(now - Duration::days(BACK_DAYS));
    while from < now && Instant::now() < until {
        let to = (from + Duration::days(1)).min(now);
        let read: Vec<Check> = backend
            .query_all(
                Query::new("core.status-history")
                    .filter(json!({ "at": { "gte": from, "lt": to } }))
                    .order(Order::asc("at")),
            )
            .await?;
        for check in read {
            checks += 1;
            let key = format!("doc:{}", check.name);
            let subject = subject_of(&mut subjects, key, "component", &check.name);
            let down = check.state == "down"
                || (definitions.degraded_is_down && check.state == "degraded");
            let up =
                check.state == "up" || (!definitions.degraded_is_down && check.state == "degraded");
            let watched_from = subject
                .checked_at
                .filter(|last| check.at - *last <= Duration::seconds(GAP_SECONDS))
                .unwrap_or(check.at);
            watching.add(&subject.subject, watched_from, check.at, down);
            subject.since.get_or_insert(check.at);
            if down {
                subject.failing += 1;
                let since = *subject.failing_since.get_or_insert(check.at);
                if subject.failing >= definitions.failures && subject.state != "down" {
                    let detail = check.detail.clone();
                    record::open(backend, &subject.subject, since, "status", detail, None).await?;
                    subject.state = "down".into();
                }
            } else if up {
                if subject.state == "down" {
                    record::close(backend, &subject.subject, check.at, Some("status")).await?;
                }
                subject.state = "up".into();
                subject.failing = 0;
                subject.failing_since = None;
            }
            subject.checked_at = Some(check.at);
            subject.problem = check.detail.map(|detail| detail.chars().take(500).collect());
        }
        from = to;
    }
    kept.components_to = Some(from);

    let mut from = kept.plugins_to.unwrap_or(now - Duration::days(BACK_DAYS));
    while from < now && Instant::now() < until {
        let to = (from + Duration::days(BACK_DAYS)).min(now);
        let read: Vec<Change> = backend
            .query_all(
                Query::new("core.plugin-status-history")
                    .filter(json!({ "at": { "gte": from, "lt": to } }))
                    .order(Order::asc("at")),
            )
            .await?;
        for change in read {
            changes += 1;
            let key = format!("plugin:{}", change.plugin);
            let subject = subject_of(&mut subjects, key, "plugin", &change.plugin);
            subject.since.get_or_insert(change.at);
            match change.state.as_str() {
                "error" | "removed" if subject.state != "down" => {
                    let detail = change.error.clone().or_else(|| {
                        (change.state == "removed").then(|| "it left the registry".to_string())
                    });
                    record::open(backend, &subject.subject, change.at, "status", detail, None)
                        .await?;
                    subject.state = "down".into();
                }
                "running" | "cancelled" => {
                    if subject.state == "down" {
                        record::close(backend, &subject.subject, change.at, Some("status")).await?;
                    }
                    subject.state = "up".into();
                }
                _ => {}
            }
            subject.checked_at = Some(change.at);
            subject.problem = change.error.map(|error| error.chars().take(500).collect());
        }
        from = to;
    }
    kept.plugins_to = Some(from);

    let writes: Vec<DataRequest> = subjects
        .values()
        .map(|subject| DataRequest::upsert("subjects", &["subject"], json!(subject)))
        .collect();
    for chunk in writes.chunks(BATCH) {
        backend.batch(chunk.to_vec()).await?;
    }
    watching.flush(backend).await?;
    let caught_up = kept.components_to.is_some_and(|at| now - at < Duration::minutes(10));
    backend.state_set("platform/read", json!(kept)).await?;
    if !caught_up {
        backend.task(json!({ "platform": true })).await?;
    }
    Ok(json!({ "checks": checks, "changes": changes, "caught_up": caught_up }))
}
