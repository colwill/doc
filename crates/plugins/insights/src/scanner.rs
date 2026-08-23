//! The long-running `run`: takes each repository waiting to be scanned, fetches it through its
//! source's archive link, and runs ccc over it. The commit scanned before is fetched too and
//! committed under it, so ccc can say what changed since, with no clone and no credentials here.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use chrono::Utc;
use doc_plugin_sdk::{Backend, PluginError};
use serde_json::{Value, json};
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::ID;
use crate::archive;
use crate::report::{self, Reports};
use crate::settings::{self, SCANS, Scanning};
use crate::store::{self, Asked, Ended, Found, Named, Repository, Scan, Wanted};

/// How long the engine waits for a nudge before looking for work anyway.
const IDLE: Duration = Duration::from_secs(30);
/// The most of a failed command's error output kept to say why.
const SAID: usize = 600;
/// The tag the commit scanned before is given, which ccc diffs against.
const BASE: &str = "doc-base";
/// ccc's own installer, which fetches the release built for this machine.
const INSTALLER: &str = "https://raw.githubusercontent.com/colwill/ccc/main/install.sh";
/// How long installing ccc may take.
const INSTALLING: Duration = Duration::from_secs(180);

/// What the scans run with, found where the plugin runs.
#[derive(Debug, Clone)]
pub struct Tools {
    pub ccc: PathBuf,
    /// ccc's version, or why it could not be run.
    pub version: Result<String, String>,
    /// git's version, without which changes since the scan before are not worked out.
    pub git: Option<String>,
}

impl Tools {
    /// ccc as the deployment names it or on the path; else the copy this plugin installed; else
    /// installed now, from ccc's own repository.
    async fn find() -> Self {
        let git = version_of(Path::new("git")).await.ok();
        let named = settings::ccc();
        let missing = match version_of(&named).await {
            Ok(version) => return Self { ccc: named, version: Ok(version), git },
            Err(err) => format!("ccc could not be run as `{}` ({err})", named.display()),
        };
        let installed = settings::tools_dir().join("ccc");
        if let Ok(version) = version_of(&installed).await {
            return Self { ccc: installed, version: Ok(version), git };
        }
        if !settings::installs() {
            let version = Err(format!(
                "{missing}: install it where this plugin runs, or name it with DOC_INSIGHTS_CCC"
            ));
            return Self { ccc: named, version, git };
        }
        let version = match install().await {
            Ok(()) => version_of(&installed).await.map_err(|err| {
                format!("{missing}, and the copy installed could not be run either ({err})")
            }),
            Err(err) => Err(format!("{missing}, and installing it failed: {err}")),
        };
        if let Ok(version) = &version {
            tracing::info!(%version, path = %installed.display(), "ccc was installed");
        }
        Self { ccc: installed, version, git }
    }
}

/// Runs ccc's installer into this plugin's tools directory: `curl -fsSL <install.sh> | bash`.
async fn install() -> Result<(), String> {
    let dir = settings::tools_dir();
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|err| format!("{} could not be made: {err}", dir.display()))?;
    tracing::info!(from = INSTALLER, into = %dir.display(), "ccc is not installed; installing it");
    let mut command = command(Path::new("bash"), &dir, &dir);
    command
        .arg("-c")
        .arg(format!("set -o pipefail; curl -fsSL {INSTALLER} | bash"))
        .env("CCC_INSTALL_DIR", &dir);
    if let Ok(version) = std::env::var("DOC_INSIGHTS_CCC_VERSION") {
        command.env("CCC_VERSION", version);
    }
    let output = tokio::time::timeout(INSTALLING, command.output())
        .await
        .map_err(|_| format!("it took longer than {}s", INSTALLING.as_secs()))?
        .map_err(|err| format!("bash could not be run: {err}"))?;
    match output.status.success() {
        true => Ok(()),
        false => Err(said(&output.stderr)),
    }
}

async fn version_of(program: &Path) -> Result<String, String> {
    let output = tokio::process::Command::new(program)
        .arg("--version")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|err| err.to_string())?;
    match output.status.success() {
        true => Ok(String::from_utf8_lossy(&output.stdout).trim().to_string()),
        false => Err(format!("it exited with {}", output.status)),
    }
}

/// What the routes, events and the engine share in this process.
#[derive(Default)]
pub struct Scanner {
    nudge: Notify,
    stop: Mutex<CancellationToken>,
    tools: RwLock<Option<Tools>>,
}

impl Scanner {
    /// Tells the engine something may be waiting.
    pub fn wake(&self) {
        self.nudge.notify_one();
    }

    /// Stops the engine, and the scan it is running.
    pub fn stop(&self) {
        if let Ok(stop) = self.stop.lock() {
            stop.cancel();
        }
        self.wake();
    }

