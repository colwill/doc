//! What the plugin is configured with: each cluster as a named credential holding the kubeconfig of
//! a read-only service account, the environment each cluster (or a namespace in one) is, which
//! environments are production, how often clusters are read and which namespaces are left out.

use std::collections::BTreeMap;
use std::time::Duration;

use doc_plugin_sdk::{NamedSecrets, Setting, SettingKind, Settings};
use serde_json::json;

pub const ENVIRONMENTS: &str = "environments";
/// Where the Settings page asks which clusters and namespaces, and which environments, there are.
pub const ENVIRONMENT_CHOICES: &str = "settings/environments";
pub const ENVIRONMENT_NAMES: &str = "environment-names";
pub const PRODUCTION: &str = "production";
pub const INTERVAL: &str = "interval";
pub const IGNORED: &str = "ignored-namespaces";

const DEFAULT_INTERVAL: f64 = 60.0;
const DEFAULT_ENVIRONMENTS: [&str; 3] = ["Development", "Staging", "Production"];
const DEFAULT_IGNORED: [&str; 4] =
    ["kube-system", "kube-public", "kube-node-lease", "local-path-storage"];

pub fn named() -> NamedSecrets {
    NamedSecrets::new(
        "Clusters",
        "Each cluster by a name of your choosing, such as prod-eu, holding the kubeconfig of a \
         read-only service account in it: its server, certificate authority and token. The \
         Kubernetes page shows the manifest that makes one.",
    )
}

pub fn declared() -> Vec<Setting> {
    vec![
        Setting::new(ENVIRONMENTS, "Environments", SettingKind::Map)
            .choices_from(ENVIRONMENT_CHOICES)
            .hinted(
                "Each cluster with the environment it is, such as prod-eu with Production. A \
                 namespace can be given its own as cluster/namespace, which wins over its \
                 cluster's. A workload in neither has no environment and counts for no metrics.",
            )
            .grouped("Environments"),
        Setting::new(ENVIRONMENT_NAMES, "Environment names", SettingKind::List)
            .defaulting(json!(DEFAULT_ENVIRONMENTS))
            .hinted("The environments offered above.")
            .grouped("Environments"),
        Setting::new(PRODUCTION, "Production environments", SettingKind::List)
            .defaulting(json!(["Production"]))
            .hinted(
                "A finished rollout in one of these is a deployment for DORA metrics, and a \
                 service with none of its pods available in one is down for Reliability.",
            )
            .grouped("Environments"),
        Setting::new(INTERVAL, "How often each cluster is read", SettingKind::Duration)
            .defaulting(json!(DEFAULT_INTERVAL))
            .between(15.0, 3_600.0)
            .hinted(
                "Rollouts and outages keep the times Kubernetes gives them, so this only decides \
                 how soon they are seen.",
            )
            .grouped("Reading"),
        Setting::new(IGNORED, "Namespaces left out", SettingKind::List)
            .defaulting(json!(DEFAULT_IGNORED))
            .hinted("Namespaces never shown or counted in any cluster, such as kube-system.")
            .grouped("Reading"),
    ]
}

/// The settings a read of the clusters goes by, read once.
#[derive(Debug, Clone)]
pub struct Config {
    /// The clusters' names, which are the named credentials'.
    pub clusters: Vec<String>,
    /// `cluster` or `cluster/namespace` to an environment, as written.
    pub environments: BTreeMap<String, String>,
    pub names: Vec<String>,
    production: Vec<String>,
    pub interval: Duration,
    pub ignored: Vec<String>,
}

impl Config {
    pub fn read(settings: &Settings) -> Self {
        let lines = |key: &str| -> Vec<String> {
            settings
                .list(key)
                .into_iter()
                .map(|line| line.trim().to_string())
                .filter(|line| !line.is_empty())
                .collect()
        };
        let mut clusters = settings.named_names();
        clusters.sort();
        let environments = settings
            .map(ENVIRONMENTS)
            .into_iter()
            .filter_map(|(key, values)| {
                let key = key.trim().to_ascii_lowercase();
                let environment = values
                    .into_iter()
                    .map(|value| value.trim().to_string())
                    .find(|value| !value.is_empty())?;
                (!key.is_empty()).then_some((key, environment))
            })
            .collect();
        Self {
            clusters,
            environments,
            names: lines(ENVIRONMENT_NAMES),
            production: lines(PRODUCTION).iter().map(|name| name.to_ascii_lowercase()).collect(),
            interval: settings
                .duration(INTERVAL)
                .map(|interval| interval.clamp(Duration::from_secs(15), Duration::from_secs(3_600)))
                .unwrap_or(Duration::from_secs_f64(DEFAULT_INTERVAL)),
            ignored: lines(IGNORED).iter().map(|name| name.to_ascii_lowercase()).collect(),
        }
    }

    /// A namespace's environment: its own, else its cluster's.
    pub fn environment_of(&self, cluster: &str, namespace: &str) -> Option<String> {
        self.environments
            .get(&format!("{cluster}/{namespace}").to_ascii_lowercase())
            .or_else(|| self.environments.get(&cluster.to_ascii_lowercase()))
            .cloned()
    }

    pub fn is_production(&self, environment: Option<&str>) -> bool {
        environment.is_some_and(|environment| {
            self.production.iter().any(|name| name.eq_ignore_ascii_case(environment))
        })
    }

    pub fn ignores(&self, namespace: &str) -> bool {
        self.ignored.iter().any(|name| name.eq_ignore_ascii_case(namespace))
    }
}
