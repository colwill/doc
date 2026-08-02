//! Working a repository's figures out from a source's workflow runs: each workflow's runs summed
//! day by day, on the default branch and off it, and each time it failed on the default branch
//! until a run passed again. Always recomputed from the source rather than patched, so a late run
//! or a workflow moved to another stage comes out as if it had been there all along.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration as Wait, Instant};

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use doc_plugin_sdk::{Aggregate, Backend, DataRequest, Measure, Order, PluginError, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::ID;
use crate::settings::Definitions;
use crate::store::{Day, Recovery, Run};

/// How long one task recomputes before it hands the rest on, inside the 30 s a run gets.
const BUDGET: Wait = Wait::from_secs(20);
const BATCH: usize = 100;
/// How much a nightly recompute goes back over, so late runs are counted.
pub const NIGHTLY_DAYS: i64 = 30;
/// The most durations one day of one workflow keeps.
const MAX_DURATIONS: usize = 2_000;
/// A broken or fixed branch is announced only if it happened this recently, so reading months of
/// runs for the first time does not announce every one of them.
const ANNOUNCE_HOURS: i64 = 24;

pub fn median(values: &[f64]) -> Option<f64> {
    percentile(values, 50.0)
}

/// The `p`th percentile, interpolated between the two values either side of it.
pub fn percentile(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let at = p / 100.0 * (sorted.len() - 1) as f64;
    let (below, above) = (at.floor() as usize, at.ceil() as usize);
    Some(sorted[below] + (sorted[above] - sorted[below]) * (at - below as f64))
}

pub fn midnight(at: DateTime<Utc>) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(at.year(), at.month(), at.day(), 0, 0, 0).single().unwrap_or(at)
}

/// GitHub's own number for a run, the last part of its ID.
fn number(run: &Run) -> &str {
    run.id.rsplit('/').next().unwrap_or(&run.id)
}

/// Every day's figures from `from_day` on, and every broken spell, of one repository's runs.
pub fn derive(
    definitions: &Definitions,
    source: &str,
    repository: &str,
    runs: &[Run],
    from_day: DateTime<Utc>,
) -> (Vec<Day>, Vec<Recovery>) {
    let stage = |run: &Run| {
        definitions.stages.of(repository, &run.workflow, run.path.as_deref().unwrap_or("")).key()
    };
    let mut sorted: Vec<&Run> = runs.iter().collect();
    sorted.sort_by_key(|run| run.finished_at);

    let mut days: BTreeMap<(i64, bool, DateTime<Utc>), Day> = BTreeMap::new();
    for run in sorted.iter().filter(|run| run.finished_at >= from_day) {
        let day = midnight(run.finished_at);
        let branch = if run.default_branch { "default" } else { "other" };
        let held = days.entry((run.workflow_id, run.default_branch, day)).or_insert_with(|| Day {
            id: format!(
                "{source}:{repository}:{}:{branch}:{}",
                run.workflow_id,
                day.format("%Y-%m-%d")
            ),
            source: source.to_string(),
            repository: repository.to_string(),
            workflow_id: run.workflow_id,
            workflow: String::new(),
            path: None,
            stage: String::new(),
            default_branch: run.default_branch,
            day,
            runs: 0,
            succeeded: 0,
            failed: 0,
            cancelled: 0,
            reruns: 0,
            durations: Vec::new(),
        });
        // Named as its latest run names it, so a renamed workflow reads as it is called now.
        held.workflow.clone_from(&run.workflow);
        held.path.clone_from(&run.path);
        held.stage = stage(run).to_string();
        held.runs += 1;
        if run.succeeded() {
            held.succeeded += 1;
            held.reruns += i64::from(run.attempt > 1);
            if held.durations.len() < MAX_DURATIONS {
                held.durations.push(run.seconds());
            }
        } else if run.failed() {
            held.failed += 1;
        } else if run.conclusion == "cancelled" {
            held.cancelled += 1;
        }
    }

    // A spell starts at the first failure after a pass, and ends at the next pass; a cancelled or
    // skipped run says nothing either way.
    let mut recoveries = Vec::new();
    let mut open: BTreeMap<i64, Recovery> = BTreeMap::new();
    for run in sorted.iter().filter(|run| run.default_branch) {
        if run.failed() {
            match open.get_mut(&run.workflow_id) {
                Some(spell) => spell.failed_runs += 1,
                None => {
                    let spell = Recovery {
                        id: format!("{source}:{repository}:{}:{}", run.workflow_id, number(run)),
                        source: source.to_string(),
                        repository: repository.to_string(),
                        workflow_id: run.workflow_id,
                        workflow: run.workflow.clone(),
                        stage: stage(run).to_string(),
                        broke_at: run.finished_at,
                        broke_url: run.url.clone(),
                        fixed_at: None,
                        fixed_url: None,
                        seconds: None,
                        failed_runs: 1,
                    };
                    open.insert(run.workflow_id, spell);
                }
            }
        } else if run.succeeded()
            && let Some(mut spell) = open.remove(&run.workflow_id)
        {
            spell.fixed_at = Some(run.finished_at);
            spell.fixed_url.clone_from(&run.url);
            spell.seconds = Some((run.finished_at - spell.broke_at).num_seconds() as f64);
            recoveries.push(spell);
        }
    }
    recoveries.extend(open.into_values());
    recoveries.sort_by_key(|spell| spell.broke_at);
    (days.into_values().collect(), recoveries)
}

