//! The pages. Server-rendered, so a map reads with no script at all (ADR-0017 §8); the canvas of
//! §5 is enhancement over these forms rather than a replacement for them.

use askama::Template;
use doc_plugin_sdk::{Backend, Request, Response};
use std::collections::BTreeMap;
use uuid::Uuid;

use crate::directory;
use crate::model::{
    BACKWARD, BORDERS, BOTH, COLOURS, Claim, Component, Crossing, FORWARD, OWNER_KINDS, Placement,
    RELATIONSHIPS, REPOSITORY, ROLES, Refusal, View, relationship_label, role_label,
};
use crate::ops;
use crate::store::Store;

/// One option on a form, with whether it is the chosen one already worked out: a template should
/// not be comparing types.
pub struct Choice {
    value: String,
    label: String,
    chosen: bool,
}

fn choices(allowed: &[(&str, &str)], chosen: &str) -> Vec<Choice> {
    allowed
        .iter()
        .map(|(value, label)| Choice {
            value: (*value).to_string(),
            label: (*label).to_string(),
            chosen: *value == chosen,
        })
        .collect()
}

#[derive(Default)]
pub struct Flash {
    notice: Option<String>,
    error: Option<String>,
}

impl Flash {
    fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    fn refused(refusal: &Refusal) -> Self {
        Self { notice: None, error: Some(refusal.detail.clone()) }
    }
}

/// A view as a row reads it, with its owner already resolved to words and a link.
struct ViewRow {
    id: Uuid,
    name: String,
    owner_kind: String,
    owner_label_kind: String,
    owner_name: String,
    owner_href: String,
    /// How many things are on it, which is what "in frame" now means.
    on_it: usize,
}

struct ComponentRow {
    /// How a claim and a placement name it, which is what the canvas posts when it moves.
    node: String,
    /// Where it sits in its column now, so the form can offer the row above and below.
    row: i64,
    shown: String,
    service: String,
    role: String,
    origin: String,
    href: String,
    /// Whether it is a whole service rather than a part of one: a service came from the
    /// Catalogue, so this plugin may take it off a view but never delete it.
    whole_service: bool,
    /// Where it sits on the canvas, in grid steps, and how big it was drawn.
    x: i64,
    y: i64,
    w: i64,
    h: i64,
    /// How it looks on this view, which only a view's own page has.
    dress: Dress,
}

/// How a node looks on one view, as the canvas draws it and its panel offers to change it.
#[derive(Default)]
struct Dress {
    /// Its own name, which is what it goes back to when the name on the view is cleared.
    usual: String,
    description: String,
    colour: String,
    border: String,
    /// Whether the view opens centred on it.
    primary: bool,
    colours: Vec<Choice>,
    borders: Vec<Choice>,
    /// The other views it may open into, with the one it does chosen.
    views: Vec<Choice>,
    /// Where its expand button goes, and what that view is called. Empty when it opens into
    /// nothing, or into a view the reader cannot see.
    opens_href: String,
    opens_name: String,
}

/// One thing in the palette, as its Add button needs it.
struct PaletteItem {
    node: String,
    shown: String,
    role: String,
}

struct ClaimRow {
    id: Uuid,
    /// What its menu offers instead of what it says now.
    others: Vec<Choice>,
    /// Which side of each end it leaves from and arrives at.
    from_side: String,
    to_side: String,
    /// The references, for the canvas to join the right two things.
    from_node: String,
    to_node: String,
    from: String,
    to: String,
    relationship: String,
    description: String,
    /// Whether it has an arrow at each end, and the ways its menu offers it to point.
    both: bool,
    arrows: Vec<Choice>,
    origin: String,
    /// `Ingress`, `Egress` or empty: which way it crosses the boundary (§6).
    crossing: &'static str,
}

#[derive(Template)]
#[template(path = "views.html")]
struct ViewsPage {
    flash: Flash,
    writes: bool,
    views: Vec<ViewRow>,
    components: Vec<ComponentRow>,
}

#[derive(Template)]
#[template(path = "view_form.html")]
struct ViewForm {
    flash: Flash,
    name: String,
    owner: String,
}

