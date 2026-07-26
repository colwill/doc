//! What a notification is, and the one collection that holds every user's: created for them by
//! `discovery/notify`, read only by the user it was made for.

use chrono::Utc;
use doc_plugin_sdk::{Backend, Collection, Declaration, Field, Order, PluginError, Query};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::Refusal;

pub const MAX_TITLE: usize = 200;
pub const MAX_BODY: usize = 2_000;
pub const MAX_URL: usize = 2_000;
pub const MAX_SOURCE: usize = 100;
pub const PAGE: u32 = 20;
pub const BELL: u32 = 8;

pub fn declaration() -> Declaration {
    Declaration::default().collection(
        "notifications",
        Collection::new()
            .field("id", Field::uuid().key())
            // `core.users` is not this plugin's own, so it carries no `on_delete`: a deleted
            // user's notifications are just orphaned, the same as any other plugin's records of
            // someone who has left would be.
            .field(
                "user",
                Field::reference("core.users").required().describe("Who this was made for"),
            )
            .field("title", Field::text().required().max(MAX_TITLE as f64))
            .field("body", Field::text().required().default(json!("")).max(MAX_BODY as f64))
            .field("url", Field::text().required().default(json!("")).max(MAX_URL as f64))
            .field(
                "source",
                Field::text()
                    .required()
                    .default(json!(""))
                    .max(MAX_SOURCE as f64)
                    .describe("What made it: a plugin id, or automation"),
            )
            .field("read_at", Field::timestamp().describe("Unset until the user reads it"))
            .field("archived_at", Field::timestamp())
            .index(&["user"]),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notification {
    pub id: Uuid,
    pub user: Uuid,
    pub title: String,
    pub body: String,
    pub url: String,
    pub source: String,
    #[serde(default)]
    pub read_at: Option<String>,
    #[serde(default)]
    pub archived_at: Option<String>,
    #[serde(rename = "_created_at")]
    pub created_at: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct New {
    pub user: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub source: String,
}

fn required(text: &str, max: usize, what: &str) -> Result<String, Refusal> {
    let text = text.trim();
    match text.is_empty() || text.chars().count() > max {
        true => Err(Refusal::bad(format!("{what} is 1 to {max} characters"))),
        false => Ok(text.to_string()),
    }
}

fn optional(text: &str, max: usize, what: &str) -> Result<String, Refusal> {
    match text.trim() {
        "" => Ok(String::new()),
        text if text.chars().count() > max => {
            Err(Refusal::bad(format!("{what} is at most {max} characters")))
        }
        text => Ok(text.to_string()),
    }
}

fn user_id(text: &str) -> Result<Uuid, Refusal> {
    Uuid::parse_str(text.trim()).map_err(|_| Refusal::bad("`user` is not a user id"))
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    /// The signed-in caller's own id; every route but `discovery/notify` is scoped to it.
    pub fn caller(&self) -> Result<Uuid, Refusal> {
        let id = self.0.caller().and_then(|caller| caller.id.as_deref());
        id.and_then(|id| Uuid::parse_str(id).ok())
            .ok_or_else(|| Refusal::forbidden("sign in to see your notifications"))
    }

    pub async fn create(&self, asked: New) -> Result<Notification, Refusal> {
        let record = json!({
            "id": Uuid::now_v7(),
            "user": user_id(&asked.user)?,
            "title": required(&asked.title, MAX_TITLE, "a title")?,
            "body": optional(&asked.body, MAX_BODY, "the body")?,
            "url": optional(&asked.url, MAX_URL, "the link")?,
            "source": optional(&asked.source, MAX_SOURCE, "the source")?,
        });
        Ok(self.0.insert("notifications", record).await?)
    }

    /// The caller's own, newest first; `archived` picks the tab, `after` continues a page.
    pub async fn list(
        &self,
        archived: bool,
        limit: u32,
        after: Option<String>,
    ) -> Result<(Vec<Notification>, Option<String>), Refusal> {
        let user = self.caller()?;
        let filter = json!({ "user": user, "archived_at": { "is_null": !archived } });
        let query = Query::new("notifications")
            .filter(filter)
            .order(Order::desc("_created_at"))
            .limit(limit)
            .after(after);
        let page = self.0.query(query).await?;
        Ok((page.records, page.next))
    }

    /// How many of the caller's own are unread, for the bell's badge.
    pub async fn unread(&self) -> Result<usize, Refusal> {
        #[derive(Deserialize)]
        struct Id {
            #[allow(dead_code)]
            id: Uuid,
        }
        let user = self.caller()?;
        let filter = json!({ "user": user, "archived_at": { "is_null": true }, "read_at": { "is_null": true } });
        let found: Vec<Id> =
            self.0.query_all(Query::new("notifications").filter(filter).fields(&["id"])).await?;
        Ok(found.len())
    }

    /// The most recent still unread, for the dropdown: one read (here, on the inbox or by
    /// following it) has been dealt with, and the inbox keeps it.
    pub async fn recent(&self, limit: u32) -> Result<Vec<Notification>, Refusal> {
        let user = self.caller()?;
        let filter = json!({ "user": user, "archived_at": { "is_null": true }, "read_at": { "is_null": true } });
        let query = Query::new("notifications")
            .filter(filter)
            .order(Order::desc("_created_at"))
            .limit(limit);
        Ok(self.0.query(query).await?.records)
    }

    /// A 404, not a 403, when it belongs to someone else: whether it exists at all is their
    /// business, not the asking caller's.
    async fn owned(&self, id: &str) -> Result<Notification, Refusal> {
        let id =
            Uuid::parse_str(id).map_err(|_| Refusal::missing("there is no such notification"))?;
        let found: Option<Notification> = self.0.get("notifications", id.to_string()).await?;
        match found {
            Some(found) if found.user == self.caller()? => Ok(found),
            _ => Err(Refusal::missing("there is no such notification")),
        }
    }

    pub async fn mark_read(&self, id: &str) -> Result<Notification, Refusal> {
        let found = self.owned(id).await?;
        if found.read_at.is_some() {
            return Ok(found);
        }
        let updated: Option<Notification> = self
            .0
            .update("notifications", found.id.to_string(), json!({ "read_at": now() }), None)
            .await?;
        updated.ok_or_else(|| Refusal::missing("there is no such notification"))
    }

    pub async fn archive(&self, id: &str) -> Result<Notification, Refusal> {
        let found = self.owned(id).await?;
        if found.archived_at.is_some() {
            return Ok(found);
        }
        let updated: Option<Notification> = self
            .0
            .update("notifications", found.id.to_string(), json!({ "archived_at": now() }), None)
            .await?;
        updated.ok_or_else(|| Refusal::missing("there is no such notification"))
    }

    pub async fn remove(&self, id: &str) -> Result<(), Refusal> {
        let found = self.owned(id).await?;
        self.0.delete("notifications", found.id.to_string(), None).await?;
        Ok(())
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        match err.problem() {
            Some((status, _)) if status < 500 => Self { status, detail: err.detail() },
            _ => {
                tracing::warn!(%err, "a call to the backend failed");
                Self::unavailable(format!("notifications storage failed: {err}"))
            }
        }
    }
}
