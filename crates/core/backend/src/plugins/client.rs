//! What the backend calls on a plugin. The trait is what T22's endpoint tests replace with a fake;
//! the HTTP/3 implementation dials the address the plugin advertised when it registered, presenting
//! the per-instance secret it was given so the plugin knows the call is really from core.

use std::future::Future;
use std::net::{SocketAddr, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};
use doc_plugin_protocol::calls::{SettingsChanged, SettingsCheck, SettingsVerdict};
use doc_plugin_protocol::{
    Caller, Event, LoadRequest, PluginState, RunInput, RunOutput, Secret, UnloadResponse, header,
    host,
};
use doc_transport::{EndpointConfig, H3Client, endpoint};
use http::{Method, StatusCode};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tracing::Instrument;

pub const LOAD_DEADLINE: Duration = Duration::from_secs(60);
pub const CALL_DEADLINE: Duration = Duration::from_secs(30);
pub const EVENT_DEADLINE: Duration = Duration::from_secs(30);
const HEALTH_DEADLINE: Duration = Duration::from_secs(5);
/// A plugin checking proposed settings may have to ask an external service, so it gets longer
/// than a request would, and a person is waiting, so not much longer (ADR-0007).
const SETTINGS_DEADLINE: Duration = Duration::from_secs(10);
const EXIT_DEADLINE: Duration = Duration::from_secs(5);

#[derive(Debug, thiserror::Error)]
pub enum CallError {
    #[error("{0} is unreachable: {1}")]
    Unreachable(String, String),
    /// The plugin has not stored its registration secret yet, so it is refusing core as well.
    #[error("{0} is not ready")]
    NotReady(String),
    #[error("{0} refused the call: {1} {2}")]
    Refused(String, u16, String),
    #[error("{0} did not answer in time")]
    Deadline(String),
    #[error("{0} answered with something unreadable: {1}")]
    Malformed(String, String),
}

impl CallError {
    /// The `detail` of the plugin's own problem answer, or the error itself if it never answered.
    pub fn detail(&self) -> String {
        match self {
            Self::Refused(_, _, body) => serde_json::from_str::<Value>(body)
                .ok()
                .and_then(|problem| problem["detail"].as_str().map(str::to_string))
                .unwrap_or_else(|| body.clone()),
            other => other.to_string(),
        }
    }
}

