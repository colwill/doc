//! What the plugin keeps: templates, and each request with the resource it became, its status,
//! what the vendor said about it and when it expires.

use doc_plugin_sdk::{Backend, Collection, Declaration, Field, ListOf, Order, PluginError, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;

/// The statuses of a request that still holds, or is about to hold, a resource.
pub const LIVE: [&str; 5] = ["pending", "provisioning", "active", "expiring", "deleting"];
const ATTEMPTS: usize = 5;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "templates",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("name", Field::text().required())
                .field("title", Field::text().required())
                .field("description", Field::text().required().default(json!("")))
                .field(
                    "vendor",
                    Field::text().required().one_of(&["linode", "aws", "gcp", "azure"]),
                )
                .field("resource_type", Field::text().required())
                .field("regions", Field::list(ListOf::Text).required().default(json!([])))
                .field("sizes", Field::list(ListOf::Text).required().default(json!([])))
                .field("default_region", Field::text().required())
                .field("default_size", Field::text())
                .field("default_lifetime_minutes", Field::integer().required())
                .field("max_lifetime_minutes", Field::integer().required())
                .field("team_quota", Field::integer().required())
                .field("created_by", Field::text().required())
                .unique(&["name"])
                .index(&["vendor", "name"]),
        )
        .collection(
            "requests",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("template", Field::reference("templates").required())
                .field("vendor", Field::text().required())
                .field("resource_type", Field::text().required())
                .field("name", Field::text().required())
                .field("region", Field::text().required())
                .field("size", Field::text())
                .field("team", Field::text().required())
                .field("service", Field::text())
                .field("requester", Field::text().required())
                .field(
                    "environment",
                    Field::text()
                        .required()
                        .default(json!(DEVELOPMENT))
                        .one_of(&[DEVELOPMENT, PRODUCTION])
                        .describe("Which infrastructure it is part of"),
                )
                .field(
                    "status",
                    Field::text().required().default(json!("pending")).one_of(&[
                        "pending",
                        "provisioning",
                        "active",
                        "expiring",
                        "deleting",
                        "deleted",
                        "failed",
                    ]),
                )
                .field("vendor_id", Field::text())
                .field("detail", Field::json().required().default(json!({})))
                .field("error", Field::text())
                .field("expires_at", Field::timestamp().required())
                .field("warned_at", Field::timestamp())
                .field("checked_at", Field::timestamp())
                .field(
                    "slot",
                    Field::integer().describe("Which of its team's quota a live request holds"),
                )
                // A quota is per team, per template and per environment, so what development
                // holds never uses up what production may have.
                .unique(&["team", "template", "environment", "slot"])
                .index(&["environment", "status"])
                .index(&["expires_at"]),
        )
}

/// The two infrastructures a platform keeps: what people build against, and what serves its users.
pub const DEVELOPMENT: &str = "development";
pub const PRODUCTION: &str = "production";
pub const ENVIRONMENTS: [&str; 2] = [DEVELOPMENT, PRODUCTION];

fn development() -> String {
    DEVELOPMENT.to_string()
}

