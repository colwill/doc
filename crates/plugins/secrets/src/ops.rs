//! What can be done, however it is asked for: pages, the API, core and other plugins all come
//! through here, so each rule about who may do what is written once.

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::protocol::Secret as Hidden;
use doc_plugin_sdk::{Backend, SealedValue};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::directory::{Directory, Me};
use crate::store::{
    ACCOUNTS, ACTIVE, ALLOWANCES, Account, Allowance, Kept, REVOKED, SECRETS, Secret, Store,
    TOKENS, Token,
};
use crate::vendors::{self, Issued, Vendor};
use crate::{ID, Refusal};

/// The longest value kept, which a private key or a certificate chain fits in.
const MAX_VALUE: usize = 64 * 1024;

/// Who is asking and who is who, read once for the request.
pub struct Context<'a> {
    pub backend: &'a Backend,
    pub me: Me,
    pub directory: Directory,
}

impl<'a> Context<'a> {
    pub async fn read(backend: &'a Backend) -> Result<Self, Refusal> {
        Ok(Self { backend, me: Me::of(backend), directory: Directory::read(backend).await? })
    }

    pub fn store(&self) -> Store<'a> {
        Store(self.backend)
    }

    pub fn writer(&self) -> Result<(), Refusal> {
        match self.me.writes || self.me.admin {
            true => Ok(()),
            false => Err(Refusal::forbidden("that needs plugin:secrets:user:rw")),
        }
    }

    pub fn managing(&self, owner: &str) -> Result<(), Refusal> {
        match self.me.manages(&self.directory, owner) {
            true => Ok(()),
            false => Err(Refusal::forbidden(format!(
                "only whoever manages {}'s secrets may do that",
                self.directory.owner_name(owner)
            ))),
        }
    }

    /// What a secret is called wherever it is chosen: its owner and its name.
    pub fn label(&self, secret: &Secret) -> String {
        format!("{}: {}", self.directory.owner_name(&secret.owner), secret.name)
    }

    pub async fn audit(&self, action: &str, subject: Uuid, detail: Value) {
        if let Err(err) = self.backend.audit(action, Some(&subject.to_string()), detail).await {
            tracing::warn!(%err, action, "not audited");
        }
    }
}

pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    !name.is_empty()
        && name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn named(name: &str) -> Result<String, Refusal> {
    let name = name.trim().to_ascii_lowercase();
    match valid_name(&name) {
        true => Ok(name),
        false => Err(Refusal::bad(
            "a name is up to 64 of a-z, 0-9 and -, starting with a letter or a digit",
        )),
    }
}

pub fn valued(value: &str) -> Result<Hidden<String>, Refusal> {
    let value = value.trim_end_matches(['\r', '\n']);
    match (value.trim().is_empty(), value.len() > MAX_VALUE) {
        (true, _) => Err(Refusal::bad("give the value to keep")),
        (_, true) => Err(Refusal::bad("a value is at most 64 KiB")),
        _ => Ok(Hidden::new(value.to_string())),
    }
}

pub async fn seal(
    backend: &Backend,
    label: &str,
    value: &Hidden<String>,
) -> Result<SealedValue, Refusal> {
    backend.seal(label, value).await.map_err(|err| {
        Refusal::unavailable(format!("core could not seal the value: {}", err.detail()))
    })
}

/// A sealed value opened, sealed again when a newer key has come since.
pub async fn open(
    backend: &Backend,
    collection: &str,
    id: Uuid,
    label: &str,
    sealed: Option<&SealedValue>,
) -> Result<Hidden<String>, String> {
    let sealed = sealed.ok_or("it has no value")?;
    let (value, stale) = backend.open(label, sealed).await.map_err(|err| err.detail())?;
    if stale {
        match backend.seal(label, &value).await {
            Ok(fresh) => {
                let _ = backend
                    .update::<Value>(collection, json!(id), json!({ "sealed": fresh }), None)
                    .await;
            }
            Err(err) => {
                tracing::warn!(%err, %id, "a value was not sealed again under the newest key")
            }
        }
    }
    Ok(value)
}

