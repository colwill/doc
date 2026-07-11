//! Pipeline data: every GitHub Actions workflow run of each chosen repository — which workflow,
//! branch and commit, how it ended, when it started and finished, and how many attempts it took —
//! kept as a collection exported to `cicd`. Read like delivery data: on a schedule from where the
//! last read stopped, sooner on a webhook, in a chain of tasks that each stop in time.

use std::time::Instant;

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, Collection, DataRequest, Declaration, Export, Field, PluginError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::delivery::{
    BATCH, BUDGET, OVERLAP_HOURS, Outcome, PER_PAGE, Queued, Reading, chosen, earliest, stopped,
};
use crate::github::{GitHub, Read, Refused};
use crate::settings::Settings;
use crate::sync::Tokens;

/// What a run can end as, as GitHub says it.
const CONCLUSIONS: [&str; 9] = [
    "success",
    "failure",
    "cancelled",
    "skipped",
    "timed_out",
    "startup_failure",
    "neutral",
    "action_required",
    "stale",
];

pub fn declared(declaration: Declaration) -> Declaration {
    declaration.collection(
        "workflow-runs",
        Collection::new()
            .field("id", Field::text().key().describe("owner/name/runs/<GitHub's run ID>"))
            .field("repository", Field::text().required().describe("owner/name, in lower case"))
            .field("workflow_id", Field::integer().required())
            .field("workflow", Field::text().required().describe("The workflow's name"))
            .field("path", Field::text().describe("Its file, such as .github/workflows/ci.yml"))
            .field(
                "event",
                Field::text().describe("What started it: push, pull_request, schedule…"),
            )
            .field("branch", Field::text())
            .field(
                "default_branch",
                Field::boolean()
                    .required()
                    .default(json!(false))
                    .describe("It ran on the repository's default branch"),
            )
            .field("sha", Field::text().required())
            .field("run_number", Field::integer())
            .field("conclusion", Field::text().required().one_of(&CONCLUSIONS))
            .field(
                "attempt",
                Field::integer()
                    .required()
                    .default(json!(1))
                    .min(1.0)
                    .describe("Its latest attempt: more than 1 means it was re-run"),
            )
            .field("created_at", Field::timestamp().required())
            .field("started_at", Field::timestamp().describe("When its latest attempt started"))
            .field("finished_at", Field::timestamp().required())
            .field("url", Field::text())
            .index(&["created_at"])
            .index(&["repository", "created_at"])
            .index(&["finished_at"])
            .index(&["repository", "finished_at"])
            .export(Export::to(&["cicd"])),
    )
}

#[derive(Deserialize)]
struct Run {
    id: u64,
    #[serde(default)]
    name: Option<String>,
    workflow_id: u64,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    event: Option<String>,
    #[serde(default)]
    head_branch: Option<String>,
    head_sha: String,
    #[serde(default)]
    run_number: Option<u64>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    run_attempt: Option<u64>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    run_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    html_url: Option<String>,
}

/// Where a repository's reads have got to. A first read of a busy repository takes many pages, so
/// one cut short by its time or the rate limit carries on from the page it reached: new runs only
/// push older ones further back, so carrying on re-reads a few rather than missing any.
#[derive(Default, Serialize, Deserialize)]
struct Kept {
    #[serde(default)]
    synced_at: Option<DateTime<Utc>>,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    resume: Option<Resume>,
}

#[derive(Clone, Copy, Serialize, Deserialize)]
struct Resume {
    page: usize,
    started: DateTime<Utc>,
    since: DateTime<Utc>,
}

fn kept_key(repository: &str) -> String {
    format!("pipelines/{}", repository.to_ascii_lowercase())
}

