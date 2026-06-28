//! Platform configuration: a TOML file with environment-variable overrides.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use doc_plugin_protocol::{Capability, Classification};
use doc_secret::Secret;
use serde::{Deserialize, Serialize};

pub const CONFIG_ENV: &str = "DOC_CONFIG";
/// The longest one attempt at an async plugin's background `run` may take.
pub const MAX_ASYNC_DEADLINE: Duration = Duration::from_secs(30);
/// The longest a one-shot plugin's `run` after `load` may take.
pub const MAX_ONE_SHOT_DEADLINE: Duration = Duration::from_secs(600);
/// How long the plugin-runs queue holds a run: as long as the longest background run may take,
/// so a slow one is never handed out twice.
pub const PLUGIN_RUN_LEASE: Duration = MAX_ONE_SHOT_DEADLINE;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub database: Database,
    pub secrets: Secrets,
    pub bootstrap: Bootstrap,
    pub auth: Auth,
    pub fabric: Fabric,
    pub plugins: Plugins,
    pub limits: Limits,
    pub environments: Environments,
    pub instance: Instance,
}

/// Where this DOC is (`[instance]`), which every plugin is given with its settings.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Instance {
    /// The domain people reach DOC at, such as `doc.acme.com`.
    pub domain: String,
    /// The machine's IP address as other machines reach it; empty is the one it has on its
    /// network, found when it starts.
    pub address: String,
}

impl Instance {
    /// Checked and tidied: a bare domain, and an address that is one.
    fn checked(mut self) -> Result<Self> {
        self.domain = self.domain.trim().trim_end_matches('.').to_ascii_lowercase();
        if self.domain.contains(['/', ':', ' ']) {
            anyhow::bail!(
                "[instance] domain is `{}`; give the domain alone, such as doc.acme.com",
                self.domain
            );
        }
        self.address = self.address.trim().to_string();
        if self.address.is_empty() {
            self.address = own_address().unwrap_or_default();
        } else if self.address.parse::<std::net::IpAddr>().is_err() {
            anyhow::bail!("[instance] address is `{}`, which is not an IP address", self.address);
        }
        Ok(self)
    }

    pub fn for_plugins(&self) -> doc_plugin_protocol::calls::Instance {
        doc_plugin_protocol::calls::Instance {
            domain: self.domain.clone(),
            address: self.address.clone(),
        }
    }
}

/// The address a route out of this machine leaves by. Nothing is sent: connecting a UDP socket
/// only picks the route, here towards a documentation address no network carries.
fn own_address() -> Option<String> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then(|| ip.to_string())
}

/// The environments that count as production, which a call guarded against changing production
/// may not change (FEAT-AGENT: runbooks). Each plugin names its own environments; these are the
/// names that mean production wherever they appear.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Environments {
    /// Compared without regard to case.
    pub production: Vec<String>,
}

