//! Scoring: for each model, every component it grades, judged against its criteria and written
//! down as a scorecard.
//!
//! This is the automation the models are checked by. It runs on a schedule, and a change to a
//! model or a criterion scores that model again at once, so a criterion somebody adds shows on the
//! scorecards without anybody waiting a day for it. Each run publishes what moved, which is what
//! an Automation listens to when a team wants telling that a service slipped a grade.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use doc_plugin_sdk::{Backend, DataRequest, PluginError};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::checks::{self, encoded};
use crate::model::{Component, Grade, Kind, Owner, Result_};
use crate::store::{Attestation, Criterion, Model, Scorecard, Store};
use crate::{ID, Refusal};

/// The most components of one kind a model grades.
const MOST: usize = 500;

/// Said when a scorecard is written for the first time, and whenever its grade moves.
pub const GRADED: &str = "plugin.maturity.component.graded";
/// Said when a grade falls, so an Automation can tell whoever keeps it without watching the rest.
pub const SLIPPED: &str = "plugin.maturity.component.slipped";

/// The Catalogue saying something about it changed. Almost every criterion is decided from what
/// the Catalogue holds, so a grade that only followed the schedule would be a day out of date the
/// moment somebody connected a repository to a service.
pub const CATALOGUE_CHANGED: &str = "plugin.resources.changed";

/// How long the Catalogue is given to stop changing before everything is scored again. Changes
/// arrive in bursts — one apply writes a resource and each of its connections, and a sync writes
/// hundreds — so the first of them starts the wait and everything inside it is scored by the same
/// run, rather than one run per document.
const SETTLE: Duration = Duration::from_secs(120);

/// Whether a scoring run is already waiting to be queued for changes that have landed.
#[derive(Default, Clone)]
pub struct Settling(Arc<AtomicBool>);

impl Settling {
    /// Scores everything once the Catalogue has been quiet for `SETTLE`. A change arriving while
    /// one of these is waiting is covered by it, so a burst costs one run.
    pub fn after_the_change(&self, backend: &Backend) {
        if self.0.swap(true, Ordering::SeqCst) {
            return;
        }
        let backend = backend.clone();
        let waiting = self.0.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SETTLE).await;
            // Cleared first, so a change landing while this is being queued starts the next wait
            // rather than being swallowed by the run that is already on its way.
            waiting.store(false, Ordering::SeqCst);
            // Nobody started this, so it goes as whoever last gave leave to read the Catalogue.
            match crate::leave::score_as_whoever_gave_leave(&backend, json!({})).await {
                Ok(said) => {
                    tracing::info!(%said, "the Catalogue changed, so everything is scored again");
                }
                Err(err) => {
                    tracing::warn!(%err, "the Catalogue changed but nothing could be queued");
                }
            }
        });
    }
}

/// What a run carries from one model to the next: what has been scored so far, and what each
/// model came to for each component, which is what a model standing on another reads.
#[derive(Default)]
struct Progress {
    standing: checks::Standing,
    scored: Scored,
}

/// What one run came to, for the page and the API to show.
#[derive(Debug, Default, Clone)]
pub struct Scored {
    pub models: usize,
    pub components: usize,
    pub moved: usize,
    pub slipped: usize,
    pub forgotten: usize,
    pub problems: Vec<String>,
}

impl Scored {
    pub fn json(&self) -> Value {
        json!({
            "models": self.models,
            "components": self.components,
            "moved": self.moved,
            "slipped": self.slipped,
            "forgotten": self.forgotten,
            "problems": self.problems,
        })
    }
}

/// The components a model grades: everything of the kinds it names, narrowed to a team's own
/// things where the model is a team's.
async fn components(
    backend: &Backend,
    model: &Model,
    owned: &BTreeSet<String>,
) -> Result<Vec<Component>, Refusal> {
    let mut found = Vec::new();
    for kind in model.kinds() {
        let query = encoded(&[("kind", kind.id()), ("limit", &MOST.to_string())]);
        let listed = match backend.ask("resources", "GET", "resources", Some(&query), None).await {
            Ok((200, Value::Array(listed))) => listed,
            Ok((status, body)) => {
                return Err(Refusal::unavailable(format!(
                    "the Catalogue answered {status} for {}: {}",
                    kind.many(),
                    body["detail"].as_str().unwrap_or("it refused")
                )));
            }
            Err(err) => {
                return Err(Refusal::unavailable(format!(
                    "the Catalogue could not be asked: {err}"
                )));
            }
        };
        for resource in listed {
            let Some(name) = resource["name"].as_str().filter(|name| !name.is_empty()) else {
                continue;
            };
            // A team's model grades the team's own things. The Catalogue names an owning team on a
            // resource, so that is what decides it rather than anything this plugin keeps.
            let theirs = match model.owner() {
                Some(owner) if owner.is_organisation() => true,
                Some(_) => owned.contains(&format!("{}:{name}", kind.id())),
                None => true,
            };
            if theirs {
                found.push(Component { kind, name: name.to_string() });
            }
        }
    }
    Ok(found)
}