/// The schedule: every chosen repository in each organisation, read in a chain of tasks.
pub async fn scheduled(
    backend: &Backend,
    github: &GitHub,
    settings: &Settings,
    tokens: &Tokens,
    plugin: &str,
) -> Result<Value, PluginError> {
    if let Some(problem) = settings.pipeline_problem() {
        return Ok(json!({ "read": false, "why": problem }));
    }
    let chosen = chosen(github, settings, tokens).await?;
    read(backend, github, settings, tokens, plugin, chosen, true).await
}

/// Reads repositories in turn until the time runs out, then queues the rest — the one it stopped
/// in first — as the next task. A chain the schedule began records when it has read them all.
pub async fn read(
    backend: &Backend,
    github: &GitHub,
    settings: &Settings,
    tokens: &Tokens,
    plugin: &str,
    queue: Vec<Queued>,
    whole: bool,
) -> Result<Value, PluginError> {
    if let Some(problem) = settings.pipeline_problem() {
        return Ok(json!({ "read": false, "why": problem }));
    }
    let until = Instant::now() + BUDGET;
    let mut done = 0;
    let mut waiting = queue.into_iter().peekable();
    while let Some(next) = waiting.peek().cloned() {
        if Instant::now() >= until {
            break;
        }
        let org = next.name.split('/').next().unwrap_or_default();
        let token = tokens.for_org(github, settings, org).await.map_err(PluginError::from)?;
        let reading = Reading {
            backend,
            github,
            settings,
            token: token.expose(),
            plugin,
            repository: &next.name,
            until,
        };
        match repository(&reading, next.branch.as_deref()).await? {
            Outcome::Done => {
                done += 1;
                waiting.next();
            }
            Outcome::OutOfTime => break,
            Outcome::Limited(why) => {
                tracing::warn!(repository = %next.name, %why, "GitHub's rate limit is spent; the next read carries on");
                return Ok(json!({ "read": done, "stopped": why }));
            }
        }
    }
    let rest: Vec<Queued> = waiting.collect();
    if !rest.is_empty() {
        let left = rest.len();
        backend.task(json!({ "pipelines": { "repositories": rest, "whole": whole } })).await?;
        return Ok(json!({ "read": done, "left": left }));
    }
    if whole {
        backend.state_set("pipelines/synced", json!({ "at": Utc::now(), "read": done })).await?;
    }
    tracing::info!(read = done, "pipeline data read");
    Ok(json!({ "read": done }))
}

/// A run as it is kept: only a finished one, since a run still going has nothing to say yet.
fn record(repository: &str, branch: Option<&str>, run: Run) -> Option<Value> {
    if run.status.as_deref() != Some("completed") {
        return None;
    }
    let conclusion = run.conclusion.filter(|said| CONCLUSIONS.contains(&said.as_str()))?;
    let url = run.html_url.filter(|url| url.starts_with("https://") || url.starts_with("http://"));
    Some(json!({
        "id": format!("{repository}/runs/{}", run.id),
        "repository": repository,
        "workflow_id": run.workflow_id,
        "workflow": run.name.unwrap_or_else(|| format!("workflow {}", run.workflow_id)),
        "path": run.path,
        "event": run.event,
        "default_branch": branch.is_some() && run.head_branch.as_deref() == branch,
        "branch": run.head_branch,
        "sha": run.head_sha,
        "run_number": run.run_number,
        "conclusion": conclusion,
        "attempt": run.run_attempt.unwrap_or(1).max(1),
        "created_at": run.created_at,
        "started_at": run.run_started_at,
        "finished_at": run.updated_at,
        "url": url,
    }))
}

