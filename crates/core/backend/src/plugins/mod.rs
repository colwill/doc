//! The plugin host: what registration checks, the state machine from §5, and the registry of the
//! plugins the backend is holding a connection to. The registry is the live truth; `core.plugins`
//! is the durable record of it, so a restarted backend can say what it had before plugins
//! re-register.

pub mod access;
pub mod api;
pub mod client;
pub mod context;
pub mod events;
pub mod handover;
pub mod host;
pub mod routes;
pub mod runs;
pub mod service;
pub mod settings;
pub mod switches;

use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use doc_cachebus::NamespaceSpec;
use doc_eventbus::{Event, Topic, TopicFilter};
use doc_permissions::Permission;
use doc_plugin_protocol::{
    Capability, Classification, Liveness, Manifest, Operation, PermissionKind, PluginState,
    RESERVED_IDS, RegisterRequest, RegisterResponse, valid_plugin_id, valid_version,
};
use serde::Serialize;
use serde_json::{Value, json};
use tokio::sync::{RwLock, watch};
use uuid::Uuid;

use crate::api::AppState;
use crate::config::Config;
use crate::db::repositories::{DeclaredPermission, PluginRecord};
use crate::identity::{AuditEntry, Principal};
use crate::status::plugins::{self as status, PluginChange, PluginStatus};
use client::{CallError, Connector, PluginClient, RefusingConnector};
use context::{Context, Contexts};
use handover::Gate;

pub const SOURCE: &str = "core.plugins";
pub const UNREACHABLE: &str = "unreachable";
/// How long the backend waits for a freshly registered plugin to have stored its secret.
const READY_ATTEMPTS: u32 = 10;
const READY_WAIT: Duration = Duration::from_millis(200);
const SECRET_BYTES: usize = 32;
const MAX_SCHEDULES: usize = 16;
const MAX_OPERATIONS: usize = 20;
const MAX_OPERATION_PARAMS: usize = 10;

fn state_topic(id: &str) -> String {
    format!("platform.plugin.{id}.state")
}

/// A plugin the backend has a connection to. The secret is deliberately not `Debug` or
/// `Serialize`: it is the only thing proving a `/host/v1` call came from core.
#[derive(Clone)]
pub struct Registered {
    pub id: String,
    /// One per registration, so work tied to a process can tell it is still talking to that one.
    pub instance: Uuid,
    pub manifest: Manifest,
    pub address: String,
    pub binary_sha256: String,
    pub state: PluginState,
    pub error: Option<String>,
    /// When it entered its state, which is what the plugin probe times `loading` out against.
    pub since: DateTime<Utc>,
    /// When its state or error last changed.
    pub at: DateTime<Utc>,
    pub registered_at: DateTime<Utc>,
    pub last_seen: DateTime<Utc>,
    pub client: Arc<dyn PluginClient>,
}

impl Registered {
    pub fn classification(&self) -> Classification {
        self.manifest.classification
    }

    pub fn change(&self) -> PluginChange {
        PluginChange {
            plugin: self.id.clone(),
            version: self.manifest.version.clone(),
            classification: self.manifest.classification,
            instance: self.instance,
            state: Some(self.state),
            error: self.error.clone(),
            since: self.since,
            at: self.at,
            registered_at: self.registered_at,
        }
    }

    /// Moves to `next` with `error`, returning what to announce unless neither has changed.
    fn enter(&mut self, next: PluginState, error: Option<&str>) -> Option<PluginChange> {
        let error = error.map(str::to_string);
        if self.state == next && self.error == error {
            return None;
        }
        let now = status::now();
        if self.state != next {
            self.since = now;
        }
        self.state = next;
        self.error = error;
        self.at = now;
        Some(self.change())
    }

    /// The registry's view in the plugin probe's shape, for when no probe is keeping it.
    pub fn status(&self) -> PluginStatus {
        PluginStatus {
            plugin: self.id.clone(),
            version: self.manifest.version.clone(),
            classification: self.manifest.classification,
            instance: self.instance,
            state: Some(self.state),
            error: self.error.clone(),
            since: self.since,
            at: self.at,
            registered_at: self.registered_at,
            last_error: self.error.clone(),
            last_error_at: self.error.as_ref().map(|_| self.at),
            checked_at: Utc::now(),
        }
    }
}

#[derive(Clone)]
pub struct Registry {
    plugins: Arc<RwLock<BTreeMap<String, Registered>>>,
    connector: Arc<dyn Connector>,
    pub contexts: Arc<Contexts>,
    /// One stop switch per plugin for the loops delivering its subscribed events.
    deliveries: Arc<parking_lot::Mutex<BTreeMap<String, watch::Sender<bool>>>>,
    /// Plugins whose `plugin.<id>` Service Bus address this backend already answers.
    serving: Arc<parking_lot::Mutex<BTreeSet<String>>>,
    /// A newer registration being brought up by a handover while the one above still serves.
    pending: Arc<parking_lot::Mutex<BTreeMap<String, Registered>>>,
    gates: Arc<parking_lot::Mutex<BTreeMap<String, Arc<Gate>>>>,
    /// The registration each plugin's storage was last prepared for, held while storage changes.
    pub(crate) prepared: Arc<tokio::sync::Mutex<BTreeMap<String, Uuid>>>,
    /// The plugins somebody turned off, held `cancelled` and offered to nobody.
    off: Arc<parking_lot::Mutex<BTreeMap<String, TurnedOff>>>,
    /// The flag each plugin that follows one is turned on and off by, and what it last read.
    follows: Arc<parking_lot::Mutex<BTreeMap<String, Following>>>,
    /// Wakes whatever reads those flags, when one of them may have changed.
    pub(crate) nudge: Arc<tokio::sync::Notify>,
}

/// Who turned a plugin off, and when.
#[derive(Debug, Clone, Serialize)]
pub struct TurnedOff {
    pub by: Option<String>,
    pub at: DateTime<Utc>,
}

/// The flag a plugin is turned on and off by, who chose it, and what reading it last found.
#[derive(Debug, Clone, Serialize)]
pub struct Following {
    pub flag: String,
    pub by: Option<String>,
    pub at: DateTime<Utc>,
    /// What the flag was the last time it was read.
    pub read: Option<bool>,
    pub read_at: Option<DateTime<Utc>>,
    /// Why it could not be followed the last time it was tried, which left the plugin as it was.
    pub problem: Option<String>,
}

impl Default for Registry {
    fn default() -> Self {
        Self::new(Arc::new(RefusingConnector))
    }
}

