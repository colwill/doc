//! What the plugin keeps: processes, and each occurrence with its status, its checklist as ticked,
//! the calendar event it was given, and whether its reminder and missed notice have gone out.

use doc_plugin_sdk::{
    Backend, Collection, Declaration, Field, ListOf, OnDelete, Order, PluginError, Query,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::Refusal;

const OPEN: [&str; 2] = ["pending", "missed"];

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "processes",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("resource", Field::text().required())
                .field("title", Field::text().required())
                .field("description", Field::text().required().default(json!("")))
                .field(
                    "cadence",
                    Field::text().required().one_of(&[
                        "daily",
                        "weekly",
                        "monthly",
                        "quarterly",
                        "yearly",
                    ]),
                )
                .field("weekday", Field::integer().required().default(json!(1)))
                .field("month", Field::integer().required().default(json!(1)))
                .field("day", Field::integer().required().default(json!(1)))
                .field("at_time", Field::text().required())
                .field("timezone", Field::text().required())
                .field("duration_minutes", Field::integer().required().default(json!(30)))
                .field("remind_minutes", Field::integer())
                .field("grace_minutes", Field::integer().required().default(json!(1440)))
                .field("owner", Field::text().required())
                .field("assignees", Field::json().required().default(json!([])))
                .field("checklist", Field::list(ListOf::Text).required().default(json!([])))
                .field("plan_from", Field::timestamp().required())
                .field("created_by", Field::text().required())
                .index(&["resource", "title"]),
        )
        .collection(
            "occurrences",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "process",
                    Field::reference("processes").required().on_delete(OnDelete::Cascade),
                )
                .field("due_local", Field::text().required())
                .field("due_at", Field::timestamp().required())
                .field(
                    "status",
                    Field::text()
                        .required()
                        .default(json!("pending"))
                        .one_of(&["pending", "done", "skipped", "missed"]),
                )
                .field("checklist", Field::json().required().default(json!([])))
                .field("event", Field::uuid())
                .field("note", Field::text().required().default(json!("")))
                .field("finished_by", Field::text())
                .field("finished_at", Field::timestamp())
                .field("reminded_at", Field::timestamp())
                .field("missed_at", Field::timestamp())
                .unique(&["process", "due_local"])
                .index(&["due_at"]),
        )
}

/// Whom a process is for: a person by login, or every member of a team.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Assignee {
    User(String),
    Team(String),
}

