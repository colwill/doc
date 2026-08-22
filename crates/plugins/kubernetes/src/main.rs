//! Kubernetes: each connected cluster's namespaces, deployments and their pods, read with a
//! read-only service account, and each cluster (or a namespace in one) tied to an environment,
//! such as Development or Production. What it sees feeds the platform's metrics: finished
//! production rollouts are DORA's deployments, every rollout is a delivery run for CI/CD/CT, and a
//! production service with no pods available is an outage for Reliability.
//!
//! Its long-running `run` reads every cluster in turn; a task started as whoever let it read the
//! Catalogue maps services to repositories. Rollouts are published as `plugin.kubernetes.rollout.*`
//! and outages as `plugin.kubernetes.outage.started` and `.ended`, for automations.

mod api;
mod catalogue;
mod kube;
mod settings;
mod store;
mod ui;
mod watch;

use std::collections::BTreeMap;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, Manifest, Nav, Plugin, PluginError, Request, ResourcePanel, Response,
    RunInput, RunOutput, Settings, SettingsChanged, SettingsVerdict,
};
use serde_json::{Value, json};

use kube::Connection;
use watch::Watcher;

pub const ID: &str = "kubernetes";

#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub detail: String,
}

impl Refusal {
    pub fn bad(detail: impl Into<String>) -> Self {
        Self { status: 400, detail: detail.into() }
    }

    pub fn missing(detail: impl Into<String>) -> Self {
        Self { status: 404, detail: detail.into() }
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self { status: 503, detail: detail.into() }
    }

    pub fn response(&self) -> Response {
        let kind = match self.status {
            400 => "bad-request",
            403 => "forbidden",
            404 => "not-found",
            _ => "unavailable",
        };
        Response::problem(self.status, kind, &self.detail)
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        match err {
            PluginError::Message(detail) => Self::bad(detail),
            err => match err.problem() {
                Some((status, _)) if status < 500 => Self { status, detail: err.detail() },
                _ => Self::unavailable(err.to_string()),
            },
        }
    }
}

/// Who is asking, as a record says it.
pub fn who(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "someone".to_string())
}

/// What a read-only service account needs, and what it must not be able to do.
const NEEDED: [(&str, &str, &str); 6] = [
    ("list", "", "namespaces"),
    ("list", "", "nodes"),
    ("list", "", "pods"),
    ("list", "apps", "deployments"),
    ("list", "apps", "replicasets"),
    ("get", "", "namespaces"),
];
const FORBIDDEN: [(&str, &str, &str); 6] = [
    ("create", "apps", "deployments"),
    ("update", "apps", "deployments"),
    ("patch", "apps", "deployments"),
    ("delete", "apps", "deployments"),
    ("delete", "", "pods"),
    ("get", "", "secrets"),
];

/// Tries one cluster's kubeconfig: that the server answers, and that the account can read what
/// the plugin reads but change nothing and read no secrets.
async fn tried(kubeconfig: &str) -> Result<String, String> {
    let connection = Connection::parse(kubeconfig)?;
    let api = connection.client()?;
    let version: kube::Version = api.get("version").await.map_err(|failed| failed.detail)?;
    // Asked all at once, since a check has ten seconds and a cluster may be far away.
    let api = &api;
    let asked = |rules: &'static [(&'static str, &'static str, &'static str)]| {
        futures::future::join_all(rules.iter().map(|&(verb, group, resource)| async move {
            let may = api.may(verb, group, resource).await.map_err(|failed| failed.detail)?;
            Ok::<_, String>((may, format!("{verb} {resource}")))
        }))
    };
    let mut cannot = Vec::new();
    for answer in asked(&NEEDED).await {
        let (may, what) = answer?;
        if !may {
            cannot.push(what);
        }
    }
    if !cannot.is_empty() {
        return Err(format!(
            "its service account cannot {}; bind it to the doc-read-only ClusterRole",
            cannot.join(", ")
        ));
    }
    let mut can = Vec::new();
    for answer in asked(&FORBIDDEN).await {
        let (may, what) = answer?;
        if may {
            can.push(what);
        }
    }
    if !can.is_empty() {
        return Err(format!(
            "its service account can {}; DOC connects only with one that reads, such as the \
             doc-read-only ClusterRole gives",
            can.join(", ")
        ));
    }
    let insecure = match connection.insecure() {
        true => ", without checking its certificate",
        false => "",
    };
    Ok(format!("Kubernetes {}{insecure}", version.git_version))
}

