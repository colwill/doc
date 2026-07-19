//! What the plugin keeps: calendars, one for each resource, who subscribes to which, and the secret
//! URLs of each person's ICS feeds.

use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::{
    Backend, Collection, Declaration, Field, OnDelete, Order, PluginError, Query,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "calendars",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("resource", Field::text().required())
                .field("name", Field::text().required())
                .field("description", Field::text().required().default(json!("")))
                .field("timezone", Field::text().required().default(json!("UTC")))
                .field("owner", Field::text())
                .field("created_by", Field::text().required())
                .field(
                    "slot",
                    Field::text()
                        .required()
                        .describe("One calendar per resource, and one of their own per person"),
                )
                .unique(&["slot"])
                .index(&["name"]),
        )
        .collection(
            "subscriptions",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("principal", Field::text().required())
                .field(
                    "calendar",
                    Field::reference("calendars").required().on_delete(OnDelete::Cascade),
                )
                .field(
                    "muted",
                    Field::boolean()
                        .required()
                        .default(json!(false))
                        .describe("Turned off, for a calendar somebody is in by default"),
                )
                .unique(&["principal", "calendar"]),
        )
        .collection(
            "feeds",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("principal", Field::text().required())
                .field("label", Field::text().required())
                .field("calendar", Field::reference("calendars").on_delete(OnDelete::Cascade))
                .field("token", Field::text().required())
                .field("rotated_at", Field::timestamp())
                .field(
                    "slot",
                    Field::text().required().describe("One feed per person and calendar"),
                )
                .unique(&["token"])
                .unique(&["slot"])
                .index(&["principal"]),
        )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Calendar {
    pub id: Uuid,
    /// `kind:name`; a person's own calendar is `user:<login>`, and only its owner sees it.
    pub resource: String,
    pub name: String,
    pub description: String,
    pub timezone: String,
    /// `user:<id>` for a person's own calendar.
    pub owner: Option<String>,
    pub created_by: String,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    #[serde(alias = "_updated_at", default)]
    pub updated_at: String,
}

impl Calendar {
    fn slot(&self) -> String {
        match &self.owner {
            Some(owner) => format!("owner:{owner}"),
            None => format!("resource:{}", self.resource),
        }
    }
}

