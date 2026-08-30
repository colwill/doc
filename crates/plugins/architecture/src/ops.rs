//! What can be done, however it is asked for. Who may do it is checked once, here, so a page and
//! the API cannot disagree about it.

use doc_plugin_sdk::Backend;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::model::{
    BACKWARD, BORDERS, BOTH, COLOURS, Claim, Component, DRAWN, FORWARD, OWNER_KINDS, RELATIONSHIPS,
    ROLES, Refusal, View, inside, side,
};
use crate::store::{CLAIMS, COMPONENTS, PLACEMENTS, Store, VIEWS};
use crate::{directory, sync};

/// A name as a reference may carry it: lowercase, and nothing that would need escaping in a URL.
pub fn named(name: &str) -> Result<String, Refusal> {
    let name = name.trim().to_ascii_lowercase();
    let fine = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-';
    match !name.is_empty()
        && name.len() <= 200
        && name.chars().all(fine)
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
    {
        true => Ok(name),
        false => Err(Refusal::bad(
            "a name is up to 200 of a-z, 0-9 and -, starting with a letter or a digit",
        )),
    }
}

fn one_of(value: &str, allowed: &[(&str, &str)], what: &str) -> Result<String, Refusal> {
    match allowed.iter().any(|(id, _)| *id == value) {
        true => Ok(value.to_string()),
        false => Err(Refusal::bad(format!("{value} is not {what}"))),
    }
}

/// The views anybody may see. Reading is not ownership (ADR-0017 §7): a view is readable, and what
/// it holds is filtered to the viewer when it is drawn.
pub async fn views(store: &Store<'_>) -> Result<Vec<View>, Refusal> {
    let mut views = store.views().await?;
    views.sort_by_key(|view| view.name.to_lowercase());
    Ok(views)
}

/// Makes a view, owned by a team, a service or an organisation.
pub async fn make_view(
    backend: &Backend,
    store: &Store<'_>,
    name: &str,
    owner: &str,
) -> Result<View, Refusal> {
    writes(backend)?;
    let name = name.trim();
    if name.is_empty() || name.len() > 200 {
        return Err(Refusal::bad("give the view a name of up to 200 characters"));
    }
    // One field, `kind:name`, because that is what an owner is and it is what the Catalogue's own
    // picker hands back. Its kind is lowercased, so `Team:payments` from the picker and
    // `team:payments` typed by hand are the same owner.
    let (kind, owner_name) = owner
        .split_once(':')
        .ok_or_else(|| Refusal::bad("name the owner as a team, a service or an organisation"))?;
    let kind =
        one_of(&kind.trim().to_ascii_lowercase(), &OWNER_KINDS, "somebody who can own a view")?;
    let owner_name = named(owner_name)?;
    let owner = format!("{kind}:{owner_name}");
    // Whoever is asking has to manage the owner, which for a service means managing the team that
    // owns it (§7): a service is not a principal, so the question is resolved through it.
    directory::managing(backend, &owner).await?;
    let label = directory::owner_label(backend, &owner).await;
    store
        .insert(
            VIEWS,
            json!({
                "id": Uuid::now_v7(), "name": name, "owner": owner, "owner_label": label,
                "created_by": directory::me(backend),
            }),
        )
        .await
}

/// Renames a view. Only its name: who owns it is a separate question with a check of its own.
pub async fn rename_view(
    backend: &Backend,
    store: &Store<'_>,
    id: Uuid,
    name: &str,
) -> Result<View, Refusal> {
    writes(backend)?;
    let view = store.view(id).await?;
    directory::managing(backend, &view.owner).await?;
    let name = name.trim();
    if name.is_empty() || name.len() > 200 {
        return Err(Refusal::bad("give the view a name of up to 200 characters"));
    }
    store.update(VIEWS, id, json!({ "name": name })).await
}

pub async fn remove_view(backend: &Backend, store: &Store<'_>, id: Uuid) -> Result<(), Refusal> {
    writes(backend)?;
    let view = store.view(id).await?;
    directory::managing(backend, &view.owner).await?;
    // Placements cascade with the view; the claims do not, because they are the estate's (§7).
    store.delete(VIEWS, id).await
}

