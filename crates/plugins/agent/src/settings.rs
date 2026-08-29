//! What Agent Smith is configured with: Claude's key and model for the jobs DOC runs, how long a
//! run may go on, the sources `doc_fetch` may read and their credentials, and where agents reach DOC.

use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::{NamedSecrets, Setting, SettingKind, Settings};
use serde_json::json;

pub const API_KEY: &str = "anthropic-api-key";
pub const MODEL: &str = "anthropic-model";
pub const MAX_TURNS: &str = "max-turns";
pub const SOURCES: &str = "sources";
pub const API_URL: &str = "api-url";

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
const DEFAULT_TURNS: i64 = 40;

pub fn declared() -> Vec<Setting> {
    vec![
        Setting::secret(API_KEY, "Claude API key")
            .hinted(
                "An Anthropic API key, for the jobs DOC runs itself. It can come from Secret \
                 Storage. Your own agent needs none: it connects over MCP.",
            )
            .grouped("Claude"),
        Setting::new(MODEL, "Model", SettingKind::Text)
            .defaulting(json!(DEFAULT_MODEL))
            .hinted("Any Claude model the key can use, such as claude-opus-5-5 or claude-sonnet-5.")
            .grouped("Claude"),
        Setting::new(MAX_TURNS, "Most turns a run takes", SettingKind::Number)
            .defaulting(json!(DEFAULT_TURNS))
            .hinted(
                "Each turn is one answer from Claude; a run that reaches this stops and reports.",
            )
            .grouped("Claude"),
        Setting::new(SOURCES, "Sources a job may read", SettingKind::List)
            .hinted(
                "One to a line: an address, and how to sign in there with a credential held by \
                 name below, such as `https://backstage.acme.dev bearer backstage` or \
                 `https://acme.atlassian.net basic confluence` (the credential being user:token). \
                 Nothing else is fetched.",
            )
            .grouped("Sources"),
        Setting::new(API_URL, "Where agents reach DOC's API", SettingKind::Url)
            .hinted(
                "The address your own agent calls, such as https://doc.acme.dev. Empty uses \
                 DOC_API_PUBLIC_URL, or http://localhost:8080.",
            )
            .grouped("Your own agent"),
    ]
}

pub fn named() -> NamedSecrets {
    NamedSecrets {
        label: "Source credential".into(),
        hint: "What a source in the list above signs in with, by the name it gives: a token, or \
               user:token for basic sign-in. Each can come from Secret Storage."
            .into(),
    }
}

/// A source `doc_fetch` may read, and how it signs in there.
#[derive(Clone)]
pub struct Source {
    pub base: url::Url,
    pub auth: Auth,
}

#[derive(Clone)]
pub enum Auth {
    Bearer(Secret<String>),
    Basic(String, Secret<String>),
    None,
}

pub struct Config {
    pub api_key: Option<Secret<String>>,
    pub model: String,
    pub max_turns: usize,
    pub sources: Vec<Source>,
    /// Lines that name a credential nobody has added, so the page can say so.
    pub unmet: Vec<String>,
    pub api_url: String,
}

impl Config {
    pub fn read(settings: &Settings) -> Self {
        let mut sources = Vec::new();
        let mut unmet = Vec::new();
        for line in settings.list(SOURCES) {
            let words: Vec<&str> = line.split_whitespace().collect();
            let Some(base) = words.first().and_then(|base| url::Url::parse(base).ok()) else {
                continue;
            };
            let credential =
                |name: &str| settings.named(name).filter(|value| !value.expose().is_empty());
            let auth = match (words.get(1).copied(), words.get(2).copied()) {
                (Some("bearer"), Some(name)) => match credential(name) {
                    Some(secret) => Auth::Bearer(secret),
                    None => {
                        unmet.push(name.to_string());
                        continue;
                    }
                },
                (Some("basic"), Some(name)) => match credential(name) {
                    Some(secret) => match secret.expose().split_once(':') {
                        Some((user, token)) => {
                            Auth::Basic(user.to_string(), Secret::new(token.to_string()))
                        }
                        None => {
                            unmet.push(name.to_string());
                            continue;
                        }
                    },
                    None => {
                        unmet.push(name.to_string());
                        continue;
                    }
                },
                _ => Auth::None,
            };
            sources.push(Source { base, auth });
        }
        let model = settings.text(MODEL);
        let api_url = match settings.text(API_URL).trim() {
            "" => std::env::var("DOC_API_PUBLIC_URL")
                .unwrap_or_else(|_| "http://localhost:8080".to_string()),
            given => given.to_string(),
        };
        Self {
            api_key: settings.secret(API_KEY).filter(|value| !value.expose().is_empty()),
            model: match model.trim() {
                "" => DEFAULT_MODEL.to_string(),
                given => given.to_string(),
            },
            max_turns: settings
                .integer(MAX_TURNS)
                .filter(|turns| *turns > 0)
                .map_or(DEFAULT_TURNS as usize, |turns| turns.min(200) as usize),
            sources,
            unmet,
            api_url: api_url.trim_end_matches('/').to_string(),
        }
    }
}