/// What somebody said about one calendar: `muted` is a team's calendar they have turned off.
#[derive(Debug, Clone, Copy)]
pub struct Subscribed {
    pub calendar: Uuid,
    pub muted: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Feed {
    pub id: Uuid,
    pub principal: String,
    pub label: String,
    /// None for the feed of everything its owner subscribes to.
    pub calendar: Option<Uuid>,
    /// The feed's URL is the capability, so its owner can see it again; nothing else may.
    pub token: Secret<String>,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    pub rotated_at: Option<String>,
}

fn feed_slot(principal: &str, calendar: Option<Uuid>) -> String {
    let calendar = calendar.map_or_else(|| "all".to_string(), |id| id.to_string());
    format!("{principal}|{calendar}")
}

/// Another writer got there first, so the one it made is what there is.
fn raced(err: &PluginError) -> bool {
    err.is_duplicate()
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    async fn one<T: serde::de::DeserializeOwned>(
        &self,
        collection: &str,
        filter: Value,
    ) -> Result<Option<T>, Refusal> {
        let page = self.0.query(Query::new(collection).filter(filter).limit(1)).await?;
        Ok(page.records.into_iter().next())
    }

    pub async fn calendar(&self, id: Uuid) -> Result<Option<Calendar>, Refusal> {
        Ok(self.0.get("calendars", id.to_string()).await?)
    }

    /// The shared calendar of a resource; people's own calendars are found by their owner.
    pub async fn for_resource(&self, resource: &str) -> Result<Option<Calendar>, Refusal> {
        self.one("calendars", json!({ "slot": format!("resource:{resource}") })).await
    }

    pub async fn for_owner(&self, owner: &str) -> Result<Option<Calendar>, Refusal> {
        self.one("calendars", json!({ "slot": format!("owner:{owner}") })).await
    }

    pub async fn calendars(&self) -> Result<Vec<Calendar>, Refusal> {
        Ok(self.0.query_all(Query::new("calendars").order(Order::asc("name"))).await?)
    }

    /// Makes the calendar unless one for the resource already exists, answering whichever there is.
    pub async fn ensure(&self, calendar: &Calendar) -> Result<Calendar, Refusal> {
        let slot = calendar.slot();
        if let Some(existing) = self.one("calendars", json!({ "slot": slot })).await? {
            return Ok(existing);
        }
        let values = json!({
            "id": calendar.id, "resource": calendar.resource, "name": calendar.name,
            "description": calendar.description, "timezone": calendar.timezone,
            "owner": calendar.owner, "created_by": calendar.created_by, "slot": slot,
        });
        match self.0.insert("calendars", values).await {
            Ok(made) => Ok(made),
            Err(err) if raced(&err) => self
                .one("calendars", json!({ "slot": calendar.slot() }))
                .await?
                .ok_or_else(|| Refusal::unavailable("the calendar was not saved")),
            Err(err) => Err(err.into()),
        }
    }

    pub async fn update(&self, calendar: &Calendar) -> Result<(), Refusal> {
        let set = json!({
            "name": calendar.name, "description": calendar.description, "timezone": calendar.timezone,
        });
        let _: Option<Value> =
            self.0.update("calendars", calendar.id.to_string(), set, None).await?;
        Ok(())
    }

    pub async fn delete(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("calendars", id.to_string(), None).await?;
        Ok(())
    }

    /// What somebody has said about each calendar: the ones they asked for, and the ones they
    /// are in by default and have turned off.
    pub async fn subscriptions(&self, principal: &str) -> Result<Vec<Subscribed>, Refusal> {
        let query = Query::new("subscriptions").filter(json!({ "principal": principal }));
        let found: Vec<Value> = self.0.query_all(query).await?;
        Ok(found
            .iter()
            .filter_map(|row| {
                Some(Subscribed {
                    calendar: row.get("calendar")?.as_str()?.parse().ok()?,
                    muted: row.get("muted").and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect())
    }

    /// Turns a calendar on or off for somebody. Turning off one they are in `by_default`, such as
    /// their team's, is remembered as a mute; anything else is simply forgotten.
    pub async fn subscribe(
        &self,
        principal: &str,
        calendar: Uuid,
        on: bool,
        by_default: bool,
    ) -> Result<(), Refusal> {
        let values = json!({ "principal": principal, "calendar": calendar });
        match (on, by_default) {
            (true, _) | (false, true) => {
                let mut values = values;
                values["muted"] = json!(!on);
                let _: (Value, bool) =
                    self.0.upsert("subscriptions", &["principal", "calendar"], values).await?;
            }
            (false, false) => {
                self.0.delete_where("subscriptions", "id", values).await?;
            }
        }
        Ok(())
    }

    pub async fn feeds(&self, principal: &str) -> Result<Vec<Feed>, Refusal> {
        let query = Query::new("feeds")
            .filter(json!({ "principal": principal }))
            .order(Order::asc("_created_at"));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn feed(&self, id: Uuid) -> Result<Option<Feed>, Refusal> {
        Ok(self.0.get("feeds", id.to_string()).await?)
    }

    pub async fn feed_by_token(&self, token: &str) -> Result<Option<Feed>, Refusal> {
        self.one("feeds", json!({ "token": token })).await
    }

    /// A person's feed of one calendar, or of their subscriptions, made the first time it is asked for.
    pub async fn ensure_feed(&self, feed: &Feed) -> Result<Feed, Refusal> {
        let slot = feed_slot(&feed.principal, feed.calendar);
        if let Some(existing) = self.one("feeds", json!({ "slot": slot })).await? {
            return Ok(existing);
        }
        let values = json!({
            "id": feed.id, "principal": feed.principal, "label": feed.label,
            "calendar": feed.calendar, "token": feed.token.expose(), "slot": slot,
        });
        match self.0.insert("feeds", values).await {
            Ok(made) => Ok(made),
            Err(err) if raced(&err) => self
                .one("feeds", json!({ "slot": feed_slot(&feed.principal, feed.calendar) }))
                .await?
                .ok_or_else(|| Refusal::unavailable("the feed was not saved")),
            Err(err) => Err(err.into()),
        }
    }

    pub async fn rotate(&self, id: Uuid, token: &Secret<String>) -> Result<(), Refusal> {
        let set = json!({ "token": token.expose(), "rotated_at": chrono::Utc::now().to_rfc3339() });
        let _: Option<Value> = self.0.update("feeds", id.to_string(), set, None).await?;
        Ok(())
    }

    pub async fn delete_feed(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("feeds", id.to_string(), None).await?;
        Ok(())
    }
}
