//! Watercooler: discussions tagged with the resources they are about; hackathons, game nights and
//! challenges, each with a discussion and a place on its team's calendar; and cards, which stay
//! hidden from their recipient until revealed, and kudos.

mod api;
mod card_pages;
mod cards;
mod event_pages;
mod events;
mod markdown;
mod store;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, CustomPermission, DashboardItem, Manifest, Nav, Plugin, PluginError,
    Request, ResourcePanel, Response, RunInput, RunOutput, Schedule,
};
use serde_json::Value;

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
        tracing::warn!(%err, "a call to the backend failed");
        Self::unavailable(format!("the discussions' storage failed: {err}"))
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

#[derive(Default)]
struct Watercooler;

#[async_trait]
impl Plugin for Watercooler {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), "watercooler loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    /// The `cards` schedule, each minute.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        match input.payload["schedule"].as_str() {
            Some("cards") => Ok(RunOutput { payload: cards::deliver(backend).await? }),
            _ => Err(PluginError::from("watercooler runs only its cards schedule")),
        }
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        api::handle(backend, request).await
    }
}

doc_plugin_sdk::main!(
    Watercooler,
    Manifest {
        id: "water".into(),
        classification: Classification::Synchronous,
        custom_permissions: vec![
            CustomPermission::user("hackathon"),
            CustomPermission::user(api::MODERATE)
                .describes("Deleting anybody's discussion or reply in Watercooler"),
            CustomPermission::service(api::MODERATE).describes(
                "Deleting any discussion or reply in Watercooler, for automated moderation"
            ),
        ],
        // Its own menu in the bar, one entry per section, in the order its tabs run.
        nav: vec![
            Nav::new("Discussions", "/")
                .described("What everyone is talking about, by tag and by team")
                .grouped("Watercooler"),
            Nav::new("Events", "/events")
                .described("Hackathons, game nights and everything else in the diary")
                .grouped("Watercooler"),
            Nav::new("Kudos", "/kudos")
                .described("Thanks and praise people have given each other")
                .grouped("Watercooler"),
            Nav::new("Cards", "/cards")
                .described("Cards announcing what is worth everyone knowing")
                .grouped("Watercooler"),
        ],
        dashboard: vec![
            DashboardItem::new("tagged", "Discussions you are tagged in", "/dashboard/tagged")
                .described("The newest discussions that name you."),
            DashboardItem::new("kudos", "Kudos for you", "/kudos/dashboard")
                .described("The thanks and praise people have given you lately."),
        ],
        resource_panels: [
            "organisation",
            "service",
            "repository",
            "team",
            "role",
            "user",
            "service-account",
            "documentation",
            "cloud-resource",
            "attribute",
            "permission"
        ]
        .into_iter()
        .map(|kind| ResourcePanel::new(kind, "Discussions", "/panel"))
        .chain(["team", "user"].into_iter().map(|kind| ResourcePanel::new(
            kind,
            "Kudos",
            "/kudos/panel"
        )),)
        .collect(),
        schedules: vec![Schedule::new(
            "cards",
            "* * * * *",
            "Announces each card as its reveal time comes, as plugin.water.card.delivered"
        )],
        data: store::declaration(),
        ..Manifest::default()
    }
);
