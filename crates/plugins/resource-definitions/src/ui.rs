//! The plugin's pages, shown by the frontend at `/p/resources/...`. Only someone who can write to
//! `resources` sees the editing controls; everyone else reads.

use std::collections::BTreeMap;
use std::time::Duration;

use askama::Template;
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Map, Value, json};
use url::form_urlencoded::Serializer;

use crate::api::{queries, query};
use crate::apply::{self, Mode};
use crate::definitions::{Definition, connectable, unconnectable};
use crate::graph::{Graph, Level};
use crate::kinds::{self, ALL, Kind, Ref};
use crate::model::{Neighbour, Node, Refusal, Resource};
use crate::ops;
use crate::rbac::Rbac;
use crate::store::Store;

mod dashboard;
mod team;

const LISTED: i64 = 200;
const APPLY_EXAMPLE: &str = "kind: Service
name: my-service
title: My service
description: What it does, in a sentence.
owner: my-team
connections:
  Organisation: payments
  Teams: [my-team]
";

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    fn refused(refusal: &Refusal) -> Self {
        Self { notice: None, error: Some(refusal.detail.clone()) }
    }
}

type Form = Vec<(String, String)>;

fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> String {
    form.iter().find(|(key, _)| key == name).map(|(_, value)| value.clone()).unwrap_or_default()
}

/// Every value sent under one name, such as the form's connection fields.
fn many(form: &Form, name: &str) -> Vec<String> {
    form.iter().filter(|(key, _)| key == name).map(|(_, value)| value.clone()).collect()
}

/// A page's link to a resource, whatever its kind.
pub fn href(kind: Kind, name: &str) -> String {
    format!("/p/resources/r/{}/{name}", kind.slug())
}

fn encoded(pairs: &[(&str, &str)]) -> String {
    let mut query = Serializer::new(String::new());
    for (key, value) in pairs {
        query.append_pair(key, value);
    }
    query.finish()
}

