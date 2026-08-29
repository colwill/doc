//! The pages: secrets, vendor accounts and tokens, each list with the button that adds to it, and a
//! page per secret and per account with its sections at the left. No page ever shows a stored value.
//!
//! A proxied account's pages are in [`proxied`], because what they are about is different: rules,
//! denials, DOC keys and the log of every call made, rather than tokens issued.

mod proxied;

use std::collections::BTreeMap;

use askama::Template;
use chrono::{DateTime, NaiveDate, Utc};
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::Value;
use uuid::Uuid;

use crate::ops::{self, Context, Keeping, NewAccount, NewSecret};
use crate::store::{Account, Allowance, Secret, Token};
use crate::vendors::{Vendor, strings};
use crate::{Refusal, parameter};

pub(crate) type Form = Vec<(String, String)>;
pub(crate) type Page = Result<String, Refusal>;
pub(crate) type Drawing<'a> = std::pin::Pin<Box<dyn Future<Output = Page> + Send + 'a>>;

pub(crate) fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

pub(crate) fn field(form: &Form, name: &str) -> String {
    form.iter().find(|(key, _)| key == name).map(|(_, value)| value.to_string()).unwrap_or_default()
}

pub(crate) fn fields(form: &Form, name: &str) -> Vec<String> {
    form.iter().filter(|(key, _)| key == name).map(|(_, value)| value.trim().to_string()).collect()
}

pub(crate) fn id(text: &str) -> Result<Uuid, Refusal> {
    text.parse().map_err(|_| Refusal::bad("that is not an ID"))
}

pub fn when(at: DateTime<Utc>) -> String {
    at.format("%-d %b %Y, %H:%M UTC").to_string()
}

fn day(at: &Option<DateTime<Utc>>) -> String {
    at.map(|at| at.format("%-d %b %Y").to_string()).unwrap_or_default()
}

/// A date typed into a form, as the end of that day.
fn date(text: &str) -> Result<Option<DateTime<Utc>>, Refusal> {
    match text.trim() {
        "" => Ok(None),
        text => NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .ok()
            .and_then(|day| day.and_hms_opt(23, 59, 59))
            .map(|at| Some(at.and_utc()))
            .ok_or_else(|| Refusal::bad("write the date as YYYY-MM-DD")),
    }
}

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    pub(crate) fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    pub(crate) fn refused(refusal: &Refusal) -> Self {
        Self { notice: None, error: Some(refusal.detail.clone()) }
    }
}

