//! Agent Smith: an agent that uses DOC (FEAT-AGENT). Your own agent connects to one MCP server
//! carrying DOC's tools, every plugin's and the playbooks; jobs DOC runs itself drive Claude with
//! the same, each tool call made as the job's owner.

mod claude;
mod jobs;
mod mcp;
mod playbooks;
mod runbooks;
mod settings;
mod store;
mod tools;
mod ui;

use std::sync::Arc;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Capability, Classification, CustomPermission, DashboardItem, Event, Manifest, Nav,
    Operation, OperationParam, OwnAccount, Plugin, PluginError, Request, Response, RunInput,
    RunOutput, Schedule,
};
use serde_json::{Value, json};

pub const ID: &str = "agent";
/// The service account Agent Smith acts as when it runs a runbook by itself.
pub const ACCOUNT: &str = "agent-smith";

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
        Self::unavailable(format!("Agent Smith's storage failed: {}", err.detail()))
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

/// One query parameter, decoded.
pub fn parameter(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Default)]
struct Agent {
    worker: Arc<claude::Worker>,
}

#[async_trait]
impl Plugin for Agent {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        tracing::info!(version = backend.version(), "agent loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        self.worker.stop();
        Ok(None)
    }

    /// The long-running run drives Claude through the jobs; a queued run is one tool call made as
    /// a job's owner, or the minute's schedule.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        if input.task.is_none() {
            self.worker.run(backend).await?;
            return Ok(RunOutput { payload: json!({ "stopped": true }) });
        }
        let payload = match input.payload["schedule"].as_str() {
            Some(_) => jobs::tick(backend).await?,
            None => self.worker.answer(backend, &input.payload).await?,
        };
        Ok(RunOutput { payload })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        self.worker.stop();
        Ok(())
    }

    async fn on_event(&self, backend: &Backend, event: Event) -> Result<(), PluginError> {
        jobs::heard(backend, &event).await?;
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let path = request.path.trim_end_matches('/').to_string();
        let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        match segments.as_slice() {
            ["ui", route @ ..] => ui::handle(backend, &request, route).await,
            ["api", "mcp"] => mcp::handle(backend, &request).await,
            ["api", "runbooks", route @ ..] => {
                match Box::pin(runbooks::api(backend, &request, route)).await {
                    Ok(answer) if request.method == "POST" => {
                        let mut response = Response::json(&answer);
                        response.status = 202;
                        response
                    }
                    Ok(answer) => Response::json(&answer),
                    Err(refusal) => refusal.response(),
                }
            }
            _ => Refusal::missing("no such route").response(),
        }
    }
}

doc_plugin_sdk::main!(
    Agent,
    Manifest {
        id: ID.into(),
        classification: Classification::LongRunning,
        capabilities: vec![Capability::TeamWriter, Capability::ServiceAccount],
        service_account: Some(OwnAccount::new(
            ACCOUNT,
            "Agent Smith running runbooks by itself, for schedules, events and automations. It \
             holds only what administrators grant it.",
        )),
        custom_permissions: vec![CustomPermission::user(runbooks::PERMISSION).describes(
            "Approve runbooks for Agent Smith to run by itself, with its own access, and set \
             schedules, events and automations to run them",
        )],
        operations: vec![
            Operation::new("run-runbook", "Run a runbook with Agent Smith", "runbooks/run")
                .described(
                    "Runs a runbook from the Knowledge Base with Agent Smith's own access, at the \
                     version approved and against the environment approved for it. The person \
                     the automation runs as must hold Agent Smith's runbooks permission.",
                )
                .param(OperationParam::required("space", "Space").hinted("The space's key."))
                .param(
                    OperationParam::required("path", "Page")
                        .hinted("The runbook's path in the space, such as docs/runbook.md."),
                ),
        ],
        nav: vec![
            Nav::new("Agent Smith", "/")
                .described(
                    "An agent that uses DOC: connect your own over MCP, or set it jobs to run \
                     by itself",
                )
                .grouped("Workspace"),
        ],
        read_routes: vec!["mcp".into()],
        subscriptions: jobs::EVENTS.iter().map(|(topic, _)| topic.to_string()).collect(),
        schedules: vec![Schedule::new(
            "tick",
            "* * * * *",
            "Delivers reminders that are due, and starts jobs whose schedule has come round",
        )],
        settings: settings::declared(),
        named_secrets: Some(settings::named()),
        dashboard: vec![DashboardItem::new("runs", "Agent Smith", "/dashboard").described(
            "The runbooks and jobs you asked Agent Smith to run lately, how each went, and \
                 your reminders still to come.",
        ),],
        data: store::declaration(),
        ..Manifest::default()
    }
);