#[derive(Template)]
#[template(path = "view.html")]
struct ViewPage {
    flash: Flash,
    writes: bool,
    id: Uuid,
    /// What could be put on this view and is not on it yet — the palette a drop draws from.
    palette: Vec<PaletteItem>,
    view: View,
    owner_kind: String,
    owner_label_kind: String,
    owner_name: String,
    owner_href: String,
    components: Vec<ComponentRow>,
    claims: Vec<ClaimRow>,
    /// Nothing in the corner: the page says who owns it above the canvas.
    caption: Option<Caption>,
}

/// What a view drawn away from its own page says in the canvas's corner: who owns it, and its name
/// as a way to it where nothing else there links to it.
struct Caption {
    owner: String,
    name: String,
    /// Empty where something else on the page already goes there.
    href: String,
}

/// What a service's diagram is called when it is pinned to the top of the service's page.
pub const DIAGRAM: &str = "Architecture diagram";

#[derive(Template)]
#[template(path = "component_form.html")]
struct ComponentForm {
    flash: Flash,
    view_id: Uuid,
    service: String,
    name: String,
    title: String,
    description: String,
    roles: Vec<Choice>,
    /// The Catalogue's services, so the field offers what exists.
    services: Vec<Choice>,
}

#[derive(Template)]
#[template(path = "view_rename.html")]
struct ViewRename {
    flash: Flash,
    id: Uuid,
    name: String,
}

/// A view on the page of the service it is centred on. Never arranged there, so it is drawn as a
/// page that cannot save would draw it.
#[derive(Template)]
#[template(path = "service_view.html")]
struct ServiceView {
    writes: bool,
    id: Uuid,
    view: View,
    /// Pinned to the top of the page, a picture brings its own heading, which links to the view.
    pinned: bool,
    heading: &'static str,
    caption: Option<Caption>,
    components: Vec<ComponentRow>,
    claims: Vec<ClaimRow>,
    /// Any other views centred on the same service, to be linked to.
    others: Vec<Choice>,
}

fn html<T: Template>(page: &T) -> Response {
    match page.render() {
        Ok(html) => Response::html(html),
        Err(err) => Refusal::unavailable(format!("the page could not be drawn: {err}")).response(),
    }
}

/// The words for an owner's kind, and where its own page is.
fn owner_parts(owner: &str) -> (String, String, String, String) {
    let (kind, name) = owner.split_once(':').unwrap_or(("team", owner));
    let label =
        OWNER_KINDS.iter().find(|(id, _)| *id == kind).map_or("Owner", |(_, label)| match *label {
            "A team" => "Team",
            "A service" => "Service",
            _ => "Organisation",
        });
    (kind.to_string(), label.to_string(), name.to_string(), format!("/p/resources/r/{kind}/{name}"))
}

fn component_row(component: &Component) -> ComponentRow {
    placed_row(component, &Placement::default())
}

/// A row with the view's own placement on it, for the pages that can move things.
/// A component the Catalogue holds that this plugin has not declared, so its service and what
/// sort of thing it is are not known here. It still belongs on a map.
fn catalogued_row(name: &str, placed: &Placement) -> ComponentRow {
    ComponentRow {
        node: format!("component:{name}"),
        row: placed.row,
        x: placed.x,
        y: placed.y,
        w: placed.w,
        h: placed.h,
        shown: name.rsplit('/').next().unwrap_or(name).to_string(),
        service: name.split_once('/').map_or("Components", |(service, _)| service).to_string(),
        role: "Component".to_string(),
        origin: "In the Catalogue".to_string(),
        href: format!("/p/resources/r/component/{name}"),
        whole_service: true,
        dress: Dress::default(),
    }
}

/// A whole service on a view, which the Catalogue owns and this plugin only arranges.
fn service_row(service: &str, placed: &Placement) -> ComponentRow {
    ComponentRow {
        node: format!("service:{service}"),
        row: placed.row,
        x: placed.x,
        y: placed.y,
        w: placed.w,
        h: placed.h,
        shown: service.to_string(),
        service: service.to_string(),
        role: "Service".to_string(),
        origin: "In the Catalogue".to_string(),
        href: format!("/p/resources/r/service/{service}"),
        whole_service: true,
        dress: Dress::default(),
    }
}

