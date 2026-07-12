//! What the plugin hands out: resources, the nodes they connect to, and refusals.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{PluginError, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::kinds::Kind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resource {
    pub id: Uuid,
    pub kind: Kind,
    pub name: String,
    pub title: String,
    pub description: String,
    pub metadata: Map<String, Value>,
    /// The owning team's name.
    pub owner: Option<String>,
    /// A team's address, which the platform keeps (T69).
    pub email: Option<String>,
    /// `apply`, the plugin whose sync events keep it, or `the platform` for what core keeps.
    pub source: String,
    #[serde(alias = "_created_at")]
    pub created_at: DateTime<Utc>,
    #[serde(alias = "_updated_at")]
    pub updated_at: DateTime<Utc>,
}

/// One end of a connection: `reference` is a role's `plugin/name`, or anything else's UUID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Node {
    pub kind: Kind,
    pub reference: String,
    pub name: String,
    #[serde(default)]
    pub title: String,
    /// What names it without doubt, for links and lookups: a user's `provider/login`, else its name.
    #[serde(default)]
    pub key: String,
}

impl Node {
    pub fn key(&self) -> (Kind, &str) {
        (self.kind, &self.reference)
    }
}

/// A neighbour as the API shows it: `out` points down the levels, `in` up.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Neighbour {
    pub kind: Kind,
    pub name: String,
    pub key: String,
    pub title: String,
    pub direction: &'static str,
    pub derived: bool,
    /// Where a derived neighbour came from: `rbac`, or `owner` for what a team owns. Empty for a
    /// connection somebody made.
    pub via: &'static str,
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

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        tracing::warn!(%err, "a call to the backend failed");
        match err {
            PluginError::Refused { status: 409, body } => Self::conflict(format!(
                "the catalogue changed while this was being written; try again: {body}"
            )),
            err => Self::unavailable(format!("the resources plugin's storage failed: {err}")),
        }
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}
