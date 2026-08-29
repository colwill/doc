//! What Secret Storage keeps: secrets and vendor accounts, sealed by core and never in the clear;
//! the allowances saying who may ask an account for what; the DOC keys callers reach a proxied
//! account with; every call made through one; every token issued; and what happened.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{
    Backend, Collection, Declaration, Field, ListOf, OnDelete, Order, Query, SealedValue,
};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;

pub const SECRETS: &str = "secrets";
pub const ACCOUNTS: &str = "accounts";
pub const ALLOWANCES: &str = "allowances";
pub const DENIALS: &str = "denials";
pub const KEYS: &str = "keys";
pub const CALLS: &str = "calls";
pub const TOKENS: &str = "tokens";
pub const HISTORY: &str = "history";

pub const ACTIVE: &str = "active";
pub const REVOKED: &str = "revoked";
pub const EXPIRED: &str = "expired";

/// The vendor kind of an account DOC proxies rather than issues from (ADR-0014).
pub const PROXIED: &str = "proxied";

/// How a proxied call ended, which is the first thing anybody reading the log looks at.
pub const ALLOWED: &str = "allowed";
/// A denial stopped it, and no allowance could have let it through.
pub const DENIED: &str = "denied";
/// Nothing the caller is covered by allowed it: the deny-by-default answer.
pub const NO_RULE: &str = "no-rule";
/// The DOC key was missing, unknown, expired or revoked, so there was nobody to allow.
pub const NO_KEY: &str = "no-key";
/// Nobody was addressing an account this instance has. Worth its own outcome rather than being
/// filed under a bad key: somebody working through account names is a different thing to look at.
pub const NO_ACCOUNT: &str = "no-account";
/// DOC let it through and it went wrong anyway: the vendor was unreachable, or the stream broke.
pub const FAILED: &str = "failed";

pub const OUTCOMES: [&str; 6] = [ALLOWED, DENIED, NO_RULE, NO_KEY, NO_ACCOUNT, FAILED];

