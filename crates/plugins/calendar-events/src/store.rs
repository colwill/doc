//! What the plugin keeps: events, with their wall-clock times, rule and exceptions, each person's
//! answer to them, and which reminders have gone out.

use doc_plugin_sdk::{Backend, Collection, Declaration, Field, ListOf, OnDelete, Order, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "events",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("calendar", Field::uuid().required())
                .field("calendar_resource", Field::text().required())
                .field("title", Field::text().required())
                .field("description", Field::text().required().default(json!("")))
                .field("location", Field::text().required().default(json!("")))
                .field("link", Field::text().required().default(json!("")))
                .field("all_day", Field::boolean().required().default(json!(false)))
                .field(
                    "away",
                    Field::text()
                        .required()
                        .default(json!(""))
                        .one_of(&["", "holiday", "sick", "other"])
                        .describe("That the person whose calendar it is will not be working"),
                )
                .field("timezone", Field::text().required().default(json!("UTC")))
                .field("starts_local", Field::text().required())
                .field("ends_local", Field::text().required())
                .field("rrule", Field::text())
                .field("exdates", Field::list(ListOf::Text).required().default(json!([])))
                .field("starts_utc", Field::timestamp().required())
                .field("until_utc", Field::timestamp())
                .field("reminder_minutes", Field::integer())
                .field("attendees", Field::json().required().default(json!([])))
                .field("resource", Field::text())
                .field("source_plugin", Field::text())
                .field("created_by", Field::text().required())
                .field("created_by_label", Field::text().required())
                .field("sequence", Field::integer().required().default(json!(0)))
                .index(&["calendar", "starts_utc"])
                .index(&["starts_utc"]),
        )
        .collection(
            "rsvps",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("event", Field::reference("events").required().on_delete(OnDelete::Cascade))
                .field("occurrence", Field::text().required().default(json!("")))
                .field("principal", Field::text().required())
                .field("label", Field::text().required())
                .field("response", Field::text().required().one_of(&["yes", "no", "maybe"]))
                .unique(&["event", "occurrence", "principal"]),
        )
        .collection(
            "reminders",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("event", Field::reference("events").required().on_delete(OnDelete::Cascade))
                .field("occurrence", Field::text().required())
                .unique(&["event", "occurrence"]),
        )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub calendar: Uuid,
    /// The calendar's resource, `kind:name`, which reminders name.
    pub calendar_resource: String,
    pub title: String,
    pub description: String,
    pub location: String,
    pub link: String,
    pub all_day: bool,
    /// Why the person whose calendar this is will not be working: `holiday`, `sick` or `other`,
    /// and empty for an event that says nothing about it. Only a person's own calendar carries it
    /// — a meeting in a team's calendar is not anybody being away.
    #[serde(default)]
    pub away: String,
    pub timezone: String,
    /// Wall-clock times in `timezone`; an all-day event runs from one midnight to another.
    pub starts_local: String,
    pub ends_local: String,
    pub rrule: Option<String>,
    /// Wall-clock starts of occurrences that do not happen.
    pub exdates: Vec<String>,
    pub starts_utc: String,
    pub until_utc: Option<String>,
    pub reminder_minutes: Option<i64>,
    pub attendees: Vec<Value>,
    pub resource: Option<String>,
    pub source_plugin: Option<String>,
    pub created_by: String,
    pub created_by_label: String,
    pub sequence: i64,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    #[serde(alias = "_updated_at", default)]
    pub updated_at: String,
}

impl Event {
    /// What an edit may change; who made the event and where it lives stay as they were.
    fn changes(&self) -> Value {
        json!({
            "title": self.title, "description": self.description, "location": self.location,
            "link": self.link, "all_day": self.all_day, "away": self.away,
            "timezone": self.timezone,
            "starts_local": self.starts_local, "ends_local": self.ends_local, "rrule": self.rrule,
            "exdates": self.exdates, "starts_utc": self.starts_utc, "until_utc": self.until_utc,
            "reminder_minutes": self.reminder_minutes, "attendees": self.attendees,
            "resource": self.resource, "sequence": self.sequence,
        })
    }

