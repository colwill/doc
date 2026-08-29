//! Runbooks: Knowledge Base pages saying what to do when something goes wrong, which Agent Smith
//! runs with Claude (FEAT-AGENT). Whoever presses Run lends it their access for that run, and is
//! its requester; a schedule, an event or an automation runs one with Agent Smith's own access, and
//! only at the version somebody holding `runbooks` approved, so editing the page cannot steer that
//! access. Against production a run only reads; against development or test, nothing it asks may
//! change production.

use chrono::Utc;
use doc_plugin_sdk::{Backend, Guard};
use serde_json::{Value, json};
use url::form_urlencoded::byte_serialize;
use uuid::Uuid;

use crate::Refusal;
use crate::jobs::{Asker, WAKE};
use crate::store::{
    AGENT, APPROVALS, Approval, ENVIRONMENTS, Job, QUEUED, REQUESTER, RUNS, Run, Store,
};

/// Approving runbooks for Agent Smith to run by itself, and setting jobs and automations to.
pub const PERMISSION: &str = "runbooks";

/// How a page and a form name a runbook: its space and its path there.
pub fn key(space: &str, path: &str) -> String {
    format!("{}/{}", space.trim(), path.trim().trim_start_matches('/'))
}

/// A runbook's space and path, from its key.
pub fn split(key: &str) -> Option<(&str, &str)> {
    key.split_once('/').filter(|(space, path)| !space.is_empty() && !path.is_empty())
}

/// Its page in Agent Smith.
pub fn href(key: &str) -> String {
    format!("/p/agent/runbooks/{key}")
}

pub fn text(value: &Value, field: &str) -> String {
    value[field].as_str().unwrap_or_default().to_string()
}

fn encoded(part: &str) -> String {
    byte_serialize(part.as_bytes()).collect()
}

/// One part of a page's address, as it was before it was put in one.
pub fn decoded(part: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", part.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

/// The environment a form chose, or `fallback` when it chose none.
pub fn environment(chosen: &str, fallback: &str) -> Result<&'static str, Refusal> {
    let chosen = match chosen.trim() {
        "" => fallback,
        chosen => chosen,
    };
    ENVIRONMENTS
        .iter()
        .find(|known| **known == chosen)
        .copied()
        .ok_or_else(|| Refusal::bad("run it against development, test or production"))
}

/// What a run against `environment` may change: nothing in production; in development or test,
/// nothing in production either. A job's run, which has no environment, is limited by nothing.
pub fn guard(environment: &str) -> Option<Guard> {
    match environment {
        "" => None,
        "production" => Some(Guard::ReadOnly),
        _ => Some(Guard::NotProduction),
    }
}

fn answered(asked: Result<(u16, Value), doc_plugin_sdk::PluginError>) -> Result<Value, Refusal> {
    match asked {
        Ok((200, body)) => Ok(body),
        Ok((status, body)) => {
            let detail = body["detail"].as_str().map_or_else(|| body.to_string(), str::to_string);
            let status = if matches!(status, 400 | 403 | 404) { status } else { 503 };
            Err(Refusal { status, detail: format!("the Knowledge Base said: {detail}") })
        }
        Err(err) => Err(Refusal::unavailable(format!(
            "the Knowledge Base could not be asked: {}",
            err.detail()
        ))),
    }
}

/// One runbook, with what it says and the hash of that, read as whoever this backend acts for.
pub async fn read(backend: &Backend, key: &str) -> Result<Value, Refusal> {
    let (space, path) = split(key).ok_or_else(|| Refusal::bad("name a runbook as space/path"))?;
    let path: Vec<String> = path.split('/').map(encoded).collect();
    let route = format!("runbooks/{}/{}", encoded(space), path.join("/"));
    answered(backend.ask("kb", "GET", &route, None, None).await)
}

/// Every runbook whoever this backend acts for can read.
pub async fn list(backend: &Backend) -> Result<Vec<Value>, Refusal> {
    let body = answered(backend.ask("kb", "GET", "runbooks", None, None).await)?;
    Ok(body["runbooks"].as_array().cloned().unwrap_or_default())
}

