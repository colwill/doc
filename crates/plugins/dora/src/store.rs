//! What DORA keeps: each production deployment worked out from a source's delivery data, with
//! its lead times, whether it failed and when it recovered, and the counters automations add to.
//! Records are per repository; services, teams and organisations are the repositories connected to
//! them in the Catalogue, read when somebody looks, as them.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Collection, Declaration, Field};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "deployments",
            Collection::new()
                .field("id", Field::text().key().describe("The source, then its own ID for it"))
                .field("source", Field::text().required())
                .field("repository", Field::text().required())
                .field("environment", Field::text().required())
                .field("kind", Field::text().required())
                .field("sha", Field::text().required())
                .field("url", Field::text())
                .field("deployed_at", Field::timestamp().required())
                .field("commits", Field::integer().required().default(json!(0)))
                .field(
                    "lead_times",
                    Field::json()
                        .required()
                        .default(json!([]))
                        .describe("Seconds from each change it shipped to its deployment"),
                )
                .field("lead_median", Field::number())
                .field("failed", Field::boolean().required().default(json!(false)))
                .field(
                    "failure",
                    Field::text().describe("What said so: revert, rollback, hotfix or a counter"),
                )
                .field("failure_at", Field::timestamp())
                .field("failure_url", Field::text())
                .field("recovered_at", Field::timestamp())
                .field("recovery_seconds", Field::number())
                .index(&["deployed_at"])
                .index(&["repository", "deployed_at"]),
        )
        .collection(
            "counters",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("counter", Field::text().required().max(64.0))
                .field("amount", Field::integer().required().default(json!(1)).min(1.0))
                .field("service", Field::text())
                .field("repository", Field::text())
                .field("at", Field::timestamp().required().default(json!("now")))
                .field("by", Field::text())
                .field("note", Field::text().max(500.0))
                .index(&["at"])
                .index(&["repository", "at"]),
        )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Deployment {
    pub id: String,
    pub source: String,
    pub repository: String,
    pub environment: String,
    pub kind: String,
    pub sha: String,
    #[serde(default)]
    pub url: Option<String>,
    pub deployed_at: DateTime<Utc>,
    #[serde(default)]
    pub commits: i64,
    #[serde(default)]
    pub lead_times: Vec<f64>,
    #[serde(default)]
    pub lead_median: Option<f64>,
    #[serde(default)]
    pub failed: bool,
    #[serde(default)]
    pub failure: Option<String>,
    #[serde(default)]
    pub failure_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub failure_url: Option<String>,
    #[serde(default)]
    pub recovered_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub recovery_seconds: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Counted {
    pub id: Uuid,
    pub counter: String,
    pub amount: i64,
    #[serde(default)]
    pub service: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

/// A deployment as a source exports it (`github.deployments`).
#[derive(Debug, Clone, Deserialize)]
pub struct Shipped {
    pub id: String,
    pub environment: String,
    pub kind: String,
    pub sha: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub rollback: bool,
    #[serde(default)]
    pub commits: Vec<Commit>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Commit {
    pub sha: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub message: String,
}

/// A merged pull request as a source exports it (`github.pull-requests`).
#[derive(Debug, Clone, Deserialize)]
pub struct Merged {
    pub title: String,
    #[serde(default)]
    pub labels: Vec<String>,
    pub merged_at: DateTime<Utc>,
    #[serde(default)]
    pub merge_sha: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
}
