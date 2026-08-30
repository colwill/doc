//! Grafana's HTTP API, reached through a proxied vendor account in Secret Storage as this plugin
//! itself, so nothing here holds a credential and every call is recorded there. Slow-changing
//! answers are kept in the Cache Bus for two minutes, a panel's answer for thirty seconds.

use std::time::Duration;

use doc_plugin_sdk::Backend;
use doc_plugin_sdk::telemetry::external;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::Refusal;
use crate::settings::{Config, RULES};

const SECRETS: &str = "secrets";
/// How long dashboards, the list of them and the data sources are kept.
const SLOW: Duration = Duration::from_secs(120);
/// How long a panel's answer is kept, so a page many people have open asks Grafana once.
const QUICK: Duration = Duration::from_secs(30);
/// The largest answer kept in the Cache Bus; anything bigger is asked for each time.
const KEPT: usize = 512 * 1024;
/// The most dashboards one search lists.
const LISTED: usize = 5_000;

/// A dashboard as a search lists it.
#[derive(Debug, Clone)]
pub struct Found {
    pub uid: String,
    pub title: String,
    pub folder: String,
    pub tags: Vec<String>,
}

impl Found {
    fn read(value: &Value) -> Option<Self> {
        Some(Self {
            uid: value["uid"].as_str()?.to_string(),
            title: value["title"].as_str().unwrap_or_default().to_string(),
            folder: value["folderTitle"].as_str().unwrap_or("General").to_string(),
            tags: strings(&value["tags"]),
        })
    }
}

#[derive(Debug, Clone)]
pub struct Datasource {
    pub uid: String,
    pub name: String,
    pub kind: String,
    pub default: bool,
}

impl Datasource {
    fn read(value: &Value) -> Option<Self> {
        Some(Self {
            uid: value["uid"].as_str()?.to_string(),
            name: value["name"].as_str().unwrap_or_default().to_string(),
            kind: value["type"].as_str().unwrap_or_default().to_string(),
            default: value["isDefault"].as_bool().unwrap_or(false),
        })
    }
}

/// A proxied account in Secret Storage that lets this plugin through.
#[derive(Debug, Clone)]
pub struct Account {
    pub name: String,
    pub title: String,
    pub address: String,
}

pub struct Grafana<'a> {
    backend: &'a Backend,
    account: &'a str,
}

impl<'a> Grafana<'a> {
    pub fn new(backend: &'a Backend, account: &'a str) -> Self {
        Self { backend, account }
    }

    /// One call, answered whatever its status: only failing to ask at all is refused here.
    async fn exchange(
        &self,
        method: &str,
        path: &str,
        query: Option<&str>,
        body: Option<Value>,
        doing: &str,
    ) -> Result<(u16, Value), Refusal> {
        let route = format!("via/{}/{path}", self.account);
        match self.backend.discovery(SECRETS, method, &route, query, body).await {
            Ok((status, answer)) => {
                external("grafana", doing, Some(status), (200..300).contains(&status));
                Ok((status, answer))
            }
            Err(err) => {
                external("grafana", doing, None, false);
                Err(Refusal::unavailable(format!(
                    "Secret Storage could not be asked to reach Grafana: {}",
                    err.detail()
                )))
            }
        }
    }

    async fn call(
        &self,
        method: &str,
        path: &str,
        query: Option<&str>,
        body: Option<Value>,
        doing: &str,
    ) -> Result<Value, Refusal> {
        let (status, answer) = self.exchange(method, path, query, body, doing).await?;
        match status {
            200..=299 => Ok(answer),
            _ => Err(self.refused(status, &answer, method, path, doing)),
        }
    }

    /// A refusal that says who refused and what to do about it.
    fn refused(&self, status: u16, body: &Value, method: &str, path: &str, doing: &str) -> Refusal {
        let account = self.account;
        if let Some(detail) = body["detail"].as_str() {
            let detail = match detail {
                "no allowance on this account names you" => format!(
                    "No allowance on the {account} account in Secret Storage names the grafana \
                     plugin. Whoever manages the account allows it, with rules for {}.",
                    RULES.join(", ")
                ),
                "no rule allows that here" => format!(
                    "The {account} account's allowance for the grafana plugin has no rule for \
                     {method} /{path}. Whoever manages the account adds one."
                ),
                "there is no such account here" => {
                    format!("Secret Storage has no proxied account called {account}.")
                }
                other => format!("Secret Storage refused it: {other}"),
            };
            return Refusal { status, detail };
        }
        let said = body["message"]
            .as_str()
            .or_else(|| body.as_str())
            .map(|said| format!(": {}", said.chars().take(300).collect::<String>()))
            .unwrap_or_default();
        let detail = match status {
            401 => "Grafana refused the vendor account's credential".to_string(),
            403 => format!("Grafana forbade the vendor account from {doing}{said}"),
            404 => format!("Grafana found nothing when {doing}{said}"),
            _ => format!("Grafana answered {status} when {doing}{said}"),
        };
        Refusal { status: if status >= 500 { 502 } else { status }, detail }
    }

    async fn kept(&self, key: &str) -> Option<Value> {
        match self.backend.cache_get(key).await {
            Ok(kept) => kept,
            Err(err) => {
                tracing::debug!(%err, key, "the cache was not read");
                None
            }
        }
    }

    async fn keep(&self, key: &str, value: &Value, ttl: Duration) {
        if value.to_string().len() > KEPT {
            return;
        }
        if let Err(err) = self.backend.cache_set(key, value.clone(), Some(ttl)).await {
            tracing::debug!(%err, key, "the cache was not written");
        }
    }

