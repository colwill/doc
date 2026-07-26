//! `discovery/notify` (any plugin, for any user) and the JSON `api/` routes (the caller's own, for
//! programmatic use — the pages under `ui/` are what the browser actually drives).

use doc_plugin_sdk::{Backend, Caller, Request, Response};
use serde_json::json;

use crate::Refusal;
use crate::store::{New, Notification, Store};

fn shown(notification: &Notification) -> serde_json::Value {
    json!({
        "id": notification.id,
        "title": notification.title,
        "body": notification.body,
        "url": notification.url,
        "source": notification.source,
        "read": notification.read_at.is_some(),
        "archived": notification.archived_at.is_some(),
        "created_at": notification.created_at,
    })
}

/// `discovery/notify`: any plugin may call this for any user — it decides its own authorization by
/// being a discovery route at all, the same reasoning `resource-definitions`' sync routes use.
pub async fn notify(backend: &Backend, request: &Request) -> Response {
    let mut asked: New = match request.json() {
        Ok(asked) => asked,
        Err(err) => {
            return Refusal::bad(format!("the body is not what this takes: {err}")).response();
        }
    };
    // What made it is the plugin that sent it, as core vouches, not what the body claims.
    if let Some(Caller { kind, id: Some(id), .. }) = backend.caller()
        && kind == "plugin"
    {
        asked.source = id.clone();
    }
    match Store(backend).create(asked).await {
        Ok(notification) => {
            crate::ui::announce(backend).await;
            Response::json(&json!({ "id": notification.id }))
        }
        Err(refusal) => refusal.response(),
    }
}

fn query(request: &Request, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    match (request.method.as_str(), path) {
        ("GET", ["list"]) => list(backend, request).await,
        _ => Refusal::missing("no such route").response(),
    }
}

async fn list(backend: &Backend, request: &Request) -> Response {
    let archived = query(request, "archived").as_deref() == Some("true");
    let limit: u32 = query(request, "limit").and_then(|limit| limit.parse().ok()).unwrap_or(50);
    let after = query(request, "after");
    match Store(backend).list(archived, limit, after).await {
        Ok((notifications, next)) => Response::json(&json!({
            "notifications": notifications.iter().map(shown).collect::<Vec<_>>(),
            "next": next,
        })),
        Err(refusal) => refusal.response(),
    }
}
