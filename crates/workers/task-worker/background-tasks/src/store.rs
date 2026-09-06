//! What a background task needs from storage, and an in-memory implementation the backend's
//! endpoint tests run against.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::Value;
use uuid::Uuid;

use crate::{Actor, NewTask, Task, TaskState};

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("storage unavailable: {0}")]
    Unavailable(String),
    #[error("{0}")]
    Other(String),
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Succeeded(Value),
    Failed(String),
    Cancelled,
    /// Back to `queued` with the attempt counted, so the Service Bus can hand it out again.
    Retry(String),
}

impl Outcome {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Succeeded(_) => "succeeded",
            Self::Failed(_) => "failed",
            Self::Cancelled => "cancelled",
            Self::Retry(_) => "retry",
        }
    }
}

#[derive(Debug, Clone)]
pub struct TaskFilter {
    pub kind: Option<String>,
    pub state: Option<TaskState>,
    /// Only what this actor started, which is all a caller without `core` read access may see.
    pub owner: Option<Actor>,
    pub limit: i64,
}

impl Default for TaskFilter {
    fn default() -> Self {
        Self { kind: None, state: None, owner: None, limit: 100 }
    }
}

#[async_trait]
pub trait TaskStore: Send + Sync {
    async fn create(&self, task: NewTask) -> Result<Task, TaskError>;

    async fn get(&self, id: Uuid) -> Result<Option<Task>, TaskError>;

    async fn list(&self, filter: TaskFilter) -> Result<Vec<Task>, TaskError>;

    /// The tasks queued while `root`'s run went on, however many hands they passed through.
    async fn chain(&self, root: Uuid) -> Result<Vec<Task>, TaskError>;

    /// Takes the task under a lease if it is queued, or if it is running but whoever held it
    /// stopped renewing — which is what a crashed worker leaves behind.
    async fn claim(&self, id: Uuid, lease: Duration) -> Result<Option<Task>, TaskError>;

    /// Extends the lease of a task this worker is running, so a live task is never reclaimed.
    async fn renew(&self, id: Uuid, lease: Duration) -> Result<(), TaskError>;

    async fn finish(&self, id: Uuid, outcome: Outcome) -> Result<(), TaskError>;

    /// Asks a task to stop. A queued task can be cancelled outright; a running one is only asked.
    async fn request_cancel(&self, id: Uuid) -> Result<bool, TaskError>;

    async fn cancel_requested(&self, id: Uuid) -> Result<bool, TaskError>;

    /// Running tasks whose lease has run out, returned to `queued` and named with their kind so
    /// each can be put back on its own queue.
    async fn requeue_stranded(&self) -> Result<Vec<(Uuid, String)>, TaskError>;

    async fn set_kind_paused(&self, kind: &str, paused: bool) -> Result<(), TaskError>;

    async fn paused_kinds(&self) -> Result<Vec<String>, TaskError>;
}

#[derive(Default)]
struct Held {
    tasks: Vec<(Task, Option<chrono::DateTime<Utc>>)>,
    paused: Vec<String>,
}

#[derive(Default)]
pub struct MemoryTasks {
    held: Mutex<Held>,
}

impl MemoryTasks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Drops the lease so `requeue_stranded` sees the task, which is how a crash is arranged.
    pub fn strand(&self, id: Uuid) {
        let mut held = self.held.lock();
        if let Some((_, lease)) = held.tasks.iter_mut().find(|(task, _)| task.id == id) {
            *lease = Some(Utc::now() - chrono::Duration::seconds(1));
        }
    }
}

#[async_trait]
impl TaskStore for MemoryTasks {
    async fn create(&self, new: NewTask) -> Result<Task, TaskError> {
        let task = Task {
            id: Uuid::now_v7(),
            kind: new.kind,
            state: TaskState::Queued,
            payload: new.payload,
            result: None,
            error: None,
            attempts: 0,
            max_attempts: new.max_attempts,
            cancel_requested: false,
            started_by: new.started_by,
            chain: new.chain,
            created_at: Utc::now(),
            started_at: None,
            finished_at: None,
        };
        self.held.lock().tasks.push((task.clone(), None));
        Ok(task)
    }

    async fn chain(&self, root: Uuid) -> Result<Vec<Task>, TaskError> {
        let held = self.held.lock();
        Ok(held
            .tasks
            .iter()
            .filter(|(task, _)| task.chain == Some(root))
            .map(|(task, _)| task.clone())
            .collect())
    }

    async fn get(&self, id: Uuid) -> Result<Option<Task>, TaskError> {
        Ok(self.held.lock().tasks.iter().find(|(task, _)| task.id == id).map(|(t, _)| t.clone()))
    }