/// Somebody pressing Run: the runbook runs with their access, as it is at each call, and they are
/// its requester. `job` is the runbook's job, when they pressed Run on that.
pub async fn requested(
    backend: &Backend,
    asker: &Asker,
    key: &str,
    chosen: &str,
    job: Option<&Job>,
) -> Result<Run, Refusal> {
    if !asker.writes {
        return Err(Refusal::forbidden("that needs plugin:agent:user:rw"));
    }
    let runbook = read(backend, key).await?;
    let environment = environment(chosen, &text(&runbook, "environment"))?;
    let title = text(&runbook, "title");
    let delegation =
        backend.delegate(&format!("Agent Smith: the runbook {title}")).await.map_err(|err| {
            Refusal::forbidden(format!("leave to act as you could not be taken: {}", err.detail()))
        })?;
    let values = json!({
        "id": Uuid::now_v7(), "job": job.map(|job| job.id), "owner": asker.id,
        "owner_label": asker.login, "why": format!("{} pressing Run", asker.login),
        "runbook": key, "runbook_title": title, "environment": environment, "access": REQUESTER,
        "delegation": delegation, "hash": text(&runbook, "hash"), "events": [], "state": QUEUED,
    });
    let run: Run = match Store(backend).insert(RUNS, values).await {
        Ok(run) => run,
        Err(refusal) => {
            let _ = backend.revoke(delegation).await;
            return Err(refusal);
        }
    };
    let detail = json!({ "runbook": key, "environment": environment, "requester": asker.login });
    let _ = backend.audit("runbook.requested", Some(&run.id.to_string()), detail).await;
    WAKE.notify_one();
    Ok(run)
}

/// A run with Agent Smith's own access, of the version of a runbook that was approved, against
/// the environment it was approved for: for a schedule, an event or an automation.
pub async fn by_itself(
    backend: &Backend,
    approval: &Approval,
    requester: (Uuid, &str),
    why: &str,
    job: Option<Uuid>,
    events: Vec<Value>,
) -> Result<Run, Refusal> {
    let values = json!({
        "id": Uuid::now_v7(), "job": job, "owner": requester.0, "owner_label": requester.1,
        "why": why, "runbook": approval.runbook, "runbook_title": approval.title,
        "environment": approval.environment, "access": AGENT, "hash": approval.hash,
        "events": events, "state": QUEUED,
    });
    let run: Run = Store(backend).insert(RUNS, values).await?;
    let detail = json!({ "runbook": approval.runbook, "environment": approval.environment, "requester": requester.1 });
    let _ = backend.audit("runbook.queued", Some(&run.id.to_string()), detail).await;
    WAKE.notify_one();
    Ok(run)
}

/// An automation asking, as the person it acts for: the runbook runs by itself, if they hold
/// `runbooks` and it was approved.
pub async fn automated(backend: &Backend, key: &str) -> Result<Run, Refusal> {
    let caller = backend.caller().ok_or_else(|| Refusal::forbidden("nobody is asking"))?;
    let id = caller.id.as_deref().and_then(|id| id.parse::<Uuid>().ok());
    let id = id.ok_or_else(|| Refusal::forbidden("an automation runs a runbook for somebody"))?;
    if !backend.allows(PERMISSION, true) {
        return Err(Refusal::forbidden(
            "an automation runs a runbook by itself only for somebody holding Agent Smith's \
             runbooks permission",
        ));
    }
    let approval = Store(backend).approval(key).await?.ok_or_else(|| {
        Refusal::forbidden(format!(
            "{key} is not approved for Agent Smith to run by itself: somebody holding the \
             runbooks permission approves it on its page in Agent Smith"
        ))
    })?;
    let label = caller.label.clone().unwrap_or_default();
    by_itself(backend, &approval, (id, &label), &format!("an automation of {label}"), None, vec![])
        .await
}

/// Approves the runbook as it reads now, against `chosen`, for Agent Smith to run by itself.
pub async fn approve(
    backend: &Backend,
    asker: &Asker,
    key: &str,
    chosen: &str,
) -> Result<Approval, Refusal> {
    if !backend.allows(PERMISSION, true) {
        return Err(Refusal::forbidden(
            "approving a runbook needs Agent Smith's runbooks permission",
        ));
    }
    let runbook = read(backend, key).await?;
    let environment = environment(chosen, &text(&runbook, "environment"))?;
    let values = json!({
        "runbook": key, "title": text(&runbook, "title"), "hash": text(&runbook, "hash"),
        "environment": environment, "approved_by": asker.id, "approved_by_label": asker.login,
        "approved_at": Utc::now(),
    });
    let store = Store(backend);
    let approval: Approval = match store.approval(key).await? {
        Some(held) => store.update(APPROVALS, held.id, values).await?,
        None => {
            let mut values = values;
            values["id"] = json!(Uuid::now_v7());
            store.insert(APPROVALS, values).await?
        }
    };
    let detail = json!({ "runbook": key, "environment": environment, "hash": approval.hash });
    let _ = backend.audit("runbook.approved", Some(key), detail).await;
    Ok(approval)
}