/// A caller's request on its way to a plugin's `api/`, `ui/`, `public/` or `internal/` routes.
#[derive(Debug, Clone)]
pub struct Forwarded {
    pub method: Method,
    pub path: String,
    pub query: Option<String>,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

/// What the plugin answered, handed back to the caller as it is.
#[derive(Debug, Clone)]
pub struct Answer {
    pub status: StatusCode,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

impl Answer {
    pub fn json(status: StatusCode, body: &Value) -> Self {
        Self {
            status,
            headers: vec![("content-type".into(), "application/json".into())],
            body: Bytes::from(body.to_string()),
        }
    }
}

/// Every call but `health` carries the context token issued for it, which is what the plugin
/// hands back to act as whoever the call is for.
#[async_trait]
pub trait PluginClient: Send + Sync {
    async fn load(&self, previous: Option<Value>, context: &str) -> Result<(), CallError>;
    async fn unload(&self, context: &str) -> Result<Option<Value>, CallError>;
    async fn cancel(&self, context: &str) -> Result<(), CallError>;
    /// No deadline is a long-running run, which is meant to last until it is cancelled.
    async fn run(
        &self,
        input: &RunInput,
        caller: &Caller,
        context: &str,
        deadline: Option<Duration>,
    ) -> Result<RunOutput, CallError>;
    async fn request(
        &self,
        request: &Forwarded,
        caller: &Caller,
        context: &str,
        deadline: Duration,
    ) -> Result<Answer, CallError>;
    async fn event(&self, event: &Event, context: &str) -> Result<(), CallError>;
    /// What the plugin thinks of settings nothing has stored yet (ADR-0007). A plugin that does
    /// not serve this call answers `404`, which core takes as having no objection.
    async fn settings_check(
        &self,
        check: &SettingsCheck,
        context: &str,
    ) -> Result<SettingsVerdict, CallError>;
    /// The keys that changed, after they were stored.
    async fn settings_changed(
        &self,
        changed: &SettingsChanged,
        context: &str,
    ) -> Result<(), CallError>;
    /// The plugin's own view of its state, which a probe compares with the registry's.
    async fn health(&self) -> Result<PluginState, CallError>;
    async fn exit(&self) -> Result<(), CallError>;
}

/// Escapes non-ASCII as `\uXXXX`, since a header value must be ASCII and a label may not be.
pub fn caller_header(caller: &Caller) -> String {
    let json = serde_json::to_string(caller).unwrap_or_else(|_| "{}".into());
    let mut out = String::with_capacity(json.len());
    for c in json.chars() {
        if c.is_ascii() {
            out.push(c);
        } else {
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                out.push_str(&format!("\\u{unit:04x}"));
            }
        }
    }
    out
}

/// Makes a client for a plugin that has just registered. Swapped out in tests.
#[async_trait]
pub trait Connector: Send + Sync {
    async fn connect(
        &self,
        id: &str,
        address: &str,
        secret: &str,
    ) -> Result<Arc<dyn PluginClient>, String>;
}

pub struct H3Connector {
    secrets: std::path::PathBuf,
}

impl H3Connector {
    pub fn new(secrets: impl Into<std::path::PathBuf>) -> Self {
        Self { secrets: secrets.into() }
    }
}

#[async_trait]
impl Connector for H3Connector {
    async fn connect(
        &self,
        id: &str,
        address: &str,
        secret: &str,
    ) -> Result<Arc<dyn PluginClient>, String> {
        let addr = resolve(address).map_err(|err| format!("resolving {address}: {err}"))?;
        let endpoint = endpoint(&EndpointConfig::client(self.secrets.clone()))
            .map_err(|err| format!("opening a client endpoint: {err}"))?
            .0;
        // The name is what the plugin's certificate is issued for, so dialling the wrong process
        // fails the handshake rather than reaching it.
        let client = H3Client::new(endpoint, addr, format!("plugin-{id}"));
        Ok(Arc::new(H3PluginClient {
            id: id.to_string(),
            secret: Secret::new(secret.to_string()),
            client,
        }))
    }
}

fn resolve(address: &str) -> anyhow::Result<SocketAddr> {
    address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("{address} resolved to nothing"))
}

pub struct H3PluginClient {
    id: String,
    secret: Secret<String>,
    client: H3Client,
}

impl H3PluginClient {
    async fn post<R: DeserializeOwned>(
        &self,
        path: &str,
        body: &Value,
        deadline: Option<Duration>,
        context: Option<&str>,
        caller: Option<&Caller>,
    ) -> Result<R, CallError> {
        let payload = Bytes::from(
            serde_json::to_vec(body)
                .map_err(|err| CallError::Malformed(self.id.clone(), err.to_string()))?,
        );
        let authorization = format!("Bearer {}", self.secret.expose());
        let remaining = deadline.map(|deadline| deadline.as_millis().to_string());
        let caller = caller.map(caller_header);
        let mut headers: Vec<(&str, &str)> =
            vec![("content-type", "application/json"), ("authorization", &authorization)];
        if let Some(remaining) = &remaining {
            headers.push((header::DEADLINE_MS, remaining));
        }
        if let Some(context) = context {
            headers.push((header::CONTEXT, context));
        }
        if let Some(caller) = &caller {
            headers.push((header::CALLER, caller));
        }
        let parent = doc_telemetry::traceparent();
        if let Some(parent) = &parent {
            headers.push((header::TRACEPARENT, parent));
        }
        let call = self.client.call(Method::POST, path, &headers, payload);
        let answered = match deadline {
            Some(deadline) => tokio::time::timeout(deadline, call)
                .await
                .map_err(|_| CallError::Deadline(self.id.clone()))?,
            None => call.await,
        };
        let (status, bytes) =
            answered.map_err(|err| CallError::Unreachable(self.id.clone(), err.to_string()))?;
        if status == StatusCode::SERVICE_UNAVAILABLE && bytes.is_empty() {
            return Err(CallError::NotReady(self.id.clone()));
        }
        if !status.is_success() {
            let body = String::from_utf8_lossy(&bytes).into_owned();
            return Err(CallError::Refused(self.id.clone(), status.as_u16(), body));
        }
        let bytes = if bytes.is_empty() { Bytes::from_static(b"null") } else { bytes };
        serde_json::from_slice(&bytes)
            .map_err(|err| CallError::Malformed(self.id.clone(), err.to_string()))
    }

