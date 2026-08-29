//! The JSON routes: the models and their criteria, what has been attested, the scorecards, and a
//! readiness answer so the rest of the platform can ask how mature a service is the same way it
//! asks everything else (DOC-SPEC §9.2).

use std::collections::{BTreeMap, BTreeSet};

use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::model::{Check, Component, Kind, Owner};
use crate::store::{Criterion, Model, Store};
use crate::{ID, Refusal, parameter};

type Answer = Result<(u16, Value), Refusal>;

/// Under this grade a service is not ready; at or above it, it is. Ten is everything, so asking
/// for ten would make readiness say no forever.
const READY_AT: u8 = 8;
const WARN_AT: u8 = 5;

fn id(text: &str) -> Result<Uuid, Refusal> {
    text.parse().map_err(|_| Refusal::bad("that is not an ID"))
}

pub fn model_shown(model: &Model, criteria: usize) -> Value {
    json!({
        "id": model.id,
        "name": model.name,
        "description": model.description,
        "owner": model.owner,
        "grades": model.kinds.clone(),
        "enabled": model.enabled,
        "criteria": criteria,
        "url": model.href(),
    })
}

pub fn criterion_shown(criterion: &Criterion) -> Value {
    let check = criterion.check();
    json!({
        "id": criterion.id,
        "model": criterion.model,
        "title": criterion.title,
        "description": criterion.description,
        "applies_to": criterion.kind,
        "weight": criterion.weight(),
        "check": criterion.check,
        "about": check.as_ref().map(Check::about).unwrap_or_default(),
        "problem": check.err(),
    })
}

/// What a model is made of, checked: everything a page or the API sends goes through here, so a
/// criterion that could never be decided is refused when it is written rather than each time it
/// is scored.
pub fn checked_criterion(
    model: Uuid,
    title: &str,
    kind: &str,
    weight: i64,
    check: &Value,
) -> Result<Value, Refusal> {
    let title = title.trim();
    if title.is_empty() {
        return Err(Refusal::bad("give the criterion a title: what has to be true"));
    }
    let kind = Kind::parse(kind).ok_or_else(|| {
        Refusal::bad("a criterion is about a service, repository, documentation or cloud-resource")
    })?;
    let read: Check = serde_json::from_value(check.clone())
        .map_err(|err| Refusal::bad(format!("that check could not be read: {err}")))?;
    if let Some(why) = read.unsuitable(kind) {
        return Err(Refusal::bad(format!("this check cannot decide a {}: {why}", kind.one())));
    }
    if let Check::Connected { kind: wanted, .. } = &read
        && Kind::parse(wanted).is_none()
    {
        return Err(Refusal::bad(format!(
            "`{wanted}` is not something the Catalogue connects to: name a service, repository, \
             documentation or cloud-resource"
        )));
    }
    if let Check::Metadata { key, .. } = &read
        && key.trim().is_empty()
    {
        return Err(Refusal::bad("name the metadata key the criterion looks for"));
    }
    if let Check::Readiness { plugin, .. } = &read
        && plugin.trim().is_empty()
    {
        return Err(Refusal::bad("name the plugin whose readiness the criterion asks for"));
    }
    if let Check::Model { model: stands_on, .. } = &read
        && *stands_on == model
    {
        return Err(Refusal::bad("a model cannot stand on itself"));
    }
    Ok(json!({
        "model": model,
        "title": title,
        "kind": kind.id(),
        "weight": weight.clamp(1, 10),
        "check": check,
    }))
}

/// A model as it is written, checked.
pub fn checked_model(name: &str, owner: &str, kinds: &[String]) -> Result<Value, Refusal> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Refusal::bad("give the model a name"));
    }
    let owner = Owner::parse(owner).map_err(Refusal::bad)?;
    let kinds: Vec<String> = Kind::ALL
        .into_iter()
        .filter(|kind| kinds.iter().any(|named| Kind::parse(named) == Some(*kind)))
        .map(|kind| kind.id().to_string())
        .collect();
    if kinds.is_empty() {
        return Err(Refusal::bad("choose at least one thing for the model to grade"));
    }
    Ok(json!({ "name": name, "owner": owner.reference(), "kinds": kinds }))
}

/// Who is asking, as leave records them.
fn asker(backend: &Backend) -> Option<String> {
    let caller = backend.caller()?;
    (caller.kind == "user").then(|| caller.label.clone().or_else(|| caller.id.clone()))?
}

/// Whether making `model` stand on `on` would close a circle: `on`, or something `on` stands on,
/// already stands on `model`. Two models waiting on each other could never be scored, so it is
/// refused when it is written rather than found out at five in the morning.
pub async fn would_circle(
    store: &Store<'_>,
    model: Uuid,
    on: Uuid,
) -> Result<Option<String>, Refusal> {
    let mut seen: BTreeSet<Uuid> = BTreeSet::new();
    let mut walking = vec![on];
    while let Some(next) = walking.pop() {
        if next == model {
            let named = store.model(on).await.ok().map(|found| found.name);
            let named = named.unwrap_or_else(|| "that model".to_string());
            return Ok(Some(format!(
                "{named} already stands on this one, directly or through another, and two models \
                 waiting on each other could never be scored"
            )));
        }
        if !seen.insert(next) {
            continue;
        }
        for criterion in store.criteria(Some(next)).await? {
            if let Some(further) = criterion.check().ok().and_then(|check| check.stands_on()) {
                walking.push(further);
            }
        }
    }
    Ok(None)
}

