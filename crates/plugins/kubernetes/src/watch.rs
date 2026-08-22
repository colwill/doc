//! Reading each cluster, every little while, for as long as the plugin runs (its long-running
//! `run`). A read takes the cluster's nodes, namespaces, deployments, replica sets and pods, and
//! metrics-server's figures where it is installed, and works out:
//!
//! - each workload as it is now, and its namespace's and cluster's counts;
//! - a rollout for each revision of a deployment, from its replica sets, timed as Kubernetes timed
//!   it: started when its replica set was made, finished when the deployment's Progressing
//!   condition said the new replica set was available, failed when it said the deadline passed;
//!   a replica set taken up again under a new revision is a rollback;
//! - an outage whenever a production workload of a service has none of its pods available,
//!   from when its Available condition turned false until it turned true again.
//!
//! Only what changed is written, and DORA and CI/CD/CT are told which repositories to work out
//! again. What was already done when the plugin first read a deployment is kept as backfilled:
//! a deployment, but with no duration.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Mutex;
use std::time::Duration as Wait;

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, DataRequest, PluginError, Query};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::ID;
use crate::catalogue;
use crate::kube::{
    self, Connection, Deployment, Failed, Meta, Namespace, Node, Pod, PodMetrics, ReplicaSet,
    Template, Version,
};
use crate::settings::Config;
use crate::store::{
    ClusterRecord, FAILURE, IN_PROGRESS, NamespaceRecord, OutageRecord, Rollout, SUCCESS,
    SUPERSEDED, Stats, WorkloadRecord,
};

/// The longest one cluster's read may take.
const READ_TIMEOUT: Wait = Wait::from_secs(60);
const BATCH: usize = 100;
/// History older than this is forgotten, once a day.
const KEEP_DAYS: i64 = 400;

/// Names a workload's service, overriding its labels.
pub const SERVICE: &str = "rundoc.sh/service";
/// Names a workload's repository, as owner/name, when the Catalogue should not be asked.
pub const REPOSITORY: &str = "rundoc.sh/repository";
/// The commit a pod template was built from.
pub const COMMIT: &str = "rundoc.sh/commit";
const NAME_LABEL: &str = "app.kubernetes.io/name";
const VERSION_LABEL: &str = "app.kubernetes.io/version";

#[derive(Default)]
struct Memory {
    /// `collection:id` to what was last written, so only what changed is written again.
    written: HashMap<String, Value>,
    rollouts: HashMap<String, Rollout>,
    /// Each workload's open outage.
    open: HashMap<String, OutageRecord>,
    /// Clusters whose records have been read back since the plugin started.
    loaded: HashSet<String>,
    forgotten_at: Option<DateTime<Utc>>,
}

#[derive(Default)]
pub struct Watcher {
    nudge: Notify,
    stop: Mutex<CancellationToken>,
    memory: tokio::sync::Mutex<Memory>,
}

/// What one read comes to: records to write and delete, and whom to tell.
#[derive(Default)]
struct Plan {
    writes: Vec<DataRequest>,
    /// Repositories with a production rollout that changed, and the earliest one's start.
    delivery: BTreeMap<String, DateTime<Utc>>,
    pipelines: BTreeMap<String, DateTime<Utc>>,
    events: Vec<(String, Value)>,
}

impl Plan {
    fn earliest(map: &mut BTreeMap<String, DateTime<Utc>>, repository: &str, at: DateTime<Utc>) {
        let held = map.entry(repository.to_string()).or_insert(at);
        *held = (*held).min(at);
    }
}

impl Memory {
    /// Writes `value` as record `id` of `collection`, unless it is what was written last.
    fn put<T: Serialize>(&mut self, plan: &mut Plan, collection: &str, id: &str, value: &T) {
        let value = json!(value);
        let key = format!("{collection}:{id}");
        if self.written.get(&key) == Some(&value) {
            return;
        }
        plan.writes.push(DataRequest::upsert(collection, &["id"], value.clone()));
        self.written.insert(key, value);
    }

    fn remember<T: Serialize>(&mut self, collection: &str, id: &str, value: &T) {
        self.written.insert(format!("{collection}:{id}"), json!(value));
    }

