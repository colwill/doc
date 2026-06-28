//! Service accounts and the accounts that own them. Any user may create one, for themselves or for a
//! team they are in; its tokens are shown once, like personal access tokens. A team's account is
//! managed by its members and the members of the teams below it (ADR-0004). Holding
//! `plugin:rbac:user:rw` manages every account and disables users, so these routes check that
//! themselves rather than being guarded on `core`.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use chrono::{Duration, Utc};
use doc_eventbus::{Event, Topic};
use doc_permissions::Access;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::auth::TokenView;
use super::problem::Problem;
use crate::auth::{Auth, forget, forget_all};
use crate::db::repositories::NewToken;
use crate::identity::{AuditEntry, Principal, ServiceAccount, TokenOwner, User, token_hash};
use crate::permissions;
use crate::secrets::{TokenKind, generate_token};
use crate::teams::{AccountOwner, Owners};

const RBAC: &str = "rbac";
const MAX_TOKEN_DAYS: i64 = 365;
const MAX_NAME: usize = 64;

#[derive(Debug, Deserialize)]
pub struct NewAccount {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// A team the creator is in, to own the account instead of them.
    #[serde(default)]
    pub team: Option<Uuid>,
}

/// One user or one team to own an account.
#[derive(Debug, Deserialize)]
pub struct NewOwner {
    #[serde(default)]
    pub user: Option<Uuid>,
    #[serde(default)]
    pub team: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct NewAccountToken {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub struct SetDisabled {
    pub disabled: bool,
}

#[derive(Debug, Deserialize)]
pub struct NewGrant {
    pub permission: String,
}

#[derive(Debug, Serialize)]
pub struct AccountView {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub owner_id: Option<Uuid>,
    pub owner_team_id: Option<Uuid>,
    pub disabled: bool,
    pub created_at: chrono::DateTime<Utc>,
}

impl From<ServiceAccount> for AccountView {
    fn from(account: ServiceAccount) -> Self {
        Self {
            id: account.id,
            name: account.name,
            description: account.description,
            owner_id: account.owner_id,
            owner_team_id: account.owner_team_id,
            disabled: account.disabled,
            created_at: account.created_at,
        }
    }
}

/// Only a user can own an account, so only a user can create one.
fn owner(principal: &Principal) -> Result<&User, Problem> {
    principal.as_user().ok_or_else(|| Problem::forbidden("only a user can own a service account"))
}

pub(crate) async fn manages_identity(state: &AppState, principal: &Principal) -> bool {
    permissions::holds(state, principal, RBAC, Access::Write).await
}

/// The teams whose service accounts a user manages: those they are in, and every team above those.
pub(crate) async fn reach_of(state: &AppState, user: Uuid) -> Result<Vec<Uuid>, Problem> {
    let memberships = state.repos.teams.memberships(user).await?;
    if memberships.is_empty() {
        return Ok(Vec::new());
    }
    let teams = state.repos.teams.teams().await?;
    Ok(crate::teams::reach(&teams, &memberships))
}

/// The owner, a member of the owning team or a team below it, or an identity administrator. Anyone
/// else is refused rather than told nothing exists, since the account's ID was already known to
/// whoever asked.
async fn may_manage(
    state: &AppState,
    principal: &Principal,
    account: &ServiceAccount,
) -> Result<(), Problem> {
    if let Some(user) = principal.as_user() {
        if account.owner_id == Some(user.id) {
            return Ok(());
        }
        if let Some(team) = account.owner_team_id
            && reach_of(state, user.id).await?.contains(&team)
        {
            return Ok(());
        }
    }
    if manages_identity(state, principal).await {
        return Ok(());
    }
    Err(Problem::forbidden(
        "needs plugin:rbac:user:rw, ownership of this service account, or membership of the team \
         that owns it",
    ))
}

/// A team that a user may give an account to: one they are in or below, unless they administer
/// identity, who may give one to any team.
async fn receiving_team(
    state: &AppState,
    principal: &Principal,
    team: Uuid,
) -> Result<(), Problem> {
    state.repos.teams.team(team).await?.ok_or_else(|| Problem::not_found("team"))?;
    let reached = match principal.as_user() {
        Some(user) => reach_of(state, user.id).await?,
        None => Vec::new(),
    };
    if reached.contains(&team) || manages_identity(state, principal).await {
        return Ok(());
    }
    Err(Problem::forbidden("only a member of a team gives it a service account"))
}

async fn account(state: &AppState, id: Uuid) -> Result<ServiceAccount, Problem> {
    state
        .repos
        .identity
        .service_account_by_id(id)
        .await?
        .ok_or_else(|| Problem::not_found("service account"))
}

/// Published for the RBAC plugin and the frontend; a bus that is down fails nothing that succeeded.
pub(crate) async fn announce(state: &AppState, event: &str, detail: Value) {
    let name = format!("platform.iam.{event}");
    let Ok(topic) = Topic::new(name.clone()) else { return };
    match state.buses.events.publish(Event::new(topic, crate::fabric::SOURCE, detail)).await {
        Ok(ack) => tracing::debug!(id = %ack.id, topic = %name, "announced an identity change"),
        Err(err) => tracing::warn!(%err, topic = %name, "could not announce an identity change"),
    }
}

pub async fn create_account(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<NewAccount>,
) -> Result<impl IntoResponse, Problem> {
    let user = owner(&auth.0)?;
    let name = body.name.trim();
    if name.is_empty() || name.len() > MAX_NAME {
        return Err(Problem::bad_request(format!("a name is 1 to {MAX_NAME} characters")));
    }
    if state.repos.identity.service_account_by_name(name).await?.is_some() {
        return Err(Problem::conflict(format!("a service account called {name} already exists")));
    }
    let description = body.description.as_deref().map(str::trim).filter(|d| !d.is_empty());
    let owner = match body.team {
        Some(team) => {
            receiving_team(&state, &auth.0, team).await?;
            AccountOwner::Team(team)
        }
        None => AccountOwner::User(user.id),
    };
    let account = state.repos.identity.create_service_account(name, description, owner).await?;
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("iam.service-account.created")
                .by(&auth.0)
                .subject(account.id.to_string())
                .detail(json!({ "name": account.name })),
        )
        .await;
    announce(
        &state,
        "service-account.created",
        json!({
            "id": account.id, "name": account.name,
            "owner": account.owner_id, "team": account.owner_team_id,
        }),
    )
    .await;
    Ok((StatusCode::CREATED, Json(AccountView::from(account))))
}

pub async fn list_accounts(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<AccountView>>, Problem> {
    let scope = match manages_identity(&state, &auth.0).await {
        true => None,
        false => {
            let user = owner(&auth.0)?.id;
            Some(Owners { user, teams: reach_of(&state, user).await? })
        }
    };
    let accounts = state.repos.identity.list_service_accounts(scope.as_ref()).await?;
    Ok(Json(accounts.into_iter().map(AccountView::from).collect()))
}

pub async fn show_account(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<AccountView>, Problem> {
    let account = account(&state, id).await?;
    may_manage(&state, &auth.0, &account).await?;
    Ok(Json(AccountView::from(account)))
}

pub async fn set_account_disabled(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<SetDisabled>,
) -> Result<Json<AccountView>, Problem> {
    let account = account(&state, id).await?;
    may_manage(&state, &auth.0, &account).await?;
    let updated = state
        .repos
        .identity
        .set_service_account_disabled(id, body.disabled)
        .await?
        .ok_or_else(|| Problem::not_found("service account"))?;
    // Disabling cannot find the token hashes that authenticate the account, so every decision goes.
    forget_all(&state).await;
    let action = match body.disabled {
        true => "disabled",
        false => "enabled",
    };
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new(format!("iam.service-account.{action}"))
                .by(&auth.0)
                .subject(id.to_string()),
        )
        .await;
    announce(&state, &format!("service-account.{action}"), json!({ "id": id })).await;
    Ok(Json(AccountView::from(updated)))
}

/// Gives the account to one user or one team. Its managers can give it to themselves or to a team
/// they are in; only an identity administrator can give it to someone else.
pub async fn set_account_owner(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<NewOwner>,
) -> Result<Json<AccountView>, Problem> {
    let account = account(&state, id).await?;
    may_manage(&state, &auth.0, &account).await?;
    let owner = match (body.user, body.team) {
        (Some(user), None) => {
            let yourself = auth.0.as_user().is_some_and(|me| me.id == user);
            if !yourself && !manages_identity(&state, &auth.0).await {
                return Err(Problem::forbidden(
                    "only an identity administrator gives a service account to someone else",
                ));
            }
            state
                .repos
                .identity
                .user_by_id(user)
                .await?
                .ok_or_else(|| Problem::not_found("user"))?;
            AccountOwner::User(user)
        }
        (None, Some(team)) => {
            receiving_team(&state, &auth.0, team).await?;
            AccountOwner::Team(team)
        }
        _ => return Err(Problem::bad_request("give it to one user or one team")),
    };
    let updated = state
        .repos
        .identity
        .set_service_account_owner(id, owner)
        .await?
        .ok_or_else(|| Problem::not_found("service account"))?;
    let detail = json!({ "owner": updated.owner_id, "team": updated.owner_team_id });
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("iam.service-account.owner.changed")
                .by(&auth.0)
                .subject(id.to_string())
                .detail(detail.clone()),
        )
        .await;
    let mut event = detail;
    event["id"] = json!(id);
    announce(&state, "service-account.owner.changed", event).await;
    Ok(Json(AccountView::from(updated)))
}

pub async fn list_account_tokens(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<TokenView>>, Problem> {
    let account = account(&state, id).await?;
    may_manage(&state, &auth.0, &account).await?;
    let owner = TokenOwner::ServiceAccount(id);
    let tokens = state.repos.identity.list_tokens(&owner, TokenKind::Service).await?;
    Ok(Json(tokens.into_iter().map(TokenView::from).collect()))
}

pub async fn create_account_token(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<NewAccountToken>,
) -> Result<impl IntoResponse, Problem> {
    let account = account(&state, id).await?;
    may_manage(&state, &auth.0, &account).await?;
    if account.disabled {
        return Err(Problem::conflict("this service account is disabled"));
    }
    // A plugin's own account is acted as by that plugin alone, so nothing else may hold its access.
    if let Some(plugin) = state.repos.identity.service_account_plugin(id).await? {
        return Err(Problem::conflict(format!(
            "this is {plugin}'s own service account, which only {plugin} acts as, so it has no tokens"
        )));
    }
    let expires_at = match body.expires_in_days {
        Some(days) if !(1..=MAX_TOKEN_DAYS).contains(&days) => {
            return Err(Problem::bad_request(format!(
                "expires_in_days must be between 1 and {MAX_TOKEN_DAYS}"
            )));
        }
        Some(days) => Some(Utc::now() + Duration::days(days)),
        None => None,
    };
    let secret = generate_token(TokenKind::Service)
        .map_err(|err| Problem::internal(format!("issuing a token: {err}")))?;
    let token = state
        .repos
        .identity
        .issue_token(NewToken {
            kind: TokenKind::Service,
            token_hash: token_hash(secret.expose()),
            name: body.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(str::to_string),
            owner: TokenOwner::ServiceAccount(id),
            expires_at,
            scopes: None,
            issued_by: None,
        })
        .await?;
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("iam.service-account.token.created")
                .by(&auth.0)
                .subject(token.id.to_string())
                .detail(json!({ "service_account": id })),
        )
        .await;
    announce(&state, "service-account.token.created", json!({ "id": id, "token": token.id })).await;
    let view = TokenView::from(token);
    Ok((StatusCode::CREATED, Json(json!({ "token": secret.expose(), "created": view }))))
}

pub async fn revoke_account_token(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, token)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, Problem> {
    let account = account(&state, id).await?;
    may_manage(&state, &auth.0, &account).await?;
    let owner = TokenOwner::ServiceAccount(id);
    let Some(hash) = state.repos.identity.revoke_token(token, &owner).await? else {
        return Err(Problem::not_found("token"));
    };
    forget(&state, &hash).await;
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("iam.service-account.token.revoked")
                .by(&auth.0)
                .subject(token.to_string())
                .detail(json!({ "service_account": id })),
        )
        .await;
    announce(&state, "service-account.token.revoked", json!({ "id": id, "token": token })).await;
    Ok(StatusCode::NO_CONTENT)
}

