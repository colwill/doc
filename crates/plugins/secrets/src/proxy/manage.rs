//! Looking after proxied accounts: onboarding one, writing the rules and the denials, issuing
//! and revoking DOC keys, and reading what was done (ADR-0014). Every one of these is a
//! control-plane act — it goes through core, it is audited, and it waits when core is away.

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::Utc;
use doc_plugin_sdk::protocol::Secret as Hidden;
use serde_json::{Value, json};
use uuid::Uuid;

use super::keys;
use super::rules::Rule;
use super::upstream::{self, Upstream};
use crate::ops::{self, Context};
use crate::store::{
    ACCOUNTS, ACTIVE, ALLOWANCES, Account, Allowance, CALLS, Call, DENIALS, Denial, KEYS, Key,
    PROXIED, REVOKED,
};
use crate::{CHANGED, Refusal};

/// The longest a DOC key may last. A key is DOC's own, so nothing at the vendor bounds it —
/// which means DOC has to.
const LONGEST: i64 = 366 * 24 * 60;
const SHORTEST: i64 = 5;

/// How long DOC waits when it tries a credential against the path the form named.
const TRYING: Duration = Duration::from_secs(20);

pub struct NewAccount {
    pub owner: String,
    pub name: String,
    pub title: String,
    pub config: BTreeMap<String, String>,
    pub credential: String,
}

/// Onboards a proxied account: one address, one credential, one way of applying it. There is no
/// per-vendor form, because there is no per-vendor anything.
pub async fn onboard(cx: &Context<'_>, new: NewAccount) -> Result<(Account, String), Refusal> {
    cx.writer()?;
    if new.owner.starts_with("user:") {
        return Err(Refusal::bad("a vendor account belongs to an organisation or a team"));
    }
    cx.managing(&new.owner)?;
    let name = ops::named(&new.name)?;
    // The name is half the address a caller writes, so it has to be one account's alone —
    // unlike an issuing account's, which is only ever unique to its owner.
    let taken = cx
        .store()
        .accounts()
        .await?
        .into_iter()
        .any(|held| held.vendor == PROXIED && held.name == name);
    if taken {
        return Err(Refusal::conflict(
            "another proxied account is called that, and callers address an account by its name",
        ));
    }
    let config = upstream::config(&new.config).map_err(Refusal::bad)?;
    let credential = ops::valued(&new.credential)?;
    let tried = try_credential(&config, &credential).await.map_err(Refusal::bad)?;
    let id = Uuid::now_v7();
    let sealed = ops::seal(cx.backend, &format!("account/{id}"), &credential).await?;
    let account: Account = cx
        .store()
        .insert(
            ACCOUNTS,
            json!({
                "id": id, "owner": new.owner, "name": name, "title": new.title.trim(),
                "vendor": PROXIED, "config": config, "sealed": sealed,
                "created_by": cx.me.login, "updated_by": cx.me.login,
            }),
        )
        .await?;
    cx.store().note(id, &cx.me.login, "onboarded", &tried).await;
    hosted(cx, &account, true).await;
    cx.audit(
        "account.onboarded",
        id,
        json!({ "name": account.name, "vendor": PROXIED, "owner": account.owner }),
    )
    .await;
    told(cx).await;
    Ok((account, tried))
}

/// Tries the credential against the path the form named, before anything is stored. A vendor
/// that refuses it says so here rather than in a log an hour later.
pub async fn try_credential(config: &Value, credential: &Hidden<String>) -> Result<String, String> {
    let upstream = Upstream::read(config)?;
    let probe = config["probe"].as_str().unwrap_or("/");
    let client = reqwest::Client::builder()
        .timeout(TRYING)
        .redirect(reqwest::redirect::Policy::none())
        .user_agent("doc-secrets")
        .build()
        .map_err(|err| format!("no HTTP client: {err}"))?;
    let mut asking = client.get(upstream.address(probe, "", credential));
    if let Some((name, value)) = upstream.header(credential) {
        asking = asking.header(name, value);
    }
    let answered = asking
        .send()
        .await
        .map_err(|err| format!("{} could not be reached: {err}", upstream.host))?;
    let status = answered.status();
    match status.as_u16() {
        401 | 403 => Err(format!("{} refused the credential ({status})", upstream.host)),
        code if code >= 400 => Err(format!(
            "{} answered {status} for {probe}; give a path it will answer",
            upstream.host
        )),
        _ => Ok(format!("{} answered {status} for {probe}", upstream.host)),
    }
}

