//! Automation: on any resource, a trigger (a schedule, an event, a queue or a webhook), conditions
//! on what it brought, and actions that run as the automation's owner. The long-running `run` is
//! the engine: it fires schedules and turns waiting triggers into runs, each a task with retries.

mod actions;
mod api;
mod conditions;
mod engine;
mod graph;
mod model;
mod operations;
mod store;
mod templates;
mod ui;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Capability, Classification, Event, Manifest, Nav, Plugin, PluginError, Request,
    ResourcePanel, Response, RunInput, RunOutput, Setting, SettingKind,
};
use serde_json::{Value, json};
use tokio::sync::{Notify, RwLock};
use uuid::Uuid;

use crate::model::{OWN_TOPICS, topic_matches};

/// How long the engine waits for a nudge before looking for triggers anyway.
const IDLE: Duration = Duration::from_secs(5);
/// How stale the event automations this process knows of may get, since another instance may add one.
const EVENT_CACHE: Duration = Duration::from_secs(15);

#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub detail: String,
}

impl Refusal {
    pub fn bad(detail: impl Into<String>) -> Self {
        Self { status: 400, detail: detail.into() }
    }

    pub fn unauthorised(detail: impl Into<String>) -> Self {
        Self { status: 401, detail: detail.into() }
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
            401 => "unauthorised",
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
            PluginError::Refused { status, body } => {
                let detail = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|problem| problem["detail"].as_str().map(str::to_string))
                    .unwrap_or(body);
                Self { status: if status == 403 { 403 } else { 503 }, detail }
            }
            other => Self::unavailable(format!("the backend failed: {other}")),
        }
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

/// The automations listening for events, as `(id, topic filter)`, and when they were read.
type Listening = Option<(Instant, Vec<(Uuid, String)>)>;

/// What the routes, events and the engine share in this process.
#[derive(Default)]
pub struct Engine {
    nudge: Notify,
    cancelled: AtomicBool,
    events: RwLock<Listening>,
}

impl Engine {
    /// Tells the engine a trigger is waiting.
    pub fn wake(&self) {
        self.nudge.notify_one();
    }

    /// An automation was made, changed or deleted here.
    pub fn changed(&self) {
        if let Ok(mut events) = self.events.try_write() {
            *events = None;
        }
        self.wake();
    }

    async fn listening(&self, backend: &Backend) -> Result<Vec<(Uuid, String)>, Refusal> {
        if let Some((at, found)) = self.events.read().await.as_ref()
            && at.elapsed() < EVENT_CACHE
        {
            return Ok(found.clone());
        }
        let found = store::Store(backend).triggered_by("event").await?;
        *self.events.write().await = Some((Instant::now(), found.clone()));
        Ok(found)
    }

    async fn run(&self, backend: &Backend) -> Result<(), PluginError> {
        tracing::info!("the automation engine started");
        let mut minute = 0;
        while !self.cancelled.load(Ordering::Acquire) {
            let now = chrono::Utc::now().timestamp() / 60;
            if now != minute {
                minute = now;
                if let Err(refusal) = engine::tick(backend).await {
                    tracing::warn!(detail = %refusal.detail, "cron automations were not checked");
                }
            }
            match engine::consume(backend).await {
                Ok(0) => {
                    let _ = tokio::time::timeout(IDLE, self.nudge.notified()).await;
                }
                Ok(_) => {}
                Err(refusal) => {
                    tracing::warn!(detail = %refusal.detail, "the engine could not take triggers");
                    tokio::time::sleep(IDLE).await;
                }
            }
        }
        tracing::info!("the automation engine stopped");
        Ok(())
    }
}

#[derive(Default)]
struct Automation {
    engine: Engine,
}

/// A sample is kept at most this often per topic or queue, so a busy topic costs a write now and
/// then rather than one per event.
const SAMPLE_EVERY: Duration = Duration::from_secs(600);
/// Nor is an input this large kept: a condition's fields are found in far less.
const SAMPLE_BYTES: usize = 16 * 1024;

/// When each topic or queue was last sampled by this process.
static SAMPLED: std::sync::LazyLock<std::sync::Mutex<std::collections::HashMap<String, Instant>>> =
    std::sync::LazyLock::new(Default::default);

