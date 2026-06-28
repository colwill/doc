//! Your account: who you are to DOC, the accounts you sign in with, and linking more (ADR-0005).
//! After a first sign-in the welcome page offers each account that a plugin you can use needs, once
//! an administrator's first has been through setting DOC up.

use std::collections::BTreeMap;

use askama::Template;
use axum::extract::{Extension, Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use http::StatusCode;
use serde::Deserialize;
use serde_json::Value;
use uuid::Uuid;

use super::AppState;
use super::accounts::when;
use super::auth::{local_path, provider_name};
use super::csrf::Csrf;
use super::error::WebError;
use super::navigation::{Crumb, crumbs};
use super::pages::{Chrome, Section};
use crate::backend::{Access, BackendError, MyIdentities};
use crate::session::{self, Signed};

/// One of your accounts.
pub struct Linked {
    pub id: String,
    pub provider: String,
    pub login: String,
    pub since: String,
    pub last_used: Option<String>,
    pub how: &'static str,
    /// Where to change its password, for an account DOC keeps, when you can open that page.
    pub password: Option<String>,
    /// For an account DOC keeps whose page you can't open, which admins give a new password.
    pub ask_for_password: bool,
}

/// A provider you can link an account with, and the plugins you can use that need one.
pub struct Offer {
    pub provider: String,
    pub name: String,
    pub needed_by: Vec<String>,
}

impl Offer {
    /// The plugins that need it, as a sentence lists them: "A", "A and B", "A, B and C".
    pub fn plugins(&self) -> String {
        match self.needed_by.split_last() {
            None => String::new(),
            Some((last, [])) => last.clone(),
            Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        }
    }
}

/// What a plugin is called: its first navigation entry, or its provider's name when it has none.
pub fn plugin_label(id: &str, plugin: &crate::backend::PluginAccess) -> String {
    plugin.nav.first().map_or_else(|| provider_name(id), |nav| nav.label.clone())
}

/// Whether the account page shows its overview or the accounts you sign in with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MeSection {
    Overview,
    Accounts,
}

impl MeSection {
    fn title(self) -> &'static str {
        match self {
            Self::Overview => "Overview",
            Self::Accounts => "Linked accounts",
        }
    }

    fn href(self) -> &'static str {
        match self {
            Self::Overview => "/account",
            Self::Accounts => "/account/accounts",
        }
    }
}

/// The accounts you could link, on a page of their own.
#[derive(Template)]
#[template(path = "me_links.html")]
pub struct LinksPage {
    pub chrome: Chrome,
    pub offered: Vec<Offer>,
}

#[derive(Template)]
#[template(path = "me.html")]
pub struct MePage {
    pub chrome: Chrome,
    pub section: MeSection,
    pub sections: Vec<Section>,
    pub login: String,
    pub name: Option<String>,
    pub first_name: Option<String>,
    pub surname: Option<String>,
    pub email: Option<String>,
    pub accounts: Vec<Linked>,
    /// Whether one can go: you always keep one to sign in with.
    pub can_unlink: bool,
    pub offered: Vec<Offer>,
    pub notice: Option<String>,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "welcome.html")]
pub struct WelcomePage {
    pub chrome: Chrome,
    pub offered: Vec<Offer>,
    pub return_to: String,
    /// This page, to come back to after each link.
    pub here: String,
}

pub fn how(source: &str) -> &'static str {
    match source {
        "sign-in" => "Signed in with it",
        "link" => "Linked from this page",
        "admin" => "Linked by an administrator",
        "provider" => "Added by its provider",
        _ => "Linked",
    }
}

fn encoded(text: &str) -> String {
    url::form_urlencoded::byte_serialize(text.as_bytes()).collect()
}

/// Each provider the plugins you can use need an account with, and those plugins' names.
fn needed(access: &Access) -> BTreeMap<String, Vec<String>> {
    let mut needed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (id, plugin) in access.plugins.iter().filter(|(_, plugin)| plugin.read && plugin.running) {
        let label = plugin_label(id, plugin);
        for provider in &plugin.links {
            needed.entry(provider.clone()).or_default().push(label.clone());
        }
    }
    needed
}

