//! What the Maturity Model keeps: the models an organisation and its teams have written, the
//! criteria in each, what somebody has attested to by hand, and the last scorecard for every
//! component a model grades.
//!
//! A scorecard is kept rather than worked out on the page, because a grade is only useful if you
//! can see it move: it carries when it was scored and what each criterion came to, so the page
//! can say what changed as well as where things stand.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{
    Backend, Collection, DataRequest, Declaration, Field, ListOf, OnDelete, Order, Query,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::model::{Check, Component, Grade, Kind, Owner, Result_};

pub const MODELS: &str = "models";
pub const CRITERIA: &str = "criteria";
pub const ATTESTATIONS: &str = "attestations";
pub const SCORECARDS: &str = "scorecards";

/// How many of anything one page reads.
const MOST: u32 = 1_000;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            MODELS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("name", Field::text().required())
                .field("description", Field::text().max(1_000.0))
                .field(
                    "owner",
                    Field::text()
                        .required()
                        .describe("Who keeps it: organisation:<name> or team:<name>"),
                )
                .field(
                    "kinds",
                    Field::list(ListOf::Text).required().default(json!([])).describe(
                        "What it grades: service, repository, documentation, cloud-resource",
                    ),
                )
                .field("enabled", Field::boolean().required().default(json!(true)))
                .field("created_by", Field::text())
                .unique(&["owner", "name"])
                .index(&["owner"]),
        )
        .collection(
            CRITERIA,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("model", Field::reference(MODELS).required().on_delete(OnDelete::Cascade))
                .field("title", Field::text().required())
                .field("description", Field::text().max(1_000.0))
                .field("kind", Field::text().required())
                .field(
                    "weight",
                    Field::integer()
                        .required()
                        .default(json!(1))
                        .describe("What it counts for beside the others, 1 to 10"),
                )
                .field("check", Field::json().required().default(json!({ "check": "manual" })))
                .field("position", Field::integer().required().default(json!(0)))
                .index(&["model", "position"]),
        )
        .collection(
            ATTESTATIONS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "criterion",
                    Field::reference(CRITERIA).required().on_delete(OnDelete::Cascade),
                )
                .field("component", Field::text().required())
                .field("met", Field::boolean().required().default(json!(true)))
                .field("note", Field::text().max(1_000.0))
                .field("who", Field::text())
                .field("at", Field::timestamp())
                .unique(&["criterion", "component"])
                .index(&["component"]),
        )
        .collection(
            SCORECARDS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("model", Field::reference(MODELS).required().on_delete(OnDelete::Cascade))
                .field("component", Field::text().required())
                .field("kind", Field::text().required())
                .field("name", Field::text().required())
                .field("grade", Field::integer().required().default(json!(1)))
                .field("met", Field::integer().required().default(json!(0)))
                .field("asked", Field::integer().required().default(json!(0)))
                .field("results", Field::json().required().default(json!([])))
                .field("scored_at", Field::timestamp())
                .field(
                    "was",
                    Field::integer().describe("The grade before this one, where it moved"),
                )
                .unique(&["model", "component"])
                .index(&["model", "grade"])
                .index(&["component"]),
        )
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Model {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub description: String,
    pub owner: String,
    #[serde(default)]
    pub kinds: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub created_by: Option<String>,
}

fn yes() -> bool {
    true
}

impl Model {
    pub fn owner(&self) -> Option<Owner> {
        Owner::parse(&self.owner).ok()
    }

    /// The kinds it grades, in the platform's order and without anything it cannot read.
    pub fn kinds(&self) -> Vec<Kind> {
        Kind::ALL
            .into_iter()
            .filter(|kind| self.kinds.iter().any(|named| named == kind.id()))
            .collect()
    }