    /// What scans run with, found once and again whenever the engine starts.
    pub async fn tools(&self) -> Tools {
        if let Some(tools) = self.tools.read().await.as_ref() {
            return tools.clone();
        }
        let found = Tools::find().await;
        *self.tools.write().await = Some(found.clone());
        found
    }

    async fn idle(&self, stop: &CancellationToken) {
        tokio::select! {
            () = stop.cancelled() => {}
            () = self.nudge.notified() => {}
            () = tokio::time::sleep(IDLE) => {}
        }
    }

    pub async fn run(&self, backend: &Backend) -> Result<(), PluginError> {
        let stop = CancellationToken::new();
        if let Ok(mut held) = self.stop.lock() {
            *held = stop.clone();
        }
        let tools = Tools::find().await;
        *self.tools.write().await = Some(tools.clone());
        match &tools.version {
            Ok(version) => tracing::info!(%version, git = ?tools.git, "the scanner started"),
            Err(problem) => tracing::warn!(%problem, "the scanner started, but cannot scan"),
        }
        let mut recovered = false;
        let mut listed = false;
        while !stop.is_cancelled() {
            if !backend.feature(SCANS) {
                self.idle(&stop).await;
                continue;
            }
            let scanning = Scanning::read(&backend.settings());
            if !recovered {
                let longest = chrono::Duration::from_std(scanning.timeout).unwrap_or_default()
                    + chrono::Duration::minutes(5);
                match store::recover(backend, longest).await {
                    Ok(0) => recovered = true,
                    Ok(count) => {
                        recovered = true;
                        tracing::info!(
                            count,
                            "scans left going by a stopped process are queued again"
                        );
                    }
                    Err(err) => tracing::warn!(%err, "scans left going could not be looked for"),
                }
            }
            if !listed {
                listed = true;
                match list_packages(backend, &scanning).await {
                    Ok(0) => {}
                    Ok(count) => tracing::info!(
                        count,
                        "repositories scanned before their packages were kept are queued to list them"
                    ),
                    Err(err) => {
                        tracing::warn!(%err, "repositories without their packages could not be looked for")
                    }
                }
            }
            let claimed = match store::claim(backend).await {
                Ok(Some(claimed)) => claimed,
                Ok(None) => {
                    self.idle(&stop).await;
                    continue;
                }
                Err(err) => {
                    tracing::warn!(%err, "the queue could not be read");
                    self.idle(&stop).await;
                    continue;
                }
            };
            let ended = tokio::select! {
                () = stop.cancelled() => Ended::Stopped,
                done = tokio::time::timeout(scanning.timeout, scan(backend, &tools, &claimed, &scanning)) => {
                    match done {
                        Ok(ended) => ended,
                        Err(_) => Ended::Failed(format!(
                            "the scan took longer than {}s, the longest one may take",
                            scanning.timeout.as_secs()
                        )),
                    }
                }
            };
            if let Err(err) = store::finish(backend, &claimed, &ended, scanning.keep).await {
                tracing::warn!(repository = %claimed.id, %err, "how the scan ended could not be recorded");
            }
            announce(backend, &claimed, &ended).await;
        }
        tracing::info!("the scanner stopped");
        Ok(())
    }
}

/// Queues once more every repository scanned before its packages were kept, so End of life has
/// them without waiting for the next merge. A scan of a branch that has not moved only lists them.
async fn list_packages(backend: &Backend, scanning: &Scanning) -> Result<usize, PluginError> {
    let mut queued = 0;
    for held in store::unlisted(backend).await? {
        let Ok(named) = Named::new(&held.source, &held.repository) else { continue };
        let wanted = Wanted {
            why: "Its packages had not been listed for End of life".into(),
            trigger: None,
            at: None,
        };
        let branch = if held.branch.is_empty() { scanning.branch.as_str() } else { &held.branch };
        if matches!(store::want(backend, &named, branch, wanted).await?, Asked::Queued) {
            queued += 1;
        }
    }
    Ok(queued)
}