/// Scores one model again, now, rather than waiting for the schedule. A change nobody sees until
/// tomorrow is a change nobody trusts.
///
/// This also takes leave to read the Catalogue as whoever asked. A run nobody starts — the
/// schedule, or a Catalogue change — has nobody to read it as otherwise, and the Catalogue tells
/// the plugin nothing, so without this the only scoring that ever works is the kind somebody
/// presses a button for.
pub async fn rescore(backend: &Backend, model: Uuid) {
    if let Some(who) = asker(backend) {
        crate::leave::grant(backend, &who).await;
    }
    if let Err(err) = backend.task(json!({ "model": model })).await {
        tracing::warn!(%err, plugin = ID, "a model changed but was not scored again");
    }
}

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let answer = route(backend, request, &segments).await;
    match answer {
        Ok((status, value)) => {
            let mut response = Response::json(&value);
            response.status = status;
            response
        }
        Err(refusal) => refusal.response(),
    }
}

async fn route(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let store = Store(backend);
    let query = request.query.as_str();
    match (request.method.as_str(), path) {
        ("GET", ["api", "models"]) => {
            let models = store.models().await?;
            let criteria = store.criteria(None).await?;
            let shown: Vec<Value> = models
                .iter()
                .map(|model| {
                    let count =
                        criteria.iter().filter(|criterion| criterion.model == model.id).count();
                    model_shown(model, count)
                })
                .collect();
            Ok((200, json!({ "models": shown })))
        }
        ("GET", ["api", "models", model]) => {
            let model = store.model(id(model)?).await?;
            let criteria = store.criteria(Some(model.id)).await?;
            let cards = store.scorecards(Some(model.id)).await?;
            Ok((
                200,
                json!({
                    "model": model_shown(&model, criteria.len()),
                    "criteria": criteria.iter().map(criterion_shown).collect::<Vec<_>>(),
                    "scorecards": cards.iter().map(crate::store::Scorecard::json).collect::<Vec<_>>(),
                }),
            ))
        }
        ("GET", ["api", "scorecards"]) => {
            let wanted = parameter(query, "component");
            let cards = match &wanted {
                Some(component) => store.of_component(component).await?,
                None => store.scorecards(None).await?,
            };
            Ok((
                200,
                json!({ "scorecards": cards.iter().map(crate::store::Scorecard::json).collect::<Vec<_>>() }),
            ))
        }
        // The same answer every plugin that measures a service gives, so the roadmap and anything
        // else can ask how mature a service is without knowing anything about models.
        ("GET", ["api", "readiness"]) => readiness(backend, query).await,
        ("POST", ["api", "score"]) => {
            let only = parameter(query, "model").map(|model| id(&model)).transpose()?;
            if let Some(who) = asker(backend) {
                crate::leave::grant(backend, &who).await;
            }
            let task = backend
                .task(match only {
                    Some(model) => json!({ "model": model }),
                    None => json!({}),
                })
                .await
                .map_err(Refusal::from)?;
            Ok((202, json!({ "task": task })))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

/// How mature each service asked about is, worst model first.
async fn readiness(backend: &Backend, query: &str) -> Answer {
    let store = Store(backend);
    let names: Vec<String> = url::form_urlencoded::parse(query.as_bytes())
        .filter(|(key, _)| key == "service")
        .map(|(_, value)| value.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() {
        return Err(Refusal::bad("name at least one service"));
    }
    let models: BTreeMap<Uuid, Model> =
        store.models().await?.into_iter().map(|model| (model.id, model)).collect();
    let mut services = serde_json::Map::new();
    for name in &names {
        let component = Component { kind: Kind::Service, name: name.clone() };
        let cards = store.of_component(&component.reference()).await?;
        let worst = cards.iter().min_by_key(|card| card.grade);
        let answer = match worst {
            None => json!({
                "state": "unknown",
                "summary": "No maturity model grades it yet",
            }),
            Some(card) => {
                let grade = card.grade().value();
                let state = match grade {
                    grade if grade >= READY_AT => "ready",
                    grade if grade >= WARN_AT => "warning",
                    _ => "blocked",
                };
                let named = models.get(&card.model).map(|model| model.name.clone());
                let against = named.unwrap_or_else(|| "a maturity model".to_string());
                let short: Vec<&str> = card
                    .results
                    .iter()
                    .filter(|result| !result.met && !result.unknown)
                    .map(|result| result.title.as_str())
                    .take(3)
                    .collect();
                let summary = match short.is_empty() {
                    true => format!("{grade} of 10 against {against}"),
                    false => {
                        format!("{grade} of 10 against {against}; short on {}", short.join(", "))
                    }
                };
                json!({
                    "state": state,
                    "summary": summary,
                    "href": format!("/p/maturity/models/{}", card.model),
                })
            }
        };
        services.insert(name.clone(), answer);
    }
    Ok((200, json!({ "title": "Maturity", "services": services })))
}