pub async fn managed_proxied(cx: &Context<'_>, id: Uuid) -> Result<Account, Refusal> {
    let account = ops::managed(cx, id).await?;
    match account.vendor == PROXIED {
        true => Ok(account),
        false => Err(Refusal::bad("this account issues tokens rather than being proxied")),
    }
}

pub async fn test_account(cx: &Context<'_>, id: Uuid) -> Result<String, Refusal> {
    let account = managed_proxied(cx, id).await?;
    let credential = ops::credential(cx, &account).await?;
    try_credential(&account.config, &credential).await.map_err(Refusal::bad)
}

/// Writes the rules of an allowance. `sweeping` is the tick on the form: a rule that lets
/// through everything is a decision somebody makes out loud, not a default (§7).
pub async fn allow(
    cx: &Context<'_>,
    account: Uuid,
    who: &str,
    written: &[(String, String)],
    minutes: i64,
    sweeping: bool,
) -> Result<Allowance, Refusal> {
    cx.writer()?;
    let account = managed_proxied(cx, account).await?;
    if !covers_somebody(cx, who) {
        return Err(Refusal::bad("choose who the allowance is for"));
    }
    let rules = read(written, sweeping)?;
    let minutes = minutes.clamp(SHORTEST, LONGEST);
    let allowance: Allowance = cx
        .store()
        .insert(
            ALLOWANCES,
            json!({
                "id": Uuid::now_v7(), "account": account.id, "who": who,
                "who_label": label(cx, who), "grants": {},
                "rules": rules.iter().map(Rule::value).collect::<Vec<_>>(),
                "minutes": minutes, "created_by": cx.me.login,
            }),
        )
        .await?;
    let said = format!(
        "{} may {}",
        allowance.who_label,
        rules.iter().map(Rule::shown).collect::<Vec<_>>().join(", ")
    );
    cx.store().note(account.id, &cx.me.login, "allowed", &said).await;
    cx.audit(
        "allowance.made",
        account.id,
        json!({ "who": who, "rules": allowance.rules, "minutes": minutes }),
    )
    .await;
    told(cx).await;
    Ok(allowance)
}

/// The rules from what the form sent, as `(method, path)` pairs.
fn read(written: &[(String, String)], sweeping: bool) -> Result<Vec<Rule>, Refusal> {
    let mut rules: Vec<Rule> = Vec::new();
    for (method, path) in written {
        let (method, path) = (method.trim(), path.trim());
        if path.is_empty() {
            continue;
        }
        let rule = Rule::read(&json!({ "method": method, "path": path }))
            .ok_or_else(|| Refusal::bad(format!("{method} {path} is not a rule")))?;
        if rule.sweeping() && !sweeping {
            return Err(Refusal::bad(format!(
                "{} lets through everything; say so on the form if you mean it",
                rule.shown()
            )));
        }
        if !rules.contains(&rule) {
            rules.push(rule);
        }
    }
    match rules.is_empty() {
        true => Err(Refusal::bad("write at least one rule: nothing is allowed by default")),
        false => Ok(rules),
    }
}

/// Whether `who` names somebody there is. Beside the directory's own kinds, an allowance may
/// name a permission or an attribute, which RBAC answers rather than this plugin (§6).
fn covers_somebody(cx: &Context<'_>, who: &str) -> bool {
    match who.split_once(':') {
        Some(("permission", name)) => !name.trim().is_empty(),
        Some(("attribute", pair)) => pair.split_once('=').is_some_and(|(key, _)| !key.is_empty()),
        _ => ops::known(cx, who),
    }
}

fn label(cx: &Context<'_>, who: &str) -> String {
    match who.split_once(':') {
        Some(("permission", name)) => format!("whoever holds {name}"),
        Some(("attribute", pair)) => match pair.split_once('=') {
            Some((key, value)) => format!("whoever is {key} {value}"),
            None => who.to_string(),
        },
        _ => cx.directory.label(who),
    }
}

