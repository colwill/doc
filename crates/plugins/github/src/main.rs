//! GitHub plugin (T27, T39, T68): sign-in through a GitHub OAuth app, a scheduled sync of each
//! organisation's teams and members into the platform and of its repositories into Resource
//! Definitions, delivery data for DORA, workflow runs for CI/CD/CT metrics, archive links and
//! files for other plugins, and pull requests proposing changes made in DOC. Built as `github` for GitHub.com or, with the `ghe` feature, as `ghe` for GitHub
//! Enterprise at a URL of its own.

mod delivery;
mod github;
mod oauth;
mod pipelines;
mod proposals;
mod settings;
mod sync;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Capability, Classification, CustomPermission, Manifest, Plugin, PluginError,
    PluginState, Request, Response, RunInput, RunOutput, Schedule, SignIn, SignInKind,
};
use parking_lot::Mutex;
use serde_json::{Value, json};

use github::GitHub;
use settings::Settings;

#[cfg(not(feature = "ghe"))]
const ID: &str = "github";
#[cfg(feature = "ghe")]
const ID: &str = "ghe";

#[derive(Default)]
struct Sso {
    settings: Option<Result<Settings, String>>,
    github: Option<GitHub>,
    problem: Mutex<Option<String>>,
    tokens: sync::Tokens,
}

impl Sso {
    fn ready(&self) -> Result<(&Settings, &GitHub), String> {
        let settings = match &self.settings {
            Some(Ok(settings)) => settings,
            Some(Err(problem)) => return Err(problem.clone()),
            None => return Err(format!("{ID} is not loaded")),
        };
        let github = self.github.as_ref().ok_or_else(|| format!("{ID} is not loaded"))?;
        Ok((settings, github))
    }

    /// A configuration the platform cannot sign anyone in with puts the plugin in `error`.
    async fn fail(&self, backend: &Backend, problem: String) -> PluginError {
        *self.problem.lock() = Some(problem.clone());
        if let Err(err) = backend.set_state(PluginState::Error, Some(&problem)).await {
            tracing::warn!(%err, "the error could not be reported");
        }
        PluginError::Message(problem)
    }
}

