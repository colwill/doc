//! End of life: what each service in the Catalogue runs, found in its repositories or named in its
//! metadata, and where each release stands, from DOC's own copy of endoflife.date — which teams can
//! browse, and read from their tools in endoflife.date's format.

mod api;
mod faux;
mod lifecycle;
mod manifests;
mod mcp;
mod mirror;
mod packages;
mod repositories;
mod scope;
mod settings;
mod store;
mod timeline;
mod ui;
mod view;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, DashboardItem, Event, Insight, Manifest, Nav, Plugin, PluginError,
    Request, ResourcePanel, Response, RunInput, RunOutput, Schedule,
};
use serde_json::{Value, json};

use settings::{LIFECYCLE, REPOSITORIES};

pub const ID: &str = "eol";
/// An administrator let this plugin have what it asked for: archive links, or the Catalogue.
const ACCESS_APPROVED: &str = "platform.plugin.eol.access.approved";

#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub detail: String,
}

impl Refusal {
    pub fn bad(detail: impl Into<String>) -> Self {
        Self { status: 400, detail: detail.into() }
    }

    pub fn forbidden(detail: impl Into<String>) -> Self {
        Self { status: 403, detail: detail.into() }
    }

    pub fn missing(detail: impl Into<String>) -> Self {
        Self { status: 404, detail: detail.into() }
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self { status: 503, detail: detail.into() }
    }

    pub fn response(&self) -> Response {
        let kind = match self.status {
            400 => "bad-request",
            403 => "forbidden",
            404 => "not-found",
            _ => "unavailable",
        };
        Response::problem(self.status, kind, &self.detail)
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        match err {
            PluginError::Forbidden(permission) => {
                Self::forbidden(format!("that needs {permission}:rw"))
            }
            PluginError::Message(detail) => Self::bad(detail),
            err => match err.problem() {
                Some((status, _)) if status < 500 => Self { status, detail: err.detail() },
                _ => Self::unavailable(err.to_string()),
            },
        }
    }
}

/// A parameter of a query string.
pub fn parameter(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

#[derive(Default)]
struct Eol {
    /// Whether a read is already waiting for the Catalogue to stop changing.
    settling: repositories::Settling,
}

#[async_trait]
impl Plugin for Eol {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        lifecycle::client()?;
        tracing::info!(
            version = backend.version(),
            on = backend.feature(LIFECYCLE),
            repositories = backend.feature(REPOSITORIES),
            "eol loaded"
        );
        // The copy of endoflife.date was never read, failed, is stale or was read from elsewhere.
        if backend.feature(LIFECYCLE) && mirror::due_at_load(backend).await {
            backend.task(json!({ "read": "copy" })).await?;
            tracing::info!("endoflife.date is read now: never read, failed, stale or moved");
        }
        // Nothing has been read yet, a repository's last read failed, or runtimes have just been
        // turned on: it is read now rather than overnight.
        if backend.feature(LIFECYCLE) && repositories::due_at_load(backend).await {
            backend.task(json!({ "read": "repositories", "from": 0 })).await?;
            tracing::info!(
                "repositories are read now: never read, a read failed, or runtimes came on"
            );
        }
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let payload = &input.payload;
        let done = match (payload["schedule"].as_str(), payload["read"].as_str()) {
            (Some("refresh"), _) if backend.feature(LIFECYCLE) => {
                lifecycle::refresh_all(backend).await?
            }
            (Some("refresh"), _) => json!({ "read": 0, "why": "End-of-life data is off" }),
            (_, Some("copy")) if backend.feature(LIFECYCLE) => mirror::read(backend).await?,
            (_, Some("copy")) => json!({ "read": 0, "why": "End-of-life data is off" }),
            // Reading repositories takes longer than one attempt is given, so each attempt does
            // what it can and queues the next with where it got to.
            (Some("repositories"), _) | (_, Some("repositories")) => {
                let from = payload["from"].as_u64().unwrap_or_default();
                repositories::read_all(backend, usize::try_from(from).unwrap_or_default()).await?
            }
            _ => return Err(PluginError::from("eol runs only its own schedule")),
        };
        Ok(RunOutput { payload: done })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    /// Repository Insights listed a repository's packages: what its services run is matched now,
    /// not at the next run. The Catalogue changed a connection: every repository is read again once
    /// it has stopped changing, so connecting a service to its repository shows in minutes.
    async fn on_event(&self, backend: &Backend, event: Event) -> Result<(), PluginError> {
        if !backend.feature(LIFECYCLE) {
            return Ok(());
        }
        // A source now gives this plugin archive links, or the Catalogue lets it read: what
        // failed for want of that is read again now.
        if event.topic == ACCESS_APPROVED {
            backend.task(json!({ "read": "repositories", "from": 0 })).await?;
            tracing::info!(plugin = %event.payload["plugin"], "access was approved, so repositories are read");
            return Ok(());
        }
        if event.topic == repositories::CATALOGUE_CHANGED {
            if repositories::moves_connections(&event.payload) {
                self.settling.after_the_change(backend);
            }
            return Ok(());
        }
        if event.topic != packages::LISTED {
            return Ok(());
        }
        let (Some(id), Some(repository)) =
            (event.payload["id"].as_str(), event.payload["repository"].as_str())
        else {
            return Ok(());
        };
        repositories::heard(backend, id, repository).await
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        faux::check(backend).await;
        let response = match request.is_ui() {
            true => ui::handle(backend, &request).await,
            false => api::handle(backend, &request).await,
        };
        faux::marked(response)
    }
}

doc_plugin_sdk::main!(
    Eol,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("End of life", "/")
                .described(
                    "The languages, frameworks, databases and operating systems each service \
                     runs, and which are supported, ending soon or past their end of life",
                )
                .grouped("Technology"),
        ],
        resource_panels: ["service", "team", "organisation"]
            .into_iter()
            .map(|kind| ResourcePanel::new(kind, "End of life", "/panel"))
            .collect(),
        // Its panel with nothing named covers every service the viewer may see.
        dashboard: vec![DashboardItem::new("end-of-life", "End of life", "/panel").described(
            "What the services you can see run that is past its end of life or near it.",
        ),],
        // How much of what it runs is out of support, on its own.
        insights: ui::INSIGHTS
            .iter()
            .flat_map(|(id, status)| {
                ["service", "team", "organisation"].into_iter().map(move |kind| {
                    Insight::new(kind, id, status.word(), &format!("/insight/{id}"))
                        .described("The products it runs that are there, and how many services")
                })
            })
            .collect(),
        schedules: vec![
            Schedule::new(
                "refresh",
                settings::REFRESH,
                "Reads every product endoflife.date tracks again, and each service's own file",
            )
            .of_feature(LIFECYCLE)
            .from_setting(settings::REFRESH_SCHEDULE),
            Schedule::new(
                "repositories",
                settings::READ,
                "Reads what the repositories connected to each service are built on",
            )
            .of_feature(LIFECYCLE)
            .from_setting(settings::READ_SCHEDULE),
        ],
        // Repository Insights finishing a scan, which lists the repository's packages, and the
        // Catalogue connecting or disconnecting a repository.
        subscriptions: vec![
            packages::LISTED.into(),
            repositories::CATALOGUE_CHANGED.into(),
            ACCESS_APPROVED.into(),
        ],
        settings: settings::declared(),
        features: settings::features(),
        data: store::declaration(),
        // An agent asks over MCP with a POST that only reads.
        read_routes: vec!["mcp".into()],
        ..Manifest::default()
    }
);