impl Default for Environments {
    fn default() -> Self {
        Self { production: ["production", "prod", "live"].map(str::to_string).to_vec() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    /// Unknown tokens and refused sign-ins one client may present in a minute.
    pub auth_failures_per_minute: u32,
    /// Registrations one plugin may make in a minute, which a crash-looping plugin backs off from.
    pub registrations_per_minute: u32,
    /// Calls one client may make on one plugin's public routes, its inbound webhooks, in a minute.
    pub public_calls_per_minute: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            auth_failures_per_minute: 20,
            registrations_per_minute: 10,
            public_calls_per_minute: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub http_addr: String,
    pub quic_addr: String,
    /// Addresses and networks whose `x-forwarded-for` names the real client, such as the frontend's.
    pub trusted_proxies: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Database {
    pub url: Secret<String>,
    pub migration_url: Secret<String>,
    pub max_connections: u32,
    /// Where plugins' collections are kept (T62): its own database, on its own cluster if wanted.
    pub plugins_url: Secret<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Secrets {
    pub dir: PathBuf,
    pub owner_uid: Option<u32>,
    pub owner_gid: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Bootstrap {
    pub admins: Vec<String>,
    pub operator: String,
    /// Read-only account the frontend calls the API with, since the API is guarded from T15.
    pub frontend: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Auth {
    /// Development accounts once kept here, which DOC accounts (the `local` plugin) replaced in
    /// T65. An old file that still has them starts, with a warning, and they are not read.
    pub local: Option<toml::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FabricMode {
    /// One process, no replication: used by tests and until the clusters exist (T12).
    #[default]
    Memory,
    /// The three Raft clusters.
    Cluster,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Fabric {
    pub mode: FabricMode,
    /// The port every bus node listens on; node names come from the lists below.
    pub port: u16,
    pub event_bus: Vec<String>,
    pub service_bus: Vec<String>,
    pub cache_bus: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Plugins {
    pub ids: Vec<String>,
    /// §5's allow-list: a capability is refused unless this names the plugin asking for it.
    pub capabilities: BTreeMap<String, Vec<Capability>>,
    /// What the plugins are sorted into, in the order the Plugins page shows them: a section each
    /// there, and a pill beside a plugin's name elsewhere. A plugin in none is shown under Other.
    pub categories: Vec<Category>,
    /// How long a plugin may go without reporting liveness before the backend calls it unreachable.
    pub liveness_grace_s: u64,
    /// Development only: lets a rebuilt binary register under a version it has already used, so
    /// a plugin can be restarted on every edit without a version bump.
    pub allow_rebuilds: bool,
    /// How long a forwarded request or a synchronous `run` may take before the caller gets `504`.
    pub request_deadline_s: u64,
    /// How long one attempt at an async plugin's background `run` may take, up to
    /// `MAX_ASYNC_DEADLINE`. Also bounds background runs a plugin of another classification queues.
    pub async_deadline_s: u64,
    /// How long a one-shot plugin's `run` after `load` may take, up to `MAX_ONE_SHOT_DEADLINE`.
    pub one_shot_deadline_s: u64,
    /// How long a request waits while a hot reload has the plugin paused, before it is `503`.
    pub handover_timeout_s: u64,
    /// How long a hot reload waits for requests already on the old version to finish.
    pub drain_timeout_s: u64,
    /// How long the plugin probe lets a plugin stay `loading` or `unloading` before timing it out.
    pub stuck_after_s: u64,
}

/// One of `[[plugins.categories]]`: its name as people read it, and the plugin IDs in it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Category {
    pub name: String,
    #[serde(default)]
    pub plugins: Vec<String>,
}

impl Default for Plugins {
    fn default() -> Self {
        Self {
            ids: Vec::new(),
            capabilities: BTreeMap::new(),
            categories: Vec::new(),
            liveness_grace_s: 20,
            allow_rebuilds: false,
            request_deadline_s: 30,
            async_deadline_s: 30,
            one_shot_deadline_s: 600,
            handover_timeout_s: 5,
            drain_timeout_s: 10,
            stuck_after_s: 90,
        }
    }
}

impl Plugins {
    pub fn allows(&self, plugin: &str, capability: Capability) -> bool {
        self.capabilities.get(plugin).is_some_and(|allowed| allowed.contains(&capability))
    }

    /// The category a plugin is in: the first that names it.
    pub fn category(&self, plugin: &str) -> Option<&str> {
        self.categories
            .iter()
            .find(|category| category.plugins.iter().any(|named| named == plugin))
            .map(|category| category.name.as_str())
    }

    pub fn grace(&self) -> Duration {
        Duration::from_secs(self.liveness_grace_s.max(1))
    }

    pub fn request_deadline(&self) -> Duration {
        Duration::from_secs(self.request_deadline_s.max(1))
    }

    /// One attempt at a background `run`. Operators may shorten these limits but not lengthen them.
    pub fn run_deadline(&self, classification: Classification) -> Duration {
        match classification {
            Classification::OneShot => {
                Duration::from_secs(self.one_shot_deadline_s.max(1)).min(MAX_ONE_SHOT_DEADLINE)
            }
            _ => Duration::from_secs(self.async_deadline_s.max(1)).min(MAX_ASYNC_DEADLINE),
        }
    }

    pub fn handover_timeout(&self) -> Duration {
        Duration::from_secs(self.handover_timeout_s)
    }

    pub fn drain_timeout(&self) -> Duration {
        Duration::from_secs(self.drain_timeout_s)
    }

    pub fn stuck_after(&self) -> Duration {
        Duration::from_secs(self.stuck_after_s.max(1))
    }
}

impl Default for Server {
    fn default() -> Self {
        Self {
            http_addr: "0.0.0.0:8080".into(),
            quic_addr: "0.0.0.0:4433".into(),
            trusted_proxies: vec!["127.0.0.0/8".into(), "::1/128".into()],
        }
    }
}

impl Default for Database {
    fn default() -> Self {
        Self {
            url: Secret::new("postgres://doc_app@127.0.0.1:5432/status".into()),
            migration_url: Secret::new("postgres://doc_owner@127.0.0.1:5432/status".into()),
            max_connections: 16,
            plugins_url: Secret::new("postgres://doc_data@127.0.0.1:5432/doc_plugins".into()),
        }
    }
}

impl Default for Secrets {
    fn default() -> Self {
        Self { dir: PathBuf::from("/secrets"), owner_uid: None, owner_gid: None }
    }
}

impl Default for Bootstrap {
    fn default() -> Self {
        Self { admins: Vec::new(), operator: "operator".into(), frontend: "frontend".into() }
    }
}

impl Default for Fabric {
    fn default() -> Self {
        Self {
            mode: FabricMode::default(),
            port: 4433,
            event_bus: bus_nodes("eventbus"),
            service_bus: bus_nodes("servicebus"),
            cache_bus: bus_nodes("cachebus"),
        }
    }
}

fn bus_nodes(bus: &str) -> Vec<String> {
    (1..=3).map(|n| format!("{bus}-{n}")).collect()
}

impl Fabric {
    pub fn nodes(&self) -> impl Iterator<Item = &String> {
        self.event_bus.iter().chain(&self.service_bus).chain(&self.cache_bus)
    }

    /// `host:port` addresses for one bus, which is what the cluster client dials.
    pub fn addresses(&self, nodes: &[String]) -> Vec<String> {
        nodes
            .iter()
            .map(
                |node| {
                    if node.contains(':') { node.clone() } else { format!("{node}:{}", self.port) }
                },
            )
            .collect()
    }
}

/// `doc.toml`'s untracked companion, `doc.local.toml`: machine-only settings, such as a login of
/// your own among the bootstrap admins, that must never be in the configuration everyone shares.
fn local_path(path: &Path) -> PathBuf {
    let stem = path.file_stem().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{stem}.local.toml"))
}

fn read_table(path: &Path) -> Result<toml::Table> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading configuration from {}", path.display()))?;
    toml::from_str(&text).with_context(|| format!("parsing configuration in {}", path.display()))
}

/// Lays `local` over `base`: tables merge key by key, and anything else replaces what was there.
fn overlay(base: &mut toml::Table, local: toml::Table) {
    for (key, value) in local {
        match (base.get_mut(&key), value) {
            (Some(toml::Value::Table(below)), toml::Value::Table(above)) => overlay(below, above),
            (_, value) => {
                base.insert(key, value);
            }
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let from_env = std::env::var(CONFIG_ENV).ok().map(PathBuf::from);
        let path = path.map(Path::to_path_buf).or(from_env);
        let mut config = match &path {
            Some(path) => {
                let mut table = read_table(path)?;
                let local = local_path(path);
                if local.exists() {
                    tracing::info!(path = %local.display(), "local configuration applied");
                    overlay(&mut table, read_table(&local)?);
                }
                table
                    .try_into()
                    .with_context(|| format!("parsing configuration in {}", path.display()))?
            }
            None => Self::default(),
        };
        config.apply_env()?;
        config.instance = config.instance.checked()?;
        if config.auth.local.is_some() {
            tracing::warn!(
                "[auth.local] is no longer read: people sign in with DOC accounts, the `local` \
                 plugin. Remove it from the configuration."
            );
        }
        Ok(config)
    }

    fn apply_env(&mut self) -> Result<()> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        if let Some(v) = var("DOC_DATABASE_URL") {
            self.database.url = Secret::new(v);
        }
        if let Some(v) = var("DOC_MIGRATION_DATABASE_URL") {
            self.database.migration_url = Secret::new(v);
        }
        if let Some(v) = var("DOC_PLUGINS_DATABASE_URL") {
            self.database.plugins_url = Secret::new(v);
        }
        if let Some(v) = var("DOC_HTTP_ADDR") {
            self.server.http_addr = v;
        }
        if let Some(v) = var("DOC_TRUSTED_PROXIES") {
            self.server.trusted_proxies = v.split(',').map(|n| n.trim().to_string()).collect();
        }
        if let Some(v) = var("DOC_QUIC_ADDR") {
            self.server.quic_addr = v;
        }
        if let Some(v) = var("DOC_SECRETS_DIR") {
            self.secrets.dir = PathBuf::from(v);
        }
        if let Some(v) = var("DOC_SECRETS_UID").and_then(|v| v.parse().ok()) {
            self.secrets.owner_uid = Some(v);
        }
        if let Some(v) = var("DOC_SECRETS_GID").and_then(|v| v.parse().ok()) {
            self.secrets.owner_gid = Some(v);
        }
        if let Some(v) = var("DOC_EVENT_BUS_NODES") {
            self.fabric.event_bus = v.split(',').map(|n| n.trim().to_string()).collect();
        }
        if let Some(v) = var("DOC_SERVICE_BUS_NODES") {
            self.fabric.service_bus = v.split(',').map(|n| n.trim().to_string()).collect();
        }
        if let Some(v) = var("DOC_CACHE_BUS_NODES") {
            self.fabric.cache_bus = v.split(',').map(|n| n.trim().to_string()).collect();
        }
        if let Some(v) = var("DOC_INSTANCE_DOMAIN") {
            self.instance.domain = v;
        }
        if let Some(v) = var("DOC_INSTANCE_ADDRESS") {
            self.instance.address = v;
        }
        if let Some(v) = var("DOC_PLUGINS_ALLOW_REBUILDS").and_then(|v| v.parse().ok()) {
            self.plugins.allow_rebuilds = v;
        }
        // A value that is neither is refused rather than ignored: `just dev fabric=cluster`
        // passes the whole string, and would otherwise run in memory without saying so.
        match var("DOC_FABRIC_MODE").as_deref() {
            Some("memory") => self.fabric.mode = FabricMode::Memory,
            Some("cluster") => self.fabric.mode = FabricMode::Cluster,
            Some(other) => {
                anyhow::bail!("DOC_FABRIC_MODE is `{other}`; it must be memory or cluster")
            }
            None => {}
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_configuration_parses() {
        let text = include_str!("../../../../config/doc.toml");
        let config: Config = toml::from_str(text).expect("example configuration parses");
        assert!(config.plugins.ids.contains(&"rbac".to_string()));
        assert_eq!(config.fabric.nodes().count(), 9);
        assert!(config.auth.local.is_none(), "development accounts left configuration in T65");
        assert!(!config.plugins.allow_rebuilds, "rebuilds under a known version are dev-only");
    }

    #[test]
    fn a_local_file_is_laid_over_the_configuration() {
        let dir = std::env::temp_dir().join(format!("doc-config-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let shared = dir.join("doc.toml");
        std::fs::write(&shared, include_str!("../../../../config/doc.toml")).unwrap();
        let without = Config::load(Some(&shared)).expect("loads alone");
        assert_ne!(without.bootstrap.admins, ["ada"]);

        // A file from before T65 still has development accounts, which start with a warning.
        let local = "[bootstrap]\nadmins = [\"ada\"]\n\n[auth.local]\nenabled = true\n\
                     users = [{ username = \"ada\", password_hash = \"$argon2id$x\" }]\n";
        std::fs::write(dir.join("doc.local.toml"), local).unwrap();
        let with = Config::load(Some(&shared)).expect("loads with the local file");
        assert_eq!(with.bootstrap.admins, ["ada"]);
        assert_eq!(with.bootstrap.operator, without.bootstrap.operator, "tables merge key by key");
        assert_eq!(with.plugins.ids, without.plugins.ids, "untouched tables are kept whole");
        assert_eq!(with.fabric.nodes().count(), 9);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn background_runs_are_held_to_their_classifications_limit() {
        let mut plugins = Plugins::default();
        assert_eq!(plugins.run_deadline(Classification::Async), Duration::from_secs(30));
        assert_eq!(plugins.run_deadline(Classification::OneShot), Duration::from_secs(600));
        assert_eq!(plugins.run_deadline(Classification::Synchronous), Duration::from_secs(30));

        plugins.async_deadline_s = 3600;
        plugins.one_shot_deadline_s = 3600;
        assert_eq!(plugins.run_deadline(Classification::Async), MAX_ASYNC_DEADLINE);
        assert_eq!(plugins.run_deadline(Classification::OneShot), MAX_ONE_SHOT_DEADLINE);

        plugins.async_deadline_s = 5;
        plugins.one_shot_deadline_s = 60;
        assert_eq!(plugins.run_deadline(Classification::Async), Duration::from_secs(5));
        assert_eq!(plugins.run_deadline(Classification::OneShot), Duration::from_secs(60));
        assert!(PLUGIN_RUN_LEASE >= MAX_ASYNC_DEADLINE.max(MAX_ONE_SHOT_DEADLINE));
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<Config>("[server]\nhttp_addrs = \"0.0.0.0:1\"\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"), "{err}");
    }
}
