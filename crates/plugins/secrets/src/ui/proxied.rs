//! The pages of a proxied account (ADR-0014). They are about different things from an issuing
//! account's: the rules saying what may be reached, the denials nobody may cross, the DOC keys
//! callers hold, and the log of every call made — which is the whole point, since the vendor's
//! own log shows one machine account for everybody.

use std::collections::BTreeMap;

use askama::Template;
use doc_plugin_sdk::Request;
use uuid::Uuid;

use super::{
    Choice, Drawing, Flash, Form, HistoryRow, Page, chosen, field, fields, history, id, lasting,
    render, section, when,
};
use crate::ops::{self, Context};
use crate::proxy::manage;
use crate::proxy::rules::{self, Rule};
use crate::proxy::upstream;
use crate::store::{
    ALLOWED, Account, Allowance, Call, DENIED, FAILED, NO_ACCOUNT, NO_KEY, NO_RULE, PROXIED,
};
use crate::{Refusal, parameter};

/// What the card on the onboarding page says, since there is one card for every vendor there
/// will ever be.
pub const NAME: &str = "Any vendor, through DOC";
pub const ABOUT: &str = "DOC holds the credential and nobody else ever does. Callers hold a DOC \
                         key, reach the vendor through DOC, and every call they make is \
                         recorded. One way in, whichever vendor it is.";

pub struct RuleRow {
    pub method: String,
    pub path: String,
}

pub struct AllowanceRow {
    pub id: Uuid,
    pub who: String,
    pub rules: Vec<RuleRow>,
    pub longest: String,
}

pub struct DenialRow {
    pub id: Option<Uuid>,
    pub rule: String,
    pub reason: String,
    /// DOC's own, which nobody may lift.
    pub fixed: bool,
    pub instance: bool,
}

pub struct KeyRow {
    pub id: Uuid,
    pub shown: String,
    pub subject: String,
    pub purpose: String,
    pub expires: String,
    pub used: String,
    pub state: &'static str,
    pub badge: &'static str,
    pub revocable: bool,
}

pub struct CallRow {
    pub at: String,
    pub who: String,
    pub call: String,
    pub outcome: &'static str,
    pub badge: &'static str,
    pub answer: String,
    pub moved: String,
    pub took: String,
    pub why: String,
    pub correlation: String,
}

#[derive(Template)]
#[template(path = "proxied_new.html")]
struct OnboardForm {
    flash: Flash,
    owners: Vec<Choice>,
    config: Vec<super::ConfigRow>,
    name: String,
    title: String,
}

#[derive(Template)]
#[template(path = "proxied_account.html")]
struct AccountPage {
    flash: Flash,
    section: &'static str,
    manages: bool,
    writes: bool,
    covered: bool,
    account: Account,
    owner: String,
    about: String,
    reached_at: String,
    updated: String,
    allowances: Vec<AllowanceRow>,
    denials: Vec<DenialRow>,
    keys: Vec<KeyRow>,
    calls: Vec<CallRow>,
    history: Vec<HistoryRow>,
}

#[derive(Template)]
#[template(path = "proxied_allowance.html")]
struct AllowanceForm {
    flash: Flash,
    account: Account,
    who: Vec<super::WhoGroup>,
    permission: String,
    attribute: String,
    kind: String,
    methods: Vec<Choice>,
    rules: Vec<RuleRow>,
    minutes: String,
    sweeping: bool,
}

#[derive(Template)]
#[template(path = "proxied_denial.html")]
struct DenialForm {
    flash: Flash,
    account: Account,
    methods: Vec<Choice>,
    path: String,
    reason: String,
}

#[derive(Template)]
#[template(path = "proxied_key.html")]
struct KeyForm {
    flash: Flash,
    account: Account,
    manages: bool,
    allowances: Vec<AllowanceRow>,
    allowance: String,
    subjects: Vec<Choice>,
    purpose: String,
    minutes: String,
}

#[derive(Template)]
#[template(path = "proxied_issued.html")]
struct IssuedPage {
    flash: Flash,
    account: Account,
    key: String,
    base: String,
    expires: String,
    covers: Vec<RuleRow>,
}

