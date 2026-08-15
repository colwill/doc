//! Jira (ADR-0007 §3): each project's releases — its fix versions and their dates — and the issues
//! in them, read on a schedule and exported to the delivery roadmap. Built as `jira` for Jira
//! Cloud or, with the `dc` feature, as `jira-dc` for Jira Data Center and Server; what differs is
//! kept behind one client. Incidents for DORA are the rest of the ADR, not yet built.

mod jira;
mod releases;
mod settings;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, Manifest, Plugin, PluginError, Request, Response, RunInput, RunOutput,
    Schedule, Settings, SettingsVerdict,
};
use serde_json::{Value, json};

use settings::{API_TOKEN, BASE_URL, Config, RELEASES, TOKEN};

#[cfg(not(feature = "dc"))]
pub const ID: &str = "jira";
#[cfg(feature = "dc")]
pub const ID: &str = "jira-dc";
const DC: bool = cfg!(feature = "dc");

#[derive(Default)]
struct JiraPlugin;

#[async_trait]
impl Plugin for JiraPlugin {
    /// With release data on and never read, the first read is queued now rather than at the
    /// schedule, since turning the feature on reloads the plugin.
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let config = Config::read(&backend.settings(), DC);
        tracing::info!(
            version = backend.version(),
            releases = config.on,
            configured = config.problem().is_none(),
            "{ID} loaded"
        );
        releases::begin(backend, &config).await;
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let config = Config::read(&backend.settings(), DC);
        let payload = &input.payload;
        let asked = match (payload["schedule"].as_str(), payload.get("releases")) {
            (Some("releases"), _) => Some(None),
            (None, Some(releases)) => Some(releases["after"].as_str().map(str::to_string)),
            _ => None,
        };
        let Some(after) = asked else {
            return Err(PluginError::from("jira runs only its own schedule"));
        };
        let done = match config.on {
            true => releases::read(backend, &config, DC, after).await?,
            false => json!({ "read": false, "why": "Release data is off" }),
        };
        Ok(RunOutput { payload: done })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let config = Config::read(&backend.settings(), DC);
        match (request.method.as_str(), request.path.trim_end_matches('/')) {
            // Nothing secret: whether release data is on and read, for the roadmap to show.
            ("GET", "discovery/releases") => {
                Response::json(&releases::status(backend, &config).await)
            }
            // Core has checked write access; the read runs as a background task, like the schedule.
            ("POST", "api/releases/read") => match backend.task(json!({ "releases": {} })).await {
                Ok(task) => {
                    Response::new(202, "application/json", json!({ "task": task }).to_string())
                }
                Err(err) => Response::problem(503, "unavailable", &err.to_string()),
            },
            _ => Response::not_found(),
        }
    }

    /// Tries the Jira URL and credentials before they are stored, so a wrong one lands on its field.
    async fn settings_check(&self, _backend: &Backend, proposed: &Settings) -> SettingsVerdict {
        let config = Config::read(proposed, DC);
        if config.problem().is_some() {
            return SettingsVerdict::ok();
        }
        let credential = if DC { TOKEN } else { API_TOKEN };
        let jira = match jira::Jira::new(&config, DC) {
            Ok(jira) => jira,
            Err(problem) => return SettingsVerdict::wrong(BASE_URL, &problem),
        };
        match jira.myself().await {
            Ok(who) => SettingsVerdict::saying(&format!("Jira knows these credentials as {who}")),
            Err(refused) if matches!(refused.status, Some(401 | 403)) => {
                SettingsVerdict::wrong(credential, &refused.detail)
            }
            Err(refused) => SettingsVerdict::wrong(BASE_URL, &refused.detail),
        }
    }
}

doc_plugin_sdk::main!(
    JiraPlugin,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        schedules: vec![
            Schedule::new(
                "releases",
                settings::READ,
                "Reads each project's releases and the issues changed in them",
            )
            .of_feature(RELEASES)
            .from_setting(settings::RELEASE_SCHEDULE),
        ],
        settings: settings::declared(DC),
        features: settings::features(),
        data: releases::declaration(),
        ..Manifest::default()
    }
);
