//! A map as the viewer may see it: the walk, the layout and the drawing, kept in the Cache Bus under
//! the viewer's access and the catalogue's version, so an unchanged map is not walked again.

use std::collections::BTreeMap;
use std::time::Duration;

use doc_plugin_sdk::Backend;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Refusal;
use crate::graph::{Params, Walker};
use crate::layout::{self, Item, Layout, Link};
use crate::svg;

/// Roles and memberships come from `rbac`, which has no version, so a map is kept only this long.
const KEPT_FOR: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Map {
    pub layout: Layout,
    pub svg: String,
    pub hidden: Vec<String>,
    pub truncated: bool,
    /// Nodes whose own connections are not drawn yet, as ID and label.
    pub frontier: Vec<(String, String)>,
    pub version: i64,
}

fn fnv(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// The kinds the catalogue holds, as column headings, in the order it ranks them.
fn column_order(overview: &Value) -> Vec<String> {
    let mut ranked: Vec<(u64, String)> = overview["kinds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|kind| {
            let plural = kind["plural"].as_str().or_else(|| kind["kind"].as_str())?;
            Some((kind["rank"].as_u64().unwrap_or(u64::MAX), plural.to_string()))
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, plural)| plural).collect()
}

/// `ServiceAccount` as a URL names it: `service-account`.
pub fn slug(kind: &str) -> String {
    let mut slug = String::new();
    for (index, c) in kind.chars().enumerate() {
        if c.is_ascii_uppercase() && index > 0 {
            slug.push('-');
        }
        slug.push(c.to_ascii_lowercase());
    }
    slug
}

pub fn plural(kind: &str) -> String {
    match kind {
        "Documentation" => kind.to_string(),
        kind if kind.ends_with('y') => format!("{}ies", &kind[..kind.len() - 1]),
        kind => format!("{kind}s"),
    }
}

async fn ask(backend: &Backend, route: &str) -> Result<Value, Refusal> {
    let (status, body) =
        backend.ask("resources", "GET", route, None, None).await.map_err(|err| {
            Refusal::unavailable(format!("Resource Definitions could not be asked: {err}"))
        })?;
    match status {
        200 => Ok(body),
        status => Err(Refusal {
            status,
            detail: body["detail"].as_str().map_or_else(|| body.to_string(), str::to_string),
        }),
    }
}

/// What the cache key must tell apart: whatever changes what the viewer may see.
async fn access_key(backend: &Backend) -> Result<u64, Refusal> {
    let access = backend
        .request("core.access", "access", json!({}), Duration::from_secs(10))
        .await
        .map_err(|err| Refusal::unavailable(format!("core could not say who is viewing: {err}")))?;
    let seen = json!({
        "admin": access["admin"],
        "resources": access["plugins"]["resources"],
        "rbac": access["plugins"]["rbac"],
    });
    Ok(fnv(&seen.to_string()))
}

pub async fn build(backend: &Backend, params: &Params) -> Result<Map, Refusal> {
    let version = ask(backend, "version").await?["version"].as_i64().unwrap_or_default();
    let key =
        format!("map:{version}:{:016x}:{:016x}", access_key(backend).await?, fnv(&params.query()));
    if let Ok(Some(cached)) = backend.cache_get(&key).await
        && let Ok(map) = serde_json::from_value::<Map>(cached)
    {
        return Ok(map);
    }
    // The columns are the catalogue's own order of kinds, which it declares (`rank`) rather than
    // leaving to be worked out from whichever connections happen to exist. A kind it does not
    // name still draws: `lay_out` puts a column it was not told about after the ones it was.
    let order = column_order(&ask(backend, "kinds").await?);
    let graph = Walker::new(backend).walk(params).await?;
    let mut named: BTreeMap<(&str, &str), usize> = BTreeMap::new();
    for node in graph.nodes.values() {
        *named.entry((node.kind.as_str(), node.name.as_str())).or_default() += 1;
    }
    let items: Vec<Item> = graph
        .nodes
        .values()
        .map(|node| Item {
            id: node.id.clone(),
            column: plural(&node.kind),
            label: match named.get(&(node.kind.as_str(), node.name.as_str())) {
                Some(1) => node.name.clone(),
                _ => node.key.clone(),
            },
            note: node.title.clone(),
            href: Some(format!("/p/resources/r/{}/{}", slug(&node.kind), node.key)),
            focus: node.focus,
            order: None,
            ghost: false,
        })
        .collect();
    let links: Vec<Link> = graph
        .edges
        .iter()
        .map(|edge| Link { from: edge.from.clone(), to: edge.to.clone(), derived: edge.derived })
        .collect();
    let laid = layout::lay_out(&items, links, &order);
    let about = params
        .focus
        .clone()
        .or_else(|| params.organisation.clone())
        .or_else(|| params.team.clone());
    let description = format!("A map of {}", about.unwrap_or_default());
    let frontier = graph
        .nodes
        .values()
        .filter(|node| !node.opened)
        .map(|node| (node.id.clone(), node.id.replacen(':', ": ", 1)))
        .collect();
    let map = Map {
        svg: svg::draw(&laid, &description),
        layout: laid,
        hidden: graph.hidden.into_iter().collect(),
        truncated: graph.truncated,
        frontier,
        version,
    };
    if let Ok(value) = serde_json::to_value(&map)
        && let Err(err) = backend.cache_set(&key, value, Some(KEPT_FOR)).await
    {
        tracing::warn!(%err, "a map was drawn but not cached");
    }
    Ok(map)
}
