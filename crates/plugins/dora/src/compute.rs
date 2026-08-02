//! Working a repository's deployments out from a source's delivery data (ADR-0007 §4): the lead
//! time of everything each one shipped, whether it caused a failure — a revert, a rollback, a
//! hotfix or a counted incident soon after it — and how long until the next deployment put that
//! right. Always recomputed from the source rather than patched, so late data and changed
//! definitions come out the same as if they had been there all along.

use std::collections::BTreeSet;
use std::time::{Duration as Wait, Instant};

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Aggregate, Backend, DataRequest, Measure, Order, PluginError, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::settings::{Definitions, From, LeadFrom};
use crate::store::{Commit, Counted, Deployment, Merged, Shipped};

/// How long one task recomputes before it hands the rest on, inside the 30 s a run gets.
const BUDGET: Wait = Wait::from_secs(20);
const BATCH: usize = 100;
/// How far before a deployment a pull request it shipped may have been merged, for lead times
/// measured from the merge.
const MERGED_BEFORE_DAYS: i64 = 90;
/// How much a nightly recompute goes back over, so late data is counted.
pub const NIGHTLY_DAYS: i64 = 30;

/// A sign that a deployment caused a failure: what it was, when, and where to read about it.
struct Signal {
    at: DateTime<Utc>,
    cause: String,
    url: Option<String>,
}

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

fn seconds(from: DateTime<Utc>, to: DateTime<Utc>) -> f64 {
    (to - from).num_seconds() as f64
}

fn is_revert(text: &str) -> bool {
    text.starts_with("Revert ")
}

/// Every deployment of one repository, worked out from what its source shipped, merged and what
/// was counted against it.
pub fn derive(
    definitions: &Definitions,
    source: &str,
    repository: &str,
    shipped: &[Shipped],
    merged: &[Merged],
    counted: &[Counted],
) -> Vec<Deployment> {
    let mut deployments: Vec<Deployment> = shipped
        .iter()
        .filter_map(|shipped| {
            let at = shipped.finished_at?;
            let lead_times: Vec<f64> = match definitions.lead_from {
                LeadFrom::Commit => {
                    shipped.commits.iter().map(|commit| seconds(commit.at, at)).collect()
                }
                LeadFrom::Merge => {
                    let shas: BTreeSet<&str> = shipped
                        .commits
                        .iter()
                        .map(|commit| commit.sha.as_str())
                        .chain([shipped.sha.as_str()])
                        .collect();
                    merged
                        .iter()
                        .filter(|pull| {
                            pull.merge_sha.as_deref().is_some_and(|sha| shas.contains(sha))
                        })
                        .map(|pull| seconds(pull.merged_at, at))
                        .collect()
                }
            };
            let lead_times: Vec<f64> = lead_times.into_iter().filter(|lead| *lead >= 0.0).collect();
            Some(Deployment {
                id: format!("{source}:{}", shipped.id),
                source: source.to_string(),
                repository: repository.to_string(),
                environment: shipped.environment.clone(),
                kind: shipped.kind.clone(),
                sha: shipped.sha.clone(),
                url: shipped.url.clone(),
                deployed_at: at,
                commits: shipped.commits.len() as i64,
                lead_median: median(&lead_times),
                lead_times,
                failed: false,
                failure: None,
                failure_at: None,
                failure_url: None,
                recovered_at: None,
                recovery_seconds: None,
            })
        })
        .collect();
    deployments.sort_by_key(|deployment| deployment.deployed_at);

    let mut signals = Vec::new();
    if definitions.rollbacks {
        for rolled in shipped.iter().filter(|shipped| shipped.rollback) {
            if let Some(at) = rolled.finished_at {
                signals.push(Signal { at, cause: "rollback".into(), url: rolled.url.clone() });
            }
        }
    }
    if definitions.reverts {
        for shipped in shipped {
            for commit in shipped.commits.iter().filter(|commit| is_revert(&commit.message)) {
                let url = shipped.url.clone();
                signals.push(Signal { at: commit.at, cause: "revert".into(), url });
            }
        }
        for pull in merged.iter().filter(|pull| is_revert(&pull.title)) {
            signals.push(Signal {
                at: pull.merged_at,
                cause: "revert".into(),
                url: pull.url.clone(),
            });
        }
    }
    if definitions.hotfixes {
        let hotfix = |pull: &&Merged| {
            pull.title.to_ascii_lowercase().starts_with("hotfix")
                || pull.labels.iter().any(|label| {
                    definitions.hotfix_labels.contains(&label.trim().to_ascii_lowercase())
                })
        };
        for pull in merged.iter().filter(hotfix) {
            signals.push(Signal {
                at: pull.merged_at,
                cause: "hotfix".into(),
                url: pull.url.clone(),
            });
        }
    }
    for count in counted.iter().filter(|count| definitions.counters.contains(&count.counter)) {
        signals.push(Signal { at: count.at, cause: count.counter.clone(), url: None });
    }
    signals.sort_by_key(|signal| signal.at);

    // Each signal is put down to the most recent deployment before it, if that was recent enough;
    // the first signal against a deployment is the one it is shown with.
    for signal in signals {
        let Some(index) = deployments.iter().rposition(|d| d.deployed_at < signal.at) else {
            continue;
        };
        let deployment = &mut deployments[index];
        if deployment.failed || signal.at - deployment.deployed_at > definitions.window() {
            continue;
        }
        deployment.failed = true;
        deployment.failure = Some(signal.cause);
        deployment.failure_at = Some(signal.at);
        deployment.failure_url = signal.url;
    }

    // Recovered by the next deployment to the same environment once the failure was known: the
    // rollback itself, or whatever shipped the fix.
    for index in 0..deployments.len() {
        let (Some(failure_at), true) = (deployments[index].failure_at, deployments[index].failed)
        else {
            continue;
        };
        let environment = deployments[index].environment.clone();
        let deployed_at = deployments[index].deployed_at;
        let next = deployments[index + 1..]
            .iter()
            .find(|next| next.environment == environment && next.deployed_at >= failure_at)
            .map(|next| next.deployed_at);
        deployments[index].recovered_at = next;
        deployments[index].recovery_seconds = next.map(|next| seconds(deployed_at, next));
    }
    deployments
}

