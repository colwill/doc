//! DOC accounts (T65): the default identity plugin, keeping usernames and passwords in DOC so that
//! it can be tried without a company directory. An admin adds an account and hands its person a
//! one-time password; they sign in with it once and choose their own (ADR-0005).
//!
//! The first time it loads with no accounts, it takes over the accounts people signed in with
//! before (the development accounts in configuration it replaces), and makes the first sign-in
//! the bootstrap job wrote to `first-sign-in` in the secrets volume.

mod accounts;
mod people;
mod sign_in;
mod ui;

use std::path::PathBuf;

use async_trait::async_trait;
use doc_plugin_sdk::protocol::calls::UserRequest;
use doc_plugin_sdk::{
    Backend, Capability, Classification, Manifest, Plugin, PluginError, Query, Request, Response,
    RunInput, RunOutput, Setting, SignIn, SignInKind,
};
use serde_json::{Map, Value};

use crate::accounts::Store;

pub(crate) const ID: &str = "local";
const FIRST_SIGN_IN: &str = "first-sign-in";

#[derive(Default)]
struct Local;

#[async_trait]
impl Plugin for Local {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        if let Err(refusal) = first_start(backend).await {
            tracing::warn!(detail = %refusal.detail, "the first accounts could not be made");
        }
        tracing::info!(version = backend.version(), "DOC accounts loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, _backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        Err(PluginError::from("DOC accounts has nothing to run"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let path = request.path.trim_end_matches('/').to_string();
        let segments: Vec<&str> = path.split('/').collect();
        match (request.method.as_str(), segments.as_slice()) {
            ("POST", ["public", "sign-in"]) => sign_in::sign_in(backend, &request).await,
            ("POST", ["public", "password"]) => sign_in::choose(backend, &request).await,
            ("POST", ["internal", "logins"]) => people::login(backend, &request).await,
            (_, ["ui", route @ ..]) => ui::handle(backend, &request, route).await,
            _ => Response::not_found(),
        }
    }
}

/// With no accounts yet: an account, with no password, for everyone who signed in with the
/// configuration's development accounts, whose records they keep; then the first sign-in.
async fn first_start(backend: &Backend) -> Result<(), accounts::Refusal> {
    let store = Store(backend);
    if store.any().await? {
        return Ok(());
    }
    let before: Vec<Map<String, Value>> = backend
        .query_all(Query::new("core.identities").filter(serde_json::json!({ "provider": ID })))
        .await
        .map_err(|err| accounts::Refusal::unavailable(&err))?;
    for identity in &before {
        let Some(username) = identity.get("external_id").and_then(Value::as_str) else { continue };
        if accounts::username(username).is_err() {
            continue;
        }
        let text = |field: &str| identity.get(field).and_then(Value::as_str).unwrap_or_default();
        let profile = [text("first_name"), text("surname"), text("email")];
        if let Err(refusal) = store.create(username, profile, None).await {
            tracing::warn!(username, detail = %refusal.detail, "an earlier account was left out");
        }
    }
    if !before.is_empty() {
        tracing::info!(
            accounts = before.len(),
            "DOC accounts took over the accounts people signed in with before; give each a \
             one-time password to use them"
        );
    }
    let Some((username, password)) = first_sign_in() else {
        tracing::info!("no first sign-in was written to the secrets volume");
        return Ok(());
    };
    let username = accounts::username(&username)?;
    let account = match store.get(&username).await? {
        Some(_) => store.give_one_time(&username, &password, None).await?,
        None => Some(store.create(&username, ["", "", ""], Some((&password, None))).await?),
    };
    if account.is_some() {
        let described = UserRequest {
            provider: ID.into(),
            external_id: username.clone(),
            login: username.clone(),
            ..UserRequest::default()
        };
        if let Err(err) = backend.provide_user(described).await {
            tracing::warn!(%err, "DOC has no user for the first sign-in until it is used");
        }
        tracing::warn!(
            %username,
            "the first sign-in is ready: its one-time password is in `{FIRST_SIGN_IN}` in the \
             secrets volume"
        );
    }
    Ok(())
}

/// The username and one-time password the bootstrap job wrote, if it did.
fn first_sign_in() -> Option<(String, String)> {
    let dir =
        std::env::var("DOC_SECRETS_DIR").map_or_else(|_| PathBuf::from("/secrets"), PathBuf::from);
    let text = std::fs::read_to_string(dir.join(FIRST_SIGN_IN)).ok()?;
    let value = |name: &str| {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == name).then(|| value.trim().trim_matches('"').to_string())
        })
    };
    Some((value("username")?, value("password")?))
}

doc_plugin_sdk::main!(
    Local,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        capabilities: vec![Capability::IdentityProvider, Capability::PublicRoutes],
        public_routes: vec!["sign-in".into(), "password".into()],
        // Changing your own password is yours to do, whatever else you may write.
        read_routes: vec!["ui/password".into()],
        // People are added on the platform's People page (FEAT-PEOPLE); this keeps their DOC
        // passwords, which each person's page links to.
        settings: vec![
            Setting::secret("smtp-url", "SMTP URL")
                .hinted(
                    "Where sign-in links are emailed from, such as \
                     smtps://user:password@smtp.example:465. Without one, whoever adds somebody \
                     is shown their one-time password to hand over."
                )
                .grouped("Email"),
            Setting::text("smtp-from", "Send email from")
                .hinted("The address sign-in links come from, such as doc@acme.com.")
                .grouped("Email"),
        ],
        sign_in: Some(SignIn::new("DOC account", SignInKind::Password)),
        data: accounts::declaration(),
        ..Manifest::default()
    }
);
