//! What the repositories connected to a service are built on. A repository is the honest answer
//! to "what could go end of life here": a service's `endoflife.date/products` metadata has to be
//! kept up by hand, and a lockfile does not.
//!
//! Two things are read. The packages Repository Insights listed with ccc at its last scan, which
//! costs nothing here but a read of what it exports; and, with **Runtimes from repository files**
//! on, the files ccc does not read — Dockerfiles, `go.mod`, `.nvmrc` and the rest — which costs a
//! fetch of the repository's archive, so a repository is fetched again only once the Catalogue says
//! it has been pushed to since. Both happen on a schedule rather than when a page is drawn, and
//! what was found is kept per repository; Insights finishing a scan has its packages read at once.

use std::collections::BTreeMap;
use std::io::Read;
use std::time::Duration;

use chrono::{DateTime, Utc};
use doc_plugin_sdk::telemetry::sent;
use doc_plugin_sdk::{Backend, DataRequest, PluginError, Query};
use serde_json::{Value, json};

use crate::manifests::{self, Found};
use crate::packages::{self, Listed};
use crate::scope::encoded;
use crate::settings::{Definitions, LIFECYCLE, MOST_READ, REPOSITORIES};
use crate::store::Scanned;

/// The largest archive fetched. A repository bigger than this is not read, rather than being let
/// through: the point is a handful of manifests, not the repository.
const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
/// The most the files kept from one archive may come to.
const MAX_KEPT: u64 = 8 * 1024 * 1024;
/// The source plugin's setting naming which plugins it hands archive links to.
const ARCHIVE_SETTING: &str = "archive-plugins";
/// How long a source plugin is given to answer for a link, and the fetch itself.
const ASKING: Duration = Duration::from_secs(20);
const FETCHING: Duration = Duration::from_secs(90);
/// Repositories fetched in one attempt at the run, which has thirty seconds; the rest are queued.
const PER_ATTEMPT: usize = 4;
/// Repositories whose attachment is refreshed in one attempt, which costs one call each.
const ATTACHMENTS_PER_ATTEMPT: usize = 60;
/// Repositories whose packages are matched to products in one attempt: each is one read of a list
/// that can run to thousands.
const LISTS_PER_ATTEMPT: usize = 20;

/// An archive link for a repository, from the first source plugin that gives one.
async fn link(
    backend: &Backend,
    sources: &[String],
    repository: &str,
    reference: &str,
) -> Result<String, String> {
    let mut said = Vec::new();
    for source in sources {
        let asked = json!({ "repository": repository, "ref": reference });
        let answered = tokio::time::timeout(
            ASKING,
            backend.discovery(source, "POST", "archive-links", None, Some(asked)),
        )
        .await;
        let (status, answer) = match answered {
            Ok(Ok(answered)) => answered,
            Ok(Err(err)) => {
                said.push(format!("{source}: {}", err.detail()));
                continue;
            }
            Err(_) => {
                said.push(format!("{source} did not answer in time"));
                continue;
            }
        };
        match status {
            200 => match answer["url"].as_str().filter(|url| !url.is_empty()) {
                Some(url) => return Ok(url.to_string()),
                None => said.push(format!("{source} gave an empty link")),
            },
            // Asked too often: it is not this repository that is wrong, and the next run will do.
            429 => return Err(format!("{source} asked to be left a while: {}", detail(&answer))),
            403 => said.push(refused(backend, source).await),
            status => said.push(format!("{source} answered {status}: {}", detail(&answer))),
        }
    }
    Err(match said.is_empty() {
        true => "no plugin is named to give archives: name one on the Settings page".to_string(),
        false => said.join("; "),
    })
}

/// The source plugin does not give this one archive links. Whoever administers it is asked to
/// approve, as Repository Insights asks, rather than the run simply failing every night.
async fn refused(backend: &Backend, source: &str) -> String {
    let reason = "End of life reads a repository's own files — Dockerfiles, go.mod, .nvmrc and \
                  the rest — to work out what the services connected to it are built on, and \
                  which of those are past their end of life.";
    let asked = backend.request_access(source, ARCHIVE_SETTING, reason).await;
    let manually =
        format!("add eol to \"Plugins given archive links\" on {source}'s Settings page");
    match asked {
        Ok(answer) if answer.state == "pending" => format!(
            "{source} does not give this plugin archive links yet: whoever administers it has been \
             asked to approve, and reading starts once they do"
        ),
        Ok(answer) if answer.state == "denied" => format!(
            "{source} does not give this plugin archive links: {} denied the request; {manually} \
             to read anyway",
            answer.decided_by.as_deref().unwrap_or("an administrator")
        ),
        Ok(_) => {
            format!("{source} gives this plugin archive links now; it is read on the next run")
        }
        Err(err) => {
            format!(
                "{source} does not give this plugin archive links: {manually} ({})",
                err.detail()
            )
        }
    }
}

