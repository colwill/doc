//! The pages: the models an organisation and its teams keep, what each one grades and how far,
//! the criteria behind a grade, and a panel on whatever a model grades so the grade is where the
//! thing is rather than only here.

use std::collections::BTreeMap;

use askama::Template;
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::api;
use crate::model::{Check, Component, Grade, Kind, Owner};
use crate::store::{Criterion, Model, Scorecard, Store};
use crate::{Refusal, parameter};

type Form = Vec<(String, String)>;

fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> Option<String> {
    form.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn fields(form: &Form, name: &str) -> Vec<String> {
    form.iter()
        .filter(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    fn refused(refusal: &Refusal) -> Self {
        Self { notice: None, error: Some(refusal.detail.clone()) }
    }
}

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

/// One component's grade against one model, as a row.
pub struct CardRow {
    pub name: String,
    pub kind: &'static str,
    pub href: String,
    pub grade: u8,
    pub grade_class: String,
    pub word: &'static str,
    pub met: i64,
    pub asked: i64,
    pub short: Vec<String>,
    /// How it last moved, in words; empty where it has not.
    pub moved: String,
}

/// A model, as a card on the front page.
pub struct ModelRow {
    pub name: String,
    pub description: String,
    pub owner: String,
    pub owner_href: String,
    pub grades: String,
    pub criteria: usize,
    pub components: usize,
    pub average: Option<u8>,
    pub average_class: String,
    pub enabled: bool,
    pub href: String,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    flash: Flash,
    writes: bool,
    /// Who scoring reads the Catalogue as when nobody started it, and nothing where nobody has
    /// said: with nobody, the schedule scores nothing at all, so the page says so.
    reading_as: Option<String>,
    models: Vec<ModelRow>,
    /// Every grade in use, worst first, so somebody can see the shape of the estate at a glance.
    spread: Vec<(u8, String, usize)>,
    graded: usize,
}

pub struct CriterionRow {
    pub id: String,
    pub title: String,
    pub description: String,
    pub kind: &'static str,
    pub weight: u32,
    pub about: String,
    pub automatic: bool,
    pub problem: Option<String>,
}

#[derive(Template)]
#[template(path = "model.html")]
struct ModelPage {
    flash: Flash,
    writes: bool,
    id: String,
    name: String,
    description: String,
    owner: String,
    owner_href: String,
    grades: String,
    enabled: bool,
    criteria: Vec<CriterionRow>,
    cards: Vec<CardRow>,
    scored: Option<String>,
    spread: Vec<(u8, String, usize)>,
}

pub struct Choice {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

#[derive(Template)]
#[template(path = "model_form.html")]
struct ModelForm {
    flash: Flash,
    editing: Option<String>,
    name: String,
    description: String,
    owner: String,
    kinds: Vec<Choice>,
}

/// Each free-text field is offered what the platform already knows, as a `datalist`: a handful of
/// words rather than resources to search, so it needs no endpoint and nothing of the browser but
/// the markup. "Kept by" is the exception — it names a resource, so it uses the Catalogue's own
/// picker, narrowed to the two kinds that may keep a model.
///
/// The plugins there are, so a criterion that asks one for readiness offers them rather than
/// leaving somebody to remember the name. The four that answer it today come first, and every
/// other plugin follows, since a plugin added later may answer it too.
async fn plugin_names(backend: &Backend) -> Vec<String> {
    const ANSWER_READINESS: [&str; 4] = ["cicd", "dora", "eol", "reliability"];
    let listed: Vec<serde_json::Map<String, Value>> = backend
        .query_all(doc_plugin_sdk::Query::new("core.plugins").limit(200))
        .await
        .unwrap_or_default();
    let mut ids: Vec<String> = listed
        .iter()
        .filter_map(|plugin| plugin.get("id").and_then(Value::as_str))
        .filter(|id| !ANSWER_READINESS.contains(id))
        .map(str::to_string)
        .collect();
    ids.sort();
    ANSWER_READINESS.into_iter().map(str::to_string).chain(ids).collect()
}

/// The metadata keys already set on the things a model grades, so a criterion about metadata
/// offers what is really there rather than asking somebody to remember how it was spelled.
async fn metadata_keys(backend: &Backend, model: &Model) -> Vec<String> {
    const SAMPLE: usize = 200;
    let mut keys: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for kind in model.kinds() {
        let query = crate::checks::encoded(&[("kind", kind.id()), ("limit", &SAMPLE.to_string())]);
        let Ok((200, Value::Array(listed))) =
            backend.ask("resources", "GET", "resources", Some(&query), None).await
        else {
            continue;
        };
        for resource in listed {
            for key in resource["metadata"].as_object().into_iter().flatten().map(|(key, _)| key) {
                keys.insert(key.clone());
            }
        }
    }
    keys.into_iter().collect()
}

#[derive(Template)]
#[template(path = "criterion_form.html")]
struct CriterionForm {
    flash: Flash,
    /// The metadata keys already in use on what this model grades.
    metadata_keys: Vec<String>,
    /// The plugins a readiness criterion could ask.
    plugins: Vec<String>,
    model: String,
    model_name: String,
    editing: Option<String>,
    title: String,
    description: String,
    weight: i64,
    /// What the criterion is about: only what this model grades, since it grades nothing else.
    kinds: Vec<Choice>,
    /// What a `connected` criterion may ask for a connection to: every kind the Catalogue joins
    /// things by, whatever the model grades. A model that grades only services still asks whether
    /// each one has a repository.
    connected_kinds: Vec<Choice>,
    checks: Vec<Choice>,
    /// The check's own fields, whichever kind it is; which of `connected_kinds` is chosen is
    /// marked on the choice itself.
    connected_least: u32,
    metadata_key: String,
    metadata_one_of: String,
    readiness_plugin: String,
    readiness_allow_warning: bool,
    /// The other models this one could stand on: every model but itself.
    models: Vec<Choice>,
    /// The grade that other model must reach, 10 being all of it.
    model_least: u8,
}

/// What one criterion came to, on a panel or a scorecard.
pub struct ResultRow {
    pub title: String,
    pub why: String,
    pub met: bool,
    pub unknown: bool,
    pub weight: u32,
    pub criterion: String,
    pub manual: bool,
}

/// One model's grade for the thing the panel is on, with what each of its criteria came to.
pub struct PanelCard {
    pub name: String,
    pub href: String,
    pub grade: u8,
    pub grade_class: String,
    pub word: &'static str,
    pub results: Vec<ResultRow>,
}

#[derive(Template)]
#[template(path = "panel.html")]
struct Panel {
    message: Option<String>,
    component: String,
    cards: Vec<PanelCard>,
    writes: bool,
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

/// Every grade in use among these scorecards, worst first.
fn spread(cards: &[Scorecard]) -> Vec<(u8, String, usize)> {
    let mut counted: BTreeMap<u8, usize> = BTreeMap::new();
    for card in cards {
        *counted.entry(card.grade().value()).or_default() += 1;
    }
    counted
        .into_iter()
        .map(|(grade, count)| (grade, Grade::parse(i64::from(grade)).class(), count))
        .collect()
}

fn card_row(card: &Scorecard) -> CardRow {
    let grade = card.grade();
    let component = card.component();
    CardRow {
        name: card.name.clone(),
        kind: component.map(|component| component.kind.one()).unwrap_or("component"),
        href: Component::parse(&card.component)
            .map(|component| component.href())
            .unwrap_or_default(),
        grade: grade.value(),
        grade_class: grade.class(),
        word: grade.word(),
        met: card.met,
        asked: card.asked,
        short: card
            .results
            .iter()
            .filter(|result| !result.met && !result.unknown)
            .map(|result| result.title.clone())
            .take(4)
            .collect(),
        moved: match card.moved() {
            Some(moved) if moved > 0 => format!("Up {moved} since last time"),
            Some(moved) => format!("Down {} since last time", -moved),
            None => String::new(),
        },
    }
}

/// `named` turns a model's id into its name, so a criterion standing on another model reads
/// "It meets Development Ready in full" rather than naming an id nobody knows.
fn criterion_row(criterion: &Criterion, named: &BTreeMap<Uuid, String>) -> CriterionRow {
    let check = criterion.check();
    CriterionRow {
        id: criterion.id.to_string(),
        title: criterion.title.clone(),
        description: criterion.description.clone(),
        kind: criterion.kind().map(Kind::one).unwrap_or("component"),
        weight: criterion.weight(),
        about: check
            .as_ref()
            .map(|check| check.about_with(|id| named.get(&id).cloned()))
            .unwrap_or_default(),
        automatic: check.as_ref().map(Check::automatic).unwrap_or(false),
        problem: check.err(),
    }
}

fn kind_choices(chosen: &[String]) -> Vec<Choice> {
    Kind::ALL
        .into_iter()
        .map(|kind| Choice {
            value: kind.id().to_string(),
            label: kind.many().to_string(),
            selected: chosen.iter().any(|named| named == kind.id()),
        })
        .collect()
}

fn check_choices(chosen: &str) -> Vec<Choice> {
    [
        ("connected", "It is connected to something in the Catalogue"),
        ("owned", "A team owns it"),
        ("metadata", "Its metadata says something"),
        ("readiness", "A plugin that measures services says it is ready"),
        ("model", "It meets another maturity model"),
        ("manual", "Somebody attests to it"),
    ]
    .into_iter()
    .map(|(value, label)| Choice {
        value: value.to_string(),
        label: label.to_string(),
        selected: value == chosen,
    })
    .collect()
}

fn sets(backend: &Backend) -> bool {
    backend.writes()
}

async fn home(backend: &Backend, flash: Flash) -> Result<String, Refusal> {
    let store = Store(backend);
    let models = store.models().await?;
    let criteria = store.criteria(None).await?;
    let cards = store.scorecards(None).await?;
    let rows: Vec<ModelRow> = models
        .iter()
        .map(|model| {
            let theirs: Vec<&Scorecard> =
                cards.iter().filter(|card| card.model == model.id).collect();
            let average = match theirs.is_empty() {
                true => None,
                false => {
                    let total: i64 = theirs.iter().map(|card| card.grade).sum();
                    Some((total as f64 / theirs.len() as f64).round() as u8)
                }
            };
            let owner = model.owner();
            ModelRow {
                name: model.name.clone(),
                description: model.description.clone(),
                owner: owner.as_ref().map(Owner::label).unwrap_or_else(|| model.owner.clone()),
                owner_href: owner.as_ref().map(Owner::href).unwrap_or_default(),
                grades: model
                    .kinds()
                    .iter()
                    .map(|kind| kind.many().to_lowercase())
                    .collect::<Vec<_>>()
                    .join(", "),
                criteria: criteria.iter().filter(|one| one.model == model.id).count(),
                components: theirs.len(),
                average,
                average_class: average
                    .map(|grade| Grade::parse(i64::from(grade)).class())
                    .unwrap_or_default(),
                enabled: model.enabled,
                href: model.href(),
            }
        })
        .collect();
    render(&HomePage {
        flash,
        writes: sets(backend),
        reading_as: crate::leave::reading(backend).await.by,
        models: rows,
        spread: spread(&cards),
        graded: cards.len(),
    })
}

async fn model_page(backend: &Backend, id: Uuid, flash: Flash) -> Result<String, Refusal> {
    let store = Store(backend);
    let model = store.model(id).await?;
    let criteria = store.criteria(Some(id)).await?;
    let cards = store.scorecards(Some(id)).await?;
    let owner = model.owner();
    let scored = cards
        .iter()
        .filter_map(|card| card.scored_at)
        .max()
        .map(|at| at.format("%-d %b %Y, %H:%M UTC").to_string());
    // A criterion standing on another model names it rather than showing its id.
    let named: BTreeMap<Uuid, String> =
        store.models().await?.into_iter().map(|other| (other.id, other.name)).collect();
    render(&ModelPage {
        flash,
        writes: sets(backend),
        id: model.id.to_string(),
        name: model.name.clone(),
        description: model.description.clone(),
        owner: owner.as_ref().map(Owner::label).unwrap_or_else(|| model.owner.clone()),
        owner_href: owner.as_ref().map(Owner::href).unwrap_or_default(),
        grades: model
            .kinds()
            .iter()
            .map(|kind| kind.many().to_lowercase())
            .collect::<Vec<_>>()
            .join(", "),
        enabled: model.enabled,
        criteria: criteria.iter().map(|criterion| criterion_row(criterion, &named)).collect(),
        cards: cards.iter().map(card_row).collect(),
        scored,
        spread: spread(&cards),
    })
}

fn model_form(model: Option<&Model>, flash: Flash) -> Result<String, Refusal> {
    render(&ModelForm {
        flash,
        editing: model.map(|model| model.id.to_string()),
        name: model.map(|model| model.name.clone()).unwrap_or_default(),
        description: model.map(|model| model.description.clone()).unwrap_or_default(),
        owner: model.map(|model| model.owner.clone()).unwrap_or_default(),
        kinds: kind_choices(&model.map(|model| model.kinds.clone()).unwrap_or_default()),
    })
}

async fn criterion_form(
    backend: &Backend,
    model: &Model,
    criterion: Option<&Criterion>,
    flash: Flash,
) -> Result<String, Refusal> {
    // Every other model, for a criterion that stands on one. A model never stands on itself.
    let others: Vec<Model> =
        Store(backend).models().await?.into_iter().filter(|other| other.id != model.id).collect();
    let check = criterion.and_then(|criterion| criterion.check().ok()).unwrap_or_default();
    let (connected_kind, connected_least) = match &check {
        Check::Connected { kind, least } => (kind.clone(), *least),
        _ => (Kind::Repository.id().to_string(), 1),
    };
    let (metadata_key, metadata_one_of) = match &check {
        Check::Metadata { key, one_of } => (key.clone(), one_of.join(", ")),
        _ => (String::new(), String::new()),
    };
    let (stands_on, model_least) = match &check {
        Check::Model { model, least } => (Some(*model), *least),
        _ => (None, crate::model::BEST),
    };
    let (readiness_plugin, readiness_allow_warning) = match &check {
        Check::Readiness { plugin, allow_warning } => (plugin.clone(), *allow_warning),
        _ => (String::new(), false),
    };
    render(&CriterionForm {
        flash,
        metadata_keys: metadata_keys(backend, model).await,
        plugins: plugin_names(backend).await,
        model: model.id.to_string(),
        model_name: model.name.clone(),
        editing: criterion.map(|criterion| criterion.id.to_string()),
        title: criterion.map(|criterion| criterion.title.clone()).unwrap_or_default(),
        description: criterion.map(|criterion| criterion.description.clone()).unwrap_or_default(),
        weight: criterion.map_or(1, |criterion| criterion.weight),
        kinds: Kind::ALL
            .into_iter()
            .filter(|kind| model.kinds().contains(kind))
            .map(|kind| Choice {
                value: kind.id().to_string(),
                label: kind.many().to_string(),
                selected: criterion.and_then(Criterion::kind) == Some(kind),
            })
            .collect(),
        connected_kinds: Kind::ALL
            .into_iter()
            .map(|kind| Choice {
                value: kind.id().to_string(),
                label: kind.many().to_string(),
                selected: kind.id() == connected_kind,
            })
            .collect(),
        checks: check_choices(check.id()),
        connected_least,
        metadata_key,
        metadata_one_of,
        readiness_plugin,
        readiness_allow_warning,
        models: others
            .into_iter()
            .map(|other| Choice {
                value: other.id.to_string(),
                label: other.name,
                selected: stands_on == Some(other.id),
            })
            .collect(),
        model_least,
    })
}

/// The check a form describes, put together from the fields the chosen kind uses.
fn check_of(form: &Form) -> Value {
    match field(form, "check").unwrap_or_default().as_str() {
        "connected" => json!({
            "check": "connected",
            "kind": field(form, "connected_kind").unwrap_or_default(),
            "least": field(form, "connected_least")
                .and_then(|least| least.parse::<u32>().ok())
                .unwrap_or(1)
                .max(1),
        }),
        "owned" => json!({ "check": "owned" }),
        "metadata" => json!({
            "check": "metadata",
            "key": field(form, "metadata_key").unwrap_or_default(),
            "one_of": field(form, "metadata_one_of")
                .map(|listed| {
                    listed
                        .split(',')
                        .map(|value| value.trim().to_string())
                        .filter(|value| !value.is_empty())
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        }),
        "model" => json!({
            "check": "model",
            "model": field(form, "stands_on").unwrap_or_default(),
            "least": field(form, "model_least")
                .and_then(|least| least.parse::<u8>().ok())
                .unwrap_or(crate::model::BEST)
                .clamp(crate::model::WORST, crate::model::BEST),
        }),
        "readiness" => json!({
            "check": "readiness",
            "plugin": field(form, "readiness_plugin").unwrap_or_default(),
            "allow_warning": field(form, "readiness_allow_warning").is_some(),
        }),
        _ => json!({ "check": "manual" }),
    }
}

#[derive(Template)]
#[template(path = "insight.html")]
struct InsightTile {
    label: String,
    value: Option<String>,
    /// The medal's colour for that grade, `doc-grade--1` to `--10`.
    grade_class: String,
    note: String,
    href: String,
}

/// **Maturity level**, for pinning to the top of a resource's page: the grade of the model that
/// grades it, or the mean where more than one does, since a component held to two models is as
/// mature as both of them together say it is.
async fn insight(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let mut tile = InsightTile {
        label: "Maturity level".into(),
        value: None,
        grade_class: String::new(),
        note: "Nothing grades it yet".into(),
        href: "/p/maturity/".into(),
    };
    let Some(reference) = parameter(&request.query, "resource") else {
        return render(&tile);
    };
    let Some(component) = Component::parse(&reference) else {
        return render(&tile);
    };
    let cards = Store(backend).of_component(&component.reference()).await?;
    if cards.is_empty() {
        return render(&tile);
    }
    let total: i64 = cards.iter().map(|card| card.grade).sum();
    let grade = Grade::parse(total / cards.len() as i64);
    tile.value = Some(grade.value().to_string());
    tile.grade_class = grade.class();
    tile.note = match cards.len() {
        1 => grade.word().to_string(),
        many => format!("{}, across {many} models", grade.word()),
    };
    tile.href = format!("/p/resources/r/{}/{}", component.kind.id(), component.name);
    render(&tile)
}

async fn panel(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let store = Store(backend);
    let Some(reference) = parameter(&request.query, "resource") else {
        return render(&Panel {
            message: Some("Nothing was named to show the maturity of.".into()),
            component: String::new(),
            cards: Vec::new(),
            writes: false,
        });
    };
    let Some(component) = Component::parse(&reference) else {
        return render(&Panel {
            message: Some("No maturity model grades this kind of thing.".into()),
            component: reference,
            cards: Vec::new(),
            writes: false,
        });
    };
    let cards = store.of_component(&component.reference()).await?;
    if cards.is_empty() {
        return render(&Panel {
            message: Some(format!("No maturity model grades this {} yet.", component.kind.one())),
            component: component.reference(),
            cards: Vec::new(),
            writes: false,
        });
    }
    let models: BTreeMap<Uuid, Model> =
        store.models().await?.into_iter().map(|model| (model.id, model)).collect();
    // Which criteria only a person can decide, so the panel can offer to attest to them here
    // rather than sending somebody to the model's page to do it.
    let mut manual: BTreeMap<String, bool> = BTreeMap::new();
    for card in &cards {
        for criterion in store.criteria(Some(card.model)).await? {
            manual.insert(
                criterion.id.to_string(),
                !criterion.check().map(|check| check.automatic()).unwrap_or(true),
            );
        }
    }
    let shown = cards
        .iter()
        .map(|card| {
            let grade = card.grade();
            let name = models
                .get(&card.model)
                .map(|model| model.name.clone())
                .unwrap_or_else(|| "A maturity model".to_string());
            let rows: Vec<ResultRow> = card
                .results
                .iter()
                .map(|result| ResultRow {
                    title: result.title.clone(),
                    why: result.why.clone(),
                    met: result.met,
                    unknown: result.unknown,
                    weight: result.weight,
                    manual: manual.get(&result.criterion).copied().unwrap_or(false),
                    criterion: result.criterion.clone(),
                })
                .collect();
            PanelCard {
                name,
                href: format!("/p/maturity/models/{}", card.model),
                grade: grade.value(),
                grade_class: grade.class(),
                word: grade.word(),
                results: rows,
            }
        })
        .collect();
    render(&Panel {
        message: None,
        component: component.reference(),
        cards: shown,
        writes: sets(backend),
    })
}

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    let path = request.path.trim_start_matches("ui").trim_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
    let page = route(backend, request, &segments).await;
    match page {
        Ok(html) => Response::html(html),
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(&refusal) };
            let html = page.render().unwrap_or_else(|_| refusal.detail.clone());
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

fn writer(backend: &Backend) -> Result<(), Refusal> {
    match backend.writes() {
        true => Ok(()),
        false => Err(Refusal::forbidden("that needs plugin:maturity:user:rw")),
    }
}

async fn route(backend: &Backend, request: &Request, path: &[&str]) -> Result<String, Refusal> {
    let store = Store(backend);
    let id = |text: &str| -> Result<Uuid, Refusal> {
        text.parse().map_err(|_| Refusal::bad("that is not an ID"))
    };
    match (request.method.as_str(), path) {
        ("GET", []) => home(backend, Flash::default()).await,
        ("GET", ["panel"]) => panel(backend, request).await,
        ("GET", ["insight"]) => insight(backend, request).await,
        ("GET", ["models", "new"]) => {
            writer(backend)?;
            model_form(None, Flash::default())
        }
        ("POST", ["models", "new"]) => {
            writer(backend)?;
            let form = form(request);
            let values = api::checked_model(
                &field(&form, "name").unwrap_or_default(),
                &field(&form, "owner").unwrap_or_default(),
                &fields(&form, "kinds"),
            );
            match values {
                Err(refusal) => model_form(None, Flash::refused(&refusal)),
                Ok(mut values) => {
                    values["id"] = json!(Uuid::now_v7());
                    values["description"] = json!(field(&form, "description").unwrap_or_default());
                    values["created_by"] = json!(
                        backend
                            .caller()
                            .and_then(|caller| caller.label.clone())
                            .unwrap_or_default()
                    );
                    let model = store.save_model(values).await?;
                    api::rescore(backend, model.id).await;
                    model_page(
                        backend,
                        model.id,
                        Flash::done(format!(
                            "{} is written. Add the criteria it grades by.",
                            model.name
                        )),
                    )
                    .await
                }
            }
        }
        ("GET", ["models", model]) => model_page(backend, id(model)?, Flash::default()).await,
        ("GET", ["models", model, "edit"]) => {
            writer(backend)?;
            let model = store.model(id(model)?).await?;
            model_form(Some(&model), Flash::default())
        }
        ("POST", ["models", model, "edit"]) => {
            writer(backend)?;
            let held = store.model(id(model)?).await?;
            let form = form(request);
            let values = api::checked_model(
                &field(&form, "name").unwrap_or_default(),
                &field(&form, "owner").unwrap_or_default(),
                &fields(&form, "kinds"),
            );
            match values {
                Err(refusal) => model_form(Some(&held), Flash::refused(&refusal)),
                Ok(mut values) => {
                    values["id"] = json!(held.id);
                    values["description"] = json!(field(&form, "description").unwrap_or_default());
                    values["enabled"] = json!(field(&form, "enabled").is_some());
                    let model = store.save_model(values).await?;
                    api::rescore(backend, model.id).await;
                    model_page(backend, model.id, Flash::done("Saved, and being scored again."))
                        .await
                }
            }
        }
        ("POST", ["models", model, "delete"]) => {
            writer(backend)?;
            let held = store.model(id(model)?).await?;
            store.delete_model(held.id).await?;
            home(backend, Flash::done(format!("{} and its scorecards are gone.", held.name))).await
        }
        ("POST", ["models", model, "score"]) => {
            writer(backend)?;
            let model = id(model)?;
            api::rescore(backend, model).await;
            model_page(backend, model, Flash::done("Being scored again now.")).await
        }
        ("GET", ["models", model, "criteria", "new"]) => {
            writer(backend)?;
            let model = store.model(id(model)?).await?;
            criterion_form(backend, &model, None, Flash::default()).await
        }
        ("POST", ["models", model, "criteria", "new"]) => {
            writer(backend)?;
            let held = store.model(id(model)?).await?;
            let form = form(request);
            save_criterion(backend, &store, &held, None, &form).await
        }
        ("GET", ["criteria", criterion, "edit"]) => {
            writer(backend)?;
            let criterion = store.criterion(id(criterion)?).await?;
            let model = store.model(criterion.model).await?;
            criterion_form(backend, &model, Some(&criterion), Flash::default()).await
        }
        ("POST", ["criteria", criterion, "edit"]) => {
            writer(backend)?;
            let held = store.criterion(id(criterion)?).await?;
            let model = store.model(held.model).await?;
            let form = form(request);
            save_criterion(backend, &store, &model, Some(&held), &form).await
        }
        ("POST", ["criteria", criterion, "delete"]) => {
            writer(backend)?;
            let held = store.criterion(id(criterion)?).await?;
            store.delete_criterion(held.id).await?;
            api::rescore(backend, held.model).await;
            model_page(backend, held.model, Flash::done(format!("{} is gone.", held.title))).await
        }
        // Attesting to a criterion nothing can decide, from the panel on the thing itself.
        ("POST", ["attest"]) => {
            writer(backend)?;
            let form = form(request);
            let criterion = field(&form, "criterion")
                .ok_or_else(|| Refusal::bad("name the criterion being attested to"))?;
            let component = field(&form, "component")
                .ok_or_else(|| Refusal::bad("name what is being attested to"))?;
            let held = store.criterion(id(&criterion)?).await?;
            store
                .attest(json!({
                    "id": Uuid::now_v7(),
                    "criterion": held.id,
                    "component": component,
                    "met": field(&form, "met").is_some(),
                    "note": field(&form, "note").unwrap_or_default(),
                    "who": backend.caller().and_then(|caller| caller.label.clone()),
                    "at": chrono::Utc::now(),
                }))
                .await?;
            api::rescore(backend, held.model).await;
            let mut request = request.clone();
            request.query = format!("resource={component}");
            panel(backend, &request).await
        }
        _ => Err(Refusal::missing("no such page")),
    }
}

async fn save_criterion(
    backend: &Backend,
    store: &Store<'_>,
    model: &Model,
    held: Option<&Criterion>,
    form: &Form,
) -> Result<String, Refusal> {
    let check = check_of(form);
    let values = api::checked_criterion(
        model.id,
        &field(form, "title").unwrap_or_default(),
        &field(form, "kind").unwrap_or_default(),
        field(form, "weight").and_then(|weight| weight.parse().ok()).unwrap_or(1),
        &check,
    );
    // A model standing on another must not close a circle, which needs the other models read.
    // Only once everything decided without them holds, so the plainer refusals are the ones seen.
    let stands_on = match &values {
        Ok(_) => serde_json::from_value::<Check>(check.clone()).ok().and_then(|c| c.stands_on()),
        Err(_) => None,
    };
    let values = match stands_on {
        Some(on) => match api::would_circle(store, model.id, on).await? {
            Some(why) => Err(Refusal::bad(why)),
            None => values,
        },
        None => values,
    };
    match values {
        Err(refusal) => criterion_form(backend, model, held, Flash::refused(&refusal)).await,
        Ok(mut values) => {
            values["id"] = json!(held.map_or_else(Uuid::now_v7, |held| held.id));
            values["description"] = json!(field(form, "description").unwrap_or_default());
            // New ones go last: the clock only ever moves forward, so nothing has to be renumbered.
            values["position"] =
                json!(held.map_or_else(|| chrono::Utc::now().timestamp(), |held| held.position));
            let criterion = store.save_criterion(values).await?;
            api::rescore(backend, model.id).await;
            model_page(
                backend,
                model.id,
                Flash::done(format!(
                    "{} is saved, and everything is being scored again.",
                    criterion.title
                )),
            )
            .await
        }
    }
}