    /// Forwards the caller's method, query and body, and returns the answer unjudged.
    async fn exchange(
        &self,
        request: &Forwarded,
        caller: &Caller,
        context: &str,
        deadline: Duration,
    ) -> Result<Answer, CallError> {
        let unreachable =
            |err: anyhow::Error| CallError::Unreachable(self.id.clone(), err.to_string());
        let mut target = format!("{}{}", host::REQUEST_PREFIX, request.path);
        if let Some(query) = &request.query {
            target = format!("{target}?{query}");
        }
        let mut builder = http::Request::builder()
            .method(request.method.clone())
            .uri(self.client.uri(&target))
            .header("authorization", format!("Bearer {}", self.secret.expose()))
            .header(header::DEADLINE_MS, deadline.as_millis().to_string())
            .header(header::CONTEXT, context)
            .header(header::CALLER, caller_header(caller));
        // The caller's trace continues from this call's span, not beside it.
        let parent = doc_telemetry::traceparent();
        for (name, value) in &request.headers {
            if parent.is_some() && name.eq_ignore_ascii_case(header::TRACEPARENT) {
                continue;
            }
            builder = builder.header(name, value);
        }
        if let Some(parent) = parent {
            builder = builder.header(header::TRACEPARENT, parent);
        }
        let head = builder
            .body(())
            .map_err(|err| CallError::Malformed(self.id.clone(), err.to_string()))?;
        let mut stream = self
            .client
            .open(head)
            .await
            .map_err(|err| CallError::Unreachable(self.id.clone(), err.to_string()))?;
        if !request.body.is_empty() {
            stream.send_data(request.body.clone()).await.map_err(unreachable)?;
        }
        stream.finish().await.map_err(unreachable)?;
        let response = stream.recv_response().await.map_err(unreachable)?;
        let mut body = BytesMut::new();
        while let Some(chunk) = stream.recv_data().await.map_err(unreachable)? {
            body.extend_from_slice(&chunk);
        }
        if response.status() == StatusCode::SERVICE_UNAVAILABLE && body.is_empty() {
            return Err(CallError::NotReady(self.id.clone()));
        }
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
            })
            .collect();
        Ok(Answer { status: response.status(), headers, body: body.freeze() })
    }
}

#[async_trait]
impl PluginClient for H3PluginClient {
    async fn load(&self, previous: Option<Value>, context: &str) -> Result<(), CallError> {
        let body = serde_json::to_value(LoadRequest { previous })
            .map_err(|err| CallError::Malformed(self.id.clone(), err.to_string()))?;
        let _: Value =
            self.post(host::LOAD, &body, Some(LOAD_DEADLINE), Some(context), None).await?;
        Ok(())
    }

    /// An empty answer means nothing was carried, which is not a failure: a plugin with no state
    /// to hand over has nothing to say.
    async fn unload(&self, context: &str) -> Result<Option<Value>, CallError> {
        let answer: Option<UnloadResponse> =
            self.post(host::UNLOAD, &Value::Null, Some(CALL_DEADLINE), Some(context), None).await?;
        Ok(answer.and_then(|answer| answer.state))
    }

    async fn cancel(&self, context: &str) -> Result<(), CallError> {
        let _: Value =
            self.post(host::CANCEL, &Value::Null, Some(CALL_DEADLINE), Some(context), None).await?;
        Ok(())
    }

    async fn run(
        &self,
        input: &RunInput,
        caller: &Caller,
        context: &str,
        deadline: Option<Duration>,
    ) -> Result<RunOutput, CallError> {
        let body = serde_json::to_value(input)
            .map_err(|err| CallError::Malformed(self.id.clone(), err.to_string()))?;
        self.post(host::RUN, &body, deadline, Some(context), Some(caller)).await
    }

