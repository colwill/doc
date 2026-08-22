//! What the plugin keeps from reading its clusters. Clusters, namespaces and workloads are how they
//! were at the last read. Rollouts and outages are history, kept as Kubernetes timed them:
//!
//! - `rollouts` are exported to `dora`, whose production deployments they are;
//! - `workflow-runs` are the same rollouts as CI/CD/CT reads pipelines, exported to `cicd`, where
//!   a workflow named `rollout …` counts as continuous delivery;
//! - `outages` are exported to `reliability`: a production workload of a service with none of its
//!   pods available.
//!
//! `services` is the Catalogue's map of each service to its repositories, read as whoever let the
//! plugin do that.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Collection, Declaration, Export, Field};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub const IN_PROGRESS: &str = "in_progress";
pub const SUCCESS: &str = "success";
pub const FAILURE: &str = "failure";
pub const SUPERSEDED: &str = "superseded";

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "clusters",
            Collection::new()
                .field("id", Field::text().key().describe("The cluster's name"))
                .field("server", Field::text())
                .field("environment", Field::text())
                .field("state", Field::text().required().one_of(&["ok", "error"]))
                .field("problem", Field::text())
                .field("insecure", Field::boolean().required().default(json!(false)))
                .field("version", Field::text())
                .field("stats", Field::json().required().default(json!({})))
                .field("polled_at", Field::timestamp().required()),
        )
        .collection(
            "namespaces",
            Collection::new()
                .field("id", Field::text().key().describe("cluster/namespace"))
                .field("cluster", Field::text().required())
                .field("name", Field::text().required())
                .field("environment", Field::text())
                .field("production", Field::boolean().required().default(json!(false)))
                .field("phase", Field::text())
                .field("stats", Field::json().required().default(json!({})))
                .field("created_at", Field::timestamp())
                .index(&["cluster"]),
        )
        .collection(
            "workloads",
            Collection::new()
                .field("id", Field::text().key().describe("cluster/namespace/name"))
                .field("cluster", Field::text().required())
                .field("namespace", Field::text().required())
                .field("name", Field::text().required())
                .field("environment", Field::text())
                .field("production", Field::boolean().required().default(json!(false)))
                .field("service", Field::text())
                .field("repository", Field::text())
                .field("images", Field::json().required().default(json!([])))
                .field("commit", Field::text())
                .field("revision", Field::integer())
                .field("state", Field::text().required())
                .field("message", Field::text())
                .field("stats", Field::json().required().default(json!({})))
                .field("created_at", Field::timestamp())
                .field("rolled_at", Field::timestamp())
                .index(&["cluster"])
                .index(&["service"]),
        )
        .collection(
            "rollouts",
            Collection::new()
                .field("id", Field::text().key().describe("cluster/namespace/name/uid/revision"))
                .field("kind", Field::text().required().default(json!("rollout")))
                .field("cluster", Field::text().required())
                .field("namespace", Field::text().required())
                .field("workload", Field::text().required())
                .field("environment", Field::text())
                .field("production", Field::boolean().required().default(json!(false)))
                .field("service", Field::text())
                .field("repository", Field::text().describe("owner/name, in lower case"))
                .field("sha", Field::text().required().default(json!("")))
                .field("image", Field::text())
                .field("revision", Field::integer().required())
                .field(
                    "rollback",
                    Field::boolean()
                        .required()
                        .default(json!(false))
                        .describe("It took an earlier template up again, as a rollout undo does"),
                )
                .field(
                    "state",
                    Field::text().required().one_of(&[IN_PROGRESS, SUCCESS, FAILURE, SUPERSEDED]),
                )
                .field("reason", Field::text())
                .field("started_at", Field::timestamp().required())
                .field("finished_at", Field::timestamp())
                .field("seconds", Field::number())
                .field(
                    "backfilled",
                    Field::boolean()
                        .required()
                        .default(json!(false))
                        .describe("Found already done when first read, so its duration is unknown"),
                )
                .field("url", Field::text())
                .index(&["started_at"])
                .index(&["finished_at"])
                .index(&["repository", "finished_at"])
                .index(&["service"])
                .export(Export::to(&["dora"])),
        )
        .collection(
            "workflow-runs",
            Collection::new()
                .field("id", Field::text().key())
                .field("repository", Field::text().required())
                .field("workflow_id", Field::integer().required())
                .field("workflow", Field::text().required())
                .field("path", Field::text())
                .field("event", Field::text())
                .field("branch", Field::text())
                .field("default_branch", Field::boolean().required().default(json!(true)))
                .field("sha", Field::text().required())
                .field("run_number", Field::integer())
                .field("conclusion", Field::text().required())
                .field("attempt", Field::integer().required().default(json!(1)))
                .field("created_at", Field::timestamp().required())
                .field("started_at", Field::timestamp())
                .field("finished_at", Field::timestamp().required())
                .field("url", Field::text())
                .index(&["finished_at"])
                .index(&["repository", "finished_at"])
                .export(Export::to(&["cicd"])),
        )
        .collection(
            "outages",
            Collection::new()
                .field("id", Field::text().key())
                .field("service", Field::text().required())
                .field("environment", Field::text())
                .field("cluster", Field::text().required())
                .field("namespace", Field::text().required())
                .field("workload", Field::text().required())
                .field("started_at", Field::timestamp().required())
                .field("ended_at", Field::timestamp())
                .field("detail", Field::text())
                .index(&["started_at"])
                .index(&["cluster", "ended_at"])
                .export(Export::to(&["reliability"])),
        )
        .collection(
            "services",
            Collection::new()
                .field("id", Field::text().key().describe("The service's name in the Catalogue"))
                .field("repositories", Field::json().required().default(json!([]))),
        )
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ClusterRecord {
    pub id: String,
    #[serde(default)]
    pub server: Option<String>,
    #[serde(default)]
    pub environment: Option<String>,
    pub state: String,
    #[serde(default)]
    pub problem: Option<String>,
    #[serde(default)]
    pub insecure: bool,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub stats: Stats,
    pub polled_at: DateTime<Utc>,
}

