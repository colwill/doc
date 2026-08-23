//! What the plugin keeps: a record per repository, which is also its place in the queue, and each
//! scan of it. However many merges arrive while one is waiting or being scanned, a repository is
//! scanned once more, not once per merge.

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, Collection, Declaration, Export, Field, Order, PluginError, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::report::Packages;

pub const REPOSITORIES: &str = "repositories";
pub const SCANS: &str = "scans";
pub const PACKAGES: &str = "packages";

pub const QUEUED: &str = "queued";
pub const SCANNING: &str = "scanning";
pub const SCANNED: &str = "scanned";
pub const FAILED: &str = "failed";

/// How many times a record that changed under a write is read and written again.
const ATTEMPTS: usize = 5;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            REPOSITORIES,
            Collection::new()
                .field("id", Field::text().key().describe("source/owner/name, in lower case"))
                .field("source", Field::text().required())
                .field("repository", Field::text().required())
                .field("branch", Field::text().required())
                .field(
                    "state",
                    Field::text().required().one_of(&[QUEUED, SCANNING, SCANNED, FAILED]),
                )
                .field("wanted_at", Field::timestamp())
                .field("why", Field::text())
                .field("trigger", Field::json())
                .field(
                    "again",
                    Field::boolean()
                        .required()
                        .default(json!(false))
                        .describe("Something was merged while it was being scanned"),
                )
                .field("started_at", Field::timestamp())
                .field("finished_at", Field::timestamp())
                .field(
                    "through",
                    Field::timestamp()
                        .describe("When the archive the latest scan read was fetched: merges before it are in it"),
                )
                .field("commit", Field::text())
                .field("web", Field::text())
                .field("latest", Field::text())
                .field("summary", Field::json())
                .field("problem", Field::text())
                .field("failures", Field::integer().required().default(json!(0)))
                .index(&["state", "wanted_at"])
                .index(&["wanted_at"])
                .index(&["finished_at"])
                .search(&["repository"]),
        )
        .collection(
            SCANS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("repository_id", Field::text().required())
                .field("source", Field::text().required())
                .field("repository", Field::text().required())
                .field("branch", Field::text().required())
                .field("commit", Field::text().required())
                .field("base_commit", Field::text())
                .field("why", Field::text())
                .field("trigger", Field::json())
                .field("started_at", Field::timestamp().required())
                .field("finished_at", Field::timestamp().required())
                .field("took_ms", Field::integer())
                .field("ccc", Field::text())
                .field("summary", Field::json().required())
                .field("report", Field::json().required())
                .index(&["repository_id", "finished_at"]),
        )
        // What End of life judges a repository's services by. A package list is no more than
        // anybody reading the repository sees, and carries no advisories, so it is not kept
        // behind the `security` permission.
        .collection(
            PACKAGES,
            Collection::new()
                .field("id", Field::text().key().describe("Its repository's: source/owner/name"))
                .field("source", Field::text().required())
                .field("repository", Field::text().required())
                .field("commit", Field::text().required().describe("The commit they were read at"))
                .field("listed_at", Field::timestamp().required())
                .field(
                    "packages",
                    Field::json().required().default(json!([])).describe(
                        "Each package ccc audit resolved from a lockfile: ecosystem, name, \
                         version, direct, dev and lockfile",
                    ),
                )
                .field("total", Field::integer().required().default(json!(0)))
                .index(&["listed_at"])
                .export(Export::to(&["eol"])),
        )
}

/// A repository's record, which says where it stands and what its latest scan found.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Repository {
    pub id: String,
    pub source: String,
    pub repository: String,
    pub branch: String,
    pub state: String,
    #[serde(default)]
    pub wanted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub trigger: Option<Value>,
    #[serde(default)]
    pub again: bool,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub through: Option<DateTime<Utc>>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub web: Option<String>,
    #[serde(default)]
    pub latest: Option<String>,
    #[serde(default)]
    pub summary: Option<Value>,
    #[serde(default)]
    pub problem: Option<String>,
    #[serde(default)]
    pub failures: i64,
    #[serde(default, rename = "_version")]
    pub version: Option<i64>,
}

impl Repository {
    /// Its page: `/p/insights/r/<source>/<owner>/<name>`.
    pub fn href(&self) -> String {
        format!("/p/insights/r/{}", self.id)
    }
}

/// One scan, and the report the pages are drawn from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scan {
    pub id: Uuid,
    pub repository_id: String,
    pub source: String,
    pub repository: String,
    pub branch: String,
    pub commit: String,
    #[serde(default)]
    pub base_commit: Option<String>,
    #[serde(default)]
    pub why: Option<String>,
    #[serde(default)]
    pub trigger: Option<Value>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    #[serde(default)]
    pub took_ms: Option<i64>,
    #[serde(default)]
    pub ccc: Option<String>,
    pub summary: Value,
    #[serde(default)]
    pub report: Value,
}

