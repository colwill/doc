//! The pages at `/p/kubernetes/...`: every cluster with its counts and recent rollouts and
//! outages, each cluster's namespaces and workloads, each workload with its rollouts, and a panel
//! on each service's page in the Catalogue. Writers can have the clusters read at once and let the
//! plugin read the Catalogue as them; connecting a cluster is on the Settings page.

use askama::Template;
use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Order, Query, Request, Response};
use serde_json::json;

use crate::api::parameter;
use crate::catalogue::{self, Reading};
use crate::settings::Config;
use crate::store::{
    ClusterRecord, FAILURE, IN_PROGRESS, NamespaceRecord, OutageRecord, Rollout, SUCCESS, Stats,
    WorkloadRecord,
};
use crate::watch::{COMMIT, REPOSITORY, SERVICE, Watcher};
use crate::{Refusal, who};

const MANIFEST: &str = include_str!("../deploy/read-only.yaml");
const RECENT: u32 = 20;

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn when(at: DateTime<Utc>) -> String {
    at.format("%-d %b %Y, %H:%M UTC").to_string()
}

fn ago(at: DateTime<Utc>) -> String {
    let seconds = (Utc::now() - at).num_seconds().max(0);
    match seconds {
        0..60 => "just now".into(),
        60..3_600 => plural(seconds / 60, "minute") + " ago",
        3_600..86_400 => plural(seconds / 3_600, "hour") + " ago",
        _ => plural(seconds / 86_400, "day") + " ago",
    }
}

fn plural(count: i64, word: &str) -> String {
    match count {
        1 => format!("1 {word}"),
        _ => format!("{count} {word}s"),
    }
}

pub fn duration(seconds: f64) -> String {
    let seconds = seconds.round().max(0.0) as i64;
    match seconds {
        0..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m {}s", seconds / 60, seconds % 60),
        3_600..86_400 => format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60),
        _ => format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3_600),
    }
}

fn cores(cores: f64) -> String {
    match cores < 1.0 {
        true => format!("{}m", (cores * 1000.0).round()),
        false => format!("{cores:.1}"),
    }
}

fn bytes(bytes: f64) -> String {
    let units = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    match unit {
        0 => format!("{value:.0} {}", units[unit]),
        _ => format!("{value:.1} {}", units[unit]),
    }
}

/// Use, and of how much where that is known: `350m of 8 cores`.
fn used(used: Option<f64>, of: Option<f64>, show: fn(f64) -> String, unit: &str) -> String {
    match (used, of) {
        (Some(used), Some(of)) => format!("{} of {}{unit}", show(used), show(of)),
        (Some(used), None) => show(used),
        (None, Some(of)) => format!("{}{unit} in all", show(of)),
        (None, None) => "Not known".into(),
    }
}

fn badge(state: &str) -> &'static str {
    match state {
        "available" | "ok" | SUCCESS => "up",
        "degraded" => "degraded",
        "progressing" | IN_PROGRESS => "loading",
        "unavailable" | "failed" | "error" | FAILURE => "down",
        _ => "unknown",
    }
}

fn state_words(state: &str) -> String {
    match state {
        IN_PROGRESS => "In progress".into(),
        "scaled-down" => "Scaled to zero".into(),
        other => {
            let mut chars = other.chars();
            chars
                .next()
                .map(|first| first.to_uppercase().chain(chars).collect())
                .unwrap_or_default()
        }
    }
}

fn workload_href(cluster: &str, namespace: &str, name: &str) -> String {
    format!("/p/kubernetes/workloads/{cluster}/{namespace}/{name}")
}

pub struct ClusterRow {
    pub name: String,
    pub href: String,
    pub environment: String,
    pub badge: &'static str,
    pub state: String,
    pub version: String,
    pub nodes: String,
    pub namespaces: String,
    pub workloads: String,
    pub pods: String,
    pub restarts: i64,
    pub cpu: String,
    pub memory: String,
    pub read: String,
    pub problem: Option<String>,
    pub insecure: bool,
}

