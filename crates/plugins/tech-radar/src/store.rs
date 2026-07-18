//! What the radar keeps: its entries, each in one quadrant and one ring, and every ring each entry
//! has been in, which is its timeline. Only writers change them; the backend has checked that.

use chrono::Utc;
use doc_plugin_sdk::{
    Backend, Collection, DataRequest, Declaration, Field, OnDelete, PluginError, Query,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::model::{self, Moved};

const ATTEMPTS: usize = 5;
pub const MAX_TITLE: usize = 100;
pub const MAX_DESCRIPTION: usize = 10_000;
pub const MAX_NOTE: usize = 1_000;
pub const MAX_KEY: usize = 64;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "entries",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "key",
                    Field::text()
                        .required()
                        .max(MAX_KEY as f64)
                        .describe("A short name for links and imports, such as rust"),
                )
                .field("title", Field::text().required().max(MAX_TITLE as f64))
                .field("quadrant", Field::text().required().one_of(&model::quadrant_ids()))
                .field("ring", Field::text().required().one_of(&model::ring_ids()))
                .field(
                    "description",
                    Field::text()
                        .required()
                        .default(json!(""))
                        .max(MAX_DESCRIPTION as f64)
                        .describe("Markdown: why it is in its ring"),
                )
                .field("url", Field::text().required().default(json!("")))
                .field("moved", Field::text().required().default(json!("new")).one_of(&Moved::ALL))
                .field("moved_at", Field::timestamp().required().default(json!("now")))
                .unique(&["key"])
                .search(&["title", "description"]),
        )
        .collection(
            "movements",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("entry", Field::reference("entries").required().on_delete(OnDelete::Cascade))
                .field("ring", Field::text().required().one_of(&model::ring_ids()))
                .field("from", Field::text().describe("The ring it left; none when it was added"))
                .field("moved", Field::text().required().one_of(&Moved::ALL))
                .field("note", Field::text().required().default(json!("")))
                .field("author", Field::text().required())
                .field("at", Field::timestamp().required().default(json!("now")))
                .index(&["entry"]),
        )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: Uuid,
    pub key: String,
    pub title: String,
    pub quadrant: String,
    pub ring: String,
    pub description: String,
    pub url: String,
    pub moved: Moved,
    pub moved_at: String,
    #[serde(rename = "_version")]
    pub version: i64,
    #[serde(rename = "_updated_at")]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Movement {
    pub id: Uuid,
    pub entry: Uuid,
    pub ring: String,
    #[serde(default)]
    pub from: Option<String>,
    pub moved: Moved,
    pub note: String,
    pub author: String,
    pub at: String,
}

/// An entry as someone asked for it, before it is checked.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewEntry {
    pub title: String,
    pub quadrant: String,
    pub ring: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub url: String,
    /// Why it starts in its ring, for the timeline.
    #[serde(default)]
    pub note: String,
}

/// What an edit changes; the ring changes only by a move, so the timeline records it.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub title: Option<String>,
    pub quadrant: Option<String>,
    pub description: Option<String>,
    pub url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Move {
    pub ring: String,
    #[serde(default)]
    pub note: String,
}

/// `Languages & Frameworks!` as a key: `languages-frameworks`.
pub fn slug(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        match c {
            c if c.is_ascii_alphanumeric() => slug.push(c),
            '+' => slug.push_str("plus"),
            '#' => slug.push_str("sharp"),
            _ if !slug.is_empty() && !slug.ends_with('-') => slug.push('-'),
            _ => {}
        }
    }
    slug.trim_end_matches('-')
        .chars()
        .take(MAX_KEY)
        .collect::<String>()
        .trim_end_matches('-')
        .into()
}

pub fn check_title(title: &str) -> Result<String, Refusal> {
    let title = title.trim();
    match title.is_empty() || title.chars().count() > MAX_TITLE {
        true => Err(Refusal::bad(format!("a title is 1 to {MAX_TITLE} characters"))),
        false => Ok(title.to_string()),
    }
}

pub fn check_quadrant(quadrant: &str) -> Result<String, Refusal> {
    match model::quadrant_index(quadrant) {
        Some(_) => Ok(quadrant.to_string()),
        None => Err(Refusal::bad(format!(
            "the quadrant is one of {}",
            model::quadrant_ids().join(", ")
        ))),
    }
}