/// A repository as the plugin names it: which source it comes from, and `owner/name` there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Named {
    pub source: String,
    pub repository: String,
}

impl Named {
    /// `owner/name`, with nothing else in it that could climb out of a path or a URL.
    pub fn new(source: &str, repository: &str) -> Result<Self, String> {
        let source = source.trim().to_ascii_lowercase();
        let repository = repository.trim().trim_end_matches(".git").trim_matches('/');
        let plain = |part: &str| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        };
        let valid_source = !source.is_empty()
            && source.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        match repository.split_once('/') {
            Some((owner, name)) if valid_source && plain(owner) && plain(name) => {
                Ok(Self { source, repository: format!("{owner}/{name}") })
            }
            _ if !valid_source => Err(format!("`{source}` is not a plugin ID")),
            _ => Err(format!("`{repository}` is not written owner/name")),
        }
    }

    pub fn id(&self) -> String {
        format!("{}/{}", self.source, self.repository).to_ascii_lowercase()
    }
}

/// Why a repository is wanted, and when what caused it happened.
pub struct Wanted {
    pub why: String,
    pub trigger: Option<Value>,
    pub at: Option<DateTime<Utc>>,
}

/// What asking for a scan came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Asked {
    /// It will be scanned.
    Queued,
    /// It was already waiting to be.
    Waiting,
    /// It is being scanned, and will be scanned once more after.
    Again,
    /// The latest scan read the repository after what caused this happened.
    Covered,
}

impl Asked {
    pub fn said(self) -> &'static str {
        match self {
            Self::Queued => "It is queued to be scanned.",
            Self::Waiting => "It was already waiting to be scanned.",
            Self::Again => "It is being scanned now, and will be scanned once more after that.",
            Self::Covered => "Its latest scan already includes that.",
        }
    }
}

pub async fn repository(backend: &Backend, id: &str) -> Result<Option<Repository>, PluginError> {
    backend.get::<Repository>(REPOSITORIES, id).await
}

pub async fn scan(backend: &Backend, id: &str) -> Result<Option<Scan>, PluginError> {
    match id.parse::<Uuid>() {
        Ok(id) => backend.get::<Scan>(SCANS, id.to_string()).await,
        Err(_) => Ok(None),
    }
}

/// Every repository, most recently scanned first.
pub async fn repositories(backend: &Backend) -> Result<Vec<Repository>, PluginError> {
    let mut all: Vec<Repository> = backend.query_all(Query::new(REPOSITORIES)).await?;
    all.sort_by(|a, b| {
        b.finished_at.cmp(&a.finished_at).then_with(|| a.repository.cmp(&b.repository))
    });
    Ok(all)
}

/// A repository's scans, newest first, without their reports.
pub async fn history(backend: &Backend, id: &str, limit: u32) -> Result<Vec<Scan>, PluginError> {
    let fields = [
        "id",
        "repository_id",
        "source",
        "repository",
        "branch",
        "commit",
        "base_commit",
        "why",
        "started_at",
        "finished_at",
        "took_ms",
        "ccc",
        "summary",
    ];
    let page = backend
        .query::<Scan>(
            Query::new(SCANS)
                .filter(json!({ "repository_id": id }))
                .order(Order::desc("_created_at"))
                .fields(&fields)
                .limit(limit),
        )
        .await?;
    Ok(page.records)
}

