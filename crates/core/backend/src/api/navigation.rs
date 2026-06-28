//! How the navigation is arranged: one layout for the whole platform, set by `core` writers. It
//! orders, renames, groups and hides pages, and can add links of its own; it never grants anything,
//! since the frontend still shows each person only the pages they may open.

use axum::Json;
use axum::extract::State;
use doc_permissions::{Access, CORE};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::AppState;
use super::problem::Problem;
use crate::auth::Auth;
use crate::identity::AuditEntry;
use crate::permissions;

pub const MAX_ENTRIES: usize = 200;
/// Cards the landing page can be given; more than this is a list, not a landing page.
pub const MAX_LANDING: usize = 24;
pub const MAX_LABEL: usize = 60;
pub const MAX_HREF: usize = 512;
/// Announced whenever the layout changes, so every frontend drops the navigation it cached.
pub const CHANGED: &str = "platform.navigation.changed";

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Layout {
    /// In the order they are shown; a group sits where its first entry does.
    #[serde(default)]
    pub entries: Vec<Entry>,
    /// The pages the landing page shows as cards, in order. Empty leaves the frontend its own
    /// choice, so a platform nobody has arranged still lands on something useful.
    #[serde(default)]
    pub landing: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    /// A page of this platform, such as `/tokens` or `/p/rbac/people`.
    pub href: String,
    /// Shown instead of the page's own name; needed for a page nothing else names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// The menu it is listed under, rather than on the bar itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

fn named(text: Option<String>, what: &str) -> Result<Option<String>, String> {
    let Some(text) = text.map(|text| text.trim().to_string()).filter(|text| !text.is_empty())
    else {
        return Ok(None);
    };
    if text.chars().count() > MAX_LABEL || text.chars().any(char::is_control) {
        return Err(format!("a {what} is at most {MAX_LABEL} characters, on one line"));
    }
    Ok(Some(text))
}

/// Only this platform's own pages: a link elsewhere would look like part of it.
fn page(href: &str) -> Result<String, String> {
    let href = href.trim();
    let own = href.starts_with('/') && !href.starts_with("//") && !href.contains('\\');
    if !own || href.len() > MAX_HREF || href.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!(
            "{href:?} is not a page of this platform: links start with a single /, such as /p/rbac/people"
        ));
    }
    Ok(href.to_string())
}

impl Layout {
    /// Tidies what was sent, or says what is wrong with it.
    pub fn checked(self) -> Result<Self, String> {
        if self.entries.len() > MAX_ENTRIES {
            return Err(format!("the navigation holds at most {MAX_ENTRIES} entries"));
        }
        let mut seen = std::collections::BTreeSet::new();
        let mut entries = Vec::with_capacity(self.entries.len());
        for entry in self.entries {
            let href = page(&entry.href)?;
            if !seen.insert(href.clone()) {
                return Err(format!("{href} is listed twice"));
            }
            entries.push(Entry {
                href,
                label: named(entry.label, "label")?,
                group: named(entry.group, "group name")?,
                hidden: entry.hidden,
            });
        }
        if self.landing.len() > MAX_LANDING {
            return Err(format!("the landing page holds at most {MAX_LANDING} cards"));
        }
        let mut chosen = std::collections::BTreeSet::new();
        let mut landing = Vec::with_capacity(self.landing.len());
        for href in self.landing {
            let href = page(&href)?;
            if !chosen.insert(href.clone()) {
                return Err(format!("{href} is on the landing page twice"));
            }
            landing.push(href);
        }
        Ok(Self { entries, landing })
    }
}

/// The saved layout, or nothing when it has never been set or cannot be read.
pub async fn saved(state: &AppState) -> Option<Layout> {
    match state.repos.plugins.navigation().await {
        Ok(layout) => layout.and_then(|layout| serde_json::from_value(layout).ok()),
        Err(err) => {
            tracing::warn!(%err, "the navigation is shown as the pages offer it");
            None
        }
    }
}

pub async fn show(State(state): State<AppState>, auth: Auth) -> Result<Json<Layout>, Problem> {
    if !permissions::holds(&state, &auth.0, CORE, Access::Read).await {
        return Err(Problem::forbidden("needs plugin:core:user:ro"));
    }
    Ok(Json(saved(&state).await.unwrap_or_default()))
}

pub async fn set(
    State(state): State<AppState>,
    auth: Auth,
    Json(layout): Json<Layout>,
) -> Result<Json<Layout>, Problem> {
    if !permissions::holds(&state, &auth.0, CORE, Access::Write).await {
        return Err(Problem::forbidden("needs plugin:core:user:rw"));
    }
    let layout = layout.checked().map_err(Problem::bad_request)?;
    let value = serde_json::to_value(&layout).map_err(|err| Problem::internal(err.to_string()))?;
    state.repos.plugins.set_navigation(&value).await.map_err(|err| {
        tracing::error!(%err, "the navigation could not be saved");
        Problem::internal("the navigation could not be saved")
    })?;
    let _ =
        state
            .repos
            .identity
            .record_audit(AuditEntry::new("navigation.changed").by(&auth.0).detail(
                json!({ "entries": layout.entries.len(), "landing": layout.landing.len() }),
            ))
            .await;
    announce(&state).await;
    Ok(Json(layout))
}