pub fn check_ring(ring: &str) -> Result<String, Refusal> {
    match model::ring_index(ring) {
        Some(_) => Ok(ring.to_string()),
        None => Err(Refusal::bad(format!("the ring is one of {}", model::ring_ids().join(", ")))),
    }
}

pub fn check_description(description: &str) -> Result<String, Refusal> {
    match description.chars().count() > MAX_DESCRIPTION {
        true => Err(Refusal::bad(format!("a description is at most {MAX_DESCRIPTION} characters"))),
        false => Ok(description.trim().to_string()),
    }
}

pub fn check_note(note: &str) -> Result<String, Refusal> {
    match note.chars().count() > MAX_NOTE {
        true => Err(Refusal::bad(format!("a note is at most {MAX_NOTE} characters"))),
        false => Ok(note.trim().to_string()),
    }
}

/// Empty, or an http or https address.
pub fn check_url(address: &str) -> Result<String, Refusal> {
    let address = address.trim();
    if address.is_empty() {
        return Ok(String::new());
    }
    match url::Url::parse(address) {
        Ok(parsed) if matches!(parsed.scheme(), "http" | "https") => Ok(parsed.to_string()),
        _ => Err(Refusal::bad("a link is an http or https address")),
    }
}

pub fn check_key(key: &str) -> Result<String, Refusal> {
    let fine = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    match !key.is_empty() && key.len() <= MAX_KEY && key.chars().all(fine) {
        true => Ok(key.to_string()),
        false => Err(Refusal::bad(format!(
            "a key is 1 to {MAX_KEY} lowercase letters, digits and -, such as rust"
        ))),
    }
}

/// Who is writing, as the timeline names them.
pub fn author(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "someone".into())
}