/// Announces a branch that broke or was fixed lately, once each.
async fn announce(backend: &Backend, spell: &Recovery) -> Result<(), PluginError> {
    let lately = Utc::now() - Duration::hours(ANNOUNCE_HOURS);
    let about = json!({
        "repository": spell.repository,
        "workflow": spell.workflow,
        "stage": spell.stage,
        "broke_at": spell.broke_at,
        "url": spell.broke_url,
    });
    if spell.broke_at >= lately {
        let topic = format!("plugin.{ID}.pipeline.broken");
        backend.publish_once(&topic, about.clone(), &format!("broken:{}", spell.id)).await?;
    }
    if let Some(fixed_at) = spell.fixed_at.filter(|at| *at >= lately) {
        let mut fixed = about;
        fixed["fixed_at"] = json!(fixed_at);
        fixed["seconds"] = json!(spell.seconds);
        fixed["failed_runs"] = json!(spell.failed_runs);
        fixed["url"] = json!(spell.fixed_url);
        let topic = format!("plugin.{ID}.pipeline.fixed");
        backend.publish_once(&topic, fixed, &format!("fixed:{}", spell.id)).await?;
    }
    Ok(())
}

async fn written<T: Serialize>(
    backend: &Backend,
    collection: &str,
    records: &[T],
) -> Result<(), PluginError> {
    for chunk in records.chunks(BATCH) {
        let writes =
            chunk.iter().map(|record| DataRequest::upsert(collection, &["id"], json!(record)));
        backend.batch(writes.collect()).await?;
    }
    Ok(())
}

/// Removes what a recompute no longer comes to: `held` less the IDs just written.
async fn unwritten(
    backend: &Backend,
    collection: &str,
    filter: Value,
    written: &BTreeSet<&str>,
) -> Result<(), PluginError> {
    let held: Vec<Map<String, Value>> =
        backend.query_all(Query::new(collection).filter(filter).fields(&["id"])).await?;
    let stale: Vec<DataRequest> = held
        .iter()
        .filter_map(|record| record.get("id").and_then(Value::as_str))
        .filter(|id| !written.contains(id))
        .map(|id| DataRequest::delete(collection, id))
        .collect();
    for chunk in stale.chunks(BATCH) {
        backend.batch(chunk.to_vec()).await?;
    }
    Ok(())
}