/// Counts and use, for a cluster, a namespace or a workload; what does not apply stays empty.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Stats {
    pub nodes: Option<i64>,
    pub nodes_ready: Option<i64>,
    pub namespaces: Option<i64>,
    pub workloads: Option<i64>,
    pub desired: Option<i64>,
    pub ready: Option<i64>,
    pub available: Option<i64>,
    pub updated: Option<i64>,
    pub pods: i64,
    pub running: i64,
    pub pending: i64,
    pub failed: i64,
    pub restarts: i64,
    /// Cores, where metrics-server is installed.
    pub cpu: Option<f64>,
    /// Bytes, where metrics-server is installed.
    pub memory: Option<f64>,
    pub cpu_allocatable: Option<f64>,
    pub memory_allocatable: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamespaceRecord {
    pub id: String,
    pub cluster: String,
    pub name: String,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub production: bool,
    #[serde(default)]
    pub phase: Option<String>,
    #[serde(default)]
    pub stats: Stats,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkloadRecord {
    pub id: String,
    pub cluster: String,
    pub namespace: String,
    pub name: String,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub production: bool,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub images: Vec<String>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub revision: Option<i64>,
    pub state: String,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub stats: Stats,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub rolled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Rollout {
    pub id: String,
    #[serde(default = "rollout")]
    pub kind: String,
    pub cluster: String,
    pub namespace: String,
    pub workload: String,
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub production: bool,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub sha: String,
    #[serde(default)]
    pub image: Option<String>,
    pub revision: i64,
    #[serde(default)]
    pub rollback: bool,
    pub state: String,
    #[serde(default)]
    pub reason: Option<String>,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub seconds: Option<f64>,
    #[serde(default)]
    pub backfilled: bool,
    #[serde(default)]
    pub url: Option<String>,
}

fn rollout() -> String {
    "rollout".into()
}

impl Rollout {
    pub fn terminal(&self) -> bool {
        matches!(self.state.as_str(), SUCCESS | FAILURE | SUPERSEDED)
    }

    /// The rollout as CI/CD/CT reads a pipeline run, once it has ended, belongs to a repository and
    /// was seen through, so its duration is known.
    pub fn as_run(&self) -> Option<Value> {
        let repository = self.repository.as_ref()?;
        let finished_at = self.finished_at?;
        if self.backfilled || !self.terminal() {
            return None;
        }
        let conclusion = match self.state.as_str() {
            SUCCESS => "success",
            FAILURE => "failure",
            _ => "cancelled",
        };
        let workflow = format!("rollout {}/{}/{}", self.cluster, self.namespace, self.workload);
        Some(json!({
            "id": format!("kubernetes/{}", self.id),
            "repository": repository,
            "workflow_id": workflow_id(&workflow),
            "workflow": workflow,
            "event": "rollout",
            "branch": self.environment,
            "default_branch": true,
            "sha": self.sha,
            "run_number": self.revision,
            "conclusion": conclusion,
            "attempt": 1,
            "created_at": self.started_at,
            "started_at": self.started_at,
            "finished_at": finished_at,
            "url": self.url,
        }))
    }
}

/// A steady number for a workload's rollouts as one workflow, from its name: FNV-1a, kept positive.
fn workflow_id(name: &str) -> i64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in name.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    (hash >> 1) as i64
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutageRecord {
    pub id: String,
    pub service: String,
    #[serde(default)]
    pub environment: Option<String>,
    pub cluster: String,
    pub namespace: String,
    pub workload: String,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mapped {
    pub id: String,
    #[serde(default)]
    pub repositories: Vec<String>,
}