/// Writes a denial. One on the instance is a platform administrator's, or a holder of this
/// plugin's `admin`; one on an account is its manager's. Neither can be crossed by an allowance.
pub async fn deny(
    cx: &Context<'_>,
    account: Option<Uuid>,
    method: &str,
    path: &str,
    reason: &str,
) -> Result<Denial, Refusal> {
    cx.writer()?;
    match account {
        Some(account) => {
            managed_proxied(cx, account).await?;
        }
        None if !cx.me.admin => {
            return Err(Refusal::forbidden(
                "only whoever administers DOC writes a denial for every account",
            ));
        }
        None => {}
    }
    let rule = Rule::read(&json!({ "method": method, "path": path }))
        .ok_or_else(|| Refusal::bad("write the denial as a method and a path"))?;
    let denial: Denial = cx
        .store()
        .insert(
            DENIALS,
            json!({
                "id": Uuid::now_v7(), "account": account, "method": rule.method,
                "path": rule.path, "reason": reason.trim(), "created_by": cx.me.login,
            }),
        )
        .await?;
    if let Some(account) = account {
        cx.store().note(account, &cx.me.login, "denied", &rule.shown()).await;
    }
    cx.audit(
        "denial.written",
        account.unwrap_or(denial.id),
        json!({ "rule": rule.shown(), "account": account, "reason": denial.reason }),
    )
    .await;
    told(cx).await;
    Ok(denial)
}

pub async fn undeny(cx: &Context<'_>, id: Uuid) -> Result<Denial, Refusal> {
    cx.writer()?;
    let denial = cx.store().denial(id).await?;
    match denial.account {
        Some(account) => {
            managed_proxied(cx, account).await?;
        }
        None if !cx.me.admin => {
            return Err(Refusal::forbidden("only whoever administers DOC lifts that denial"));
        }
        None => {}
    }
    cx.store().delete(DENIALS, id).await?;
    cx.audit(
        "denial.lifted",
        denial.account.unwrap_or(denial.id),
        json!({ "rule": Rule::of(&denial).shown(), "account": denial.account }),
    )
    .await;
    told(cx).await;
    Ok(denial)
}

/// Issues a DOC key. Somebody an allowance covers asks for their own; whoever manages the
/// account issues one for somebody else. The value is answered once and never kept.
pub async fn issue(
    cx: &Context<'_>,
    account: Uuid,
    allowance: Uuid,
    subject: Option<&str>,
    purpose: &str,
    minutes: Option<i64>,
) -> Result<(Key, Hidden<String>), Refusal> {
    cx.writer()?;
    let account = managed_or_covered(cx, account, allowance, subject).await?;
    let held = cx.store().allowance(allowance).await?;
    if held.account != account.id {
        return Err(Refusal::bad("that allowance is on another account"));
    }
    let subject = subject.map(str::to_string).unwrap_or_else(|| cx.me.reference());
    let minutes = minutes.unwrap_or(held.minutes).clamp(SHORTEST, held.minutes.max(SHORTEST));
    let minted = keys::mint()?;
    let key: Key = cx
        .store()
        .insert(
            KEYS,
            json!({
                "id": Uuid::now_v7(), "account": account.id, "allowance": allowance,
                "subject": subject, "subject_label": cx.directory.label(&subject),
                "purpose": purpose.trim(), "shown": minted.shown, "digest": minted.digest,
                "expires_at": Utc::now() + chrono::Duration::minutes(minutes),
                "state": ACTIVE, "created_by": cx.me.login,
            }),
        )
        .await?;
    cx.store()
        .note(account.id, &cx.me.login, "issued", &format!("a key for {}", key.subject_label))
        .await;
    cx.audit(
        "key.issued",
        account.id,
        json!({ "key": key.id, "subject": key.subject, "allowance": allowance, "purpose": key.purpose }),
    )
    .await;
    told(cx).await;
    Ok((key, minted.value))
}

/// Whoever manages the account may issue a key for anybody; anybody else may issue their own,
/// and only where an allowance covers them.
async fn managed_or_covered(
    cx: &Context<'_>,
    account: Uuid,
    allowance: Uuid,
    subject: Option<&str>,
) -> Result<Account, Refusal> {
    if let Ok(managed) = managed_proxied(cx, account).await {
        return Ok(managed);
    }
    if subject.is_some() {
        return Err(Refusal::forbidden(
            "only whoever manages the account issues a key for somebody else",
        ));
    }
    let held = cx.store().allowance(allowance).await?;
    let covered = match held.who.split_once(':') {
        // A permission or an attribute is RBAC's answer, and asking core per call is the
        // addition ADR-0014 leaves open — so it is settled here, once, when the key is made.
        Some(("permission", name)) => cx.backend.allows(name.trim(), false),
        Some(("attribute", pair)) => match pair.split_once('=') {
            Some((key, value)) => cx.backend.attribute(key.trim()) == Some(value.trim()),
            None => false,
        },
        _ => cx.me.covered(&cx.directory, &held.who),
    };
    match covered {
        true => managed_account_of(cx, account).await,
        false => Err(Refusal::forbidden("no allowance on this account covers you")),
    }
}

