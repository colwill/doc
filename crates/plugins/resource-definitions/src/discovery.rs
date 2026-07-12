//! What other plugins read as themselves. Every `api/` route answers whoever a call is for, and a
//! plugin's own work — a nightly schedule, an event it hears — is for nobody, so core refuses it.
//! The plugins `reader-plugins` names may read the catalogue's own resources and the connections
//! between them here instead: the same answers `api/` gives anybody who can read the catalogue,
//! since none of it is narrowed to the viewer. People, service accounts and roles are left out:
//! they are core's and `rbac`'s, read as whoever asks.

use doc_plugin_sdk::{Backend, Request, Response, Setting, SettingKind};
use serde_json::{Value, json};

use crate::kinds;

pub const READER_PLUGINS: &str = "reader-plugins";
/// End of life finds which services each repository belongs to overnight and whenever a scan or
/// a connection changes what it knows.
const READERS: [&str; 1] = ["eol"];

pub fn declared() -> Vec<Setting> {
    vec![
        Setting::new(
            READER_PLUGINS,
            "Plugins that read the catalogue as themselves",
            SettingKind::List,
        )
        .defaulting(json!(READERS))
        .hinted(
            "Which plugins may read services, repositories and the connections between them for \
             work nobody started, such as a nightly read, rather than as whoever is looking. A \
             plugin left out asks to be added, and whoever may change these settings approves \
             or denies it.",
        )
        .requestable()
        .grouped("Plugins"),
    ]
}

/// A kind other plugins may read here: one the catalogue keeps or writes itself.
fn shared(kind: &str) -> bool {
    kinds::kind(kind).is_ok_and(kinds::Kind::written)
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let asking = match backend.caller() {
        Some(caller) if caller.kind == "plugin" => caller.id.clone().unwrap_or_default(),
        _ => return Response::problem(403, "forbidden", "discovery routes are for plugins"),
    };
    if !backend.settings().list(READER_PLUGINS).contains(&asking) {
        let detail = format!(
            "{asking} is not one of the plugins that read the catalogue as themselves; it asks to \
             be added to {READER_PLUGINS}"
        );
        return Response::problem(403, "not-listed", &detail);
    }
    if request.method != "GET" {
        return Response::problem(405, "method-not-allowed", "the catalogue is only read here");
    }
    let allowed = match path {
        ["version"] | ["neighbours"] => true,
        ["resources"] => crate::api::query(request, "kind").is_some_and(|kind| shared(&kind)),
        ["resources", kind, _, ..] => shared(kind),
        _ => false,
    };
    if !allowed {
        return Response::problem(
            404,
            "not-found",
            "plugins read services, repositories and the rest of the catalogue's own kinds here",
        );
    }
    let answered = crate::api::api(backend, request, path).await;
    match answered {
        Ok((status, mut value)) => {
            // A neighbour worked out from membership or `rbac` is a person or a role: not shared.
            if let Some(neighbours) = value.get_mut("neighbours").and_then(Value::as_array_mut) {
                neighbours.retain(|neighbour| neighbour["kind"].as_str().is_some_and(shared));
                value["hidden"] = json!([]);
            }
            Response::new(
                status,
                "application/json",
                serde_json::to_vec(&value).unwrap_or_default(),
            )
        }
        Err(refusal) => refusal.response(),
    }
}