/// Says a scan ended, and has every open page showing repositories draw them again.
async fn announce(backend: &Backend, claimed: &Repository, ended: &Ended) {
    let (topic, payload) = match ended {
        Ended::Scanned(scan, _) => {
            let summary = &scan.summary;
            (
                "scan.completed",
                json!({
                    "repository": claimed.repository,
                    "source": claimed.source,
                    "branch": claimed.branch,
                    "commit": scan.commit,
                    "base_commit": scan.base_commit,
                    "url": claimed.href(),
                    "files": summary["files"],
                    "lines": summary["lines"],
                    "functions": summary["functions"],
                    "lint_warnings": summary["lint_warnings"],
                    "untested": summary["untested"],
                    "changed_files": summary["changes"]["files"],
                }),
            )
        }
        Ended::Failed(problem) => (
            "scan.failed",
            json!({
                "repository": claimed.repository,
                "source": claimed.source,
                "branch": claimed.branch,
                "problem": problem,
                "url": claimed.href(),
            }),
        ),
        Ended::Unchanged { .. } | Ended::Stopped => ("", Value::Null),
    };
    if !topic.is_empty()
        && let Err(err) = backend.publish(&format!("plugin.{ID}.{topic}"), payload).await
    {
        tracing::warn!(%err, "the scan's end could not be announced");
    }
    let listed = match ended {
        Ended::Scanned(_, found) => found.packages.as_ref(),
        Ended::Unchanged { packages, .. } => packages.as_ref(),
        _ => None,
    };
    if let Some(packages) = listed {
        let payload = json!({
            "id": claimed.id,
            "repository": claimed.repository,
            "source": claimed.source,
            "packages": packages.total,
        });
        if let Err(err) = backend.publish(&format!("plugin.{ID}.packages.listed"), payload).await {
            tracing::warn!(%err, "that its packages were listed could not be announced");
        }
    }
    let _ =
        backend.publish(&format!("plugin.{ID}.ui.repositories"), json!({ "id": claimed.id })).await;
}

/// One repository, from its archive to what ccc found in it.
async fn scan(
    backend: &Backend,
    tools: &Tools,
    claimed: &Repository,
    scanning: &Scanning,
) -> Ended {
    match scanned(backend, tools, claimed, scanning).await {
        Ok(ended) => ended,
        Err(problem) => {
            tracing::warn!(repository = %claimed.id, %problem, "the scan failed");
            Ended::Failed(problem)
        }
    }
}

async fn scanned(
    backend: &Backend,
    tools: &Tools,
    claimed: &Repository,
    scanning: &Scanning,
) -> Result<Ended, String> {
    let version = tools.version.clone()?;
    let started = Utc::now();
    let clock = Instant::now();
    let work = tempfile::Builder::new()
        .prefix("doc-insights-")
        .tempdir_in(settings::work_dir())
        .map_err(|err| format!("no room to unpack the repository: {err}"))?;
    let root = work.path();

    let through = Utc::now();
    let link =
        archive::link(backend, &claimed.source, &claimed.repository, &claimed.branch).await?;
    archive::fetch(&link.url, &root.join("head.tar.gz")).await?;
    let head = root.join("head");
    let unpacked = archive::unpack(root.join("head.tar.gz"), head.clone()).await?;
    let _ = tokio::fs::remove_file(root.join("head.tar.gz")).await;
    let commit = unpacked.commit.clone().unwrap_or_else(|| "unknown".into());
    let path = head.to_string_lossy().to_string();
    if claimed.latest.is_some() && claimed.commit.as_deref() == Some(commit.as_str()) {
        let packages = match store::listed(backend, &claimed.id).await {
            Ok(false) => report::packages(&audit(tools, root, &path, scanning).await),
            _ => None,
        };
        return Ok(Ended::Unchanged { through, packages });
    }
    tracing::info!(repository = %claimed.id, %commit, files = unpacked.files, "scanning");

    let base = match (&tools.git, claimed.commit.as_deref()) {
        (Some(_), Some(before)) if claimed.latest.is_some() => {
            match based(backend, claimed, root, &head, before).await {
                Ok(()) => Some(before.to_string()),
                Err(problem) => {
                    tracing::warn!(repository = %claimed.id, %problem, "changes since the scan before are left out");
                    committed(root, &head).await?;
                    None
                }
            }
        }
        (Some(_), _) => {
            committed(root, &head).await?;
            None
        }
        (None, _) => None,
    };

    let mut asked = vec!["insights", path.as_str()];
    if base.is_some() {
        asked.extend(["--base", BASE]);
    }
    let insights = ccc(tools, root, &asked, &[0]).await?;
    let sast = ccc(tools, root, &["sast", &path, "--format", "json"], &[0, 1]).await;
    let audit = audit(tools, root, &path, scanning).await;
    let packages = report::packages(&audit);
    let mut reports = Reports { insights, sast, audit };
    if base.is_none() && tools.git.is_none() {
        reports.insights["changes"] = json!({
            "available": false,
            "reason": "git is not installed where this plugin runs, so changes are not worked out",
        });
    }
    let condensed = report::condense(&reports)?;
    let finished = Utc::now();
    let scan = Scan {
        id: Uuid::now_v7(),
        repository_id: claimed.id.clone(),
        source: claimed.source.clone(),
        repository: claimed.repository.clone(),
        branch: claimed.branch.clone(),
        commit,
        base_commit: base,
        why: claimed.why.clone(),
        trigger: claimed.trigger.clone(),
        started_at: started,
        finished_at: finished,
        took_ms: Some(i64::try_from(clock.elapsed().as_millis()).unwrap_or(i64::MAX)),
        ccc: Some(version),
        summary: condensed.summary,
        report: condensed.report,
    };
    Ok(Ended::Scanned(Box::new(scan), Found { through, web: link.web, packages }))
}