    /// Deletes the records of `collection` in `cluster` that this read did not see.
    fn prune(&mut self, plan: &mut Plan, collection: &str, cluster: &str, seen: &HashSet<String>) {
        let prefix = format!("{collection}:{cluster}/");
        let gone: Vec<String> = self
            .written
            .keys()
            .filter(|key| key.starts_with(&prefix) && !seen.contains(&key[collection.len() + 1..]))
            .cloned()
            .collect();
        for key in gone {
            self.written.remove(&key);
            plan.writes.push(DataRequest::delete(collection, &key[collection.len() + 1..]));
        }
    }
}

/// Everything read from one cluster.
struct Read {
    server: String,
    insecure: bool,
    version: Version,
    nodes: Vec<Node>,
    namespaces: Vec<Namespace>,
    deployments: Vec<Deployment>,
    replica_sets: Vec<ReplicaSet>,
    pods: Vec<Pod>,
    metrics: Option<Vec<PodMetrics>>,
}

async fn fetch(kubeconfig: &str) -> Result<Read, String> {
    let connection = Connection::parse(kubeconfig)?;
    let api = connection.client()?;
    let detail = |failed: Failed| failed.detail;
    let (version, nodes, namespaces, deployments, replica_sets, pods) = tokio::try_join!(
        api.get::<Version>("version"),
        api.list::<Node>("api/v1/nodes"),
        api.list::<Namespace>("api/v1/namespaces"),
        api.list::<Deployment>("apis/apps/v1/deployments"),
        api.list::<ReplicaSet>("apis/apps/v1/replicasets"),
        api.list::<Pod>("api/v1/pods"),
    )
    .map_err(detail)?;
    // Only where metrics-server is installed, and the service account may read what it says.
    let metrics = api.list::<PodMetrics>("apis/metrics.k8s.io/v1beta1/pods").await.ok();
    Ok(Read {
        server: connection.server.host_str().unwrap_or_default().to_string(),
        insecure: connection.insecure(),
        version,
        nodes,
        namespaces,
        deployments,
        replica_sets,
        pods,
        metrics,
    })
}