fn cluster_row(cluster: &ClusterRecord, config: &Config) -> ClusterRow {
    let stats = &cluster.stats;
    let late = (Utc::now() - cluster.polled_at).num_seconds() as f64
        > config.interval.as_secs_f64() * 3.0 + 60.0;
    let problem = match (&cluster.problem, late) {
        (Some(problem), _) => Some(problem.clone()),
        (None, true) => Some(format!("not read since {}", when(cluster.polled_at))),
        (None, false) => None,
    };
    ClusterRow {
        name: cluster.id.clone(),
        href: format!("/p/kubernetes/clusters/{}", cluster.id),
        environment: cluster.environment.clone().unwrap_or_else(|| "None".into()),
        badge: match problem.is_some() {
            true => "down",
            false => "up",
        },
        state: match (&cluster.problem, late) {
            (Some(_), _) => "Not read".into(),
            (None, true) => "Late".into(),
            (None, false) => "Read".into(),
        },
        version: cluster.version.clone().unwrap_or_default(),
        nodes: format!("{} of {}", stats.nodes_ready.unwrap_or(0), stats.nodes.unwrap_or(0)),
        namespaces: stats.namespaces.unwrap_or(0).to_string(),
        workloads: stats.workloads.unwrap_or(0).to_string(),
        pods: format!("{} running of {}", stats.running, stats.pods),
        restarts: stats.restarts,
        cpu: used(stats.cpu, stats.cpu_allocatable, cores, " cores"),
        memory: used(stats.memory, stats.memory_allocatable, bytes, ""),
        read: ago(cluster.polled_at),
        problem,
        insecure: cluster.insecure,
    }
}

pub struct RolloutRow {
    pub when: String,
    pub workload: String,
    pub href: String,
    pub environment: String,
    pub revision: i64,
    pub badge: &'static str,
    pub state: String,
    pub took: String,
    pub commit: String,
    pub service: Option<String>,
    pub repository: Option<String>,
    pub rollback: bool,
    pub backfilled: bool,
    pub reason: Option<String>,
}

fn rollout_row(rollout: &Rollout) -> RolloutRow {
    RolloutRow {
        when: when(rollout.started_at),
        workload: format!("{}/{}/{}", rollout.cluster, rollout.namespace, rollout.workload),
        href: workload_href(&rollout.cluster, &rollout.namespace, &rollout.workload),
        environment: rollout.environment.clone().unwrap_or_else(|| "None".into()),
        revision: rollout.revision,
        badge: badge(&rollout.state),
        state: state_words(&rollout.state),
        took: match (rollout.backfilled, rollout.seconds) {
            (true, _) => "Not known".into(),
            (false, Some(seconds)) => duration(seconds),
            (false, None) => "Still going".into(),
        },
        commit: rollout.sha.chars().take(12).collect(),
        service: rollout.service.clone(),
        repository: rollout.repository.clone(),
        rollback: rollout.rollback,
        backfilled: rollout.backfilled,
        reason: rollout.reason.clone().filter(|reason| !reason.is_empty()),
    }
}

pub struct OutageRow {
    pub service: String,
    pub workload: String,
    pub href: String,
    pub environment: String,
    pub started: String,
    pub ended: String,
    pub lasted: String,
    pub detail: String,
}

fn outage_row(outage: &OutageRecord) -> OutageRow {
    OutageRow {
        service: outage.service.clone(),
        workload: format!("{}/{}/{}", outage.cluster, outage.namespace, outage.workload),
        href: workload_href(&outage.cluster, &outage.namespace, &outage.workload),
        environment: outage.environment.clone().unwrap_or_default(),
        started: when(outage.started_at),
        ended: outage.ended_at.map_or_else(|| "Still down".into(), when),
        lasted: duration(
            (outage.ended_at.unwrap_or_else(Utc::now) - outage.started_at).num_seconds() as f64,
        ),
        detail: outage.detail.clone().unwrap_or_default(),
    }
}