pub fn declaration() -> Declaration {
    let sealed = || Field::json().describe("The value as core sealed it, never the value itself");
    Declaration::default()
        .collection(
            SECRETS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "owner",
                    Field::text().required().describe("organisation:<id>, team:<id> or user:<id>"),
                )
                .field("name", Field::text().required())
                .field("title", Field::text())
                .field("description", Field::text().max(1_000.0))
                .field(
                    "plugins",
                    Field::list(ListOf::Text)
                        .required()
                        .default(json!([]))
                        .describe("The plugins that may be given its value"),
                )
                .field("sealed", sealed())
                .field("version", Field::integer().required().default(json!(1)))
                .field("expires_at", Field::timestamp())
                .field(
                    "kept",
                    Field::json().describe("A token kept for a plugin: where from, and renewal"),
                )
                .field("used", Field::json().describe("When each plugin was last given it"))
                .field("warned", Field::integer().describe("Days before expiry last warned of"))
                .field("created_by", Field::text())
                .field("updated_by", Field::text())
                .unique(&["owner", "name"])
                .index(&["owner"]),
        )
        .collection(
            ACCOUNTS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("owner", Field::text().required())
                .field("name", Field::text().required())
                .field("title", Field::text())
                .field(
                    "vendor",
                    Field::text().required().one_of(&[
                        PROXIED,
                        "artifactory",
                        "github-app",
                        "aws-sts",
                        "linode",
                    ]),
                )
                .field(
                    "config",
                    Field::json().required().default(json!({})).describe(
                        "For a proxied account: its base address, and how its credential is put \
                         on the call DOC makes",
                    ),
                )
                .field("sealed", sealed())
                .field("created_by", Field::text())
                .field("updated_by", Field::text())
                .unique(&["owner", "name"])
                .index(&["owner"]),
        )
        .collection(
            ALLOWANCES,
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "account",
                    Field::reference(ACCOUNTS).required().on_delete(OnDelete::Cascade),
                )
                .field(
                    "who",
                    Field::text().required().describe(
                        "team:<id>, user:<id>, service:<id>, plugin:<id>, organisation:<id>, \
                         permission:<name> or attribute:<key>=<value>",
                    ),
                )
                .field("who_label", Field::text())
                .field("grants", Field::json().required().default(json!({})))
                .field(
                    "rules",
                    Field::json().required().default(json!([])).describe(
                        "For a proxied account: the {method, path} pairs this allowance grants",
                    ),
                )
                .field("minutes", Field::integer().required())
                .field("created_by", Field::text())
                .index(&["account"]),
        )
        .collection(
            DENIALS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "account",
                    Field::reference(ACCOUNTS).on_delete(OnDelete::Cascade).describe(
                        "The account it is written on; none means every account on this instance",
                    ),
                )
                .field("method", Field::text().required().default(json!("ANY")))
                .field("path", Field::text().required())
                .field("reason", Field::text().max(300.0))
                .field("created_by", Field::text())
                .index(&["account"]),
        )
        .collection(
            KEYS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "account",
                    Field::reference(ACCOUNTS).required().on_delete(OnDelete::Cascade),
                )
                .field("allowance", Field::uuid().required())
                .field(
                    "subject",
                    Field::text()
                        .required()
                        .describe("Whose key it is: user:<id>, service:<id> or plugin:<id>"),
                )
                .field("subject_label", Field::text())
                .field("purpose", Field::text())
                .field(
                    "shown",
                    Field::text().required().describe("The part of the key a page may show"),
                )
                .field(
                    "digest",
                    Field::text().required().describe("SHA-256 of the key, never the key itself"),
                )
                .field("expires_at", Field::timestamp().required())
                .field("state", Field::text().required().one_of(&[ACTIVE, REVOKED, EXPIRED]))
                .field("revoked_at", Field::timestamp())
                .field("revoked_by", Field::text())
                .field("last_used_at", Field::timestamp())
                .field("created_by", Field::text())
                .unique(&["digest"])
                .index(&["account"])
                .index(&["subject"])
                .index(&["state"]),
        )
        .collection(CALLS, calls())
        .collection(
            TOKENS,
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "account",
                    Field::reference(ACCOUNTS).required().on_delete(OnDelete::Cascade),
                )
                .field("allowance", Field::uuid())
                .field("asked_by", Field::text().required())
                .field("asked_by_label", Field::text())
                .field("purpose", Field::text())
                .field("restrictions", Field::json().required().default(json!({})))
                .field("vendor_id", Field::text().describe("The token's ID at the vendor"))
                .field("expires_at", Field::timestamp().required())
                .field("state", Field::text().required().one_of(&[ACTIVE, REVOKED, EXPIRED]))
                .field("revoked_at", Field::timestamp())
                .field("revoked_by", Field::text())
                .field("kept_in", Field::uuid().describe("The secret it is kept in, for a plugin"))
                .index(&["account"])
                .index(&["asked_by"])
                .index(&["state"]),
        )
        .collection(
            HISTORY,
            Collection::new()
                .field("id", Field::uuid().key())
                .field("about", Field::uuid().required())
                .field("by", Field::text())
                .field("action", Field::text().required())
                .field("detail", Field::text().max(1_000.0))
                .index(&["about"]),
        )
}

/// What a proxied call is written down as (ADR-0014 §12). Every field is a copy rather than a
/// reference: the record has to still read correctly once the account is gone and the person has
/// left, and an audit that dies with what it describes is not one.
fn calls() -> Collection {
    Collection::new()
        .field("id", Field::uuid().key())
        .field("at", Field::timestamp().required().describe("When the call started"))
        .field(
            "account",
            Field::uuid().describe("None when nobody was addressing an account DOC has"),
        )
        .field(
            "account_name",
            Field::text().required().describe("The account as the caller named it"),
        )
        .field("vendor", Field::text().describe("The host it was made to"))
        .field(
            "who",
            Field::text()
                .required()
                .describe("user:<id>, service:<id>, plugin:<id>, or unknown for an unusable key"),
        )
        .field("who_label", Field::text())
        .field("key", Field::uuid().describe("The DOC key it was made with"))
        .field("allowance", Field::uuid())
        .field("rule", Field::text().describe("The rule that allowed it"))
        .field("method", Field::text().required())
        .field("path", Field::text().required().describe("The path as it was sent to the vendor"))
        .field(
            "query",
            Field::list(ListOf::Text)
                .required()
                .default(json!([]))
                .describe("The query's keys; never its values"),
        )
        .field("outcome", Field::text().required().one_of(&OUTCOMES))
        .field("denial", Field::text().describe("The denial that stopped it"))
        .field("status", Field::integer().describe("What the vendor answered"))
        .field("sent", Field::integer().required().default(json!(0)))
        .field("received", Field::integer().required().default(json!(0)))
        .field(
            "cut_short",
            Field::boolean().required().default(json!(false)).describe("The stream ended early"),
        )
        .field("ms", Field::integer().required().default(json!(0)))
        .field(
            "correlation",
            Field::text().required().describe("Sent upstream too, so the two logs join"),
        )
        .field(
            "vendor_call",
            Field::text().describe("The vendor's own ID for it, where it gives one"),
        )
        .field("replica", Field::text().describe("Which proxy served it"))
        .field("detail", Field::text().max(300.0))
        .index(&["account"])
        .index(&["who"])
        .index(&["key"])
        .index(&["outcome"])
}