/// The routes that belong to a proxied account. `None` means this is not one of them, and the
/// ordinary pages carry on.
pub async fn routed<'a>(
    cx: &'a Context<'_>,
    request: &'a Request,
    path: &[&str],
    given: &'a Form,
    moved: &mut Option<String>,
) -> Option<Page> {
    let mine = match path {
        ["accounts", "new"] => {
            field(given, "vendor") == PROXIED
                || parameter(&request.query, "vendor").as_deref() == Some(PROXIED)
        }
        ["keys", ..] | ["denials", ..] => true,
        ["accounts", account, ..] => match account.parse::<Uuid>() {
            Ok(account) => {
                matches!(cx.store().account(account).await, Ok(held) if held.vendor == PROXIED)
            }
            Err(_) => false,
        },
        _ => false,
    };
    match mine {
        // Boxed, like every other page here: these routes await the whole of `manage`, and a
        // future holding all of that inlined overflows the stack when `ui::route` embeds it.
        true => Some(Box::pin(route(cx, request, path, given, moved)).await),
        false => None,
    }
}

async fn route(
    cx: &Context<'_>,
    request: &Request,
    path: &[&str],
    given: &Form,
    moved: &mut Option<String>,
) -> Page {
    let mut went = |url: String| *moved = Some(url);
    match (request.method.as_str(), path) {
        ("GET", ["accounts", "new"]) => onboard_form(cx, given, Flash::default()),
        ("POST", ["accounts", "new"]) => {
            let config: BTreeMap<String, String> = upstream::config_fields()
                .iter()
                .map(|found| (found.key.to_string(), field(given, found.key)))
                .collect();
            let new = manage::NewAccount {
                owner: field(given, "owner"),
                name: field(given, "name"),
                title: field(given, "title"),
                config,
                credential: field(given, "credential"),
            };
            match Box::pin(manage::onboard(cx, new)).await {
                Ok((account, tried)) => {
                    went(account.href());
                    let said = format!(
                        "{tried} Nothing may be reached through it yet: write the rules under \
                         Allowances."
                    );
                    page(cx, account.id, "overview", Flash::done(said)).await
                }
                Err(refusal) => onboard_form(cx, given, Flash::refused(&refusal)),
            }
        }
        ("GET", ["accounts", account]) => {
            let at = section(
                request,
                &["overview", "allowances", "denials", "keys", "calls", "history"],
            );
            page(cx, id(account)?, at, Flash::default()).await
        }
        ("POST", ["accounts", account, "test"]) => {
            let account = id(account)?;
            let flash = match Box::pin(manage::test_account(cx, account)).await {
                Ok(said) => Flash::done(said),
                Err(refusal) => Flash::refused(&refusal),
            };
            page(cx, account, "overview", flash).await
        }
        ("GET", ["accounts", account, "allowances", "new"]) => {
            let account = manage::managed_proxied(cx, id(account)?).await?;
            allowance_form(cx, account, given, Flash::default())
        }
        ("POST", ["accounts", account, "allowances", "new"]) => {
            let account = id(account)?;
            let written: Vec<(String, String)> = fields(given, "method")
                .into_iter()
                .zip(fields(given, "path"))
                .filter(|(_, path)| !path.trim().is_empty())
                .collect();
            let minutes = field(given, "minutes").trim().parse::<i64>().unwrap_or(60);
            let who = who_of(given);
            match Box::pin(manage::allow(
                cx,
                account,
                &who,
                &written,
                minutes,
                field(given, "sweeping") == "yes",
            ))
            .await
            {
                Ok(allowance) => {
                    went(format!("/p/secrets/accounts/{account}?section=allowances"));
                    let said = format!(
                        "{} may now reach it. They need a key before they can: issue one under \
                         Keys.",
                        allowance.who_label
                    );
                    page(cx, account, "allowances", Flash::done(said)).await
                }
                Err(refusal) => {
                    let held = manage::managed_proxied(cx, account).await?;
                    allowance_form(cx, held, given, Flash::refused(&refusal))
                }
            }
        }
        ("POST", ["allowances", allowance, "remove"]) => {
            let allowance = id(allowance)?;
            let held = cx.store().allowance(allowance).await?;
            let flash = match Box::pin(ops::disallow(cx, allowance)).await {
                Ok(gone) => Flash::done(format!(
                    "{} may no longer reach it. Revoke their keys too, or they stay until they \
                     run out.",
                    gone.who_label
                )),
                Err(refusal) => Flash::refused(&refusal),
            };
            page(cx, held.account, "allowances", flash).await
        }
        ("GET", ["accounts", account, "denials", "new"]) => {
            let account = manage::managed_proxied(cx, id(account)?).await?;
            render(&DenialForm {
                flash: Flash::default(),
                methods: methods(&field(given, "method")),
                path: field(given, "path"),
                reason: field(given, "reason"),
                account,
            })
        }
        ("POST", ["accounts", account, "denials", "new"]) => {
            let account = id(account)?;
            match Box::pin(manage::deny(
                cx,
                Some(account),
                &field(given, "method"),
                &field(given, "path"),
                &field(given, "reason"),
            ))
            .await
            {
                Ok(denial) => {
                    went(format!("/p/secrets/accounts/{account}?section=denials"));
                    let said = format!(
                        "{} is denied to everybody on this account, whatever an allowance says.",
                        Rule::new(&denial.method, &denial.path).shown()
                    );
                    page(cx, account, "denials", Flash::done(said)).await
                }
                Err(refusal) => {
                    let held = manage::managed_proxied(cx, account).await?;
                    render(&DenialForm {
                        flash: Flash::refused(&refusal),
                        methods: methods(&field(given, "method")),
                        path: field(given, "path"),
                        reason: field(given, "reason"),
                        account: held,
                    })
                }
            }
        }
        ("POST", ["denials", denial, "lift"]) => {
            let denial = id(denial)?;
            let held = cx.store().denial(denial).await?;
            let flash = match Box::pin(manage::undeny(cx, denial)).await {
                Ok(_) => Flash::done("Lifted. An allowance may reach it again."),
                Err(refusal) => Flash::refused(&refusal),
            };
            match held.account {
                Some(account) => page(cx, account, "denials", flash).await,
                None => super::home(cx, "accounts", flash).await,
            }
        }
        ("GET", ["accounts", account, "keys", "new"]) => {
            key_form(cx, id(account)?, request, given, Flash::default()).await
        }
        ("POST", ["accounts", account, "keys", "new"]) => {
            let account = id(account)?;
            let subject = field(given, "subject");
            let minutes = field(given, "minutes").trim().parse::<i64>().ok();
            match Box::pin(manage::issue(
                cx,
                account,
                id(&field(given, "allowance"))?,
                Some(subject.as_str()).filter(|held| !held.is_empty()),
                &field(given, "purpose"),
                minutes,
            ))
            .await
            {
                Ok((key, value)) => {
                    let held = cx.store().account(account).await?;
                    let allowance = cx.store().allowance(key.allowance).await?;
                    render(&IssuedPage {
                        flash: Flash::default(),
                        base: base(cx, &held).await,
                        key: value.expose().clone(),
                        expires: when(key.expires_at),
                        covers: Rule::all(&allowance.rules).iter().map(row).collect(),
                        account: held,
                    })
                }
                Err(refusal) => {
                    key_form(cx, account, request, given, Flash::refused(&refusal)).await
                }
            }
        }
        ("POST", ["keys", key, "revoke"]) => {
            let key = id(key)?;
            let held = cx.store().key(key).await?;
            let flash = match Box::pin(manage::revoke(cx, key, "revoked on its page")).await {
                Ok(_) => Flash::done("Revoked. The next call it is used for is refused."),
                Err(refusal) => Flash::refused(&refusal),
            };
            page(cx, held.account, "keys", flash).await
        }
        ("POST", ["accounts", account, "delete"]) => {
            match Box::pin(ops::remove_account(cx, id(account)?)).await {
                Ok(gone) => {
                    went("/p/secrets/accounts".into());
                    let said = format!(
                        "{} is removed. Its keys are gone with it, and its log stays.",
                        gone.shown()
                    );
                    super::home(cx, "accounts", Flash::done(said)).await
                }
                Err(refusal) => page(cx, id(account)?, "overview", Flash::refused(&refusal)).await,
            }
        }
        ("GET", ["accounts", account, "credential"]) => {
            let held = manage::managed_proxied(cx, id(account)?).await?;
            render(&super::CredentialForm { flash: Flash::default(), vendor: None, account: held })
        }
        ("POST", ["accounts", account, "credential"]) => {
            let account = id(account)?;
            match Box::pin(replace(cx, account, &field(given, "credential"))).await {
                Ok(tried) => {
                    went(format!("/p/secrets/accounts/{account}"));
                    page(cx, account, "overview", Flash::done(tried)).await
                }
                Err(refusal) => {
                    let held = manage::managed_proxied(cx, account).await?;
                    render(&super::CredentialForm {
                        flash: Flash::refused(&refusal),
                        vendor: None,
                        account: held,
                    })
                }
            }
        }
        _ => Err(Refusal::missing("no such page")),
    }
}

