//! The plugin runtime: an HTTP/3 endpoint for the backend's calls, a client for the backend API,
//! registration that keeps retrying, liveness reports, and exit when the backend says so. Every call
//! must carry the per-instance secret from registration, so an unregistered process refuses all.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use doc_plugin_protocol::calls::{SettingsChanged, SettingsCheck};
use doc_plugin_protocol::{
    Caller, Classification, Liveness, LoadRequest, PluginState, RegisterRequest, RegisterResponse,
    RunInput, Secret, UnloadResponse, backend as backend_paths, header, host,
};
use doc_transport::{
    EndpointConfig, H3Client, H3ServerStream, bearer, endpoint, read_body, respond, serve,
};
use http::{Method, StatusCode};
use serde_json::{Value, json};
use tokio::sync::{RwLock, RwLockWriteGuard, Semaphore, watch};
use tracing::Instrument;
use uuid::Uuid;

use crate::backend::Inner;
use crate::{Backend, Build, Plugin, PluginError, Request, Response};

const REGISTER_DEADLINE: Duration = Duration::from_secs(10);
const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(15);
const LIVENESS: Duration = Duration::from_secs(5);
/// A settings reload runs `load`, which the backend gives a minute; a long-running run waits that long.
const RELOAD_WAIT: Duration = Duration::from_secs(60);
/// Bounds how many of the backend's calls run at once, so one slow route cannot swamp the process.
const WORKERS: usize = 32;
/// What the backend answers a liveness report with when it has no record of this plugin.
const UNAUTHORIZED: u16 = 401;
const NOT_FOUND: u16 = 404;
/// What it answers when a newer registration has replaced this process.
const GONE: u16 = 410;

struct Config {
    token: Secret<String>,
    secrets: PathBuf,
    backend: SocketAddr,
    bind: SocketAddr,
    advertise: String,
}

fn config(id: &str) -> Result<Config> {
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());
    let secrets = PathBuf::from(var("DOC_SECRETS_DIR").unwrap_or_else(|| "/secrets".into()));
    let token = match var("DOC_PLUGIN_TOKEN") {
        Some(token) => token,
        None => {
            let path = secrets.join(format!("tokens/plugins/{id}.token"));
            std::fs::read_to_string(&path)
                .with_context(|| format!("reading the registration token from {}", path.display()))?
                .trim()
                .to_string()
        }
    };
    let backend = var("DOC_BACKEND_QUIC").unwrap_or_else(|| "backend:4433".into());
    let backend = resolve(&backend).with_context(|| format!("resolving {backend}"))?;
    let bind: SocketAddr =
        var("DOC_PLUGIN_BIND").unwrap_or_else(|| "0.0.0.0:4440".into()).parse()?;
    // A container's hostname is its own, so two versions side by side during a handover differ.
    let host = var("HOSTNAME").unwrap_or_else(|| format!("plugin-{id}"));
    let advertise =
        var("DOC_PLUGIN_ADVERTISE").unwrap_or_else(|| format!("{host}:{}", bind.port()));
    Ok(Config { token: Secret::new(token), secrets, backend, bind, advertise })
}

fn resolve(address: &str) -> Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("{address} resolved to nothing"))
}

/// The binary's own SHA-256. The backend refuses a version it has seen before with a different one.
fn binary_hash() -> Result<String> {
    use sha2::Digest;
    let path = std::env::current_exe().context("finding this binary")?;
    let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(hex::encode(sha2::Sha256::digest(&bytes)))
}

struct Runtime<P: Plugin> {
    plugin: RwLock<P>,
    backend: Backend,
    build: Build,
    secret: RwLock<Option<Secret<String>>>,
    instance: RwLock<Option<Uuid>>,
    state: RwLock<PluginState>,
    /// Held shared by every `run` in progress, so `load` and `unload` can wait for the last one.
    runs: RwLock<()>,
    /// Counts the reloads this process makes of itself for changed settings, so a long-running
    /// `run` they end can tell that from being cancelled.
    reloads: std::sync::atomic::AtomicU64,
    permits: Arc<Semaphore>,
    stop: watch::Sender<bool>,
    /// Set by `exit`, and acted on once the answer is on the wire rather than tearing it down.
    stopping: std::sync::atomic::AtomicBool,
    /// Kept so liveness reporting can register again when a restarted backend has no record.
    advertise: String,
    hash: String,
}

impl<P: Plugin> Runtime<P> {
    async fn authorised(&self, presented: Option<&str>) -> bool {
        match (self.secret.read().await.as_ref(), presented) {
            (Some(secret), Some(presented)) => secret.matches(presented),
            _ => false,
        }
    }

