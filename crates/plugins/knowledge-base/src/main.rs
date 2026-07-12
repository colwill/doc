//! Knowledge Base: documents from MkDocs projects, Markdown, GitHub, Confluence and Google Drive,
//! rendered safely, searched with Postgres full-text search, shown by service in a catalogue and
//! served to MCP clients.

mod api;
mod archive;
mod confluence;
mod drive;
mod edits;
mod faux;
mod git;
mod imports;
mod markdown;
mod mcp;
mod mkdocs;
mod owners;
mod removal;
mod repositories;
mod runbooks;
mod sources;
mod storage;
mod store;
mod text;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, DashboardItem, Feature, Manifest, NamedSecrets, Nav, Plugin,
    PluginError, Request, ResourcePanel, Response, RunInput, RunOutput, Schedule, Setting,
    SettingKind,
};
use serde_json::{Value, json};
use uuid::Uuid;

/// The plugins that may write pages into a space themselves, which a plugin left out asks to join.
pub const PUBLISHER_PLUGINS: &str = "publisher-plugins";

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
            409 => "conflict",
            _ => "unavailable",
        };
        Response::problem(self.status, kind, &self.detail)
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        tracing::warn!(%err, "a call to the backend failed");
        Self::unavailable(format!("the Knowledge Base's storage failed: {err}"))
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

#[derive(Default)]
struct KnowledgeBase;

#[async_trait]
impl Plugin for KnowledgeBase {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        // A source fetches with a credential named in its own configuration, so what core gave
        // us is kept where those fetches can reach it (ADR-0007).
        sources::remember(backend);
        // What the catalogue knows of the sources is refreshed here as well as when one is
        // added, so a Knowledge Base filled before there were source resources has them.
        sources::announce(backend, &store::Store(backend)).await;
        tracing::info!(version = backend.version(), "kb loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    /// One batch of an import, or one source's sync.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let store = store::Store(backend);
        let payload = &input.payload;
        let id = |key: &str| payload[key].as_str().and_then(|id| id.parse::<Uuid>().ok());
        if payload["schedule"] == "sources" {
            return Ok(RunOutput { payload: sources::due(backend, &store).await? });
        }
        if payload["schedule"] == repositories::FEATURE {
            return Ok(RunOutput { payload: repositories::follow(backend, &store).await? });
        }
        let output = match (id("import"), id("source")) {
            (Some(import), _) => {
                let from =
                    payload["from"].as_i64().and_then(|from| i32::try_from(from).ok()).unwrap_or(0);
                imports::batch(backend, &store, import, from).await?
            }
            (None, Some(source)) => sources::sync(backend, &store, source).await?,
            _ => return Err(PluginError::from("a run names an import or a source")),
        };
        Ok(RunOutput { payload: output })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        api::handle(backend, request).await
    }
}

doc_plugin_sdk::main!(
    KnowledgeBase,
    Manifest {
        id: "kb".into(),
        classification: Classification::Async,
        settings: vec![
            Setting::new(PUBLISHER_PLUGINS, "Plugins that publish pages", SettingKind::List)
                .defaulting(json!(["dns"]))
                .hinted(
                    "Which plugins may write pages into a space themselves, such as DNS writing \
                     how to use it into development-environment. Each keeps its own pages there \
                     and replaces only those. A plugin left out asks to be added, and whoever may \
                     change these settings approves or denies it.",
                )
                .requestable()
                .grouped("Plugins"),
            Setting::new("google-api", "Google API URL", SettingKind::Url)
                .hinted("Where Google Drive is read from. Empty uses Google's own.")
                .grouped("Google Drive"),
            Setting::new(
                repositories::SCHEDULE_KEY,
                "How often to look at the repositories",
                SettingKind::Cron,
            )
            .hinted(
                "A cron expression in UTC. Each run takes on any new repository and re-reads \
                 the ones that have been pushed to since they were last read.",
            )
            .defaulting(json!(repositories::SCHEDULE))
            .grouped("Documentation from repositories")
            .of_feature(repositories::FEATURE),
            Setting::new(
                repositories::PER_RUN_KEY,
                "Repositories read in one run",
                SettingKind::Number,
            )
            .hinted(
                "GitHub hands out 60 archive links an hour to anyone asking without a token, \
                 which the default stays under. Raise it where the GitHub plugin has a token or \
                 an App, which are limited far more generously.",
            )
            .defaulting(json!(repositories::PER_RUN))
            .between(1.0, 200.0)
            .grouped("Documentation from repositories")
            .of_feature(repositories::FEATURE),
            Setting::new(
                repositories::EXCLUDE_KEY,
                "Repositories to leave out",
                SettingKind::List,
            )
            .hinted(
                "Whole names or patterns, such as `acme/*` or `*-archive`. One to a line, or \
                 separated by commas. Archived repositories are left out anyway.",
            )
            .grouped("Documentation from repositories")
            .of_feature(repositories::FEATURE),
        ],
        features: vec![
            Feature::new(
                repositories::FEATURE,
                "Documentation from repositories",
                "Keeps a source for every repository the platform knows, so the Markdown \
                 written in a repository is documentation in DOC: searchable, in the catalogue, \
                 and under the Docs tab on the repository's own page.",
            )
            .warning(
                "Every repository is read through a GitHub archive link, a few at a time, and \
                 again whenever it has been pushed to. Leave out what you do not want read.",
            ),
        ],
        // Each source names the credential it fetches with, so they are held by name rather than
        // declared one by one: a Confluence token here, a Drive service account there.
        named_secrets: Some(NamedSecrets::new(
            "Source credentials",
            "Each source names the credential it is fetched with. Add one here under that name, \
             such as `confluence` or `drive-platform`.",
        )),
        schedules: vec![
            Schedule::new(
                "sources",
                "* * * * *",
                "Syncs each source whose own schedule has come round",
            ),
            Schedule::new(
                repositories::FEATURE,
                repositories::SCHEDULE,
                "Takes on new repositories and re-reads the ones that have been pushed to",
            )
            .of_feature(repositories::FEATURE)
            .from_setting(repositories::SCHEDULE_KEY),
        ],
        nav: vec![
            Nav::new("Knowledge", "/")
                .described("Documentation for every service, searchable in one place")
                .grouped("Workspace")
        ],
        read_routes: vec!["mcp".into()],
        dashboard: vec![
            DashboardItem::new("recent", "Docs changed lately", "/dashboard")
                .described("The pages changed most recently in the spaces your teams keep."),
        ],
        resource_panels: ["organisation", "service", "repository", "team"]
            .into_iter()
            .map(|kind| ResourcePanel::new(kind, "Docs", "/panel"))
            // A page's own summary reads best after whatever else the page has to say about it.
            .chain([ResourcePanel::new("documentation", "Summary", "/summary").after(10)])
            .collect(),
        data: store::declaration(),
        ..Manifest::default()
    }
);
