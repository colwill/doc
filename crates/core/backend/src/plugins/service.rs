//! `core.plugins` on the Service Bus, for the plugin probe (T24): the registry it compares its rows
//! with, and putting a plugin it found stuck in `loading` or `unloading` into `error`.

use std::sync::Arc;

use anyhow::{Context, Result};
use async_trait::async_trait;
use doc_plugin_protocol::PluginState;
use doc_servicebus::{Address, Request, ServiceHandler};
use serde_json::Value;

use super::{Registered, changed};
use crate::api::AppState;
use crate::status::plugins::{self, Snapshot, TimeOut, Verdict};

pub async fn serve(state: &AppState) -> Result<()> {
    let address = Address::core(plugins::SERVICE)?;
    let handler = Arc::new(Service(state.clone()));
    state.buses.services.serve(address, handler).await.context("serving core.plugins")
}

struct Service(AppState);

#[async_trait]
impl ServiceHandler for Service {
    async fn handle(&self, request: Request) -> Result<Value, String> {
        // A relayed request always carries a principal, so only the platform's own arrive without.
        if request.principal.is_some() {
            return Err("core.plugins is for the platform alone".into());
        }
        let answer = match request.subject.as_str() {
            plugins::REGISTRY => serde_json::to_value(snapshot(&self.0).await),
            plugins::TIME_OUT => {
                let asked: TimeOut = serde_json::from_value(request.payload)
                    .map_err(|err| format!("not a time-out: {err}"))?;
                serde_json::to_value(time_out(&self.0, &asked).await)
            }
            other => return Err(format!("core.plugins does not answer `{other}`")),
        };
        answer.map_err(|err| err.to_string())
    }
}

/// Stamped after the registry is read, so a plugin missing from it had left by then.
pub async fn snapshot(state: &AppState) -> Snapshot {
    let plugins = state.plugins.list().await.iter().map(Registered::change).collect();
    Snapshot { at: plugins::now(), plugins }
}

