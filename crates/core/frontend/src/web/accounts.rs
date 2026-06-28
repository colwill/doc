//! Service accounts: create and list your own and your teams', issue and revoke their tokens, and
//! grant them permissions up to your own access (rule 6), give them to a team, or disable them.

use askama::Template;
use axum::extract::{Extension, Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use serde::Deserialize;

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::navigation::{Crumb, crumbs};
use super::pages::{Chrome, Section};
use crate::backend::{Account, BackendError, Holdings, Team, TokenView};
use crate::session::Signed;

const MAX_DAYS: i64 = 365;

pub fn when(at: &chrono::DateTime<chrono::Utc>) -> String {
    at.format("%-d %b %Y %H:%M UTC").to_string()
}

/// Who owns an account, as a page says it, with where to find them.
pub struct Owner {
    pub label: String,
    pub href: Option<String>,
}

/// Who owns `account`, told from the viewer's side.
fn owner(account: &Account, me: &str, teams: &[Team]) -> Owner {
    match (&account.owner_id, &account.owner_team_id) {
        (Some(user), _) if user == me => Owner { label: "You".into(), href: None },
        (Some(_), _) => Owner { label: "Someone else".into(), href: None },
        (None, Some(team)) => {
            let title =
                teams.iter().find(|found| &found.id == team).map(|found| found.title.clone());
            Owner {
                label: format!("The {} team", title.unwrap_or_else(|| "owning".into())),
                href: Some(format!("/teams/{team}")),
            }
        }
        (None, None) => Owner { label: "The platform".into(), href: None },
    }
}

fn me(signed: &Signed) -> String {
    signed.me.get("id").and_then(serde_json::Value::as_str).unwrap_or_default().to_string()
}

#[derive(Template)]
#[template(path = "accounts.html")]
pub struct AccountsPage {
    pub chrome: Chrome,
    pub accounts: Vec<(Account, Owner)>,
}

impl AccountsPage {
    fn when(at: &chrono::DateTime<chrono::Utc>) -> String {
        when(at)
    }
}

pub async fn list(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    let accounts = state.backend.accounts(signed.token()).await?;
    let everyone = state.backend.teams(signed.token()).await?;
    let me = me(&signed);
    let accounts = accounts
        .into_iter()
        .map(|account| {
            let owner = owner(&account, &me, &everyone);
            (account, owner)
        })
        .collect();
    let chrome = Chrome::new("Service accounts", "/service-accounts")
        .signed(&signed, &csrf)
        .with_plugins(&state, &signed)
        .await;
    Ok(Html(AccountsPage { chrome, accounts }.render()?))
}

#[derive(Template)]
#[template(path = "account_new.html")]
pub struct NewAccountPage {
    pub chrome: Chrome,
    /// The teams the viewer can give an account to: those they are in and above.
    pub teams: Vec<Team>,
    pub values: NewAccount,
    pub error: Option<String>,
}

async fn new_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    values: NewAccount,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let teams = super::teams::reach(state, signed).await?;
    let chrome = Chrome::new("Add a service account", "/service-accounts")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    Ok(Html(NewAccountPage { chrome, teams, values, error }.render()?))
}

#[derive(Debug, Default, Deserialize)]
pub struct Listed {
    /// A team to make an account for, chosen in the form.
    #[serde(default)]
    pub team: Option<String>,
}

pub async fn new(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Query(listed): Query<Listed>,
) -> Result<Html<String>, WebError> {
    let values = NewAccount { team: listed.team.unwrap_or_default(), ..NewAccount::default() };
    new_page(&state, &signed, &csrf, values, None).await
}

#[derive(Debug, Default, Deserialize)]
pub struct NewAccount {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Empty for the creator, or a team they are in to own it.
    #[serde(default)]
    pub team: String,
}