pub struct Choice {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

pub(crate) fn chosen(options: Vec<(String, String)>, selected: &str) -> Vec<Choice> {
    options
        .into_iter()
        .map(|(value, label)| Choice { selected: value == selected, value, label })
        .collect()
}

pub(crate) fn render<T: Template>(page: &T) -> Page {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

pub struct SecretRow {
    pub href: String,
    pub name: String,
    pub title: String,
    pub owner: String,
    pub plugins: String,
    pub expires: String,
    pub kept: bool,
    pub problem: Option<String>,
}

pub struct AccountRow {
    pub href: String,
    pub title: String,
    pub vendor: &'static str,
    pub owner: String,
    pub covering: usize,
    pub manages: bool,
}

pub struct TokenRow {
    pub id: Uuid,
    pub account: String,
    pub account_href: String,
    pub asked_by: String,
    pub about: String,
    pub issued: String,
    pub expires: String,
    pub state: &'static str,
    pub badge: &'static str,
    pub revocable: bool,
}

#[derive(Template)]
#[template(path = "home.html")]
struct Home {
    flash: Flash,
    section: &'static str,
    can_store: bool,
    can_onboard: bool,
    can_ask: bool,
    secrets: Vec<SecretRow>,
    accounts: Vec<AccountRow>,
    tokens: Vec<TokenRow>,
}

fn token_row(cx: &Context<'_>, accounts: &[Account], token: &Token) -> TokenRow {
    let account = accounts.iter().find(|account| account.id == token.account);
    let vendor = account.and_then(|account| Vendor::parse(&account.vendor));
    let (state, badge) = match (token.live(), token.state.as_str()) {
        (true, _) => ("Active", "ready"),
        (false, "revoked") => ("Revoked", "error"),
        _ => ("Ended", "loading"),
    };
    let manages = account.is_some_and(|account| cx.me.manages(&cx.directory, &account.owner));
    let by_id = matches!(vendor, Some(Vendor::Artifactory | Vendor::Linode));
    let revocable = token.live()
        && ((by_id && (token.asked_by == cx.me.reference() || manages))
            || (token.kept_in.is_some() && manages));
    TokenRow {
        id: token.id,
        account: account.map(Account::shown).unwrap_or_else(|| "a removed account".into()),
        account_href: account.map(Account::href).unwrap_or_default(),
        asked_by: token.asked_by_label.clone(),
        about: vendor.map(|vendor| vendor.describe(&token.restrictions)).unwrap_or_default(),
        issued: token.issued_at.map(when).unwrap_or_default(),
        expires: when(token.expires_at),
        state,
        badge,
        revocable,
    }
}

/// Boxed, so every route awaiting it holds a pointer rather than the whole page's state.
/// A secret somebody manages that runs out soon, as their dashboard lists it.
pub struct Expiring {
    pub href: String,
    pub title: String,
    pub owner: String,
    pub expires: String,
    pub gone: bool,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct ExpiringFragment {
    rows: Vec<Expiring>,
}

/// Secrets running out, on a person's dashboard: those they manage that expire within a month or
/// already have, soonest first, so a credential is renewed before what uses it stops.
async fn expiring(cx: &Context<'_>) -> Page {
    let soon = Utc::now() + chrono::Duration::days(30);
    let mut held: Vec<Secret> = cx
        .store()
        .secrets()
        .await?
        .into_iter()
        .filter(|secret| secret.expires_at.is_some_and(|at| at <= soon))
        .filter(|secret| cx.me.manages(&cx.directory, &secret.owner))
        .collect();
    held.sort_by_key(|secret| secret.expires_at);
    let now = Utc::now();
    let rows = held
        .iter()
        .take(6)
        .map(|secret| Expiring {
            href: secret.href(),
            title: match secret.title.is_empty() {
                true => secret.name.clone(),
                false => secret.title.clone(),
            },
            owner: cx.directory.owner_label(&secret.owner),
            expires: day(&secret.expires_at),
            gone: secret.expires_at.is_some_and(|at| at <= now),
        })
        .collect();
    render(&ExpiringFragment { rows })
}

pub(crate) fn home<'a>(cx: &'a Context<'_>, section: &'static str, flash: Flash) -> Drawing<'a> {
    Box::pin(drawn_home(cx, section, flash))
}

async fn drawn_home(cx: &Context<'_>, section: &'static str, flash: Flash) -> Page {
    let store = cx.store();
    let owners = cx.me.owners(&cx.directory);
    let writes = cx.me.writes || cx.me.admin;
    let mut secrets: Vec<SecretRow> = Vec::new();
    let mut accounts: Vec<AccountRow> = Vec::new();
    let mut tokens: Vec<TokenRow> = Vec::new();
    let visible = ops::visible_accounts(cx).await?;
    let can_ask = {
        let mut any = false;
        for (account, _) in &visible {
            any |= !ops::covering(cx, account.id).await?.is_empty();
        }
        any
    };
    match section {
        "accounts" => {
            for (account, manages) in &visible {
                let covering = ops::covering(cx, account.id).await?.len();
                accounts.push(AccountRow {
                    href: account.href(),
                    title: account.shown(),
                    vendor: match account.vendor == crate::store::PROXIED {
                        true => "Proxied by DOC",
                        false => Vendor::parse(&account.vendor).map_or("", Vendor::name),
                    },
                    owner: cx.directory.owner_label(&account.owner),
                    covering,
                    manages: *manages,
                });
            }
        }
        "tokens" => {
            let all: Vec<Account> = store.accounts().await?;
            let filter = match cx.me.admin {
                true => serde_json::json!({}),
                false => serde_json::json!({ "asked_by": cx.me.reference() }),
            };
            tokens = store
                .tokens(filter)
                .await?
                .iter()
                .take(200)
                .map(|token| token_row(cx, &all, token))
                .collect();
        }
        _ => {
            let mut held: Vec<Secret> = store
                .secrets()
                .await?
                .into_iter()
                .filter(|secret| cx.me.manages(&cx.directory, &secret.owner))
                .collect();
            held.sort_by_key(|secret| cx.label(secret).to_lowercase());
            secrets = held
                .iter()
                .map(|secret| SecretRow {
                    href: secret.href(),
                    name: secret.name.clone(),
                    title: secret.title.clone(),
                    owner: cx.directory.owner_label(&secret.owner),
                    plugins: match secret.plugins.is_empty() {
                        true => "no plugin yet".into(),
                        false => secret.plugins.join(", "),
                    },
                    expires: day(&secret.expires_at),
                    kept: secret.kept.is_some(),
                    problem: secret.kept.as_ref().and_then(|kept| kept.problem.clone()),
                })
                .collect();
        }
    }
    let can_onboard = writes && owners.iter().any(|(owner, _)| !owner.starts_with("user:"));
    render(&Home {
        flash,
        section,
        can_store: writes && !owners.is_empty(),
        can_onboard,
        can_ask,
        secrets,
        accounts,
        tokens,
    })
}

pub struct PluginRow {
    pub id: String,
    pub label: String,
    pub used: String,
}

pub struct HistoryRow {
    pub at: String,
    pub by: String,
    pub action: String,
    pub detail: String,
}

#[derive(Template)]
#[template(path = "secret.html")]
struct SecretPage {
    flash: Flash,
    section: &'static str,
    writes: bool,
    secret: Secret,
    owner: String,
    updated: String,
    expires: String,
    kept: Option<String>,
    kept_problem: Option<String>,
    plugins: Vec<PluginRow>,
    history: Vec<HistoryRow>,
}

pub(crate) async fn history(cx: &Context<'_>, about: Uuid) -> Result<Vec<HistoryRow>, Refusal> {
    Ok(cx
        .store()
        .history(about)
        .await?
        .into_iter()
        .map(|happened| HistoryRow {
            at: happened.at.map(when).unwrap_or_default(),
            by: happened.by,
            action: happened.action,
            detail: happened.detail,
        })
        .collect())
}

/// Boxed, so every route awaiting it holds a pointer rather than the whole page's state.
fn secret_page<'a>(
    cx: &'a Context<'_>,
    secret: Uuid,
    section: &'static str,
    flash: Flash,
) -> Drawing<'a> {
    Box::pin(drawn_secret_page(cx, secret, section, flash))
}