/// Tells core which secrets changed, so every plugin using one reads its settings again.
pub async fn changed(backend: &Backend, secrets: Vec<Uuid>) {
    if let Err(err) = backend.secrets_changed(secrets, false).await {
        tracing::warn!(%err, "core was not told secrets changed; plugins see them at their next load");
    }
}

pub struct NewSecret {
    pub owner: String,
    pub name: String,
    pub title: String,
    pub description: String,
    pub value: String,
    pub expires_at: Option<DateTime<Utc>>,
    pub plugins: Vec<String>,
}

fn plugins_known(cx: &Context<'_>, plugins: &[String]) -> Result<Vec<String>, Refusal> {
    let mut known = Vec::new();
    for plugin in plugins.iter().map(|plugin| plugin.trim()).filter(|plugin| !plugin.is_empty()) {
        if plugin == ID {
            return Err(Refusal::bad("Secret Storage keeps its own credentials itself"));
        }
        if !cx.directory.plugins.iter().any(|found| found.id == plugin) {
            return Err(Refusal::bad(format!("there is no plugin called {plugin}")));
        }
        if !known.iter().any(|held: &String| held == plugin) {
            known.push(plugin.to_string());
        }
    }
    Ok(known)
}

pub async fn store_secret(cx: &Context<'_>, new: NewSecret) -> Result<Secret, Refusal> {
    cx.writer()?;
    cx.managing(&new.owner)?;
    let name = named(&new.name)?;
    let value = valued(&new.value)?;
    let plugins = plugins_known(cx, &new.plugins)?;
    let id = Uuid::now_v7();
    let sealed = seal(cx.backend, &format!("secret/{id}"), &value).await?;
    let secret: Secret = cx
        .store()
        .insert(
            SECRETS,
            json!({
                "id": id, "owner": new.owner, "name": name, "title": new.title.trim(),
                "description": new.description.trim(), "plugins": plugins, "sealed": sealed,
                "version": 1, "expires_at": new.expires_at, "used": {},
                "created_by": cx.me.login, "updated_by": cx.me.login,
            }),
        )
        .await?;
    cx.store().note(id, &cx.me.login, "stored", &format!("shared with {}", shown(&plugins))).await;
    cx.audit(
        "secret.stored",
        id,
        json!({ "name": secret.name, "owner": secret.owner, "plugins": plugins }),
    )
    .await;
    Ok(secret)
}

fn shown(plugins: &[String]) -> String {
    match plugins.is_empty() {
        true => "no plugin yet".to_string(),
        false => plugins.join(", "),
    }
}

/// The secret, for somebody who manages it.
pub async fn managed_secret(cx: &Context<'_>, id: Uuid) -> Result<Secret, Refusal> {
    let secret = cx.store().secret(id).await?;
    cx.managing(&secret.owner)?;
    Ok(secret)
}

pub async fn replace_value(cx: &Context<'_>, id: Uuid, value: &str) -> Result<Secret, Refusal> {
    cx.writer()?;
    let secret = managed_secret(cx, id).await?;
    if secret.kept.is_some() {
        return Err(Refusal::conflict(
            "this secret is a token kept for a plugin: it is renewed, not typed",
        ));
    }
    let value = valued(value)?;
    let sealed = seal(cx.backend, &secret.label(), &value).await?;
    let version = secret.version.max(1) + 1;
    let set =
        json!({ "sealed": sealed, "version": version, "updated_by": cx.me.login, "warned": null });
    let secret: Secret = cx.store().update(SECRETS, id, set).await?;
    cx.store().note(id, &cx.me.login, "replaced", &format!("version {version}")).await;
    cx.audit("secret.replaced", id, json!({ "name": secret.name, "version": version })).await;
    changed(cx.backend, vec![id]).await;
    Ok(secret)
}

pub async fn change_secret(
    cx: &Context<'_>,
    id: Uuid,
    title: &str,
    description: &str,
    expires_at: Option<DateTime<Utc>>,
) -> Result<Secret, Refusal> {
    cx.writer()?;
    managed_secret(cx, id).await?;
    let set = json!({
        "title": title.trim(), "description": description.trim(), "expires_at": expires_at,
        "warned": null, "updated_by": cx.me.login,
    });
    let secret: Secret = cx.store().update(SECRETS, id, set).await?;
    cx.store().note(id, &cx.me.login, "changed", "its title, description or expiry").await;
    Ok(secret)
}

