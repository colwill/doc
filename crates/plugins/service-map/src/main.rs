//! Service Map (T40): maps of services and what surrounds them, built from Resource Definitions as
//! the person viewing, laid out one column per kind and drawn as SVG. Other plugins can have their
//! own graphs laid out and drawn too.

mod graph;
mod layout;
mod map;
mod svg;

use askama::Template;
use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, Manifest, Nav, Plugin, PluginError, Request, ResourcePanel, Response,
    RunInput, RunOutput,
};
use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};
use url::form_urlencoded::Serializer;

use graph::{DEFAULT_KINDS, MAX_DEPTH, Params};
use layout::{Item, Link};

/// The most a drawing asked for by another plugin may hold.
const MAX_DRAWN: usize = 500;
/// Frontier nodes offered as "go further" buttons, so a wide map keeps a short list.
const MAX_EXPANDERS: usize = 40;
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub detail: String,
}

impl Refusal {
    pub fn bad(detail: impl Into<String>) -> Self {
        Self { status: 400, detail: detail.into() }
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self { status: 503, detail: detail.into() }
    }

    fn response(&self) -> Response {
        let kind = match self.status {
            400 => "bad-request",
            403 => "forbidden",
            404 => "not-found",
            _ => "unavailable",
        };
        Response::problem(self.status, kind, &self.detail)
    }
}

#[derive(Template)]
#[template(path = "map.html")]
struct MapFragment {
    error: Option<String>,
    svg: String,
    hidden: Vec<String>,
    truncated: bool,
    expanders: Vec<(String, String)>,
    more: usize,
    full: Option<String>,
}

struct Choice {
    value: String,
    label: String,
    chosen: bool,
}

#[derive(Template)]
#[template(path = "page.html")]
struct MapPage {
    /// What a map can be of, grouped by kind.
    groups: Vec<(&'static str, Vec<Choice>)>,
    team_filters: Vec<Choice>,
    organisation_filters: Vec<Choice>,
    depths: Vec<(usize, bool)>,
    kinds: Vec<(String, bool)>,
    /// How many kinds are shown, so the folded list says so without being opened.
    shown_kinds: usize,
    /// The map fragment, drawn on its own since HTMX also asks for it alone.
    map: String,
}

fn html<T: Template>(page: &T) -> Response {
    match page.render() {
        Ok(html) => Response::html(html),
        Err(err) => Refusal::unavailable(format!("the page could not be drawn: {err}")).response(),
    }
}

fn fragment(
    params: &Params,
    built: Result<map::Map, Refusal>,
    full: Option<String>,
) -> MapFragment {
    match built {
        Ok(map) => {
            let expanders: Vec<(String, String)> = map
                .frontier
                .iter()
                .take(MAX_EXPANDERS)
                .map(|(id, label)| {
                    let mut further = params.clone();
                    further.expand.push(id.clone());
                    (label.clone(), format!("/p/service-map/map?{}", further.query()))
                })
                .collect();
            MapFragment {
                error: None,
                svg: map.svg,
                hidden: map.hidden,
                truncated: map.truncated,
                more: map.frontier.len().saturating_sub(expanders.len()),
                expanders,
                full,
            }
        }
        Err(refusal) => MapFragment {
            error: Some(refusal.detail),
            svg: String::new(),
            hidden: Vec::new(),
            truncated: false,
            expanders: Vec::new(),
            more: 0,
            full: None,
        },
    }
}

/// A kind's names, for the pickers, as the viewer may see them.
/// Every kind the catalogue holds, in the order it declares them, so the filters beside the map
/// read the same way round as its columns do.
async fn every_kind(backend: &Backend) -> Vec<String> {
    let Ok((200, overview)) = backend.ask("resources", "GET", "kinds", None, None).await else {
        return Vec::new();
    };
    let mut ranked: Vec<(u64, String)> = overview["kinds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|kind| {
            let name = kind["kind"].as_str()?;
            Some((kind["rank"].as_u64().unwrap_or(u64::MAX), name.to_string()))
        })
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, kind)| kind).collect()
}