/// The running providers you have no account with and can link one from, by signing in to them,
/// or only those a plugin needs.
fn offers(mine: &MyIdentities, access: &Access, only_needed: bool) -> Vec<Offer> {
    let needed = needed(access);
    mine.providers
        .iter()
        .filter(|provider| !provider.is_password())
        .filter(|provider| !mine.identities.iter().any(|held| held.provider == provider.id))
        .filter(|provider| !only_needed || needed.contains_key(&provider.id))
        .map(|provider| Offer {
            provider: provider.id.clone(),
            name: provider.title.clone(),
            needed_by: needed.get(&provider.id).cloned().unwrap_or_default(),
        })
        .collect()
}

impl MePage {
    fn on_accounts(&self) -> bool {
        self.section == MeSection::Accounts
    }
}

async fn render(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    section: MeSection,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let mine = state.backend.my_identities(signed.token()).await?;
    let access = session::access(state, signed).await;
    let offered = offers(&mine, &access, false);
    let text = |field: &str| signed.me.get(field).and_then(Value::as_str).map(str::to_string);
    // Changing a DOC account's password is the `local` plugin's page, which needs read access to it.
    let opens_local = access.plugins.get("local").is_some_and(|plugin| plugin.read);
    let accounts = mine
        .identities
        .iter()
        .map(|identity| {
            let kept = identity.provider == "local";
            Linked {
                id: identity.id.clone(),
                provider: provider_name(&identity.provider),
                login: identity.login.clone(),
                since: when(&identity.created_at),
                last_used: identity.last_used_at.as_ref().map(when),
                how: how(&identity.source),
                password: (kept && opens_local).then(|| "/p/local/password".to_string()),
                ask_for_password: kept && !opens_local,
            }
        })
        .collect();
    let mut chrome = Chrome::new("Your account", "/account")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    if section != MeSection::Overview {
        let above = vec![Crumb::linked("Your account", "/account")];
        chrome.crumbs = crumbs(&chrome.nav, above, section.title());
    }
    let sections = [MeSection::Overview, MeSection::Accounts]
        .into_iter()
        .map(|shown| Section::new(shown.title(), shown.href().to_string(), shown == section))
        .collect();
    let page = MePage {
        chrome,
        section,
        sections,
        login: text("login").unwrap_or_default(),
        name: text("name"),
        first_name: text("first_name"),
        surname: text("surname"),
        email: text("email"),
        can_unlink: mine.identities.len() > 1,
        accounts,
        offered,
        notice,
        error,
    };
    Ok(Html(page.render()?))
}

