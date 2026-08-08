//! Reliability: each service's availability against its objective, its mean time to recover, and
//! its recovery time and recovery point objectives — from health checks, and from the outages,
//! backups and restores automations report — for every service, team and organisation in the
//! Catalogue; and the same for DOC itself, from its own status history, so the teams that use DOC
//! can see what it promises them and whether it keeps it.

mod api;
mod faux;
mod mcp;
mod metrics;
mod platform;
mod probe;
mod record;
mod scope;
mod settings;
mod sourced;
mod store;
mod ui;
mod view;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use doc_plugin_sdk::{
    Backend, Classification, DashboardItem, Insight, Manifest, Nav, Operation, OperationParam,
    Plugin, PluginError, Query, Request, ResourcePanel, Response, RunInput, RunOutput, Schedule,
};
use serde_json::{Value, json};

use settings::{Definitions, PLATFORM, SERVICES};

pub const ID: &str = "reliability";
const FORGET: &str = "20 3 * * *";

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

/// A parameter of a query string.
pub fn parameter(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Who is changing or reporting something, as it is recorded.
pub fn who(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "someone".to_string())
}

/// What is older than the settings keep.
async fn forget(backend: &Backend, definitions: &Definitions) -> Result<usize, PluginError> {
    let cutoff = Utc::now() - Duration::days(definitions.keep_days);
    let mut forgotten = 0;
    for (collection, field) in [
        ("outages", "started_at"),
        ("backups", "at"),
        ("restores", "started_at"),
        ("coverage", "day"),
    ] {
        let filter = json!({ field: { "lt": cutoff } });
        forgotten += backend.delete_where(collection, "id", filter).await?;
    }
    // Telemetry goes sooner, and how much sooner is not entirely the settings' to say.
    let days = definitions.telemetry_days(exported(backend).await);
    let filter = json!({ "at": { "lt": Utc::now() - Duration::days(days) } });
    forgotten += backend.delete_where("samples", "id", filter).await?;
    Ok(forgotten)
}

/// Whether a plugin is taking DOC's telemetry somewhere that keeps it, which is what lets the
/// week be raised. Asked each time it is needed, so onboarding one takes effect at the next
/// tidy-up rather than at the next restart.
async fn exported(backend: &Backend) -> bool {
    let asked = Query::new("core.plugins")
        .filter(json!({ "telemetry": true, "registered_at": { "is_null": false } }))
        .fields(&["id"])
        .limit(1);
    match backend.query::<Value>(asked).await {
        Ok(found) => !found.records.is_empty(),
        Err(err) => {
            tracing::warn!(%err, "could not tell whether telemetry is exported; keeping a week");
            false
        }
    }
}

#[derive(Default)]
struct Reliability {
    http: Option<reqwest::Client>,
}

#[async_trait]
impl Plugin for Reliability {
    /// With DOC's reliability on and its status history never read, the first read is queued
    /// now rather than at the schedule, since turning a feature on reloads the plugin.
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        self.http = Some(probe::client()?);
        tracing::info!(
            version = backend.version(),
            services = backend.feature(SERVICES),
            platform = backend.feature(PLATFORM),
            "reliability loaded"
        );
        if backend.feature(PLATFORM) && backend.state_get("platform/read").await?.is_none() {
            match backend.task(json!({ "platform": true })).await {
                Ok(task) => {
                    tracing::info!(%task, "the first read of DOC's status history is queued")
                }
                Err(err) => {
                    tracing::warn!(%err, "the first read of DOC's status history could not be queued")
                }
            }
        }
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let definitions = Definitions::read(&backend.settings());
        let payload = &input.payload;
        let done = match (payload["schedule"].as_str(), payload.get("platform")) {
            (Some("platform"), _) | (None, Some(_)) => match backend.feature(PLATFORM) {
                true => platform::read(backend, &definitions).await?,
                false => json!({ "read": false, "why": "DOC's reliability is off" }),
            },
            (Some("check"), _) => match (backend.feature(SERVICES), &self.http) {
                (true, Some(http)) => {
                    let mut done = probe::run(backend, http, &definitions).await?;
                    done["sourced"] = sourced::read(backend, &definitions).await?;
                    done
                }
                (false, _) => json!({ "checked": false, "why": "Service reliability is off" }),
                (true, None) => return Err(PluginError::from("reliability is not loaded")),
            },
            (Some("forget"), _) => json!({ "forgotten": forget(backend, &definitions).await? }),
            _ => return Err(PluginError::from("reliability runs only its own schedules")),
        };
        Ok(RunOutput { payload: done })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        faux::check(backend).await;
        let response = match request.is_ui() {
            true => ui::handle(backend, &request).await,
            false => api::handle(backend, &request).await,
        };
        faux::marked(response)
    }
}

