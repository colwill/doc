//! The plugins page and each plugin's page: state, errors and history for `core` readers, and on
//! a plugin's page, enabling, reloading, unloading, cancelling, resuming, turning it off and on,
//! the flag it follows and registration tokens for `core` writers.

use askama::Template;
use axum::extract::{Extension, Form, Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use http::HeaderMap;
use serde::Deserialize;

use super::AppState;
use super::categories::Section;
use super::csrf::Csrf;
use super::error::WebError;
use super::pages::Chrome;
use crate::backend::{PluginDetail, PluginList, PluginRow, TokenView};
use crate::session::{self, Signed};

/// What a plugin's page offers a `core` writer, each posted to `/plugins/{id}/{action}`.
const ACTIONS: [&str; 7] =
    ["enable", "reload", "unload", "cancel", "resume", "turn-off", "turn-on"];
const MAX_DAYS: i64 = 365;

/// A lifecycle state's badge modifier and its word.
pub fn badge(state: Option<&str>) -> (&'static str, String) {
    match state {
        Some("running") => ("up", "Running".into()),
        Some("loading") => ("loading", "Loading".into()),
        Some("cancelled") => ("degraded", "Cancelled".into()),
        Some("unloading") => ("unloading", "Unloading".into()),
        Some("error") => ("down", "Error".into()),
        Some("removed") => ("unknown", "Removed".into()),
        Some(other) => ("unknown", other.to_string()),
        None => ("unknown", "Not running".into()),
    }
}

pub fn when(at: &chrono::DateTime<chrono::Utc>) -> String {
    at.format("%-d %b %H:%M:%S UTC").to_string()
}

/// Whether this viewer may administer the platform's plugins, which is what the Registration
/// tokens tab needs; the settings tabs are a plugin's own permission (ADR-0007).
pub async fn writes_core(state: &AppState, signed: &Signed) -> bool {
    writes(state, signed).await
}

async fn writes(state: &AppState, signed: &Signed) -> bool {
    let access = session::access(state, signed).await;
    access.admin || access.plugins.get("core").is_some_and(|core| core.write)
}

#[derive(Template)]
#[template(path = "plugins_panel.html")]
pub struct PluginsPanel {
    /// The plugins, a section per category.
    pub sections: Vec<Section<PluginRow>>,
    /// Whether each row offers **Edit**, which opens the plugin's page and what it can do there.
    pub writes: bool,
}

impl PluginsPanel {
    fn badge(state: &Option<String>) -> (&'static str, String) {
        badge(state.as_deref())
    }

    fn when(at: &chrono::DateTime<chrono::Utc>) -> String {
        when(at)
    }
}

#[derive(Template)]
#[template(path = "plugins.html")]
pub struct PluginsPage {
    pub chrome: Chrome,
    pub panel: PluginsPanel,
}

async fn panel(state: &AppState, signed: &Signed) -> Result<PluginsPanel, WebError> {
    let PluginList { plugins, .. } = state.backend.plugins(signed.token()).await?;
    let categories = session::access(state, signed).await.categories;
    let sections = super::categories::sections(&categories, plugins, |plugin| &plugin.id);
    Ok(PluginsPanel { sections, writes: writes(state, signed).await })
}

pub async fn list(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    let panel = panel(&state, &signed).await?;
    let chrome = Chrome::new("Plugins", "/plugins")
        .signed(&signed, &csrf)
        .with_plugins(&state, &signed)
        .await;
    Ok(Html(PluginsPage { chrome, panel }.render()?))
}

pub async fn list_panel(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Html<String>, WebError> {
    Ok(Html(panel(&state, &signed).await?.render()?))
}

/// One of the plugin page's buttons. The page comes back saying what happened, or why it was
/// refused — such as turning off a plugin the platform cannot do without — rather than as an
/// error page; HTMX picks the state and the buttons out of it.
pub async fn act(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path((id, action)): Path<(String, String)>,
) -> Result<Html<String>, WebError> {
    if !ACTIONS.contains(&action.as_str()) {
        return Err(WebError::NotFound);
    }
    let done = match action.as_str() {
        "enable" => state.backend.enable_plugin(signed.token(), &id).await.map(|saved| {
            let changed: Vec<&str> = saved["changed"]
                .as_array()
                .map(|changed| changed.iter().filter_map(serde_json::Value::as_str).collect())
                .unwrap_or_default();
            match changed.is_empty() {
                true => format!("{id} was already enabled."),
                false => format!("{id} is enabled: turned on {}.", changed.join(", ")),
            }
        }),
        action => {
            state.backend.plugin_action(signed.token(), &id, action).await.map(|()| match action {
                "reload" => format!("{id} is being reloaded."),
                "unload" => format!("{id} is unloaded."),
                "cancel" => format!("{id}'s work is cancelled."),
                "resume" => format!("{id} is resumed."),
                "turn-off" => format!("{id} is turned off."),
                _ => format!("{id} is turned on."),
            })
        }
    };
    let (notice, error) = match done {
        Ok(notice) => (Some(notice), None),
        Err(err) if matches!(err.status(), Some(403 | 404 | 409)) => (None, Some(err.detail())),
        Err(err) => return Err(err.into()),
    };
    detail_page(&state, &signed, &csrf, &id, notice, error).await
}

#[derive(Template)]
#[template(path = "plugin_detail.html")]
pub struct DetailPage {
    pub chrome: Chrome,
    pub detail: PluginDetail,
    /// Which of the plugin's tabs this viewer is offered (ADR-0007). Each tab's own route checks
    /// for itself, so leaving one out here is what the page shows, not what keeps anybody out.
    pub settings_tab: bool,
    pub features_tab: bool,
    pub tokens_tab: bool,
    /// Required settings nobody has set, which is why an unconfigured plugin serves so little.
    pub missing: Vec<String>,
    /// Whether this viewer administers the platform's plugins, and so is offered its buttons and
    /// may choose a flag for it.
    pub writes: bool,
    /// What the button just pressed did, or why it was refused.
    pub notice: Option<String>,
    pub error: Option<String>,
}

/// The plugin that keeps the flags, which the platform reads the flags it follows from.
const FLAGS: &str = "flags";
/// The service the platform reads its flags as.
const PLATFORM_SERVICE: &str = "doc";
/// How many flags the **Follow a flag** field offers at once.
const MAX_FLAG_OPTIONS: usize = 20;

/// Choosing the flag a plugin is turned on and off by, on a page of its own.
#[derive(Template)]
#[template(path = "plugin_flag.html")]
pub struct FlagPage {
    pub chrome: Chrome,
    pub plugin_id: String,
    pub flag: String,
    pub error: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct FollowForm {
    #[serde(default)]
    pub flag: String,
}

/// What the **Follow a flag** field offers as its key is typed.
#[derive(Template)]
#[template(path = "flag_options.html")]
pub struct FlagOptions {
    pub flags: Vec<FlagOption>,
    pub text: String,
    /// Why nothing can be offered at all, rather than nothing matching.
    pub unavailable: Option<&'static str>,
}

/// A flag a plugin could follow, and what it is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagOption {
    pub key: String,
    pub on: bool,
}

/// The Registration tokens tab, which is a platform administrator's.
#[derive(Template)]
#[template(path = "plugin_tokens.html")]
pub struct TokensPage {
    pub chrome: Chrome,
    pub plugin_id: String,
    pub settings_tab: bool,
    pub features_tab: bool,
    pub tokens_tab: bool,
    pub tokens: Vec<TokenView>,
    pub created: Option<String>,
}

/// Issuing a registration token, on a page of its own.
#[derive(Template)]
#[template(path = "plugin_token_new.html")]
pub struct NewTokenPage {
    pub chrome: Chrome,
    pub plugin_id: String,
    pub values: NewToken,
    pub error: Option<String>,
}

impl TokensPage {
    fn when(at: &chrono::DateTime<chrono::Utc>) -> String {
        when(at)
    }
}

impl DetailPage {
    fn badge(state: &Option<String>) -> (&'static str, String) {
        badge(state.as_deref())
    }

    fn state_badge(state: &str) -> (&'static str, String) {
        badge(Some(state))
    }

    fn when(at: &chrono::DateTime<chrono::Utc>) -> String {
        when(at)
    }
}

/// Which tabs a viewer is offered on a plugin's page: what their access allows, and what the
/// plugin actually has. Worked out in one place, so every tab of the page shows the same set —
/// opening one must never make another disappear.
#[derive(Debug, Clone, Copy, Default)]
pub struct Tabs {
    pub settings: bool,
    pub features: bool,
    pub tokens: bool,
}

pub async fn tabs(state: &AppState, signed: &Signed, id: &str) -> Tabs {
    let may = may_configure(state, signed, id).await;
    // A plugin with no features has no Features tab, whichever of its tabs is open.
    let configured = match may {
        true => state.backend.plugin_settings(signed.token(), id).await.ok(),
        false => None,
    };
    tabs_from(may, configured.as_ref(), writes(state, signed).await)
}

/// The same, for a page that has already read the plugin's settings for its own sake.
pub fn tabs_from(
    may_configure: bool,
    settings: Option<&crate::backend::PluginSettings>,
    writes: bool,
) -> Tabs {
    Tabs {
        settings: may_configure,
        features: settings.is_some_and(|settings| !settings.features.is_empty()),
        tokens: writes,
    }
}

async fn may_configure(state: &AppState, signed: &Signed, id: &str) -> bool {
    let access = session::access(state, signed).await;
    access.admin || access.plugins.get(id).is_some_and(|entry| entry.settings)
}

pub async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    detail_page(&state, &signed, &csrf, &id, None, None).await
}

async fn detail_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: &str,
    notice: Option<String>,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    let detail = state.backend.plugin(signed.token(), id).await?;
    // Read once: the same answer says which tabs this plugin has and what it still needs set.
    let may = may_configure(state, signed, id).await;
    let configured = match may {
        true => state.backend.plugin_settings(signed.token(), id).await.ok(),
        false => None,
    };
    let tabs = tabs_from(may, configured.as_ref(), writes(state, signed).await);
    // What is missing is worth saying on the Overview tab, since it explains a plugin that is
    // running and still doing nothing. Only to somebody who may see the settings at all.
    let missing = configured.map(|settings| settings.missing).unwrap_or_default();
    let chrome = Chrome::new(id.to_string(), "/plugins")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await
        .about_plugin(id);
    Ok(Html(
        DetailPage {
            chrome,
            detail,
            settings_tab: tabs.settings,
            features_tab: tabs.features,
            tokens_tab: tabs.tokens,
            missing,
            writes: tabs.tokens,
            notice,
            error,
        }
        .render()?,
    ))
}