pub async fn share(
    cx: &Context<'_>,
    id: Uuid,
    plugin: &str,
    sharing: bool,
) -> Result<Secret, Refusal> {
    cx.writer()?;
    let secret = managed_secret(cx, id).await?;
    let mut plugins = secret.plugins.clone();
    match sharing {
        true => {
            let plugin = plugins_known(cx, &[plugin.to_string()])?.remove(0);
            if !plugins.contains(&plugin) {
                plugins.push(plugin);
            }
        }
        false => plugins.retain(|held| held != plugin),
    }
    let secret: Secret = cx.store().update(SECRETS, id, json!({ "plugins": plugins })).await?;
    let action = if sharing { "shared" } else { "unshared" };
    let said = if sharing {
        format!("shared with {plugin}")
    } else {
        format!("no longer shared with {plugin}")
    };
    cx.store().note(id, &cx.me.login, action, &said).await;
    cx.audit(&format!("secret.{action}"), id, json!({ "name": secret.name, "plugin": plugin }))
        .await;
    changed(cx.backend, vec![id]).await;
    Ok(secret)
}

pub async fn delete_secret(cx: &Context<'_>, id: Uuid) -> Result<Secret, Refusal> {
    cx.writer()?;
    let secret = managed_secret(cx, id).await?;
    if let Some(kept) = &secret.kept
        && let Some(token) = kept.token
    {
        let _ = end_token(cx, token, "its secret was deleted").await;
    }
    cx.store().delete(SECRETS, id).await?;
    cx.audit("secret.deleted", id, json!({ "name": secret.name, "owner": secret.owner })).await;
    changed(cx.backend, vec![id]).await;
    Ok(secret)
}

pub struct NewAccount {
    pub owner: String,
    pub name: String,
    pub title: String,
    pub vendor: String,
    pub config: std::collections::BTreeMap<String, String>,
    pub credential: String,
}

pub fn vendor_of(account: &Account) -> Result<Vendor, Refusal> {
    Vendor::parse(&account.vendor)
        .ok_or_else(|| Refusal::unavailable("this account's vendor is not one DOC knows"))
}

pub async fn onboard(cx: &Context<'_>, new: NewAccount) -> Result<(Account, String), Refusal> {
    cx.writer()?;
    if new.owner.starts_with("user:") {
        return Err(Refusal::bad("a vendor account belongs to an organisation or a team"));
    }
    cx.managing(&new.owner)?;
    let vendor = Vendor::parse(&new.vendor).ok_or_else(|| Refusal::bad("choose a vendor"))?;
    let name = named(&new.name)?;
    let config = vendor.config(&new.config).map_err(Refusal::bad)?;
    let credential = valued(&new.credential)?;
    let tried = vendors::test(vendor, &config, &credential).await.map_err(Refusal::bad)?;
    let id = Uuid::now_v7();
    let sealed = seal(cx.backend, &format!("account/{id}"), &credential).await?;
    let account: Account = cx
        .store()
        .insert(
            ACCOUNTS,
            json!({
                "id": id, "owner": new.owner, "name": name, "title": new.title.trim(),
                "vendor": vendor.id(), "config": config, "sealed": sealed,
                "created_by": cx.me.login, "updated_by": cx.me.login,
            }),
        )
        .await?;
    cx.store().note(id, &cx.me.login, "onboarded", &tried).await;
    cx.audit(
        "account.onboarded",
        id,
        json!({ "name": account.name, "vendor": vendor.id(), "owner": account.owner }),
    )
    .await;
    Ok((account, tried))
}

pub async fn managed_account(cx: &Context<'_>, id: Uuid) -> Result<(Account, Vendor), Refusal> {
    let account = managed(cx, id).await?;
    let vendor = vendor_of(&account)?;
    Ok((account, vendor))
}

/// An account whoever is asking manages, whichever kind it is.
pub async fn managed(cx: &Context<'_>, id: Uuid) -> Result<Account, Refusal> {
    let account = cx.store().account(id).await?;
    cx.managing(&account.owner)?;
    Ok(account)
}