    async fn set_state(&self, next: PluginState) {
        *self.state.write().await = next;
    }
}

/// Serves the backend's calls until it says to exit, then returns so the process can.
pub fn launch<P: Plugin + Default>(build: Build) -> ExitCode {
    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("could not start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(run::<P>(build)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            tracing::error!(error = %format!("{err:#}"), "the plugin stopped");
            ExitCode::FAILURE
        }
    }
}

async fn run<P: Plugin + Default>(build: Build) -> Result<()> {
    let id = build.manifest.id.clone();
    let _telemetry = crate::telemetry::init(&id, build.version);
    doc_transport::install_crypto();

    let config = config(&id)?;
    let hash = binary_hash()?;
    tracing::info!(
        plugin = %id,
        version = %build.version,
        bind = %config.bind,
        backend = %config.backend,
        "plugin starting"
    );

    let (listener, report) =
        endpoint(&EndpointConfig::server(config.bind, &config.secrets, format!("plugin-{id}")))
            .context("opening the plugin's QUIC endpoint")?;
    tracing::debug!(?report, "socket buffers");
    let client_endpoint =
        endpoint(&EndpointConfig::client(&config.secrets)).context("opening a client endpoint")?.0;
    let client = H3Client::new(client_endpoint, config.backend, "backend");

    let inner = Arc::new(Inner {
        client,
        token: config.token.clone(),
        id: id.clone(),
        version: build.version.to_string(),
        settings: std::sync::RwLock::new(Arc::new(crate::Settings::default())),
    });
    let (stop, mut stopped) = watch::channel(false);
    let runtime = Arc::new(Runtime::<P> {
        plugin: RwLock::new(P::default()),
        backend: Backend::new(inner),
        build,
        secret: RwLock::new(None),
        instance: RwLock::new(None),
        state: RwLock::new(PluginState::Loading),
        runs: RwLock::new(()),
        reloads: std::sync::atomic::AtomicU64::new(0),
        permits: Arc::new(Semaphore::new(WORKERS)),
        stop,
        stopping: std::sync::atomic::AtomicBool::new(false),
        advertise: config.advertise.clone(),
        hash: hash.clone(),
    });

    let serving = runtime.clone();
    tokio::spawn(serve(listener, move |request, stream, peer| {
        let runtime = serving.clone();
        async move { dispatch(runtime, request, stream, peer).await }
    }));

    let registered = register(runtime.clone()).await?;
    tracing::info!(plugin = %id, state = %registered.state.as_str(), "registered with the backend");
    liveness(runtime.clone());

    let _ = stopped.changed().await;
    tracing::info!(plugin = %id, "exiting");
    Ok(())
}

/// Keeps trying until a backend answers. A plugin started before the backend is normal, not an
/// error, so this backs off and says so rather than exiting.
async fn register<P: Plugin>(runtime: Arc<Runtime<P>>) -> Result<RegisterResponse> {
    let request = RegisterRequest {
        manifest: runtime.build.manifest.clone(),
        address: runtime.advertise.clone(),
        binary_sha256: runtime.hash.clone(),
        started_at: Some(chrono::Utc::now()),
    };
    let mut wait = RETRY_MIN;
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        let call = runtime.backend.call::<_, RegisterResponse>(
            backend_paths::REGISTER,
            &request,
            REGISTER_DEADLINE,
        );
        match call.await {
            Ok(response) => {
                *runtime.secret.write().await = Some(response.secret.clone());
                *runtime.instance.write().await = response.instance;
                runtime.set_state(response.state).await;
                return Ok(response);
            }
            Err(err) => {
                tracing::warn!(
                    attempt,
                    retry_in_ms = wait.as_millis() as u64,
                    error = %err,
                    "registration did not succeed yet"
                );
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(RETRY_MAX);
            }
        }
    }
}

