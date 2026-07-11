//! Delivery data (ADR-0007 §2): each chosen repository's production deployments — GitHub's own,
//! and successful runs of the workflows that deploy — with the commits each one shipped, and its
//! merged pull requests, kept as collections exported to `dora`. A schedule reads each repository
//! from where it last stopped, a webhook has one read again as soon as it changes, and the work
//! runs as a chain of tasks that each stop in time and hand on what is left.

use std::collections::BTreeMap;
use std::time::{Duration as Wait, Instant};

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{
    Backend, Collection, DataRequest, Declaration, Export, Field, ListOf, Order, PluginError,
    Query, Request, Response,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::github::{GitHub, Read, Refused};
use crate::settings::Settings;
use crate::sync::Tokens;

/// How long one task reads before it hands what is left to the next, inside the 30 s a run gets.
pub(crate) const BUDGET: Wait = Wait::from_secs(20);
pub(crate) const PER_PAGE: usize = 100;
/// A list is read this many pages back at most in one read; the next one carries on.
const MAX_PAGES: usize = 10;
/// Each read overlaps the last by this much, so what was still running then is read again.
pub(crate) const OVERLAP_HOURS: i64 = 24;
/// The most commits kept for one deployment: what the compare API answers with in one page.
const MAX_COMMITS: usize = 250;
/// A commit's message is kept to its first line, and to this much of it.
const MESSAGE: usize = 200;
/// Writes per batch, the data API's own limit.
pub(crate) const BATCH: usize = 100;

pub const SUCCESS: &str = "success";
pub const FAILURE: &str = "failure";
pub const RUNNING: &str = "in_progress";
/// A deployment GitHub marked inactive before it ever succeeded: superseded, never live.
pub const INACTIVE: &str = "inactive";

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "deployments",
            Collection::new()
                .field("id", Field::text().key())
                .field("repository", Field::text().required())
                .field("environment", Field::text().required())
                .field(
                    "kind",
                    Field::text()
                        .required()
                        .one_of(&["deployment", "workflow"])
                        .describe("GitHub's Deployments API, or a run of a workflow that deploys"),
                )
                .field(
                    "state",
                    Field::text().required().one_of(&[SUCCESS, FAILURE, RUNNING, INACTIVE]),
                )
                .field("sha", Field::text().required())
                .field("branch", Field::text())
                .field("workflow", Field::text())
                .field("url", Field::text())
                .field("started_at", Field::timestamp().required())
                .field("finished_at", Field::timestamp())
                .field("previous_sha", Field::text())
                .field(
                    "rollback",
                    Field::boolean()
                        .required()
                        .default(json!(false))
                        .describe("It deployed a commit older than the deployment before it"),
                )
                .field(
                    "commits",
                    Field::json()
                        .required()
                        .default(json!([]))
                        .describe("What it shipped since the one before: sha, at and message"),
                )
                .field("commits_resolved", Field::boolean().required().default(json!(false)))
                .index(&["finished_at"])
                .index(&["repository", "finished_at"])
                .export(Export::to(&["dora"])),
        )
        .collection(
            "pull-requests",
            Collection::new()
                .field("id", Field::text().key())
                .field("repository", Field::text().required())
                .field("number", Field::integer().required())
                .field("title", Field::text().required())
                .field("labels", Field::list(ListOf::Text).required().default(json!([])))
                .field("merged_at", Field::timestamp().required())
                .field("merge_sha", Field::text())
                .field("url", Field::text())
                .index(&["merged_at"])
                .index(&["repository", "merged_at"])
                .export(Export::to(&["dora"])),
        )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub at: DateTime<Utc>,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deployment {
    pub id: String,
    pub repository: String,
    pub environment: String,
    pub kind: String,
    pub state: String,
    pub sha: String,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub workflow: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub previous_sha: Option<String>,
    #[serde(default)]
    pub rollback: bool,
    #[serde(default)]
    pub commits: Vec<Commit>,
    #[serde(default)]
    pub commits_resolved: bool,
}

