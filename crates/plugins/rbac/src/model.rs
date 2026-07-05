//! What the plugin stores and answers with, and the refusals its checks end in.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use doc_permissions::{Group, MemberKind, Permission};
use doc_plugin_sdk::{PluginError, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

/// Whoever holds permissions or attributes: a user or service account by ID, or a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Holder {
    User {
        id: Uuid,
    },
    Service {
        id: Uuid,
    },
    /// A team of core's (T67). What it holds, its people hold, and so do the people of the teams
    /// inside it, since access flows down a team's tree and never up.
    Team {
        id: Uuid,
    },
    Group {
        plugin: String,
        name: String,
    },
}

impl Holder {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::User { .. } => "user",
            Self::Service { .. } => "service",
            Self::Team { .. } => "team",
            Self::Group { .. } => "group",
        }
    }

    /// How the attributes table names a holder.
    pub fn key(&self) -> String {
        match self {
            Self::User { id } | Self::Service { id } | Self::Team { id } => id.to_string(),
            Self::Group { plugin, name } => format!("{plugin}/{name}"),
        }
    }

    pub fn member_kind(&self) -> Option<MemberKind> {
        match self {
            // A team holds what its people hold, so it holds a person's kind of permission.
            Self::User { .. } | Self::Team { .. } => Some(MemberKind::User),
            Self::Service { .. } => Some(MemberKind::Service),
            Self::Group { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub holder: Holder,
    pub label: Option<String>,
    pub permission: String,
    /// `api`, `self-service` or `onboarding`.
    pub source: String,
    pub granted_by: String,
    pub granted_at: DateTime<Utc>,
}

/// A group is a role: a named set of permissions for users or for service accounts, which may
/// reach across plugins. `plugin` is the one it is listed under, which gives it its name and
/// holds its membership permission; where it grants is wherever its permissions point.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRecord {
    pub plugin: String,
    pub name: String,
    pub kind: MemberKind,
    pub description: String,
    pub permissions: Vec<String>,
    pub members: i64,
}

impl GroupRecord {
    pub fn membership(&self) -> String {
        membership(&self.plugin, &self.name)
    }

    /// Every plugin it grants on, the one it is listed under first and the rest in name order.
    pub fn plugins(&self) -> Vec<String> {
        let mut plugins = vec![self.plugin.clone()];
        let mut others: Vec<String> = self
            .permissions
            .iter()
            .filter_map(|held| plugin_of(held))
            .filter(|plugin| *plugin != self.plugin)
            .map(str::to_string)
            .collect();
        others.sort();
        others.dedup();
        plugins.extend(others);
        plugins
    }

    pub fn group(&self) -> Result<Group, Refusal> {
        let mut group = Group::new(&self.plugin, &self.name, self.kind).map_err(Refusal::from)?;
        for permission in &self.permissions {
            group.insert(permission.parse().map_err(Refusal::from)?).map_err(Refusal::from)?;
        }
        Ok(group)
    }
}

pub fn membership(plugin: &str, name: &str) -> String {
    format!("plugin:{plugin}:group:{name}")
}

/// The plugin a written permission names, without parsing the whole of it.
pub fn plugin_of(permission: &str) -> Option<&str> {
    permission.strip_prefix("plugin:")?.split(':').next().filter(|plugin| !plugin.is_empty())
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Conditions {
    pub provider: Option<String>,
    pub organisation: Option<String>,
    /// A team a provider reported at sign-in, written `<organisation>/<team>`. Rules are no longer
    /// written on one: a provider's teams are kept in core and hold permissions themselves (T68).
    /// It is still read so that a rule written before that still loads — and such a rule matches
    /// nobody until it is rewritten, rather than quietly applying to everyone it once narrowed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub team: Option<String>,
    pub attributes: BTreeMap<String, String>,
}

impl Conditions {
    /// Every condition given must hold, so a rule with none applies to everyone.
    pub fn matches(&self, sign_in: &SignIn, attributes: &BTreeMap<String, String>) -> bool {
        if self.names_a_team() {
            return false;
        }
        let same = |a: &str, b: &str| a.eq_ignore_ascii_case(b);
        let listed = |wanted: &str, list: &[String]| list.iter().any(|item| same(item, wanted));
        self.provider.as_deref().is_none_or(|provider| same(provider, &sign_in.provider))
            && self.organisation.as_deref().is_none_or(|org| listed(org, &sign_in.organisations))
            && self.attributes.iter().all(|(key, value)| attributes.get(key) == Some(value))
    }

    /// A rule written before teams moved into core, which is applied to nobody.
    pub fn names_a_team(&self) -> bool {
        self.team.as_deref().is_some_and(|team| !team.is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupRef {
    pub plugin: String,
    pub name: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuleGrants {
    pub groups: Vec<GroupRef>,
    pub permissions: Vec<String>,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rule {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    pub conditions: Conditions,
    pub grants: RuleGrants,
    pub enabled: bool,
    #[serde(alias = "_created_at")]
    pub created_at: DateTime<Utc>,
    #[serde(alias = "_updated_at")]
    pub updated_at: DateTime<Utc>,
}

/// A user's most recent sign-in, which onboarding rules are matched against.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SignIn {
    pub user: Uuid,
    pub login: String,
    pub provider: String,
    pub organisations: Vec<String>,
    pub teams: Vec<String>,
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

    /// Core reads the status from the body, since the Service Bus passes on only a failure's text.
    pub fn internal(&self) -> Response {
        Response::json(&json!({ "refused": { "status": self.status, "detail": self.detail } }))
    }
}

impl From<doc_permissions::Error> for Refusal {
    fn from(err: doc_permissions::Error) -> Self {
        Self::bad(err.to_string())
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        tracing::warn!(%err, "a call to the backend failed");
        Self::unavailable(format!("the RBAC plugin's storage failed: {err}"))
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

pub fn parse(permission: &str) -> Result<Permission, Refusal> {
    permission
        .parse()
        .map_err(|err| Refusal::bad(format!("`{permission}` is not a permission: {err}")))
}