/// Missed reports put the plugin in `error`; a `404` means the backend forgot it, and `410` replaced it.
fn liveness<P: Plugin>(runtime: Arc<Runtime<P>>) {
    tokio::spawn(async move {
        let mut refused = false;
        loop {
            tokio::time::sleep(LIVENESS).await;
            // Never waits on the plugin, or a slow `load` would stop these reports and read as dead.
            let error = runtime.plugin.try_read().ok().and_then(|plugin| plugin.error());
            let state = *runtime.state.read().await;
            let report = Liveness {
                id: runtime.build.manifest.id.clone(),
                state,
                error,
                instance: *runtime.instance.read().await,
            };
            let call = runtime.backend.call::<_, Value>(backend_paths::LIVENESS, &report, LIVENESS);
            match call.await {
                Ok(_) => refused = false,
                Err(PluginError::Refused { status, .. }) if status == UNAUTHORIZED => {
                    if !std::mem::replace(&mut refused, true) {
                        tracing::warn!("the backend refuses this plugin's registration token");
                    }
                }
                Err(PluginError::Refused { status, .. })
                    if status == GONE
                        || (status == NOT_FOUND && state == PluginState::Unloading) =>
                {
                    tracing::info!(status, "the backend has moved on from this process; exiting");
                    let _ = runtime.stop.send(true);
                    return;
                }
                Err(PluginError::Refused { status, .. }) if status == NOT_FOUND => {
                    tracing::info!("the backend has no record of this plugin; registering again");
                    *runtime.secret.write().await = None;
                    match register(runtime.clone()).await {
                        Ok(response) => tracing::info!(
                            state = %response.state.as_str(),
                            "registered with the backend again"
                        ),
                        Err(err) => tracing::warn!(%err, "registering again did not succeed"),
                    }
                }
                Err(err) => tracing::debug!(%err, "a liveness report did not reach the backend"),
            }
        }
    });
}

async fn dispatch<P: Plugin>(
    runtime: Arc<Runtime<P>>,
    request: http::Request<()>,
    mut stream: H3ServerStream,
    _peer: SocketAddr,
) -> Result<()> {
    let path = request.uri().path().to_string();
    if !path.starts_with(host::PREFIX) {
        return respond(&mut stream, StatusCode::NOT_FOUND, Bytes::new()).await;
    }
    if !runtime.authorised(bearer(&request)).await {
        // Before registration there is no secret, so every call is refused: nothing else has the
        // right to drive this process.
        return respond(&mut stream, StatusCode::SERVICE_UNAVAILABLE, Bytes::new()).await;
    }
    let Ok(permit) = runtime.permits.clone().try_acquire_owned() else {
        return respond(&mut stream, StatusCode::TOO_MANY_REQUESTS, Bytes::new()).await;
    };
    let deadline = request
        .headers()
        .get(header::DEADLINE_MS)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok())
        .map(Duration::from_millis);
    let caller = request
        .headers()
        .get(header::CALLER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| serde_json::from_str::<Caller>(value).ok());
    let context = request
        .headers()
        .get(header::CONTEXT)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let body = read_body(&mut stream).await?;
    let span = traced(&runtime, &path, doc_telemetry::header(request.headers()));

    // Its own task, so a panic fails this one call rather than the connection it arrived on.
    let handled = handle(runtime.clone(), request, path.clone(), body, context, caller);
    let work = tokio::spawn(handled.instrument(span.clone()));
    let abort = work.abort_handle();
    let finished = match deadline {
        Some(deadline) => tokio::time::timeout(deadline, work).await.ok(),
        None => Some(work.await),
    };
    let answer = match finished {
        Some(Ok(answer)) => answer,
        Some(Err(err)) => panicked(&runtime, &path, err).await,
        None => {
            abort.abort();
            Response::problem(504, "deadline", "the plugin did not answer in time")
        }
    };
    drop(permit);
    span.record("http.response.status_code", answer.status);
    if answer.status >= 500 {
        let problem: String = String::from_utf8_lossy(&answer.body).chars().take(300).collect();
        span.record("otel.status_description", problem.as_str());
    }

    let mut builder = http::Response::builder().status(answer.status);
    for (name, value) in &answer.headers {
        builder = builder.header(name, value);
    }
    stream.send_response(builder.body(())?).await?;
    if !answer.body.is_empty() {
        stream.send_data(answer.body).await?;
    }
    let finished = stream.finish().await;
    if runtime.stopping.load(std::sync::atomic::Ordering::Acquire) {
        let _ = runtime.stop.send(true);
    }
    finished
}

/// A span for each of the backend's calls but `health`, continuing the backend's trace.
fn traced<P: Plugin>(runtime: &Runtime<P>, path: &str, parent: Option<&str>) -> tracing::Span {
    let called = path.trim_start_matches(host::PREFIX).trim_start_matches('/');
    let function = match called.strip_prefix("request/") {
        Some(route) => route.split('/').next().unwrap_or_default(),
        None => called,
    };
    if path == host::HEALTH {
        return tracing::Span::none();
    }
    let id = &runtime.build.manifest.id;
    let span = tracing::info_span!(
        target: "doc",
        "plugin.handle",
        otel.name = %format!("{id} {function}"),
        otel.kind = "server",
        otel.status_description = tracing::field::Empty,
        doc.plugin.id = %id,
        doc.plugin.version = runtime.build.version,
        doc.plugin.function = %function,
        http.response.status_code = tracing::field::Empty,
    );
    doc_telemetry::adopt(&span, parent);
    span
}