/// The account's permissions, group memberships and attributes, as the RBAC plugin holds them.
pub async fn account_permissions(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, Problem> {
    let account = account(&state, id).await?;
    may_manage(&state, &auth.0, &account).await?;
    let holder = json!({ "holder": { "kind": "service", "id": id } });
    Ok(Json(rbac(&state, "principal", holder).await?))
}

/// Rule 6: the RBAC plugin holds the grant to what the owner's own access covers, from core's view.
pub async fn grant_account_permission(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<NewGrant>,
) -> Result<impl IntoResponse, Problem> {
    let account = account(&state, id).await?;
    may_manage(&state, &auth.0, &account).await?;
    let held = permissions::grants(&state, &auth.0).await;
    let unlimited =
        permissions::is_admin(&held, &auth.0) || manages_identity(&state, &auth.0).await;
    let request = json!({
        "account": id,
        "permission": body.permission,
        "by": auth.0.label(),
        "grants": held,
        "unlimited": unlimited,
    });
    let granted = rbac(&state, "self-service/grant", request).await?;
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("iam.service-account.permission.granted")
                .by(&auth.0)
                .subject(id.to_string())
                .detail(json!({ "permission": body.permission })),
        )
        .await;
    Ok((StatusCode::CREATED, Json(granted)))
}