/// Asks for `named` to be scanned. A merge the latest scan already read changes nothing, and one
/// that arrives while a scan is going has it scanned once more afterwards.
pub async fn want(
    backend: &Backend,
    named: &Named,
    branch: &str,
    wanted: Wanted,
) -> Result<Asked, PluginError> {
    let id = named.id();
    let now = Utc::now();
    for _ in 0..ATTEMPTS {
        let Some(held) = repository(backend, &id).await? else {
            let record = json!({
                "id": id,
                "source": named.source,
                "repository": named.repository,
                "branch": branch,
                "state": QUEUED,
                "wanted_at": now,
                "why": wanted.why,
                "trigger": wanted.trigger,
            });
            match backend.insert::<Value>(REPOSITORIES, record).await {
                Ok(_) => return Ok(Asked::Queued),
                Err(err) if err.is_duplicate() => continue,
                Err(err) => return Err(err),
            }
        };
        if let (Some(at), Some(through)) = (wanted.at, held.through)
            && at <= through
        {
            return Ok(Asked::Covered);
        }
        let (set, asked) = match held.state.as_str() {
            QUEUED => {
                // The newest reason is the one shown; an older merge arriving late changes nothing.
                let older = wanted.at.is_some_and(|at| {
                    trigger_at(held.trigger.as_ref()).is_some_and(|held_at| at <= held_at)
                });
                if older {
                    return Ok(Asked::Waiting);
                }
                (
                    json!({ "why": wanted.why, "trigger": wanted.trigger, "branch": branch }),
                    Asked::Waiting,
                )
            }
            SCANNING => (
                json!({ "again": true, "why": wanted.why, "trigger": wanted.trigger }),
                Asked::Again,
            ),
            _ => (
                json!({
                    "state": QUEUED,
                    "wanted_at": now,
                    "why": wanted.why,
                    "trigger": wanted.trigger,
                    "branch": branch,
                }),
                Asked::Queued,
            ),
        };
        match backend.update::<Value>(REPOSITORIES, id.as_str(), set, held.version).await {
            Ok(_) => return Ok(asked),
            Err(err) if err.is_version_conflict() => continue,
            Err(err) => return Err(err),
        }
    }
    Err(PluginError::from(format!("{id} kept changing; try again")))
}

fn trigger_at(trigger: Option<&Value>) -> Option<DateTime<Utc>> {
    trigger?["merged_at"]
        .as_str()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
}

