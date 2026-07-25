//! What the plugin keeps: automations, the triggers waiting for the engine, and each run with its
//! input, every action's outcome and its error.

use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::{
    Aggregate, Backend, Collection, DataRequest, Declaration, Field, Measure, OnDelete, Order,
    PluginError, Query,
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::model::{Action, Condition, Trigger};

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "automations",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("name", Field::text().required())
                .field("resource", Field::text().required())
                .field("owner", Field::text().required())
                .field("owner_label", Field::text().required())
                .field("delegation", Field::uuid().required())
                .field("trigger", Field::json().required())
                .field("trigger_kind", Field::text().required())
                .field("trigger_key", Field::text())
                .field("conditions", Field::json().required().default(json!([])))
                .field("actions", Field::json().required())
                .field("enabled", Field::boolean().required().default(json!(true)))
                .field("secret", Field::text())
                .field("template", Field::text())
                .field("last_fired_at", Field::timestamp())
                .index(&["resource", "name"])
                .index(&["trigger_kind", "trigger_key"]),
        )
        .collection(
            // Every queue a plugin has sent to over the Service Bus, whether or not an automation
            // listened: the Service Bus keeps no list of them, and this is what there is to choose.
            "queues",
            Collection::new()
                .field("name", Field::text().key())
                .field("sender", Field::text().required().describe("The plugin that sent last"))
                .field("received", Field::integer().required().default(json!(0)))
                .field("last_at", Field::timestamp().required().default(json!("now"))),
        )
        .collection(
            // The latest input each Event Bus topic and Service Bus queue brought, so a condition
            // can be offered the fields a trigger really carries, with values it really had.
            "samples",
            Collection::new()
                .field("key", Field::text().key().describe("`event:<topic>` or `queue:<name>`"))
                .field("kind", Field::text().required())
                .field("name", Field::text().required())
                .field("input", Field::json().required())
                .field("seen_at", Field::timestamp().required().default(json!("now")))
                .index(&["kind"]),
        )
        .collection(
            "triggers",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "automation",
                    Field::reference("automations").required().on_delete(OnDelete::Cascade),
                )
                .field("kind", Field::text().required())
                .field("input", Field::json().required())
                .field("state", Field::text().required().default(json!("pending")))
                .field("run", Field::uuid())
                .field("detail", Field::text())
                .field("received_at", Field::timestamp().required().default(json!("now")))
                .field("claimed_at", Field::timestamp())
                .field("done_at", Field::timestamp())
                .index(&["received_at"])
                .index(&["automation", "received_at"]),
        )
        .collection(
            "runs",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "automation",
                    Field::reference("automations").required().on_delete(OnDelete::Cascade),
                )
                .field("trigger", Field::uuid())
                .field("task", Field::uuid())
                .field("input", Field::json().required())
                .field("state", Field::text().required().default(json!("queued")))
                .field("steps", Field::json().required().default(json!([])))
                .field("error", Field::text())
                .field("attempts", Field::integer().required().default(json!(0)))
                .field("max_attempts", Field::integer().required().default(json!(3)))
                .field("test", Field::boolean().required().default(json!(false)))
                .field("started_at", Field::timestamp())
                .field("finished_at", Field::timestamp())
                .index(&["automation"]),
        )
}