fn detail(answer: &Value) -> String {
    answer["detail"].as_str().unwrap_or("it gave no reason").to_string()
}

/// The archive, held in memory only as long as it takes to pick the manifests out of it.
async fn fetch(url: &str) -> Result<Vec<u8>, String> {
    let client = crate::lifecycle::client().map_err(|err| err.to_string())?;
    let answer = tokio::time::timeout(FETCHING, client.get(url).send())
        .await
        .map_err(|_| "the archive was not fetched in time".to_string())?;
    sent("archive", "fetch", &answer);
    let answer = answer.map_err(|err| format!("the archive could not be fetched: {err}"))?;
    if !answer.status().is_success() {
        return Err(format!("the archive link answered {}", answer.status()));
    }
    if answer.content_length().is_some_and(|length| length > MAX_ARCHIVE) {
        return Err(format!("the archive is over {} MiB", MAX_ARCHIVE / 1024 / 1024));
    }
    let bytes = answer.bytes().await.map_err(|err| format!("the archive stopped short: {err}"))?;
    if bytes.len() as u64 > MAX_ARCHIVE {
        return Err(format!("the archive is over {} MiB", MAX_ARCHIVE / 1024 / 1024));
    }
    Ok(bytes.to_vec())
}

/// Only the files the manifests are read from, without their top directory. Everything else in the
/// archive is stepped over rather than held, so a large repository costs little.
fn manifests_in(gzipped: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(gzipped));
    let entries = archive.entries().map_err(|err| format!("the archive is unreadable: {err}"))?;
    let (mut kept, mut total) = (BTreeMap::new(), 0u64);
    for entry in entries {
        let mut entry = entry.map_err(|err| format!("the archive is unreadable: {err}"))?;
        if !entry.header().entry_type().is_file() {
            continue;
        }
        let path = match entry.path() {
            Ok(path) => path.to_string_lossy().into_owned(),
            Err(_) => continue,
        };
        if path.contains("..") {
            continue;
        }
        // GitHub's tarballs put everything under one directory named for the commit.
        let inside = path.split_once('/').map_or(path.as_str(), |(_, rest)| rest).to_string();
        if inside.is_empty() || !manifests::wanted(&inside) {
            continue;
        }
        total += entry.size();
        if total > MAX_KEPT {
            return Err(format!("its manifests come to more than {} MiB", MAX_KEPT / 1024 / 1024));
        }
        let mut bytes = Vec::new();
        if entry.read_to_end(&mut bytes).is_ok() {
            kept.insert(inside, bytes);
        }
    }
    Ok(kept)
}

/// Every repository the Catalogue holds, with when it was last pushed to and its default branch.
/// The Catalogue's own setting naming the plugins that read it as themselves.
const CATALOGUE_READERS: &str = "reader-plugins";

/// One of the Catalogue's `discovery/` routes, read as this plugin: the nightly read and the events it
/// hears are for nobody, and the Catalogue's `api/` routes answer only whoever a call is for.
/// Where the Catalogue does not list this plugin, its administrators are asked to.
async fn catalogue(backend: &Backend, route: &str, query: &str) -> Result<(u16, Value), String> {
    let answered = backend
        .discovery("resources", "GET", route, Some(query), None)
        .await
        .map_err(|err| format!("the Catalogue could not be asked: {}", err.detail()))?;
    if answered.0 != 403 {
        return Ok(answered);
    }
    let reason = "End of life finds which services each repository belongs to, so that what \
                  Repository Insights found in a repository is judged for the services that run it. \
                  It does so overnight and whenever a scan or a connection changes, when nobody is \
                  looking.";
    let manually = "add eol to \"Plugins that read the catalogue as themselves\" on the Catalogue's \
                    Settings page";
    Err(match backend.request_access("resources", CATALOGUE_READERS, reason).await {
        Ok(answer) if answer.state == "pending" => {
            "the Catalogue does not let this plugin read it \
             yet: whoever administers it has been asked to approve, and reading starts once they do"
                .to_string()
        }
        Ok(answer) if answer.state == "denied" => format!(
            "the Catalogue does not let this plugin read it: {} denied the request; {manually} to \
             read anyway",
            answer.decided_by.as_deref().unwrap_or("an administrator")
        ),
        Ok(_) => "the Catalogue lets this plugin read it now; it is read on the next run".into(),
        Err(err) => {
            format!("the Catalogue does not let this plugin read it: {manually} ({})", err.detail())
        }
    })
}