async fn drawn_secret_page(
    cx: &Context<'_>,
    secret: Uuid,
    section: &'static str,
    flash: Flash,
) -> Page {
    let secret = ops::managed_secret(cx, secret).await?;
    let plugins = secret
        .plugins
        .iter()
        .map(|plugin| PluginRow {
            id: plugin.clone(),
            label: cx
                .directory
                .plugins
                .iter()
                .find(|found| &found.id == plugin)
                .map_or_else(|| plugin.clone(), |found| found.shown()),
            used: secret.used.get(plugin).map(|at| when(*at)).unwrap_or_else(|| "not yet".into()),
        })
        .collect();
    let (kept, kept_problem) = match &secret.kept {
        Some(kept) => {
            let account = cx.store().account(kept.account).await.ok();
            let from =
                account.as_ref().map_or_else(|| "a removed account".to_string(), Account::shown);
            let renews = match kept.renew {
                true => "It is renewed before it ends.",
                false => "It is not renewed: when it ends, keep another.",
            };
            (
                Some(format!("A token from {from}, kept for a plugin. {renews}")),
                kept.problem.clone(),
            )
        }
        None => (None, None),
    };
    let history = match section {
        "history" => history(cx, secret.id).await?,
        _ => Vec::new(),
    };
    render(&SecretPage {
        flash,
        section,
        writes: cx.me.writes || cx.me.admin,
        owner: cx.directory.owner_label(&secret.owner),
        updated: secret.updated_at.map(when).unwrap_or_default(),
        expires: day(&secret.expires_at),
        secret,
        kept,
        kept_problem,
        plugins,
        history,
    })
}

#[derive(Template)]
#[template(path = "secret_new.html")]
struct SecretForm {
    flash: Flash,
    owners: Vec<Choice>,
    plugins: Vec<Choice>,
    name: String,
    title: String,
    description: String,
    expires: String,
}

fn secret_form(cx: &Context<'_>, given: &Form, flash: Flash) -> Page {
    let picked = fields(given, "plugins");
    let plugins = cx
        .directory
        .plugins
        .iter()
        .filter(|plugin| plugin.id != crate::ID)
        .map(|plugin| Choice {
            selected: picked.contains(&plugin.id),
            value: plugin.id.clone(),
            label: plugin.shown(),
        })
        .collect();
    render(&SecretForm {
        flash,
        owners: chosen(cx.me.owners(&cx.directory), &field(given, "owner")),
        plugins,
        name: field(given, "name"),
        title: field(given, "title"),
        description: field(given, "description"),
        expires: field(given, "expires"),
    })
}

#[derive(Template)]
#[template(path = "secret_change.html")]
struct SecretChange {
    flash: Flash,
    secret: Secret,
    /// `value`, `edit` or `share`.
    what: &'static str,
    title: String,
    description: String,
    expires: String,
    plugins: Vec<Choice>,
}

/// Boxed, so every route awaiting it holds a pointer rather than the whole page's state.
fn secret_change<'a>(
    cx: &'a Context<'_>,
    secret: Uuid,
    what: &'static str,
    given: &'a Form,
    flash: Flash,
) -> Drawing<'a> {
    Box::pin(drawn_secret_change(cx, secret, what, given, flash))
}

async fn drawn_secret_change(
    cx: &Context<'_>,
    secret: Uuid,
    what: &'static str,
    given: &Form,
    flash: Flash,
) -> Page {
    let secret = ops::managed_secret(cx, secret).await?;
    let plugins = cx
        .directory
        .plugins
        .iter()
        .filter(|plugin| plugin.id != crate::ID && !secret.plugins.contains(&plugin.id))
        .map(|plugin| Choice {
            selected: field(given, "plugin") == plugin.id,
            value: plugin.id.clone(),
            label: plugin.shown(),
        })
        .collect();
    let typed = |name: &str, held: String| match given.is_empty() {
        true => held,
        false => field(given, name),
    };
    render(&SecretChange {
        flash,
        what,
        title: typed("title", secret.title.clone()),
        description: typed("description", secret.description.clone()),
        expires: typed(
            "expires",
            secret.expires_at.map(|at| at.format("%Y-%m-%d").to_string()).unwrap_or_default(),
        ),
        plugins,
        secret,
    })
}

pub struct VendorCard {
    pub id: &'static str,
    pub name: &'static str,
    pub about: &'static str,
}

pub(crate) struct ConfigRow {
    pub key: &'static str,
    pub label: &'static str,
    pub hint: &'static str,
    pub required: bool,
    pub lines: bool,
    pub value: String,
}

