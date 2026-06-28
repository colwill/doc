//! Scoped tokens (FEAT-VACUUM): a person's, limited to named permissions for minutes, for handing an
//! agent just enough of their access for one job. A token reaches only the routes of the plugins
//! its scopes name, holds at most what its holder holds there, is never an administrator, cannot
//! mint another, delegate, or be passed on to other plugins as its holder, and ends by itself.
//!
//! People mint their own at `/api/v1/tokens/scoped`; a plugin with the `token-issuer` capability
//! mints one for whoever is asking it, and may revoke those it minted.

use chrono::{DateTime, Duration, Utc};
use doc_secret::Secret;
use serde::{Deserialize, Serialize};
use serde_json::json;
use uuid::Uuid;

use crate::api::AppState;
use crate::api::problem::Problem;
use crate::db::repositories::NewToken;
use crate::identity::{ApiToken, AuditEntry, Principal, token_hash};
use crate::permissions::{self, is_admin, narrowed, scope_of};
use crate::secrets::{TokenKind, generate_token};

/// Long enough for an agent to work through a job, short enough that a token copied somewhere
/// careless is soon worth nothing.
pub const DEFAULT_MINUTES: i64 = 60;
pub const MAX_MINUTES: i64 = 8 * 60;
const MAX_SCOPES: usize = 12;
const MAX_NAME: usize = 100;

#[derive(Debug, Deserialize)]
pub struct NewScopedToken {
    pub name: String,
    pub scopes: Vec<String>,
    #[serde(default)]
    pub expires_in_minutes: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct ScopedView {
    pub id: Uuid,
    pub name: Option<String>,
    pub scopes: Vec<String>,
    pub issued_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
    pub expires_at: Option<DateTime<Utc>>,
}

impl From<ApiToken> for ScopedView {
    fn from(token: ApiToken) -> Self {
        Self {
            id: token.id,
            name: token.name,
            scopes: token.scopes.unwrap_or_default(),
            issued_by: token.issued_by,
            created_at: token.created_at,
            last_used_at: token.last_used_at,
            expires_at: token.expires_at,
        }
    }
}

pub struct Minted {
    pub secret: Secret<String>,
    pub token: ApiToken,
}

/// Mints a scoped token for `holder`, a person asking for themselves, not through another scoped
/// token. Every scope must be a plugin's user permission they hold at least in part; what they
/// hold less of than asked is narrowed when the token is used, as their access changes.
pub async fn mint(
    state: &AppState,
    holder: &Principal,
    asked: &NewScopedToken,
    issued_by: Option<&str>,
) -> Result<Minted, Problem> {
    let Principal::User(user) = holder else {
        return Err(Problem::forbidden("only a person has scoped tokens"));
    };
    if holder.scopes().is_some() {
        return Err(Problem::forbidden("a scoped token cannot mint another"));
    }
    let name = asked.name.trim();
    if name.is_empty() || name.chars().count() > MAX_NAME || name.contains(char::is_control) {
        return Err(Problem::bad_request(format!("a token's name is 1 to {MAX_NAME} characters")));
    }
    if asked.scopes.is_empty() || asked.scopes.len() > MAX_SCOPES {
        return Err(Problem::bad_request(format!("a scoped token names 1 to {MAX_SCOPES} scopes")));
    }
    let minutes = asked.expires_in_minutes.unwrap_or(DEFAULT_MINUTES);
    if !(1..=MAX_MINUTES).contains(&minutes) {
        return Err(Problem::bad_request(format!(
            "a scoped token lasts 1 to {MAX_MINUTES} minutes"
        )));
    }
    let held = permissions::grants(state, holder).await;
    let admin = is_admin(&held, holder);
    let mut scopes = Vec::new();
    for text in &asked.scopes {
        let scope = scope_of(text).ok_or_else(|| {
            Problem::bad_request(format!(
                "`{text}` is not a scope: name a plugin's user permission, such as plugin:kb:user:ro"
            ))
        })?;
        let text = scope.to_string();
        if narrowed(&held, admin, std::slice::from_ref(&text)).permissions.is_empty() {
            return Err(Problem::bad_request(format!(
                "you hold nothing on {} that {text} could give",
                scope.plugin
            )));
        }
        if !scopes.contains(&text) {
            scopes.push(text);
        }
    }
    let secret = generate_token(TokenKind::Scoped)
        .map_err(|err| Problem::internal(format!("issuing a token: {err}")))?;
    let token = state
        .repos
        .identity
        .issue_token(NewToken {
            kind: TokenKind::Scoped,
            token_hash: token_hash(secret.expose()),
            name: Some(name.to_string()),
            owner: holder.owner(),
            expires_at: Some(Utc::now() + Duration::minutes(minutes)),
            scopes: Some(scopes.clone()),
            issued_by: issued_by.map(str::to_string),
        })
        .await?;
    let entry = AuditEntry::new("auth.token.scoped.created")
        .by(holder)
        .subject(token.id.to_string())
        .detail(json!({
            "scopes": scopes, "expires_at": token.expires_at, "issued_by": issued_by,
            "user": user.login,
        }));
    let _ = state.repos.identity.record_audit(entry).await;
    Ok(Minted { secret, token })
}

/// Someone's scoped tokens that are still in force.
pub async fn active(state: &AppState, holder: &Principal) -> Result<Vec<ScopedView>, Problem> {
    let now = Utc::now();
    let tokens = state.repos.identity.list_tokens(&holder.owner(), TokenKind::Scoped).await?;
    Ok(tokens.into_iter().filter(|token| token.usable(now)).map(ScopedView::from).collect())
}

#[cfg(test)]
mod tests {
    use http::StatusCode;
    use serde_json::{Value, json};