async fn flag_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: &str,
    flag: String,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    if !writes(state, signed).await {
        return Err(WebError::NotFound);
    }
    let chrome = Chrome::new("Follow a flag", "/plugins")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    Ok(Html(FlagPage { chrome, plugin_id: id.to_string(), flag, error }.render()?))
}

/// The flags the **Follow a flag** field offers: those the platform reads as the service `doc`
/// that are true or false, which are all a plugin can follow. The flags plugin is asked as the
/// person typing, so they are offered only what their own access lets them read.
pub async fn flag_options(
    State(state): State<AppState>,
    signed: Signed,
    Path(_id): Path<String>,
    Query(typed): Query<FollowForm>,
) -> Result<Html<String>, WebError> {
    if !writes(&state, &signed).await {
        return Err(WebError::NotFound);
    }
    let text = typed.flag.trim().to_lowercase();
    let route = format!("evaluate?service={PLATFORM_SERVICE}");
    let (flags, unavailable) = match state.backend.plugin_get(signed.token(), FLAGS, &route).await {
        Ok(read) => (followable(&read, &text), None),
        Err(err) if err.status() == Some(403) => {
            (Vec::new(), Some("You may not read feature flags, so none can be offered."))
        }
        Err(err) => {
            tracing::debug!(%err, "the flags plugin did not answer for the flag field");
            (Vec::new(), Some("Feature flags cannot be read now."))
        }
    };
    Ok(Html(FlagOptions { flags, text, unavailable }.render()?))
}

