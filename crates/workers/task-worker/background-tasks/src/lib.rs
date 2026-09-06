//! TaskWorker background tasks pool. A task is a row that outlives the process running it: the
//! Service Bus hands it out, the row records what happened, and a lease that has run out is how a
//! task stranded by a crash is found again.

pub mod postgres;
pub mod store;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use doc_servicebus::{Address, Message, ServiceBus};
use opentelemetry::KeyValue;
use opentelemetry::metrics::Histogram;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tracing::Instrument;
use uuid::Uuid;

pub use store::{Outcome, TaskError, TaskFilter, TaskStore};

/// The queue the backend already registers, which is how a task reaches a worker.
pub const QUEUE: &str = "tasks";
/// Plugin runs have their own queue: only the backend, which holds their connections, can run them.
pub const PLUGIN_RUNS: &str = "plugin-runs";

pub fn queue_for(kind: &str) -> &'static str {
    if kind.starts_with("plugin.") { PLUGIN_RUNS } else { QUEUE }
}

/// Deliberately shorter than the queue's own visibility timeout: when a worker dies, the lease has
/// to be stale by the time the Service Bus hands the task to someone else, or nobody may take it.
const LEASE: Duration = Duration::from_secs(20);
const RENEW_EVERY: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

impl std::str::FromStr for TaskState {
    type Err = TaskError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "queued" => Ok(Self::Queued),
            "running" => Ok(Self::Running),
            "succeeded" => Ok(Self::Succeeded),
            "failed" => Ok(Self::Failed),
            "cancelled" => Ok(Self::Cancelled),
            other => Err(TaskError::Other(format!("{other} is not a task state"))),
        }
    }
}

/// Who started the task, kept so `GET /api/v1/tasks/{id}` can answer without a second lookup.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Actor {
    pub kind: String,
    pub id: Option<String>,
    pub label: Option<String>,
}

impl Actor {
    pub fn platform() -> Self {
        Self { kind: "platform".into(), id: None, label: None }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: Uuid,
    pub kind: String,
    pub state: TaskState,
    pub payload: Value,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub attempts: i32,
    pub max_attempts: i32,
    pub cancel_requested: bool,
    pub started_by: Actor,
    /// The first task of the run it is part of: set when a task queues more work while it runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chain: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub finished_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone)]
pub struct NewTask {
    pub kind: String,
    pub payload: Value,
    pub max_attempts: i32,
    pub started_by: Actor,
    pub chain: Option<Uuid>,
}

impl NewTask {
    pub fn new(kind: impl Into<String>, payload: Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
            max_attempts: 3,
            started_by: Actor::platform(),
            chain: None,
        }
    }

    pub fn by(mut self, actor: Actor) -> Self {
        self.started_by = actor;
        self
    }
}

/// Asked to stop, rather than stopped: a handler decides where it is safe to give up.
#[derive(Clone, Default)]
pub struct Cancel(Arc<AtomicBool>);

impl Cancel {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[async_trait]
pub trait TaskHandler: Send + Sync {
    async fn run(&self, task: &Task, cancel: Cancel) -> Result<Value, String>;

    /// Called once a task has its final outcome, and never for an attempt that will be retried.
    async fn finished(&self, _task: &Task, _outcome: &Outcome) {}
}

/// Told of every task a pool finishes, whichever handler ran it.
#[async_trait]
pub trait Finished: Send + Sync {
    async fn finished(&self, task: &Task, outcome: &Outcome);
}

/// Writes the row and puts it on the queue. The row comes first, so a task that reaches the queue
/// is always one that can be looked up.
pub async fn start(
    store: &dyn TaskStore,
    bus: &dyn ServiceBus,
    new: NewTask,
) -> Result<Task, TaskError> {
    let task = store.create(new).await?;
    if let Err(err) = enqueue(bus, &task.kind, task.id).await {
        store.finish(task.id, Outcome::Failed(format!("could not be queued: {err}"))).await?;
        return Err(err);
    }
    Ok(task)
}

pub async fn enqueue(bus: &dyn ServiceBus, kind: &str, id: Uuid) -> Result<(), TaskError> {
    let address =
        Address::core(queue_for(kind)).map_err(|err| TaskError::Other(err.to_string()))?;
    bus.send(Message::new(address, "run", json!({ "task": id })))
        .await
        .map_err(|err| TaskError::Other(err.to_string()))?;
    Ok(())
}

#[derive(Clone)]
pub struct Pool {
    store: Arc<dyn TaskStore>,
    bus: Arc<dyn ServiceBus>,
    queue: &'static str,
    handlers: BTreeMap<String, Arc<dyn TaskHandler>>,
    /// Takes every kind no handler is named for, which is how one handler runs every plugin.
    otherwise: Option<Arc<dyn TaskHandler>>,
    lease: Duration,
    poll: Duration,
    /// How many tasks run at once. One by default, so a pool works through its queue in order.
    permits: Arc<Semaphore>,
    announce: Option<Arc<dyn Finished>>,
}

impl Pool {
    pub fn new(store: Arc<dyn TaskStore>, bus: Arc<dyn ServiceBus>) -> Self {
        Self {
            store,
            bus,
            queue: QUEUE,
            handlers: BTreeMap::new(),
            otherwise: None,
            lease: LEASE,
            poll: Duration::from_millis(500),
            permits: Arc::new(Semaphore::new(1)),
            announce: None,
        }
    }

