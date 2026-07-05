//! The access map. Who can reach a resource is drawn by the Service Map from Resource Definitions,
//! asked as the viewer; what a principal can reach is known here, and the Service Map lays it out.

use std::collections::{BTreeMap, BTreeSet};

use doc_plugin_sdk::Backend;
use serde_json::{Value, json};
use url::form_urlencoded::Serializer;

use crate::model::{Holder, Refusal};
use crate::ops;
use crate::store::Store;

/// The kinds of resource the Catalogue keeps, which are the ones worth mapping access to.
const CHOOSABLE: [&str; 6] =
    ["Organisation", "Service", "Repository", "Team", "Documentation", "CloudResource"];
/// How many of each kind the chooser lists before asking the viewer to type.
const PER_KIND: usize = 12;
const MAX_FOUND: usize = 40;

/// A resource the viewer can map, as `Kind:name`.
pub struct Pick {
    pub value: String,
    pub name: String,
    pub title: String,
}

/// Resources of one kind, and how many more there are than are listed.
pub struct Kinded {
    pub plural: String,
    pub picks: Vec<Pick>,
    pub more: usize,
}

async fn catalogue(backend: &Backend, route: &str, query: &str) -> Result<Value, Refusal> {
    match backend.ask("resources", "GET", route, Some(query), None).await {
        Ok((200, body)) => Ok(body),
        Ok((status, body)) => Err(refused("the Catalogue would not list them", status, &body)),
        Err(err) => Err(Refusal::unavailable(format!("the Catalogue could not be asked: {err}"))),
    }
}

fn pick(resource: &Value) -> Option<Pick> {
    let kind = kind_named(resource["kind"].as_str()?)?;
    let name = resource["name"].as_str()?.to_string();
    let title = resource["title"].as_str().filter(|title| *title != name).unwrap_or_default();
    Some(Pick { value: format!("{kind}:{name}"), title: title.to_string(), name })
}

/// Resources to map, as the viewer sees them in the Catalogue: a few of every kind, or those whose
/// name matches what was typed. `service:card` narrows to one kind.
pub async fn choices(backend: &Backend, typed: &str) -> Result<Vec<Kinded>, Refusal> {
    let overview = catalogue(backend, "kinds", "").await?;
    let mut plurals = BTreeMap::new();
    let mut counts = BTreeMap::new();
    for kind in overview["kinds"].as_array().into_iter().flatten() {
        let Some(name) = kind["kind"].as_str().and_then(kind_named) else { continue };
        plurals.insert(name, kind["plural"].as_str().unwrap_or(name).to_string());
        counts.insert(name, kind["count"].as_u64().unwrap_or_default());
    }
    let (kind, text) = match typed.split_once(':') {
        Some((kind, text)) => (kind_named(kind), text.trim()),
        None => (None, typed.trim()),
    };
    let kinds: Vec<&str> = CHOOSABLE
        .into_iter()
        .filter(|choosable| kind.is_none_or(|kind| kind == *choosable))
        .collect();
    let mut grouped: BTreeMap<&str, Vec<Pick>> = BTreeMap::new();
    if text.is_empty() {
        for kind in &kinds {
            if counts.get(kind).copied().unwrap_or_default() == 0 {
                continue;
            }
            let query = Serializer::new(String::new())
                .append_pair("kind", kind)
                .append_pair("limit", &PER_KIND.to_string())
                .finish();
            let listed = catalogue(backend, "resources", &query).await?;
            grouped
                .insert(kind, listed.as_array().into_iter().flatten().filter_map(pick).collect());
        }
    } else {
        let query = Serializer::new(String::new())
            .append_pair("q", text)
            .append_pair("kinds", &kinds.join(","))
            .append_pair("limit", &MAX_FOUND.to_string())
            .finish();
        let found = catalogue(backend, "search", &query).await?;
        for pick in found.as_array().into_iter().flatten().filter_map(pick) {
            let kind =
                kinds.iter().copied().find(|kind| pick.value.starts_with(&format!("{kind}:")));
            if let Some(kind) = kind {
                grouped.entry(kind).or_default().push(pick);
            }
        }
    }
    Ok(kinds
        .into_iter()
        .filter_map(|kind| {
            let picks = grouped.remove(kind).filter(|picks| !picks.is_empty())?;
            let total = usize::try_from(counts.get(kind).copied().unwrap_or_default()).unwrap_or(0);
            let more = match text.is_empty() {
                true => total.saturating_sub(picks.len()),
                false => 0,
            };
            Some(Kinded {
                plural: plurals.get(kind).cloned().unwrap_or_else(|| kind.into()),
                picks,
                more,
            })
        })
        .collect())
}

/// A drawing and what could not be put on it.
pub struct Drawn {
    pub svg: String,
    pub notes: Vec<String>,
}

fn refused(what: &str, status: u16, body: &Value) -> Refusal {
    let detail = body["detail"].as_str().map_or_else(|| body.to_string(), str::to_string);
    Refusal { status, detail: format!("{what}: {detail}") }
}

const KINDS: [&str; 9] = [
    "Organisation",
    "Service",
    "Repository",
    "Team",
    "Role",
    "User",
    "ServiceAccount",
    "Documentation",
    "CloudResource",
];

/// A kind as Resource Definitions names it, from however it was written: `service-account` works.
fn kind_named(text: &str) -> Option<&'static str> {
    let wanted: String = text.chars().filter(char::is_ascii_alphanumeric).collect();
    // Organisations were called verticals, and older links still say so.
    if wanted.eq_ignore_ascii_case("vertical") || wanted.eq_ignore_ascii_case("organization") {
        return Some("Organisation");
    }
    KINDS.into_iter().find(|kind| kind.eq_ignore_ascii_case(&wanted))
}