    async fn request(
        &self,
        request: &Forwarded,
        caller: &Caller,
        context: &str,
        deadline: Duration,
    ) -> Result<Answer, CallError> {
        tokio::time::timeout(deadline, self.exchange(request, caller, context, deadline))
            .await
            .map_err(|_| CallError::Deadline(self.id.clone()))?
    }

    async fn event(&self, event: &Event, context: &str) -> Result<(), CallError> {
        let body = serde_json::to_value(event)
            .map_err(|err| CallError::Malformed(self.id.clone(), err.to_string()))?;
        let _: Value =
            self.post(host::EVENT, &body, Some(EVENT_DEADLINE), Some(context), None).await?;
        Ok(())
    }

    async fn settings_check(
        &self,
        check: &SettingsCheck,
        context: &str,
    ) -> Result<SettingsVerdict, CallError> {
        let body = serde_json::to_value(check)
            .map_err(|err| CallError::Malformed(self.id.clone(), err.to_string()))?;
        let answer: Option<SettingsVerdict> = self
            .post(host::SETTINGS_CHECK, &body, Some(SETTINGS_DEADLINE), Some(context), None)
            .await?;
        Ok(answer.unwrap_or_default())
    }

    async fn settings_changed(
        &self,
        changed: &SettingsChanged,
        context: &str,
    ) -> Result<(), CallError> {
        let body = serde_json::to_value(changed)
            .map_err(|err| CallError::Malformed(self.id.clone(), err.to_string()))?;
        let _: Value = self
            .post(host::SETTINGS_CHANGED, &body, Some(CALL_DEADLINE), Some(context), None)
            .await?;
        Ok(())
    }

    async fn health(&self) -> Result<PluginState, CallError> {
        let answer: Value =
            self.post(host::HEALTH, &Value::Null, Some(HEALTH_DEADLINE), None, None).await?;
        serde_json::from_value(answer.get("state").cloned().unwrap_or(Value::Null))
            .map_err(|err| CallError::Malformed(self.id.clone(), err.to_string()))
    }

    async fn exit(&self) -> Result<(), CallError> {
        let _: Value = self.post(host::EXIT, &Value::Null, Some(EXIT_DEADLINE), None, None).await?;
        Ok(())
    }
}

/// Puts every call but `health` in a span tagged with the plugin's ID, version and function.
pub struct Traced {
    id: String,
    version: String,
    inner: Arc<dyn PluginClient>,
}

impl Traced {
    pub fn wrap(id: &str, version: &str, inner: Arc<dyn PluginClient>) -> Arc<dyn PluginClient> {
        Arc::new(Self { id: id.to_string(), version: version.to_string(), inner })
    }

    fn span(&self, function: &str) -> tracing::Span {
        tracing::info_span!(
            target: "doc",
            "plugin.call",
            otel.name = %format!("{} {function}", self.id),
            otel.kind = "client",
            otel.status_description = tracing::field::Empty,
            doc.plugin.id = %self.id,
            doc.plugin.version = %self.version,
            doc.plugin.function = function,
            http.response.status_code = tracing::field::Empty,
        )
    }

    async fn traced<T>(
        &self,
        function: &str,
        call: impl Future<Output = Result<T, CallError>>,
    ) -> Result<T, CallError> {
        let span = self.span(function);
        let result = call.instrument(span.clone()).await;
        if let Err(err) = &result {
            span.record("otel.status_description", tracing::field::display(err));
        }
        result
    }
}

#[async_trait]
impl PluginClient for Traced {
    async fn load(&self, previous: Option<Value>, context: &str) -> Result<(), CallError> {
        self.traced("load", self.inner.load(previous, context)).await
    }

    async fn unload(&self, context: &str) -> Result<Option<Value>, CallError> {
        self.traced("unload", self.inner.unload(context)).await
    }

    async fn cancel(&self, context: &str) -> Result<(), CallError> {
        self.traced("cancel", self.inner.cancel(context)).await
    }

    async fn run(
        &self,
        input: &RunInput,
        caller: &Caller,
        context: &str,
        deadline: Option<Duration>,
    ) -> Result<RunOutput, CallError> {
        self.traced("run", self.inner.run(input, caller, context, deadline)).await
    }