pub async fn revoke_account_permission(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, permission)): Path<(Uuid, String)>,
) -> Result<StatusCode, Problem> {
    let account = account(&state, id).await?;
    may_manage(&state, &auth.0, &account).await?;
    let request = json!({ "account": id, "permission": permission, "by": auth.0.label() });
    rbac(&state, "self-service/revoke", request).await?;
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("iam.service-account.permission.revoked")
                .by(&auth.0)
                .subject(id.to_string())
                .detail(json!({ "permission": permission })),
        )
        .await;
    Ok(StatusCode::NO_CONTENT)
}

/// The RBAC plugin's answer, or the problem it refused with; a plugin that cannot answer is a `503`.
async fn rbac(state: &AppState, route: &str, payload: Value) -> Result<Value, Problem> {
    let answer = permissions::ask_rbac(state.buses.services.as_ref(), route, payload)
        .await
        .map_err(|err| Problem::unavailable(format!("the RBAC plugin did not answer: {err}")))?;
    let Some(refused) = answer.get("refused") else { return Ok(answer) };
    let detail = refused["detail"].as_str().unwrap_or("refused by the RBAC plugin").to_string();
    Err(match refused["status"].as_u64() {
        Some(400) => Problem::bad_request(detail),
        Some(403) => Problem::forbidden(detail),
        Some(404) => Problem::new(StatusCode::NOT_FOUND, "not-found", "Not found").detail(detail),
        Some(409) => Problem::conflict(detail),
        _ => Problem::unavailable(detail),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::memory::{FakeHealth, FakeIdentity};
    use crate::fabric::Buses;
    use crate::identity::User;
    use crate::permissions::PermissionSource;
    use crate::testing::{delete_as, get_as, patch_json, post_json};
    use async_trait::async_trait;
    use axum::Router;
    use doc_permissions::Grants;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    /// Keyed by the principal's label, so one harness can hold an administrator and an ordinary user.
    struct Source(BTreeMap<String, Vec<String>>);

    #[async_trait]
    impl PermissionSource for Source {
        async fn grants(&self, principal: &Principal, _teams: &[Uuid]) -> Result<Grants, String> {
            let held = self.0.get(&principal.label()).cloned().unwrap_or_default();
            serde_json::from_value(json!({ "permissions": held })).map_err(|err| err.to_string())
        }
    }

    struct Harness {
        app: Router,
        identity: Arc<FakeIdentity>,
    }

    impl Harness {
        /// Everyone here is an ordinary user; `admin` is the one holding `plugin:rbac:user:rw`.
        fn new(admins: &[&str]) -> Self {
            let identity = FakeIdentity::empty();
            let held = admins
                .iter()
                .map(|name| ((*name).to_string(), vec!["plugin:rbac:user:rw".to_string()]))
                .collect();
            let repos = crate::testing::repositories(FakeHealth::up(), identity.clone());
            let state = AppState::new(Config::default(), repos, Buses::in_memory())
                .with_permissions(Arc::new(Source(held)));
            Harness { app: super::super::router(state), identity }
        }

        fn user(&self, login: &str) -> (User, String) {
            let user = self.identity.add_user(login);
            let secret = format!("doc_ses_{login}");
            self.identity.give(&secret, TokenKind::Session, TokenOwner::User(user.id), None, false);
            (user, secret)
        }

        async fn create(&self, token: &str, name: &str) -> (StatusCode, Value) {
            let (status, body, _) = post_json(
                &self.app,
                "/api/v1/service-accounts",
                Some(token),
                json!({ "name": name }),
            )
            .await;
            (status, body)
        }
    }

    #[tokio::test]
    async fn any_user_can_create_a_service_account_and_owns_it() {
        let app = Harness::new(&[]);
        let (ada, token) = app.user("ada");
        let (status, body) = app.create(&token, "deployer").await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["name"], "deployer");
        assert_eq!(body["owner_id"], ada.id.to_string());
        assert_eq!(body["disabled"], false);
    }

    #[tokio::test]
    async fn a_duplicate_name_is_refused() {
        let app = Harness::new(&[]);
        let (_, token) = app.user("ada");
        app.create(&token, "deployer").await;
        let (status, _) = app.create(&token, "deployer").await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn a_service_account_cannot_create_service_accounts() {
        let app = Harness::new(&[]);
        let account = app.identity.add_service_account("bot");
        app.identity.give(
            "doc_svc_bot",
            TokenKind::Service,
            TokenOwner::ServiceAccount(account.id),
            None,
            false,
        );
        let (status, _) = app.create("doc_svc_bot", "another").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_non_owner_cannot_read_or_manage_the_account() {
        let app = Harness::new(&[]);
        let (_, ada) = app.user("ada");
        let (_, bob) = app.user("bob");
        let (_, created) = app.create(&ada, "deployer").await;
        let id = created["id"].as_str().expect("id");

        let (status, _, _) =
            get_as(&app.app, &format!("/api/v1/service-accounts/{id}"), &bob).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "reading someone else's account");

        let (status, _, _) = patch_json(
            &app.app,
            &format!("/api/v1/service-accounts/{id}"),
            &bob,
            json!({ "disabled": true }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "disabling someone else's account");

        let (status, _, _) = post_json(
            &app.app,
            &format!("/api/v1/service-accounts/{id}/tokens"),
            Some(&bob),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "issuing a token for someone else's account");
    }

    #[tokio::test]
    async fn an_identity_administrator_manages_every_account() {
        let app = Harness::new(&["root"]);
        let (_, ada) = app.user("ada");
        let (_, root) = app.user("root");
        let (_, created) = app.create(&ada, "deployer").await;
        let id = created["id"].as_str().expect("id");

        let (status, _, _) =
            get_as(&app.app, &format!("/api/v1/service-accounts/{id}"), &root).await;
        assert_eq!(status, StatusCode::OK, "plugin:rbac:user:rw reads any account");

        let (status, body, _) = patch_json(
            &app.app,
            &format!("/api/v1/service-accounts/{id}"),
            &root,
            json!({ "disabled": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["disabled"], true);
    }

    #[tokio::test]
    async fn listing_shows_only_your_own_accounts_unless_you_administer_identity() {
        let app = Harness::new(&["root"]);
        let (_, ada) = app.user("ada");
        let (_, bob) = app.user("bob");
        let (_, root) = app.user("root");
        app.create(&ada, "ada-deployer").await;
        app.create(&bob, "bob-deployer").await;

        let (_, body, _) = get_as(&app.app, "/api/v1/service-accounts", &ada).await;
        let names: Vec<&str> =
            body.as_array().expect("list").iter().map(|a| a["name"].as_str().expect("n")).collect();
        assert_eq!(names, ["ada-deployer"], "a user sees only what they own");

        let (_, body, _) = get_as(&app.app, "/api/v1/service-accounts", &root).await;
        assert_eq!(body.as_array().expect("list").len(), 2, "an administrator sees every account");
    }

    #[tokio::test]
    async fn a_token_secret_is_returned_once_and_never_listed() {
        let app = Harness::new(&[]);
        let (_, ada) = app.user("ada");
        let (_, created) = app.create(&ada, "deployer").await;
        let id = created["id"].as_str().expect("id");

        let (status, body, _) = post_json(
            &app.app,
            &format!("/api/v1/service-accounts/{id}/tokens"),
            Some(&ada),
            json!({ "name": "ci" }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let secret = body["token"].as_str().expect("the secret is returned once").to_string();
        assert!(secret.starts_with("doc_svc_"), "{secret}");

        let (status, body, _) =
            get_as(&app.app, &format!("/api/v1/service-accounts/{id}/tokens"), &ada).await;
        assert_eq!(status, StatusCode::OK);
        let listed = body.as_array().expect("tokens");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["name"], "ci");
        assert!(listed[0].get("token").is_none(), "a listing must never carry the secret");
        assert!(!body.to_string().contains(&secret), "the secret must not appear anywhere");

        let (status, body, _) = get_as(&app.app, "/api/v1/me", &secret).await;
        assert_eq!(status, StatusCode::OK, "the issued secret authenticates");
        assert_eq!(body["name"], "deployer");
    }

    #[tokio::test]
    async fn revoking_a_token_stops_it_working() {
        let app = Harness::new(&[]);
        let (_, ada) = app.user("ada");
        let (_, created) = app.create(&ada, "deployer").await;
        let id = created["id"].as_str().expect("id");
        let (_, body, _) = post_json(
            &app.app,
            &format!("/api/v1/service-accounts/{id}/tokens"),
            Some(&ada),
            json!({}),
        )
        .await;
        let secret = body["token"].as_str().expect("secret").to_string();
        let token_id = body["created"]["id"].as_str().expect("id");

        let (status, _, _) =
            delete_as(&app.app, &format!("/api/v1/service-accounts/{id}/tokens/{token_id}"), &ada)
                .await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) = get_as(&app.app, "/api/v1/me", &secret).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "a revoked token stops working at once");
    }

    #[tokio::test]
    async fn disabling_an_account_stops_its_tokens_working() {
        let app = Harness::new(&[]);
        let (_, ada) = app.user("ada");
        let (_, created) = app.create(&ada, "deployer").await;
        let id = created["id"].as_str().expect("id");
        let (_, body, _) = post_json(
            &app.app,
            &format!("/api/v1/service-accounts/{id}/tokens"),
            Some(&ada),
            json!({}),
        )
        .await;
        let secret = body["token"].as_str().expect("secret").to_string();
        let (status, _, _) = get_as(&app.app, "/api/v1/me", &secret).await;
        assert_eq!(status, StatusCode::OK, "the token works before the account is disabled");

        let (status, _, _) = patch_json(
            &app.app,
            &format!("/api/v1/service-accounts/{id}"),
            &ada,
            json!({ "disabled": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = get_as(&app.app, "/api/v1/me", &secret).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "the cached decision must be dropped");
    }

    #[tokio::test]
    async fn a_disabled_account_cannot_be_given_new_tokens() {
        let app = Harness::new(&[]);
        let (_, ada) = app.user("ada");
        let (_, created) = app.create(&ada, "deployer").await;
        let id = created["id"].as_str().expect("id");
        patch_json(
            &app.app,
            &format!("/api/v1/service-accounts/{id}"),
            &ada,
            json!({ "disabled": true }),
        )
        .await;
        let (status, _, _) = post_json(
            &app.app,
            &format!("/api/v1/service-accounts/{id}/tokens"),
            Some(&ada),
            json!({}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn only_an_identity_administrator_can_disable_a_user() {
        let app = Harness::new(&["root"]);
        let (bob, _) = app.user("bob");
        let (_, ada) = app.user("ada");
        let (_, root) = app.user("root");

        let (status, _, _) = patch_json(
            &app.app,
            &format!("/api/v1/users/{}", bob.id),
            &ada,
            json!({ "disabled": true }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "an ordinary user cannot disable anyone");

        let (status, body, _) = patch_json(
            &app.app,
            &format!("/api/v1/users/{}", bob.id),
            &root,
            json!({ "disabled": true }),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["disabled"], true);
    }

    /// Whatever answers `plugin.rbac` on the Service Bus, keeping what core asked it.
    struct FakeRbac {
        answer: Value,
        asked: parking_lot::Mutex<Vec<(String, Value)>>,
    }

    #[async_trait]
    impl doc_servicebus::ServiceHandler for FakeRbac {
        async fn handle(&self, request: doc_servicebus::Request) -> Result<Value, String> {
            self.asked.lock().push((request.subject, request.payload));
            Ok(self.answer.clone())
        }
    }

    fn rbac(answer: Value) -> Arc<FakeRbac> {
        Arc::new(FakeRbac { answer, asked: parking_lot::Mutex::new(Vec::new()) })
    }

    impl Harness {
        async fn with_rbac(held: &[(&str, &str)], rbac: Option<Arc<FakeRbac>>) -> Self {
            let identity = FakeIdentity::empty();
            let mut grants: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (login, permission) in held {
                grants.entry((*login).to_string()).or_default().push((*permission).to_string());
            }
            let repos = crate::testing::repositories(FakeHealth::up(), identity.clone());
            let state = AppState::new(Config::default(), repos, Buses::in_memory())
                .with_permissions(Arc::new(Source(grants)));
            if let Some(rbac) = rbac {
                let address = doc_servicebus::Address::plugin("rbac").unwrap();
                state.buses.services.serve(address, rbac).await.unwrap();
            }
            Harness { app: super::super::router(state), identity }
        }

        async fn grant(
            &self,
            token: &str,
            account: &Value,
            permission: &str,
        ) -> (StatusCode, Value) {
            let path =
                format!("/api/v1/service-accounts/{}/permissions", account["id"].as_str().unwrap());
            let (status, body, _) =
                post_json(&self.app, &path, Some(token), json!({ "permission": permission })).await;
            (status, body)
        }
    }

    #[tokio::test]
    async fn an_owner_grants_through_the_rbac_plugin_with_their_own_access() {
        let fake = rbac(json!({ "permission": "plugin:kb:service:wo", "added": true }));
        let app = Harness::with_rbac(&[("ada", "plugin:kb:user:rw")], Some(fake.clone())).await;
        let (_, ada) = app.user("ada");
        let (_, account) = app.create(&ada, "deployer").await;

        let (status, body) = app.grant(&ada, &account, "plugin:kb:service:wo").await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(body["permission"], "plugin:kb:service:wo");

        let asked = fake.asked.lock().clone();
        let (subject, request) = &asked[0];
        assert_eq!(subject, "self-service/grant");
        assert_eq!(request["account"], account["id"]);
        assert_eq!(request["by"], "ada");
        assert_eq!(request["unlimited"], false, "an owner is held to rule 6");
        assert_eq!(request["grants"]["permissions"], json!(["plugin:kb:user:rw"]));
        let audited = app.identity.audit_actions();
        assert!(audited.contains(&"iam.service-account.permission.granted".to_string()));
    }

    #[tokio::test]
    async fn the_rbac_plugins_refusal_reaches_the_caller() {
        let refused = json!({ "refused": {
            "status": 403,
            "detail": "plugin:kb:service:rw goes beyond your own access to kb",
        }});
        let app = Harness::with_rbac(&[("ada", "plugin:kb:user:ro")], Some(rbac(refused))).await;
        let (_, ada) = app.user("ada");
        let (_, account) = app.create(&ada, "deployer").await;

        let (status, body) = app.grant(&ada, &account, "plugin:kb:service:rw").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["detail"], "plugin:kb:service:rw goes beyond your own access to kb");
        let audited = app.identity.audit_actions();
        assert!(!audited.contains(&"iam.service-account.permission.granted".to_string()));
    }

    #[tokio::test]
    async fn only_the_owner_or_an_identity_administrator_grants() {
        let fake = rbac(json!({ "added": true }));
        let app = Harness::with_rbac(&[("root", "plugin:rbac:user:rw")], Some(fake.clone())).await;
        let (_, ada) = app.user("ada");
        let (_, bob) = app.user("bob");
        let (_, root) = app.user("root");
        let (_, account) = app.create(&ada, "deployer").await;

        let (status, _) = app.grant(&bob, &account, "plugin:kb:service").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(fake.asked.lock().is_empty(), "a stranger's grant never reaches the plugin");

        let (status, _) = app.grant(&root, &account, "plugin:kb:service").await;
        assert_eq!(status, StatusCode::CREATED);
        assert_eq!(
            fake.asked.lock()[0].1["unlimited"],
            true,
            "rule 6 limits owners, not administrators"
        );
    }

    #[tokio::test]
    async fn with_the_rbac_plugin_down_a_grant_is_unavailable() {
        let app = Harness::with_rbac(&[("ada", "plugin:kb:user:rw")], None).await;
        let (_, ada) = app.user("ada");
        let (_, account) = app.create(&ada, "deployer").await;
        let (status, _) = app.grant(&ada, &account, "plugin:kb:service").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn an_owner_reads_and_revokes_their_accounts_permissions() {
        let fake = rbac(json!({ "label": "deployer", "assignments": [] }));
        let app = Harness::with_rbac(&[], Some(fake.clone())).await;
        let (_, ada) = app.user("ada");
        let (_, account) = app.create(&ada, "deployer").await;
        let path =
            format!("/api/v1/service-accounts/{}/permissions", account["id"].as_str().unwrap());

        let (status, body, _) = get_as(&app.app, &path, &ada).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["label"], "deployer");
        let (status, _, _) =
            delete_as(&app.app, &format!("{path}/plugin:kb:service:ro"), &ada).await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let asked = fake.asked.lock().clone();
        assert_eq!(asked[0].0, "principal");
        assert_eq!(asked[0].1["holder"], json!({ "kind": "service", "id": account["id"] }));
        assert_eq!(asked[1].0, "self-service/revoke");
        assert_eq!(asked[1].1["permission"], "plugin:kb:service:ro");
    }

    #[tokio::test]
    async fn a_user_cannot_disable_themselves() {
        let app = Harness::new(&["root"]);
        let (root_user, root) = app.user("root");
        let (status, _, _) = patch_json(
            &app.app,
            &format!("/api/v1/users/{}", root_user.id),
            &root,
            json!({ "disabled": true }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
    }
}