doc_plugin_sdk::main!(
    Reliability,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("Reliability", "/")
                .described(
                    "Each service's availability, time to recover, recovery time and recovery \
                     point against its objectives, and DOC's own",
                )
                .grouped("Platform"),
        ],
        dashboard: vec![
            DashboardItem::new("down", "Down now", "/dashboard")
                .described("Outages still going on, on the services you can see."),
        ],
        resource_panels: ["service", "team", "organisation"]
            .into_iter()
            .map(|kind| ResourcePanel::new(kind, "Reliability", "/panel"))
            .collect(),
        // Each objective on its own, for the top of a service's page.
        insights: ui::INSIGHTS
            .iter()
            .map(|(id, label)| {
                Insight::new("service", id, label, &format!("/insight/{id}"))
                    .described("Against its objective, as the Reliability panel measures it")
            })
            .collect(),
        schedules: vec![
            Schedule::new("check", settings::PROBE, "Checks each service's health URL")
                .of_feature(SERVICES)
                .from_setting(settings::PROBE_SCHEDULE),
            Schedule::new(
                "platform",
                settings::READ,
                "Reads DOC's status history on from where the last read stopped",
            )
            .of_feature(PLATFORM)
            .from_setting(settings::PLATFORM_SCHEDULE),
            Schedule::new("forget", FORGET, "Forgets what is older than the settings keep"),
        ],
        settings: settings::declared(),
        features: settings::features(),
        data: store::declaration(),
        // An agent asks over MCP with a POST that only reads.
        read_routes: vec!["mcp".into()],
        operations: vec![
            Operation::new("down", "Mark a service down", "services/{service}/down")
                .described(
                    "Opens an outage of a service from now, or from `at`, unless one is open \
                     already: for an alert from monitoring DOC does not do itself.",
                )
                .param(
                    OperationParam::required("service", "Service")
                        .hinted("Its name in the Catalogue.")
                )
                .param(OperationParam::optional("note", "What is wrong"))
                .param(
                    OperationParam::optional("at", "Since")
                        .hinted("An RFC 3339 time; now unless it says.")
                ),
            Operation::new("up", "Mark a service up", "services/{service}/up")
                .described("Ends a service's open outage now, or at `at`.")
                .param(
                    OperationParam::required("service", "Service")
                        .hinted("Its name in the Catalogue.")
                )
                .param(
                    OperationParam::optional("at", "Since")
                        .hinted("An RFC 3339 time; now unless it says.")
                ),
            Operation::new("backup", "Record a backup", "backups")
                .described(
                    "Records a backup a service can be restored from, so its recovery point is \
                     measured: from the job that takes it, once it has.",
                )
                .param(
                    OperationParam::optional("service", "Service")
                        .hinted("Its name in the Catalogue.")
                )
                .param(
                    OperationParam::optional("component", "Part of DOC")
                        .hinted("postgres, for DOC's own database, instead of a service."),
                )
                .param(
                    OperationParam::optional("at", "Restores to")
                        .hinted("An RFC 3339 time; now unless it says.")
                )
                .param(
                    OperationParam::optional("kind", "Kind")
                        .hinted("Such as full, incremental or snapshot.")
                )
                .param(OperationParam::optional("note", "Note")),
            Operation::new("restore", "Record a restore", "restores")
                .described(
                    "Records a restore from backup — a drill or the real thing — and how long it \
                     took, which the recovery time objective is judged by.",
                )
                .param(
                    OperationParam::optional("service", "Service")
                        .hinted("Its name in the Catalogue.")
                )
                .param(
                    OperationParam::optional("component", "Part of DOC")
                        .hinted("postgres, instead of a service.")
                )
                .param(
                    OperationParam::required("started_at", "Started").hinted("An RFC 3339 time.")
                )
                .param(
                    OperationParam::optional("finished_at", "Finished")
                        .hinted("An RFC 3339 time; now unless it says.")
                )
                .param(
                    OperationParam::optional("succeeded", "Succeeded")
                        .hinted("true unless it says false.")
                )
                .param(OperationParam::optional("note", "Note")),
        ],
        ..Manifest::default()
    }
);