    pub fn announcing(mut self, hook: Arc<dyn Finished>) -> Self {
        self.announce = Some(hook);
        self
    }

    pub fn on_queue(mut self, queue: &'static str) -> Self {
        self.queue = queue;
        self
    }

    pub fn handling(mut self, kind: &str, handler: Arc<dyn TaskHandler>) -> Self {
        self.handlers.insert(kind.to_string(), handler);
        self
    }

    pub fn otherwise(mut self, handler: Arc<dyn TaskHandler>) -> Self {
        self.otherwise = Some(handler);
        self
    }

    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.permits = Arc::new(Semaphore::new(concurrency.max(1)));
        self
    }

    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    pub fn kinds(&self) -> Vec<&str> {
        self.handlers.keys().map(String::as_str).collect()
    }

    /// Returns stranded tasks to the queue, then takes work until the process stops.
    pub async fn run(&self) -> Result<(), TaskError> {
        let address = Address::core(self.queue).map_err(|err| TaskError::Other(err.to_string()))?;
        self.recover().await;
        loop {
            let permit = self
                .permits
                .clone()
                .acquire_owned()
                .await
                .map_err(|err| TaskError::Other(err.to_string()))?;
            match self.bus.receive(&address).await {
                Ok(Some(lease)) => {
                    let pool = self.clone();
                    tokio::spawn(async move {
                        pool.deliver(lease).await;
                        drop(permit);
                    });
                }
                Ok(None) => tokio::time::sleep(self.poll).await,
                Err(err) => {
                    tracing::warn!(%err, queue = self.queue, "the task queue could not be read");
                    tokio::time::sleep(self.poll).await;
                }
            }
        }
    }

    /// Requeues the tasks a dead worker left `running`, each on the queue for its kind.
    pub async fn recover(&self) {
        match self.store.requeue_stranded().await {
            Ok(stranded) if stranded.is_empty() => {}
            Ok(stranded) => {
                tracing::info!(count = stranded.len(), "requeued tasks left running by a crash");
                for (id, kind) in stranded {
                    if let Err(err) = enqueue(self.bus.as_ref(), &kind, id).await {
                        tracing::warn!(%err, %id, "a stranded task could not be requeued");
                    }
                }
            }
            Err(err) => tracing::warn!(%err, "could not look for stranded tasks"),
        }
    }

    async fn deliver(&self, lease: doc_servicebus::Lease) {
        let Some(id) = lease.message.payload["task"].as_str().and_then(|id| id.parse().ok()) else {
            let _ = self.bus.nack(lease.id, "the message does not name a task").await;
            return;
        };
        match self.run_one(id, lease.message.trace.as_deref()).await {
            Ok(()) => {
                let _ = self.bus.ack(lease.id).await;
            }
            Err(Retry(reason)) => {
                let _ = self.bus.nack(lease.id, &reason).await;
            }
        }
    }

