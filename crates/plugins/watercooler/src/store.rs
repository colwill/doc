//! What the plugin keeps: threads, with every resource they are tagged with and the words the
//! search covers, each message as written and as rendered, events, cards and kudos.

use std::collections::BTreeMap;

use doc_plugin_sdk::{
    Aggregate, Backend, Collection, DataRequest, Declaration, Field, ListOf, Measure, OnDelete,
    Order, PluginError, Query,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::Refusal;

const ATTEMPTS: usize = 5;
/// The words a search headline shows around a match, and how many stretches of them at most.
const HEADLINE_WORDS: usize = 18;
const HEADLINE_FRAGMENTS: usize = 2;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "threads",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("title", Field::text().required())
                .field("author", Field::text().required())
                .field("tags", Field::list(ListOf::Text).required().default(json!([])))
                .field("replies", Field::integer().required().default(json!(0)))
                .field(
                    "words",
                    Field::text()
                        .required()
                        .default(json!(""))
                        .describe("Every message's words, for the search"),
                )
                .field("last_at", Field::timestamp().required().default(json!("now")))
                .index(&["last_at"])
                .search(&["title", "words"]),
        )
        .collection(
            "messages",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "thread",
                    Field::reference("threads").required().on_delete(OnDelete::Cascade),
                )
                .field("author", Field::text().required())
                .field("author_ref", Field::text().required())
                .field("body", Field::text().required())
                .field("html", Field::text().required())
                .field("text", Field::text().required())
                .field("tags", Field::list(ListOf::Text).required().default(json!([])))
                .field("edited_at", Field::timestamp().describe("When its author last changed it"))
                .field(
                    "parent",
                    Field::uuid().describe(
                        "The message it answers; none for the opening post and replies to it",
                    ),
                )
                .index(&["thread"]),
        )
        .collection(
            "likes",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "message",
                    Field::reference("messages").required().on_delete(OnDelete::Cascade),
                )
                .field("thread", Field::uuid().required())
                .field("by", Field::text().required().describe("user:<id> or service:<id>"))
                .field("name", Field::text().required())
                .unique(&["message", "by"])
                .index(&["thread"]),
        )
        .collection(
            "events",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "kind",
                    Field::text().required().one_of(&["hackathon", "game-night", "challenge"]),
                )
                .field("title", Field::text().required())
                .field("description", Field::text().required().default(json!("")))
                .field("html", Field::text().required().default(json!("")))
                .field("team", Field::text().required())
                .field("organiser", Field::text().required())
                .field("starts_local", Field::text().required())
                .field("ends_local", Field::text().required())
                .field("timezone", Field::text().required())
                .field("location", Field::text().required().default(json!("")))
                .field("thread", Field::reference("threads").on_delete(OnDelete::Null))
                .field("calendar_event", Field::uuid())
                .field("higher_wins", Field::boolean().required().default(json!(true)))
                .field("unit", Field::text().required().default(json!("")))
                .field(
                    "status",
                    Field::text().required().default(json!("open")).one_of(&[
                        "open",
                        "closed",
                        "announced",
                    ]),
                )
                .field("winners", Field::json().required().default(json!([])))
                .index(&["starts_local", "title"]),
        )
        .collection(
            "registrations",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("event", Field::reference("events").required().on_delete(OnDelete::Cascade))
                .field("team", Field::text().required())
                .field("registered_by", Field::text().required())
                .unique(&["event", "team"]),
        )
        .collection(
            "submissions",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "registration",
                    Field::reference("registrations").required().on_delete(OnDelete::Cascade),
                )
                .field("event", Field::uuid().required())
                .field("team", Field::text().required())
                .field("title", Field::text().required())
                .field("description", Field::text().required().default(json!("")))
                .field("html", Field::text().required().default(json!("")))
                .field("links", Field::list(ListOf::Text).required().default(json!([])))
                .field("submitted_by", Field::text().required())
                .unique(&["event", "team"]),
        )
        .collection(
            "entries",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("event", Field::reference("events").required().on_delete(OnDelete::Cascade))
                .field("entrant", Field::text().required())
                .field("score", Field::number().required())
                .field("note", Field::text().required().default(json!("")))
                .index(&["event", "entrant"]),
        )
        .collection(
            "cards",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("kind", Field::text().required())
                .field("title", Field::text().required())
                .field("recipient", Field::text().required())
                .field("team", Field::text())
                .field("creator", Field::text().required())
                .field("reveal_at", Field::timestamp().required())
                .field("timezone", Field::text().required())
                .field("delivered_at", Field::timestamp())
                .index(&["reveal_at"]),
        )
        .collection(
            "signatures",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("card", Field::reference("cards").required().on_delete(OnDelete::Cascade))
                .field("signer", Field::text().required())
                .field("message", Field::text().required())
                .field("html", Field::text().required())
                .field("signed_at", Field::timestamp().required().default(json!("now")))
                .unique(&["card", "signer"]),
        )
        .collection(
            "kudos",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("giver", Field::text().required())
                .field("to_kind", Field::text().required().one_of(&["user", "team"]))
                .field("to_name", Field::text().required())
                .field("team", Field::text())
                .field("message", Field::text().required())
                .field("html", Field::text().required())
                .index(&["to_kind", "to_name"]),
        )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: Uuid,
    pub title: String,
    pub author: String,
    /// Every resource the thread's messages tag, as `kind:name`.
    pub tags: Vec<String>,
    pub replies: i64,
    pub last_at: String,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    /// Where a search matched, with each match between `[[` and `]]`.
    #[serde(default)]
    pub headline: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: Uuid,
    pub thread: Uuid,
    pub author: String,
    #[serde(default)]
    pub author_ref: String,
    pub body: String,
    pub html: String,
    #[serde(default)]
    pub text: String,
    pub tags: Vec<String>,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    #[serde(default)]
    pub edited_at: Option<String>,
    /// The message it answers; none for the opening post and replies to it.
    #[serde(default)]
    pub parent: Option<Uuid>,
}