    pub fn href(&self) -> String {
        format!("/p/maturity/models/{}", self.id)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Criterion {
    pub id: Uuid,
    pub model: Uuid,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub kind: String,
    #[serde(default = "one")]
    pub weight: i64,
    #[serde(default)]
    pub check: Value,
    #[serde(default)]
    pub position: i64,
}

fn one() -> i64 {
    1
}

impl Criterion {
    pub fn kind(&self) -> Option<Kind> {
        Kind::parse(&self.kind)
    }

    /// How it is decided; a check that cannot be read is treated as one nobody can answer, which
    /// shows on the scorecard rather than silently passing.
    pub fn check(&self) -> Result<Check, String> {
        serde_json::from_value(self.check.clone())
            .map_err(|err| format!("this criterion's check could not be read: {err}"))
    }

    pub fn weight(&self) -> u32 {
        u32::try_from(self.weight.clamp(1, 10)).unwrap_or(1)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Attestation {
    pub id: Uuid,
    pub criterion: Uuid,
    pub component: String,
    #[serde(default)]
    pub met: bool,
    #[serde(default)]
    pub note: String,
    #[serde(default)]
    pub who: Option<String>,
    #[serde(default)]
    pub at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Scorecard {
    pub id: Uuid,
    pub model: Uuid,
    pub component: String,
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub grade: i64,
    #[serde(default)]
    pub met: i64,
    #[serde(default)]
    pub asked: i64,
    #[serde(default)]
    pub results: Vec<Result_>,
    #[serde(default)]
    pub scored_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub was: Option<i64>,
}

impl Scorecard {
    pub fn grade(&self) -> Grade {
        Grade::parse(self.grade)
    }

    pub fn component(&self) -> Option<Component> {
        Component::parse(&self.component)
    }

    /// Which way it last moved, where it moved at all.
    pub fn moved(&self) -> Option<i64> {
        self.was.map(|was| self.grade - was).filter(|moved| *moved != 0)
    }

    pub fn json(&self) -> Value {
        json!({
            "model": self.model,
            "component": self.component,
            "kind": self.kind,
            "name": self.name,
            "grade": self.grade,
            "met": self.met,
            "asked": self.asked,
            "scored_at": self.scored_at,
            "moved": self.moved(),
            "criteria": self.results.iter().map(Result_::json).collect::<Vec<_>>(),
        })
    }
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    async fn all<T: serde::de::DeserializeOwned>(&self, query: Query) -> Result<Vec<T>, Refusal> {
        Ok(self.0.query_all(query.limit(MOST)).await?)
    }

    pub async fn models(&self) -> Result<Vec<Model>, Refusal> {
        let mut models: Vec<Model> =
            self.all(Query::new(MODELS).order(Order::asc("owner"))).await?;
        // The organisation's first: it is the baseline every team's adds to.
        models.sort_by(|a, b| {
            let rank = |model: &Model| {
                (!model.owner().is_some_and(|owner| owner.is_organisation()), model.name.clone())
            };
            rank(a).cmp(&rank(b))
        });
        Ok(models)
    }

    pub async fn model(&self, id: Uuid) -> Result<Model, Refusal> {
        self.0
            .get(MODELS, id.to_string())
            .await?
            .ok_or_else(|| Refusal::missing("there is no such maturity model"))
    }

    pub async fn save_model(&self, values: Value) -> Result<Model, Refusal> {
        let (saved, _): (Map<String, Value>, bool) = self.0.upsert(MODELS, &["id"], values).await?;
        read(saved)
    }

    pub async fn delete_model(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete(MODELS, id.to_string(), None).await?;
        Ok(())
    }

    pub async fn criteria(&self, model: Option<Uuid>) -> Result<Vec<Criterion>, Refusal> {
        let query = match model {
            Some(model) => Query::new(CRITERIA)
                .filter(json!({ "model": model }))
                .order(Order::asc("model"))
                .order(Order::asc("position")),
            None => Query::new(CRITERIA).order(Order::asc("model")).order(Order::asc("position")),
        };
        self.all(query).await
    }

    pub async fn criterion(&self, id: Uuid) -> Result<Criterion, Refusal> {
        self.0
            .get(CRITERIA, id.to_string())
            .await?
            .ok_or_else(|| Refusal::missing("there is no such criterion"))
    }

    pub async fn save_criterion(&self, values: Value) -> Result<Criterion, Refusal> {
        let (saved, _): (Map<String, Value>, bool) =
            self.0.upsert(CRITERIA, &["id"], values).await?;
        read(saved)
    }

    pub async fn delete_criterion(&self, id: Uuid) -> Result<(), Refusal> {
        self.0.delete(CRITERIA, id.to_string(), None).await?;
        Ok(())
    }

    /// What has been attested by hand, for the components asked about.
    pub async fn attestations(&self, component: Option<&str>) -> Result<Vec<Attestation>, Refusal> {
        let query = match component {
            Some(component) => Query::new(ATTESTATIONS).filter(json!({ "component": component })),
            None => Query::new(ATTESTATIONS),
        };
        self.all(query).await
    }

    pub async fn attest(&self, values: Value) -> Result<Attestation, Refusal> {
        let (saved, _): (Map<String, Value>, bool) =
            self.0.upsert(ATTESTATIONS, &["criterion", "component"], values).await?;
        read(saved)
    }

    pub async fn scorecards(&self, model: Option<Uuid>) -> Result<Vec<Scorecard>, Refusal> {
        let query = match model {
            Some(model) => Query::new(SCORECARDS)
                .filter(json!({ "model": model }))
                .order(Order::asc("model"))
                .order(Order::asc("grade")),
            None => Query::new(SCORECARDS).order(Order::asc("model")).order(Order::asc("grade")),
        };
        self.all(query).await
    }

    /// Every scorecard for one component, whichever model wrote it.
    pub async fn of_component(&self, component: &str) -> Result<Vec<Scorecard>, Refusal> {
        self.all(Query::new(SCORECARDS).filter(json!({ "component": component }))).await
    }

    pub async fn write_scorecards(&self, writes: Vec<DataRequest>) -> Result<(), Refusal> {
        for batch in writes.chunks(100) {
            self.0.batch(batch.to_vec()).await?;
        }
        Ok(())
    }

    /// Scorecards for components a model no longer grades, which are removed rather than left to
    /// say something that stopped being true.
    pub async fn forget(&self, model: Uuid, keeping: &[String]) -> Result<usize, Refusal> {
        let held = self.scorecards(Some(model)).await?;
        let gone: Vec<DataRequest> = held
            .iter()
            .filter(|card| !keeping.iter().any(|kept| kept == &card.component))
            .map(|card| DataRequest::delete(SCORECARDS, card.id.to_string()))
            .collect();
        let count = gone.len();
        self.write_scorecards(gone).await?;
        Ok(count)
    }
}

fn read<T: serde::de::DeserializeOwned>(record: Map<String, Value>) -> Result<T, Refusal> {
    serde_json::from_value(Value::Object(record))
        .map_err(|err| Refusal::unavailable(format!("a stored record could not be read: {err}")))
}