#[derive(Template)]
#[template(path = "account_new.html")]
struct AccountForm {
    flash: Flash,
    vendors: Vec<VendorCard>,
    vendor: Option<Vendor>,
    owners: Vec<Choice>,
    config: Vec<ConfigRow>,
    name: String,
    title: String,
}

fn account_form(cx: &Context<'_>, vendor: Option<Vendor>, given: &Form, flash: Flash) -> Page {
    let owners = cx
        .me
        .owners(&cx.directory)
        .into_iter()
        .filter(|(owner, _)| !owner.starts_with("user:"))
        .collect();
    let config = vendor
        .map(|vendor| {
            vendor
                .config_fields()
                .into_iter()
                .map(|found| ConfigRow {
                    key: found.key,
                    label: found.label,
                    hint: found.hint,
                    required: found.required,
                    lines: found.lines,
                    value: field(given, found.key),
                })
                .collect()
        })
        .unwrap_or_default();
    let mut vendors =
        vec![VendorCard { id: crate::store::PROXIED, name: proxied::NAME, about: proxied::ABOUT }];
    vendors.extend(Vendor::ALL.into_iter().map(|vendor| VendorCard {
        id: vendor.id(),
        name: vendor.name(),
        about: vendor.about(),
    }));
    render(&AccountForm {
        flash,
        vendors,
        vendor,
        owners: chosen(owners, &field(given, "owner")),
        config,
        name: field(given, "name"),
        title: field(given, "title"),
    })
}

pub struct AllowanceRow {
    pub id: Uuid,
    pub who: String,
    pub about: String,
    pub longest: String,
}

#[derive(Template)]
#[template(path = "account.html")]
struct AccountPage {
    flash: Flash,
    section: &'static str,
    manages: bool,
    writes: bool,
    account: Account,
    vendor: Vendor,
    owner: String,
    config: Vec<(String, String)>,
    updated: String,
    allowances: Vec<AllowanceRow>,
    tokens: Vec<TokenRow>,
    covering: usize,
    history: Vec<HistoryRow>,
}

/// A lifetime in minutes, in words.
pub(crate) fn lasting(minutes: i64) -> String {
    match minutes {
        minutes if minutes % (24 * 60) == 0 => match minutes / (24 * 60) {
            1 => "a day".into(),
            days => format!("{days} days"),
        },
        minutes if minutes % 60 == 0 => match minutes / 60 {
            1 => "an hour".into(),
            hours => format!("{hours} hours"),
        },
        minutes => format!("{minutes} minutes"),
    }
}

/// Boxed, so every route awaiting it holds a pointer rather than the whole page's state.
fn account_page<'a>(
    cx: &'a Context<'_>,
    account: Uuid,
    section: &'static str,
    flash: Flash,
) -> Drawing<'a> {
    Box::pin(drawn_account_page(cx, account, section, flash))
}

async fn drawn_account_page(
    cx: &Context<'_>,
    account: Uuid,
    section: &'static str,
    flash: Flash,
) -> Page {
    let account = cx.store().account(account).await?;
    let vendor = ops::vendor_of(&account)?;
    let manages = cx.me.manages(&cx.directory, &account.owner);
    let covering = ops::covering(cx, account.id).await?;
    if !manages && covering.is_empty() {
        return Err(Refusal::forbidden(
            "only its managers, and whoever an allowance covers, see an account",
        ));
    }
    let labels: BTreeMap<&str, &str> =
        vendor.config_fields().iter().map(|found| (found.key, found.label)).collect();
    let config = account
        .config
        .as_object()
        .into_iter()
        .flatten()
        .map(|(key, value)| {
            let shown = match value {
                Value::Array(_) => strings(value).join(", "),
                other => other.as_str().unwrap_or_default().to_string(),
            };
            (labels.get(key.as_str()).copied().unwrap_or(key).to_string(), shown)
        })
        .collect();
    let allowances: Vec<Allowance> = match manages {
        true => cx.store().allowances(Some(account.id)).await?,
        false => covering.clone(),
    };
    let allowances = allowances
        .iter()
        .map(|allowance| AllowanceRow {
            id: allowance.id,
            who: cx.directory.label(&allowance.who),
            about: vendor.describe(&allowance.grants),
            longest: lasting(allowance.minutes),
        })
        .collect();
    let tokens = match section {
        "tokens" => {
            let filter = match manages {
                true => serde_json::json!({ "account": account.id }),
                false => {
                    serde_json::json!({ "account": account.id, "asked_by": cx.me.reference() })
                }
            };
            let accounts = [account.clone()];
            cx.store()
                .tokens(filter)
                .await?
                .iter()
                .take(200)
                .map(|token| token_row(cx, &accounts, token))
                .collect()
        }
        _ => Vec::new(),
    };
    let history = match (section, manages) {
        ("history", true) => history(cx, account.id).await?,
        _ => Vec::new(),
    };
    render(&AccountPage {
        flash,
        section,
        manages,
        writes: cx.me.writes || cx.me.admin,
        owner: cx.directory.owner_label(&account.owner),
        updated: account.updated_at.map(when).unwrap_or_default(),
        vendor,
        config,
        allowances,
        tokens,
        covering: covering.len(),
        history,
        account,
    })
}