pub async fn credential(cx: &Context<'_>, account: &Account) -> Result<Hidden<String>, Refusal> {
    open(cx.backend, ACCOUNTS, account.id, &account.label(), account.sealed.as_ref()).await.map_err(
        |why| Refusal::unavailable(format!("the account's credential could not be opened: {why}")),
    )
}

pub async fn test_account(cx: &Context<'_>, id: Uuid) -> Result<String, Refusal> {
    let (account, vendor) = managed_account(cx, id).await?;
    let credential = credential(cx, &account).await?;
    vendors::test(vendor, &account.config, &credential).await.map_err(Refusal::bad)
}

pub async fn replace_credential(
    cx: &Context<'_>,
    id: Uuid,
    given: &str,
) -> Result<String, Refusal> {
    cx.writer()?;
    let (account, vendor) = managed_account(cx, id).await?;
    let value = valued(given)?;
    let tried = vendors::test(vendor, &account.config, &value).await.map_err(Refusal::bad)?;
    let sealed = seal(cx.backend, &account.label(), &value).await?;
    let _: Account = cx
        .store()
        .update(ACCOUNTS, id, json!({ "sealed": sealed, "updated_by": cx.me.login }))
        .await?;
    cx.store().note(id, &cx.me.login, "replaced", &tried).await;
    cx.audit("account.replaced", id, json!({ "name": account.name })).await;
    Ok(tried)
}

pub async fn remove_account(cx: &Context<'_>, id: Uuid) -> Result<Account, Refusal> {
    cx.writer()?;
    let account = managed(cx, id).await?;
    let live = cx.store().tokens(json!({ "account": id, "state": ACTIVE })).await?;
    for token in live {
        let _ = end_token(cx, token.id, "its account was removed").await;
    }
    let kept: Vec<Uuid> = cx
        .store()
        .secrets()
        .await?
        .into_iter()
        .filter(|secret| secret.kept.as_ref().is_some_and(|kept| kept.account == id))
        .map(|secret| secret.id)
        .collect();
    for secret in &kept {
        cx.store().delete(SECRETS, *secret).await?;
    }
    cx.store().delete(ACCOUNTS, id).await?;
    cx.audit(
        "account.removed",
        id,
        json!({ "name": account.name, "kept_secrets_removed": kept.len() }),
    )
    .await;
    if !kept.is_empty() {
        changed(cx.backend, kept).await;
    }
    // A proxied account is addressed by name, so its host goes with it, and every replica has to
    // stop serving it now rather than at the end of a refresh.
    if account.vendor == crate::store::PROXIED {
        crate::proxy::manage::hosted(cx, &account, false).await;
        crate::proxy::manage::told(cx).await;
    }
    Ok(account)
}

/// Whether `who` names someone or something there is: a team, person, service account, plugin
/// or organisation.
pub fn known(cx: &Context<'_>, who: &str) -> bool {
    let Some((kind, id)) = who.split_once(':') else { return false };
    let uuid = id.parse::<Uuid>().ok();
    match kind {
        "team" => uuid.and_then(|id| cx.directory.team(id)).is_some(),
        "user" => uuid.is_some_and(|id| cx.directory.people.contains_key(&id)),
        "service" => {
            uuid.is_some_and(|id| cx.directory.services.iter().any(|found| found.id == id))
        }
        "organisation" => {
            uuid.is_some_and(|id| cx.directory.organisations.iter().any(|found| found.id == id))
        }
        "plugin" => id != ID && cx.directory.plugins.iter().any(|found| found.id == id),
        _ => false,
    }
}

