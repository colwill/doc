//! A person's dashboard, the page they land on when they sign in: the items they chose to see, in
//! order, from what the plugins they can read offer and the platform's own. Somebody who has not
//! chosen sees the starter set.

use std::collections::BTreeSet;

use axum::Json;
use axum::extract::State;
use doc_permissions::Access;
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::plugin_settings::admitted;
use super::problem::Problem;
use crate::auth::Auth;
use crate::identity::{Principal, User};

/// What somebody who has not chosen sees: what is waiting for them, and what is coming up.
pub const STARTER: [&str; 4] =
    ["notifications/inbox", "calendar/upcoming", "water/tagged", "core/access-requests"];
/// The platform's own item: other plugins asking to join a list in a plugin's settings, for
/// whoever may change that plugin's settings.
pub const ACCESS_REQUESTS: &str = "core/access-requests";
/// The most items one dashboard holds.
const MOST: usize = 24;

#[derive(Debug, Deserialize)]
pub struct Chosen {
    pub items: Vec<String>,
}

fn person(principal: &Principal) -> Result<&User, Problem> {
    principal.as_user().ok_or_else(|| Problem::forbidden("only a person has a dashboard"))
}

/// The plugins whose settings `principal` may change, which is whose access requests they decide.
async fn deciding(state: &AppState, principal: &Principal) -> Result<Vec<String>, Problem> {
    let mut decides = Vec::new();
    for entry in state.plugins.list().await {
        if admitted(state, principal, &entry.id, Access::Write).await? {
            decides.push(entry.id);
        }
    }
    Ok(decides)
}

/// Every item `principal` may put on their dashboard now, as `plugin/id`.
async fn offered(state: &AppState, principal: &Principal) -> Result<BTreeSet<String>, Problem> {
    let access = super::tasks::access_of(state, principal).await?;
    let mut offered = BTreeSet::new();
    for (plugin, held) in access["plugins"].as_object().into_iter().flatten() {
        for item in held["dashboard"].as_array().into_iter().flatten() {
            if let Some(id) = item["id"].as_str() {
                offered.insert(format!("{plugin}/{id}"));
            }
        }
    }
    if !deciding(state, principal).await?.is_empty() {
        offered.insert(ACCESS_REQUESTS.to_string());
    }
    Ok(offered)
}

/// `GET /api/v1/me/dashboard`: what the caller chose, or the starter set, and whether they chose.
pub async fn show(State(state): State<AppState>, auth: Auth) -> Result<Json<Value>, Problem> {
    let me = person(&auth.0)?;
    let chosen = state.repos.identity.dashboard(me.id).await?;
    let items = match &chosen {
        Some(items) => items.clone(),
        None => STARTER.iter().map(|item| item.to_string()).collect(),
    };
    Ok(Json(json!({ "items": items, "chosen": chosen.is_some() })))
}

/// `PUT /api/v1/me/dashboard`: the items to show, in order. Each is one offered to the caller now,
/// or one they had already, so a plugin restarting does not lose its place.
pub async fn set(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<Chosen>,
) -> Result<Json<Value>, Problem> {
    let me = person(&auth.0)?;
    if body.items.len() > MOST {
        return Err(Problem::bad_request(format!("a dashboard holds at most {MOST} items")));
    }
    let held: BTreeSet<String> =
        state.repos.identity.dashboard(me.id).await?.unwrap_or_default().into_iter().collect();
    let offered = offered(&state, &auth.0).await?;
    let mut items: Vec<String> = Vec::new();
    for item in &body.items {
        let item = item.trim().to_string();
        if items.contains(&item) {
            continue;
        }
        if !offered.contains(&item) && !held.contains(&item) {
            return Err(Problem::bad_request(format!(
                "{item} is not something offered to you for your dashboard"
            )));
        }
        items.push(item);
    }
    state.repos.identity.set_dashboard(me.id, &items).await?;
    Ok(Json(json!({ "items": items, "chosen": true })))
}

/// `GET /api/v1/me/access-requests`: what other plugins are waiting for the caller to decide, in
/// the settings of every plugin they may change, newest first.
pub async fn waiting(State(state): State<AppState>, auth: Auth) -> Result<Json<Value>, Problem> {
    person(&auth.0)?;
    let mut waiting = Vec::new();
    for plugin in deciding(&state, &auth.0).await? {
        let Some(entry) = state.plugins.get(&plugin).await else { continue };
        for request in crate::plugins::access::listed(&state, &plugin, &entry.manifest).await {
            if request["state"] == "pending" {
                let mut request = request;
                request["target"] = json!(plugin);
                waiting.push(request);
            }
        }
    }
    waiting.sort_by(|a, b| b["created_at"].as_str().cmp(&a["created_at"].as_str()));
    Ok(Json(json!({ "requests": waiting })))
}

#[cfg(test)]
mod tests {
    use doc_plugin_protocol::{DashboardItem, Manifest, RegisterRequest};
    use http::StatusCode;
    use serde_json::json;

    use crate::identity::Principal;
    use crate::testing::{ADMIN, get_as, plugin_host, put_json};

    #[tokio::test]
    async fn a_person_chooses_only_from_what_they_are_offered_and_keeps_it() {
        let host = plugin_host();
        let manifest = Manifest {
            id: "hello".into(),
            version: "1.0.0".into(),
            dashboard: vec![DashboardItem::new("greetings", "Your greetings", "/dashboard")],
            ..Manifest::default()
        };
        let request = RegisterRequest {
            manifest,
            address: "plugin-hello:4440".into(),
            binary_sha256: "a".repeat(64),
            started_at: None,
        };
        let hello = Principal::Plugin { id: "hello".into() };
        crate::plugins::register(&host.state, &hello, request).await.expect("registered");
        host.settle("hello").await;
        let ada = host.user_holding("ada", &["plugin:hello:user:ro"]).await;
        let eve = host.user_holding("eve", &[]).await;

        let (status, shown, _) = get_as(&host.app, "/api/v1/me/dashboard", &ada).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            (shown["chosen"].as_bool(), shown["items"][0].as_str()),
            (Some(false), Some("notifications/inbox"))
        );

        let path = "/api/v1/me/dashboard";
        let (status, _, _) =
            put_json(&host.app, path, &ada, json!({ "items": ["kb/nothing"] })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "nothing offers it");
        let (status, _, _) =
            put_json(&host.app, path, &ada, json!({ "items": ["core/access-requests"] })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "ada decides no access requests");
        let twice = json!({ "items": ["hello/greetings", "hello/greetings"] });
        let (status, saved, _) = put_json(&host.app, path, &ada, twice).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(saved["items"], json!(["hello/greetings"]), "once each");
        let (_, shown, _) = get_as(&host.app, path, &ada).await;
        assert_eq!(
            (shown["chosen"].as_bool(), &shown["items"]),
            (Some(true), &json!(["hello/greetings"]))
        );

        let (status, _, _) =
            put_json(&host.app, path, &eve, json!({ "items": ["hello/greetings"] })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "eve cannot read hello");
        let both = json!({ "items": ["core/access-requests", "hello/greetings"] });
        let (status, _, _) = put_json(&host.app, path, ADMIN, both).await;
        assert_eq!(status, StatusCode::OK, "an administrator decides every plugin's requests");
    }
}
