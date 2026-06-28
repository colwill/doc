//! Hot reload (T23): the new version is prepared while the old one serves, then requests wait at
//! the gate while the old one drains and unloads, the new one loads with what it handed over, and
//! the old process exits, or is loaded again if the new one fails. `reload` (T25) does it in place.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use doc_plugin_protocol::PluginState;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::time::Instant;

use super::client::CallError;
use super::{Registered, TransitionError, client};
use crate::api::AppState;
use crate::identity::Principal;

const DRAIN_POLL: Duration = Duration::from_millis(10);

/// Holds a plugin's requests while a handover swaps its process, and counts those let through.
pub struct Gate {
    closed: AtomicBool,
    wake: watch::Sender<()>,
    in_flight: AtomicUsize,
}

impl Default for Gate {
    fn default() -> Self {
        Self {
            closed: AtomicBool::new(false),
            wake: watch::Sender::new(()),
            in_flight: AtomicUsize::new(0),
        }
    }
}

impl Gate {
    /// Counts the caller in before looking at the gate, so nothing slips past one being closed.
    pub async fn pass(self: &Arc<Self>, patience: Duration) -> Option<Pass> {
        let until = Instant::now() + patience;
        let mut wake = self.wake.subscribe();
        loop {
            self.in_flight.fetch_add(1, Ordering::SeqCst);
            if !self.closed.load(Ordering::SeqCst) {
                return Some(Pass(self.clone()));
            }
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            if !matches!(tokio::time::timeout_at(until, wake.changed()).await, Ok(Ok(()))) {
                return None;
            }
        }
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    pub(super) fn open(&self) {
        self.closed.store(false, Ordering::SeqCst);
        self.wake.send_replace(());
    }

    async fn drained(&self, patience: Duration) -> bool {
        let until = Instant::now() + patience;
        while self.in_flight.load(Ordering::SeqCst) > 0 {
            if Instant::now() >= until {
                return false;
            }
            tokio::time::sleep(DRAIN_POLL).await;
        }
        true
    }
}

/// A request in flight on its plugin, until it is dropped.
pub struct Pass(Arc<Gate>);

impl Drop for Pass {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Opens the gate and clears the way for the next registration however the handover ends.
struct Finished {
    state: AppState,
    id: String,
}

impl Drop for Finished {
    fn drop(&mut self) {
        self.state.plugins.unpend(&self.id);
        self.state.plugins.gate(&self.id).open();
    }
}

/// Tells a replaced process to exit, without waiting on it: it may already be gone.
pub(super) fn dismiss(entry: Registered) {
    tokio::spawn(async move {
        match entry.client.exit().await {
            Ok(()) => {
                tracing::info!(plugin = %entry.id, instance = %entry.instance, "told a replaced process to exit")
            }
            Err(err) => {
                tracing::debug!(plugin = %entry.id, %err, "a replaced process was not told to exit")
            }
        }
    });
}

/// A version that loaded but cannot be marked running was usually timed out by the plugin probe.
async fn not_running(state: &AppState, new: &Registered, err: &TransitionError) -> String {
    let current = state.plugins.get(&new.id).await.filter(|entry| entry.instance == new.instance);
    match current {
        Some(entry) if entry.state == PluginState::Error => {
            entry.error.unwrap_or_else(|| err.to_string())
        }
        _ => err.to_string(),
    }
}

async fn unload(state: &AppState, old: &Registered) -> Result<Option<Value>, CallError> {
    let context = super::own_context(state, &old.id, client::CALL_DEADLINE)?;
    old.client.unload(context.token()).await
}

async fn audit(state: &AppState, entry: &Registered, action: &str, detail: Value) {
    let principal = Principal::Plugin { id: entry.id.clone() };
    super::audit(state, &principal, action, &entry.id, detail).await;
}

pub(super) async fn run(state: AppState, old: Registered, new: Registered) {
    let _finished = Finished { state: state.clone(), id: new.id.clone() };
    let id = new.id.as_str();
    let (from, to) = (old.manifest.version.as_str(), new.manifest.version.as_str());
    tracing::info!(plugin = %id, from, to, "handover starting");

    // The new version's storage goes first, while the old one still serves.
    if let Err(reason) = super::prepare(&state, &new).await {
        let reason = format!("{to} was not taken on: {reason}");
        tracing::warn!(plugin = %id, %reason, "handover abandoned before pausing anything");
        super::note(&state, &old, &reason).await;
        audit(
            &state,
            &new,
            "plugin.reload-failed",
            json!({ "from": from, "to": to, "error": reason }),
        )
        .await;
        dismiss(new.clone());
        return;
    }

    let gate = state.plugins.gate(id);
    gate.close();
    let paused = Instant::now();
    if let Err(err) = super::settle(&state, &old, PluginState::Unloading, None).await {
        tracing::warn!(plugin = %id, %err, "the old version could not be marked unloading");
    }
    if !gate.drained(state.config.plugins.drain_timeout()).await {
        tracing::warn!(plugin = %id, "requests were still going on the old version when it unloaded");
    }
    let previous = match unload(&state, &old).await {
        Ok(carried) => {
            super::save_handover(&state, id, carried.as_ref()).await;
            carried
        }
        Err(err) => {
            tracing::warn!(plugin = %id, %err, "the old version did not unload; carrying on from its last handover");
            super::handed_over(&state, id).await
        }
    };

    super::install(&state, &new).await;
    let loaded = match super::load(&state, id, new.client.as_ref(), previous.clone()).await {
        Ok(()) => match super::settle(&state, &new, PluginState::Running, None).await {
            Ok(_) => Ok(()),
            Err(err) => Err(not_running(&state, &new, &err).await),
        },
        Err(err) => Err(err.detail()),
    };
    match loaded {
        Ok(()) => {
            gate.open();
            crate::data::complete(&state, id, new.instance, &new.manifest.data).await;
            let paused_ms = paused.elapsed().as_millis() as u64;
            tracing::info!(plugin = %id, from, to, paused_ms, "handover finished");
            audit(
                &state,
                &new,
                "plugin.reloaded",
                json!({ "from": from, "to": to, "paused_ms": paused_ms }),
            )
            .await;
            dismiss(old);
        }
        Err(detail) => {
            let reason = format!("{to} failed to load, so {from} was loaded again: {detail}");
            tracing::warn!(plugin = %id, %reason, "handover rolled back");
            super::install(&state, &old).await;
            match super::load(&state, id, old.client.as_ref(), previous).await {
                Ok(()) => {
                    if let Err(err) =
                        super::settle(&state, &old, PluginState::Running, Some(&reason)).await
                    {
                        tracing::warn!(plugin = %id, %err, "the old version could not be marked running");
                    }
                }
                Err(again) => {
                    let reason = format!("{reason}; then {from} failed too: {}", again.detail());
                    super::fail(&state, &old, &reason).await;
                }
            }
            gate.open();
            audit(
                &state,
                &new,
                "plugin.reload-failed",
                json!({ "from": from, "to": to, "error": reason }),
            )
            .await;
            dismiss(new);
        }
    }
}

/// An operator's reload: the same process unloads and loads again, or, from `error`, loads again.
pub async fn reload(state: &AppState, id: &str) -> Result<PluginState, TransitionError> {
    let entry =
        state.plugins.get(id).await.ok_or_else(|| TransitionError::Unknown(id.to_string()))?;
    let first = match entry.state {
        PluginState::Running | PluginState::Cancelled => PluginState::Unloading,
        PluginState::Error => PluginState::Loading,
        underway => return Err(TransitionError::Underway(id.to_string(), underway.as_str())),
    };
    if !state.plugins.pend(&entry) {
        return Err(TransitionError::Busy(id.to_string()));
    }
    tokio::spawn(in_place(state.clone(), entry));
    Ok(first)
}

async fn in_place(state: AppState, entry: Registered) {
    let _finished = Finished { state: state.clone(), id: entry.id.clone() };
    let id = entry.id.as_str();
    if let Err(reason) = super::prepare(&state, &entry).await {
        let reason = format!("reload failed: {reason}");
        match entry.state {
            PluginState::Error => super::fail(&state, &entry, &reason).await,
            _ => super::note(&state, &entry, &reason).await,
        }
        return;
    }
    let previous = if entry.state == PluginState::Error {
        super::handed_over(&state, id).await
    } else {
        let gate = state.plugins.gate(id);
        gate.close();
        if let Err(err) = super::settle(&state, &entry, PluginState::Unloading, None).await {
            tracing::warn!(plugin = %id, %err, "a reload could not unload the plugin");
            return;
        }
        if !gate.drained(state.config.plugins.drain_timeout()).await {
            tracing::warn!(plugin = %id, "requests were still going when the plugin unloaded to reload");
        }
        match unload(&state, &entry).await {
            Ok(carried) => {
                super::save_handover(&state, id, carried.as_ref()).await;
                carried
            }
            Err(err) => {
                super::fail(&state, &entry, &format!("unload failed: {}", err.detail())).await;
                return;
            }
        }
    };
    super::install(&state, &entry).await;
    match super::load(&state, id, entry.client.as_ref(), previous).await {
        Ok(()) => match super::settle(&state, &entry, PluginState::Running, None).await {
            Ok(_) => {
                crate::data::complete(&state, id, entry.instance, &entry.manifest.data).await;
                tracing::info!(plugin = %id, "plugin reloaded")
            }
            Err(err) => {
                tracing::warn!(plugin = %id, %err, "a reloaded plugin was not marked running")
            }
        },
        Err(err) => super::fail(&state, &entry, &format!("load failed: {}", err.detail())).await,
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::atomic::Ordering;

    use axum::body::Body;
    use doc_eventbus::{ConsumerGroup, TopicFilter};
    use doc_plugin_protocol::{
        Classification, Liveness, Manifest, RegisterRequest, RegisterResponse,
    };
    use http::StatusCode;
    use tower::ServiceExt;
    use uuid::Uuid;

    use super::*;
    use crate::config::Config;
    use crate::db::repositories::PluginRepository;
    use crate::plugins::client::FakePlugin;
    use crate::plugins::{self, RegisterError, TransitionError};
    use crate::status::plugins::TimeOut;
    use crate::testing::{ADMIN, Host, plugin_host, plugin_host_with};

    fn manifest(version: &str, classification: Classification) -> Manifest {
        Manifest {
            id: "hello".into(),
            version: version.into(),
            classification,
            ..Manifest::default()
        }
    }

    /// Registers `version`, answered by `process`, as a new deployment of hello would.
    async fn deploy(
        host: &Host,
        version: &str,
        process: &Arc<FakePlugin>,
    ) -> Result<RegisterResponse, RegisterError> {
        deploy_as(host, manifest(version, Classification::Synchronous), process).await
    }

    async fn deploy_as(
        host: &Host,
        manifest: Manifest,
        process: &Arc<FakePlugin>,
    ) -> Result<RegisterResponse, RegisterError> {
        host.connector.hand_out(process.clone());
        let request = RegisterRequest {
            address: format!("hello-{}:4440", manifest.version),
            binary_sha256: format!("{:0>64}", manifest.version.replace('.', "")),
            started_at: None,
            manifest,
        };
        plugins::register(&host.state, &host.as_plugin("hello"), request).await
    }

    async fn eventually<F: Fn() -> Fut, Fut: Future<Output = bool>>(what: &str, check: F) {
        for _ in 0..200 {
            if check().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{what} did not happen within 2s");
    }

    /// Waits for `instance` to be the registration requests go to, and not loading any more.
    async fn serving(host: &Host, instance: Uuid) -> PluginState {
        eventually("the registration settling", || async {
            let current = host.state.plugins.get("hello").await;
            current.is_some_and(|entry| {
                entry.instance == instance && entry.state != PluginState::Loading
            }) && !host.state.plugins.handing_over("hello")
        })
        .await;
        host.state_of("hello").await.expect("registered")
    }

    async fn running(host: &Host, version: &str, process: &Arc<FakePlugin>) -> Uuid {
        let response = deploy(host, version, process).await.expect("registered");
        let instance = response.instance.expect("an instance");
        assert_eq!(serving(host, instance).await, PluginState::Running);
        instance
    }

    async fn greet(host: &Host) -> StatusCode {
        let request = http::Request::builder()
            .uri("/api/v1/plugins/hello/api/greetings")
            .header("authorization", format!("Bearer {ADMIN}"))
            .body(Body::empty())
            .expect("request");
        host.app.clone().oneshot(request).await.expect("answered").status()
    }

    #[tokio::test]
    async fn a_new_version_takes_over_with_what_the_old_one_handed_over() {
        let host = plugin_host();
        let (one, two) = (FakePlugin::new(), FakePlugin::new());
        running(&host, "1.0.0", &one).await;
        one.set_carries(Some(json!({ "greeted": 5 })));
        let filter = TopicFilter::new("platform.plugin.hello.state").expect("filter");
        let mut states = host
            .state
            .buses
            .events
            .subscribe(ConsumerGroup::new("test", filter))
            .await
            .expect("subscribed");

        let instance = running(&host, "2.0.0", &two).await;
        assert_eq!(
            host.state.plugins.get("hello").await.expect("registered").manifest.version,
            "2.0.0"
        );
        assert_eq!(two.saw_previous(), Some(json!({ "greeted": 5 })));
        assert_eq!(
            host.store.handover("hello").await.expect("read"),
            Some(json!({ "greeted": 5 }))
        );
        assert!(
            one.answered("unload") < two.answered("load"),
            "unloaded before the new one loaded"
        );
        eventually("the old process being told to exit", || async {
            one.exited.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(two.exited.load(Ordering::SeqCst), 0);
        assert!(instance != Uuid::nil());

        let mut seen = Vec::new();
        while let Ok(Some(delivery)) =
            tokio::time::timeout(Duration::from_millis(100), states.next()).await
        {
            seen.push(delivery.event.payload["state"].as_str().unwrap_or_default().to_string());
        }
        assert!(
            seen.ends_with(&["unloading".into(), "loading".into(), "running".into()]),
            "{seen:?}"
        );
    }

    #[tokio::test]
    async fn requests_wait_during_a_handover_and_the_new_version_answers_them() {
        let host = plugin_host();
        let (one, two) = (FakePlugin::new(), FakePlugin::new());
        running(&host, "1.0.0", &one).await;
        two.slow_down(Some(Duration::from_millis(300)));
        let response = deploy(&host, "2.0.0", &two).await.expect("registered");
        eventually("the gate closing", || async { host.state.plugins.gate("hello").is_closed() })
            .await;

        assert_eq!(greet(&host).await, StatusCode::OK, "it waited rather than failing");
        assert!(one.requests().is_empty(), "nothing reached the old version once it was paused");
        assert_eq!(two.requests().len(), 1);
        assert_eq!(
            serving(&host, response.instance.expect("an instance")).await,
            PluginState::Running
        );
    }

    #[tokio::test]
    async fn a_request_kept_waiting_past_the_handover_timeout_is_503() {
        let mut config = Config::default();
        config.bootstrap.admins = vec!["tester".into()];
        config.plugins.ids = vec!["hello".into()];
        config.plugins.handover_timeout_s = 1;
        let host = plugin_host_with(config);
        let (one, two) = (FakePlugin::new(), FakePlugin::new());
        running(&host, "1.0.0", &one).await;
        two.slow_down(Some(Duration::from_millis(1500)));
        deploy(&host, "2.0.0", &two).await.expect("registered");
        eventually("the gate closing", || async { host.state.plugins.gate("hello").is_closed() })
            .await;
        assert_eq!(greet(&host).await, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn requests_already_on_the_old_version_finish_before_it_unloads() {
        let host = plugin_host();
        let (one, two) = (FakePlugin::new(), FakePlugin::new());
        running(&host, "1.0.0", &one).await;
        one.slow_down(Some(Duration::from_millis(300)));
        let first = tokio::spawn({
            let host_app = host.app.clone();
            async move {
                let request = http::Request::builder()
                    .uri("/api/v1/plugins/hello/api/greetings")
                    .header("authorization", format!("Bearer {ADMIN}"))
                    .body(Body::empty())
                    .expect("request");
                host_app.oneshot(request).await.expect("answered").status()
            }
        });
        eventually("the request reaching the old version", || async { one.requests().len() == 1 })
            .await;

        running(&host, "2.0.0", &two).await;
        assert_eq!(first.await.expect("joined"), StatusCode::OK);
        assert!(one.answered("request") < one.answered("unload"), "drained before it unloaded");
    }

    #[tokio::test]
    async fn a_new_version_that_fails_to_load_is_rolled_back_to_the_old_one() {
        let host = plugin_host();
        let (one, two) = (FakePlugin::new(), FakePlugin::new());
        let first = running(&host, "1.0.0", &one).await;
        one.set_carries(Some(json!({ "greeted": 7 })));
        two.set_failing(Some("the new schema is wrong"));
        deploy(&host, "2.0.0", &two).await.expect("registered");

        assert_eq!(serving(&host, first).await, PluginState::Running, "the old version is back");
        let error = host.error_of("hello").await.unwrap_or_default();
        assert!(error.contains("2.0.0 failed to load, so 1.0.0 was loaded again"), "{error}");
        assert!(error.contains("the new schema is wrong"), "{error}");
        assert_eq!(one.loaded.load(Ordering::SeqCst), 2);
        assert_eq!(
            one.saw_previous(),
            Some(json!({ "greeted": 7 })),
            "with the state it handed over"
        );
        eventually("the new process being told to exit", || async {
            two.exited.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(one.exited.load(Ordering::SeqCst), 0);
        assert_eq!(greet(&host).await, StatusCode::OK);
        assert_eq!(one.requests().len(), 1);
    }

    /// The plugin probe timing the new version out while it loads counts as a failed load.
    #[tokio::test]
    async fn a_new_version_timed_out_while_it_loads_is_rolled_back() {
        let host = plugin_host();
        let (one, two) = (FakePlugin::new(), FakePlugin::new());
        let first = running(&host, "1.0.0", &one).await;
        one.set_carries(Some(json!({ "greeted": 3 })));
        two.slow_down(Some(Duration::from_millis(300)));
        let response = deploy(&host, "2.0.0", &two).await.expect("registered");
        let second = response.instance.expect("an instance");
        eventually("the new version loading", || async {
            let current = host.state.plugins.get("hello").await;
            current.is_some_and(|entry| entry.instance == second)
        })
        .await;
        let since = host.state.plugins.get("hello").await.expect("registered").since;
        let asked = TimeOut { plugin: "hello".into(), instance: second, since };
        assert!(plugins::service::time_out(&host.state, &asked).await.timed_out);

        assert_eq!(serving(&host, first).await, PluginState::Running, "the old version is back");
        let error = host.error_of("hello").await.unwrap_or_default();
        assert!(
            error.ends_with("2.0.0 failed to load, so 1.0.0 was loaded again: timed out"),
            "{error}"
        );
        assert_eq!(two.loaded.load(Ordering::SeqCst), 1, "its load finished, too late");
        assert_eq!(one.saw_previous(), Some(json!({ "greeted": 3 })));
        eventually("the new process being told to exit", || async {
            two.exited.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(greet(&host).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_new_version_whose_storage_cannot_be_prepared_never_pauses_the_old_one() {
        use doc_plugin_protocol::data::{Collection, Declaration, Field};
        let host = plugin_host();
        let (one, two) = (FakePlugin::new(), FakePlugin::new());
        let first = running(&host, "1.0.0", &one).await;
        host.data.set_broken(true);
        let mut broken = manifest("2.0.0", Classification::Synchronous);
        broken.data = Declaration::default()
            .collection("greetings", Collection::new().field("id", Field::uuid().key()));
        deploy_as(&host, broken, &two).await.expect("registered");
        eventually("the handover being abandoned", || async {
            !host.state.plugins.handing_over("hello")
        })
        .await;

        assert_eq!(serving(&host, first).await, PluginState::Running);
        assert_eq!(one.unloaded.load(Ordering::SeqCst), 0);
        assert!(
            host.error_of("hello").await.unwrap_or_default().contains("2.0.0 was not taken on")
        );
        eventually("the new process being told to exit", || async {
            two.exited.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(greet(&host).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn a_run_cut_short_by_a_handover_finishes_on_the_new_version() {
        let host = plugin_host();
        let (one, two) = (FakePlugin::new(), FakePlugin::new());
        deploy_as(&host, manifest("1.0.0", Classification::Async), &one).await.expect("registered");
        eventually("1.0.0 running", || async {
            host.state_of("hello").await == Some(PluginState::Running)
        })
        .await;
        super::super::runs::start_pool(&host.state);
        one.hold_runs(true);
        let task = super::super::runs::queue(
            &host.state,
            "hello",
            doc_background_tasks::Actor::platform(),
            json!({ "n": 1 }),
            3,
        )
        .await
        .expect("queued");
        eventually("the run starting on 1.0.0", || async { one.runs().len() == 1 }).await;

        deploy_as(&host, manifest("2.0.0", Classification::Async), &two).await.expect("registered");
        eventually("the task finishing", || async {
            host.state
                .repos
                .tasks
                .get(task.id)
                .await
                .ok()
                .flatten()
                .is_some_and(|task| task.state.finished())
        })
        .await;
        let done = host.state.repos.tasks.get(task.id).await.expect("read").expect("task");
        assert_eq!(done.state, doc_background_tasks::TaskState::Succeeded, "{:?}", done.error);
        assert_eq!(done.attempts, 1, "moving to the new version is not a failed attempt");
        assert_eq!(two.runs().len(), 1);
    }

    #[tokio::test]
    async fn a_replaced_process_is_told_it_is_gone() {
        let host = plugin_host();
        let (one, two) = (FakePlugin::new(), FakePlugin::new());
        let old = running(&host, "1.0.0", &one).await;
        let new = running(&host, "2.0.0", &two).await;
        let report = |instance| Liveness {
            id: "hello".into(),
            state: PluginState::Running,
            error: None,
            instance: Some(instance),
        };
        let principal = host.as_plugin("hello");
        let refused =
            plugins::liveness(&host.state, &principal, report(old)).await.expect_err("replaced");
        assert!(matches!(refused, TransitionError::Superseded(_)));
        assert_eq!(refused.status(), 410);
        plugins::liveness(&host.state, &principal, report(new))
            .await
            .expect("the new one is current");
    }

    #[tokio::test]
    async fn a_third_version_waits_for_the_handover_under_way() {
        let host = plugin_host();
        let (one, two, three) = (FakePlugin::new(), FakePlugin::new(), FakePlugin::new());
        running(&host, "1.0.0", &one).await;
        two.slow_down(Some(Duration::from_millis(300)));
        deploy(&host, "2.0.0", &two).await.expect("registered");
        let refused = deploy(&host, "3.0.0", &three).await.expect_err("busy");
        assert!(matches!(refused, RegisterError::Busy(_)));
        assert_eq!(refused.status(), 503, "the plugin tries again, as for any 503");
    }

    #[tokio::test]
    async fn unloading_keeps_the_state_and_permissions_and_ends_the_process() {
        let host = plugin_host();
        let one = FakePlugin::new();
        running(&host, "1.0.0", &one).await;
        one.set_carries(Some(json!({ "greeted": 3 })));
        plugins::unload(&host.state, "hello").await.expect("unloaded");

        assert_eq!(
            host.store.handover("hello").await.expect("read"),
            Some(json!({ "greeted": 3 }))
        );
        eventually("the process being told to exit", || async {
            one.exited.load(Ordering::SeqCst) == 1
        })
        .await;
        assert_eq!(greet(&host).await, StatusCode::SERVICE_UNAVAILABLE);
        let permissions = host.store.permissions("hello").await.expect("read");
        assert!(!permissions.is_empty(), "its permissions are kept for when it comes back");
    }

    #[tokio::test]
    async fn what_was_handed_over_survives_a_backend_restart() {
        let host = plugin_host();
        let one = FakePlugin::new();
        running(&host, "1.0.0", &one).await;
        one.set_carries(Some(json!({ "greeted": 9 })));
        plugins::unload(&host.state, "hello").await.expect("unloaded");

        let restarted = host.restarted();
        let two = FakePlugin::new();
        running(&restarted, "1.0.0", &two).await;
        assert_eq!(two.saw_previous(), Some(json!({ "greeted": 9 })));
    }

    async fn schedules(host: &Host) -> Vec<(String, String)> {
        let recorded = host.state.repos.cron.list().await.expect("listed");
        recorded
            .into_iter()
            .filter(|task| task.name.starts_with("plugin.hello."))
            .map(|task| (task.name, task.schedule))
            .collect()
    }

    #[tokio::test]
    async fn a_version_s_schedules_are_recorded_and_those_it_drops_removed() {
        let host = plugin_host();
        let nightly = doc_plugin_protocol::Schedule::new("nightly", "0 2 * * *", "Greets everyone");
        let hourly = doc_plugin_protocol::Schedule::new("hourly", "5 * * * *", "");
        let mut one = manifest("1.0.0", Classification::Synchronous);
        one.schedules = vec![nightly.clone(), hourly];
        let response = deploy_as(&host, one, &FakePlugin::new()).await.expect("registered");
        assert_eq!(
            serving(&host, response.instance.expect("an instance")).await,
            PluginState::Running
        );
        assert_eq!(
            schedules(&host).await,
            vec![
                ("plugin.hello.hourly".to_string(), "5 * * * *".to_string()),
                ("plugin.hello.nightly".to_string(), "0 2 * * *".to_string()),
            ]
        );

        let mut two = manifest("2.0.0", Classification::Synchronous);
        two.schedules = vec![nightly];
        let response = deploy_as(&host, two, &FakePlugin::new()).await.expect("registered");
        assert_eq!(
            serving(&host, response.instance.expect("an instance")).await,
            PluginState::Running
        );
        let kept = vec![("plugin.hello.nightly".to_string(), "0 2 * * *".to_string())];
        assert_eq!(schedules(&host).await, kept, "2.0.0 no longer runs hourly");
    }

    #[tokio::test]
    async fn a_schedule_that_is_not_cron_is_refused() {
        let host = plugin_host();
        let mut bad = manifest("1.0.0", Classification::Synchronous);
        bad.schedules = vec![doc_plugin_protocol::Schedule::new("nightly", "every night", "")];
        let refused = deploy_as(&host, bad, &FakePlugin::new()).await.expect_err("refused");
        assert_eq!((refused.status(), refused.kind()), (400, "bad-schedule"));
        let mut named = manifest("1.0.0", Classification::Synchronous);
        named.schedules = vec![doc_plugin_protocol::Schedule::new("Night Time", "0 2 * * *", "")];
        let refused = deploy_as(&host, named, &FakePlugin::new()).await.expect_err("refused");
        assert_eq!(refused.kind(), "bad-schedule");
        assert!(schedules(&host).await.is_empty());
    }
}
