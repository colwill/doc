//! Versioned messages exchanged between plugins and the backend
//! backend serves `/plugin/v1/*` and a plugin serves `/host/v1/*`

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
pub use doc_secret::Secret;

pub mod data;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

/// Headers the backend puts on every call it makes to a plugin.
pub mod header {
    /// Short-lived token for this call; `/plugin/v1/services` refuses a request without it.
    pub const CONTEXT: &str = "x-doc-context";
    /// Compact JSON: who is calling, and the custom permissions and attributes they hold.
    pub const CALLER: &str = "x-doc-caller";
    /// How long is left. The SDK cancels the handler when it runs out.
    pub const DEADLINE_MS: &str = "x-doc-deadline-ms";
    pub const REQUEST_ID: &str = "x-request-id";
    pub const TRACEPARENT: &str = "traceparent";
    /// On a plugin's answer: the page shows faux data made up by the plugin this names, which the
    /// frontend says at the top of the page (FIX-FAUX-DATA).
    pub const FAUX: &str = "x-doc-faux";
}

/// What a plugin serves. The backend dials these.
pub mod host {
    pub const PREFIX: &str = "/host/v1";
    pub const HEALTH: &str = "/host/v1/health";
    pub const LOAD: &str = "/host/v1/load";
    pub const UNLOAD: &str = "/host/v1/unload";
    pub const RUN: &str = "/host/v1/run";
    pub const CANCEL: &str = "/host/v1/cancel";
    pub const EVENT: &str = "/host/v1/event";
    /// Answered `204`, after which the process closes its endpoint and exits with code 0.
    pub const EXIT: &str = "/host/v1/exit";
    /// `/host/v1/request/{*path}`: an API or UI route forwarded from the backend.
    pub const REQUEST_PREFIX: &str = "/host/v1/request/";
    /// Proposed settings for the plugin to say what it thinks of, before anything is stored
    /// (ADR-0007). Answering is optional: a plugin that does not is taken to have no objection.
    pub const SETTINGS_CHECK: &str = "/host/v1/settings/check";
    /// The keys that changed, after they were stored.
    pub const SETTINGS_CHANGED: &str = "/host/v1/settings/changed";
}

/// What the backend serves. Plugins dial these.
pub mod backend {
    pub const PREFIX: &str = "/plugin/v1";
    pub const REGISTER: &str = "/plugin/v1/register";
    pub const LIVENESS: &str = "/plugin/v1/liveness";
    pub const DATA: &str = "/plugin/v1/data";
    pub const EVENTS: &str = "/plugin/v1/events";
    pub const SERVICES: &str = "/plugin/v1/services";
    pub const CACHE: &str = "/plugin/v1/cache";
    pub const TASKS: &str = "/plugin/v1/tasks";
    pub const DELEGATIONS: &str = "/plugin/v1/delegations";
    pub const STATE: &str = "/plugin/v1/state";
    pub const AUDIT: &str = "/plugin/v1/audit";
    pub const STATUS: &str = "/plugin/v1/status";
    pub const IDENTITY: &str = "/plugin/v1/identity";
    pub const IDENTITY_LINK: &str = "/plugin/v1/identity/link";
    pub const USERS: &str = "/plugin/v1/users";
    pub const TEAMS: &str = "/plugin/v1/teams";
    /// An organisation made in DOC, for a plugin with the `team-writer` capability (T69).
    pub const ORGANISATION_WRITE: &str = "/plugin/v1/organisations/write";
    /// A team made in DOC, for a plugin with the `team-writer` capability (T69).
    pub const TEAM_WRITE: &str = "/plugin/v1/teams/write";
    pub const TEAM_MEMBERS: &str = "/plugin/v1/teams/members";
    pub const TEAM_REMOVE: &str = "/plugin/v1/teams/remove";
    /// The plugin's own settings and features, values and all (ADR-0007). Only the plugin that
    /// declared them may read them, which is what the registration token on the call decides.
    pub const SETTINGS: &str = "/plugin/v1/settings";
    /// An identity provider saying a person is gone from the directory it speaks for (T67).
    pub const DEPROVISION: &str = "/plugin/v1/identity/deprovision";
    /// What an offboarding rule asks core to do about somebody who has left (T67).
    pub const OFFBOARD: &str = "/plugin/v1/offboard";
    /// Asking an administrator to add this plugin to another plugin's requestable setting.
    pub const ACCESS_REQUESTS: &str = "/plugin/v1/access-requests";
    /// A scoped token for whoever is asking, for a plugin with the `token-issuer` capability.
    pub const SCOPED_TOKENS: &str = "/plugin/v1/tokens/scoped";
    /// Revoking a scoped token the plugin minted.
    pub const SCOPED_TOKEN_REVOKE: &str = "/plugin/v1/tokens/scoped/revoke";
    /// A value sealed under the platform's settings key, for the plugin with `secret-store`.
    pub const SEAL: &str = "/plugin/v1/seal";
    /// A value `SEAL` sealed, opened again.
    pub const OPEN: &str = "/plugin/v1/open";
    /// The secret store saying which of its secrets changed, so core tells the plugins using them.
    pub const SECRETS_CHANGED: &str = "/plugin/v1/secrets/changed";
    /// Somebody added by email address (FEAT-PEOPLE), for a `team-writer` acting for a person.
    pub const PEOPLE: &str = "/plugin/v1/people";
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Classification {
    /// `run` starts once after `load` and keeps going until `cancel` or `unload`.
    LongRunning,
    /// `run` happens once after each `load` and must return.
    OneShot,
    /// `run` is on demand and in the background; the caller gets a task ID.
    Async,
    /// `run` is on demand and the caller waits, within a deadline.
    Synchronous,
}

impl Classification {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LongRunning => "long-running",
            Self::OneShot => "one-shot",
            Self::Async => "async",
            Self::Synchronous => "synchronous",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PluginState {
    Loading,
    Running,
    Cancelled,
    Unloading,
    Error,
}

impl PluginState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Loading => "loading",
            Self::Running => "running",
            Self::Cancelled => "cancelled",
            Self::Unloading => "unloading",
            Self::Error => "error",
        }
    }

    pub fn serves_requests(self) -> bool {
        matches!(self, Self::Running | Self::Cancelled)
    }

    pub fn accepts_run(self) -> bool {
        self == Self::Running
    }

    /// The transitions §5 allows. Leaving the registry altogether is not a state, so it is not here.
    /// A plugin somebody turned off finishes loading as `cancelled`, never passing through
    /// `running`.
    pub fn may_become(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Loading, Self::Running | Self::Cancelled | Self::Error)
                | (Self::Running, Self::Cancelled | Self::Unloading | Self::Error)
                | (Self::Cancelled, Self::Running | Self::Unloading | Self::Error)
                | (Self::Unloading, Self::Error)
                | (Self::Error, Self::Loading | Self::Unloading)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Capability {
    PermissionProvider,
    IdentityProvider,
    /// Making and syncing teams, and the users in them, from a directory such as GitHub's (ADR-0004).
    TeamProvider,
    /// Making and changing the organisations and teams that were made in DOC, rather than a
    /// directory's, for a plugin that shows them as part of what it keeps, such as the Catalogue
    /// (T69). When somebody is asking, they must administer identity themselves.
    TeamWriter,
    /// Acting on somebody who has left: disabling them, taking their accounts, teams and tokens
    /// away (T67). A capability of its own, since it undoes access rather than granting it.
    Offboarding,
    /// Minting a scoped token for whoever is asking, limited to plugins' permissions they hold,
    /// for minutes (FEAT-VACUUM), and revoking the ones it minted.
    TokenIssuer,
    /// Keeping credentials for the rest of the platform (FEAT-SECRETS): sealing and opening values
    /// under the platform's settings key, and answering for the secrets other plugins' settings
    /// point at.
    SecretStore,
    /// Taking DOC's telemetry somewhere that keeps it — Grafana, InfluxDB, whatever an
    /// organisation already runs. DOC keeps a week of it and no more; a plugin with this
    /// capability is how a longer history is had, and it lets that week be raised, because
    /// what is kept here stops being the only copy.
    TelemetrySink,
    PublicRoutes,
    /// A service account of its own, which core makes when the plugin registers and which the
    /// plugin is checked as when it asks another plugin's `api/` as itself, rather than holding
    /// nothing (FEAT-AGENT: runbooks). It starts with no permissions: administrators grant it some
    /// in RBAC, as they would any service account.
    ServiceAccount,
}

