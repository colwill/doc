//! What a component is, what a claim says, and what a view holds (ADR-0017).

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{PluginError, Response};
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

/// A column nobody wrote comes back as `null`, and `serde(default)` alone does not cover a key
/// that is present and null — only one that is absent. Every optional text field reads through
/// this, as `secrets` does for the same reason.
pub fn or_default<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub detail: String,
}

impl Refusal {
    pub fn bad(detail: impl Into<String>) -> Self {
        Self { status: 400, detail: detail.into() }
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self { status: 403, detail: detail.into() }
    }

    pub fn missing(detail: impl Into<String>) -> Self {
        Self { status: 404, detail: detail.into() }
    }

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self { status: 409, detail: detail.into() }
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self { status: 503, detail: detail.into() }
    }

    pub fn response(&self) -> Response {
        let kind = match self.status {
            400 => "bad-request",
            403 => "forbidden",
            404 => "not-found",
            409 => "conflict",
            _ => "unavailable",
        };
        Response::problem(self.status, kind, &self.detail)
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{}", self.detail)
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        tracing::warn!(%err, "a call to the backend failed");
        Self::unavailable(format!("the Architecture map's storage failed: {}", err.detail()))
    }
}

/// What sort of thing a component is. A closed set, because the shapes on the map are drawn from
/// it and a set anybody may add to is a set of shapes nobody recognises.
pub const ROLES: [(&str, &str); 7] = [
    ("api", "API"),
    ("worker", "Worker"),
    ("job", "Job"),
    ("queue", "Queue"),
    ("database", "Database"),
    ("cache", "Cache"),
    ("site", "Site"),
];

pub fn role_label(role: &str) -> &str {
    ROLES.iter().find(|(id, _)| *id == role).map_or("Component", |(_, label)| *label)
}

/// How one thing reaches another (ADR-0017 §2). Closed on purpose: an open set means every team
/// invents its own words and no two maps can be read against each other.
pub const RELATIONSHIPS: [(&str, &str); 5] = [
    ("calls", "calls"),
    ("reads", "reads"),
    ("writes", "writes"),
    ("publishes", "publishes to"),
    ("subscribes", "subscribes to"),
];

/// Which way a line's arrows point, as its menu offers them: the way it runs, the other way — which
/// is the line turned around — or both ways.
pub const FORWARD: &str = "forward";
pub const BACKWARD: &str = "backward";
pub const BOTH: &str = "both";

pub fn relationship_label(relationship: &str) -> &str {
    RELATIONSHIPS
        .iter()
        .find(|(id, _)| *id == relationship)
        .map_or(relationship, |(_, label)| *label)
}

/// The sides a line may leave from and arrive at. A closed set, because each is a place on a box
/// rather than a direction anybody may invent.
pub const SIDES: [&str; 4] = ["top", "right", "bottom", "left"];

/// One side, or empty for the middle, which is where a line goes until somebody draws it from a
/// particular handle.
pub fn side(given: &str) -> String {
    let given = given.trim().to_ascii_lowercase();
    match SIDES.contains(&given.as_str()) {
        true => given,
        false => String::new(),
    }
}

/// Where a declaration came from, which is how much to trust it (§2).
pub const DRAWN: &str = "drawn";
pub const REPOSITORY: &str = "repository";

/// Whether a reference names something of this estate: a service the Catalogue holds, or a
/// component of one. Anything else is beyond the boundary.
pub fn inside(reference: &str) -> bool {
    reference.starts_with("service:") || reference.starts_with("component:")
}

/// Which way a line crosses the boundary of the context (§6). Worked out from its ends rather
/// than stored: an end is inside or it is not, and that settles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Crossing {
    /// Something outside reaching in. Declared only — DOC does not see inbound traffic.
    Ingress,
    /// Something inside reaching out, which the vendor proxy's records can evidence.
    Egress,
    /// Both ends inside the context.
    Inside,
}

impl Crossing {
    pub fn of(from: &str, to: &str) -> Self {
        let within = inside;
        match (within(from), within(to)) {
            (false, true) => Self::Ingress,
            (true, false) => Self::Egress,
            _ => Self::Inside,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Ingress => "Ingress",
            Self::Egress => "Egress",
            Self::Inside => "",
        }
    }
}

/// A part of a service that runs or stores.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Component {
    pub id: Uuid,
    /// The service it belongs to, by the Catalogue's name for it.
    pub service: String,
    pub name: String,
    #[serde(default, deserialize_with = "or_default")]
    pub title: String,
    #[serde(default, deserialize_with = "or_default")]
    pub role: String,
    #[serde(default, deserialize_with = "or_default")]
    pub description: String,
    /// `drawn` or `repository`.
    pub origin: String,
    /// Where a declared one was read from, for the page to link to.
    #[serde(default, deserialize_with = "or_default")]
    pub source: String,
    #[serde(default, deserialize_with = "or_default")]
    pub created_by: String,
}