#[derive(Default)]
struct Kubernetes {
    watcher: Watcher,
}

#[async_trait]
impl Plugin for Kubernetes {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        tracing::info!(version = backend.version(), "kubernetes loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        self.watcher.stop();
        Ok(None)
    }

    /// The reader, when the backend starts the long-running run; a read of the Catalogue, when
    /// started as whoever let the plugin do that.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        if input.payload["catalogue"].as_bool() == Some(true) {
            return Ok(RunOutput { payload: catalogue::read(backend).await? });
        }
        if input.task.is_none() && input.payload.is_null() {
            self.watcher.run(backend).await?;
            return Ok(RunOutput { payload: json!({ "stopped": true }) });
        }
        Err(PluginError::from("kubernetes runs only its reader and its Catalogue reads"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        self.watcher.stop();
        Ok(())
    }

    /// Each read takes the settings afresh, so a change is read at once rather than reloaded.
    async fn settings_changed(
        &self,
        _backend: &Backend,
        _changed: &SettingsChanged,
    ) -> Result<bool, PluginError> {
        self.watcher.wake();
        Ok(true)
    }

    async fn settings_check(&self, _backend: &Backend, proposed: &Settings) -> SettingsVerdict {
        let mut problems: BTreeMap<String, String> = BTreeMap::new();
        let mut said = Vec::new();
        let names = proposed.named_names();
        let tries = names.iter().filter_map(|name| {
            let kubeconfig = proposed.named(name)?;
            Some(async move { (name, tried(kubeconfig.expose()).await) })
        });
        for (name, outcome) in futures::future::join_all(tries).await {
            match outcome {
                Ok(found) => said.push(format!("{name}: {found}")),
                Err(problem) => {
                    problems.insert(name.clone(), problem);
                }
            }
        }
        let config = settings::Config::read(proposed);
        let unknown: Vec<&String> = config
            .environments
            .keys()
            .filter(|key| {
                let cluster = key.split('/').next().unwrap_or(key);
                !names.iter().any(|name| name.eq_ignore_ascii_case(cluster))
            })
            .collect();
        if !unknown.is_empty() && !names.is_empty() {
            problems.insert(
                settings::ENVIRONMENTS.to_string(),
                format!(
                    "{} names no cluster; the clusters are {}",
                    unknown.iter().map(|key| key.as_str()).collect::<Vec<_>>().join(", "),
                    names.join(", ")
                ),
            );
        }
        match problems.is_empty() {
            true if !said.is_empty() => SettingsVerdict::saying(&said.join("; ")),
            true => SettingsVerdict::ok(),
            false => {
                let problem = problems
                    .iter()
                    .filter(|(key, _)| names.contains(key))
                    .map(|(name, problem)| format!("{name}: {problem}"))
                    .collect::<Vec<_>>();
                problems.retain(|key, _| !names.contains(key));
                SettingsVerdict {
                    problems,
                    problem: (!problem.is_empty()).then(|| problem.join("; ")),
                    message: None,
                }
            }
        }
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let path = request.path.trim_end_matches('/').to_string();
        let segments: Vec<&str> = path.split('/').collect();
        match segments.as_slice() {
            ["ui", route @ ..] => ui::handle(backend, &self.watcher, &request, route).await,
            ["api", route @ ..] => api::handle(backend, &request, route).await,
            _ => Refusal::missing("no such route").response(),
        }
    }
}

doc_plugin_sdk::main!(
    Kubernetes,
    Manifest {
        id: ID.into(),
        classification: Classification::LongRunning,
        nav: vec![
            Nav::new("Kubernetes", "/")
                .described(
                    "Each cluster's namespaces, deployments and pods, the environment each is, \
                     and every rollout",
                )
                .grouped("Platform"),
        ],
        resource_panels: vec![ResourcePanel::new("service", "Kubernetes", "/panel")],
        settings: settings::declared(),
        named_secrets: Some(settings::named()),
        data: store::declaration(),
        ..Manifest::default()
    }
);