    async fn run_one(&self, id: Uuid, trace: Option<&str>) -> Result<(), Retry> {
        let Ok(paused) = self.store.paused_kinds().await else {
            return Err(Retry("the paused kinds could not be read".into()));
        };
        let Ok(Some(peek)) = self.store.get(id).await else {
            // The row is gone, so nothing will ever run: acknowledge rather than retry forever.
            return Ok(());
        };
        if paused.contains(&peek.kind) {
            return Err(Retry(format!("{} is paused", peek.kind)));
        }
        let Some(handler) = self.handlers.get(&peek.kind).or(self.otherwise.as_ref()).cloned()
        else {
            let reason = format!("no handler for {}", peek.kind);
            let _ = self.store.finish(id, Outcome::Failed(reason)).await;
            return Ok(());
        };
        let span = tracing::info_span!(
            target: "doc",
            "task.run",
            otel.name = %format!("task {}", peek.kind),
            otel.kind = "consumer",
            otel.status_description = tracing::field::Empty,
            doc.task.id = %id,
            doc.task.kind = %peek.kind,
            doc.task.queue = self.queue,
            doc.task.attempt = tracing::field::Empty,
            doc.task.outcome = tracing::field::Empty,
        );
        doc_telemetry::adopt(&span, trace);
        self.run_claimed(id, handler.as_ref()).instrument(span).await
    }

    async fn run_claimed(&self, id: Uuid, handler: &dyn TaskHandler) -> Result<(), Retry> {
        let Ok(Some(task)) = self.store.claim(id, self.lease).await else {
            // Cancelled, finished, or already held by another worker.
            return Ok(());
        };
        let span = tracing::Span::current();
        span.record("doc.task.attempt", task.attempts);
        let started = std::time::Instant::now();
        let cancel = Cancel::default();
        let watcher = self.supervise(id, cancel.clone());
        let outcome = match handler.run(&task, cancel.clone()).await {
            Ok(value) => Outcome::Succeeded(value),
            Err(_) if cancel.is_cancelled() => Outcome::Cancelled,
            // `claim` has already counted this attempt.
            Err(reason) if task.attempts < task.max_attempts => Outcome::Retry(reason),
            Err(reason) => Outcome::Failed(reason),
        };
        watcher.abort();
        span.record("doc.task.outcome", outcome.name());
        ran(&task.kind, outcome.name(), started.elapsed());
        if let Outcome::Failed(reason) | Outcome::Retry(reason) = &outcome {
            span.record("otel.status_description", reason.as_str());
        }
        let retry = match &outcome {
            Outcome::Retry(reason) => Some(reason.clone()),
            _ => None,
        };
        if let Err(err) = self.store.finish(id, outcome.clone()).await {
            tracing::warn!(%err, %id, "a finished task could not be recorded");
        }
        match retry {
            Some(reason) => Err(Retry(reason)),
            None => {
                handler.finished(&task, &outcome).await;
                if let Some(hook) = &self.announce {
                    hook.finished(&task, &outcome).await;
                }
                Ok(())
            }
        }
    }

    /// Holds the lease while the handler works, and passes on a cancellation the moment it is
    /// asked for. Stopping this is what makes a crashed worker's task available again.
    fn supervise(&self, id: Uuid, cancel: Cancel) -> tokio::task::JoinHandle<()> {
        let store = self.store.clone();
        let poll = self.poll;
        let lease = self.lease;
        tokio::spawn(async move {
            let mut since_renewal = Duration::ZERO;
            loop {
                tokio::time::sleep(poll).await;
                since_renewal += poll;
                if since_renewal >= RENEW_EVERY {
                    since_renewal = Duration::ZERO;
                    if let Err(err) = store.renew(id, lease).await {
                        tracing::warn!(%err, %id, "a task lease could not be renewed");
                    }
                }
                if store.cancel_requested(id).await.unwrap_or(false) {
                    cancel.cancel();
                    return;
                }
            }
        })
    }
}

struct Retry(String);

/// Records a run, as `doc.tasks.run.duration` by kind and outcome, which also counts them.
fn ran(kind: &str, outcome: &'static str, took: Duration) {
    static RUNS: OnceLock<Histogram<f64>> = OnceLock::new();
    let runs = RUNS.get_or_init(|| {
        doc_telemetry::seconds(
            "doc.tasks.run.duration",
            "How long each task run took, by kind and outcome",
        )
    });
    let labels = [KeyValue::new("kind", kind.to_string()), KeyValue::new("outcome", outcome)];
    runs.record(took.as_secs_f64(), &labels);
}
