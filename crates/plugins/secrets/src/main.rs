//! Secret Storage: credentials kept once for the whole platform (FEAT-SECRETS). Core seals every
//! value under its settings key; a secret goes only to the plugins it is shared with and is never
//! shown to anybody, and a vendor account issues child tokens narrower than itself.
//!
//! A **proxied** account works the other way round (ADR-0014): nobody but DOC holds the vendor's
//! credential, callers hold a DOC key and reach the vendor through DOC, and every call they make
//! is written down. That path listens for itself, beside the routes core forwards here.

mod api;
mod directory;
mod ops;
mod proxy;
mod store;
mod ui;
mod vendors;
mod work;

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Capability, Classification, CustomPermission, DashboardItem, Event, Feature, Manifest,
    Nav, Plugin, PluginError, Request, Response, RunInput, RunOutput, Schedule, Setting,
    SettingKind,
};
use serde_json::{Value, json};
use tokio::sync::watch;

pub const ID: &str = "secrets";
/// Managing every organisation's secrets and accounts, beside a platform administrator.
pub const ADMIN: &str = "admin";
/// The feature that turns the vendor proxy on. Off until somebody means it: it opens a port and
/// puts DOC on the critical path of everything its callers do at their vendors.
pub const PROXY: &str = "proxy";
/// Said when anything the proxy serves from changes, so every replica re-reads it at once rather
/// than at the end of its refresh interval (ADR-0011).
pub const CHANGED: &str = "plugin.secrets.proxy.changed";
/// Said when a proxied account's own host should start or stop existing (ADR-0015 §4). Whatever
/// names things — DNS — writes the record; a name DOC answers for is not this plugin's to keep.
pub const HOSTED: &str = "plugin.secrets.proxy.hosted";
pub const UNHOSTED: &str = "plugin.secrets.proxy.unhosted";

/// How long after loading the store tells core it is back, so plugins waiting on it ask again:
/// long enough for core to have it serving.
const SETTLED: Duration = Duration::from_secs(3);

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

    pub fn conflict(detail: impl Into<String>) -> Self {
        Self { status: 409, detail: detail.into() }
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

impl std::fmt::Display for Refusal {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(out, "{}", self.detail)
    }
}

impl From<PluginError> for Refusal {
    fn from(err: PluginError) -> Self {
        tracing::warn!(%err, "a call to the backend failed");
        Self::unavailable(format!("Secret Storage's storage failed: {}", err.detail()))
    }
}

impl From<Refusal> for PluginError {
    fn from(refusal: Refusal) -> Self {
        Self::Message(refusal.detail)
    }
}

/// One query parameter, decoded.
pub fn parameter(query: &str, name: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// The running proxy: its working set, the word that stops it, and the client for plugins' calls.
struct Running {
    working: Arc<proxy::Working>,
    stop: watch::Sender<bool>,
    client: reqwest::Client,
}

#[derive(Default)]
struct Secrets {
    proxy: Option<Running>,
}

#[async_trait]
impl Plugin for Secrets {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let told = backend.clone();
        tokio::spawn(async move {
            tokio::time::sleep(SETTLED).await;
            if let Err(err) = told.secrets_changed(Vec::new(), true).await {
                tracing::warn!(%err, "core was not told Secret Storage is back");
            }
        });
        self.proxy = start(backend);
        tracing::info!(version = backend.version(), "secrets loaded");
        Ok(())
    }

    async fn unload(&mut self, backend: &Backend) -> Result<Option<Value>, PluginError> {
        if let Some(running) = self.proxy.take() {
            let _ = running.stop.send(true);
            // Nothing here waits on core either, for the same reason `load` does not: core is
            // waiting for this call to answer, and a plugin that calls back into core while it
            // does is a plugin core stops waiting for. What is still buffered is written by a
            // task that outlives the handover — the process stays up across a reload — and the
            // keeper writes the rest on its next beat.
            let backend = backend.clone();
            tokio::spawn(async move { drained(&running.working, &backend).await });
        }
        Ok(None)
    }

    async fn on_event(&self, backend: &Backend, event: Event) -> Result<(), PluginError> {
        if let Some(running) = &self.proxy
            && event.topic == CHANGED
            && let Err(err) = running.working.fill(backend).await
        {
            tracing::warn!(%err, "the proxy's working set was not refreshed on the change");
        }
        Ok(())
    }

    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        let payload = match input.payload["schedule"].as_str() {
            Some("daily") => work::daily(backend).await?,
            _ => work::renew(backend).await?,
        };
        Ok(RunOutput { payload })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    /// What the plugin itself has noticed. The one thing worth saying is that the proxy has
    /// stopped because it cannot write the audit down: everything else is a call's own refusal.
    fn error(&self) -> Option<String> {
        let running = self.proxy.as_ref()?;
        match running.working.recorder.room() {
            true => None,
            false => Some(format!(
                "the proxy is refusing calls: {} records are waiting to be written and the \
                 buffer is full",
                running.working.recorder.waiting()
            )),
        }
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        let path = request.path.trim_end_matches('/').to_string();
        let segments: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        match segments.as_slice() {
            // Answered here rather than in `api`, because only this process knows what its own
            // proxy is holding.
            ["api", "proxy"] => self.state(backend).await,
            ["ui", route @ ..] => ui::handle(backend, &request, route).await,
            ["api", route @ ..] => api::handle(backend, &request, route).await,
            ["internal", route @ ..] => api::internal(backend, &request, route).await,
            // A plugin calling a proxied account as itself; only this process holds the working set.
            ["discovery", "via", account, route @ ..] => {
                self.via(backend, &request, account, route).await
            }
            ["discovery", route @ ..] => api::discovery(backend, &request, route).await,
            _ => Refusal::missing("no such route").response(),
        }
    }
}