#[derive(Template)]
#[template(path = "account_credential.html")]
pub(crate) struct CredentialForm {
    pub(crate) flash: Flash,
    pub(crate) account: Account,
    /// None for a proxied account, which has no issuing vendor to name.
    pub(crate) vendor: Option<Vendor>,
}

pub(crate) struct WhoGroup {
    pub label: &'static str,
    pub choices: Vec<Choice>,
}

#[derive(Template)]
#[template(path = "allowance_new.html")]
struct AllowanceForm {
    flash: Flash,
    account: Account,
    vendor: Vendor,
    who: Vec<WhoGroup>,
    roles: Vec<Choice>,
    given: BTreeMap<String, String>,
    minutes: String,
    least: i64,
    most: i64,
}

impl AllowanceForm {
    /// What was typed into a box, kept when the form comes back refused.
    fn typed(&self, key: &str) -> String {
        self.given.get(key).cloned().unwrap_or_default()
    }
}

fn allowance_form(
    cx: &Context<'_>,
    account: Account,
    vendor: Vendor,
    given: &Form,
    flash: Flash,
) -> Page {
    let picked = field(given, "who");
    let pick = |value: String, label: String| Choice { selected: value == picked, value, label };
    let directory = &cx.directory;
    let mut teams: Vec<Choice> = directory
        .teams
        .iter()
        .map(|team| {
            pick(format!("team:{}", team.id), directory.label(&format!("team:{}", team.id)))
        })
        .collect();
    teams.sort_by_key(|choice| choice.label.to_lowercase());
    let mut people: Vec<Choice> = directory
        .people
        .values()
        .filter(|person| !person.disabled)
        .map(|person| pick(format!("user:{}", person.id), person.login.clone()))
        .collect();
    people.sort_by_key(|choice| choice.label.to_lowercase());
    let services = directory
        .services
        .iter()
        .filter(|found| !found.disabled)
        .map(|found| pick(format!("service:{}", found.id), found.name.clone()))
        .collect();
    let plugins = directory
        .plugins
        .iter()
        .filter(|plugin| plugin.id != crate::ID)
        .map(|plugin| pick(format!("plugin:{}", plugin.id), plugin.shown()))
        .collect();
    let organisations = directory
        .organisations
        .iter()
        .map(|found| {
            pick(
                format!("organisation:{}", found.id),
                directory.label(&format!("organisation:{}", found.id)),
            )
        })
        .collect();
    let ticked = fields(given, "roles");
    let roles = strings(&account.config["roles"])
        .into_iter()
        .map(|role| Choice { selected: ticked.contains(&role), label: role.clone(), value: role })
        .collect();
    let (least, most) = vendor.lifetime();
    render(&AllowanceForm {
        flash,
        who: vec![
            WhoGroup { label: "Teams", choices: teams },
            WhoGroup { label: "People", choices: people },
            WhoGroup { label: "Service accounts", choices: services },
            WhoGroup { label: "Plugins", choices: plugins },
            WhoGroup { label: "Everyone in an organisation", choices: organisations },
        ],
        roles,
        given: given.iter().cloned().collect(),
        minutes: match field(given, "minutes") {
            none if none.is_empty() => most.min(24 * 60).to_string(),
            some => some,
        },
        least,
        most,
        account,
        vendor,
    })
}

pub struct Tick {
    pub field: &'static str,
    pub value: String,
    pub label: String,
    pub checked: bool,
}

#[derive(Template)]
#[template(path = "issue.html")]
struct IssueForm {
    flash: Flash,
    account: Account,
    vendor: Vendor,
    keeping: bool,
    /// The allowances to choose from, before one is chosen.
    allowances: Vec<AllowanceRow>,
    allowance: Option<AllowanceRow>,
    ticks: Vec<Tick>,
    minutes: String,
    least: i64,
    most: i64,
    purpose: String,
    name: String,
    renew: bool,
}

/// Boxed, so every route awaiting it holds a pointer rather than the whole page's state.
fn issue_form<'a>(
    cx: &'a Context<'_>,
    account: Uuid,
    keeping: bool,
    request: &'a Request,
    given: &'a Form,
    flash: Flash,
) -> Drawing<'a> {
    Box::pin(drawn_issue_form(cx, account, keeping, request, given, flash))
}