/// Everything a team owns, as `kind:name`, so a team's model grades its own and nothing else.
async fn owned_by(backend: &Backend, owner: &Owner) -> BTreeSet<String> {
    let mut owned = BTreeSet::new();
    if owner.is_organisation() {
        return owned;
    }
    let query = encoded(&[("of", &format!("team:{}", owner.name))]);
    let Ok((200, body)) = backend.ask("resources", "GET", "neighbours", Some(&query), None).await
    else {
        return owned;
    };
    for neighbour in body["neighbours"].as_array().into_iter().flatten() {
        let (Some(kind), Some(name)) = (neighbour["kind"].as_str(), neighbour["name"].as_str())
        else {
            continue;
        };
        if let Some(kind) = Kind::parse(kind) {
            owned.insert(format!("{}:{name}", kind.id()));
        }
    }
    owned
}

/// One model, scored over everything it grades.
async fn score_model(
    backend: &Backend,
    store: &Store<'_>,
    model: &Model,
    criteria: &[Criterion],
    attestations: &[Attestation],
    held: &BTreeMap<String, Scorecard>,
    run: &mut Progress,
) -> Result<Vec<String>, Refusal> {
    let owned = match model.owner() {
        Some(owner) => owned_by(backend, &owner).await,
        None => BTreeSet::new(),
    };
    let components = components(backend, model, &owned).await?;
    let services: BTreeSet<String> = components
        .iter()
        .filter(|component| component.kind == Kind::Service)
        .map(|component| component.name.clone())
        .collect();
    let readiness = checks::readiness_for(backend, criteria, &services).await;
    let (mut writes, mut kept, mut announced) = (Vec::new(), Vec::new(), Vec::new());
    for component in &components {
        let reference = component.reference();
        kept.push(reference.clone());
        // Only the criteria written for this kind of thing: a model that grades services and
        // repositories asks each of them its own questions.
        let theirs: Vec<&Criterion> =
            criteria.iter().filter(|criterion| criterion.kind() == Some(component.kind)).collect();
        if theirs.is_empty() {
            continue;
        }
        let known = checks::known(backend, component).await;
        let results: Vec<Result_> = theirs
            .iter()
            .map(|criterion| {
                let attested = attestations.iter().find(|attestation| {
                    attestation.criterion == criterion.id && attestation.component == reference
                });
                checks::decide(criterion, component, &known, attested, &readiness, &run.standing)
            })
            .collect();
        // What could not be read is left out of both sides, so an unreachable plugin drags
        // nobody's grade down; the scorecard says which criteria those were.
        let asked: u32 = results.iter().filter(|r| !r.unknown).map(|r| r.weight).sum();
        let met: u32 = results.iter().filter(|r| r.met && !r.unknown).map(|r| r.weight).sum();
        let grade = Grade::of(met, asked);
        let before = held.get(&reference);
        let was = before.map(|card| card.grade);
        let moved = was.is_some_and(|was| was != i64::from(grade.value()));
        if moved {
            run.scored.moved += 1;
            if was.is_some_and(|was| was > i64::from(grade.value())) {
                run.scored.slipped += 1;
            }
        }
        run.scored.components += 1;
        writes.push(DataRequest::upsert(
            "scorecards",
            &["model", "component"],
            json!({
                "model": model.id,
                "component": reference,
                "kind": component.kind.id(),
                "name": component.name,
                "grade": grade.value(),
                "met": met,
                "asked": asked,
                "results": results.iter().map(Result_::json).collect::<Vec<_>>(),
                "scored_at": Utc::now(),
                "was": was,
            }),
        ));
        if before.is_none() || moved {
            announced.push((component.clone(), grade, was, model.clone()));
        }
        // What a model standing on this one reads, later in the same run.
        run.standing
            .entry(model.id)
            .or_default()
            .insert(reference, (i64::from(grade.value()), model.name.clone()));
    }
    store.write_scorecards(writes).await?;
    for (component, grade, was, model) in announced {
        let payload = json!({
            "model": model.id,
            "model_name": model.name,
            "owner": model.owner,
            "component": component.reference(),
            "kind": component.kind.id(),
            "name": component.name,
            "grade": grade.value(),
            "was": was,
            "url": format!("/p/maturity/models/{}", model.id),
        });
        let slipped = was.is_some_and(|was| was > i64::from(grade.value()));
        for topic in std::iter::once(GRADED).chain(slipped.then_some(SLIPPED)) {
            if let Err(err) = backend.publish(topic, payload.clone()).await {
                tracing::warn!(%err, topic, "a grade was written but not announced");
            }
        }
    }
    Ok(kept)
}

