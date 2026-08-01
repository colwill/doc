//! Feature flags and runtime configuration for the services this platform runs. DOC is the store:
//! a flag is a record here, and a service reads them all in one call with an ETag, so polling costs
//! a `304`. Where a team already has Unleash, Flagsmith, Runcfg or anything speaking OpenFeature's
//! remote protocol, DOC reads that too and serves it beside its own, which is what lets a service
//! read one endpoint whatever is behind it.

mod api;
mod evaluate;
mod model;
mod providers;
mod store;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Classification, Manifest, NamedSecrets, Nav, Operation, OperationParam, Plugin,
    PluginError, Request, ResourcePanel, Response, RunInput, RunOutput, Setting, SettingKind,
};
use serde_json::{Value, json};

pub const ID: &str = "flags";
/// The environment a call is about when it names none.
pub const DEFAULT_ENVIRONMENT: &str = "default-environment";
/// How often a service is told to read its flags again.
pub const REFRESH: &str = "refresh-seconds";
/// The plugins that may read flags as themselves, which a plugin left out asks to join.
pub const READER_PLUGINS: &str = "reader-plugins";

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

/// Who is changing something, as the audit trail and `updated_by` record them.
pub fn who(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "someone".to_string())
}

#[derive(Default)]
struct Flags;

#[async_trait]
impl Plugin for Flags {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), "feature flags loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, _backend: &Backend, _input: RunInput) -> Result<RunOutput, PluginError> {
        Err(PluginError::from("flags has nothing to run: it answers calls"))
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        match request.path.starts_with("ui") {
            true => ui::handle(backend, request).await,
            false => api::handle(backend, request).await,
        }
    }
}

doc_plugin_sdk::main!(
    Flags,
    Manifest {
        id: ID.into(),
        classification: Classification::Synchronous,
        nav: vec![
            Nav::new("Feature flags", "/")
                .described("Flags and runtime configuration the services read while they run")
                .grouped("Platform"),
        ],
        // OpenFeature's SDKs evaluate with a POST, which reads rather than writes: a service
        // account with `plugin:flags:service:ro` is enough to read its flags.
        read_routes: vec!["ofrep/*".into()],
        resource_panels: vec![ResourcePanel::new("service", "Feature flags", "/panel")],
        settings: vec![
            Setting::text(DEFAULT_ENVIRONMENT, "Default environment")
                .defaulting(json!("production"))
                .hinted("What a service is taken to be asking about when it names no environment.")
                .grouped("Serving"),
            Setting::new(REFRESH, "How often a service reads them again", SettingKind::Number)
                .defaulting(json!(30))
                .between(5.0, 3600.0)
                .hinted("Seconds. Served in every answer, so a service follows what is set here.")
                .grouped("Serving"),
            Setting::new(READER_PLUGINS, "Plugins that read flags", SettingKind::List)
                .defaulting(json!([]))
                .hinted(
                    "Which plugins may read flags as themselves, as the service named after them \
                     unless they name another. A plugin left out asks to be added, and whoever \
                     may change these settings approves or denies it.",
                )
                .requestable()
                .grouped("Plugins"),
        ],
        named_secrets: Some(NamedSecrets::new(
            "Provider credentials",
            "The credentials upstream providers are read with. Each provider chooses its own from \
             these on the Providers page.",
        )),
        data: store::declaration(),
        // What automations can do here by name (ADR-0012).
        operations: vec![
            Operation::new("set", "Set a flag or a setting", "entries")
                .described(
                    "Turns a flag on or off, or sets a value, for every service or one. It is \
                     made if it is not there yet.",
                )
                .param(OperationParam::required("key", "Key").hinted("Such as new-checkout."))
                .param(
                    OperationParam::required("value", "Value")
                        .hinted("true or false for a flag; for a setting, what it should be."),
                )
                .param(
                    OperationParam::optional("role", "Flag or setting")
                        .hinted("flag or config. A flag unless it says."),
                )
                .param(
                    OperationParam::optional("kind", "Kind")
                        .hinted("boolean, string, number or json. On or off unless it says."),
                )
                .param(
                    OperationParam::optional("service", "Service")
                        .hinted("Leave it out for every service."),
                )
                .param(
                    OperationParam::optional("environment", "Environment")
                        .hinted("Leave it out for every environment."),
                ),
        ],
        ..Manifest::default()
    }
);
