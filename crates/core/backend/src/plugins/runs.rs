//! `run`, scheduled as §5's classifications say: a synchronous run in the immediate pool while its
//! caller waits, an async or one-shot run as a task on the plugin-runs queue, and a long-running run
//! held open for as long as the plugin is `running`. A run that fails fails only itself.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use doc_background_tasks::{
    Actor, Cancel, NewTask, Outcome, Task, TaskFilter, TaskHandler, TaskState,
};
use doc_eventbus::{Event, Topic};
use doc_immediate_tasks::ImmediateError;
use doc_permissions::Access;
use doc_plugin_protocol::{Caller, Classification, PluginState, RunInput, RunOutput};
use serde_json::{Value, json};
use uuid::Uuid;

use super::client::CallError;
use super::{Registered, SOURCE};
use crate::api::AppState;
use crate::identity::Principal;
use crate::permissions::{self, Authorised};

/// A long-running run has no deadline, but its context needs an expiry; this one outlives it.
const LONG_RUN_CONTEXT: Duration = Duration::from_secs(10 * 365 * 24 * 3600);
const CANCEL_POLL: Duration = Duration::from_millis(200);
const READY_POLL: Duration = Duration::from_millis(250);
/// Plugin runs are independent of one another, so one slow import does not hold up the rest.
const CONCURRENCY: usize = 16;

pub fn kind(plugin: &str) -> String {
    format!("plugin.{plugin}.run")
}

fn plugin_of(kind: &str) -> Option<&str> {
    kind.strip_prefix("plugin.")?.strip_suffix(".run")
}

pub fn caller_of(authorised: &Authorised) -> Caller {
    let mut caller = bare_caller(&authorised.principal);
    caller.admin = authorised.admin;
    caller.scope = authorised.scope.map(|scope| scope.to_string());
    caller.custom =
        authorised.custom.iter().map(|(name, scope)| (name.clone(), scope.to_string())).collect();
    caller.attributes = authorised.attributes.clone();
    caller
}

