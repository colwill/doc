//! DORA metrics (ADR-0007 §4, FEAT-DORA): deployment frequency, change lead time, change fail rate
//! and failed deployment recovery time for every service, team and organisation in the Catalogue,
//! worked out from the delivery data a source control plugin exports — `github` and `ghe` today.

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
    Backend, Classification, Event, Insight, Manifest, Nav, Operation, OperationParam, Plugin,
    PluginError, Request, ResourcePanel, Response, RunInput, RunOutput, Schedule,
};
use serde_json::{Value, json};

use compute::{NIGHTLY_DAYS, Pending};
use settings::{Definitions, METRICS};

pub const ID: &str = "dora";
/// Where the definitions the stored records were worked out under are kept.
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

/// Who is counting something, as the counter records them.
pub fn who(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "someone".to_string())
}

fn since_of(value: &Value, days: i64) -> DateTime<Utc> {
    value
        .as_str()
        .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
        .map(|at| at.with_timezone(&Utc))
        .unwrap_or_else(|| Utc::now() - Duration::days(days))
}

#[derive(Default)]
struct Dora;

#[async_trait]
impl Plugin for Dora {
    /// With the metrics on, records worked out under other definitions — or none at all, the first
    /// time — are worked out again over everything kept, in the background.
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), on = backend.feature(METRICS), "dora loaded");
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
        tracing::info!(%task, "the definitions changed, so everything kept is worked out again");
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
                payload: json!({ "recomputed": false, "why": "DORA metrics are off" }),
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
            return Err(PluginError::from("dora runs only its own recomputes"));
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

    /// A source has read a repository again: that repository is worked out again, from the
    /// earliest thing that changed, in the background.
    async fn on_event(&self, backend: &Backend, event: Event) -> Result<(), PluginError> {
        if !backend.feature(METRICS) {
            return Ok(());
        }
        let Some(source) = event.topic.split('.').nth(1) else { return Ok(()) };
        let definitions = Definitions::read(&backend.settings());
        // A rollout source's repository is worked out against the first delivery source.
        let source = match definitions.rollouts.iter().any(|rolls| rolls == source) {
            true => definitions.sources.first().map_or(source, String::as_str),
            false if definitions.sources.iter().any(|wanted| wanted == source) => source,
            false => return Ok(()),
        };
        let Some(repository) = event.payload["repository"].as_str() else { return Ok(()) };
        let refresh =
            json!({ "source": source, "repository": repository, "since": event.payload["since"] });
        backend.task(json!({ "refresh": refresh })).await?;
        Ok(())
    }
}

doc_plugin_sdk::main!(
    Dora,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("DORA metrics", "/")
                .described(
                    "How often each service deploys, how fast a change gets to production, how \
                     often a deployment fails and how quickly it recovers",
                )
                .grouped("Platform"),
        ],
        resource_panels: ["service", "team", "organisation"]
            .into_iter()
            .map(|kind| ResourcePanel::new(kind, "Delivery", "/panel"))
            .collect(),
        // Each of the four on its own, for whoever wants one at the top of a page rather than
        // the whole panel.
        insights: ui::INSIGHTS
            .iter()
            .flat_map(|(id, label)| {
                ["service", "team", "organisation"].into_iter().map(move |kind| {
                    Insight::new(kind, id, label, &format!("/insight/{id}"))
                        .described("Over the last 30 days, as the Delivery panel measures it")
                })
            })
            .collect(),
        // Whichever source control plugin read a repository again: `github`, `ghe`, or one to come.
        subscriptions: vec!["plugin.*.delivery.synced".into()],
        schedules: vec![
            Schedule::new(
                "recompute",
                settings::RECOMPUTE,
                "Works the last 30 days out again so late data counts, and forgets what is older than the settings keep",
            )
            .of_feature(METRICS)
            .from_setting(settings::SCHEDULE),
        ],
        settings: settings::declared(),
        features: settings::features(),
        data: store::declaration(),
        // An agent asks over MCP with a POST that only reads.
        read_routes: vec!["mcp".into()],
        operations: vec![
            Operation::new("increment", "Count one", "counters/{counter}/increment")
                .described(
                    "Counts a hotfix, an incident or anything else against a service or a \
                     repository. A counter the settings call a failure marks the deployment \
                     before it as having caused one.",
                )
                .param(
                    OperationParam::required("counter", "Counter")
                        .hinted("Such as hotfixes or incidents."),
                )
                .param(OperationParam::optional("service", "Service").hinted("Its name in the Catalogue."))
                .param(
                    OperationParam::optional("repository", "Repository")
                        .hinted("owner/name, where the event names one rather than a service."),
                )
                .param(OperationParam::optional("by", "By how much").hinted("1 unless it says."))
                .param(OperationParam::optional("note", "Note")),
        ],
        ..Manifest::default()
    }
);