/// Joins what follows the kind in a route back into a name, which may hold `/`.
fn named(slug: &str, rest: &[&str]) -> Result<Ref, Refusal> {
    if rest.is_empty() {
        return Err(Refusal::missing("name a resource"));
    }
    Ok(Ref::new(kinds::kind(slug)?, crate::api::decoded(&rest.join("/"))))
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let store = Store(backend);
    let form = form(request);
    // Where the address bar goes when a form lands somewhere other than where it was.
    let mut pushed: Option<String> = None;
    let page = match (request.method.as_str(), path) {
        ("GET", [] | [""]) => home(backend, &store, Flash::default()).await,
        ("GET", ["search"]) => search(&store, request).await,
        ("GET", ["dashboard"]) => dashboard::services(backend, &store).await,
        // The options behind a resource picker, wherever a plugin asks somebody to name one.
        ("GET", ["options"]) => options(&store, request).await,
        ("GET", ["kinds", slug]) => kind_page(backend, &store, slug, request).await,
        ("GET", ["kinds", slug, "rows"]) => rows(backend, &store, slug, request).await,
        ("GET", ["r", slug, rest @ ..]) => match named(slug, rest) {
            Ok(at) if at.kind == Kind::Team => {
                let tab = team::Tab::named(query(request, "tab").as_deref());
                team::page(backend, &store, &at.name, tab, Flash::default()).await
            }
            Ok(at) => {
                let section = query(request, "section").unwrap_or_default();
                resource_at(backend, &store, &at, &section, Flash::default()).await
            }
            Err(refusal) => Err(refusal),
        },
        ("GET", ["connect", slug, rest @ ..]) => match named(slug, rest) {
            Ok(at) => connect_page(&at, String::new(), Flash::default()),
            Err(refusal) => Err(refusal),
        },
        ("POST", ["delete", slug, rest @ ..]) => delete(backend, &store, slug, rest).await,
        ("POST", ["disconnect", slug, rest @ ..]) => {
            disconnect(backend, &store, slug, rest, &form).await
        }
        ("GET", ["explore"]) => explore(backend, &store, request).await,
        ("GET", ["explore", "level"]) => level(&store, request).await,
        ("GET", ["apply"]) => apply_page(backend, &store, request).await,
        ("POST", ["apply"]) => applied(backend, &store, &form).await,
        // The same thing as a form, for anyone who would rather fill one in than write YAML.
        ("GET", ["new"]) => {
            new_page(backend, query(request, "kind"), Fields::default(), Flash::default())
        }
        ("POST", ["new"]) => added(backend, &store, &form).await,
        // The same form, filled in with what is there: YAML is still a way to change a resource,
        // not the way.
        // Editing has parts of its own: what the resource says, and what is pinned to the top of
        // its page.
        ("GET", ["edit", slug, rest @ ..]) => match query(request, "section").as_deref() {
            Some("pinned") => pinning(backend, &store, slug, rest).await,
            _ => editing(backend, &store, slug, rest).await,
        },
        ("POST", ["edit", slug, rest @ ..]) => match named(slug, rest) {
            Ok(at) => changed(backend, &store, at, &form).await,
            Err(refusal) => Err(refusal),
        },
        ("POST", ["connect", slug, rest @ ..]) => {
            let (page, url) = connect(backend, &store, slug, rest, &form).await;
            pushed = url;
            page
        }
        ("GET", ["pinned", slug, rest @ ..]) => pinning(backend, &store, slug, rest).await,
        ("POST", ["pinned", slug, rest @ ..]) => pinned(backend, &store, slug, rest, &form).await,
        _ => Err(Refusal::missing("no such page")),
    };
    match page {
        Ok(html) => match pushed {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(&refusal) };
            let html = page.render().unwrap_or_else(|_| refusal.detail.clone());
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

struct KindCount {
    slug: &'static str,
    plural: &'static str,
    from: &'static str,
    count: Option<i64>,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    flash: Flash,
    writes: bool,
    kinds: Vec<KindCount>,
    definitions: Vec<(String, String, String)>,
}

fn explore_url(definition: &str, path: &[String]) -> String {
    let mut pairs = vec![("definition", definition)];
    pairs.extend(path.iter().map(|at| ("path", at.as_str())));
    format!("/p/resources/explore?{}", encoded(&pairs))
}

async fn home(backend: &Backend, store: &Store<'_>, flash: Flash) -> Result<String, Refusal> {
    let counts = store.counts().await?;
    let kinds = ALL
        .into_iter()
        .filter(|kind| !matches!(kind, Kind::Attribute | Kind::Permission))
        .map(|kind| KindCount {
            slug: kind.slug(),
            plural: kind.plural(),
            from: match kind {
                kind if kind.stored() => {
                    kind.publishers().first().copied().unwrap_or("this catalogue")
                }
                kind if kind.in_core() => "core",
                _ => "rbac",
            },
            count: counts.get(&kind).copied().or(kind.stored().then_some(0)),
        })
        .collect();
    let definitions = store
        .definitions()
        .await?
        .iter()
        .map(|definition| {
            let kinds: Vec<&str> = definition.kinds.iter().map(|kind| kind.plural()).collect();
            (definition.name.clone(), kinds.join(" → "), explore_url(&definition.name, &[]))
        })
        .collect();
    render(&HomePage { flash, writes: backend.writes(), kinds, definitions })
}

struct Found {
    kind: Kind,
    name: String,
    title: String,
    href: String,
}

impl Found {
    fn of(node: &Node) -> Self {
        Self {
            kind: node.kind,
            name: node.name.clone(),
            title: node.title.clone(),
            href: href(node.kind, &node.key),
        }
    }
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchFragment {
    text: String,
    found: Vec<Found>,
}

async fn search(store: &Store<'_>, request: &Request) -> Result<String, Refusal> {
    let text = query(request, "q").unwrap_or_default();
    let found = match text.is_empty() {
        true => Vec::new(),
        false => {
            let kinds: Vec<Kind> =
                ALL.into_iter().filter(|kind| kind.written() || *kind == Kind::User).collect();
            store.search(&text, &kinds, 20).await?.iter().map(Found::of).collect()
        }
    };
    render(&SearchFragment { text, found })
}

/// One choice in a resource picker: what it is called, and the `kind:name` a field wants.
struct Option_ {
    value: String,
    kind: String,
    name: String,
    title: String,
}

#[derive(Template)]
#[template(path = "options.html")]
struct OptionsFragment {
    text: String,
    found: Vec<Option_>,
    /// There are more than are shown, so typing narrows it.
    more: bool,
}

/// The picker's options: what matches what has been typed, or a few of everything to start from.
/// `kinds` narrows it to the kinds a field takes, such as `Service,Team`.
async fn options(store: &Store<'_>, request: &Request) -> Result<String, Refusal> {
    const SHOWN: usize = 20;
    // A picker sends the field's own value, since a form field carries its own name; `field` says
    // which one it is, and `q` is taken as well for anything asking directly.
    let named = query(request, "field").unwrap_or_else(|| "q".to_string());
    let text = query(request, "q")
        .or_else(|| query(request, &named))
        .unwrap_or_default()
        .trim()
        .to_string();
    let wanted: Vec<String> = query(request, "kinds")
        .unwrap_or_default()
        .split(',')
        .map(|kind| kind.trim().to_string())
        .filter(|kind| !kind.is_empty())
        .collect();
    // A field that tags one resource to another says what it is tagging from, as `for` or as the
    // form's own `kind`, and is offered everything a connection definition allows next to it —
    // roles included, which is how a team comes to hold one.
    let tagging = query(request, "for").or_else(|| query(request, "kind"));
    let tagging = tagging.as_deref().and_then(|kind| kinds::kind(kind).ok());
    let kinds: Vec<Kind> = match (wanted.is_empty(), tagging) {
        (false, _) => wanted.iter().filter_map(|kind| kinds::kind(kind).ok()).collect(),
        (true, Some(from)) => {
            let definitions = store.definitions().await?;
            ALL.into_iter().filter(|kind| connectable(&definitions, from, *kind)).collect()
        }
        (true, None) => ALL.into_iter().filter(|kind| kind.written()).collect(),
    };
    let limit = i64::try_from(SHOWN).unwrap_or(20) + 1;
    let mut found = store.search(&text, &kinds, limit).await?;
    // Roles are `rbac`'s, so they are asked for as the viewer rather than searched here.
    if kinds.contains(&Kind::Role) {
        let mut roles: Vec<Node> = Rbac::new(store.0)
            .roles()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|role| crate::store::closeness(&text, &role.name, "", Kind::Role).is_some())
            .collect();
        roles.sort_by(|a, b| a.name.cmp(&b.name));
        found.extend(roles);
        found.truncate(limit.max(0) as usize);
    }
    let more = found.len() > SHOWN;
    // A field that takes a bare name, such as an owning team, asks for `values=name`.
    let bare = query(request, "values").as_deref() == Some("name");
    let found = found
        .iter()
        .take(SHOWN)
        .map(|node| Option_ {
            value: match bare {
                true => node.name.clone(),
                false => format!("{}:{}", node.kind.name(), node.name),
            },
            kind: node.kind.name().to_string(),
            name: node.name.clone(),
            title: node.title.clone(),
        })
        .collect();
    render(&OptionsFragment { text, found, more })
}

struct Row {
    name: String,
    key: String,
    title: String,
    href: String,
    owner: Option<String>,
    source: String,
}

#[derive(Template)]
#[template(path = "kind.html")]
struct KindPage {
    flash: Flash,
    writes: bool,
    kind: Kind,
    listed: bool,
    rows: Vec<Row>,
    hidden: Option<String>,
}

#[derive(Template)]
#[template(path = "rows.html")]
struct RowsFragment {
    kind: Kind,
    rows: Vec<Row>,
    hidden: Option<String>,
}

/// A kind's resources matching `text`, or why the viewer may not see them.
async fn listed(
    backend: &Backend,
    store: &Store<'_>,
    kind: Kind,
    text: Option<&str>,
) -> Result<(Vec<Row>, Option<String>), Refusal> {
    let of = |node: &Node| Row {
        name: node.name.clone(),
        key: node.key.clone(),
        title: node.title.clone(),
        href: href(node.kind, &node.key),
        owner: None,
        source: String::new(),
    };
    match kind {
        kind if kind.written() => {
            let resources = store.resources(kind, text, LISTED, 0).await?;
            let rows = resources
                .iter()
                .map(|resource| Row {
                    name: resource.name.clone(),
                    key: resource.name.clone(),
                    title: resource.title.clone(),
                    href: href(kind, &resource.name),
                    owner: resource.owner.clone(),
                    source: resource.source.clone(),
                })
                .collect();
            Ok((rows, None))
        }
        Kind::User | Kind::ServiceAccount => {
            Ok((store.principals(kind, text, LISTED, 0).await?.iter().map(of).collect(), None))
        }
        Kind::Role => match Rbac::new(backend).roles().await {
            Ok(roles) => {
                let text = text.map(str::to_lowercase);
                let rows = roles
                    .iter()
                    .filter(|role| text.as_deref().is_none_or(|text| role.name.contains(text)))
                    .map(of)
                    .collect();
                Ok((rows, None))
            }
            Err(reason) => Ok((Vec::new(), Some(reason))),
        },
        _ => Ok((Vec::new(), None)),
    }
}

async fn kind_page(
    backend: &Backend,
    store: &Store<'_>,
    slug: &str,
    request: &Request,
) -> Result<String, Refusal> {
    let kind = kinds::kind(slug)?;
    let text = query(request, "q").unwrap_or_default();
    let listed_here = !matches!(kind, Kind::Attribute | Kind::Permission);
    let filter = Some(text.as_str()).filter(|text| !text.is_empty());
    let (rows, hidden) = listed(backend, store, kind, filter).await?;
    render(&KindPage {
        flash: Flash::default(),
        writes: backend.writes(),
        kind,
        listed: listed_here,
        rows,
        hidden,
    })
}

async fn rows(
    backend: &Backend,
    store: &Store<'_>,
    slug: &str,
    request: &Request,
) -> Result<String, Refusal> {
    let kind = kinds::kind(slug)?;
    let text = query(request, "q").filter(|text| !text.is_empty());
    let (rows, hidden) = listed(backend, store, kind, text.as_deref()).await?;
    render(&RowsFragment { kind, rows, hidden })
}

/// What a resource is related to that is of one kind, under that kind's heading.
struct Related {
    heading: &'static str,
    rows: Vec<Linked>,
}

struct Linked {
    kind: Kind,
    name: String,
    key: String,
    title: String,
    href: String,
    derived: bool,
    /// Where a derived link came from: `rbac`, or `owner` for what a team owns.
    via: String,
}

struct Panel {
    /// What the contents beside the page links to: the plugin's name, with a number after it for
    /// a second panel from the same plugin, so no two parts of the page answer to one link.
    key: String,
    plugin: String,
    label: String,
    url: String,
    /// Where the plugin asked for it among the rest; equal ones keep the order of their plugins.
    order: i64,
}

impl Panel {
    /// What the contents beside the page links to, and the card answers to.
    fn id(&self) -> String {
        self.key.clone()
    }
}

/// One row of the editor: an insight the viewer could pin, and whether it is.
struct PinRow {
    insight: Pinnable,
    pinned: bool,
}

/// One entry in the contents beside a resource: a part of the page, by the name it is headed with.
struct Section {
    id: String,
    label: String,
}

/// An insight a plugin offers about this kind of resource, and whether it is pinned here.
#[derive(Clone)]
struct Pinnable {
    /// `plugin:insight`, which is how a pin names it.
    id: String,
    plugin: String,
    label: String,
    description: String,
    /// Where its tile is fetched from, already carrying the resource.
    url: String,
    /// A picture rather than a figure, drawn across the page below the row of figures.
    wide: bool,
}

/// One part of editing a resource, for the contents beside it.
struct EditPart {
    label: String,
    href: String,
    current: bool,
}

/// The parts of editing a resource: what it says, and what is pinned to the top of its page.
fn edit_parts(at: &Ref, current: &str) -> Vec<EditPart> {
    let here = format!("/p/resources/edit/{}/{}", at.kind.slug(), at.name);
    vec![
        EditPart { label: "Details".into(), href: here.clone(), current: current == "details" },
        EditPart {
            label: "Pinned".into(),
            href: format!("{here}?section=pinned"),
            current: current == "pinned",
        },
    ]
}

/// One line of metadata: what it is, what it says, and where it goes when it names somewhere.
struct Shown {
    key: String,
    value: String,
    href: Option<String>,
}

#[derive(Template)]
#[template(path = "pinned.html")]
struct PinningPage {
    flash: Flash,
    contents: Vec<EditPart>,
    kind: Kind,
    name: String,
    title: String,
    rows: Vec<PinRow>,
}

impl PinningPage {
    /// Where a row sits, counted from one: the order is changed by dragging, and this is what the
    /// platform's `doc-reorder` renumbers as it goes.
    fn order(index: &usize) -> usize {
        index + 1
    }
}

#[derive(Template)]
#[template(path = "resource.html")]
struct ResourcePage {
    flash: Flash,
    writes: bool,
    kind: Kind,
    name: String,
    title: String,
    resource: Option<Resource>,
    metadata: Vec<Shown>,
    /// What it is related to, a group per kind in the kinds' order. Which of two related things
    /// the definitions put higher is left out: it is the order to drill through kinds in, and
    /// says nothing a reader needs here, such as which of them owns the other.
    related: Vec<Related>,
    hidden: Vec<String>,
    panels: Vec<Panel>,
    /// The figures pinned to the top of this page, in the order somebody put them in, and below
    /// them the pictures, in theirs.
    pinned: Vec<Pinnable>,
    pictures: Vec<Pinnable>,
    /// The parts of the page, for the contents beside it, each a page of its own.
    sections: Vec<Section>,
    /// The part open: `details`, `connections` or a panel's ID.
    section: String,
    /// The resource's own page, which each part is a query on.
    here: String,
    edit_url: String,
    owner_href: Option<String>,
    /// Where the platform keeps it, for a kind core keeps rather than the catalogue (T69).
    platform: Option<String>,
}

impl ResourcePage {
    fn action(&self, what: &str) -> String {
        format!("/p/resources/{what}/{}/{}", self.kind.slug(), self.name)
    }

    /// What the page calls the resource, as its heading does: its title, or its name without one.
    fn called(&self) -> &str {
        if self.title.is_empty() { &self.name } else { &self.title }
    }

    fn part(&self, id: &str) -> String {
        match id {
            "details" => self.here.clone(),
            id => format!("{}?{}", self.here, encoded(&[("section", id)])),
        }
    }
}

/// Connecting a resource to another, on a page of its own, with what was typed when refused.
#[derive(Template)]
#[template(path = "connect.html")]
struct ConnectPage {
    flash: Flash,
    kind: Kind,
    name: String,
    here: String,
    typed: String,
}

impl ConnectPage {
    fn action(&self, what: &str) -> String {
        format!("/p/resources/{what}/{}/{}", self.kind.slug(), self.name)
    }
}

fn connect_page(at: &Ref, typed: String, flash: Flash) -> Result<String, Refusal> {
    render(&ConnectPage {
        flash,
        kind: at.kind,
        name: at.name.clone(),
        here: href(at.kind, &at.name),
        typed,
    })
}

fn shown_value(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// Metadata that names somewhere is shown as a link to it: a repository's `url`, a page's own
/// address in the Knowledge Base. Only the web and this platform's own paths, so nothing stored
/// as metadata can put a `javascript:` link on a page.
fn linked_value(value: &str) -> Option<String> {
    let web = value.starts_with("https://") || value.starts_with("http://");
    let here = value.starts_with('/') && !value.starts_with("//");
    (web || here).then(|| value.to_string())
}

/// What other plugins add to this kind's pages — whole panels, and the single facts that may be
/// pinned above them — from those the viewer can read. Both come from the same one answer, so a
/// page costs one call however many plugins have something to say.
async fn offered(backend: &Backend, kind: Kind, at: &Ref) -> (Vec<Panel>, Vec<Pinnable>) {
    let access = backend.request("core.access", "access", json!({}), Duration::from_secs(10)).await;
    let Ok(access) = access else { return (Vec::new(), Vec::new()) };
    let resource = encoded(&[("resource", &at.to_string())]);
    let (mut panels, mut pinnable) = (Vec::new(), Vec::new());
    let url = |plugin: &str, path: Option<&str>| {
        let path = path.unwrap_or("/");
        format!("/p/{plugin}/{}?{resource}", path.strip_prefix('/').unwrap_or(path))
    };
    for (plugin, reach) in access["plugins"].as_object().into_iter().flatten() {
        let mut nth = 0;
        for panel in reach["panels"].as_array().into_iter().flatten() {
            if panel["resource"].as_str() != Some(kind.slug()) {
                continue;
            }
            nth += 1;
            panels.push(Panel {
                key: match nth {
                    1 => format!("panel-{plugin}"),
                    n => format!("panel-{plugin}-{n}"),
                },
                plugin: plugin.clone(),
                label: panel["label"].as_str().unwrap_or(plugin).to_string(),
                url: url(plugin, panel["path"].as_str()),
                order: panel["order"].as_i64().unwrap_or_default(),
            });
        }
        for insight in reach["insights"].as_array().into_iter().flatten() {
            let (Some(id), Some(label)) = (insight["id"].as_str(), insight["label"].as_str())
            else {
                continue;
            };
            if insight["resource"].as_str() != Some(kind.slug()) {
                continue;
            }
            pinnable.push(Pinnable {
                id: format!("{plugin}:{id}"),
                plugin: plugin.clone(),
                label: label.to_string(),
                description: insight["description"].as_str().unwrap_or_default().to_string(),
                url: url(plugin, insight["path"].as_str()),
                wide: insight["wide"].as_bool().unwrap_or_default(),
            });
        }
    }
    panels.sort_by_key(|panel| panel.order);
    pinnable.sort_by(|a, b| (&a.label, &a.id).cmp(&(&b.label, &b.id)));
    (panels, pinnable)
}

async fn resource(
    backend: &Backend,
    store: &Store<'_>,
    at: &Ref,
    flash: Flash,
) -> Result<String, Refusal> {
    resource_at(backend, store, at, "details", flash).await
}

async fn resource_at(
    backend: &Backend,
    store: &Store<'_>,
    at: &Ref,
    section: &str,
    flash: Flash,
) -> Result<String, Refusal> {
    let graph = Graph::new(store).await?;
    let node = graph.node(at).await?;
    let stored = match at.kind.written() {
        true => store.resource(at.kind, &at.name).await?,
        false => None,
    };
    let (neighbours, hidden) = graph.neighbours(&node).await?;
    let linked = |neighbour: &Neighbour| Linked {
        kind: neighbour.kind,
        name: neighbour.name.clone(),
        key: neighbour.key.clone(),
        title: neighbour.title.clone(),
        href: href(neighbour.kind, &neighbour.key),
        derived: neighbour.derived,
        via: neighbour.via.to_string(),
    };
    let mut rows: Vec<Linked> = neighbours.iter().map(linked).collect();
    rows.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
    let mut related: Vec<Related> = Vec::new();
    for row in rows {
        match related.last_mut() {
            Some(group) if group.rows[0].kind == row.kind => group.rows.push(row),
            _ => related.push(Related { heading: row.kind.heading(), rows: vec![row] }),
        }
    }
    let metadata: Vec<Shown> = stored
        .as_ref()
        .map(|resource| {
            resource
                .metadata
                .iter()
                .map(|(key, value)| {
                    let value = shown_value(value);
                    Shown { key: key.clone(), href: linked_value(&value), value }
                })
                .collect()
        })
        .unwrap_or_default();
    let owner_href = stored
        .as_ref()
        .and_then(|resource| resource.owner.as_deref())
        .map(|owner| href(Kind::Team, owner));
    // An organisation is the platform's: it is changed here or there, and deleted only there.
    let platform = stored
        .as_ref()
        .filter(|_| at.kind == Kind::Organisation)
        .map(|resource| format!("/organisations/{}", resource.id));
    let (panels, offered) = offered(backend, at.kind, at).await;
    // What is pinned is read in the order somebody put it in; anything pinned whose plugin has
    // since gone quiet is simply not there to draw, and the rest keeps its places.
    let chosen = store.pinned(&at.to_string()).await.unwrap_or_default();
    // Figures sit in a row at the top, and pictures go across the page below them: never above,
    // whatever order they were pinned in, because a picture above the row pushes it out of sight.
    let (pictures, pinned): (Vec<Pinnable>, Vec<Pinnable>) = chosen
        .iter()
        .filter_map(|id| offered.iter().find(|insight| insight.id == *id))
        .map(Pinnable::clone)
        .partition(|insight| insight.wide);
    let sections = sections(&panels);
    let section = match sections.iter().any(|known| known.id == section) {
        true => section.to_string(),
        false => "details".to_string(),
    };
    render(&ResourcePage {
        section,
        here: href(at.kind, &at.name),
        flash,
        writes: backend.writes() && at.kind.written(),
        kind: at.kind,
        name: node.name.clone(),
        title: node.title.clone(),
        resource: stored,
        metadata,
        related,
        hidden,
        panels,
        pinned,
        pictures,
        sections,
        edit_url: format!("/p/resources/apply?{}", encoded(&[("from", &at.to_string())])),
        owner_href,
        platform,
    })
}

/// The parts of a resource's page, for the contents beside it: its details, what it is related
/// to, and each plugin's panel, each a page of its own.
fn sections(panels: &[Panel]) -> Vec<Section> {
    let mut sections = vec![
        Section { id: "details".into(), label: "Details".into() },
        Section { id: "connections".into(), label: "Related".into() },
    ];
    for panel in panels {
        sections.push(Section { id: panel.id(), label: panel.label.clone() });
    }
    sections
}

/// The **Pinned** part of editing a resource: what goes at the top of its page, and in what order.
///
/// Every insight the viewer could pin is a row — the pinned ones first, in their order, then the
/// rest — dragged into place by its handle or moved with the arrow keys, and ticked to pin it.
/// The order fields are renumbered by the platform's `doc-reorder`, so a browser with no script
/// still chooses what is pinned; only the dragging needs one.
async fn pinning(
    backend: &Backend,
    store: &Store<'_>,
    slug: &str,
    rest: &[&str],
) -> Result<String, Refusal> {
    let at = named(slug, rest)?;
    let (_, offered) = offered(backend, at.kind, &at).await;
    let chosen = store.pinned(&at.to_string()).await?;
    let mut rows: Vec<PinRow> = Vec::new();
    for id in &chosen {
        if let Some(insight) = offered.iter().find(|insight| insight.id == *id) {
            rows.push(PinRow { insight: insight.clone(), pinned: true });
        }
    }
    for insight in offered {
        if !chosen.contains(&insight.id) {
            rows.push(PinRow { insight, pinned: false });
        }
    }
    // In the order the page shows them: the pinned figures, then the pinned pictures that go below
    // them, then what is not pinned.
    rows.sort_by_key(|row| (!row.pinned, row.insight.wide));
    let node = Graph::new(store).await?.node(&at).await?;
    render(&PinningPage {
        flash: Flash::default(),
        contents: edit_parts(&at, "pinned"),
        kind: at.kind,
        name: node.name.clone(),
        title: node.title.clone(),
        rows,
    })
}

/// Saves the row: what is ticked, in the order the form was left in.
async fn pinned(
    backend: &Backend,
    store: &Store<'_>,
    slug: &str,
    rest: &[&str],
    form: &Form,
) -> Result<String, Refusal> {
    let at = named(slug, rest)?;
    let result = async {
        let wanted = wanted_pins(form)?;
        let by = ops::actor(backend);
        store.set_pinned(&at.to_string(), &wanted, &by).await?;
        let detail = json!({ "pinned": wanted, "by": by });
        ops::record(backend, store, "pinned", &at.to_string(), detail).await;
        Ok::<_, Refusal>(match wanted.len() {
            0 => "Nothing is pinned to the top of this page now.".to_string(),
            1 => "One insight is pinned to the top of this page.".to_string(),
            many => format!("{many} insights are pinned to the top of this page."),
        })
    }
    .await;
    let flash = match result {
        Ok(said) => Flash::done(said),
        Err(refusal) => Flash::refused(&refusal),
    };
    resource(backend, store, &at, flash).await
}

/// The insights ticked, in the order asked for: `insight.N` names each row, `pin.N` says it is
/// ticked, and `order.N` is where it was left. The same shape the landing page's rows use.
fn wanted_pins(form: &Form) -> Result<Vec<String>, Refusal> {
    let field = |name: &str, at: usize| {
        let key = format!("{name}.{at}");
        form.iter().find(|(field, _)| *field == key).map(|(_, value)| value.trim())
    };
    let mut picked: Vec<(usize, usize, String)> = Vec::new();
    let indices =
        form.iter().filter_map(|(name, _)| name.strip_prefix("insight.")?.parse::<usize>().ok());
    for at in indices {
        let insight = field("insight", at).unwrap_or_default().to_string();
        if insight.is_empty() || field("pin", at).is_none() {
            continue;
        }
        let order = match field("order", at).unwrap_or_default() {
            "" => usize::MAX,
            text => text
                .parse()
                .map_err(|_| Refusal::bad(format!("the order of {insight} is not a number")))?,
        };
        picked.push((order, at, insight));
    }
    picked.sort_by_key(|(order, at, _)| (*order, *at));
    Ok(picked.into_iter().map(|(_, _, insight)| insight).collect())
}

async fn delete(
    backend: &Backend,
    store: &Store<'_>,
    slug: &str,
    rest: &[&str],
) -> Result<String, Refusal> {
    let at = named(slug, rest)?;
    if !at.kind.stored() {
        return Err(Refusal::bad(format!("{} are not kept here", at.kind.plural())));
    }
    let resource = store
        .resource(at.kind, &at.name)
        .await?
        .ok_or_else(|| Refusal::missing(format!("there is no {at}")))?;
    store.delete(&resource).await?;
    let detail = json!({ "by": ops::actor(backend), "id": resource.id });
    ops::record(backend, store, "deleted", &at.to_string(), detail).await;
    home(backend, store, Flash::done(format!("Deleted {at}. Nothing is related to it any more.")))
        .await
}

async fn disconnect(
    backend: &Backend,
    store: &Store<'_>,
    slug: &str,
    rest: &[&str],
    form: &Form,
) -> Result<String, Refusal> {
    let at = named(slug, rest)?;
    let other = Ref::new(kinds::kind(&field(form, "kind"))?, field(form, "name"));
    let result = async {
        let graph = Graph::new(store).await?;
        let (a, b) = (graph.node(&at).await?, graph.node(&other).await?);
        for node in [&a, &b] {
            if node.kind == Kind::ServiceAccount {
                ops::may_assign(backend, store, node).await?;
            }
        }
        if !store.disconnect(&a, &b).await? {
            return Err(Refusal::missing(format!("{at} and {other} are not related")));
        }
        let detail = json!({ "from": other.to_string(), "by": ops::actor(backend) });
        ops::record(backend, store, "disconnected", &at.to_string(), detail).await;
        Ok(())
    }
    .await;
    let flash = match result {
        Ok(()) => Flash::done(format!("{other} is no longer related to it.")),
        Err(refusal) => Flash::refused(&refusal),
    };
    resource_at(backend, store, &at, "connections", flash).await
}

struct Start {
    label: String,
    href: String,
    level_url: String,
    target: String,
}

#[derive(Template)]
#[template(path = "explore.html")]
struct ExplorePage {
    flash: Flash,
    definitions: Vec<(String, bool)>,
    chosen: Option<Definition>,
    first: Option<Kind>,
    next: Option<Kind>,
    starts: Vec<Start>,
    hidden: Option<String>,
}

/// A stable element ID for a place in the tree, so each level loads under its own node.
fn target(path: &[String]) -> String {
    let joined = path.join("\u{1f}");
    let hash = joined.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    });
    format!("x-{hash:016x}")
}

fn level_url(definition: &str, path: &[String]) -> String {
    let mut pairs = vec![("definition", definition)];
    pairs.extend(path.iter().map(|at| ("path", at.as_str())));
    format!("/p/resources/explore/level?{}", encoded(&pairs))
}

async fn explore(
    backend: &Backend,
    store: &Store<'_>,
    request: &Request,
) -> Result<String, Refusal> {
    let all = store.definitions().await?;
    let wanted = query(request, "definition");
    let chosen = match &wanted {
        Some(name) => Some(store.definition(name).await?),
        None => all.first().cloned(),
    };
    let definitions = all
        .iter()
        .map(|definition| {
            let picked = chosen.as_ref().is_some_and(|chosen| chosen.name == definition.name);
            (definition.name.clone(), picked)
        })
        .collect();
    let (mut starts, mut hidden, mut first, mut next) = (Vec::new(), None, None, None);
    if let Some(definition) = &chosen {
        first = definition.kinds.first().copied();
        next = definition.kinds.get(1).copied();
        if let Some(kind) = first {
            let (rows, reason) = listed(backend, store, kind, None).await?;
            hidden = reason;
            starts = rows
                .iter()
                .map(|row| {
                    let path = vec![Ref::new(kind, row.key.clone()).to_string()];
                    let label = match row.title.is_empty() {
                        true => row.name.clone(),
                        false => format!("{} ({})", row.title, row.name),
                    };
                    Start {
                        label,
                        href: row.href.clone(),
                        level_url: level_url(&definition.name, &path),
                        target: target(&path),
                    }
                })
                .collect();
        }
    }
    render(&ExplorePage {
        flash: Flash::default(),
        definitions,
        chosen,
        first,
        next,
        starts,
        hidden,
    })
}

struct Branch {
    kind: Kind,
    label: String,
    href: String,
    derived: bool,
    level_url: Option<String>,
    target: String,
}

#[derive(Template)]
#[template(path = "level.html")]
struct LevelFragment {
    kind: Option<Kind>,
    next: Option<Kind>,
    branches: Vec<Branch>,
    hidden: Option<String>,
}

async fn level(store: &Store<'_>, request: &Request) -> Result<String, Refusal> {
    let name = query(request, "definition").ok_or_else(|| Refusal::bad("name a definition"))?;
    let definition = store.definition(&name).await?;
    let asked = queries(request, "path");
    let path: Vec<Ref> = asked.iter().map(|text| Ref::parse(text)).collect::<Result<_, _>>()?;
    let kinds: Vec<Kind> = path.iter().map(|at| at.kind).collect();
    if path.is_empty() || !definition.follows(&kinds) {
        return Err(Refusal::bad(format!("the path does not follow {definition}")));
    }
    let graph = Graph::new(store).await?;
    let mut nodes = Vec::new();
    for at in &path {
        nodes.push(graph.node(at).await?);
    }
    let Level { kind, children, hidden } = graph.expand(&definition, &nodes, 1).await?;
    let next = kind
        .and_then(|kind| definition.position(kind))
        .and_then(|index| definition.kinds.get(index + 1))
        .copied();
    let branches = children
        .iter()
        .map(|child| {
            let mut below = asked.clone();
            below.push(Ref::new(child.kind, child.key.clone()).to_string());
            let label = match child.title.is_empty() {
                true => child.name.clone(),
                false => format!("{} ({})", child.title, child.name),
            };
            Branch {
                kind: child.kind,
                label,
                href: href(child.kind, &child.key),
                derived: child.derived,
                level_url: next.map(|_| level_url(&definition.name, &below)),
                target: target(&below),
            }
        })
        .collect();
    render(&LevelFragment { kind, next, branches, hidden })
}

#[derive(Template)]
#[template(path = "apply.html")]
struct ApplyPage {
    flash: Flash,
    writes: bool,
    documents: String,
    report: Option<Report>,
}

struct Change {
    action: String,
    what: String,
    detail: String,
}

struct Report {
    dry_run: bool,
    changes: Vec<Change>,
    unchanged: usize,
}

/// What a stored resource would be applied as, so editing starts from what is there.
async fn as_yaml(store: &Store<'_>, at: &Ref) -> Result<String, Refusal> {
    let resource = store
        .resource(at.kind, &at.name)
        .await?
        .ok_or_else(|| Refusal::missing(format!("there is no {at}")))?;
    let mut document = Map::new();
    document.insert("kind".into(), json!(resource.kind));
    document.insert("name".into(), json!(resource.name));
    for (key, value) in [("title", &resource.title), ("description", &resource.description)] {
        if !value.is_empty() {
            document.insert(key.into(), json!(value));
        }
    }
    for (key, value) in [("owner", &resource.owner), ("email", &resource.email)] {
        if let Some(value) = value {
            document.insert(key.into(), json!(value));
        }
    }
    if !resource.metadata.is_empty() {
        document.insert("metadata".into(), Value::Object(resource.metadata.clone()));
    }
    let node = Node {
        kind: resource.kind,
        reference: resource.id.to_string(),
        name: resource.name.clone(),
        title: String::new(),
        key: resource.name.clone(),
    };
    // Only what lies below is written, so a connection is declared once — by its parent — and the
    // YAML applies back as it was read. Which end that is comes from the definitions, not from
    // where the two kinds happen to be drawn.
    let definitions = store.definitions().await?;
    let mut connections: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    for link in store.links(&node, None).await? {
        let below = crate::definitions::above(&definitions, resource.kind, link.kind)
            .unwrap_or(link.kind > resource.kind);
        if below {
            connections.entry(link.kind.plural()).or_default().push(link.key);
        }
    }
    if !connections.is_empty() {
        document.insert("connections".into(), json!(connections));
    }
    serde_yaml_ng::to_string(&document).map_err(|err| {
        Refusal::unavailable(format!("the resource could not be written as YAML: {err}"))
    })
}

async fn apply_page(
    backend: &Backend,
    store: &Store<'_>,
    request: &Request,
) -> Result<String, Refusal> {
    let documents = match query(request, "from") {
        Some(from) => as_yaml(store, &Ref::parse(&from)?).await?,
        None => APPLY_EXAMPLE.to_string(),
    };
    render(&ApplyPage {
        flash: Flash::default(),
        writes: backend.writes(),
        documents,
        report: None,
    })
}

fn report_of(plan_report: &Value) -> Report {
    let changes = plan_report["changes"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|change| {
            let action = change["action"].as_str().unwrap_or_default().to_string();
            let what = change["resource"]
                .as_str()
                .or_else(|| change["definition"].as_str())
                .unwrap_or_default()
                .to_string();
            let detail = match (change["to"].as_str(), change["fields"].as_array()) {
                (Some(to), _) => format!("to {to}"),
                (None, Some(fields)) => {
                    let fields: Vec<&str> = fields.iter().filter_map(Value::as_str).collect();
                    fields.join(", ")
                }
                _ => String::new(),
            };
            Change { action, what, detail }
        })
        .collect();
    Report {
        dry_run: plan_report["dry_run"] == true,
        changes,
        unchanged: usize::try_from(plan_report["unchanged"].as_u64().unwrap_or_default())
            .unwrap_or_default(),
    }
}

/// What the new-resource form holds, so a refused one comes back with what was typed still in it.
#[derive(Default, Clone)]
pub struct Fields {
    pub kind: String,
    pub name: String,
    pub title: String,
    pub description: String,
    pub owner: String,
    pub email: String,
    pub metadata: String,
    pub connections: Vec<String>,
}

impl Fields {
    fn of(form: &Form) -> Self {
        Self {
            kind: field(form, "kind"),
            name: field(form, "name"),
            title: field(form, "title"),
            description: field(form, "description"),
            owner: field(form, "owner"),
            email: field(form, "email"),
            metadata: field(form, "metadata"),
            connections: many(form, "connection"),
        }
    }

    /// A connection for every one that was chosen, and an empty one to choose in.
    fn slots(&self) -> Vec<String> {
        let mut slots: Vec<String> =
            self.connections.iter().filter(|value| !value.is_empty()).cloned().collect();
        slots.extend(std::iter::repeat_n(String::new(), CONNECTION_SLOTS));
        slots
    }
}

/// How many empty connection fields a fresh form offers.
const CONNECTION_SLOTS: usize = 3;

#[derive(Template)]
#[template(path = "new.html")]
struct NewPage {
    flash: Flash,
    writes: bool,
    /// Where the form posts when it is changing a resource rather than making one.
    editing: Option<String>,
    /// Where the same resource is edited as YAML, for whoever would rather.
    yaml: Option<String>,
    /// The parts of editing it, when it is being edited rather than made.
    contents: Vec<EditPart>,
    kinds: Vec<Choice>,
    fields: Fields,
    slots: Vec<String>,
}

struct Choice {
    value: String,
    label: String,
    selected: bool,
}

fn new_page(
    backend: &Backend,
    wanted: Option<String>,
    mut fields: Fields,
    flash: Flash,
) -> Result<String, Refusal> {
    if fields.kind.is_empty() {
        fields.kind = wanted.unwrap_or_else(|| Kind::Service.name().to_string());
    }
    // What the catalogue writes: its own kinds, and the platform's organisations and teams.
    // People, roles and permissions are made where they are kept.
    let kinds = ALL
        .into_iter()
        .filter(|kind| kind.written())
        .map(|kind| Choice {
            value: kind.name().to_string(),
            label: kind.name().to_string(),
            selected: kind.name() == fields.kind,
        })
        .collect();
    let slots = fields.slots();
    render(&NewPage {
        flash,
        writes: backend.writes(),
        editing: None,
        yaml: None,
        contents: Vec::new(),
        kinds,
        fields,
        slots,
    })
}

/// `key=value` a line at a time, as the metadata box takes them.
fn pairs(text: &str) -> Result<Map<String, Value>, Refusal> {
    let mut pairs = Map::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| Refusal::bad(format!("write metadata as key=value, not `{line}`")))?;
        let (key, value) = (key.trim(), value.trim());
        if key.is_empty() {
            return Err(Refusal::bad("a metadata key cannot be empty"));
        }
        pairs.insert(key.to_string(), Value::String(value.to_string()));
    }
    Ok(pairs)
}