/// Only the stay the probe saw is timed out, never one a newer registration or state began.
pub async fn time_out(state: &AppState, asked: &TimeOut) -> Verdict {
    let moved = {
        let mut registered = state.plugins.plugins.write().await;
        let Some(entry) = registered.get_mut(&asked.plugin) else {
            return Verdict::not("it is not registered");
        };
        if entry.instance != asked.instance {
            return Verdict::not("a newer registration has taken its place");
        }
        if !matches!(entry.state, PluginState::Loading | PluginState::Unloading) {
            return Verdict::not(format!("it is {} now", entry.state.as_str()));
        }
        if entry.since > asked.since {
            return Verdict::not(format!("it went back into {} since", entry.state.as_str()));
        }
        let from = entry.state;
        entry.enter(PluginState::Error, Some(plugins::TIMED_OUT)).map(|change| (from, change))
    };
    if let Some((from, change)) = moved {
        tracing::warn!(plugin = %asked.plugin, was = %from.as_str(), since = %asked.since, "a plugin was stuck and has been timed out");
        changed(state, from, &change).await;
    }
    Verdict::timed_out()
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use doc_plugin_protocol::{Manifest, RegisterRequest};
    use serde_json::json;
    use uuid::Uuid;

    use super::*;
    use crate::plugins;
    use crate::status::plugins::{REGISTRY, SERVICE, TIME_OUT, TIMED_OUT};
    use crate::testing::{Host, plugin_host};

    const DEADLINE: Duration = Duration::from_secs(5);

    async fn serving() -> Host {
        let host = plugin_host();
        serve(&host.state).await.expect("serving core.plugins");
        host
    }

    async fn register_hello(host: &Host) -> Uuid {
        let manifest =
            Manifest { id: "hello".into(), version: "1.0.0".into(), ..Manifest::default() };
        let request = RegisterRequest {
            manifest,
            address: "plugin-hello:4440".into(),
            binary_sha256: "a".repeat(64),
            started_at: None,
        };
        let principal = host.as_plugin("hello");
        let response =
            plugins::register(&host.state, &principal, request).await.expect("registered");
        response.instance.expect("an instance")
    }

    async fn ask<T: serde::de::DeserializeOwned>(host: &Host, subject: &str, payload: Value) -> T {
        let address = Address::core(SERVICE).expect("an address");
        let answer = host.state.buses.services.request(&address, subject, payload, DEADLINE).await;
        serde_json::from_value(answer.expect("answered")).expect("the answer's shape")
    }

    #[tokio::test]
    async fn the_registry_is_served_to_the_plugin_probe() {
        let host = serving().await;
        let instance = register_hello(&host).await;
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));

        let snapshot: Snapshot = ask(&host, REGISTRY, Value::Null).await;
        let [hello] = snapshot.plugins.as_slice() else { panic!("one plugin: {snapshot:?}") };
        assert_eq!(hello.instance, instance);
        assert_eq!((hello.state, hello.version.as_str()), (Some(PluginState::Running), "1.0.0"));
        assert!(hello.registered_at <= hello.since && hello.since <= hello.at);
        assert!(hello.at <= snapshot.at, "stamped after the registry was read");
    }

    #[tokio::test]
    async fn a_plugin_still_loading_is_timed_out() {
        let host = serving().await;
        host.plugin.slow_down(Some(Duration::from_millis(300)));
        let instance = register_hello(&host).await;
        let loading = host.state.plugins.get("hello").await.expect("registered");
        assert_eq!(loading.state, PluginState::Loading);

        let asked = TimeOut { plugin: "hello".into(), instance, since: loading.since };
        let verdict: Verdict = ask(&host, TIME_OUT, json!(asked)).await;
        assert!(verdict.timed_out, "{verdict:?}");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Error));
        assert_eq!(host.error_of("hello").await.as_deref(), Some(TIMED_OUT));

        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(host.plugin.loaded.load(Ordering::SeqCst), 1, "the load did finish, too late");
        assert_eq!(host.state_of("hello").await, Some(PluginState::Error), "and changed nothing");
    }

    /// A time-out names one stay, so anything that has moved on since is left alone.
    #[tokio::test]
    async fn only_the_stay_the_probe_saw_is_timed_out() {
        let host = serving().await;
        let instance = register_hello(&host).await;
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
        let first = host.state.plugins.get("hello").await.expect("registered").registered_at;
        let refusals = [
            ("hello", instance, "it is running now"),
            ("hello", Uuid::now_v7(), "a newer registration has taken its place"),
            ("ghost", instance, "it is not registered"),
        ];
        for (plugin, instance, reason) in refusals {
            let asked = TimeOut { plugin: plugin.into(), instance, since: first };
            let verdict: Verdict = ask(&host, TIME_OUT, json!(asked)).await;
            assert!(!verdict.timed_out);
            assert_eq!(verdict.reason.as_deref(), Some(reason));
        }

        // Loading again, after the stay that began at registration, is a stay the probe never saw.
        plugins::transition(&host.state, "hello", PluginState::Error, Some("fell over"))
            .await
            .unwrap();
        plugins::transition(&host.state, "hello", PluginState::Loading, None).await.unwrap();
        let asked = TimeOut { plugin: "hello".into(), instance, since: first };
        let verdict: Verdict = ask(&host, TIME_OUT, json!(asked)).await;
        assert_eq!(verdict.reason.as_deref(), Some("it went back into loading since"));
        assert_eq!(host.state_of("hello").await, Some(PluginState::Loading));
    }

    /// A plugin's Service Bus calls are always made for someone, so none of them can reach this.
    #[tokio::test]
    async fn core_plugins_cannot_be_reached_on_anyones_behalf() {
        let host = serving().await;
        register_hello(&host).await;
        let address = Address::core(SERVICE).expect("an address");
        let services = &host.state.buses.services;
        let relayed = services
            .request_as(&address, TIME_OUT, Value::Null, DEADLINE, Some("plugin:hello"))
            .await
            .expect_err("refused");
        assert!(relayed.to_string().contains("for the platform alone"), "{relayed}");
        let unknown = services.request(&address, "restart", Value::Null, DEADLINE).await;
        assert!(unknown.expect_err("refused").to_string().contains("does not answer `restart`"));
    }
}