async fn announce(state: &AppState) {
    let Ok(topic) = doc_eventbus::Topic::new(CHANGED) else { return };
    let event = doc_eventbus::Event::new(topic, crate::fabric::SOURCE, Value::Null);
    if let Err(err) = state.buses.events.publish(event).await {
        tracing::warn!(%err, "frontends will notice the new navigation as their caches expire");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::memory::{FakeHealth, FakeIdentity};
    use crate::fabric::Buses;
    use crate::identity::TokenOwner;
    use crate::secrets::TokenKind;
    use crate::testing::{get_as, put_json, repositories};
    use http::StatusCode;
    use std::sync::Arc;

    const ADMIN: &str = "doc_ses_admin";
    const PLAIN: &str = "doc_ses_plain";

    fn app() -> (axum::Router, Arc<FakeIdentity>) {
        let mut config = Config::default();
        config.bootstrap.admins = vec!["root".into()];
        let identity = FakeIdentity::empty();
        let root = identity.add_user("root");
        identity.give(ADMIN, TokenKind::Session, TokenOwner::User(root.id), None, false);
        let ada = identity.add_user("ada");
        identity.give(PLAIN, TokenKind::Session, TokenOwner::User(ada.id), None, false);
        let repos = repositories(FakeHealth::up(), identity.clone());
        (super::super::router(AppState::new(config, repos, Buses::in_memory())), identity)
    }

    #[tokio::test]
    async fn an_administrator_arranges_the_navigation_and_everyone_is_sent_it() {
        let (app, identity) = app();
        let layout = json!({ "entries": [
            { "href": "/p/rbac/people", "label": "Users", "group": "Access" },
            { "href": "/tokens", "group": "Access" },
            { "href": "/status", "hidden": true },
        ]});

        let (status, _, _) = put_json(&app, "/api/v1/navigation", PLAIN, layout.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "only core writers arrange the navigation");

        let (status, saved, _) = put_json(&app, "/api/v1/navigation", ADMIN, layout).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["entries"][0]["label"], "Users");
        assert!(identity.audit_actions().contains(&"navigation.changed".to_string()));

        let (status, shown, _) = get_as(&app, "/api/v1/navigation", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(shown, saved);

        let (_, access, _) = get_as(&app, "/api/v1/me/access", PLAIN).await;
        assert_eq!(access["navigation"], saved, "everyone's access carries the layout");
    }

    #[tokio::test]
    async fn a_layout_that_links_elsewhere_is_refused() {
        let (app, _) = app();
        let layout = json!({ "entries": [{ "href": "https://example.com", "label": "Out" }] });
        let (status, body, _) = put_json(&app, "/api/v1/navigation", ADMIN, layout).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body["detail"].as_str().unwrap_or_default().contains("not a page of this platform")
        );
        let (_, access, _) = get_as(&app, "/api/v1/me/access", ADMIN).await;
        assert!(access["navigation"].is_null(), "nothing was saved");
    }

    #[tokio::test]
    async fn an_administrator_chooses_the_landing_page_cards() {
        let (app, _) = app();
        let layout = json!({
            "entries": [{ "href": "/p/kb/", "group": "Workspace" }],
            "landing": ["/p/kb/", "/p/calendar/", "/p/resources/"],
        });
        let (status, saved, _) = put_json(&app, "/api/v1/navigation", ADMIN, layout).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["landing"][0], "/p/kb/");
        assert_eq!(saved["landing"].as_array().map(Vec::len), Some(3));

        let (_, access, _) = get_as(&app, "/api/v1/me/access", PLAIN).await;
        assert_eq!(access["navigation"]["landing"][2], "/p/resources/", "everyone is sent it");

        let twice = json!({ "landing": ["/p/kb/", "/p/kb/"] });
        let (status, body, _) = put_json(&app, "/api/v1/navigation", ADMIN, twice).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["detail"].as_str().unwrap_or_default().contains("twice"));

        let elsewhere = json!({ "landing": ["https://example.com"] });
        let (status, _, _) = put_json(&app, "/api/v1/navigation", ADMIN, elsewhere).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "a card links to a page of this platform");

        let many: Vec<String> = (0..MAX_LANDING + 1).map(|at| format!("/p/kb/{at}")).collect();
        let (status, _, _) =
            put_json(&app, "/api/v1/navigation", ADMIN, json!({ "landing": many })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    fn entry(href: &str) -> Entry {
        Entry { href: href.into(), label: None, group: None, hidden: false }
    }

    #[test]
    fn labels_and_groups_are_trimmed_and_blank_ones_dropped() {
        let layout = Layout {
            entries: vec![Entry {
                href: " /tokens ".into(),
                label: Some("  Tokens ".into()),
                group: Some("   ".into()),
                hidden: false,
            }],
            ..Layout::default()
        };
        let checked = layout.checked().unwrap();
        assert_eq!(checked.entries[0].href, "/tokens");
        assert_eq!(checked.entries[0].label.as_deref(), Some("Tokens"));
        assert_eq!(checked.entries[0].group, None);
    }

    #[test]
    fn only_this_platforms_pages_can_be_linked() {
        for href in ["https://example.com", "//example.com/x", "tokens", "/a b", "/\\evil"] {
            let layout = Layout { entries: vec![entry(href)], ..Layout::default() };
            assert!(layout.checked().is_err(), "{href} should be refused");
        }
        let fine = Layout { entries: vec![entry("/p/rbac/people?x=1")], ..Layout::default() };
        assert!(fine.checked().is_ok());
    }

    #[test]
    fn a_page_is_listed_once_and_names_are_short() {
        let twice =
            Layout { entries: vec![entry("/tokens"), entry("/tokens")], ..Layout::default() };
        assert!(twice.checked().unwrap_err().contains("twice"));
        let long = Layout {
            entries: vec![Entry { label: Some("x".repeat(MAX_LABEL + 1)), ..entry("/tokens") }],
            ..Layout::default()
        };
        assert!(long.checked().is_err());
    }
}