/// The account page with what was refused, answered with the refusal's status.
pub async fn refused(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    err: &BackendError,
) -> Response {
    let status = err
        .status()
        .filter(|status| (400..500).contains(status))
        .and_then(|status| StatusCode::from_u16(status).ok())
        .unwrap_or(StatusCode::BAD_GATEWAY);
    match render(state, signed, csrf, MeSection::Accounts, None, Some(err.detail())).await {
        Ok(page) => (status, page).into_response(),
        Err(err) => err.into_response(),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Shown {
    /// The provider an account was just linked with, which the page confirms if it is so.
    #[serde(default)]
    pub linked: Option<String>,
}

/// What the page confirms: that an account was linked, when one just was.
async fn linked(
    state: &AppState,
    signed: &Signed,
    shown: Shown,
) -> Result<Option<String>, WebError> {
    let Some(provider) = shown.linked else { return Ok(None) };
    let mine = state.backend.my_identities(signed.token()).await?;
    Ok(mine
        .identities
        .iter()
        .any(|held| held.provider == provider)
        .then(|| format!("Your {} account is linked.", provider_name(&provider))))
}

pub async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Query(shown): Query<Shown>,
) -> Result<Html<String>, WebError> {
    let notice = linked(&state, &signed, shown).await?;
    render(&state, &signed, &csrf, MeSection::Overview, notice, None).await
}

pub async fn accounts(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Query(shown): Query<Shown>,
) -> Result<Html<String>, WebError> {
    let notice = linked(&state, &signed, shown).await?;
    render(&state, &signed, &csrf, MeSection::Accounts, notice, None).await
}

pub async fn links(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    let mine = state.backend.my_identities(signed.token()).await?;
    let access = session::access(&state, &signed).await;
    let offered = offers(&mine, &access, false);
    let mut chrome = Chrome::new("Link another account", "/account")
        .signed(&signed, &csrf)
        .with_plugins(&state, &signed)
        .await;
    let above = vec![
        Crumb::linked("Your account", "/account"),
        Crumb::linked("Linked accounts", "/account/accounts"),
    ];
    chrome.crumbs = crumbs(&chrome.nav, above, "Link another account");
    Ok(Html(LinksPage { chrome, offered }.render()?))
}

#[derive(Debug, Deserialize)]
pub struct LinkForm {
    #[serde(default)]
    pub return_to: Option<String>,
}

#[derive(Template)]
#[template(path = "link_bounce.html")]
pub struct LinkBouncePage {
    pub location: String,
    pub name: String,
}

/// Sends you to sign in to the provider, which links the account and brings you back.
///
/// This can't answer with an HTTP redirect straight to the provider: Chrome checks `form-action`
/// against a form submission's whole redirect chain, not just its own `action`, so a redirect
/// leaving `'self'` here is blocked even though the form posted to this same origin. A same-origin
/// 200 page that bounces onward is a fresh navigation, which `form-action` does not cover.
pub async fn link(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(provider): Path<String>,
    Form(form): Form<LinkForm>,
) -> Result<Response, WebError> {
    let back = match form.return_to.as_deref() {
        Some(to) => local_path(Some(to)),
        None => format!("/account/accounts?linked={}", encoded(&provider)),
    };
    match state.backend.start_link(signed.token(), &provider, &back).await {
        Ok(location) if location.starts_with("https://") || location.starts_with("http://") => {
            let page = LinkBouncePage { location, name: provider_name(&provider) };
            Ok(Html(page.render()?).into_response())
        }
        Ok(_) => Err(WebError::NotFound),
        Err(err) if err.status().is_some_and(|status| (400..500).contains(&status)) => {
            Ok(refused(&state, &signed, &csrf, &err).await)
        }
        Err(err) => Err(err.into()),
    }
}

pub async fn unlink(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<Uuid>,
) -> Result<Response, WebError> {
    match state.backend.unlink_mine(signed.token(), &id.to_string()).await {
        Ok(()) => {
            session::forget_access(&state).await;
            let notice = Some("The account is unlinked.".to_string());
            let section = MeSection::Accounts;
            Ok(render(&state, &signed, &csrf, section, notice, None).await?.into_response())
        }
        Err(err) if err.status().is_some_and(|status| (400..500).contains(&status)) => {
            Ok(refused(&state, &signed, &csrf, &err).await)
        }
        Err(err) => Err(err.into()),
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct Welcomed {
    #[serde(default)]
    pub return_to: Option<String>,
}

/// After a first sign-in: each account a plugin you can use needs, to link now or skip. With none
/// left to offer, you go straight on.
pub async fn welcome(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Query(welcomed): Query<Welcomed>,
) -> Result<Response, WebError> {
    let return_to = local_path(welcomed.return_to.as_deref());
    let access = session::access(&state, &signed).await;
    // An administrator's first sign-in to a DOC nobody has set up opens the setup, which comes
    // back here when it is finished or skipped.
    if access.admin && super::setup::unopened(&state, &signed).await {
        return Ok(Redirect::to("/setup").into_response());
    }
    let mine = state.backend.my_identities(signed.token()).await?;
    let offered = offers(&mine, &access, true);
    if offered.is_empty() {
        return Ok(Redirect::to(&return_to).into_response());
    }
    let here = format!("/welcome?return_to={}", encoded(&return_to));
    let chrome = Chrome::new("Welcome", "/welcome")
        .headed("Welcome to DOC")
        .signed(&signed, &csrf)
        .with_plugins(&state, &signed)
        .await;
    Ok(Html(WelcomePage { chrome, offered, return_to, here }.render()?).into_response())
}