/// Turns a panic into this call's answer; a panicking `load` or `unload` leaves the plugin in error.
async fn panicked<P: Plugin>(
    runtime: &Runtime<P>,
    path: &str,
    err: tokio::task::JoinError,
) -> Response {
    let message = match err.try_into_panic() {
        Ok(payload) => payload
            .downcast_ref::<&str>()
            .map(|message| (*message).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "no message".into()),
        Err(err) => err.to_string(),
    };
    tracing::error!(path, %message, "a call panicked");
    if path == host::LOAD || path == host::UNLOAD {
        runtime.set_state(PluginState::Error).await;
    }
    Response::problem(500, "panicked", &format!("panicked: {message}"))
}

/// Asks any run still going to stop and waits for it, since `load` and `unload` need the plugin alone.
async fn drained<'a, P: Plugin>(
    runtime: &'a Runtime<P>,
    backend: &Backend,
) -> RwLockWriteGuard<'a, ()> {
    if let Ok(idle) = runtime.runs.try_write() {
        return idle;
    }
    if let Err(err) = runtime.plugin.read().await.cancel(backend).await {
        tracing::warn!(%err, "a run in progress could not be cancelled");
    }
    runtime.runs.write().await
}

async fn handle<P: Plugin>(
    runtime: Arc<Runtime<P>>,
    request: http::Request<()>,
    path: String,
    body: Bytes,
    context: Option<String>,
    caller: Option<Caller>,
) -> Response {
    let backend = runtime.backend.for_call(context, caller);
    match path.as_str() {
        host::HEALTH => {
            let state = *runtime.state.read().await;
            Response::json(&json!({
                "id": runtime.build.manifest.id,
                "version": runtime.build.version,
                "state": state,
            }))
        }
        host::LOAD => {
            let previous = serde_json::from_slice::<LoadRequest>(&body)
                .map(|request| request.previous)
                .unwrap_or_default();
            runtime.set_state(PluginState::Loading).await;
            // Settings are read before `load`, so a plugin has them the moment it starts and
            // never has to ask for them itself (ADR-0007).
            if let Err(err) = backend.refresh_settings().await {
                tracing::warn!(%err, "the settings could not be read; the declared defaults apply");
            }
            let idle = drained(&runtime, &backend).await;
            // The plugin is released before the state is touched: holding one while waiting for
            // the other is a lock-order deadlock with anything that takes them the other way round.
            let loaded = runtime.plugin.write().await.load(&backend, previous).await;
            drop(idle);
            match loaded {
                Ok(()) => {
                    runtime.set_state(PluginState::Running).await;
                    Response::json(&json!({ "state": PluginState::Running }))
                }
                Err(err) => {
                    runtime.set_state(PluginState::Error).await;
                    Response::problem(500, "load-failed", &err.to_string())
                }
            }
        }
        host::UNLOAD => {
            runtime.set_state(PluginState::Unloading).await;
            let idle = drained(&runtime, &backend).await;
            let unloaded = runtime.plugin.write().await.unload(&backend).await;
            drop(idle);
            // The process stays up, so a failed handover can load it again, until told to exit.
            match unloaded {
                Ok(state) => Response::json(&UnloadResponse { state }),
                Err(err) => {
                    runtime.set_state(PluginState::Error).await;
                    Response::problem(500, "unload-failed", &err.to_string())
                }
            }
        }
        host::EXIT => {
            runtime.stopping.store(true, std::sync::atomic::Ordering::Release);
            Response { status: 204, headers: Vec::new(), body: Bytes::new() }
        }
        host::RUN => {
            let input = serde_json::from_slice::<RunInput>(&body).unwrap_or_default();
            let long = input.task.is_none()
                && matches!(runtime.build.manifest.classification, Classification::LongRunning);
            loop {
                let reloads = runtime.reloads.load(std::sync::atomic::Ordering::Acquire);
                let ran = {
                    let _running = runtime.runs.read().await;
                    // Checked once the run is counted, so one cannot slip in after `unload` has drained.
                    if *runtime.state.read().await == PluginState::Unloading {
                        return Response::problem(503, "unavailable", "unloading");
                    }
                    runtime.plugin.read().await.run(&backend, input.clone()).await
                };
                // The backend takes the end of a long-running run as the plugin failing, so one a
                // settings reload in this process ended starts again once the plugin has loaded.
                if long
                    && runtime.reloads.load(std::sync::atomic::Ordering::Acquire) != reloads
                    && reloaded(&runtime).await
                {
                    tracing::info!("the long-running run starts again after the settings reload");
                    continue;
                }
                return match ran {
                    Ok(output) => Response::json(&output),
                    Err(err) => Response::problem(500, "run-failed", &err.to_string()),
                };
            }
        }
        host::CANCEL => {
            let cancelled = runtime.plugin.read().await.cancel(&backend).await;
            match cancelled {
                Ok(()) => {
                    runtime.set_state(PluginState::Cancelled).await;
                    Response::json(&json!({ "state": PluginState::Cancelled }))
                }
                Err(err) => Response::problem(500, "cancel-failed", &err.to_string()),
            }
        }
        host::SETTINGS_CHECK => {
            let proposed = match serde_json::from_slice::<SettingsCheck>(&body) {
                Ok(proposed) => crate::Settings::proposed(proposed),
                Err(err) => return Response::problem(400, "bad-request", &err.to_string()),
            };
            let verdict = runtime.plugin.read().await.settings_check(&backend, &proposed).await;
            Response::json(&verdict)
        }
        host::SETTINGS_CHANGED => {
            let changed = serde_json::from_slice::<SettingsChanged>(&body).unwrap_or_default();
            if let Err(err) = backend.refresh_settings().await {
                tracing::warn!(%err, "the new settings could not be read");
            }
            let handled = runtime.plugin.read().await.settings_changed(&backend, &changed).await;
            match handled {
                // The default: the plugin is reloaded in this process, so whatever it did with
                // its old settings at `load` it does again with the new ones.
                Ok(false) => reload(&runtime, &backend).await,
                Ok(true) => Response::json(&json!({ "reloaded": false })),
                Err(err) => Response::problem(500, "settings-changed-failed", &err.to_string()),
            }
        }
        host::EVENT => match serde_json::from_slice(&body) {
            Ok(event) => match runtime.plugin.read().await.on_event(&backend, event).await {
                Ok(()) => Response::json(&json!({ "acknowledged": true })),
                Err(err) => Response::problem(500, "event-failed", &err.to_string()),
            },
            Err(err) => Response::problem(400, "bad-request", &err.to_string()),
        },
        _ if path.starts_with(host::REQUEST_PREFIX) => {
            let state = *runtime.state.read().await;
            if !state.serves_requests() {
                return Response::problem(503, "unavailable", state.as_str());
            }
            let forwarded = Request {
                method: request.method().as_str().to_string(),
                path: path[host::REQUEST_PREFIX.len()..].to_string(),
                query: request.uri().query().unwrap_or_default().to_string(),
                headers: request
                    .headers()
                    .iter()
                    .filter_map(|(name, value)| {
                        Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
                    })
                    .collect(),
                body,
            };
            runtime.plugin.read().await.handle(&backend, forwarded).await
        }
        _ => Response::not_found(),
    }
}