/// Declares a component. Drawn here, so its origin says so and a file may later own it instead.
pub async fn add_component(
    backend: &Backend,
    store: &Store<'_>,
    service: &str,
    name: &str,
    title: &str,
    role: &str,
    description: &str,
) -> Result<Component, Refusal> {
    writes(backend)?;
    let service = named(service)?;
    let name = named(name)?;
    let role = one_of(role, &ROLES, "a kind of component")?;
    let component: Component = store
        .insert(
            COMPONENTS,
            json!({
                "id": Uuid::now_v7(), "service": service, "name": name,
                "title": title.trim(), "role": role, "description": description.trim(),
                "origin": DRAWN, "created_by": directory::me(backend),
            }),
        )
        .await
        .map_err(|refusal| match refusal.status {
            409 => Refusal::conflict("that service already has a component of that name"),
            _ => refusal,
        })?;
    sync::synced(backend, &component).await;
    Ok(component)
}

pub async fn remove_component(
    backend: &Backend,
    store: &Store<'_>,
    id: Uuid,
) -> Result<Component, Refusal> {
    writes(backend)?;
    let component = store.component(id).await?;
    if component.origin != DRAWN {
        return Err(Refusal::bad(
            "a component a repository declares is removed by changing that file",
        ));
    }
    store.delete(COMPONENTS, id).await?;
    // The lines it was an end of go with it: a claim about something that is not there says
    // nothing, and leaving it would show as a disagreement nobody could resolve.
    for claim in store.claims().await? {
        let reference = component.reference();
        if claim.from == reference || claim.to == reference {
            store.delete(CLAIMS, claim.id).await?;
        }
    }
    sync::removed(backend, &component).await;
    Ok(component)
}

/// Draws a line, which is to say makes a claim (§5). It is checked against what the platform
/// observes exactly as a line from a repository file is.
#[allow(clippy::too_many_arguments)]
pub async fn connect(
    backend: &Backend,
    store: &Store<'_>,
    from: &str,
    to: &str,
    relationship: &str,
    description: &str,
    sides: (&str, &str),
) -> Result<Claim, Refusal> {
    writes(backend)?;
    let relationship = one_of(relationship, &RELATIONSHIPS, "a way of reaching something")?;
    let from = reference(backend, from).await?;
    let to = reference(backend, to).await?;
    if from == to {
        return Err(Refusal::bad("a component cannot reach itself"));
    }
    // A line with both ends outside says nothing about this estate, and there would be nothing
    // for the platform to check about it.
    if !inside(&from) && !inside(&to) {
        return Err(Refusal::bad("a line has at least one end inside the estate"));
    }
    store
        .insert(
            CLAIMS,
            json!({
                "id": Uuid::now_v7(), "from": from, "to": to, "relationship": relationship,
                "description": description.trim(), "origin": DRAWN,
                "from_side": side(sides.0), "to_side": side(sides.1),
                "created_by": directory::me(backend),
            }),
        )
        .await
        .map_err(|refusal| match refusal.status {
            409 => Refusal::conflict("that relationship is already drawn"),
            _ => refusal,
        })
}

/// One end of a line: a component of ours, a proxied vendor account, or a named external system.
///
/// Either end may be outside the context, and which one it is decides the direction of the
/// crossing (§6): outside reaching in is **ingress**, inside reaching out is **egress**. Nothing
/// more is needed to model it — an external party is the same kind of thing whichever way the
/// arrow points, so there is no separate notion of an actor.
async fn reference(backend: &Backend, given: &str) -> Result<String, Refusal> {
    let given = given.trim();
    let (kind, rest) = given
        .split_once(':')
        .ok_or_else(|| Refusal::bad(format!("{given} does not name anything")))?;
    match kind {
        // A whole service is an end in its own right: an architecture is usually drawn between
        // services first and opened up into components afterwards (§10).
        "service" => Ok(format!("service:{}", named(rest)?)),
        // A component is whatever the Catalogue calls one, which is either a name this plugin
        // declared as `<service>/<name>` or one applied straight to the Catalogue. Both are real
        // components, so both are checked against the Catalogue rather than this plugin's table.
        "component" => {
            let wanted = rest.trim().to_ascii_lowercase();
            let parts: Vec<&str> = wanted.split('/').collect();
            if parts.is_empty() || parts.len() > 2 {
                return Err(Refusal::bad("a component is named <name> or <service>/<name>"));
            }
            for part in &parts {
                named(part)?;
            }
            match directory::catalogued(backend).await.iter().any(|(name, _)| *name == wanted) {
                true => Ok(format!("component:{wanted}")),
                false => Err(Refusal::missing(format!("the Catalogue has no component {wanted}"))),
            }
        }
        "account" | "external" => Ok(format!("{kind}:{}", named(rest)?)),
        other => Err(Refusal::bad(format!("{other} is not something a line may reach"))),
    }
}

