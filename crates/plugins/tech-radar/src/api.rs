//! The JSON routes under `api/`, and the writes both they and the pages make: each is published,
//! audited where it changes what people are told to use, and redraws every open radar.

use doc_plugin_sdk::{Backend, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::backstage::{self, Import, Outcome};
use crate::model::{self, Moved};
use crate::store::{Edit, Entry, Move, Movement, NewEntry, Store};
use crate::{Refusal, ui};

type Answer = Result<(u16, Value), Refusal>;

pub fn query(request: &Request, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn body<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T, Refusal> {
    let bytes = if request.body.is_empty() { &b"{}"[..] } else { &request.body[..] };
    serde_json::from_slice(bytes)
        .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))
}

/// Where people see an entry in DOC.
pub fn page(entry: &Entry) -> String {
    let base = std::env::var("DOC_PUBLIC_URL").unwrap_or_default();
    format!("{}/p/radar/entries/{}", base.trim_end_matches('/'), entry.key)
}

pub fn entry_shown(entry: &Entry, number: Option<usize>) -> Value {
    json!({
        "id": entry.id,
        "key": entry.key,
        "number": number,
        "title": entry.title,
        "quadrant": entry.quadrant,
        "quadrant_name": model::quadrant_name(&entry.quadrant),
        "ring": entry.ring,
        "ring_name": model::ring_name(&entry.ring),
        "description": entry.description,
        "url": entry.url,
        "moved": entry.moved,
        "moved_at": entry.moved_at,
        "updated_at": entry.updated_at,
        "page": page(entry),
    })
}

fn movement_shown(movement: &Movement) -> Value {
    json!({
        "ring": movement.ring,
        "from": movement.from,
        "moved": movement.moved,
        "note": movement.note,
        "author": movement.author,
        "at": movement.at,
    })
}

/// Tells subscribers and every open radar; the change is already stored, so a failure is logged.
async fn announce(backend: &Backend, what: &str, entry: &Entry, detail: Value) {
    let mut payload = json!({ "id": entry.id, "key": entry.key, "title": entry.title });
    if let (Some(payload), Value::Object(detail)) = (payload.as_object_mut(), detail) {
        payload.extend(detail);
    }
    let topic = format!("plugin.radar.entry.{what}");
    if let Err(err) = backend.publish(&topic, payload.clone()).await {
        tracing::warn!(%err, topic, "a change was stored but not announced");
    }
    if matches!(what, "moved" | "removed")
        && let Err(err) = backend.audit(&format!("entry.{what}"), Some(&entry.key), payload).await
    {
        tracing::warn!(%err, "a change was stored but not audited");
    }
    redraw(backend).await;
}

async fn redraw(backend: &Backend) {
    if let Err(err) = backend.publish("plugin.radar.ui.radar", json!({})).await {
        tracing::warn!(%err, "open radars were not told to redraw");
    }
}

pub async fn add(backend: &Backend, asked: NewEntry) -> Result<Entry, Refusal> {
    let entry = Store(backend).create(asked).await?;
    announce(backend, "added", &entry, json!({ "quadrant": entry.quadrant, "ring": entry.ring }))
        .await;
    Ok(entry)
}

pub async fn edit(backend: &Backend, id: &str, asked: Edit) -> Result<Entry, Refusal> {
    let entry = Store(backend).edit(id, asked).await?;
    announce(backend, "changed", &entry, json!({ "quadrant": entry.quadrant })).await;
    Ok(entry)
}

pub async fn move_to(backend: &Backend, id: &str, asked: Move) -> Result<(Entry, Moved), Refusal> {
    let note = asked.note.clone();
    let (entry, moved) = Store(backend).move_to(id, asked).await?;
    let detail = json!({ "ring": entry.ring, "moved": moved, "note": note });
    announce(backend, "moved", &entry, detail).await;
    Ok((entry, moved))
}

pub async fn remove(backend: &Backend, id: &str) -> Result<Entry, Refusal> {
    let entry = Store(backend).remove(id).await?;
    announce(backend, "removed", &entry, json!({ "ring": entry.ring })).await;
    Ok(entry)
}

pub async fn import(backend: &Backend, file: &Import) -> Result<Outcome, Refusal> {
    let planned = backstage::plan(file, &crate::store::now())?;
    let outcome = backstage::apply(&Store(backend), planned).await;
    // Whatever was imported before a failure is stored, so the radars redraw either way.
    redraw(backend).await;
    let outcome = outcome?;
    let detail = json!({
        "added": outcome.added,
        "updated": outcome.updated,
        "moved": outcome.moved,
        "unchanged": outcome.unchanged,
    });
    if let Err(err) = backend.publish("plugin.radar.imported", detail.clone()).await {
        tracing::warn!(%err, "an import was stored but not announced");
    }
    if let Err(err) = backend.audit("imported", None, detail).await {
        tracing::warn!(%err, "an import was stored but not audited");
    }
    Ok(outcome)
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    match route(backend, request, path).await {
        Ok((204, _)) => Response::new(204, "application/json", Vec::new()),
        Ok((status, value)) => Response::new(
            status,
            "application/json",
            serde_json::to_vec(&value).unwrap_or_default(),
        ),
        Err(refusal) => refusal.response(),
    }
}

async fn route(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let store = Store(backend);
    match (request.method.as_str(), path) {
        ("GET", ["entries"]) => {
            let (quadrant, ring) = (query(request, "quadrant"), query(request, "ring"));
            let numbered = ui::numbered(store.entries().await?);
            let wanted = match query(request, "q") {
                Some(text) => Some(store.search(&text).await?),
                None => None,
            };
            let listed: Vec<Value> = numbered
                .iter()
                .filter(|(_, entry)| quadrant.as_ref().is_none_or(|q| *q == entry.quadrant))
                .filter(|(_, entry)| ring.as_ref().is_none_or(|r| *r == entry.ring))
                .filter(|(_, entry)| {
                    wanted.as_ref().is_none_or(|found| found.iter().any(|f| f.id == entry.id))
                })
                .map(|(number, entry)| entry_shown(entry, Some(*number)))
                .collect();
            Ok((200, json!(listed)))
        }
        ("POST", ["entries"]) => {
            let entry = add(backend, body(request)?).await?;
            Ok((201, entry_shown(&entry, None)))
        }
        ("GET", ["entries", id]) => {
            let entry = store.found(id).await?;
            let timeline = store.movements(entry.id).await?;
            let mut shown = entry_shown(&entry, None);
            shown["timeline"] = json!(timeline.iter().map(movement_shown).collect::<Vec<_>>());
            Ok((200, shown))
        }
        ("PATCH", ["entries", id]) => {
            let entry = edit(backend, id, body(request)?).await?;
            Ok((200, entry_shown(&entry, None)))
        }
        ("POST", ["entries", id, "move"]) => {
            let (entry, moved) = move_to(backend, id, body(request)?).await?;
            Ok((200, json!({ "entry": entry_shown(&entry, None), "moved": moved })))
        }
        ("DELETE", ["entries", id]) => {
            remove(backend, id).await?;
            Ok((204, Value::Null))
        }
        ("GET", ["radar"]) => {
            let entries = store.entries().await?;
            let movements: Vec<Movement> =
                backend.query_all(doc_plugin_sdk::Query::new("movements")).await?;
            Ok((200, backstage::export(&entries, &movements)))
        }
        ("POST", ["import"]) => {
            let outcome = import(backend, &body(request)?).await?;
            Ok((
                200,
                json!({
                    "added": outcome.added,
                    "updated": outcome.updated,
                    "moved": outcome.moved,
                    "unchanged": outcome.unchanged,
                }),
            ))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}