/// The service account a plugin with the `service-account` capability acts as.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnAccount {
    /// As service accounts are named, such as `agent-smith`.
    pub name: String,
    /// What it is, as People and RBAC show it.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl OwnAccount {
    pub fn new(name: &str, description: &str) -> Self {
        Self { name: name.to_string(), description: description.to_string() }
    }
}

/// What a relayed call may not change, put on it by the plugin that asked and carried by core to
/// every call made while it is handled, so it cannot be stepped round by asking another plugin to
/// ask (FEAT-AGENT: runbooks). The stricter of two guards is the greater.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Guard {
    /// Nothing in production may change. A plugin that keeps things per environment refuses a
    /// change to one of the environments [`Caller::production`] names.
    NotProduction,
    /// Nothing may change: core refuses every route that writes.
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PermissionKind {
    PluginUser,
    PluginService,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomPermission {
    pub kind: PermissionKind,
    pub name: String,
    /// What holding it allows, in one line, as the plugin's Permissions tab shows it (ADR-0007).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
}

impl CustomPermission {
    pub fn user(name: &str) -> Self {
        Self {
            kind: PermissionKind::PluginUser,
            name: name.to_string(),
            description: String::new(),
        }
    }

    pub fn service(name: &str) -> Self {
        Self {
            kind: PermissionKind::PluginService,
            name: name.to_string(),
            description: String::new(),
        }
    }

    pub fn describes(mut self, description: &str) -> Self {
        self.description = description.to_string();
        self
    }
}

/// What a setting takes. Core checks a value against this before it is stored, so a plugin never
/// has to defend itself against a number where it declared a URL (ADR-0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SettingKind {
    Text,
    Number,
    Boolean,
    /// One of `one_of`.
    Choice,
    /// A list of lines, stored as an array of strings.
    List,
    Url,
    /// A span of time, such as `30s` or `2h`, stored as the seconds it comes to.
    Duration,
    /// A cron expression, checked against the same parser the schedules use.
    Cron,
    /// Written once and never read back: the page and every answer say only whether it is set.
    Secret,
    /// Keys, each with a list of values, such as a Jira project and the services it is for,
    /// stored as an object of arrays. Written `KEY=one,two`, one to a line or separated by `;`.
    Map,
}

impl SettingKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Number => "number",
            Self::Boolean => "boolean",
            Self::Choice => "choice",
            Self::List => "list",
            Self::Url => "url",
            Self::Duration => "duration",
            Self::Cron => "cron",
            Self::Secret => "secret",
            Self::Map => "map",
        }
    }

    pub fn is_secret(self) -> bool {
        self == Self::Secret
    }
}

/// One setting a plugin declares, which is what the platform builds its Settings page from.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Setting {
    /// `^[a-z][a-z0-9-]{0,63}$`, and the plugin's own name for it.
    pub key: String,
    /// What the field is called on the page; the key is used when it has none.
    pub label: String,
    /// One sentence under the field.
    pub hint: String,
    /// The heading it sits under, such as `Credentials`. Settings with no group come first.
    pub group: String,
    pub kind: SettingKind,
    pub default: Option<Value>,
    /// A plugin whose required settings are unset serves the routes that do not need them and
    /// says on its page what is missing, rather than failing.
    pub required: bool,
    /// For `number` and `duration` (seconds), and the length of `text` and `list` entries.
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// The choices, for `choice`.
    pub one_of: Vec<String>,
    /// A `GET` route under the plugin's `api/` that answers [`Choices`] when the Settings page is
    /// drawn, as whoever is looking: the options of a `choice`, the boxes to tick of a `list`, and
    /// the keys and values of a `map`. Core does not hold a value to them, since what is offered
    /// changes; the plugin passes over what it no longer knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choices: Option<String>,
    /// A regular expression the whole value must match, for `text`, `list` and `url`.
    pub pattern: Option<String>,
    /// The feature it belongs to. The field is still shown while the feature is off, so it can be
    /// filled in before the feature is turned on.
    pub feature: Option<String>,
    /// For a `list` of plugin IDs: another plugin may ask to be added, with `access-requests`, and
    /// an administrator approves or denies it (DOC-SPEC §9.15). Nothing else can be asked for.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub requestable: bool,
}

impl Default for Setting {
    fn default() -> Self {
        Self {
            key: String::new(),
            label: String::new(),
            hint: String::new(),
            group: String::new(),
            kind: SettingKind::Text,
            default: None,
            required: false,
            min: None,
            max: None,
            one_of: Vec::new(),
            choices: None,
            pattern: None,
            feature: None,
            requestable: false,
        }
    }
}

/// What a setting's `choices` route answers: what can be chosen, and for a `map`, what each key
/// can be given.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Choices {
    pub choices: Vec<Choice>,
    /// For a `map`: the values any key can be given.
    pub values: Vec<Choice>,
    /// For a `map`: what its keys and its values are called, such as `Jira project` and
    /// `Services`, above the columns they are chosen in.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub choices_label: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub values_label: String,
}