    async fn request(
        &self,
        request: &Forwarded,
        caller: &Caller,
        context: &str,
        deadline: Duration,
    ) -> Result<Answer, CallError> {
        let function = request.path.split('/').next().unwrap_or_default();
        let span = self.span(function);
        let answer =
            self.inner.request(request, caller, context, deadline).instrument(span.clone()).await;
        match &answer {
            Ok(answer) => {
                span.record("http.response.status_code", answer.status.as_u16());
                if answer.status.is_server_error() {
                    span.record("otel.status_description", answer.status.as_str());
                }
            }
            Err(err) => {
                span.record("otel.status_description", tracing::field::display(err));
            }
        }
        answer
    }

    async fn event(&self, event: &Event, context: &str) -> Result<(), CallError> {
        self.traced("event", self.inner.event(event, context)).await
    }

    async fn settings_check(
        &self,
        check: &SettingsCheck,
        context: &str,
    ) -> Result<SettingsVerdict, CallError> {
        self.traced("settings-check", self.inner.settings_check(check, context)).await
    }

    async fn settings_changed(
        &self,
        changed: &SettingsChanged,
        context: &str,
    ) -> Result<(), CallError> {
        self.traced("settings-changed", self.inner.settings_changed(changed, context)).await
    }

    async fn health(&self) -> Result<PluginState, CallError> {
        self.inner.health().await
    }

    async fn exit(&self) -> Result<(), CallError> {
        self.traced("exit", self.inner.exit()).await
    }
}

/// What a registry with no transport uses. A plugin can still register against it, and then goes
/// straight into `error`, which is the truth: nothing can reach it.
pub struct RefusingConnector;

#[async_trait]
impl Connector for RefusingConnector {
    async fn connect(
        &self,
        _id: &str,
        _address: &str,
        _secret: &str,
    ) -> Result<Arc<dyn PluginClient>, String> {
        Err("this backend has no plugin transport".into())
    }
}

/// The fake the endpoint tests register against, so the whole state machine can be driven without
/// a plugin process.
#[derive(Default)]
pub struct FakePlugin {
    pub loaded: std::sync::atomic::AtomicU32,
    pub cancelled: std::sync::atomic::AtomicU32,
    pub unloaded: std::sync::atomic::AtomicU32,
    pub exited: std::sync::atomic::AtomicU32,
    /// When each call was answered, so a test can put calls to two fakes in order.
    journal: parking_lot::Mutex<Vec<(&'static str, tokio::time::Instant)>>,
    failure: parking_lot::Mutex<Option<String>>,
    carries: parking_lot::Mutex<Option<Value>>,
    /// What the last `load` was handed, which is what a handover test looks at.
    seen: parking_lot::Mutex<Option<Value>>,
    events: parking_lot::Mutex<Vec<Event>>,
    /// Refuses this many deliveries before accepting, which is how a test sees the retries.
    event_failures: std::sync::atomic::AtomicU32,
    /// Every context token handed over, and whom it resolved to while its call was running.
    contexts: parking_lot::Mutex<Vec<(String, Option<String>)>>,
    resolver: parking_lot::Mutex<Option<(String, Arc<super::context::Contexts>)>>,
    runs: parking_lot::Mutex<Vec<(RunInput, Caller)>>,
    run_failure: parking_lot::Mutex<Option<String>>,
    /// Loses the connection under this many runs, the way a host that stalls or sleeps does.
    cut_runs: std::sync::atomic::AtomicU32,
    /// Keeps each run going until `cancel`, the way a long-running plugin's does.
    holding: std::sync::atomic::AtomicBool,
    cancels: tokio::sync::watch::Sender<u64>,
    requests: parking_lot::Mutex<Vec<(Forwarded, Caller)>>,
    answer: parking_lot::Mutex<Option<Answer>>,
    /// How long a run or a request takes, which is how a test passes a deadline.
    slowness: parking_lot::Mutex<Option<Duration>>,
    /// What the fake says of proposed settings, and every set it was shown (ADR-0007).
    verdict: parking_lot::Mutex<Option<SettingsVerdict>>,
    checked: parking_lot::Mutex<Vec<SettingsCheck>>,
    told: parking_lot::Mutex<Vec<SettingsChanged>>,
}

impl FakePlugin {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Makes every call fail, which is how a test drives `loading` to `error`.
    pub fn set_failing(&self, reason: Option<&str>) {
        *self.failure.lock() = reason.map(str::to_string);
    }

