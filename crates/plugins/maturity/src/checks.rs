//! Deciding a criterion for a component.
//!
//! Every check is answered from something the platform already holds, so nobody has to feed this
//! plugin: the Catalogue says what a thing is connected to, who owns it and what its metadata
//! says, and the plugins that measure a service answer `api/readiness` about it (DOC-SPEC §9.2).
//!
//! Nothing here guesses. A check that cannot be answered — a plugin that is not running, a
//! Catalogue that will not say — comes back `unknown` rather than met or unmet, and a scorecard
//! shows it as such. A grade built on a failed read would be worse than no grade.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use doc_plugin_sdk::Backend;
use serde_json::Value;
use uuid::Uuid;

use crate::model::{BEST, Check, Component, Kind, Result_, WORST};
use crate::store::{Attestation, Criterion};

/// How long a plugin is given to answer about readiness, as the roadmap gives them.
const ASKING: Duration = Duration::from_secs(5);

/// What the Catalogue says about one component, read once and used by every criterion about it.
#[derive(Debug, Clone, Default)]
pub struct Known {
    /// Its metadata, as the Catalogue holds it.
    pub metadata: BTreeMap<String, Value>,
    /// The team that owns it, where the Catalogue names one.
    pub owner: Option<String>,
    /// What it is connected to, by kind as a URL names it.
    pub connected: BTreeMap<String, usize>,
    /// Why nothing could be read, where that is what happened.
    pub problem: Option<String>,
}

pub fn encoded(pairs: &[(&str, &str)]) -> String {
    let mut out = url::form_urlencoded::Serializer::new(String::new());
    for (key, value) in pairs {
        out.append_pair(key, value);
    }
    out.finish()
}

/// Everything the Catalogue knows about a component: its own record and what it is connected to,
/// which its detail route answers in one call. Asked as whoever the scoring is being done for, so
/// nobody is graded on what they could not see.
pub async fn known(backend: &Backend, component: &Component) -> Known {
    let path = format!("resources/{}/{}", component.kind.id(), component.name);
    let mut known = Known::default();
    let body = match backend.ask("resources", "GET", &path, None, None).await {
        Ok((200, body)) => body,
        Ok((404, _)) => {
            known.problem = Some("the Catalogue has no record of it".to_string());
            return known;
        }
        Ok((status, body)) => {
            known.problem = Some(format!(
                "the Catalogue answered {status}: {}",
                body["detail"].as_str().unwrap_or("it refused")
            ));
            return known;
        }
        Err(err) => {
            known.problem = Some(format!("the Catalogue could not be asked: {err}"));
            return known;
        }
    };
    let resource = &body["resource"];
    known.metadata = resource["metadata"]
        .as_object()
        .map(|metadata| metadata.iter().map(|(key, value)| (key.clone(), value.clone())).collect())
        .unwrap_or_default();
    known.owner = resource["owner"].as_str().filter(|owner| !owner.is_empty()).map(str::to_string);
    for connection in body["connections"].as_array().into_iter().flatten() {
        let Some(kind) = connection["kind"].as_str() else { continue };
        *known.connected.entry(slug(kind)).or_default() += 1;
    }
    known
}

/// A Catalogue kind as a URL names it: `CloudResource` is `cloud-resource`.
fn slug(kind: &str) -> String {
    Kind::parse(kind).map(|kind| kind.id().to_string()).unwrap_or_else(|| {
        kind.chars()
            .enumerate()
            .flat_map(|(at, c)| {
                let dash = c.is_ascii_uppercase() && at > 0;
                dash.then_some('-').into_iter().chain(std::iter::once(c.to_ascii_lowercase()))
            })
            .collect()
    })
}

/// What one plugin says about the services asked about, as the roadmap asks it.
async fn readiness(
    backend: &Backend,
    plugin: &str,
    services: &BTreeSet<String>,
) -> BTreeMap<String, (String, String)> {
    let mut said = BTreeMap::new();
    if services.is_empty() {
        return said;
    }
    let query: String = services
        .iter()
        .fold(url::form_urlencoded::Serializer::new(String::new()), |mut query, service| {
            query.append_pair("service", service);
            query
        })
        .finish();
    let asked =
        tokio::time::timeout(ASKING, backend.ask(plugin, "GET", "readiness", Some(&query), None));
    let Ok(Ok((200, body))) = asked.await else { return said };
    for (service, answer) in body["services"].as_object().into_iter().flatten() {
        let state = answer["state"].as_str().unwrap_or("unknown").to_string();
        let summary = answer["summary"].as_str().unwrap_or_default().to_string();
        said.insert(service.clone(), (state, summary));
    }
    said
}

/// Everything the criteria need that is not in the Catalogue: what each readiness plugin says
/// about each service. Read once for the whole run rather than per component.
pub async fn readiness_for(
    backend: &Backend,
    criteria: &[Criterion],
    services: &BTreeSet<String>,
) -> BTreeMap<String, BTreeMap<String, (String, String)>> {
    let plugins: BTreeSet<String> = criteria
        .iter()
        .filter_map(|criterion| match criterion.check().ok()? {
            Check::Readiness { plugin, .. } => Some(plugin),
            _ => None,
        })
        .collect();
    let mut by_plugin = BTreeMap::new();
    for plugin in plugins {
        let said = readiness(backend, &plugin, services).await;
        by_plugin.insert(plugin, said);
    }
    by_plugin
}

