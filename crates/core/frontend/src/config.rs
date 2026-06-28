//! Frontend configuration: a TOML file with environment-variable overrides.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use doc_secret::Secret;
use serde::{Deserialize, Serialize};

pub const CONFIG_ENV: &str = "DOC_CONFIG";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub server: Server,
    pub backend: Backend,
    pub secrets: Secrets,
    pub fabric: Fabric,
    pub instance: Instance,
    pub dev: Dev,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Instance {
    /// Shown before the DOC logo, so `DEV` reads as DEV[DOC]. Empty shows the rundoc logo instead.
    pub name: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Dev {
    /// Serves `/dev/boot` and the script that reloads the page when this process is replaced.
    pub reload: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Server {
    pub http_addr: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Backend {
    pub base_url: String,
    /// Bearer token sent with every backend call; empty means no `Authorization` header.
    pub token: Secret<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Secrets {
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FabricMode {
    #[default]
    Memory,
    Cluster,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Fabric {
    pub mode: FabricMode,
    pub port: u16,
    pub event_bus: Vec<String>,
    pub cache_bus: Vec<String>,
}

impl Default for Server {
    fn default() -> Self {
        Self { http_addr: "0.0.0.0:8081".into() }
    }
}

impl Default for Backend {
    fn default() -> Self {
        Self { base_url: "http://127.0.0.1:8080".into(), token: Secret::default() }
    }
}

impl Default for Secrets {
    fn default() -> Self {
        Self { dir: PathBuf::from("/secrets") }
    }
}

impl Default for Fabric {
    fn default() -> Self {
        Self {
            mode: FabricMode::default(),
            port: 4433,
            event_bus: bus_nodes("eventbus"),
            cache_bus: bus_nodes("cachebus"),
        }
    }
}

fn bus_nodes(bus: &str) -> Vec<String> {
    (1..=3).map(|n| format!("{bus}-{n}")).collect()
}

impl Fabric {
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

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let from_env = std::env::var(CONFIG_ENV).ok().map(PathBuf::from);
        let path = path.map(Path::to_path_buf).or(from_env);
        let mut config = match &path {
            Some(path) => {
                let text = std::fs::read_to_string(path)
                    .with_context(|| format!("reading configuration from {}", path.display()))?;
                toml::from_str(&text)
                    .with_context(|| format!("parsing configuration in {}", path.display()))?
            }
            None => Self::default(),
        };
        config.apply_env()?;
        config.adopt_bootstrap_token();
        Ok(config)
    }

    /// The API is guarded from T15, so with no token configured the frontend uses the read-only
    /// service account the bootstrap job wrote to the secrets volume.
    fn adopt_bootstrap_token(&mut self) {
        if !self.backend.token.is_empty() {
            return;
        }
        let path = self.secrets.dir.join("tokens/frontend.token");
        match std::fs::read_to_string(&path) {
            Ok(token) => self.backend.token = Secret::new(token.trim().to_string()),
            Err(err) => {
                tracing::warn!(path = %path.display(), %err, "no backend token; the API will refuse")
            }
        }
    }

    fn apply_env(&mut self) -> Result<()> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        if let Some(v) = var("DOC_HTTP_ADDR") {
            self.server.http_addr = v;
        }
        if let Some(v) = var("DOC_BACKEND_URL") {
            self.backend.base_url = v;
        }
        if let Some(v) = var("DOC_BACKEND_TOKEN") {
            self.backend.token = Secret::new(v);
        }
        if let Some(v) = var("DOC_SECRETS_DIR") {
            self.secrets.dir = PathBuf::from(v);
        }
        if let Some(v) = var("DOC_EVENT_BUS_NODES") {
            self.fabric.event_bus = v.split(',').map(|n| n.trim().to_string()).collect();
        }
        if let Some(v) = var("DOC_CACHE_BUS_NODES") {
            self.fabric.cache_bus = v.split(',').map(|n| n.trim().to_string()).collect();
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
        if let Some(v) = var("DOC_INSTANCE_NAME") {
            self.instance.name = v;
        }
        if let Some(v) = var("DOC_DEV_RELOAD") {
            self.dev.reload = matches!(v.as_str(), "1" | "true" | "yes");
        }
        Ok(())
    }
}
