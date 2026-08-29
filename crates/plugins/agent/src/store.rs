//! What Agent Smith keeps: jobs and the leave each holds to act as its owner, every run with each
//! tool call it made, reminders waiting to be delivered, and the runbooks approved for it to run
//! by itself.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Collection, Declaration, Field, ListOf, OnDelete, Order, Query};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;

pub const JOBS: &str = "jobs";
pub const RUNS: &str = "runs";
pub const CALLS: &str = "calls";
pub const REMINDERS: &str = "reminders";
pub const APPROVALS: &str = "approvals";

pub const QUEUED: &str = "queued";
pub const RUNNING: &str = "running";
pub const DONE: &str = "done";
pub const FAILED: &str = "failed";
pub const STOPPED: &str = "stopped";

/// Whose access a runbook's run has: whoever asked for it, or Agent Smith's own.
pub const REQUESTER: &str = "requester";
pub const AGENT: &str = "agent";

/// The environments a runbook is run against.
pub const ENVIRONMENTS: [&str; 3] = ["development", "test", "production"];

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            JOBS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("owner", Field::uuid().required().describe("The person it runs as"))
                .field("owner_label", Field::text())
                .field("title", Field::text().required())
                .field("playbook", Field::text().required())
                .field("brief", Field::text().max(4_000.0))
                .field(
                    "scope",
                    Field::text().describe("The service, team or organisation it is about"),
                )
                .field("trigger", Field::text().required().one_of(&["manual", "schedule", "event"]))
                .field("cron", Field::text())
                .field("events", Field::list(ListOf::Text).required().default(json!([])))
                .field("deliver", Field::list(ListOf::Text).required().default(json!([])))
                .field(
                    "delegation",
                    Field::uuid().describe("Leave to act as its owner; a runbook's job has none"),
                )
                .field("enabled", Field::boolean().required().default(json!(true)))
                .field("next_at", Field::timestamp())
                .field(
                    "runbook",
                    Field::text().describe("The runbook a runbook's job runs, as space/path"),
                )
                .index(&["owner"]),
        )
        .collection(
            RUNS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "job",
                    Field::reference(JOBS)
                        .on_delete(OnDelete::Cascade)
                        .describe("Its job; a runbook somebody asked to run has none"),
                )
                .field(
                    "owner",
                    Field::uuid().required().describe("Who asked for it, and is told how it went"),
                )
                .field("owner_label", Field::text())
                .field("why", Field::text().describe("What started it"))
                .field("runbook", Field::text().describe("The runbook it runs, as space/path"))
                .field("runbook_title", Field::text())
                .field("environment", Field::text().one_of(&ENVIRONMENTS))
                .field(
                    "access",
                    Field::text()
                        .one_of(&[REQUESTER, AGENT])
                        .describe("Whose access a runbook's run has"),
                )
                .field(
                    "delegation",
                    Field::uuid().describe("Leave from whoever asked, given back when it ends"),
                )
                .field("hash", Field::text().describe("The version of the runbook it ran"))
                .field(
                    "events",
                    Field::json().describe("What happened, for a run an event started"),
                )
                .field(
                    "state",
                    Field::text().required().one_of(&[QUEUED, RUNNING, DONE, FAILED, STOPPED]),
                )
                .field("turns", Field::integer().required().default(json!(0)))
                .field("input_tokens", Field::integer().required().default(json!(0)))
                .field("output_tokens", Field::integer().required().default(json!(0)))
                .field("report", Field::text())
                .field("error", Field::text())
                .field("started_at", Field::timestamp())
                .field("finished_at", Field::timestamp())
                .index(&["job"])
                .index(&["state"])
                .index(&["owner"]),
        )
        .collection(
            CALLS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("run", Field::reference(RUNS).required().on_delete(OnDelete::Cascade))
                .field("turn", Field::integer().required())
                .field("tool", Field::text().required())
                .field("input", Field::json())
                .field("result", Field::text())
                .field("failed", Field::boolean())
                .field("done", Field::boolean().required().default(json!(false)))
                .index(&["run"]),
        )
        .collection(
            REMINDERS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("at", Field::timestamp().required())
                .field("message", Field::text().required().max(1_000.0))
                .field("url", Field::text())
                .field("to", Field::list(ListOf::Text).required().describe("User IDs"))
                .field("to_label", Field::text())
                .field("by", Field::uuid().required())
                .field("by_label", Field::text())
                .field("delivered_at", Field::timestamp())
                .index(&["by"])
                .index(&["at"]),
        )
        .collection(
            APPROVALS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("runbook", Field::text().required().describe("space/path"))
                .field("title", Field::text())
                .field(
                    "hash",
                    Field::text()
                        .required()
                        .describe("The version approved, which runs must match"),
                )
                .field("environment", Field::text().required().one_of(&ENVIRONMENTS))
                .field("approved_by", Field::uuid().required())
                .field("approved_by_label", Field::text())
                .field("approved_at", Field::timestamp().required())
                .unique(&["runbook"]),
        )
}

