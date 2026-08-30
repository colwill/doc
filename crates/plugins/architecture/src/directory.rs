//! Who is asking, and whether they may change what somebody owns (ADR-0017 §7).

use std::time::Duration;

use doc_plugin_sdk::Backend;
use serde_json::json;

use crate::model::Refusal;

/// Whoever is asking, as `created_by` records them.
pub fn me(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "the platform".to_string())
}

/// Whether the caller may change something owned by `owner`.
///
/// **This is not yet the check ADR-0017 §7 describes.** It admits a platform admin, anybody with
/// write on this plugin, and an RBAC admin — which is the same fallback the Catalogue uses for its
/// own ownership questions, because neither plugin can resolve a team's members from `Caller`
/// alone: it carries the caller's scopes and attributes, not their teams. Doing it properly means
/// reading core's people and teams as `secrets` does, and resolving a `service:` owner through the
/// Catalogue to the team that owns it. Until then this is deliberately coarse rather than
/// pretending to be fine-grained.
pub async fn managing(backend: &Backend, owner: &str) -> Result<(), Refusal> {
    let caller = backend.caller().ok_or_else(|| Refusal::forbidden("nobody is asking"))?;
    if caller.admin || caller.writes() {
        return Ok(());
    }
    let access = backend
        .request("core.access", "access", json!({}), Duration::from_secs(10))
        .await
        .map_err(|err| Refusal::unavailable(format!("core could not say who you are: {err}")))?;
    if access["admin"] == true || access["plugins"]["rbac"]["write"] == true {
        return Ok(());
    }
    Err(Refusal::forbidden(format!("only whoever looks after {owner} may change that")))
}

/// What the owner is called, for a page to show. Its reference where the Catalogue has no title
/// for it, which is better than nothing and never wrong.
pub async fn owner_label(backend: &Backend, owner: &str) -> String {
    let Some((kind, name)) = owner.split_once(':') else { return owner.to_string() };
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("kind", kind)
        .append_pair("name", name)
        .finish();
    match backend.ask("resources", "GET", "resources", Some(&query), None).await {
        Ok((200, listed)) => listed
            .as_array()
            .into_iter()
            .flatten()
            .find(|resource| resource["name"] == json!(name))
            .and_then(|resource| resource["title"].as_str().map(str::to_string))
            .filter(|title| !title.is_empty())
            .unwrap_or_else(|| name.to_string()),
        _ => name.to_string(),
    }
}

/// The Catalogue's names for a kind, for a form to offer. Empty where it cannot be asked, which
/// leaves the field a plain one rather than taking the page down.
pub async fn names(backend: &Backend, kind: &str) -> Vec<String> {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("kind", kind)
        .append_pair("limit", "500")
        .finish();
    match backend.ask("resources", "GET", "resources", Some(&query), None).await {
        Ok((200, listed)) => {
            let mut names: Vec<String> = listed
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|resource| resource["name"].as_str().map(str::to_string))
                .collect();
            names.sort();
            names.dedup();
            names
        }
        _ => Vec::new(),
    }
}

/// Every `Component` the Catalogue holds, as `(name, title)`. A component may be declared here by
/// this plugin or applied straight to the Catalogue — DOC's own platform components are applied —
/// and either way it is a real component that belongs on a map, so the palette offers both.
pub async fn catalogued(backend: &Backend) -> Vec<(String, String)> {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("kind", "Component")
        .append_pair("limit", "500")
        .finish();
    match backend.ask("resources", "GET", "resources", Some(&query), None).await {
        Ok((200, listed)) => {
            let mut found: Vec<(String, String)> = listed
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|resource| {
                    let name = resource["name"].as_str()?.to_string();
                    let title = resource["title"].as_str().unwrap_or(&name).to_string();
                    Some((name, title))
                })
                .collect();
            found.sort();
            found.dedup();
            found
        }
        _ => Vec::new(),
    }
}