/// Replaces a proxied account's credential, trying it against the account's own path first so a
/// wrong one lands on the form rather than on every caller at once.
async fn replace(cx: &Context<'_>, account: Uuid, given: &str) -> Result<String, Refusal> {
    cx.writer()?;
    let account = manage::managed_proxied(cx, account).await?;
    let value = ops::valued(given)?;
    let tried = manage::try_credential(&account.config, &value).await.map_err(Refusal::bad)?;
    let sealed = ops::seal(cx.backend, &account.label(), &value).await?;
    let _: serde_json::Value = cx
        .store()
        .update(
            crate::store::ACCOUNTS,
            account.id,
            serde_json::json!({ "sealed": sealed, "updated_by": cx.me.login }),
        )
        .await?;
    cx.store().note(account.id, &cx.me.login, "replaced", &tried).await;
    cx.audit("account.replaced", account.id, serde_json::json!({ "name": account.name })).await;
    Ok(tried)
}

fn onboard_form(cx: &Context<'_>, given: &Form, flash: Flash) -> Page {
    let owners = cx
        .me
        .owners(&cx.directory)
        .into_iter()
        .filter(|(owner, _)| !owner.starts_with("user:"))
        .collect();
    let config = upstream::config_fields()
        .into_iter()
        .map(|found| super::ConfigRow {
            key: found.key,
            label: found.label,
            hint: found.hint,
            required: found.required,
            lines: found.lines,
            value: field(given, found.key),
        })
        .collect();
    render(&OnboardForm {
        flash,
        owners: chosen(owners, &field(given, "owner")),
        config,
        name: field(given, "name"),
        title: field(given, "title"),
    })
}