/// One criterion, decided.
/// What the models scored before this one came to, by model and then by component: what a
/// criterion standing on another model reads. A model is always scored after the ones it stands
/// on, so this is the grade from this same run.
pub type Standing = BTreeMap<Uuid, BTreeMap<String, (i64, String)>>;

pub fn decide(
    criterion: &Criterion,
    component: &Component,
    known: &Known,
    attested: Option<&Attestation>,
    readiness: &BTreeMap<String, BTreeMap<String, (String, String)>>,
    standing: &Standing,
) -> Result_ {
    let mut result = Result_ {
        criterion: criterion.id.to_string(),
        title: criterion.title.clone(),
        weight: criterion.weight(),
        met: false,
        why: String::new(),
        unknown: false,
    };
    let check = match criterion.check() {
        Ok(check) => check,
        Err(why) => {
            result.unknown = true;
            result.why = why;
            return result;
        }
    };
    // Nothing about the component could be read, so nothing about it can be judged either.
    if let Some(problem) = &known.problem
        && check.automatic()
    {
        result.unknown = true;
        result.why = problem.clone();
        return result;
    }
    match check {
        Check::Manual => match attested {
            Some(attestation) => {
                result.met = attestation.met;
                let who = attestation.who.as_deref().unwrap_or("somebody");
                let when = attestation
                    .at
                    .map(|at| at.format("%-d %b %Y").to_string())
                    .unwrap_or_else(|| "at some point".to_string());
                let said = match attestation.note.trim() {
                    "" => String::new(),
                    note => format!(": {note}"),
                };
                result.why = match attestation.met {
                    true => format!("{who} attested to it on {when}{said}"),
                    false => format!("{who} said it is not met on {when}{said}"),
                };
            }
            None => {
                result.unknown = true;
                result.why = "Nobody has said either way".to_string();
            }
        },
        Check::Owned => {
            result.met = known.owner.is_some();
            result.why = match &known.owner {
                Some(owner) => format!("Owned by {owner}"),
                None => "The Catalogue names no owning team".to_string(),
            };
        }
        Check::Connected { ref kind, least } => {
            let wanted = Kind::parse(kind)
                .map(|kind| kind.id().to_string())
                .unwrap_or_else(|| kind.trim().to_ascii_lowercase());
            let found = known.connected.get(&wanted).copied().unwrap_or(0);
            let least = least.max(1) as usize;
            result.met = found >= least;
            let named = Kind::parse(&wanted).map_or(wanted.clone(), |kind| match least == 1 {
                true => kind.one().to_string(),
                false => kind.many().to_lowercase(),
            });
            result.why = match (found, least) {
                (0, 1) => format!("It is connected to no {named}"),
                (0, _) => format!("It is connected to no {named}"),
                (found, least) if found >= least => format!("Connected to {found}"),
                (found, least) => format!("Connected to {found}, and {least} are asked for"),
            };
        }
        Check::Metadata { ref key, ref one_of } => {
            let held = known.metadata.get(key);
            let text = held.map(text_of);
            result.met = match (&text, one_of.is_empty()) {
                (Some(value), true) => !value.trim().is_empty(),
                (Some(value), false) => one_of.iter().any(|wanted| wanted == value),
                (None, _) => false,
            };
            result.why = match text {
                Some(value) if value.trim().is_empty() => format!("`{key}` is set to nothing"),
                Some(value) => format!("`{key}` is `{value}`"),
                None => format!("`{key}` is not set on it in the Catalogue"),
            };
        }
        Check::Model { model, least } => {
            let least = i64::from(least.clamp(WORST, BEST));
            match standing.get(&model).and_then(|cards| cards.get(&component.reference())) {
                Some((grade, name)) => {
                    result.met = *grade >= least;
                    result.why = match (*grade >= least, least >= i64::from(BEST)) {
                        (true, true) => format!("It meets {name} in full"),
                        (true, false) => format!("{name} grades it {grade}"),
                        (false, true) => format!("{name} grades it {grade}, not the full 10"),
                        (false, false) => format!("{name} grades it {grade}, and {least} is asked"),
                    };
                }
                // Nothing is known either way: the other model may not grade this kind of thing,
                // or may have been deleted. Counting that against the grade would be a guess.
                None => {
                    result.unknown = true;
                    result.why =
                        "The model this stands on does not grade it, or has gone".to_string();
                }
            }
        }
        Check::Readiness { ref plugin, allow_warning } => {
            let said = readiness.get(plugin).and_then(|by_service| by_service.get(&component.name));
            match said {
                Some((state, summary)) if state == "unknown" => {
                    result.unknown = true;
                    result.why = match summary.trim() {
                        "" => format!("{plugin} has nothing to say about it"),
                        summary => format!("{plugin}: {summary}"),
                    };
                }
                Some((state, summary)) => {
                    result.met = state == "ready" || (allow_warning && state == "warning");
                    result.why = match summary.trim() {
                        "" => format!("{plugin} says {state}"),
                        summary => format!("{plugin} says {state}: {summary}"),
                    };
                }
                None => {
                    result.unknown = true;
                    result.why = format!("{plugin} did not answer; it may not be running");
                }
            }
        }
    }
    result
}

/// A metadata value as a criterion compares it: a string as it is, anything else as JSON writes it.
fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}
