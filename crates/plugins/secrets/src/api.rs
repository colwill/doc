//! The JSON routes: the API for people and service accounts, `internal/` for core resolving the
//! secrets settings point at, and `discovery/` for a plugin asking for a token an allowance names
//! it in, or for the proxied accounts it may call through as itself.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ops::{self, Context, NewSecret};
use crate::proxy::manage;
use crate::proxy::rules::Rule;
use crate::store::{Call, Key, PROXIED, Secret, Token};
use crate::{Refusal, answered};

type Answer = Result<(u16, Value), Refusal>;

fn id(text: &str) -> Result<Uuid, Refusal> {
    text.parse().map_err(|_| Refusal::bad("that is not an ID"))
}

fn body<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T, Refusal> {
    request.json().map_err(|err| Refusal::bad(err.to_string()))
}

fn reply(answer: Answer) -> Response {
    match answer {
        Ok((status, value)) => answered(status, &value),
        Err(refusal) => refusal.response(),
    }
}

/// A secret as any answer shows it: everything but its value.
pub fn shown(cx: &Context<'_>, secret: &Secret) -> Value {
    json!({
        "id": secret.id,
        "owner": secret.owner,
        "owner_label": cx.directory.owner_label(&secret.owner),
        "name": secret.name,
        "label": cx.label(secret),
        "title": secret.title,
        "description": secret.description,
        "plugins": secret.plugins,
        "version": secret.version,
        "expires_at": secret.expires_at,
        "kept": secret.kept.is_some(),
        "used": secret.used,
        "updated_by": secret.updated_by,
        "updated_at": secret.updated_at,
    })
}

fn token_shown(token: &Token) -> Value {
    json!({
        "id": token.id,
        "account": token.account,
        "asked_by": token.asked_by_label,
        "purpose": token.purpose,
        "restrictions": token.restrictions,
        "issued_at": token.issued_at,
        "expires_at": token.expires_at,
        "state": if token.live() { "active" } else { token.state.as_str() },
    })
}

#[derive(Deserialize)]
struct Storing {
    owner: String,
    name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    description: String,
    value: String,
    #[serde(default)]
    expires_at: Option<DateTime<Utc>>,
    #[serde(default)]
    plugins: Vec<String>,
}

#[derive(Deserialize)]
struct Replacing {
    value: String,
}

#[derive(Deserialize)]
struct Sharing {
    plugin: String,
}

/// A DOC key as any answer shows it: never the key itself, only what it opens and until when.
fn key_shown(key: &Key) -> Value {
    json!({
        "id": key.id,
        "account": key.account,
        "allowance": key.allowance,
        "subject": key.subject,
        "subject_label": key.subject_label,
        "purpose": key.purpose,
        "shown": key.shown,
        "made_at": key.made_at,
        "expires_at": key.expires_at,
        "state": if key.live() { "active" } else { key.state.as_str() },
        "last_used_at": key.last_used_at,
    })
}

/// One line of the audit: who reached what, through which account, and what came back.
fn call_shown(call: &Call) -> Value {
    json!({
        "id": call.id,
        "at": call.at,
        "who": call.who,
        "who_label": call.who_label,
        "account": call.account,
        "account_name": call.account_name,
        "vendor": call.vendor,
        "method": call.method,
        "path": call.path,
        "query": call.query,
        "outcome": call.outcome,
        "rule": call.rule,
        "denial": call.denial,
        "status": call.status,
        "sent": call.sent,
        "received": call.received,
        "cut_short": call.cut_short,
        "ms": call.ms,
        "correlation": call.correlation,
        "vendor_call": call.vendor_call,
        "detail": call.detail,
    })
}

#[derive(Deserialize)]
struct Onboarding {
    owner: String,
    name: String,
    #[serde(default)]
    title: String,
    #[serde(default)]
    config: std::collections::BTreeMap<String, String>,
    credential: String,
}

#[derive(Deserialize)]
struct Allowing {
    who: String,
    #[serde(default)]
    rules: Vec<Ruling>,
    #[serde(default)]
    minutes: Option<i64>,
    /// Said out loud when a rule lets through everything.
    #[serde(default)]
    sweeping: bool,
}

#[derive(Deserialize)]
struct Ruling {
    #[serde(default = "any")]
    method: String,
    path: String,
}

