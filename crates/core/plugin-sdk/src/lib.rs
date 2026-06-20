//! The `Plugin` trait, the runtime that serves the backend's calls, and the client a plugin makes
//! its own calls with. A plugin is its own process: `main!` gives it an entry point that registers,
//! serves `/host/v1/*` over HTTP/3 and keeps trying until a backend answers.

pub mod backend;
pub mod runtime;
pub mod telemetry;

use std::collections::BTreeMap;

use async_trait::async_trait;
use bytes::Bytes;
use serde::Serialize;
use serde_json::Value;

pub use backend::{Backend, Flags, Page, Settings};
pub use doc_plugin_protocol as protocol;
pub use doc_plugin_protocol::calls::{
    AccessAnswer, PersonRequest, SealedValue, SettingsChanged, SettingsVerdict,
};
pub use doc_plugin_protocol::data::{
    Aggregate, Bucket, Collection, DataAnswer, DataRequest, Declaration, Direction, Export, Field,
    FieldType, ListOf, Measure, OnDelete, Order, Query,
};
pub use doc_plugin_protocol::{
    Caller, Capability, Choice, Choices, Classification, CustomPermission, DashboardItem, Event,
    Feature, Guard, Insight, Manifest, NamedSecrets, Nav, Need, Operation, OperationParam,
    OwnAccount, PluginState, ResourcePanel, RunInput, RunOutput, Schedule, Setting, SettingKind,
    SignIn, SignInKind,
};

#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("{0}")]
    Message(String),
    #[error("the backend is unreachable: {0}")]
    Unreachable(String),
    #[error("the backend refused this call: {status} {body}")]
    Refused { status: u16, body: String },
    #[error("{0} is required")]
    Forbidden(String),
    #[error("cancelled")]
    Cancelled,
    #[error("the deadline passed")]
    Deadline,
}

impl PluginError {
    pub(crate) fn encode(err: &serde_json::Error) -> Self {
        Self::Message(format!("malformed message: {err}"))
    }

    /// The status and kind of the problem the backend refused with, such as `(409, "version-conflict")`.
    pub fn problem(&self) -> Option<(u16, String)> {
        let Self::Refused { status, body } = self else { return None };
        let problem: protocol::Problem = serde_json::from_str(body).ok()?;
        Some((*status, problem.kind.trim_start_matches("/problems/").to_string()))
    }

    /// A write refused because its key, or a unique constraint's values, belong to another record.
    pub fn is_duplicate(&self) -> bool {
        self.problem().is_some_and(|(status, kind)| status == 409 && kind == "duplicate-record")
    }

    /// A write refused because the record's `_version` was not the one it named.
    pub fn is_version_conflict(&self) -> bool {
        self.problem().is_some_and(|(status, kind)| status == 409 && kind == "version-conflict")
    }

    /// What the backend said was wrong, which is written for a person to read.
    pub fn detail(&self) -> String {
        match self {
            Self::Refused { body, .. } => serde_json::from_str::<protocol::Problem>(body)
                .ok()
                .and_then(|problem| problem.detail)
                .unwrap_or_else(|| body.clone()),
            other => other.to_string(),
        }
    }
}

impl From<String> for PluginError {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

impl From<&str> for PluginError {
    fn from(message: &str) -> Self {
        Self::Message(message.to_string())
    }
}

/// An API or UI route the backend forwarded. The body is passed through untouched, so a plugin
/// serving HTML fragments gets exactly what the browser sent.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: String,
    /// The part after `/host/v1/request/`, with no leading slash.
    pub path: String,
    pub query: String,
    pub headers: BTreeMap<String, String>,
    pub body: Bytes,
}

impl Request {
    /// True for a UI route, which returns an HTML fragment rather than JSON (T22, T32).
    pub fn is_ui(&self) -> bool {
        self.path.starts_with("ui/") || self.path == "ui"
    }

    pub fn is_htmx(&self) -> bool {
        self.headers.contains_key("hx-request")
    }