/// `ccc audit`: the packages in the repository's lockfiles, checked against OSV only where
/// Dependency advisories is on.
async fn audit(
    tools: &Tools,
    root: &Path,
    path: &str,
    scanning: &Scanning,
) -> Result<Value, String> {
    let mut asked = vec!["audit", path, "--format", "json"];
    if !scanning.advisories {
        asked.push("--offline");
    }
    ccc(tools, root, &asked, &[0, 1]).await
}

/// The commit scanned before, committed with the one being scanned on top of it and tagged, so
/// ccc's change set is exactly what came between them.
async fn based(
    backend: &Backend,
    claimed: &Repository,
    root: &Path,
    head: &Path,
    before: &str,
) -> Result<(), String> {
    let link = archive::link(backend, &claimed.source, &claimed.repository, before).await?;
    archive::fetch(&link.url, &root.join("base.tar.gz")).await?;
    let base = root.join("base");
    archive::unpack(root.join("base.tar.gz"), base.clone()).await?;
    let _ = tokio::fs::remove_file(root.join("base.tar.gz")).await;
    git(root, &base, &["init", "-q", "-b", "scan"]).await?;
    git(root, &base, &["add", "-A", "-f"]).await?;
    git(root, &base, &["commit", "-q", "--no-verify", "--allow-empty", "-m", before]).await?;
    git(root, &base, &["tag", BASE]).await?;
    tokio::fs::rename(base.join(".git"), head.join(".git"))
        .await
        .map_err(|err| format!("the history could not be moved: {err}"))?;
    let _ = tokio::fs::remove_dir_all(&base).await;
    git(root, head, &["add", "-A", "-f"]).await?;
    git(root, head, &["commit", "-q", "--no-verify", "--allow-empty", "-m", "scanned"]).await?;
    Ok(())
}

/// The commit being scanned on its own, for a repository scanned for the first time.
async fn committed(root: &Path, head: &Path) -> Result<(), String> {
    git(root, head, &["init", "-q", "-b", "scan"]).await?;
    git(root, head, &["add", "-A", "-f"]).await?;
    git(root, head, &["commit", "-q", "--no-verify", "--allow-empty", "-m", "scanned"]).await?;
    Ok(())
}

/// A command with nothing of this process's environment but its path, and a home of its own, so
/// neither a configuration nor anybody's history is read.
fn command(program: &Path, home: &Path, dir: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(program);
    command
        .current_dir(dir)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("TMPDIR", home)
        .env("LANG", "C.UTF-8")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command
}

async fn git(home: &Path, dir: &Path, args: &[&str]) -> Result<(), String> {
    let mut asked = vec![
        "-c",
        "user.name=DOC",
        "-c",
        "user.email=insights@doc.invalid",
        "-c",
        "commit.gpgsign=false",
        "-c",
        "core.hooksPath=/dev/null",
        "-c",
        "core.autocrlf=false",
    ];
    asked.extend(args);
    let output = command(Path::new("git"), home, dir)
        .args(&asked)
        .output()
        .await
        .map_err(|err| format!("git could not be run: {err}"))?;
    match output.status.success() {
        true => Ok(()),
        false => Err(format!("git {} failed: {}", args.join(" "), said(&output.stderr))),
    }
}

/// ccc's JSON answer, from a run that exited with one of `ok`.
async fn ccc(tools: &Tools, home: &Path, args: &[&str], ok: &[i32]) -> Result<Value, String> {
    let output = command(&tools.ccc, home, home)
        .args(args)
        .output()
        .await
        .map_err(|err| format!("ccc could not be run: {err}"))?;
    let exited = output.status.code();
    if !exited.is_some_and(|code| ok.contains(&code)) {
        return Err(format!(
            "ccc {} failed ({}): {}",
            args[0],
            output.status,
            said(&output.stderr)
        ));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|err| format!("ccc {} answered with something other than JSON: {err}", args[0]))
}

/// The end of what a command said on its error output, which is where the reason is.
fn said(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim();
    let start = text.char_indices().rev().nth(SAID).map_or(0, |(at, _)| at);
    match text[start..].trim() {
        "" => "it gave no reason".into(),
        tail => tail.to_string(),
    }
}
