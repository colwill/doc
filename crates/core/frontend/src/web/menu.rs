//! A menu's own page: `Workspace` is not a page anywhere, but it gathers pages, so `/menu/workspace`
//! lists what is in it as cards. It is what the breadcrumb on every page under a menu leads back
//! to, and it holds only the entries this viewer may open.

use askama::Template;
use axum::extract::{Extension, Path, State};
use axum::response::Html;

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::navigation::{Card, menu_slug, offered};
use super::pages::Chrome;
use crate::session::{self, Signed};

#[derive(Template)]
#[template(path = "menu.html")]
pub struct MenuPage {
    pub chrome: Chrome,
    pub label: String,
    pub description: Option<String>,
    pub cards: Vec<Card>,
}

/// One sentence for each menu the platform names itself, so its page says what it gathers. A menu
/// a plugin or an administrator invents has none, and its page is the cards alone.
fn described(label: &str) -> Option<&'static str> {
    match label {
        "Workspace" => {
            Some("The day to day: what is happening, what is written down, and what is due.")
        }
        "Platform" => Some(
            "What this organisation runs: its catalogue, its infrastructure and how it all connects.",
        ),
        "Technology" => {
            Some("The technology this organisation uses, and what it is moving towards.")
        }
        "Access" => Some("Who works here, who they work with, and what they can reach."),
        "Watercooler" => Some("The social side: talking, thanking, and what is coming up."),
        "Admin" => Some("Running the platform itself."),
        _ => None,
    }
}

pub async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Path(slug): Path<String>,
) -> Result<Html<String>, WebError> {
    let here = format!("/menu/{slug}");
    let access = session::access(&state, &signed).await;
    let offered = offered(&access);
    let nav = super::navigation::arrange(&offered, access.navigation.as_ref(), &access, &here);
    let menu = nav
        .iter()
        .find(|item| item.is_group() && menu_slug(&item.label) == slug)
        .ok_or(WebError::NotFound)?;
    let cards = menu
        .children
        .iter()
        .map(|child| Card {
            description: offered
                .iter()
                .find(|page| page.href == child.href)
                .and_then(|page| page.description.clone()),
            label: child.label.clone(),
            href: child.href.clone(),
        })
        .collect();
    let label = menu.label.clone();
    let description = described(&label).map(str::to_string);
    let chrome = Chrome::new(label.clone(), &here)
        .signed(&signed, &csrf)
        .with_plugins(&state, &signed)
        .await;
    Ok(Html(MenuPage { chrome, label, description, cards }.render()?))
}