/// The models in an order where each comes after the ones it stands on, so a criterion reading
/// another model's grade reads this run's. Anything left in a knot — which writing a criterion
/// refuses, but an older one or a hand-edited record could hold — goes last in its declared order
/// and reads the grade from the run before, which is the most that can be said about it.
fn in_order(models: Vec<Model>, criteria: &BTreeMap<Uuid, Vec<Criterion>>) -> Vec<Model> {
    let stands_on = |model: &Model| -> BTreeSet<Uuid> {
        criteria
            .get(&model.id)
            .into_iter()
            .flatten()
            .filter_map(|criterion| criterion.check().ok()?.stands_on())
            .collect()
    };
    let (mut waiting, mut ordered, mut placed) = (models, Vec::new(), BTreeSet::new());
    while !waiting.is_empty() {
        let ready: Vec<Model> = waiting
            .iter()
            .filter(|model| {
                stands_on(model).iter().all(|on| {
                    placed.contains(on) || {
                        // One that is not being scored at all cannot be waited for.
                        !waiting.iter().any(|other| other.id == *on)
                    }
                })
            })
            .cloned()
            .collect();
        // A knot: nothing else can be placed, so the rest go as they came.
        let ready = match ready.is_empty() {
            true => waiting.clone(),
            false => ready,
        };
        for model in ready {
            placed.insert(model.id);
            waiting.retain(|other| other.id != model.id);
            ordered.push(model);
        }
    }
    ordered
}

/// Every model, scored. `only` scores one, which is what a change to a model does.
pub async fn run(backend: &Backend, only: Option<Uuid>) -> Result<Scored, PluginError> {
    let store = Store(backend);
    let wanted: Vec<Model> = store
        .models()
        .await?
        .into_iter()
        .filter(|model| model.enabled && !model.kinds().is_empty())
        .filter(|model| only.is_none_or(|wanted| model.id == wanted))
        .collect();
    let mut all: BTreeMap<Uuid, Vec<Criterion>> = BTreeMap::new();
    for model in &wanted {
        all.insert(model.id, store.criteria(Some(model.id)).await?);
    }
    let models = in_order(wanted, &all);
    let mut run = Progress::default();
    // A model scored on its own still needs what it stands on, which is not being scored now: its
    // last grades are read from the scorecards already written.
    if only.is_some() {
        for model in store.models().await? {
            for card in store.scorecards(Some(model.id)).await? {
                run.standing
                    .entry(model.id)
                    .or_default()
                    .insert(card.component.clone(), (card.grade, model.name.clone()));
            }
        }
    }
    let attestations = store.attestations(None).await?;
    for model in &models {
        let criteria = all.get(&model.id).cloned().unwrap_or_default();
        if criteria.is_empty() {
            continue;
        }
        let held: BTreeMap<String, Scorecard> = store
            .scorecards(Some(model.id))
            .await?
            .into_iter()
            .map(|card| (card.component.clone(), card))
            .collect();
        run.scored.models += 1;
        match score_model(backend, &store, model, &criteria, &attestations, &held, &mut run).await {
            Ok(kept) => match store.forget(model.id, &kept).await {
                Ok(gone) => run.scored.forgotten += gone,
                Err(refusal) => run.scored.problems.push(refusal.detail),
            },
            Err(refusal) => {
                run.scored.problems.push(format!("{}: {}", model.name, refusal.detail));
            }
        }
    }
    let scored = run.scored;
    match scored.problems.is_empty() {
        true => tracing::info!(
            models = scored.models,
            components = scored.components,
            moved = scored.moved,
            plugin = ID,
            "maturity scored"
        ),
        // A run that scored nothing because it was refused looks exactly like one with nothing to
        // do, so what went wrong is said here rather than left in the run's output for whoever
        // thinks to look.
        false => tracing::warn!(
            models = scored.models,
            components = scored.components,
            moved = scored.moved,
            problems = %scored.problems.join("; "),
            plugin = ID,
            "maturity scored, but not everything it grades"
        ),
    }
    Ok(scored)
}