/// Starts the proxy when the feature is on: it listens straight away and fills its working set
/// in the background.
///
/// **Nothing here waits on core.** `load` holds the plugin's own lock and core is waiting for it
/// to answer, so a `load` that calls back into core is a plugin that cannot answer anything while
/// core cannot answer it either. Filling the working set is a dozen round trips, and it belongs
/// in [`keeping`]. The listener refuses every call until the first fill lands, which is the right
/// answer for a replica that knows nothing.
fn start(backend: &Backend) -> Option<Running> {
    let configured = proxy::Configured::read(backend);
    if !configured.on {
        return None;
    }
    let tls = match proxy::tls(backend) {
        Ok(tls) => tls,
        // A certificate that was asked for and cannot be used stops the proxy, rather than its
        // callers finding out by sending credentials over a plain connection.
        Err(wrong) => {
            tracing::error!(wrong, "the vendor proxy will not listen without working TLS");
            return None;
        }
    };
    let client = match proxy::listen::outbound() {
        Ok(client) => client,
        Err(err) => {
            tracing::error!(err, "the vendor proxy has no client to call vendors with");
            return None;
        }
    };
    let working = Arc::new(proxy::Working::new(configured, tls));
    let (stop, listening) = watch::channel(false);
    tokio::spawn({
        let working = working.clone();
        async move {
            if let Err(err) = proxy::listen::serve(working, listening).await {
                tracing::error!(err, "the vendor proxy stopped");
            }
        }
    });
    tokio::spawn(keeping(backend.clone(), working.clone(), stop.subscribe()));
    Some(Running { working, stop, client })
}

/// Keeps the working set current and the audit written. Both happen on the same beat because
/// both are about core being reachable, and the proxy carries on serving whether it is or not.
///
/// The first fill is the first thing it does, off `load`'s thread, so the window in which the
/// proxy knows nothing is as short as core takes to answer rather than as long as `load`.
async fn keeping(backend: Backend, working: Arc<proxy::Working>, mut stop: watch::Receiver<bool>) {
    let mut beat = tokio::time::interval(working.configured.refresh);
    beat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        match working.fill(&backend).await {
            // A replica that cannot fill holds no credentials, so there is nothing it could
            // safely serve. It refuses and tries again on the next beat (ADR-0014 §11).
            Err(err) => tracing::warn!(%err, "the proxy is serving what it already had"),
            Ok(held) => tracing::debug!(accounts = held, "the proxy's working set is current"),
        }
        drained(&working, &backend).await;
        tokio::select! {
            _ = beat.tick() => {}
            _ = stop.changed() => break,
        }
        if *stop.borrow() {
            break;
        }
    }
}

/// Writes what the audit is holding, in rounds, and notes when each key was last used. Bounded:
/// this is called from `unload` as well, which core is also waiting on.
async fn drained(working: &proxy::Working, backend: &Backend) {
    /// Enough for the default buffer several times over, and still a finite number of calls.
    const ROUNDS: usize = 64;
    for _ in 0..ROUNDS {
        if working.recorder.flush(backend).await == 0 {
            break;
        }
    }
    working.recorder.touch(backend).await;
}

impl Secrets {
    /// A call another plugin makes to a proxied account as itself, ruled and recorded as any.
    async fn via(
        &self,
        backend: &Backend,
        request: &Request,
        account: &str,
        route: &[&str],
    ) -> Response {
        let caller = backend.caller().filter(|caller| caller.kind == "plugin");
        let Some(plugin) = caller.and_then(|caller| caller.id.clone()) else {
            return Refusal::forbidden("only a plugin asks here").response();
        };
        let Some(running) = &self.proxy else {
            return Refusal::unavailable(
                "Secret Storage's vendor proxy is off. Whoever may change Secret Storage's \
                 settings turns on Vendor proxy under its features.",
            )
            .response();
        };
        proxy::through::call(&running.working, &running.client, request, &plugin, account, route)
            .await
    }

