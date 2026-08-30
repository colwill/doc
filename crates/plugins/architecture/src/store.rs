//! What the Architecture map keeps: the components of a service, the claims about how they reach
//! each other, the views people draw, and where each node sits in one (ADR-0017).
//!
//! Layout lives beside the claims rather than inside them, because the two are different kinds of
//! thing: a position cannot be wrong, and a line can (§5).

use doc_plugin_sdk::{Backend, Collection, Declaration, Field, OnDelete, Query};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::model::{Claim, Component, Placement, Refusal, View};

pub const COMPONENTS: &str = "components";
pub const CLAIMS: &str = "claims";
pub const VIEWS: &str = "views";
pub const PLACEMENTS: &str = "placements";

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            COMPONENTS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "service",
                    Field::text().required().describe("The Catalogue's name for its service"),
                )
                .field("name", Field::text().required().max(200.0))
                .field("title", Field::text().max(200.0))
                .field(
                    "role",
                    Field::text()
                        .required()
                        .default(json!("api"))
                        .describe("api, worker, job, queue, database, cache or site"),
                )
                .field("description", Field::text().max(2_000.0))
                .field(
                    "origin",
                    Field::text()
                        .required()
                        .default(json!("drawn"))
                        .describe("`drawn` here, or `repository` where a file declares it"),
                )
                .field("source", Field::text().describe("Where a declared one was read from"))
                .field("created_by", Field::text())
                .unique(&["service", "name"])
                .index(&["service"]),
        )
        .collection(
            CLAIMS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("from", Field::text().required().describe("`component:<service>/<name>`"))
                .field(
                    "to",
                    Field::text().required().describe(
                        "A component, an `external:<name>` outside the context, or an \
                         `account:<name>` the vendor proxy holds",
                    ),
                )
                .field(
                    "relationship",
                    Field::text()
                        .required()
                        .describe("calls, reads, writes, publishes or subscribes"),
                )
                .field("description", Field::text().max(2_000.0))
                // Which side of each end the line was drawn from and to, so a map keeps the shape
                // somebody gave it. Empty means the middle, which is where a line goes until
                // anybody says otherwise.
                .field("from_side", Field::text())
                .field("to_side", Field::text())
                // Whether it holds both ways, so its line has an arrow at each end.
                .field("both", Field::boolean().required().default(json!(false)))
                .field("origin", Field::text().required().default(json!("drawn")))
                .field("source", Field::text())
                .field("created_by", Field::text())
                .unique(&["from", "to", "relationship"])
                .index(&["from"])
                .index(&["to"]),
        )
        .collection(
            VIEWS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("name", Field::text().required().max(200.0))
                .field(
                    "owner",
                    Field::text().required().describe(
                        "Who may change it: `team:<name>`, `service:<name>` or \
                         `organisation:<name>`",
                    ),
                )
                .field("owner_label", Field::text())
                .field("description", Field::text().max(2_000.0))
                // A view used to declare the services in frame. What is on it says that now, so
                // this is dead — but a field has to ship deprecated before it may be dropped, or
                // core refuses the declaration outright, so it stays here for one version.
                .field("services", Field::json().deprecated())
                // The node the canvas centres on when the view opens. Empty means its top left.
                .field("primary", Field::text())
                .field("created_by", Field::text())
                .index(&["owner"]),
        )
        .collection(
            PLACEMENTS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("view", Field::reference(VIEWS).required().on_delete(OnDelete::Cascade))
                .field("node", Field::text().required())
                .field("column", Field::integer().required().default(json!(0)))
                .field("row", Field::integer().required().default(json!(0)))
                // Where it sits on the canvas, snapped to a grid step. `column` and `row` stay
                // for the server-rendered view, which has no canvas to position against and
                // orders things within a column instead (ADR-0017 §8).
                .field("x", Field::integer().required().default(json!(0)))
                .field("y", Field::integer().required().default(json!(0)))
                // How big it was drawn, in the same grid steps. Zero means the stylesheet decides,
                // which is what everything starts at.
                .field("w", Field::integer().required().default(json!(0)))
                .field("h", Field::integer().required().default(json!(0)))
                // How it looks on this view: the name it goes by here, a description under it,
                // and its border's colour and style. Empty is the usual, so a node nobody has
                // dressed keeps following its own name.
                .field("label", Field::text().max(200.0))
                .field("description", Field::text().max(2_000.0))
                .field("colour", Field::text())
                .field("border", Field::text())
                // Another view this node opens into. A view that goes leaves the box as it was
                // with nothing to open, rather than taking the box with it.
                .field("opens", Field::reference(VIEWS).on_delete(OnDelete::Null))
                .field("collapsed", Field::boolean().required().default(json!(false)))
                .unique(&["view", "node"])
                .index(&["view"]),
        )
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    pub async fn insert<T: DeserializeOwned>(
        &self,
        collection: &str,
        values: Value,
    ) -> Result<T, Refusal> {
        Ok(self.0.insert(collection, values).await?)
    }

    /// An update of a record that is gone is a refusal rather than a silence: whoever asked was
    /// looking at it a moment ago and should be told it went.
    pub async fn update<T: DeserializeOwned>(
        &self,
        collection: &str,
        id: Uuid,
        values: Value,
    ) -> Result<T, Refusal> {
        self.0
            .update(collection, id.to_string(), values, None)
            .await?
            .ok_or_else(|| Refusal::missing("that is no longer there"))
    }

    pub async fn delete(&self, collection: &str, id: Uuid) -> Result<(), Refusal> {
        self.0.delete(collection, json!(id), None).await?;
        Ok(())
    }

    pub async fn views(&self) -> Result<Vec<View>, Refusal> {
        Ok(self.0.query_all(Query::new(VIEWS)).await?)
    }

    pub async fn view(&self, id: Uuid) -> Result<View, Refusal> {
        self.0
            .get(VIEWS, id.to_string())
            .await?
            .ok_or_else(|| Refusal::missing("there is no such view"))
    }

    pub async fn components(&self, service: Option<&str>) -> Result<Vec<Component>, Refusal> {
        let query = match service {
            Some(service) => Query::new(COMPONENTS).filter(json!({ "service": service })),
            None => Query::new(COMPONENTS),
        };
        Ok(self.0.query_all(query).await?)
    }

    pub async fn component(&self, id: Uuid) -> Result<Component, Refusal> {
        self.0
            .get(COMPONENTS, id.to_string())
            .await?
            .ok_or_else(|| Refusal::missing("there is no such component"))
    }

    pub async fn claims(&self) -> Result<Vec<Claim>, Refusal> {
        Ok(self.0.query_all(Query::new(CLAIMS)).await?)
    }

    pub async fn claim(&self, id: Uuid) -> Result<Claim, Refusal> {
        self.0
            .get(CLAIMS, id.to_string())
            .await?
            .ok_or_else(|| Refusal::missing("there is no such relationship"))
    }

    pub async fn placements(&self, view: Uuid) -> Result<Vec<Placement>, Refusal> {
        Ok(self.0.query_all(Query::new(PLACEMENTS).filter(json!({ "view": view }))).await?)
    }
}