fn any() -> String {
    crate::proxy::rules::ANY.to_string()
}

#[derive(Deserialize)]
struct Denying {
    #[serde(default = "any")]
    method: String,
    path: String,
    #[serde(default)]
    reason: String,
}

#[derive(Deserialize)]
struct Issuing {
    allowance: Uuid,
    /// Only whoever manages the account may name somebody else.
    #[serde(default)]
    subject: Option<String>,
    #[serde(default)]
    purpose: String,
    #[serde(default)]
    minutes: Option<i64>,
}

#[derive(Deserialize)]
struct Revoking {
    #[serde(default)]
    why: String,
}

#[derive(Deserialize)]
struct Asking {
    #[serde(default)]
    allowance: Option<Uuid>,
    #[serde(default)]
    restrictions: Value,
    #[serde(default)]
    minutes: Option<i64>,
    #[serde(default)]
    purpose: String,
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    reply(Box::pin(route(backend, request, path)).await)
}

async fn route(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let cx = Context::read(backend).await?;
    match (request.method.as_str(), path) {
        ("GET", ["secrets"]) => {
            let secrets: Vec<Value> = cx
                .store()
                .secrets()
                .await?
                .iter()
                .filter(|secret| cx.me.manages(&cx.directory, &secret.owner))
                .map(|secret| shown(&cx, secret))
                .collect();
            Ok((200, json!({ "secrets": secrets })))
        }
        ("POST", ["secrets"]) => {
            let asked: Storing = body(request)?;
            let new = NewSecret {
                owner: asked.owner,
                name: asked.name,
                title: asked.title,
                description: asked.description,
                value: asked.value,
                expires_at: asked.expires_at,
                plugins: asked.plugins,
            };
            let secret = ops::store_secret(&cx, new).await?;
            Ok((201, shown(&cx, &secret)))
        }
        ("GET", ["secrets", secret]) => {
            let secret = ops::managed_secret(&cx, id(secret)?).await?;
            Ok((200, shown(&cx, &secret)))
        }
        ("PUT", ["secrets", secret, "value"]) => {
            let asked: Replacing = body(request)?;
            let secret = ops::replace_value(&cx, id(secret)?, &asked.value).await?;
            Ok((200, shown(&cx, &secret)))
        }
        ("POST", ["secrets", secret, "plugins"]) => {
            let asked: Sharing = body(request)?;
            let secret = ops::share(&cx, id(secret)?, &asked.plugin, true).await?;
            Ok((200, shown(&cx, &secret)))
        }
        ("DELETE", ["secrets", secret, "plugins", plugin]) => {
            let secret = ops::share(&cx, id(secret)?, plugin, false).await?;
            Ok((200, shown(&cx, &secret)))
        }
        ("DELETE", ["secrets", secret]) => {
            ops::delete_secret(&cx, id(secret)?).await?;
            Ok((200, json!({ "deleted": true })))
        }
        ("GET", ["accounts"]) => {
            let mut accounts = Vec::new();
            for (account, manages) in ops::visible_accounts(&cx).await? {
                let vendor = ops::vendor_of(&account)?;
                let covering: Vec<Value> = ops::covering(&cx, account.id)
                    .await?
                    .iter()
                    .map(|allowance| {
                        json!({
                            "id": allowance.id, "grants": allowance.grants,
                            "about": vendor.describe(&allowance.grants), "minutes": allowance.minutes,
                        })
                    })
                    .collect();
                accounts.push(json!({
                    "id": account.id, "name": account.name, "title": account.title,
                    "vendor": vendor.id(), "owner_label": cx.directory.owner_label(&account.owner),
                    "manages": manages, "allowances": covering,
                }));
            }
            Ok((200, json!({ "accounts": accounts })))
        }
        ("POST", ["accounts", account, "tokens"]) => {
            let asked: Asking = body(request)?;
            let account = id(account)?;
            let allowance = match asked.allowance {
                Some(allowance) => allowance,
                None => fitting(&cx, account, &asked.restrictions).await?,
            };
            let minutes = asked.minutes.unwrap_or(60);
            let (token, value) =
                ops::issue(&cx, account, allowance, &asked.restrictions, minutes, &asked.purpose)
                    .await?;
            let mut shown = token_shown(&token);
            shown["token"] = json!(value.expose());
            Ok((201, shown))
        }
        ("POST", ["accounts", "proxied"]) => {
            let asked: Onboarding = body(request)?;
            let new = manage::NewAccount {
                owner: asked.owner,
                name: asked.name,
                title: asked.title,
                config: asked.config,
                credential: asked.credential,
            };
            let (account, tried) = Box::pin(manage::onboard(&cx, new)).await?;
            Ok((201, json!({ "id": account.id, "name": account.name, "tried": tried })))
        }
        ("POST", ["accounts", account, "test"]) => {
            let tried = manage::test_account(&cx, id(account)?).await?;
            Ok((200, json!({ "tried": tried })))
        }
        ("POST", ["accounts", account, "allowances"]) => {
            let asked: Allowing = body(request)?;
            let written: Vec<(String, String)> =
                asked.rules.into_iter().map(|rule| (rule.method, rule.path)).collect();
            let allowance = Box::pin(manage::allow(
                &cx,
                id(account)?,
                &asked.who,
                &written,
                asked.minutes.unwrap_or(60),
                asked.sweeping,
            ))
            .await?;
            Ok((
                201,
                json!({
                    "id": allowance.id, "who": allowance.who, "who_label": allowance.who_label,
                    "rules": allowance.rules, "minutes": allowance.minutes,
                }),
            ))
        }
        ("POST", ["accounts", account, "denials"]) => {
            let asked: Denying = body(request)?;
            let denial =
                manage::deny(&cx, Some(id(account)?), &asked.method, &asked.path, &asked.reason)
                    .await?;
            Ok((201, json!({ "id": denial.id, "method": denial.method, "path": denial.path })))
        }
        ("POST", ["denials"]) => {
            let asked: Denying = body(request)?;
            let denial = manage::deny(&cx, None, &asked.method, &asked.path, &asked.reason).await?;
            Ok((201, json!({ "id": denial.id, "method": denial.method, "path": denial.path })))
        }
        ("DELETE", ["denials", denial]) => {
            manage::undeny(&cx, id(denial)?).await?;
            Ok((200, json!({ "lifted": true })))
        }
        ("POST", ["accounts", account, "keys"]) => {
            let asked: Issuing = body(request)?;
            let (key, value) = Box::pin(manage::issue(
                &cx,
                id(account)?,
                asked.allowance,
                asked.subject.as_deref(),
                &asked.purpose,
                asked.minutes,
            ))
            .await?;
            let mut shown = key_shown(&key);
            // The only time the key is ever answered. DOC keeps its digest and nothing else.
            shown["key"] = json!(value.expose());
            Ok((201, shown))
        }
        ("GET", ["keys"]) => {
            let mine = cx.store().keys(json!({ "subject": cx.me.reference() })).await?;
            Ok((200, json!({ "keys": mine.iter().map(key_shown).collect::<Vec<_>>() })))
        }
        ("POST", ["keys", key, "revoke"]) => {
            let asked: Revoking = body(request).unwrap_or(Revoking { why: String::new() });
            let key = manage::revoke(&cx, id(key)?, &asked.why).await?;
            Ok((200, key_shown(&key)))
        }
        ("GET", ["accounts", account, "calls"]) => {
            let calls = manage::log(&cx, Some(id(account)?), limit(request)).await?;
            Ok((200, json!({ "calls": calls.iter().map(call_shown).collect::<Vec<_>>() })))
        }
        ("GET", ["calls"]) => {
            let calls = manage::log(&cx, None, limit(request)).await?;
            Ok((200, json!({ "calls": calls.iter().map(call_shown).collect::<Vec<_>>() })))
        }
        ("GET", ["tokens"]) => {
            let mine = cx.store().tokens(json!({ "asked_by": cx.me.reference() })).await?;
            Ok((200, json!({ "tokens": mine.iter().map(token_shown).collect::<Vec<_>>() })))
        }
        ("POST", ["tokens", token, "revoke"]) => {
            let token = ops::revoke(&cx, id(token)?).await?;
            Ok((200, token_shown(&token)))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

/// How many rows an answer carries, within what the store allows.
fn limit(request: &Request) -> u32 {
    crate::parameter(&request.query, "limit").and_then(|limit| limit.parse().ok()).unwrap_or(100)
}

/// The first allowance covering the caller that grants all of what they asked for.
async fn fitting(cx: &Context<'_>, account: Uuid, asked: &Value) -> Result<Uuid, Refusal> {
    let vendor = ops::vendor_of(&cx.store().account(account).await?)?;
    let covering = ops::covering(cx, account).await?;
    if covering.is_empty() {
        return Err(Refusal::forbidden("no allowance on this account covers you"));
    }
    covering
        .iter()
        .find(|allowance| vendor.within(&allowance.grants, asked).is_ok())
        .map(|allowance| allowance.id)
        .ok_or_else(|| Refusal::forbidden("no allowance covering you grants all of that"))
}

#[derive(Deserialize)]
struct Resolving {
    plugin: String,
    #[serde(default)]
    secrets: Vec<Uuid>,
}

/// Core's calls, as the platform: the values a plugin's settings point at, and what it may choose.
pub async fn internal(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let platform = backend.caller().is_some_and(|caller| caller.kind == "platform");
    if !platform {
        return Refusal::forbidden("only core asks this").response();
    }
    let answer = match (request.method.as_str(), path) {
        ("POST", ["resolve"]) => match body::<Resolving>(request) {
            Ok(asked) => {
                ops::resolve(backend, &asked.plugin, &asked.secrets).await.map(|found| (200, found))
            }
            Err(refusal) => Err(refusal),
        },
        ("POST", ["shared"]) => match body::<Resolving>(request) {
            Ok(asked) => ops::shared(backend, &asked.plugin).await.map(|found| (200, found)),
            Err(refusal) => Err(refusal),
        },
        _ => Err(Refusal::missing("no such route")),
    };
    reply(answer)
}

#[derive(Deserialize)]
struct PluginAsking {
    account: String,
    #[serde(default)]
    restrictions: Value,
    #[serde(default)]
    minutes: Option<i64>,
    #[serde(default)]
    purpose: String,
}

/// Another plugin, as itself: a token from an account naming it, or the proxied accounts it uses.
pub async fn discovery(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let answer = async {
        let cx = Context::read(backend).await?;
        if cx.me.plugin.is_none() {
            return Err(Refusal::forbidden("only a plugin asks here"));
        }
        match (request.method.as_str(), path) {
            ("GET", ["accounts"]) => {
                let me = cx.me.reference();
                let allowances = cx.store().allowances(None).await?;
                let accounts: Vec<Value> = cx
                    .store()
                    .accounts()
                    .await?
                    .into_iter()
                    .filter(|account| account.vendor == PROXIED)
                    .filter_map(|account| {
                        let naming: Vec<_> = allowances
                            .iter()
                            .filter(|allowance| allowance.account == account.id)
                            .filter(|allowance| allowance.who == me)
                            .collect();
                        let rules: Vec<String> = naming
                            .iter()
                            .flat_map(|allowance| Rule::all(&allowance.rules))
                            .map(|rule| rule.shown())
                            .collect();
                        (!naming.is_empty()).then(|| {
                            json!({
                                "name": account.name, "title": account.title,
                                "address": account.config["base"], "rules": rules,
                            })
                        })
                    })
                    .collect();
                Ok((200, json!({ "accounts": accounts })))
            }
            ("POST", ["tokens"]) => {
                let asked: PluginAsking = body(request)?;
                let accounts = cx.store().accounts().await?;
                let account = accounts
                    .iter()
                    .find(|account| {
                        account.id.to_string() == asked.account || account.name == asked.account
                    })
                    .ok_or_else(|| Refusal::missing("there is no such vendor account"))?;
                let allowance = fitting(&cx, account.id, &asked.restrictions).await?;
                let minutes = asked.minutes.unwrap_or(60);
                let (token, value) = ops::issue(
                    &cx,
                    account.id,
                    allowance,
                    &asked.restrictions,
                    minutes,
                    &asked.purpose,
                )
                .await?;
                let mut shown = token_shown(&token);
                shown["token"] = json!(value.expose());
                Ok((201, shown))
            }
            _ => Err(Refusal::missing("no such route")),
        }
    }
    .await;
    reply(answer)
}