    /// How the proxy on this replica is doing: what it is serving from, how fresh that is, and
    /// how much of the audit is still waiting to be written.
    async fn state(&self, backend: &Backend) -> Response {
        if backend.require(ADMIN, false).is_err() && !backend.allows("user", false) {
            return Refusal::forbidden("that needs plugin:secrets:user:ro").response();
        }
        let Some(running) = &self.proxy else {
            return Response::json(&json!({ "on": false }));
        };
        let set = running.working.set.read().await;
        Response::json(&json!({
            "on": true,
            "replica": running.working.replica,
            "port": running.working.configured.port,
            "filled_at": set.filled_at,
            "fresh": running.working.fresh().await,
            "recording": running.working.recorder.room(),
            "waiting": running.working.recorder.waiting(),
            "written": running.working.recorder.written(),
        }))
    }
}

doc_plugin_sdk::main!(
    Secrets,
    Manifest {
        id: ID.into(),
        classification: Classification::Async,
        capabilities: vec![Capability::SecretStore],
        dashboard: vec![
            DashboardItem::new("expiring", "Secrets running out", "/dashboard")
                .described("Secrets you look after that expire within a month, or already have.",),
        ],
        nav: vec![
            Nav::new("Secret Storage", "/")
                .described(
                    "Credentials kept once for every plugin, and child tokens from the vendors \
                     DOC is onboarded to",
                )
                .grouped("Access"),
        ],
        custom_permissions: vec![
            CustomPermission::user(ADMIN)
                .describes("Managing every organisation's secrets and vendor accounts"),
        ],
        features: vec![
            Feature::new(
                PROXY,
                "Vendor proxy",
                "Callers reach a vendor through DOC instead of holding the vendor's credential \
                 — people and services with a DOC key, plugins as themselves — and every call \
                 is recorded.",
            )
            .warning(
                "This opens a port of its own and puts DOC in the path of every call its \
                 callers make to their vendors.",
            ),
        ],
        settings: vec![
            Setting::new("proxy-port", "Port", SettingKind::Number)
                .hinted("The port the proxy listens on for callers, behind the ingress.")
                .defaulting(json!(proxy::PORT))
                .between(1.0, 65_535.0)
                .of_feature(PROXY)
                .grouped("Vendor proxy"),
            Setting::new("proxy-address", "Address callers use", SettingKind::Url)
                .hinted(
                    "Such as https://proxy.doc.example. Used to point paging links back at DOC \
                     rather than at the vendor.",
                )
                .of_feature(PROXY)
                .grouped("Vendor proxy"),
            Setting::new("proxy-zone", "Domain accounts answer under", SettingKind::Text)
                .hinted(
                    "Such as rundoc.sh, so an account called github answers at \
                     github.rundoc.sh and callers write the vendor's own paths. Leave it empty \
                     to use /via/<account>/<path> alone.",
                )
                .of_feature(PROXY)
                .grouped("Vendor proxy"),
            Setting::new("proxy-certificate", "Certificate", SettingKind::Text)
                .hinted(
                    "The chain the listener presents, in PEM, leaf first. Wildcard for the \
                     domain above, so onboarding an account needs no certificate work. Leave it \
                     empty where TLS stops at the ingress instead.",
                )
                .of_feature(PROXY)
                .grouped("Vendor proxy"),
            Setting::new("proxy-certificate-key", "Certificate key", SettingKind::Secret)
                .hinted("The private key for that certificate, in PEM.")
                .of_feature(PROXY)
                .grouped("Vendor proxy"),
            Setting::new("proxy-refresh", "Read the rules again every", SettingKind::Duration)
                .hinted("Changes also arrive at once on the Event Bus; this is the backstop.")
                .defaulting(json!(60))
                .of_feature(PROXY)
                .grouped("Vendor proxy"),
            Setting::new("proxy-stale-after", "Serve alone for at most", SettingKind::Duration)
                .hinted(
                    "How long the proxy keeps working without hearing from the platform. Past \
                     it, calls are refused rather than allowed on rules it cannot vouch for.",
                )
                .defaulting(json!(4 * 3_600))
                .of_feature(PROXY)
                .grouped("Vendor proxy"),
            Setting::new("proxy-held-calls", "Records held while offline", SettingKind::Number)
                .hinted(
                    "Calls waiting to be written down while the platform is away. When this \
                     fills, the proxy stops: a call that cannot be recorded is not made.",
                )
                .defaulting(json!(5_000))
                .between(100.0, 1_000_000.0)
                .of_feature(PROXY)
                .grouped("Vendor proxy"),
        ],
        subscriptions: vec![CHANGED.into()],
        schedules: vec![
            Schedule::new(
                "renew",
                "*/5 * * * *",
                "Renews the tokens kept for plugins before they end, and marks spent tokens",
            ),
            Schedule::new(
                "daily",
                "0 7 * * *",
                "Warns before secrets expire, and seals values again under the newest key",
            ),
        ],
        data: store::declaration(),
        ..Manifest::default()
    }
);

/// A JSON answer with a status.
pub fn answered(status: u16, value: &Value) -> Response {
    let mut response = Response::json(value);
    response.status = status;
    response
}

/// Nothing to say, for a run that had no work.
pub fn nothing() -> Value {
    json!({})
}