async fn managed_account_of(cx: &Context<'_>, id: Uuid) -> Result<Account, Refusal> {
    let account = cx.store().account(id).await?;
    match account.vendor == PROXIED {
        true => Ok(account),
        false => Err(Refusal::bad("this account issues tokens rather than being proxied")),
    }
}

/// Ends a key. Whoever manages the account, or whoever it was issued to.
pub async fn revoke(cx: &Context<'_>, id: Uuid, why: &str) -> Result<Key, Refusal> {
    cx.writer()?;
    let key = cx.store().key(id).await?;
    let theirs = key.subject == cx.me.reference();
    if !theirs {
        managed_proxied(cx, key.account).await?;
    }
    let key: Key = cx
        .store()
        .update(
            KEYS,
            id,
            json!({ "state": REVOKED, "revoked_at": Utc::now(), "revoked_by": cx.me.login }),
        )
        .await?;
    cx.store()
        .note(key.account, &cx.me.login, "revoked", &format!("{}'s key: {why}", key.subject_label))
        .await;
    cx.audit("key.revoked", key.account, json!({ "key": key.id, "why": why })).await;
    told(cx).await;
    Ok(key)
}

/// What was done through an account, for whoever manages it; or what somebody did, for
/// themselves. The audit is read here and written only by the proxy.
pub async fn log(
    cx: &Context<'_>,
    account: Option<Uuid>,
    limit: u32,
) -> Result<Vec<Call>, Refusal> {
    let filter = match account {
        // Whoever manages the account reads all of it; anybody else reads what they did with
        // it, which is the half of the audit that is about them.
        Some(account) => match managed_proxied(cx, account).await {
            Ok(_) => json!({ "account": account }),
            Err(_) => json!({ "account": account, "who": cx.me.reference() }),
        },
        None => json!({ "who": cx.me.reference() }),
    };
    cx.store().calls(filter, limit.clamp(1, 500)).await
}

/// Everything an account's page needs about what it is: where its calls go, and how.
pub fn about(account: &Account) -> String {
    upstream::describe(&account.config)
}

/// Says that a proxied account's host should exist, or should not any more (ADR-0015 §4). It is
/// an announcement rather than a write: this plugin does not own DOC's names, and a deployment
/// that keeps them by hand simply has nothing listening.
pub(crate) async fn hosted(cx: &Context<'_>, account: &Account, there: bool) {
    let configured = super::Configured::read(cx.backend);
    if configured.zone.is_empty() {
        return;
    }
    let host = format!("{}.{}", account.name, configured.zone);
    let topic = match there {
        true => crate::HOSTED,
        false => crate::UNHOSTED,
    };
    let payload = json!({
        "account": account.name,
        "host": host,
        "points_at": configured.public_host(),
    });
    if let Err(err) = cx.backend.publish(topic, payload).await {
        tracing::warn!(%err, %host, "an account's host was not announced; it has to be named by hand");
    }
}
/// Tells every replica that what the proxy serves from has changed, so a revocation lands at
/// once rather than at the end of a refresh interval (ADR-0011).
pub(crate) async fn told(cx: &Context<'_>) {
    if let Err(err) = cx.backend.publish(CHANGED, json!({})).await {
        tracing::warn!(%err, "the proxies were not told; they will read it again shortly");
    }
}

/// How long the calls are kept. Because DOC's record is the counterpart of the vendor's (§7),
/// keeping it for less time than the vendor keeps theirs makes the join fail exactly when
/// somebody is looking, so the floor is a year and a day.
pub const KEPT_DAYS: i64 = 400;

/// Rolls off records past their time. Called from the daily schedule, and it is the only thing
/// that ever deletes one.
pub async fn roll_off(backend: &doc_plugin_sdk::Backend) -> Result<u64, Refusal> {
    let before = Utc::now() - chrono::Duration::days(KEPT_DAYS);
    let gone = backend
        .delete_where(CALLS, "id", json!({ "at": { "$lt": before } }))
        .await
        .map_err(Refusal::from)?;
    Ok(gone as u64)
}