/// The true-or-false flags in what the flags plugin read, the way the platform finds a followed
/// flag: among the flags, then among the configuration. Those starting with what was typed come
/// first, then those that only contain it.
fn followable(read: &serde_json::Value, text: &str) -> Vec<FlagOption> {
    let mut found: Vec<FlagOption> = Vec::new();
    for role in ["flags", "config"] {
        let Some(values) = read[role].as_object() else { continue };
        for (key, value) in values {
            let Some(on) = value.as_bool() else { continue };
            if key.to_lowercase().contains(text) && found.iter().all(|flag| flag.key != *key) {
                found.push(FlagOption { key: key.clone(), on });
            }
        }
    }
    found.sort_by(|a, b| {
        let starts = |flag: &FlagOption| !flag.key.to_lowercase().starts_with(text);
        starts(a).cmp(&starts(b)).then_with(|| a.key.cmp(&b.key))
    });
    found.truncate(MAX_FLAG_OPTIONS);
    found
}

pub async fn follow_page(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    flag_page(&state, &signed, &csrf, &id, String::new(), None).await
}

/// **Follow it**, or **Stop following** with no flag, which leaves the plugin as it is. A refusal
/// comes back to the form, since it says what is wrong with the flag.
pub async fn follow(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
    Form(form): Form<FollowForm>,
) -> Result<Response, WebError> {
    let flag = Some(form.flag.trim()).filter(|flag| !flag.is_empty());
    match state.backend.set_plugin_flag(signed.token(), &id, flag).await {
        Ok(_) => Ok(Redirect::to(&format!("/plugins/{id}")).into_response()),
        Err(err) if flag.is_some() && matches!(err.status(), Some(400 | 409)) => {
            let page = flag_page(&state, &signed, &csrf, &id, form.flag, Some(err.detail()));
            Ok(page.await?.into_response())
        }
        Err(err) => Err(err.into()),
    }
}