pub async fn disconnect(backend: &Backend, store: &Store<'_>, id: Uuid) -> Result<(), Refusal> {
    writes(backend)?;
    let claim = store.claim(id).await?;
    if claim.origin != DRAWN {
        return Err(Refusal::bad(
            "a relationship a repository declares is removed by changing that file",
        ));
    }
    store.delete(CLAIMS, id).await
}

/// Puts a node on a view for the first time, at the end of its column. This is what the palette's
/// Add button does, and so what a drop does (§5) — the drop presses the button rather than moving
/// anything itself, which is why dragging and the keyboard reach the same place.
pub async fn add_to_view(
    backend: &Backend,
    store: &Store<'_>,
    view: Uuid,
    node: &str,
    dropped: Option<(i64, i64)>,
) -> Result<(), Refusal> {
    writes(backend)?;
    let held = store.view(view).await?;
    directory::managing(backend, &held.owner).await?;
    let node = match inside(node) {
        true => reference(backend, node).await?,
        false => {
            return Err(Refusal::bad("a view holds the estate's own services and components"));
        }
    };
    let placed = store.placements(view).await?;
    if placed.iter().any(|placement| placement.node == node) {
        return Err(Refusal::conflict("that is on this view already"));
    }
    // Where it was dropped, when it was dropped somewhere. Otherwise laid out in a cascade, so
    // several added by the button one after another are readable without dragging them apart.
    let (x, y) = match dropped {
        Some((x, y)) => (x.clamp(0, 2_000), y.clamp(0, 2_000)),
        None => {
            let at = placed.len() as i64;
            (4 + (at % 4) * 24, 4 + (at / 4) * 14)
        }
    };
    let _: Value = store
        .insert(
            PLACEMENTS,
            json!({
                "id": Uuid::now_v7(), "view": view, "node": node,
                "column": 0, "row": y, "x": x, "y": y,
            }),
        )
        .await?;
    Ok(())
}

/// Takes something off a view. The component and its lines stay: they are the estate's, and only
/// this picture of them has changed (§7).
pub async fn take_off_view(
    backend: &Backend,
    store: &Store<'_>,
    view: Uuid,
    node: &str,
) -> Result<(), Refusal> {
    writes(backend)?;
    let held = store.view(view).await?;
    directory::managing(backend, &held.owner).await?;
    let placed = store.placements(view).await?;
    let Some(placement) = placed.iter().find(|placement| placement.node == node) else {
        return Err(Refusal::missing("that is not on this view"));
    };
    store.delete(PLACEMENTS, placement.id).await?;
    // Its lines go with it. A line to something no longer on the view says nothing here, and one
    // nobody can see is one nobody can correct.
    for claim in store.claims().await? {
        if (claim.from == node || claim.to == node) && claim.origin == DRAWN {
            store.delete(CLAIMS, claim.id).await?;
        }
    }
    Ok(())
}

/// Moves a node on a view's canvas (§5). Layout has no truth value, so this is the one kind of
/// change nothing ever verifies.
///
/// `row` follows `y`, so the server-rendered view — which has no canvas to position against and
/// orders things within a column instead — reads top to bottom the way the canvas looks.
pub async fn place(
    backend: &Backend,
    store: &Store<'_>,
    view: Uuid,
    node: &str,
    x: i64,
    y: i64,
    size: Option<(i64, i64)>,
) -> Result<(), Refusal> {
    writes(backend)?;
    let held = store.view(view).await?;
    directory::managing(backend, &held.owner).await?;
    let (x, y) = (x.clamp(0, 2_000), y.clamp(0, 2_000));
    let placed = store.placements(view).await?;
    let mut set = json!({ "x": x, "y": y, "row": y });
    // A size is only sent when something was resized, so a move never flattens one back to the
    // stylesheet's own.
    if let Some((w, h)) = size {
        set["w"] = json!(w.clamp(0, 400));
        set["h"] = json!(h.clamp(0, 400));
    }
    match placed.iter().find(|placement| placement.node == node) {
        Some(placement) => {
            let _: Value = store.update(PLACEMENTS, placement.id, set).await?;
        }
        None => {
            let mut values = set;
            values["id"] = json!(Uuid::now_v7());
            values["view"] = json!(view);
            values["node"] = json!(node);
            let _: Value = store.insert(PLACEMENTS, values).await?;
        }
    }
    Ok(())
}

