//! The first setup, `/api/v1/setup`: how far administrators have got with the frontend's guided
//! first run — when it was opened, the tools they use, the steps done, and when it was finished —
//! kept in core so every frontend offers it alike. Each step's work goes through its own API.

use axum::Json;
use axum::extract::State;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::AppState;
use super::problem::Problem;
use crate::auth::Auth;
use crate::identity::AuditEntry;
use crate::permissions;

/// At most this many steps, and this many tools, each named in at most `MAX_NAME` characters.
const MAX_NAMES: usize = 24;
const MAX_NAME: usize = 32;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Setup {
    /// When an administrator first opened it. Until then it opens by itself after a first sign-in.
    pub started_at: Option<DateTime<Utc>>,
    /// The tools the organisation uses, such as `github` or `jira`, which shape what it suggests.
    pub tools: Vec<String>,
    /// The steps marked done, by name, in the order they were.
    pub done: Vec<String>,
    /// When it was finished or put aside, and by whom; it is then offered to nobody.
    pub finished_at: Option<DateTime<Utc>>,
    pub finished_by: Option<String>,
}

/// What a call changes. Any change at all means it has been opened.
#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Change {
    pub started: bool,
    /// A step to mark done.
    pub step: Option<String>,
    /// The tools it uses, replacing those given before.
    pub tools: Option<Vec<String>>,
    /// `true` finishes it, `false` offers it again.
    pub finished: Option<bool>,
}

fn name(name: &str) -> Result<String, Problem> {
    let fine = !name.is_empty()
        && name.len() <= MAX_NAME
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    match fine {
        true => Ok(name.to_string()),
        false => Err(Problem::bad_request(format!(
            "a step or a tool is named in lowercase letters, digits and dashes, at most \
             {MAX_NAME} of them"
        ))),
    }
}

async fn saved(state: &AppState) -> Result<Setup, Problem> {
    let stored = state.repos.plugins.setup().await?;
    Ok(stored.and_then(|value| serde_json::from_value(value).ok()).unwrap_or_default())
}

async fn administrator(state: &AppState, auth: &Auth) -> Result<(), Problem> {
    match permissions::is_admin_of(state, &auth.0).await {
        true => Ok(()),
        false => Err(Problem::forbidden("only platform administrators set DOC up")),
    }
}

pub async fn show(State(state): State<AppState>, auth: Auth) -> Result<Json<Setup>, Problem> {
    administrator(&state, &auth).await?;
    Ok(Json(saved(&state).await?))
}

pub async fn change(
    State(state): State<AppState>,
    auth: Auth,
    Json(change): Json<Change>,
) -> Result<Json<Setup>, Problem> {
    administrator(&state, &auth).await?;
    let step = change.step.as_deref().map(name).transpose()?;
    let mut setup = saved(&state).await?;
    let now = Utc::now();
    setup.started_at.get_or_insert(now);
    if let Some(step) = step
        && !setup.done.contains(&step)
    {
        if setup.done.len() >= MAX_NAMES {
            return Err(Problem::bad_request(format!("at most {MAX_NAMES} steps are kept")));
        }
        setup.done.push(step);
    }
    if let Some(tools) = change.tools {
        if tools.len() > MAX_NAMES {
            return Err(Problem::bad_request(format!("at most {MAX_NAMES} tools are kept")));
        }
        let mut named = Vec::with_capacity(tools.len());
        for tool in &tools {
            let tool = name(tool)?;
            if !named.contains(&tool) {
                named.push(tool);
            }
        }
        setup.tools = named;
    }
    let action = match change.finished {
        Some(true) if setup.finished_at.is_none() => {
            setup.finished_at = Some(now);
            setup.finished_by = Some(auth.0.label());
            Some("setup.finished")
        }
        Some(false) if setup.finished_at.is_some() => {
            setup.finished_at = None;
            setup.finished_by = None;
            Some("setup.reopened")
        }
        _ => None,
    };
    let value = serde_json::to_value(&setup).map_err(|err| Problem::internal(err.to_string()))?;
    state.repos.plugins.set_setup(&value).await.map_err(|err| {
        tracing::error!(%err, "how far the setup has got could not be saved");
        Problem::internal("the setup could not be saved")
    })?;
    if let Some(action) = action {
        let entry = AuditEntry::new(action).by(&auth.0).detail(json!({ "done": setup.done }));
        let _ = state.repos.identity.record_audit(entry).await;
    }
    Ok(Json(setup))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use http::StatusCode;
    use serde_json::json;

    use crate::api::AppState;
    use crate::config::Config;
    use crate::db::memory::{FakeHealth, FakeIdentity};
    use crate::fabric::Buses;
    use crate::identity::TokenOwner;
    use crate::secrets::TokenKind;
    use crate::testing::{get_as, patch_json, repositories};

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
        (crate::api::router(AppState::new(config, repos, Buses::in_memory())), identity)
    }

    #[tokio::test]
    async fn administrators_work_through_the_setup_and_finish_it() {
        let (app, identity) = app();
        let (status, body, _) = get_as(&app, "/api/v1/setup", ADMIN).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["started_at"], json!(null), "nobody has opened it yet");
        assert_eq!(body["done"], json!([]));

        let (status, _, _) = get_as(&app, "/api/v1/setup", PLAIN).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "it is only for administrators");
        let (status, _, _) =
            patch_json(&app, "/api/v1/setup", PLAIN, json!({ "step": "name" })).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, body, _) =
            patch_json(&app, "/api/v1/setup", ADMIN, json!({ "started": true })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let started = body["started_at"].clone();
        assert!(started.is_string(), "opening it is remembered: {body}");

        let tools = json!({ "tools": ["github", "jira", "github"] });
        let (status, body, _) = patch_json(&app, "/api/v1/setup", ADMIN, tools).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["tools"], json!(["github", "jira"]), "each tool is kept once");
        let (_, body, _) = patch_json(&app, "/api/v1/setup", ADMIN, json!({ "tools": [] })).await;
        assert_eq!(body["tools"], json!([]), "the tools given replace those given before");

        for step in ["name", "plugins", "name"] {
            let (status, body, _) =
                patch_json(&app, "/api/v1/setup", ADMIN, json!({ "step": step })).await;
            assert_eq!(status, StatusCode::OK, "{body}");
        }
        let (_, body, _) = get_as(&app, "/api/v1/setup", ADMIN).await;
        assert_eq!(body["done"], json!(["name", "plugins"]), "a step done twice is kept once");
        assert_eq!(body["started_at"], started, "it was opened once, when it was first opened");

        let (status, _, _) =
            patch_json(&app, "/api/v1/setup", ADMIN, json!({ "step": "Not a step!" })).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let (status, body, _) =
            patch_json(&app, "/api/v1/setup", ADMIN, json!({ "finished": true })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["finished_at"].is_string());
        assert_eq!(body["finished_by"], "root");
        assert!(identity.audit_actions().contains(&"setup.finished".to_string()));

        let (_, body, _) =
            patch_json(&app, "/api/v1/setup", ADMIN, json!({ "finished": false })).await;
        assert_eq!(body["finished_at"], json!(null), "it can be offered again");
        assert_eq!(body["done"], json!(["name", "plugins"]), "keeping what was done");
        assert!(identity.audit_actions().contains(&"setup.reopened".to_string()));
    }
}
