//! Typed client for the backend API: a base URL, a bearer token, and RFC 9457 problems
//! turned into a typed error.

use std::time::Duration;

use anyhow::{Context, Result};
use doc_secret::Secret;
use http::Method;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use url::Url;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Deserialize)]
pub struct Problem {
    #[serde(rename = "type", default)]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    pub status: u16,
    #[serde(default)]
    pub detail: Option<String>,
    #[serde(flatten)]
    pub extensions: Map<String, Value>,
}

impl std::fmt::Display for Problem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.detail {
            Some(detail) => write!(f, "{} ({}): {detail}", self.title, self.status),
            None => write!(f, "{} ({})", self.title, self.status),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BackendError {
    #[error("{0}")]
    Problem(Box<Problem>),
    #[error("the backend is unreachable: {0}")]
    Unreachable(String),
    #[error("the backend's response could not be read: {0}")]
    Decode(String),
    #[error("the backend returned {status}: {body}")]
    Unexpected { status: u16, body: String },
}

impl BackendError {
    /// The HTTP status the backend answered with, when it answered at all.
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Problem(problem) => Some(problem.status),
            Self::Unexpected { status, .. } => Some(*status),
            Self::Unreachable(_) | Self::Decode(_) => None,
        }
    }

    /// What a person can be shown: the problem's detail rather than its whole body.
    pub fn detail(&self) -> String {
        match self {
            Self::Problem(problem) => {
                problem.detail.clone().unwrap_or_else(|| problem.title.clone())
            }
            other => other.to_string(),
        }
    }

    /// One of the problem's extensions, such as the `problems` a refused save puts against each
    /// field (ADR-0007).
    pub fn extension(&self, key: &str) -> Option<Value> {
        match self {
            Self::Problem(problem) => problem.extensions.get(key).cloned(),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Health {
    pub status: String,
    pub version: String,
    pub uptime_s: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Level {
    Up,
    Degraded,
    Down,
    Unknown,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NodeStatus {
    pub address: String,
    pub state: Level,
    pub role: Option<String>,
    pub term: Option<u64>,
    pub leader: Option<u64>,
    pub last_applied: Option<u64>,
    pub replication_lag: Option<u64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Component {
    pub kind: String,
    pub name: String,
    pub state: Level,
    pub detail: Option<String>,
    #[serde(default)]
    pub nodes: Vec<NodeStatus>,
    /// Each component is checked on its own schedule, so each card carries its own time.
    pub checked_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub latency_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginStatus {
    pub id: String,
    pub state: Level,
    /// `running`, `loading`, `cancelled`, `unloading` or `error`; absent when no process registered.
    #[serde(default)]
    pub lifecycle: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Status {
    pub state: Level,
    pub version: String,
    pub uptime_s: u64,
    pub checked_at: chrono::DateTime<chrono::Utc>,
    /// `workers` when the probes wrote these, `live` when the backend checked for itself.
    #[serde(default)]
    pub source: String,
    pub components: Vec<Component>,
    pub plugins: Vec<PluginStatus>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Readiness {
    pub status: String,
    pub checks: Value,
}

/// An identity provider that is running, and how it signs people in.
#[derive(Debug, Clone, Deserialize)]
pub struct Offered {
    pub id: String,
    pub title: String,
    /// `redirect` (off to the provider and back) or `password` (a form here).
    pub kind: String,
}

impl Offered {
    pub fn is_password(&self) -> bool {
        self.kind == "password"
    }
}

/// An organisation, and the identity providers its people sign in with.
#[derive(Debug, Clone, Deserialize)]
pub struct SignInOrganisation {
    pub id: String,
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub providers: Vec<Offered>,
}

/// What a sign-in page may offer: each organisation's running identity providers.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Providers {
    #[serde(default)]
    pub organisations: Vec<SignInOrganisation>,
}

/// An identity provider as an organisation's page offers it, and which organisation chose it.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderChoice {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub organisation: Option<String>,
}

/// A new session, however it was started, or an account linked for whoever is signed in already.
#[derive(Debug, Clone, Deserialize)]
pub struct Session {
    /// Absent when the sign-in linked an account rather than starting a session.
    #[serde(default)]
    pub token: Option<Secret<String>>,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub return_to: Option<String>,
    /// The user's first sign-in, which the welcome page follows.
    #[serde(default)]
    pub first: bool,
    /// An account was linked to the person who asked, and nobody was signed in.
    #[serde(default)]
    pub linked: bool,
    /// A one-time password worked: they choose their own, with this ticket, before a session.
    #[serde(default)]
    pub change_password: bool,
    #[serde(default)]
    pub ticket: Option<Secret<String>>,
    #[serde(default)]
    pub username: Option<String>,
}

/// An account someone signs in with or has linked (ADR-0005).
#[derive(Debug, Clone, Deserialize)]
pub struct IdentityView {
    pub id: String,
    pub provider: String,
    pub external_id: String,
    pub login: String,
    /// `sign-in`, `link`, `admin` or `provider`.
    pub source: String,
    /// What the provider last reported of the person.
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub surname: Option<String>,
    #[serde(default)]
    pub reported_at: Option<chrono::DateTime<chrono::Utc>>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Your own accounts, and the identity providers running now.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct MyIdentities {
    #[serde(default)]
    pub identities: Vec<IdentityView>,
    #[serde(default)]
    pub providers: Vec<Offered>,
}

/// A user and every account linked to them.
#[derive(Debug, Clone, Deserialize)]
pub struct UserView {
    pub id: String,
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub first_name: Option<String>,
    #[serde(default)]
    pub surname: Option<String>,
    /// The organisation they belong to.
    #[serde(default)]
    pub organisation: Option<Organisation>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub first_signed_in_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub last_signed_in_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub identities: Vec<IdentityView>,
    #[serde(default)]
    pub teams: Vec<TeamOf>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenView {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub revoked_at: Option<chrono::DateTime<chrono::Utc>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CreatedToken {
    pub token: Secret<String>,
    pub created: TokenView,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct NavEntry {
    pub label: String,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The menu it sits in until an administrator arranges the navigation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Counted for access and breadcrumbs, but never drawn as a link in the bar.
    #[serde(default)]
    pub hidden: bool,
}

#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct PluginAccess {
    #[serde(default)]
    pub read: bool,
    /// Whether this viewer may see the plugin's settings, and change them (ADR-0007).
    #[serde(default)]
    pub settings: bool,
    #[serde(default)]
    pub settings_write: bool,
    #[serde(default)]
    pub write: bool,
    /// Whether its pages can be opened now.
    #[serde(default)]
    pub running: bool,
    #[serde(default)]
    pub nav: Vec<NavEntry>,
    /// The providers it needs the caller to have linked an account with.
    #[serde(default)]
    pub links: Vec<String>,
    /// What it offers for the caller's dashboard.
    #[serde(default)]
    pub dashboard: Vec<DashboardItem>,
}

/// Something a plugin offers for a person's dashboard: its heading and the fragment it is drawn
/// from, under the plugin's own pages.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct DashboardItem {
    pub id: String,
    pub label: String,
    pub path: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub wide: bool,
}

/// What a person chose for their dashboard, as `plugin/id` in order, and whether they chose it or
/// it is the starter set.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct Dashboard {
    #[serde(default)]
    pub items: Vec<String>,
    #[serde(default)]
    pub chosen: bool,
}

/// The caller's access to every plugin, which the navigation is built from.
#[derive(Debug, Clone, Default, Deserialize, serde::Serialize)]
pub struct Access {
    #[serde(default)]
    pub admin: bool,
    #[serde(default)]
    pub plugins: std::collections::BTreeMap<String, PluginAccess>,
    /// How an administrator arranged the navigation; `None` shows pages in the order offered.
    #[serde(default)]
    pub navigation: Option<Layout>,
    /// The caller's linked accounts, as provider to login.
    #[serde(default)]
    pub linked: std::collections::BTreeMap<String, String>,
    /// What an administrator set for the whole platform.
    #[serde(default)]
    pub settings: Settings,
    /// What the plugins are sorted into, in the order they are shown (`[[plugins.categories]]`).
    #[serde(default)]
    pub categories: Vec<Category>,
}

/// One of the categories plugins are sorted into: its name, and the plugin IDs in it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, serde::Serialize)]
pub struct Category {
    pub name: String,
    #[serde(default)]
    pub plugins: Vec<String>,
}

/// The platform's own settings, as `/api/v1/settings` holds them.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, serde::Serialize)]
pub struct Settings {
    /// Shown before the logo; empty leaves each frontend its configured name.
    #[serde(default)]
    pub instance_name: String,
}

/// How far the first setup has got, as `/api/v1/setup` keeps it.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Setup {
    /// When an administrator first opened it; until then it opens by itself after a first sign-in.
    #[serde(default)]
    pub started_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The tools the organisation uses, such as `github`, which shape what it suggests.
    #[serde(default)]
    pub tools: Vec<String>,
    /// The steps marked done, by name.
    #[serde(default)]
    pub done: Vec<String>,
    /// When it was finished or put aside; it is then offered to nobody.
    #[serde(default)]
    pub finished_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub finished_by: Option<String>,
}

/// The navigation's arrangement, as `/api/v1/navigation` holds it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, serde::Serialize)]
pub struct Layout {
    #[serde(default)]
    pub entries: Vec<LayoutEntry>,
    /// The pages the landing page shows as cards, in order; empty leaves it its own choice.
    #[serde(default)]
    pub landing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
pub struct LayoutEntry {
    pub href: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

/// A plugin as the plugins page lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct PluginRow {
    pub id: String,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub classification: Option<String>,
    /// §5's lifecycle state; none while no process is registered.
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
    #[serde(default)]
    pub last_error_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub since: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub registered_at: Option<chrono::DateTime<chrono::Utc>>,
    /// What **Enable** would switch, for a plugin whose features work from other plugins'.
    #[serde(default)]
    pub enable: Option<EnableOffer>,
    /// Who turned it off and when, while it is off: held cancelled and offered to nobody.
    #[serde(default)]
    pub turned_off: Option<TurnedOff>,
    /// The flag it is turned on and off by, while it follows one.
    #[serde(default)]
    pub follows: Option<Following>,
}

/// The flag a plugin is turned on and off by, and what reading it last found.
#[derive(Debug, Clone, Deserialize)]
pub struct Following {
    pub flag: String,
    #[serde(default)]
    pub by: Option<String>,
    #[serde(default)]
    pub read: Option<bool>,
    #[serde(default)]
    pub read_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub problem: Option<String>,
}

/// Somebody turning a plugin off.
#[derive(Debug, Clone, Deserialize)]
pub struct TurnedOff {
    #[serde(default)]
    pub by: Option<String>,
    pub at: chrono::DateTime<chrono::Utc>,
}

/// A plugin's own features that work from other plugins' features, and those it would turn on
/// with them; `blocked` says why it cannot be offered yet.
#[derive(Debug, Clone, Deserialize)]
pub struct EnableOffer {
    #[serde(default)]
    pub features: Vec<String>,
    #[serde(default)]
    pub with: Vec<EnableWith>,
    #[serde(default)]
    pub blocked: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct EnableWith {
    pub plugin: String,
    pub feature: String,
}

impl EnableOffer {
    /// What the button's hint says it does beyond the plugin itself.
    pub fn also(&self) -> Option<String> {
        let named: Vec<String> =
            self.with.iter().map(|with| format!("{}'s {}", with.plugin, with.feature)).collect();
        match named.as_slice() {
            [] => None,
            [one] => Some(format!("Also turns on {one}.")),
            [rest @ .., last] => Some(format!("Also turns on {} and {last}.", rest.join(", "))),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginList {
    #[serde(default)]
    pub source: String,
    pub plugins: Vec<PluginRow>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Registration {
    #[serde(default)]
    pub instance: Option<String>,
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub last_seen: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub busy: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StateChange {
    pub version: String,
    pub instance: String,
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    pub at: chrono::DateTime<chrono::Utc>,
    pub source: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginDetail {
    #[serde(flatten)]
    pub plugin: PluginRow,
    #[serde(default)]
    pub registration: Option<Registration>,
    #[serde(default)]
    pub history: Vec<StateChange>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct History {
    pub checks: Vec<Component>,
}

/// One setting as the plugin declared it, which is what the field on the page is built from.
#[derive(Debug, Clone, Deserialize)]
pub struct SettingSchema {
    pub key: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub hint: String,
    #[serde(default)]
    pub group: String,
    pub kind: String,
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub one_of: Vec<String>,
    /// The plugin's own `api/` route that offers what can be chosen, asked when the page is drawn.
    #[serde(default)]
    pub choices: Option<String>,
    #[serde(default)]
    pub feature: Option<String>,
}

/// What a setting's choices route answers (DOC-SPEC §9.14): what can be chosen and, for a `map`,
/// what each key can be given.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SettingChoices {
    pub choices: Vec<SettingChoice>,
    pub values: Vec<SettingChoice>,
    pub choices_label: String,
    pub values_label: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct SettingChoice {
    pub value: String,
    pub label: String,
    pub hint: String,
}

/// What it is set to now, and where that came from.
#[derive(Debug, Clone, Deserialize)]
pub struct SettingValue {
    pub key: String,
    pub source: String,
    #[serde(default)]
    pub value: Value,
    #[serde(default)]
    pub set: bool,
    #[serde(default)]
    pub unreadable: bool,
    /// The variable the deployment sets this with, when it does: what a value here overrides, and
    /// what clearing it falls back to.
    #[serde(default)]
    pub environment: Option<String>,
    #[serde(default)]
    pub updated_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub updated_by: Option<String>,
    /// The secret in Secret Storage a secret setting points at, rather than one typed in.
    #[serde(default)]
    pub from_store: Option<FromStore>,
}

/// Which secret in Secret Storage a setting points at, what the store calls it, and why it gives
/// the plugin nothing, when it does not (FEAT-SECRETS).
#[derive(Debug, Clone, Deserialize)]
pub struct FromStore {
    pub secret: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub problem: Option<String>,
}

/// The secrets a plugin's secret settings may point at: those Secret Storage shares with it.
#[derive(Debug, Clone, Deserialize)]
pub struct StoreOffer {
    pub store: String,
    #[serde(default)]
    pub secrets: Vec<StoreChoice>,
    #[serde(default)]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StoreChoice {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub hint: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FeatureView {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub warning: String,
    pub enabled: bool,
}

/// What a plugin says a credential it holds by name means.
#[derive(Debug, Clone, Deserialize)]
pub struct NamedSecrets {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub hint: String,
}

/// One such credential, by name and never by value.
#[derive(Debug, Clone, Deserialize)]
pub struct NamedSecret {
    pub name: String,
    #[serde(default)]
    pub unreadable: bool,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub updated_by: Option<String>,
    #[serde(default)]
    pub from_store: Option<FromStore>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginSettings {
    pub plugin: String,
    #[serde(default)]
    pub writes: bool,
    #[serde(default)]
    pub settings: Vec<SettingSchema>,
    #[serde(default)]
    pub current: Vec<SettingValue>,
    #[serde(default)]
    pub features: Vec<FeatureView>,
    /// Set when this plugin holds credentials by name, with what to call them.
    #[serde(default)]
    pub named_secrets: Option<NamedSecrets>,
    #[serde(default)]
    pub named: Vec<NamedSecret>,
    #[serde(default)]
    pub missing: Vec<String>,
    #[serde(default)]
    pub secrets_available: bool,
    /// Other plugins asking to join one of its requestable settings.
    #[serde(default)]
    pub requests: Vec<AccessRequestView>,
    /// What its secret settings may point at in Secret Storage, when a plugin keeps secrets here.
    #[serde(default)]
    pub store: Option<StoreOffer>,
}

/// A plugin asking to join one of another plugin's requestable settings.
#[derive(Debug, Clone, Deserialize)]
pub struct AccessRequestView {
    pub id: String,
    pub requester: String,
    pub setting_label: String,
    #[serde(default)]
    pub reason: String,
    pub state: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default)]
    pub decided_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub decided_by: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginPermission {
    pub permission: String,
    #[serde(default)]
    pub description: String,
    /// Whoever the permission provider says holds it, or nothing when it could not be asked.
    #[serde(default)]
    pub holders: Option<Vec<String>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PluginPermissions {
    pub plugin: String,
    #[serde(default)]
    pub permissions: Vec<PluginPermission>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Account {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub owner_id: Option<String>,
    /// The team that owns it, whose members manage it.
    #[serde(default)]
    pub owner_team_id: Option<String>,
    pub disabled: bool,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Organisation {
    pub id: String,
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// How many teams it has, where a list gives it.
    #[serde(default)]
    pub teams: usize,
    /// The email domains it approves, where its own page gives them (FEAT-PEOPLE).
    #[serde(default)]
    pub domains: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Team {
    pub id: String,
    pub organisation_id: String,
    #[serde(default)]
    pub parent_id: Option<String>,
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    /// Where to write to the team; empty for none.
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub default: bool,
    /// The plugin that provides it; `None` for a team made in DOC.
    #[serde(default)]
    pub provider: Option<String>,
    /// The member who leads it.
    #[serde(default)]
    pub lead_id: Option<String>,
    /// Its organisation's name, how many people are in it and who leads it, where a list gives
    /// them.
    #[serde(default)]
    pub organisation: String,
    #[serde(default)]
    pub members: usize,
    #[serde(default)]
    pub lead: Option<Lead>,
}

/// Who leads a team, as the list of teams names them.
#[derive(Debug, Clone, Deserialize)]
pub struct Lead {
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Member {
    pub user_id: String,
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    /// `admin`, `default` or `provider`.
    pub source: String,
    #[serde(default)]
    pub provider: Option<String>,
    /// The name of the position they hold in the team.
    #[serde(default)]
    pub position: Option<String>,
}

/// A token limited to some plugins' permissions for minutes, such as one the Data Vacuum gave an
/// administrator's agent.
#[derive(Debug, Clone, Deserialize)]
pub struct ScopedToken {
    pub id: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub issued_by: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    pub expires_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// A name typed into a form, made safe as one segment of a path; the platform says whether it is a
/// name at all.
fn segment(name: &str) -> String {
    url::form_urlencoded::byte_serialize(name.trim().as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

/// A position an organisation defines, such as Senior Software Engineer.
#[derive(Debug, Clone, Deserialize)]
pub struct Position {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub responsibilities: Vec<String>,
}

/// What one team changes about a position it inherits, or a position of its own.
#[derive(Debug, Clone, Deserialize)]
pub struct PositionChange {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
    #[serde(default)]
    pub hidden: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Holder {
    pub user_id: String,
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
}

/// A position as one team has it: inherited and changed on the way down, or its own.
#[derive(Debug, Clone, Deserialize)]
pub struct TeamPosition {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub responsibilities: Vec<String>,
    #[serde(default)]
    pub hidden: bool,
    /// `organisation`, or the name of the team that made it its own.
    pub origin: String,
    #[serde(default)]
    pub changed_by: Vec<String>,
    #[serde(default)]
    pub changed_here: bool,
    #[serde(default)]
    pub holders: Vec<Holder>,
    /// What this team itself changes of it.
    #[serde(default)]
    pub change: Option<PositionChange>,
}

/// A team with its organisation, where it sits, who is in it and, for those who manage them, the
/// service accounts it owns.
#[derive(Debug, Clone, Deserialize)]
pub struct TeamDetail {
    #[serde(flatten)]
    pub team: Team,
    #[serde(rename = "organisation")]
    pub organisation_of: Organisation,
    #[serde(default)]
    pub ancestors: Vec<Team>,
    #[serde(default)]
    pub sub_teams: Vec<Team>,
    #[serde(default)]
    pub members: Vec<Member>,
    #[serde(default)]
    pub service_accounts: Option<Vec<Account>>,
    #[serde(default)]
    pub positions: Vec<TeamPosition>,
    /// Whether the viewer changes its lead and positions: its lead, a lead above it, or an
    /// identity manager.
    #[serde(default)]
    pub arranges: bool,
}

/// A team someone is in, as their user record lists it.
#[derive(Debug, Clone, Deserialize)]
pub struct TeamOf {
    pub id: String,
    pub name: String,
    pub title: String,
    pub source: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Grant {
    pub permission: String,
    pub source: String,
    pub granted_by: String,
    pub granted_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Reach {
    #[serde(default)]
    pub scope: Option<String>,
    #[serde(default)]
    pub custom: std::collections::BTreeMap<String, String>,
}

/// What the RBAC plugin holds for a principal: its grants, and what they allow per plugin.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Holdings {
    #[serde(default)]
    pub assignments: Vec<Grant>,
    #[serde(default)]
    pub access: std::collections::BTreeMap<String, Reach>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuditRow {
    pub at: chrono::DateTime<chrono::Utc>,
    pub actor_kind: String,
    #[serde(default)]
    pub actor_label: Option<String>,
    pub action: String,
    #[serde(default)]
    pub subject: Option<String>,
    #[serde(default)]
    pub detail: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AuditLog {
    pub entries: Vec<AuditRow>,
}

/// A plugin route's answer, passed on as it came.
pub struct Forwarded {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: bytes::Bytes,
}

#[derive(Clone)]
pub struct BackendClient {
    http: reqwest::Client,
    base: Url,
    token: Option<Secret<String>>,
}

impl BackendClient {
    pub fn new(config: &crate::config::Backend) -> Result<Self> {
        let mut base = config.base_url.clone();
        if !base.ends_with('/') {
            base.push('/');
        }
        let base = Url::parse(&base)
            .with_context(|| format!("parsing the backend URL {}", config.base_url))?;
        let http = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("doc-frontend/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("building the backend HTTP client")?;
        let token = Some(config.token.clone()).filter(|token| !token.is_empty());
        Ok(Self { http, base, token })
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    pub async fn health(&self) -> Result<Health, BackendError> {
        self.call(Method::GET, "healthz", None, None).await
    }

    pub async fn ready(&self) -> Result<Readiness, BackendError> {
        self.call(Method::GET, "readyz", None, None).await
    }

    pub async fn status(&self, session: &str) -> Result<Status, BackendError> {
        self.call(Method::GET, "api/v1/status", Some(session), None).await
    }

    pub async fn me(&self, session: &str) -> Result<Value, BackendError> {
        self.call(Method::GET, "api/v1/me", Some(session), None).await
    }

    pub async fn providers(&self) -> Result<Providers, BackendError> {
        self.call(Method::GET, "api/v1/auth/providers", None, None).await
    }

    /// A sign-in with a username and a password, at a provider that takes them.
    pub async fn password_sign_in(
        &self,
        provider: &str,
        username: &str,
        password: &str,
        return_to: &str,
    ) -> Result<Session, BackendError> {
        let body = serde_json::json!({
            "username": username, "password": password, "return_to": return_to,
        });
        let path = format!("api/v1/plugins/{provider}/public/sign-in");
        self.call(Method::POST, &path, None, Some(&body)).await
    }

    /// The password someone chose after signing in with a one-time one.
    pub async fn choose_password(
        &self,
        provider: &str,
        ticket: &str,
        password: &str,
        return_to: &str,
    ) -> Result<Session, BackendError> {
        let body =
            serde_json::json!({ "ticket": ticket, "password": password, "return_to": return_to });
        let path = format!("api/v1/plugins/{provider}/public/password");
        self.call(Method::POST, &path, None, Some(&body)).await
    }

    pub async fn logout(&self, session: &str) -> Result<(), BackendError> {
        self.call::<Value>(Method::POST, "api/v1/auth/logout", Some(session), None).await.map(drop)
    }

    pub async fn tokens(&self, session: &str) -> Result<Vec<TokenView>, BackendError> {
        self.call(Method::GET, "api/v1/tokens", Some(session), None).await
    }

    /// The caller's scoped tokens still in force: their own, and those a plugin minted for them.
    pub async fn scoped_tokens(&self, session: &str) -> Result<Vec<ScopedToken>, BackendError> {
        self.call(Method::GET, "api/v1/tokens/scoped", Some(session), None).await
    }

    pub async fn create_token(
        &self,
        session: &str,
        name: &str,
        days: Option<i64>,
    ) -> Result<CreatedToken, BackendError> {
        let body = serde_json::json!({ "name": name, "expires_in_days": days });
        self.call(Method::POST, "api/v1/tokens", Some(session), Some(&body)).await
    }

    pub async fn revoke_token(&self, session: &str, id: &str) -> Result<(), BackendError> {
        let path = format!("api/v1/tokens/{id}");
        self.call::<Value>(Method::DELETE, &path, Some(session), None).await.map(drop)
    }

    pub async fn access(&self, session: &str) -> Result<Access, BackendError> {
        self.call(Method::GET, "api/v1/me/access", Some(session), None).await
    }

    pub async fn dashboard(&self, session: &str) -> Result<Dashboard, BackendError> {
        self.call(Method::GET, "api/v1/me/dashboard", Some(session), None).await
    }

    pub async fn set_dashboard(
        &self,
        session: &str,
        items: &[String],
    ) -> Result<Dashboard, BackendError> {
        let body = serde_json::json!({ "items": items });
        self.call(Method::PUT, "api/v1/me/dashboard", Some(session), Some(&body)).await
    }

    /// Other plugins asking to join a list in the settings of a plugin the caller may change.
    pub async fn waiting_requests(&self, session: &str) -> Result<Value, BackendError> {
        self.call(Method::GET, "api/v1/me/access-requests", Some(session), None).await
    }

    /// A plugin's `api/` route, read as the signed-in user; `route` carries its own query.
    pub async fn plugin_get(
        &self,
        session: &str,
        plugin: &str,
        route: &str,
    ) -> Result<Value, BackendError> {
        let path = format!("api/v1/plugins/{plugin}/api/{route}");
        self.call(Method::GET, &path, Some(session), None).await
    }

    pub async fn navigation(&self, session: &str) -> Result<Layout, BackendError> {
        self.call(Method::GET, "api/v1/navigation", Some(session), None).await
    }

    pub async fn set_navigation(
        &self,
        session: &str,
        layout: &Layout,
    ) -> Result<Layout, BackendError> {
        let body =
            serde_json::to_value(layout).map_err(|err| BackendError::Decode(err.to_string()))?;
        self.call(Method::PUT, "api/v1/navigation", Some(session), Some(&body)).await
    }

    pub async fn settings(&self, session: &str) -> Result<Settings, BackendError> {
        self.call(Method::GET, "api/v1/settings", Some(session), None).await
    }

    pub async fn set_settings(
        &self,
        session: &str,
        settings: &Settings,
    ) -> Result<Settings, BackendError> {
        let body =
            serde_json::to_value(settings).map_err(|err| BackendError::Decode(err.to_string()))?;
        self.call(Method::PUT, "api/v1/settings", Some(session), Some(&body)).await
    }

    pub async fn setup(&self, session: &str) -> Result<Setup, BackendError> {
        self.call(Method::GET, "api/v1/setup", Some(session), None).await
    }

    /// `{"started": true}`, `{"step": "<name>"}` to mark one done, or `{"finished": true}`.
    pub async fn change_setup(&self, session: &str, change: &Value) -> Result<Setup, BackendError> {
        self.call(Method::PATCH, "api/v1/setup", Some(session), Some(change)).await
    }

    /// A plugin's `ui` route, as the signed-in user; the answer is returned whatever its status.
    pub async fn forward(
        &self,
        session: &str,
        method: Method,
        path: &str,
        headers: Vec<(String, String)>,
        body: bytes::Bytes,
    ) -> Result<Forwarded, BackendError> {
        let mut request =
            self.http.request(method, format!("{}{path}", self.base)).bearer_auth(session);
        for (name, value) in headers {
            request = request.header(name, value);
        }
        if let Some(parent) = doc_telemetry::traceparent() {
            request = request.header(doc_telemetry::TRACEPARENT, parent);
        }
        if let Some(chain) = crate::web::client::chain() {
            request = request.header("x-forwarded-for", chain);
        }
        let response = request
            .body(body)
            .send()
            .await
            .map_err(|err| BackendError::Unreachable(err.to_string()))?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
            .collect();
        let body = response.bytes().await.map_err(|err| BackendError::Decode(err.to_string()))?;
        Ok(Forwarded { status, headers, body })
    }

    pub async fn status_history(&self, session: &str, hours: u32) -> Result<History, BackendError> {
        let path = format!("api/v1/status/history?hours={hours}");
        self.call(Method::GET, &path, Some(session), None).await
    }

    pub async fn plugins(&self, session: &str) -> Result<PluginList, BackendError> {
        self.call(Method::GET, "api/v1/plugins", Some(session), None).await
    }

    pub async fn plugin(&self, session: &str, id: &str) -> Result<PluginDetail, BackendError> {
        self.call(Method::GET, &format!("api/v1/plugins/{id}"), Some(session), None).await
    }

    /// `reload`, `unload` or `cancel`, or `resume`, which puts a cancelled plugin back to running.
    pub async fn plugin_action(
        &self,
        session: &str,
        id: &str,
        action: &str,
    ) -> Result<(), BackendError> {
        let (method, path, body) = match action {
            "resume" => (
                Method::POST,
                format!("api/v1/plugins/{id}/state"),
                serde_json::json!({ "state": "running" }),
            ),
            "turn-off" | "turn-on" => (
                Method::PUT,
                format!("api/v1/plugins/{id}/enabled"),
                serde_json::json!({ "enabled": action == "turn-on" }),
            ),
            other => (Method::POST, format!("api/v1/plugins/{id}/{other}"), serde_json::json!({})),
        };
        self.call::<Value>(method, &path, Some(session), Some(&body)).await.map(drop)
    }

    /// Has a plugin follow a flag, or with none, go back to being turned on and off by hand.
    pub async fn set_plugin_flag(
        &self,
        session: &str,
        id: &str,
        flag: Option<&str>,
    ) -> Result<Value, BackendError> {
        let path = format!("api/v1/plugins/{id}/flag");
        let body = serde_json::json!({ "flag": flag });
        self.call(Method::PUT, &path, Some(session), Some(&body)).await
    }

    /// Turns a plugin on with the features of other plugins it works from, in one go.
    pub async fn enable_plugin(&self, session: &str, id: &str) -> Result<Value, BackendError> {
        let path = format!("api/v1/plugins/{id}/enable");
        self.call(Method::POST, &path, Some(session), Some(&serde_json::json!({}))).await
    }

    /// A plugin's Settings and Features tabs, built from what the plugin declares (ADR-0007).
    pub async fn plugin_settings(
        &self,
        session: &str,
        id: &str,
    ) -> Result<PluginSettings, BackendError> {
        self.call(Method::GET, &format!("api/v1/plugins/{id}/settings"), Some(session), None).await
    }

    pub async fn save_plugin_settings(
        &self,
        session: &str,
        id: &str,
        values: &Map<String, Value>,
        named: &std::collections::BTreeMap<String, Option<String>>,
        named_from_store: &std::collections::BTreeMap<String, String>,
    ) -> Result<Value, BackendError> {
        let body = serde_json::json!({
            "values": values,
            "named_secrets": named,
            "named_from_store": named_from_store,
        });
        let path = format!("api/v1/plugins/{id}/settings");
        self.call(Method::PUT, &path, Some(session), Some(&body)).await
    }

    /// Approves or denies another plugin's request to join one of this plugin's settings.
    pub async fn decide_access_request(
        &self,
        session: &str,
        id: &str,
        request: &str,
        verdict: &str,
    ) -> Result<Value, BackendError> {
        let path = format!("api/v1/plugins/{id}/access-requests/{request}/{verdict}");
        self.call(Method::POST, &path, Some(session), Some(&serde_json::json!({}))).await
    }

    /// **Test connection**: what the plugin thinks of these settings, with nothing stored.
    pub async fn check_plugin_settings(
        &self,
        session: &str,
        id: &str,
        values: &Map<String, Value>,
    ) -> Result<Value, BackendError> {
        let body = serde_json::json!({ "values": values });
        let path = format!("api/v1/plugins/{id}/settings/check");
        self.call(Method::POST, &path, Some(session), Some(&body)).await
    }

    pub async fn set_plugin_features(
        &self,
        session: &str,
        id: &str,
        features: &std::collections::BTreeMap<String, bool>,
    ) -> Result<Value, BackendError> {
        let body = serde_json::json!({ "features": features });
        let path = format!("api/v1/plugins/{id}/features");
        self.call(Method::PUT, &path, Some(session), Some(&body)).await
    }

    pub async fn plugin_permission_holders(
        &self,
        session: &str,
        id: &str,
    ) -> Result<PluginPermissions, BackendError> {
        let path = format!("api/v1/plugins/{id}/settings/permissions");
        self.call(Method::GET, &path, Some(session), None).await
    }

    pub async fn plugin_tokens(
        &self,
        session: &str,
        id: &str,
    ) -> Result<Vec<TokenView>, BackendError> {
        self.call(Method::GET, &format!("api/v1/plugins/{id}/tokens"), Some(session), None).await
    }

    pub async fn create_plugin_token(
        &self,
        session: &str,
        id: &str,
        name: Option<&str>,
        days: Option<i64>,
    ) -> Result<CreatedToken, BackendError> {
        let body = serde_json::json!({ "name": name, "expires_in_days": days });
        let path = format!("api/v1/plugins/{id}/tokens");
        self.call(Method::POST, &path, Some(session), Some(&body)).await
    }

    pub async fn revoke_plugin_token(
        &self,
        session: &str,
        id: &str,
        token: &str,
    ) -> Result<(), BackendError> {
        let path = format!("api/v1/plugins/{id}/tokens/{token}");
        self.call::<Value>(Method::DELETE, &path, Some(session), None).await.map(drop)
    }

    pub async fn my_identities(&self, session: &str) -> Result<MyIdentities, BackendError> {
        self.call(Method::GET, "api/v1/me/identities", Some(session), None).await
    }

    /// Where to send the browser to sign in to `provider`, which then links the account.
    pub async fn start_link(
        &self,
        session: &str,
        provider: &str,
        return_to: &str,
    ) -> Result<String, BackendError> {
        let body = serde_json::json!({ "provider": provider, "return_to": return_to });
        let started: Value =
            self.call(Method::POST, "api/v1/me/links", Some(session), Some(&body)).await?;
        started["location"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| BackendError::Decode("a link started with nowhere to go".into()))
    }

    pub async fn unlink_mine(&self, session: &str, id: &str) -> Result<(), BackendError> {
        let path = format!("api/v1/me/identities/{id}");
        self.call::<Value>(Method::DELETE, &path, Some(session), None).await.map(drop)
    }

    pub async fn users(&self, session: &str) -> Result<Vec<UserView>, BackendError> {
        self.call(Method::GET, "api/v1/users", Some(session), None).await
    }

    pub async fn user(&self, session: &str, id: &str) -> Result<UserView, BackendError> {
        self.call(Method::GET, &format!("api/v1/users/{id}"), Some(session), None).await
    }

    pub async fn create_user(&self, session: &str, user: &Value) -> Result<UserView, BackendError> {
        self.call(Method::POST, "api/v1/users", Some(session), Some(user)).await
    }

    pub async fn set_user_disabled(
        &self,
        session: &str,
        id: &str,
        disabled: bool,
    ) -> Result<(), BackendError> {
        let body = serde_json::json!({ "disabled": disabled });
        let path = format!("api/v1/users/{id}");
        self.call::<Value>(Method::PATCH, &path, Some(session), Some(&body)).await.map(drop)
    }

    /// Whom it merged in to link it, if anyone.
    pub async fn attach_identity(
        &self,
        session: &str,
        id: &str,
        account: &Value,
    ) -> Result<Option<String>, BackendError> {
        let path = format!("api/v1/users/{id}/identities");
        let linked: Value = self.call(Method::POST, &path, Some(session), Some(account)).await?;
        Ok(linked["merged"].as_str().map(str::to_string))
    }

    pub async fn detach_identity(
        &self,
        session: &str,
        id: &str,
        identity: &str,
    ) -> Result<(), BackendError> {
        let path = format!("api/v1/users/{id}/identities/{identity}");
        self.call::<Value>(Method::DELETE, &path, Some(session), None).await.map(drop)
    }

    pub async fn merge_user(
        &self,
        session: &str,
        id: &str,
        from: &str,
    ) -> Result<(), BackendError> {
        let body = serde_json::json!({ "from": from });
        let path = format!("api/v1/users/{id}/merge");
        self.call::<Value>(Method::POST, &path, Some(session), Some(&body)).await.map(drop)
    }

    pub async fn accounts(&self, session: &str) -> Result<Vec<Account>, BackendError> {
        self.call(Method::GET, "api/v1/service-accounts", Some(session), None).await
    }

    /// Owned by the team when one is given, which must be one the caller is in or below.
    pub async fn create_account(
        &self,
        session: &str,
        name: &str,
        description: Option<&str>,
        team: Option<&str>,
    ) -> Result<Account, BackendError> {
        let body = serde_json::json!({ "name": name, "description": description, "team": team });
        self.call(Method::POST, "api/v1/service-accounts", Some(session), Some(&body)).await
    }

    /// Gives the account to one user or one team: `{"user": …}` or `{"team": …}`.
    pub async fn set_account_owner(
        &self,
        session: &str,
        id: &str,
        owner: &Value,
    ) -> Result<Account, BackendError> {
        let path = format!("api/v1/service-accounts/{id}/owner");
        self.call(Method::PUT, &path, Some(session), Some(owner)).await
    }

    pub async fn organisations(&self, session: &str) -> Result<Vec<Organisation>, BackendError> {
        self.call(Method::GET, "api/v1/organisations", Some(session), None).await
    }

    /// The organisation, its teams, and the identity providers there are to choose from.
    pub async fn organisation(
        &self,
        session: &str,
        id: &str,
    ) -> Result<(Organisation, Vec<Team>, Vec<ProviderChoice>), BackendError> {
        let found: Value = self
            .call(Method::GET, &format!("api/v1/organisations/{id}"), Some(session), None)
            .await?;
        let decode = |value: &Value| BackendError::Decode(value.to_string());
        let mut organisation: Organisation = serde_json::from_value(found["organisation"].clone())
            .map_err(|_| decode(&found["organisation"]))?;
        organisation.domains = serde_json::from_value(found["domains"].clone()).unwrap_or_default();
        let teams =
            serde_json::from_value(found["teams"].clone()).map_err(|_| decode(&found["teams"]))?;
        let providers = serde_json::from_value(found["providers"].clone()).unwrap_or_default();
        Ok((organisation, teams, providers))
    }

    pub async fn set_organisation_domains(
        &self,
        session: &str,
        id: &str,
        domains: &[String],
    ) -> Result<(), BackendError> {
        let body = serde_json::json!({ "domains": domains });
        let path = format!("api/v1/organisations/{id}/domains");
        self.call::<Value>(Method::PUT, &path, Some(session), Some(&body)).await.map(drop)
    }

    /// Adds somebody by email address (FEAT-PEOPLE): the person, whether they were made or were
    /// there already, and the DOC password login they were given.
    pub async fn add_person(&self, session: &str, person: &Value) -> Result<Value, BackendError> {
        self.call(Method::POST, "api/v1/people", Some(session), Some(person)).await
    }

    pub async fn set_organisation_providers(
        &self,
        session: &str,
        id: &str,
        providers: &[String],
    ) -> Result<(), BackendError> {
        let body = serde_json::json!({ "providers": providers });
        let path = format!("api/v1/organisations/{id}/providers");
        self.call::<Value>(Method::PUT, &path, Some(session), Some(&body)).await.map(drop)
    }

    /// Moves someone to another organisation, out of their old one's teams.
    pub async fn move_user(
        &self,
        session: &str,
        id: &str,
        organisation: &str,
    ) -> Result<(), BackendError> {
        let body = serde_json::json!({ "organisation": organisation });
        let path = format!("api/v1/users/{id}");
        self.call::<Value>(Method::PATCH, &path, Some(session), Some(&body)).await.map(drop)
    }

    pub async fn create_organisation(
        &self,
        session: &str,
        organisation: &Value,
    ) -> Result<Organisation, BackendError> {
        self.call(Method::POST, "api/v1/organisations", Some(session), Some(organisation)).await
    }

    pub async fn update_organisation(
        &self,
        session: &str,
        id: &str,
        changes: &Value,
    ) -> Result<Organisation, BackendError> {
        let path = format!("api/v1/organisations/{id}");
        self.call(Method::PATCH, &path, Some(session), Some(changes)).await
    }

    pub async fn delete_organisation(&self, session: &str, id: &str) -> Result<(), BackendError> {
        let path = format!("api/v1/organisations/{id}");
        self.call::<Value>(Method::DELETE, &path, Some(session), None).await.map(drop)
    }

    pub async fn teams(&self, session: &str) -> Result<Vec<Team>, BackendError> {
        self.call(Method::GET, "api/v1/teams", Some(session), None).await
    }

    pub async fn team(&self, session: &str, id: &str) -> Result<TeamDetail, BackendError> {
        self.call(Method::GET, &format!("api/v1/teams/{id}"), Some(session), None).await
    }

    pub async fn create_team(&self, session: &str, team: &Value) -> Result<Team, BackendError> {
        self.call(Method::POST, "api/v1/teams", Some(session), Some(team)).await
    }

    pub async fn update_team(
        &self,
        session: &str,
        id: &str,
        changes: &Value,
    ) -> Result<Team, BackendError> {
        self.call(Method::PATCH, &format!("api/v1/teams/{id}"), Some(session), Some(changes)).await
    }

    pub async fn delete_team(&self, session: &str, id: &str) -> Result<(), BackendError> {
        let path = format!("api/v1/teams/{id}");
        self.call::<Value>(Method::DELETE, &path, Some(session), None).await.map(drop)
    }

    pub async fn add_team_member(
        &self,
        session: &str,
        team: &str,
        user: &str,
    ) -> Result<(), BackendError> {
        let body = serde_json::json!({ "user": user });
        let path = format!("api/v1/teams/{team}/members");
        self.call::<Value>(Method::POST, &path, Some(session), Some(&body)).await.map(drop)
    }

    pub async fn remove_team_member(
        &self,
        session: &str,
        team: &str,
        user: &str,
    ) -> Result<(), BackendError> {
        let path = format!("api/v1/teams/{team}/members/{user}");
        self.call::<Value>(Method::DELETE, &path, Some(session), None).await.map(drop)
    }

    pub async fn organisation_positions(
        &self,
        session: &str,
        organisation: &str,
    ) -> Result<Vec<Position>, BackendError> {
        let path = format!("api/v1/organisations/{organisation}/positions");
        self.call(Method::GET, &path, Some(session), None).await
    }

    /// Makes or changes the organisation's position of that name.
    pub async fn put_organisation_position(
        &self,
        session: &str,
        organisation: &str,
        name: &str,
        position: &Value,
    ) -> Result<(), BackendError> {
        let path = format!("api/v1/organisations/{organisation}/positions/{}", segment(name));
        self.call::<Value>(Method::PUT, &path, Some(session), Some(position)).await.map(drop)
    }

    /// Who was left holding no position, as `{team, user}`.
    pub async fn delete_organisation_position(
        &self,
        session: &str,
        organisation: &str,
        name: &str,
    ) -> Result<Vec<Value>, BackendError> {
        let path = format!("api/v1/organisations/{organisation}/positions/{}", segment(name));
        let answer: Value = self.call(Method::DELETE, &path, Some(session), None).await?;
        Ok(answer["vacated"].as_array().cloned().unwrap_or_default())
    }

    /// Replaces what a team changes about a position; who was left holding none, as `{team, user}`.
    pub async fn put_team_position(
        &self,
        session: &str,
        team: &str,
        name: &str,
        change: &Value,
    ) -> Result<Vec<Value>, BackendError> {
        let path = format!("api/v1/teams/{team}/positions/{}", segment(name));
        let answer: Value = self.call(Method::PUT, &path, Some(session), Some(change)).await?;
        Ok(answer["vacated"].as_array().cloned().unwrap_or_default())
    }

    pub async fn delete_team_position(
        &self,
        session: &str,
        team: &str,
        name: &str,
    ) -> Result<Vec<Value>, BackendError> {
        let path = format!("api/v1/teams/{team}/positions/{}", segment(name));
        let answer: Value = self.call(Method::DELETE, &path, Some(session), None).await?;
        Ok(answer["vacated"].as_array().cloned().unwrap_or_default())
    }

    pub async fn set_team_lead(
        &self,
        session: &str,
        team: &str,
        user: Option<&str>,
    ) -> Result<(), BackendError> {
        let body = serde_json::json!({ "user": user });
        let path = format!("api/v1/teams/{team}/lead");
        self.call::<Value>(Method::PUT, &path, Some(session), Some(&body)).await.map(drop)
    }

    pub async fn set_member_position(
        &self,
        session: &str,
        team: &str,
        user: &str,
        position: Option<&str>,
    ) -> Result<(), BackendError> {
        let body = serde_json::json!({ "position": position });
        let path = format!("api/v1/teams/{team}/members/{user}/position");
        self.call::<Value>(Method::PUT, &path, Some(session), Some(&body)).await.map(drop)
    }

    pub async fn account(&self, session: &str, id: &str) -> Result<Account, BackendError> {
        self.call(Method::GET, &format!("api/v1/service-accounts/{id}"), Some(session), None).await
    }

    pub async fn set_account_disabled(
        &self,
        session: &str,
        id: &str,
        disabled: bool,
    ) -> Result<(), BackendError> {
        let body = serde_json::json!({ "disabled": disabled });
        let path = format!("api/v1/service-accounts/{id}");
        self.call::<Value>(Method::PATCH, &path, Some(session), Some(&body)).await.map(drop)
    }

    pub async fn account_tokens(
        &self,
        session: &str,
        id: &str,
    ) -> Result<Vec<TokenView>, BackendError> {
        let path = format!("api/v1/service-accounts/{id}/tokens");
        self.call(Method::GET, &path, Some(session), None).await
    }

    pub async fn create_account_token(
        &self,
        session: &str,
        id: &str,
        name: Option<&str>,
        days: Option<i64>,
    ) -> Result<CreatedToken, BackendError> {
        let body = serde_json::json!({ "name": name, "expires_in_days": days });
        let path = format!("api/v1/service-accounts/{id}/tokens");
        self.call(Method::POST, &path, Some(session), Some(&body)).await
    }

    pub async fn revoke_account_token(
        &self,
        session: &str,
        id: &str,
        token: &str,
    ) -> Result<(), BackendError> {
        let path = format!("api/v1/service-accounts/{id}/tokens/{token}");
        self.call::<Value>(Method::DELETE, &path, Some(session), None).await.map(drop)
    }

    pub async fn account_holdings(
        &self,
        session: &str,
        id: &str,
    ) -> Result<Holdings, BackendError> {
        let path = format!("api/v1/service-accounts/{id}/permissions");
        self.call(Method::GET, &path, Some(session), None).await
    }

    pub async fn grant_account(
        &self,
        session: &str,
        id: &str,
        permission: &str,
    ) -> Result<(), BackendError> {
        let body = serde_json::json!({ "permission": permission });
        let path = format!("api/v1/service-accounts/{id}/permissions");
        self.call::<Value>(Method::POST, &path, Some(session), Some(&body)).await.map(drop)
    }

    pub async fn revoke_account(
        &self,
        session: &str,
        id: &str,
        permission: &str,
    ) -> Result<(), BackendError> {
        let encoded: String = url::form_urlencoded::byte_serialize(permission.as_bytes()).collect();
        let path = format!("api/v1/service-accounts/{id}/permissions/{encoded}");
        self.call::<Value>(Method::DELETE, &path, Some(session), None).await.map(drop)
    }

    pub async fn audit(&self, session: &str, query: &str) -> Result<AuditLog, BackendError> {
        self.call(Method::GET, &format!("api/v1/audit?{query}"), Some(session), None).await
    }

    /// Where the identity provider's start route sends the browser.
    pub async fn oauth_start(&self, provider: &str, query: &str) -> Result<String, BackendError> {
        let url = format!("{}api/v1/plugins/{provider}/public/oauth/start?{query}", self.base);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|err| BackendError::Unreachable(err.to_string()))?;
        let status = response.status().as_u16();
        let location =
            response.headers().get(http::header::LOCATION).and_then(|value| value.to_str().ok());
        match (status, location) {
            (300..=399, Some(location)) => Ok(location.to_string()),
            _ => Err(Self::failure(status, response.text().await.unwrap_or_default())),
        }
    }

    pub async fn oauth_callback(
        &self,
        provider: &str,
        query: &str,
    ) -> Result<Session, BackendError> {
        let path = format!("api/v1/plugins/{provider}/public/oauth/callback?{query}");
        self.call(Method::GET, &path, None, None).await
    }

    fn failure(status: u16, body: String) -> BackendError {
        match serde_json::from_str::<Problem>(&body) {
            Ok(problem) => BackendError::Problem(Box::new(problem)),
            Err(_) => BackendError::Unexpected { status, body },
        }
    }

    /// With the session's token when there is one, otherwise with the frontend's own.
    async fn call<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        session: Option<&str>,
        body: Option<&Value>,
    ) -> Result<T, BackendError> {
        let mut request = self.http.request(method, format!("{}{path}", self.base));
        if let Some(token) = session.or(self.token.as_ref().map(|token| token.expose().as_str())) {
            request = request.bearer_auth(token);
        }
        if let Some(parent) = doc_telemetry::traceparent() {
            request = request.header(doc_telemetry::TRACEPARENT, parent);
        }
        if let Some(chain) = crate::web::client::chain() {
            request = request.header("x-forwarded-for", chain);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response =
            request.send().await.map_err(|err| BackendError::Unreachable(err.to_string()))?;
        let status = response.status().as_u16();
        let body = response.text().await.map_err(|err| BackendError::Decode(err.to_string()))?;
        if (200..300).contains(&status) {
            let body = if body.is_empty() { "null" } else { &body };
            return serde_json::from_str(body).map_err(|err| BackendError::Decode(err.to_string()));
        }
        Err(Self::failure(status, body))
    }
}