/// How a node looks on one view, as its panel sends it.
pub struct Look {
    /// The name it goes by here, or empty for its own.
    pub label: String,
    pub description: String,
    pub colour: String,
    pub border: String,
    /// Another view it opens into.
    pub opens: Option<Uuid>,
    /// Whether the view opens centred on it.
    pub primary: bool,
}

/// Changes how a node looks on a view (§5). Like a position it has no truth value, so it belongs
/// to the view rather than the component: renaming a box here renames nothing in the Catalogue.
pub async fn dress(
    backend: &Backend,
    store: &Store<'_>,
    view: Uuid,
    node: &str,
    look: Look,
) -> Result<(), Refusal> {
    writes(backend)?;
    let held = store.view(view).await?;
    directory::managing(backend, &held.owner).await?;
    let placed = store.placements(view).await?;
    let Some(placement) = placed.iter().find(|placement| placement.node == node) else {
        return Err(Refusal::missing("that is not on this view"));
    };
    let usual = |value: &str, allowed: &[(&str, &str)], what: &str| match value {
        "" => Ok(String::new()),
        value => one_of(value, allowed, what),
    };
    let colour = usual(&look.colour, &COLOURS, "a colour a border may be")?;
    let border = usual(&look.border, &BORDERS, "a style a border may have")?;
    let label = look.label.trim();
    let description = look.description.trim();
    if label.chars().count() > 200 {
        return Err(Refusal::bad("a name on a view is up to 200 characters"));
    }
    if description.chars().count() > 2_000 {
        return Err(Refusal::bad("a description is up to 2,000 characters"));
    }
    if let Some(opens) = look.opens {
        if opens == view {
            return Err(Refusal::bad("a view cannot open into itself"));
        }
        store.view(opens).await?;
    }
    let _: Value = store
        .update(
            PLACEMENTS,
            placement.id,
            json!({
                "label": label, "description": description, "colour": colour, "border": border,
                "opens": look.opens,
            }),
        )
        .await?;
    // One primary a view: choosing this one is choosing it over whichever was before, and only
    // unticking the one that is primary leaves the view with none.
    let primary = match (look.primary, held.primary == node) {
        (true, false) => Some(node),
        (false, true) => Some(""),
        _ => None,
    };
    if let Some(primary) = primary {
        let _: Value = store.update(VIEWS, view, json!({ "primary": primary })).await?;
    }
    Ok(())
}

/// Changes what a line claims, which is a claim about the system and so is checked like any
/// other (§4). The line is the same line: only what it says it does has changed.
///
/// `arrows` says which way it points. Pointing the other way is the line turned around — what
/// called becomes what is called, each end keeping the side it was drawn to — and the relationship
/// is unchanged, so `calls` read the other way is "called by" without a second word for it. Both
/// ways says it holds in each direction. Empty leaves the arrows as they are.
pub async fn relate(
    backend: &Backend,
    store: &Store<'_>,
    claim: Uuid,
    relationship: &str,
    description: &str,
    arrows: &str,
) -> Result<(), Refusal> {
    writes(backend)?;
    let held = store.claim(claim).await?;
    if held.origin != DRAWN {
        return Err(Refusal::bad(
            "a relationship a repository declares is changed by changing that file",
        ));
    }
    let relationship = one_of(relationship, &RELATIONSHIPS, "a way of reaching something")?;
    let mut set = json!({ "relationship": relationship, "description": description.trim() });
    match arrows {
        "" => {}
        FORWARD | BOTH => set["both"] = json!(arrows == BOTH),
        BACKWARD => {
            let already = store.claims().await?.into_iter().any(|other| {
                other.from == held.to && other.to == held.from && other.relationship == relationship
            });
            if already {
                return Err(Refusal::conflict("that line already runs the other way"));
            }
            set["from"] = json!(held.to);
            set["to"] = json!(held.from);
            set["from_side"] = json!(held.to_side);
            set["to_side"] = json!(held.from_side);
            set["both"] = json!(false);
        }
        other => return Err(Refusal::bad(format!("{other} is not a way for arrows to point"))),
    }
    let _: Value =
        store.update(CLAIMS, claim, set).await.map_err(|refusal| match refusal.status {
            409 => Refusal::conflict("that relationship is already drawn"),
            _ => refusal,
        })?;
    Ok(())
}

/// Whether this instance takes writes at all; a read-only replica says so rather than failing
/// halfway through.
fn writes(backend: &Backend) -> Result<(), Refusal> {
    match backend.writes() {
        true => Ok(()),
        false => Err(Refusal::forbidden("this instance is read-only just now")),
    }
}