fn placed_row(component: &Component, placed: &Placement) -> ComponentRow {
    ComponentRow {
        node: component.reference(),
        row: placed.row,
        x: placed.x,
        y: placed.y,
        w: placed.w,
        h: placed.h,
        shown: component.shown().to_string(),
        whole_service: false,
        service: component.service.clone(),
        role: role_label(&component.role).to_string(),
        origin: match component.origin.as_str() {
            REPOSITORY => "In its repository".to_string(),
            _ => "Drawn here".to_string(),
        },
        href: format!("/p/resources/r/component/{}/{}", component.service, component.name),
        dress: Dress::default(),
    }
}

/// A node on a view, whichever kind of thing it is: a whole service, a component declared here,
/// or one the Catalogue holds that this plugin has not declared.
fn row_for(placed: &Placement, all: &[Component]) -> Option<ComponentRow> {
    if let Some(service) = placed.node.strip_prefix("service:") {
        return Some(service_row(service, placed));
    }
    if let Some(component) = all.iter().find(|component| component.reference() == placed.node) {
        return Some(placed_row(component, placed));
    }
    placed.node.strip_prefix("component:").map(|name| catalogued_row(name, placed))
}

/// The row as this view has dressed it: the name it goes by here, its border, and the view it opens
/// into. `views` are those the reader may see, so a box never offers a way into one they may not.
fn dressed(mut row: ComponentRow, placed: &Placement, view: &View, views: &[View]) -> ComponentRow {
    let usual = std::mem::take(&mut row.shown);
    row.shown = match placed.label.is_empty() {
        true => usual.clone(),
        false => placed.label.clone(),
    };
    // What the panel shows as chosen when nobody has chosen: a service and a Catalogue component
    // are drawn dashed, and a component declared here solid.
    let border = match (placed.border.as_str(), row.whole_service) {
        ("", true) => "dashed",
        ("", false) => "solid",
        (chosen, _) => chosen,
    };
    let opens = placed.opens.and_then(|id| views.iter().find(|other| other.id == id));
    row.dress = Dress {
        usual,
        description: placed.description.clone(),
        colour: placed.colour.clone(),
        border: placed.border.clone(),
        primary: !view.primary.is_empty() && view.primary == row.node,
        colours: choices(&COLOURS, &placed.colour),
        borders: choices(&BORDERS, border),
        views: views
            .iter()
            .filter(|other| other.id != view.id)
            .map(|other| Choice {
                value: other.id.to_string(),
                label: other.name.clone(),
                chosen: placed.opens == Some(other.id),
            })
            .collect(),
        opens_href: opens.map(|to| format!("/p/architecture/views/{}", to.id)).unwrap_or_default(),
        opens_name: opens.map(|to| to.name.clone()).unwrap_or_default(),
    };
    row
}

/// What a reference is called on a page: a component by its own words, and anything outside the
/// context by what it is and what it is called.
fn end_label(reference: &str, components: &BTreeMap<String, Component>) -> String {
    if let Some(component) = components.get(reference) {
        return format!("{} ({})", component.shown(), component.service);
    }
    match reference.split_once(':') {
        Some(("service", name)) => format!("{name} (the whole service)"),
        Some(("account", name)) => format!("{name} — a vendor DOC proxies"),
        Some(("external", name)) => name.to_string(),
        Some(("component", name)) => name.to_string(),
        _ => reference.to_string(),
    }
}

/// The Catalogue's services as options, with those already in frame marked.
async fn service_choices(backend: &Backend, chosen: &[String]) -> Vec<Choice> {
    directory::names(backend, "Service")
        .await
        .into_iter()
        .map(|name| Choice { chosen: chosen.contains(&name), label: name.clone(), value: name })
        .collect()
}

