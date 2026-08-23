//! Data Vacuum: takes documentation and catalogue data from Confluence, Backstage, Jira, Markdown
//! and MkDocs into DOC, with an LLM doing the reading and reshaping (FEAT-VACUUM). Either DOC
//! drives Claude itself, with the API key in the settings, or it gives an administrator's own agent
//! instructions and a scoped token that lasts minutes. Whatever the LLM hands in is staged, and
//! only what an administrator approves is written to the Knowledge Base, Watercooler and the
//! Catalogue, as them.

mod api;
mod apply;
mod claude;
mod instructions;
mod settings;
mod stage;
mod store;
mod ui;

use std::sync::Arc;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Capability, Classification, Manifest, Nav, Plugin, PluginError, Request, Response,
    RunInput, RunOutput,
};
use serde_json::{Value, json};

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
        Self::unavailable(format!("the Data Vacuum's storage failed: {err}"))
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

#[derive(Default)]
struct Vacuum {
    worker: Arc<claude::Worker>,
}

#[async_trait]
impl Plugin for Vacuum {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        tracing::info!(version = backend.version(), "vacuum loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        self.worker.stop();
        Ok(None)
    }

    /// The long-running run: Claude working through the runs DOC drives, one at a time.
    async fn run(&self, backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        self.worker.run(backend).await?;
        Ok(RunOutput { payload: json!({ "stopped": true }) })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        self.worker.stop();
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let path = request.path.trim_end_matches('/').to_string();
        let segments: Vec<&str> = path.split('/').collect();
        match segments.as_slice() {
            ["ui", route @ ..] => ui::handle(backend, &self.worker, &request, route).await,
            ["api", route @ ..] => api::handle(backend, &request, route).await,
            _ => Refusal::missing("no such route").response(),
        }
    }
}

doc_plugin_sdk::main!(
    Vacuum,
    Manifest {
        id: "vacuum".into(),
        classification: Classification::LongRunning,
        capabilities: vec![Capability::TokenIssuer],
        nav: vec![
            Nav::new("Data Vacuum", "/")
                .described(
                    "Takes Confluence, Backstage, Jira, Markdown and MkDocs into DOC with an LLM, \
                     for you to approve"
                )
                .grouped("Admin")
        ],
        settings: settings::declared(),
        data: store::declaration(),
        ..Manifest::default()
    }
);
