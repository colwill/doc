//! The JSON routes at `api/`: what an administrator's own agent calls with the scoped token a run
//! gave it, to read its brief, hand items in and finish. Only the administrator who started the run
//! reaches it, whether through that token or signed in, and only while it is taking items in.

use chrono::Utc;
use doc_plugin_sdk::{Backend, Caller, Request, Response};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::stage::{self, NewDiscussion, NewPage, NewResource};
use crate::store::{COLLECTING, Item, REVIEW, Run, Store};

/// However many an LLM sends at once, one call hands in no more than this.
pub const BATCH: usize = 50;

pub type Answer = Result<Value, Refusal>;

pub fn id_of(text: &str, what: &str) -> Result<Uuid, Refusal> {
    text.parse().map_err(|_| Refusal::missing(format!("there is no such {what}")))
}

fn body(request: &Request) -> Result<Value, Refusal> {
    let bytes = if request.body.is_empty() { &b"{}"[..] } else { &request.body[..] };
    serde_json::from_slice(bytes)
        .map_err(|err| Refusal::bad(format!("the body is not JSON: {err}")))
}

/// Who is asking: their user ID, and whether they administer the platform.
pub fn caller(backend: &Backend) -> Result<(Option<Uuid>, bool), Refusal> {
    match backend.caller() {
        Some(Caller { kind, id, admin, .. }) if kind == "user" => {
            Ok((id.as_deref().and_then(|id| id.parse().ok()), *admin))
        }
        Some(Caller { admin: true, .. }) => Ok((None, true)),
        _ => Err(Refusal::forbidden("the Data Vacuum is for administrators")),
    }
}

/// The run, for the administrator who started it: through the token it gave their agent, or
/// signed in. Anyone else is refused, other administrators included, since a run's items are
/// that administrator's to hand in.
pub async fn owned(backend: &Backend, id: &str) -> Result<Run, Refusal> {
    let (user, _) = caller(backend)?;
    let run = Store(backend)
        .run(id_of(id, "run")?)
        .await?
        .ok_or_else(|| Refusal::missing("there is no such run"))?;
    match user == Some(run.created_by_id) {
        true => Ok(run),
        false => Err(Refusal::forbidden("this run is its administrator's")),
    }
}

/// The kind of thing handed in, and the key its list comes under.
#[derive(Clone, Copy)]
pub enum Handed {
    Pages,
    Resources,
    Discussions,
}

impl Handed {
    pub fn key(self) -> &'static str {
        match self {
            Self::Pages => "pages",
            Self::Resources => "resources",
            Self::Discussions => "discussions",
        }
    }

    fn checked(self, run: &Run, value: Value) -> Result<Item, Refusal> {
        let read = |err: serde_json::Error| Refusal::bad(format!("not what this takes: {err}"));
        match self {
            Self::Pages => {
                stage::page(run, &serde_json::from_value::<NewPage>(value).map_err(read)?)
            }
            Self::Resources => {
                stage::resource(run, serde_json::from_value::<NewResource>(value).map_err(read)?)
            }
            Self::Discussions => stage::discussion(
                run,
                &serde_json::from_value::<NewDiscussion>(value).map_err(read)?,
            ),
        }
    }
}

/// Stages each item handed in, answering how many were and why the others were not, so the LLM
/// can put them right and hand them in again.
pub async fn hand_in(backend: &Backend, run: &Run, handed: Handed, asked: &Value) -> Answer {
    let list = match asked.get(handed.key()) {
        Some(Value::Array(list)) => list.clone(),
        _ => return Err(Refusal::bad(format!("send {{\"{}\": [ … ]}}", handed.key()))),
    };
    if list.is_empty() || list.len() > BATCH {
        return Err(Refusal::bad(format!("hand in 1 to {BATCH} at a time")));
    }
    let store = Store(backend);
    let (mut staged, mut refused) = (0usize, Vec::new());
    for (index, value) in list.into_iter().enumerate() {
        let kept = match handed.checked(run, value) {
            Ok(item) => stage::keep(&store, run, &item).await,
            Err(refusal) => Err(refusal),
        };
        match kept {
            Ok(_) => staged += 1,
            Err(refusal) if refusal.status < 500 => {
                refused.push(json!({ "index": index, "reason": refusal.detail }));
            }
            Err(refusal) => return Err(refusal),
        }
    }
    Ok(json!({ "staged": staged, "refused": refused }))
}

/// Ends a run's taking in: what was handed in waits for the administrator, and the agent's token,
/// if it had one, stops working.
pub async fn finish(backend: &Backend, run: &Run, summary: &str) -> Result<Run, Refusal> {
    if run.state != COLLECTING {
        return Err(Refusal::conflict("this run has finished already"));
    }
    let mut run = run.clone();
    run.state = REVIEW.to_string();
    run.summary = summary.trim().chars().take(20_000).collect();
    run.finished_at = Some(Utc::now());
    revoke(backend, &mut run).await;
    Store(backend).save_run(&run).await?;
    let payload =
        json!({ "run": run.id, "title": run.title, "url": format!("/p/vacuum/runs/{}", run.id) });
    if let Err(err) = backend.publish("plugin.vacuum.run.finished", payload).await {
        tracing::warn!(%err, "a finished run was not announced");
    }
    Ok(run)
}

/// Takes the agent's token away, if the run gave it one.
pub async fn revoke(backend: &Backend, run: &mut Run) {
    if let Some(token) = run.token_id.take() {
        if let Err(err) = backend.revoke_scoped_token(token).await {
            tracing::warn!(%err, run = %run.id, "an agent's token could not be revoked; it expires by itself");
        }
        run.token_expires_at = None;
    }
}

pub fn run_shown(run: &Run, items: &[Item]) -> Value {
    let count = |state: &str| items.iter().filter(|item| item.state == state).count();
    json!({
        "id": run.id, "title": run.title, "mode": run.mode, "sources": run.sources,
        "brief": run.brief, "state": run.state, "summary": run.summary,
        "items": items.len(),
        "staged": count(crate::store::STAGED), "approved": count(crate::store::APPROVED),
        "applied": count(crate::store::APPLIED), "rejected": count(crate::store::REJECTED),
    })
}

async fn route(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let store = Store(backend);
    match (request.method.as_str(), path) {
        ("GET", ["runs", id]) => {
            let run = owned(backend, id).await?;
            Ok(run_shown(&run, &store.items(run.id).await?))
        }
        ("GET", ["runs", id, "items"]) => {
            let run = owned(backend, id).await?;
            let items = store.items(run.id).await?;
            Ok(json!({ "items": items.iter().map(|item| json!({
                "id": item.id, "destination": item.destination, "key": item.key,
                "title": item.title, "source": item.source, "state": item.state,
            })).collect::<Vec<_>>() }))
        }
        ("POST", ["runs", id, kind @ ("pages" | "resources" | "discussions")]) => {
            let run = owned(backend, id).await?;
            let handed = match *kind {
                "pages" => Handed::Pages,
                "resources" => Handed::Resources,
                _ => Handed::Discussions,
            };
            hand_in(backend, &run, handed, &body(request)?).await
        }
        ("POST", ["runs", id, "finish"]) => {
            let run = owned(backend, id).await?;
            let summary = body(request)?["summary"].as_str().unwrap_or_default().to_string();
            let run = finish(backend, &run, &summary).await?;
            Ok(run_shown(&run, &store.items(run.id).await?))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    match route(backend, request, path).await {
        Ok(value) => Response::json(&value),
        Err(refusal) => refusal.response(),
    }
}