pub fn or_default<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub id: Uuid,
    pub owner: Uuid,
    #[serde(default, deserialize_with = "or_default")]
    pub owner_label: String,
    pub title: String,
    pub playbook: String,
    #[serde(default, deserialize_with = "or_default")]
    pub brief: String,
    #[serde(default, deserialize_with = "or_default")]
    pub scope: String,
    pub trigger: String,
    #[serde(default, deserialize_with = "or_default")]
    pub cron: String,
    #[serde(default, deserialize_with = "or_default")]
    pub events: Vec<String>,
    #[serde(default, deserialize_with = "or_default")]
    pub deliver: Vec<String>,
    #[serde(default)]
    pub delegation: Option<Uuid>,
    #[serde(default, deserialize_with = "or_default")]
    pub enabled: bool,
    #[serde(default)]
    pub next_at: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "or_default")]
    pub runbook: String,
    #[serde(rename = "_created_at", default)]
    pub created_at: Option<DateTime<Utc>>,
}

impl Job {
    pub fn href(&self) -> String {
        format!("/p/agent/jobs/{}", self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: Uuid,
    #[serde(default)]
    pub job: Option<Uuid>,
    pub owner: Uuid,
    #[serde(default, deserialize_with = "or_default")]
    pub owner_label: String,
    #[serde(default, deserialize_with = "or_default")]
    pub why: String,
    #[serde(default, deserialize_with = "or_default")]
    pub runbook: String,
    #[serde(default, deserialize_with = "or_default")]
    pub runbook_title: String,
    #[serde(default, deserialize_with = "or_default")]
    pub environment: String,
    #[serde(default, deserialize_with = "or_default")]
    pub access: String,
    #[serde(default)]
    pub delegation: Option<Uuid>,
    #[serde(default, deserialize_with = "or_default")]
    pub hash: String,
    #[serde(default, deserialize_with = "or_default")]
    pub events: Value,
    pub state: String,
    #[serde(default, deserialize_with = "or_default")]
    pub turns: i64,
    #[serde(default, deserialize_with = "or_default")]
    pub input_tokens: i64,
    #[serde(default, deserialize_with = "or_default")]
    pub output_tokens: i64,
    #[serde(default, deserialize_with = "or_default")]
    pub report: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(rename = "_created_at", default)]
    pub created_at: Option<DateTime<Utc>>,
}

impl Run {
    pub fn href(&self) -> String {
        format!("/p/agent/runs/{}", self.id)
    }