/// Takes the repository that has waited longest, if any, marking it as being scanned.
pub async fn claim(backend: &Backend) -> Result<Option<Repository>, PluginError> {
    for _ in 0..ATTEMPTS {
        let waiting = backend
            .query::<Repository>(
                Query::new(REPOSITORIES)
                    .filter(json!({ "state": QUEUED }))
                    .order(Order::asc("wanted_at"))
                    .limit(1),
            )
            .await?;
        let Some(next) = waiting.records.into_iter().next() else { return Ok(None) };
        let set = json!({ "state": SCANNING, "started_at": Utc::now(), "again": false });
        match backend.update::<Repository>(REPOSITORIES, next.id.as_str(), set, next.version).await
        {
            Ok(Some(claimed)) => return Ok(Some(claimed)),
            Ok(None) => continue,
            Err(err) if err.is_version_conflict() => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(None)
}

/// Scans that were going when their process stopped are queued again, once they have been going
/// for longer than any scan may.
pub async fn recover(backend: &Backend, longest: Duration) -> Result<usize, PluginError> {
    let stale: Vec<Repository> =
        backend.query_all(Query::new(REPOSITORIES).filter(json!({ "state": SCANNING }))).await?;
    let cutoff = Utc::now() - longest;
    let mut recovered = 0;
    for held in stale.into_iter().filter(|held| held.started_at.is_none_or(|at| at < cutoff)) {
        let set = json!({ "state": QUEUED });
        match backend.update::<Value>(REPOSITORIES, held.id.as_str(), set, held.version).await {
            Ok(_) => recovered += 1,
            Err(err) if err.is_version_conflict() => {}
            Err(err) => return Err(err),
        }
    }
    Ok(recovered)
}

/// How a scan ended.
pub enum Ended {
    Scanned(Box<Scan>, Found),
    /// The branch was at the commit the latest scan already read. Its packages are listed all the
    /// same where they never were, for a repository scanned before they were kept.
    Unchanged {
        through: DateTime<Utc>,
        packages: Option<Packages>,
    },
    Failed(String),
    /// The plugin was cancelled or unloaded while it was going.
    Stopped,
}

/// What a finished scan leaves on its repository besides the scan itself.
pub struct Found {
    pub through: DateTime<Utc>,
    pub web: Option<String>,
    pub packages: Option<Packages>,
}

/// Whether a repository's packages have been listed at all.
pub async fn listed(backend: &Backend, id: &str) -> Result<bool, PluginError> {
    Ok(backend.get::<Value>(PACKAGES, id).await?.is_some())
}

/// Repositories scanned before their packages were kept, which are scanned once more to list them.
pub async fn unlisted(backend: &Backend) -> Result<Vec<Repository>, PluginError> {
    let listed: Vec<Value> = backend.query_all(Query::new(PACKAGES).fields(&["id"])).await?;
    let listed: std::collections::BTreeSet<&str> =
        listed.iter().filter_map(|held| held["id"].as_str()).collect();
    let scanned: Vec<Repository> =
        backend.query_all(Query::new(REPOSITORIES).filter(json!({ "state": SCANNED }))).await?;
    Ok(scanned
        .into_iter()
        .filter(|held| held.latest.is_some() && !listed.contains(held.id.as_str()))
        .collect())
}

/// Keeps what a scan found a repository's packages to be, for End of life.
async fn keep_packages(
    backend: &Backend,
    claimed: &Repository,
    commit: &str,
    packages: &Packages,
) -> Result<(), PluginError> {
    let record = json!({
        "id": claimed.id,
        "source": claimed.source,
        "repository": claimed.repository,
        "commit": commit,
        "listed_at": Utc::now(),
        "packages": packages.listed,
        "total": packages.total,
    });
    backend.upsert::<Value>(PACKAGES, &["id"], record).await?;
    Ok(())
}

/// Records how a scan ended on its repository, which is queued again if something was merged
/// while it was going.
pub async fn finish(
    backend: &Backend,
    claimed: &Repository,
    ended: &Ended,
    keep: usize,
) -> Result<(), PluginError> {
    let now = Utc::now();
    if let Ended::Scanned(scan, _) = ended {
        backend.insert::<Value>(SCANS, json!(scan)).await?;
    }
    let changed = backend
        .change(REPOSITORIES, claimed.id.as_str(), |current| {
            let again = current.get("again").and_then(Value::as_bool).unwrap_or_default();
            let failures = current.get("failures").and_then(Value::as_i64).unwrap_or_default();
            let next = if again { QUEUED } else { SCANNED };
            let mut set = match ended {
                Ended::Scanned(scan, found) => json!({
                    "state": next,
                    "finished_at": now,
                    "through": found.through,
                    "commit": scan.commit,
                    "web": found.web,
                    "latest": scan.id,
                    "summary": scan.summary,
                    "problem": null,
                    "failures": 0,
                }),
                Ended::Unchanged { through, .. } => json!({
                    "state": next,
                    "finished_at": now,
                    "through": through,
                    "problem": null,
                    "failures": 0,
                }),
                Ended::Failed(problem) => json!({
                    "state": if again { QUEUED } else { FAILED },
                    "finished_at": now,
                    "problem": problem,
                    "failures": failures + 1,
                }),
                Ended::Stopped => json!({ "state": QUEUED }),
            };
            if again {
                set["again"] = json!(false);
                set["wanted_at"] = json!(now);
            }
            Some(set)
        })
        .await?;
    if changed.is_none() {
        tracing::warn!(repository = %claimed.id, "the repository went away while it was scanned");
    }
    let packages = match ended {
        Ended::Scanned(scan, found) => {
            found.packages.as_ref().map(|found| (scan.commit.as_str(), found))
        }
        Ended::Unchanged { packages, .. } => {
            packages.as_ref().zip(claimed.commit.as_deref()).map(|(found, commit)| (commit, found))
        }
        _ => None,
    };
    if let Some((commit, packages)) = packages {
        keep_packages(backend, claimed, commit, packages).await?;
    }
    if matches!(ended, Ended::Scanned(..)) {
        prune(backend, &claimed.id, keep).await?;
    }
    Ok(())
}

/// Keeps a repository's newest `keep` scans and deletes the rest.
async fn prune(backend: &Backend, id: &str, keep: usize) -> Result<(), PluginError> {
    let held = backend
        .query::<Value>(
            Query::new(SCANS)
                .filter(json!({ "repository_id": id }))
                .order(Order::desc("_created_at"))
                .fields(&["id"])
                .limit(1_000),
        )
        .await?;
    for old in held.records.iter().skip(keep.max(1)) {
        if let Some(scan) = old["id"].as_str() {
            backend.delete(SCANS, scan, None).await?;
        }
    }
    Ok(())
}

/// Queues again every repository from `source` whose last scan failed, once what they failed for
/// has changed, such as an administrator approving this plugin's archive links there.
pub async fn requeue_failed(
    backend: &Backend,
    source: &str,
    why: &str,
) -> Result<usize, PluginError> {
    let failed: Vec<Repository> = backend
        .query_all(Query::new(REPOSITORIES).filter(json!({ "state": FAILED, "source": source })))
        .await?;
    let mut queued = 0;
    for held in failed {
        let set = json!({ "state": QUEUED, "wanted_at": Utc::now(), "why": why });
        match backend.update::<Value>(REPOSITORIES, held.id.as_str(), set, held.version).await {
            Ok(_) => queued += 1,
            Err(err) if err.is_version_conflict() => {}
            Err(err) => return Err(err),
        }
    }
    Ok(queued)
}

/// Forgets a repository, every scan of it and its packages.
pub async fn forget(backend: &Backend, id: &str) -> Result<bool, PluginError> {
    backend.delete_where(SCANS, "id", json!({ "repository_id": id })).await?;
    backend.delete(PACKAGES, id, None).await?;
    backend.delete(REPOSITORIES, id, None).await
}
