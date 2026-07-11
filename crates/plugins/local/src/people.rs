//! A DOC password for somebody the platform has just added by email address (FEAT-PEOPLE), so they
//! can sign in before their organisation's SSO is set up. Only the platform asks, on
//! `internal/logins`. With an SMTP server set, the one-time password travels as a sign-in link to
//! their address; without one, it goes back to be shown to whoever added them, once.

use std::time::Duration;

use chrono::{Duration as Days, Utc};
use doc_plugin_sdk::{Backend, Request, Response};
use serde::Deserialize;
use serde_json::json;

use crate::accounts::{self, ONE_TIME_DAYS, Refusal, Store};

#[derive(Debug, Deserialize)]
struct Asked {
    username: String,
    email: String,
    #[serde(default)]
    name: Option<String>,
}

/// A name as a first name and a surname: its first word, and the rest.
fn split(name: &str) -> (String, String) {
    let name = name.trim();
    match name.split_once(char::is_whitespace) {
        Some((first, rest)) => (first.to_string(), rest.trim().to_string()),
        None => (name.to_string(), String::new()),
    }
}

/// Where the sign-in link goes: the sign-in page, with the username and code filled in.
fn link(username: &str, code: &str) -> Option<String> {
    let base = std::env::var("DOC_PUBLIC_URL").ok().filter(|url| !url.trim().is_empty())?;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("provider", crate::ID)
        .append_pair("username", username)
        .append_pair("code", code)
        .finish();
    Some(format!("{}/sign-in?{query}", base.trim_end_matches('/')))
}

/// Emails the link, when an SMTP server and an address to send from are set.
async fn email(backend: &Backend, to: &str, name: &str, link: &str) -> Result<(), String> {
    use lettre::message::header::ContentType;
    use lettre::{AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor};
    let settings = backend.settings();
    let url = settings.secret("smtp-url").ok_or("no SMTP server is set")?;
    let from = settings.text("smtp-from");
    if from.trim().is_empty() {
        return Err("no address to send from is set".into());
    }
    let greeting = if name.is_empty() { "Hello".to_string() } else { format!("Hello {name}") };
    let body = format!(
        "{greeting},\n\nYou have been added to DOC. Sign in with this link within {ONE_TIME_DAYS} \
         days, then choose your own password:\n\n{link}\n\nIt works once. If you were not \
         expecting it, you can ignore this email.\n"
    );
    let message = Message::builder()
        .from(from.parse().map_err(|err| format!("`{from}` is not an address: {err}"))?)
        .to(to.parse().map_err(|err| format!("`{to}` is not an address: {err}"))?)
        .subject("Your sign-in to DOC")
        .header(ContentType::TEXT_PLAIN)
        .body(body)
        .map_err(|err| format!("the email could not be made: {err}"))?;
    let transport = AsyncSmtpTransport::<Tokio1Executor>::from_url(url.expose())
        .map_err(|err| format!("the SMTP URL is not one: {err}"))?
        .timeout(Some(Duration::from_secs(30)))
        .build();
    transport.send(message).await.map_err(|err| format!("the email was not sent: {err}"))?;
    Ok(())
}

pub async fn login(backend: &Backend, request: &Request) -> Response {
    if backend.caller().is_none_or(|caller| caller.kind != "platform") {
        return Response::problem(403, "forbidden", "only the platform asks for a login");
    }
    let asked: Asked = match request.json() {
        Ok(asked) => asked,
        Err(err) => return Response::problem(400, "bad-request", &err.to_string()),
    };
    let made = async {
        let username = accounts::username(&asked.username)?;
        let store = Store(backend);
        if store.get(&username).await?.is_some() {
            return Err(Refusal::new(409, format!("there is a DOC account called {username}")));
        }
        let (first_name, surname) = split(asked.name.as_deref().unwrap_or_default());
        let password = accounts::one_time_password()?;
        let expires_at = Utc::now() + Days::days(ONE_TIME_DAYS);
        let profile = [first_name.as_str(), surname.as_str(), asked.email.as_str()];
        store.create(&username, profile, Some((&password, Some(expires_at)))).await?;
        Ok::<_, Refusal>((username, password))
    };
    let (username, password) = match made.await {
        Ok(made) => made,
        Err(refusal) => return Response::problem(refusal.status, "refused", &refusal.detail),
    };
    let _ = backend.audit("account.created", Some(&username), json!({ "by": "people" })).await;
    let sent = match link(&username, &password) {
        Some(link) => {
            email(backend, &asked.email, asked.name.as_deref().unwrap_or_default(), &link).await
        }
        None => Err("DOC_PUBLIC_URL is not set, so there is no link to send".into()),
    };
    match sent {
        Ok(()) => Response::json(&json!({ "emailed": true })),
        Err(why) => {
            tracing::info!(%username, %why, "the one-time password goes to whoever added them");
            Response::json(&json!({ "emailed": false, "password": password, "why": why }))
        }
    }
}
