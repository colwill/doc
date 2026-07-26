//! Notifications: a small inbox per user. Any plugin tells someone something with
//! `Backend::notify` (or an Automation "Notify someone in DOC" action, built on the same call);
//! the bell in the header and `/p/notifications/` are how they see it.

mod api;
mod store;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, DashboardItem, Manifest, Nav, Plugin, PluginError, Request, Response,
    RunInput, RunOutput,
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

#[derive(Default)]
struct Notifications;

#[async_trait]
impl Plugin for Notifications {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), "notifications loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, _backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        Err(PluginError::from("notifications has nothing to run"))
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
            ["discovery", "notify"] if request.method == "POST" => {
                api::notify(backend, &request).await
            }
            _ => Refusal::missing("no such route").response(),
        }
    }
}

doc_plugin_sdk::main!(
    Notifications,
    Manifest {
        id: "notifications".into(),
        classification: Classification::Synchronous,
        data: store::declaration(),
        // Hidden: the header bell is the only way in, not a navbar link. Registered anyway so
        // core's breadcrumbs have a section to anchor "Inbox" to on /p/notifications/*.
        nav: vec![Nav::new("Inbox", "/").hidden()],
        dashboard: vec![
            DashboardItem::new("inbox", "Your inbox", "/dashboard/inbox")
                .described("What is waiting for you: the newest of your unread notifications.")
        ],
        ..Manifest::default()
    }
);
