//! A person's dashboard, the page they land on when they sign in: what they chose to see at a
//! glance, each item drawn by the plugin that offers it, as them. They choose on a page of their
//! own, as an administrator arranges the sections; the sections the landing page used to open with
//! are a page of their own, **All sections**.

use askama::Template;
use axum::extract::{Extension, Form, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use http::StatusCode;
use serde_json::Value;

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::navigation::{Card, landing};
use super::pages::Chrome;
use crate::backend::Access;
use crate::session::{self, Signed};

/// The platform's own item: plugins asking to join a list in a plugin's settings.
pub const ACCESS_REQUESTS: &str = "core/access-requests";
/// The most waiting requests the item lists before linking to the rest.
const LISTED: usize = 5;

/// Something that can be on a dashboard.
pub struct Offer {
    /// `plugin/id`, which is how a dashboard names it.
    pub key: String,
    pub label: String,
    pub description: String,
    /// Which part of DOC it comes from, in the words its menu entry uses.
    pub from: String,
    /// The fragment it is drawn from.
    pub href: String,
    pub wide: bool,
}

/// Everything the person may put on their dashboard: the platform's own, then what each plugin they
/// can read and that is running offers, in plugin order.
pub fn offers(access: &Access) -> Vec<Offer> {
    let mut offers = Vec::new();
    if access.admin || access.plugins.values().any(|held| held.settings_write) {
        offers.push(Offer {
            key: ACCESS_REQUESTS.into(),
            label: "Access requests for you to decide".into(),
            description:
                "Plugins asking to join a list in the settings of a plugin you look after.".into(),
            from: "DOC".into(),
            href: "/dashboard/access-requests".into(),
            wide: false,
        });
    }
    for (plugin, held) in &access.plugins {
        if !held.read || !held.running {
            continue;
        }
        let from = held.nav.first().map_or_else(|| plugin.clone(), |entry| entry.label.clone());
        for item in &held.dashboard {
            offers.push(Offer {
                key: format!("{plugin}/{}", item.id),
                label: item.label.clone(),
                description: item.description.clone(),
                from: from.clone(),
                href: format!("/p/{plugin}{}", item.path),
                wide: item.wide,
            });
        }
    }
    offers
}

/// What the person's dashboard shows: the items they chose, or the starter set, in their order,
/// leaving out any nothing offers them now, such as a plugin that is not running.
pub async fn shown(state: &AppState, signed: &Signed, access: &Access) -> (Vec<Offer>, bool) {
    let dashboard = match state.backend.dashboard(signed.token()).await {
        Ok(dashboard) => dashboard,
        Err(err) => {
            tracing::warn!(%err, "the dashboard could not be read");
            return (Vec::new(), false);
        }
    };
    let mut offered = offers(access);
    let mut shown = Vec::new();
    for key in &dashboard.items {
        if let Some(at) = offered.iter().position(|offer| offer.key == *key) {
            shown.push(offered.remove(at));
        }
    }
    (shown, dashboard.chosen)
}

#[derive(Template)]
#[template(path = "sections.html")]
pub struct SectionsPage {
    pub chrome: Chrome,
    pub cards: Vec<Card>,
}

/// `GET /sections`: the pages an administrator chose, as cards, which the landing page used to be.
pub async fn sections(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    let access = session::access(&state, &signed).await;
    let chrome = Chrome::new("All sections", "/sections")
        .signed(&signed, &csrf)
        .with_plugins(&state, &signed)
        .await;
    let cards = landing(&access).into_iter().filter(|card| card.href != "/").collect();
    Ok(Html(SectionsPage { chrome, cards }.render()?))
}

/// An item as the choosing form lists it.
pub struct Row {
    pub key: String,
    pub label: String,
    pub description: String,
    pub from: String,
    pub chosen: bool,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct ChoosePage {
    pub chrome: Chrome,
    pub rows: Vec<Row>,
    pub error: Option<String>,
}

impl ChoosePage {
    /// Where the row is, counted from one.
    fn order(index: &usize) -> usize {
        index + 1
    }
}

/// What the dashboard shows now, in its order, then everything else it could, so the form reads
/// as the dashboard does.
fn rows(offered: Vec<Offer>, chosen: &[String]) -> Vec<Row> {
    let row = |offer: Offer, chosen: bool| Row {
        key: offer.key,
        label: offer.label,
        description: offer.description,
        from: offer.from,
        chosen,
    };
    let mut rest = offered;
    let mut rows = Vec::new();
    for key in chosen {
        if let Some(at) = rest.iter().position(|offer| offer.key == *key) {
            rows.push(row(rest.remove(at), true));
        }
    }
    rows.extend(rest.into_iter().map(|offer| row(offer, false)));
    rows
}

async fn render(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    rows: Vec<Row>,
    error: Option<String>,
) -> Result<Response, WebError> {
    let chrome = Chrome::new("Choose what you see", "/dashboard")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    let status = if error.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    Ok((status, Html(ChoosePage { chrome, rows, error }.render()?)).into_response())
}

/// `GET /dashboard`: everything the person may see, the chosen ticked and in their order.
pub async fn choose(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Response, WebError> {
    let access = session::access(&state, &signed).await;
    let dashboard = state.backend.dashboard(signed.token()).await?;
    let rows = rows(offers(&access), &dashboard.items);
    render(&state, &signed, &csrf, rows, None).await
}

/// The items ticked, in the order asked for.
fn ticked(form: &[(String, String)]) -> Vec<String> {
    let field = |name: &str, at: usize| {
        let key = format!("{name}.{at}");
        form.iter().find(|(field, _)| *field == key).map(|(_, value)| value.trim())
    };
    let mut picked: Vec<(usize, usize, String)> = Vec::new();
    let indices =
        form.iter().filter_map(|(name, _)| name.strip_prefix("item.")?.parse::<usize>().ok());
    for at in indices {
        let item = field("item", at).unwrap_or_default().to_string();
        if item.is_empty() || field("chosen", at).is_none() {
            continue;
        }
        let order = field("order", at).and_then(|order| order.parse().ok()).unwrap_or(usize::MAX);
        picked.push((order, at, item));
    }
    picked.sort_by_key(|(order, at, _)| (*order, *at));
    picked.into_iter().map(|(_, _, item)| item).collect()
}

/// `POST /dashboard`: saves the items ticked, in their order, and goes back to the dashboard.
pub async fn save(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let items = ticked(&form);
    match state.backend.set_dashboard(signed.token(), &items).await {
        Ok(_) => Ok(Redirect::to("/").into_response()),
        Err(err) if err.status() == Some(400) => {
            let access = session::access(&state, &signed).await;
            let rows = rows(offers(&access), &items);
            render(&state, &signed, &csrf, rows, Some(err.detail())).await
        }
        Err(err) => Err(err.into()),
    }
}

/// A plugin waiting for somebody to let it join a list in another's settings.
pub struct Waiting {
    pub requester: String,
    pub setting: String,
    pub target: String,
    pub reason: String,
    pub href: String,
}

#[derive(Template)]
#[template(path = "dashboard_requests.html")]
pub struct RequestsItem {
    pub waiting: Vec<Waiting>,
    pub more: usize,
}

fn text(value: &Value, field: &str) -> String {
    value[field].as_str().unwrap_or_default().to_string()
}

/// `GET /dashboard/access-requests`: the platform's own item, drawn into the dashboard.
pub async fn access_requests(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Html<String>, WebError> {
    let answer = state.backend.waiting_requests(signed.token()).await?;
    let all: Vec<Waiting> = answer["requests"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|request| Waiting {
            requester: text(request, "requester"),
            setting: text(request, "setting_label"),
            reason: text(request, "reason"),
            href: format!("/plugins/{}/settings", text(request, "target")),
            target: text(request, "target"),
        })
        .collect();
    let more = all.len().saturating_sub(LISTED);
    let waiting = all.into_iter().take(LISTED).collect();
    Ok(Html(RequestsItem { waiting, more }.render()?))
}