async fn names(backend: &Backend, kind: &str) -> Vec<String> {
    let query = Serializer::new(String::new())
        .append_pair("kind", kind)
        .append_pair("limit", "500")
        .finish();
    match backend.ask("resources", "GET", "resources", Some(&query), None).await {
        Ok((200, listed)) => listed
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|resource| resource["name"].as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

fn choices(kind: &str, names: &[String], chosen: Option<&str>, prefixed: bool) -> Vec<Choice> {
    names
        .iter()
        .map(|name| {
            let value = if prefixed { format!("{kind}:{name}") } else { name.clone() };
            Choice { chosen: chosen == Some(value.as_str()), value, label: name.clone() }
        })
        .collect()
}

async fn page(backend: &Backend, request: &Request) -> Response {
    let mut params = match Params::from_query(&request.query) {
        Ok(params) => params,
        Err(refusal) => return refusal.response(),
    };
    let (services, teams, organisations, kinds) = (
        names(backend, "Service").await,
        names(backend, "Team").await,
        names(backend, "Organisation").await,
        every_kind(backend).await,
    );
    if params.focus.is_none() && params.organisation.is_none() && params.team.is_none() {
        params.focus = services.first().map(|service| format!("Service:{service}"));
    }
    let map = match params.focus.is_some() || params.organisation.is_some() || params.team.is_some()
    {
        true => fragment(&params, map::build(backend, &params).await, None),
        false => fragment(&params, Err(Refusal::bad("There are no services to map yet.")), None),
    };
    let focus = params.focus.as_deref();
    html(&MapPage {
        groups: vec![
            ("Services", choices("Service", &services, focus, true)),
            ("Teams", choices("Team", &teams, focus, true)),
            ("Organisations", choices("Organisation", &organisations, focus, true)),
        ],
        team_filters: choices("Team", &teams, params.team.as_deref(), false),
        organisation_filters: choices(
            "Organisation",
            &organisations,
            params.organisation.as_deref(),
            false,
        ),
        depths: (1..=MAX_DEPTH).map(|depth| (depth, depth == params.depth)).collect(),
        shown_kinds: kinds.iter().filter(|kind| params.kinds.contains(*kind)).count(),
        kinds: kinds.iter().map(|kind| (kind.clone(), params.kinds.contains(kind))).collect(),
        map: map.render().unwrap_or_default(),
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Drawing {
    items: Vec<Item>,
    #[serde(default)]
    links: Vec<Link>,
    /// Column labels in the order to draw them; others follow in the order they first appear.
    #[serde(default)]
    columns: Vec<String>,
    /// What to write over a column, when it is not the column's own name: a chain of columns may
    /// all be headed the same.
    #[serde(default)]
    headings: BTreeMap<String, String>,
    #[serde(default)]
    description: String,
}

/// Another plugin's own graph, laid out and drawn; it holds only what that plugin sent.
fn draw(request: &Request) -> Response {
    let drawing: Drawing = match request.json() {
        Ok(drawing) => drawing,
        Err(err) => return Refusal::bad(format!("the body is not a drawing: {err}")).response(),
    };
    if drawing.items.len() > MAX_DRAWN || drawing.links.len() > MAX_DRAWN * 4 {
        return Refusal::bad(format!("a drawing holds at most {MAX_DRAWN} nodes")).response();
    }
    let mut laid = layout::lay_out(&drawing.items, drawing.links, &drawing.columns);
    for column in &mut laid.columns {
        if let Some(heading) = drawing.headings.get(&column.label) {
            column.label = heading.clone();
        }
    }
    let svg = svg::draw(&laid, &drawing.description);
    Response::json(&json!({ "svg": svg, "layout": laid }))
}

#[derive(Default)]
struct ServiceMap;

#[async_trait]
impl Plugin for ServiceMap {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), "service-map loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, _backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        Err(PluginError::from("service-map has nothing to run: ask it for a map"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let path = request.path.trim_end_matches('/');
        match (request.method.as_str(), path) {
            ("GET", "ui") => page(backend, &request).await,
            ("GET", "ui/map") => match Params::from_query(&request.query) {
                Ok(params) => html(&fragment(&params, map::build(backend, &params).await, None)),
                Err(refusal) => refusal.response(),
            },
            ("GET", "ui/panel") => {
                let resource = url::form_urlencoded::parse(request.query.as_bytes())
                    .find(|(key, _)| key == "resource")
                    .map(|(_, value)| value.into_owned());
                let params = Params {
                    focus: resource,
                    depth: 1,
                    kinds: DEFAULT_KINDS.iter().map(|kind| (*kind).to_string()).collect(),
                    ..Params::default()
                };
                let full =
                    format!("/p/service-map/?{}", Params { depth: 2, ..params.clone() }.query());
                html(&fragment(&params, map::build(backend, &params).await, Some(full)))
            }
            ("GET", "api/graph") => match Params::from_query(&request.query) {
                Ok(params) => match map::build(backend, &params).await {
                    Ok(map) => Response::json(&map),
                    Err(refusal) => refusal.response(),
                },
                Err(refusal) => refusal.response(),
            },
            ("POST", "discovery/draw") => draw(&request),
            _ => Refusal { status: 404, detail: "no such route".into() }.response(),
        }
    }
}

doc_plugin_sdk::main!(
    ServiceMap,
    Manifest {
        id: "service-map".into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("Service Map", "/")
                .described("See how services depend on each other and what surrounds them")
                .grouped("Technology")
        ],
        // A service's map, and a team's: what it owns, and what those reach.
        resource_panels: ["service", "team"]
            .into_iter()
            .map(|resource| ResourcePanel::new(resource, "Map", "/panel"))
            .collect(),
        ..Manifest::default()
    }
);