fn bare_caller(principal: &Principal) -> Caller {
    let (kind, id) = match principal {
        Principal::User(user) => ("user", user.id.to_string()),
        Principal::ServiceAccount(account) => ("service", account.id.to_string()),
        Principal::Plugin { id } => ("plugin", id.clone()),
    };
    let linked = principal.as_user().map(|user| user.linked.clone()).unwrap_or_default();
    Caller {
        kind: kind.into(),
        id: Some(id),
        label: Some(principal.label()),
        linked,
        ..Caller::default()
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RunError {
    #[error("{0} runs {1}; it does not take runs on demand")]
    NotOnDemand(String, &'static str),
    #[error("{0} did not finish its run in time")]
    Deadline(String),
    #[error("too many runs are waiting; try again shortly")]
    Busy,
    #[error("{0}")]
    Failed(String),
    #[error("{0}")]
    Unavailable(String),
}

/// What `POST /api/v1/plugins/{id}/run` does, by classification.
pub enum Started {
    Finished(RunOutput),
    Queued(Box<Task>),
}

pub async fn on_demand(
    state: &AppState,
    entry: &Registered,
    authorised: &Authorised,
    payload: Value,
    max_attempts: i32,
) -> Result<Started, RunError> {
    match entry.classification() {
        Classification::Synchronous => {
            now(state, entry, authorised, payload).await.map(Started::Finished)
        }
        Classification::Async => {
            let actor = crate::api::tasks::actor(&authorised.principal);
            queue(state, &entry.id, actor, payload, max_attempts)
                .await
                .map(|task| Started::Queued(Box::new(task)))
        }
        Classification::OneShot => {
            Err(RunError::NotOnDemand(entry.id.clone(), "once after each load"))
        }
        Classification::LongRunning => {
            Err(RunError::NotOnDemand(entry.id.clone(), "for as long as it is running"))
        }
    }
}

/// Runs while the caller waits, through the immediate pool so a flood is refused, not queued.
async fn now(
    state: &AppState,
    entry: &Registered,
    authorised: &Authorised,
    payload: Value,
) -> Result<RunOutput, RunError> {
    let deadline = state.config.plugins.request_deadline();
    let context = state
        .plugins
        .contexts
        .issue(&entry.id, authorised.principal.clone(), deadline)
        .map_err(RunError::Unavailable)?;
    let caller = caller_of(authorised);
    let input = RunInput { task: None, payload };
    let client = entry.client.clone();
    let token = context.token().to_string();
    let ran = state
        .plugin_runs
        .run(async move { Ok(client.run(&input, &caller, &token, Some(deadline)).await) })
        .await;
    drop(context);
    match ran {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(CallError::Deadline(_))) | Err(ImmediateError::Timeout(_)) => {
            Err(RunError::Deadline(entry.id.clone()))
        }
        Ok(Err(err)) => {
            tracing::warn!(plugin = %entry.id, %err, "a synchronous run failed");
            Err(RunError::Failed(err.detail()))
        }
        Err(ImmediateError::Busy) => Err(RunError::Busy),
        Err(ImmediateError::Failed(reason)) => Err(RunError::Failed(reason)),
    }
}

pub async fn queue(
    state: &AppState,
    plugin: &str,
    started_by: Actor,
    payload: Value,
    max_attempts: i32,
) -> Result<Task, RunError> {
    let mut new = NewTask::new(kind(plugin), payload).by(started_by);
    new.max_attempts = max_attempts;
    let task =
        doc_background_tasks::start(state.repos.tasks.as_ref(), state.buses.services.as_ref(), new)
            .await
            .map_err(|err| RunError::Unavailable(err.to_string()))?;
    crate::api::tasks::announce(state, "started", &task).await;
    Ok(task)
}

/// Asks the plugin's queued and running tasks to stop, since a cancelled plugin takes no runs.
pub async fn cancel_tasks(state: &AppState, plugin: &str) -> usize {
    let mut asked = 0;
    for waiting in [TaskState::Queued, TaskState::Running] {
        let filter =
            TaskFilter { kind: Some(kind(plugin)), state: Some(waiting), owner: None, limit: 500 };
        let Ok(tasks) = state.repos.tasks.list(filter).await else { continue };
        for task in tasks {
            if state.repos.tasks.request_cancel(task.id).await.unwrap_or(false) {
                asked += 1;
            }
        }
    }
    asked
}

/// Starts a long-running run after each load or resume, and queues a one-shot run after each load.
pub fn started(state: &AppState, id: &str, from: PluginState) {
    let (state, id) = (state.clone(), id.to_string());
    tokio::spawn(async move {
        let Some(entry) = state.plugins.get(&id).await else { return };
        match (entry.classification(), from) {
            (Classification::LongRunning, PluginState::Loading | PluginState::Cancelled) => {
                keep_running(state, entry).await;
            }
            (Classification::OneShot, PluginState::Loading) => {
                match queue(&state, &id, Actor::platform(), Value::Null, 3).await {
                    Ok(task) => {
                        tracing::info!(plugin = %id, task = %task.id, "one-shot run queued")
                    }
                    Err(err) => {
                        tracing::warn!(plugin = %id, %err, "the one-shot run was not queued")
                    }
                }
            }
            _ => {}
        }
    });
}

/// Holds a long-running run open; if it ends while this registration still runs, that is an error.
async fn keep_running(state: AppState, entry: Registered) {
    let id = entry.id.clone();
    let principal = Principal::Plugin { id: id.clone() };
    let context = match state.plugins.contexts.issue(&id, principal.clone(), LONG_RUN_CONTEXT) {
        Ok(context) => context,
        Err(err) => {
            let reason = format!("the long-running run could not start: {err}");
            super::fail(&state, &entry, &reason).await;
            return;
        }
    };
    tracing::info!(plugin = %id, "long-running run started");
    let caller = bare_caller(&principal);
    let ended = entry.client.run(&RunInput::default(), &caller, context.token(), None).await;
    drop(context);
    let current = state.plugins.get(&id).await;
    let still_running = current.is_some_and(|current| {
        current.instance == entry.instance && current.state == PluginState::Running
    });
    if !still_running {
        tracing::info!(plugin = %id, "long-running run stopped");
        return;
    }
    // A lost connection says nothing about the plugin, which may still be working, so it is only
    // unreachable: its next liveness report reloads it, which ends the old run before a new one.
    let reason = match ended {
        Ok(output) => format!("the long-running run returned: {}", output.payload),
        Err(err @ CallError::Unreachable(..)) => {
            format!("{}: the long-running run lost its connection ({err})", super::UNREACHABLE)
        }
        Err(err) => format!("the long-running run failed: {}", err.detail()),
    };
    super::fail(&state, &entry, &reason).await;
}

/// Runs every `plugin.<id>.run` task. It lives in the backend, beside the plugins' connections.
pub fn start_pool(state: &AppState) {
    let pool =
        doc_background_tasks::Pool::new(state.repos.tasks.clone(), state.buses.services.clone())
            .on_queue(doc_background_tasks::PLUGIN_RUNS)
            .with_concurrency(CONCURRENCY)
            .otherwise(Arc::new(Runs { state: state.clone() }))
            .announcing(Arc::new(crate::api::tasks::Announcer(state.buses.events.clone())));
    tokio::spawn(async move {
        if let Err(err) = pool.run().await {
            tracing::error!(%err, "the plugin-runs pool stopped");
        }
    });
}

struct Runs {
    state: AppState,
}

impl Runs {
    /// Waits up to the liveness grace for a loading or re-registering plugin to take runs.
    async fn ready(&self, plugin: &str, cancel: &Cancel) -> Result<Registered, String> {
        let until = tokio::time::Instant::now() + self.state.config.plugins.grace();
        loop {
            let found = self.state.plugins.get(plugin).await;
            let paused = self.state.plugins.gate(plugin).is_closed();
            if let Some(entry) = found.as_ref().filter(|entry| !paused && entry.state.accepts_run())
            {
                return Ok(entry.clone());
            }
            if tokio::time::Instant::now() >= until || cancel.is_cancelled() {
                return Err(match found {
                    Some(entry) => {
                        format!("{plugin} is {} and not taking runs", entry.state.as_str())
                    }
                    None => format!("{plugin} is not running"),
                });
            }
            tokio::time::sleep(READY_POLL).await;
        }
    }

    async fn handed_over(&self, plugin: &str, instance: Uuid) -> bool {
        self.state.plugins.handing_over(plugin)
            || self
                .state
                .plugins
                .get(plugin)
                .await
                .is_some_and(|current| current.instance != instance)
    }

    /// Whoever started the task, as they are now: a disabled account is not acted for.
    async fn starter(&self, plugin: &str, actor: &Actor) -> Result<Principal, String> {
        let identity = &self.state.repos.identity;
        let id = || actor.id.as_deref().and_then(|id| id.parse::<Uuid>().ok());
        let principal = match actor.kind.as_str() {
            "user" => id()
                .map(|id| identity.user_by_id(id))
                .ok_or("the task names no user")?
                .await
                .map_err(|err| err.to_string())?
                .map(Principal::User),
            "service-account" => id()
                .map(|id| identity.service_account_by_id(id))
                .ok_or("the task names no service account")?
                .await
                .map_err(|err| err.to_string())?
                .map(Principal::ServiceAccount),
            "plugin" => actor.id.clone().map(|id| Principal::Plugin { id }),
            _ => Some(Principal::Plugin { id: plugin.to_string() }),
        };
        match principal {
            Some(principal) if principal.disabled() => {
                Err(format!("{} is disabled", principal.label()))
            }
            Some(principal) => Ok(principal),
            None => Err("whoever started this task no longer exists".into()),
        }
    }
}

async fn stopped(cancel: &Cancel) {
    while !cancel.is_cancelled() {
        tokio::time::sleep(CANCEL_POLL).await;
    }
}

#[async_trait]
impl TaskHandler for Runs {
    async fn run(&self, task: &Task, cancel: Cancel) -> Result<Value, String> {
        let plugin =
            plugin_of(&task.kind).ok_or_else(|| format!("{} is not a plugin run", task.kind))?;
        let principal = self.starter(plugin, &task.started_by).await?;
        let (authorised, _) =
            permissions::assess(&self.state, principal.clone(), plugin, Access::Write).await;
        let caller = caller_of(&authorised);
        let input = RunInput { task: Some(task.id), payload: task.payload.clone() };
        loop {
            let entry = self.ready(plugin, &cancel).await?;
            let deadline = self.state.config.plugins.run_deadline(entry.classification());
            let chain = task.chain.unwrap_or(task.id);
            let contexts = &self.state.plugins.contexts;
            let context = contexts.issue_in(plugin, principal.clone(), deadline, chain)?;
            let call = entry.client.run(&input, &caller, context.token(), Some(deadline));
            let ran = tokio::select! {
                ran = tokio::time::timeout(deadline, call) => ran,
                () = stopped(&cancel) => return Err("cancelled".into()),
            };
            match ran {
                Ok(Ok(output)) => return Ok(output.payload),
                // The old version stopped it to unload; it runs again on the new one.
                _ if self.handed_over(plugin, entry.instance).await => {
                    tracing::info!(plugin, task = %task.id, "a run cut short by a handover moves on");
                }
                Ok(Err(err)) => return Err(err.detail()),
                Err(_) => {
                    return Err(format!("{plugin} did not finish within {}s", deadline.as_secs()));
                }
            }
        }
    }

    async fn finished(&self, task: &Task, outcome: &Outcome) {
        let Some(plugin) = plugin_of(&task.kind) else { return };
        let mut payload = json!({ "task": task.id, "plugin": plugin });
        match outcome {
            Outcome::Succeeded(result) => {
                payload["state"] = json!("succeeded");
                payload["result"] = result.clone();
            }
            Outcome::Failed(error) => {
                payload["state"] = json!("failed");
                payload["error"] = json!(error);
            }
            Outcome::Cancelled => payload["state"] = json!("cancelled"),
            Outcome::Retry(_) => return,
        }
        let Ok(topic) = Topic::new(format!("plugin.{plugin}.run.completed")) else { return };
        if let Err(err) = self.state.buses.events.publish(Event::new(topic, SOURCE, payload)).await
        {
            tracing::warn!(%err, plugin, task = %task.id, "a run's completion was not announced");
        }
    }
}