#[derive(Deserialize)]
struct ListedDeployment {
    id: u64,
    sha: String,
    #[serde(rename = "ref", default)]
    branch: Option<String>,
    environment: String,
    created_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct Status {
    state: String,
    created_at: DateTime<Utc>,
    #[serde(default)]
    log_url: Option<String>,
    #[serde(default)]
    target_url: Option<String>,
}

#[derive(Deserialize)]
struct Workflow {
    id: u64,
    name: String,
    path: String,
}

#[derive(Deserialize)]
struct Run {
    id: u64,
    head_sha: String,
    #[serde(default)]
    head_branch: Option<String>,
    #[serde(default)]
    conclusion: Option<String>,
    created_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    run_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Deserialize)]
struct Label {
    name: String,
}

/// The branch a pull request was merged into.
#[derive(Deserialize)]
struct Base {
    #[serde(rename = "ref")]
    branch: String,
}

#[derive(Deserialize)]
struct Pull {
    number: u64,
    title: String,
    #[serde(default)]
    labels: Vec<Label>,
    #[serde(default)]
    base: Option<Base>,
    #[serde(default)]
    merged_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
    #[serde(default)]
    merge_commit_sha: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Deserialize)]
struct Compared {
    status: String,
    #[serde(default)]
    commits: Vec<Listed>,
}

#[derive(Deserialize)]
struct Listed {
    sha: String,
    commit: Detail,
}

#[derive(Deserialize)]
struct Detail {
    message: String,
    #[serde(default)]
    author: Option<Signed>,
    #[serde(default)]
    committer: Option<Signed>,
}

#[derive(Deserialize)]
struct Signed {
    date: DateTime<Utc>,
}

impl Listed {
    /// When it was committed, which is where its lead time starts.
    fn commit(self) -> Option<Commit> {
        let at = self.commit.committer.or(self.commit.author)?.date;
        let line = self.commit.message.lines().next().unwrap_or_default();
        Some(Commit { sha: self.sha, at, message: line.chars().take(MESSAGE).collect() })
    }
}

/// What a repository's last read left for the next: where it got to, and the ETags of the first
/// page of each list, so a list that has not changed is not read again.
#[derive(Default, Serialize, Deserialize)]
struct Kept {
    #[serde(default)]
    synced_at: Option<DateTime<Utc>>,
    #[serde(default)]
    etags: BTreeMap<String, String>,
}

fn kept_key(repository: &str) -> String {
    format!("delivery/{}", repository.to_ascii_lowercase())
}

/// One repository waiting to be read, with the branch its deploy workflows run on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Queued {
    pub name: String,
    #[serde(default)]
    pub branch: Option<String>,
}

pub(crate) enum Outcome {
    Done,
    OutOfTime,
    /// GitHub's rate limit is spent: the next schedule carries on.
    Limited(String),
}

/// Everything one repository's read needs, and when it has to stop.
pub(crate) struct Reading<'a> {
    pub backend: &'a Backend,
    pub github: &'a GitHub,
    pub settings: &'a Settings,
    pub token: &'a str,
    pub plugin: &'a str,
    pub repository: &'a str,
    pub until: Instant,
}

impl Reading<'_> {
    pub(crate) fn out_of_time(&self) -> bool {
        Instant::now() >= self.until
    }

    /// A list read a page at a time, newest first, until an item is older than the read began or
    /// the pages run out. The first page is asked for with its ETag; `None` means GitHub says it
    /// has not changed, and so neither has anything after it.
    async fn pages<T: DeserializeOwned>(
        &self,
        path: &str,
        field: Option<&str>,
        etag: (&str, &mut BTreeMap<String, String>),
        older: impl Fn(&T) -> bool,
    ) -> Result<Option<Vec<T>>, Refused> {
        let (key, etags) = etag;
        let joiner = if path.contains('?') { '&' } else { '?' };
        let mut items = Vec::new();
        for page in 1..=MAX_PAGES {
            let paged = format!("{path}{joiner}per_page={PER_PAGE}&page={page}");
            let asked = match page {
                1 => etags.get(key).map(String::as_str),
                _ => None,
            };
            let body = match self.github.read(self.settings, self.token, &paged, asked).await? {
                Read::Unchanged => return Ok(None),
                Read::Fresh(body, fresh) => {
                    if let (1, Some(fresh)) = (page, fresh) {
                        etags.insert(key.to_string(), fresh);
                    }
                    body
                }
            };
            let listed = match field {
                Some(field) => body[field].clone(),
                None => body,
            };
            let Value::Array(listed) = listed else { break };
            let count = listed.len();
            let mut reached = false;
            for item in listed {
                let item: T = serde_json::from_value(item).map_err(|err| Refused {
                    status: None,
                    detail: format!("GitHub's {path} could not be read: {err}"),
                })?;
                match older(&item) {
                    true => reached = true,
                    false => items.push(item),
                }
            }
            if reached || count < PER_PAGE {
                break;
            }
        }
        Ok(Some(items))
    }

    async fn one<T: DeserializeOwned>(&self, path: &str) -> Result<T, Refused> {
        match self.github.read(self.settings, self.token, path, None).await? {
            Read::Fresh(body, _) => serde_json::from_value(body).map_err(|err| Refused {
                status: None,
                detail: format!("GitHub's {path} could not be read: {err}"),
            }),
            Read::Unchanged => {
                Err(Refused { status: Some(304), detail: format!("{path} came back unchanged") })
            }
        }
    }
}