/// One thing that can be chosen: what is stored, and what the page calls it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Choice {
    pub value: String,
    pub label: String,
    /// A few words after it, such as where it comes from.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub hint: String,
}

impl Choice {
    pub fn new(value: &str, label: &str) -> Self {
        Self { value: value.to_string(), label: label.to_string(), hint: String::new() }
    }

    pub fn hinted(mut self, hint: &str) -> Self {
        self.hint = hint.to_string();
        self
    }
}

impl Setting {
    pub fn new(key: &str, label: &str, kind: SettingKind) -> Self {
        Self { key: key.to_string(), label: label.to_string(), kind, ..Self::default() }
    }

    pub fn text(key: &str, label: &str) -> Self {
        Self::new(key, label, SettingKind::Text)
    }

    pub fn secret(key: &str, label: &str) -> Self {
        Self::new(key, label, SettingKind::Secret)
    }

    pub fn hinted(mut self, hint: &str) -> Self {
        self.hint = hint.to_string();
        self
    }

    pub fn grouped(mut self, group: &str) -> Self {
        self.group = group.to_string();
        self
    }

    pub fn required(mut self) -> Self {
        self.required = true;
        self
    }

    pub fn defaulting(mut self, default: Value) -> Self {
        self.default = Some(default);
        self
    }

    pub fn between(mut self, min: f64, max: f64) -> Self {
        (self.min, self.max) = (Some(min), Some(max));
        self
    }

    pub fn one_of(mut self, choices: &[&str]) -> Self {
        self.one_of = choices.iter().map(|choice| (*choice).to_string()).collect();
        self.kind = SettingKind::Choice;
        self
    }

    pub fn matching(mut self, pattern: &str) -> Self {
        self.pattern = Some(pattern.to_string());
        self
    }

    /// Lets another plugin ask an administrator to be added to this list of plugin IDs.
    pub fn requestable(mut self) -> Self {
        self.requestable = true;
        self
    }

    /// Offers what can be chosen from the plugin's own `api/{route}`, answered when the page is
    /// drawn (see [`Choices`]).
    pub fn choices_from(mut self, route: &str) -> Self {
        self.choices = Some(route.trim_start_matches('/').to_string());
        self
    }

    /// Ties the setting to a feature, so the page shows it with that feature's other settings.
    pub fn of_feature(mut self, feature: &str) -> Self {
        self.feature = Some(feature.to_string());
        self
    }

    /// What the setting is worth before anyone sets it: its declared default, or the empty value
    /// of its kind, so a plugin reading a setting nobody touched gets something of the right shape.
    pub fn fallback(&self) -> Value {
        if let Some(default) = &self.default {
            return default.clone();
        }
        match self.kind {
            SettingKind::Boolean => Value::Bool(false),
            SettingKind::Number | SettingKind::Duration => Value::Null,
            SettingKind::List => Value::Array(Vec::new()),
            SettingKind::Map => Value::Object(serde_json::Map::new()),
            _ => Value::String(String::new()),
        }
    }
}

/// A plugin whose credentials are named at runtime — one per source, per vendor, per account —
/// declares this instead of a secret setting for each (ADR-0007). An administrator adds them by
/// name on the Settings page; core keeps them encrypted like any other secret, and only this
/// plugin is ever given their values.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamedSecrets {
    /// The heading the page puts them under, such as `Credentials`.
    pub label: String,
    /// One sentence saying what a name means here, since the plugin decides that.
    pub hint: String,
}

impl NamedSecrets {
    pub fn new(label: &str, hint: &str) -> Self {
        Self { label: label.to_string(), hint: hint.to_string() }
    }
}

/// A named switch on a plugin: what it does, and whether it starts on (ADR-0007). A schedule may
/// name the feature it belongs to, and is not run while that feature is off.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Feature {
    /// `^[a-z][a-z0-9-]{0,31}$`.
    pub name: String,
    pub label: String,
    pub description: String,
    /// Whether it is on until somebody turns it off.
    pub default: bool,
    /// Shown on the Features tab beside the switch, for a feature that needs saying twice about.
    pub warning: String,
    /// Other plugins' features this one works from. The plugins page offers to turn it on with
    /// them in one go, on whichever of the named plugins is running and configured.
    pub needs: Vec<Need>,
}

/// A feature of another plugin that one of this plugin's features works from, such as DORA's
/// metrics needing GitHub's delivery data: any one of `plugins` with `feature` on will do.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Need {
    pub plugins: Vec<String>,
    pub feature: String,
}

impl Feature {
    pub fn new(name: &str, label: &str, description: &str) -> Self {
        Self {
            name: name.to_string(),
            label: label.to_string(),
            description: description.to_string(),
            ..Self::default()
        }
    }

    /// On until somebody turns it off.
    pub fn on(mut self) -> Self {
        self.default = true;
        self
    }

    pub fn warning(mut self, warning: &str) -> Self {
        self.warning = warning.to_string();
        self
    }

    /// Works from `feature` of any one of `plugins`, which the plugins page turns on with it.
    pub fn needs(mut self, plugins: &[&str], feature: &str) -> Self {
        self.needs.push(Need {
            plugins: plugins.iter().map(|plugin| (*plugin).to_string()).collect(),
            feature: feature.to_string(),
        });
        self
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nav {
    pub label: String,
    pub path: String,
    /// One sentence on what the page is for, shown on its card on the landing page.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The menu it sits in until an administrator arranges the navigation, such as `Catalogue`.
    /// An entry with none of its own stands on its own in the bar, so a new plugin is not hidden.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Known to core for access and breadcrumbs, but not drawn as a link in the bar: for a page
    /// reached another way, such as a header icon of its own, that still wants its place in the
    /// trail.
    #[serde(default, skip_serializing_if = "is_false")]
    pub hidden: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl Nav {
    pub fn new(label: &str, path: &str) -> Self {
        Self {
            label: label.to_string(),
            path: path.to_string(),
            description: None,
            group: None,
            hidden: false,
        }
    }

    pub fn described(mut self, description: &str) -> Self {
        self.description = Some(description.to_string());
        self
    }

    /// Puts the entry in a menu. Core's own are `Platform`, `Access`, `Catalogue`, `Workspace`
    /// and `Help`; naming one of those joins it, and any other name makes a menu of its own.
    pub fn grouped(mut self, group: &str) -> Self {
        self.group = Some(group.to_string());
        self
    }

    /// Reached its own way rather than the bar, but still counted for access and breadcrumbs.
    pub fn hidden(mut self) -> Self {
        self.hidden = true;
        self
    }
}

/// How an identity provider signs people in, which the sign-in page follows (DOC-SPEC §9.9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SignInKind {
    /// Off to the provider and back: `public/oauth/start`, then `public/oauth/callback`.
    Redirect,
    /// A username and password on the sign-in page, sent to `public/sign-in`; a one-time password
    /// is changed at `public/password`.
    Password,
}

/// What the sign-in page shows for an identity provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignIn {
    /// What people know it as, such as `GitHub` or `Acme single sign-on`.
    pub title: String,
    pub kind: SignInKind,
    /// The feature that decides whether the provider is offered at all (ADR-0007). A plugin that
    /// can sign people in once it is configured declares its sign-in here and names the feature,
    /// and nobody is offered it until an administrator turns that feature on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
    /// A `text` setting whose value replaces `title` when it is set, so what the sign-in page
    /// calls this provider — "Acme single sign-on" — is changed in DOC rather than in a file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setting: Option<String>,
}

impl SignIn {
    pub fn new(title: &str, kind: SignInKind) -> Self {
        Self { title: title.to_string(), kind, feature: None, setting: None }
    }