/// The form as one document, which is then applied exactly as a pasted one is: the same plan, the
/// same checks, the same audit entry.
fn document(fields: &Fields) -> Result<Value, Refusal> {
    let kind = kinds::kind(&fields.kind)?;
    if fields.name.trim().is_empty() {
        return Err(Refusal::bad("give it a name"));
    }
    let mut document = Map::new();
    document.insert("kind".into(), Value::String(kind.name().to_string()));
    document.insert("name".into(), Value::String(fields.name.trim().to_string()));
    for (key, value) in [
        ("title", &fields.title),
        ("description", &fields.description),
        ("owner", &fields.owner),
        ("email", &fields.email),
    ] {
        if !value.trim().is_empty() {
            document.insert(key.into(), Value::String(value.trim().to_string()));
        }
    }
    let metadata = pairs(&fields.metadata)?;
    if !metadata.is_empty() {
        document.insert("metadata".into(), Value::Object(metadata));
    }
    // A connection is named after what it points at, so `Repository:acme/doc` goes under
    // `Repositories` and the one organisation under `Organisation`.
    let mut connections: Map<String, Value> = Map::new();
    for chosen in fields.connections.iter().filter(|value| !value.trim().is_empty()) {
        let at = Ref::parse(chosen.trim())?;
        let group = match at.kind == Kind::Organisation {
            true => at.kind.name().to_string(),
            false => at.kind.plural().to_string(),
        };
        match connections.get_mut(&group) {
            Some(Value::Array(names)) => names.push(Value::String(at.name.clone())),
            Some(one) => *one = Value::String(at.name.clone()),
            None if at.kind == Kind::Organisation => {
                connections.insert(group, Value::String(at.name.clone()));
            }
            None => {
                connections.insert(group, Value::Array(vec![Value::String(at.name.clone())]));
            }
        }
    }
    if !connections.is_empty() {
        document.insert("connections".into(), Value::Object(connections));
    }
    Ok(Value::Object(document))
}

