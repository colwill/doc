//! Jobs: what somebody set Agent Smith to do, and when. Writing one takes leave to act as them;
//! runs are queued when asked, on a schedule or when an event names what the job is about.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Event};
use serde_json::{Value, json};
use tokio::sync::Notify;
use uuid::Uuid;

use crate::store::{JOBS, Job, QUEUED, REMINDERS, RUNS, Run, Store};
use crate::{Refusal, playbooks, runbooks};

/// Wakes the worker when a run is queued, rather than it waiting to look again.
pub static WAKE: Notify = Notify::const_new();

/// The events a job may be started by, and what the page calls each.
pub const EVENTS: [(&str, &str); 4] = [
    ("plugin.cicd.pipeline.broken", "A pipeline breaks"),
    ("plugin.reliability.outage.started", "An outage starts"),
    ("plugin.maturity.component.slipped", "A grade slips"),
    ("plugin.insights.scan.failed", "A repository scan fails"),
];

/// Where a finished run's report goes, besides its own page.
pub const DELIVER: [(&str, &str); 3] = [
    ("notify", "Notify me"),
    ("discuss", "Start a Watercooler discussion, tagged with what it is about"),
    ("team", "Notify everyone in the team it is about"),
];

/// The kinds a job may be about, as Watercooler tags and the Catalogue name them.
pub const SCOPES: [&str; 3] = ["service", "team", "organisation"];

pub struct NewJob {
    pub title: String,
    pub playbook: String,
    pub brief: String,
    pub scope: String,
    pub trigger: String,
    pub cron: String,
    pub events: Vec<String>,
    pub deliver: Vec<String>,
    pub enabled: bool,
    /// The runbook a runbook's job runs, as space/path.
    pub runbook: String,
}

/// Who is asking, for a page or a tool: a person, and whether they administer the platform.
pub struct Asker {
    pub id: Uuid,
    pub login: String,
    pub admin: bool,
    pub writes: bool,
}

impl Asker {
    pub fn of(backend: &Backend) -> Result<Self, Refusal> {
        let caller = backend.caller().ok_or_else(|| Refusal::forbidden("nobody is asking"))?;
        let id =
            caller.id.as_deref().and_then(|id| id.parse().ok()).filter(|_| caller.kind == "user");
        let id = id.ok_or_else(|| Refusal::forbidden("Agent Smith's jobs are set by people"))?;
        Ok(Self {
            id,
            login: caller.label.clone().unwrap_or_default(),
            admin: caller.admin,
            writes: backend.writes(),
        })
    }

    pub fn sees(&self, job: &Job) -> bool {
        self.admin || job.owner == self.id
    }

    /// A run is seen by whoever asked for it, its job's owner and administrators: it can hold
    /// whatever the access it ran with could read.
    pub fn sees_run(&self, run: &Run, job: Option<&Job>) -> bool {
        self.admin || run.owner == self.id || job.is_some_and(|job| job.owner == self.id)
    }
}

/// The next time a schedule falls due after `from`, and that it is no more often than hourly.
pub fn next(cron: &str, from: DateTime<Utc>) -> Result<DateTime<Utc>, String> {
    let parsed = cron
        .trim()
        .parse::<croner::Cron>()
        .map_err(|err| format!("`{cron}` is not a schedule: {err}"))?;
    let first = parsed
        .find_next_occurrence(&from, false)
        .map_err(|err| format!("`{cron}` never falls due: {err}"))?;
    let second = parsed
        .find_next_occurrence(&first, false)
        .map_err(|err| format!("`{cron}` never falls due again: {err}"))?;
    match second - first < chrono::Duration::minutes(59) {
        true => {
            Err("a job runs at most hourly: each run is a conversation with Claude, paid for"
                .into())
        }
        false => Ok(first),
    }
}