pub async fn create(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Form(form): Form<NewAccount>,
) -> Result<Response, WebError> {
    let description = Some(form.description.trim()).filter(|text| !text.is_empty());
    let team = Some(form.team.trim()).filter(|team| !team.is_empty());
    match state.backend.create_account(signed.token(), form.name.trim(), description, team).await {
        Ok(account) => {
            Ok(Redirect::to(&format!("/service-accounts/{}", account.id)).into_response())
        }
        Err(err) if err.status().is_some_and(|status| (400..500).contains(&status)) => {
            let error = Some(err.detail());
            Ok(new_page(&state, &signed, &csrf, form, error).await?.into_response())
        }
        Err(err) => Err(err.into()),
    }
}

/// The sections of a service account's page, each a page of its own with the contents beside it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountSection {
    Overview,
    Permissions,
    Tokens,
}

impl AccountSection {
    const ALL: [Self; 3] = [Self::Overview, Self::Permissions, Self::Tokens];

    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Permissions => "Permissions",
            Self::Tokens => "Tokens",
        }
    }

    fn key(self) -> &'static str {
        match self {
            Self::Overview => "overview",
            Self::Permissions => "permissions",
            Self::Tokens => "tokens",
        }
    }

    fn href(self, account: &str) -> String {
        match self {
            Self::Overview => format!("/service-accounts/{account}"),
            Self::Permissions => format!("/service-accounts/{account}/permissions"),
            Self::Tokens => format!("/service-accounts/{account}/tokens"),
        }
    }
}

#[derive(Template)]
#[template(path = "account.html")]
pub struct AccountPage {
    pub chrome: Chrome,
    pub account: Account,
    pub owner: Owner,
    pub section: AccountSection,
    pub sections: Vec<Section>,
    /// Whether it can be given to anybody else: a team of the viewer's, or the viewer.
    pub givable: bool,
    pub tokens: Vec<TokenView>,
    /// `None` when the RBAC plugin could not say, which the page explains.
    pub holdings: Option<Holdings>,
    pub holdings_problem: Option<String>,
    pub created: Option<String>,
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl AccountPage {
    fn when(at: &chrono::DateTime<chrono::Utc>) -> String {
        when(at)
    }

    fn on(&self, key: &str) -> bool {
        self.section.key() == key
    }
}

/// One thing done to a service account, on a page of its own: giving it away, granting it a
/// permission or issuing it a token.
#[derive(Template)]
#[template(path = "account_form.html")]
pub struct AccountForm {
    pub chrome: Chrome,
    pub account: Account,
    /// `owner`, `permission` or `token`.
    pub form: &'static str,
    /// The teams the viewer can give it to: those they are in and above, but not its owner.
    pub teams: Vec<Team>,
    /// Whether the viewer owns it, so that giving it to themselves is no choice.
    pub mine: bool,
    pub values: Vec<(String, String)>,
    pub error: Option<String>,
}

impl AccountForm {
    fn value(&self, name: &str) -> &str {
        self.values.iter().find(|(key, _)| key == name).map_or("", |(_, value)| value.as_str())
    }
}

#[derive(Default)]
struct Outcome {
    created: Option<String>,
    notice: Option<String>,
    error: Option<String>,
}

/// A service account's chrome: its name, below the list of them, and the section or form it is.
async fn account_chrome(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    account: &Account,
    here: &str,
) -> Chrome {
    let mut chrome = Chrome::new(account.name.clone(), "/service-accounts")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    if here != "Overview" {
        let above =
            vec![Crumb::linked(&account.name, &format!("/service-accounts/{}", account.id))];
        chrome.crumbs = crumbs(&chrome.nav, above, here);
    }
    chrome
}

/// The teams the viewer can give `account` to: those they are in and above, but not its owner.
async fn givable_to(
    state: &AppState,
    signed: &Signed,
    account: &Account,
) -> Result<Vec<Team>, WebError> {
    Ok(super::teams::reach(state, signed)
        .await?
        .into_iter()
        .filter(|team| account.owner_team_id.as_ref() != Some(&team.id))
        .collect())
}

async fn account_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: &str,
    section: AccountSection,
    outcome: Outcome,
) -> Result<Html<String>, WebError> {
    let account = state.backend.account(signed.token(), id).await?;
    let tokens = match section {
        AccountSection::Tokens => state.backend.account_tokens(signed.token(), id).await?,
        _ => Vec::new(),
    };
    let (holdings, holdings_problem) = match section {
        AccountSection::Permissions => {
            match state.backend.account_holdings(signed.token(), id).await {
                Ok(holdings) => (Some(holdings), None),
                Err(err) => (None, Some(err.detail())),
            }
        }
        _ => (None, None),
    };
    let chrome = account_chrome(state, signed, csrf, &account, section.title()).await;
    let Outcome { created, notice, error } = outcome;
    let me = me(signed);
    let everyone = state.backend.teams(signed.token()).await?;
    let owner = owner(&account, &me, &everyone);
    let mine = account.owner_id.as_deref() == Some(me.as_str());
    let givable = !mine || !givable_to(state, signed, &account).await?.is_empty();
    let sections = AccountSection::ALL
        .into_iter()
        .map(|shown| Section::new(shown.title(), shown.href(&account.id), shown == section))
        .collect();
    let page = AccountPage {
        chrome,
        account,
        owner,
        section,
        sections,
        givable,
        tokens,
        holdings,
        holdings_problem,
        created,
        notice,
        error,
    };
    Ok(Html(page.render()?))
}