    use crate::testing::{ADMIN, Host, delete_as, get_as, manifest, plugin_host, post_json};

    const MINT: &str = "/api/v1/tokens/scoped";
    const HELLO: &str = "/api/v1/plugins/hello/api/things";

    async fn minted(host: &Host, holder: &str, asked: Value) -> (String, String) {
        let (status, made, _) = post_json(&host.app, MINT, Some(holder), asked.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{asked}: {made}");
        let token = made["token"].as_str().expect("a token").to_string();
        (token, made["created"]["id"].as_str().expect("an id").to_string())
    }

    #[tokio::test]
    async fn a_scoped_token_reaches_only_what_it_names_at_no_more_than_its_holder_has() {
        let host = plugin_host();
        host.register(manifest("hello", "1.0.0")).await;
        let ada = host.user_holding("ada", &["plugin:hello:user:ro"]).await;
        for (asked, why) in [
            (json!({ "name": "a", "scopes": ["plugin:rbac:user:ro"] }), "she holds nothing there"),
            (json!({ "name": "a", "scopes": ["plugin:core:user:rw"] }), "core is never a scope"),
            (json!({ "name": "a", "scopes": ["plugin:*:user:ro"] }), "nor every plugin"),
            (json!({ "name": "a", "scopes": [] }), "it names something"),
            (
                json!({ "name": "a", "scopes": ["plugin:hello:user:ro"], "expires_in_minutes": 600 }),
                "minutes, not days",
            ),
        ] {
            let (status, _, _) = post_json(&host.app, MINT, Some(&ada), asked).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{why}");
        }
        let asked = json!({ "name": "agent", "scopes": ["plugin:hello:user:rw"], "expires_in_minutes": 30 });
        let (token, _) = minted(&host, &ada, asked).await;
        assert!(token.starts_with("doc_scp_"));

        let (status, body, _) = get_as(&host.app, HELLO, &token).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["caller"]["scope"], "ro", "rw asked of someone who reads gives ro");
        assert_eq!(
            body["caller"]["login"].as_str().or(body["caller"]["label"].as_str()),
            Some("ada")
        );
        let (status, _, _) = post_json(&host.app, HELLO, Some(&token), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "no more than she holds");

        for path in
            ["/api/v1/me", "/api/v1/tokens", "/api/v1/plugins/rbac/api/x", "/api/v1/plugins"]
        {
            let (status, _, _) = get_as(&host.app, path, &token).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path} is not a route it names");
        }
        let again = json!({ "name": "b", "scopes": ["plugin:hello:user:ro"] });
        let (status, _, _) = post_json(&host.app, MINT, Some(&token), again).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "a scoped token mints no other");
    }

    #[tokio::test]
    async fn an_administrators_scoped_token_is_no_administrator_and_ends_when_revoked() {
        let host = plugin_host();
        host.register(manifest("hello", "1.0.0")).await;
        let asked = json!({ "name": "agent", "scopes": ["plugin:hello:user:rw"] });
        let (token, id) = minted(&host, ADMIN, asked).await;

        let (status, body, _) = post_json(&host.app, HELLO, Some(&token), json!({})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["caller"]["admin"], false);
        assert_eq!(body["caller"]["scope"], "rw");
        let (status, _, _) = get_as(&host.app, "/api/v1/users", &token).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (_, listed, _) = get_as(&host.app, MINT, ADMIN).await;
        assert_eq!(listed[0]["scopes"], json!(["plugin:hello:user:rw"]));
        assert!(listed[0]["expires_at"].is_string(), "a scoped token always ends");
        let (status, _, _) = delete_as(&host.app, &format!("/api/v1/tokens/{id}"), ADMIN).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) = get_as(&host.app, HELLO, &token).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
}