async fn listed(backend: &Backend) -> Result<Vec<Value>, PluginError> {
    let query = encoded(&[("kind", "repository"), ("limit", &MOST_READ.to_string())]);
    match catalogue(backend, "resources", &query).await.map_err(PluginError::from)? {
        (200, Value::Array(listed)) => Ok(listed),
        (status, body) => {
            Err(PluginError::from(format!("the Catalogue answered {status}: {}", detail(&body))))
        }
    }
}

/// The services a repository is connected to in the Catalogue, or why the Catalogue would not
/// say. A refusal is not "none": taken for one, it would drop what every repository was found to
/// use.
pub async fn services_of(backend: &Backend, repository: &str) -> Result<Vec<String>, String> {
    let query = encoded(&[("of", &format!("repository:{repository}"))]);
    let body = match catalogue(backend, "neighbours", &query).await? {
        (200, body) => body,
        // Not in the Catalogue at all: connected to nothing.
        (404, _) => return Ok(Vec::new()),
        (status, body) => {
            return Err(format!("the Catalogue answered {status}: {}", detail(&body)));
        }
    };
    Ok(body["neighbours"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|neighbour| {
            neighbour["kind"].as_str().is_some_and(|kind| kind.eq_ignore_ascii_case("service"))
        })
        .filter_map(|neighbour| neighbour["name"].as_str().map(str::to_string))
        .collect())
}

/// The Catalogue saying something in it changed.
pub const CATALOGUE_CHANGED: &str = "plugin.resources.changed";
/// How long the Catalogue is given to stop changing before every repository is read again. One
/// apply or sync writes many things at once, and the first of them starts the wait.
const SETTLE: Duration = Duration::from_secs(120);
/// When the last read of every repository finished.
const READ_AT: &str = "repositories-read";

/// Whether a change can have connected a repository to a service or taken it away: pinning an
/// insight, say, cannot.
pub fn moves_connections(change: &Value) -> bool {
    matches!(
        change["change"].as_str(),
        Some("applied" | "connected" | "disconnected" | "deleted" | "synced" | "removed")
    )
}

/// Whether a read of every repository has ever finished.
pub async fn ever_read(backend: &Backend) -> bool {
    matches!(backend.state_get(READ_AT).await, Ok(Some(_)))
}

/// Whether the plugin should read every repository as it loads: nothing has been read yet, a
/// repository's last read failed and may not fail now, or runtimes have just been turned on and a
/// repository's own files have never been read.
pub async fn due_at_load(backend: &Backend) -> bool {
    if !ever_read(backend).await {
        return true;
    }
    let files = backend.feature(REPOSITORIES);
    all(backend).await.is_ok_and(|kept| {
        kept.values().any(|held| held.problem.is_some() || (files && held.read_at.is_none()))
    })
}

/// The repositories connected to `service`, or to any service, as last read: what a page with
/// nothing to show says was found in them, rather than that nothing is connected.
pub async fn connected(backend: &Backend, service: Option<&str>) -> Vec<Scanned> {
    let Ok(kept) = all(backend).await else { return Vec::new() };
    kept.into_values()
        .filter(|held| match service {
            Some(service) => held.services.iter().any(|name| name == service),
            None => !held.services.is_empty(),
        })
        .collect()
}