pub async fn allow(
    cx: &Context<'_>,
    account: Uuid,
    who: &str,
    given: &std::collections::BTreeMap<String, String>,
    minutes: i64,
) -> Result<Allowance, Refusal> {
    cx.writer()?;
    let (account, vendor) = managed_account(cx, account).await?;
    if !known(cx, who) {
        return Err(Refusal::bad("choose who the allowance is for"));
    }
    let grants = vendor.grants(&account.config, given).map_err(Refusal::bad)?;
    let (least, most) = vendor.lifetime();
    let minutes = minutes.clamp(least, most);
    let allowance: Allowance = cx
        .store()
        .insert(
            ALLOWANCES,
            json!({
                "id": Uuid::now_v7(), "account": account.id, "who": who,
                "who_label": cx.directory.label(who), "grants": grants, "minutes": minutes,
                "created_by": cx.me.login,
            }),
        )
        .await?;
    let said = format!("{} may ask for {}", allowance.who_label, vendor.describe(&grants));
    cx.store().note(account.id, &cx.me.login, "allowed", &said).await;
    cx.audit(
        "allowance.made",
        account.id,
        json!({ "who": who, "grants": grants, "minutes": minutes }),
    )
    .await;
    Ok(allowance)
}

pub async fn disallow(cx: &Context<'_>, id: Uuid) -> Result<Allowance, Refusal> {
    cx.writer()?;
    let allowance = cx.store().allowance(id).await?;
    // Ownership alone: an allowance sits on accounts of both kinds, and a proxied one has no
    // vendor for `managed_account` to parse.
    managed(cx, allowance.account).await?;
    cx.store().delete(ALLOWANCES, id).await?;
    let said = format!("{} may no longer ask", allowance.who_label);
    cx.store().note(allowance.account, &cx.me.login, "disallowed", &said).await;
    cx.audit("allowance.removed", allowance.account, json!({ "who": allowance.who })).await;
    crate::proxy::manage::told(cx).await;
    Ok(allowance)
}

/// The allowances on an account that cover whoever is asking.
pub async fn covering(cx: &Context<'_>, account: Uuid) -> Result<Vec<Allowance>, Refusal> {
    let allowances = cx.store().allowances(Some(account)).await?;
    Ok(allowances
        .into_iter()
        .filter(|allowance| cx.me.covered(&cx.directory, &allowance.who))
        .collect())
}

/// Accounts somebody may see: those they manage, and those with an allowance covering them.
pub async fn visible_accounts(cx: &Context<'_>) -> Result<Vec<(Account, bool)>, Refusal> {
    let allowances = cx.store().allowances(None).await?;
    let mut shown = Vec::new();
    for account in cx.store().accounts().await? {
        let manages = cx.me.manages(&cx.directory, &account.owner);
        let covered = allowances.iter().any(|allowance| {
            allowance.account == account.id && cx.me.covered(&cx.directory, &allowance.who)
        });
        if manages || covered {
            shown.push((account, manages));
        }
    }
    shown.sort_by_key(|(account, _)| account.shown().to_lowercase());
    Ok(shown)
}

fn subject(me: &Me) -> String {
    let login: String = me
        .login
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' })
        .collect();
    format!("doc-{login}")
}

/// What an allowance lets be asked for, checked, and how long for.
fn narrowed(
    vendor: Vendor,
    allowance: &Allowance,
    asked: &Value,
    minutes: i64,
) -> Result<(Value, i64), Refusal> {
    let restrictions = vendor.within(&allowance.grants, asked).map_err(Refusal::bad)?;
    let (least, most) = vendor.lifetime();
    Ok((restrictions, minutes.clamp(least, allowance.minutes.min(most).max(least))))
}

async fn record(
    cx: &Context<'_>,
    account: &Account,
    allowance: &Allowance,
    restrictions: &Value,
    issued: &Issued,
    purpose: &str,
    kept_in: Option<Uuid>,
) -> Result<Token, Refusal> {
    cx.store()
        .insert(
            TOKENS,
            json!({
                "id": Uuid::now_v7(), "account": account.id, "allowance": allowance.id,
                "asked_by": cx.me.reference(), "asked_by_label": cx.me.login, "purpose": purpose,
                "restrictions": restrictions, "vendor_id": issued.vendor_id,
                "expires_at": issued.expires_at, "state": ACTIVE, "kept_in": kept_in,
            }),
        )
        .await
}

