//! Authentication: turning a bearer token into a `Principal`. Verified records are cached in the
//! Cache Bus. Revoking a token drops its entry immediately; anything else that invalidates a
//! record — disabling a user or a service account (T16) — must call `forget_all`, or the change
//! waits out `CACHE_TTL`.

use std::time::Duration;

use axum::extract::FromRequestParts;
use chrono::Utc;
use doc_cachebus::Namespace;
use http::header::AUTHORIZATION;
use http::request::Parts;

use crate::api::AppState;
use crate::api::problem::Problem;
use crate::db::repositories::TokenRecord;
use crate::identity::{Principal, token_hash};
use crate::secrets::TokenKind;

const NAMESPACE: &str = "core.tokens";
/// Deliberately shorter than the namespace default: this entry is an authorisation decision, so
/// the window in which a change that cannot invalidate it is still honoured stays small.
const CACHE_TTL: Duration = Duration::from_secs(60);

/// A caller that presented a usable token. Handlers take this to require authentication.
pub struct Auth(pub Principal);

impl Auth {
    pub fn principal(&self) -> &Principal {
        &self.0
    }
}

impl FromRequestParts<AppState> for Auth {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let client = state.limits.client(parts);
        let token = bearer(parts).ok_or_else(Problem::unauthorized)?;
        let principal = authenticate_from(state, &token, &client).await?;
        // A scoped token opens the routes of the plugins it names and nothing of core's: nothing
        // else checks it, so a core route that decides by who is asking cannot be reached with one.
        if let Some(scopes) = principal.scopes() {
            let plugin = crate::permissions::plugin_of(&crate::permissions::path_of(parts));
            if !crate::permissions::reaches(scopes, &plugin) {
                return Err(Problem::forbidden(
                    "a scoped token reaches only the routes of the plugins it names",
                ));
            }
        }
        Ok(Self(principal))
    }
}

fn bearer(parts: &Parts) -> Option<String> {
    let value = parts.headers.get(AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim().to_string())
}

/// Why a token was refused: one nobody was ever given counts against its client, a revoked or
/// expired one does not, so a browser holding an old session cannot lock its neighbours out.
enum Refused {
    Unknown,
    Problem(Problem),
}

/// As `authenticate`, for `client`, which waits a while once it has presented too many unknown tokens.
pub async fn authenticate_from(
    state: &AppState,
    token: &str,
    client: &str,
) -> Result<Principal, Problem> {
    let limiter = &state.limits.auth_failures;
    if let Some(wait) = limiter.waiting(client) {
        return Err(Problem::too_many("too many unknown tokens from this client", wait));
    }
    match verify(state, token).await {
        Ok(principal) => Ok(principal),
        Err(Refused::Unknown) => {
            let _ = limiter.hit(client);
            Err(Problem::unauthorized())
        }
        Err(Refused::Problem(problem)) => Err(problem),
    }
}

/// Every failure is the same 401: which of them it was must not be observable from outside.
pub async fn authenticate(state: &AppState, token: &str) -> Result<Principal, Problem> {
    verify(state, token).await.map_err(|refused| match refused {
        Refused::Unknown => Problem::unauthorized(),
        Refused::Problem(problem) => problem,
    })
}

async fn verify(state: &AppState, token: &str) -> Result<Principal, Refused> {
    if TokenKind::of(token).is_none() {
        return Err(Refused::Unknown);
    }
    let hash = token_hash(token);
    let record = match cached(state, &hash).await {
        Some(record) => record,
        None => {
            let record = state
                .repos
                .identity
                .token_by_hash(&hash)
                .await
                .map_err(|err| Refused::Problem(Problem::from(err)))?
                .ok_or(Refused::Unknown)?;
            cache(state, &hash, &record).await;
            let _ = state.repos.identity.touch_token(record.token.id).await;
            record
        }
    };
    if !record.token.usable(Utc::now()) || record.principal.disabled() {
        forget(state, &hash).await;
        return Err(Refused::Problem(Problem::unauthorized()));
    }
    let mut principal = record.principal;
    if record.token.kind == TokenKind::Scoped {
        let Principal::User(user) = &mut principal else {
            return Err(Refused::Problem(Problem::unauthorized()));
        };
        user.scopes = Some(record.token.scopes.unwrap_or_default());
    }
    Ok(principal)
}

fn namespace() -> Option<Namespace> {
    Namespace::new(NAMESPACE).ok()
}

fn key(hash: &[u8]) -> String {
    hex::encode(hash)
}

async fn cached(state: &AppState, hash: &[u8]) -> Option<TokenRecord> {
    let entry = state.buses.cache.get(&namespace()?, &key(hash)).await.ok()??;
    serde_json::from_value(entry.value).ok()
}

async fn cache(state: &AppState, hash: &[u8], record: &TokenRecord) {
    let Some(namespace) = namespace() else { return };
    let Ok(value) = serde_json::to_value(record) else { return };
    let _ = state.buses.cache.set(&namespace, &key(hash), value, Some(CACHE_TTL)).await;
}

pub async fn forget(state: &AppState, hash: &[u8]) {
    let Some(namespace) = namespace() else { return };
    let _ = state.buses.cache.delete(&namespace, &key(hash)).await;
}

/// Drops every cached decision. Callers that disable an account use this, since they hold the
/// account rather than the token hashes that authenticate it.
pub async fn forget_all(state: &AppState) {
    let Some(namespace) = namespace() else { return };
    let _ = state.buses.cache.clear(&namespace).await;
}