/// Somebody liking a message, once each.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Like {
    pub message: Uuid,
    pub by: String,
    pub name: String,
}

impl Message {
    /// Whether `who`, as `user:<id>` or `service:<id>`, wrote it.
    pub fn by(&self, who: &str) -> bool {
        !self.author_ref.is_empty() && self.author_ref == who
    }
}

fn inserted(message: &Message) -> DataRequest {
    DataRequest::insert(
        "messages",
        json!({
            "id": message.id, "thread": message.thread, "author": message.author,
            "author_ref": message.author_ref, "body": message.body, "html": message.html,
            "text": message.text, "tags": message.tags, "parent": message.parent,
        }),
    )
}

/// A hackathon, game night or challenge, organised by a team.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub kind: String,
    pub title: String,
    pub description: String,
    pub html: String,
    pub team: String,
    pub organiser: String,
    /// Wall-clock times in `timezone`.
    pub starts_local: String,
    pub ends_local: String,
    pub timezone: String,
    pub location: String,
    pub thread: Option<Uuid>,
    pub calendar_event: Option<Uuid>,
    /// For a challenge: whether the highest score leads, and what scores count.
    pub higher_wins: bool,
    pub unit: String,
    pub status: String,
    /// For a hackathon: `{"team", "place", "note"}` for each winner.
    pub winners: Vec<Value>,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    #[serde(alias = "_updated_at", default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Registration {
    pub team: String,
    pub registered_by: String,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Submission {
    pub event: Uuid,
    pub team: String,
    pub title: String,
    pub description: String,
    pub html: String,
    pub links: Vec<String>,
    pub submitted_by: String,
    #[serde(alias = "_updated_at", default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: Uuid,
    pub event: Uuid,
    pub entrant: String,
    pub score: f64,
    pub note: String,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
}

