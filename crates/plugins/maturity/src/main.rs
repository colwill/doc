//! Maturity Model: what an organisation and its teams expect of the things they run, and how far
//! each of them meets it.
//!
//! A **model** is a set of **criteria**, kept by the organisation or by one team. Each criterion is
//! one thing that is either met or not, worth a weight beside the others, and decided from what
//! the platform already knows — what the Catalogue says a thing is connected to, who owns it, what
//! its metadata says, and what the plugins that measure a service say about it. What no plugin can
//! see is attested by a person instead.
//!
//! Each component a model grades gets a **scorecard**: every criterion with whether it was met and
//! why, and a **grade from 1 to 10** — one for meeting nothing, ten for meeting everything, and
//! the scale in between coloured red through amber to green. Scoring is the automation: it runs on
//! a schedule and whenever a model changes, and publishes what moved so an Automation can tell
//! whoever keeps a component that it slipped.

mod api;
mod checks;
mod leave;
mod model;
mod score;
mod settings;
mod store;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, Event, Insight, Manifest, Nav, Plugin, PluginError, Request,
    ResourcePanel, Response, RunInput, RunOutput, Schedule,
};
use serde_json::{Value, json};

pub const ID: &str = "maturity";

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
            409 => "conflict",
            _ => "unavailable",
        };
        Response::problem(self.status, kind, &self.detail)
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        match err {
            PluginError::Forbidden(permission) => {
                Self::forbidden(format!("that needs {permission}"))
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

#[derive(Default)]
struct Maturity {
    /// Whether a scoring run is already waiting on the Catalogue to stop changing.
    settling: score::Settling,
}

#[async_trait]
impl Plugin for Maturity {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), "maturity loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        // The schedule runs as the plugin, and the Catalogue answers the plugin nothing, so the
        // work goes round again as whoever last gave leave. Everything else was asked for by
        // somebody and already runs as them.
        if input.payload["schedule"].is_string() {
            let payload = leave::score_as_whoever_gave_leave(backend, json!({})).await?;
            return Ok(RunOutput { payload });
        }
        // One model when a change asked for it, everything otherwise.
        let only = input.payload["model"].as_str().and_then(|id| id.parse().ok());
        let done = score::run(backend, only).await?;
        Ok(RunOutput { payload: done.json() })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    /// The Catalogue changed. Nearly every criterion is decided from what it holds, so a grade
    /// that only followed the schedule would be a day behind the moment somebody connected a
    /// repository to a service; this scores everything again once the changes have settled.
    async fn on_event(&self, backend: &Backend, event: Event) -> Result<(), PluginError> {
        if event.topic == score::CATALOGUE_CHANGED {
            self.settling.after_the_change(backend);
        }
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        match request.is_ui() {
            true => ui::handle(backend, &request).await,
            false => api::handle(backend, &request).await,
        }
    }
}

doc_plugin_sdk::main!(
    Maturity,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("Maturity", "/")
                .described(
                    "What an organisation and its teams expect of what they run, and how far \
                     each service, repository, page and cloud resource meets it",
                )
                .grouped("Platform"),
        ],
        // On whatever a model can grade, so a grade is where the thing is rather than only here.
        resource_panels: ["service", "repository", "documentation", "cloud-resource"]
            .into_iter()
            .map(|kind| ResourcePanel::new(kind, "Maturity", "/panel"))
            .collect(),
        // One number from that panel, for whoever wants it at the top of the page instead.
        insights: ["service", "repository", "documentation", "cloud-resource"]
            .into_iter()
            .map(|kind| {
                Insight::new(kind, "level", "Maturity level", "/insight")
                    .described("Its grade from 1 to 10, against the models that grade it")
            })
            .collect(),
        schedules: vec![
            Schedule::new(
                "score",
                settings::SCORE,
                "Scores every component each model grades, and says what moved",
            )
            .from_setting(settings::SCORE_SCHEDULE),
        ],
        settings: settings::declared(),
        subscriptions: vec![score::CATALOGUE_CHANGED.into()],
        data: store::declaration(),
        ..Manifest::default()
    }
);

/// What a run says when nothing asked for one.
pub fn nothing() -> Value {
    json!({ "models": 0, "components": 0 })
}
