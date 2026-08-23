//! What the plugin keeps: each run, which an administrator starts to take documentation and
//! catalogue data from other tools, and every item an LLM staged in one, which nothing outside the
//! plugin sees until the administrator approves it.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Collection, Declaration, Field, ListOf, OnDelete, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;

/// The tools an administrator can take data from, and what each is called on the page.
pub const SOURCES: [(&str, &str); 5] = [
    ("confluence", "Confluence"),
    ("backstage", "Backstage"),
    ("jira", "Jira"),
    ("markdown", "Markdown"),
    ("mkdocs", "MkDocs"),
];

/// Claude, driven by DOC with the API key in the settings.
pub const CLAUDE: &str = "claude";
/// The administrator's own agent, following the instructions and token the plugin gives them.
pub const AGENT: &str = "agent";

/// Still taking items in.
pub const COLLECTING: &str = "collecting";
/// Done taking them in, waiting for the administrator to approve or reject them.
pub const REVIEW: &str = "review";
/// Everything decided and written.
pub const DONE: &str = "done";
pub const CANCELLED: &str = "cancelled";
pub const FAILED: &str = "failed";

pub const STAGED: &str = "staged";
pub const APPROVED: &str = "approved";
pub const REJECTED: &str = "rejected";
pub const APPLIED: &str = "applied";
pub const NOT_APPLIED: &str = "failed";

pub const KB: &str = "kb";
pub const CATALOGUE: &str = "catalogue";
pub const WATER: &str = "water";

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "runs",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("title", Field::text().required())
                .field("mode", Field::text().required().one_of(&[CLAUDE, AGENT]))
                .field("sources", Field::list(ListOf::Text).required().default(json!([])))
                .field(
                    "brief",
                    Field::text()
                        .required()
                        .default(json!(""))
                        .describe("What to take, in the administrator's words"),
                )
                .field(
                    "state",
                    Field::text().required().one_of(&[COLLECTING, REVIEW, DONE, CANCELLED, FAILED]),
                )
                .field("created_by", Field::text().required().describe("The administrator's login"))
                .field("created_by_id", Field::uuid().required())
                .field(
                    "token_id",
                    Field::uuid().describe("The agent's scoped token, while it has one"),
                )
                .field("token_expires_at", Field::timestamp())
                .field("summary", Field::text().required().default(json!("")))
                .field("error", Field::text())
                .field("turns", Field::integer().required().default(json!(0)))
                .field("finished_at", Field::timestamp())
                .index(&["state"]),
        )
        .collection(
            "items",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("run", Field::reference("runs").required().on_delete(OnDelete::Cascade))
                .field("destination", Field::text().required().one_of(&[KB, CATALOGUE, WATER]))
                .field(
                    "key",
                    Field::text().required().describe(
                        "What it is in its destination: `<space>/<path>`, `Kind:name` or a title",
                    ),
                )
                .field("title", Field::text().required())
                .field("space", Field::text().describe("For a page: its space's key"))
                .field("space_title", Field::text())
                .field("path", Field::text().describe("For a page: where it sits in its space"))
                .field(
                    "content",
                    Field::text()
                        .required()
                        .describe("Markdown for a page or a discussion, JSON for a resource"),
                )
                .field("tags", Field::list(ListOf::Text).required().default(json!([])))
                .field("source", Field::text().required())
                .field("source_url", Field::text().required().default(json!("")))
                .field(
                    "state",
                    Field::text().required().one_of(&[
                        STAGED,
                        APPROVED,
                        REJECTED,
                        APPLIED,
                        NOT_APPLIED,
                    ]),
                )
                .field("error", Field::text())
                .field("applied_at", Field::timestamp())
                .unique(&["run", "destination", "key"])
                .index(&["run", "destination"])
                .index(&["destination", "space"]),
        )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    pub id: Uuid,
    pub title: String,
    pub mode: String,
    #[serde(default)]
    pub sources: Vec<String>,
    #[serde(default)]
    pub brief: String,
    pub state: String,
    pub created_by: String,
    pub created_by_id: Uuid,
    #[serde(default)]
    pub token_id: Option<Uuid>,
    #[serde(default)]
    pub token_expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub turns: i64,
    #[serde(default, alias = "_created_at")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: Uuid,
    pub run: Uuid,
    pub destination: String,
    pub key: String,
    pub title: String,
    #[serde(default)]
    pub space: Option<String>,
    #[serde(default)]
    pub space_title: Option<String>,
    #[serde(default)]
    pub path: Option<String>,
    pub content: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source: String,
    #[serde(default)]
    pub source_url: String,
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub applied_at: Option<DateTime<Utc>>,
    #[serde(default, alias = "_updated_at")]
    pub updated_at: Option<DateTime<Utc>>,
}

