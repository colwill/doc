//! What the plugin keeps: the templates, every run of one, and the lines each run wrote.

use chrono::Utc;
use doc_plugin_sdk::{Backend, Collection, DataRequest, Declaration, Field, ListOf, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::model::{Definition, Lifecycle};

/// The states a run passes through. `queued` and `running` are the live ones.
pub const STATES: [&str; 5] = ["queued", "running", "succeeded", "failed", "cancelled"];

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "templates",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("name", Field::text().required())
                .field("title", Field::text().required())
                .field("description", Field::text().required().default(json!("")))
                .field("kind", Field::text())
                .field("tags", Field::list(ListOf::Text).required().default(json!([])))
                .field("owner", Field::text())
                .field(
                    "definition",
                    Field::json().required().describe("The whole template, as it was written"),
                )
                .field(
                    "builtin",
                    Field::boolean()
                        .required()
                        .default(json!(false))
                        .describe("One the plugin seeded, which it puts back if it is removed"),
                )
                .field("author", Field::text().required().default(json!("")))
                .unique(&["name"])
                // The cards are listed by title, which the data API only sorts by with an index.
                .index(&["title"])
                .search(&["name", "title", "description"]),
        )
        .collection(
            "runs",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("template", Field::text().required())
                .field("title", Field::text().required())
                .field("answers", Field::json().required().default(json!({})))
                .field("state", Field::text().required().default(json!("queued")).one_of(&STATES))
                .field(
                    "lifecycle",
                    Field::text()
                        .required()
                        .default(json!(Lifecycle::Experiment.name()))
                        .one_of(&Lifecycle::ALL.map(Lifecycle::name))
                        .describe("How far along what the run creates is"),
                )
                .field(
                    "steps",
                    Field::json().required().default(json!([])).describe("How each step went"),
                )
                .field(
                    "cursor",
                    Field::integer().required().default(json!(0)).describe("The next step to take"),
                )
                .field("outputs", Field::json().required().default(json!({})))
                .field("links", Field::json().required().default(json!([])))
                .field("error", Field::text())
                .field("requester", Field::text().required())
                .field("requester_label", Field::text().required().default(json!("")))
                .field("delegation", Field::uuid())
                .field("task", Field::uuid())
                .field("attempts", Field::integer().required().default(json!(0)))
                .field("cancelled", Field::boolean().required().default(json!(false)))
                .field(
                    "template_version",
                    Field::integer()
                        .required()
                        .default(json!(0))
                        .describe("The version of the template it was launched from"),
                )
                .field("started_at", Field::timestamp())
                .field("finished_at", Field::timestamp())
                .index(&["state"])
                .index(&["template"])
                .index(&["requester"])
                .index(&["lifecycle"]),
        )
        .collection(
            "lines",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("run", Field::uuid().required())
                .field("seq", Field::integer().required())
                .field("at", Field::timestamp().required())
                .field("step", Field::text())
                .field(
                    "level",
                    Field::text()
                        .required()
                        .default(json!("info"))
                        .one_of(&["info", "success", "warning", "error", "muted", "command"]),
                )
                .field("text", Field::text().required())
                .index(&["run", "seq"])
                // A log is read in order, and the data API sorts only by a leading indexed field.
                .index(&["seq"]),
        )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub id: Uuid,
    pub name: String,
    pub title: String,
    pub description: String,
    pub kind: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub owner: Option<String>,
    pub definition: Value,
    #[serde(default)]
    pub builtin: bool,
    #[serde(default)]
    pub author: String,
    /// Moves on with every edit, so a run can tell whether the template changed after it.
    #[serde(rename = "_version", default)]
    pub version: i64,
}