    /// What it is called on its page and in what it tells people.
    pub fn title(&self, job: Option<&Job>) -> String {
        match (job, self.runbook_title.as_str()) {
            (Some(job), _) => job.title.clone(),
            (None, "") => "A runbook".to_string(),
            (None, title) => title.to_string(),
        }
    }
}

/// A runbook approved for Agent Smith to run by itself, at one version, against one environment.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Approval {
    pub id: Uuid,
    pub runbook: String,
    #[serde(default, deserialize_with = "or_default")]
    pub title: String,
    pub hash: String,
    pub environment: String,
    pub approved_by: Uuid,
    #[serde(default, deserialize_with = "or_default")]
    pub approved_by_label: String,
    pub approved_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub id: Uuid,
    pub run: Uuid,
    pub turn: i64,
    pub tool: String,
    #[serde(default, deserialize_with = "or_default")]
    pub input: Value,
    #[serde(default, deserialize_with = "or_default")]
    pub result: String,
    #[serde(default, deserialize_with = "or_default")]
    pub failed: bool,
    #[serde(default, deserialize_with = "or_default")]
    pub done: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Reminder {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    pub message: String,
    #[serde(default, deserialize_with = "or_default")]
    pub url: String,
    #[serde(default, deserialize_with = "or_default")]
    pub to: Vec<String>,
    #[serde(default, deserialize_with = "or_default")]
    pub to_label: String,
    pub by: Uuid,
    #[serde(default, deserialize_with = "or_default")]
    pub by_label: String,
    #[serde(default)]
    pub delivered_at: Option<DateTime<Utc>>,
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    async fn all<T: DeserializeOwned>(&self, query: Query) -> Result<Vec<T>, Refusal> {
        Ok(self.0.query_all(query).await?)
    }

    async fn one<T: DeserializeOwned>(&self, collection: &str, id: Uuid) -> Result<T, Refusal> {
        let found: Option<T> = self.0.get(collection, json!(id)).await?;
        found.ok_or_else(|| Refusal::missing("there is nothing here by that ID"))
    }

    pub async fn jobs(&self, owner: Option<Uuid>) -> Result<Vec<Job>, Refusal> {
        let query = match owner {
            Some(owner) => Query::new(JOBS).filter(json!({ "owner": owner })),
            None => Query::new(JOBS),
        };
        self.all(query).await
    }

    pub async fn job(&self, id: Uuid) -> Result<Job, Refusal> {
        self.one(JOBS, id).await
    }

    pub async fn runs(&self, filter: Value, limit: u32) -> Result<Vec<Run>, Refusal> {
        let query = Query::new(RUNS).filter(filter).order(Order::desc("_created_at")).limit(limit);
        Ok(self.0.query(query).await?.records)
    }

    pub async fn run(&self, id: Uuid) -> Result<Run, Refusal> {
        self.one(RUNS, id).await
    }

    pub async fn calls(&self, run: Uuid) -> Result<Vec<Call>, Refusal> {
        let query =
            Query::new(CALLS).filter(json!({ "run": run })).order(Order::asc("_created_at"));
        self.all(query).await
    }

    pub async fn call(&self, id: Uuid) -> Result<Call, Refusal> {
        self.one(CALLS, id).await
    }

    pub async fn approvals(&self) -> Result<Vec<Approval>, Refusal> {
        self.all(Query::new(APPROVALS).order(Order::asc("runbook"))).await
    }

    pub async fn approval(&self, runbook: &str) -> Result<Option<Approval>, Refusal> {
        let query = Query::new(APPROVALS).filter(json!({ "runbook": runbook })).limit(1);
        Ok(self.0.query(query).await?.records.into_iter().next())
    }

    pub async fn reminders(&self, filter: Value) -> Result<Vec<Reminder>, Refusal> {
        let query = Query::new(REMINDERS).filter(filter).order(Order::asc("at"));
        self.all(query).await
    }

    pub async fn reminder(&self, id: Uuid) -> Result<Reminder, Refusal> {
        self.one(REMINDERS, id).await
    }

    pub async fn insert<T: DeserializeOwned>(
        &self,
        collection: &str,
        values: Value,
    ) -> Result<T, Refusal> {
        Ok(self.0.insert(collection, values).await?)
    }

    pub async fn update<T: DeserializeOwned>(
        &self,
        collection: &str,
        id: Uuid,
        set: Value,
    ) -> Result<T, Refusal> {
        let updated: Option<T> = self.0.update(collection, json!(id), set, None).await?;
        updated.ok_or_else(|| Refusal::missing("it is gone"))
    }

    pub async fn delete(&self, collection: &str, id: Uuid) -> Result<(), Refusal> {
        self.0.delete(collection, json!(id), None).await?;
        Ok(())
    }
}