#[async_trait]
impl Plugin for Sso {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        self.github = Some(GitHub::new()?);
        self.settings = Some(Settings::read(backend, ID, cfg!(feature = "ghe")));
        *self.problem.lock() = None;
        match &self.settings {
            Some(Ok(settings)) => tracing::info!(
                version = backend.version(),
                github = %settings.web,
                organisations = ?settings.organisations,
                callback = settings.oauth.as_ref().map(|oauth| oauth.redirect.to_string()),
                sign_in = settings.oauth.is_some(),
                sync = settings.syncs,
                "{ID} loaded"
            ),
            Some(Err(problem)) => {
                tracing::warn!(%problem, "{ID} loaded without a usable OAuth app")
            }
            None => {}
        }
        // Here rather than in the one-shot run, since turning the feature on reloads the plugin in
        // its own process, and only a load core starts is followed by that run.
        if let Some(Ok(settings)) = &self.settings {
            delivery::begin(backend, settings).await;
            pipelines::begin(backend, settings).await;
        }
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    /// After each load, the one-shot check that the OAuth app is accepted; on its schedules, the
    /// sync and the reads of delivery and pipeline data, which carry on in tasks of their own.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        if input.payload["schedule"] == "delivery" {
            let (settings, github) = self.ready().map_err(PluginError::from)?;
            let read = delivery::scheduled(backend, github, settings, &self.tokens, ID).await?;
            return Ok(RunOutput { payload: read });
        }
        if let Some(carried) = input.payload.get("delivery") {
            let (settings, github) = self.ready().map_err(PluginError::from)?;
            let queue = serde_json::from_value(carried["repositories"].clone())
                .map_err(|err| PluginError::Message(format!("an unreadable queue: {err}")))?;
            let whole = carried["whole"].as_bool().unwrap_or_default();
            let read =
                delivery::read(backend, github, settings, &self.tokens, ID, queue, whole).await?;
            return Ok(RunOutput { payload: read });
        }
        if input.payload["schedule"] == "pipelines" {
            let (settings, github) = self.ready().map_err(PluginError::from)?;
            let read = pipelines::scheduled(backend, github, settings, &self.tokens, ID).await?;
            return Ok(RunOutput { payload: read });
        }
        if let Some(carried) = input.payload.get("pipelines") {
            let (settings, github) = self.ready().map_err(PluginError::from)?;
            let queue = serde_json::from_value(carried["repositories"].clone())
                .map_err(|err| PluginError::Message(format!("an unreadable queue: {err}")))?;
            let whole = carried["whole"].as_bool().unwrap_or_default();
            let read =
                pipelines::read(backend, github, settings, &self.tokens, ID, queue, whole).await?;
            return Ok(RunOutput { payload: read });
        }
        if input.payload["schedule"] == "sync" {
            let (settings, github) = self.ready().map_err(PluginError::from)?;
            if let Some(problem) = settings.sync_problem() {
                return Ok(RunOutput { payload: json!({ "synced": false, "why": problem }) });
            }
            let synced = sync::run(backend, github, settings, &self.tokens, ID).await?;
            return Ok(RunOutput { payload: synced });
        }
        let (settings, github) = match self.ready() {
            Ok(ready) => ready,
            Err(problem) => {
                return Err(self.fail(backend, format!("not configured: {problem}")).await);
            }
        };
        github
            .reachable(settings)
            .await
            .map_err(|err| PluginError::Message(format!("GitHub is unreachable: {err}")))?;
        let Some(oauth) = &settings.oauth else {
            tracing::info!(github = %settings.web, "GitHub answers; sign-in is off");
            return Ok(RunOutput {
                payload: json!({ "github": settings.web.as_str(), "app": null }),
            });
        };
        if let Err(problem) = github.check_app(settings, oauth).await {
            return Err(self.fail(backend, problem).await);
        }
        tracing::info!(github = %settings.web, "the OAuth app is accepted and GitHub answers");
        Ok(RunOutput { payload: json!({ "github": settings.web.as_str(), "app": "accepted" }) })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    fn error(&self) -> Option<String> {
        self.problem.lock().clone()
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let (settings, github) = match self.ready() {
            Ok(ready) => ready,
            Err(problem) => return Response::problem(503, "not-configured", &problem),
        };
        match (request.method.as_str(), request.path.as_str(), &settings.oauth) {
            ("GET", "public/oauth/start", Some(app)) => {
                oauth::start(backend, settings, app, &request).await
            }
            ("GET", "public/oauth/callback", Some(app)) => {
                oauth::callback(backend, github, settings, app, &request).await
            }
            ("POST", "internal/link", Some(app)) => {
                oauth::link(backend, settings, app, &request).await
            }
            ("POST", "public/webhooks", _) => delivery::webhook(backend, settings, &request).await,
            // Nothing secret: whether delivery data is on and read, for `dora` to show.
            ("GET", "discovery/delivery", _) => {
                Response::json(&delivery::status(backend, settings).await)
            }
            // The same for workflow runs, for `cicd`.
            ("GET", "discovery/pipelines", _) => {
                Response::json(&pipelines::status(backend, settings).await)
            }
            // Core has checked write access; the sync runs as a background task, like the schedule.
            ("POST", "api/sync", _) => match backend.task(json!({ "schedule": "sync" })).await {
                Ok(task) => {
                    Response::new(202, "application/json", json!({ "task": task }).to_string())
                }
                Err(err) => Response::problem(503, "unavailable", &err.to_string()),
            },
            ("POST", "discovery/archive-links", _) => {
                let asking =
                    backend.caller().and_then(|caller| caller.id.clone()).unwrap_or_default();
                if !settings.archive_plugins.contains(&asking) {
                    let detail = format!("{asking} is not one of the plugins given archive links");
                    return Response::problem(403, "forbidden", &detail);
                }
                let asked: sync::ArchiveRequest = match request.json() {
                    Ok(asked) => asked,
                    Err(err) => return Response::problem(400, "bad-request", &err.to_string()),
                };
                match sync::archive_link(github, settings, &self.tokens, &asked).await {
                    Ok(link) => Response::json(&link),
                    Err((status, detail)) => Response::problem(status, "refused", &detail),
                }
            }
            // A file of a repository, for the same plugins: what they could read from an archive.
            ("POST", "discovery/files", _) => {
                let asking =
                    backend.caller().and_then(|caller| caller.id.clone()).unwrap_or_default();
                if !settings.archive_plugins.contains(&asking) {
                    let detail = format!("{asking} is not one of the plugins given archive links");
                    return Response::problem(403, "forbidden", &detail);
                }
                let asked: proposals::FileRequest = match request.json() {
                    Ok(asked) => asked,
                    Err(err) => return Response::problem(400, "bad-request", &err.to_string()),
                };
                match Box::pin(proposals::file(github, settings, &self.tokens, &asked)).await {
                    Ok(file) => Response::json(&file),
                    Err((status, detail)) => Response::problem(status, "refused", &detail),
                }
            }
            // Core has checked write access; opening a pull request also takes its own permission.
            ("POST", "api/pull-requests", _) => {
                if !backend.allows(proposals::PULL_REQUESTS, false)
                    && !backend.allows(proposals::PULL_REQUESTS, true)
                {
                    let detail =
                        format!("needs plugin:{ID}:pluginuser:{}", proposals::PULL_REQUESTS);
                    return Response::problem(403, "forbidden", &detail);
                }
                let asked: proposals::Proposal = match request.json() {
                    Ok(asked) => asked,
                    Err(err) => return Response::problem(400, "bad-request", &err.to_string()),
                };
                let by = backend
                    .caller()
                    .and_then(|caller| caller.label.clone())
                    .unwrap_or_else(|| "someone".into());
                // Boxed, so the many calls it makes are not all held in this one handler's state.
                match Box::pin(proposals::propose(github, settings, &self.tokens, &asked, &by))
                    .await
                {
                    Ok(pull) => Response::json(&pull),
                    Err((status, detail)) => Response::problem(status, "refused", &detail),
                }
            }
            _ => Response::not_found(),
        }
    }
}