/// Boxed, so every route awaiting it holds a pointer rather than the whole page's state.
pub fn page<'a>(cx: &'a Context<'_>, account: Uuid, at: &'static str, flash: Flash) -> Drawing<'a> {
    Box::pin(drawn(cx, account, at, flash))
}

async fn drawn(cx: &Context<'_>, account: Uuid, at: &'static str, flash: Flash) -> Page {
    let account = cx.store().account(account).await?;
    let manages = cx.me.manages(&cx.directory, &account.owner);
    let covering = ops::covering(cx, account.id).await?;
    if !manages && covering.is_empty() {
        return Err(Refusal::forbidden(
            "only its managers, and whoever an allowance covers, see an account",
        ));
    }
    let held: Vec<Allowance> = match manages {
        true => cx.store().allowances(Some(account.id)).await?,
        false => covering.clone(),
    };
    let allowances = held.iter().map(|allowance| allowance_row(cx, allowance)).collect();
    let denials = match at {
        "denials" => shown_denials(cx, account.id).await?,
        _ => Vec::new(),
    };
    let keys = match at {
        "keys" => shown_keys(cx, &account, manages).await?,
        _ => Vec::new(),
    };
    let calls = match at {
        "calls" => manage::log(cx, Some(account.id), 200).await?.iter().map(call_row).collect(),
        _ => Vec::new(),
    };
    let history = match (at, manages) {
        ("history", true) => history(cx, account.id).await?,
        _ => Vec::new(),
    };
    render(&AccountPage {
        flash,
        section: at,
        manages,
        writes: cx.me.writes || cx.me.admin,
        covered: !covering.is_empty(),
        owner: cx.directory.owner_label(&account.owner),
        about: manage::about(&account),
        reached_at: base(cx, &account).await,
        updated: account.updated_at.map(when).unwrap_or_default(),
        allowances,
        denials,
        keys,
        calls,
        history,
        account,
    })
}

/// Where callers address this account, as the page tells them to write it.
async fn base(cx: &Context<'_>, account: &Account) -> String {
    let settings = cx.backend.settings();
    let public = settings.text("proxy-address");
    let public = match public.trim().is_empty() {
        true => "https://<the proxy>".to_string(),
        false => public.trim_end_matches('/').to_string(),
    };
    format!("{public}{}{}", crate::proxy::VIA, account.name)
}