/// Takes the approval away: schedules, events and automations stop running it.
pub async fn withdraw(backend: &Backend, key: &str) -> Result<(), Refusal> {
    if !backend.allows(PERMISSION, true) {
        return Err(Refusal::forbidden(
            "withdrawing an approval needs Agent Smith's runbooks permission",
        ));
    }
    let store = Store(backend);
    if let Some(held) = store.approval(key).await? {
        store.delete(APPROVALS, held.id).await?;
        let _ = backend.audit("runbook.withdrawn", Some(key), json!({ "runbook": key })).await;
    }
    Ok(())
}

/// What Claude is asked, for a runbook's run: the runbook itself follows as the first message.
pub fn instructions(run: &Run, who: &str) -> String {
    let limits = match run.environment.as_str() {
        "production" => {
            "This run is against production, so it only reads: every call that would change \
             something is refused. Where the runbook says to change something, check whether it \
             is needed, and put it in the report as a step for the requester, with exactly what \
             to do."
        }
        _ => {
            "This run is against development or test: you may change what the access you have \
             allows there, but nothing in production, which is refused whatever it is."
        }
    };
    format!(
        "Run the runbook \"{title}\", which follows, against {environment}. {limits}\n\n\
         1. Work out from what DOC can see which of the runbook's situations applies, if any, \
         before acting on one.\n\
         2. Do each step you have a tool for, and check it worked.\n\
         3. For each step outside DOC, such as a command, a console or a phone call, write \
         exactly what the requester should do; you cannot do those.\n\
         4. Finish with the report: what you found, what you did, what was refused and why, and \
         what is left for the requester, in that order.\n\n\
         {who}",
        title = run.runbook_title,
        environment = run.environment,
    )
}

/// Every runbook the caller can read, with whether it is approved to run by itself and whether the
/// page still says what was approved.
pub async fn shown(backend: &Backend) -> Result<Vec<Value>, Refusal> {
    let approvals = Store(backend).approvals().await?;
    let mut listed = list(backend).await?;
    for runbook in &mut listed {
        let key = key(&text(runbook, "space"), &text(runbook, "path"));
        runbook["approval"] = match approvals.iter().find(|approval| approval.runbook == key) {
            Some(approval) => json!({
                "environment": approval.environment, "hash": approval.hash,
                "by": approval.approved_by_label, "at": approval.approved_at,
            }),
            None => Value::Null,
        };
        runbook["key"] = json!(key);
    }
    Ok(listed)
}

/// `GET api/runbooks` and `POST api/runbooks/run`. Somebody asking themselves runs a runbook with
/// their own access; an automation asking for them runs one by itself, approved; nothing else does.
pub async fn api(
    backend: &Backend,
    request: &doc_plugin_sdk::Request,
    route: &[&str],
) -> Result<Value, Refusal> {
    match (request.method.as_str(), route) {
        ("GET", []) => Ok(json!({ "runbooks": shown(backend).await? })),
        ("POST", ["run"]) => {
            let body: Value = request.json().map_err(|err| Refusal::bad(err.to_string()))?;
            let named = match body["runbook"].as_str() {
                Some(runbook) => runbook.trim().to_string(),
                None => key(&text(&body, "space"), &text(&body, "path")),
            };
            if split(&named).is_none() {
                return Err(Refusal::bad(
                    "name the runbook: space and path, or runbook as space/path",
                ));
            }
            let via = backend.caller().and_then(|caller| caller.via.clone());
            let run = match via.as_deref() {
                None => {
                    let asker = Asker::of(backend)?;
                    requested(backend, &asker, &named, &text(&body, "environment"), None).await?
                }
                Some("automation") => automated(backend, &named).await?,
                Some(other) => {
                    return Err(Refusal::forbidden(format!(
                        "{other} cannot ask Agent Smith to run a runbook: people and automations do"
                    )));
                }
            };
            Ok(json!({
                "run": run.id, "href": run.href(), "access": run.access,
                "environment": run.environment, "requester": run.owner_label,
            }))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}
