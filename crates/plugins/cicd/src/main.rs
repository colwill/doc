//! CI/CD/CT metrics: how often each service's GitHub Actions pipelines succeed, how long they
//! take, how quickly a broken default branch is fixed and how often a run passes only when re-run,
//! for continuous integration, delivery and testing, for every service, team and organisation in
//! the Catalogue, from the workflow runs a source control plugin exports — `github` and `ghe`.

mod api;
mod compute;
mod faux;
mod mcp;
mod metrics;
mod scope;
mod settings;
mod store;
mod ui;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{
    Backend, Classification, Event, Insight, Manifest, Nav, Plugin, PluginError, Request,
    ResourcePanel, Response, RunInput, RunOutput, Schedule,
};
use serde_json::{Value, json};

use compute::{NIGHTLY_DAYS, Pending};
use settings::{Definitions, METRICS};

pub const ID: &str = "cicd";
/// Where the definitions the stored figures were worked out under are kept.
const DEFINED: &str = "definitions";

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

fn since_of(value: &Value, days: i64) -> DateTime<Utc> {
    value
        .as_str()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .unwrap_or_else(|| Utc::now() - Duration::days(days))
}

#[derive(Default)]
struct Cicd;

#[async_trait]
impl Plugin for Cicd {
    /// With the metrics on, figures worked out under other stages — or none at all, the first
    /// time — are worked out again over everything kept, in the background.
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), on = backend.feature(METRICS), "cicd loaded");
        if !backend.feature(METRICS) {
            return Ok(());
        }
        let definitions = Definitions::read(&backend.settings());
        let fingerprint = definitions.fingerprint();
        let held = backend.state_get(DEFINED).await?;
        if held.as_ref().and_then(Value::as_str) == Some(fingerprint.as_str()) {
            return Ok(());
        }
        let task = backend.task(json!({ "recompute": { "days": definitions.keep_days } })).await?;
        backend.state_set(DEFINED, json!(fingerprint)).await?;
        tracing::info!(%task, "the stages changed, so everything kept is worked out again");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let definitions = Definitions::read(&backend.settings());
        let payload = &input.payload;
        if !backend.feature(METRICS) {
            return Ok(RunOutput {
                payload: json!({ "recomputed": false, "why": "CI/CD/CT metrics are off" }),
            });
        }
        let done = if payload["schedule"] == "recompute" {
            let forgotten = compute::forget(backend, &definitions).await?;
            let pending = compute::everything(backend, &definitions, NIGHTLY_DAYS).await?;
            let since = Utc::now() - Duration::days(NIGHTLY_DAYS);
            let mut done = compute::work(backend, &definitions, pending, since).await?;
            done["forgotten"] = json!(forgotten);
            done
        } else if let Some(asked) = payload.get("recompute") {
            let days = asked["days"].as_i64().unwrap_or(definitions.keep_days).clamp(1, 800);
            let since = since_of(&asked["since"], days);
            let pending: Vec<Pending> = match asked.get("pending") {
                Some(pending) => serde_json::from_value(pending.clone())
                    .map_err(|err| PluginError::Message(format!("an unreadable queue: {err}")))?,
                None => compute::everything(backend, &definitions, days).await?,
            };
            compute::work(backend, &definitions, pending, since).await?
        } else if let Some(asked) = payload.get("refresh") {
            let pending: Pending = serde_json::from_value(asked.clone())
                .map_err(|err| PluginError::Message(format!("an unreadable refresh: {err}")))?;
            let since = since_of(&asked["since"], NIGHTLY_DAYS);
            compute::work(backend, &definitions, vec![pending], since).await?
        } else {
            return Err(PluginError::from("cicd runs only its own recomputes"));
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

    /// A source has read a repository's runs again: that repository is worked out again, from the
    /// earliest run that changed, in the background.
    async fn on_event(&self, backend: &Backend, event: Event) -> Result<(), PluginError> {
        if !backend.feature(METRICS) {
            return Ok(());
        }
        let Some(source) = event.topic.split('.').nth(1) else { return Ok(()) };
        let definitions = Definitions::read(&backend.settings());
        if !definitions.sources.iter().any(|wanted| wanted == source) {
            return Ok(());
        }
        let Some(repository) = event.payload["repository"].as_str() else { return Ok(()) };
        let refresh =
            json!({ "source": source, "repository": repository, "since": event.payload["since"] });
        backend.task(json!({ "refresh": refresh })).await?;
        Ok(())
    }
}

doc_plugin_sdk::main!(
    Cicd,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("CI/CD/CT metrics", "/")
                .described(
                    "How often each service's pipelines pass, how long they take, how quickly a \
                     broken default branch is fixed and how often a run only passes when re-run",
                )
                .grouped("Platform"),
        ],
        resource_panels: ["service", "team", "organisation"]
            .into_iter()
            .map(|kind| ResourcePanel::new(kind, "Pipelines", "/panel"))
            .collect(),
        // Each figure on its own, for whoever wants one at the top of a page rather than the
        // whole panel.
        insights: ui::INSIGHTS
            .iter()
            .flat_map(|(id, label)| {
                ["service", "repository", "team", "organisation"].into_iter().map(move |kind| {
                    Insight::new(kind, id, label, &format!("/insight/{id}"))
                        .described("Over the last 30 days, as the Pipelines panel measures it")
                })
            })
            .collect(),
        // Whichever source control plugin read a repository's runs again: `github`, `ghe`, or one
        // to come.
        subscriptions: vec!["plugin.*.pipelines.synced".into()],
        schedules: vec![
            Schedule::new(
                "recompute",
                settings::RECOMPUTE,
                "Works the last 30 days out again so late runs count, and forgets what is older than the settings keep",
            )
            .of_feature(METRICS)
            .from_setting(settings::SCHEDULE),
        ],
        settings: settings::declared(),
        features: settings::features(),
        data: store::declaration(),
        // An agent asks over MCP with a POST that only reads.
        read_routes: vec!["mcp".into()],
        ..Manifest::default()
    }
);
