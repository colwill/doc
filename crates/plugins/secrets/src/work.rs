//! What runs by itself: kept tokens renewed before they end, tokens and DOC keys past their time
//! marked so, managers warned before a secret expires, values sealed again once a newer key
//! comes, and the proxy's log rolled off once it is older than anybody needs it.

use chrono::{Duration, Utc};
use doc_plugin_sdk::Backend;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::directory::Directory;
use crate::ops;
use crate::proxy::manage;
use crate::store::{ACCOUNTS, ACTIVE, EXPIRED, KEYS, SECRETS, Secret, Store, TOKENS};

/// Every few minutes: renewals first, then tokens that have run out.
pub async fn renew(backend: &Backend) -> Result<Value, Refusal> {
    let store = Store(backend);
    let (mut renewed, mut failed) = (0, 0);
    for secret in store.secrets().await?.into_iter().filter(|secret| secret.kept.is_some()) {
        match ops::renew(backend, &secret).await {
            Ok(true) => renewed += 1,
            Ok(false) => {}
            Err(why) => {
                failed += 1;
                tracing::warn!(secret = %secret.id, %why, "a kept token was not renewed");
            }
        }
    }
    let mut expired = 0;
    for token in
        store.tokens(json!({ "state": ACTIVE, "expires_at": { "lt": Utc::now() } })).await?
    {
        let _ = backend
            .update::<Value>(TOKENS, json!(token.id), json!({ "state": EXPIRED }), None)
            .await;
        expired += 1;
    }
    // A key past its time is refused by the proxy whatever this says — it checks the time
    // itself — but the row should read as what it is.
    let mut ended = 0;
    for key in store.keys(json!({ "state": ACTIVE, "expires_at": { "lt": Utc::now() } })).await? {
        let _ =
            backend.update::<Value>(KEYS, json!(key.id), json!({ "state": EXPIRED }), None).await;
        ended += 1;
    }
    Ok(json!({ "renewed": renewed, "failed": failed, "expired": expired, "keys_ended": ended }))
}

/// The days before expiry a secret's managers are warned: two weeks, the day before, the day.
const WARNINGS: [i64; 3] = [14, 1, 0];

/// Who is told a secret is expiring: the person who owns it, a team's lead or the nearest lead
/// above it, and for an organisation's, whoever last changed it.
fn told(directory: &Directory, secret: &Secret) -> Vec<Uuid> {
    let by_login = |login: &Option<String>| {
        login.as_ref().and_then(|login| {
            directory.people.values().find(|person| &person.login == login).map(|person| person.id)
        })
    };
    let (kind, id) = secret.owner.split_once(':').unwrap_or_default();
    let id = id.parse::<Uuid>().ok();
    let mut told = match (kind, id) {
        ("user", Some(user)) => vec![user],
        ("team", Some(team)) => directory.lead_for(team).into_iter().collect(),
        _ => Vec::new(),
    };
    if told.is_empty() {
        told.extend(by_login(&secret.updated_by).or_else(|| by_login(&secret.created_by)));
    }
    told
}

/// Once a day: expiry warnings, and every value sealed again that a rotation left under an older key.
pub async fn daily(backend: &Backend) -> Result<Value, Refusal> {
    let store = Store(backend);
    let directory = Directory::read(backend).await?;
    let mut warned = 0;
    for secret in store.secrets().await? {
        let Some(ends) = secret.expires_at else { continue };
        let left = (ends - Utc::now()).num_days().max(0);
        let Some(at) = WARNINGS.iter().copied().find(|warning| left <= *warning) else { continue };
        if secret.warned.is_some_and(|already| already <= at) || secret.kept.is_some() {
            continue;
        }
        let when = match left {
            0 => "today".to_string(),
            1 => "tomorrow".to_string(),
            days => format!("in {days} days"),
        };
        let title = format!("{} expires {when}", secret.name);
        let body = "Replace its value in Secret Storage, and every plugin using it follows.";
        for user in told(&directory, &secret) {
            let _ = backend.notify(&user.to_string(), &title, body, Some(&secret.href())).await;
        }
        let _ =
            backend.update::<Value>(SECRETS, json!(secret.id), json!({ "warned": at }), None).await;
        warned += 1;
    }
    let mut resealed = 0;
    for secret in store.secrets().await? {
        if let (Some(sealed), true) = (
            &secret.sealed,
            secret.expires_at.is_none_or(|ends| ends > Utc::now() - Duration::days(30)),
        ) {
            resealed += again(backend, SECRETS, secret.id, &secret.label(), sealed).await;
        }
    }
    for account in store.accounts().await? {
        if let Some(sealed) = &account.sealed {
            resealed += again(backend, ACCOUNTS, account.id, &account.label(), sealed).await;
        }
    }
    let rolled = match manage::roll_off(backend).await {
        Ok(rolled) => rolled,
        Err(why) => {
            tracing::warn!(%why, "the proxy's log was not rolled off");
            0
        }
    };
    Ok(json!({ "warned": warned, "resealed": resealed, "calls_rolled_off": rolled }))
}

/// Seals a value again when it is under an older key than the current one.
async fn again(
    backend: &Backend,
    collection: &str,
    id: Uuid,
    label: &str,
    sealed: &doc_plugin_sdk::SealedValue,
) -> usize {
    let Ok((value, true)) = backend.open(label, sealed).await else { return 0 };
    match backend.seal(label, &value).await {
        Ok(fresh) => {
            let _ = backend
                .update::<Value>(collection, json!(id), json!({ "sealed": fresh }), None)
                .await;
            1
        }
        Err(err) => {
            tracing::warn!(%err, %id, "a value was not sealed again under the newest key");
            0
        }
    }
}
