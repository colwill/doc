//! Building a graph from Resource Definitions, asked as the person viewing, so it holds only what
//! they may see: a breadth-first walk of `neighbours` from the focus, as deep and as wide as asked.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use doc_plugin_sdk::Backend;
use serde::Serialize;
use serde_json::Value;
use url::form_urlencoded::Serializer;

use crate::Refusal;

/// A graph of this many nodes is cut short, and says so.
pub const MAX_NODES: usize = 200;
pub const MAX_DEPTH: usize = 4;
/// Shown unless the viewer picks otherwise: everything but the long tails `rbac` answers with.
pub const DEFAULT_KINDS: [&str; 9] = [
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

#[derive(Debug, Clone, Default)]
pub struct Params {
    pub focus: Option<String>,
    pub depth: usize,
    pub kinds: BTreeSet<String>,
    pub organisation: Option<String>,
    pub team: Option<String>,
    pub expand: Vec<String>,
}

impl Params {
    pub fn from_query(query: &str) -> Result<Self, Refusal> {
        let mut params = Self { depth: 2, ..Self::default() };
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            let value = value.trim().to_string();
            if value.is_empty() {
                continue;
            }
            match key.as_ref() {
                "focus" => params.focus = Some(value),
                "organisation" | "vertical" => params.organisation = Some(value),
                "team" => params.team = Some(value),
                "expand" => params.expand.push(value),
                "kinds" => {
                    params.kinds.extend(value.split(',').map(|kind| kind.trim().to_string()))
                }
                "depth" => {
                    params.depth = value
                        .parse()
                        .ok()
                        .filter(|depth| (1..=MAX_DEPTH).contains(depth))
                        .ok_or_else(|| Refusal::bad(format!("`depth` is from 1 to {MAX_DEPTH}")))?;
                }
                _ => {}
            }
        }
        if params.kinds.is_empty() {
            params.kinds = DEFAULT_KINDS.iter().map(|kind| (*kind).to_string()).collect();
        }
        params.expand.sort();
        params.expand.dedup();
        Ok(params)
    }

    /// The query that asks for this graph again, for links and the cache key alike.
    pub fn query(&self) -> String {
        let mut query = Serializer::new(String::new());
        let optional =
            [("focus", &self.focus), ("organisation", &self.organisation), ("team", &self.team)];
        for (key, value) in optional {
            if let Some(value) = value {
                query.append_pair(key, value);
            }
        }
        query.append_pair("depth", &self.depth.to_string());
        query.append_pair("kinds", &self.kinds.iter().cloned().collect::<Vec<_>>().join(","));
        for expand in &self.expand {
            query.append_pair("expand", expand);
        }
        query.finish()
    }

    /// Where the walk starts: the focus, or else the organisation or team it is filtered to.
    fn start(&self) -> Result<String, Refusal> {
        let from_filter = || {
            self.organisation
                .as_ref()
                .map(|organisation| format!("Organisation:{organisation}"))
                .or_else(|| self.team.as_ref().map(|team| format!("Team:{team}")))
        };
        self.focus.clone().or_else(from_filter).ok_or_else(|| {
            Refusal::bad("say what the map is of: a focus, an organisation or a team")
        })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Node {
    pub id: String,
    pub kind: String,
    pub name: String,
    pub key: String,
    pub title: String,
    pub depth: usize,
    pub focus: bool,
    /// Whether its own neighbours were fetched; the rest can be expanded.
    pub opened: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub derived: bool,
}

#[derive(Debug, Default, Serialize)]
pub struct Graph {
    pub nodes: BTreeMap<String, Node>,
    pub edges: BTreeSet<Edge>,
    pub hidden: BTreeSet<String>,
    pub truncated: bool,
}

/// A node's neighbours as the viewer may see them, from Resource Definitions.
struct Neighbours {
    node: Value,
    neighbours: Vec<Value>,
    hidden: Vec<String>,
}

pub struct Walker<'a> {
    backend: &'a Backend,
    asked: HashMap<String, Neighbours>,
}

fn problem(status: u16, body: &Value) -> Refusal {
    let detail = body["detail"].as_str().map_or_else(|| body.to_string(), str::to_string);
    Refusal { status, detail }
}

impl<'a> Walker<'a> {
    pub fn new(backend: &'a Backend) -> Self {
        Self { backend, asked: HashMap::new() }
    }

    async fn neighbours(&mut self, id: &str) -> Result<&Neighbours, Refusal> {
        if !self.asked.contains_key(id) {
            let query = Serializer::new(String::new()).append_pair("of", id).finish();
            let (status, body) = self
                .backend
                .ask("resources", "GET", "neighbours", Some(&query), None)
                .await
                .map_err(|err| {
                    Refusal::unavailable(format!("Resource Definitions could not be asked: {err}"))
                })?;
            if status != 200 {
                return Err(problem(status, &body));
            }
            let found = Neighbours {
                node: body["node"].clone(),
                neighbours: body["neighbours"].as_array().cloned().unwrap_or_default(),
                hidden: body["hidden"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|reason| reason.as_str().map(str::to_string))
                    .collect(),
            };
            self.asked.insert(id.to_string(), found);
        }
        self.asked.get(id).ok_or_else(|| Refusal::unavailable("a lookup was lost"))
    }

    /// A service outside the organisation or team the map is filtered to is left out.
    async fn passes(&mut self, params: &Params, id: &str) -> Result<bool, Refusal> {
        if !id.starts_with("Service:") {
            return Ok(true);
        }
        let wanted: Vec<String> = [
            params.organisation.as_ref().map(|organisation| format!("Organisation:{organisation}")),
            params.team.as_ref().map(|team| format!("Team:{team}")),
        ]
        .into_iter()
        .flatten()
        .collect();
        if wanted.is_empty() {
            return Ok(true);
        }
        let around = self.neighbours(id).await?;
        let near: BTreeSet<String> = around.neighbours.iter().map(identity).collect();
        Ok(wanted.iter().all(|wanted| near.contains(wanted)))
    }

    /// Adds `id`'s neighbours of the chosen kinds, and returns the ones new to the graph.
    async fn open(
        &mut self,
        graph: &mut Graph,
        params: &Params,
        id: &str,
        depth: usize,
    ) -> Result<Vec<String>, Refusal> {
        if let Some(node) = graph.nodes.get_mut(id) {
            node.opened = true;
        }
        let around = match self.neighbours(id).await {
            Ok(around) => around,
            // What lies past a node the viewer may not open is left out, and the map says why.
            Err(refusal) if matches!(refusal.status, 403 | 404) && depth > 0 => {
                graph.hidden.insert(refusal.detail);
                return Ok(Vec::new());
            }
            Err(refusal) => return Err(refusal),
        };
        let (neighbours, hidden) = (around.neighbours.clone(), around.hidden.clone());
        graph.hidden.extend(hidden);
        let mut fresh = Vec::new();
        for neighbour in &neighbours {
            let kind = neighbour["kind"].as_str().unwrap_or_default().to_string();
            if !params.kinds.contains(&kind) {
                continue;
            }
            let other = identity(neighbour);
            if !graph.nodes.contains_key(&other) {
                if graph.nodes.len() >= MAX_NODES {
                    graph.truncated = true;
                    continue;
                }
                if !self.passes(params, &other).await? {
                    continue;
                }
                graph.nodes.insert(other.clone(), node_of(neighbour, depth + 1, false));
                fresh.push(other.clone());
            }
            let derived = neighbour["derived"] == true;
            let (from, to) = match neighbour["direction"].as_str() {
                Some("in") => (other.clone(), id.to_string()),
                _ => (id.to_string(), other.clone()),
            };
            graph.edges.insert(Edge { from, to, derived });
        }
        Ok(fresh)
    }

    pub async fn walk(&mut self, params: &Params) -> Result<Graph, Refusal> {
        let start = params.start()?;
        let root = self.neighbours(&start).await?.node.clone();
        let mut graph = Graph::default();
        let root_id = identity(&root);
        graph.nodes.insert(root_id.clone(), node_of(&root, 0, params.focus.is_some()));
        let mut frontier = vec![root_id];
        for depth in 0..params.depth {
            let mut next = Vec::new();
            for id in frontier {
                next.extend(self.open(&mut graph, params, &id, depth).await?);
            }
            frontier = next;
        }
        for id in &params.expand {
            if let Some(depth) =
                graph.nodes.get(id).filter(|node| !node.opened).map(|node| node.depth)
            {
                self.open(&mut graph, params, id, depth).await?;
            }
        }
        Ok(graph)
    }
}

/// `Kind:key`, which names a node without doubt: a user by `provider/login`.
pub fn identity(node: &Value) -> String {
    format!("{}:{}", node["kind"].as_str().unwrap_or_default(), key(node))
}

fn key(node: &Value) -> &str {
    node["key"]
        .as_str()
        .filter(|key| !key.is_empty())
        .or_else(|| node["name"].as_str())
        .unwrap_or_default()
}

fn node_of(value: &Value, depth: usize, focus: bool) -> Node {
    Node {
        id: identity(value),
        kind: value["kind"].as_str().unwrap_or_default().to_string(),
        name: value["name"].as_str().unwrap_or_default().to_string(),
        key: key(value).to_string(),
        title: value["title"].as_str().unwrap_or_default().to_string(),
        depth,
        focus,
        opened: false,
    }
}
