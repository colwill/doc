//! Triggers become runs: the engine takes what webhooks, events, queues and schedules left waiting,
//! tests each against its automation's conditions, and queues a run as the automation's owner. A run
//! does the actions in order, keeping each outcome, so a retry skips what already succeeded.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, PluginError};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::actions::{Team, perform};
use crate::conditions::{context, first_failing};
use crate::store::{Automation, Run, Store};

const BATCH: i64 = 20;
pub const MAX_ATTEMPTS: i64 = 3;

/// Takes one batch of waiting triggers, answering how many it took.
pub async fn consume(backend: &Backend) -> Result<usize, Refusal> {
    let store = Store(backend);
    let claimed = store.claim(BATCH).await?;
    for trigger in &claimed {
        let (state, run, detail) = match store.automation(trigger.automation).await? {
            None => ("skipped", None, Some("the automation is gone".to_string())),
            Some(automation) if !automation.enabled => {
                ("skipped", None, Some("the automation is turned off".to_string()))
            }
            Some(automation) => {
                match first_failing(&automation.conditions, &context(&trigger.input, &automation)) {
                    Some(failed) => {
                        let op = serde_json::to_value(failed.op).unwrap_or_default();
                        let op = op.as_str().unwrap_or_default();
                        let why = format!(
                            "the condition {} {op} {} did not hold",
                            failed.field, failed.value
                        );
                        ("skipped", None, Some(why))
                    }
                    None => match start(
                        backend,
                        &automation,
                        Some(trigger.id),
                        trigger.input.clone(),
                        false,
                    )
                    .await
                    {
                        Ok(run) => ("done", Some(run.id), None),
                        Err((run, reason)) => ("failed", run, Some(reason)),
                    },
                }
            }
        };
        store.settle(trigger.id, state, run, detail.as_deref()).await?;
    }
    Ok(claimed.len())
}

/// Records a run and queues it as the owner; a refusal leaves the run failed with the reason.
pub async fn start(
    backend: &Backend,
    automation: &Automation,
    trigger: Option<Uuid>,
    input: Value,
    test: bool,
) -> Result<Run, (Option<Uuid>, String)> {
    let store = Store(backend);
    let now = Utc::now().to_rfc3339();
    let mut run = Run {
        id: Uuid::now_v7(),
        automation: automation.id,
        trigger,
        task: None,
        input,
        state: "queued".into(),
        steps: Vec::new(),
        error: None,
        attempts: 0,
        max_attempts: MAX_ATTEMPTS,
        test,
        created_at: now,
        started_at: None,
        finished_at: None,
    };
    store.new_run(&run).await.map_err(|refusal| (None, refusal.detail))?;
    let payload = json!({ "run": run.id });
    let attempts = i32::try_from(MAX_ATTEMPTS).ok();
    match backend.task_as(automation.delegation, payload, attempts).await {
        Ok(task) => {
            store.queued(run.id, task).await.map_err(|refusal| (Some(run.id), refusal.detail))?;
            run.task = Some(task);
            Ok(run)
        }
        Err(err) => {
            let reason = format!("the run could not start as {}: {err}", automation.owner_label);
            let _ = store.record(run.id, "failed", &[], Some(&reason)).await;
            Err((Some(run.id), reason))
        }
    }
}

/// Leaves a trigger for every cron automation whose latest time has come since it last fired.
pub async fn tick(backend: &Backend) -> Result<Value, Refusal> {
    let store = Store(backend);
    let now = Utc::now();
    let mut fired = Vec::new();
    for (id, schedule) in store.triggered_by("cron").await? {
        let Ok(cron) = schedule.parse::<croner::Cron>() else { continue };
        let Ok(latest) = cron.find_previous_occurrence(&now, true) else { continue };
        let Some(automation) = store.automation(id).await? else { continue };
        let since = automation.last_fired_at.as_deref().unwrap_or(&automation.created_at);
        let since =
            DateTime::parse_from_rfc3339(since).map(|at| at.with_timezone(&Utc)).unwrap_or(now);
        if latest <= since || !store.fire(id, &latest.to_rfc3339()).await? {
            continue;
        }
        let input = json!({ "trigger": "cron", "schedule": schedule, "scheduled_for": latest.to_rfc3339() });
        store.enqueue(&[id], "cron", &input).await?;
        fired.push(id);
    }
    Ok(json!({ "fired": fired }))
}

/// One attempt at a run, as the owner: every action not yet done, in order.
pub async fn execute(backend: &Backend, id: Uuid) -> Result<Value, PluginError> {
    let store = Store(backend);
    let run =
        store.run(id).await?.ok_or_else(|| PluginError::from(format!("there is no run {id}")))?;
    let automation = store
        .automation(run.automation)
        .await?
        .ok_or_else(|| PluginError::from("the run's automation is gone"))?;
    if matches!(run.state.as_str(), "succeeded" | "skipped") {
        return Ok(json!({ "run": id, "state": run.state }));
    }
    let as_owner = backend
        .caller()
        .and_then(|caller| Some(format!("{}:{}", caller.kind, caller.id.as_deref()?)))
        .is_some_and(|reference| reference == automation.owner);
    if !as_owner || !backend.writes() {
        let reason = format!("{} can no longer run automations", automation.owner_label);
        store.record(id, "failed", &run.steps, Some(&reason)).await?;
        return Err(PluginError::from(reason));
    }
    store.attempt(id).await?;
    let attempts = run.attempts + 1;
    let context = context(&run.input, &automation);
    let mut team = Team::of(backend, &automation.resource);
    let mut steps = run.steps.clone();
    steps.resize(automation.actions.len(), Value::Null);
    for (index, action) in automation.actions.iter().enumerate() {
        if steps[index]["state"] == "succeeded" {
            continue;
        }
        let outcome = perform(backend, action, &context, &mut team).await;
        let at = Utc::now().to_rfc3339();
        match outcome {
            Ok(output) => {
                steps[index] = json!({ "action": action.kind(), "state": "succeeded", "output": output, "at": at });
                store.record(id, "running", &steps, None).await?;
            }
            Err(reason) => {
                steps[index] = json!({ "action": action.kind(), "state": "failed", "error": reason, "at": at });
                let state = if attempts < run.max_attempts { "retrying" } else { "failed" };
                let error = format!("action {} ({}) failed: {reason}", index + 1, action.kind());
                store.record(id, state, &steps, Some(&error)).await?;
                return Err(PluginError::from(error));
            }
        }
    }
    store.record(id, "succeeded", &steps, None).await?;
    Ok(json!({ "run": id, "state": "succeeded", "actions": steps.len() }))
}