    pub fn set_carries(&self, carries: Option<Value>) {
        *self.carries.lock() = carries;
    }

    pub fn saw_previous(&self) -> Option<Value> {
        self.seen.lock().clone()
    }

    pub fn fail_events(&self, times: u32) {
        self.event_failures.store(times, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn events(&self) -> Vec<Event> {
        self.events.lock().clone()
    }

    /// Resolves each context as it arrives, so a test can see it worked during its call.
    pub fn resolve_with(&self, plugin: &str, contexts: Arc<super::context::Contexts>) {
        *self.resolver.lock() = Some((plugin.to_string(), contexts));
    }

    pub fn contexts(&self) -> Vec<(String, Option<String>)> {
        self.contexts.lock().clone()
    }

    pub fn runs(&self) -> Vec<(RunInput, Caller)> {
        self.runs.lock().clone()
    }

    pub fn fail_runs(&self, reason: Option<&str>) {
        *self.run_failure.lock() = reason.map(str::to_string);
    }

    pub fn cut_runs(&self, runs: u32) {
        self.cut_runs.store(runs, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn hold_runs(&self, holding: bool) {
        self.holding.store(holding, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn requests(&self) -> Vec<(Forwarded, Caller)> {
        self.requests.lock().clone()
    }

    pub fn answer_with(&self, answer: Answer) {
        *self.answer.lock() = Some(answer);
    }

    pub fn slow_down(&self, by: Option<Duration>) {
        *self.slowness.lock() = by;
    }

    /// Makes the fake object to proposed settings, the way a plugin whose credential is refused does.
    pub fn judge_settings_with(&self, verdict: Option<SettingsVerdict>) {
        *self.verdict.lock() = verdict;
    }

    pub fn settings_checked(&self) -> Vec<SettingsCheck> {
        self.checked.lock().clone()
    }

    pub fn settings_told(&self) -> Vec<SettingsChanged> {
        self.told.lock().clone()
    }

    /// When `what` (`load`, `unload`, `request` or `exit`) was first answered.
    pub fn answered(&self, what: &str) -> Option<tokio::time::Instant> {
        self.journal.lock().iter().find(|(call, _)| *call == what).map(|(_, at)| *at)
    }

    fn note(&self, what: &'static str) {
        self.journal.lock().push((what, tokio::time::Instant::now()));
    }

    async fn dawdle(&self) {
        let slowness = *self.slowness.lock();
        if let Some(slowness) = slowness {
            tokio::time::sleep(slowness).await;
        }
    }

    fn saw_context(&self, token: &str) {
        let resolved = self.resolver.lock().as_ref().and_then(|(plugin, contexts)| {
            contexts.resolve(plugin, token).map(|principal| principal.reference())
        });
        self.contexts.lock().push((token.to_string(), resolved));
    }

    fn refuse(&self) -> Option<CallError> {
        self.failure.lock().clone().map(|reason| CallError::Refused("fake".into(), 500, reason))
    }
}

#[async_trait]
impl PluginClient for FakePlugin {
    async fn load(&self, previous: Option<Value>, context: &str) -> Result<(), CallError> {
        self.saw_context(context);
        self.dawdle().await;
        if let Some(err) = self.refuse() {
            return Err(err);
        }
        *self.seen.lock() = previous;
        self.loaded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.note("load");
        Ok(())
    }

    /// Stops any run still going first, as the SDK does, since `unload` needs the plugin alone.
    async fn unload(&self, context: &str) -> Result<Option<Value>, CallError> {
        self.saw_context(context);
        if let Some(err) = self.refuse() {
            return Err(err);
        }
        self.cancels.send_modify(|count| *count += 1);
        self.unloaded.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.note("unload");
        Ok(self.carries.lock().clone())
    }

    async fn cancel(&self, context: &str) -> Result<(), CallError> {
        self.saw_context(context);
        if let Some(err) = self.refuse() {
            return Err(err);
        }
        self.cancelled.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.cancels.send_modify(|count| *count += 1);
        Ok(())
    }

    async fn run(
        &self,
        input: &RunInput,
        caller: &Caller,
        context: &str,
        _deadline: Option<Duration>,
    ) -> Result<RunOutput, CallError> {
        self.saw_context(context);
        let mut cancelled = self.cancels.subscribe();
        cancelled.mark_unchanged();
        self.runs.lock().push((input.clone(), caller.clone()));
        self.dawdle().await;
        let cut = self.cut_runs.fetch_update(
            std::sync::atomic::Ordering::SeqCst,
            std::sync::atomic::Ordering::SeqCst,
            |left| left.checked_sub(1),
        );
        if cut.is_ok() {
            return Err(CallError::Unreachable("fake".into(), "the connection closed".into()));
        }
        if self.holding.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = cancelled.changed().await;
            let stopped = json!({ "detail": "cancelled" }).to_string();
            return Err(CallError::Refused("fake".into(), 500, stopped));
        }
        if let Some(reason) = self.run_failure.lock().clone() {
            return Err(CallError::Refused(
                "fake".into(),
                500,
                json!({ "detail": reason }).to_string(),
            ));
        }
        Ok(RunOutput { payload: json!({ "ran": input.payload }) })
    }

    async fn request(
        &self,
        request: &Forwarded,
        caller: &Caller,
        context: &str,
        _deadline: Duration,
    ) -> Result<Answer, CallError> {
        self.saw_context(context);
        self.requests.lock().push((request.clone(), caller.clone()));
        self.dawdle().await;
        self.note("request");
        let answer = self.answer.lock().clone();
        Ok(answer.unwrap_or_else(|| {
            Answer::json(StatusCode::OK, &json!({ "path": request.path, "caller": caller }))
        }))
    }

    async fn event(&self, event: &Event, context: &str) -> Result<(), CallError> {
        use std::sync::atomic::Ordering;
        self.saw_context(context);
        let refusing =
            self.event_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |left| left.checked_sub(1));
        if refusing.is_ok() {
            return Err(CallError::Refused("fake".into(), 500, "not yet".into()));
        }
        self.events.lock().push(event.clone());
        Ok(())
    }

    async fn settings_check(
        &self,
        check: &SettingsCheck,
        context: &str,
    ) -> Result<SettingsVerdict, CallError> {
        self.saw_context(context);
        self.checked.lock().push(check.clone());
        Ok(self.verdict.lock().clone().unwrap_or_default())
    }

    async fn settings_changed(
        &self,
        changed: &SettingsChanged,
        context: &str,
    ) -> Result<(), CallError> {
        self.saw_context(context);
        self.told.lock().push(changed.clone());
        Ok(())
    }

    async fn health(&self) -> Result<PluginState, CallError> {
        match self.refuse() {
            Some(err) => Err(err),
            None => Ok(PluginState::Running),
        }
    }

    async fn exit(&self) -> Result<(), CallError> {
        self.exited.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.note("exit");
        Ok(())
    }
}

/// Hands every registration the same `FakePlugin`, unless a test queued one of its own.
pub struct FakeConnector {
    plugin: Arc<FakePlugin>,
    queued: parking_lot::Mutex<std::collections::VecDeque<Arc<FakePlugin>>>,
    refuse: parking_lot::Mutex<Option<String>>,
}

impl FakeConnector {
    pub fn new(plugin: Arc<FakePlugin>) -> Arc<Self> {
        Arc::new(Self {
            plugin,
            queued: parking_lot::Mutex::new(std::collections::VecDeque::new()),
            refuse: parking_lot::Mutex::new(None),
        })
    }

    /// The process the next registration dials, so a handover has two distinct ones.
    pub fn hand_out(&self, plugin: Arc<FakePlugin>) {
        self.queued.lock().push_back(plugin);
    }

    pub fn set_unreachable(&self, reason: Option<&str>) {
        *self.refuse.lock() = reason.map(str::to_string);
    }
}

#[async_trait]
impl Connector for FakeConnector {
    async fn connect(
        &self,
        _id: &str,
        _address: &str,
        _secret: &str,
    ) -> Result<Arc<dyn PluginClient>, String> {
        match self.refuse.lock().clone() {
            Some(reason) => Err(reason),
            None => Ok(self.queued.lock().pop_front().unwrap_or_else(|| self.plugin.clone())),
        }
    }
}