doc_plugin_sdk::main!(
    Sso,
    Manifest {
        id: ID.into(),
        classification: Classification::OneShot,
        // Declared whatever this platform is configured with: what it actually offers follows its
        // settings and features, which an administrator changes in DOC (ADR-0007). The deployment
        // still decides whether the capability is allowed at all, in `[plugins.capabilities]`.
        capabilities: vec![
            Capability::IdentityProvider,
            Capability::PublicRoutes,
            Capability::TeamProvider,
        ],
        public_routes: vec!["oauth/start".into(), "oauth/callback".into(), "webhooks".into()],
        custom_permissions: vec![CustomPermission::user(proposals::PULL_REQUESTS).describes(
            "Opening pull requests that propose changes made in DOC, such as an edited Knowledge \
             Base page",
        )],
        // Whatever it does as someone needs their GitHub account, so they are offered the link.
        linked_accounts: vec![ID.into()],
        // Offered on the sign-in page only once somebody turns the Sign-in feature on.
        sign_in: Some(
            SignIn::new(
                if ID == "ghe" { "GitHub Enterprise" } else { "GitHub" },
                SignInKind::Redirect,
            )
            .of_feature(settings::SIGN_IN),
        ),
        schedules: vec![
            Schedule::new(
                "sync",
                settings::SYNC_SCHEDULE,
                "Brings each allowed organisation's teams and members into the platform, and its repositories into Resource Definitions",
            )
            .of_feature(settings::SYNC)
            .from_setting(settings::SYNC_SCHEDULE_KEY),
            Schedule::new(
                "delivery",
                settings::DELIVERY_SCHEDULE,
                "Reads each chosen repository's deployments and merged pull requests since the last read",
            )
            .of_feature(settings::DELIVERY)
            .from_setting(settings::DELIVERY_SCHEDULE_KEY),
            Schedule::new(
                "pipelines",
                settings::PIPELINE_SCHEDULE,
                "Reads each chosen repository's GitHub Actions workflow runs since the last read",
            )
            .of_feature(settings::PIPELINES)
            .from_setting(settings::PIPELINE_SCHEDULE_KEY),
        ],
        data: pipelines::declared(delivery::declaration()),
        settings: settings::declared(cfg!(feature = "ghe")),
        features: settings::features(),
        ..Manifest::default()
    }
);