impl Template {
    /// The definition as the model reads it. A stored template was checked before it was stored,
    /// so this only fails if the collection was written around the plugin.
    pub fn definition(&self) -> Result<Definition, Refusal> {
        serde_json::from_value(self.definition.clone()).map_err(|err| {
            Refusal::unavailable(format!("the template `{}` cannot be read: {err}", self.name))
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Run {
    pub id: Uuid,
    pub template: String,
    pub title: String,
    #[serde(default)]
    pub answers: Value,
    pub state: String,
    /// What the run creates, as it was when it was created; the thing itself moves on from here.
    #[serde(default)]
    pub lifecycle: Lifecycle,
    #[serde(default)]
    pub steps: Value,
    #[serde(default)]
    pub cursor: i64,
    #[serde(default)]
    pub outputs: Value,
    #[serde(default)]
    pub links: Value,
    pub error: Option<String>,
    pub requester: String,
    #[serde(default)]
    pub requester_label: String,
    pub delegation: Option<Uuid>,
    pub task: Option<Uuid>,
    #[serde(default)]
    pub attempts: i64,
    #[serde(default)]
    pub cancelled: bool,
    /// The version of the template it was launched from.
    #[serde(default)]
    pub template_version: i64,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
}

impl Run {
    pub fn live(&self) -> bool {
        matches!(self.state.as_str(), "queued" | "running")
    }

    /// How each step went, in the order they run.
    pub fn outcomes(&self) -> Vec<Outcome> {
        serde_json::from_value(self.steps.clone()).unwrap_or_default()
    }
}

/// One step's outcome, kept on the run so a retry skips what already worked.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Outcome {
    pub id: String,
    pub title: String,
    pub action: String,
    /// `done`, `failed`, `skipped` or `running`.
    pub state: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub output: Value,
    #[serde(default)]
    pub started_at: Option<String>,
    #[serde(default)]
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Line {
    pub id: Uuid,
    pub run: Uuid,
    pub seq: i64,
    pub at: String,
    pub step: Option<String>,
    pub level: String,
    pub text: String,
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    pub async fn templates(&self, search: Option<&str>) -> Result<Vec<Template>, Refusal> {
        let query = match search.map(str::trim).filter(|text| !text.is_empty()) {
            Some(text) => Query::new("templates").search(text),
            None => Query::new("templates").order(Order::asc("title")),
        };
        Ok(self.0.query(query.limit(200)).await?.records)
    }

    pub async fn template(&self, name: &str) -> Result<Option<Template>, Refusal> {
        let found: Vec<Template> = self
            .0
            .query(Query::new("templates").filter(json!({ "name": name })).limit(1))
            .await?
            .records;
        Ok(found.into_iter().next())
    }

    /// Stores a template, replacing one of the same name. The definition is checked by the caller.
    pub async fn save(
        &self,
        definition: &Definition,
        author: &str,
        builtin: bool,
    ) -> Result<Template, Refusal> {
        let values = json!({
            "name": definition.name,
            "title": definition.label(),
            "description": definition.description,
            "kind": definition.kind,
            "tags": definition.tags,
            "owner": definition.owner,
            "definition": serde_json::to_value(definition).unwrap_or_default(),
            "builtin": builtin,
            "author": author,
        });
        let (template, _) = self.0.upsert("templates", &["name"], values).await?;
        Ok(template)
    }

    pub async fn remove(&self, name: &str) -> Result<bool, Refusal> {
        let Some(template) = self.template(name).await? else { return Ok(false) };
        Ok(self.0.delete("templates", template.id.to_string(), None).await?)
    }

    pub async fn new_run(&self, run: &Run) -> Result<Run, Refusal> {
        let values = json!({
            "id": run.id,
            "template": run.template,
            "title": run.title,
            "answers": run.answers,
            "state": run.state,
            "lifecycle": run.lifecycle,
            "steps": run.steps,
            "outputs": run.outputs,
            "requester": run.requester,
            "requester_label": run.requester_label,
            "delegation": run.delegation,
            "template_version": run.template_version,
        });
        Ok(self.0.insert("runs", values).await?)
    }

    pub async fn run(&self, id: Uuid) -> Result<Option<Run>, Refusal> {
        Ok(self.0.get("runs", id.to_string()).await?)
    }

    /// The newest runs, of one template or of all of them, of one requester or of everyone, and
    /// of one lifecycle or of all of them.
    pub async fn runs(
        &self,
        template: Option<&str>,
        requester: Option<&str>,
        lifecycle: Option<Lifecycle>,
        limit: u32,
    ) -> Result<Vec<Run>, Refusal> {
        let mut filter = serde_json::Map::new();
        if let Some(template) = template {
            filter.insert("template".into(), json!(template));
        }
        if let Some(requester) = requester {
            filter.insert("requester".into(), json!(requester));
        }
        if let Some(lifecycle) = lifecycle {
            filter.insert("lifecycle".into(), json!(lifecycle.name()));
        }
        let mut query = Query::new("runs").order(Order::desc("_created_at")).limit(limit);
        if !filter.is_empty() {
            query = query.filter(Value::Object(filter));
        }
        Ok(self.0.query(query).await?.records)
    }

    pub async fn set(&self, id: Uuid, set: Value) -> Result<Option<Run>, Refusal> {
        Ok(self.0.update("runs", id.to_string(), set, None).await?)
    }

    /// Stops a run: one that has not started yet ends here, and one that is going is asked to stop
    /// before its next step. A run that has already finished is left as it is.
    pub async fn cancel(&self, id: Uuid) -> Result<Option<Run>, Refusal> {
        let Some(run) = self.run(id).await? else { return Ok(None) };
        if !run.live() {
            return Ok(Some(run));
        }
        let set = match run.started_at.is_none() {
            true => json!({
                "cancelled": true,
                "state": "cancelled",
                "finished_at": Utc::now().to_rfc3339(),
            }),
            false => json!({ "cancelled": true }),
        };
        self.set(id, set).await
    }

    /// Appends lines to a run's log in one write, answering the sequence number to carry on from.
    pub async fn log(
        &self,
        run: Uuid,
        from: i64,
        lines: &[(String, String, Option<String>)],
    ) -> Result<i64, Refusal> {
        if lines.is_empty() {
            return Ok(from);
        }
        let at = Utc::now().to_rfc3339();
        let writes: Vec<DataRequest> = lines
            .iter()
            .enumerate()
            .map(|(offset, (level, text, step))| {
                DataRequest::insert(
                    "lines",
                    json!({
                        "run": run,
                        "seq": from + offset as i64,
                        "at": at,
                        "step": step,
                        "level": level,
                        "text": text.chars().take(2_000).collect::<String>(),
                    }),
                )
            })
            .collect();
        for batch in writes.chunks(100) {
            self.0.batch(batch.to_vec()).await?;
        }
        Ok(from + lines.len() as i64)
    }

    pub async fn lines(&self, run: Uuid, from: i64, limit: u32) -> Result<Vec<Line>, Refusal> {
        Ok(self
            .0
            .query(
                Query::new("lines")
                    .filter(json!({ "run": run, "seq": { "gte": from } }))
                    .order(Order::asc("seq"))
                    .limit(limit),
            )
            .await?
            .records)
    }

    /// How many lines a run has written, which is where the next one goes.
    pub async fn next_line(&self, run: Uuid) -> Result<i64, Refusal> {
        let last: Vec<Line> = self
            .0
            .query(
                Query::new("lines")
                    .filter(json!({ "run": run }))
                    .order(Order::desc("seq"))
                    .limit(1),
            )
            .await?
            .records;
        Ok(last.first().map_or(0, |line| line.seq + 1))
    }
}
