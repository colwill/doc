//! How Jira is reached and what is read from it (ADR-0007 §3). Jira Cloud signs in with an
//! account's email address and an API token; Data Center and Server with a personal access token.
//! Both are secret settings, and only this plugin is ever given them.

use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::{Feature, Setting, SettingKind, Settings};
use serde_json::json;

pub const RELEASES: &str = "release-data";

pub const BASE_URL: &str = "base-url";
pub const EMAIL: &str = "email";
pub const API_TOKEN: &str = "api-token";
pub const TOKEN: &str = "token";
pub const PROJECTS: &str = "projects";
pub const DAYS: &str = "release-days";
pub const RELEASE_SCHEDULE: &str = "release-schedule";
pub const READ: &str = "23 * * * *";

pub fn declared(dc: bool) -> Vec<Setting> {
    let mut settings = vec![
        Setting::new(BASE_URL, "Jira URL", SettingKind::Url)
            .required()
            .hinted(match dc {
                true => {
                    "Where your Jira Data Center or Server is, such as https://jira.acme.example."
                }
                false => "Your Jira Cloud site, such as https://acme.atlassian.net.",
            })
            .grouped("Connection"),
    ];
    match dc {
        true => settings.push(
            Setting::secret(TOKEN, "Personal access token")
                .required()
                .hinted(
                    "Made under Profile → Personal Access Tokens by an account that can browse the \
                     projects to read. DOC only reads.",
                )
                .grouped("Connection"),
        ),
        false => {
            settings.push(
                Setting::new(EMAIL, "Account email address", SettingKind::Text)
                    .required()
                    .hinted("The Atlassian account the API token belongs to.")
                    .grouped("Connection"),
            );
            settings.push(
                Setting::secret(API_TOKEN, "API token")
                    .required()
                    .hinted(
                        "Made at id.atlassian.com under Security → API tokens, by an account that \
                         can browse the projects to read. DOC only reads.",
                    )
                    .grouped("Connection"),
            );
        }
    }
    settings.extend([
        Setting::new(PROJECTS, "Projects", SettingKind::List)
            .defaulting(json!([]))
            .hinted(
                "Project keys, such as PAY. Empty reads every project the account can see, up to \
                 200.",
            )
            .grouped("Release data")
            .of_feature(RELEASES),
        Setting::new(DAYS, "Released versions from the last, in days", SettingKind::Number)
            .defaulting(json!(365))
            .between(1.0, 3_650.0)
            .hinted(
                "Every unreleased version is read, and released ones from this far back, so the \
                 roadmap can show what shipped.",
            )
            .grouped("Release data")
            .of_feature(RELEASES),
        Setting::new(RELEASE_SCHEDULE, "How often to read releases", SettingKind::Cron)
            .defaulting(json!(READ))
            .hinted(
                "A cron expression in UTC. Each read asks only for issues changed since the one \
                 before.",
            )
            .grouped("Release data")
            .of_feature(RELEASES),
    ]);
    settings
}

pub fn features() -> Vec<Feature> {
    vec![Feature::new(
        RELEASES,
        "Release data",
        "Reads each project's releases — its fix versions, with their start and release dates — \
         and the issues in them, for the delivery roadmap.",
    )]
}

/// How the plugin signs in to Jira.
#[derive(Clone)]
pub enum Auth {
    /// Cloud: an account's email address and API token.
    Basic { email: String, token: Secret<String> },
    /// Data Center and Server: a personal access token.
    Bearer(Secret<String>),
}

/// What the settings come to.
#[derive(Clone)]
pub struct Config {
    pub base: Option<String>,
    pub auth: Option<Auth>,
    pub projects: Vec<String>,
    pub days: i64,
    pub on: bool,
}

impl Config {
    pub fn read(settings: &Settings, dc: bool) -> Self {
        let base = settings
            .some_text(BASE_URL)
            .map(|base| base.trim().trim_end_matches('/').to_string())
            .filter(|base| !base.is_empty());
        let auth = match dc {
            true => settings.secret(TOKEN).map(Auth::Bearer),
            false => match (settings.some_text(EMAIL), settings.secret(API_TOKEN)) {
                (Some(email), Some(token)) => {
                    Some(Auth::Basic { email: email.trim().into(), token })
                }
                _ => None,
            },
        };
        let mut projects: Vec<String> = settings
            .list(PROJECTS)
            .into_iter()
            .map(|key| key.trim().to_ascii_uppercase())
            .filter(|key| !key.is_empty())
            .collect();
        projects.sort();
        projects.dedup();
        Self {
            base,
            auth,
            projects,
            days: settings.integer(DAYS).unwrap_or(365).clamp(1, 3_650),
            on: settings.feature(RELEASES),
        }
    }

    /// Why releases cannot be read, where they cannot.
    pub fn problem(&self) -> Option<String> {
        match (&self.base, &self.auth) {
            (None, _) => Some("no Jira URL is set".into()),
            (_, None) => Some("no credentials for Jira are set".into()),
            _ => None,
        }
    }
}
