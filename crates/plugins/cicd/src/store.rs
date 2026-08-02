//! What CI/CD/CT metrics keep: each workflow's runs summed day by day, on its repository's default
//! branch and off it, and each time a workflow broke on the default branch until it passed again.
//! Records are per repository; services, teams and organisations are the repositories connected to
//! them in the Catalogue, read when somebody looks, as them.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Collection, Declaration, Field};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "days",
            Collection::new()
                .field("id", Field::text().key().describe("source:repository:workflow:branch:day"))
                .field("source", Field::text().required())
                .field("repository", Field::text().required())
                .field("workflow_id", Field::integer().required())
                .field("workflow", Field::text().required())
                .field("path", Field::text())
                .field("stage", Field::text().required().one_of(&["ci", "cd", "ct"]))
                .field("default_branch", Field::boolean().required().default(json!(false)))
                .field("day", Field::timestamp().required().describe("Midnight UTC"))
                .field("runs", Field::integer().required().default(json!(0)))
                .field("succeeded", Field::integer().required().default(json!(0)))
                .field(
                    "failed",
                    Field::integer()
                        .required()
                        .default(json!(0))
                        .describe("Failed, timed out or failed to start"),
                )
                .field("cancelled", Field::integer().required().default(json!(0)))
                .field(
                    "reruns",
                    Field::integer()
                        .required()
                        .default(json!(0))
                        .describe("Succeeded, but only on a later attempt"),
                )
                .field(
                    "durations",
                    Field::json()
                        .required()
                        .default(json!([]))
                        .describe("Seconds each successful run took"),
                )
                .index(&["day"])
                .index(&["repository", "day"]),
        )
        .collection(
            "recoveries",
            Collection::new()
                .field(
                    "id",
                    Field::text().key().describe("source:repository:workflow:run that broke it"),
                )
                .field("source", Field::text().required())
                .field("repository", Field::text().required())
                .field("workflow_id", Field::integer().required())
                .field("workflow", Field::text().required())
                .field("stage", Field::text().required().one_of(&["ci", "cd", "ct"]))
                .field("broke_at", Field::timestamp().required())
                .field("broke_url", Field::text())
                .field("fixed_at", Field::timestamp())
                .field("fixed_url", Field::text())
                .field("seconds", Field::number())
                .field("failed_runs", Field::integer().required().default(json!(1)))
                .index(&["broke_at"])
                .index(&["repository", "broke_at"]),
        )
}

/// One workflow's runs on one day, on the default branch or off it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Day {
    pub id: String,
    pub source: String,
    pub repository: String,
    pub workflow_id: i64,
    pub workflow: String,
    #[serde(default)]
    pub path: Option<String>,
    pub stage: String,
    #[serde(default)]
    pub default_branch: bool,
    pub day: DateTime<Utc>,
    #[serde(default)]
    pub runs: i64,
    #[serde(default)]
    pub succeeded: i64,
    #[serde(default)]
    pub failed: i64,
    #[serde(default)]
    pub cancelled: i64,
    #[serde(default)]
    pub reruns: i64,
    #[serde(default)]
    pub durations: Vec<f64>,
}

/// A workflow broken on the default branch: from the run that failed first to the next that passed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Recovery {
    pub id: String,
    pub source: String,
    pub repository: String,
    pub workflow_id: i64,
    pub workflow: String,
    pub stage: String,
    pub broke_at: DateTime<Utc>,
    #[serde(default)]
    pub broke_url: Option<String>,
    #[serde(default)]
    pub fixed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub fixed_url: Option<String>,
    #[serde(default)]
    pub seconds: Option<f64>,
    #[serde(default)]
    pub failed_runs: i64,
}

/// A workflow run as a source exports it (`github.workflow-runs`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: String,
    pub repository: String,
    pub workflow_id: i64,
    pub workflow: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub event: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub default_branch: bool,
    pub sha: String,
    pub conclusion: String,
    #[serde(default = "first")]
    pub attempt: i64,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: DateTime<Utc>,
    #[serde(default)]
    pub url: Option<String>,
}

fn first() -> i64 {
    1
}

impl Run {
    pub fn succeeded(&self) -> bool {
        self.conclusion == "success"
    }

    pub fn failed(&self) -> bool {
        matches!(self.conclusion.as_str(), "failure" | "timed_out" | "startup_failure")
    }

    /// How long its latest attempt took.
    pub fn seconds(&self) -> f64 {
        let started = self.started_at.unwrap_or(self.created_at);
        (self.finished_at - started).num_seconds().max(0) as f64
    }
}