async fn account_form(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: &str,
    form: &'static str,
    values: Vec<(String, String)>,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let account = state.backend.account(signed.token(), id).await?;
    let here = match form {
        "owner" => "Give it to",
        "permission" => "Grant a permission",
        _ => "Issue a token",
    };
    let chrome = account_chrome(state, signed, csrf, &account, here).await;
    let mine = account.owner_id.as_deref() == Some(me(signed).as_str());
    let teams = match form {
        "owner" => givable_to(state, signed, &account).await?,
        _ => Vec::new(),
    };
    Ok(Html(AccountForm { chrome, account, form, teams, mine, values, error }.render()?))
}

pub async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    account_page(&state, &signed, &csrf, &id, AccountSection::Overview, Outcome::default()).await
}

pub async fn permissions(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    let section = AccountSection::Permissions;
    account_page(&state, &signed, &csrf, &id, section, Outcome::default()).await
}

pub async fn tokens(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    account_page(&state, &signed, &csrf, &id, AccountSection::Tokens, Outcome::default()).await
}

pub async fn owner_form(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    account_form(&state, &signed, &csrf, &id, "owner", Vec::new(), None).await
}

pub async fn permission_form(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    account_form(&state, &signed, &csrf, &id, "permission", Vec::new(), None).await
}

pub async fn token_form(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    account_form(&state, &signed, &csrf, &id, "token", Vec::new(), None).await
}

/// A refusal the person can act on is shown on the page; anything else is an error page.
fn refused(err: BackendError) -> Result<Outcome, WebError> {
    match err.status() {
        Some(400 | 403 | 404 | 409) => {
            Ok(Outcome { error: Some(err.detail()), ..Outcome::default() })
        }
        _ => Err(err.into()),
    }
}

#[derive(Debug, Deserialize)]
pub struct NewToken {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub days: String,
}

pub async fn create_token(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
    Form(form): Form<NewToken>,
) -> Result<Html<String>, WebError> {
    let values =
        vec![("name".to_string(), form.name.clone()), ("days".to_string(), form.days.clone())];
    let days = match form.days.trim() {
        "" => Ok(None),
        text => text
            .parse::<i64>()
            .ok()
            .filter(|days| (1..=MAX_DAYS).contains(days))
            .map(Some)
            .ok_or(()),
    };
    let Ok(days) = days else {
        let error = Some(format!("Expiry is 1 to {MAX_DAYS} days, or empty for never."));
        return account_form(&state, &signed, &csrf, &id, "token", values, error).await;
    };
    let name = Some(form.name.trim()).filter(|name| !name.is_empty());
    let outcome = match state.backend.create_account_token(signed.token(), &id, name, days).await {
        Ok(created) => {
            Outcome { created: Some(created.token.expose().clone()), ..Outcome::default() }
        }
        Err(err) => {
            let Outcome { error, .. } = refused(err)?;
            return account_form(&state, &signed, &csrf, &id, "token", values, error).await;
        }
    };
    account_page(&state, &signed, &csrf, &id, AccountSection::Tokens, outcome).await
}