fn row(rule: &Rule) -> RuleRow {
    RuleRow { method: rule.method.clone(), path: rule.path.clone() }
}

fn allowance_row(cx: &Context<'_>, allowance: &Allowance) -> AllowanceRow {
    AllowanceRow {
        id: allowance.id,
        who: match allowance.who_label.is_empty() {
            true => cx.directory.label(&allowance.who),
            false => allowance.who_label.clone(),
        },
        rules: Rule::all(&allowance.rules).iter().map(row).collect(),
        longest: lasting(allowance.minutes),
    }
}

async fn shown_denials(cx: &Context<'_>, account: Uuid) -> Result<Vec<DenialRow>, Refusal> {
    let mut shown = vec![DenialRow {
        id: None,
        rule: "Anything that mints or changes a credential at the vendor".to_string(),
        reason: rules::MINTING_DENIAL.to_string(),
        fixed: true,
        instance: true,
    }];
    shown.push(DenialRow {
        id: None,
        rule: "DELETE, everywhere".to_string(),
        reason: "until a rule names the path it is allowed on".to_string(),
        fixed: true,
        instance: true,
    });
    for denial in cx.store().denials(None).await? {
        let mine = denial.account == Some(account);
        if !mine && denial.account.is_some() {
            continue;
        }
        shown.push(DenialRow {
            id: Some(denial.id),
            rule: Rule::of(&denial).shown(),
            reason: denial.reason.clone(),
            fixed: false,
            instance: denial.account.is_none(),
        });
    }
    Ok(shown)
}

async fn shown_keys(
    cx: &Context<'_>,
    account: &Account,
    manages: bool,
) -> Result<Vec<KeyRow>, Refusal> {
    let filter = match manages {
        true => serde_json::json!({ "account": account.id }),
        false => serde_json::json!({ "account": account.id, "subject": cx.me.reference() }),
    };
    Ok(cx
        .store()
        .keys(filter)
        .await?
        .iter()
        .take(200)
        .map(|key| {
            let (state, badge) = match (key.live(), key.state.as_str()) {
                (true, _) => ("Active", "ready"),
                (false, "revoked") => ("Revoked", "error"),
                _ => ("Ended", "loading"),
            };
            KeyRow {
                id: key.id,
                shown: key.shown.clone(),
                subject: key.subject_label.clone(),
                purpose: key.purpose.clone(),
                expires: when(key.expires_at),
                used: key.last_used_at.map(when).unwrap_or_else(|| "not yet".into()),
                state,
                badge,
                revocable: key.live()
                    && (manages || key.subject == cx.me.reference())
                    && (cx.me.writes || cx.me.admin),
            }
        })
        .collect())
}

fn call_row(call: &Call) -> CallRow {
    let (outcome, badge) = match call.outcome.as_str() {
        ALLOWED => ("Allowed", "ready"),
        DENIED => ("Denied", "error"),
        NO_RULE => ("No rule", "error"),
        NO_KEY => ("No key", "error"),
        NO_ACCOUNT => ("No account", "error"),
        FAILED => ("Failed", "loading"),
        _ => ("Unknown", "loading"),
    };
    CallRow {
        at: when(call.at),
        who: match call.who_label.is_empty() {
            true => call.who.clone(),
            false => call.who_label.clone(),
        },
        call: format!("{} {}", call.method, call.path),
        outcome,
        badge,
        answer: call.status.map(|status| status.to_string()).unwrap_or_default(),
        moved: format!("{} up, {} down", sized(call.sent), sized(call.received)),
        took: format!("{} ms", call.ms),
        why: match (call.denial.is_empty(), call.rule.is_empty()) {
            (false, _) => format!("denied by {}", call.denial),
            (true, false) => call.rule.clone(),
            _ => call.detail.clone(),
        },
        correlation: call.correlation.clone(),
    }
}

/// Bytes, as a person reads them.
fn sized(bytes: i64) -> String {
    const STEPS: [(i64, &str); 3] = [(1_073_741_824, "GB"), (1_048_576, "MB"), (1_024, "kB")];
    for (step, name) in STEPS {
        if bytes >= step {
            return format!("{:.1} {name}", bytes as f64 / step as f64);
        }
    }
    format!("{bytes} B")
}