/// A token for whoever is asking, within an allowance covering them, shown to them once.
pub async fn issue(
    cx: &Context<'_>,
    account: Uuid,
    allowance: Uuid,
    asked: &Value,
    minutes: i64,
    purpose: &str,
) -> Result<(Token, Hidden<String>), Refusal> {
    let account = cx.store().account(account).await?;
    let vendor = vendor_of(&account)?;
    let allowance = cx.store().allowance(allowance).await?;
    if allowance.account != account.id || !cx.me.covered(&cx.directory, &allowance.who) {
        return Err(Refusal::forbidden("no allowance on this account covers you"));
    }
    let (restrictions, minutes) = narrowed(vendor, &allowance, asked, minutes)?;
    let credential = credential(cx, &account).await?;
    let purpose = match purpose.trim() {
        "" => format!("DOC, for {}", cx.me.login),
        given => {
            format!("DOC, for {}: {}", cx.me.login, given.chars().take(120).collect::<String>())
        }
    };
    let issued = vendors::issue(
        vendor,
        &account.config,
        &credential,
        &restrictions,
        minutes,
        &subject(&cx.me),
        &purpose,
    )
    .await
    .map_err(Refusal::unavailable)?;
    let token = record(cx, &account, &allowance, &restrictions, &issued, &purpose, None).await?;
    let said = format!("for {}: {}", cx.me.login, vendor.describe(&restrictions));
    cx.store().note(account.id, &cx.me.login, "issued", &said).await;
    cx.audit(
        "token.issued",
        account.id,
        json!({ "token": token.id, "restrictions": restrictions, "expires_at": issued.expires_at }),
    )
    .await;
    Ok((token, issued.value))
}

pub struct Keeping {
    pub account: Uuid,
    pub allowance: Uuid,
    pub asked: Value,
    pub minutes: i64,
    pub renew: bool,
    pub name: String,
}

/// A token kept in a secret of its own for the plugin an allowance names, never shown to anybody.
pub async fn keep(cx: &Context<'_>, keeping: Keeping) -> Result<Secret, Refusal> {
    cx.writer()?;
    let (account, vendor) = managed_account(cx, keeping.account).await?;
    if !vendor.keepable() {
        return Err(Refusal::bad(format!(
            "a token from {} is three values, not one a setting can hold",
            vendor.name()
        )));
    }
    let allowance = cx.store().allowance(keeping.allowance).await?;
    let plugin = match allowance.who.split_once(':') {
        Some(("plugin", plugin)) if allowance.account == account.id => plugin.to_string(),
        _ => return Err(Refusal::bad("choose an allowance naming the plugin the token is for")),
    };
    let (restrictions, minutes) = narrowed(vendor, &allowance, &keeping.asked, keeping.minutes)?;
    let name = named(&keeping.name)?;
    let credential = credential(cx, &account).await?;
    let purpose = format!("DOC, kept for the {plugin} plugin");
    let issued = vendors::issue(
        vendor,
        &account.config,
        &credential,
        &restrictions,
        minutes,
        &format!("doc-plugin-{plugin}"),
        &purpose,
    )
    .await
    .map_err(Refusal::unavailable)?;
    let id = Uuid::now_v7();
    let sealed = seal(cx.backend, &format!("secret/{id}"), &issued.value).await?;
    let token =
        record(cx, &account, &allowance, &restrictions, &issued, &purpose, Some(id)).await?;
    let kept = Kept {
        account: account.id,
        allowance: allowance.id,
        restrictions: restrictions.clone(),
        minutes,
        renew: keeping.renew,
        token: Some(token.id),
        previous: None,
        problem: None,
    };
    let secret: Secret = cx
        .store()
        .insert(
            SECRETS,
            json!({
                "id": id, "owner": account.owner, "name": name,
                "title": format!("{} token for {plugin}", account.shown()),
                "description": vendor.describe(&restrictions), "plugins": [plugin],
                "sealed": sealed, "version": 1, "expires_at": issued.expires_at, "kept": kept,
                "used": {}, "created_by": cx.me.login, "updated_by": cx.me.login,
            }),
        )
        .await?;
    cx.store()
        .note(id, &cx.me.login, "kept", &format!("a token from {} for {plugin}", account.shown()))
        .await;
    cx.store()
        .note(
            account.id,
            &cx.me.login,
            "kept",
            &format!("a token for {plugin}: {}", vendor.describe(&restrictions)),
        )
        .await;
    cx.audit(
        "token.kept",
        account.id,
        json!({ "token": token.id, "secret": id, "plugin": plugin }),
    )
    .await;
    Ok(secret)
}