/// What was found in a repository that turned up nothing endoflife.date tracks, in a sentence.
pub fn nothing_found(held: &Scanned, files: bool) -> String {
    let services = held.services.join(", ");
    let packages = match (&held.commit, held.listed) {
        (Some(commit), Some(total)) => format!(
            "Repository Insights listed {total} packages in it at {}, and none of them is a product \
             endoflife.date tracks",
            commit.get(..7).unwrap_or(commit)
        ),
        (Some(commit), None) => format!(
            "none of the packages Repository Insights listed in it at {} is a product \
             endoflife.date tracks",
            commit.get(..7).unwrap_or(commit)
        ),
        (None, _) => "Repository Insights has not listed its packages yet".to_string(),
    };
    let runtimes = match (files, &held.problem, held.read_at) {
        (false, _, _) => "the runtimes its own files name — a Dockerfile, go.mod, .nvmrc or \
                          rust-toolchain — are read only with Runtimes from repository files on"
            .to_string(),
        (true, Some(problem), _) => format!("its own files could not be read: {problem}"),
        (true, None, None) => "its own files have not been read yet".to_string(),
        (true, None, Some(_)) => "its own files name no runtime with a version either".to_string(),
    };
    format!("{} ({services}): {packages}; {runtimes}.", held.repository)
}