/// A setting on the deployment, or on its pod template.
fn annotation(meta: &Meta, template: &Template, key: &str) -> Option<String> {
    meta.annotations
        .get(key)
        .or_else(|| template.metadata.annotations.get(key))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn label(meta: &Meta, template: &Template, key: &str) -> Option<String> {
    meta.labels
        .get(key)
        .or_else(|| template.metadata.labels.get(key))
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// A commit's SHA, full or short, perhaps written `sha-…` as image tags often are.
fn sha(text: &str) -> Option<String> {
    let text = text.trim();
    let text = text.strip_prefix("sha-").unwrap_or(text);
    (7..=40).contains(&text.len()).then_some(())?;
    text.chars().all(|c| c.is_ascii_hexdigit()).then(|| text.to_ascii_lowercase())
}

/// The commit a template was built from: `rundoc.sh/commit`, else a version label or an image tag
/// that is a SHA.
fn commit_of(template: &Template, deployment: Option<&Meta>) -> Option<String> {
    if let Some(commit) = template
        .metadata
        .annotations
        .get(COMMIT)
        .or_else(|| deployment.and_then(|meta| meta.annotations.get(COMMIT)))
    {
        return sha(commit);
    }
    if let Some(version) = template.metadata.labels.get(VERSION_LABEL).and_then(|v| sha(v)) {
        return Some(version);
    }
    template.spec.containers.iter().find_map(|container| {
        let image = container.image.split('@').next().unwrap_or_default();
        let (_, tag) = image.rsplit_once(':').filter(|(name, _)| !name.ends_with('/'))?;
        sha(tag)
    })
}

/// The one repository of a service; with several, the one named like its image or itself.
fn repository_of(
    mapping: &BTreeMap<String, Vec<String>>,
    service: &str,
    images: &[String],
) -> Option<String> {
    let repositories = mapping.get(service)?;
    if let [only] = repositories.as_slice() {
        return Some(only.clone());
    }
    let named = |repository: &&String| {
        let short = repository.rsplit('/').next().unwrap_or(repository);
        short.eq_ignore_ascii_case(service)
            || images.iter().any(|image| {
                let path = image.split(['@', ':']).next().unwrap_or(image);
                path.rsplit('/').next().is_some_and(|last| last.eq_ignore_ascii_case(short))
            })
    };
    let matching: Vec<&String> = repositories.iter().filter(named).collect();
    match matching.as_slice() {
        [only] => Some((*only).clone()),
        _ => None,
    }
}

fn count(stats: &mut Stats, pod: &Pod, used: Option<(f64, f64)>) {
    stats.pods += 1;
    match pod.status.phase.as_str() {
        "Running" => stats.running += 1,
        "Pending" => stats.pending += 1,
        "Failed" => stats.failed += 1,
        _ => {}
    }
    stats.restarts += pod.restarts();
    if let Some((cpu, memory)) = used {
        stats.cpu = Some(stats.cpu.unwrap_or(0.0) + cpu);
        stats.memory = Some(stats.memory.unwrap_or(0.0) + memory);
    }
}

pub fn page(cluster: &str, namespace: &str, name: &str) -> String {
    let base = std::env::var("DOC_PUBLIC_URL").unwrap_or_default();
    format!("{}/p/kubernetes/workloads/{cluster}/{namespace}/{name}", base.trim_end_matches('/'))
}

impl Watcher {
    pub fn wake(&self) {
        self.nudge.notify_one();
    }

    pub fn stop(&self) {
        if let Ok(stop) = self.stop.lock() {
            stop.cancel();
        }
        self.wake();
    }

    pub async fn run(&self, backend: &Backend) -> Result<(), PluginError> {
        let stop = CancellationToken::new();
        if let Ok(mut held) = self.stop.lock() {
            *held = stop.clone();
        }
        tracing::info!("reading clusters");
        while !stop.is_cancelled() {
            let config = Config::read(&backend.settings());
            catalogue::when_due(backend).await;
            let mapping = catalogue::mapping(backend).await;
            for cluster in &config.clusters {
                let read = tokio::select! {
                    () = stop.cancelled() => None,
                    read = self.read_cluster(backend, &config, &mapping, cluster) => Some(read),
                };
                match read {
                    None => break,
                    Some(Err(problem)) => {
                        tracing::warn!(cluster, %problem, "a cluster could not be read");
                    }
                    Some(Ok(())) => {}
                }
            }
            if let Err(err) = self.tidy(backend, &config).await {
                tracing::warn!(%err, "clusters no longer configured could not be tidied away");
            }
            tokio::select! {
                () = stop.cancelled() => {}
                () = self.nudge.notified() => {}
                () = tokio::time::sleep(config.interval) => {}
            }
        }
        tracing::info!("stopped reading clusters");
        Ok(())
    }

    /// Reads back what is kept for a cluster, so a restarted plugin carries on where it was.
    async fn load(
        &self,
        backend: &Backend,
        memory: &mut Memory,
        cluster: &str,
    ) -> Result<(), PluginError> {
        let of = |collection: &str| Query::new(collection).filter(json!({ "cluster": cluster }));
        for workload in backend.query_all::<WorkloadRecord>(of("workloads")).await? {
            memory.remember("workloads", &workload.id, &workload);
        }
        for namespace in backend.query_all::<NamespaceRecord>(of("namespaces")).await? {
            memory.remember("namespaces", &namespace.id, &namespace);
        }
        for rollout in backend.query_all::<Rollout>(of("rollouts")).await? {
            memory.rollouts.insert(rollout.id.clone(), rollout);
        }
        for outage in backend.query_all::<OutageRecord>(of("outages")).await? {
            if outage.ended_at.is_none() {
                memory.open.insert(
                    format!("{}/{}/{}", outage.cluster, outage.namespace, outage.workload),
                    outage,
                );
            }
        }
        memory.loaded.insert(cluster.to_string());
        Ok(())
    }

    async fn read_cluster(
        &self,
        backend: &Backend,
        config: &Config,
        mapping: &BTreeMap<String, Vec<String>>,
        cluster: &str,
    ) -> Result<(), String> {
        let Some(kubeconfig) = backend.settings().named(cluster) else { return Ok(()) };
        let polled_at = Utc::now();
        let fetched = tokio::time::timeout(READ_TIMEOUT, fetch(kubeconfig.expose()))
            .await
            .unwrap_or_else(|_| Err("reading it took longer than a minute".into()));
        let mut memory = self.memory.lock().await;
        if !memory.loaded.contains(cluster) {
            self.load(backend, &mut memory, cluster).await.map_err(|err| {
                format!("what is kept for it could not be read: {}", err.detail())
            })?;
        }
        let mut plan = Plan::default();
        let environment = config.environments.get(&cluster.to_ascii_lowercase()).cloned();
        let read = match fetched {
            Ok(read) => read,
            Err(problem) => {
                // The last counts stay, marked as not current.
                let key = format!("clusters:{cluster}");
                let before: Option<ClusterRecord> = memory
                    .written
                    .get(&key)
                    .and_then(|value| serde_json::from_value(value.clone()).ok());
                let record = ClusterRecord {
                    id: cluster.to_string(),
                    environment,
                    state: "error".into(),
                    problem: Some(problem.clone()),
                    polled_at,
                    ..before.unwrap_or(ClusterRecord {
                        id: cluster.to_string(),
                        server: None,
                        environment: None,
                        state: "error".into(),
                        problem: None,
                        insecure: false,
                        version: None,
                        stats: Stats::default(),
                        polled_at,
                    })
                };
                memory.put(&mut plan, "clusters", cluster, &record);
                self.write(backend, plan).await.map_err(|err| err.detail())?;
                return Err(problem);
            }
        };
        self.observe(
            config,
            mapping,
            cluster,
            environment,
            &read,
            polled_at,
            &mut memory,
            &mut plan,
        );
        drop(memory);
        self.write(backend, plan).await.map_err(|err| err.detail())
    }

    /// Works out every record one read comes to.
    #[allow(clippy::too_many_arguments)]
    fn observe(
        &self,
        config: &Config,
        mapping: &BTreeMap<String, Vec<String>>,
        cluster: &str,
        cluster_environment: Option<String>,
        read: &Read,
        polled_at: DateTime<Utc>,
        memory: &mut Memory,
        plan: &mut Plan,
    ) {
        let usage: HashMap<(&str, &str), (f64, f64)> = read
            .metrics
            .iter()
            .flatten()
            .map(|pod| {
                let (mut cpu, mut bytes) = (0.0, 0.0);
                for container in &pod.containers {
                    cpu +=
                        container.usage.get("cpu").and_then(|q| kube::quantity(q)).unwrap_or(0.0);
                    bytes += container
                        .usage
                        .get("memory")
                        .and_then(|q| kube::quantity(q))
                        .unwrap_or(0.0);
                }
                ((pod.metadata.namespace.as_str(), pod.metadata.name.as_str()), (cpu, bytes))
            })
            .collect();
        let used = |pod: &Pod| {
            read.metrics.as_ref()?;
            Some(
                usage
                    .get(&(pod.metadata.namespace.as_str(), pod.metadata.name.as_str()))
                    .copied()
                    .unwrap_or((0.0, 0.0)),
            )
        };
        // Replica sets by the deployment they belong to, and each by namespace and name.
        let mut sets: HashMap<&str, Vec<&ReplicaSet>> = HashMap::new();
        let mut owner: HashMap<(&str, &str), &str> = HashMap::new();
        for set in &read.replica_sets {
            if let Some(uid) = set.deployment() {
                sets.entry(uid).or_default().push(set);
                owner.insert((set.metadata.namespace.as_str(), set.metadata.name.as_str()), uid);
            }
        }
        // When each replica set's oldest pod was made: when a replica set taken up again, which
        // keeps its old creation time, began its rollout.
        let mut born: HashMap<(&str, &str), DateTime<Utc>> = HashMap::new();
        for pod in &read.pods {
            let (Some(set), Some(at)) = (pod.replica_set(), pod.metadata.creation_timestamp) else {
                continue;
            };
            let held = born.entry((pod.metadata.namespace.as_str(), set)).or_insert(at);
            *held = (*held).min(at);
        }
        let mut by_deployment: HashMap<&str, Stats> = HashMap::new();
        let mut by_namespace: HashMap<&str, Stats> = HashMap::new();
        let mut whole = Stats::default();
        for pod in read.pods.iter().filter(|pod| !config.ignores(&pod.metadata.namespace)) {
            let used = used(pod);
            count(&mut whole, pod, used);
            count(by_namespace.entry(pod.metadata.namespace.as_str()).or_default(), pod, used);
            if let Some(uid) =
                pod.replica_set().and_then(|set| owner.get(&(pod.metadata.namespace.as_str(), set)))
            {
                count(by_deployment.entry(uid).or_default(), pod, used);
            }
        }

        let mut workloads_seen = HashSet::new();
        let mut rollouts_seen = HashSet::new();
        let mut workloads_in: HashMap<&str, i64> = HashMap::new();
        for deployment in read
            .deployments
            .iter()
            .filter(|deployment| !config.ignores(&deployment.metadata.namespace))
        {
            let meta = &deployment.metadata;
            let (namespace, name) = (meta.namespace.as_str(), meta.name.as_str());
            *workloads_in.entry(namespace).or_default() += 1;
            let id = format!("{cluster}/{namespace}/{name}");
            workloads_seen.insert(id.clone());
            let environment = config.environment_of(cluster, namespace);
            let production = config.is_production(environment.as_deref());
            let template = &deployment.spec.template;
            let images: Vec<String> =
                template.spec.containers.iter().map(|container| container.image.clone()).collect();
            let service =
                annotation(meta, template, SERVICE).or_else(|| label(meta, template, NAME_LABEL));
            let repository = annotation(meta, template, REPOSITORY)
                .map(|repository| repository.to_ascii_lowercase())
                .or_else(|| service.as_deref().and_then(|s| repository_of(mapping, s, &images)));
            let status = &deployment.status;
            let desired = deployment.desired();
            let progressing = deployment.condition("Progressing");
            let available = deployment.condition("Available");
            let complete = status.observed_generation >= meta.generation
                && status.updated_replicas >= desired
                && status.available_replicas >= desired
                && status.replicas <= status.updated_replicas
                && (desired == 0
                    || progressing.is_none_or(|p| p.reason == "NewReplicaSetAvailable"));
            let failed = progressing.is_some_and(|p| p.status == "False");
            let state = match () {
                () if desired == 0 => "scaled-down",
                () if failed => "failed",
                () if status.available_replicas == 0 => "unavailable",
                () if !complete => "progressing",
                () if status.available_replicas < desired => "degraded",
                () => "available",
            };
            let message = match state {
                "failed" => progressing.map(|p| p.message.clone()),
                "unavailable" | "degraded" => available.map(|a| a.message.clone()),
                _ => None,
            }
            .filter(|message| !message.is_empty());

            // Its rollouts, one for each revision a replica set holds.
            let uid: String = meta.uid.chars().take(8).collect();
            let current = deployment.revision();
            let url = page(cluster, namespace, name);
            let mut rolled_at: Option<DateTime<Utc>> = None;
            let own: Vec<&ReplicaSet> = sets.get(meta.uid.as_str()).cloned().unwrap_or_default();
            for set in &own {
                let Some(revision) = set.revision() else { continue };
                // A rollback takes up a template first rolled out before the one it replaces, as
                // `kubectl rollout undo` does; bringing back a later one is rolling forward.
                let replaced = own.iter().find(|other| other.served(revision - 1));
                let rollback =
                    set.reused() && replaced.is_some_and(|replaced| set.first() < replaced.first());
                let rid = format!("{id}/{uid}/{revision}");
                rollouts_seen.insert(rid.clone());
                let known = memory.rollouts.get(&rid).cloned();
                let pods_born = born.get(&(namespace, set.metadata.name.as_str())).copied();
                let is_current = Some(revision) == current;
                let set_template = &set.spec.template;
                let mut rollout = known.clone().unwrap_or_else(|| Rollout {
                    id: rid.clone(),
                    kind: "rollout".into(),
                    cluster: cluster.to_string(),
                    namespace: namespace.to_string(),
                    workload: name.to_string(),
                    environment: None,
                    production: false,
                    service: None,
                    repository: None,
                    sha: String::new(),
                    image: None,
                    revision,
                    rollback,
                    state: IN_PROGRESS.into(),
                    reason: None,
                    // A replica set taken up again was made long ago: its rollout started with
                    // its oldest pod, or now if it has none yet.
                    started_at: match set.reused() {
                        true => pods_born.unwrap_or(polled_at).min(polled_at),
                        false => set.metadata.creation_timestamp.unwrap_or(polled_at),
                    },
                    finished_at: None,
                    seconds: None,
                    backfilled: false,
                    url: None,
                });
                rollout.rollback = rollback;
                rollout.environment.clone_from(&environment);
                rollout.production = production;
                rollout.service.clone_from(&service);
                if repository.is_some() {
                    rollout.repository.clone_from(&repository);
                }
                // Kept as first read, since a deployment's own annotation is only its current one's.
                if rollout.sha.is_empty() {
                    rollout.sha =
                        commit_of(set_template, is_current.then_some(meta)).unwrap_or_default();
                }
                rollout.image =
                    set_template.spec.containers.first().map(|container| container.image.clone());
                rollout.url = Some(url.clone());
                if is_current && !rollout.terminal() {
                    if complete {
                        let done = progressing
                            .filter(|p| p.reason == "NewReplicaSetAvailable")
                            .and_then(|p| p.last_update_time)
                            .unwrap_or(polled_at);
                        rollout.state = SUCCESS.into();
                        // Found done and taken up again with no pods to tell when it started.
                        if known.is_none() && set.reused() && pods_born.is_none() {
                            rollout.started_at = done;
                            rollout.backfilled = true;
                        }
                        rollout.finished_at = Some(done.max(rollout.started_at));
                    } else if failed {
                        let at = progressing.and_then(|p| p.last_update_time).unwrap_or(polled_at);
                        rollout.state = FAILURE.into();
                        rollout.finished_at = Some(at.max(rollout.started_at));
                        rollout.reason = progressing.map(|p| p.message.clone());
                    }
                } else if !is_current && current.is_some_and(|current| revision < current) {
                    match &known {
                        None => {
                            rollout.state = SUCCESS.into();
                            rollout.finished_at = Some(rollout.started_at);
                            rollout.backfilled = true;
                        }
                        Some(known) if !known.terminal() => {
                            rollout.state = SUPERSEDED.into();
                            rollout.finished_at = Some(polled_at);
                            rollout.reason =
                                Some("a later revision started before it finished".into());
                        }
                        Some(_) => {}
                    }
                }
                rollout.seconds = match (rollout.backfilled, rollout.finished_at) {
                    (false, Some(finished)) => {
                        Some((finished - rollout.started_at).num_milliseconds() as f64 / 1000.0)
                    }
                    _ => None,
                };
                if rollout.state == SUCCESS {
                    rolled_at = rolled_at.max(rollout.finished_at);
                }
                if known.as_ref() != Some(&rollout) {
                    self.record_rollout(memory, plan, &rollout, known.as_ref());
                }
            }

            // An outage: a production workload of a service, meant to run, with nothing available.
            let down =
                production && service.is_some() && desired > 0 && status.available_replicas == 0;
            match (down, memory.open.get(&id).cloned()) {
                (true, None) => {
                    let started_at = available
                        .filter(|a| a.status == "False")
                        .and_then(|a| a.last_transition_time)
                        .unwrap_or(polled_at);
                    let outage = OutageRecord {
                        id: format!("{id}/{}", started_at.timestamp()),
                        service: service.clone().unwrap_or_default(),
                        environment: environment.clone(),
                        cluster: cluster.to_string(),
                        namespace: namespace.to_string(),
                        workload: name.to_string(),
                        started_at,
                        ended_at: None,
                        detail: message.clone().or_else(|| Some("no pods are available".into())),
                    };
                    self.record_outage(memory, plan, &id, outage, "started");
                }
                (false, Some(mut outage)) => {
                    let ended_at = available
                        .filter(|a| a.status == "True")
                        .and_then(|a| a.last_transition_time)
                        .filter(|at| *at >= outage.started_at)
                        .unwrap_or(polled_at);
                    outage.ended_at = Some(ended_at);
                    self.record_outage(memory, plan, &id, outage, "ended");
                }
                _ => {}
            }

            let mut stats = by_deployment.remove(meta.uid.as_str()).unwrap_or_default();
            stats.desired = Some(desired);
            stats.ready = Some(status.ready_replicas);
            stats.available = Some(status.available_replicas);
            stats.updated = Some(status.updated_replicas);
            let workload = WorkloadRecord {
                id: id.clone(),
                cluster: cluster.to_string(),
                namespace: namespace.to_string(),
                name: name.to_string(),
                environment,
                production,
                service,
                repository,
                commit: commit_of(template, Some(meta)),
                images,
                revision: current,
                state: state.to_string(),
                message,
                stats,
                created_at: meta.creation_timestamp,
                rolled_at,
            };
            memory.put(plan, "workloads", &id, &workload);
        }

        // Rollouts of deployments that are gone, never seen through, and outages of them.
        let stranded: Vec<Rollout> = memory
            .rollouts
            .values()
            .filter(|rollout| rollout.cluster == cluster && !rollout.terminal())
            .filter(|rollout| !rollouts_seen.contains(&rollout.id))
            .cloned()
            .collect();
        for known in stranded {
            let mut rollout = known.clone();
            rollout.state = SUPERSEDED.into();
            rollout.finished_at = Some(polled_at);
            rollout.reason = Some("its deployment or replica set was removed".into());
            self.record_rollout(memory, plan, &rollout, Some(&known));
        }
        let vanished: Vec<(String, OutageRecord)> = memory
            .open
            .iter()
            .filter(|(id, outage)| outage.cluster == cluster && !workloads_seen.contains(*id))
            .map(|(id, outage)| (id.clone(), outage.clone()))
            .collect();
        for (id, mut outage) in vanished {
            outage.ended_at = Some(polled_at);
            outage.detail = Some("the deployment was removed".into());
            self.record_outage(memory, plan, &id, outage, "ended");
        }
        memory.prune(plan, "workloads", cluster, &workloads_seen);

        let mut namespaces_seen = HashSet::new();
        for namespace in
            read.namespaces.iter().filter(|namespace| !config.ignores(&namespace.metadata.name))
        {
            let name = namespace.metadata.name.as_str();
            let id = format!("{cluster}/{name}");
            namespaces_seen.insert(id.clone());
            let environment = config.environment_of(cluster, name);
            let mut stats = by_namespace.remove(name).unwrap_or_default();
            stats.workloads = Some(workloads_in.get(name).copied().unwrap_or(0));
            let record = NamespaceRecord {
                id: id.clone(),
                cluster: cluster.to_string(),
                name: name.to_string(),
                production: config.is_production(environment.as_deref()),
                environment,
                phase: Some(namespace.status.phase.clone()).filter(|phase| !phase.is_empty()),
                stats,
                created_at: namespace.metadata.creation_timestamp,
            };
            memory.put(plan, "namespaces", &id, &record);
        }
        memory.prune(plan, "namespaces", cluster, &namespaces_seen);

        let allocatable = |key: &str| -> Option<f64> {
            let values: Vec<f64> = read
                .nodes
                .iter()
                .filter_map(|node| node.status.allocatable.get(key).and_then(|q| kube::quantity(q)))
                .collect();
            (!values.is_empty()).then(|| values.iter().sum())
        };
        whole.nodes = Some(read.nodes.len() as i64);
        whole.nodes_ready = Some(read.nodes.iter().filter(|node| node.ready()).count() as i64);
        whole.namespaces = Some(namespaces_seen.len() as i64);
        whole.workloads = Some(workloads_seen.len() as i64);
        whole.cpu_allocatable = allocatable("cpu");
        whole.memory_allocatable = allocatable("memory");
        let record = ClusterRecord {
            id: cluster.to_string(),
            server: Some(read.server.clone()),
            environment: cluster_environment,
            state: "ok".into(),
            problem: None,
            insecure: read.insecure,
            version: Some(read.version.git_version.clone()).filter(|version| !version.is_empty()),
            stats: whole,
            // Written every read, so the page can say how fresh it is.
            polled_at,
        };
        memory.put(plan, "clusters", cluster, &record);
    }

    fn record_rollout(
        &self,
        memory: &mut Memory,
        plan: &mut Plan,
        rollout: &Rollout,
        known: Option<&Rollout>,
    ) {
        plan.writes.push(DataRequest::upsert("rollouts", &["id"], json!(rollout)));
        memory.rollouts.insert(rollout.id.clone(), rollout.clone());
        if let Some(repository) = &rollout.repository {
            if rollout.production && rollout.terminal() {
                Plan::earliest(&mut plan.delivery, repository, rollout.started_at);
            }
            if let Some(run) = rollout.as_run() {
                plan.writes.push(DataRequest::upsert("workflow-runs", &["id"], run));
                Plan::earliest(&mut plan.pipelines, repository, rollout.started_at);
            }
        }
        let ended = rollout.terminal() && !known.is_some_and(Rollout::terminal);
        if ended && !rollout.backfilled {
            plan.events.push((
                format!("plugin.{ID}.rollout.{}", rollout.state.replace('_', "-")),
                json!({
                    "cluster": rollout.cluster,
                    "namespace": rollout.namespace,
                    "workload": rollout.workload,
                    "environment": rollout.environment,
                    "service": rollout.service,
                    "repository": rollout.repository,
                    "revision": rollout.revision,
                    "sha": rollout.sha,
                    "rollback": rollout.rollback,
                    "seconds": rollout.seconds,
                    "url": rollout.url,
                }),
            ));
        }
    }

    fn record_outage(
        &self,
        memory: &mut Memory,
        plan: &mut Plan,
        workload: &str,
        outage: OutageRecord,
        what: &str,
    ) {
        plan.writes.push(DataRequest::upsert("outages", &["id"], json!(outage)));
        plan.events.push((
            format!("plugin.{ID}.outage.{what}"),
            json!({
                "service": outage.service,
                "environment": outage.environment,
                "cluster": outage.cluster,
                "namespace": outage.namespace,
                "workload": outage.workload,
                "started_at": outage.started_at,
                "ended_at": outage.ended_at,
                "detail": outage.detail,
            }),
        ));
        match outage.ended_at {
            Some(_) => memory.open.remove(workload),
            None => memory.open.insert(workload.to_string(), outage),
        };
    }

    async fn write(&self, backend: &Backend, plan: Plan) -> Result<(), PluginError> {
        for chunk in plan.writes.chunks(BATCH) {
            backend.batch(chunk.to_vec()).await?;
        }
        for (repository, since) in plan.delivery {
            let payload = json!({ "repository": repository, "since": since });
            if let Err(err) =
                backend.publish(&format!("plugin.{ID}.delivery.synced"), payload).await
            {
                tracing::warn!(%err, repository, "DORA was not told of new rollouts");
            }
        }
        for (repository, since) in plan.pipelines {
            let payload = json!({ "repository": repository, "since": since });
            if let Err(err) =
                backend.publish(&format!("plugin.{ID}.pipelines.synced"), payload).await
            {
                tracing::warn!(%err, repository, "CI/CD/CT was not told of new rollouts");
            }
        }
        for (topic, payload) in plan.events {
            if let Err(err) = backend.publish(&topic, payload).await {
                tracing::warn!(%err, topic, "an event was not published");
            }
        }
        Ok(())
    }

    /// Takes away clusters no longer in the settings, closing their outages, and forgets old
    /// history once a day.
    async fn tidy(&self, backend: &Backend, config: &Config) -> Result<(), PluginError> {
        let kept: Vec<ClusterRecord> = backend.query_all(Query::new("clusters")).await?;
        let now = Utc::now();
        for gone in kept.iter().filter(|record| !config.clusters.contains(&record.id)) {
            let cluster = gone.id.as_str();
            let open: Vec<OutageRecord> = backend
                .query_all(Query::new("outages").filter(json!({ "cluster": cluster })))
                .await?;
            for mut outage in open.into_iter().filter(|outage| outage.ended_at.is_none()) {
                outage.ended_at = Some(now);
                outage.detail = Some("its cluster was taken out of DOC".into());
                backend.upsert::<Value>("outages", &["id"], json!(outage)).await?;
            }
            for collection in ["workloads", "namespaces"] {
                backend.delete_where(collection, "id", json!({ "cluster": cluster })).await?;
            }
            backend.delete("clusters", cluster, None).await?;
            let mut memory = self.memory.lock().await;
            memory.loaded.remove(cluster);
            let own = |key: &str| {
                key == format!("clusters:{cluster}")
                    || key.starts_with(&format!("workloads:{cluster}/"))
                    || key.starts_with(&format!("namespaces:{cluster}/"))
            };
            memory.written.retain(|key, _| !own(key));
            memory.rollouts.retain(|_, rollout| rollout.cluster != cluster);
            memory.open.retain(|_, outage| outage.cluster != cluster);
            tracing::info!(cluster, "a cluster taken out of the settings was tidied away");
        }
        let mut memory = self.memory.lock().await;
        if memory.forgotten_at.is_none_or(|at| now - at > Duration::days(1)) {
            memory.forgotten_at = Some(now);
            drop(memory);
            let before = now - Duration::days(KEEP_DAYS);
            let old = json!({ "lt": before });
            backend.delete_where("rollouts", "id", json!({ "started_at": old })).await?;
            backend.delete_where("workflow-runs", "id", json!({ "finished_at": old })).await?;
            backend.delete_where("outages", "id", json!({ "ended_at": old })).await?;
        }
        Ok(())
    }
}