fn checked(new: &NewJob) -> Result<Value, Refusal> {
    let title = new.title.trim();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(Refusal::bad("give the job a title, in at most 200 characters"));
    }
    if playbooks::named(&new.playbook).is_none() {
        return Err(Refusal::bad("choose a playbook"));
    }
    let runbook = match new.playbook.as_str() {
        playbooks::RUNBOOK => {
            let named = new.runbook.trim();
            if runbooks::split(named).is_none() {
                return Err(Refusal::bad("choose the runbook it runs"));
            }
            named
        }
        _ => "",
    };
    if new.playbook == "custom" && new.brief.trim().is_empty() {
        return Err(Refusal::bad("a custom job needs a brief: say what to do"));
    }
    if new.brief.chars().count() > 4_000 {
        return Err(Refusal::bad("a brief is at most 4,000 characters"));
    }
    let scope = new.scope.trim();
    if !scope.is_empty() {
        let fine = scope
            .split_once(':')
            .is_some_and(|(kind, name)| SCOPES.contains(&kind) && !name.trim().is_empty());
        if !fine {
            return Err(Refusal::bad(
                "say what the job is about as kind:name, such as service:payments-api",
            ));
        }
    }
    let now = Utc::now();
    let next_at = match new.trigger.as_str() {
        "manual" => None,
        "schedule" => Some(next(&new.cron, now).map_err(Refusal::bad)?),
        "event" => {
            if new.events.is_empty() {
                return Err(Refusal::bad("choose what starts it"));
            }
            if let Some(unknown) =
                new.events.iter().find(|event| !EVENTS.iter().any(|(topic, _)| topic == event))
            {
                return Err(Refusal::bad(format!("{unknown} is not an event a job listens for")));
            }
            None
        }
        _ => return Err(Refusal::bad("choose when it runs")),
    };
    if let Some(unknown) =
        new.deliver.iter().find(|how| !DELIVER.iter().any(|(known, _)| known == how))
    {
        return Err(Refusal::bad(format!("{unknown} is not somewhere a report goes")));
    }
    if new.deliver.iter().any(|how| how == "team") && !scope.starts_with("team:") {
        return Err(Refusal::bad("a report goes to a team only when the job is about a team"));
    }
    Ok(json!({
        "title": title, "playbook": new.playbook, "brief": new.brief.trim(), "scope": scope,
        "trigger": new.trigger, "cron": new.cron.trim(), "events": new.events,
        "deliver": new.deliver, "enabled": new.enabled, "next_at": next_at, "runbook": runbook,
    }))
}

/// A runbook's job runs by itself with Agent Smith's own access, so only somebody holding
/// `runbooks` sets one, and only for a runbook that was approved.
async fn runbook_allowed(backend: &Backend, new: &NewJob) -> Result<(), Refusal> {
    if new.playbook != playbooks::RUNBOOK {
        return Ok(());
    }
    if !backend.allows(runbooks::PERMISSION, true) {
        return Err(Refusal::forbidden(
            "a runbook's job runs with Agent Smith's own access, so setting one needs its \
             runbooks permission",
        ));
    }
    match Store(backend).approval(new.runbook.trim()).await? {
        Some(_) => Ok(()),
        None => Err(Refusal::bad(
            "that runbook is not approved for Agent Smith to run by itself: approve it on its page \
             first",
        )),
    }
}

/// Leave to act as whoever is writing the job, which is what its runs go as.
async fn leave(backend: &Backend, title: &str) -> Result<Uuid, Refusal> {
    backend.delegate(&format!("Agent Smith: the job {title}")).await.map_err(|err| {
        Refusal::forbidden(format!("leave to act as you could not be taken: {}", err.detail()))
    })
}

pub async fn create(backend: &Backend, asker: &Asker, new: NewJob) -> Result<Job, Refusal> {
    if !asker.writes {
        return Err(Refusal::forbidden("that needs plugin:agent:user:rw"));
    }
    let mut values = checked(&new)?;
    runbook_allowed(backend, &new).await?;
    values["id"] = json!(Uuid::now_v7());
    values["owner"] = json!(asker.id);
    values["owner_label"] = json!(asker.login);
    // A runbook's job acts as Agent Smith, so it needs no leave to act as anybody.
    if new.playbook != playbooks::RUNBOOK {
        values["delegation"] = json!(leave(backend, &new.title).await?);
    }
    let job: Job = Store(backend).insert(JOBS, values).await?;
    let _ = backend
        .audit(
            "job.created",
            Some(&job.id.to_string()),
            json!({ "title": job.title, "playbook": job.playbook, "trigger": job.trigger }),
        )
        .await;
    Ok(job)
}

/// A job changed by its owner, or taken over by whoever changes it: its runs go as them from now.
pub async fn change(
    backend: &Backend,
    asker: &Asker,
    id: Uuid,
    new: NewJob,
) -> Result<Job, Refusal> {
    let store = Store(backend);
    let held = store.job(id).await?;
    if !asker.sees(&held) || !asker.writes {
        return Err(Refusal::forbidden("only its owner changes a job"));
    }
    let mut values = checked(&new)?;
    runbook_allowed(backend, &new).await?;
    let delegation = match new.playbook.as_str() {
        playbooks::RUNBOOK => None,
        _ => Some(leave(backend, &new.title).await?),
    };
    values["owner"] = json!(asker.id);
    values["owner_label"] = json!(asker.login);
    values["delegation"] = json!(delegation);
    let job: Job = store.update(JOBS, id, values).await?;
    if let Some(given) = held.delegation.filter(|given| Some(*given) != delegation) {
        let _ = backend.revoke(given).await;
    }
    let _ = backend
        .audit(
            "job.changed",
            Some(&id.to_string()),
            json!({ "title": job.title, "owner": asker.login }),
        )
        .await;
    Ok(job)
}