/// Keeps `input` as the sample of a topic or queue, unless one was kept recently or it is large.
pub async fn sample(backend: &Backend, kind: &str, name: &str, input: &Value) {
    let key = format!("{kind}:{name}");
    {
        let Ok(mut sampled) = SAMPLED.lock() else { return };
        if sampled.get(&key).is_some_and(|at| at.elapsed() < SAMPLE_EVERY) {
            return;
        }
        sampled.insert(key, Instant::now());
    }
    if serde_json::to_vec(input).map_or(true, |bytes| bytes.len() > SAMPLE_BYTES) {
        return;
    }
    let _ = store::Store(backend).sampled(kind, name, input).await;
}

#[async_trait]
impl Plugin for Automation {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // An action reads its credential while it runs, so what core gave us is kept where those
        // actions can reach it (ADR-0007).
        actions::remember(backend);
        tracing::info!(version = backend.version(), "automation loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        self.engine.cancelled.store(true, Ordering::Release);
        self.engine.wake();
        Ok(None)
    }

    /// The engine, when the backend starts the long-running run, or one automation's run.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let payload = &input.payload;
        if let Some(run) = payload["run"].as_str().and_then(|id| id.parse().ok()) {
            return Ok(RunOutput { payload: engine::execute(backend, run).await? });
        }
        if input.task.is_none() && payload.is_null() {
            self.engine.cancelled.store(false, Ordering::Release);
            self.engine.run(backend).await?;
            return Ok(RunOutput { payload: json!({ "stopped": true }) });
        }
        Err(PluginError::from("a run is the engine or {\"run\": <id>}"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        self.engine.cancelled.store(true, Ordering::Release);
        self.engine.wake();
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        api::handle(backend, &self.engine, request).await
    }

    /// Leaves a trigger for each automation listening for this topic; its own events never count.
    async fn on_event(&self, backend: &Backend, event: Event) -> Result<(), PluginError> {
        if event.source == "plugin.automation" || event.topic.starts_with(OWN_TOPICS) {
            return Ok(());
        }
        let input = json!({
            "trigger": "event",
            "topic": event.topic,
            "source": event.source,
            "event": event.id,
            "at": event.at,
            "payload": event.payload,
        });
        sample(backend, "event", &event.topic, &input).await;
        let listening = self.engine.listening(backend).await?;
        let matched: Vec<Uuid> = listening
            .into_iter()
            .filter(|(_, filter)| topic_matches(filter, &event.topic))
            .map(|(id, _)| id)
            .collect();
        if matched.is_empty() {
            return Ok(());
        }
        store::Store(backend).enqueue(&matched, "event", &input).await?;
        self.engine.wake();
        Ok(())
    }
}

doc_plugin_sdk::main!(
    Automation,
    Manifest {
        id: "automation".into(),
        classification: Classification::LongRunning,
        capabilities: vec![Capability::PublicRoutes],
        public_routes: vec!["hooks/*".into()],
        nav: vec![
            Nav::new("Automations", "/")
                .described("Run actions on a schedule, an event or a webhook")
                .grouped("Workspace")
        ],
        settings: vec![
            Setting::secret("slack-token", "Slack bot token")
                .hinted("What the Slack action posts with, such as xoxb-….")
                .grouped("Slack"),
            Setting::new("slack-api", "Slack API URL", SettingKind::Url)
                .hinted("Leave it empty for Slack's own.")
                .grouped("Slack"),
            Setting::secret("smtp-url", "SMTP URL")
                .hinted("Where email is sent, such as smtps://user:password@smtp.example:465.")
                .grouped("Email"),
            Setting::text("smtp-from", "Send email from")
                .hinted("The address email actions come from.")
                .grouped("Email"),
        ],
        resource_panels: ["organisation", "service", "repository", "team", "cloud-resource"]
            .into_iter()
            .map(|kind| ResourcePanel::new(kind, "Automations", "/panel"))
            .collect(),
        subscriptions: vec!["plugin.>".into(), "platform.>".into()],
        data: store::declaration(),
        ..Manifest::default()
    }
);
