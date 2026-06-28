//! Who am I, signing in and out, and personal access tokens. The token secret is returned by the
//! call that creates it and never again; only its SHA-256 is stored.

use axum::Json;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use chrono::{Duration, Utc};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::problem::Problem;
use crate::auth::{Auth, forget};
use crate::db::repositories::NewToken;
use crate::identity::{ApiToken, AuditEntry, Principal, User, token_hash};
use crate::secrets::{TokenKind, generate_token};
use doc_secret::Secret;

const SESSION_HOURS: i64 = 12;
const MAX_TOKEN_DAYS: i64 = 365;

#[derive(Debug, Deserialize)]
pub struct NewPersonalToken {
    pub name: String,
    #[serde(default)]
    pub expires_in_days: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct TokenView {
    pub id: Uuid,
    pub name: Option<String>,
    pub created_at: chrono::DateTime<Utc>,
    pub last_used_at: Option<chrono::DateTime<Utc>>,
    pub expires_at: Option<chrono::DateTime<Utc>>,
    pub revoked_at: Option<chrono::DateTime<Utc>>,
}

impl From<ApiToken> for TokenView {
    fn from(token: ApiToken) -> Self {
        Self {
            id: token.id,
            name: token.name,
            created_at: token.created_at,
            last_used_at: token.last_used_at,
            expires_at: token.expires_at,
            revoked_at: token.revoked_at,
        }
    }
}

pub async fn me(auth: Auth) -> Json<Principal> {
    Json(auth.0)
}

/// An identity provider that is running, as sign-in and linking offer it.
#[derive(Debug, Clone, Serialize)]
pub struct Offered {
    pub id: String,
    pub title: String,
    pub kind: doc_plugin_protocol::SignInKind,
}

/// Every identity provider that is running: what people can sign in with, or link.
pub async fn identity_providers(state: &AppState) -> Vec<Offered> {
    let running: Vec<_> = state
        .plugins
        .list()
        .await
        .into_iter()
        .filter(|entry| {
            entry.state.serves_requests()
                && entry
                    .manifest
                    .capabilities
                    .contains(&doc_plugin_protocol::Capability::IdentityProvider)
        })
        .collect();
    // A provider whose sign-in belongs to a feature is offered only while that feature is on, so
    // a plugin that can sign people in once it is configured is not offered before it is; and
    // what it is called may be a setting of its own (ADR-0007).
    let mut offered: Vec<Offered> = Vec::new();
    for entry in running {
        let sign_in = entry.manifest.sign_in.clone();
        let configured = match sign_in
            .as_ref()
            .is_some_and(|sign_in| sign_in.feature.is_some() || sign_in.setting.is_some())
        {
            true => {
                Some(crate::plugins::settings::resolve(state, &entry.id, &entry.manifest).await)
            }
            false => None,
        };
        let wanted = match (&sign_in, &configured) {
            (Some(sign_in), Some(configured)) => {
                sign_in.feature.as_ref().is_none_or(|feature| configured.feature(feature))
            }
            _ => true,
        };
        if !wanted {
            continue;
        }
        let named = sign_in.as_ref().and_then(|sign_in| {
            let setting = sign_in.setting.as_ref()?;
            configured.as_ref()?.text(setting)
        });
        offered.push(Offered {
            title: named.unwrap_or_else(|| {
                sign_in.as_ref().map_or_else(|| entry.id.clone(), |sign_in| sign_in.title.clone())
            }),
            kind: sign_in.map_or(doc_plugin_protocol::SignInKind::Redirect, |sign_in| sign_in.kind),
            id: entry.id,
        });
    }
    offered.sort_by(|a, b| a.id.cmp(&b.id));
    offered
}

/// What a sign-in page offers: each organisation's identity providers that are running now, since
/// people sign in with their own organisation's (ADR-0005).
pub async fn providers(State(state): State<AppState>) -> Result<Json<Value>, Problem> {
    let running = identity_providers(&state).await;
    let chosen = state.repos.teams.providers().await?;
    let mut organisations = Vec::new();
    for organisation in state.repos.teams.organisations().await? {
        let offered: Vec<&Offered> = running
            .iter()
            .filter(|offered| {
                chosen
                    .iter()
                    .any(|(provider, by)| provider == &offered.id && *by == organisation.id)
            })
            .collect();
        if offered.is_empty() {
            continue;
        }
        organisations.push(json!({
            "id": organisation.id, "name": organisation.name, "title": organisation.title,
            "providers": offered,
        }));
    }
    Ok(Json(json!({ "organisations": organisations })))
}

/// What the identity provider knew of a user at sign-in, which onboarding rules match on.
#[derive(Debug, Clone, Default)]
pub struct Membership {
    pub organisations: Vec<String>,
    pub teams: Vec<String>,
}

/// A new session, whose secret is shown only once.
pub struct Started {
    pub secret: Secret<String>,
    pub expires_at: Option<chrono::DateTime<Utc>>,
    /// The user's first sign-in, which the frontend follows with the accounts plugins need.
    pub first: bool,
}

/// Starts a session however the user signed in.
pub async fn start_session(
    state: &AppState,
    user: &User,
    provider: &str,
    membership: &Membership,
) -> Result<Started, Problem> {
    let first = user.first_signed_in_at.is_none();
    state.repos.identity.record_sign_in(user.id).await?;
    let secret = generate_token(TokenKind::Session)
        .map_err(|err| Problem::internal(format!("issuing a session token: {err}")))?;
    let token = state
        .repos
        .identity
        .issue_token(NewToken {
            kind: TokenKind::Session,
            token_hash: token_hash(secret.expose()),
            name: Some(format!("{provider} sign-in")),
            owner: crate::identity::TokenOwner::User(user.id),
            expires_at: Some(Utc::now() + Duration::hours(SESSION_HOURS)),
            scopes: None,
            issued_by: None,
        })
        .await?;
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("auth.sign-in")
                .by(&Principal::User(user.clone()))
                .detail(json!({ "provider": provider })),
        )
        .await;
    let signed_in = json!({
        "user": { "id": user.id, "login": user.login, "provider": provider },
        "provider": provider,
        "first": first,
        "organisations": membership.organisations,
        "teams": membership.teams,
    });
    super::iam::announce(state, "user.signed-in", signed_in).await;
    Ok(Started { secret, expires_at: token.expires_at, first })
}