/// A resource as the form holds it: what is stored, and what it is connected to now.
async fn kept(store: &Store<'_>, graph: &Graph<'_>, at: &Ref) -> Result<Fields, Refusal> {
    let resource = store
        .resource(at.kind, &at.name)
        .await?
        .ok_or_else(|| Refusal::missing(format!("there is no {at}")))?;
    let node = graph.node(at).await?;
    let (neighbours, _) = graph.neighbours(&node).await?;
    Ok(Fields {
        kind: at.kind.name().to_string(),
        name: resource.name,
        title: resource.title,
        description: resource.description,
        owner: resource.owner.unwrap_or_default(),
        email: resource.email.unwrap_or_default(),
        metadata: resource
            .metadata
            .iter()
            .map(|(key, value)| format!("{key}={}", shown_value(value)))
            .collect::<Vec<_>>()
            .join("\n"),
        // What somebody drew, which is what they can change here; what is derived from ownership
        // or read from rbac is not a connection to edit.
        connections: neighbours
            .iter()
            .filter(|neighbour| {
                // What somebody drew is what they can change here; permissions and attributes are
                // rbac's own and are not tags.
                !neighbour.derived && !matches!(neighbour.kind, Kind::Permission | Kind::Attribute)
            })
            .map(|neighbour| format!("{}:{}", neighbour.kind.name(), neighbour.key))
            .collect(),
    })
}