/// A resource, the teams connected to it, their roles and whoever holds them, as the viewer sees it.
pub async fn resource(backend: &Backend, resource: &str) -> Result<Drawn, Refusal> {
    let (kind, name) = resource
        .split_once(':')
        .and_then(|(kind, name)| {
            Some((kind_named(kind)?, name.trim())).filter(|(_, name)| !name.is_empty())
        })
        .ok_or_else(|| {
            Refusal::bad(format!("write the resource as kind:name, not `{resource}`"))
        })?;
    let mut shown = vec!["Team", "Role", "User", "ServiceAccount"];
    if !shown.contains(&kind) {
        shown.push(kind);
    }
    let query = Serializer::new(String::new())
        .append_pair("focus", &format!("{kind}:{name}"))
        .append_pair("depth", "3")
        .append_pair("kinds", &shown.join(","))
        .finish();
    let (status, body) =
        backend.ask("service-map", "GET", "graph", Some(&query), None).await.map_err(|err| {
            Refusal::unavailable(format!("the Service Map could not be asked: {err}"))
        })?;
    if status != 200 {
        return Err(refused("the Service Map would not draw it", status, &body));
    }
    let notes = body["hidden"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|reason| {
            reason.as_str().map(|reason| format!("Some of it is hidden from you: {reason}"))
        })
        .collect();
    Ok(Drawn { svg: body["svg"].as_str().unwrap_or_default().to_string(), notes })
}

/// Which plugin a permission belongs to: `plugin:<id>:…`.
fn plugin_of(permission: &str) -> Option<&str> {
    permission.strip_prefix("plugin:")?.split(':').next()
}

fn item(id: &str, column: &str, label: &str, note: &str, href: Option<&str>, focus: bool) -> Value {
    json!({ "id": id, "column": column, "label": label, "note": note, "href": href, "focus": focus })
}

/// A principal, its groups and permissions, and the plugins they reach, which this plugin knows.
pub async fn principal(
    backend: &Backend,
    store: &Store<'_>,
    holder: &Holder,
) -> Result<Drawn, Refusal> {
    let view = ops::principal(store, holder).await?;
    let label = view["label"].as_str().unwrap_or_default();
    let column = match holder {
        Holder::Service { .. } => "Service account",
        _ => "Person",
    };
    let mut items = vec![item("principal", column, label, "", None, true)];
    let mut links: BTreeSet<(String, String)> = BTreeSet::new();
    let mut permissions: BTreeSet<String> = BTreeSet::new();
    for assignment in view["assignments"].as_array().into_iter().flatten() {
        let permission = assignment["permission"].as_str().unwrap_or_default();
        if permission.contains(":group:") {
            continue;
        }
        permissions.insert(permission.to_string());
        links.insert(("principal".into(), format!("permission:{permission}")));
    }
    for group in view["groups"].as_array().into_iter().flatten() {
        let name = format!(
            "{}/{}",
            group["plugin"].as_str().unwrap_or_default(),
            group["name"].as_str().unwrap_or_default()
        );
        let id = format!("group:{name}");
        let page = format!("/p/rbac/groups/{name}");
        items.push(item(&id, "Groups", &name, "", Some(&page), false));
        links.insert(("principal".into(), id.clone()));
        for permission in
            group["permissions"].as_array().into_iter().flatten().filter_map(Value::as_str)
        {
            permissions.insert(permission.to_string());
            links.insert((id.clone(), format!("permission:{permission}")));
        }
    }
    let reach: BTreeMap<String, String> = view["access"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(plugin, access)| {
            (plugin.clone(), access["scope"].as_str().unwrap_or("custom only").to_string())
        })
        .collect();
    for permission in &permissions {
        items.push(item(
            &format!("permission:{permission}"),
            "Permissions",
            permission,
            "",
            None,
            false,
        ));
        if let Some(plugin) = plugin_of(permission) {
            links.insert((format!("permission:{permission}"), format!("plugin:{plugin}")));
        }
    }
    for (plugin, scope) in &reach {
        items.push(item(
            &format!("plugin:{plugin}"),
            "Plugins",
            &format!("{plugin} ({scope})"),
            scope,
            None,
            false,
        ));
    }
    let links: Vec<Value> =
        links.iter().map(|(from, to)| json!({ "from": from, "to": to })).collect();
    let drawing = json!({
        "items": items,
        "links": links,
        "columns": [column, "Groups", "Permissions", "Plugins"],
        "description": format!("What {label} can reach"),
    });
    let (status, body) =
        backend.discovery("service-map", "POST", "draw", None, Some(drawing)).await.map_err(
            |err| Refusal::unavailable(format!("the Service Map could not be asked: {err}")),
        )?;
    if status != 200 {
        return Err(refused("the Service Map would not draw it", status, &body));
    }
    Ok(Drawn { svg: body["svg"].as_str().unwrap_or_default().to_string(), notes: Vec::new() })
}

#[cfg(test)]
mod choice_tests {
    use super::*;

    #[test]
    fn a_catalogue_resource_becomes_a_choice_however_its_kind_is_written() {
        let chosen =
            pick(&json!({ "kind": "cloud-resource", "name": "db-1", "title": "Orders DB" }))
                .expect("a choice");
        assert_eq!(chosen.value, "CloudResource:db-1");
        assert_eq!(chosen.title, "Orders DB");
        let untitled = pick(&json!({ "kind": "Service", "name": "api", "title": "api" })).unwrap();
        assert_eq!(untitled.title, "", "a title that only repeats the name is left out");
        assert!(pick(&json!({ "kind": "Planet", "name": "mars" })).is_none());
    }
}