/// `unload` then `load` in this process, which is what a plugin does about changed settings
/// unless it says it has handled them itself (ADR-0007). What `unload` hands back is handed
/// straight to `load`, so the plugin keeps its place.
async fn reload<P: Plugin>(runtime: &Arc<Runtime<P>>, backend: &Backend) -> Response {
    runtime.reloads.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    runtime.set_state(PluginState::Loading).await;
    let idle = drained(runtime, backend).await;
    let carried = {
        let mut plugin = runtime.plugin.write().await;
        match plugin.unload(backend).await {
            Ok(carried) => carried,
            Err(err) => {
                drop(plugin);
                drop(idle);
                runtime.set_state(PluginState::Error).await;
                return Response::problem(500, "unload-failed", &err.to_string());
            }
        }
    };
    let loaded = runtime.plugin.write().await.load(backend, carried).await;
    drop(idle);
    match loaded {
        Ok(()) => {
            runtime.set_state(PluginState::Running).await;
            Response::json(&json!({ "reloaded": true }))
        }
        Err(err) => {
            runtime.set_state(PluginState::Error).await;
            Response::problem(500, "load-failed", &err.to_string())
        }
    }
}

/// Waits for a reload in this process to finish: true when the plugin is running again.
async fn reloaded<P: Plugin>(runtime: &Runtime<P>) -> bool {
    let until = tokio::time::Instant::now() + RELOAD_WAIT;
    loop {
        match *runtime.state.read().await {
            PluginState::Running => return true,
            PluginState::Loading if tokio::time::Instant::now() < until => {}
            _ => return false,
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// So a plugin that only ever answers `GET` still compiles against the whole surface.
pub fn is_read(method: &Method) -> bool {
    matches!(*method, Method::GET | Method::HEAD)
}
