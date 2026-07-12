//! OIDC plugin (T66): signs people in through any OpenID Connect provider — Entra ID, Google,
//! Keycloak, Okta — and reports what its ID token claims about them. Everything about the provider
//! is discovered from its issuer, so a deployment configures an issuer, a client and a secret and
//! nothing else. Without an issuer the plugin runs with sign-in off and offers nothing.

mod provider;
mod settings;
mod sign_in;

use async_trait::async_trait;
use doc_plugin_sdk::protocol::calls::DeprovisionRequest;
use doc_plugin_sdk::{
    Backend, Capability, Classification, Manifest, Plugin, PluginError, PluginState, Request,
    Response, RunInput, RunOutput, SignIn, SignInKind,
};
use serde_json::Value;

use provider::Provider;
use settings::Settings;

#[derive(Default)]
struct Oidc {
    settings: Option<Settings>,
    provider: Option<Provider>,
}

impl Oidc {
    fn ready(&self) -> Result<(&Settings, &Provider), Response> {
        match (&self.settings, &self.provider) {
            (Some(settings), Some(provider)) => Ok((settings, provider)),
            _ => Err(Response::problem(503, "not-configured", "the plugin is not loaded")),
        }
    }
}

#[async_trait]
impl Plugin for Oidc {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let settings = Settings::read(backend);
        let provider = Provider::new();
        match settings.provider() {
            None => {
                let why =
                    settings.sign_in_problem().unwrap_or_else(|| "it is not configured".into());
                tracing::info!(%why, "sign-in through this provider is off");
            }
            Some((issuer, _, secret)) => {
                // The provider is asked about itself at load, so a wrong issuer is a plain error
                // here rather than a puzzle at someone's first sign-in.
                match provider.discovery(issuer).await {
                    Ok(discovery) => tracing::info!(
                        issuer = %discovery.issuer,
                        "the identity provider answered; people can sign in through it"
                    ),
                    Err(err) => {
                        let problem = format!("{issuer} could not be asked about itself: {err}");
                        let _ = backend.set_state(PluginState::Error, Some(&problem)).await;
                        return Err(PluginError::Message(problem));
                    }
                }
                if secret.is_empty() {
                    tracing::warn!(
                        "no client secret is set; the provider must allow a public client"
                    );
                }
            }
        }
        self.settings = Some(settings);
        self.provider = Some(provider);
        // Which instance this is, since a platform runs this binary once per provider and each
        // reads its own settings.
        tracing::info!(
            version = backend.version(),
            id = %settings::id(),
            settings = %format!("DOC_{}_*", settings::id().to_ascii_uppercase().replace('-', "_")),
            signs_in = backend.feature(settings::SIGN_IN),
            "single sign-on loaded"
        );
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, _backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        Err(PluginError::from("single sign-on has nothing to run"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let (settings, provider) = match self.ready() {
            Ok(ready) => ready,
            Err(response) => return response,
        };
        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "public/oauth/start") => {
                sign_in::start(backend, provider, settings, &request).await
            }
            ("GET", "public/oauth/callback") => {
                sign_in::callback(backend, provider, settings, &request).await
            }
            ("POST", "internal/link") => sign_in::link(backend, provider, settings, &request).await,
            // Somebody has gone from the directory. A provider that can call out — Entra's
            // and Okta's provisioning webhooks, a SCIM bridge, a nightly script — tells the
            // platform here, and the offboarding rules decide what happens to them (T67).
            ("POST", "api/deprovision") => deprovision(backend, &request).await,
            _ => Response::not_found(),
        }
    }
}

/// Takes the provider's own ID for the person, since that is what their account here is known by.
async fn deprovision(backend: &Backend, request: &Request) -> Response {
    #[derive(serde::Deserialize)]
    struct Gone {
        external_id: String,
        #[serde(default)]
        reason: Option<String>,
    }
    let asked: Gone = match request.json() {
        Ok(asked) => asked,
        Err(err) => return Response::problem(400, "bad-request", &err.to_string()),
    };
    if asked.external_id.trim().is_empty() {
        return Response::problem(400, "bad-request", "say whose account is gone");
    }
    let told = DeprovisionRequest {
        external_id: asked.external_id.trim().to_string(),
        reason: asked.reason,
    };
    match backend.deprovision(told).await {
        Ok(answer) => {
            match answer.user_id {
                Some(user) => tracing::info!(%user, "the platform was told somebody has left"),
                None => tracing::info!("somebody left whom this platform never knew"),
            }
            Response::json(&answer)
        }
        Err(err) => Response::problem(503, "unavailable", &err.to_string()),
    }
}

doc_plugin_sdk::main!(
    Oidc,
    Manifest {
        id: settings::id(),
        classification: Classification::Synchronous,
        // Declared whatever this instance is configured with: whether it is offered at all
        // follows its Sign-in feature, and what it is called follows its `title` setting
        // (ADR-0007). The deployment still allows the capability in `[plugins.capabilities]`.
        capabilities: vec![Capability::IdentityProvider, Capability::PublicRoutes],
        public_routes: vec!["oauth/start".into(), "oauth/callback".into()],
        sign_in: Some(
            SignIn::new("single sign-on", SignInKind::Redirect)
                .of_feature(settings::SIGN_IN)
                .from_setting(settings::TITLE_KEY),
        ),
        settings: settings::declared(),
        features: settings::features(),
        ..Manifest::default()
    }
);
