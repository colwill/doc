//! Who looks after a space: the teams whose documentation it is, or the organisation as a whole.
//!
//! This is a different question from what a space's pages *document*, which `resource` answers. A
//! runbook space kept by the platform team documents nothing in particular; a repository's space
//! documents that repository and is kept by whichever team owns it. Both go out to the catalogue
//! on every page, so it can say "this page is about the payments service" and "this page is the
//! platform team's" — the second being what `Organisation-to-Documentation` and
//! `Team-to-Documentation` were added for.

use std::collections::BTreeMap;

use doc_plugin_sdk::Backend;
use serde_json::Value;

use crate::Refusal;
use crate::imports::checked_resource;
use crate::store::{Space, Store};

/// The kinds that may look after a space, as `checked_resource` writes them.
pub const TEAM: &str = "team";
pub const ORGANISATION: &str = "organisation";

/// Said wherever a space is made without saying who keeps it.
pub const NEEDED: &str = "say who looks after the space: one or more teams, or the organisation, \
     written kind:name, such as team:payments-core";

/// The most teams or organisations the picker offers.
const MOST: &str = "500";

/// One thing a space may belong to, as the picker offers it.
#[derive(Debug, Clone)]
pub struct Choice {
    /// `team:payments-core`, which is what the form sends.
    pub value: String,
    /// `Organisation` or `Team`, said beside the name so the two groups are told apart.
    pub kind: &'static str,
    pub label: String,
    pub checked: bool,
    /// The first of its kind, which the "or" between the two groups is drawn above.
    pub divided: bool,
}

/// Owners as they are stored: each written `kind:name`, all teams or one organisation.
///
/// Nothing is required here — a space made before this keeps going without owners, and a
/// repository whose catalogue entry names no team is left unowned rather than guessed at. It is
/// the places that *make* a space by hand that insist, with `NEEDED`.
pub fn checked(owners: &[String]) -> Result<Vec<String>, Refusal> {
    let mut named = Vec::new();
    for owner in owners.iter().filter(|owner| !owner.trim().is_empty()) {
        let owner = checked_resource(owner)?;
        let kind = owner.split(':').next().unwrap_or_default();
        if kind != TEAM && kind != ORGANISATION {
            return Err(Refusal::bad(format!(
                "a space belongs to teams or to an organisation, not to `{kind}`"
            )));
        }
        if !named.contains(&owner) {
            named.push(owner);
        }
    }
    let organisations = named.iter().filter(|owner| owner.starts_with(ORGANISATION)).count();
    if organisations > 0 && organisations < named.len() {
        return Err(Refusal::bad(
            "a space belongs to its teams or to the organisation as a whole, not to both",
        ));
    }
    if organisations > 1 {
        return Err(Refusal::bad("a space belongs to one organisation"));
    }
    named.sort();
    Ok(named)
}

/// The teams and organisations there are, the ones already chosen ticked. A catalogue that cannot
/// be reached offers nothing rather than refusing, so a space page still draws.
pub async fn choices(backend: &Backend, chosen: &[String]) -> Vec<Choice> {
    let mut offered: Vec<Choice> = Vec::new();
    for (kind, written) in [("Organisation", ORGANISATION), ("Team", TEAM)] {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("kind", kind)
            .append_pair("limit", MOST)
            .finish();
        let Ok((200, listed)) =
            backend.ask("resources", "GET", "resources", Some(&query), None).await
        else {
            continue;
        };
        for resource in listed.as_array().into_iter().flatten() {
            let Some(name) = resource["name"].as_str().filter(|name| !name.is_empty()) else {
                continue;
            };
            let value = format!("{written}:{name}");
            offered.push(Choice {
                checked: chosen.contains(&value),
                kind,
                divided: offered.last().is_some_and(|last| last.kind != kind),
                label: resource["title"]
                    .as_str()
                    .filter(|title| !title.is_empty())
                    .unwrap_or(name)
                    .to_string(),
                value,
            });
        }
    }
    // Somebody may have been named before the catalogue knew them, or after it forgot: whoever a
    // space already belongs to is always offered, so saving the form again does not drop them.
    for owner in chosen {
        if !offered.iter().any(|choice| &choice.value == owner) {
            let (kind, name) = owner.split_once(':').unwrap_or((TEAM, owner));
            let kind = if kind == ORGANISATION { "Organisation" } else { "Team" };
            offered.push(Choice {
                value: owner.clone(),
                divided: offered.last().is_some_and(|last| last.kind != kind),
                kind,
                label: name.to_string(),
                checked: true,
            });
        }
    }
    offered
}

/// Sets who looks after a space and tells the catalogue, by writing the owners onto every page the
/// space holds and announcing each of them again. Returns how many pages were re-announced.
///
/// A page's `resources` is what the catalogue connects it to, so the owners have to be on the page
/// and not only on the space: the alternative is waiting for the next sync, which for an upload
/// source never comes.
pub async fn set(
    backend: &Backend,
    store: &Store<'_>,
    space: &Space,
    owners: &[String],
) -> Result<usize, Refusal> {
    let owners = checked(owners)?;
    if owners == space.owners {
        return Ok(0);
    }
    store.set_owners(&space.key, &owners).await?;
    let sources: BTreeMap<String, String> = crate::sources::list(store)
        .await?
        .iter()
        .map(|source| {
            (
                source["id"].as_str().unwrap_or_default().to_string(),
                crate::sources::resource_name(source),
            )
        })
        .collect();
    let (mut writes, mut announced) = (Vec::new(), Vec::new());
    for page in store.pages_of(&space.key).await? {
        let id = page.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
        let path = page.get("path").and_then(Value::as_str).unwrap_or_default().to_string();
        let mut resources: Vec<String> = page
            .get("resources")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|resource| !space.owners.iter().any(|was| was == resource))
            .map(str::to_string)
            .collect();
        for owner in &owners {
            if !resources.contains(owner) {
                resources.push(owner.clone());
            }
        }
        resources.sort();
        resources.dedup();
        announced.push(serde_json::json!({
            "space": space.key,
            "path": path,
            "title": page.get("title"),
            "url": crate::imports::page_url(&space.key, &path),
            "resources": resources,
            "source": page
                .get("source")
                .and_then(Value::as_str)
                .and_then(|source| sources.get(source))
                .cloned()
                .unwrap_or_default(),
        }));
        writes.push((id, resources));
    }
    store.set_page_resources(writes).await?;
    for document in &announced {
        if let Err(err) = backend.publish(crate::imports::IMPORTED, document.clone()).await {
            tracing::warn!(%err, "a page's owners were saved but not announced");
            break;
        }
    }
    Ok(announced.len())
}