pub async fn delete(backend: &Backend, asker: &Asker, id: Uuid) -> Result<Job, Refusal> {
    let store = Store(backend);
    let job = store.job(id).await?;
    if !asker.sees(&job) || !asker.writes {
        return Err(Refusal::forbidden("only its owner deletes a job"));
    }
    if let Some(given) = job.delegation {
        let _ = backend.revoke(given).await;
    }
    store.delete(JOBS, id).await?;
    let _ =
        backend.audit("job.deleted", Some(&id.to_string()), json!({ "title": job.title })).await;
    Ok(job)
}

/// Queues a run of `job`, or adds `event` to the one already waiting, so a burst of events is
/// one run rather than many.
pub async fn queue(
    backend: &Backend,
    job: &Job,
    why: &str,
    event: Option<Value>,
) -> Result<Run, Refusal> {
    let store = Store(backend);
    let waiting = store.runs(json!({ "job": job.id, "state": QUEUED }), 1).await?;
    if let Some(run) = waiting.into_iter().next() {
        let mut events = run.events.as_array().cloned().unwrap_or_default();
        if let Some(event) = event {
            events.push(event);
        }
        let run: Run = store.update(RUNS, run.id, json!({ "events": events })).await?;
        WAKE.notify_one();
        return Ok(run);
    }
    let events: Vec<Value> = event.into_iter().collect();
    // A runbook's job runs by itself, at the version approved; one no longer approved does not.
    if job.playbook == playbooks::RUNBOOK {
        let approval = store.approval(&job.runbook).await?.ok_or_else(|| {
            Refusal::conflict(format!(
                "{} is no longer approved for Agent Smith to run by itself",
                job.runbook
            ))
        })?;
        let requester = (job.owner, job.owner_label.as_str());
        return runbooks::by_itself(backend, &approval, requester, why, Some(job.id), events).await;
    }
    let run: Run = store
        .insert(
            RUNS,
            json!({
                "id": Uuid::now_v7(), "job": job.id, "owner": job.owner,
                "owner_label": job.owner_label, "why": why, "events": events, "state": QUEUED,
            }),
        )
        .await?;
    WAKE.notify_one();
    Ok(run)
}

/// An event some job listens for: each enabled job it concerns gets a run.
pub async fn heard(backend: &Backend, event: &Event) -> Result<(), Refusal> {
    let store = Store(backend);
    let said = event.payload.to_string().to_lowercase();
    for job in store.jobs(None).await? {
        if !job.enabled || job.trigger != "event" || !job.events.contains(&event.topic) {
            continue;
        }
        let about = job
            .scope
            .split_once(':')
            .map(|(_, name)| name.trim().to_lowercase())
            .unwrap_or_default();
        if !about.is_empty() && !said.contains(&about) {
            continue;
        }
        let happened = json!({ "topic": event.topic, "at": Utc::now(), "payload": event.payload });
        // One job that cannot run, such as a runbook no longer approved, stops none of the others.
        if let Err(refusal) =
            queue(backend, &job, &format!("heard {}", event.topic), Some(happened)).await
        {
            tracing::warn!(job = %job.id, detail = %refusal.detail, "a job was not run");
        }
    }
    Ok(())
}

/// Every minute: reminders that are due, and the jobs whose schedule has come round.
pub async fn tick(backend: &Backend) -> Result<Value, Refusal> {
    let store = Store(backend);
    let now = Utc::now();
    let due = store
        .reminders(json!({ "at": { "lte": now }, "delivered_at": { "is_null": true } }))
        .await?;
    let mut delivered = 0;
    for reminder in &due {
        for user in &reminder.to {
            let url = Some(reminder.url.as_str()).filter(|url| !url.is_empty());
            if let Err(err) = backend.notify(user, "A reminder", &reminder.message, url).await {
                tracing::warn!(%err, reminder = %reminder.id, "a reminder was not delivered");
            }
        }
        let _ = store.update::<Value>(REMINDERS, reminder.id, json!({ "delivered_at": now })).await;
        delivered += 1;
    }
    let mut queued = 0;
    for job in store.jobs(None).await? {
        let (true, "schedule", Some(at)) = (job.enabled, job.trigger.as_str(), job.next_at) else {
            continue;
        };
        if at > now {
            continue;
        }
        let following = next(&job.cron, now).ok();
        let _ = store.update::<Value>(JOBS, job.id, json!({ "next_at": following })).await;
        if queue(backend, &job, "its schedule", None).await.is_ok() {
            queued += 1;
        }
    }
    Ok(json!({ "reminders": delivered, "queued": queued }))
}