pub(crate) fn encoded(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

pub(crate) fn earliest(changed: &mut Option<DateTime<Utc>>, at: DateTime<Utc>) {
    *changed = Some(changed.map_or(at, |held| held.min(at)));
}

/// Every chosen repository in each organisation, with its default branch.
pub(crate) async fn chosen(
    github: &GitHub,
    settings: &Settings,
    tokens: &Tokens,
) -> Result<Vec<Queued>, PluginError> {
    let mut chosen = Vec::new();
    for org in &settings.organisations {
        let token = tokens.for_org(github, settings, org).await.map_err(PluginError::from)?;
        let repositories =
            github.repositories(settings, token.expose(), org).await.map_err(PluginError::from)?;
        for repository in repositories {
            if !repository.archived
                && settings.delivery.reads(&repository.full_name, &repository.topics)
            {
                chosen
                    .push(Queued { name: repository.full_name, branch: repository.default_branch });
            }
        }
    }
    Ok(chosen)
}

/// The schedule: every chosen repository in each organisation, read in a chain of tasks.
pub async fn scheduled(
    backend: &Backend,
    github: &GitHub,
    settings: &Settings,
    tokens: &Tokens,
    plugin: &str,
) -> Result<Value, PluginError> {
    if let Some(problem) = settings.delivery_problem() {
        return Ok(json!({ "read": false, "why": problem }));
    }
    let chosen = chosen(github, settings, tokens).await?;
    read(backend, github, settings, tokens, plugin, chosen, true).await
}

/// Reads repositories in turn until the time runs out, then queues the rest as the next task. A
/// chain the schedule began records when it has read them all.
pub async fn read(
    backend: &Backend,
    github: &GitHub,
    settings: &Settings,
    tokens: &Tokens,
    plugin: &str,
    queue: Vec<Queued>,
    whole: bool,
) -> Result<Value, PluginError> {
    if let Some(problem) = settings.delivery_problem() {
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
        backend.task(json!({ "delivery": { "repositories": rest, "whole": whole } })).await?;
        return Ok(json!({ "read": done, "left": left }));
    }
    if whole {
        backend.state_set("delivery/synced", json!({ "at": Utc::now(), "read": done })).await?;
    }
    tracing::info!(read = done, "delivery data read");
    Ok(json!({ "read": done }))
}

/// A refusal that stops this repository: a spent rate limit stops the chain, anything else is
/// logged and the repository is left to be read again next time from where it was.
pub(crate) fn stopped(repository: &str, refused: Refused) -> Outcome {
    match refused.limited() {
        true => Outcome::Limited(refused.detail),
        false => {
            tracing::warn!(repository, problem = %refused, "a repository could not be read");
            Outcome::Done
        }
    }
}

async fn repository(reading: &Reading<'_>, branch: Option<&str>) -> Result<Outcome, PluginError> {
    let backend = reading.backend;
    let name = reading.repository;
    let key = kept_key(name);
    let mut kept: Kept = backend
        .state_get(&key)
        .await?
        .and_then(|held| serde_json::from_value(held).ok())
        .unwrap_or_default();
    let started = Utc::now();
    let since = match kept.synced_at {
        Some(at) => at - Duration::hours(OVERLAP_HOURS),
        None => started - Duration::days(reading.settings.delivery.days),
    };
    let mut changed = None;

    for environment in &reading.settings.delivery.production {
        let path = format!("repos/{name}/deployments?environment={}", encoded(environment));
        let etag = format!("deployments:{environment}");
        let listed = reading
            .pages::<ListedDeployment>(&path, None, (&etag, &mut kept.etags), |d| {
                d.created_at < since
            })
            .await;
        let listed = match listed {
            Ok(listed) => listed.unwrap_or_default(),
            Err(refused) => return Ok(stopped(name, refused)),
        };
        for deployment in listed {
            if reading.out_of_time() {
                return Ok(Outcome::OutOfTime);
            }
            if let Err(refused) = deployed(reading, deployment, &mut changed).await? {
                return Ok(stopped(name, refused));
            }
        }
    }
    // Those still running at an earlier read, whose list may not have changed since.
    let running: Vec<Deployment> = backend
        .query_all(
            Query::new("deployments")
                .filter(json!({ "repository": name, "kind": "deployment", "state": RUNNING })),
        )
        .await?;
    for held in running {
        if reading.out_of_time() {
            return Ok(Outcome::OutOfTime);
        }
        let Some(number) = held.id.rsplit('/').next().and_then(|id| id.parse::<u64>().ok()) else {
            continue;
        };
        let again = ListedDeployment {
            id: number,
            sha: held.sha.clone(),
            branch: held.branch.clone(),
            environment: held.environment.clone(),
            created_at: held.started_at,
        };
        if let Err(refused) = deployed(reading, again, &mut changed).await? {
            return Ok(stopped(name, refused));
        }
    }

    if !reading.settings.delivery.workflows.is_empty() {
        // Listed afresh every time, so a change to which workflows deploy counts at once.
        let path = format!("repos/{name}/actions/workflows");
        let mut unkept = BTreeMap::new();
        let deploying: Vec<(u64, String)> = match reading
            .pages::<Workflow>(&path, Some("workflows"), ("workflows", &mut unkept), |_| false)
            .await
        {
            Ok(workflows) => workflows
                .unwrap_or_default()
                .into_iter()
                .filter(|w| reading.settings.delivery.deploys(name, &w.name, &w.path))
                .map(|w| (w.id, w.name))
                .collect(),
            // A repository with Actions turned off answers 404, and simply has no runs.
            Err(refused) if refused.status == Some(404) => Vec::new(),
            Err(refused) => return Ok(stopped(name, refused)),
        };
        let on = branch.map(|branch| format!("&branch={}", encoded(branch))).unwrap_or_default();
        for (id, workflow) in deploying {
            let path = format!("repos/{name}/actions/workflows/{id}/runs?status=completed{on}");
            let etag = format!("runs:{id}");
            let runs = reading
                .pages::<Run>(&path, Some("workflow_runs"), (&etag, &mut kept.etags), |run| {
                    run.created_at < since
                })
                .await;
            let runs = match runs {
                Ok(runs) => runs.unwrap_or_default(),
                Err(refused) => return Ok(stopped(name, refused)),
            };
            ran(reading, &workflow, runs, &mut changed).await?;
            if reading.out_of_time() {
                return Ok(Outcome::OutOfTime);
            }
        }
    }

    let path = format!("repos/{name}/pulls?state=closed&sort=updated&direction=desc");
    match reading
        .pages::<Pull>(&path, None, ("pulls", &mut kept.etags), |pull| pull.updated_at < since)
        .await
    {
        Ok(pulls) => merged(reading, pulls.unwrap_or_default(), &mut changed).await?,
        Err(refused) => return Ok(stopped(name, refused)),
    }

    match shipped(reading, &mut changed).await? {
        Ok(true) => {}
        Ok(false) => return Ok(Outcome::OutOfTime),
        Err(refused) => return Ok(stopped(name, refused)),
    }

    kept.synced_at = Some(started);
    backend.state_set(&key, json!(kept)).await?;
    if let Some(since) = changed {
        let topic = format!("plugin.{}.delivery.synced", reading.plugin);
        backend.publish(&topic, json!({ "repository": name, "since": since })).await?;
    }
    Ok(Outcome::Done)
}

/// One deployment from GitHub's Deployments API, at the state its statuses say it reached. One
/// already recorded as finished is left as it is, so its commits are never read twice.
async fn deployed(
    reading: &Reading<'_>,
    listed: ListedDeployment,
    changed: &mut Option<DateTime<Utc>>,
) -> Result<Result<(), Refused>, PluginError> {
    let backend = reading.backend;
    let name = reading.repository;
    let id = format!("{name}/deployments/{}", listed.id);
    if let Some(held) = backend.get::<Deployment>("deployments", id.as_str()).await?
        && held.state != RUNNING
    {
        return Ok(Ok(()));
    }
    let path = format!("repos/{name}/deployments/{}/statuses?per_page={PER_PAGE}", listed.id);
    let statuses: Vec<Status> = match reading.one(&path).await {
        Ok(statuses) => statuses,
        Err(refused) => return Ok(Err(refused)),
    };
    // The first time it succeeded is when it reached production; GitHub later marks a successful
    // deployment inactive once a newer one replaces it, which changes nothing about when it went.
    let succeeded =
        statuses.iter().filter(|status| status.state == SUCCESS).min_by_key(|s| s.created_at);
    let (state, finished_at, said) = match (succeeded, statuses.first()) {
        (Some(status), _) => (SUCCESS, Some(status.created_at), Some(status)),
        (None, Some(last)) if matches!(last.state.as_str(), "failure" | "error") => {
            (FAILURE, Some(last.created_at), Some(last))
        }
        (None, Some(last)) if last.state == INACTIVE => {
            (INACTIVE, Some(last.created_at), Some(last))
        }
        (None, last) => (RUNNING, None, last),
    };
    let url = said
        .and_then(|status| status.log_url.clone().or_else(|| status.target_url.clone()))
        .filter(|url| url.starts_with("https://") || url.starts_with("http://"))
        .or_else(|| {
            reading.settings.web.join(&format!("{name}/deployments")).ok().map(String::from)
        });
    let record = Deployment {
        id,
        repository: name.to_string(),
        environment: listed.environment,
        kind: "deployment".into(),
        state: state.into(),
        sha: listed.sha,
        branch: listed.branch,
        workflow: None,
        url,
        started_at: listed.created_at,
        finished_at,
        previous_sha: None,
        rollback: false,
        commits: Vec::new(),
        commits_resolved: false,
    };
    backend.upsert::<Value>("deployments", &["id"], json!(record)).await?;
    finished(reading, &record, changed).await?;
    Ok(Ok(()))
}

/// Successful and failed runs of a workflow that deploys; a cancelled or skipped run deployed
/// nothing, so it is not one.
async fn ran(
    reading: &Reading<'_>,
    workflow: &str,
    runs: Vec<Run>,
    changed: &mut Option<DateTime<Utc>>,
) -> Result<(), PluginError> {
    let backend = reading.backend;
    let name = reading.repository;
    let environment = reading
        .settings
        .delivery
        .production
        .first()
        .cloned()
        .unwrap_or_else(|| "production".into());
    for run in runs {
        let state = match run.conclusion.as_deref() {
            Some("success") => SUCCESS,
            Some("failure" | "timed_out" | "startup_failure") => FAILURE,
            _ => continue,
        };
        let id = format!("{name}/runs/{}", run.id);
        if backend.get::<Value>("deployments", id.as_str()).await?.is_some() {
            continue;
        }
        let record = Deployment {
            id,
            repository: name.to_string(),
            environment: environment.clone(),
            kind: "workflow".into(),
            state: state.into(),
            sha: run.head_sha,
            branch: run.head_branch,
            workflow: Some(workflow.to_string()),
            url: run.html_url,
            started_at: run.run_started_at.unwrap_or(run.created_at),
            finished_at: Some(run.updated_at),
            previous_sha: None,
            rollback: false,
            commits: Vec::new(),
            commits_resolved: false,
        };
        backend.upsert::<Value>("deployments", &["id"], json!(record)).await?;
        finished(reading, &record, changed).await?;
    }
    Ok(())
}

/// Announces a deployment that has finished, once for each state it finishes in.
async fn finished(
    reading: &Reading<'_>,
    record: &Deployment,
    changed: &mut Option<DateTime<Utc>>,
) -> Result<(), PluginError> {
    let Some(at) = record.finished_at.filter(|_| record.state != RUNNING) else { return Ok(()) };
    earliest(changed, at);
    let topic = format!("plugin.{}.deployment.recorded", reading.plugin);
    let payload = json!({
        "deployment": record.id,
        "repository": record.repository,
        "environment": record.environment,
        "kind": record.kind,
        "state": record.state,
        "sha": record.sha,
        "url": record.url,
        "finished_at": at,
    });
    let key = format!("deployment:{}:{}", record.id, record.state);
    reading.backend.publish_once(&topic, payload, &key).await?;
    Ok(())
}

/// Merged pull requests, written in batches; each is announced once, the first time it is seen.
async fn merged(
    reading: &Reading<'_>,
    pulls: Vec<Pull>,
    changed: &mut Option<DateTime<Utc>>,
) -> Result<(), PluginError> {
    let name = reading.repository;
    let pulls: Vec<(Pull, DateTime<Utc>)> =
        pulls.into_iter().filter_map(|pull| pull.merged_at.map(|at| (pull, at))).collect();
    for chunk in pulls.chunks(BATCH) {
        let writes: Vec<DataRequest> = chunk
            .iter()
            .map(|(pull, at)| {
                let values = json!({
                    "id": format!("{name}#{}", pull.number),
                    "repository": name,
                    "number": pull.number,
                    "title": pull.title,
                    "labels": pull.labels.iter().map(|label| label.name.clone()).collect::<Vec<_>>(),
                    "merged_at": at,
                    "merge_sha": pull.merge_commit_sha,
                    "url": pull.html_url,
                });
                DataRequest::upsert("pull-requests", &["id"], values)
            })
            .collect();
        let answers = reading.backend.batch(writes).await?;
        for ((pull, at), answer) in chunk.iter().zip(answers) {
            earliest(changed, *at);
            if answer.created != Some(true) {
                continue;
            }
            let topic = format!("plugin.{}.pull-request.merged", reading.plugin);
            let payload = json!({
                "repository": name,
                "number": pull.number,
                "title": pull.title,
                "labels": pull.labels.iter().map(|label| label.name.clone()).collect::<Vec<_>>(),
                "url": pull.html_url,
                "merged_at": at,
                "base": pull.base.as_ref().map(|base| base.branch.as_str()),
                "merge_sha": pull.merge_commit_sha,
            });
            let key = format!("pull:{name}#{}", pull.number);
            reading.backend.publish_once(&topic, payload, &key).await?;
        }
    }
    Ok(())
}

/// What each successful deployment shipped: the commits since the deployment before it to the
/// same environment, in the order they were deployed. Deploying a commit behind the one before is
/// a rollback, and shipped nothing new. `Ok(false)` means the time ran out first.
async fn shipped(
    reading: &Reading<'_>,
    changed: &mut Option<DateTime<Utc>>,
) -> Result<Result<bool, Refused>, PluginError> {
    let backend = reading.backend;
    let name = reading.repository;
    let pending: Vec<Deployment> = backend
        .query_all(
            Query::new("deployments")
                .filter(json!({ "repository": name, "state": SUCCESS, "commits_resolved": false }))
                .order(Order::asc("finished_at")),
        )
        .await?;
    for deployment in pending {
        if reading.out_of_time() {
            return Ok(Ok(false));
        }
        let Some(at) = deployment.finished_at else { continue };
        let before = backend
            .query::<Deployment>(
                Query::new("deployments")
                    .filter(json!({
                        "repository": name,
                        "environment": deployment.environment,
                        "kind": deployment.kind,
                        "state": SUCCESS,
                        "finished_at": { "lt": at },
                    }))
                    .order(Order::desc("finished_at"))
                    .limit(1),
            )
            .await?
            .records
            .into_iter()
            .next();
        let (commits, rollback) = match &before {
            Some(before) if before.sha == deployment.sha => (Vec::new(), false),
            Some(before) => {
                let path = format!("repos/{name}/compare/{}...{}", before.sha, deployment.sha);
                match reading.one::<Compared>(&path).await {
                    Ok(compared) if compared.status == "behind" => (Vec::new(), true),
                    Ok(compared) => (
                        compared
                            .commits
                            .into_iter()
                            .filter_map(Listed::commit)
                            .take(MAX_COMMITS)
                            .collect(),
                        false,
                    ),
                    // A commit force-pushed away cannot be compared: what it shipped stays unknown.
                    Err(refused) if refused.status == Some(404) => (Vec::new(), false),
                    Err(refused) => return Ok(Err(refused)),
                }
            }
            None => match reading
                .one::<Listed>(&format!("repos/{name}/commits/{}", deployment.sha))
                .await
            {
                Ok(listed) => (listed.commit().into_iter().collect(), false),
                Err(refused) if refused.status == Some(404) => (Vec::new(), false),
                Err(refused) => return Ok(Err(refused)),
            },
        };
        let set = json!({
            "commits": commits,
            "commits_resolved": true,
            "rollback": rollback,
            "previous_sha": before.map(|before| before.sha),
        });
        backend.update::<Value>("deployments", deployment.id.as_str(), set, None).await?;
        earliest(changed, at);
    }
    Ok(Ok(true))
}

/// A webhook from GitHub, checked against the secret; the repository it is about is then read
/// again in the background. The payload is only ever a nudge: what is recorded is what GitHub
/// answers when asked, so a replayed delivery can make the plugin read, never write.
pub async fn webhook(backend: &Backend, settings: &Settings, request: &Request) -> Response {
    let listening = settings.delivery.on || settings.pipelines.on;
    let Some(secret) = settings.delivery.webhook_secret.as_ref().filter(|_| listening) else {
        return Response::not_found();
    };
    let given = request
        .headers
        .get("x-hub-signature-256")
        .and_then(|given| given.trim().strip_prefix("sha256="))
        .and_then(|given| hex::decode(given).ok());
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret.expose().as_bytes());
    if !given.is_some_and(|given| ring::hmac::verify(&key, &request.body, &given).is_ok()) {
        return Response::problem(401, "unauthorised", "the signature does not match the body");
    }
    let event = request.headers.get("x-github-event").map(String::as_str).unwrap_or_default();
    if event == "ping" {
        return Response::json(&json!({ "pong": true }));
    }
    let Ok(payload) = serde_json::from_slice::<Value>(&request.body) else {
        return Response::problem(400, "bad-request", "the body is not JSON");
    };
    let action = payload["action"].as_str().unwrap_or_default();
    let delivery = settings.delivery.on
        && match event {
            "deployment_status" => true,
            "workflow_run" => action == "completed",
            "pull_request" => {
                action == "closed" && payload["pull_request"]["merged"] == json!(true)
            }
            _ => false,
        };
    let pipelines = settings.pipelines.on && event == "workflow_run" && action == "completed";
    let repository = &payload["repository"];
    let name = repository["full_name"].as_str().unwrap_or_default();
    let org = name.split('/').next().unwrap_or_default().to_string();
    let topics: Vec<String> = repository["topics"]
        .as_array()
        .map(|topics| topics.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default();
    if !(delivery || pipelines)
        || !settings.allows(&[org])
        || !settings.delivery.reads(name, &topics)
    {
        return Response::new(202, "application/json", json!({ "read": false }).to_string());
    }
    let queued = Queued {
        name: name.to_string(),
        branch: repository["default_branch"].as_str().map(str::to_string),
    };
    let mut tasks = Vec::new();
    for (read, on) in [("delivery", delivery), ("pipelines", pipelines)] {
        if !on {
            continue;
        }
        let payload = json!({ read: { "repositories": [&queued], "whole": false } });
        match backend.task(payload).await {
            Ok(task) => tasks.push(task),
            Err(err) => return Response::problem(503, "unavailable", &err.to_string()),
        }
    }
    Response::new(202, "application/json", json!({ "read": true, "tasks": tasks }).to_string())
}

/// How delivery data stands, for `dora` to say on its own page why it has nothing yet.
pub async fn status(backend: &Backend, settings: &Settings) -> Value {
    let synced = backend.state_get("delivery/synced").await.ok().flatten();
    json!({
        "on": settings.delivery.on,
        "problem": settings.delivery_problem(),
        "webhooks": settings.delivery.webhook_secret.is_some(),
        "synced": synced,
    })
}

/// The first read after the feature comes on, rather than up to an hour later at the schedule;
/// asked for once, and the schedule carries on from there.
pub async fn begin(backend: &Backend, settings: &Settings) {
    if settings.delivery_problem().is_some() {
        return;
    }
    let begun = backend.state_get("delivery/begun").await.ok().flatten();
    let synced = backend.state_get("delivery/synced").await.ok().flatten();
    if begun.is_some() || synced.is_some() {
        return;
    }
    match backend.task(json!({ "schedule": "delivery" })).await {
        Ok(task) => {
            let _ = backend
                .state_set("delivery/begun", json!({ "at": Utc::now(), "task": task }))
                .await;
            tracing::info!(%task, "the first read of delivery data is queued");
        }
        Err(err) => tracing::warn!(%err, "the first read of delivery data could not be queued"),
    }
}