pub struct WorkloadRow {
    pub name: String,
    pub href: String,
    pub namespace: String,
    pub cluster: String,
    pub environment: String,
    pub service: Option<String>,
    pub replicas: String,
    pub badge: &'static str,
    pub state: String,
    pub restarts: i64,
    pub cpu: String,
    pub memory: String,
    pub rolled: String,
}

fn workload_row(workload: &WorkloadRecord) -> WorkloadRow {
    let stats = &workload.stats;
    WorkloadRow {
        name: workload.name.clone(),
        href: workload_href(&workload.cluster, &workload.namespace, &workload.name),
        namespace: workload.namespace.clone(),
        cluster: workload.cluster.clone(),
        environment: workload.environment.clone().unwrap_or_else(|| "None".into()),
        service: workload.service.clone(),
        replicas: format!("{} of {}", stats.available.unwrap_or(0), stats.desired.unwrap_or(0)),
        badge: badge(&workload.state),
        state: state_words(&workload.state),
        restarts: stats.restarts,
        cpu: stats.cpu.map_or_else(|| "Not known".into(), cores),
        memory: stats.memory.map_or_else(|| "Not known".into(), bytes),
        rolled: workload.rolled_at.map_or_else(|| "Not seen".into(), ago),
    }
}

pub struct NamespaceRow {
    pub name: String,
    pub environment: String,
    pub production: bool,
    pub workloads: i64,
    pub pods: String,
    pub restarts: i64,
    pub cpu: String,
    pub memory: String,
}

fn namespace_row(namespace: &NamespaceRecord) -> NamespaceRow {
    let stats: &Stats = &namespace.stats;
    NamespaceRow {
        name: namespace.name.clone(),
        environment: namespace.environment.clone().unwrap_or_else(|| "None".into()),
        production: namespace.production,
        workloads: stats.workloads.unwrap_or(0),
        pods: format!("{} running of {}", stats.running, stats.pods),
        restarts: stats.restarts,
        cpu: stats.cpu.map_or_else(|| "Not known".into(), cores),
        memory: stats.memory.map_or_else(|| "Not known".into(), bytes),
    }
}

/// Whose leave the Catalogue is read with, for the page.
pub struct CatalogueView {
    pub by: Option<String>,
    pub read: Option<String>,
    pub services: usize,
    pub problem: Option<String>,
}

fn catalogue_view(reading: &Reading) -> CatalogueView {
    CatalogueView {
        by: reading.delegation.and(reading.by.clone()),
        read: reading.read_at.map(ago),
        services: reading.services,
        problem: reading.problem.clone(),
    }
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    flash: Flash,
    writes: bool,
    admin: bool,
    clusters: Vec<ClusterRow>,
    rollouts: Vec<RolloutRow>,
    outages: Vec<OutageRow>,
    catalogue: CatalogueView,
    manifest: &'static str,
    service_key: &'static str,
    repository_key: &'static str,
    commit_key: &'static str,
}

#[derive(Template)]
#[template(path = "cluster.html")]
struct ClusterPage {
    cluster: ClusterRow,
    namespaces: Vec<NamespaceRow>,
    workloads: Vec<WorkloadRow>,
}

pub struct Detail {
    pub key: &'static str,
    pub value: String,
}

#[derive(Template)]
#[template(path = "workload.html")]
struct WorkloadPage {
    cluster: String,
    cluster_href: String,
    title: String,
    badge: &'static str,
    state: String,
    message: Option<String>,
    service: Option<String>,
    details: Vec<Detail>,
    rollouts: Vec<RolloutRow>,
    outages: Vec<OutageRow>,
}