    async fn list(&self, filter: TaskFilter) -> Result<Vec<Task>, TaskError> {
        let held = self.held.lock();
        let mut found: Vec<Task> = held
            .tasks
            .iter()
            .map(|(task, _)| task)
            .filter(|task| filter.kind.as_ref().is_none_or(|kind| &task.kind == kind))
            .filter(|task| filter.state.is_none_or(|state| task.state == state))
            .filter(|task| {
                filter.owner.as_ref().is_none_or(|owner| {
                    task.started_by.kind == owner.kind && task.started_by.id == owner.id
                })
            })
            .cloned()
            .collect();
        found.sort_by_key(|task| std::cmp::Reverse(task.created_at));
        found.truncate(filter.limit.max(0) as usize);
        Ok(found)
    }

    async fn claim(&self, id: Uuid, lease: Duration) -> Result<Option<Task>, TaskError> {
        let mut held = self.held.lock();
        let Some((task, until)) = held.tasks.iter_mut().find(|(task, _)| task.id == id) else {
            return Ok(None);
        };
        let stale =
            task.state == TaskState::Running && until.is_none_or(|until| until < Utc::now());
        if task.cancel_requested || (task.state != TaskState::Queued && !stale) {
            return Ok(None);
        }
        task.state = TaskState::Running;
        task.attempts += 1;
        task.started_at = Some(Utc::now());
        *until = Some(Utc::now() + chrono::Duration::from_std(lease).unwrap_or_default());
        Ok(Some(task.clone()))
    }

    async fn renew(&self, id: Uuid, lease: Duration) -> Result<(), TaskError> {
        let mut held = self.held.lock();
        if let Some((_, until)) = held.tasks.iter_mut().find(|(task, _)| task.id == id) {
            *until = Some(Utc::now() + chrono::Duration::from_std(lease).unwrap_or_default());
        }
        Ok(())
    }

    async fn finish(&self, id: Uuid, outcome: Outcome) -> Result<(), TaskError> {
        let mut held = self.held.lock();
        let Some((task, until)) = held.tasks.iter_mut().find(|(task, _)| task.id == id) else {
            return Ok(());
        };
        *until = None;
        match outcome {
            Outcome::Succeeded(result) => {
                task.state = TaskState::Succeeded;
                task.result = Some(result);
                task.finished_at = Some(Utc::now());
            }
            Outcome::Failed(error) => {
                task.state = TaskState::Failed;
                task.error = Some(error);
                task.finished_at = Some(Utc::now());
            }
            Outcome::Cancelled => {
                task.state = TaskState::Cancelled;
                task.finished_at = Some(Utc::now());
            }
            Outcome::Retry(error) => {
                task.state = TaskState::Queued;
                task.error = Some(error);
                task.started_at = None;
            }
        }
        Ok(())
    }

    async fn request_cancel(&self, id: Uuid) -> Result<bool, TaskError> {
        let mut held = self.held.lock();
        let Some((task, until)) = held.tasks.iter_mut().find(|(task, _)| task.id == id) else {
            return Ok(false);
        };
        if task.state.finished() {
            return Ok(false);
        }
        task.cancel_requested = true;
        if task.state == TaskState::Queued {
            task.state = TaskState::Cancelled;
            task.finished_at = Some(Utc::now());
            *until = None;
        }
        Ok(true)
    }

    async fn cancel_requested(&self, id: Uuid) -> Result<bool, TaskError> {
        Ok(self
            .held
            .lock()
            .tasks
            .iter()
            .find(|(task, _)| task.id == id)
            .is_some_and(|(task, _)| task.cancel_requested))
    }

    async fn requeue_stranded(&self) -> Result<Vec<(Uuid, String)>, TaskError> {
        let now = Utc::now();
        let mut held = self.held.lock();
        let mut requeued = Vec::new();
        for (task, until) in held.tasks.iter_mut() {
            if task.state == TaskState::Running && until.is_some_and(|until| until < now) {
                task.state = TaskState::Queued;
                task.started_at = None;
                *until = None;
                requeued.push((task.id, task.kind.clone()));
            }
        }
        Ok(requeued)
    }

    async fn set_kind_paused(&self, kind: &str, paused: bool) -> Result<(), TaskError> {
        let mut held = self.held.lock();
        held.paused.retain(|held| held != kind);
        if paused {
            held.paused.push(kind.to_string());
        }
        Ok(())
    }

    async fn paused_kinds(&self) -> Result<Vec<String>, TaskError> {
        Ok(self.held.lock().paused.clone())
    }
}
