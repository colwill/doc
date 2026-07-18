//! Tech Radar: the technologies and techniques the organisation uses, trials, assesses or holds,
//! in four quadrants, as Backstage's tech radar draws them. Readers see the radar, each entry's
//! reasoning and every ring it has been in; writers add entries, move them between rings and
//! import a radar in Backstage's JSON.
//!
//! Changes are published as `plugin.radar.entry.added`, `.moved`, `.changed` and `.removed`, and
//! every open radar redraws itself on `plugin.radar.ui.radar`.

mod api;
mod backstage;
mod draw;
mod markdown;
mod model;
mod store;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, Manifest, Nav, Plugin, PluginError, Request, Response, RunInput,
    RunOutput,
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

#[derive(Default)]
struct TechRadar;

#[async_trait]
impl Plugin for TechRadar {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), "radar loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, _backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        Err(PluginError::from("radar has nothing to run: POST api/import to import a radar"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let path = request.path.trim_end_matches('/').to_string();
        let segments: Vec<&str> = path.split('/').collect();
        match segments.as_slice() {
            ["ui", route @ ..] => ui::handle(backend, &request, route).await,
            ["api", route @ ..] => api::handle(backend, &request, route).await,
            _ => Refusal::missing("no such route").response(),
        }
    }
}

doc_plugin_sdk::main!(
    TechRadar,
    Manifest {
        id: "radar".into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("Radar", "/")
                .described("What we adopt, trial, assess and hold, and why")
                .grouped("Technology")
        ],
        data: store::declaration(),
        ..Manifest::default()
    }
);