impl Component {
    /// How a claim names it: stable across a rename of its title but not of its name, which is
    /// what the Catalogue keys it on too.
    pub fn reference(&self) -> String {
        format!("component:{}/{}", self.service, self.name)
    }

    pub fn shown(&self) -> &str {
        match self.title.is_empty() {
            true => &self.name,
            false => &self.title,
        }
    }
}

/// One thing reaching another. A line somebody drew, or a line a repository declared.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Claim {
    pub id: Uuid,
    /// `component:<service>/<name>`.
    pub from: String,
    /// A component, an `external:<name>` outside the context, or an `account:<name>` the vendor
    /// proxy holds (§6).
    pub to: String,
    pub relationship: String,
    /// What it is for, in words. Shared, because it describes the relationship rather than one
    /// picture of it.
    #[serde(default, deserialize_with = "or_default")]
    pub description: String,
    /// Which side of each end it was drawn from and to: `top`, `right`, `bottom`, `left`, or
    /// empty for the middle.
    #[serde(default, deserialize_with = "or_default")]
    pub from_side: String,
    #[serde(default, deserialize_with = "or_default")]
    pub to_side: String,
    /// Whether it holds both ways, so its line has an arrow at each end.
    #[serde(default, deserialize_with = "or_default")]
    pub both: bool,
    pub origin: String,
    #[serde(default, deserialize_with = "or_default")]
    pub source: String,
    #[serde(default, deserialize_with = "or_default")]
    pub created_by: String,
}

/// A picture of part of the estate, owned by whoever made it (§7).
///
/// What is in frame is whatever has been put on it — the placements — rather than a list declared
/// up front. One answer to "what is this a picture of" rather than two that can disagree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct View {
    pub id: Uuid,
    pub name: String,
    /// `team:<name>`, `service:<name>` or `organisation:<name>`.
    pub owner: String,
    #[serde(default, deserialize_with = "or_default")]
    pub owner_label: String,
    #[serde(default, deserialize_with = "or_default")]
    pub description: String,
    /// The node the canvas centres on when the view opens, or empty for its top left.
    #[serde(default, deserialize_with = "or_default")]
    pub primary: String,
    #[serde(default, deserialize_with = "or_default")]
    pub created_by: String,
    #[serde(default, alias = "_created_at")]
    pub created_at: Option<DateTime<Utc>>,
}

/// Where a node sits in one view, which has no truth value and is never derived (§5).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Placement {
    #[serde(default)]
    pub id: Uuid,
    #[serde(default)]
    pub view: Uuid,
    /// The node this places: a component, an external system or a proxied account.
    pub node: String,
    /// A grid rather than free pixels, so placing can be done from the keyboard (§5).
    #[serde(default, deserialize_with = "or_default")]
    pub column: i64,
    #[serde(default, deserialize_with = "or_default")]
    pub row: i64,
    #[serde(default, deserialize_with = "or_default")]
    pub collapsed: bool,
    /// Where it sits on the canvas, in grid steps from the top left.
    #[serde(default, deserialize_with = "or_default")]
    pub x: i64,
    #[serde(default, deserialize_with = "or_default")]
    pub y: i64,
    /// How big it was drawn, in grid steps. Zero leaves it to the stylesheet.
    #[serde(default, deserialize_with = "or_default")]
    pub w: i64,
    #[serde(default, deserialize_with = "or_default")]
    pub h: i64,
    /// How it looks on this view, which is this picture's business and nobody else's: the name it
    /// goes by here, a line of description under it, and its border. Empty is the usual.
    #[serde(default, deserialize_with = "or_default")]
    pub label: String,
    #[serde(default, deserialize_with = "or_default")]
    pub description: String,
    #[serde(default, deserialize_with = "or_default")]
    pub colour: String,
    #[serde(default, deserialize_with = "or_default")]
    pub border: String,
    /// Another view this node opens into, for a box that is drawn again in more detail elsewhere.
    #[serde(default, deserialize_with = "or_default")]
    pub opens: Option<Uuid>,
}

/// The colours a border may be. A closed set, so each is a colour the stylesheet has chosen to
/// read against the canvas rather than whatever a picker last held.
pub const COLOURS: [(&str, &str); 8] = [
    ("blue", "Blue"),
    ("teal", "Teal"),
    ("green", "Green"),
    ("yellow", "Yellow"),
    ("orange", "Orange"),
    ("red", "Red"),
    ("pink", "Pink"),
    ("purple", "Purple"),
];

pub const BORDERS: [(&str, &str); 3] =
    [("solid", "Solid"), ("dashed", "Dashed"), ("dotted", "Dotted")];

/// Who may own a view, in the words the form offers.
pub const OWNER_KINDS: [(&str, &str); 3] =
    [("team", "A team"), ("service", "A service"), ("organisation", "An organisation")];
