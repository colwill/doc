//! Kinds other plugins keep here. They publish `plugin.<id>.<kind>.synced`, or `.resources.synced`
//! for several kinds at once, with documents like the ones `apply` takes, and `.removed` with names.
//! A sync owns the connections it made, so one it no longer lists is removed.

use doc_plugin_sdk::{Backend, Event, PluginError};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::apply::{self, Document, Mode};
use crate::kinds::{self, ALL, Kind};
use crate::ops;
use crate::store::Store;

/// What the Knowledge Base says of each page it imports, and of each it removes.
const DOCUMENT_IMPORTED: &str = "plugin.kb.document.imported";
const DOCUMENT_REMOVED: &str = "plugin.kb.document.removed";

// Teams are no longer among them: core keeps teams, and GitHub provides them there (T68, T69).
pub const TOPICS: [&str; 16] = [
    DOCUMENT_IMPORTED,
    DOCUMENT_REMOVED,
    // The components the Architecture map declares. It was named as their publisher without being
    // listened to, so none ever arrived and a link to one found nothing.
    "plugin.architecture.component.synced",
    "plugin.architecture.component.removed",
    "plugin.github.resources.synced",
    "plugin.github.resources.removed",
    "plugin.github.repository.synced",
    "plugin.github.repository.removed",
    "plugin.ghe.resources.synced",
    "plugin.ghe.resources.removed",
    "plugin.kb.documentation.synced",
    "plugin.kb.documentation.removed",
    "plugin.kb.documentation-source.synced",
    "plugin.kb.documentation-source.removed",
    "plugin.infra.cloud-resource.synced",
    "plugin.infra.cloud-resource.removed",
];

/// Which plugin published, the kind the topic names (none for `resources`), and what happened.
fn topic(topic: &str) -> Option<(&str, Option<Kind>, &str)> {
    let mut parts = topic.split('.');
    let (Some("plugin"), Some(publisher), Some(slug), Some(action), None) =
        (parts.next(), parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return None;
    };
    if slug == "resources" {
        return Some((publisher, None, action));
    }
    let kind = ALL.into_iter().find(|kind| kind.slug() == slug)?;
    kind.publishers().contains(&publisher).then_some((publisher, Some(kind), action))
}

/// One document, or `{"documents": […]}`.
fn items(payload: Value) -> Value {
    match payload {
        Value::Object(mut object) if object.contains_key("documents") => {
            object.remove("documents").unwrap_or_default()
        }
        other => Value::Array(vec![other]),
    }
}

/// A page's Documentation resource: its space and path, in the characters a name may hold.
fn page_name(payload: &Value) -> String {
    let space = payload["space"].as_str().unwrap_or_default();
    let path = payload["path"].as_str().unwrap_or_default();
    let named = format!("{space}/{}", path.strip_suffix(".md").unwrap_or(path));
    named
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.:/@+".contains(c) { c } else { '-' })
        .collect()
}

/// A page as a Documentation document, connected to what it documents and to whoever keeps its
/// space, and to no others: a kind seeded empty here has its connections replaced by what the
/// event lists, so a page that stops documenting a service, or a space that changes hands, does
/// not keep the connection it had.
fn page_document(payload: &Value) -> Value {
    let mut connections = serde_json::Map::from_iter([
        ("Services".to_string(), json!([])),
        ("Organisations".to_string(), json!([])),
        ("Teams".to_string(), json!([])),
    ]);
    // The source that brought the page in, which the Knowledge Base names the same way in its
    // own sync. A page imported before sources were catalogued names none, and keeps none.
    let source = payload["source"].as_str().unwrap_or_default();
    if !source.is_empty() {
        connections.insert("DocumentationSources".to_string(), json!([source]));
    }
    for resource in payload["resources"].as_array().into_iter().flatten().filter_map(Value::as_str)
    {
        if let Some((kind, name)) = resource.split_once(':')
            && let Some(kind) = Kind::parse(kind)
        {
            let listed = connections.entry(kind.plural().to_string()).or_insert_with(|| json!([]));
            if let Some(names) = listed.as_array_mut() {
                names.push(json!(name));
            }
        }
    }
    json!({
        "kind": "Documentation",
        "name": page_name(payload),
        "title": payload["title"],
        "metadata": { "space": payload["space"], "path": payload["path"], "url": payload["url"] },
        "connections": connections,
    })
}

pub async fn on_event(backend: &Backend, event: Event) -> Result<(), PluginError> {
    match event.topic.as_str() {
        DOCUMENT_IMPORTED => {
            let documents = json!({ "documents": [page_document(&event.payload)] });
            return synced(backend, &Store(backend), "kb", Some(Kind::Documentation), documents)
                .await;
        }
        DOCUMENT_REMOVED => {
            let gone = json!({ "kind": "Documentation", "name": page_name(&event.payload) });
            return removed(backend, &Store(backend), "kb", Some(Kind::Documentation), gone).await;
        }
        _ => {}
    }
    let Some((publisher, kind, action)) = topic(&event.topic) else { return Ok(()) };
    let store = Store(backend);
    match action {
        "synced" => synced(backend, &store, publisher, kind, event.payload).await,
        "removed" => removed(backend, &store, publisher, kind, event.payload).await,
        _ => Ok(()),
    }
}

async fn synced(
    backend: &Backend,
    store: &Store<'_>,
    publisher: &str,
    kind: Option<Kind>,
    payload: Value,
) -> Result<(), PluginError> {
    let mut documents: Vec<Document> = serde_json::from_value(items(payload)).map_err(|err| {
        PluginError::from(format!("{publisher}'s documents were not understood: {err}"))
    })?;
    for document in &mut documents {
        if let Some(kind) = kind.filter(|_| document.kind.is_empty()) {
            document.kind = kind.name().into();
        }
    }
    let mode = Mode::Sync { publisher };
    let plan = apply::plan(backend, store, documents, &mode).await?;
    for skipped in &plan.skipped {
        tracing::info!(publisher, skipped, "part of a sync was left out");
    }
    apply::execute(store, &plan, publisher).await?;
    if plan.changed() > 0 {
        let detail = json!({ "by": publisher, "changes": plan.changes });
        ops::record(backend, store, "synced", publisher, detail).await;
    }
    Ok(())
}

#[derive(Deserialize)]
struct Gone {
    #[serde(default)]
    kind: String,
    name: String,
}

async fn removed(
    backend: &Backend,
    store: &Store<'_>,
    publisher: &str,
    kind: Option<Kind>,
    payload: Value,
) -> Result<(), PluginError> {
    let gone: Vec<Gone> = serde_json::from_value(items(payload)).map_err(|err| {
        PluginError::from(format!("{publisher}'s removals were not understood: {err}"))
    })?;
    for item in gone {
        let named = match kind {
            Some(kind) if item.kind.is_empty() => Ok(kind),
            _ => kinds::kind(&item.kind),
        };
        let Some(kind) = named.ok().filter(|kind| kind.publishers().contains(&publisher)) else {
            tracing::info!(publisher, kind = %item.kind, "a removal of a kind it does not publish was ignored");
            continue;
        };
        if store.forget(kind, &item.name, publisher).await? {
            let subject = format!("{kind}:{}", item.name);
            ops::record(backend, store, "removed", &subject, json!({ "by": publisher })).await;
        }
    }
    Ok(())
}