    fn values(&self) -> Value {
        let mut values = self.changes();
        let fixed = json!({
            "id": self.id, "calendar": self.calendar, "calendar_resource": self.calendar_resource,
            "source_plugin": self.source_plugin, "created_by": self.created_by,
            "created_by_label": self.created_by_label,
        });
        if let (Some(values), Value::Object(fixed)) = (values.as_object_mut(), fixed) {
            values.extend(fixed);
        }
        values
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rsvp {
    pub event: Uuid,
    pub occurrence: String,
    pub principal: String,
    pub label: String,
    pub response: String,
    #[serde(alias = "_updated_at", default)]
    pub updated_at: String,
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    pub async fn event(&self, id: Uuid) -> Result<Option<Event>, Refusal> {
        Ok(self.0.get("events", id.to_string()).await?)
    }

    /// Events on these calendars that may have an occurrence between `from` and `to`.
    pub async fn around(
        &self,
        calendars: &[Uuid],
        from: &str,
        to: &str,
    ) -> Result<Vec<Event>, Refusal> {
        let query = Query::new("events")
            .filter(json!({
                "calendar": { "in": calendars },
                "starts_utc": { "lt": to },
                "any": [{ "until_utc": null }, { "until_utc": { "gt": from } }],
            }))
            .order(Order::asc("starts_utc"));
        Ok(self.0.query_all(query).await?)
    }

    /// Every event saying somebody is away, on the calendars named, that could touch `from`..`to`.
    /// A recurring one is kept whatever its window, since the rule decides when it lands.
    pub async fn away_on(
        &self,
        calendars: &[String],
        from: &str,
        to: &str,
    ) -> Result<Vec<Event>, Refusal> {
        let query = Query::new("events")
            .filter(json!({
                "calendar_resource": { "in": calendars },
                "away": { "ne": "" },
                "starts_utc": { "lt": to },
                "any": [{ "until_utc": null }, { "until_utc": { "gt": from } }],
            }))
            .order(Order::asc("starts_utc"));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn on(&self, calendars: &[Uuid]) -> Result<Vec<Event>, Refusal> {
        let query = Query::new("events")
            .filter(json!({ "calendar": { "in": calendars } }))
            .order(Order::asc("starts_utc"));
        Ok(self.0.query_all(query).await?)
    }

    /// Events with reminders whose occurrences may start before `until`.
    pub async fn reminded(&self, now: &str, until: &str) -> Result<Vec<Event>, Refusal> {
        let query = Query::new("events").filter(json!({
            "reminder_minutes": { "is_null": false },
            "starts_utc": { "lt": until },
            "any": [{ "until_utc": null }, { "until_utc": { "gt": now } }],
        }));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn save(&self, event: &Event) -> Result<(), Refusal> {
        let id = event.id.to_string();
        let updated: Option<Value> = self.0.update("events", id, event.changes(), None).await?;
        if updated.is_none() {
            let _: Value = self.0.insert("events", event.values()).await?;
        }
        Ok(())
    }

    pub async fn delete(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("events", id.to_string(), None).await?;
        Ok(())
    }

    pub async fn delete_calendar(&self, calendar: Uuid) -> Result<(), Refusal> {
        self.0.delete_where("events", "id", json!({ "calendar": calendar })).await?;
        Ok(())
    }

    pub async fn answer(&self, rsvp: &Rsvp) -> Result<(), Refusal> {
        let values = json!({
            "event": rsvp.event, "occurrence": rsvp.occurrence, "principal": rsvp.principal,
            "label": rsvp.label, "response": rsvp.response,
        });
        let _: (Value, bool) =
            self.0.upsert("rsvps", &["event", "occurrence", "principal"], values).await?;
        Ok(())
    }

    pub async fn rsvps(&self, events: &[Uuid]) -> Result<Vec<Rsvp>, Refusal> {
        let query = Query::new("rsvps")
            .filter(json!({ "event": { "in": events } }))
            .order(Order::asc("_updated_at"));
        Ok(self.0.query_all(query).await?)
    }

    /// Takes the reminder for one occurrence, answering whether it was still to be sent.
    pub async fn remind(&self, event: Uuid, occurrence: &str) -> Result<bool, Refusal> {
        let values = json!({ "event": event, "occurrence": occurrence });
        let (_, taken): (Value, bool) =
            self.0.upsert("reminders", &["event", "occurrence"], values).await?;
        Ok(taken)
    }
}
