//! What an administrator set for the whole platform: `GET /api/v1/settings` for anyone signed in,
//! and `PUT` for platform administrators. One record, as the navigation has, so every frontend
//! shows the same thing; what is not set here falls back to each service's configuration.

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

/// Announced whenever the settings change, so every frontend drops what it cached.
pub const CHANGED: &str = "platform.settings.changed";
const MAX_INSTANCE: usize = 16;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Shown before the logo, so `DEV` reads as DEV[DOC]. Empty shows the logo on its own, and
    /// leaves each frontend its configured name.
    pub instance_name: String,
}

impl Settings {
    fn checked(self) -> Result<Self, String> {
        let name = self.instance_name.trim().to_string();
        let fine = name.chars().count() <= MAX_INSTANCE
            && !name.chars().any(|c| c.is_control() || c == '<' || c == '>');
        match fine {
            true => Ok(Self { instance_name: name }),
            false => Err(format!("a name is at most {MAX_INSTANCE} characters, on one line")),
        }
    }
}

pub async fn saved(state: &AppState) -> Option<Settings> {
    let stored = state.repos.plugins.settings().await;
    match stored {
        Ok(Some(value)) => serde_json::from_value(value).ok(),
        Ok(None) => None,
        Err(err) => {
            tracing::warn!(%err, "the platform's settings could not be read");
            None
        }
    }
}

/// Anyone signed in reads them: they say how the platform names itself, which every page shows.
pub async fn show(State(state): State<AppState>, _: Auth) -> Json<Settings> {
    Json(saved(&state).await.unwrap_or_default())
}

pub async fn set(
    State(state): State<AppState>,
    auth: Auth,
    Json(settings): Json<Settings>,
) -> Result<Json<Settings>, Problem> {
    if !permissions::holds(&state, &auth.0, CORE, Access::Write).await {
        return Err(Problem::forbidden("needs plugin:core:user:rw"));
    }
    let settings = settings.checked().map_err(Problem::bad_request)?;
    let value =
        serde_json::to_value(&settings).map_err(|err| Problem::internal(err.to_string()))?;
    state.repos.plugins.set_settings(&value).await.map_err(|err| {
        tracing::error!(%err, "the platform's settings could not be saved");
        Problem::internal("the settings could not be saved")
    })?;
    let entry = AuditEntry::new("settings.changed")
        .by(&auth.0)
        .detail(json!({ "instance_name": settings.instance_name }));
    let _ = state.repos.identity.record_audit(entry).await;
    announce(&state).await;
    Ok(Json(settings))
}

async fn announce(state: &AppState) {
    let Ok(topic) = doc_eventbus::Topic::new(CHANGED) else { return };
    let event = doc_eventbus::Event::new(topic, crate::fabric::SOURCE, Value::Null);
    if let Err(err) = state.buses.events.publish(event).await {
        tracing::warn!(%err, "frontends will notice the new settings as their caches expire");
    }
}

#[cfg(test)]
mod tests {
    use http::StatusCode;
    use serde_json::json;

    use crate::testing::{ADMIN, get_as, plugin_host, put_json};

    #[tokio::test]
    async fn administrators_name_the_platform_and_everyone_signed_in_reads_it() {
        let host = plugin_host();
        let (status, body, _) = get_as(&host.app, "/api/v1/settings", ADMIN).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["instance_name"], "", "nothing is set to begin with");

        let (status, body, _) =
            put_json(&host.app, "/api/v1/settings", ADMIN, json!({ "instance_name": " DEV " }))
                .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["instance_name"], "DEV", "it is taken as typed, without the spaces");
        let (_, body, _) = get_as(&host.app, "/api/v1/settings", ADMIN).await;
        assert_eq!(body["instance_name"], "DEV");

        let (status, body, _) = put_json(
            &host.app,
            "/api/v1/settings",
            ADMIN,
            json!({ "instance_name": "much too long a name for the plate" }),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        // It travels with the access summary, so every page can name the platform.
        let (status, body, _) = get_as(&host.app, "/api/v1/me/access", ADMIN).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["settings"]["instance_name"], "DEV");

        assert!(host.identity.audit_actions().contains(&"settings.changed".to_string()));
    }
}