    /// Offered only while this feature is on.
    pub fn of_feature(mut self, feature: &str) -> Self {
        self.feature = Some(feature.to_string());
        self
    }

    /// What it is called may be changed on the Settings page, through this `text` setting.
    pub fn from_setting(mut self, setting: &str) -> Self {
        self.setting = Some(setting.to_string());
        self
    }
}

/// A panel this plugin adds to another plugin's resource pages, loaded by HTMX (T32).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourcePanel {
    pub resource: String,
    pub label: String,
    pub path: String,
    /// Where it sits among the panels on that page: lowest first, and panels that say nothing
    /// come before those that do, in the order of the plugins they belong to. A panel that reads
    /// best after whatever else the page has to say — a summary of what the resource *is* —
    /// asks to come later.
    #[serde(default, skip_serializing_if = "is_first")]
    pub order: i32,
}

fn is_first(order: &i32) -> bool {
    *order == 0
}

impl ResourcePanel {
    pub fn new(resource: &str, label: &str, path: &str) -> Self {
        Self {
            resource: resource.to_string(),
            label: label.to_string(),
            path: path.to_string(),
            order: 0,
        }
    }

    /// Put it after the panels that ask for nothing, and before any that ask for more.
    pub fn after(mut self, order: i32) -> Self {
        self.order = order;
        self
    }
}

/// One measured fact about a resource, small enough to read at a glance: a pipeline's success
/// rate, how often a service is deployed, how mature it is.
///
/// A panel is the whole of what a plugin has to say about a resource, and a page of them is a page
/// nobody reads. An insight is one number from it, which somebody may **pin** to the top of a
/// resource's page in the Catalogue. Declaring one grants nothing: `path` is a route the plugin
/// already serves under `ui/`, asked as whoever is looking, and a viewer who may not read the
/// plugin is never offered its insights at all.
///
/// The route is given `?resource=<kind>:<name>` and answers a fragment: a value big enough to read
/// across a room, and as little else as says what it means — or, for a `wide` one, the picture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Insight {
    /// What it is about, as a URL names a kind: `service`, `repository`.
    pub resource: String,
    /// Its name within this plugin, which with the plugin's id is how a pin names it.
    pub id: String,
    /// What it is called where somebody chooses it, such as "Deployment frequency". It is read
    /// away from the plugin that offers it, so it says what it measures rather than where from.
    pub label: String,
    pub path: String,
    /// A sentence for whoever is choosing what to pin, where the label does not say enough.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// A picture rather than a figure, such as a diagram: drawn across the page below the row of
    /// figures rather than as one of them, and never above them whatever order it is pinned in.
    /// Its route answers with its own heading as well, so the heading can link to whatever it is a
    /// picture of.
    #[serde(default, skip_serializing_if = "is_false")]
    pub wide: bool,
}

impl Insight {
    pub fn new(resource: &str, id: &str, label: &str, path: &str) -> Self {
        Self {
            resource: resource.to_string(),
            id: id.to_string(),
            label: label.to_string(),
            path: path.to_string(),
            description: String::new(),
            wide: false,
        }
    }

    pub fn described(mut self, description: &str) -> Self {
        self.description = description.to_string();
        self
    }

    /// Drawn across the page below the pinned figures, as a picture is.
    pub fn wide(mut self) -> Self {
        self.wide = true;
        self
    }
}

/// Something a person may put on their dashboard, the page they land on when they sign in: a few
/// lines about them, at a glance, such as what is in their inbox or when they are next on call. It
/// is a route the plugin already serves under `ui/`, asked as whoever is looking, and a viewer who
/// may not read the plugin is never offered its items.
///
/// The route answers a fragment: at most a handful of rows, each linking to where it is dealt with,
/// a link to the rest, and a line saying so when there is nothing, rather than an empty box.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DashboardItem {
    /// Its name within this plugin, which with the plugin's id is how a dashboard names it.
    pub id: String,
    /// Its heading on the dashboard and where it is chosen, such as "Your inbox".
    pub label: String,
    pub path: String,
    /// A sentence for whoever is choosing what to see.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    /// Across the whole dashboard rather than one column of it.
    #[serde(default, skip_serializing_if = "is_false")]
    pub wide: bool,
}

impl DashboardItem {
    pub fn new(id: &str, label: &str, path: &str) -> Self {
        Self {
            id: id.to_string(),
            label: label.to_string(),
            path: path.to_string(),
            description: String::new(),
            wide: false,
        }
    }

    pub fn described(mut self, description: &str) -> Self {
        self.description = description.to_string();
        self
    }

    /// Across the whole dashboard.
    pub fn wide(mut self) -> Self {
        self.wide = true;
        self
    }
}

/// Something this plugin offers automations to call by name, such as `dora`'s `increment`
/// (ADR-0012). It is a route the plugin already serves under `api/`, called as the automation's
/// owner, so declaring it grants nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operation {
    pub name: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default = "posting")]
    pub method: String,
    /// Under the plugin's `api/`; `{name}` is filled in with that parameter, URL-encoded.
    pub route: String,
    #[serde(default)]
    pub params: Vec<OperationParam>,
}

fn posting() -> String {
    "POST".into()
}

/// One value an operation is called with. Those not in its route go in the body, or in the query
/// for `GET` and `DELETE`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationParam {
    pub name: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hint: String,
    #[serde(default)]
    pub required: bool,
}

impl Operation {
    pub fn new(name: &str, label: &str, route: &str) -> Self {
        Self {
            name: name.to_string(),
            label: label.to_string(),
            description: String::new(),
            method: posting(),
            route: route.to_string(),
            params: Vec::new(),
        }
    }