#[derive(Template)]
#[template(path = "panel.html")]
struct Panel {
    service: String,
    workloads: Vec<WorkloadRow>,
    rollouts: Vec<RolloutRow>,
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

async fn home(backend: &Backend, flash: Flash) -> Result<String, Refusal> {
    let config = Config::read(&backend.settings());
    let clusters: Vec<ClusterRecord> =
        backend.query_all(Query::new("clusters").order(Order::asc("id"))).await?;
    let rollouts = backend
        .query::<Rollout>(Query::new("rollouts").order(Order::desc("started_at")).limit(RECENT))
        .await?
        .records;
    let outages = backend
        .query::<OutageRecord>(Query::new("outages").order(Order::desc("started_at")).limit(RECENT))
        .await?
        .records;
    let admin = backend.caller().is_some_and(|caller| caller.admin);
    render(&HomePage {
        flash,
        writes: backend.writes(),
        admin,
        clusters: clusters.iter().map(|cluster| cluster_row(cluster, &config)).collect(),
        rollouts: rollouts.iter().map(rollout_row).collect(),
        outages: outages.iter().map(outage_row).collect(),
        catalogue: catalogue_view(&catalogue::reading(backend).await),
        manifest: MANIFEST,
        service_key: SERVICE,
        repository_key: REPOSITORY,
        commit_key: COMMIT,
    })
}

async fn cluster_page(backend: &Backend, name: &str) -> Result<String, Refusal> {
    let config = Config::read(&backend.settings());
    let cluster: ClusterRecord = backend
        .get("clusters", name)
        .await?
        .ok_or_else(|| Refusal::missing(format!("there is no cluster called {name}")))?;
    let of = |collection: &str| {
        Query::new(collection).filter(json!({ "cluster": name })).order(Order::asc("id"))
    };
    let namespaces: Vec<NamespaceRecord> = backend.query_all(of("namespaces")).await?;
    let workloads: Vec<WorkloadRecord> = backend.query_all(of("workloads")).await?;
    render(&ClusterPage {
        cluster: cluster_row(&cluster, &config),
        namespaces: namespaces.iter().map(namespace_row).collect(),
        workloads: workloads.iter().map(workload_row).collect(),
    })
}

async fn workload_page(
    backend: &Backend,
    cluster: &str,
    namespace: &str,
    name: &str,
) -> Result<String, Refusal> {
    let id = format!("{cluster}/{namespace}/{name}");
    let workload: WorkloadRecord = backend
        .get("workloads", id.as_str())
        .await?
        .ok_or_else(|| Refusal::missing(format!("there is no deployment {id}")))?;
    let about = json!({ "cluster": cluster, "namespace": namespace, "workload": name });
    let rollouts = backend
        .query::<Rollout>(
            Query::new("rollouts").filter(about.clone()).order(Order::desc("started_at")).limit(50),
        )
        .await?
        .records;
    let outages = backend
        .query::<OutageRecord>(
            Query::new("outages").filter(about).order(Order::desc("started_at")).limit(50),
        )
        .await?
        .records;
    let stats = &workload.stats;
    let mut details = vec![
        Detail {
            key: "Environment",
            value: workload.environment.clone().unwrap_or_else(|| {
                "None: give its cluster or namespace one in the settings".into()
            }),
        },
        Detail {
            key: "Pods available",
            value: format!(
                "{} of {} ({} ready, {} up to date)",
                stats.available.unwrap_or(0),
                stats.desired.unwrap_or(0),
                stats.ready.unwrap_or(0),
                stats.updated.unwrap_or(0)
            ),
        },
        Detail { key: "Restarts", value: stats.restarts.to_string() },
        Detail {
            key: "CPU",
            value: stats.cpu.map_or_else(
                || "Not known: metrics-server is not installed".into(),
                |cpu| format!("{} cores", cores(cpu)),
            ),
        },
        Detail { key: "Memory", value: stats.memory.map_or_else(|| "Not known".into(), bytes) },
        Detail { key: "Images", value: workload.images.join(", ") },
        Detail {
            key: "Commit",
            value: workload.commit.clone().unwrap_or_else(|| "Not known".into()),
        },
        Detail {
            key: "Repository",
            value: workload.repository.clone().unwrap_or_else(|| {
                "Not known: its service has no single repository in the Catalogue".into()
            }),
        },
    ];
    if let Some(revision) = workload.revision {
        details.push(Detail { key: "Revision", value: revision.to_string() });
    }
    if let Some(created) = workload.created_at {
        details.push(Detail { key: "Created", value: when(created) });
    }
    render(&WorkloadPage {
        cluster: cluster.to_string(),
        cluster_href: format!("/p/kubernetes/clusters/{cluster}"),
        title: format!("{namespace}/{name}"),
        badge: badge(&workload.state),
        state: state_words(&workload.state),
        message: workload.message.clone(),
        service: workload.service.clone(),
        details,
        rollouts: rollouts.iter().map(rollout_row).collect(),
        outages: outages.iter().map(outage_row).collect(),
    })
}

async fn panel(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let resource = parameter(&request.query, "resource").unwrap_or_default();
    let service = resource
        .split_once(':')
        .filter(|(kind, _)| kind.eq_ignore_ascii_case("service"))
        .map(|(_, name)| name.to_string())
        .ok_or_else(|| Refusal::bad("the panel is for a service: resource=service:<name>"))?;
    let workloads: Vec<WorkloadRecord> = backend
        .query_all(
            Query::new("workloads").filter(json!({ "service": service })).order(Order::asc("id")),
        )
        .await?;
    let rollouts = backend
        .query::<Rollout>(
            Query::new("rollouts")
                .filter(json!({ "service": service }))
                .order(Order::desc("started_at"))
                .limit(5),
        )
        .await?
        .records;
    render(&Panel {
        service,
        workloads: workloads.iter().map(workload_row).collect(),
        rollouts: rollouts.iter().map(rollout_row).collect(),
    })
}

pub async fn handle(
    backend: &Backend,
    watcher: &Watcher,
    request: &Request,
    path: &[&str],
) -> Response {
    let fragment = matches!(path, ["panel"]);
    match route(backend, watcher, request, path).await {
        Ok(html) => Response::html(html),
        Err(refusal) if fragment => {
            let text = format!(
                "<p class=\"doc-error-message\" role=\"alert\">{}</p>",
                escape(&refusal.detail)
            );
            Response::new(refusal.status, "text/html; charset=utf-8", text)
        }
        Err(refusal) => {
            let page = Blank { flash: Flash { notice: None, error: Some(refusal.detail.clone()) } };
            let html = page.render().unwrap_or_else(|_| escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

async fn route(
    backend: &Backend,
    watcher: &Watcher,
    request: &Request,
    path: &[&str],
) -> Result<String, Refusal> {
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => home(backend, Flash::default()).await,
        ("GET", ["panel"]) => panel(backend, request).await,
        ("GET", ["clusters", name]) => cluster_page(backend, name).await,
        ("GET", ["workloads", cluster, namespace, name]) => {
            workload_page(backend, cluster, namespace, name).await
        }
        ("POST", ["read"]) => {
            watcher.wake();
            let notice = "Every cluster is being read now; this page shows it within a minute.";
            home(backend, Flash { notice: Some(notice.into()), error: None }).await
        }
        ("POST", ["catalogue"]) => {
            let flash = match catalogue::grant(backend, &who(backend)).await {
                Ok(_) => Flash {
                    notice: Some(
                        "The Catalogue is being read as you, now and every few hours while you \
                         can read it."
                            .into(),
                    ),
                    error: None,
                },
                Err(err) => Flash { notice: None, error: Some(err.detail()) },
            };
            home(backend, flash).await
        }
        ("POST", ["catalogue", "stop"]) => {
            let flash = match catalogue::stop(backend).await {
                Ok(()) => Flash {
                    notice: Some(
                        "The Catalogue is no longer read; the repositories read last are kept."
                            .into(),
                    ),
                    error: None,
                },
                Err(err) => Flash { notice: None, error: Some(err.detail()) },
            };
            home(backend, flash).await
        }
        _ => Err(Refusal::missing("no such page")),
    }
}