/// The environment somebody named, or development when they named nothing this platform has.
pub fn environment_of(named: Option<&str>) -> String {
    named
        .map(str::trim)
        .filter(|named| ENVIRONMENTS.contains(named))
        .unwrap_or(DEVELOPMENT)
        .to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Template {
    pub id: Uuid,
    pub name: String,
    pub title: String,
    pub description: String,
    pub vendor: String,
    pub resource_type: String,
    pub regions: Vec<String>,
    pub sizes: Vec<String>,
    pub default_region: String,
    pub default_size: Option<String>,
    pub default_lifetime_minutes: i64,
    pub max_lifetime_minutes: i64,
    /// How many live resources from this template each team may have.
    pub team_quota: i64,
    pub created_by: String,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    #[serde(alias = "_updated_at", default)]
    pub updated_at: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub id: Uuid,
    pub template: Uuid,
    /// `development` or `production`; anything made before there were two is development.
    #[serde(default = "development")]
    pub environment: String,
    pub vendor: String,
    pub resource_type: String,
    pub name: String,
    pub region: String,
    pub size: Option<String>,
    pub team: String,
    pub service: Option<String>,
    pub requester: String,
    pub status: String,
    /// What the vendor calls it, once made.
    pub vendor_id: Option<String>,
    pub detail: Value,
    pub error: Option<String>,
    pub expires_at: String,
    pub warned_at: Option<String>,
    pub checked_at: Option<String>,
    #[serde(alias = "_created_at", default)]
    pub created_at: String,
    #[serde(alias = "_updated_at", default)]
    pub updated_at: String,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// An insert that lost a slot to another at the same moment, which is worth trying again.
fn lost(err: &PluginError) -> bool {
    err.is_duplicate()
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    pub async fn templates(&self) -> Result<Vec<Template>, Refusal> {
        let query = Query::new("templates").order(Order::asc("vendor")).order(Order::asc("name"));
        Ok(self.0.query_all(query).await?)
    }

    pub async fn template(&self, name: &str) -> Result<Option<Template>, Refusal> {
        let page =
            self.0.query(Query::new("templates").filter(json!({ "name": name })).limit(1)).await?;
        Ok(page.records.into_iter().next())
    }

    pub async fn template_by_id(&self, id: Uuid) -> Result<Option<Template>, Refusal> {
        Ok(self.0.get("templates", id.to_string()).await?)
    }

    pub async fn save_template(&self, template: &Template) -> Result<(), Refusal> {
        let set = json!({
            "title": template.title, "description": template.description,
            "regions": template.regions, "sizes": template.sizes,
            "default_region": template.default_region, "default_size": template.default_size,
            "default_lifetime_minutes": template.default_lifetime_minutes,
            "max_lifetime_minutes": template.max_lifetime_minutes, "team_quota": template.team_quota,
        });
        let updated: Option<Value> =
            self.0.update("templates", template.id.to_string(), set.clone(), None).await?;
        if updated.is_none() {
            let mut values = set;
            values["id"] = json!(template.id);
            values["name"] = json!(template.name);
            values["vendor"] = json!(template.vendor);
            values["resource_type"] = json!(template.resource_type);
            values["created_by"] = json!(template.created_by);
            let inserted: Result<Value, _> = self.0.insert("templates", values).await;
            match inserted {
                Err(err) if err.is_duplicate() => {
                    let taken = format!("there is a template called {} already", template.name);
                    return Err(Refusal::conflict(taken));
                }
                inserted => inserted?,
            };
        }
        Ok(())
    }

    /// Deletes a template no request was ever made from, answering whether it could.
    pub async fn delete_template(&self, id: Uuid) -> Result<bool, Refusal> {
        match self.0.delete("templates", id.to_string(), None).await {
            Ok(deleted) => Ok(deleted),
            Err(err) if err.problem().is_some_and(|(status, _)| status == 400) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// Adds a request in a free slot of the team's quota, which is unique, or says how many are live.
    pub async fn add_request(
        &self,
        request: &Request,
        quota: i64,
    ) -> Result<Result<(), i64>, Refusal> {
        for _ in 0..ATTEMPTS {
            let held = Query::new("requests").filter(json!({
                "team": request.team, "template": request.template,
                "environment": request.environment, "slot": { "is_null": false },
            }));
            let live: Vec<Value> = self.0.query_all(held).await?;
            let taken: Vec<i64> =
                live.iter().filter_map(|held| held.get("slot")?.as_i64()).collect();
            let Some(slot) = (0..quota).find(|slot| !taken.contains(slot)) else {
                return Ok(Err(live.len() as i64));
            };
            let values = json!({
                "id": request.id, "template": request.template, "vendor": request.vendor,
                "resource_type": request.resource_type, "name": request.name,
                "region": request.region, "size": request.size, "team": request.team,
                "service": request.service, "requester": request.requester,
                "environment": request.environment, "expires_at": request.expires_at,
                "slot": slot,
            });
            match self.0.insert::<Value>("requests", values).await {
                Ok(_) => return Ok(Ok(())),
                Err(err) if lost(&err) => continue,
                Err(err) => return Err(err.into()),
            }
        }
        Err(Refusal::unavailable("the team's quota kept changing; try again"))
    }

    pub async fn request(&self, id: Uuid) -> Result<Option<Request>, Refusal> {
        Ok(self.0.get("requests", id.to_string()).await?)
    }

    /// Every request, newest first.
    pub async fn requests(&self) -> Result<Vec<Request>, Refusal> {
        Ok(self.0.query_all(Query::new("requests").order(Order::desc("_created_at"))).await?)
    }

    /// Requests in any of these statuses, soonest to expire first.
    pub async fn in_status(&self, statuses: &[&str]) -> Result<Vec<Request>, Refusal> {
        let query = Query::new("requests")
            .filter(json!({ "status": { "in": statuses } }))
            .order(Order::asc("expires_at"));
        Ok(self.0.query_all(query).await?)
    }

    /// Changes a request as it is now, again if another writer got there first; false if declined.
    async fn change(
        &self,
        id: Uuid,
        change: impl Fn(&Request) -> Option<Value>,
    ) -> Result<bool, Refusal> {
        let changed = self
            .0
            .change("requests", id.to_string(), |current| {
                let request: Request =
                    serde_json::from_value(Value::Object(current.clone())).ok()?;
                let mut set = change(&request)?;
                let status = set.get("status").and_then(Value::as_str).unwrap_or(&request.status);
                if !LIVE.contains(&status) {
                    set["slot"] = Value::Null;
                }
                Some(set)
            })
            .await?;
        Ok(changed.is_some())
    }

    /// Moves a request from one of `from` to `to`, answering whether it was in one of them.
    pub async fn shift(&self, id: Uuid, from: &[&str], to: &str) -> Result<bool, Refusal> {
        self.change(id, |request| {
            from.contains(&request.status.as_str()).then(|| json!({ "status": to }))
        })
        .await
    }

    /// Records what the vendor made, or why it could not.
    pub async fn settle(
        &self,
        id: Uuid,
        status: &str,
        vendor_id: Option<&str>,
        detail: &Value,
        error: Option<&str>,
    ) -> Result<(), Refusal> {
        self.change(id, |request| {
            let vendor_id = vendor_id.map(str::to_string).or_else(|| request.vendor_id.clone());
            Some(json!({
                "status": status, "vendor_id": vendor_id, "detail": detail, "error": error,
                "checked_at": now(),
            }))
        })
        .await?;
        Ok(())
    }

    pub async fn extend(&self, id: Uuid, expires_at: &str) -> Result<(), Refusal> {
        self.change(id, |request| {
            let status = if request.status == "expiring" { "active" } else { &request.status };
            Some(json!({ "expires_at": expires_at, "warned_at": null, "status": status }))
        })
        .await?;
        Ok(())
    }

    /// Takes the expiry warning for a request, answering whether it was still to be given.
    pub async fn warn(&self, id: Uuid) -> Result<bool, Refusal> {
        self.change(id, |request| {
            (request.warned_at.is_none() && request.status == "active")
                .then(|| json!({ "warned_at": now(), "status": "expiring" }))
        })
        .await
    }

    pub async fn checked(&self, id: Uuid) -> Result<(), Refusal> {
        self.change(id, |_| Some(json!({ "checked_at": now() }))).await?;
        Ok(())
    }
}
