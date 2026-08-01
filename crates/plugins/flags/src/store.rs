//! What the plugin keeps: every flag and setting, and the upstream providers it reads beside them.

use std::collections::BTreeMap;

use doc_plugin_sdk::{Backend, Collection, Declaration, Field, ListOf, Order, Query};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::model::{ANY, Kind, Precedence, Role, Upstream};

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "entries",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "role",
                    Field::text()
                        .required()
                        .default(json!("flag"))
                        .one_of(&["flag", "config"])
                        .describe("A switch, or a value read at runtime"),
                )
                .field("key", Field::text().required())
                .field(
                    "service",
                    Field::text()
                        .required()
                        .default(json!(ANY))
                        .describe("The service it is for, or * for every one of them"),
                )
                .field("environment", Field::text().required().default(json!(ANY)))
                .field(
                    "kind",
                    Field::text()
                        .required()
                        .default(json!("boolean"))
                        .one_of(&["boolean", "string", "number", "json"]),
                )
                .field("value", Field::json().required().default(json!(false)))
                .field("description", Field::text().required().default(json!("")))
                .field(
                    "enabled",
                    Field::boolean()
                        .required()
                        .default(json!(true))
                        .describe("A flag that is off is not served at all"),
                )
                .field("updated_by", Field::text().required().default(json!("")))
                .unique(&["role", "service", "environment", "key"])
                .index(&["service"])
                .index(&["key"])
                .index(&["role"])
                .search(&["key", "description", "service"]),
        )
        .collection(
            "providers",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("name", Field::text().required())
                .field(
                    "kind",
                    Field::text().required().one_of(&[
                        "unleash",
                        "flagsmith",
                        "ofrep",
                        "runcfg",
                        "http",
                    ]),
                )
                .field("url", Field::text().required())
                .field("environment", Field::text().required().default(json!(ANY)))
                .field(
                    "services",
                    Field::list(ListOf::Text)
                        .required()
                        .default(json!([]))
                        .describe("The services it is read for; empty means all of them"),
                )
                .field(
                    "precedence",
                    Field::text().required().default(json!("doc")).one_of(&["doc", "upstream"]),
                )
                .field(
                    "credential",
                    Field::text().describe(
                        "The names of the credentials on the Settings page it is read with, in \
                         the order its kind reads them, separated by commas",
                    ),
                )
                .field(
                    "configs",
                    Field::list(ListOf::Text).required().default(json!([])).describe(
                        "The configurations it reads by name, for a kind that holds them that way",
                    ),
                )
                .field(
                    "suggested",
                    Field::json().required().default(json!({})).describe(
                        "What it last sent that DOC does not know to be a flag or a setting, and \
                         what each held, until somebody says",
                    ),
                )
                .field(
                    "adopted",
                    Field::json().required().default(json!({})).describe(
                        "What somebody said each field it sends is: flag, config or ignored",
                    ),
                )
                .field("enabled", Field::boolean().required().default(json!(true)))
                .field("refresh_seconds", Field::integer().required().default(json!(60)))
                .field("checked_at", Field::timestamp())
                .field("problem", Field::text())
                .unique(&["name"])
                .index(&["enabled"]),
        )
}

#[derive(Debug, Clone, Deserialize)]
pub struct Entry {
    pub id: Uuid,
    pub role: String,
    pub key: String,
    pub service: String,
    pub environment: String,
    pub kind: String,
    pub value: Value,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub updated_by: String,
    #[serde(alias = "_updated_at", default)]
    pub updated_at: String,
}

impl Entry {
    pub fn role(&self) -> Role {
        Role::named(&self.role).unwrap_or(Role::Flag)
    }

    pub fn kind(&self) -> Kind {
        Kind::named(&self.kind).unwrap_or_default()
    }

