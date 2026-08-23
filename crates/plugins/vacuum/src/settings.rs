//! What the Data Vacuum is configured with: Claude's API key for runs DOC drives itself, where
//! those runs may read from and with what credentials, where an administrator's own agent reaches
//! DOC, and how long the token it is given lasts.

use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::{Setting, SettingKind, Settings};
use serde_json::json;

pub const API_KEY: &str = "anthropic-api-key";
pub const MODEL: &str = "anthropic-model";
pub const MAX_TURNS: &str = "max-turns";
pub const CONFLUENCE_URL: &str = "confluence-url";
pub const CONFLUENCE_EMAIL: &str = "confluence-email";
pub const CONFLUENCE_TOKEN: &str = "confluence-token";
pub const JIRA_URL: &str = "jira-url";
pub const JIRA_EMAIL: &str = "jira-email";
pub const JIRA_TOKEN: &str = "jira-token";
pub const BACKSTAGE_URL: &str = "backstage-url";
pub const BACKSTAGE_TOKEN: &str = "backstage-token";
pub const HOSTS: &str = "allowed-hosts";
pub const API_URL: &str = "api-url";
pub const TOKEN_MINUTES: &str = "token-minutes";

pub const DEFAULT_MODEL: &str = "claude-opus-5-5";
const DEFAULT_TURNS: i64 = 80;
const DEFAULT_MINUTES: i64 = 60;

pub fn declared() -> Vec<Setting> {
    vec![
        Setting::secret(API_KEY, "Claude API key")
            .hinted(
                "An Anthropic API key, for runs DOC drives itself. Without one, runs are for your \
                 own agent: DOC gives it instructions and a short-lived token instead.",
            )
            .grouped("Claude"),
        Setting::new(MODEL, "Model", SettingKind::Text)
            .defaulting(json!(DEFAULT_MODEL))
            .hinted("Any Claude model the key can use, such as claude-opus-5-5 or claude-sonnet-5.")
            .grouped("Claude"),
        Setting::new(MAX_TURNS, "Most turns a run takes", SettingKind::Number)
            .defaulting(json!(DEFAULT_TURNS))
            .hinted(
                "Each turn is one answer from Claude; a run that reaches this stops where it is.",
            )
            .grouped("Claude"),
        Setting::new(CONFLUENCE_URL, "Confluence", SettingKind::Url)
            .hinted("Such as https://acme.atlassian.net/wiki, for runs DOC drives itself.")
            .grouped("Sources"),
        Setting::new(CONFLUENCE_EMAIL, "Confluence account's email", SettingKind::Text)
            .grouped("Sources"),
        Setting::secret(CONFLUENCE_TOKEN, "Confluence API token").grouped("Sources"),
        Setting::new(JIRA_URL, "Jira", SettingKind::Url)
            .hinted("Such as https://acme.atlassian.net.")
            .grouped("Sources"),
        Setting::new(JIRA_EMAIL, "Jira account's email", SettingKind::Text).grouped("Sources"),
        Setting::secret(JIRA_TOKEN, "Jira API token").grouped("Sources"),
        Setting::new(BACKSTAGE_URL, "Backstage", SettingKind::Url)
            .hinted("Its backend, such as https://backstage.acme.dev.")
            .grouped("Sources"),
        Setting::secret(BACKSTAGE_TOKEN, "Backstage token")
            .hinted("A static token its backend accepts, sent as a bearer token.")
            .grouped("Sources"),
        Setting::new(HOSTS, "Other hosts Claude may read", SettingKind::List)
            .hinted(
                "Where Markdown and MkDocs sites are, such as raw.githubusercontent.com or \
                 docs.acme.dev, one to a line. Nothing else is fetched.",
            )
            .grouped("Sources"),
        Setting::new(API_URL, "Where agents reach DOC's API", SettingKind::Url)
            .hinted(
                "The address your own agent calls, such as https://doc.acme.dev. Empty uses \
                 DOC_API_PUBLIC_URL, or http://localhost:8080.",
            )
            .grouped("Your own agent"),
        Setting::new(TOKEN_MINUTES, "Its token lasts, in minutes", SettingKind::Number)
            .defaulting(json!(DEFAULT_MINUTES))
            .hinted(
                "1 to 480. The token reaches this plugin, and reads the Knowledge Base, \
                     Watercooler and the Catalogue, and nothing else.",
            )
            .grouped("Your own agent"),
    ]
}

/// A source DOC reads for Claude, and how it signs in there.
#[derive(Clone)]
pub struct Connection {
    pub name: &'static str,
    pub base: url::Url,
    pub auth: Auth,
}

#[derive(Clone)]
pub enum Auth {
    Basic { user: String, secret: Secret<String> },
    Bearer(Secret<String>),
    None,
}

pub struct Config {
    pub api_key: Option<Secret<String>>,
    pub model: String,
    pub max_turns: usize,
    pub connections: Vec<Connection>,
    pub hosts: Vec<String>,
    pub api_url: String,
    pub token_minutes: i64,
}

fn parsed(settings: &Settings, key: &str) -> Option<url::Url> {
    let text = settings.text(key);
    let text = text.trim().trim_end_matches('/');
    if text.is_empty() {
        return None;
    }
    url::Url::parse(&format!("{text}/")).ok()
}

impl Config {
    pub fn read(settings: &Settings) -> Self {
        let secret = |key: &str| settings.secret(key).filter(|value| !value.expose().is_empty());
        let basic = |email: &str, token: &str| match (settings.text(email), secret(token)) {
            (user, Some(secret)) if !user.trim().is_empty() => {
                Auth::Basic { user: user.trim().to_string(), secret }
            }
            _ => Auth::None,
        };
        let mut connections = Vec::new();
        if let Some(base) = parsed(settings, CONFLUENCE_URL) {
            connections.push(Connection {
                name: "confluence",
                base,
                auth: basic(CONFLUENCE_EMAIL, CONFLUENCE_TOKEN),
            });
        }
        if let Some(base) = parsed(settings, JIRA_URL) {
            connections.push(Connection {
                name: "jira",
                base,
                auth: basic(JIRA_EMAIL, JIRA_TOKEN),
            });
        }
        if let Some(base) = parsed(settings, BACKSTAGE_URL) {
            let auth = secret(BACKSTAGE_TOKEN).map_or(Auth::None, Auth::Bearer);
            connections.push(Connection { name: "backstage", base, auth });
        }
        let model = settings.text(MODEL);
        let api_url = settings.text(API_URL);
        let api_url = match api_url.trim() {
            "" => std::env::var("DOC_API_PUBLIC_URL")
                .unwrap_or_else(|_| "http://localhost:8080".to_string()),
            given => given.to_string(),
        };
        Self {
            api_key: secret(API_KEY),
            model: match model.trim() {
                "" => DEFAULT_MODEL.to_string(),
                given => given.to_string(),
            },
            max_turns: settings
                .integer(MAX_TURNS)
                .filter(|turns| *turns > 0)
                .map_or(DEFAULT_TURNS as usize, |turns| turns.min(500) as usize),
            connections,
            hosts: settings
                .list(HOSTS)
                .into_iter()
                .map(|host| host.trim().trim_end_matches('/').to_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
            api_url: api_url.trim_end_matches('/').to_string(),
            token_minutes: settings.integer(TOKEN_MINUTES).unwrap_or(DEFAULT_MINUTES).clamp(1, 480),
        }
    }
}