/// Which of a source's kinds of deployment count for a repository: GitHub's deployments where it
/// uses them, and otherwise the workflows that deploy, unless the settings choose one.
async fn counted_kind(
    backend: &Backend,
    definitions: &Definitions,
    exported: &str,
    repository: &str,
) -> Result<&'static str, PluginError> {
    Ok(match definitions.from {
        From::Deployments => "deployment",
        From::Workflows => "workflow",
        From::PreferDeployments => {
            let found = backend
                .query::<Value>(
                    Query::new(exported)
                        .filter(json!({ "repository": repository, "kind": "deployment", "state": "success" }))
                        .fields(&["id"])
                        .limit(1),
                )
                .await?;
            match found.records.is_empty() {
                true => "workflow",
                false => "deployment",
            }
        }
    })
}

/// A finished rollout as a rollout source exports it (`kubernetes.rollouts`).
#[derive(Debug, Clone, Deserialize)]
struct Rolled {
    id: String,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    sha: String,
    #[serde(default)]
    url: Option<String>,
    finished_at: DateTime<Utc>,
    #[serde(default)]
    rollback: bool,
}

/// Whether one commit SHA is the other, either written short.
fn same_commit(one: &str, other: &str) -> bool {
    !one.is_empty() && !other.is_empty() && (one.starts_with(other) || other.starts_with(one))
}

