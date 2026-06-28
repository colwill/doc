//! The landing page's cards: which pages open the platform, and in what order. One choice for the
//! whole platform, made by a `core` writer and kept beside the navigation's layout, so a person
//! still sees only the cards for pages they may open.

use askama::Template;
use axum::extract::{Extension, Form, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use http::StatusCode;

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::navigation::{DEFAULT_LANDING, Offered, Saved, forbidden, offered, writes};
use super::pages::Chrome;
use crate::backend::Layout;
use crate::session::Signed;

/// A page as the choosing form lists it.
pub struct Row {
    pub href: String,
    pub label: String,
    pub description: String,
    pub chosen: bool,
}

#[derive(Template)]
#[template(path = "landing.html")]
pub struct LandingPage {
    pub chrome: Chrome,
    pub rows: Vec<Row>,
    pub error: Option<String>,
    pub saved: bool,
}

impl LandingPage {
    /// Where the row is, counted from one, since the order is changed by dragging rather than by
    /// typing a number between two others.
    fn order(index: &usize) -> usize {
        index + 1
    }
}

/// What the landing page shows now: the chosen pages in their order, then everything else, so the
/// form reads as the page does. With nothing chosen, the cards it opens with are ticked.
fn rows(offered: &[Offered], layout: &Layout) -> Vec<Row> {
    let chosen: Vec<String> = match layout.landing.is_empty() {
        true => DEFAULT_LANDING.iter().map(|href| (*href).to_string()).collect(),
        false => layout.landing.clone(),
    };
    let row = |page: &Offered, chosen: bool| Row {
        href: page.href.clone(),
        label: page.label.clone(),
        description: page.description.clone().unwrap_or_default(),
        chosen,
    };
    let mut rows: Vec<Row> = chosen
        .iter()
        .filter_map(|href| offered.iter().find(|page| page.href == *href))
        .map(|page| row(page, true))
        .collect();
    rows.extend(
        offered
            .iter()
            .filter(|page| page.visible && !chosen.contains(&page.href))
            .map(|page| row(page, false)),
    );
    rows
}

async fn render(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    rows: Vec<Row>,
    error: Option<String>,
    saved: bool,
) -> Result<Response, WebError> {
    let chrome = Chrome::new("All sections", "/landing")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    let status = if error.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    Ok((status, Html(LandingPage { chrome, rows, error, saved }.render()?)).into_response())
}

pub async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Query(saved): Query<Saved>,
) -> Result<Response, WebError> {
    let Some(access) = writes(&state, &signed).await else {
        return Ok(forbidden());
    };
    let layout = state.backend.navigation(signed.token()).await?;
    let rows = rows(&offered(&access), &layout);
    render(&state, &signed, &csrf, rows, None, saved.is_saved()).await
}

/// The pages ticked, in the order asked for.
fn chosen(form: &[(String, String)]) -> Result<Vec<String>, String> {
    let field = |name: &str, at: usize| {
        let key = format!("{name}.{at}");
        form.iter().find(|(field, _)| *field == key).map(|(_, value)| value.trim())
    };
    let mut picked: Vec<(usize, usize, String)> = Vec::new();
    let indices =
        form.iter().filter_map(|(name, _)| name.strip_prefix("href.")?.parse::<usize>().ok());
    for at in indices {
        let href = field("href", at).unwrap_or_default().to_string();
        if href.is_empty() || field("chosen", at).is_none() {
            continue;
        }
        let order = match field("order", at).unwrap_or_default() {
            "" => usize::MAX,
            text => {
                text.parse().map_err(|_| format!("The order of {href} is not a whole number."))?
            }
        };
        picked.push((order, at, href));
    }
    picked.sort_by_key(|(order, at, _)| (*order, *at));
    Ok(picked.into_iter().map(|(_, _, href)| href).collect())
}

pub async fn save(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let Some(access) = writes(&state, &signed).await else {
        return Ok(forbidden());
    };
    let reset = form.iter().any(|(name, _)| name == "reset");
    let landing = match (reset, chosen(&form)) {
        (true, _) => Vec::new(),
        (false, Ok(landing)) => landing,
        (false, Err(problem)) => {
            let layout = state.backend.navigation(signed.token()).await?;
            let rows = rows(&offered(&access), &layout);
            return render(&state, &signed, &csrf, rows, Some(problem), false).await;
        }
    };
    // The bar's layout is kept as it is: this page changes only which cards the landing page opens
    // with, and both are held in the one layout.
    let layout = Layout { landing, ..state.backend.navigation(signed.token()).await? };
    match state.backend.set_navigation(signed.token(), &layout).await {
        Ok(_) => {}
        Err(err) if err.status() == Some(400) => {
            let rows = rows(&offered(&access), &layout);
            return render(&state, &signed, &csrf, rows, Some(err.detail()), false).await;
        }
        Err(err) => return Err(err.into()),
    }
    crate::session::forget_access(&state).await;
    Ok(Redirect::to("/landing?saved=1").into_response())
}
