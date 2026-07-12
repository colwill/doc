//! What the plugin is told about its provider (ADR-0007): declared to the platform, shown on the
//! plugin's **Settings** page, and seeded by the variables that used to be its only source —
//! `DOC_OIDC_ISSUER` for the plugin called `oidc`, `DOC_ENTRA_ISSUER` for one called `entra` — so
//! nothing about one deployment's identity provider is written down in the repository and an
//! administrator can still change it without touching the deployment.
//!
//! One thing stays the deployment's alone: `DOC_OIDC_PLUGIN_ID`, which decides *which plugin this
//! process is*. A plugin's own ID cannot come from that plugin's settings, because its settings
//! are the ones kept under that ID.

use std::env::var as env;

use doc_plugin_sdk::{Backend, Feature, Setting, SettingKind};
use serde_json::json;
use url::Url;

/// The scopes asked for unless a deployment says otherwise. `openid` is what makes it OIDC;
/// `profile` and `email` are what the name and email address come from.
const SCOPES: &str = "openid profile email";
const TITLE: &str = "single sign-on";

/// Offered on the sign-in page only once somebody turns this on.
pub const SIGN_IN: &str = "sign-in";

pub const ISSUER: &str = "issuer";
pub const CLIENT_ID: &str = "client-id";
pub const CLIENT_SECRET: &str = "client-secret";
pub const SCOPES_KEY: &str = "scopes";
pub const REDIRECT: &str = "redirect";
pub const TITLE_KEY: &str = "title";

pub struct Settings {
    /// The provider's issuer, from which everything else is discovered. Sign-in is off without it.
    pub issuer: Option<Url>,
    pub client_id: String,
    pub client_secret: String,
    pub scopes: String,
    /// Where the provider sends the person back, which it must be configured to allow.
    pub redirect: Url,
}

/// The plugin's own ID, so one deployment can run this binary once per provider: `oidc` by
/// default, and say `entra` or `okta` for another, each with settings under its own name and each
/// chosen by the organisations that sign in through it. From the environment, and only there.
pub fn id() -> String {
    env("DOC_OIDC_PLUGIN_ID")
        .ok()
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| {
            !value.is_empty()
                && value.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
        .unwrap_or_else(|| "oidc".to_string())
}

/// What this instance's settings are named in a deployment: `DOC_ENTRA_ISSUER` for `entra`.
pub fn variable(key: &str) -> String {
    format!("DOC_{}_{}", id(), key).to_ascii_uppercase().replace('-', "_")
}

/// The fields the platform renders this plugin's Settings page from.
pub fn declared() -> Vec<Setting> {
    vec![
        Setting::new(ISSUER, "Issuer", SettingKind::Url)
            .hinted(
                "The provider's issuer URL. Everything else — the authorization and token \
                 endpoints, the keys, the userinfo endpoint — is discovered from it.",
            )
            .grouped("Provider"),
        Setting::text(CLIENT_ID, "Client ID").grouped("Provider"),
        Setting::secret(CLIENT_SECRET, "Client secret").grouped("Provider"),
        Setting::text(TITLE_KEY, "What to call it")
            .hinted("What the sign-in page offers, such as \"Acme single sign-on\".")
            .defaulting(json!(TITLE))
            .grouped("Sign-in"),
        Setting::text(SCOPES_KEY, "Scopes")
            .hinted("What DOC asks the provider for.")
            .defaulting(json!(SCOPES))
            .grouped("Sign-in"),
        Setting::new(REDIRECT, "Callback URL", SettingKind::Url)
            .hinted(
                "Where the provider sends people back, which it must be configured to allow. \
                 Empty uses this platform's own address.",
            )
            .grouped("Sign-in"),
    ]
}

pub fn features() -> Vec<Feature> {
    vec![Feature::new(
        SIGN_IN,
        "Sign in with this provider",
        "Offers it on the sign-in page, for the organisations that choose it. It needs an \
         issuer, a client ID and a client secret.",
    )]
}

impl Settings {
    /// Read from what core hands the plugin: what an administrator set, or the deployment's
    /// variable where nothing is set, or the declared default.
    pub fn read(backend: &Backend) -> Self {
        let settings = backend.settings();
        let public = env("DOC_PUBLIC_URL")
            .ok()
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| "http://127.0.0.1:8081".into());
        let redirect = settings
            .some_text(REDIRECT)
            .or_else(|| Some(format!("{}/auth/{}/callback", public.trim_end_matches('/'), id())))
            .and_then(|url| Url::parse(&url).ok())
            .unwrap_or_else(|| {
                Url::parse("http://127.0.0.1:8081/auth/oidc/callback").expect("a known URL")
            });
        Self {
            issuer: settings.some_text(ISSUER).and_then(|issuer| {
                // Every discovery and token URL hangs off it, so it ends in a slash to join onto.
                let issuer = format!("{}/", issuer.trim_end_matches('/'));
                Url::parse(&issuer)
                    .map_err(|err| tracing::warn!(%err, setting = %variable(ISSUER), "the issuer is not a URL"))
                    .ok()
            }),
            client_id: settings.some_text(CLIENT_ID).unwrap_or_default(),
            client_secret: settings
                .secret(CLIENT_SECRET)
                .map(|secret| secret.expose().clone())
                .unwrap_or_default(),
            scopes: settings.some_text(SCOPES_KEY).unwrap_or_else(|| SCOPES.to_string()),
            redirect,
        }
    }

    /// Everything a sign-in needs; `None` when this deployment has no provider.
    pub fn provider(&self) -> Option<(&Url, &str, &str)> {
        let issuer = self.issuer.as_ref()?;
        match self.client_id.is_empty() {
            true => None,
            false => Some((issuer, &self.client_id, &self.client_secret)),
        }
    }

    /// What is missing before anybody can sign in with it, in words for the plugin's page.
    pub fn sign_in_problem(&self) -> Option<String> {
        match (self.issuer.is_some(), self.client_id.is_empty(), self.client_secret.is_empty()) {
            (false, _, _) => Some(format!("no issuer is set ({})", variable(ISSUER))),
            (true, true, _) => Some(format!("no client ID is set ({})", variable(CLIENT_ID))),
            (true, false, true) => {
                Some(format!("no client secret is set ({})", variable(CLIENT_SECRET)))
            }
            (true, false, false) => None,
        }
    }
}