async fn tokens_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: &str,
    created: Option<String>,
) -> Result<Html<String>, WebError> {
    let tokens = state.backend.plugin_tokens(signed.token(), id).await?;
    let tabs = tabs(state, signed, id).await;
    let chrome = Chrome::new(id.to_string(), "/plugins")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await
        .about_plugin(id);
    Ok(Html(
        TokensPage {
            chrome,
            plugin_id: id.to_string(),
            settings_tab: tabs.settings,
            features_tab: tabs.features,
            tokens_tab: tabs.tokens,
            tokens,
            created,
        }
        .render()?,
    ))
}

async fn new_token_page(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    id: &str,
    values: NewToken,
    error: Option<String>,
) -> Result<Html<String>, WebError> {
    // Only whoever sees the tab issues its tokens.
    if !writes(state, signed).await {
        return Err(WebError::NotFound);
    }
    let chrome = Chrome::new("Issue a registration token", "/plugins")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    Ok(Html(NewTokenPage { chrome, plugin_id: id.to_string(), values, error }.render()?))
}

pub async fn new_token(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    new_token_page(&state, &signed, &csrf, &id, NewToken::default(), None).await
}

pub async fn tokens(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(id): Path<String>,
) -> Result<Html<String>, WebError> {
    tokens_page(&state, &signed, &csrf, &id, None).await
}

#[derive(Debug, Default, Deserialize)]
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
    let days = match form.days.trim() {
        "" => None,
        text => match text.parse::<i64>().ok().filter(|days| (1..=MAX_DAYS).contains(days)) {
            Some(days) => Some(days),
            None => {
                let problem = format!("Expiry is 1 to {MAX_DAYS} days, or empty for never.");
                return new_token_page(&state, &signed, &csrf, &id, form, Some(problem)).await;
            }
        },
    };
    let name = Some(form.name.trim()).filter(|name| !name.is_empty());
    let created = state.backend.create_plugin_token(signed.token(), &id, name, days).await?;
    tokens_page(&state, &signed, &csrf, &id, Some(created.token.expose().clone())).await
}

pub async fn revoke_token(
    State(state): State<AppState>,
    signed: Signed,
    Path((id, token)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, WebError> {
    state.backend.revoke_plugin_token(signed.token(), &id, &token).await?;
    if headers.contains_key("hx-request") {
        return Ok(
            Html("<strong class=\"doc-badge doc-badge--down\">Revoked</strong>").into_response()
        );
    }
    Ok(Redirect::to(&format!("/plugins/{id}/tokens")).into_response())
}