fn fields_of(value: &impl Serialize, keep_key: bool) -> Value {
    let mut value = serde_json::to_value(value).unwrap_or_default();
    if let Some(fields) = value.as_object_mut() {
        fields.remove("created_at");
        fields.remove("updated_at");
        if !keep_key {
            fields.remove("id");
        }
    }
    value
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    pub async fn runs(&self) -> Result<Vec<Run>, Refusal> {
        let mut runs: Vec<Run> = self.0.query_all(Query::new("runs")).await?;
        runs.sort_by_key(|run| std::cmp::Reverse(run.id));
        Ok(runs)
    }

    pub async fn runs_in(&self, state: &str) -> Result<Vec<Run>, Refusal> {
        Ok(self.0.query_all(Query::new("runs").filter(json!({ "state": state }))).await?)
    }

    pub async fn run(&self, id: Uuid) -> Result<Option<Run>, Refusal> {
        Ok(self.0.get("runs", id.to_string()).await?)
    }

    pub async fn save_run(&self, run: &Run) -> Result<(), Refusal> {
        let updated: Option<Value> =
            self.0.update("runs", run.id.to_string(), fields_of(run, false), None).await?;
        if updated.is_none() {
            let _: Value = self.0.insert("runs", fields_of(run, true)).await?;
        }
        Ok(())
    }

    pub async fn items(&self, run: Uuid) -> Result<Vec<Item>, Refusal> {
        let mut items: Vec<Item> =
            self.0.query_all(Query::new("items").filter(json!({ "run": run }))).await?;
        items.sort_by(|a, b| (&a.destination, &a.key).cmp(&(&b.destination, &b.key)));
        Ok(items)
    }

    pub async fn item(&self, id: Uuid) -> Result<Option<Item>, Refusal> {
        Ok(self.0.get("items", id.to_string()).await?)
    }

    /// Every page ever applied to or approved for a space, from any run.
    pub async fn pages_of(&self, space: &str) -> Result<Vec<Item>, Refusal> {
        let query = Query::new("items").filter(json!({
            "destination": KB, "space": space, "state": { "in": [APPLIED, APPROVED] },
        }));
        Ok(self.0.query_all(query).await?)
    }

    /// Stages an item, or changes the one of the same run, destination and key still staged;
    /// answers whether it was new.
    pub async fn stage(&self, item: &Item) -> Result<bool, Refusal> {
        let query = Query::new("items").filter(json!({
            "run": item.run, "destination": item.destination, "key": item.key,
        }));
        let held: Vec<Item> = self.0.query_all(query).await?;
        match held.first() {
            Some(found) if found.state != STAGED => Err(Refusal::conflict(format!(
                "{} is {} already, so it stays as it is",
                item.key, found.state
            ))),
            Some(found) => {
                let changed = Item { id: found.id, ..item.clone() };
                self.save_item(&changed).await?;
                Ok(false)
            }
            None => {
                let _: Value = self.0.insert("items", fields_of(item, true)).await?;
                Ok(true)
            }
        }
    }

    pub async fn save_item(&self, item: &Item) -> Result<(), Refusal> {
        let _: Option<Value> =
            self.0.update("items", item.id.to_string(), fields_of(item, false), None).await?;
        Ok(())
    }

    pub async fn count(&self, run: Uuid) -> Result<usize, Refusal> {
        let query = Query::new("items").filter(json!({ "run": run })).fields(&["id"]);
        let found: Vec<Value> = self.0.query_all(query).await?;
        Ok(found.len())
    }
}