    pub fn json<T: serde::de::DeserializeOwned>(&self) -> Result<T, PluginError> {
        serde_json::from_slice(&self.body).map_err(|err| PluginError::encode(&err))
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Response {
    pub fn new(status: u16, content_type: &str, body: impl Into<Bytes>) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), content_type.into())],
            body: body.into(),
        }
    }

    pub fn json<T: Serialize>(value: &T) -> Self {
        match serde_json::to_vec(value) {
            Ok(body) => Self::new(200, "application/json", body),
            Err(err) => Self::problem(500, "internal", &err.to_string()),
        }
    }

    /// Plugins supply their UI as fragments; the frontend wraps them in the layout (T32).
    pub fn html(body: impl Into<String>) -> Self {
        Self::new(200, "text/html; charset=utf-8", body.into().into_bytes())
    }

    pub fn text(body: impl Into<String>) -> Self {
        Self::new(200, "text/plain; charset=utf-8", body.into().into_bytes())
    }

    pub fn not_found() -> Self {
        Self::problem(404, "not-found", "no such route")
    }

    pub fn problem(status: u16, kind: &str, detail: &str) -> Self {
        let problem = protocol::Problem::new(status, kind, kind).detail(detail);
        let body = serde_json::to_vec(&problem).unwrap_or_default();
        Self::new(status, "application/problem+json", body)
    }

    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.to_string(), value.to_string()));
        self
    }

    /// Marks a page, panel or answer as showing faux data made up by `provider`, a plugin's ID;
    /// the frontend then says so at the top of the page, so no plugin words it for itself.
    pub fn faux(self, provider: &str) -> Self {
        self.with_header(protocol::header::FAUX, provider)
    }
}

/// The five functions from PLAN.md, plus `handle` for the plugin's own routes and `on_event` for
/// the events it subscribed to. Everything is async because every backend call is.
#[async_trait]
pub trait Plugin: Send + Sync + 'static {
    /// `previous` is whatever the last `unload` of any version returned, so a hot reload can pick
    /// up where the old one left off.
    async fn load(&mut self, backend: &Backend, previous: Option<Value>)
    -> Result<(), PluginError>;

    async fn unload(&mut self, backend: &Backend) -> Result<Option<Value>, PluginError>;

    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError>;

    async fn cancel(&self, backend: &Backend) -> Result<(), PluginError>;

    /// The last error the plugin itself noticed, which is what the `error` state reports.
    fn error(&self) -> Option<String> {
        None
    }

    async fn handle(&self, _backend: &Backend, _request: Request) -> Response {
        Response::not_found()
    }

    async fn on_event(&self, _backend: &Backend, _event: Event) -> Result<(), PluginError> {
        Ok(())
    }

    /// What the plugin thinks of settings somebody has typed but nothing has stored yet
    /// (ADR-0007). This is where a credential is tried against the service it is for, so
    /// "the token is refused" lands on the field rather than in a log an hour later. The
    /// default has no opinion, and core stores whatever passed its own checks.
    async fn settings_check(&self, _backend: &Backend, _proposed: &Settings) -> SettingsVerdict {
        SettingsVerdict::ok()
    }

    /// Core's word that the settings changed, once they are stored; `backend.settings()` already
    /// has the new ones. The default returns `false`, which has the runtime reload the plugin in
    /// this process — `unload` then `load` — so every plugin follows a change without a restart.
    /// A plugin that can do better returns `true` to say it has handled it itself.
    async fn settings_changed(
        &self,
        _backend: &Backend,
        _changed: &SettingsChanged,
    ) -> Result<bool, PluginError> {
        Ok(false)
    }
}

/// What `main!` collects at compile time and hands to the runtime.
pub struct Build {
    pub version: &'static str,
    pub manifest: Manifest,
}

/// Generates the process entry point: telemetry, the HTTP/3 endpoint, registration with retry,
/// liveness reports and the worker pool. The version comes from `Cargo.toml`; the binary's SHA-256
/// is read at start-up, because a binary cannot hash itself while it is being compiled.
#[macro_export]
macro_rules! main {
    ($plugin:ty, $manifest:expr) => {
        fn main() -> ::std::process::ExitCode {
            let mut manifest: $crate::Manifest = $manifest;
            manifest.version = env!("CARGO_PKG_VERSION").to_string();
            $crate::runtime::launch::<$plugin>($crate::Build {
                version: env!("CARGO_PKG_VERSION"),
                manifest,
            })
        }
    };
}
