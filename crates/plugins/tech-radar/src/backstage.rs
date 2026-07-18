//! The radar in Backstage's shape (`TechRadarLoaderResponse`), so a radar kept in Backstage can be
//! brought over and Backstage's own radar can be drawn from DOC.
//!
//! An import is matched to this radar's quadrants and rings by ID, then by name, then by position
//! in the file's own lists, and to its entries by key, so importing the same file twice changes
//! nothing the second time.

use std::collections::BTreeMap;

use chrono::{DateTime, NaiveDate, Utc};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::Refusal;
use crate::model::{Moved, QUADRANTS, RINGS};
use crate::store::{self, Edit, Entry, Move, Movement, Store};

pub const MAX_ENTRIES: usize = 1_000;
/// A timeline is written with its entry in one batch, which holds 100 writes.
pub const MAX_TIMELINE: usize = 99;

/// Backstage's own ring colours, which its radar draws with.
const RING_COLOURS: [&str; 4] = ["#5BA300", "#009EB0", "#C7BA00", "#E09B96"];

pub fn export(entries: &[Entry], movements: &[Movement]) -> Value {
    let mut timelines: BTreeMap<_, Vec<&Movement>> = BTreeMap::new();
    for movement in movements {
        timelines.entry(movement.entry).or_default().push(movement);
    }
    let entries: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let mut timeline = timelines.remove(&entry.id).unwrap_or_default();
            timeline.sort_by(|a, b| b.at.cmp(&a.at));
            let links = match entry.url.is_empty() {
                true => json!([]),
                false => json!([{ "url": entry.url, "title": "Learn more" }]),
            };
            json!({
                "id": entry.key,
                "key": entry.key,
                "title": entry.title,
                "quadrant": entry.quadrant,
                "description": entry.description,
                "url": entry.url,
                "links": links,
                "timeline": timeline.iter().map(|line| json!({
                    "moved": line.moved.backstage(),
                    "ringId": line.ring,
                    "date": line.at,
                    "description": line.note,
                })).collect::<Vec<_>>(),
            })
        })
        .collect();
    json!({
        "quadrants": QUADRANTS.iter().map(|quadrant| json!({
            "id": quadrant.id,
            "name": quadrant.name,
        })).collect::<Vec<_>>(),
        "rings": RINGS.iter().zip(RING_COLOURS).map(|(ring, colour)| json!({
            "id": ring.id,
            "name": ring.name,
            "color": colour,
            "description": ring.meaning,
        })).collect::<Vec<_>>(),
        "entries": entries,
    })
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Import {
    pub quadrants: Vec<Named>,
    pub rings: Vec<Named>,
    pub entries: Vec<Imported>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Named {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Imported {
    pub id: String,
    pub key: String,
    pub title: String,
    pub quadrant: String,
    pub description: String,
    pub url: String,
    pub links: Vec<Link>,
    pub timeline: Vec<Line>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Link {
    pub url: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Line {
    pub ring_id: String,
    pub date: String,
    pub description: String,
}

/// One imported entry, matched to this radar and checked.
#[derive(Debug, PartialEq)]
pub struct Planned {
    pub key: String,
    pub title: String,
    pub quadrant: String,
    pub ring: String,
    pub description: String,
    pub url: String,
    /// Its timeline, oldest first: the ring, when, and the note.
    pub history: Vec<(String, String, String)>,
}

/// `id` as one of this radar's IDs, matched by ID, by name, then by position in the file's list.
fn matched(id: &str, listed: &[Named], ours: &[(&'static str, &'static str)]) -> Option<String> {
    let same = |a: &str, b: &str| store::slug(a) == store::slug(b);
    let by_id = ours.iter().find(|(ours, _)| same(ours, id));
    let named = listed.iter().find(|named| same(&named.id, id));
    let by_name = named.and_then(|named| ours.iter().find(|(_, name)| same(name, &named.name)));
    let by_position =
        listed.iter().position(|named| same(&named.id, id)).and_then(|at| ours.get(at));
    by_id.or(by_name).or(by_position).map(|(id, _)| (*id).to_string())
}

/// A date as a timestamp in UTC, so timelines sort as text: RFC 3339, or a plain date at midnight.
fn timestamp(date: &str) -> Option<String> {
    let date = date.trim();
    if let Ok(at) = DateTime::parse_from_rfc3339(date) {
        return Some(at.with_timezone(&Utc).to_rfc3339());
    }
    let day = NaiveDate::parse_from_str(date.get(..10)?, "%Y-%m-%d").ok()?;
    Some(day.and_hms_opt(0, 0, 0)?.and_utc().to_rfc3339())
}

pub fn plan(import: &Import, now: &str) -> Result<Vec<Planned>, Refusal> {
    if import.entries.len() > MAX_ENTRIES {
        return Err(Refusal::bad(format!("an import holds at most {MAX_ENTRIES} entries")));
    }
    let quadrants: Vec<_> = QUADRANTS.iter().map(|quadrant| (quadrant.id, quadrant.name)).collect();
    let rings: Vec<_> = RINGS.iter().map(|ring| (ring.id, ring.name)).collect();
    let mut keys = BTreeMap::new();
    let mut planned = Vec::with_capacity(import.entries.len());
    for (index, entry) in import.entries.iter().enumerate() {
        let named = if entry.title.is_empty() {
            format!("entry {}", index + 1)
        } else {
            entry.title.clone()
        };
        let refuse = |detail: String| Refusal::bad(format!("{named}: {detail}"));
        let title = store::check_title(&entry.title).map_err(|refusal| refuse(refusal.detail))?;
        let key = [&entry.key, &entry.id, &entry.title]
            .into_iter()
            .map(|key| store::slug(key))
            .find(|key| !key.is_empty())
            .unwrap_or_default();
        let key = store::check_key(&key).map_err(|refusal| refuse(refusal.detail))?;
        if let Some(other) = keys.insert(key.clone(), title.clone()) {
            return Err(refuse(format!("its key {key} is also {other}'s")));
        }
        let quadrant = matched(&entry.quadrant, &import.quadrants, &quadrants)
            .ok_or_else(|| refuse(format!("there is no quadrant {}", entry.quadrant)))?;
        if entry.timeline.is_empty() {
            return Err(refuse("it has no timeline, so no ring".into()));
        }
        if entry.timeline.len() > MAX_TIMELINE {
            return Err(refuse(format!("a timeline has at most {MAX_TIMELINE} lines")));
        }
        let mut history = Vec::with_capacity(entry.timeline.len());
        for line in &entry.timeline {
            let ring = matched(&line.ring_id, &import.rings, &rings)
                .ok_or_else(|| refuse(format!("there is no ring {}", line.ring_id)))?;
            let at = timestamp(&line.date).unwrap_or_else(|| now.to_string());
            let note =
                store::check_note(&line.description).map_err(|refusal| refuse(refusal.detail))?;
            history.push((ring, at, note));
        }
        // Backstage lists the newest first; a stable sort keeps undated lines in the file's order.
        history.reverse();
        history.sort_by(|a, b| a.1.cmp(&b.1));
        let url = match entry.url.is_empty() {
            true => entry.links.first().map(|link| link.url.clone()).unwrap_or_default(),
            false => entry.url.clone(),
        };
        planned.push(Planned {
            key,
            title,
            quadrant,
            ring: history.last().map(|(ring, _, _)| ring.clone()).unwrap_or_default(),
            description: store::check_description(&entry.description)
                .map_err(|refusal| refuse(refusal.detail))?,
            url: store::check_url(&url).map_err(|refusal| refuse(refusal.detail))?,
            history,
        });
    }
    Ok(planned)
}

#[derive(Debug, Default)]
pub struct Outcome {
    pub added: usize,
    pub updated: usize,
    pub moved: usize,
    pub unchanged: usize,
}

/// Applies a plan: new entries arrive with their whole timeline, and entries already here take
/// the file's details and, if it differs, its ring, as a move.
pub async fn apply(store: &Store<'_>, planned: Vec<Planned>) -> Result<Outcome, Refusal> {
    let mut done = Outcome::default();
    for entry in planned {
        let Some(current) = store.find(&entry.key).await? else {
            store.create_with_history(&entry).await?;
            done.added += 1;
            continue;
        };
        let edit = Edit {
            title: (current.title != entry.title).then(|| entry.title.clone()),
            quadrant: (current.quadrant != entry.quadrant).then(|| entry.quadrant.clone()),
            description: (current.description != entry.description)
                .then(|| entry.description.clone()),
            url: (current.url != entry.url).then(|| entry.url.clone()),
        };
        let edited = edit.title.is_some()
            || edit.quadrant.is_some()
            || edit.description.is_some()
            || edit.url.is_some();
        if edited {
            store.edit(&entry.key, edit).await?;
        }
        if current.ring != entry.ring {
            let note = entry.history.last().map(|(_, _, note)| note.clone()).unwrap_or_default();
            let note = if note.is_empty() { "Imported".to_string() } else { note };
            store.move_to(&entry.key, Move { ring: entry.ring.clone(), note }).await?;
            done.moved += 1;
        } else if edited {
            done.updated += 1;
        } else {
            done.unchanged += 1;
        }
    }
    Ok(done)
}

impl Store<'_> {
    /// An imported entry and its whole timeline, together.
    async fn create_with_history(&self, planned: &Planned) -> Result<(), Refusal> {
        let id = json!(uuid::Uuid::now_v7());
        let last = planned.history.last();
        let moved = match planned.history.len() {
            1 => Moved::New,
            _ => {
                let before = &planned.history[planned.history.len() - 2].0;
                Moved::between(Some(before), &planned.ring)
            }
        };
        let mut writes = vec![doc_plugin_sdk::DataRequest::insert(
            "entries",
            json!({
                "id": id,
                "key": planned.key,
                "title": planned.title,
                "quadrant": planned.quadrant,
                "ring": planned.ring,
                "description": planned.description,
                "url": planned.url,
                "moved": moved.id(),
                "moved_at": last.map_or_else(store::now, |(_, at, _)| at.clone()),
            }),
        )];
        let author = store::author(self.0);
        let mut from: Option<&str> = None;
        for (ring, at, note) in &planned.history {
            writes.push(self.movement_at(&id, ring, from, note, None, &author, at));
            from = Some(ring);
        }
        self.0.batch(writes).await?;
        Ok(())
    }
}