/// The view a service is the primary component of, drawn as it opens.
///
/// Pinned, it brings its own heading — the Catalogue only knows what it is called — so the heading
/// can be the way to the view. As a part of the page, the Catalogue heads it, and the view's name
/// in the canvas's corner is the way there instead.
async fn service_diagram(
    store: &Store<'_>,
    request: &Request,
    pinned: bool,
) -> Result<Response, Refusal> {
    let centre = format!("service:{}", panel_service(request));
    let mut views: Vec<View> =
        ops::views(store).await?.into_iter().filter(|view| view.primary == centre).collect();
    if views.is_empty() {
        let heading = match pinned {
            true => format!("<h3>{DIAGRAM}</h3>"),
            false => String::new(),
        };
        return Ok(Response::html(format!(
            "{heading}<p class=\"doc-empty\">No view of the architecture is centred on this \
             service yet. A view shows here once this service is its primary component.</p>\
             <p><a href=\"/p/architecture/\">The architecture map</a></p>"
        )));
    }
    let view = views.remove(0);
    let all = store.components(None).await?;
    let (components, claims) = drawing(store, &view, &all).await?;
    let (_, _, owner_name, _) = owner_parts(&view.owner);
    Ok(html(&ServiceView {
        writes: false,
        id: view.id,
        pinned,
        heading: DIAGRAM,
        caption: Some(Caption {
            owner: match view.owner_label.is_empty() {
                true => owner_name,
                false => view.owner_label.clone(),
            },
            name: view.name.clone(),
            href: match pinned {
                true => String::new(),
                false => format!("/p/architecture/views/{}", view.id),
            },
        }),
        components,
        claims,
        others: views
            .iter()
            .map(|other| Choice {
                value: other.id.to_string(),
                label: other.name.clone(),
                chosen: false,
            })
            .collect(),
        view,
    }))
}

/// The service a Catalogue panel is for, from the `service:<name>` it is asked with.
fn panel_service(request: &Request) -> String {
    let resource = url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(key, _)| key == "resource")
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default();
    resource.split_once(':').map_or(resource.clone(), |(_, name)| name.to_string())
}

fn field(form: &BTreeMap<String, String>, key: &str) -> String {
    form.get(key).cloned().unwrap_or_default()
}