/// Whether a read is already waiting for the Catalogue to stop changing.
#[derive(Default, Clone)]
pub struct Settling(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl Settling {
    /// Reads every repository once the Catalogue has been quiet for `SETTLE`. A change arriving
    /// while one of these waits is covered by it, so a burst costs one read.
    pub fn after_the_change(&self, backend: &Backend) {
        use std::sync::atomic::Ordering;
        if self.0.swap(true, Ordering::SeqCst) {
            return;
        }
        let (backend, waiting) = (backend.clone(), self.0.clone());
        tokio::spawn(async move {
            tokio::time::sleep(SETTLE).await;
            // Cleared first, so a change landing while this is queued starts the next wait rather
            // than being swallowed by the read already on its way.
            waiting.store(false, Ordering::SeqCst);
            match backend.task(json!({ "read": "repositories", "from": 0 })).await {
                Ok(task) => {
                    tracing::info!(%task, "the Catalogue changed, so repositories are read")
                }
                Err(err) => tracing::warn!(%err, "the Catalogue changed but no read was queued"),
            }
        });
    }
}

/// Where the map of what each service runs is up to: it changes whenever what was found in a
/// repository does, so a page drawn after a read shows it rather than what was kept before.
const GENERATION: &str = "repositories/generation";

pub async fn generation(backend: &Backend) -> i64 {
    match backend.cache_get(GENERATION).await {
        Ok(Some(value)) => value.as_i64().unwrap_or_default(),
        _ => 0,
    }
}

async fn moved_on(backend: &Backend) {
    let now = Utc::now().timestamp_millis();
    if let Err(err) = backend.cache_set(GENERATION, json!(now), None).await {
        tracing::debug!(%err, "what the repositories use changed, but pages may show it late");
    }
}

/// Whether a repository is worth fetching again: one never read is, and one pushed to since it was
/// last read is. A repository whose source says nothing about when it was pushed to is read again
/// a week after the last time, rather than never or on every run.
fn worth_reading(pushed_at: Option<&str>, kept: Option<&Scanned>) -> bool {
    let Some(kept) = kept else { return true };
    // A read that failed — its source had not given this plugin archive links yet, say — is tried
    // again at the next run, not left until somebody next pushes.
    if kept.problem.is_some() {
        return true;
    }
    let Some(read_at) = kept.read_at else { return true };
    match pushed_at.and_then(|at| DateTime::parse_from_rfc3339(at).ok()) {
        Some(pushed) => pushed > read_at,
        None => Utc::now().signed_duration_since(read_at).num_days() >= 7,
    }
}

fn row(scanned: &Scanned) -> Value {
    json!({
        "repository": scanned.repository,
        "services": scanned.services,
        "found": scanned.found,
        "files": scanned.files,
        "packages": scanned.packages,
        "commit": scanned.commit,
        "listed_at": scanned.listed_at,
        "listed": scanned.listed,
        "pushed_at": scanned.pushed_at,
        "read_at": scanned.read_at,
        "problem": scanned.problem,
    })
}

/// Reads one repository: its archive, the manifests in it, and what they say it is built on.
async fn read_one(
    backend: &Backend,
    definitions: &Definitions,
    repository: &str,
    branch: &str,
) -> (Vec<Found>, Option<i64>, Option<String>) {
    let url = match link(backend, &definitions.sources, repository, branch).await {
        Ok(url) => url,
        Err(why) => return (Vec::new(), None, Some(why)),
    };
    let archive = match fetch(&url).await {
        Ok(archive) => archive,
        Err(why) => return (Vec::new(), None, Some(why)),
    };
    match manifests_in(&archive) {
        Ok(files) => {
            let count = i64::try_from(files.len()).unwrap_or_default();
            (manifests::products(&files), Some(count), None)
        }
        Err(why) => (Vec::new(), None, Some(why)),
    }
}

/// Everything read, by repository.
pub async fn all(backend: &Backend) -> Result<BTreeMap<String, Scanned>, PluginError> {
    let kept: Vec<Scanned> = backend.query_all(Query::new("repositories")).await?;
    Ok(kept.into_iter().map(|scanned| (scanned.repository.clone(), scanned)).collect())
}

/// What each service's repositories are built on, by service, from what was last read: the
/// products among their packages, and what their own files say where `files` asks for it.
pub async fn by_service(backend: &Backend, files: bool) -> BTreeMap<String, Vec<(String, Found)>> {
    let mut found: BTreeMap<String, Vec<(String, Found)>> = BTreeMap::new();
    let Ok(kept) = all(backend).await else { return found };
    for scanned in kept.values() {
        let read =
            scanned.packages.iter().chain(files.then_some(&scanned.found).into_iter().flatten());
        let read: Vec<&Found> = read.collect();
        if read.is_empty() {
            continue;
        }
        for service in &scanned.services {
            let listed = found.entry(service.clone()).or_default();
            for one in &read {
                listed.push((scanned.repository.clone(), (*one).clone()));
            }
        }
    }
    found
}

/// Whether what Repository Insights listed is newer than what was matched from it.
fn newer(listed: &Listed, held: Option<&Scanned>) -> bool {
    held.is_none_or(|held| {
        held.commit.as_deref() != Some(listed.commit.as_str())
            || held.listed_at.is_none_or(|at| at < listed.listed_at)
    })
}

/// Matches a repository's packages to products, where Repository Insights listed any.
async fn match_packages(
    backend: &Backend,
    scanned: &mut Scanned,
    id: &str,
    index: &BTreeMap<String, String>,
) -> Result<(), PluginError> {
    match packages::of(backend, id).await? {
        Some((listed, held)) => {
            scanned.packages = packages::products(&held, index);
            scanned.commit = Some(listed.commit);
            scanned.listed_at = Some(listed.listed_at);
            scanned.listed = Some(listed.total);
        }
        None => forget_packages(scanned),
    }
    Ok(())
}

fn forget_packages(scanned: &mut Scanned) {
    scanned.packages.clear();
    scanned.listed = None;
    scanned.commit = None;
    scanned.listed_at = None;
}

/// Repository Insights has just listed a repository's packages: they are matched at once, for
/// the services the Catalogue says the repository belongs to.
pub async fn heard(backend: &Backend, id: &str, repository: &str) -> Result<(), PluginError> {
    let services = services_of(backend, repository).await.map_err(PluginError::from)?;
    if services.is_empty() {
        return Ok(());
    }
    let kept = all(backend).await?;
    let held = kept.into_values().find(|held| held.repository.eq_ignore_ascii_case(repository));
    let mut scanned = held
        .unwrap_or_else(|| Scanned { repository: repository.to_string(), ..Scanned::default() });
    scanned.services = services;
    matched(backend, scanned, id).await
}

async fn matched(backend: &Backend, mut scanned: Scanned, id: &str) -> Result<(), PluginError> {
    let index = packages::index(backend).await.map_err(PluginError::from)?;
    match_packages(backend, &mut scanned, id, &index).await?;
    backend.upsert::<Value>("repositories", &["repository"], row(&scanned)).await?;
    moved_on(backend).await;
    let repository = scanned.repository.as_str();
    tracing::info!(repository, products = scanned.packages.len(), "its packages were matched");
    Ok(())
}

/// One attempt at the run that reads repositories: it refreshes which services each repository
/// belongs to, matches the packages Repository Insights listed since, fetches the few that have
/// moved where their own files are read, and queues itself again while there is more. Each attempt
/// has the platform's thirty seconds, so the work is cut to fit rather than abandoned.
pub async fn read_all(backend: &Backend, from: usize) -> Result<Value, PluginError> {
    if !backend.feature(LIFECYCLE) {
        return Ok(json!({ "read": 0, "why": "End-of-life data is off" }));
    }
    let files = backend.feature(REPOSITORIES);
    let definitions = Definitions::read(&backend.settings());
    let listed = listed(backend).await?;
    let kept = all(backend).await?;
    let total = listed.len();
    // What Repository Insights listed, by repository; where two sources hold one name, the newer.
    let mut lists: BTreeMap<String, Listed> = BTreeMap::new();
    for list in packages::every(backend).await? {
        let name = list.repository.to_ascii_lowercase();
        if lists.get(&name).is_none_or(|held| held.listed_at < list.listed_at) {
            lists.insert(name, list);
        }
    }
    let mut index: Option<Result<BTreeMap<String, String>, String>> = None;
    let (mut matched, mut unmatched) = (0usize, false);
    // Which services each repository belongs to, refreshed a batch at a time: it is one call per
    // repository, and it is what decides whether a repository is read at all.
    let attaching: Vec<&Value> = listed.iter().skip(from).take(ATTACHMENTS_PER_ATTEMPT).collect();
    let (mut writes, mut read, mut problems, mut fetched) = (Vec::new(), 0usize, 0usize, 0usize);
    let mut unattached = 0usize;
    for resource in attaching {
        let Some(name) = resource["name"].as_str().filter(|name| !name.is_empty()) else {
            continue;
        };
        // An archived repository is not what anything runs.
        if resource["metadata"]["archived"] == json!(true) {
            continue;
        }
        let services = match services_of(backend, name).await {
            Ok(services) => services,
            Err(why) => {
                // Nothing is dropped for it: the Catalogue did not say it belongs to nothing.
                tracing::warn!(repository = name, %why, "which services it belongs to is not known");
                problems += 1;
                continue;
            }
        };
        if services.is_empty() {
            unattached += 1;
            // Nothing here belongs to a service any more: drop what was kept of it.
            if kept.contains_key(name) {
                writes.push(DataRequest::delete("repositories", name));
            }
            continue;
        }
        read += 1;
        let pushed_at = resource["metadata"]["pushed_at"].as_str().map(str::to_string);
        let held = kept.get(name);
        let mut scanned = Scanned {
            repository: name.to_string(),
            services,
            pushed_at: pushed_at.clone(),
            ..held.cloned().unwrap_or_default()
        };
        match lists.get(&name.to_ascii_lowercase()) {
            Some(list) if newer(list, held) && matched >= LISTS_PER_ATTEMPT => unmatched = true,
            Some(list) if newer(list, held) => {
                if index.is_none() {
                    index = Some(packages::index(backend).await);
                }
                // Without the index nothing can be matched; what was matched before is kept, and
                // the next run tries again.
                if let Some(Ok(index)) = &index {
                    match_packages(backend, &mut scanned, &list.id, index).await?;
                    matched += 1;
                }
            }
            Some(_) => {}
            // Repository Insights has not scanned it, or has forgotten it.
            None => forget_packages(&mut scanned),
        }
        if files && fetched < PER_ATTEMPT && worth_reading(pushed_at.as_deref(), held) {
            let branch = resource["metadata"]["default_branch"]
                .as_str()
                .filter(|branch| !branch.is_empty())
                .unwrap_or("HEAD");
            let (found, files, problem) = read_one(backend, &definitions, name, branch).await;
            fetched += 1;
            if problem.is_some() {
                problems += 1;
            }
            // What was found before is kept when a read failed, so one bad fetch does not empty a
            // service's page; the problem is recorded beside it.
            if problem.is_none() {
                scanned.found = found;
                scanned.files = files;
            }
            scanned.problem = problem;
            scanned.read_at = Some(Utc::now());
        }
        writes.push(DataRequest::upsert("repositories", &["repository"], row(&scanned)));
    }
    if !writes.is_empty() {
        backend.batch(writes).await?;
        moved_on(backend).await;
    }
    // More to do: either repositories not looked at yet, or ones that wanted fetching and did not
    // fit in this attempt. The second is why `next` does not always move on.
    let looked_at = from + ATTACHMENTS_PER_ATTEMPT;
    let next = match fetched >= PER_ATTEMPT || unmatched {
        true => from,
        false => looked_at,
    };
    if next < total {
        backend.task(json!({ "read": "repositories", "from": next })).await?;
    } else if let Err(err) = backend.state_set(READ_AT, json!(Utc::now())).await {
        tracing::warn!(%err, "that every repository was read could not be kept");
    }
    Ok(json!({
        "repositories": total,
        "attached": read,
        "unattached": unattached,
        "matched": matched,
        "fetched": fetched,
        "problems": problems,
        "index": index.and_then(Result::err),
        "next": (next < total).then_some(next),
    }))
}