pub async fn revoke_token(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, token)): Path<(String, String)>,
) -> Result<Html<String>, WebError> {
    let outcome = match state.backend.revoke_account_token(signed.token(), &id, &token).await {
        Ok(()) => Outcome { notice: Some("The token was revoked.".into()), ..Outcome::default() },
        Err(err) => refused(err)?,
    };
    account_page(&state, &signed, &csrf, &id, AccountSection::Tokens, outcome).await
}

#[derive(Debug, Deserialize)]
pub struct Permission {
    pub permission: String,
}

pub async fn grant(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
    Form(form): Form<Permission>,
) -> Result<Html<String>, WebError> {
    let permission = form.permission.trim();
    let outcome = match state.backend.grant_account(signed.token(), &id, permission).await {
        Ok(()) => Outcome { notice: Some(format!("Granted {permission}.")), ..Outcome::default() },
        Err(err) => {
            let Outcome { error, .. } = refused(err)?;
            let values = vec![("permission".to_string(), permission.to_string())];
            return account_form(&state, &signed, &csrf, &id, "permission", values, error).await;
        }
    };
    account_page(&state, &signed, &csrf, &id, AccountSection::Permissions, outcome).await
}

pub async fn revoke(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
    Form(form): Form<Permission>,
) -> Result<Html<String>, WebError> {
    let permission = form.permission.trim();
    let outcome = match state.backend.revoke_account(signed.token(), &id, permission).await {
        Ok(()) => Outcome { notice: Some(format!("Revoked {permission}.")), ..Outcome::default() },
        Err(err) => refused(err)?,
    };
    account_page(&state, &signed, &csrf, &id, AccountSection::Permissions, outcome).await
}

#[derive(Debug, Deserialize)]
pub struct Disabled {
    pub disabled: bool,
}

pub async fn set_disabled(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
    Form(form): Form<Disabled>,
) -> Result<Html<String>, WebError> {
    let outcome = match state.backend.set_account_disabled(signed.token(), &id, form.disabled).await
    {
        Ok(()) => Outcome {
            notice: Some(match form.disabled {
                true => "The account is disabled, and its tokens stopped working at once.".into(),
                false => "The account is enabled again.".into(),
            }),
            ..Outcome::default()
        },
        Err(err) => refused(err)?,
    };
    account_page(&state, &signed, &csrf, &id, AccountSection::Overview, outcome).await
}

#[derive(Debug, Deserialize)]
pub struct NewOwner {
    /// `me`, or the ID of a team the viewer is in or above.
    pub owner: String,
}

/// Gives the account to the viewer or to one of their teams.
pub async fn set_owner(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
    Form(form): Form<NewOwner>,
) -> Result<Html<String>, WebError> {
    let owner = match form.owner.trim() {
        "me" => serde_json::json!({ "user": me(&signed) }),
        team => match team.parse::<uuid::Uuid>() {
            Ok(team) => serde_json::json!({ "team": team }),
            Err(_) => {
                let error = Some("Choose who is to own it.".to_string());
                return account_form(&state, &signed, &csrf, &id, "owner", Vec::new(), error).await;
            }
        },
    };
    let outcome = match state.backend.set_account_owner(signed.token(), &id, &owner).await {
        Ok(_) => Outcome { notice: Some("It has a new owner.".into()), ..Outcome::default() },
        Err(err) => {
            let Outcome { error, .. } = refused(err)?;
            return account_form(&state, &signed, &csrf, &id, "owner", Vec::new(), error).await;
        }
    };
    account_page(&state, &signed, &csrf, &id, AccountSection::Overview, outcome).await
}