    /// Whether the account reaches Grafana at all: one dashboard asked for, nothing kept.
    pub async fn reaches(&self) -> Result<(), Refusal> {
        let query = "type=dash-db&limit=1";
        self.call("GET", "api/search", Some(query), None, "listing dashboards").await.map(|_| ())
    }

    /// Every dashboard the account can see, or those matching `text`.
    pub async fn search(&self, text: &str) -> Result<Vec<Found>, Refusal> {
        let key = format!("search:{}", self.account);
        let kept = match text.is_empty() {
            true => self.kept(&key).await,
            false => None,
        };
        let listed = match kept {
            Some(kept) => kept,
            None => {
                let query = {
                    let mut query = url::form_urlencoded::Serializer::new(String::new());
                    query.append_pair("type", "dash-db").append_pair("limit", &LISTED.to_string());
                    if !text.is_empty() {
                        query.append_pair("query", text);
                    }
                    query.finish()
                };
                let listed = self
                    .call("GET", "api/search", Some(&query), None, "listing dashboards")
                    .await?;
                if text.is_empty() {
                    self.keep(&key, &listed, SLOW).await;
                }
                listed
            }
        };
        Ok(listed.as_array().into_iter().flatten().filter_map(Found::read).collect())
    }

    /// A dashboard as Grafana keeps it: `dashboard` and `meta`.
    pub async fn dashboard(&self, uid: &str) -> Result<Value, Refusal> {
        let key = format!("dashboard:{}:{uid}", self.account);
        if let Some(kept) = self.kept(&key).await {
            return Ok(kept);
        }
        let path = format!("api/dashboards/uid/{uid}");
        let answer = self.call("GET", &path, None, None, "reading the dashboard").await?;
        self.keep(&key, &answer, SLOW).await;
        Ok(answer)
    }

    /// A library panel's own model, for a dashboard that holds only a reference to it.
    pub async fn library_panel(&self, uid: &str) -> Result<Value, Refusal> {
        let key = format!("library:{}:{uid}", self.account);
        if let Some(kept) = self.kept(&key).await {
            return Ok(kept);
        }
        let path = format!("api/library-elements/{uid}");
        let answer = self.call("GET", &path, None, None, "reading a library panel").await?;
        let model = answer["result"]["model"].clone();
        self.keep(&key, &model, SLOW).await;
        Ok(model)
    }

    pub async fn datasources(&self) -> Result<Vec<Datasource>, Refusal> {
        let key = format!("datasources:{}", self.account);
        let listed = match self.kept(&key).await {
            Some(kept) => kept,
            None => {
                let listed =
                    self.call("GET", "api/datasources", None, None, "listing data sources").await?;
                self.keep(&key, &listed, SLOW).await;
                listed
            }
        };
        Ok(listed.as_array().into_iter().flatten().filter_map(Datasource::read).collect())
    }

    /// Runs a panel's queries; an answer with results is one even when a query in it failed.
    pub async fn query(&self, body: &Value) -> Result<Value, Refusal> {
        let digest = hex::encode(Sha256::digest(body.to_string().as_bytes()));
        let key = format!("query:{}:{digest}", self.account);
        if let Some(kept) = self.kept(&key).await {
            return Ok(kept);
        }
        let doing = "running the panel's queries";
        let (status, answer) =
            self.exchange("POST", "api/ds/query", None, Some(body.clone()), doing).await?;
        if !answer["results"].is_object() {
            return Err(self.refused(status, &answer, "POST", "api/ds/query", doing));
        }
        self.keep(&key, &answer, QUICK).await;
        Ok(answer)
    }
}

/// The proxied accounts in Secret Storage whose allowances name this plugin.
pub async fn accounts(backend: &Backend) -> Result<Vec<Account>, Refusal> {
    match backend.discovery(SECRETS, "GET", "accounts", None, None).await {
        Ok((200, body)) => Ok(body["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|account| {
                Some(Account {
                    name: account["name"].as_str()?.to_string(),
                    title: account["title"].as_str().unwrap_or_default().to_string(),
                    address: account["address"].as_str().unwrap_or_default().to_string(),
                })
            })
            .collect()),
        Ok((status, body)) => Err(Refusal {
            status,
            detail: format!(
                "Secret Storage would not list its vendor accounts: {}",
                body["detail"].as_str().unwrap_or("it gave no reason")
            ),
        }),
        Err(err) => Err(Refusal::unavailable(format!(
            "Secret Storage could not be asked for its vendor accounts: {}",
            err.detail()
        ))),
    }
}

/// Where people open Grafana: the address the settings give, or else the vendor account's own.
pub async fn address(backend: &Backend, config: &Config) -> Option<String> {
    if let Some(address) = &config.address {
        return Some(address.clone());
    }
    let account = config.account.as_deref()?;
    let key = format!("address:{account}");
    let held = match backend.cache_get(&key).await {
        Ok(Some(kept)) => kept.as_str().unwrap_or_default().to_string(),
        _ => {
            let found = accounts(backend).await.ok()?;
            let address = found
                .into_iter()
                .find(|held| held.name == account)
                .map(|held| held.address.trim_end_matches('/').to_string())
                .unwrap_or_default();
            if let Err(err) =
                backend.cache_set(&key, Value::from(address.clone()), Some(SLOW)).await
            {
                tracing::debug!(%err, "the account's address was not kept");
            }
            address
        }
    };
    (!held.is_empty()).then_some(held)
}

pub fn strings(value: &Value) -> Vec<String> {
    value.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
}