async fn drawn_issue_form(
    cx: &Context<'_>,
    account: Uuid,
    keeping: bool,
    request: &Request,
    given: &Form,
    flash: Flash,
) -> Page {
    let account = cx.store().account(account).await?;
    let vendor = ops::vendor_of(&account)?;
    let manages = cx.me.manages(&cx.directory, &account.owner);
    let offered: Vec<Allowance> = match keeping {
        true if manages => cx
            .store()
            .allowances(Some(account.id))
            .await?
            .into_iter()
            .filter(|allowance| allowance.who.starts_with("plugin:"))
            .collect(),
        true => {
            return Err(Refusal::forbidden(
                "only the account's managers keep a token for a plugin",
            ));
        }
        false => ops::covering(cx, account.id).await?,
    };
    if offered.is_empty() {
        return Err(Refusal::forbidden(match keeping {
            true => "no allowance on this account names a plugin: allow one first",
            false => "no allowance on this account covers you",
        }));
    }
    let row = |allowance: &Allowance| AllowanceRow {
        id: allowance.id,
        who: cx.directory.label(&allowance.who),
        about: vendor.describe(&allowance.grants),
        longest: lasting(allowance.minutes),
    };
    let wanted = parameter(&request.query, "allowance")
        .or_else(|| Some(field(given, "allowance")).filter(|found| !found.is_empty()));
    let picked = match (wanted, offered.as_slice()) {
        (Some(wanted), _) => offered.iter().find(|allowance| allowance.id.to_string() == wanted),
        (None, [only]) => Some(only),
        _ => None,
    };
    let (ticks, most) = match picked {
        Some(allowance) => {
            let ticked: Vec<(String, String)> = given.clone();
            let ticks = vendor
                .choices(&allowance.grants)
                .into_iter()
                .map(|(field, value, label)| Tick {
                    checked: ticked.iter().any(|(name, held)| name == field && held == &value),
                    field,
                    value,
                    label,
                })
                .collect();
            (ticks, allowance.minutes.min(vendor.lifetime().1))
        }
        None => (Vec::new(), vendor.lifetime().1),
    };
    let least = vendor.lifetime().0;
    render(&IssueForm {
        flash,
        keeping,
        allowances: offered.iter().map(row).collect(),
        allowance: picked.map(row),
        ticks,
        minutes: match field(given, "minutes") {
            none if none.is_empty() => most.min(60).max(least).to_string(),
            some => some,
        },
        least,
        most,
        purpose: field(given, "purpose"),
        name: match field(given, "name") {
            none if none.is_empty() => picked
                .map(|allowance| {
                    format!("{}-{}", account.name, allowance.who.trim_start_matches("plugin:"))
                })
                .unwrap_or_default(),
            some => some,
        },
        renew: given.is_empty() || field(given, "renew") == "yes",
        account,
        vendor,
    })
}

#[derive(Template)]
#[template(path = "issued.html")]
struct IssuedPage {
    flash: Flash,
    account: Account,
    token: String,
    expires: String,
    about: String,
    aws: bool,
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let mut moved = None;
    match Box::pin(route(backend, request, path, &mut moved)).await {
        Ok(html) => match moved {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(&refusal) };
            let html = page.render().unwrap_or_else(|_| refusal.detail.clone());
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

pub(crate) fn section<'a>(request: &Request, known: &[&'a str]) -> &'a str {
    let asked = parameter(&request.query, "section").unwrap_or_default();
    known.iter().copied().find(|found| *found == asked).unwrap_or(known[0])
}