/// Works one repository out again from the day before `since`. A spell already open then is read
/// from its start, so it is still one spell however late its end is read.
pub async fn recompute(
    backend: &Backend,
    definitions: &Definitions,
    source: &str,
    repository: &str,
    since: DateTime<Utc>,
) -> Result<usize, PluginError> {
    let lower = repository.to_ascii_lowercase();
    let start = midnight(since - Duration::days(1));
    let spanning: Vec<Recovery> = backend
        .query_all(Query::new("recoveries").filter(json!({
            "repository": lower,
            "source": source,
            "broke_at": { "lt": start },
            "any": [{ "fixed_at": { "is_null": true } }, { "fixed_at": { "gte": start } }],
        })))
        .await?;
    let read_from = spanning.iter().map(|spell| spell.broke_at).fold(start, DateTime::min);
    let runs: Vec<Run> = backend
        .query_all(
            Query::new(&format!("{source}.workflow-runs"))
                .filter(json!({ "repository": lower, "finished_at": { "gte": read_from } }))
                .order(Order::asc("finished_at")),
        )
        .await?;

    let (days, recoveries) = derive(definitions, source, &lower, &runs, start);
    written(backend, "days", &days).await?;
    written(backend, "recoveries", &recoveries).await?;
    let kept: BTreeSet<&str> = days.iter().map(|day| day.id.as_str()).collect();
    let filter = json!({ "repository": lower, "source": source, "day": { "gte": start } });
    unwritten(backend, "days", filter, &kept).await?;
    let kept: BTreeSet<&str> = recoveries.iter().map(|spell| spell.id.as_str()).collect();
    let filter = json!({ "repository": lower, "source": source, "broke_at": { "gte": read_from } });
    unwritten(backend, "recoveries", filter, &kept).await?;
    for spell in &recoveries {
        announce(backend, spell).await?;
    }
    Ok(days.len())
}

/// One repository of one source, waiting to be recomputed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub source: String,
    pub repository: String,
}

/// Every repository each source has runs of in the last `days`, to recompute them all.
pub async fn everything(
    backend: &Backend,
    definitions: &Definitions,
    days: i64,
) -> Result<Vec<Pending>, PluginError> {
    let since = Utc::now() - Duration::days(days);
    let mut pending = Vec::new();
    for source in &definitions.sources {
        let grouped = backend
            .aggregate(
                Aggregate::new(&format!("{source}.workflow-runs"))
                    .filter(json!({ "finished_at": { "gte": since } }))
                    .group_by("repository")
                    .measure("runs", Measure::Count("*".into())),
            )
            .await;
        // A source that is not running, or exports nothing to cicd, is passed over.
        let grouped = match grouped {
            Ok(grouped) => grouped,
            Err(err) => {
                tracing::info!(source, %err, "a source's workflow runs could not be read");
                continue;
            }
        };
        for group in grouped {
            if let Some(repository) = group.get("repository").and_then(Value::as_str) {
                pending
                    .push(Pending { source: source.clone(), repository: repository.to_string() });
            }
        }
    }
    Ok(pending)
}

/// Recomputes each pending repository from `since` until the time runs out, then queues the rest
/// as the next task with the same `since`.
pub async fn work(
    backend: &Backend,
    definitions: &Definitions,
    pending: Vec<Pending>,
    since: DateTime<Utc>,
) -> Result<Value, PluginError> {
    let until = Instant::now() + BUDGET;
    let mut written = 0;
    let mut done = 0;
    let mut waiting = pending.into_iter();
    for next in waiting.by_ref() {
        written += recompute(backend, definitions, &next.source, &next.repository, since).await?;
        done += 1;
        if Instant::now() >= until {
            break;
        }
    }
    let rest: Vec<Pending> = waiting.collect();
    if !rest.is_empty() {
        let left = rest.len();
        backend.task(json!({ "recompute": { "pending": rest, "since": since } })).await?;
        return Ok(json!({ "repositories": done, "days": written, "left": left }));
    }
    tracing::info!(repositories = done, days = written, "recomputed");
    Ok(json!({ "repositories": done, "days": written }))
}

/// What is older than the settings keep.
pub async fn forget(backend: &Backend, definitions: &Definitions) -> Result<usize, PluginError> {
    let cutoff = Utc::now() - Duration::days(definitions.keep_days);
    let days = backend.delete_where("days", "id", json!({ "day": { "lt": cutoff } })).await?;
    let spells =
        backend.delete_where("recoveries", "id", json!({ "broke_at": { "lt": cutoff } })).await?;
    Ok(days + spells)
}
