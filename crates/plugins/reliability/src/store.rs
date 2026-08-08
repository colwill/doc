//! What reliability keeps. A subject is whatever is judged: a service in the Catalogue
//! (`service:<name>`), a part of DOC (`doc:<component>`) or one of its plugins (`plugin:<id>`).
//! Each has its outages, the backups it can be restored from and the restores tried, and each
//! checked one how long it was watched each day, so a gap in checking is never taken for uptime.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Collection, Declaration, Field};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::settings::Targets;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "subjects",
            Collection::new()
                .field(
                    "subject",
                    Field::text().key().describe("service:<name>, doc:<component> or plugin:<id>"),
                )
                .field("kind", Field::text().required().one_of(&["service", "component", "plugin"]))
                .field("name", Field::text().required())
                .field(
                    "url",
                    Field::text().max(2_000.0).describe("The health URL checked every minute"),
                )
                .field(
                    "sla",
                    Field::number()
                        .min(0.0)
                        .max(100.0)
                        .describe("Its own availability objective, percent"),
                )
                .field("rto", Field::number().min(0.0).describe("Seconds"))
                .field("rpo", Field::number().min(0.0).describe("Seconds"))
                .field("mttr", Field::number().min(0.0).describe("Seconds"))
                .field(
                    "state",
                    Field::text()
                        .required()
                        .default(json!("unknown"))
                        .one_of(&["up", "down", "unknown"]),
                )
                .field(
                    "failing",
                    Field::integer()
                        .required()
                        .default(json!(0))
                        .describe("Checks failed in a row"),
                )
                .field("failing_since", Field::timestamp())
                .field("since", Field::timestamp().describe("When it was first watched"))
                .field("checked_at", Field::timestamp())
                .field("status", Field::integer().describe("What its last check answered"))
                .field("latency_ms", Field::integer())
                .field("problem", Field::text().max(500.0))
                .field("updated_by", Field::text())
                .index(&["kind"]),
        )
        .collection(
            "outages",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("subject", Field::text().required())
                .field("started_at", Field::timestamp().required())
                .field("ended_at", Field::timestamp())
                .field("seconds", Field::number())
                .field(
                    "source",
                    Field::text().required().describe(
                        "check (a failed health check), report (a report through the API), status \
                         (DOC's status history), or the ID of a plugin whose outages are read, \
                         such as kubernetes",
                    ),
                )
                .field("detail", Field::text().max(500.0))
                .field("by", Field::text())
                .index(&["started_at"])
                .index(&["subject", "started_at"]),
        )
        .collection(
            "backups",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("subject", Field::text().required())
                .field("at", Field::timestamp().required().describe("The point it restores to"))
                .field("kind", Field::text().max(64.0))
                .field("note", Field::text().max(500.0))
                .field("by", Field::text())
                .index(&["at"])
                .index(&["subject", "at"]),
        )
        .collection(
            "restores",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("subject", Field::text().required())
                .field("started_at", Field::timestamp().required())
                .field("finished_at", Field::timestamp().required())
                .field("seconds", Field::number().required())
                .field("succeeded", Field::boolean().required().default(json!(true)))
                .field("note", Field::text().max(500.0))
                .field("by", Field::text())
                .index(&["started_at"])
                .index(&["subject", "started_at"]),
        )
        // Telemetry: how each subject stood in each five minutes, which is what the shortest
        // periods a page can be shown over are drawn from. A week of it is kept and no more
        // (DOC-SPEC §9.19); `coverage` keeps the day's totals for longer than that.
        .collection(
            "samples",
            Collection::new()
                .field("id", Field::text().key().describe("subject/<the second it starts at>"))
                .field("subject", Field::text().required())
                .field("at", Field::timestamp().required().describe("The five minutes it covers"))
                .field("seconds", Field::number().required().default(json!(0)).describe("Watched"))
                .field("checks", Field::integer().required().default(json!(0)))
                .field("failed", Field::integer().required().default(json!(0)))
                .index(&["at"])
                .index(&["subject", "at"]),
        )
        .collection(
            "coverage",
            Collection::new()
                .field("id", Field::text().key().describe("subject/YYYY-MM-DD"))
                .field("subject", Field::text().required())
                .field("day", Field::timestamp().required())
                .field("seconds", Field::number().required().default(json!(0)).describe("Watched"))
                .field("checks", Field::integer().required().default(json!(0)))
                .field("failed", Field::integer().required().default(json!(0)))
                .index(&["day"])
                .index(&["subject", "day"]),
        )
}

/// Whatever is judged, with what it is held to where it sets its own objectives, and how its
/// checks stand.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Subject {
    pub subject: String,
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub sla: Option<f64>,
    #[serde(default)]
    pub rto: Option<f64>,
    #[serde(default)]
    pub rpo: Option<f64>,
    #[serde(default)]
    pub mttr: Option<f64>,
    #[serde(default = "unknown")]
    pub state: String,
    #[serde(default)]
    pub failing: i64,
    #[serde(default)]
    pub failing_since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub since: Option<DateTime<Utc>>,
    #[serde(default)]
    pub checked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(default)]
    pub latency_ms: Option<i64>,
    #[serde(default)]
    pub problem: Option<String>,
    #[serde(default)]
    pub updated_by: Option<String>,
}

fn unknown() -> String {
    "unknown".to_string()
}

impl Subject {
    pub fn service(name: &str) -> Self {
        Self {
            subject: format!("service:{name}"),
            kind: "service".into(),
            name: name.to_string(),
            state: unknown(),
            ..Self::default()
        }
    }

    /// Its own objectives where it has them, and `defaults` for the rest.
    pub fn targets(&self, defaults: Targets) -> Targets {
        Targets {
            sla: self.sla.unwrap_or(defaults.sla),
            rto: self.rto.unwrap_or(defaults.rto),
            rpo: self.rpo.unwrap_or(defaults.rpo),
            mttr: self.mttr.unwrap_or(defaults.mttr),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Outage {
    pub id: Uuid,
    pub subject: String,
    pub started_at: DateTime<Utc>,
    #[serde(default)]
    pub ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub seconds: Option<f64>,
    pub source: String,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
}

impl Outage {
    /// How long it lasted, or has lasted so far.
    pub fn lasted(&self, now: DateTime<Utc>) -> f64 {
        (self.ended_at.unwrap_or(now) - self.started_at).num_seconds().max(0) as f64
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Backup {
    pub id: Uuid,
    pub subject: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Restore {
    pub id: Uuid,
    pub subject: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub seconds: f64,
    #[serde(default = "yes")]
    pub succeeded: bool,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub by: Option<String>,
}

fn yes() -> bool {
    true
}

/// Five minutes of one subject: what a page shown over an hour, or six, is drawn from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Sample {
    pub id: String,
    pub subject: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub checks: i64,
    #[serde(default)]
    pub failed: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Covered {
    pub id: String,
    pub subject: String,
    pub day: DateTime<Utc>,
    #[serde(default)]
    pub seconds: f64,
    #[serde(default)]
    pub checks: i64,
    #[serde(default)]
    pub failed: i64,
}