impl Registry {
    pub fn new(connector: Arc<dyn Connector>) -> Self {
        Self {
            plugins: Arc::new(RwLock::new(BTreeMap::new())),
            connector,
            contexts: Arc::new(Contexts::default()),
            deliveries: Arc::new(parking_lot::Mutex::new(BTreeMap::new())),
            serving: Arc::new(parking_lot::Mutex::new(BTreeSet::new())),
            pending: Arc::new(parking_lot::Mutex::new(BTreeMap::new())),
            gates: Arc::new(parking_lot::Mutex::new(BTreeMap::new())),
            prepared: Arc::new(tokio::sync::Mutex::new(BTreeMap::new())),
            off: Arc::new(parking_lot::Mutex::new(BTreeMap::new())),
            follows: Arc::new(parking_lot::Mutex::new(BTreeMap::new())),
            nudge: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// The flag `id` is turned on and off by, while it follows one.
    pub fn following(&self, id: &str) -> Option<Following> {
        self.follows.lock().get(id).cloned()
    }

    pub fn all_following(&self) -> Vec<(String, Following)> {
        self.follows.lock().iter().map(|(id, held)| (id.clone(), held.clone())).collect()
    }

    pub(crate) fn set_following(&self, id: &str, following: Option<Following>) {
        let mut held = self.follows.lock();
        match following {
            Some(following) => held.insert(id.to_string(), following),
            None => held.remove(id),
        };
    }

    /// What reading `id`'s flag found: its value, or why it could not be followed.
    pub(crate) fn note_read(&self, id: &str, read: Result<bool, String>) {
        if let Some(following) = self.follows.lock().get_mut(id) {
            following.read_at = Some(Utc::now());
            match read {
                Ok(on) => (following.read, following.problem) = (Some(on), None),
                Err(problem) => following.problem = Some(problem),
            }
        }
    }

    /// Who turned `id` off and when, while it is off.
    pub fn turned_off(&self, id: &str) -> Option<TurnedOff> {
        self.off.lock().get(id).cloned()
    }

    pub fn is_off(&self, id: &str) -> bool {
        self.off.lock().contains_key(id)
    }

    /// Whether `entry` is offered to anybody: serving requests, and not turned off.
    pub fn offers(&self, entry: &Registered) -> bool {
        entry.state.serves_requests() && !self.is_off(&entry.id)
    }

    pub(crate) fn mark_off(&self, id: &str, off: Option<TurnedOff>) {
        let mut held = self.off.lock();
        match off {
            Some(off) => held.insert(id.to_string(), off),
            None => held.remove(id),
        };
    }

    /// Every request to a plugin passes its gate, which a handover closes while it swaps processes.
    pub fn gate(&self, id: &str) -> Arc<Gate> {
        self.gates.lock().entry(id.to_string()).or_default().clone()
    }

    pub fn handing_over(&self, id: &str) -> bool {
        self.pending.lock().contains_key(id)
    }

    fn pend(&self, entry: &Registered) -> bool {
        let mut pending = self.pending.lock();
        if pending.contains_key(&entry.id) {
            return false;
        }
        pending.insert(entry.id.clone(), entry.clone());
        true
    }

    fn unpend(&self, id: &str) {
        self.pending.lock().remove(id);
    }

    fn touch_pending(&self, id: &str, instance: Uuid) -> bool {
        match self.pending.lock().get_mut(id).filter(|entry| entry.instance == instance) {
            Some(entry) => {
                entry.last_seen = Utc::now();
                true
            }
            None => false,
        }
    }

    /// Replaces whatever was delivering to this plugin before, stopping it.
    fn deliver(&self, id: &str, stop: watch::Sender<bool>) {
        if let Some(previous) = self.deliveries.lock().insert(id.to_string(), stop) {
            let _ = previous.send(true);
        }
    }

    fn stop_delivering(&self, id: &str) {
        if let Some(stop) = self.deliveries.lock().remove(id) {
            let _ = stop.send(true);
        }
    }

    pub async fn get(&self, id: &str) -> Option<Registered> {
        self.plugins.read().await.get(id).cloned()
    }

    pub async fn list(&self) -> Vec<Registered> {
        self.plugins.read().await.values().cloned().collect()
    }

    pub async fn insert(&self, entry: Registered) {
        self.plugins.write().await.insert(entry.id.clone(), entry);
    }

    pub async fn remove(&self, id: &str) -> Option<Registered> {
        self.plugins.write().await.remove(id)
    }

    async fn remove_instance(&self, entry: &Registered) {
        let mut plugins = self.plugins.write().await;
        if plugins.get(&entry.id).is_some_and(|current| current.instance == entry.instance) {
            plugins.remove(&entry.id);
        }
    }

    async fn seen(&self, id: &str) {
        if let Some(entry) = self.plugins.write().await.get_mut(id) {
            entry.last_seen = Utc::now();
        }
    }

    /// Counts every plugin as heard from now, after the backend itself could not listen.
    async fn heard_from_all(&self) {
        let now = Utc::now();
        for entry in self.plugins.write().await.values_mut() {
            entry.last_seen = now;
        }
    }

    /// Plugins that have not been heard from within the grace period and are not already in error.
    async fn silent(&self, grace: Duration) -> Vec<Registered> {
        let cutoff = Utc::now() - chrono::Duration::from_std(grace).unwrap_or_default();
        self.plugins
            .read()
            .await
            .values()
            .filter(|entry| entry.state != PluginState::Error && entry.last_seen < cutoff)
            .cloned()
            .collect()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RegisterError {
    #[error("this token does not belong to {claimed}")]
    WrongToken { claimed: String },
    #[error("`{0}` is not a plugin ID")]
    BadId(String),
    #[error("`{0}` is a reserved ID")]
    Reserved(String),
    #[error("`{0}` is not a version")]
    BadVersion(String),
    #[error("{plugin} is not allowed the {capability} capability")]
    CapabilityRefused { plugin: String, capability: String },
    #[error("public routes need the public-routes capability")]
    PublicRoutesRefused,
    #[error("`{name}` is not a permission name: {reason}")]
    BadPermission { name: String, reason: String },
    #[error("version {version} was already registered with a different binary")]
    HashConflict { version: String },
    #[error("`{filter}` is not a topic filter: {reason}")]
    BadSubscription { filter: String, reason: String },
    #[error("schedule `{name}` is not usable: {reason}")]
    BadSchedule { name: String, reason: String },
    #[error("operation `{name}` is not usable: {reason}")]
    BadOperation { name: String, reason: String },
    #[error("its own service account is not usable: {0}")]
    BadServiceAccount(String),
    #[error("dashboard item `{id}` is not usable: {reason}")]
    BadDashboardItem { id: String, reason: String },
    #[error("{0} is being reloaded or handed over to a new version; try again shortly")]
    Busy(String),
    #[error("{plugin} has registered too often; try again in {seconds}s")]
    TooOften { plugin: String, seconds: u64 },
    #[error("the data declaration is not usable: {0}")]
    BadData(String),
    #[error("the data declaration cannot take over from the serving version's: {0}")]
    IncompatibleData(String),
    #[error("{0}")]
    Storage(String),
}

impl RegisterError {
    pub fn status(&self) -> u16 {
        match self {
            Self::WrongToken { .. }
            | Self::Reserved(_)
            | Self::CapabilityRefused { .. }
            | Self::PublicRoutesRefused => 403,
            Self::BadId(_)
            | Self::BadVersion(_)
            | Self::BadPermission { .. }
            | Self::BadSubscription { .. }
            | Self::BadSchedule { .. }
            | Self::BadOperation { .. }
            | Self::BadServiceAccount(_)
            | Self::BadDashboardItem { .. }
            | Self::BadData(_)
            | Self::IncompatibleData(_) => 400,
            Self::HashConflict { .. } => 409,
            Self::TooOften { .. } => 429,
            Self::Busy(_) | Self::Storage(_) => 503,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::WrongToken { .. } => "wrong-token",
            Self::BadId(_) => "bad-id",
            Self::Reserved(_) => "reserved-id",
            Self::BadVersion(_) => "bad-version",
            Self::CapabilityRefused { .. } => "capability-refused",
            Self::PublicRoutesRefused => "public-routes-refused",
            Self::BadPermission { .. } => "bad-permission",
            Self::HashConflict { .. } => "hash-conflict",
            Self::BadSubscription { .. } => "bad-subscription",
            Self::BadSchedule { .. } => "bad-schedule",
            Self::BadOperation { .. } => "bad-operation",
            Self::BadServiceAccount(_) => "bad-service-account",
            Self::BadDashboardItem { .. } => "bad-dashboard-item",
            Self::Busy(_) => "handover-in-progress",
            Self::TooOften { .. } => "too-many-requests",
            Self::BadData(_) => "bad-data",
            Self::IncompatibleData(_) => "incompatible-data",
            Self::Storage(_) => "storage-unavailable",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TransitionError {
    #[error("no plugin `{0}` is registered")]
    Unknown(String),
    #[error("a plugin cannot go from {from} to {to}")]
    Illegal { from: &'static str, to: &'static str },
    #[error("{0} is being reloaded or handed over to a new version")]
    Busy(String),
    #[error("{0} is already {1}")]
    Underway(String, &'static str),
    /// A newer registration of the same plugin has taken this process's place.
    #[error("this process of {0} has been replaced")]
    Superseded(String),
    #[error("{0}")]
    Call(String),
    /// Turning off a plugin the platform cannot do without.
    #[error("{0} {1}, so the platform cannot do without it")]
    Needed(String, &'static str),
    #[error("{0} is turned off; turn it on to resume it")]
    Off(String),
    #[error("{0} follows the flag {1}; stop following it to turn it on or off yourself")]
    Follows(String, String),
    #[error("{0}")]
    Invalid(String),
}

impl TransitionError {
    pub fn status(&self) -> u16 {
        match self {
            Self::Unknown(_) => 404,
            Self::Illegal { .. }
            | Self::Busy(_)
            | Self::Underway(..)
            | Self::Needed(..)
            | Self::Off(_)
            | Self::Follows(..) => 409,
            Self::Invalid(_) => 400,
            Self::Superseded(_) => 410,
            Self::Call(_) => 502,
        }
    }
}

fn capability_name(capability: Capability) -> &'static str {
    match capability {
        Capability::PermissionProvider => "permission-provider",
        Capability::IdentityProvider => "identity-provider",
        Capability::TeamProvider => "team-provider",
        Capability::TeamWriter => "team-writer",
        Capability::Offboarding => "offboarding",
        Capability::TokenIssuer => "token-issuer",
        Capability::SecretStore => "secret-store",
        Capability::TelemetrySink => "telemetry-sink",
        Capability::PublicRoutes => "public-routes",
        Capability::ServiceAccount => "service-account",
    }
}

/// Step 2 of registration. Everything here is decided from the manifest and the configuration
/// alone, so it can be checked before anything is written down.
fn validate(config: &Config, manifest: &Manifest) -> Result<(), RegisterError> {
    if !valid_plugin_id(&manifest.id) {
        return Err(RegisterError::BadId(manifest.id.clone()));
    }
    if RESERVED_IDS.contains(&manifest.id.as_str()) {
        return Err(RegisterError::Reserved(manifest.id.clone()));
    }
    if !valid_version(&manifest.version) {
        return Err(RegisterError::BadVersion(manifest.version.clone()));
    }
    for capability in &manifest.capabilities {
        if !config.plugins.allows(&manifest.id, *capability) {
            return Err(RegisterError::CapabilityRefused {
                plugin: manifest.id.clone(),
                capability: capability_name(*capability).into(),
            });
        }
    }
    if !manifest.public_routes.is_empty()
        && !manifest.capabilities.contains(&Capability::PublicRoutes)
    {
        return Err(RegisterError::PublicRoutesRefused);
    }
    for filter in &manifest.subscriptions {
        TopicFilter::new(filter).map_err(|err| RegisterError::BadSubscription {
            filter: filter.clone(),
            reason: err.to_string(),
        })?;
    }
    if manifest.schedules.len() > MAX_SCHEDULES {
        let reason = format!("a plugin has at most {MAX_SCHEDULES} schedules");
        return Err(RegisterError::BadSchedule { name: String::new(), reason });
    }
    for schedule in &manifest.schedules {
        let bad =
            |reason: String| RegisterError::BadSchedule { name: schedule.name.clone(), reason };
        let named = !schedule.name.is_empty()
            && schedule.name.len() <= 32
            && schedule
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !named {
            return Err(bad("a name is up to 32 of a-z, 0-9 and -".into()));
        }
        doc_cron_tasks::next_after(&schedule.cron, Utc::now())
            .map_err(|err| bad(err.to_string()))?;
    }
    validate_operations(&manifest.operations)?;
    validate_dashboard(&manifest.dashboard)?;
    if let Some(own) = &manifest.service_account {
        if !manifest.capabilities.contains(&Capability::ServiceAccount) {
            let reason = "a service account of its own needs the service-account capability";
            return Err(RegisterError::BadServiceAccount(reason.into()));
        }
        let named = !own.name.is_empty()
            && own.name.len() <= 64
            && own.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !named {
            let reason = format!("`{}` is not a name: up to 64 of a-z, 0-9 and -", own.name);
            return Err(RegisterError::BadServiceAccount(reason));
        }
    }
    for custom in &manifest.custom_permissions {
        let kind = match custom.kind {
            PermissionKind::PluginUser => "pluginuser",
            PermissionKind::PluginService => "pluginservice",
        };
        let written = format!("plugin:{}:{kind}:{}:ro", manifest.id, custom.name);
        Permission::from_str(&written).map_err(|err| RegisterError::BadPermission {
            name: custom.name.clone(),
            reason: err.to_string(),
        })?;
    }
    Ok(())
}

/// What a plugin offers for people's dashboards: each named once, and served from its own `ui/`.
fn validate_dashboard(items: &[doc_plugin_protocol::DashboardItem]) -> Result<(), RegisterError> {
    let mut seen = BTreeSet::new();
    for item in items {
        let bad = |reason: &str| RegisterError::BadDashboardItem {
            id: item.id.clone(),
            reason: reason.to_string(),
        };
        let named = !item.id.is_empty()
            && item.id.len() <= 32
            && item.id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
        if !named {
            return Err(bad("a name is up to 32 of a-z, 0-9 and -"));
        }
        if !seen.insert(item.id.as_str()) {
            return Err(bad("it is offered twice"));
        }
        if item.label.trim().is_empty() {
            return Err(bad("it needs a label"));
        }
        if !item.path.starts_with('/') || !routes::clean(&item.path) {
            return Err(bad("its path starts with / and stays under the plugin's ui/"));
        }
    }
    Ok(())
}

/// The operations a plugin offers automations (ADR-0012): named, routed under its own `api/`, and
/// with every `{param}` in a route one it declares.
fn validate_operations(operations: &[Operation]) -> Result<(), RegisterError> {
    if operations.len() > MAX_OPERATIONS {
        let reason = format!("a plugin offers at most {MAX_OPERATIONS} operations");
        return Err(RegisterError::BadOperation { name: String::new(), reason });
    }
    let shaped = |name: &str, extra: char| {
        !name.is_empty()
            && name.len() <= 32
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == extra)
    };
    let mut seen = BTreeSet::new();
    for operation in operations {
        let bad =
            |reason: String| RegisterError::BadOperation { name: operation.name.clone(), reason };
        if !shaped(&operation.name, '-') {
            return Err(bad("a name is up to 32 of a-z, 0-9 and -".into()));
        }
        if !seen.insert(operation.name.as_str()) {
            return Err(bad("another operation has the same name".into()));
        }
        if operation.label.trim().is_empty() {
            return Err(bad("it needs a label".into()));
        }
        if !matches!(operation.method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
            return Err(bad(format!(
                "`{}` is not GET, POST, PUT, PATCH or DELETE",
                operation.method
            )));
        }
        if operation.params.len() > MAX_OPERATION_PARAMS {
            return Err(bad(format!(
                "an operation takes at most {MAX_OPERATION_PARAMS} parameters"
            )));
        }
        let mut params = BTreeSet::new();
        for param in &operation.params {
            if !shaped(&param.name, '_') {
                return Err(bad(format!(
                    "`{}` is not a parameter name: a-z, 0-9, _ and -",
                    param.name
                )));
            }
            if !params.insert(param.name.as_str()) {
                return Err(bad(format!("`{}` is declared twice", param.name)));
            }
        }
        let route = &operation.route;
        let relative = !route.is_empty()
            && !route.starts_with('/')
            && !route.contains(['?', '#'])
            && route.split('/').all(|part| !part.is_empty() && part != "." && part != "..");
        if !relative {
            return Err(bad(format!("`{route}` is not a route under the plugin's api/")));
        }
        let mut rest = route.as_str();
        while let Some(start) = rest.find('{') {
            let Some(end) = rest[start..].find('}') else {
                return Err(bad(format!("`{route}` opens a {{ it does not close")));
            };
            let named = &rest[start + 1..start + end];
            if !params.contains(named) {
                return Err(bad(format!(
                    "its route names `{{{named}}}`, which is not a parameter"
                )));
            }
            rest = &rest[start + end + 1..];
        }
    }
    Ok(())
}

/// Both permissions every plugin has by existing, then the ones its manifest declares.
fn declared(manifest: &Manifest) -> Vec<DeclaredPermission> {
    let mut permissions = vec![
        DeclaredPermission { kind: "user".into(), name: String::new() },
        DeclaredPermission { kind: "service".into(), name: String::new() },
    ];
    for custom in &manifest.custom_permissions {
        let kind = match custom.kind {
            PermissionKind::PluginUser => "pluginuser",
            PermissionKind::PluginService => "pluginservice",
        };
        permissions.push(DeclaredPermission { kind: kind.into(), name: custom.name.clone() });
    }
    permissions
}

/// This is the only thing proving a `/host/v1` call came from core, so a failure to get random
/// bytes refuses the registration rather than falling back to anything guessable.
fn instance_secret() -> Result<String, RegisterError> {
    use base64::Engine;
    let mut bytes = [0u8; SECRET_BYTES];
    getrandom::fill(&mut bytes)
        .map_err(|err| RegisterError::Storage(format!("no random bytes: {err}")))?;
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes))
}

/// T20's five steps; loading starts once the answer carrying the plugin's secret is on its way.
pub async fn register(
    state: &AppState,
    principal: &Principal,
    request: RegisterRequest,
) -> Result<RegisterResponse, RegisterError> {
    let manifest = request.manifest;
    let Principal::Plugin { id: holder } = principal else {
        return Err(RegisterError::WrongToken { claimed: manifest.id.clone() });
    };
    if holder != &manifest.id {
        return Err(RegisterError::WrongToken { claimed: manifest.id.clone() });
    }
    if let Err(wait) = state.limits.registrations.hit(holder) {
        return Err(RegisterError::TooOften {
            plugin: holder.clone(),
            seconds: wait.as_secs().max(1),
        });
    }
    validate(&state.config, &manifest)?;
    crate::data::admit(state, &manifest).await?;
    let id = manifest.id.clone();

    let repo = &state.repos.plugins;
    let storage =
        |err: crate::db::repositories::RepositoryError| RegisterError::Storage(err.to_string());
    let known = repo.version_hash(&id, &manifest.version).await.map_err(storage)?;
    if known.is_some_and(|hash| hash != request.binary_sha256) {
        if !state.config.plugins.allow_rebuilds {
            return Err(RegisterError::HashConflict { version: manifest.version.clone() });
        }
        tracing::warn!(
            plugin = %id,
            version = %manifest.version,
            "a rebuilt binary registered under a version already seen; plugins.allow_rebuilds is on"
        );
    }
    if state.plugins.handing_over(&id) {
        return Err(RegisterError::Busy(id));
    }
    let serving = state.plugins.get(&id).await.filter(|old| old.state.serves_requests());

    let secret = instance_secret()?;
    let connection = state
        .plugins
        .connector
        .connect(&id, &request.address, &secret)
        .await
        .map_err(RegisterError::Storage)?;
    let now = status::now();
    let entry = Registered {
        id: id.clone(),
        instance: Uuid::now_v7(),
        manifest: manifest.clone(),
        address: request.address.clone(),
        binary_sha256: request.binary_sha256.clone(),
        state: PluginState::Loading,
        error: None,
        since: now,
        at: now,
        registered_at: now,
        last_seen: now,
        client: client::Traced::wrap(&id, &manifest.version, connection),
    };
    let manifest_json = serde_json::to_value(&manifest).unwrap_or_default();
    repo.record_version(&id, &manifest.version, &request.binary_sha256, &manifest_json)
        .await
        .map_err(storage)?;
    repo.record_permissions(&id, &declared(&manifest)).await.map_err(storage)?;
    own_account(state, principal, &manifest).await;
    routes::serve_internal(state, &id).await;

    let previous = match &serving {
        // Another version is serving: it keeps serving until this one is ready to take over.
        Some(old) => {
            if !state.plugins.pend(&entry) {
                return Err(RegisterError::Busy(id));
            }
            tokio::spawn(handover::run(state.clone(), old.clone(), entry.clone()));
            None
        }
        None => {
            if let Some(replaced) = state.plugins.get(&id).await {
                tracing::info!(plugin = %id, was = %replaced.state.as_str(), "replacing a registration that was not serving");
                handover::dismiss(replaced);
            }
            install(state, &entry).await;
            let previous = handed_over(state, &id).await;
            tokio::spawn(bring_up(state.clone(), entry.clone(), previous.clone()));
            previous
        }
    };
    audit(
        state,
        principal,
        "plugin.registered",
        &id,
        json!({
            "version": manifest.version,
            "address": request.address,
            "binary_sha256": request.binary_sha256,
            "replacing": serving.map(|old| old.manifest.version),
        }),
    )
    .await;
    Ok(RegisterResponse {
        secret: secret.into(),
        previous,
        state: PluginState::Loading,
        liveness_ms: Some(state.config.plugins.grace().as_millis() as u64 / 4),
        instance: Some(entry.instance),
    })
}

/// The service account a `service-account` plugin acts as when it asks as itself: made the first
/// time it registers, owned by the platform, and holding nothing until administrators grant it
/// some. One somebody else made under the same name is never claimed, so declaring a name cannot
/// take over another account's access; the plugin then acts with no access of its own, as before.
async fn own_account(state: &AppState, principal: &Principal, manifest: &Manifest) {
    let Some(wanted) = &manifest.service_account else { return };
    let (identity, id) = (&state.repos.identity, &manifest.id);
    match identity.plugin_service_account(id).await {
        Ok(None) => {}
        Ok(Some(_)) => return,
        Err(err) => {
            tracing::warn!(plugin = %id, %err, "its own service account could not be looked up");
            return;
        }
    }
    match identity.service_account_by_name(&wanted.name).await {
        Ok(None) => {}
        Ok(Some(_)) => {
            tracing::warn!(
                plugin = %id,
                name = %wanted.name,
                "a service account by its name exists and was not made for it, so it acts with no access of its own"
            );
            let detail = json!({ "name": wanted.name });
            audit(state, principal, "plugin.service-account.refused", id, detail).await;
            return;
        }
        Err(err) => {
            tracing::warn!(plugin = %id, %err, "its own service account could not be looked up");
            return;
        }
    }
    let description = Some(wanted.description.as_str()).filter(|said| !said.is_empty());
    let owner = crate::teams::AccountOwner::Platform;
    let account = match identity.create_service_account(&wanted.name, description, owner).await {
        Ok(account) => account,
        Err(err) => {
            tracing::warn!(plugin = %id, %err, "its own service account could not be made");
            return;
        }
    };
    if let Err(err) = identity.link_plugin_service_account(id, account.id).await {
        tracing::warn!(plugin = %id, %err, "its own service account could not be recorded");
        return;
    }
    let detail = json!({ "service_account": account.id, "name": account.name });
    audit(state, principal, "plugin.service-account.created", id, detail).await;
}

fn record_of(entry: &Registered) -> PluginRecord {
    PluginRecord {
        id: entry.id.clone(),
        version: entry.manifest.version.clone(),
        classification: entry.manifest.classification.as_str().to_string(),
        state: entry.state.as_str().to_string(),
        address: entry.address.clone(),
        binary_sha256: entry.binary_sha256.clone(),
        manifest: serde_json::to_value(&entry.manifest).unwrap_or_default(),
        error: entry.error.clone(),
        registered_at: Some(entry.registered_at),
        last_seen_at: Some(entry.last_seen),
    }
}

/// Makes `entry` the registration requests go to, as `loading`, delivering its subscriptions.
async fn install(state: &AppState, entry: &Registered) {
    let mut entry = entry.clone();
    let now = status::now();
    (entry.state, entry.error, entry.since, entry.at) = (PluginState::Loading, None, now, now);
    entry.last_seen = Utc::now();
    if let Err(err) = state.repos.plugins.record_registration(&record_of(&entry)).await {
        tracing::warn!(plugin = %entry.id, %err, "a registration was not written down");
    }
    let (id, subscriptions, change) =
        (entry.id.clone(), entry.manifest.subscriptions.clone(), entry.change());
    let schedules = entry.manifest.schedules.clone();
    let configured = settings::resolve(state, &id, &entry.manifest).await;
    state.plugins.insert(entry).await;
    let stop = events::start(state, &id, &subscriptions).await;
    state.plugins.deliver(&id, stop);
    schedule(state, &id, &schedules, &configured).await;
    announce(state, &change).await;
}

/// Records the schedules again after a feature was turned on or off, so a schedule belonging to a
/// feature that is off stops existing rather than running for nothing (ADR-0007).
pub async fn reschedule(
    state: &AppState,
    id: &str,
    schedules: &[doc_plugin_protocol::Schedule],
    configured: &settings::Resolved,
) {
    schedule(state, id, schedules, configured).await;
}

/// Records the plugin's schedules as `plugin.<id>.<name>`, dropping any it no longer declares and
/// any belonging to a feature that is off.
async fn schedule(
    state: &AppState,
    id: &str,
    schedules: &[doc_plugin_protocol::Schedule],
    configured: &settings::Resolved,
) {
    let prefix = format!("plugin.{id}.");
    let mut kept = Vec::new();
    for schedule in schedules {
        let wanted = schedule.feature.as_ref().is_none_or(|feature| configured.feature(feature));
        if !wanted {
            continue;
        }
        // How often it runs may be a setting, so an administrator changes it on the page rather
        // than in the deployment; anything unusable there leaves the declared expression.
        let cron = schedule
            .setting
            .as_ref()
            .and_then(|key| configured.cron(key))
            .unwrap_or_else(|| schedule.cron.clone());
        let name = format!("{prefix}{}", schedule.name);
        let Ok(next) = doc_cron_tasks::next_after(&cron, Utc::now()) else { continue };
        let description = Some(schedule.description.as_str()).filter(|text| !text.is_empty());
        if let Err(err) = state.repos.cron.upsert(&name, &cron, description, next).await {
            tracing::warn!(plugin = %id, %err, schedule = %name, "a schedule was not recorded");
        }
        kept.push(name);
    }
    if let Err(err) = state.repos.cron.prune(&prefix, &kept).await {
        tracing::warn!(plugin = %id, %err, "schedules the plugin dropped were not removed");
    }
}

/// What the last `unload` of any version returned, kept in Postgres across backend restarts.
async fn handed_over(state: &AppState, id: &str) -> Option<Value> {
    state.repos.plugins.handover(id).await.unwrap_or_else(|err| {
        tracing::warn!(plugin = %id, %err, "the last handover could not be read");
        None
    })
}

async fn save_handover(state: &AppState, id: &str, carried: Option<&Value>) {
    if let Err(err) = state.repos.plugins.save_handover(id, carried).await {
        tracing::warn!(plugin = %id, %err, "what the plugin handed over was not saved");
    }
}

/// Its cache namespace and the storage its data declaration needs, before `load` can use either.
async fn prepare(state: &AppState, entry: &Registered) -> Result<(), String> {
    let namespace = api::namespace(&entry.id).map_err(|refusal| refusal.detail)?;
    let spec =
        NamespaceSpec::new(namespace, Some(api::CACHE_TTL)).with_max_entries(api::CACHE_ENTRIES);
    state.buses.cache.register_namespace(spec).await.map_err(|err| format!("cache: {err}"))?;
    crate::data::prepare(state, &entry.id, entry.instance, &entry.manifest.data)
        .await
        .map_err(|err| format!("storage: {err}"))
}

/// Step 5: storage, then `load`, then `running`, or `error` with the reason.
async fn bring_up(state: AppState, entry: Registered, previous: Option<Value>) {
    if let Err(reason) = prepare(&state, &entry).await {
        fail(&state, &entry, &reason).await;
        return;
    }
    match load(&state, &entry.id, entry.client.as_ref(), previous).await {
        // No other version serves, so storage is settled before anything can see this one running.
        Ok(()) => match complete_then_settle(&state, &entry).await {
            Ok(_) => {
                tracing::info!(plugin = %entry.id, version = %entry.manifest.version, "plugin running")
            }
            Err(err) => {
                tracing::warn!(plugin = %entry.id, %err, "a loaded plugin was not marked running")
            }
        },
        Err(err) => fail(&state, &entry, &format!("load failed: {}", err.detail())).await,
    }
}

async fn complete_then_settle(
    state: &AppState,
    entry: &Registered,
) -> Result<PluginState, TransitionError> {
    crate::data::complete(state, &entry.id, entry.instance, &entry.manifest.data).await;
    settle(state, entry, PluginState::Running, None).await
}

/// The plugin acting as itself: lifecycle calls and deliveries are its own work, not a caller's.
pub fn own_context(state: &AppState, id: &str, ttl: Duration) -> Result<Context, CallError> {
    state
        .plugins
        .contexts
        .issue(id, Principal::Plugin { id: id.to_string() }, ttl)
        .map_err(|err| CallError::Unreachable(id.to_string(), err))
}

/// A plugin answers `503` with no body until it has stored the secret from its registration
/// response, which can still be in flight when this call goes out.
async fn load(
    state: &AppState,
    id: &str,
    client: &dyn PluginClient,
    previous: Option<Value>,
) -> Result<(), CallError> {
    let mut last = None;
    for _ in 0..READY_ATTEMPTS {
        let context = own_context(state, id, client::LOAD_DEADLINE)?;
        match client.load(previous.clone(), context.token()).await {
            Ok(()) => return Ok(()),
            Err(CallError::NotReady(id)) => {
                last = Some(CallError::NotReady(id));
                tokio::time::sleep(READY_WAIT).await;
            }
            Err(err) => return Err(err),
        }
    }
    Err(last.unwrap_or_else(|| CallError::NotReady("the plugin".into())))
}

async fn fail(state: &AppState, entry: &Registered, reason: &str) {
    tracing::warn!(plugin = %entry.id, reason, "a plugin went into error");
    if let Err(err) = settle(state, entry, PluginState::Error, Some(reason)).await {
        tracing::warn!(plugin = %entry.id, %err, "the plugin could not be marked as in error");
    }
}

/// Records an error against a registration that carries on in the state it is in.
async fn note(state: &AppState, entry: &Registered, error: &str) {
    let noted = {
        let mut plugins = state.plugins.plugins.write().await;
        let current =
            plugins.get_mut(&entry.id).filter(|current| current.instance == entry.instance);
        current.and_then(|current| {
            let now = current.state;
            current.enter(now, Some(error)).map(|change| (now, change))
        })
    };
    if let Some((from, change)) = noted {
        changed(state, from, &change).await;
    }
}

/// The only way a plugin's state changes. A move to the state it is already in is a no-op rather
/// than an error, because the plugin's own liveness reports carry the states the backend sets.
pub async fn transition(
    state: &AppState,
    id: &str,
    next: PluginState,
    error: Option<&str>,
) -> Result<PluginState, TransitionError> {
    shift(state, id, None, next, error).await
}

/// `transition` for work tied to one registration, which must never move the one that replaced it.
/// A plugin somebody turned off settles `cancelled` where it would have been `running`, and is
/// told so, which is what keeps it off across restarts and new versions.
async fn settle(
    state: &AppState,
    entry: &Registered,
    next: PluginState,
    error: Option<&str>,
) -> Result<PluginState, TransitionError> {
    let held = next == PluginState::Running && state.plugins.is_off(&entry.id);
    let next = if held { PluginState::Cancelled } else { next };
    let settled = shift(state, &entry.id, Some(entry.instance), next, error).await?;
    if held {
        told_cancelled(state, entry).await;
    }
    Ok(settled)
}

async fn shift(
    state: &AppState,
    id: &str,
    instance: Option<Uuid>,
    next: PluginState,
    error: Option<&str>,
) -> Result<PluginState, TransitionError> {
    let (from, change) = {
        let mut plugins = state.plugins.plugins.write().await;
        let entry = plugins
            .get_mut(id)
            .filter(|entry| instance.is_none_or(|instance| entry.instance == instance))
            .ok_or_else(|| TransitionError::Unknown(id.to_string()))?;
        let from = entry.state;
        if from == next && instance.is_none() {
            return Ok(next);
        }
        if from != next && !from.may_become(next) {
            return Err(TransitionError::Illegal { from: from.as_str(), to: next.as_str() });
        }
        (from, entry.enter(next, error))
    };
    if let Some(change) = change {
        changed(state, from, &change).await;
    }
    Ok(next)
}

/// Writes a change down and announces it; entering `running` also starts what that state runs.
async fn changed(state: &AppState, from: PluginState, change: &PluginChange) {
    let Some(next) = change.state else { return };
    let id = &change.plugin;
    if let Err(err) =
        state.repos.plugins.set_state(id, next.as_str(), change.error.as_deref()).await
    {
        tracing::warn!(plugin = %id, %err, "a plugin state change was not written down");
    }
    if from != next {
        tracing::info!(plugin = %id, from = %from.as_str(), to = %next.as_str(), "plugin state");
    }
    announce(state, change).await;
    if next == PluginState::Running && from != next {
        runs::started(state, id, from);
    }
}

/// Every change is published, so anything watching sees the sequence the registry went through.
async fn announce(state: &AppState, change: &PluginChange) {
    let id = &change.plugin;
    let Ok(topic) = Topic::new(state_topic(id)) else { return };
    let payload = match serde_json::to_value(change) {
        Ok(payload) => payload,
        Err(err) => {
            tracing::warn!(%err, plugin = %id, "a plugin state change is not JSON");
            return;
        }
    };
    match state.buses.events.publish(Event::new(topic, SOURCE, payload)).await {
        Ok(ack) => tracing::debug!(id = %ack.id, plugin = %id, "announced a plugin state change"),
        Err(err) => tracing::warn!(%err, plugin = %id, "could not announce a plugin state change"),
    }
}

pub(crate) async fn audit(
    state: &AppState,
    principal: &Principal,
    action: &str,
    subject: &str,
    detail: Value,
) {
    let entry = AuditEntry::new(action).by(principal).subject(subject).detail(detail);
    if let Err(err) = state.repos.identity.record_audit(entry).await {
        tracing::warn!(%err, action, "an audit entry was not written");
    }
}

/// A report from a process that a handover replaced is `Superseded`, which tells it to exit.
pub async fn liveness(
    state: &AppState,
    principal: &Principal,
    report: Liveness,
) -> Result<(), TransitionError> {
    let Principal::Plugin { id } = principal else {
        return Err(TransitionError::Unknown(report.id));
    };
    if id != &report.id {
        return Err(TransitionError::Unknown(report.id));
    }
    // A reload keeps its registration pending too, so the registry's own instance is looked at first.
    let entry = state.plugins.get(id).await;
    let current = entry
        .as_ref()
        .is_some_and(|entry| report.instance.is_none_or(|instance| instance == entry.instance));
    if !current && report.instance.is_some_and(|instance| state.plugins.touch_pending(id, instance))
    {
        return Ok(());
    }
    let Some(entry) = entry else {
        return Err(TransitionError::Unknown(report.id));
    };
    if !current {
        return Err(TransitionError::Superseded(report.id));
    }
    state.plugins.seen(id).await;
    if let Err(err) = state.repos.plugins.touch(id).await {
        tracing::debug!(plugin = %id, %err, "a liveness report was not written down");
    }
    // A report can only signal failure, so a stale state cannot undo an operator's resume.
    if report.state == PluginState::Error && entry.state != PluginState::Error {
        settle(state, &entry, PluginState::Error, report.error.as_deref()).await?;
    }
    // The backend only guessed the process had gone, and it has not: a network break, or a host
    // that slept, silences a plugin without ending it. It is loaded again as an operator's reload
    // from `error` would, which a reload already under way makes a no-op.
    if entry.state == PluginState::Error
        && entry.error.as_deref().is_some_and(|error| error.starts_with(UNREACHABLE))
        && report.state != PluginState::Error
    {
        match handover::reload(state, id).await {
            Ok(_) => {
                tracing::info!(plugin = %id, "an unreachable plugin reported again; loading it")
            }
            Err(err) => {
                tracing::debug!(plugin = %id, %err, "an unreachable plugin was not reloaded")
            }
        }
    }
    Ok(())
}

/// Stops a plugin's work, its tasks included, without unloading it; §5 lets it be resumed.
pub async fn cancel(state: &AppState, id: &str) -> Result<(), TransitionError> {
    let Some(entry) = state.plugins.get(id).await else {
        return Err(TransitionError::Unknown(id.to_string()));
    };
    transition(state, id, PluginState::Cancelled, None).await?;
    let stopped = runs::cancel_tasks(state, id).await;
    if stopped > 0 {
        tracing::info!(plugin = %id, tasks = stopped, "a cancelled plugin's tasks were asked to stop");
    }
    let context = own_context(state, id, client::CALL_DEADLINE)
        .map_err(|err| TransitionError::Call(err.to_string()))?;
    if let Err(err) = entry.client.cancel(context.token()).await {
        tracing::warn!(plugin = %id, %err, "a plugin did not acknowledge being cancelled");
    }
    Ok(())
}

/// Tells a plugin it is cancelled, for one that settled that way rather than being moved there.
async fn told_cancelled(state: &AppState, entry: &Registered) {
    let context = match own_context(state, &entry.id, client::CALL_DEADLINE) {
        Ok(context) => context,
        Err(err) => {
            tracing::warn!(plugin = %entry.id, %err, "a plugin turned off was not told it is");
            return;
        }
    };
    if let Err(err) = entry.client.cancel(context.token()).await {
        tracing::warn!(plugin = %entry.id, %err, "a plugin did not acknowledge being cancelled");
    }
}

/// Keeps what `unload` returned and the plugin's permissions, then ends its process.
pub async fn unload(state: &AppState, id: &str) -> Result<Option<Value>, TransitionError> {
    let Some(entry) = state.plugins.get(id).await else {
        return Err(TransitionError::Unknown(id.to_string()));
    };
    if state.plugins.handing_over(id) {
        return Err(TransitionError::Busy(id.to_string()));
    }
    transition(state, id, PluginState::Unloading, None).await?;
    state.plugins.stop_delivering(id);
    let context = own_context(state, id, client::CALL_DEADLINE)
        .map_err(|err| TransitionError::Call(err.to_string()))?;
    let carried = match entry.client.unload(context.token()).await {
        Ok(carried) => carried,
        Err(err) => {
            fail(state, &entry, &format!("unload failed: {}", err.detail())).await;
            return Err(TransitionError::Call(err.to_string()));
        }
    };
    save_handover(state, id, carried.as_ref()).await;
    state.plugins.remove_instance(&entry).await;
    if let Err(err) = state.repos.plugins.deregister(id).await {
        tracing::warn!(plugin = %id, %err, "the removal was not written down");
    }
    let now = status::now();
    announce(
        state,
        &PluginChange { state: None, error: None, since: now, at: now, ..entry.change() },
    )
    .await;
    tracing::info!(plugin = %id, carried = carried.is_some(), "plugin unloaded and removed");
    handover::dismiss(entry);
    Ok(carried)
}

/// One pass over the registry, marking anything that has stopped reporting as unreachable.
pub async fn sweep(state: &AppState, grace: Duration) {
    for entry in state.plugins.silent(grace).await {
        tracing::warn!(plugin = %entry.id, grace_s = grace.as_secs(), "no liveness report");
        fail(state, &entry, UNREACHABLE).await;
    }
}

/// Missed liveness reports are what tell the backend a plugin's process has gone: nothing else does,
/// because the plugin is what dials the backend, not the other way round.
pub fn watch(state: AppState) {
    tokio::spawn(async move {
        let grace = state.config.plugins.grace();
        let mut ticker = tokio::time::interval(grace / 2);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last = Utc::now();
        loop {
            ticker.tick().await;
            let now = Utc::now();
            if paused(last, now, grace) {
                let paused_s = (now - last).num_seconds();
                tracing::info!(paused_s, "the backend was paused; liveness is counted from now");
                state.plugins.heard_from_all().await;
            } else {
                sweep(&state, grace).await;
            }
            last = now;
        }
    });
}

/// Whether the wall clock moved on by more than a grace period between two sweeps half a grace
/// apart. The ticker's clock stops while the host sleeps and the wall clock does not, so this is
/// the backend having been paused, not every plugin having gone quiet at once.
fn paused(last: DateTime<Utc>, now: DateTime<Utc>, grace: Duration) -> bool {
    (now - last).to_std().is_ok_and(|gap| gap > grace)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::PluginRepository;
    use crate::testing::{ADMIN, Host, get_as, plugin_host, plugin_host_with, post_json};
    use doc_eventbus::{ConsumerGroup, TopicFilter};
    use doc_plugin_protocol::data::{Collection, Declaration, Field};
    use doc_plugin_protocol::{Capability, CustomPermission, Nav, OperationParam};

    #[test]
    fn operations_are_checked_at_registration() {
        let increment = || {
            Operation::new("increment", "Count one", "counters/{counter}/increment")
                .param(OperationParam::required("counter", "Counter"))
        };
        assert!(validate_operations(&[increment()]).is_ok());
        let refused = |operation: Operation| validate_operations(&[operation]).is_err();
        assert!(refused(Operation::new("increment", "Count one", "counters/{other}/increment")));
        assert!(refused(Operation::new("increment", "Count one", "counters/{counter")));
        assert!(refused(increment().method("TRACE")));
        assert!(refused(Operation::new("Increment", "Count one", "counters")));
        assert!(refused(Operation::new("up", "Up", "../rbac/groups")));
        assert!(refused(Operation::new("up", "Up", "/counters")));
        assert!(refused(Operation::new("up", "", "counters")));
        assert!(refused(increment().param(OperationParam::optional("counter", "Again"))));
        assert!(validate_operations(&[increment(), increment()]).is_err());
    }

    use crate::data::store::DataStore;
    use http::StatusCode;
    use std::sync::atomic::Ordering;

    fn manifest(id: &str, version: &str) -> Manifest {
        Manifest {
            id: id.into(),
            version: version.into(),
            classification: Classification::Synchronous,
            custom_permissions: vec![
                CustomPermission::user("greetings"),
                CustomPermission::service("greetings"),
            ],
            nav: vec![Nav::new("Hello", "/")],
            ..Manifest::default()
        }
    }

    fn request(manifest: Manifest) -> RegisterRequest {
        RegisterRequest {
            manifest,
            address: "plugin-hello:4440".into(),
            binary_sha256: "a".repeat(64),
            started_at: None,
        }
    }

    async fn register_hello(host: &Host) -> Result<RegisterResponse, RegisterError> {
        let principal = host.as_plugin("hello");
        register(&host.state, &principal, request(manifest("hello", "1.2.3"))).await
    }

    #[tokio::test]
    async fn a_plugin_registers_and_is_loaded_into_running() {
        let host = plugin_host();
        let response = register_hello(&host).await.expect("registered");
        assert_eq!(response.state, PluginState::Loading);
        assert!(!response.secret.is_empty(), "a plugin must be given a secret to answer with");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        assert_eq!(host.plugin.loaded.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn the_secret_is_not_something_the_backend_would_accept_as_a_token() {
        let host = plugin_host();
        let response = register_hello(&host).await.expect("registered");
        assert!(
            crate::secrets::TokenKind::of(response.secret.expose()).is_none(),
            "an instance secret must not look like a platform token"
        );
    }

    #[tokio::test]
    async fn a_plugin_that_registers_too_often_is_made_to_wait() {
        let mut config = plugin_host().state.config.as_ref().clone();
        config.limits.registrations_per_minute = 2;
        let host = plugin_host_with(config);
        for _ in 0..2 {
            let _ = register_hello(&host).await;
            host.settle("hello").await;
        }
        let err = register_hello(&host).await.expect_err("a third within the minute");
        assert!(matches!(err, RegisterError::TooOften { .. }), "{err}");
        assert_eq!((err.status(), err.kind()), (429, "too-many-requests"));
        let principal = host.as_plugin("rbac");
        let other = register(&host.state, &principal, request(manifest("rbac", "1.0.0"))).await;
        assert!(
            !matches!(other, Err(RegisterError::TooOften { .. })),
            "rbac has its own allowance"
        );
    }

    #[tokio::test]
    async fn a_token_for_another_plugin_cannot_register_this_one() {
        let host = plugin_host();
        let principal = host.as_plugin("rbac");
        let err = register(&host.state, &principal, request(manifest("hello", "1.2.3")))
            .await
            .expect_err("refused");
        assert_eq!(err.status(), 403);
        assert_eq!(err.kind(), "wrong-token");
        assert!(host.state_of("hello").await.is_none(), "nothing may be registered");
    }

    #[tokio::test]
    async fn only_a_plugin_token_may_register() {
        let host = plugin_host();
        let user = Principal::Plugin { id: "hello".into() };
        assert!(register(&host.state, &user, request(manifest("hello", "1.2.3"))).await.is_ok());

        let host = plugin_host();
        let account = host.identity.add_service_account("operator");
        let principal = Principal::ServiceAccount(account);
        let err = register(&host.state, &principal, request(manifest("hello", "1.2.3")))
            .await
            .expect_err("refused");
        assert_eq!(err.kind(), "wrong-token");
    }

    #[tokio::test]
    async fn a_reserved_id_is_refused() {
        let host = plugin_host();
        let principal = host.as_plugin("core");
        let err = register(&host.state, &principal, request(manifest("core", "1.0.0")))
            .await
            .expect_err("refused");
        assert_eq!(err.kind(), "reserved-id");
        assert_eq!(err.status(), 403);
    }

    #[tokio::test]
    async fn an_id_that_is_not_a_plugin_id_is_refused() {
        let host = plugin_host();
        for bad in ["Hello", "9lives", "has_underscore", &"x".repeat(33), ""] {
            let principal = host.as_plugin(bad);
            let err = register(&host.state, &principal, request(manifest(bad, "1.0.0")))
                .await
                .expect_err("refused");
            assert_eq!(err.kind(), "bad-id", "{bad} should not be a plugin ID");
        }
    }

    #[tokio::test]
    async fn a_version_that_is_not_semver_is_refused() {
        let host = plugin_host();
        let principal = host.as_plugin("hello");
        for bad in ["1.2", "1.2.3.4", "v1.2.3", "01.2.3", "", "1.2.x"] {
            let err = register(&host.state, &principal, request(manifest("hello", bad)))
                .await
                .expect_err("refused");
            assert_eq!(err.kind(), "bad-version", "{bad} should not be a version");
        }
        for good in ["1.2.3", "0.1.0", "1.0.0-rc.1", "1.0.0+build.5"] {
            assert!(valid_version(good), "{good} should be a version");
        }
    }

    #[tokio::test]
    async fn a_capability_the_plugin_is_not_allow_listed_for_is_refused() {
        let host = plugin_host();
        let principal = host.as_plugin("hello");
        let mut wanted = manifest("hello", "1.0.0");
        wanted.capabilities = vec![Capability::IdentityProvider];
        let err = register(&host.state, &principal, request(wanted)).await.expect_err("refused");
        assert_eq!(err.kind(), "capability-refused");
        assert_eq!(err.status(), 403);
        assert!(err.to_string().contains("identity-provider"));
    }

    #[tokio::test]
    async fn an_allow_listed_capability_is_accepted() {
        let host = plugin_host();
        let principal = host.as_plugin("rbac");
        let mut wanted = manifest("rbac", "1.0.0");
        wanted.capabilities = vec![Capability::PermissionProvider];
        register(&host.state, &principal, request(wanted)).await.expect("registered");
        assert_eq!(host.settle("rbac").await, Some(PluginState::Running));
    }

    #[tokio::test]
    async fn public_routes_need_the_public_routes_capability() {
        let host = plugin_host();
        let principal = host.as_plugin("hello");
        let mut wanted = manifest("hello", "1.0.0");
        wanted.public_routes = vec!["ui/badge".into()];
        let err = register(&host.state, &principal, request(wanted)).await.expect_err("refused");
        assert_eq!(err.kind(), "public-routes-refused");
    }

    #[tokio::test]
    async fn a_custom_permission_that_is_not_a_name_is_refused() {
        let host = plugin_host();
        let principal = host.as_plugin("hello");
        let mut wanted = manifest("hello", "1.0.0");
        wanted.custom_permissions = vec![CustomPermission::user("Not A Name")];
        let err = register(&host.state, &principal, request(wanted)).await.expect_err("refused");
        assert_eq!(err.kind(), "bad-permission");
        assert_eq!(err.status(), 400);
    }

    #[tokio::test]
    async fn a_known_version_with_a_different_binary_is_refused() {
        let host = plugin_host();
        host.store.remember("hello", "1.2.3", &"b".repeat(64));
        let err = register_hello(&host).await.expect_err("refused");
        assert_eq!(err.kind(), "hash-conflict");
        assert_eq!(err.status(), 409);
    }

    #[tokio::test]
    async fn allow_rebuilds_lets_a_rebuilt_binary_register_and_remembers_it() {
        let mut config = Config::default();
        config.plugins.ids = vec!["hello".into()];
        config.plugins.allow_rebuilds = true;
        let host = plugin_host_with(config);
        host.store.remember("hello", "1.2.3", &"b".repeat(64));

        register_hello(&host).await.expect("a rebuild is let through");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        assert_eq!(
            host.store.version_hash("hello", "1.2.3").await.expect("looked up"),
            Some("a".repeat(64)),
            "the rebuild is the version now, so turning the flag off later holds it to this binary"
        );
    }

    #[tokio::test]
    async fn the_same_version_with_the_same_binary_registers_again() {
        let host = plugin_host();
        host.store.remember("hello", "1.2.3", &"a".repeat(64));
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
    }

    #[tokio::test]
    async fn registration_records_the_default_and_declared_permissions() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        let recorded = host.store.permissions("hello").await.expect("permissions");
        let written: Vec<String> =
            recorded.iter().map(|p| format!("{}:{}", p.kind, p.name)).collect();
        assert!(written.contains(&"user:".to_string()));
        assert!(written.contains(&"service:".to_string()));
        assert!(written.contains(&"pluginuser:greetings".to_string()));
        assert!(written.contains(&"pluginservice:greetings".to_string()));
    }

    #[tokio::test]
    async fn registration_records_the_version_and_the_address() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        let record = host.store.record("hello").expect("a row");
        assert_eq!(record.version, "1.2.3");
        assert_eq!(record.address, "plugin-hello:4440");
        assert_eq!(record.classification, "synchronous");
        assert_eq!(
            host.store.version_hash("hello", "1.2.3").await.expect("looked up"),
            Some("a".repeat(64))
        );
    }

    fn greetings() -> Declaration {
        Declaration::default().collection(
            "greetings",
            Collection::new().field("id", Field::uuid().key()).field("name", Field::text()),
        )
    }

    #[tokio::test]
    async fn collections_are_made_before_load_and_the_running_version_is_the_baseline() {
        let host = plugin_host();
        let principal = host.as_plugin("hello");
        let wanted = Manifest { data: greetings(), ..manifest("hello", "1.0.0") };
        register(&host.state, &principal, request(wanted)).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        let declarations = host.data.declarations("hello").await.expect("read");
        assert_eq!(declarations.storage, greetings());
        assert_eq!(declarations.serving, Some(greetings()));
    }

    #[tokio::test]
    async fn storage_that_cannot_be_prepared_puts_the_plugin_in_error() {
        let host = plugin_host();
        host.data.set_broken(true);
        let principal = host.as_plugin("hello");
        let wanted = Manifest { data: greetings(), ..manifest("hello", "1.0.0") };
        register(&host.state, &principal, request(wanted)).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Error));
        assert!(host.error_of("hello").await.expect("a reason").contains("storage"));
        assert_eq!(host.plugin.loaded.load(Ordering::SeqCst), 0, "load must not have been called");
    }

    #[tokio::test]
    async fn a_load_that_fails_puts_the_plugin_in_error() {
        let host = plugin_host();
        host.plugin.set_failing(Some("hello panics on purpose"));
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Error));
        assert!(host.error_of("hello").await.expect("a reason").contains("panics on purpose"));
    }

    #[tokio::test]
    async fn a_plugin_that_cannot_be_dialled_is_refused_rather_than_half_registered() {
        let host = plugin_host();
        host.connector.set_unreachable(Some("no route to plugin-hello"));
        let err = register_hello(&host).await.expect_err("refused");
        assert_eq!(err.status(), 503);
        assert!(host.state_of("hello").await.is_none());
    }

    /// Lifecycle calls are the plugin's own work, so their context names the plugin itself — and
    /// each one stops working the moment its call returns.
    #[tokio::test]
    async fn every_lifecycle_call_carries_a_context_that_ends_with_it() {
        let host = plugin_host();
        host.plugin.resolve_with("hello", host.state.plugins.contexts.clone());
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        cancel(&host.state, "hello").await.expect("cancelled");
        transition(&host.state, "hello", PluginState::Running, None).await.expect("resumed");
        unload(&host.state, "hello").await.expect("unloaded");

        let seen = host.plugin.contexts();
        assert_eq!(seen.len(), 3, "load, cancel and unload each get their own");
        for (token, during) in &seen {
            assert_eq!(during.as_deref(), Some("plugin:hello"));
            assert!(host.state.plugins.contexts.resolve("hello", token).is_none(), "ended");
        }
    }

    #[tokio::test]
    async fn a_subscription_that_is_not_a_topic_filter_is_refused() {
        let host = plugin_host();
        let principal = host.as_plugin("hello");
        let mut wanted = manifest("hello", "1.0.0");
        wanted.subscriptions = vec!["platform..plugin".into()];
        let err = register(&host.state, &principal, request(wanted)).await.expect_err("refused");
        assert_eq!((err.status(), err.kind()), (400, "bad-subscription"));
    }

    #[tokio::test]
    async fn every_allowed_transition_is_accepted() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));

        cancel(&host.state, "hello").await.expect("running to cancelled");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Cancelled));
        assert_eq!(host.plugin.cancelled.load(Ordering::SeqCst), 1);

        transition(&host.state, "hello", PluginState::Running, None)
            .await
            .expect("cancelled to running");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));

        host.plugin.set_carries(Some(json!({ "greeted": 7 })));
        let carried = unload(&host.state, "hello").await.expect("running to unloading");
        assert_eq!(carried, Some(json!({ "greeted": 7 })));
        assert!(host.state_of("hello").await.is_none(), "an unloaded plugin leaves the registry");
    }

    #[tokio::test]
    async fn an_error_can_be_recovered_by_loading_again() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        transition(&host.state, "hello", PluginState::Error, Some("it fell over")).await.unwrap();
        transition(&host.state, "hello", PluginState::Loading, None)
            .await
            .expect("error to loading");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Loading));
    }

    #[tokio::test]
    async fn an_illegal_transition_is_refused() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        transition(&host.state, "hello", PluginState::Error, Some("it fell over")).await.unwrap();

        // §5 allows `error` only to `loading` or `unloading`.
        for illegal in [PluginState::Running, PluginState::Cancelled] {
            let err = transition(&host.state, "hello", illegal, None).await.expect_err("refused");
            assert!(
                matches!(err, TransitionError::Illegal { from: "error", .. }),
                "error must not become {}",
                illegal.as_str()
            );
        }
        assert_eq!(host.state_of("hello").await, Some(PluginState::Error));
    }

    #[tokio::test]
    async fn a_transition_for_a_plugin_that_never_registered_is_unknown() {
        let host = plugin_host();
        let err = transition(&host.state, "ghost", PluginState::Running, None)
            .await
            .expect_err("refused");
        assert!(matches!(err, TransitionError::Unknown(_)));
        assert_eq!(err.status(), 404);
    }

    async fn published(host: &Host, count: usize) -> Vec<PluginChange> {
        let filter = TopicFilter::new("platform.plugin.>").expect("filter");
        let group = ConsumerGroup::new("test", filter);
        let mut seen = host.state.buses.events.subscribe(group).await.expect("subscribed");
        let mut changes = Vec::new();
        for _ in 0..count {
            let delivery = seen.next().await.expect("an event");
            assert_eq!(delivery.event.topic.as_str(), "platform.plugin.hello.state");
            changes.push(serde_json::from_value(delivery.event.payload.clone()).expect("a change"));
            seen.ack(delivery.id).await.expect("acked");
        }
        changes
    }

    /// Each event carries the whole change, so the plugin probe can keep its rows from events alone.
    #[tokio::test]
    async fn every_change_is_published() {
        let host = plugin_host();
        let response = register_hello(&host).await.expect("registered");
        let instance = response.instance.expect("an instance");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        cancel(&host.state, "hello").await.expect("cancelled");
        unload(&host.state, "hello").await.expect("unloaded");

        let changes = published(&host, 5).await;
        let states: Vec<Option<PluginState>> = changes.iter().map(|change| change.state).collect();
        let (loading, running, cancelled, unloading) = (
            PluginState::Loading,
            PluginState::Running,
            PluginState::Cancelled,
            PluginState::Unloading,
        );
        assert_eq!(states, [Some(loading), Some(running), Some(cancelled), Some(unloading), None]);
        for change in &changes {
            assert_eq!((change.instance, change.version.as_str()), (instance, "1.2.3"));
            assert_eq!(change.classification, Classification::Synchronous);
            assert!(change.registered_at <= change.since && change.since <= change.at);
        }
        assert!(changes.windows(2).all(|pair| pair[0].at <= pair[1].at), "in order");
    }

    #[tokio::test]
    async fn a_new_error_in_the_same_state_is_published_as_the_same_stay() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        transition(&host.state, "hello", PluginState::Error, Some("unreachable")).await.unwrap();
        let entry = host.state.plugins.get("hello").await.expect("registered");
        fail(&host.state, &entry, "load failed: boom").await;

        let changes = published(&host, 4).await;
        let (before, after) = (&changes[2], &changes[3]);
        assert_eq!(
            (before.state, after.state),
            (Some(PluginState::Error), Some(PluginState::Error))
        );
        assert_eq!(after.error.as_deref(), Some("load failed: boom"));
        assert_eq!(after.since, before.since, "still the same stay in error");
        assert!(after.at > before.at);
    }

    #[tokio::test]
    async fn a_missed_liveness_report_makes_a_plugin_unreachable() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));

        sweep(&host.state, Duration::from_secs(3600)).await;
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running), "still within grace");

        sweep(&host.state, Duration::ZERO).await;
        assert_eq!(host.state_of("hello").await, Some(PluginState::Error));
        assert_eq!(host.error_of("hello").await.as_deref(), Some(UNREACHABLE));
    }

    #[tokio::test]
    async fn an_unreachable_plugin_that_reports_again_is_loaded_again() {
        let host = plugin_host();
        let principal = host.as_plugin("hello");
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        sweep(&host.state, Duration::ZERO).await;
        assert_eq!(host.error_of("hello").await.as_deref(), Some(UNREACHABLE));

        let report = Liveness {
            id: "hello".into(),
            state: PluginState::Running,
            error: None,
            instance: None,
        };
        liveness(&host.state, &principal, report).await.expect("reported");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running), "the host had slept");
        assert_eq!(host.error_of("hello").await, None);
    }

    #[tokio::test]
    async fn a_liveness_report_does_not_undo_any_other_error() {
        let host = plugin_host();
        let principal = host.as_plugin("hello");
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        transition(&host.state, "hello", PluginState::Error, Some("marked by an operator"))
            .await
            .unwrap();

        let report = Liveness {
            id: "hello".into(),
            state: PluginState::Running,
            error: None,
            instance: None,
        };
        liveness(&host.state, &principal, report).await.expect("reported");
        assert_eq!(host.settle("hello").await, Some(PluginState::Error));
        assert_eq!(host.error_of("hello").await.as_deref(), Some("marked by an operator"));
    }

    #[test]
    fn a_wall_clock_gap_longer_than_the_grace_is_the_backend_pausing() {
        let grace = Duration::from_secs(20);
        let last = Utc::now();
        assert!(!paused(last, last + chrono::Duration::seconds(10), grace), "on time");
        assert!(!paused(last, last + chrono::Duration::seconds(19), grace), "a little late");
        assert!(paused(last, last + chrono::Duration::hours(2), grace), "the host slept");
        assert!(!paused(last, last - chrono::Duration::seconds(30), grace), "the clock went back");
    }

    #[tokio::test]
    async fn a_liveness_report_keeps_a_plugin_alive_and_carries_its_state() {
        let host = plugin_host();
        let principal = host.as_plugin("hello");
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));

        let report = Liveness {
            id: "hello".into(),
            state: PluginState::Running,
            error: None,
            instance: None,
        };
        liveness(&host.state, &principal, report).await.expect("reported");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));

        // A plugin that has decided it is in error says so, and the backend believes it.
        let report = Liveness {
            instance: None,
            id: "hello".into(),
            state: PluginState::Error,
            error: Some("its own reason".into()),
        };
        liveness(&host.state, &principal, report).await.expect("reported");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Error));
        assert_eq!(host.error_of("hello").await.as_deref(), Some("its own reason"));
    }

    /// A resumed plugin goes on beating the state it had before it was told, so believing a
    /// liveness report for anything but a failure would undo the operator's resume a second later.
    #[tokio::test]
    async fn a_stale_liveness_report_cannot_undo_a_resume() {
        let host = plugin_host();
        let principal = host.as_plugin("hello");
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));

        cancel(&host.state, "hello").await.expect("cancelled");
        transition(&host.state, "hello", PluginState::Running, None).await.expect("resumed");

        let report = Liveness {
            id: "hello".into(),
            state: PluginState::Cancelled,
            error: None,
            instance: None,
        };
        liveness(&host.state, &principal, report).await.expect("reported");
        assert_eq!(
            host.state_of("hello").await,
            Some(PluginState::Running),
            "the backend drives the lifecycle; a liveness report only signals failure"
        );
    }

    #[tokio::test]
    async fn a_liveness_report_cannot_be_sent_for_another_plugin() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));

        let principal = host.as_plugin("rbac");
        let report =
            Liveness { id: "hello".into(), state: PluginState::Error, error: None, instance: None };
        let err = liveness(&host.state, &principal, report).await.expect_err("refused");
        assert!(matches!(err, TransitionError::Unknown(_)));
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running), "unchanged");
    }

    #[tokio::test]
    async fn a_liveness_report_from_a_plugin_the_registry_forgot_says_so() {
        let host = plugin_host();
        let principal = host.as_plugin("hello");
        let report = Liveness {
            id: "hello".into(),
            state: PluginState::Running,
            error: None,
            instance: None,
        };
        let err = liveness(&host.state, &principal, report).await.expect_err("refused");
        assert!(matches!(err, TransitionError::Unknown(_)));
        assert_eq!(err.status(), 404, "404 is what tells a plugin to register again");
    }

    #[tokio::test]
    async fn registering_again_replaces_the_earlier_registration() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));

        let again = register_hello(&host).await.expect("registered again");
        let instance = again.instance.expect("an instance");
        for _ in 0..200 {
            let current = host.state.plugins.get("hello").await.expect("registered");
            if current.instance == instance && current.state == PluginState::Running {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let current = host.state.plugins.get("hello").await.expect("registered");
        assert_eq!((current.instance, current.state), (instance, PluginState::Running));
        assert_eq!(host.plugin.loaded.load(Ordering::SeqCst), 2);
        assert_eq!(host.state.plugins.list().await.len(), 1, "only one hello may be registered");
    }

    #[tokio::test]
    async fn what_unload_returned_is_handed_to_the_next_load() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        host.plugin.set_carries(Some(json!({ "greeted": 12 })));
        unload(&host.state, "hello").await.expect("unloaded");

        register_hello(&host).await.expect("registered again");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        assert_eq!(host.plugin.saw_previous(), Some(json!({ "greeted": 12 })));
    }

    #[tokio::test]
    async fn the_registry_is_served_to_an_administrator() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));

        let (status, body, _) = get_as(&host.app, "/api/v1/plugins", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        let listed = body["plugins"].as_array().expect("plugins");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["id"], "hello");
        assert_eq!(listed[0]["state"], "running");
        assert_eq!(listed[0]["version"], "1.2.3");
    }

    #[tokio::test]
    async fn the_registry_needs_a_permission() {
        let host = plugin_host();
        let (status, _, _) = crate::testing::get(&host.app, "/api/v1/plugins").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn an_unknown_plugin_is_not_found() {
        let host = plugin_host();
        let (status, body, content_type) = get_as(&host.app, "/api/v1/plugins/ghost", ADMIN).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(content_type.as_deref(), Some("application/problem+json"));
        assert_eq!(body["title"], "plugin not found");
    }

    #[tokio::test]
    async fn an_illegal_transition_through_the_api_is_a_conflict() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        transition(&host.state, "hello", PluginState::Error, Some("fell over")).await.unwrap();

        let (status, _, _) = post_json(
            &host.app,
            "/api/v1/plugins/hello/state",
            Some(ADMIN),
            json!({ "state": "running" }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(host.state_of("hello").await, Some(PluginState::Error));
    }

    #[tokio::test]
    async fn an_administrator_can_cancel_and_resume_through_the_api() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));

        let path = "/api/v1/plugins/hello/state";
        let (status, _, _) =
            post_json(&host.app, path, Some(ADMIN), json!({ "state": "cancelled" })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(host.state_of("hello").await, Some(PluginState::Cancelled));

        let (status, _, _) =
            post_json(&host.app, path, Some(ADMIN), json!({ "state": "running" })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
    }

    #[tokio::test]
    async fn the_declared_permissions_are_served_for_the_rbac_plugin() {
        let host = plugin_host();
        register_hello(&host).await.expect("registered");
        let (status, body, _) = get_as(&host.app, "/api/v1/plugins/hello/permissions", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        let named: Vec<String> = body["permissions"]
            .as_array()
            .expect("permissions")
            .iter()
            .map(|value| value.as_str().unwrap_or_default().to_string())
            .collect();
        assert!(named.contains(&"plugin:hello:user".to_string()));
        assert!(named.contains(&"plugin:hello:pluginuser:greetings".to_string()));
    }

    #[tokio::test]
    async fn a_registry_with_no_transport_cannot_bring_a_plugin_up() {
        let mut config = Config::default();
        config.plugins.ids = vec!["hello".into()];
        let host = plugin_host_with(config);
        let host = Host { state: host.state.with_plugins(Registry::default()), ..host };
        let err = register_hello(&host).await.expect_err("refused");
        assert_eq!(err.status(), 503);
    }
}