/// The form for a resource that exists, filled in with what it says now.
async fn editing(
    backend: &Backend,
    store: &Store<'_>,
    slug: &str,
    rest: &[&str],
) -> Result<String, Refusal> {
    let at = named(slug, rest)?;
    let graph = Graph::new(store).await?;
    let fields = kept(store, &graph, &at).await?;
    edit_page(backend, &at, fields, Flash::default())
}

fn edit_page(backend: &Backend, at: &Ref, fields: Fields, flash: Flash) -> Result<String, Refusal> {
    let kinds = vec![Choice {
        value: at.kind.name().to_string(),
        label: at.kind.name().to_string(),
        selected: true,
    }];
    let slots = fields.slots();
    render(&NewPage {
        flash,
        writes: backend.writes(),
        editing: Some(format!("/p/resources/edit/{}/{}", at.kind.slug(), at.name)),
        yaml: Some(format!("/p/resources/apply?{}", encoded(&[("from", &at.to_string())]))),
        contents: edit_parts(at, "details"),
        kinds,
        fields,
        slots,
    })
}

/// Changes a resource from the form, by applying what it now says.
async fn changed(
    backend: &Backend,
    store: &Store<'_>,
    at: Ref,
    form: &Form,
) -> Result<String, Refusal> {
    let mut fields = Fields::of(form);
    // The kind and the name are what it is known by, so the form cannot move it to another.
    fields.kind = at.kind.name().to_string();
    fields.name = at.name.clone();
    let result = async {
        let document = document(&fields)?;
        let parsed = apply::documents(&document.to_string())?;
        let rbac = Rbac::new(backend);
        let plan = apply::plan(backend, store, parsed, &Mode::Apply { rbac: &rbac }).await?;
        if plan.changed() > 0 {
            let by = ops::actor(backend);
            apply::execute(store, &plan, &by).await?;
            let detail = json!({ "by": by, "changes": plan.changes });
            ops::record(backend, store, "applied", "catalogue", detail).await;
        }
        Ok::<_, Refusal>(plan.changed())
    }
    .await;
    match result {
        Ok(changed) => {
            let notice = match changed {
                0 => format!("Nothing changed: {at} already says this."),
                _ => format!("{at} is changed."),
            };
            resource(backend, store, &at, Flash::done(notice)).await
        }
        Err(refusal) => edit_page(backend, &at, fields, Flash::refused(&refusal)),
    }
}