/// A field the data API may give as `null`: the type's default then, like a missing one.
pub fn or_default<'de, D: Deserializer<'de>, T: Default + Deserialize<'de>>(
    deserializer: D,
) -> Result<T, D::Error> {
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// A token kept in a secret for a plugin: which account and allowance it comes from, what it is
/// narrowed to, and how it is renewed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Kept {
    pub account: Uuid,
    pub allowance: Uuid,
    pub restrictions: Value,
    pub minutes: i64,
    pub renew: bool,
    #[serde(default)]
    pub token: Option<Uuid>,
    /// The token the last renewal replaced, revoked at the next pass once the plugin has the new one.
    #[serde(default)]
    pub previous: Option<Uuid>,
    #[serde(default)]
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Secret {
    pub id: Uuid,
    pub owner: String,
    pub name: String,
    #[serde(default, deserialize_with = "or_default")]
    pub title: String,
    #[serde(default, deserialize_with = "or_default")]
    pub description: String,
    #[serde(default, deserialize_with = "or_default")]
    pub plugins: Vec<String>,
    #[serde(default)]
    pub sealed: Option<SealedValue>,
    #[serde(default, deserialize_with = "or_default")]
    pub version: i64,
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub kept: Option<Kept>,
    #[serde(default, deserialize_with = "or_default")]
    pub used: BTreeMap<String, DateTime<Utc>>,
    #[serde(default)]
    pub warned: Option<i64>,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub updated_by: Option<String>,
    #[serde(rename = "_updated_at", default)]
    pub updated_at: Option<DateTime<Utc>>,
    #[serde(rename = "_version", default)]
    pub record: i64,
}

impl Secret {
    /// What its value is sealed under beside the plugin's ID, so it opens for this secret alone.
    pub fn label(&self) -> String {
        format!("secret/{}", self.id)
    }

    pub fn href(&self) -> String {
        format!("/p/secrets/secrets/{}", self.id)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub id: Uuid,
    pub owner: String,
    pub name: String,
    #[serde(default, deserialize_with = "or_default")]
    pub title: String,
    pub vendor: String,
    #[serde(default, deserialize_with = "or_default")]
    pub config: Value,
    #[serde(default)]
    pub sealed: Option<SealedValue>,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(default)]
    pub updated_by: Option<String>,
    #[serde(rename = "_updated_at", default)]
    pub updated_at: Option<DateTime<Utc>>,
}

impl Account {
    pub fn label(&self) -> String {
        format!("account/{}", self.id)
    }

    pub fn href(&self) -> String {
        format!("/p/secrets/accounts/{}", self.id)
    }

    pub fn shown(&self) -> String {
        match self.title.is_empty() {
            true => self.name.clone(),
            false => self.title.clone(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Allowance {
    pub id: Uuid,
    pub account: Uuid,
    pub who: String,
    #[serde(default, deserialize_with = "or_default")]
    pub who_label: String,
    #[serde(default, deserialize_with = "or_default")]
    pub grants: Value,
    #[serde(default, deserialize_with = "or_default")]
    pub rules: Value,
    pub minutes: i64,
    #[serde(default)]
    pub created_by: Option<String>,
}

/// What nobody may do, whatever an allowance says. Written on one account, or on the instance
/// when `account` is none.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Denial {
    pub id: Uuid,
    #[serde(default)]
    pub account: Option<Uuid>,
    pub method: String,
    pub path: String,
    #[serde(default, deserialize_with = "or_default")]
    pub reason: String,
    #[serde(default)]
    pub created_by: Option<String>,
}

/// A DOC key: what a caller sends to reach a proxied account. Only its digest is kept, so a key
/// lost here is a key nobody can use, and the value itself is shown once when it is made.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Key {
    pub id: Uuid,
    pub account: Uuid,
    pub allowance: Uuid,
    pub subject: String,
    #[serde(default, deserialize_with = "or_default")]
    pub subject_label: String,
    #[serde(default, deserialize_with = "or_default")]
    pub purpose: String,
    pub shown: String,
    pub digest: String,
    pub expires_at: DateTime<Utc>,
    pub state: String,
    #[serde(default)]
    pub revoked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub revoked_by: Option<String>,
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub created_by: Option<String>,
    #[serde(rename = "_created_at", default)]
    pub made_at: Option<DateTime<Utc>>,
}

impl Key {
    /// Whether it still opens anything: in force, and not past its time.
    pub fn live(&self) -> bool {
        self.state == ACTIVE && self.expires_at > Utc::now()
    }
}

/// One proxied call, written down whatever became of it (ADR-0014 §12): who asked, which account
/// answered for them, what they reached, and what came back. A refusal is as much of a record as
/// a success, and the fields are copies so that deleting the account does not delete the history.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Call {
    pub id: Uuid,
    pub at: DateTime<Utc>,
    /// None when the caller named an account this instance does not have, which is worth a row
    /// of its own: somebody guessing at account names is the first half of an attack.
    #[serde(default)]
    pub account: Option<Uuid>,
    pub account_name: String,
    /// The host the call was made to, such as `api.github.com`.
    pub vendor: String,
    pub who: String,
    #[serde(default, deserialize_with = "or_default")]
    pub who_label: String,
    #[serde(default)]
    pub key: Option<Uuid>,
    #[serde(default)]
    pub allowance: Option<Uuid>,
    #[serde(default, deserialize_with = "or_default")]
    pub rule: String,
    pub method: String,
    pub path: String,
    #[serde(default, deserialize_with = "or_default")]
    pub query: Vec<String>,
    pub outcome: String,
    #[serde(default, deserialize_with = "or_default")]
    pub denial: String,
    #[serde(default)]
    pub status: Option<i64>,
    #[serde(default, deserialize_with = "or_default")]
    pub sent: i64,
    #[serde(default, deserialize_with = "or_default")]
    pub received: i64,
    #[serde(default, deserialize_with = "or_default")]
    pub cut_short: bool,
    #[serde(default, deserialize_with = "or_default")]
    pub ms: i64,
    pub correlation: String,
    #[serde(default, deserialize_with = "or_default")]
    pub vendor_call: String,
    #[serde(default, deserialize_with = "or_default")]
    pub replica: String,
    #[serde(default, deserialize_with = "or_default")]
    pub detail: String,
}

impl Call {
    /// The record as it is written. Nothing here is derived later: a row means what it said when
    /// the call ended, whatever has happened to the account since.
    pub fn values(&self) -> Value {
        json!({
            "id": self.id, "at": self.at, "account": self.account,
            "account_name": self.account_name, "vendor": self.vendor,
            "who": self.who, "who_label": self.who_label, "key": self.key,
            "allowance": self.allowance, "rule": self.rule,
            "method": self.method, "path": self.path, "query": self.query,
            "outcome": self.outcome, "denial": self.denial, "status": self.status,
            "sent": self.sent, "received": self.received, "cut_short": self.cut_short,
            "ms": self.ms, "correlation": self.correlation, "vendor_call": self.vendor_call,
            "replica": self.replica,
            "detail": self.detail.chars().take(300).collect::<String>(),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Token {
    pub id: Uuid,
    pub account: Uuid,
    #[serde(default)]
    pub allowance: Option<Uuid>,
    pub asked_by: String,
    #[serde(default, deserialize_with = "or_default")]
    pub asked_by_label: String,
    #[serde(default, deserialize_with = "or_default")]
    pub purpose: String,
    #[serde(default, deserialize_with = "or_default")]
    pub restrictions: Value,
    #[serde(default)]
    pub vendor_id: Option<String>,
    pub expires_at: DateTime<Utc>,
    pub state: String,
    #[serde(default)]
    pub revoked_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub revoked_by: Option<String>,
    #[serde(default)]
    pub kept_in: Option<Uuid>,
    #[serde(rename = "_created_at", default)]
    pub issued_at: Option<DateTime<Utc>>,
}

impl Token {
    /// Whether it still works: in force, and not past its time.
    pub fn live(&self) -> bool {
        self.state == ACTIVE && self.expires_at > Utc::now()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Happened {
    pub id: Uuid,
    pub about: Uuid,
    #[serde(default, deserialize_with = "or_default")]
    pub by: String,
    pub action: String,
    #[serde(default, deserialize_with = "or_default")]
    pub detail: String,
    #[serde(rename = "_created_at", default)]
    pub at: Option<DateTime<Utc>>,
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    async fn all<T: DeserializeOwned>(&self, query: Query) -> Result<Vec<T>, Refusal> {
        Ok(self.0.query_all(query).await?)
    }

    async fn one<T: DeserializeOwned>(&self, collection: &str, id: Uuid) -> Result<T, Refusal> {
        let found: Option<T> = self.0.get(collection, json!(id)).await?;
        found.ok_or_else(|| Refusal::missing("there is nothing here by that ID"))
    }

    pub async fn secrets(&self) -> Result<Vec<Secret>, Refusal> {
        self.all(Query::new(SECRETS)).await
    }

    pub async fn secret(&self, id: Uuid) -> Result<Secret, Refusal> {
        self.one(SECRETS, id).await
    }

    pub async fn accounts(&self) -> Result<Vec<Account>, Refusal> {
        self.all(Query::new(ACCOUNTS)).await
    }

    pub async fn account(&self, id: Uuid) -> Result<Account, Refusal> {
        self.one(ACCOUNTS, id).await
    }

    pub async fn allowances(&self, account: Option<Uuid>) -> Result<Vec<Allowance>, Refusal> {
        let query = match account {
            Some(account) => Query::new(ALLOWANCES).filter(json!({ "account": account })),
            None => Query::new(ALLOWANCES),
        };
        self.all(query).await
    }

    pub async fn allowance(&self, id: Uuid) -> Result<Allowance, Refusal> {
        self.one(ALLOWANCES, id).await
    }

    pub async fn denials(&self, account: Option<Uuid>) -> Result<Vec<Denial>, Refusal> {
        let query = match account {
            Some(account) => Query::new(DENIALS).filter(json!({ "account": account })),
            None => Query::new(DENIALS),
        };
        self.all(query).await
    }

    pub async fn denial(&self, id: Uuid) -> Result<Denial, Refusal> {
        self.one(DENIALS, id).await
    }

    pub async fn keys(&self, filter: Value) -> Result<Vec<Key>, Refusal> {
        self.all(Query::new(KEYS).filter(filter).order(Order::desc("_created_at"))).await
    }

    pub async fn key(&self, id: Uuid) -> Result<Key, Refusal> {
        self.one(KEYS, id).await
    }

    /// What was done through the proxy, newest first. This is the audit, so it is read and never
    /// written from here: the proxy writes it as each call ends.
    pub async fn calls(&self, filter: Value, limit: u32) -> Result<Vec<Call>, Refusal> {
        let query = Query::new(CALLS).filter(filter).order(Order::desc("_created_at")).limit(limit);
        Ok(self.0.query(query).await?.records)
    }

    pub async fn tokens(&self, filter: Value) -> Result<Vec<Token>, Refusal> {
        let query = Query::new(TOKENS).filter(filter).order(Order::desc("_created_at"));
        self.all(query).await
    }

    pub async fn token(&self, id: Uuid) -> Result<Token, Refusal> {
        self.one(TOKENS, id).await
    }

    pub async fn history(&self, about: Uuid) -> Result<Vec<Happened>, Refusal> {
        let query = Query::new(HISTORY)
            .filter(json!({ "about": about }))
            .order(Order::desc("_created_at"))
            .limit(200);
        Ok(self.0.query(query).await?.records)
    }

    /// What happened to a secret or an account, for its History; a failure to note it is logged.
    pub async fn note(&self, about: Uuid, by: &str, action: &str, detail: &str) {
        let values = json!({
            "id": Uuid::now_v7(), "about": about, "by": by, "action": action,
            "detail": detail.chars().take(1_000).collect::<String>(),
        });
        if let Err(err) = self.0.insert::<Value>(HISTORY, values).await {
            tracing::warn!(%err, %about, action, "what happened was not noted");
        }
    }

    pub async fn insert<T: DeserializeOwned>(
        &self,
        collection: &str,
        values: Value,
    ) -> Result<T, Refusal> {
        self.0.insert(collection, values).await.map_err(|err| match err.is_duplicate() {
            true => Refusal::conflict("that name is taken for this owner; choose another"),
            false => Refusal::from(err),
        })
    }

    pub async fn update<T: DeserializeOwned>(
        &self,
        collection: &str,
        id: Uuid,
        set: Value,
    ) -> Result<T, Refusal> {
        let updated: Option<T> = self.0.update(collection, json!(id), set, None).await?;
        updated.ok_or_else(|| Refusal::missing("it is gone"))
    }

    pub async fn delete(&self, collection: &str, id: Uuid) -> Result<(), Refusal> {
        self.0.delete(collection, json!(id), None).await?;
        Ok(())
    }
}
