//! Calendar events: events on the Calendar plugin's calendars, with wall-clock times in their own zone,
//! recurrence rules with exceptions, attendees and RSVPs. Other plugins keep their events here too,
//! and a schedule publishes each reminder as it comes due.

mod api;
mod recur;
mod store;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, Manifest, Plugin, PluginError, Request, Response, RunInput, RunOutput,
    Schedule,
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
        Self::unavailable(format!("the events' storage failed: {err}"))
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

#[derive(Default)]
struct CalendarEvents;

#[async_trait]
impl Plugin for CalendarEvents {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), "calendar-events loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    /// The `reminders` schedule, each minute.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        match input.payload["schedule"].as_str() {
            Some("reminders") => Ok(RunOutput { payload: api::remind(backend).await? }),
            _ => Err(PluginError::from("calendar-events runs only its reminders schedule")),
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
    CalendarEvents,
    Manifest {
        id: "calendar-events".into(),
        classification: Classification::Synchronous,
        read_routes: vec!["rsvps".into()],
        schedules: vec![Schedule::new(
            "reminders",
            "* * * * *",
            "Publishes plugin.calendar-events.reminder.due as each event's reminder comes due",
        )],
        data: store::declaration(),
        ..Manifest::default()
    }
);