/// A repository's production rollouts as deployments, when a rollout source has ever rolled it
/// out to production; `None` when none has, so the delivery data's own deployments count. Each
/// rollout ships the commits the delivery data says shipped with the same commit, or else the
/// pull requests merged after the previous rollout's and up to its own.
async fn rolled(
    backend: &Backend,
    definitions: &Definitions,
    source: &str,
    delivery: bool,
    repository: &str,
    start: DateTime<Utc>,
    merged: &[Merged],
) -> Result<Option<Vec<Shipped>>, PluginError> {
    let lower = repository.to_ascii_lowercase();
    for rolls in &definitions.rollouts {
        let exported = format!("{rolls}.rollouts");
        let ever = backend
            .query::<Value>(
                Query::new(&exported)
                    .filter(json!({ "repository": lower, "production": true }))
                    .fields(&["id"])
                    .limit(1),
            )
            .await;
        match ever {
            Ok(page) if !page.records.is_empty() => {}
            Ok(_) => continue,
            // A rollout source that is not running, or exports nothing to dora, is passed over.
            Err(err) => {
                tracing::debug!(rolls, %err, "a rollout source could not be read");
                continue;
            }
        }
        let rolled: Vec<Rolled> = backend
            .query_all(
                Query::new(&exported)
                    .filter(json!({
                        "repository": lower,
                        "production": true,
                        "state": "success",
                        "finished_at": { "gte": start - Duration::days(MERGED_BEFORE_DAYS) },
                    }))
                    .order(Order::asc("finished_at")),
            )
            .await?;
        let shipped_with: Vec<Shipped> = match delivery {
            true => backend
                .query_all(Query::new(&format!("{source}.deployments")).filter(json!({
                    "repository": repository,
                    "finished_at": { "gte": start - Duration::days(MERGED_BEFORE_DAYS) },
                })))
                .await
                .unwrap_or_else(|err| {
                    tracing::debug!(source, %err, "no delivery data to give rollouts lead times");
                    Vec::new()
                }),
            false => Vec::new(),
        };
        let merge_of = |sha: &str| {
            merged
                .iter()
                .filter(|pull| {
                    pull.merge_sha.as_deref().is_some_and(|merge| same_commit(merge, sha))
                })
                .map(|pull| pull.merged_at)
                .min()
        };
        let mut before: Option<DateTime<Utc>> = None;
        let mut out = Vec::new();
        for rollout in rolled {
            let merged_at = merge_of(&rollout.sha);
            let commits = match shipped_with
                .iter()
                .find(|held| same_commit(&held.sha, &rollout.sha) && !held.commits.is_empty())
            {
                Some(held) => held.commits.clone(),
                None => match merged_at {
                    Some(up_to) => merged
                        .iter()
                        .filter(|pull| {
                            (pull.merged_at <= up_to
                                && before.is_some_and(|after| pull.merged_at > after))
                                || pull
                                    .merge_sha
                                    .as_deref()
                                    .is_some_and(|merge| same_commit(merge, &rollout.sha))
                        })
                        .map(|pull| Commit {
                            sha: pull.merge_sha.clone().unwrap_or_default(),
                            at: pull.merged_at,
                            message: pull.title.clone(),
                        })
                        .collect(),
                    None => Vec::new(),
                },
            };
            if merged_at.is_some() {
                before = merged_at;
            }
            if rollout.finished_at < start {
                continue;
            }
            out.push(Shipped {
                id: rollout.id,
                environment: rollout.environment.unwrap_or_else(|| "production".into()),
                kind: "rollout".into(),
                sha: rollout.sha,
                url: rollout.url,
                finished_at: Some(rollout.finished_at),
                rollback: rollout.rollback,
                commits,
            });
        }
        return Ok(Some(out));
    }
    Ok(None)
}