    pub fn described(mut self, description: &str) -> Self {
        self.description = description.to_string();
        self
    }

    pub fn method(mut self, method: &str) -> Self {
        self.method = method.to_ascii_uppercase();
        self
    }

    pub fn param(mut self, param: OperationParam) -> Self {
        self.params.push(param);
        self
    }
}

impl OperationParam {
    /// A parameter the operation cannot be called without.
    pub fn required(name: &str, label: &str) -> Self {
        Self {
            name: name.to_string(),
            label: label.to_string(),
            hint: String::new(),
            required: true,
        }
    }

    pub fn optional(name: &str, label: &str) -> Self {
        Self { required: false, ..Self::required(name, label) }
    }

    pub fn hinted(mut self, hint: &str) -> Self {
        self.hint = hint.to_string();
        self
    }
}

/// A cron schedule, in UTC. Each time it comes round, a background task calls `run` with
/// `{"schedule": name}`, as the plugin itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Schedule {
    pub name: String,
    pub cron: String,
    #[serde(default)]
    pub description: String,
    /// The feature this work belongs to. While that feature is off the schedule is not recorded,
    /// so nothing it drives runs (ADR-0007).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
    /// A `cron` setting whose value replaces `cron` when it is set, so how often the work runs is
    /// something an administrator changes on the Settings page rather than in a deployment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub setting: Option<String>,
}

impl Schedule {
    pub fn new(name: &str, cron: &str, description: &str) -> Self {
        Self {
            name: name.into(),
            cron: cron.into(),
            description: description.into(),
            feature: None,
            setting: None,
        }
    }

    pub fn of_feature(mut self, feature: &str) -> Self {
        self.feature = Some(feature.to_string());
        self
    }

    /// How often it runs may be changed on the Settings page, through this `cron` setting.
    pub fn from_setting(mut self, setting: &str) -> Self {
        self.setting = Some(setting.to_string());
        self
    }
}

/// What a plugin says about itself when it registers. Everything here is checked by the backend in
/// T20 before the plugin is allowed to load.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Manifest {
    pub id: String,
    pub version: String,
    pub classification: Classification,
    pub custom_permissions: Vec<CustomPermission>,
    pub nav: Vec<Nav>,
    pub capabilities: Vec<Capability>,
    /// Paths under `public/` needing no sign-in (`/*` covers below); only with `public-routes`.
    pub public_routes: Vec<String>,
    /// Paths under `api/` whose `POST` only reads, such as an MCP endpoint (`/*` covers below).
    pub read_routes: Vec<String>,
    pub resource_panels: Vec<ResourcePanel>,
    /// Single facts about a resource that somebody may pin to the top of its page.
    #[serde(default)]
    pub insights: Vec<Insight>,
    /// What a person may put on their dashboard, the page they land on.
    #[serde(default)]
    pub dashboard: Vec<DashboardItem>,
    /// Topic filters this plugin wants delivered to `on_event`.
    pub subscriptions: Vec<String>,
    pub schedules: Vec<Schedule>,
    /// The collections the plugin keeps, which the backend stores (DOC-SPEC §4.3).
    pub data: data::Declaration,
    /// Providers a caller needs an account with, linked to their DOC user, for what this plugin
    /// does as them, such as `github`. People are offered each link after their first sign-in, and
    /// [`Caller::linked`] says which they have.
    pub linked_accounts: Vec<String>,
    /// An identity provider's sign-in, as the sign-in page offers it.
    pub sign_in: Option<SignIn>,
    /// What the platform builds this plugin's Settings page from (ADR-0007). Values belong to the
    /// plugin rather than a version, so a hot reload keeps them and a new setting starts at its
    /// default.
    pub settings: Vec<Setting>,
    /// The switches on its Settings page's Features tab.
    pub features: Vec<Feature>,
    /// Credentials this plugin holds by name rather than by declared key (ADR-0007).
    pub named_secrets: Option<NamedSecrets>,
    /// What automations can call by name (ADR-0012).
    pub operations: Vec<Operation>,
    /// The service account it acts as when it asks as itself; only with `service-account`.
    pub service_account: Option<OwnAccount>,
}