/// Makes what the form describes, then shows its page.
async fn added(backend: &Backend, store: &Store<'_>, form: &Form) -> Result<String, Refusal> {
    let fields = Fields::of(form);
    let result = async {
        let document = document(&fields)?;
        let parsed = apply::documents(&document.to_string())?;
        let rbac = Rbac::new(backend);
        let plan = apply::plan(backend, store, parsed, &Mode::Apply { rbac: &rbac }).await?;
        if plan.changed() > 0 {
            let by = ops::actor(backend);
            apply::execute(store, &plan, &by).await?;
            let detail = json!({ "by": by, "changes": plan.changes });
            ops::record(backend, store, "applied", "catalogue", detail).await;
        }
        Ok::<_, Refusal>(())
    }
    .await;
    match result {
        Ok(()) => {
            let at = Ref { kind: kinds::kind(&fields.kind)?, name: fields.name.trim().to_string() };
            let notice = format!("{at} is in the catalogue.");
            resource(backend, store, &at, Flash::done(notice)).await
        }
        Err(refusal) => {
            let flash = Flash::refused(&refusal);
            new_page(backend, None, fields, flash)
        }
    }
}

/// Connects a resource to another, chosen from the picker on its page.
async fn connect(
    backend: &Backend,
    store: &Store<'_>,
    slug: &str,
    rest: &[&str],
    form: &Form,
) -> (Result<String, Refusal>, Option<String>) {
    let at = match named(slug, rest) {
        Ok(at) => at,
        Err(refusal) => return (Err(refusal), None),
    };
    let result = async {
        let chosen = field(form, "connection");
        let to = Ref::parse(chosen.trim())?;
        let graph = Graph::new(store).await?;
        if let Some(reason) = unconnectable(&graph.definitions, at.kind, to.kind) {
            return Err(Refusal::bad(reason));
        }
        let (from, to) = (graph.node(&at).await?, graph.node(&to).await?);
        let by = ops::actor(backend);
        let added = store.connect(&from, &to, "api", &by).await?;
        if added {
            let detail = json!({ "to": format!("{}:{}", to.kind.name(), to.name), "by": by });
            ops::record(backend, store, "connected", &at.to_string(), detail).await;
        }
        Ok::<_, Refusal>(format!("{} and {} are related now.", from.name, to.name))
    }
    .await;
    match result {
        Ok(notice) => {
            let page = resource_at(backend, store, &at, "connections", Flash::done(notice)).await;
            let here = href(at.kind, &at.name);
            (page, Some(format!("{here}?{}", encoded(&[("section", "connections")]))))
        }
        Err(refusal) => {
            (connect_page(&at, field(form, "connection"), Flash::refused(&refusal)), None)
        }
    }
}