impl Assignee {
    pub fn shown(&self) -> String {
        match self {
            Self::User(login) => login.clone(),
            Self::Team(team) => format!("team {team}"),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Process {
    pub id: Uuid,
    /// `kind:name`, with the kind in lower case.
    pub resource: String,
    pub title: String,
    pub description: String,
    pub cadence: String,
    pub weekday: i64,
    pub month: i64,
    pub day: i64,
    /// `HH:MM`, a wall-clock time in `timezone`.
    pub at_time: String,
    pub timezone: String,
    pub duration_minutes: i64,
    pub remind_minutes: Option<i64>,
    /// How long after it is due an occurrence may still be done before it is missed.
    pub grace_minutes: i64,
    pub owner: String,
    pub assignees: Vec<Assignee>,
    pub checklist: Vec<String>,
    /// Occurrences are planned from here on; it moves to now whenever the schedule changes.
    pub plan_from: String,
    pub created_by: String,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    #[serde(alias = "_updated_at", default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Item {
    pub text: String,
    pub done: bool,
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Occurrence {
    pub id: Uuid,
    pub process: Uuid,
    /// The wall-clock time it is due, in the process's zone, which names it.
    pub due_local: String,
    pub due_at: String,
    pub status: String,
    pub checklist: Vec<Item>,
    pub event: Option<Uuid>,
    pub note: String,
    pub finished_by: Option<String>,
    pub finished_at: Option<String>,
    pub reminded_at: Option<String>,
    pub missed_at: Option<String>,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    #[serde(alias = "_updated_at", default)]
    pub updated_at: String,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn text<'a>(record: &'a Map<String, Value>, field: &str) -> Option<&'a str> {
    record.get(field).and_then(Value::as_str)
}

/// A record another run made first, which is what `ON CONFLICT DO NOTHING` used to swallow.
fn taken(err: &PluginError) -> bool {
    err.is_duplicate()
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    async fn occurrences(&self, filter: Value) -> Result<Vec<Occurrence>, Refusal> {
        let query = Query::new("occurrences").filter(filter).order(Order::asc("due_at"));
        Ok(self.0.query_all(query).await?)
    }

    /// Changes an occurrence as it is now, answering whether `change` agreed to.
    async fn change(
        &self,
        id: Uuid,
        change: impl Fn(&Map<String, Value>) -> Option<Value>,
    ) -> Result<bool, Refusal> {
        Ok(self.0.change("occurrences", id.to_string(), change).await?.is_some())
    }

    pub async fn process(&self, id: Uuid) -> Result<Option<Process>, Refusal> {
        Ok(self.0.get("processes", id.to_string()).await?)
    }

    pub async fn processes(&self) -> Result<Vec<Process>, Refusal> {
        let query =
            Query::new("processes").order(Order::asc("resource")).order(Order::asc("title"));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn on(&self, resource: &str) -> Result<Vec<Process>, Refusal> {
        let query = Query::new("processes")
            .filter(json!({ "resource": resource }))
            .order(Order::asc("resource"))
            .order(Order::asc("title"));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn save_process(&self, process: &Process) -> Result<(), Refusal> {
        let set = json!({
            "title": process.title, "description": process.description, "cadence": process.cadence,
            "weekday": process.weekday, "month": process.month, "day": process.day,
            "at_time": process.at_time, "timezone": process.timezone,
            "duration_minutes": process.duration_minutes, "remind_minutes": process.remind_minutes,
            "grace_minutes": process.grace_minutes, "owner": process.owner,
            "assignees": process.assignees, "checklist": process.checklist,
            "plan_from": process.plan_from,
        });
        let updated: Option<Value> =
            self.0.update("processes", process.id.to_string(), set.clone(), None).await?;
        if updated.is_none() {
            let mut values = set;
            values["id"] = json!(process.id);
            values["resource"] = json!(process.resource);
            values["created_by"] = json!(process.created_by);
            let _: Value = self.0.insert("processes", values).await?;
        }
        Ok(())
    }

    pub async fn delete_process(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("processes", id.to_string(), None).await?;
        Ok(())
    }

    pub async fn occurrence(&self, id: Uuid) -> Result<Option<Occurrence>, Refusal> {
        Ok(self.0.get("occurrences", id.to_string()).await?)
    }

    /// A process's occurrences, soonest first.
    pub async fn occurrences_of(&self, process: Uuid) -> Result<Vec<Occurrence>, Refusal> {
        self.occurrences(json!({ "process": process })).await
    }

    /// Occurrences of these processes due between `from` and `to`, soonest first.
    pub async fn between(
        &self,
        processes: &[Uuid],
        from: &str,
        to: &str,
    ) -> Result<Vec<Occurrence>, Refusal> {
        self.occurrences(
            json!({ "process": { "in": processes }, "due_at": { "gte": from, "lt": to } }),
        )
        .await
    }

    /// Occurrences still pending that are due before `until`, soonest first.
    pub async fn pending_before(&self, until: &str) -> Result<Vec<Occurrence>, Refusal> {
        self.occurrences(json!({ "status": "pending", "due_at": { "lt": until } })).await
    }

    /// The wall-clock times each process already has an occurrence for, from `from` on.
    pub async fn planned(&self, from: &str) -> Result<Vec<(Uuid, String)>, Refusal> {
        let query = Query::new("occurrences")
            .filter(json!({ "due_at": { "gte": from } }))
            .fields(&["process", "due_local"]);
        let found: Vec<Map<String, Value>> = self.0.query_all(query).await?;
        Ok(found
            .iter()
            .filter_map(|row| {
                Some((text(row, "process")?.parse().ok()?, text(row, "due_local")?.to_string()))
            })
            .collect())
    }

    /// Whether the process already has an occurrence due between two wall-clock times.
    pub async fn occupied(&self, process: Uuid, from: &str, to: &str) -> Result<bool, Refusal> {
        let query = Query::new("occurrences")
            .filter(json!({ "process": process, "due_local": { "gte": from, "lt": to } }))
            .fields(&["id"])
            .limit(1);
        Ok(!self.0.query::<Value>(query).await?.records.is_empty())
    }

    /// Adds occurrences, answering the ones that were new.
    pub async fn add(&self, occurrences: &[Occurrence]) -> Result<Vec<Uuid>, Refusal> {
        let mut added = Vec::new();
        for occurrence in occurrences {
            let values = json!({
                "id": occurrence.id, "process": occurrence.process,
                "due_local": occurrence.due_local, "due_at": occurrence.due_at,
                "checklist": occurrence.checklist,
            });
            match self.0.insert::<Value>("occurrences", values).await {
                Ok(_) => added.push(occurrence.id),
                Err(err) if taken(&err) => {}
                Err(err) => return Err(err.into()),
            }
        }
        Ok(added)
    }

    /// Records the calendar event an occurrence was given, unless another run gave it one first.
    pub async fn claim_event(&self, occurrence: Uuid, event: Uuid) -> Result<bool, Refusal> {
        self.change(occurrence, |current| {
            current.get("event").is_none_or(Value::is_null).then(|| json!({ "event": event }))
        })
        .await
    }

    /// Pending occurrences that have no calendar event yet, due after `after`.
    pub async fn without_event(&self, after: &str) -> Result<Vec<Occurrence>, Refusal> {
        self.occurrences(json!({ "event": null, "status": "pending", "due_at": { "gt": after } }))
            .await
    }

    /// Replaces a pending occurrence's checklist, as its process's checklist changed.
    pub async fn set_checklist(&self, id: Uuid, checklist: &[Item]) -> Result<(), Refusal> {
        self.change(id, |current| {
            (text(current, "status") == Some("pending")).then(|| json!({ "checklist": checklist }))
        })
        .await?;
        Ok(())
    }

    /// Ticks or unticks one checklist item of an open occurrence, answering whether it was open.
    pub async fn tick(&self, id: Uuid, index: usize, item: &Item) -> Result<bool, Refusal> {
        self.change(id, |current| {
            if !OPEN.contains(&text(current, "status")?) {
                return None;
            }
            let mut checklist = current.get("checklist")?.as_array()?.clone();
            *checklist.get_mut(index)? = json!(item);
            Some(json!({ "checklist": checklist }))
        })
        .await
    }

    /// Marks an open occurrence done or skipped, answering whether it was still open.
    pub async fn finish(&self, occurrence: &Occurrence) -> Result<bool, Refusal> {
        self.change(occurrence.id, |current| {
            OPEN.contains(&text(current, "status")?).then(|| {
                json!({
                    "status": occurrence.status, "checklist": occurrence.checklist,
                    "note": occurrence.note, "finished_by": occurrence.finished_by,
                    "finished_at": now(),
                })
            })
        })
        .await
    }

    pub async fn delete_occurrence(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("occurrences", id.to_string(), None).await?;
        Ok(())
    }

    /// Takes an occurrence's reminder, answering whether it was still to be sent.
    pub async fn remind(&self, id: Uuid) -> Result<bool, Refusal> {
        self.change(id, |current| {
            let unsent = current.get("reminded_at").is_none_or(Value::is_null);
            (text(current, "status") == Some("pending") && unsent)
                .then(|| json!({ "reminded_at": now() }))
        })
        .await
    }

    /// Marks an occurrence missed, answering whether it was still pending.
    pub async fn miss(&self, id: Uuid) -> Result<bool, Refusal> {
        self.change(id, |current| {
            (text(current, "status") == Some("pending"))
                .then(|| json!({ "status": "missed", "missed_at": now() }))
        })
        .await
    }
}