#[derive(Debug, Clone, Deserialize)]
pub struct Automation {
    pub id: Uuid,
    pub name: String,
    /// `kind:name`, with the kind in one spelling, as the Knowledge Base writes it.
    pub resource: String,
    /// `user:<id>` or `service:<id>`.
    pub owner: String,
    pub owner_label: String,
    pub delegation: Uuid,
    pub trigger: Trigger,
    pub conditions: Vec<Condition>,
    pub actions: Vec<Action>,
    pub enabled: bool,
    pub secret: Option<Secret<String>>,
    pub template: Option<String>,
    pub last_fired_at: Option<String>,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    #[serde(alias = "_updated_at", default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Run {
    pub id: Uuid,
    pub automation: Uuid,
    pub trigger: Option<Uuid>,
    pub task: Option<Uuid>,
    pub input: Value,
    pub state: String,
    pub steps: Vec<Value>,
    pub error: Option<String>,
    pub attempts: i64,
    pub max_attempts: i64,
    pub test: bool,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Claimed {
    pub id: Uuid,
    pub automation: Uuid,
    pub input: Value,
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

fn conflict(err: &PluginError) -> bool {
    err.is_version_conflict()
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    /// Changes a record as it is now, again if another writer got there first; false if declined.
    async fn change(
        &self,
        collection: &str,
        id: Uuid,
        change: impl Fn(&Value) -> Option<Value>,
    ) -> Result<bool, Refusal> {
        let changed = self
            .0
            .change(collection, id.to_string(), |current| change(&Value::Object(current.clone())))
            .await?;
        Ok(changed.is_some())
    }

    pub async fn insert(&self, automation: &Automation) -> Result<(), Refusal> {
        let values = json!({
            "id": automation.id, "name": automation.name, "resource": automation.resource,
            "owner": automation.owner, "owner_label": automation.owner_label,
            "delegation": automation.delegation, "trigger": automation.trigger,
            "trigger_kind": automation.trigger.kind(), "trigger_key": automation.trigger.key(),
            "conditions": automation.conditions, "actions": automation.actions,
            "enabled": automation.enabled,
            "secret": automation.secret.as_ref().map(Secret::expose), "template": automation.template,
        });
        let _: Value = self.0.insert("automations", values).await?;
        Ok(())
    }

    /// Saves what can change; a new trigger starts its schedule afresh.
    pub async fn update(&self, automation: &Automation) -> Result<(), Refusal> {
        let trigger = json!(automation.trigger);
        self.change("automations", automation.id, |current| {
            let fired = match current.get("trigger") == Some(&trigger) {
                true => current.get("last_fired_at").cloned().unwrap_or_default(),
                false => Value::Null,
            };
            Some(json!({
                "name": automation.name, "trigger": trigger,
                "trigger_kind": automation.trigger.kind(), "trigger_key": automation.trigger.key(),
                "conditions": automation.conditions, "actions": automation.actions,
                "enabled": automation.enabled,
                "secret": automation.secret.as_ref().map(Secret::expose), "last_fired_at": fired,
            }))
        })
        .await?;
        Ok(())
    }

    pub async fn delete(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("automations", id.to_string(), None).await?;
        Ok(())
    }

    pub async fn automation(&self, id: Uuid) -> Result<Option<Automation>, Refusal> {
        Ok(self.0.get("automations", id.to_string()).await?)
    }

    /// Notes a message arriving on a queue, so the queue can be offered when a trigger is chosen.
    pub async fn heard(&self, queue: &str, sender: &str) -> Result<(), Refusal> {
        let held: Option<Value> = self.0.get("queues", queue.to_string()).await?;
        let received = held.as_ref().and_then(|held| held["received"].as_i64()).unwrap_or_default();
        let values = json!({
            "name": queue, "sender": sender, "received": received + 1,
            "last_at": Utc::now().to_rfc3339(),
        });
        let _: (Value, bool) = self.0.upsert("queues", &["name"], values).await?;
        Ok(())
    }

    /// A `core.*` collection, such as `core.users`, which every plugin may read.
    pub async fn core(&self, query: Query) -> Result<Vec<Value>, Refusal> {
        Ok(self.0.query_all(query).await?)
    }

    /// Keeps what a topic or a queue last brought, as a sample of its fields.
    pub async fn sampled(&self, kind: &str, name: &str, input: &Value) -> Result<(), Refusal> {
        let values = json!({
            "key": format!("{kind}:{name}"), "kind": kind, "name": name, "input": input,
            "seen_at": Utc::now().to_rfc3339(),
        });
        let _: (Value, bool) = self.0.upsert("samples", &["key"], values).await?;
        Ok(())
    }

    /// Every sample of one kind, as `(name, input)`.
    pub async fn samples(&self, kind: &str) -> Result<Vec<(String, Value)>, Refusal> {
        let query = Query::new("samples").filter(json!({ "kind": kind }));
        let rows: Vec<Value> = self.0.query_all(query).await?;
        Ok(rows
            .into_iter()
            .filter_map(|mut row| Some((row["name"].as_str()?.to_string(), row["input"].take())))
            .collect())
    }

    /// Every queue a plugin has sent to, with who sent last and how many messages it has carried.
    pub async fn queues(&self) -> Result<Vec<(String, String, i64)>, Refusal> {
        let rows: Vec<Value> = self.0.query_all(Query::new("queues")).await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let name = row["name"].as_str()?.to_string();
                let sender = row["sender"].as_str().unwrap_or_default().to_string();
                Some((name, sender, row["received"].as_i64().unwrap_or_default()))
            })
            .collect())
    }

    /// Every topic the platform has published, as core reads them off the Event Bus, with how
    /// many events each has carried. It is what there is to subscribe to (T71).
    pub async fn topics(&self) -> Result<Vec<(String, i64)>, Refusal> {
        let rows: Vec<serde_json::Map<String, Value>> =
            self.0.query_all(Query::new("core.topics")).await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let topic = row.get("topic")?.as_str()?.to_string();
                Some((topic, row.get("published").and_then(Value::as_i64).unwrap_or_default()))
            })
            .collect())
    }

    pub async fn automations(&self, resource: Option<&str>) -> Result<Vec<Automation>, Refusal> {
        let mut query =
            Query::new("automations").order(Order::asc("resource")).order(Order::asc("name"));
        if let Some(resource) = resource {
            query = query.filter(json!({ "resource": resource }));
        }
        Ok(self.0.query_all(query).await?)
    }

    /// Enabled automations with this kind of trigger, as `(id, key)`.
    pub async fn triggered_by(&self, kind: &str) -> Result<Vec<(Uuid, String)>, Refusal> {
        let query = Query::new("automations")
            .filter(json!({ "enabled": true, "trigger_kind": kind }))
            .fields(&["id", "trigger_key"]);
        let found: Vec<Value> = self.0.query_all(query).await?;
        Ok(found
            .iter()
            .filter_map(|row| {
                let id = row.get("id")?.as_str()?.parse().ok()?;
                Some((id, row.get("trigger_key")?.as_str().unwrap_or_default().to_string()))
            })
            .collect())
    }

    /// Takes the cron fire for `id` at `at`, unless a tick already took it or a later one.
    pub async fn fire(&self, id: Uuid, at: &str) -> Result<bool, Refusal> {
        let wanted = chrono::DateTime::parse_from_rfc3339(at).ok();
        self.change("automations", id, |current| {
            let enabled = current.get("enabled").and_then(Value::as_bool).unwrap_or(false);
            let fired = current
                .get("last_fired_at")
                .and_then(Value::as_str)
                .and_then(|fired| chrono::DateTime::parse_from_rfc3339(fired).ok());
            let due = match (fired, wanted) {
                (Some(fired), Some(wanted)) => fired < wanted,
                _ => true,
            };
            (enabled && due).then(|| json!({ "last_fired_at": at }))
        })
        .await
    }

    pub async fn enqueue(
        &self,
        automations: &[Uuid],
        kind: &str,
        input: &Value,
    ) -> Result<(), Refusal> {
        let writes: Vec<DataRequest> = automations
            .iter()
            .map(|automation| {
                let values = json!({
                    "id": Uuid::now_v7(), "automation": automation, "kind": kind, "input": input,
                });
                DataRequest::insert("triggers", values)
            })
            .collect();
        for batch in writes.chunks(100) {
            self.0.batch(batch.to_vec()).await?;
        }
        Ok(())
    }

    /// The next waiting triggers, and any that an engine which stopped held for five minutes.
    pub async fn claim(&self, limit: i64) -> Result<Vec<Claimed>, Refusal> {
        let stale = (Utc::now() - Duration::minutes(5)).to_rfc3339();
        let query = Query::new("triggers")
            .filter(json!({ "any": [
                { "state": "pending" },
                { "state": "claimed", "claimed_at": { "lt": stale } },
            ] }))
            .order(Order::asc("received_at"))
            .limit(limit.clamp(1, 1_000) as u32);
        let waiting: Vec<Value> = self.0.query(query).await?.records;
        let mut claimed = Vec::new();
        for trigger in waiting {
            let Some(id) = trigger.get("id").and_then(Value::as_str) else { continue };
            let version = trigger.get("_version").and_then(Value::as_i64);
            let set = json!({ "state": "claimed", "claimed_at": now() });
            // Another engine that claimed it first moved its version on, so this one leaves it.
            match self.0.update::<Value>("triggers", id, set, version).await {
                Ok(Some(_)) => claimed.push(serde_json::from_value(trigger).map_err(|err| {
                    Refusal::unavailable(format!("a stored trigger could not be read: {err}"))
                })?),
                Ok(None) => {}
                Err(err) if conflict(&err) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(claimed)
    }

    pub async fn settle(
        &self,
        trigger: Uuid,
        state: &str,
        run: Option<Uuid>,
        detail: Option<&str>,
    ) -> Result<(), Refusal> {
        let set = json!({ "state": state, "run": run, "detail": detail, "done_at": now() });
        let _: Option<Value> = self.0.update("triggers", trigger.to_string(), set, None).await?;
        Ok(())
    }

    pub async fn triggers(&self, automation: Uuid, limit: i64) -> Result<Vec<Value>, Refusal> {
        let query = Query::new("triggers")
            .filter(json!({ "automation": automation }))
            .order(Order::asc("automation"))
            .order(Order::desc("received_at"))
            .fields(&["id", "kind", "input", "state", "run", "detail", "received_at", "done_at"])
            .limit(limit.clamp(1, 1_000) as u32);
        Ok(self.0.query(query).await?.records)
    }

    pub async fn new_run(&self, run: &Run) -> Result<(), Refusal> {
        let values = json!({
            "id": run.id, "automation": run.automation, "trigger": run.trigger,
            "input": run.input, "max_attempts": run.max_attempts, "test": run.test,
        });
        let _: Value = self.0.insert("runs", values).await?;
        Ok(())
    }

    pub async fn queued(&self, run: Uuid, task: Uuid) -> Result<(), Refusal> {
        let _: Option<Value> =
            self.0.update("runs", run.to_string(), json!({ "task": task }), None).await?;
        Ok(())
    }

    pub async fn run(&self, id: Uuid) -> Result<Option<Run>, Refusal> {
        Ok(self.0.get("runs", id.to_string()).await?)
    }

    pub async fn runs(&self, automation: Uuid, limit: i64) -> Result<Vec<Run>, Refusal> {
        let query = Query::new("runs")
            .filter(json!({ "automation": automation }))
            .order(Order::desc("_created_at"))
            .limit(limit.clamp(1, 1_000) as u32);
        Ok(self.0.query(query).await?.records)
    }

    /// Each automation's latest run: its state and when it was queued.
    pub async fn latest_runs(&self) -> Result<BTreeMap<Uuid, (String, String)>, Refusal> {
        let latest = Aggregate::new("runs")
            .group_by("automation")
            .group_by("state")
            .measure("at", Measure::Max("_created_at".into()));
        let mut found: BTreeMap<Uuid, (String, String)> = BTreeMap::new();
        for group in self.0.aggregate(latest).await? {
            let text = |field: &str| group.get(field).and_then(Value::as_str).map(str::to_string);
            let (Some(automation), Some(state), Some(at)) =
                (text("automation"), text("state"), text("at"))
            else {
                continue;
            };
            let Ok(automation) = automation.parse() else { continue };
            if found.get(&automation).is_none_or(|(_, seen)| *seen < at) {
                found.insert(automation, (state, at));
            }
        }
        Ok(found)
    }

    /// An attempt starting: its count goes up and it is running again.
    pub async fn attempt(&self, run: Uuid) -> Result<(), Refusal> {
        self.change("runs", run, |current| {
            let attempts = current.get("attempts").and_then(Value::as_i64).unwrap_or(0);
            let started = current.get("started_at").filter(|at| !at.is_null()).cloned().unwrap_or_else(|| json!(now()));
            Some(json!({ "state": "running", "attempts": attempts + 1, "error": null, "started_at": started }))
        })
        .await?;
        Ok(())
    }

    pub async fn record(
        &self,
        run: Uuid,
        state: &str,
        steps: &[Value],
        error: Option<&str>,
    ) -> Result<(), Refusal> {
        let finished = matches!(state, "succeeded" | "failed" | "skipped").then(now);
        let set =
            json!({ "state": state, "steps": steps, "error": error, "finished_at": finished });
        let _: Option<Value> = self.0.update("runs", run.to_string(), set, None).await?;
        Ok(())
    }
}