pub async fn logout(
    State(state): State<AppState>,
    auth: Auth,
    headers: http::HeaderMap,
) -> Result<StatusCode, Problem> {
    let Some(secret) = bearer(&headers) else { return Err(Problem::unauthorized()) };
    let hash = token_hash(&secret);
    let owner = auth.0.owner();
    if let Some(record) = state.repos.identity.token_by_hash(&hash).await? {
        state.repos.identity.revoke_token(record.token.id, &owner).await?;
    }
    forget(&state, &hash).await;
    let _ = state.repos.identity.record_audit(AuditEntry::new("auth.logout").by(&auth.0)).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_personal_tokens(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<TokenView>>, Problem> {
    let tokens = state.repos.identity.list_tokens(&auth.0.owner(), TokenKind::Personal).await?;
    Ok(Json(tokens.into_iter().map(TokenView::from).collect()))
}

pub async fn create_personal_token(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<NewPersonalToken>,
) -> Result<impl IntoResponse, Problem> {
    // A plugin or service account minting its own tokens would outlive revoking the one it holds.
    if !matches!(auth.0, Principal::User(_)) {
        return Err(Problem::forbidden("only a user can have personal access tokens"));
    }
    if body.name.trim().is_empty() {
        return Err(Problem::bad_request("a token needs a name"));
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
    let secret = generate_token(TokenKind::Personal)
        .map_err(|err| Problem::internal(format!("issuing a token: {err}")))?;
    let token = state
        .repos
        .identity
        .issue_token(NewToken {
            kind: TokenKind::Personal,
            token_hash: token_hash(secret.expose()),
            name: Some(body.name.trim().to_string()),
            owner: auth.0.owner(),
            expires_at,
            scopes: None,
            issued_by: None,
        })
        .await?;
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("auth.token.created").by(&auth.0).subject(token.id.to_string()),
        )
        .await;
    let view = TokenView::from(token);
    Ok((StatusCode::CREATED, Json(json!({ "token": secret.expose(), "created": view }))))
}

/// The caller's scoped tokens still in force (FEAT-VACUUM).
pub async fn list_scoped_tokens(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<crate::scoped::ScopedView>>, Problem> {
    Ok(Json(crate::scoped::active(&state, &auth.0).await?))
}

/// A scoped token for the caller, shown once: limited to the scopes named, for minutes.
pub async fn create_scoped_token(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<crate::scoped::NewScopedToken>,
) -> Result<impl IntoResponse, Problem> {
    let minted = crate::scoped::mint(&state, &auth.0, &body, None).await?;
    let view = crate::scoped::ScopedView::from(minted.token);
    Ok((StatusCode::CREATED, Json(json!({ "token": minted.secret.expose(), "created": view }))))
}

pub async fn revoke_personal_token(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    let Some(hash) = state.repos.identity.revoke_token(id, &auth.0.owner()).await? else {
        return Err(Problem::not_found("token"));
    };
    forget(&state, &hash).await;
    let _ = state
        .repos
        .identity
        .record_audit(AuditEntry::new("auth.token.revoked").by(&auth.0).subject(id.to_string()))
        .await;
    Ok(StatusCode::NO_CONTENT)
}

fn bearer(headers: &http::HeaderMap) -> Option<String> {
    let value = headers.get(http::header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::db::memory::{FakeHealth, FakeIdentity};
    use crate::identity::TokenOwner;
    use crate::testing::{delete_as, get_as, identity_app, post_json};
    use axum::Router;
    use std::sync::Arc;

    fn app_with_user(login: &str) -> (Router, Arc<FakeIdentity>, User) {
        let (identity, user) = FakeIdentity::with_user(login);
        let app = identity_app(Config::default(), identity.clone());
        (app, identity, user)
    }

    #[tokio::test]
    async fn a_valid_session_token_identifies_its_user() {
        let (app, identity, user) = app_with_user("ada");
        identity.give("doc_ses_valid", TokenKind::Session, TokenOwner::User(user.id), None, false);
        let (status, body, _) = get_as(&app, "/api/v1/me", "doc_ses_valid").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["kind"], "user");
        assert_eq!(body["login"], "ada");
    }

    #[tokio::test]
    async fn every_stored_token_kind_authenticates() {
        for kind in [TokenKind::Session, TokenKind::Personal, TokenKind::Operator] {
            let (app, identity, user) = app_with_user("ada");
            let secret = format!("{}whatever", kind.prefix());
            identity.give(&secret, kind, TokenOwner::User(user.id), None, false);
            let (status, _, _) = get_as(&app, "/api/v1/me", &secret).await;
            assert_eq!(status, StatusCode::OK, "{kind:?} should authenticate");
        }
    }

    #[tokio::test]
    async fn a_service_account_token_identifies_its_account() {
        let identity = FakeIdentity::empty();
        let account = identity.add_service_account("deployer");
        identity.give(
            "doc_svc_x",
            TokenKind::Service,
            TokenOwner::ServiceAccount(account.id),
            None,
            false,
        );
        let app = identity_app(Config::default(), identity.clone());
        let (status, body, _) = get_as(&app, "/api/v1/me", "doc_svc_x").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["kind"], "service-account");
        assert_eq!(body["name"], "deployer");
    }

    #[tokio::test]
    async fn missing_unknown_and_malformed_tokens_are_all_refused() {
        let (app, _, _) = app_with_user("ada");
        for token in ["", "doc_ses_nosuch", "not-a-doc-token"] {
            let (status, _, _) = get_as(&app, "/api/v1/me", token).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{token:?} must not authenticate");
        }
        let (status, _, _) = crate::testing::get(&app, "/api/v1/me").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "no header at all must not authenticate");
    }

    #[tokio::test]
    async fn an_expired_token_is_refused() {
        let (app, identity, user) = app_with_user("ada");
        identity.give(
            "doc_pat_old",
            TokenKind::Personal,
            TokenOwner::User(user.id),
            Some(Utc::now() - Duration::minutes(1)),
            false,
        );
        let (status, _, _) = get_as(&app, "/api/v1/me", "doc_pat_old").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_revoked_token_is_refused() {
        let (app, identity, user) = app_with_user("ada");
        identity.give("doc_pat_gone", TokenKind::Personal, TokenOwner::User(user.id), None, true);
        let (status, _, _) = get_as(&app, "/api/v1/me", "doc_pat_gone").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_disabled_user_is_refused() {
        let (app, identity, user) = app_with_user("ada");
        identity.give("doc_ses_ok", TokenKind::Session, TokenOwner::User(user.id), None, false);
        identity.set_user_disabled(user.id, true);
        let (status, _, _) = get_as(&app, "/api/v1/me", "doc_ses_ok").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_disabled_service_account_is_refused() {
        let identity = FakeIdentity::empty();
        let account = identity.add_service_account("deployer");
        identity.give(
            "doc_svc_x",
            TokenKind::Service,
            TokenOwner::ServiceAccount(account.id),
            None,
            false,
        );
        identity.set_account_disabled(account.id, true);
        let app = identity_app(Config::default(), identity.clone());
        let (status, _, _) = get_as(&app, "/api/v1/me", "doc_svc_x").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// Disabling cannot reach the cache on its own, so T16 must call `forget_all`; this pins the
    /// behaviour that makes that necessary rather than leaving it to be discovered later.
    #[tokio::test]
    async fn disabling_takes_effect_once_the_cached_decision_is_dropped() {
        let (identity, user) = FakeIdentity::with_user("ada");
        identity.give("doc_ses_ok", TokenKind::Session, TokenOwner::User(user.id), None, false);
        let repos = crate::testing::repositories(FakeHealth::up(), identity.clone());
        let state = AppState::new(Config::default(), repos, crate::fabric::Buses::in_memory());
        let app = super::super::router(state.clone());

        let (status, _, _) = get_as(&app, "/api/v1/me", "doc_ses_ok").await;
        assert_eq!(status, StatusCode::OK, "the decision is now cached");

        identity.set_user_disabled(user.id, true);
        let (status, _, _) = get_as(&app, "/api/v1/me", "doc_ses_ok").await;
        assert_eq!(status, StatusCode::OK, "the cached decision still stands");

        crate::auth::forget_all(&state).await;
        let (status, _, _) = get_as(&app, "/api/v1/me", "doc_ses_ok").await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "dropping the cache refuses the disabled user"
        );
    }

    #[tokio::test]
    async fn personal_tokens_are_created_listed_and_revoked() {
        let (app, identity, user) = app_with_user("ada");
        identity.give("doc_ses_ok", TokenKind::Session, TokenOwner::User(user.id), None, false);

        let (status, body, _) = post_json(
            &app,
            "/api/v1/tokens",
            Some("doc_ses_ok"),
            json!({ "name": "laptop", "expires_in_days": 30 }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED);
        let secret = body["token"].as_str().expect("the secret is returned once").to_string();
        assert!(secret.starts_with("doc_pat_"), "{secret}");
        let id = body["created"]["id"].as_str().expect("id").to_string();

        let (status, body, _) = get_as(&app, "/api/v1/tokens", "doc_ses_ok").await;
        assert_eq!(status, StatusCode::OK);
        let listed = body.as_array().expect("a list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0]["name"], "laptop");
        assert!(listed[0].get("token").is_none(), "the secret must never be listed");

        let (status, _, _) = get_as(&app, "/api/v1/me", &secret).await;
        assert_eq!(status, StatusCode::OK, "the new token works");

        let (status, _, _) = delete_as(&app, &format!("/api/v1/tokens/{id}"), "doc_ses_ok").await;
        assert_eq!(status, StatusCode::NO_CONTENT);

        let (status, _, _) = get_as(&app, "/api/v1/me", &secret).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "revoking must take effect immediately");
    }

    #[tokio::test]
    async fn a_token_cannot_be_revoked_by_someone_else() {
        let (identity, ada) = FakeIdentity::with_user("ada");
        let bob = identity.add_user("bob");
        identity.give("doc_ses_ada", TokenKind::Session, TokenOwner::User(ada.id), None, false);
        identity.give("doc_ses_bob", TokenKind::Session, TokenOwner::User(bob.id), None, false);
        let app = identity_app(Config::default(), identity.clone());

        let (_, body, _) =
            post_json(&app, "/api/v1/tokens", Some("doc_ses_ada"), json!({ "name": "ada's" }))
                .await;
        let id = body["created"]["id"].as_str().expect("id").to_string();

        let (status, _, _) = delete_as(&app, &format!("/api/v1/tokens/{id}"), "doc_ses_bob").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "bob must not revoke ada's token");
    }

    #[tokio::test]
    async fn logout_revokes_the_session_it_was_called_with() {
        let (app, identity, user) = app_with_user("ada");
        identity.give("doc_ses_ok", TokenKind::Session, TokenOwner::User(user.id), None, false);
        let (status, _, _) =
            post_json(&app, "/api/v1/auth/logout", Some("doc_ses_ok"), json!({})).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) = get_as(&app, "/api/v1/me", "doc_ses_ok").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_token_needs_a_name_and_a_sane_expiry() {
        let (app, identity, user) = app_with_user("ada");
        identity.give("doc_ses_ok", TokenKind::Session, TokenOwner::User(user.id), None, false);
        for body in [
            json!({ "name": "  " }),
            json!({ "name": "ok", "expires_in_days": 0 }),
            json!({ "name": "ok", "expires_in_days": 400 }),
        ] {
            let (status, _, _) =
                post_json(&app, "/api/v1/tokens", Some("doc_ses_ok"), body.clone()).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{body} should be refused");
        }
    }

    /// A request as the server hands it over, from `peer` and, through it, `forwarded`.
    async fn me_from(
        app: &Router,
        peer: &str,
        forwarded: Option<&str>,
        token: &str,
    ) -> (StatusCode, Option<String>) {
        use axum::extract::ConnectInfo;
        use tower::ServiceExt;
        let mut request = http::Request::builder()
            .uri("/api/v1/me")
            .header(http::header::AUTHORIZATION, format!("Bearer {token}"));
        if let Some(forwarded) = forwarded {
            request = request.header("x-forwarded-for", forwarded);
        }
        let mut request = request.body(axum::body::Body::empty()).expect("request");
        let peer: std::net::SocketAddr = format!("{peer}:40000").parse().expect("address");
        request.extensions_mut().insert(ConnectInfo(peer));
        let response = app.clone().oneshot(request).await.expect("answered");
        let retry = response
            .headers()
            .get(http::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        (response.status(), retry)
    }

    fn limited_app(failures: u32) -> (Router, Arc<FakeIdentity>, User) {
        let (identity, user) = FakeIdentity::with_user("ada");
        let mut config = Config::default();
        config.limits.auth_failures_per_minute = failures;
        identity.give("doc_ses_good", TokenKind::Session, TokenOwner::User(user.id), None, false);
        identity.give("doc_ses_gone", TokenKind::Session, TokenOwner::User(user.id), None, true);
        (identity_app(config, identity.clone()), identity, user)
    }

    #[tokio::test]
    async fn a_client_presenting_unknown_tokens_is_made_to_wait() {
        let (app, _, _) = limited_app(3);
        for guess in 0..3 {
            let token = format!("doc_ses_guess{guess}");
            let (status, _) = me_from(&app, "203.0.113.7", None, &token).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let (status, retry) = me_from(&app, "203.0.113.7", None, "doc_ses_guess4").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let seconds: u64 = retry.expect("Retry-After").parse().expect("seconds");
        assert!((1..=60).contains(&seconds), "{seconds}");
        let (status, _) = me_from(&app, "203.0.113.7", None, "doc_ses_good").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "a waiting client is not checked at all");
        let (status, _) = me_from(&app, "203.0.113.8", None, "doc_ses_good").await;
        assert_eq!(status, StatusCode::OK, "another client is unaffected");
    }

    #[tokio::test]
    async fn revoked_and_expired_tokens_do_not_count_against_their_client() {
        let (app, identity, user) = limited_app(2);
        let expired = Some(Utc::now() - Duration::minutes(1));
        identity.give("doc_ses_old", TokenKind::Session, TokenOwner::User(user.id), expired, false);
        for token in ["doc_ses_gone", "doc_ses_old", "doc_ses_gone", "doc_ses_old", "doc_ses_gone"]
        {
            let (status, _) = me_from(&app, "203.0.113.7", None, token).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{token}");
        }
        let (status, _) = me_from(&app, "203.0.113.7", None, "doc_ses_good").await;
        assert_eq!(status, StatusCode::OK);
    }

    #[tokio::test]
    async fn behind_a_trusted_proxy_each_forwarded_client_is_counted_apart() {
        let (app, _, _) = limited_app(2);
        for _ in 0..2 {
            let (status, _) = me_from(&app, "127.0.0.1", Some("198.51.100.1"), "doc_ses_x").await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let (status, _) = me_from(&app, "127.0.0.1", Some("198.51.100.1"), "doc_ses_x").await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        let (status, _) = me_from(&app, "127.0.0.1", Some("198.51.100.2"), "doc_ses_good").await;
        assert_eq!(status, StatusCode::OK, "the proxy itself, and its other clients, carry on");

        for _ in 0..2 {
            let (status, _) = me_from(&app, "203.0.113.9", Some("198.51.100.3"), "doc_ses_x").await;
            assert_eq!(status, StatusCode::UNAUTHORIZED);
        }
        let (status, _) = me_from(&app, "203.0.113.9", Some("198.51.100.4"), "doc_ses_x").await;
        assert_eq!(
            status,
            StatusCode::TOO_MANY_REQUESTS,
            "an untrusted peer cannot dodge its limit by naming someone else"
        );
    }
}
