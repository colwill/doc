//! Delivering the events a plugin subscribed to. Each subscription is its own consumer group, so
//! the bus remembers how far the plugin got across restarts, and nothing is acknowledged until the
//! plugin's `on_event` has returned — until then a delivery is retried with growing gaps.

use std::time::Duration;

use doc_eventbus::{ConsumerGroup, Delivery, Subscription, TopicFilter};
use doc_plugin_protocol::PluginState;
use tokio::sync::watch;

use crate::api::AppState;
use crate::identity::Principal;
use crate::plugins::client::EVENT_DEADLINE;

const RETRY_MIN: Duration = Duration::from_millis(500);
const RETRY_MAX: Duration = Duration::from_secs(60);
/// How often a paused delivery loop looks at the plugin's state again.
const IDLE: Duration = Duration::from_millis(500);

fn group(plugin: &str, filter: &str) -> Option<ConsumerGroup> {
    let mut group =
        ConsumerGroup::new(format!("plugin.{plugin}:{filter}"), TopicFilter::new(filter).ok()?);
    // A plugin that has just been installed starts from now. Replaying a week of history into it
    // would act on events that were never meant for it.
    group.from_start = false;
    Some(group)
}

fn envelope(delivery: &Delivery) -> doc_plugin_protocol::Event {
    let event = &delivery.event;
    doc_plugin_protocol::Event {
        id: event.id,
        topic: event.topic.as_str().to_string(),
        source: event.source.clone(),
        at: event.time,
        correlation_id: event.correlation_id,
        schema_version: Some(event.schema_version),
        payload: event.payload.clone(),
    }
}

fn backoff(attempt: u32) -> Duration {
    let doublings = attempt.saturating_sub(1).min(16);
    RETRY_MIN.saturating_mul(1 << doublings).min(RETRY_MAX)
}

/// Subscribes before returning, so no event published after registration can slip past the
/// cursor. Sending `true` on the result stops every loop it started.
pub async fn start(state: &AppState, plugin: &str, filters: &[String]) -> watch::Sender<bool> {
    let (stop, stopped) = watch::channel(false);
    for filter in filters {
        let Some(group) = group(plugin, filter) else { continue };
        let subscription = match state.buses.events.subscribe(group.clone()).await {
            Ok(subscription) => Some(subscription),
            Err(err) => {
                tracing::warn!(%err, plugin, filter = %filter, "could not subscribe yet; retrying");
                None
            }
        };
        let (state, plugin) = (state.clone(), plugin.to_string());
        tokio::spawn(deliver(state, plugin, group, subscription, stopped.clone()));
    }
    stop
}

/// Waits until the plugin is `running`. False once it is stopped or has left the registry.
async fn until_running(
    state: &AppState,
    plugin: &str,
    stopped: &mut watch::Receiver<bool>,
) -> bool {
    loop {
        if *stopped.borrow() {
            return false;
        }
        match state.plugins.get(plugin).await {
            None => return false,
            Some(entry) if entry.state == PluginState::Running => return true,
            Some(_) => {}
        }
        tokio::select! {
            _ = stopped.changed() => return false,
            () = tokio::time::sleep(IDLE) => {}
        }
    }
}

