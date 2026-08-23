//! Repository Insights: runs ccc (github.com/colwill/ccc) over a repository each time a pull
//! request is merged into its main branch, through an archive its source plugin links to, and
//! keeps what it finds for a page of its own. A repository needs nothing added to it.

mod api;
mod archive;
mod report;
mod scanner;
mod settings;
mod store;
mod ui;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use doc_plugin_sdk::{
    Backend, Classification, CustomPermission, Event, Insight, Manifest, Nav, Operation,
    OperationParam, Plugin, PluginError, Request, ResourcePanel, Response, RunInput, RunOutput,
    SettingsChanged,
};
use serde_json::{Value, json};

use scanner::Scanner;
use settings::{SCANS, Scanning};
use store::{Asked, Named, Wanted};

pub const ID: &str = "insights";
/// Seeing security findings and dependencies with known advisories.
pub const SECURITY: &str = "security";

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
            PluginError::Forbidden(permission) => {
                Self::forbidden(format!("that needs {permission}:rw"))
            }
            PluginError::Message(detail) => Self::bad(detail),
            err => match err.problem() {
                Some((status, _)) if status < 500 => Self { status, detail: err.detail() },
                _ => Self::unavailable(err.to_string()),
            },
        }
    }
}

/// A parameter of a query string or a posted form.
pub fn parameter(encoded: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(encoded.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Who asked for something, as the repository's record says.
pub fn who(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "someone".to_string())
}

#[derive(Default)]
struct Insights {
    scanner: Scanner,
}

#[async_trait]
impl Plugin for Insights {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        tracing::info!(version = backend.version(), on = backend.feature(SCANS), "insights loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        self.scanner.stop();
        Ok(None)
    }

    /// The scanner, when the backend starts the long-running run.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        if input.task.is_none() && input.payload.is_null() {
            self.scanner.run(backend).await?;
            return Ok(RunOutput { payload: json!({ "stopped": true }) });
        }
        Err(PluginError::from("insights runs only its scanner"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        self.scanner.stop();
        Ok(())
    }

    /// Every scan reads the settings afresh, so a change needs no reload, which would stop a scan
    /// part-way through.
    async fn settings_changed(
        &self,
        _backend: &Backend,
        _changed: &SettingsChanged,
    ) -> Result<bool, PluginError> {
        self.scanner.wake();
        Ok(true)
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        match request.is_ui() {
            true => ui::handle(backend, &self.scanner, &request).await,
            false => api::handle(backend, &self.scanner, &request).await,
        }
    }

    /// A pull request was merged: its repository is scanned again if it went into the branch
    /// scanned, and the latest scan did not already read it.
    async fn on_event(&self, backend: &Backend, event: Event) -> Result<(), PluginError> {
        if !backend.feature(SCANS) {
            return Ok(());
        }
        // An administrator let this plugin have archive links: what failed for want of them goes again.
        if event.topic == format!("platform.plugin.{ID}.access.approved") {
            let Some(source) = event.payload["plugin"].as_str() else { return Ok(()) };
            let by = event.payload["decided_by"].as_str().unwrap_or("an administrator");
            let why = format!("{source}'s archive links were approved by {by}");
            let queued = store::requeue_failed(backend, source, &why).await?;
            tracing::info!(source, queued, "access was approved; failed scans are queued again");
            self.scanner.wake();
            let _ = backend.publish(&format!("plugin.{ID}.ui.repositories"), json!({})).await;
            return Ok(());
        }
        let Some(source) = event.topic.split('.').nth(1) else { return Ok(()) };
        let scanning = Scanning::read(&backend.settings());
        if !scanning.sources.iter().any(|wanted| wanted == source) {
            return Ok(());
        }
        let payload = &event.payload;
        let Some(repository) = payload["repository"].as_str() else { return Ok(()) };
        // A source that does not say which branch the pull request went into is taken at its word.
        if let Some(base) = payload["base"].as_str()
            && base != scanning.branch
        {
            return Ok(());
        }
        if !scanning.wants(repository) {
            return Ok(());
        }
        let Ok(named) = Named::new(source, repository) else { return Ok(()) };
        let merged_at: Option<DateTime<Utc>> = payload["merged_at"]
            .as_str()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| at.with_timezone(&Utc));
        let why = match payload["number"].as_u64() {
            Some(number) => format!("Pull request #{number} merged into {}", scanning.branch),
            None => format!("A pull request merged into {}", scanning.branch),
        };
        let trigger = json!({
            "number": payload["number"],
            "title": payload["title"],
            "url": payload["url"],
            "merged_at": payload["merged_at"],
            "merge_sha": payload["merge_sha"],
        });
        let wanted = Wanted { why, trigger: Some(trigger), at: merged_at };
        let asked = store::want(backend, &named, &scanning.branch, wanted).await?;
        if matches!(asked, Asked::Queued | Asked::Again) {
            self.scanner.wake();
            let topic = format!("plugin.{ID}.ui.repositories");
            let _ = backend.publish(&topic, json!({ "id": named.id() })).await;
        }
        Ok(())
    }
}

doc_plugin_sdk::main!(
    Insights,
    Manifest {
        id: ID.into(),
        classification: Classification::LongRunning,
        nav: vec![
            Nav::new("Repository insights", "/")
                .described(
                    "What ccc finds in each repository, scanned again after every merge into \
                     main: hotspots, services, lints, tests, security and what changed",
                )
                .grouped("Technology"),
        ],
        custom_permissions: vec![
            CustomPermission::user(SECURITY)
                .describes("Seeing security findings and dependencies with known advisories"),
            CustomPermission::service(SECURITY)
                .describes("Reading security findings and dependencies with known advisories"),
        ],
        resource_panels: vec![ResourcePanel::new("repository", "Insights", "/panel")],
        // One figure from the last scan, for the top of a repository's page.
        insights: ui::INSIGHTS
            .iter()
            .map(|(id, label, _)| {
                Insight::new("repository", id, label, &format!("/insight/{id}"))
                    .described("From the last scan of it, as the Insights panel shows")
            })
            .collect(),
        // Whichever source control plugin heard of a merge: `github`, `ghe`, or one to come.
        subscriptions: vec![
            "plugin.*.pull-request.merged".into(),
            // What administrators decide about this plugin's requests for access (DOC-SPEC §9.15).
            "platform.plugin.insights.access.approved".into(),
        ],
        settings: settings::declared(),
        features: settings::features(),
        data: store::declaration(),
        operations: vec![
            Operation::new("scan", "Scan a repository", "scans")
                .described(
                    "Queues a repository to be scanned with ccc at its main branch, unless it is \
                     already waiting.",
                )
                .param(OperationParam::required("repository", "Repository").hinted("owner/name."))
                .param(
                    OperationParam::optional("source", "Source")
                        .hinted("The plugin it is read from, such as github or ghe."),
                ),
        ],
        ..Manifest::default()
    }
);