fn methods(picked: &str) -> Vec<Choice> {
    rules::METHODS
        .iter()
        .map(|method| Choice {
            selected: *method == picked,
            value: (*method).to_string(),
            label: match *method {
                rules::ANY => "Any method but DELETE".to_string(),
                named => named.to_string(),
            },
        })
        .collect()
}

/// Who an allowance is for, from whichever of the three boxes the form used.
fn who_of(given: &Form) -> String {
    match field(given, "kind").as_str() {
        "permission" => format!("permission:{}", field(given, "permission").trim()),
        "attribute" => format!("attribute:{}", field(given, "attribute").trim()),
        _ => field(given, "who"),
    }
}

fn allowance_form(cx: &Context<'_>, account: Account, given: &Form, flash: Flash) -> Page {
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
    let written: Vec<RuleRow> = fields(given, "method")
        .into_iter()
        .zip(fields(given, "path"))
        .map(|(method, path)| RuleRow { method, path })
        .filter(|rule| !rule.path.trim().is_empty())
        .collect();
    render(&AllowanceForm {
        flash,
        who: vec![
            super::WhoGroup { label: "Teams", choices: teams },
            super::WhoGroup { label: "People", choices: people },
            super::WhoGroup { label: "Service accounts", choices: services },
            super::WhoGroup { label: "Plugins", choices: plugins },
            super::WhoGroup { label: "Everyone in an organisation", choices: organisations },
        ],
        permission: field(given, "permission"),
        attribute: field(given, "attribute"),
        kind: match field(given, "kind") {
            none if none.is_empty() => "who".to_string(),
            some => some,
        },
        methods: methods(&field(given, "method")),
        rules: match written.is_empty() {
            true => vec![RuleRow { method: rules::ANY.to_string(), path: String::new() }],
            false => written,
        },
        minutes: match field(given, "minutes") {
            none if none.is_empty() => (24 * 60).to_string(),
            some => some,
        },
        sweeping: field(given, "sweeping") == "yes",
        account,
    })
}

/// Boxed, so every route awaiting it holds a pointer rather than the whole page's state.
fn key_form<'a>(
    cx: &'a Context<'_>,
    account: Uuid,
    request: &'a Request,
    given: &'a Form,
    flash: Flash,
) -> Drawing<'a> {
    Box::pin(drawn_key_form(cx, account, request, given, flash))
}

async fn drawn_key_form(
    cx: &Context<'_>,
    account: Uuid,
    request: &Request,
    given: &Form,
    flash: Flash,
) -> Page {
    let account = cx.store().account(account).await?;
    let manages = cx.me.manages(&cx.directory, &account.owner);
    let offered: Vec<Allowance> = match manages {
        true => cx.store().allowances(Some(account.id)).await?,
        false => ops::covering(cx, account.id).await?,
    };
    if offered.is_empty() {
        return Err(Refusal::forbidden(match manages {
            true => "nothing may be reached through this account yet: write an allowance first",
            false => "no allowance on this account covers you",
        }));
    }
    let wanted = parameter(&request.query, "allowance")
        .or_else(|| Some(field(given, "allowance")).filter(|found| !found.is_empty()))
        .or_else(|| offered.first().map(|only| only.id.to_string()))
        .unwrap_or_default();
    let subjects = match manages {
        true => {
            let mut people: Vec<(String, String)> = cx
                .directory
                .people
                .values()
                .filter(|person| !person.disabled)
                .map(|person| (format!("user:{}", person.id), person.login.clone()))
                .collect();
            people.sort_by_key(|(_, login)| login.to_lowercase());
            let mut offered = vec![(String::new(), "Yourself".to_string())];
            offered.extend(people);
            offered.extend(cx.directory.services.iter().filter(|found| !found.disabled).map(
                |found| {
                    (format!("service:{}", found.id), format!("{} (service account)", found.name))
                },
            ));
            chosen(offered, &field(given, "subject"))
        }
        false => Vec::new(),
    };
    render(&KeyForm {
        flash,
        manages,
        allowances: offered.iter().map(|allowance| allowance_row(cx, allowance)).collect(),
        allowance: wanted,
        subjects,
        purpose: field(given, "purpose"),
        minutes: field(given, "minutes"),
        account,
    })
}
