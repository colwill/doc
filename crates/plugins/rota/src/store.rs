//! What the plugin keeps: each team's rotas, the shifts planned from them, and the holidays leads
//! record for their people.

use chrono::{DateTime, NaiveDate, Utc};
use doc_plugin_sdk::{Backend, Collection, Declaration, Field, ListOf, OnDelete, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;

pub const CADENCES: [(&str, &str); 4] =
    [("daily", "Daily"), ("weekly", "Weekly"), ("monthly", "Monthly"), ("quarterly", "Quarterly")];

/// Someone from the rotation, as it runs.
pub const PLANNED: &str = "planned";
/// Someone the lead picked instead.
pub const COVER: &str = "cover";
/// Nobody: whoever the rotation gives is away, and nobody has been picked instead.
pub const GAP: &str = "gap";

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "rotas",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("team_id", Field::uuid().required().describe("The core team it is for"))
                .field("team", Field::text().required().describe("That team's name"))
                .field("title", Field::text().required())
                .field("description", Field::text().required().default(json!("")))
                .field(
                    "cadence",
                    Field::text().required().one_of(&["daily", "weekly", "monthly", "quarterly"]),
                )
                .field("starts_on", Field::date().required().describe("The first shift's day"))
                .field(
                    "handover",
                    Field::text()
                        .required()
                        .default(json!("09:00"))
                        .describe("The time of day one shift hands over to the next, as HH:MM"),
                )
                .field("timezone", Field::text().required().default(json!("UTC")))
                .field(
                    "members",
                    Field::list(ListOf::Text)
                        .required()
                        .default(json!([]))
                        .describe("The logins it runs through, in order"),
                )
                .field(
                    "calendar",
                    Field::boolean()
                        .required()
                        .default(json!(true))
                        .describe("Whether shifts go on the team's and the person's calendars"),
                )
                .field("created_by", Field::text().required())
                .index(&["team_id"]),
        )
        .collection(
            "shifts",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("rota", Field::reference("rotas").required().on_delete(OnDelete::Cascade))
                .field("period", Field::integer().required().describe("Which turn of the rota"))
                .field("starts_at", Field::timestamp().required())
                .field("ends_at", Field::timestamp().required())
                .field("planned", Field::text().describe("Whom the rotation gives"))
                .field("assignee", Field::text().describe("Who is on; none for a gap"))
                .field("state", Field::text().required().one_of(&[PLANNED, COVER, GAP]))
                .field(
                    "clash",
                    Field::boolean()
                        .required()
                        .default(json!(false))
                        .describe("The cover a lead picked is away for some of it"),
                )
                .field(
                    "flagged",
                    Field::boolean()
                        .required()
                        .default(json!(false))
                        .describe("Whether the lead has been told about its gap or clash"),
                )
                .field("user_event", Field::uuid())
                .field("user_event_for", Field::text())
                .field("team_event", Field::uuid())
                .field(
                    "synced",
                    Field::text().describe("What its calendar events last said, to skip no change"),
                )
                .unique(&["rota", "period"])
                .index(&["rota", "starts_at"])
                .index(&["assignee", "starts_at"]),
        )
        .collection(
            "holidays",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("user", Field::text().required().describe("Their login"))
                .field("user_id", Field::uuid().required())
                .field("starts_on", Field::date().required())
                .field("ends_on", Field::date().required().describe("The last day away"))
                .field("note", Field::text().required().default(json!("")))
                .field("recorded_by", Field::text().required())
                .index(&["user", "starts_on"])
                .index(&["ends_on"]),
        )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rota {
    pub id: Uuid,
    pub team_id: Uuid,
    pub team: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub cadence: String,
    pub starts_on: NaiveDate,
    pub handover: String,
    pub timezone: String,
    #[serde(default)]
    pub members: Vec<String>,
    pub calendar: bool,
    pub created_by: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Shift {
    pub id: Uuid,
    pub rota: Uuid,
    pub period: i64,
    pub starts_at: DateTime<Utc>,
    pub ends_at: DateTime<Utc>,
    #[serde(default)]
    pub planned: Option<String>,
    #[serde(default)]
    pub assignee: Option<String>,
    pub state: String,
    #[serde(default)]
    pub clash: bool,
    #[serde(default)]
    pub flagged: bool,
    #[serde(default)]
    pub user_event: Option<Uuid>,
    #[serde(default)]
    pub user_event_for: Option<String>,
    #[serde(default)]
    pub team_event: Option<Uuid>,
    #[serde(default)]
    pub synced: Option<String>,
}

impl Shift {
    /// Whether the lead needs to do something about it.
    pub fn wants_cover(&self) -> bool {
        self.state == GAP || self.clash
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Holiday {
    pub id: Uuid,
    pub user: String,
    pub user_id: Uuid,
    pub starts_on: NaiveDate,
    pub ends_on: NaiveDate,
    #[serde(default)]
    pub note: String,
    pub recorded_by: String,
    /// `calendar` for an absence read from somebody's own calendar rather than recorded here;
    /// empty for one a lead wrote down. Not stored: a calendar's is never written.
    #[serde(default, skip_serializing)]
    pub source: String,
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    /// Every rota, by title.
    pub async fn rotas(&self) -> Result<Vec<Rota>, Refusal> {
        let mut rotas: Vec<Rota> = self.0.query_all(Query::new("rotas")).await?;
        rotas.sort_by_key(|rota| rota.title.to_lowercase());
        Ok(rotas)
    }

    pub async fn rotas_of(&self, team: Uuid) -> Result<Vec<Rota>, Refusal> {
        let query = Query::new("rotas").filter(json!({ "team_id": team }));
        let mut rotas: Vec<Rota> = self.0.query_all(query).await?;
        rotas.sort_by_key(|rota| rota.title.to_lowercase());
        Ok(rotas)
    }

    pub async fn rota(&self, id: Uuid) -> Result<Option<Rota>, Refusal> {
        Ok(self.0.get("rotas", id.to_string()).await?)
    }

    pub async fn save_rota(&self, rota: &Rota) -> Result<(), Refusal> {
        let values = serde_json::to_value(rota).unwrap_or_default();
        let mut set = values.clone();
        if let Some(fields) = set.as_object_mut() {
            fields.remove("id");
        }
        let updated: Option<Value> = self.0.update("rotas", rota.id.to_string(), set, None).await?;
        if updated.is_none() {
            let _: Value = self.0.insert("rotas", values).await?;
        }
        Ok(())
    }

    pub async fn delete_rota(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("rotas", id.to_string(), None).await?;
        Ok(())
    }

    /// The shifts of `rota` that end after `from`.
    pub async fn shifts_from(
        &self,
        rota: Uuid,
        from: DateTime<Utc>,
    ) -> Result<Vec<Shift>, Refusal> {
        let query = Query::new("shifts")
            .filter(json!({ "rota": rota, "ends_at": { "gt": from.to_rfc3339() } }))
            .order(Order::asc("rota"))
            .order(Order::asc("starts_at"));
        Ok(self.0.query_all(query).await?)
    }

    /// Someone's shifts that end after `from`, in every rota.
    pub async fn shifts_of(&self, login: &str, from: DateTime<Utc>) -> Result<Vec<Shift>, Refusal> {
        let query = Query::new("shifts")
            .filter(json!({ "assignee": login, "ends_at": { "gt": from.to_rfc3339() } }))
            .order(Order::asc("assignee"))
            .order(Order::asc("starts_at"));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn shift(&self, id: Uuid) -> Result<Option<Shift>, Refusal> {
        Ok(self.0.get("shifts", id.to_string()).await?)
    }

    pub async fn save_shift(&self, shift: &Shift) -> Result<(), Refusal> {
        let values = serde_json::to_value(shift).unwrap_or_default();
        let mut set = values.clone();
        if let Some(fields) = set.as_object_mut() {
            fields.remove("id");
            fields.remove("rota");
            fields.remove("period");
        }
        let updated: Option<Value> =
            self.0.update("shifts", shift.id.to_string(), set, None).await?;
        if updated.is_none() {
            let _: Value = self.0.insert("shifts", values).await?;
        }
        Ok(())
    }

    pub async fn delete_shift(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("shifts", id.to_string(), None).await?;
        Ok(())
    }

    /// Every holiday that has not ended before `from`.
    pub async fn holidays_from(&self, from: NaiveDate) -> Result<Vec<Holiday>, Refusal> {
        let query =
            Query::new("holidays").filter(json!({ "ends_on": { "gte": from.to_string() } }));
        let mut holidays: Vec<Holiday> = self.0.query_all(query).await?;
        holidays.sort_by_key(|holiday| (holiday.starts_on, holiday.user.clone()));
        Ok(holidays)
    }

    pub async fn holiday(&self, id: Uuid) -> Result<Option<Holiday>, Refusal> {
        Ok(self.0.get("holidays", id.to_string()).await?)
    }

    pub async fn add_holiday(&self, holiday: &Holiday) -> Result<(), Refusal> {
        let _: Value =
            self.0.insert("holidays", serde_json::to_value(holiday).unwrap_or_default()).await?;
        Ok(())
    }

    pub async fn delete_holiday(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("holidays", id.to_string(), None).await?;
        Ok(())
    }
}