async fn applied(backend: &Backend, store: &Store<'_>, form: &Form) -> Result<String, Refusal> {
    let documents = field(form, "documents");
    let dry_run = field(form, "dry_run") == "yes";
    let result = async {
        let parsed = apply::documents(&documents)?;
        let rbac = Rbac::new(backend);
        let plan = apply::plan(backend, store, parsed, &Mode::Apply { rbac: &rbac }).await?;
        if !dry_run && plan.changed() > 0 {
            let by = ops::actor(backend);
            apply::execute(store, &plan, &by).await?;
            let detail = json!({ "by": by, "changes": plan.changes });
            ops::record(backend, store, "applied", "catalogue", detail).await;
        }
        Ok::<_, Refusal>(plan.report(dry_run))
    }
    .await;
    let (flash, report) = match result {
        Ok(report) => {
            let changed = report["changed"].as_u64().unwrap_or_default();
            let changes = match changed {
                1 => "1 change".to_string(),
                n => format!("{n} changes"),
            };
            let notice = match (dry_run, changed) {
                (true, 0) => "Nothing would change.".to_string(),
                (true, _) => {
                    format!("Applying this would make {changes}. Nothing has changed yet.")
                }
                (false, 0) => "Nothing changed: the catalogue already says this.".to_string(),
                (false, _) => format!("Applied {changes}."),
            };
            (Flash::done(notice), Some(report_of(&report)))
        }
        Err(refusal) => (Flash::refused(&refusal), None),
    };
    render(&ApplyPage { flash, writes: backend.writes(), documents, report })
}