async fn deliver(
    state: AppState,
    plugin: String,
    group: ConsumerGroup,
    subscription: Option<Box<dyn Subscription>>,
    mut stopped: watch::Receiver<bool>,
) {
    let mut subscription = match subscription {
        Some(subscription) => subscription,
        None => loop {
            if !until_running(&state, &plugin, &mut stopped).await {
                return;
            }
            match state.buses.events.subscribe(group.clone()).await {
                Ok(subscription) => break subscription,
                Err(_) => tokio::time::sleep(RETRY_MAX / 4).await,
            }
        },
    };
    loop {
        if !until_running(&state, &plugin, &mut stopped).await {
            return;
        }
        let delivery = tokio::select! {
            _ = stopped.changed() => return,
            delivery = subscription.next() => match delivery {
                Some(delivery) => delivery,
                None => return,
            },
        };
        let running =
            state.plugins.get(&plugin).await.filter(|entry| entry.state == PluginState::Running);
        let Some(entry) = running else {
            // It stopped running while this event was on its way; hand it back for later.
            let _ = subscription.nack(delivery.id, Some(IDLE)).await;
            continue;
        };
        let context = match state.plugins.contexts.issue(
            &plugin,
            Principal::Plugin { id: plugin.clone() },
            EVENT_DEADLINE,
        ) {
            Ok(context) => context,
            Err(err) => {
                tracing::warn!(%err, plugin = %plugin, "no context for a delivery");
                let _ = subscription.nack(delivery.id, Some(backoff(delivery.attempt))).await;
                continue;
            }
        };
        match entry.client.event(&envelope(&delivery), context.token()).await {
            Ok(()) => {
                let _ = subscription.ack(delivery.id).await;
            }
            Err(err) => {
                let wait = backoff(delivery.attempt);
                tracing::warn!(
                    plugin = %plugin,
                    topic = %delivery.event.topic.as_str(),
                    attempt = delivery.attempt,
                    retry_in_ms = wait.as_millis() as u64,
                    %err,
                    "a plugin did not acknowledge an event"
                );
                let _ = subscription.nack(delivery.id, Some(wait)).await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use doc_eventbus::{Event, Topic};
    use doc_plugin_protocol::{Manifest, RegisterRequest};
    use serde_json::json;

    use super::*;
    use crate::plugins;
    use crate::testing::{Host, plugin_host};

    async fn subscribed(host: &Host) {
        let manifest = Manifest {
            id: "hello".into(),
            version: "1.0.0".into(),
            subscriptions: vec!["platform.test.>".into()],
            ..Manifest::default()
        };
        let request = RegisterRequest {
            manifest,
            address: "plugin-hello:4440".into(),
            binary_sha256: "a".repeat(64),
            started_at: None,
        };
        let principal = Principal::Plugin { id: "hello".into() };
        plugins::register(&host.state, &principal, request).await.expect("registered");
        assert_eq!(host.settle("hello").await, Some(PluginState::Running));
    }

    async fn publish(host: &Host, topic: &str) {
        let event =
            Event::new(Topic::new(topic).expect("topic"), "test", json!({ "topic": topic }));
        host.state.buses.events.publish(event).await.expect("published");
    }

    /// Polls, because delivery runs on its own task; gives up after `within`.
    async fn delivered(host: &Host, count: usize, within: Duration) -> Vec<String> {
        let started = tokio::time::Instant::now();
        loop {
            let events = host.plugin.events();
            if events.len() >= count || started.elapsed() > within {
                return events.into_iter().map(|event| event.topic).collect();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn subscribed_events_reach_on_event_with_the_plugins_own_context() {
        let host = plugin_host();
        host.plugin.resolve_with("hello", host.state.plugins.contexts.clone());
        subscribed(&host).await;
        publish(&host, "platform.test.one").await;
        publish(&host, "platform.other.ignored").await;

        assert_eq!(delivered(&host, 1, Duration::from_secs(2)).await, ["platform.test.one"]);
        let (_, during) = host.plugin.contexts().pop().expect("the delivery's context");
        assert_eq!(during.as_deref(), Some("plugin:hello"));
    }

    #[tokio::test]
    async fn a_refused_delivery_is_retried_until_the_plugin_takes_it() {
        let host = plugin_host();
        host.plugin.fail_events(1);
        subscribed(&host).await;
        publish(&host, "platform.test.retried").await;
        assert_eq!(delivered(&host, 1, Duration::from_secs(3)).await, ["platform.test.retried"]);
        assert_eq!(host.plugin.contexts().len(), 3, "load, the refused delivery and the retry");
    }

    #[tokio::test]
    async fn nothing_is_delivered_while_the_plugin_is_not_running() {
        let host = plugin_host();
        subscribed(&host).await;
        plugins::cancel(&host.state, "hello").await.expect("cancelled");
        publish(&host, "platform.test.waiting").await;
        assert!(delivered(&host, 1, Duration::from_millis(800)).await.is_empty());

        plugins::transition(&host.state, "hello", PluginState::Running, None).await.unwrap();
        assert_eq!(delivered(&host, 1, Duration::from_secs(3)).await, ["platform.test.waiting"]);
    }

    #[tokio::test]
    async fn unloading_stops_delivery() {
        let host = plugin_host();
        subscribed(&host).await;
        plugins::unload(&host.state, "hello").await.expect("unloaded");
        publish(&host, "platform.test.after").await;
        assert!(delivered(&host, 1, Duration::from_millis(800)).await.is_empty());
    }

    #[tokio::test]
    async fn events_from_before_a_plugin_subscribed_are_not_replayed() {
        let host = plugin_host();
        publish(&host, "platform.test.history").await;
        subscribed(&host).await;
        publish(&host, "platform.test.news").await;
        assert_eq!(delivered(&host, 1, Duration::from_secs(2)).await, ["platform.test.news"]);
    }
}