/// A card for one person, signed by others and hidden from them until it is revealed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Card {
    pub id: Uuid,
    pub kind: String,
    pub title: String,
    pub recipient: String,
    /// The team told when it is delivered.
    pub team: Option<String>,
    pub creator: String,
    pub reveal_at: String,
    pub timezone: String,
    pub delivered_at: Option<String>,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    #[serde(default)]
    pub signatures: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Signature {
    pub signer: String,
    pub message: String,
    pub html: String,
    pub signed_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Kudos {
    pub id: Uuid,
    pub giver: String,
    /// `user` or `team`.
    pub to_kind: String,
    pub to_name: String,
    /// The team told about it.
    pub team: Option<String>,
    pub message: String,
    pub html: String,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

fn conflict(err: &PluginError) -> bool {
    err.is_version_conflict()
}

fn taken(err: &PluginError) -> bool {
    err.is_duplicate()
}

fn folded(word: &str) -> String {
    word.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// A few words of `text` around what `asked` matched, each match between `[[` and `]]`.
fn headline(text: &str, asked: &str) -> String {
    let terms: Vec<String> = asked
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty() && term != "or")
        .collect();
    let words: Vec<&str> = text.split_whitespace().collect();
    let hit = |word: &str| {
        let word = folded(word);
        !word.is_empty() && terms.iter().any(|term| word.starts_with(term.as_str()))
    };
    let mut fragments: Vec<(usize, usize)> = Vec::new();
    for (at, word) in words.iter().enumerate() {
        if fragments.len() == HEADLINE_FRAGMENTS {
            break;
        }
        if hit(word) && fragments.last().is_none_or(|(_, end)| at >= *end) {
            let start = at.saturating_sub(HEADLINE_WORDS / 3);
            fragments.push((start, (start + HEADLINE_WORDS).min(words.len())));
        }
    }
    if fragments.is_empty() {
        fragments.push((0, HEADLINE_WORDS.min(words.len())));
    }
    fragments
        .iter()
        .map(|(start, end)| {
            words[*start..*end]
                .iter()
                .map(|word| if hit(word) { format!("[[{word}]]") } else { (*word).to_string() })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" … ")
}

/// Every tag the messages carry, first seen first, and all their words, for the thread.
fn gathered(messages: &[Message]) -> (Vec<String>, String) {
    let mut tags: Vec<String> = Vec::new();
    for tag in messages.iter().flat_map(|message| &message.tags) {
        if !tags.contains(tag) {
            tags.push(tag.clone());
        }
    }
    let words: Vec<&str> = messages.iter().map(|message| message.text.as_str()).collect();
    (tags, words.join(" "))
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    async fn threads(&self, query: Query) -> Result<Vec<Thread>, Refusal> {
        Ok(self.0.query(query).await?.records)
    }

    pub async fn start(&self, thread: &Thread, first: &Message) -> Result<(), Refusal> {
        let values = json!({
            "id": thread.id, "title": thread.title, "author": thread.author, "tags": thread.tags,
            "words": first.text,
        });
        self.0.batch(vec![DataRequest::insert("threads", values), inserted(first)]).await?;
        Ok(())
    }

    /// Adds a reply, adding the tags it brings to the thread's.
    pub async fn reply(&self, reply: &Message) -> Result<(), Refusal> {
        for _ in 0..ATTEMPTS {
            let Some(thread) =
                self.0.get::<Map<String, Value>>("threads", reply.thread.to_string()).await?
            else {
                return Err(Refusal::missing("there is no such thread"));
            };
            let mut tags: Vec<String> = thread
                .get("tags")
                .and_then(Value::as_array)
                .map(|tags| {
                    tags.iter().filter_map(|tag| tag.as_str().map(str::to_string)).collect()
                })
                .unwrap_or_default();
            for tag in &reply.tags {
                if !tags.contains(tag) {
                    tags.push(tag.clone());
                }
            }
            let replies = thread.get("replies").and_then(Value::as_i64).unwrap_or(0);
            let words = thread.get("words").and_then(Value::as_str).unwrap_or_default();
            let set = json!({
                "replies": replies + 1, "last_at": now(), "tags": tags,
                "words": format!("{words} {}", reply.text),
            });
            let version = thread.get("_version").and_then(Value::as_i64).unwrap_or_default();
            let update =
                DataRequest::update("threads", reply.thread.to_string(), set).at_version(version);
            match self.0.batch(vec![inserted(reply), update]).await {
                Ok(_) => return Ok(()),
                Err(err) if conflict(&err) => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Err(Refusal::unavailable("the thread kept changing; try again"))
    }

    /// Changes one message, and the thread with it: its title when that is given, and the tags
    /// and words it gathers from all its messages, so what an edit takes out is gone from the
    /// thread too unless another message still has it.
    pub async fn edit(&self, edited: &Message, title: Option<&str>) -> Result<(), Refusal> {
        for _ in 0..ATTEMPTS {
            let Some(thread) =
                self.0.get::<Map<String, Value>>("threads", edited.thread.to_string()).await?
            else {
                return Err(Refusal::missing("there is no such thread"));
            };
            let messages: Vec<Message> = self
                .messages(edited.thread)
                .await?
                .into_iter()
                .map(|held| if held.id == edited.id { edited.clone() } else { held })
                .collect();
            let (tags, words) = gathered(&messages);
            let mut set = json!({ "tags": tags, "words": words });
            if let Some(title) = title {
                set["title"] = json!(title);
            }
            let version = thread.get("_version").and_then(Value::as_i64).unwrap_or_default();
            let update =
                DataRequest::update("threads", edited.thread.to_string(), set).at_version(version);
            let changed = DataRequest::update(
                "messages",
                edited.id.to_string(),
                json!({
                    "body": edited.body, "html": edited.html, "text": edited.text,
                    "tags": edited.tags, "edited_at": now(),
                }),
            );
            match self.0.batch(vec![changed, update]).await {
                Ok(_) => return Ok(()),
                Err(err) if conflict(&err) => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Err(Refusal::unavailable("the thread kept changing; try again"))
    }

    /// Takes a reply out altogether. What answered it moves up to answer what it answered, and
    /// the thread loses the reply from its count, and its tags and words unless another message
    /// has them.
    pub async fn remove(&self, removed: &Message) -> Result<(), Refusal> {
        for _ in 0..ATTEMPTS {
            let Some(thread) =
                self.0.get::<Map<String, Value>>("threads", removed.thread.to_string()).await?
            else {
                return Err(Refusal::missing("there is no such thread"));
            };
            let messages = self.messages(removed.thread).await?;
            let left: Vec<Message> =
                messages.iter().filter(|message| message.id != removed.id).cloned().collect();
            let mut writes: Vec<DataRequest> = messages
                .iter()
                .filter(|message| message.parent == Some(removed.id))
                .map(|child| {
                    DataRequest::update(
                        "messages",
                        child.id.to_string(),
                        json!({ "parent": removed.parent }),
                    )
                })
                .collect();
            writes.push(DataRequest::delete("messages", removed.id.to_string()));
            let (tags, words) = gathered(&left);
            let replies = thread.get("replies").and_then(Value::as_i64).unwrap_or(1);
            let version = thread.get("_version").and_then(Value::as_i64).unwrap_or_default();
            let set = json!({ "tags": tags, "words": words, "replies": (replies - 1).max(0) });
            writes.push(
                DataRequest::update("threads", removed.thread.to_string(), set).at_version(version),
            );
            match self.0.batch(writes).await {
                Ok(_) => return Ok(()),
                Err(err) if conflict(&err) => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Err(Refusal::unavailable("the thread kept changing; try again"))
    }

    /// Takes a thread out, with every message and like in it.
    pub async fn remove_thread(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("threads", id.to_string(), None).await?;
        Ok(())
    }

    pub async fn likes(&self, thread: Uuid) -> Result<Vec<Like>, Refusal> {
        Ok(self.0.query_all(Query::new("likes").filter(json!({ "thread": thread }))).await?)
    }

    /// Likes a message as `by`; false when they already did.
    pub async fn like(&self, message: &Message, by: &str, name: &str) -> Result<bool, Refusal> {
        let values = json!({
            "id": Uuid::now_v7(), "message": message.id, "thread": message.thread,
            "by": by, "name": name,
        });
        match self.0.insert::<Value>("likes", values).await {
            Ok(_) => Ok(true),
            Err(err) if taken(&err) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// Takes `by`'s like of a message back; false when there was none.
    pub async fn unlike(&self, message: Uuid, by: &str) -> Result<bool, Refusal> {
        let deleted =
            self.0.delete_where("likes", "id", json!({ "message": message, "by": by })).await?;
        Ok(deleted > 0)
    }

    pub async fn message(&self, id: Uuid) -> Result<Option<Message>, Refusal> {
        Ok(self.0.get("messages", id.to_string()).await?)
    }

    pub async fn thread(&self, id: Uuid) -> Result<Option<Thread>, Refusal> {
        Ok(self.0.get("threads", id.to_string()).await?)
    }

    pub async fn messages(&self, thread: Uuid) -> Result<Vec<Message>, Refusal> {
        let query = Query::new("messages")
            .filter(json!({ "thread": thread }))
            .order(Order::asc("_created_at"));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn recent(&self, limit: i64) -> Result<Vec<Thread>, Refusal> {
        self.threads(
            Query::new("threads").order(Order::desc("last_at")).limit(limit.clamp(1, 1_000) as u32),
        )
        .await
    }

    /// Threads tagged with any of these resources, liveliest first.
    pub async fn tagged(&self, tags: &[String], limit: i64) -> Result<Vec<Thread>, Refusal> {
        if tags.is_empty() {
            return Ok(Vec::new());
        }
        let any: Vec<Value> =
            tags.iter().map(|tag| json!({ "tags": { "contains": tag } })).collect();
        let query = Query::new("threads")
            .filter(json!({ "any": any }))
            .order(Order::desc("last_at"))
            .limit(limit.clamp(1, 1_000) as u32);
        self.threads(query).await
    }

    /// Threads whose words match `text`, best first, only those tagged `tag` if one is given.
    pub async fn search(
        &self,
        text: &str,
        tag: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Thread>, Refusal> {
        let mut query = Query::new("threads").search(text).limit(limit.clamp(1, 1_000) as u32);
        if let Some(tag) = tag.filter(|tag| !tag.is_empty()) {
            query = query.filter(json!({ "tags": { "contains": tag } }));
        }
        let found: Vec<Map<String, Value>> = self.0.query(query).await?.records;
        found
            .into_iter()
            .map(|mut row| {
                let title = row.get("title").and_then(Value::as_str).unwrap_or_default();
                let words = row.get("words").and_then(Value::as_str).unwrap_or_default();
                let marked = headline(&format!("{title} {words}"), text);
                row.insert("headline".into(), json!(marked));
                serde_json::from_value(Value::Object(row)).map_err(|err| {
                    Refusal::unavailable(format!("a stored thread could not be read: {err}"))
                })
            })
            .collect()
    }

    pub async fn save_event(&self, event: &Event) -> Result<(), Refusal> {
        let set = json!({
            "title": event.title, "description": event.description, "html": event.html,
            "thread": event.thread, "calendar_event": event.calendar_event,
            "status": event.status, "winners": event.winners,
        });
        let updated: Option<Value> =
            self.0.update("events", event.id.to_string(), set.clone(), None).await?;
        if updated.is_none() {
            let mut values = set;
            for (field, value) in [
                ("id", json!(event.id)),
                ("kind", json!(event.kind)),
                ("team", json!(event.team)),
                ("organiser", json!(event.organiser)),
                ("starts_local", json!(event.starts_local)),
                ("ends_local", json!(event.ends_local)),
                ("timezone", json!(event.timezone)),
                ("location", json!(event.location)),
                ("higher_wins", json!(event.higher_wins)),
                ("unit", json!(event.unit)),
            ] {
                values[field] = value;
            }
            let _: Value = self.0.insert("events", values).await?;
        }
        Ok(())
    }

    pub async fn event(&self, id: Uuid) -> Result<Option<Event>, Refusal> {
        Ok(self.0.get("events", id.to_string()).await?)
    }

    /// Every event, soonest first.
    pub async fn events(&self) -> Result<Vec<Event>, Refusal> {
        let query =
            Query::new("events").order(Order::asc("starts_local")).order(Order::asc("title"));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn delete_event(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("events", id.to_string(), None).await?;
        Ok(())
    }

    /// Registers a team, answering whether it was not registered already.
    pub async fn register(&self, event: Uuid, team: &str, by: &str) -> Result<bool, Refusal> {
        let values = json!({ "event": event, "team": team, "registered_by": by });
        match self.0.insert::<Value>("registrations", values).await {
            Ok(_) => Ok(true),
            Err(err) if taken(&err) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    pub async fn registrations(&self, event: Uuid) -> Result<Vec<Registration>, Refusal> {
        let query = Query::new("registrations")
            .filter(json!({ "event": event }))
            .order(Order::asc("_created_at"));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn submit(&self, submission: &Submission) -> Result<(), Refusal> {
        let registered = Query::new("registrations")
            .filter(json!({ "event": submission.event, "team": submission.team }))
            .fields(&["id"])
            .limit(1);
        let found = self.0.query::<Map<String, Value>>(registered).await?.records;
        let registration =
            found.first().and_then(|row| row.get("id").cloned()).ok_or_else(|| {
                Refusal::bad(format!("{} is not registered for this event", submission.team))
            })?;
        let values = json!({
            "registration": registration, "event": submission.event, "team": submission.team,
            "title": submission.title, "description": submission.description, "html": submission.html,
            "links": submission.links, "submitted_by": submission.submitted_by,
        });
        let _: (Value, bool) = self.0.upsert("submissions", &["event", "team"], values).await?;
        Ok(())
    }

    pub async fn submissions(&self, event: Uuid) -> Result<Vec<Submission>, Refusal> {
        let query = Query::new("submissions")
            .filter(json!({ "event": event }))
            .order(Order::asc("event"))
            .order(Order::asc("team"));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn enter(&self, entry: &Entry) -> Result<(), Refusal> {
        let values = json!({
            "id": entry.id, "event": entry.event, "entrant": entry.entrant, "score": entry.score,
            "note": entry.note,
        });
        let _: Value = self.0.insert("entries", values).await?;
        Ok(())
    }

    pub async fn entries(&self, event: Uuid) -> Result<Vec<Entry>, Refusal> {
        let query = Query::new("entries")
            .filter(json!({ "event": event }))
            .order(Order::asc("_created_at"));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn make_card(&self, card: &Card) -> Result<(), Refusal> {
        let values = json!({
            "id": card.id, "kind": card.kind, "title": card.title, "recipient": card.recipient,
            "team": card.team, "creator": card.creator, "reveal_at": card.reveal_at,
            "timezone": card.timezone,
        });
        let _: Value = self.0.insert("cards", values).await?;
        Ok(())
    }

    /// Cards from their records, each with how many have signed it.
    async fn counted(&self, cards: Vec<Map<String, Value>>) -> Result<Vec<Card>, Refusal> {
        let ids: Vec<Value> = cards.iter().filter_map(|card| card.get("id").cloned()).collect();
        let mut signed: BTreeMap<String, i64> = BTreeMap::new();
        for chunk in ids.chunks(500) {
            let counted = Aggregate::new("signatures")
                .filter(json!({ "card": { "in": chunk } }))
                .group_by("card")
                .measure("signatures", Measure::Count("*".into()));
            for group in self.0.aggregate(counted).await? {
                let card =
                    group.get("card").and_then(Value::as_str).unwrap_or_default().to_string();
                signed.insert(card, group.get("signatures").and_then(Value::as_i64).unwrap_or(0));
            }
        }
        cards
            .into_iter()
            .map(|mut card| {
                let id = card.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                card.insert("signatures".into(), json!(signed.get(&id).copied().unwrap_or(0)));
                serde_json::from_value(Value::Object(card)).map_err(|err| {
                    Refusal::unavailable(format!("a stored card could not be read: {err}"))
                })
            })
            .collect()
    }

    pub async fn card(&self, id: Uuid) -> Result<Option<Card>, Refusal> {
        let Some(card) = self.0.get::<Map<String, Value>>("cards", id.to_string()).await? else {
            return Ok(None);
        };
        Ok(self.counted(vec![card]).await?.into_iter().next())
    }

    /// Every card, soonest to be revealed first.
    pub async fn cards(&self) -> Result<Vec<Card>, Refusal> {
        let cards = self.0.query_all(Query::new("cards").order(Order::asc("reveal_at"))).await?;
        self.counted(cards).await
    }

    pub async fn delete_card(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete("cards", id.to_string(), None).await?;
        Ok(())
    }

    /// Signs a card, or changes what the signer wrote, if it is not revealed yet.
    pub async fn sign(
        &self,
        card: Uuid,
        signer: &str,
        message: &str,
        html: &str,
    ) -> Result<bool, Refusal> {
        let Some(found) = self.0.get::<Map<String, Value>>("cards", card.to_string()).await? else {
            return Ok(false);
        };
        let reveal = found
            .get("reveal_at")
            .and_then(Value::as_str)
            .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok());
        if reveal.is_none_or(|reveal| reveal <= chrono::Utc::now()) {
            return Ok(false);
        }
        let values = json!({ "card": card, "signer": signer, "message": message, "html": html, "signed_at": now() });
        let _: (Value, bool) = self.0.upsert("signatures", &["card", "signer"], values).await?;
        Ok(true)
    }

    pub async fn signatures(&self, card: Uuid) -> Result<Vec<Signature>, Refusal> {
        let query = Query::new("signatures").filter(json!({ "card": card }));
        let mut signed: Vec<Signature> = self.0.query_all(query).await?;
        signed.sort_by(|a, b| a.signed_at.cmp(&b.signed_at));
        Ok(signed)
    }

    /// Cards whose reveal time has come and whose delivery has not been announced.
    pub async fn due_cards(&self) -> Result<Vec<Card>, Refusal> {
        let query = Query::new("cards")
            .filter(json!({ "delivered_at": null, "reveal_at": { "lte": now() } }))
            .order(Order::asc("reveal_at"));
        let cards = self.0.query_all(query).await?;
        self.counted(cards).await
    }

    pub async fn delivered(&self, id: Uuid) -> Result<bool, Refusal> {
        let changed = self
            .0
            .change("cards", id.to_string(), |card| {
                card.get("delivered_at")
                    .is_none_or(Value::is_null)
                    .then(|| json!({ "delivered_at": now() }))
            })
            .await?;
        Ok(changed.is_some())
    }

    pub async fn give(&self, kudos: &Kudos) -> Result<(), Refusal> {
        let values = json!({
            "id": kudos.id, "giver": kudos.giver, "to_kind": kudos.to_kind, "to_name": kudos.to_name,
            "team": kudos.team, "message": kudos.message, "html": kudos.html,
        });
        let _: Value = self.0.insert("kudos", values).await?;
        Ok(())
    }

    /// The latest kudos, or those for one person or team.
    pub async fn kudos(&self, to: Option<(&str, &str)>, limit: i64) -> Result<Vec<Kudos>, Refusal> {
        let mut query = Query::new("kudos")
            .order(Order::desc("_created_at"))
            .limit(limit.clamp(1, 1_000) as u32);
        if let Some((kind, name)) = to {
            query = query.filter(json!({ "to_kind": kind, "to_name": name }));
        }
        Ok(self.0.query(query).await?.records)
    }
}
