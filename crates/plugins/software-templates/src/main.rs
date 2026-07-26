//! Software Templates: the templates a platform creates things from, and every run of one. A
//! template says what it asks for and what it does with the answers — a repository with the files
//! rendered into it, an entry in the Catalogue, a call to another plugin, a word to whoever asked.
//! A run is queued as the person who launched it, so nothing is created that they could not have
//! created themselves.

mod api;
mod archive;
mod engine;
mod files;
mod model;
mod platform;
mod publish;
mod render;
mod scaffolds;
mod seed;
mod store;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, CustomPermission, Feature, Manifest, Nav, Plugin, PluginError,
    Request, Response, RunInput, RunOutput, Setting, SettingKind, Settings, SettingsVerdict,
};
use serde_json::{Value, json};
use uuid::Uuid;

pub const ID: &str = "templates";
/// Writing and removing the templates themselves, which is not the same as creating from one.
pub const AUTHOR: &str = "author";

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

    /// Everything wrong with a form, as one refusal for callers that are not the form.
    pub fn from_problems(problems: &[model::Problem]) -> Self {
        let said: Vec<String> = problems
            .iter()
            .map(|problem| format!("{}: {}", problem.parameter, problem.detail))
            .collect();
        Self::bad(said.join("; "))
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
        match err {
            PluginError::Forbidden(permission) => {
                Self::forbidden(format!("that needs {permission}:rw"))
            }
            PluginError::Message(detail) => Self::bad(detail),
            PluginError::Refused { status, body } => {
                let detail = serde_json::from_str::<Value>(&body)
                    .ok()
                    .and_then(|problem| problem["detail"].as_str().map(str::to_string))
                    .unwrap_or(body);
                Self { status: if (400..500).contains(&status) { status } else { 503 }, detail }
            }
            other => Self::unavailable(other.to_string()),
        }
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

#[derive(Default)]
struct Templates;

#[async_trait]
impl Plugin for Templates {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let seeded = seed::seed(backend).await?;
        tracing::info!(version = backend.version(), seeded, "software templates loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    /// A run of a template, queued as whoever launched it. Nothing else runs here.
    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let Some(id) = input.payload["run"].as_str().and_then(|id| Uuid::parse_str(id).ok()) else {
            return Err(PluginError::from("a run names the run it is taking"));
        };
        Ok(RunOutput { payload: engine::execute(backend, id).await? })
    }

    /// Asked for when an administrator cancels the plugin. A run that is going stops at its next
    /// step, which is what cancelling one run does too.
    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        match request.path.starts_with("ui") {
            true => ui::handle(backend, request).await,
            false => api::handle(backend, request).await,
        }
    }

    /// Tries a GitHub token before it is stored, so a wrong one lands on the field.
    async fn settings_check(&self, _backend: &Backend, proposed: &Settings) -> SettingsVerdict {
        if proposed.secret(publish::TOKEN).is_none() {
            return SettingsVerdict::ok();
        }
        match publish::GitHub::new(proposed) {
            Err(problem) => SettingsVerdict::wrong(publish::TOKEN, &problem),
            Ok(github) => match github.whoami().await {
                Ok(who) => SettingsVerdict::saying(&format!("GitHub knows this token as {who}")),
                Err(problem) => SettingsVerdict::wrong(publish::TOKEN, &problem),
            },
        }
    }
}

doc_plugin_sdk::main!(
    Templates,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        custom_permissions: vec![
            CustomPermission::user(AUTHOR).describes("Writing and removing templates"),
        ],
        nav: vec![
            Nav::new("Templates", "/")
                .described(
                    "Create a service or a tool, its repository and its Catalogue entry from a \
                     template"
                )
                .grouped("Technology"),
        ],
        settings: vec![
            Setting::secret(publish::TOKEN, "GitHub token")
                .hinted(
                    "A token allowed to create repositories where templates publish them. Without \
                     one, a template that only writes to the Catalogue still works."
                )
                .grouped("Publishing to GitHub"),
            Setting::text(publish::OWNER, "GitHub organisation")
                .hinted("Where a template publishes when it does not name an owner of its own.")
                .grouped("Publishing to GitHub"),
            Setting::new(publish::API, "GitHub API", SettingKind::Url)
                .defaulting(json!("https://api.github.com"))
                .hinted("GitHub Enterprise Server answers at https://github.acme.example/api/v3.")
                .grouped("Publishing to GitHub"),
            // One telemetry stack for the whole instance: every service scaffolded from here
            // exports to it, in its code, its container and its Kubernetes manifests, without
            // anybody being asked where it is.
            Setting::new(platform::OTLP_ENDPOINT, "OTLP endpoint", SettingKind::Url)
                .defaulting(json!("http://otel-collector:4317"))
                .hinted("Where everything this platform creates sends traces, metrics and logs.")
                .grouped("Telemetry"),
            Setting::new(platform::OTLP_PROTOCOL, "OTLP protocol", SettingKind::Choice)
                .one_of(&["grpc", "http"])
                .defaulting(json!("grpc"))
                .hinted("gRPC on 4317, or HTTP/protobuf on 4318.")
                .grouped("Telemetry"),
            Setting::new(
                platform::OTLP_INSECURE,
                "The endpoint is plain HTTP",
                SettingKind::Boolean
            )
            .defaulting(json!(true))
            .hinted("Turn this off where the collector has TLS, which is usual outside a cluster.")
            .grouped("Telemetry"),
            Setting::text(platform::OTLP_HEADERS, "OTLP headers")
                .hinted(
                    "Written into every service, so nothing secret belongs here: \
                     `x-tenant=acme`. A collector that needs a key reads it from a secret where \
                     the service runs."
                )
                .grouped("Telemetry"),
            Setting::text(platform::ENVIRONMENT, "Environment")
                .defaulting(json!("production"))
                .hinted("What a created service reports itself as running in.")
                .grouped("Telemetry"),
            Setting::text(platform::NAMESPACE, "Service namespace")
                .hinted("Grouped under this in the telemetry stack, such as your organisation.")
                .grouped("Telemetry"),
            Setting::new(platform::DOC_URL, "Where DOC is", SettingKind::Url)
                .hinted(
                    "As a service running outside DOC reaches it. Its feature flags are read \
                     from here. `DOC_PUBLIC_URL` is used when this is empty."
                )
                .grouped("Runtime"),
        ],
        features: vec![
            Feature::new(
                platform::FLAGS,
                "Feature flags",
                "Wires everything this platform creates to DOC's feature flags: the client, the \
                 environment it reads and the fallbacks it starts with.",
            )
            .on(),
        ],
        data: store::declaration(),
        ..Manifest::default()
    }
);
