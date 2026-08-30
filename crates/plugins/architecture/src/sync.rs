//! Components as Catalogue resources (ADR-0017 §1). The Catalogue keeps the kind and this plugin
//! owns it, through the sync events `resources` already listens for — the same way `github` keeps
//! repositories and `infra` keeps cloud resources.
//!
//! A sync owns the connections it made, so a component no longer listed loses them.

use doc_plugin_sdk::{Backend, PluginError};
use serde_json::{Value, json};

use crate::model::{Component, role_label};
use crate::store::Store;

/// What `resources` listens for, named after this plugin and the kind it keeps.
const SYNCED: &str = "plugin.architecture.component.synced";
const REMOVED: &str = "plugin.architecture.component.removed";

/// One component as a resource document, connected to the service it belongs to.
fn document(component: &Component) -> Value {
    json!({
        "kind": "Component",
        "name": format!("{}/{}", component.service, component.name),
        "title": component.shown(),
        "description": component.description,
        "metadata": {
            "role": component.role,
            "role_label": role_label(&component.role),
            "service": component.service,
            "origin": component.origin,
            "source": component.source,
        },
        "connections": { "Services": [component.service] },
    })
}

async fn announce(backend: &Backend, topic: &str, payload: Value) {
    if let Err(err) = backend.publish(topic, payload).await {
        tracing::warn!(%err, %topic, "the Catalogue was not told about a component");
    }
}

/// Says a component is there, or has changed. Fire and forget: the Catalogue is a reader of this,
/// never a gate on it, so a component is usable here whether or not the Catalogue heard.
pub async fn synced(backend: &Backend, component: &Component) {
    announce(backend, SYNCED, document(component)).await;
}

/// Says every component there is, in one event, and how many that was. What one declared while
/// the Catalogue was not listening needs to arrive at all, and a sync of what it already has
/// changes nothing, so this is safe to say again and again.
pub async fn everything(backend: &Backend) -> Result<usize, PluginError> {
    let components = Store(backend)
        .components(None)
        .await
        .map_err(|refusal| PluginError::from(refusal.detail))?;
    if !components.is_empty() {
        let documents: Vec<Value> = components.iter().map(document).collect();
        announce(backend, SYNCED, json!({ "documents": documents })).await;
    }
    Ok(components.len())
}

/// Says a component has gone, by the name the Catalogue knows it as. The kind comes from the
/// topic, so the payload is the name alone — the shape the Catalogue's `removed` reads.
pub async fn removed(backend: &Backend, component: &Component) {
    let name = format!("{}/{}", component.service, component.name);
    announce(backend, REMOVED, json!({ "name": name })).await;
}
