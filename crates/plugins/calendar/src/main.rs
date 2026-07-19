//! Calendar: a calendar for each team, service, organisation or person, subscriptions to the ones people
//! can see, ICS feeds at secret URLs that can be rotated, and agendas of the events in
//! `calendar-events`.

mod api;
mod ics;
mod store;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Capability, Classification, DashboardItem, Manifest, Nav, Plugin, PluginError,
    Request, ResourcePanel, Response, RunInput, RunOutput, Setting, SettingKind,
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

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        tracing::warn!(%err, "a call to the backend failed");
        Self::unavailable(format!("the calendar's storage failed: {err}"))
    }
}

#[derive(Default)]
struct CalendarPlugin;

#[async_trait]
impl Plugin for CalendarPlugin {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), "calendar loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, _backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        Err(PluginError::from("calendar has no runs; its work is its routes"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        api::handle(backend, request).await
    }
}

doc_plugin_sdk::main!(
    CalendarPlugin,
    Manifest {
        id: "calendar".into(),
        classification: Classification::Synchronous,
        capabilities: vec![Capability::PublicRoutes],
        public_routes: vec!["feeds/*".into()],
        read_routes: [
            "subscriptions",
            "feeds",
            "feeds/*",
            "ui/subscribe",
            "ui/feeds",
            "ui/feeds/*",
            "ui/rsvp"
        ]
        .into_iter()
        .map(String::from)
        .collect(),
        nav: vec![
            Nav::new("Calendar", "/")
                .described(
                    "Calendars for teams, services and people, with feeds you can subscribe to"
                )
                .grouped("Workspace")
        ],
        resource_panels: ["organisation", "service", "team"]
            .into_iter()
            .map(|kind| ResourcePanel::new(kind, "Calendar", "/panel"))
            .collect(),
        dashboard: vec![
            DashboardItem::new("upcoming", "Your week", "/dashboard/upcoming").described(
                "The next seven days on your own calendar, your teams' and the ones you subscribe \
                 to.",
            ),
        ],
        settings: vec![
            Setting::new(
                api::PERSONAL_FROM,
                "Plugins that put events on people's own calendars",
                SettingKind::List
            )
            .hinted(
                "Such as rota, for the shifts someone is on. Any other plugin keeps to the \
                 calendars of teams, services and organisations."
            )
            .defaulting(serde_json::json!(["rota"])),
        ],
        data: store::declaration(),
        ..Manifest::default()
    }
);