impl Default for Manifest {
    fn default() -> Self {
        Self {
            id: String::new(),
            version: String::new(),
            classification: Classification::Synchronous,
            custom_permissions: Vec::new(),
            nav: Vec::new(),
            capabilities: Vec::new(),
            public_routes: Vec::new(),
            read_routes: Vec::new(),
            resource_panels: Vec::new(),
            insights: Vec::new(),
            dashboard: Vec::new(),
            subscriptions: Vec::new(),
            schedules: Vec::new(),
            data: data::Declaration::default(),
            linked_accounts: Vec::new(),
            sign_in: None,
            settings: Vec::new(),
            features: Vec::new(),
            named_secrets: None,
            operations: Vec::new(),
            service_account: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterRequest {
    pub manifest: Manifest,
    /// Where the backend should dial this instance, as `host:port`.
    pub address: String,
    /// SHA-256 of the running binary. A version already seen with a different hash is refused.
    pub binary_sha256: String,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterResponse {
    /// Proves a later `/host/v1/*` call came from the backend. Never logged.
    #[serde(serialize_with = "doc_secret::exposed")]
    pub secret: Secret<String>,
    /// The state the last `unload` of any version returned, for handover.
    #[serde(default)]
    pub previous: Option<Value>,
    pub state: PluginState,
    #[serde(default)]
    pub liveness_ms: Option<u64>,
    /// This registration, carried by liveness reports so a replaced process can be told so.
    #[serde(default)]
    pub instance: Option<Uuid>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Liveness {
    pub id: String,
    pub state: PluginState,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub instance: Option<Uuid>,
}

/// Who the backend is acting for. Core has already checked `user` and `service` access; the custom
/// permissions here are the plugin's own to check.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Caller {
    /// `user`, `service`, `plugin`, `platform` (core, on `internal/*`) or `anonymous` (`public/*`).
    pub kind: String,
    pub id: Option<String>,
    pub label: Option<String>,
    /// A platform admin passes every check, including the plugin's own.
    pub admin: bool,
    /// The caller's own `user` or `service` scope on this plugin: `ro`, `rw` or `wo`.
    pub scope: Option<String>,
    /// Custom permission name to scope, as `ro`, `rw` or `wo`.
    pub custom: BTreeMap<String, String>,
    pub attributes: BTreeMap<String, String>,
    /// A user's linked accounts, as provider to login, such as `github` to `octocat`.
    pub linked: BTreeMap<String, String>,
    /// The plugin that relayed this call for whoever it is, such as `agent`; none when they asked
    /// themselves. Audit entries written while it is handled say so too.
    pub via: Option<String>,
    /// What this call may not change, when whoever relayed it limited itself.
    pub guard: Option<Guard>,
    /// The environments that count as production (`[environments]`), given with a guard.
    pub production: Vec<String>,
}

impl Caller {
    /// Whether the guard on this call lets it change something in `environment`; `None` is every
    /// environment, which includes production.
    pub fn may_change(&self, environment: Option<&str>) -> bool {
        match (self.guard, environment.map(str::trim)) {
            (None, _) => true,
            (Some(Guard::ReadOnly), _) | (Some(Guard::NotProduction), None | Some("")) => false,
            (Some(Guard::NotProduction), Some(environment)) => {
                !self.production.iter().any(|name| name.eq_ignore_ascii_case(environment))
            }
        }
    }

    /// Whether core would let this caller write to the plugin, so a page can leave out what it can't.
    pub fn writes(&self) -> bool {
        self.admin || matches!(self.scope.as_deref(), Some("rw" | "wo"))
    }

    /// The check a plugin makes for its own permissions. Scopes follow §6: `ro` reads, `wo` writes.
    pub fn allows(&self, permission: &str, write: bool) -> bool {
        if self.admin {
            return true;
        }
        match self.custom.get(permission).map(String::as_str) {
            Some("rw") => true,
            Some("ro") => !write,
            Some("wo") => write,
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct LoadRequest {
    pub previous: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UnloadResponse {
    /// Handed to the next version's `load`, so a hot reload keeps its place.
    pub state: Option<Value>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunInput {
    /// Set when this run came from a background task, so the plugin can report against it.
    pub task: Option<Uuid>,
    pub payload: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct RunOutput {
    pub payload: Value,
}

/// The Event Bus envelope from T07, repeated here so a plugin does not have to link the bus.
/// Delivery is at least once, so a plugin must treat `id` as the thing to be idempotent about.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub topic: String,
    pub source: String,
    pub at: DateTime<Utc>,
    #[serde(default)]
    pub correlation_id: Option<Uuid>,
    #[serde(default)]
    pub schema_version: Option<u32>,
    pub payload: Value,
}

/// RFC 9457, with the plugin and version added so a failure can be traced to what produced it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Problem {
    #[serde(rename = "type")]
    pub kind: String,
    pub title: String,
    pub status: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plugin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl Problem {
    pub fn new(status: u16, kind: &str, title: &str) -> Self {
        Self {
            kind: format!("/problems/{kind}"),
            title: title.to_string(),
            status,
            detail: None,
            plugin: None,
            version: None,
        }
    }

    pub fn detail(mut self, detail: impl Into<String>) -> Self {
        self.detail = Some(detail.into());
        self
    }

    pub fn about(mut self, plugin: &str, version: &str) -> Self {
        self.plugin = Some(plugin.to_string());
        self.version = Some(version.to_string());
        self
    }
}

/// `^[a-z][a-z0-9-]{0,31}$`, the same shape a permission's plugin segment takes.
pub fn valid_plugin_id(id: &str) -> bool {
    let mut chars = id.chars();
    id.len() <= 32
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// IDs the platform keeps for itself, so a plugin cannot claim to be core.
pub const RESERVED_IDS: &[&str] = &["core", "backend", "frontend", "workers", "platform", "plugin"];

/// `major.minor.patch` with optional `-pre` and `+build`, which is as much of semver as the
/// platform needs: versions are compared for equality, never ordered.
pub fn valid_version(version: &str) -> bool {
    let core = version.split(['-', '+']).next().unwrap_or_default();
    let mut parts = core.split('.');
    let numeric = |part: Option<&str>| {
        part.is_some_and(|part| {
            !part.is_empty()
                && part.chars().all(|c| c.is_ascii_digit())
                && (part == "0" || !part.starts_with('0'))
        })
    };
    numeric(parts.next())
        && numeric(parts.next())
        && numeric(parts.next())
        && parts.next().is_none()
        && !version.contains(char::is_whitespace)
}

/// The bodies of the backend API calls in MVP §5. The backend implements these in T21; they are
/// defined here so both sides compile against one description of the contract.
pub mod calls {
    use super::*;

    /// Asks for the calling plugin to be added to `plugin`'s requestable list `setting`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AccessRequest {
        pub plugin: String,
        pub setting: String,
        /// Why, in a sentence the administrator reads.
        #[serde(default)]
        pub reason: String,
    }

    /// Where a request stands: `granted` (the plugin is already in the list), `pending`, `approved`
    /// or `denied`.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AccessAnswer {
        pub state: String,
        #[serde(default)]
        pub id: Option<Uuid>,
        /// Whether this call raised it, and so notified the administrators.
        #[serde(default)]
        pub raised: bool,
        #[serde(default)]
        pub decided_by: Option<String>,
        #[serde(default)]
        pub decided_at: Option<DateTime<Utc>>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PublishRequest {
        /// Must start with this plugin's own `plugin.<id>.` prefix.
        pub topic: String,
        pub payload: Value,
        #[serde(default)]
        pub idempotency_key: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct PublishResponse {
        pub id: Uuid,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ServiceRequest {
        pub address: String,
        pub subject: String,
        pub payload: Value,
        #[serde(default)]
        pub deadline_ms: Option<u64>,
        /// A queue message rather than a request, so no reply is waited for.
        #[serde(default)]
        pub queue: bool,
        /// What a relayed `api/` call may not change; core keeps the stricter of this and the
        /// guard on the call being handled.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub guard: Option<Guard>,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct ServiceResponse {
        pub payload: Value,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "kebab-case")]
    pub enum CacheRequest {
        Get {
            key: String,
        },
        Set {
            key: String,
            value: Value,
            #[serde(default)]
            ttl_ms: Option<u64>,
        },
        Delete {
            key: String,
        },
        /// `version` is what the key must be at; `None` means it must not exist yet.
        CompareAndSet {
            key: String,
            value: Value,
            #[serde(default)]
            version: Option<u64>,
            #[serde(default)]
            ttl_ms: Option<u64>,
        },
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct CacheResponse {
        pub value: Option<Value>,
        pub version: Option<u64>,
        /// False when a compare-and-set lost, which is not an error.
        pub applied: bool,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct TaskRequest {
        pub payload: Value,
        #[serde(default)]
        pub max_attempts: Option<i32>,
        /// Started by whoever this delegation is from, rather than by whoever the call is for.
        #[serde(default, rename = "as", skip_serializing_if = "Option::is_none")]
        pub delegation: Option<Uuid>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct TaskResponse {
        pub task: Uuid,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "kebab-case")]
    pub enum DelegationRequest {
        Grant { purpose: String },
        Revoke { delegation: Uuid },
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct DelegationResponse {
        pub delegation: Option<Uuid>,
        pub revoked: Option<bool>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    #[serde(tag = "op", rename_all = "kebab-case")]
    pub enum StateRequest {
        Get { key: String },
        Set { key: String, value: Value },
        Delete { key: String },
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct StateResponse {
        pub value: Option<Value>,
    }

    /// What the platform's configuration says of the instance every plugin runs in (`[instance]`):
    /// the same for every plugin, given with its settings. Empty where nothing is configured.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(default)]
    pub struct Instance {
        /// The domain people reach it at, such as `doc.acme.com`.
        pub domain: String,
        /// The IP address of the machine it runs on, as other machines reach it.
        pub address: String,
    }

    /// What a plugin's settings come to now (ADR-0007). Every declared setting is here, at its
    /// stored value or its default, so a plugin never has to know which is which. A secret is
    /// carried apart, as a [`Secret`], and only ever to the plugin that declared it.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct SettingsView {
        pub values: BTreeMap<String, Value>,
        #[serde(serialize_with = "exposed_secrets")]
        pub secrets: BTreeMap<String, Secret<String>>,
        /// The credentials an administrator added by name, for a plugin that holds them that way.
        #[serde(serialize_with = "exposed_secrets")]
        pub named: BTreeMap<String, Secret<String>>,
        pub features: BTreeMap<String, bool>,
        /// Required settings with nothing set, so a plugin can say so rather than guess.
        pub missing: Vec<String>,
        pub instance: Instance,
    }

    /// The secrets a plugin reads go over its own authenticated connection, so they are written
    /// out here; nothing else serialises a [`Secret`] by accident.
    fn exposed_secrets<S: serde::Serializer>(
        secrets: &BTreeMap<String, Secret<String>>,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(secrets.len()))?;
        for (key, secret) in secrets {
            map.serialize_entry(key, secret.expose())?;
        }
        map.end()
    }

    /// Settings a person has typed but nothing has stored, for the plugin to object to. Secrets
    /// are included, since checking a credential is the point of asking.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct SettingsCheck {
        pub values: BTreeMap<String, Value>,
        #[serde(serialize_with = "exposed_secrets")]
        pub secrets: BTreeMap<String, Secret<String>>,
        #[serde(serialize_with = "exposed_secrets")]
        pub named: BTreeMap<String, Secret<String>>,
        pub features: BTreeMap<String, bool>,
        pub instance: Instance,
    }

    /// What the plugin thinks of them: a problem against a key puts the message on that field, and
    /// `problem` is about the settings as a whole, such as a credential the server refused.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct SettingsVerdict {
        pub problems: BTreeMap<String, String>,
        pub problem: Option<String>,
        /// Shown when everything is well, such as "Signed in to GitHub as doc-bot".
        pub message: Option<String>,
    }

    impl SettingsVerdict {
        pub fn ok() -> Self {
            Self::default()
        }

        pub fn saying(message: &str) -> Self {
            Self { message: Some(message.to_string()), ..Self::default() }
        }

        pub fn wrong(key: &str, problem: &str) -> Self {
            let problems = BTreeMap::from([(key.to_string(), problem.to_string())]);
            Self { problems, ..Self::default() }
        }

        pub fn refused(problem: &str) -> Self {
            Self { problem: Some(problem.to_string()), ..Self::default() }
        }

        pub fn is_ok(&self) -> bool {
            self.problems.is_empty() && self.problem.is_none()
        }
    }

    /// The keys whose values changed, after they were stored. Never the values themselves: the
    /// plugin reads them back over its own connection.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct SettingsChanged {
        pub keys: Vec<String>,
        pub features: Vec<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct AuditRequest {
        pub action: String,
        #[serde(default)]
        pub subject: Option<String>,
        #[serde(default)]
        pub detail: Value,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct StatusRequest {
        pub state: PluginState,
        #[serde(default)]
        pub error: Option<String>,
    }

    /// Identity providers only: creates or updates a user and starts a session for them. The
    /// profile fields are what the provider knows of the person now; core keeps the latest.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct IdentityRequest {
        pub provider: String,
        pub external_id: String,
        pub login: String,
        #[serde(default)]
        pub name: Option<String>,
        #[serde(default)]
        pub email: Option<String>,
        #[serde(default)]
        pub first_name: Option<String>,
        #[serde(default)]
        pub surname: Option<String>,
        /// What onboarding rules match on; teams are written `<organisation>/<team>`.
        #[serde(default)]
        pub organisations: Vec<String>,
        #[serde(default)]
        pub teams: Vec<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct IdentityResponse {
        pub user_id: Uuid,
        #[serde(serialize_with = "doc_secret::exposed")]
        pub session_token: Secret<String>,
        #[serde(default)]
        pub expires_at: Option<DateTime<Utc>>,
        /// This user's first sign-in, after which the frontend offers the accounts plugins need.
        #[serde(default)]
        pub first: bool,
    }

    /// Core asks an identity provider's `internal/link` route to start linking an account to
    /// someone signed in. The provider keeps `ticket` through its sign-in and hands it back in a
    /// [`LinkRequest`]; whoever holds a ticket can link an account to its user, so it goes no
    /// further, and never into a URL.
    pub const LINK_ROUTE: &str = "link";

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct LinkStart {
        #[serde(serialize_with = "doc_secret::exposed")]
        pub ticket: Secret<String>,
        /// A path on DOC to finish on, as with a sign-in's `return_to`.
        #[serde(default)]
        pub return_to: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct LinkStarted {
        /// Where to send the browser to sign in to the provider.
        pub location: String,
    }

    /// Identity providers only: links an account to the user who asked core for `ticket`, rather
    /// than signing anyone in. The provider hands the ticket through its sign-in untouched.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct LinkRequest {
        #[serde(serialize_with = "doc_secret::exposed")]
        pub ticket: Secret<String>,
        pub provider: String,
        pub external_id: String,
        pub login: String,
        #[serde(default)]
        pub name: Option<String>,
        #[serde(default)]
        pub email: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct LinkResponse {
        pub user_id: Uuid,
        /// A user who had never signed in and held this account, merged into `user_id`.
        #[serde(default)]
        pub merged: Option<Uuid>,
    }

    /// Identity and team providers only: the user an account of theirs belongs to, made with that
    /// account if there is none, such as for a directory's members who have not signed in yet.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct UserRequest {
        pub provider: String,
        pub external_id: String,
        pub login: String,
        #[serde(default)]
        pub name: Option<String>,
        #[serde(default)]
        pub email: Option<String>,
        #[serde(default)]
        pub first_name: Option<String>,
        #[serde(default)]
        pub surname: Option<String>,
        /// The name of the organisation a new user belongs to. Without it, they join the
        /// organisation that signs in with this provider.
        #[serde(default)]
        pub organisation: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct UserResponse {
        pub user_id: Uuid,
        pub created: bool,
    }

    /// Team providers only: a team the provider keeps in core, made or brought up to date. Its key
    /// is `external_id`, such as `acme/platform`, and its `parent` another of the provider's teams
    /// by that key, in the same organisation.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct TeamRequest {
        /// The name of the DOC organisation it is in, which must exist.
        pub organisation: String,
        pub external_id: String,
        pub name: String,
        pub title: String,
        #[serde(default)]
        pub description: String,
        #[serde(default)]
        pub parent: Option<String>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct TeamResponse {
        pub team_id: Uuid,
        pub created: bool,
    }

    /// With the `team-writer` capability: an organisation made in DOC, by name. What it is called
    /// and what it says are brought up to date; nothing else about it is touched.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct OrganisationRequest {
        pub name: String,
        pub title: String,
        #[serde(default)]
        pub description: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct OrganisationResponse {
        pub organisation_id: Uuid,
        pub created: bool,
    }

    /// With the `token-issuer` capability: a scoped token for whoever is asking, which is shown
    /// here once and never stored. `scopes` are plugins' user permissions, such as
    /// `plugin:kb:user:ro`.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct ScopedTokenRequest {
        pub name: String,
        pub scopes: Vec<String>,
        #[serde(default)]
        pub expires_in_minutes: Option<i64>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct ScopedTokenResponse {
        pub id: Uuid,
        #[serde(serialize_with = "doc_secret::exposed")]
        pub token: Secret<String>,
        pub scopes: Vec<String>,
        pub expires_at: DateTime<Utc>,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct RevokeScopedTokenRequest {
        pub id: Uuid,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct RevokeScopedTokenResponse {
        pub revoked: bool,
    }

    /// With the `team-writer` capability: a team made in DOC, by name in its organisation. A team
    /// a provider keeps is left as the provider has it, and only its address is written here.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct WriteTeamRequest {
        /// The organisation's name; empty for the one the plugin signs in for, or the only one.
        #[serde(default)]
        pub organisation: String,
        pub name: String,
        pub title: String,
        #[serde(default)]
        pub description: String,
        #[serde(default)]
        pub email: String,
        /// The name of the team it sits inside, in the same organisation.
        #[serde(default)]
        pub parent: Option<String>,
    }

    /// Team providers only: everyone the provider says is in one of its teams, as DOC user IDs.
    /// Only the memberships the provider made change; people added in DOC stay.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct TeamMembersRequest {
        pub external_id: String,
        pub users: Vec<Uuid>,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct TeamMembersResponse {
        pub added: Vec<Uuid>,
        pub removed: Vec<Uuid>,
    }

    /// Team providers only: a team the provider no longer has. It goes, unless something made in
    /// DOC depends on it, such as people added by hand, a sub-team or a service account it owns;
    /// then it stays as a team made in DOC.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct TeamRemoveRequest {
        pub external_id: String,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct TeamRemoveResponse {
        pub removed: bool,
        /// It stays, as a team made in DOC.
        pub released: bool,
    }

    /// An identity provider saying somebody is gone from the directory it speaks for. Core does
    /// not act on it by itself: it says so, and the offboarding rules decide what follows (T67).
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct DeprovisionRequest {
        /// The provider's own ID for them, which is what their account here is known by.
        pub external_id: String,
        /// Why they are gone, for the audit log: `left`, `suspended`, whatever the directory says.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct DeprovisionResponse {
        /// Nothing at all when no account here matches, which is not an error: a directory may
        /// speak of people this platform never knew.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub user_id: Option<Uuid>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub login: Option<String>,
    }

    /// What an offboarding rule asks core to do about somebody who has left. Every part is
    /// optional, so a rule does as much or as little as it says.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct OffboardRequest {
        pub user: Uuid,
        /// They can no longer sign in, whatever they hold.
        #[serde(default)]
        pub disable: bool,
        /// The account with this provider is taken away, so signing in with it makes a new user
        /// rather than reaching this one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub remove_identity: Option<String>,
        /// The team memberships this provider gave them; what an administrator added stays.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub remove_provided_memberships: Option<String>,
        /// Every team membership, however it came about.
        #[serde(default)]
        pub remove_memberships: bool,
        /// Their personal access tokens and sessions stop working at once.
        #[serde(default)]
        pub revoke_tokens: bool,
        /// Written to the audit log beside what was done.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        pub reason: Option<String>,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    pub struct OffboardResponse {
        pub disabled: bool,
        pub identities_removed: usize,
        pub memberships_removed: usize,
        pub tokens_revoked: usize,
    }

    /// With the `secret-store` capability: a value to seal. `label` names the record it belongs to
    /// and is bound in with the plugin's ID, so the sealed value opens under both and nothing else.
    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SealRequest {
        pub label: String,
        #[serde(serialize_with = "doc_secret::exposed")]
        pub value: Secret<String>,
    }

    /// A sealed value as the plugin keeps it: the key it was sealed under, and base64 for the rest.
    #[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
    pub struct SealedValue {
        pub key_id: String,
        pub nonce: String,
        pub ciphertext: String,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct SealResponse {
        pub sealed: SealedValue,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct OpenRequest {
        pub label: String,
        pub sealed: SealedValue,
    }

    #[derive(Debug, Clone, Serialize, Deserialize)]
    pub struct OpenResponse {
        #[serde(serialize_with = "doc_secret::exposed")]
        pub value: Secret<String>,
        /// Sealed under a key older than the current one: seal it again, so a rotation reaches it.
        #[serde(default)]
        pub stale: bool,
    }

    /// The secret store's word that some of its secrets changed, stopped being shared or went
    /// away. `loaded` says it has just started, so plugins that could not reach it try again.
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct SecretsChangedRequest {
        pub secrets: Vec<Uuid>,
        pub loaded: bool,
    }

    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct SecretsChangedResponse {
        /// The plugins told their settings changed.
        pub told: Vec<String>,
    }

    /// With the `team-writer` capability, for the person the call is for: somebody added by email
    /// address, under the same rules as `POST /api/v1/people` (FEAT-PEOPLE).
    #[derive(Debug, Clone, Default, Serialize, Deserialize)]
    #[serde(default)]
    pub struct PersonRequest {
        pub email: String,
        pub name: Option<String>,
        pub team: Option<Uuid>,
        pub organisation: Option<Uuid>,
        pub login: Option<String>,
    }
}