fn parsed(body: &[u8]) -> BTreeMap<String, String> {
    url::form_urlencoded::parse(body)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

async fn views_page(
    backend: &Backend,
    store: &Store<'_>,
    flash: Flash,
) -> Result<Response, Refusal> {
    let views = ops::views(store).await?;
    let mut rows = Vec::new();
    for view in &views {
        let on_it = store.placements(view.id).await?.len();
        let (kind, label, name, href) = owner_parts(&view.owner);
        rows.push(ViewRow {
            id: view.id,
            name: view.name.clone(),
            owner_kind: kind,
            owner_label_kind: label,
            owner_name: match view.owner_label.is_empty() {
                true => name,
                false => view.owner_label.clone(),
            },
            owner_href: href,
            on_it,
        });
    }
    let mut components = store.components(None).await?;
    components.sort_by(|a, b| (&a.service, &a.name).cmp(&(&b.service, &b.name)));
    Ok(html(&ViewsPage {
        flash,
        writes: backend.writes(),
        views: rows,
        components: components.iter().map(component_row).collect(),
    }))
}

/// What a view draws: the things on it, as it has dressed them, and the lines between them.
async fn drawing(
    store: &Store<'_>,
    view: &View,
    all: &[Component],
) -> Result<(Vec<ComponentRow>, Vec<ClaimRow>), Refusal> {
    let placements = store.placements(view.id).await?;
    // The views this reader can see, read as them: the ones a box may open into.
    let readable = ops::views(store).await?;
    // What is in frame is what has been put on it, in the order it was put there (§5). A list
    // declared up front would be a second answer to the same question. A placement names either a
    // whole service from the Catalogue or a component of one.
    let mut rows: Vec<ComponentRow> = placements
        .iter()
        .filter_map(|placed| Some(dressed(row_for(placed, all)?, placed, view, &readable)))
        .collect();
    rows.sort_by_key(|row| row.row);
    let by_reference: BTreeMap<String, Component> =
        all.iter().map(|component| (component.reference(), component.clone())).collect();
    let on_view: Vec<String> = rows.iter().map(|row| row.node.clone()).collect();
    let mut claims: Vec<Claim> = store
        .claims()
        .await?
        .into_iter()
        .filter(|claim| on_view.contains(&claim.from) || on_view.contains(&claim.to))
        .collect();
    claims.sort_by(|a, b| (&a.from, &a.to).cmp(&(&b.from, &b.to)));
    // A line's menu names its ends as the boxes on this view do, so a thing renamed here is not
    // called something else the moment one of its lines is clicked.
    let end = |reference: &str| match rows.iter().find(|row| row.node == reference) {
        Some(row) if row.shown != row.dress.usual => row.shown.clone(),
        _ => end_label(reference, &by_reference),
    };
    let claim_rows: Vec<ClaimRow> = claims
        .iter()
        .map(|claim| (claim, end(&claim.from), end(&claim.to)))
        .map(|(claim, from, to)| ClaimRow {
            id: claim.id,
            others: choices(&RELATIONSHIPS, &claim.relationship),
            from_side: claim.from_side.clone(),
            to_side: claim.to_side.clone(),
            from_node: claim.from.clone(),
            to_node: claim.to.clone(),
            both: claim.both,
            arrows: choices(
                &[
                    (FORWARD, &format!("{from} to {to}")),
                    (BACKWARD, &format!("{to} to {from}")),
                    (BOTH, "Both ways"),
                ],
                if claim.both { BOTH } else { FORWARD },
            ),
            from,
            to,
            crossing: Crossing::of(&claim.from, &claim.to).label(),
            relationship: relationship_label(&claim.relationship).to_string(),
            description: claim.description.clone(),
            origin: match claim.origin.as_str() {
                REPOSITORY => "In a repository".to_string(),
                _ => "Drawn here".to_string(),
            },
        })
        .collect();
    Ok((rows, claim_rows))
}

async fn view_page(
    backend: &Backend,
    store: &Store<'_>,
    id: Uuid,
    flash: Flash,
) -> Result<Response, Refusal> {
    let view = store.view(id).await?;
    let all = store.components(None).await?;
    let services = directory::names(backend, "Service").await;
    let (rows, claim_rows) = drawing(store, &view, &all).await?;
    let on_view: Vec<String> = rows.iter().map(|row| row.node.clone()).collect();
    let (owner_kind, owner_label_kind, owner_name, owner_href) = owner_parts(&view.owner);
    let mut palette: Vec<PaletteItem> = services
        .iter()
        .filter(|service| !on_view.contains(&format!("service:{service}")))
        .map(|service| PaletteItem {
            node: format!("service:{service}"),
            shown: service.clone(),
            role: "Service".to_string(),
        })
        .collect();
    // Components as the Catalogue holds them, which is every one there is: those declared here,
    // and those applied straight to the Catalogue, as DOC's own platform components are.
    for (name, title) in directory::catalogued(backend).await {
        let node = format!("component:{name}");
        if on_view.contains(&node) {
            continue;
        }
        let known = all.iter().find(|component| component.reference() == node);
        palette.push(PaletteItem {
            node,
            shown: title,
            role: known
                .map(|component| role_label(&component.role).to_string())
                .unwrap_or_else(|| "Component".to_string()),
        });
    }
    Ok(html(&ViewPage {
        flash,
        writes: backend.writes(),
        palette,
        owner_kind,
        owner_label_kind,
        owner_name: match view.owner_label.is_empty() {
            true => owner_name,
            false => view.owner_label.clone(),
        },
        owner_href,
        id: view.id,
        claims: claim_rows,
        components: rows,
        view,
        caption: None,
    }))
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let store = Store(backend);
    match answered(backend, &store, request, path).await {
        Ok(response) => response,
        Err(refusal) => refusal.response(),
    }
}

async fn answered(
    backend: &Backend,
    store: &Store<'_>,
    request: &Request,
    path: &[&str],
) -> Result<Response, Refusal> {
    let id =
        |text: &str| Uuid::parse_str(text).map_err(|_| Refusal::missing("there is no such view"));
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => views_page(backend, store, Flash::default()).await,
        // A view shows on a service's page when that service is its primary component: the one
        // it opens centred on is the one it is a picture of. As a part of the page of its own, and
        // pinned to the top of it.
        ("GET", ["panel"]) => service_diagram(store, request, false).await,
        ("GET", ["insight", "diagram"]) => service_diagram(store, request, true).await,
        ("GET", ["views", "new"]) => Ok(html(&ViewForm {
            flash: Flash::default(),
            name: String::new(),
            owner: String::new(),
        })),
        ("POST", ["views", "new"]) => {
            let form = parsed(&request.body);
            let made =
                ops::make_view(backend, store, &field(&form, "name"), &field(&form, "owner")).await;
            match made {
                Ok(view) => view_page(backend, store, view.id, Flash::done("Added it.")).await,
                Err(refusal) => Ok(html(&ViewForm {
                    flash: Flash::refused(&refusal),
                    name: field(&form, "name"),
                    owner: field(&form, "owner"),
                })),
            }
        }
        ("GET", ["views", view]) => view_page(backend, store, id(view)?, Flash::default()).await,
        ("GET", ["views", view, "rename"]) => {
            let view = store.view(id(view)?).await?;
            Ok(html(&ViewRename { flash: Flash::default(), id: view.id, name: view.name }))
        }
        ("POST", ["views", view, "rename"]) => {
            let view = id(view)?;
            let form = parsed(&request.body);
            match ops::rename_view(backend, store, view, &field(&form, "name")).await {
                Ok(_) => view_page(backend, store, view, Flash::done("Renamed it.")).await,
                Err(refusal) => Ok(html(&ViewRename {
                    flash: Flash::refused(&refusal),
                    id: view,
                    name: field(&form, "name"),
                })),
            }
        }
        ("POST", ["views", view, "remove"]) => {
            let view = id(view)?;
            match ops::remove_view(backend, store, view).await {
                Ok(()) => views_page(backend, store, Flash::done("It has gone.")).await,
                Err(refusal) => views_page(backend, store, Flash::refused(&refusal)).await,
            }
        }
        ("GET", ["views", view, "components", "new"]) => Ok(html(&ComponentForm {
            flash: Flash::default(),
            view_id: id(view)?,
            service: String::new(),
            name: String::new(),
            title: String::new(),
            description: String::new(),
            roles: choices(&ROLES, "api"),
            services: service_choices(backend, &[]).await,
        })),
        ("POST", ["views", view, "components", "new"]) => {
            let view = id(view)?;
            let form = parsed(&request.body);
            let added = ops::add_component(
                backend,
                store,
                &field(&form, "service"),
                &field(&form, "name"),
                &field(&form, "title"),
                &field(&form, "role"),
                &field(&form, "description"),
            )
            .await;
            match added {
                Ok(_) => view_page(backend, store, view, Flash::done("Added it.")).await,
                Err(refusal) => Ok(html(&ComponentForm {
                    flash: Flash::refused(&refusal),
                    view_id: view,
                    services: service_choices(backend, &[field(&form, "service")]).await,
                    service: field(&form, "service"),
                    name: field(&form, "name"),
                    title: field(&form, "title"),
                    description: field(&form, "description"),
                    roles: choices(&ROLES, &field(&form, "role")),
                })),
            }
        }
        ("POST", ["views", view, "connect"]) => {
            let view = id(view)?;
            let form = parsed(&request.body);
            let drawn = ops::connect(
                backend,
                store,
                &field(&form, "from"),
                &field(&form, "to"),
                &field(&form, "relationship"),
                &field(&form, "description"),
                (&field(&form, "from_side"), &field(&form, "to_side")),
            )
            .await;
            match drawn {
                Ok(_) => view_page(backend, store, view, Flash::done("Connected them.")).await,
                Err(refusal) => view_page(backend, store, view, Flash::refused(&refusal)).await,
            }
        }
        ("POST", ["views", view, "components", component, "remove"]) => {
            let view = id(view)?;
            let flash = match ops::remove_component(backend, store, id(component)?).await {
                Ok(gone) => Flash::done(format!("{} has gone, and its lines with it.", gone.name)),
                Err(refusal) => Flash::refused(&refusal),
            };
            view_page(backend, store, view, flash).await
        }
        ("POST", ["views", view, "claims", claim, "relationship"]) => {
            let view = id(view)?;
            let form = parsed(&request.body);
            let flash = match ops::relate(
                backend,
                store,
                id(claim)?,
                &field(&form, "relationship"),
                &field(&form, "description"),
                &field(&form, "arrows"),
            )
            .await
            {
                // The line itself shows the change, and a notice above would push the canvas down
                // under the pointer each time.
                Ok(()) => Flash::default(),
                Err(refusal) => Flash::refused(&refusal),
            };
            view_page(backend, store, view, flash).await
        }
        ("POST", ["views", view, "claims", claim, "remove"]) => {
            let view = id(view)?;
            let flash = match ops::disconnect(backend, store, id(claim)?).await {
                Ok(()) => Flash::done("That line has gone."),
                Err(refusal) => Flash::refused(&refusal),
            };
            view_page(backend, store, view, flash).await
        }
        // Where the canvas saves a move (§5). A drop does what this does, so the keyboard can
        // reach it too, and the form below works without any script at all.
        ("POST", ["views", view, "place"]) => {
            let view = id(view)?;
            let form = parsed(&request.body);
            let at = |key: &str| field(&form, key).parse::<f64>().unwrap_or_default() as i64;
            // A size comes only from a resize, so moving something never resets how big it is.
            let size = match form.contains_key("w") {
                true => Some((at("w"), at("h"))),
                false => None,
            };
            let flash = match ops::place(
                backend,
                store,
                view,
                &field(&form, "node"),
                at("x"),
                at("y"),
                size,
            )
            .await
            {
                Ok(()) => Flash::done("Moved it."),
                Err(refusal) => Flash::refused(&refusal),
            };
            view_page(backend, store, view, flash).await
        }
        // A node's panel: the name it goes by on this view, a description under it, its border, and
        // whether the view opens centred on it.
        ("POST", ["views", view, "look"]) => {
            let view = id(view)?;
            let form = parsed(&request.body);
            let node = field(&form, "node");
            // Its own name sent back unchanged is no name of the view's: kept empty, it goes on
            // following the Catalogue's when that changes.
            let all = store.components(None).await?;
            let usual = row_for(&Placement { node: node.clone(), ..Placement::default() }, &all)
                .map(|row| row.shown)
                .unwrap_or_default();
            let label = match field(&form, "label").trim() {
                same if same == usual => String::new(),
                given => given.to_string(),
            };
            let opens = match field(&form, "opens").trim() {
                "" => None,
                given => Some(id(given)?),
            };
            let look = ops::Look {
                label,
                description: field(&form, "description"),
                colour: field(&form, "colour"),
                border: field(&form, "border"),
                opens,
                primary: form.contains_key("primary"),
            };
            let flash = match ops::dress(backend, store, view, &node, look).await {
                // The box itself shows the change, and a notice above would push the canvas down
                // under the pointer each time.
                Ok(()) => Flash::default(),
                Err(refusal) => Flash::refused(&refusal),
            };
            view_page(backend, store, view, flash).await
        }
        ("POST", ["views", view, "add"]) => {
            let view = id(view)?;
            let form = parsed(&request.body);
            let at = |key: &str| field(&form, key).parse::<f64>().unwrap_or_default() as i64;
            // A drop says where; the Add button says nothing and is laid out for.
            let dropped = match form.contains_key("x") {
                true => Some((at("x"), at("y"))),
                false => None,
            };
            let flash = match ops::add_to_view(backend, store, view, &field(&form, "node"), dropped)
                .await
            {
                Ok(()) => Flash::done("Put it on the view."),
                Err(refusal) => Flash::refused(&refusal),
            };
            view_page(backend, store, view, flash).await
        }
        ("POST", ["views", view, "take-off"]) => {
            let view = id(view)?;
            let form = parsed(&request.body);
            let flash = match ops::take_off_view(backend, store, view, &field(&form, "node")).await
            {
                Ok(()) => Flash::done("Took it off the view. It is still declared."),
                Err(refusal) => Flash::refused(&refusal),
            };
            view_page(backend, store, view, flash).await
        }
        _ => Err(Refusal::missing("no such page")),
    }
}