/// Ends a token at the vendor where it can be, and marks it revoked.
async fn end_token(cx: &Context<'_>, id: Uuid, why: &str) -> Result<Token, Refusal> {
    let token = cx.store().token(id).await?;
    if !token.live() {
        return Ok(token);
    }
    let account = cx.store().account(token.account).await?;
    let vendor = vendor_of(&account)?;
    let credential = credential(cx, &account).await?;
    let value = match token.kept_in {
        Some(secret) => {
            let kept = cx.store().secret(secret).await.ok();
            match kept {
                Some(kept) => {
                    open(cx.backend, SECRETS, kept.id, &kept.label(), kept.sealed.as_ref())
                        .await
                        .ok()
                }
                None => None,
            }
        }
        None => None,
    };
    vendors::revoke(
        vendor,
        &account.config,
        &credential,
        token.vendor_id.as_deref(),
        value.as_ref(),
    )
    .await
    .map_err(Refusal::conflict)?;
    let set = json!({ "state": REVOKED, "revoked_at": Utc::now(), "revoked_by": cx.me.login });
    let token: Token = cx.store().update(TOKENS, id, set).await?;
    cx.store()
        .note(
            account.id,
            &cx.me.login,
            "revoked",
            &format!("a token for {}: {why}", token.asked_by_label),
        )
        .await;
    cx.audit("token.revoked", account.id, json!({ "token": id, "why": why })).await;
    Ok(token)
}

/// Revokes a token for whoever asked for it, whoever manages its account, or an administrator.
pub async fn revoke(cx: &Context<'_>, id: Uuid) -> Result<Token, Refusal> {
    let token = cx.store().token(id).await?;
    let account = cx.store().account(token.account).await?;
    let theirs = token.asked_by == cx.me.reference();
    if !theirs && !cx.me.manages(&cx.directory, &account.owner) {
        return Err(Refusal::forbidden(
            "only whoever asked for a token, or manages its account, revokes it",
        ));
    }
    end_token(cx, id, "revoked by hand").await
}

/// Core asking for the values of secrets a plugin's settings point at: each one given only if it
/// is shared with that plugin, and said why otherwise.
pub async fn resolve(backend: &Backend, plugin: &str, ids: &[Uuid]) -> Result<Value, Refusal> {
    let directory = Directory::read(backend).await?;
    let store = Store(backend);
    let mut answered = serde_json::Map::new();
    for id in ids {
        let said = match store.secret(*id).await {
            Err(_) => {
                json!({ "problem": "Secret Storage has no such secret; it may have been deleted" })
            }
            Ok(secret) => {
                let label = format!("{}: {}", directory.owner_name(&secret.owner), secret.name);
                match secret.plugins.iter().any(|shared| shared == plugin) {
                    false => {
                        json!({ "label": label, "problem": format!("it is not shared with {plugin}") })
                    }
                    true => match open(
                        backend,
                        SECRETS,
                        secret.id,
                        &secret.label(),
                        secret.sealed.as_ref(),
                    )
                    .await
                    {
                        Ok(value) => {
                            used(backend, &secret, plugin).await;
                            json!({ "label": label, "value": value.expose() })
                        }
                        Err(why) => {
                            json!({ "label": label, "problem": format!("it could not be opened: {why}") })
                        }
                    },
                }
            }
        };
        answered.insert(id.to_string(), said);
    }
    Ok(json!({ "secrets": answered }))
}

/// Notes when a plugin was given a secret, at most once an hour, so a page can say who uses it.
async fn used(backend: &Backend, secret: &Secret, plugin: &str) {
    let recent = secret.used.get(plugin).is_some_and(|at| Utc::now() - *at < Duration::hours(1));
    if recent {
        return;
    }
    let mut used = secret.used.clone();
    used.insert(plugin.to_string(), Utc::now());
    let _ = backend.update::<Value>(SECRETS, json!(secret.id), json!({ "used": used }), None).await;
}

