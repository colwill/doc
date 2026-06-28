//! The audit log, newest first, for anyone who can read the RBAC plugin or the platform.

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Utc};
use doc_permissions::{Access, CORE};
use serde::Deserialize;
use serde_json::{Value, json};

use super::AppState;
use super::problem::Problem;
use crate::auth::Auth;
use crate::db::repositories::AuditFilter;
use crate::permissions;

const RBAC: &str = "rbac";
const MAX_LIMIT: u32 = 500;

#[derive(Debug, Deserialize)]
pub struct AuditQuery {
    #[serde(default)]
    pub action: Option<String>,
    #[serde(default)]
    pub actor: Option<String>,
    #[serde(default)]
    pub before: Option<DateTime<Utc>>,
    #[serde(default)]
    pub limit: Option<u32>,
}

pub async fn list(
    State(state): State<AppState>,
    auth: Auth,
    Query(query): Query<AuditQuery>,
) -> Result<Json<Value>, Problem> {
    let reads = permissions::holds(&state, &auth.0, RBAC, Access::Read).await
        || permissions::holds(&state, &auth.0, CORE, Access::Read).await;
    if !reads {
        return Err(Problem::forbidden("needs plugin:rbac:user:ro or plugin:core:user:ro"));
    }
    let filter = AuditFilter {
        action: query.action.filter(|action| !action.is_empty()),
        actor: query.actor.filter(|actor| !actor.is_empty()),
        before: query.before,
        limit: query.limit.unwrap_or(100).clamp(1, MAX_LIMIT),
    };
    let entries = state.repos.identity.audit_log(&filter).await?;
    Ok(Json(json!({ "entries": entries })))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::config::Config;
    use crate::db::memory::{FakeHealth, FakeIdentity};
    use crate::db::repositories::IdentityRepository;
    use crate::identity::{AuditEntry, TokenOwner};
    use crate::secrets::TokenKind;
    use crate::testing::{get_as, repositories};

    #[tokio::test]
    async fn the_audit_log_is_read_by_platform_and_rbac_readers_newest_first() {
        let mut config = Config::default();
        config.bootstrap.admins = vec!["root".into()];
        let identity = FakeIdentity::empty();
        let root = identity.add_user("root");
        identity.give("doc_ses_root", TokenKind::Session, TokenOwner::User(root.id), None, false);
        let ada = identity.add_user("ada");
        identity.give("doc_ses_ada", TokenKind::Session, TokenOwner::User(ada.id), None, false);
        for action in
            ["auth.sign-in", "plugin.rbac.group.created", "plugin.rbac.assignment.granted"]
        {
            identity
                .record_audit(AuditEntry::new(action).by_login("root"))
                .await
                .expect("recorded");
        }
        let repos = repositories(FakeHealth::up(), identity);
        let state = crate::api::AppState::new(config, repos, crate::fabric::Buses::in_memory());
        let app = crate::api::router(state);

        let (status, body, _) = get_as(&app, "/api/v1/audit", "doc_ses_root").await;
        assert_eq!(status, http::StatusCode::OK);
        assert_eq!(body["entries"][0]["action"], "plugin.rbac.assignment.granted");
        let (_, body, _) =
            get_as(&app, "/api/v1/audit?action=plugin.rbac.&limit=1", "doc_ses_root").await;
        assert_eq!(body["entries"], json!([body["entries"][0].clone()]));
        assert_eq!(body["entries"][0]["action"], "plugin.rbac.assignment.granted");
        let (status, _, _) = get_as(&app, "/api/v1/audit", "doc_ses_ada").await;
        assert_eq!(status, http::StatusCode::FORBIDDEN, "ada can read neither rbac nor core");
    }
}