/// Works one repository out again from `since`, going back far enough before it that everything a
/// later signal could change is included, writes what it comes to, and removes what the
/// definitions no longer count.
pub async fn recompute(
    backend: &Backend,
    definitions: &Definitions,
    source: &str,
    repository: &str,
    since: DateTime<Utc>,
) -> Result<usize, PluginError> {
    let start = since - definitions.window() - Duration::days(1);
    let lower = repository.to_ascii_lowercase();
    // A source that only rolls out has no delivery data of its own to read.
    let delivery = !definitions.rollouts.iter().any(|rolls| rolls == source);
    let merged_since = match (definitions.lead_from, definitions.rollouts.is_empty()) {
        (LeadFrom::Commit, true) => start,
        _ => start - Duration::days(MERGED_BEFORE_DAYS),
    };
    // Rollouts count without the delivery data, which only gives them lead times, so a delivery
    // source that is not running fails the work only when its own deployments are what count.
    let (merged, unread): (Vec<Merged>, Option<PluginError>) =
        match delivery {
            true => match backend
                .query_all(Query::new(&format!("{source}.pull-requests")).filter(
                    json!({ "repository": repository, "merged_at": { "gte": merged_since } }),
                ))
                .await
            {
                Ok(merged) => (merged, None),
                Err(err) => (Vec::new(), Some(err)),
            },
            false => (Vec::new(), None),
        };
    let shipped =
        match rolled(backend, definitions, source, delivery, repository, start, &merged).await? {
            Some(rolled) => rolled,
            None if delivery => {
                if let Some(err) = unread {
                    return Err(err);
                }
                let exported = format!("{source}.deployments");
                let kind = counted_kind(backend, definitions, &exported, repository).await?;
                backend
                    .query_all(
                        Query::new(&exported)
                            .filter(json!({
                                "repository": repository,
                                "kind": kind,
                                "state": "success",
                                "finished_at": { "gte": start },
                            }))
                            .order(Order::asc("finished_at")),
                    )
                    .await?
            }
            None => Vec::new(),
        };
    let counted: Vec<Counted> = backend
        .query_all(
            Query::new("counters").filter(json!({ "repository": lower, "at": { "gte": start } })),
        )
        .await?;

    let derived = derive(definitions, source, &lower, &shipped, &merged, &counted);
    for chunk in derived.chunks(BATCH) {
        let writes = chunk
            .iter()
            .map(|deployment| DataRequest::upsert("deployments", &["id"], json!(deployment)))
            .collect();
        backend.batch(writes).await?;
    }

    let written: BTreeSet<&str> = derived.iter().map(|deployment| deployment.id.as_str()).collect();
    let held: Vec<Map<String, Value>> = backend
        .query_all(
            Query::new("deployments")
                .filter(json!({ "repository": lower, "source": source, "deployed_at": { "gte": start } }))
                .fields(&["id"]),
        )
        .await?;
    let stale: Vec<DataRequest> = held
        .iter()
        .filter_map(|record| record.get("id").and_then(Value::as_str))
        .filter(|id| !written.contains(id))
        .map(|id| DataRequest::delete("deployments", id))
        .collect();
    for chunk in stale.chunks(BATCH) {
        backend.batch(chunk.to_vec()).await?;
    }
    Ok(derived.len())
}

/// One repository of one source, waiting to be recomputed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Pending {
    pub source: String,
    pub repository: String,
}

/// Every repository each source has deployed from in the last `days`, to recompute them all.
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
                Aggregate::new(&format!("{source}.deployments"))
                    .filter(json!({ "finished_at": { "gte": since } }))
                    .group_by("repository")
                    .measure("deployments", Measure::Count("*".into())),
            )
            .await;
        // A source that is not running, or exports nothing to dora, is passed over.
        let grouped = match grouped {
            Ok(grouped) => grouped,
            Err(err) => {
                tracing::info!(source, %err, "a source's delivery data could not be read");
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
    // Repositories only a rollout source deploys are worked out against the first delivery
    // source, for their lead times, or on their own where there is none.
    for rolls in &definitions.rollouts {
        let grouped = backend
            .aggregate(
                Aggregate::new(&format!("{rolls}.rollouts"))
                    .filter(json!({ "production": true, "finished_at": { "gte": since } }))
                    .group_by("repository")
                    .measure("rollouts", Measure::Count("*".into())),
            )
            .await;
        let Ok(grouped) = grouped else { continue };
        let source = definitions.sources.first().unwrap_or(rolls);
        for group in grouped {
            let Some(repository) = group.get("repository").and_then(Value::as_str) else {
                continue;
            };
            if !pending.iter().any(|held| held.repository.eq_ignore_ascii_case(repository)) {
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
        return Ok(json!({ "repositories": done, "deployments": written, "left": left }));
    }
    tracing::info!(repositories = done, deployments = written, "recomputed");
    Ok(json!({ "repositories": done, "deployments": written }))
}

/// What is older than the settings keep: deployments and counters both.
pub async fn forget(backend: &Backend, definitions: &Definitions) -> Result<usize, PluginError> {
    let cutoff = Utc::now() - Duration::days(definitions.keep_days);
    let deployments = backend
        .delete_where("deployments", "id", json!({ "deployed_at": { "lt": cutoff } }))
        .await?;
    let counters =
        backend.delete_where("counters", "id", json!({ "at": { "lt": cutoff } })).await?;
    Ok(deployments + counters)
}