/// The secrets shared with a plugin, as its Settings page offers them.
pub async fn shared(backend: &Backend, plugin: &str) -> Result<Value, Refusal> {
    let directory = Directory::read(backend).await?;
    let mut offered: Vec<Value> = Store(backend)
        .secrets()
        .await?
        .into_iter()
        .filter(|secret| secret.plugins.iter().any(|shared| shared == plugin))
        .map(|secret| {
            json!({
                "id": secret.id,
                "label": format!("{}: {}", directory.owner_name(&secret.owner), secret.name),
                "hint": secret.title,
            })
        })
        .collect();
    offered.sort_by_key(|offer| offer["label"].as_str().unwrap_or_default().to_lowercase());
    Ok(json!({ "secrets": offered }))
}

/// Renews a kept token that is near its end, and revokes the one the last renewal replaced.
pub async fn renew(backend: &Backend, secret: &Secret) -> Result<bool, String> {
    let Some(mut kept) = secret.kept.clone() else { return Ok(false) };
    let store = Store(backend);
    let plugin = secret.plugins.first().cloned().unwrap_or_default();
    let me = Me {
        kind: "plugin".into(),
        id: None,
        plugin: Some(plugin.clone()),
        login: format!("Secret Storage, for {plugin}"),
        admin: false,
        writes: false,
    };
    let directory = Directory::read(backend).await.map_err(|refusal| refusal.detail)?;
    let cx = Context { backend, me, directory };
    if let Some(previous) = kept.previous.take() {
        if let Err(refusal) = end_token(&cx, previous, "renewed").await {
            tracing::warn!(detail = %refusal.detail, %previous, "a replaced token was not revoked");
        }
        let _ = store.update::<Value>(SECRETS, secret.id, json!({ "kept": kept })).await;
    }
    let lasts = Duration::minutes(kept.minutes.max(1));
    let due = secret
        .expires_at
        .is_none_or(|ends| ends - Utc::now() < (lasts / 4).max(Duration::minutes(15)));
    if !kept.renew || !due {
        return Ok(false);
    }
    let account = store.account(kept.account).await.map_err(|refusal| refusal.detail)?;
    let vendor = vendor_of(&account).map_err(|refusal| refusal.detail)?;
    let allowance =
        store.allowance(kept.allowance).await.map_err(|_| "its allowance was removed".to_string());
    let renewed = match allowance {
        Err(why) => Err(why),
        Ok(allowance) => {
            let credential = credential(&cx, &account).await.map_err(|refusal| refusal.detail)?;
            let purpose = format!("DOC, kept for the {plugin} plugin");
            match vendors::issue(
                vendor,
                &account.config,
                &credential,
                &kept.restrictions,
                kept.minutes,
                &format!("doc-plugin-{plugin}"),
                &purpose,
            )
            .await
            {
                Err(why) => Err(why),
                Ok(issued) => {
                    let sealed = seal(backend, &secret.label(), &issued.value)
                        .await
                        .map_err(|refusal| refusal.detail)?;
                    let token = record(
                        &cx,
                        &account,
                        &allowance,
                        &kept.restrictions,
                        &issued,
                        &purpose,
                        Some(secret.id),
                    )
                    .await
                    .map_err(|refusal| refusal.detail)?;
                    kept.previous = kept.token.replace(token.id);
                    kept.problem = None;
                    let version = secret.version.max(1) + 1;
                    let set = json!({ "sealed": sealed, "version": version, "expires_at": issued.expires_at, "kept": kept });
                    store
                        .update::<Value>(SECRETS, secret.id, set)
                        .await
                        .map_err(|refusal| refusal.detail)?;
                    store
                        .note(
                            secret.id,
                            "Secret Storage",
                            "renewed",
                            &format!(
                                "version {version}, until {}",
                                issued.expires_at.format("%d %b %Y %H:%M UTC")
                            ),
                        )
                        .await;
                    Ok(())
                }
            }
        }
    };
    match renewed {
        Ok(()) => {
            changed(backend, vec![secret.id]).await;
            Ok(true)
        }
        Err(why) => {
            kept.problem = Some(why.clone());
            let _ = store.update::<Value>(SECRETS, secret.id, json!({ "kept": kept })).await;
            Err(why)
        }
    }
}
