//! Process: recurring processes on any resource, such as a weekly on-call handover. Each occurrence
//! is planned ahead and put on the resource's calendar, its checklist is ticked off one item at a
//! time, and reminders and missed occurrences are published for automations to deliver.

mod api;
mod away;
mod cadence;
mod store;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, DashboardItem, Manifest, Nav, Plugin, PluginError, Request,
    ResourcePanel, Response, RunInput, RunOutput, Schedule,
};
use serde_json::Value;

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
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        tracing::warn!(%err, "a call to the backend failed");
        Self::unavailable(format!("the processes' storage failed: {err}"))
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

#[derive(Default)]
struct ProcessPlugin;

#[async_trait]
impl Plugin for ProcessPlugin {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), "process loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    /// The `occurrences` schedule, each minute.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        match input.payload["schedule"].as_str() {
            Some("occurrences") => Ok(RunOutput { payload: api::sweep(backend).await? }),
            _ => Err(PluginError::from("process runs only its occurrences schedule")),
        }
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        api::handle(backend, request).await
    }
}

doc_plugin_sdk::main!(
    ProcessPlugin,
    Manifest {
        id: "process".into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("Processes", "/")
                .described(
                    "Recurring processes such as on-call handovers, with checklists and reminders"
                )
                .grouped("Workspace")
        ],
        dashboard: vec![DashboardItem::new("due", "Processes due", "/dashboard").described(
            "What is due on the processes you own or are assigned to: the next two weeks, and \
                 anything overdue or missed.",
        ),],
        resource_panels: [
            "organisation",
            "service",
            "repository",
            "team",
            "role",
            "user",
            "service-account",
            "documentation",
            "cloud-resource",
            "attribute",
            "permission"
        ]
        .into_iter()
        .map(|kind| ResourcePanel::new(kind, "Processes", "/panel"))
        .collect(),
        schedules: vec![Schedule::new(
            "occurrences",
            "* * * * *",
            "Plans each process's occurrences and publishes their reminders and missed notices"
        )],
        data: store::declaration(),
        ..Manifest::default()
    }
);