    /// `payments-api/production`, as the pages and the API write what an entry is for.
    pub fn scope(&self) -> String {
        match (self.service.as_str(), self.environment.as_str()) {
            (ANY, ANY) => "Everything".to_string(),
            (service, ANY) => service.to_string(),
            (ANY, environment) => format!("Every service in {environment}"),
            (service, environment) => format!("{service} in {environment}"),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Provider {
    pub id: Uuid,
    pub name: String,
    pub kind: String,
    pub url: String,
    pub environment: String,
    #[serde(default)]
    pub services: Vec<String>,
    pub precedence: String,
    pub credential: Option<String>,
    #[serde(default)]
    pub configs: Vec<String>,
    #[serde(default)]
    pub suggested: BTreeMap<String, String>,
    #[serde(default)]
    pub adopted: BTreeMap<String, String>,
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub refresh_seconds: i64,
    pub checked_at: Option<String>,
    pub problem: Option<String>,
}

impl Provider {
    pub fn kind(&self) -> Option<Upstream> {
        Upstream::named(&self.kind)
    }

    /// The credentials it names, in the order its kind reads them.
    pub fn credentials(&self) -> Vec<String> {
        self.credential
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(|named| named.trim().to_ascii_lowercase())
            .filter(|named| !named.is_empty())
            .collect()
    }

    pub fn precedence(&self) -> Precedence {
        Precedence::named(&self.precedence).unwrap_or(Precedence::Doc)
    }

    /// Whether this provider is read for a service in an environment.
    pub fn applies(&self, service: &str, environment: &str) -> bool {
        let for_environment = self.environment == ANY || self.environment == environment;
        let for_service = self.services.is_empty()
            || self.services.iter().any(|named| named == service || named == ANY);
        self.enabled && for_environment && for_service
    }
}

/// What a write to an entry says, whether it comes from a form or from JSON.
pub struct Writing {
    pub role: Role,
    pub key: String,
    pub service: String,
    pub environment: String,
    pub kind: Kind,
    pub value: Value,
    pub description: String,
    pub enabled: bool,
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    /// Every entry of a role, newest first, or the ones a search matches.
    pub async fn entries(
        &self,
        role: Option<Role>,
        service: Option<&str>,
        search: Option<&str>,
    ) -> Result<Vec<Entry>, Refusal> {
        let mut filter = serde_json::Map::new();
        if let Some(role) = role {
            filter.insert("role".into(), json!(role.as_str()));
        }
        if let Some(service) = service.filter(|service| !service.is_empty()) {
            filter.insert("service".into(), json!({ "in": [service, ANY] }));
        }
        let mut query = match search.map(str::trim).filter(|text| !text.is_empty()) {
            Some(text) => Query::new("entries").search(text),
            None => Query::new("entries").order(Order::asc("key")),
        };
        if !filter.is_empty() {
            query = query.filter(Value::Object(filter));
        }
        Ok(self.0.query(query.limit(500)).await?.records)
    }

    pub async fn entry(&self, id: Uuid) -> Result<Option<Entry>, Refusal> {
        Ok(self.0.get("entries", id.to_string()).await?)
    }

    /// Every entry that could apply to a service in an environment, in one read.
    pub async fn for_service(
        &self,
        service: &str,
        environment: &str,
    ) -> Result<Vec<Entry>, Refusal> {
        let query = Query::new("entries")
            .filter(json!({
                "enabled": true,
                "service": { "in": [service, ANY] },
                "environment": { "in": [environment, ANY] },
            }))
            .limit(1_000);
        Ok(self.0.query_all(query).await?)
    }

    pub async fn write(&self, writing: &Writing, by: &str) -> Result<Entry, Refusal> {
        let values = json!({
            "role": writing.role.as_str(),
            "key": writing.key,
            "service": writing.service,
            "environment": writing.environment,
            "kind": writing.kind.as_str(),
            "value": writing.value,
            "description": writing.description,
            "enabled": writing.enabled,
            "updated_by": by,
        });
        let (entry, _) =
            self.0.upsert("entries", &["role", "service", "environment", "key"], values).await?;
        Ok(entry)
    }

    pub async fn set(&self, id: Uuid, set: Value) -> Result<Option<Entry>, Refusal> {
        Ok(self.0.update("entries", id.to_string(), set, None).await?)
    }

    pub async fn remove(&self, id: Uuid) -> Result<bool, Refusal> {
        Ok(self.0.delete("entries", id.to_string(), None).await?)
    }

    pub async fn providers(&self) -> Result<Vec<Provider>, Refusal> {
        Ok(self
            .0
            .query(Query::new("providers").order(Order::asc("name")).limit(100))
            .await?
            .records)
    }

    pub async fn provider(&self, id: Uuid) -> Result<Option<Provider>, Refusal> {
        Ok(self.0.get("providers", id.to_string()).await?)
    }

    pub async fn write_provider(&self, values: Value) -> Result<Provider, Refusal> {
        let (provider, _) = self.0.upsert("providers", &["name"], values).await?;
        Ok(provider)
    }

    pub async fn set_provider(&self, id: Uuid, set: Value) -> Result<Option<Provider>, Refusal> {
        Ok(self.0.update("providers", id.to_string(), set, None).await?)
    }

    pub async fn remove_provider(&self, id: Uuid) -> Result<bool, Refusal> {
        Ok(self.0.delete("providers", id.to_string(), None).await?)
    }

    /// The services that have anything of their own, for the filter on the pages.
    pub async fn services(&self) -> Result<Vec<String>, Refusal> {
        let entries: Vec<Entry> = self
            .0
            .query(Query::new("entries").order(Order::asc("service")).limit(1_000))
            .await?
            .records;
        let mut services: Vec<String> = entries
            .into_iter()
            .map(|entry| entry.service)
            .filter(|service| service != ANY)
            .collect();
        services.sort();
        services.dedup();
        Ok(services)
    }
}