async fn repository(reading: &Reading<'_>, branch: Option<&str>) -> Result<Outcome, PluginError> {
    let backend = reading.backend;
    let name = reading.repository;
    let lower = name.to_ascii_lowercase();
    let key = kept_key(name);
    let mut kept: Kept = backend
        .state_get(&key)
        .await?
        .and_then(|held| serde_json::from_value(held).ok())
        .unwrap_or_default();
    let Resume { mut page, started, since } = kept.resume.unwrap_or_else(|| {
        let started = Utc::now();
        let since = match kept.synced_at {
            Some(at) => at - Duration::hours(OVERLAP_HOURS),
            None => started - Duration::days(reading.settings.pipelines.days),
        };
        Resume { page: 1, started, since }
    });
    let fresh = kept.resume.is_none();
    let mut changed = None;
    loop {
        let path = format!("repos/{name}/actions/runs?per_page={PER_PAGE}&page={page}");
        let etag = if fresh && page == 1 { kept.etag.as_deref() } else { None };
        let body = match reading.github.read(reading.settings, reading.token, &path, etag).await {
            Ok(Read::Unchanged) => break,
            Ok(Read::Fresh(body, tag)) => {
                if fresh && page == 1 {
                    kept.etag = tag;
                }
                body
            }
            // A repository with Actions turned off answers 404, and simply has no runs.
            Err(refused) if refused.status == Some(404) => break,
            Err(refused) => {
                if refused.limited() {
                    kept.resume = Some(Resume { page, started, since });
                    backend.state_set(&key, json!(kept)).await?;
                }
                return Ok(stopped(name, refused));
            }
        };
        let listed = match serde_json::from_value::<Vec<Run>>(body["workflow_runs"].clone()) {
            Ok(listed) => listed,
            Err(err) => {
                let detail = format!("GitHub's {path} could not be read: {err}");
                return Ok(stopped(name, Refused { status: None, detail }));
            }
        };
        let count = listed.len();
        let reached = listed.iter().any(|run| run.created_at < since);
        let records: Vec<Value> = listed
            .into_iter()
            .filter(|run| run.created_at >= since)
            .filter_map(|run| {
                let at = run.created_at;
                record(&lower, branch, run).inspect(|_| earliest(&mut changed, at))
            })
            .collect();
        for chunk in records.chunks(BATCH) {
            let writes = chunk
                .iter()
                .map(|run| DataRequest::upsert("workflow-runs", &["id"], run.clone()))
                .collect();
            backend.batch(writes).await?;
        }
        if reached || count < PER_PAGE {
            break;
        }
        page += 1;
        if reading.out_of_time() {
            kept.resume = Some(Resume { page, started, since });
            backend.state_set(&key, json!(kept)).await?;
            return Ok(Outcome::OutOfTime);
        }
    }
    kept.synced_at = Some(started);
    kept.resume = None;
    backend.state_set(&key, json!(kept)).await?;
    if let Some(since) = changed {
        let topic = format!("plugin.{}.pipelines.synced", reading.plugin);
        backend.publish(&topic, json!({ "repository": lower, "since": since })).await?;
    }
    Ok(Outcome::Done)
}

/// How pipeline data stands, for `cicd` to say on its own page why it has nothing yet.
pub async fn status(backend: &Backend, settings: &Settings) -> Value {
    let synced = backend.state_get("pipelines/synced").await.ok().flatten();
    json!({
        "on": settings.pipelines.on,
        "problem": settings.pipeline_problem(),
        "webhooks": settings.delivery.webhook_secret.is_some(),
        "synced": synced,
    })
}

/// The first read after the feature comes on, rather than up to an hour later at the schedule.
pub async fn begin(backend: &Backend, settings: &Settings) {
    if settings.pipeline_problem().is_some() {
        return;
    }
    let begun = backend.state_get("pipelines/begun").await.ok().flatten();
    let synced = backend.state_get("pipelines/synced").await.ok().flatten();
    if begun.is_some() || synced.is_some() {
        return;
    }
    match backend.task(json!({ "schedule": "pipelines" })).await {
        Ok(task) => {
            let _ = backend
                .state_set("pipelines/begun", json!({ "at": Utc::now(), "task": task }))
                .await;
            tracing::info!(%task, "the first read of pipeline data is queued");
        }
        Err(err) => tracing::warn!(%err, "the first read of pipeline data could not be queued"),
    }
}