pub fn now() -> String {
    Utc::now().to_rfc3339()
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    pub async fn entries(&self) -> Result<Vec<Entry>, Refusal> {
        Ok(self.0.query_all(Query::new("entries")).await?)
    }

    pub async fn search(&self, text: &str) -> Result<Vec<Entry>, Refusal> {
        Ok(self.0.query(Query::new("entries").search(text).limit(100)).await?.records)
    }

    /// An entry by its ID or its key.
    pub async fn find(&self, id: &str) -> Result<Option<Entry>, Refusal> {
        if let Ok(id) = Uuid::parse_str(id) {
            return Ok(self.0.get("entries", id.to_string()).await?);
        }
        let query = Query::new("entries").filter(json!({ "key": id })).limit(1);
        Ok(self.0.query(query).await?.records.into_iter().next())
    }

    pub async fn found(&self, id: &str) -> Result<Entry, Refusal> {
        self.find(id).await?.ok_or_else(|| Refusal::missing("there is no such entry"))
    }

    /// The entry's timeline, newest first.
    pub async fn movements(&self, entry: Uuid) -> Result<Vec<Movement>, Refusal> {
        let query = Query::new("movements").filter(json!({ "entry": entry }));
        let mut movements: Vec<Movement> = self.0.query_all(query).await?;
        movements.sort_by(|a, b| b.at.cmp(&a.at));
        Ok(movements)
    }

    /// Adds an entry and the first line of its timeline, together.
    pub async fn create(&self, asked: NewEntry) -> Result<Entry, Refusal> {
        let title = check_title(&asked.title)?;
        let key = match asked.key.as_deref().map(str::trim).filter(|key| !key.is_empty()) {
            Some(key) => check_key(key)?,
            None => check_key(&slug(&title))
                .map_err(|_| Refusal::bad("give the entry a key: its title makes none"))?,
        };
        let (quadrant, ring) = (check_quadrant(&asked.quadrant)?, check_ring(&asked.ring)?);
        let entry = json!({
            "id": Uuid::now_v7(),
            "key": key,
            "title": title,
            "quadrant": quadrant,
            "ring": ring,
            "description": check_description(&asked.description)?,
            "url": check_url(&asked.url)?,
            "moved": Moved::New.id(),
            "moved_at": now(),
        });
        let first = self.movement(&entry["id"], &ring, None, &check_note(&asked.note)?, None);
        let answers =
            self.0.batch(vec![DataRequest::insert("entries", entry), first]).await.map_err(
                |err| match err.is_duplicate() {
                    true => Refusal::conflict(format!("the key {key} is taken by another entry")),
                    false => err.into(),
                },
            )?;
        let record = answers.into_iter().next().and_then(|answer| answer.record);
        serde_json::from_value(Value::Object(record.unwrap_or_default()))
            .map_err(|err| Refusal::unavailable(format!("the entry came back unreadable: {err}")))
    }

    pub async fn edit(&self, id: &str, edit: Edit) -> Result<Entry, Refusal> {
        let entry = self.found(id).await?;
        let mut set = serde_json::Map::new();
        if let Some(title) = &edit.title {
            set.insert("title".into(), json!(check_title(title)?));
        }
        if let Some(quadrant) = &edit.quadrant {
            set.insert("quadrant".into(), json!(check_quadrant(quadrant)?));
        }
        if let Some(description) = &edit.description {
            set.insert("description".into(), json!(check_description(description)?));
        }
        if let Some(address) = &edit.url {
            set.insert("url".into(), json!(check_url(address)?));
        }
        if set.is_empty() {
            return Ok(entry);
        }
        let changed =
            self.0.change("entries", entry.id.to_string(), |_| Some(Value::Object(set.clone())));
        let record = changed.await?.ok_or_else(|| Refusal::missing("there is no such entry"))?;
        serde_json::from_value(Value::Object(record))
            .map_err(|err| Refusal::unavailable(format!("the entry came back unreadable: {err}")))
    }

    /// Puts an entry in another ring and records it on its timeline, together. Moving to the ring
    /// it is in already only records the note.
    pub async fn move_to(&self, id: &str, asked: Move) -> Result<(Entry, Moved), Refusal> {
        let ring = check_ring(&asked.ring)?;
        let note = check_note(&asked.note)?;
        for _ in 0..ATTEMPTS {
            let entry = self.found(id).await?;
            if entry.ring == ring && note.is_empty() {
                return Err(Refusal::bad(format!(
                    "{} is in {} already; say why it stays there to note it",
                    entry.title,
                    model::ring_name(&ring)
                )));
            }
            let moved = Moved::between(Some(&entry.ring), &ring);
            let mut set = json!({ "ring": ring });
            if moved != Moved::None {
                set["moved"] = json!(moved.id());
                set["moved_at"] = json!(now());
            }
            let update =
                DataRequest::update("entries", entry.id.to_string(), set).at_version(entry.version);
            let line =
                self.movement(&json!(entry.id), &ring, Some(&entry.ring), &note, Some(moved));
            match self.0.batch(vec![update, line]).await {
                Ok(answers) => {
                    let record = answers.into_iter().next().and_then(|answer| answer.record);
                    let entry = serde_json::from_value(Value::Object(record.unwrap_or_default()))
                        .map_err(|err| {
                        Refusal::unavailable(format!("the entry came back unreadable: {err}"))
                    })?;
                    return Ok((entry, moved));
                }
                Err(err) if err.is_version_conflict() => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Err(Refusal::conflict("the entry kept changing; try again"))
    }

    pub async fn remove(&self, id: &str) -> Result<Entry, Refusal> {
        let entry = self.found(id).await?;
        match self.0.delete("entries", entry.id.to_string(), None).await? {
            true => Ok(entry),
            false => Err(Refusal::missing("there is no such entry")),
        }
    }

    /// A line of a timeline, to write alongside the entry's own change.
    pub fn movement(
        &self,
        entry: &Value,
        ring: &str,
        from: Option<&str>,
        note: &str,
        moved: Option<Moved>,
    ) -> DataRequest {
        self.movement_at(entry, ring, from, note, moved, &author(self.0), &now())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn movement_at(
        &self,
        entry: &Value,
        ring: &str,
        from: Option<&str>,
        note: &str,
        moved: Option<Moved>,
        author: &str,
        at: &str,
    ) -> DataRequest {
        DataRequest::insert(
            "movements",
            json!({
                "id": Uuid::now_v7(),
                "entry": entry,
                "ring": ring,
                "from": from,
                "moved": moved.unwrap_or_else(|| Moved::between(from, ring)).id(),
                "note": note,
                "author": author,
                "at": at,
            }),
        )
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        match err.problem() {
            Some((status, _)) if status < 500 => Self { status, detail: err.detail() },
            _ => {
                tracing::warn!(%err, "a call to the backend failed");
                Self::unavailable(format!("the radar's storage failed: {err}"))
            }
        }
    }
}