async fn route(
    backend: &Backend,
    request: &Request,
    path: &[&str],
    moved: &mut Option<String>,
) -> Page {
    let cx = Context::read(backend).await?;
    let given = form(request);
    // Boxed: this route's own future is already large, and holding a proxied account's whole
    // page inside it overflows the stack rather than failing politely.
    if let Some(page) = Box::pin(proxied::routed(&cx, request, path, &given, moved)).await {
        return page;
    }
    let mut went = |url: String| *moved = Some(url);
    match (request.method.as_str(), path) {
        ("GET", []) => home(&cx, "secrets", Flash::default()).await,
        ("GET", ["accounts"]) => home(&cx, "accounts", Flash::default()).await,
        ("GET", ["dashboard"]) => expiring(&cx).await,
        ("GET", ["tokens"]) => home(&cx, "tokens", Flash::default()).await,
        ("GET", ["secrets", "new"]) => secret_form(&cx, &given, Flash::default()),
        ("POST", ["secrets", "new"]) => {
            let new = NewSecret {
                owner: field(&given, "owner"),
                name: field(&given, "name"),
                title: field(&given, "title"),
                description: field(&given, "description"),
                value: field(&given, "value"),
                expires_at: match date(&field(&given, "expires")) {
                    Ok(at) => at,
                    Err(refusal) => return secret_form(&cx, &given, Flash::refused(&refusal)),
                },
                plugins: fields(&given, "plugins"),
            };
            match ops::store_secret(&cx, new).await {
                Ok(secret) => {
                    went(secret.href());
                    let said =
                        format!("{} is stored. Nobody will see its value again.", secret.name);
                    secret_page(&cx, secret.id, "overview", Flash::done(said)).await
                }
                Err(refusal) => secret_form(&cx, &given, Flash::refused(&refusal)),
            }
        }
        ("GET", ["secrets", secret]) => {
            let at = section(request, &["overview", "plugins", "history"]);
            secret_page(&cx, id(secret)?, at, Flash::default()).await
        }
        ("GET", ["secrets", secret, what @ ("value" | "edit" | "share")]) => {
            let what = match *what {
                "value" => "value",
                "edit" => "edit",
                _ => "share",
            };
            secret_change(&cx, id(secret)?, what, &Form::new(), Flash::default()).await
        }
        ("POST", ["secrets", secret, "value"]) => {
            let secret = id(secret)?;
            match ops::replace_value(&cx, secret, &field(&given, "value")).await {
                Ok(done) => {
                    went(done.href());
                    let said = format!(
                        "Version {} is stored. Every plugin using it has been told.",
                        done.version
                    );
                    secret_page(&cx, secret, "overview", Flash::done(said)).await
                }
                Err(refusal) => {
                    secret_change(&cx, secret, "value", &given, Flash::refused(&refusal)).await
                }
            }
        }
        ("POST", ["secrets", secret, "edit"]) => {
            let secret = id(secret)?;
            let expires = match date(&field(&given, "expires")) {
                Ok(at) => at,
                Err(refusal) => {
                    return secret_change(&cx, secret, "edit", &given, Flash::refused(&refusal))
                        .await;
                }
            };
            match ops::change_secret(
                &cx,
                secret,
                &field(&given, "title"),
                &field(&given, "description"),
                expires,
            )
            .await
            {
                Ok(done) => {
                    went(done.href());
                    secret_page(&cx, secret, "overview", Flash::done("Changed.")).await
                }
                Err(refusal) => {
                    secret_change(&cx, secret, "edit", &given, Flash::refused(&refusal)).await
                }
            }
        }
        ("POST", ["secrets", secret, "share"]) => {
            let secret = id(secret)?;
            let plugin = field(&given, "plugin");
            match ops::share(&cx, secret, &plugin, true).await {
                Ok(done) => {
                    went(format!("{}?section=plugins", done.href()));
                    let said = format!(
                        "{plugin} may now be given it. Point one of its settings at it on {plugin}'s Settings page."
                    );
                    secret_page(&cx, secret, "plugins", Flash::done(said)).await
                }
                Err(refusal) => {
                    secret_change(&cx, secret, "share", &given, Flash::refused(&refusal)).await
                }
            }
        }
        ("POST", ["secrets", secret, "plugins", plugin, "remove"]) => {
            let secret = id(secret)?;
            let flash = match ops::share(&cx, secret, plugin, false).await {
                Ok(_) => Flash::done(format!("{plugin} is no longer given it, and has been told.")),
                Err(refusal) => Flash::refused(&refusal),
            };
            secret_page(&cx, secret, "plugins", flash).await
        }
        ("POST", ["secrets", secret, "delete"]) => match ops::delete_secret(&cx, id(secret)?).await
        {
            Ok(gone) => {
                went("/p/secrets/".into());
                home(
                    &cx,
                    "secrets",
                    Flash::done(format!(
                        "{} is deleted, and every plugin using it has been told.",
                        gone.name
                    )),
                )
                .await
            }
            Err(refusal) => {
                secret_page(&cx, id(secret)?, "overview", Flash::refused(&refusal)).await
            }
        },
        ("GET", ["accounts", "new"]) => {
            let vendor =
                parameter(&request.query, "vendor").and_then(|vendor| Vendor::parse(&vendor));
            account_form(&cx, vendor, &given, Flash::default())
        }
        ("POST", ["accounts", "new"]) => {
            let vendor = Vendor::parse(&field(&given, "vendor"));
            let config: BTreeMap<String, String> = vendor
                .map(|vendor| {
                    vendor
                        .config_fields()
                        .iter()
                        .map(|found| (found.key.to_string(), field(&given, found.key)))
                        .collect()
                })
                .unwrap_or_default();
            let new = NewAccount {
                owner: field(&given, "owner"),
                name: field(&given, "name"),
                title: field(&given, "title"),
                vendor: field(&given, "vendor"),
                config,
                credential: field(&given, "credential"),
            };
            match ops::onboard(&cx, new).await {
                Ok((account, tried)) => {
                    went(account.href());
                    let said =
                        format!("{tried} Now say who may ask it for tokens, under Allowances.");
                    account_page(&cx, account.id, "overview", Flash::done(said)).await
                }
                Err(refusal) => account_form(&cx, vendor, &given, Flash::refused(&refusal)),
            }
        }
        ("GET", ["accounts", account]) => {
            let at = section(request, &["overview", "allowances", "tokens", "history"]);
            account_page(&cx, id(account)?, at, Flash::default()).await
        }
        ("POST", ["accounts", account, "test"]) => {
            let account = id(account)?;
            let flash = match ops::test_account(&cx, account).await {
                Ok(said) => Flash::done(said),
                Err(refusal) => Flash::refused(&refusal),
            };
            account_page(&cx, account, "overview", flash).await
        }
        ("GET", ["accounts", account, "credential"]) => {
            let (account, vendor) = ops::managed_account(&cx, id(account)?).await?;
            render(&CredentialForm { flash: Flash::default(), account, vendor: Some(vendor) })
        }
        ("POST", ["accounts", account, "credential"]) => {
            let account = id(account)?;
            match ops::replace_credential(&cx, account, &field(&given, "credential")).await {
                Ok(tried) => {
                    went(format!("/p/secrets/accounts/{account}"));
                    account_page(&cx, account, "overview", Flash::done(tried)).await
                }
                Err(refusal) => {
                    let (account, vendor) = ops::managed_account(&cx, account).await?;
                    render(&CredentialForm {
                        flash: Flash::refused(&refusal),
                        account,
                        vendor: Some(vendor),
                    })
                }
            }
        }
        ("POST", ["accounts", account, "delete"]) => {
            match ops::remove_account(&cx, id(account)?).await {
                Ok(gone) => {
                    went("/p/secrets/accounts".into());
                    let said = format!(
                        "{} is removed; its live tokens were revoked where the vendor allows.",
                        gone.shown()
                    );
                    home(&cx, "accounts", Flash::done(said)).await
                }
                Err(refusal) => {
                    account_page(&cx, id(account)?, "overview", Flash::refused(&refusal)).await
                }
            }
        }
        ("GET", ["accounts", account, "allowances", "new"]) => {
            let (account, vendor) = ops::managed_account(&cx, id(account)?).await?;
            allowance_form(&cx, account, vendor, &given, Flash::default())
        }
        ("POST", ["accounts", account, "allowances", "new"]) => {
            let account = id(account)?;
            let grants: BTreeMap<String, String> =
                ["groups", "repositories", "permissions", "policies", "scopes"]
                    .into_iter()
                    .map(|key| (key.to_string(), field(&given, key)))
                    .chain(std::iter::once((
                        "roles".to_string(),
                        fields(&given, "roles").join("\n"),
                    )))
                    .collect();
            let minutes = field(&given, "minutes").trim().parse::<i64>().unwrap_or(60);
            match ops::allow(&cx, account, &field(&given, "who"), &grants, minutes).await {
                Ok(allowance) => {
                    went(format!("/p/secrets/accounts/{account}?section=allowances"));
                    let said = format!("{} may now ask for tokens.", allowance.who_label);
                    account_page(&cx, account, "allowances", Flash::done(said)).await
                }
                Err(refusal) => {
                    let (held, vendor) = ops::managed_account(&cx, account).await?;
                    allowance_form(&cx, held, vendor, &given, Flash::refused(&refusal))
                }
            }
        }
        ("POST", ["allowances", allowance, "remove"]) => {
            let allowance = id(allowance)?;
            let held = cx.store().allowance(allowance).await?;
            let flash = match ops::disallow(&cx, allowance).await {
                Ok(gone) => Flash::done(format!(
                    "{} may no longer ask. Tokens already issued stay until they end or are revoked.",
                    gone.who_label
                )),
                Err(refusal) => Flash::refused(&refusal),
            };
            account_page(&cx, held.account, "allowances", flash).await
        }
        ("GET", ["accounts", account, what @ ("issue" | "keep")]) => {
            issue_form(&cx, id(account)?, *what == "keep", request, &given, Flash::default()).await
        }
        ("POST", ["accounts", account, "issue"]) => {
            let account = id(account)?;
            let allowance = id(&field(&given, "allowance"))?;
            let vendor = ops::vendor_of(&cx.store().account(account).await?)?;
            let asked = vendor.asked(&given);
            let minutes = field(&given, "minutes").trim().parse::<i64>().unwrap_or(60);
            match ops::issue(&cx, account, allowance, &asked, minutes, &field(&given, "purpose"))
                .await
            {
                Ok((token, value)) => {
                    let held = cx.store().account(account).await?;
                    render(&IssuedPage {
                        flash: Flash::default(),
                        token: value.expose().clone(),
                        expires: when(token.expires_at),
                        about: vendor.describe(&token.restrictions),
                        aws: vendor == Vendor::AwsSts,
                        account: held,
                    })
                }
                Err(refusal) => {
                    issue_form(&cx, account, false, request, &given, Flash::refused(&refusal)).await
                }
            }
        }
        ("POST", ["accounts", account, "keep"]) => {
            let account = id(account)?;
            let vendor = ops::vendor_of(&cx.store().account(account).await?)?;
            let keeping = Keeping {
                account,
                allowance: id(&field(&given, "allowance"))?,
                asked: vendor.asked(&given),
                minutes: field(&given, "minutes").trim().parse::<i64>().unwrap_or(60),
                renew: field(&given, "renew") == "yes",
                name: field(&given, "name"),
            };
            match ops::keep(&cx, keeping).await {
                Ok(secret) => {
                    went(secret.href());
                    let plugin = secret.plugins.first().cloned().unwrap_or_default();
                    let said = format!(
                        "The token is kept as {} and shared with {plugin}. Point a setting of {plugin}'s at it on its Settings page.",
                        secret.name
                    );
                    secret_page(&cx, secret.id, "overview", Flash::done(said)).await
                }
                Err(refusal) => {
                    issue_form(&cx, account, true, request, &given, Flash::refused(&refusal)).await
                }
            }
        }
        ("POST", ["tokens", token, "revoke"]) => {
            let flash = match ops::revoke(&cx, id(token)?).await {
                Ok(_) => Flash::done("Revoked. The vendor has ended it."),
                Err(refusal) => Flash::refused(&refusal),
            };
            let back = field(&given, "back");
            match back
                .strip_prefix("/p/secrets/accounts/")
                .and_then(|rest| rest.split('?').next())
                .and_then(|found| found.parse().ok())
            {
                Some(account) => account_page(&cx, account, "tokens", flash).await,
                None => home(&cx, "tokens", flash).await,
            }
        }
        _ => Err(Refusal::missing("no such page")),
    }
}
