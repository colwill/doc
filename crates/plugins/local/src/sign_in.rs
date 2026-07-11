//! Signing in: `public/sign-in` takes a username and a password, and `public/password` takes the
//! password someone chose after signing in with a one-time one. Both answer the frontend with a
//! session, as a provider's sign-in callback does, or with a ticket to choose a password with.

use doc_plugin_sdk::protocol::calls::IdentityRequest;
use doc_plugin_sdk::{Backend, Request, Response};
use serde::Deserialize;
use serde_json::json;

use crate::accounts::{Account, Checked, Refusal, Store};

#[derive(Deserialize)]
struct SigningIn {
    username: String,
    password: String,
    #[serde(default)]
    return_to: Option<String>,
}

#[derive(Deserialize)]
struct Choosing {
    ticket: String,
    password: String,
    #[serde(default)]
    return_to: Option<String>,
}

fn refused(refusal: &Refusal) -> Response {
    let kind = match refusal.status {
        400 => "bad-request",
        401 => "unauthorized",
        403 => "forbidden",
        429 => "too-many-requests",
        _ => "unavailable",
    };
    Response::problem(refusal.status, kind, &refusal.detail)
        .with_header("cache-control", "no-store")
}

/// Only a path on this site, so a sign-in cannot end on someone else's page.
fn local_path(path: Option<String>) -> Option<String> {
    path.filter(|path| path.starts_with('/') && !path.starts_with("//") && !path.contains('\\'))
}

pub async fn sign_in(backend: &Backend, request: &Request) -> Response {
    let asked: SigningIn = match request.json() {
        Ok(asked) => asked,
        Err(err) => return refused(&Refusal::bad(err.to_string())),
    };
    match signed_in(backend, asked).await {
        Ok(session) => session,
        Err(refusal) => refused(&refusal),
    }
}

async fn signed_in(backend: &Backend, asked: SigningIn) -> Result<Response, Refusal> {
    let store = Store(backend);
    let wrong = || Refusal::new(401, "the username or password is not right");
    let return_to = local_path(asked.return_to);
    match store.check(&asked.username, &asked.password).await? {
        Checked::Password(account) => session(backend, &store, &account, return_to).await,
        Checked::OneTime(account) => {
            let ticket = store.ticket(&account.username).await?;
            let chosen = json!({
                "change_password": true, "ticket": ticket, "username": account.username,
                "return_to": return_to,
            });
            Ok(Response::json(&chosen).with_header("cache-control", "no-store"))
        }
        Checked::Wrong => {
            let detail = json!({ "username": asked.username.trim() });
            let _ = backend.audit("sign-in.refused", Some(asked.username.trim()), detail).await;
            Err(wrong())
        }
        Checked::Disabled => Err(Refusal::new(403, "this account is disabled")),
        Checked::Resting => Err(Refusal::new(
            429,
            "too many refused sign-ins for that username: try again in 15 minutes",
        )),
    }
}

pub async fn choose(backend: &Backend, request: &Request) -> Response {
    let asked: Choosing = match request.json() {
        Ok(asked) => asked,
        Err(err) => return refused(&Refusal::bad(err.to_string())),
    };
    let store = Store(backend);
    let chosen = async {
        // Checked before the ticket is used, so a password too short can be tried again.
        crate::accounts::chosen(&asked.password)?;
        let username = store.redeem(&asked.ticket).await?.ok_or_else(|| {
            Refusal::new(403, "that took too long, or was done already: sign in again")
        })?;
        let account = store
            .choose(&username, &asked.password)
            .await?
            .ok_or_else(|| Refusal::new(404, "that account is gone"))?;
        let _ = backend.audit("password.chosen", Some(&account.username), json!({})).await;
        session(backend, &store, &account, local_path(asked.return_to)).await
    };
    match chosen.await {
        Ok(session) => session,
        Err(refusal) => refused(&refusal),
    }
}

/// Core's session for the account, with what DOC knows of the person, and the time it was used.
async fn session(
    backend: &Backend,
    store: &Store<'_>,
    account: &Account,
    return_to: Option<String>,
) -> Result<Response, Refusal> {
    let text = |value: &str| (!value.is_empty()).then(|| value.to_string());
    let identity = IdentityRequest {
        provider: backend.id().to_string(),
        external_id: account.username.clone(),
        login: account.username.clone(),
        name: account.name(),
        email: text(&account.email),
        first_name: text(&account.first_name),
        surname: text(&account.surname),
        ..IdentityRequest::default()
    };
    let started = backend.identity(identity).await.map_err(Refusal::from)?;
    let _ = store.set(&account.username, json!({ "signed_in_at": chrono::Utc::now() })).await;
    let session = json!({
        "token": started.session_token.expose(),
        "expires_at": started.expires_at,
        "user_id": started.user_id,
        "login": account.username,
        "first": started.first,
        "return_to": return_to,
    });
    Ok(Response::json(&session).with_header("cache-control", "no-store"))
}
