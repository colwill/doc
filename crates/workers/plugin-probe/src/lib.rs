//! Plugin probe: keeps `core.plugin_status` in step with `platform.plugin.*.state`, times out a
//! plugin left too long in `loading` or `unloading`, and compares its rows with the backend's
//! registry, recording whatever the events missed.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use doc_backend::status::plugins::{
    PluginChange, PluginStatus, PluginStatuses, REGISTRY, SERVICE, Snapshot, Source, TIME_OUT,
    TimeOut, Verdict,
};
use doc_eventbus::{ConsumerGroup, Delivery, EventBus, Subscription, TopicFilter};
use doc_plugin_protocol::PluginState;
use doc_servicebus::{Address, ServiceBus};
use serde::de::DeserializeOwned;
use serde_json::Value;
use tokio::time::{Instant, MissedTickBehavior};
use uuid::Uuid;

const GROUP: &str = "core.plugin-probe";
const TOPICS: &str = "platform.plugin.*.state";
const COMPARE_EVERY: Duration = Duration::from_secs(30);
const DEADLINE: Duration = Duration::from_secs(10);
const RETRY: Duration = Duration::from_secs(5);
const SUBSCRIBE_WAIT: Duration = Duration::from_millis(500);
const SUBSCRIBE_WAIT_MAX: Duration = Duration::from_secs(15);
/// Deadlines come from the wall clock and sleeps from the monotonic one, so a wake-up may be early.
const SLACK: Duration = Duration::from_millis(50);

/// One stay in `loading` or `unloading`, which is timed out at most once.
type Stay = (String, Uuid, DateTime<Utc>);

pub struct PluginProbe {
    events: Arc<dyn EventBus>,
    services: Arc<dyn ServiceBus>,
    store: Arc<dyn PluginStatuses>,
    stuck_after: Duration,
}

#[derive(Default)]
struct Seen {
    rows: Vec<PluginStatus>,
    /// Stays already asked about: never again once answered, or not before a retry is due.
    asked: BTreeMap<Stay, Option<Instant>>,
    /// What the last comparison found the rows behind on, and when it found a plugin gone.
    behind: BTreeSet<String>,
    missing: BTreeMap<String, DateTime<Utc>>,
    /// Set while no backend answers, so an outage is logged once rather than every comparison.
    unanswered: bool,
}

enum Next {
    Delivery(Option<Box<Delivery>>),
    Compare,
    Deadline,
}

fn stay(row: &PluginStatus) -> Option<Stay> {
    matches!(row.state, Some(PluginState::Loading | PluginState::Unloading))
        .then(|| (row.plugin.clone(), row.instance, row.since))
}

impl PluginProbe {
    pub fn new(
        events: Arc<dyn EventBus>,
        services: Arc<dyn ServiceBus>,
        store: Arc<dyn PluginStatuses>,
        stuck_after: Duration,
    ) -> Self {
        Self { events, services, store, stuck_after }
    }

    /// Runs until the process stops; one task, so its writes never race each other.
    pub async fn run(&self) -> Result<(), String> {
        let filter = TopicFilter::new(TOPICS).map_err(|err| err.to_string())?;
        let group = ConsumerGroup::new(GROUP, filter);
        let mut subscription = self.subscribe(&group).await;
        let mut seen = Seen { rows: self.current().await.unwrap_or_default(), ..Seen::default() };
        let mut compare = tokio::time::interval(COMPARE_EVERY);
        compare.set_missed_tick_behavior(MissedTickBehavior::Delay);
        tracing::info!(stuck_after_s = self.stuck_after.as_secs(), "plugin probe running");
        loop {
            let wake = self.next_deadline(&seen);
            let next = tokio::select! {
                delivery = subscription.next() => Next::Delivery(delivery.map(Box::new)),
                _ = compare.tick() => Next::Compare,
                () = tokio::time::sleep_until(wake) => Next::Deadline,
            };
            match next {
                Next::Delivery(Some(delivery)) => {
                    self.consume(subscription.as_mut(), *delivery).await
                }
                Next::Delivery(None) => {
                    tracing::warn!("plugin state events stopped arriving; subscribing again");
                    subscription = self.subscribe(&group).await;
                }
                Next::Compare => self.compare(&mut seen).await,
                Next::Deadline => self.time_out(&mut seen).await,
            }
            if let Some(rows) = self.current().await {
                seen.rows = rows;
            }
        }
    }

    async fn subscribe(&self, group: &ConsumerGroup) -> Box<dyn Subscription> {
        let mut wait = SUBSCRIBE_WAIT;
        loop {
            match self.events.subscribe(group.clone()).await {
                Ok(subscription) => return subscription,
                Err(err) => {
                    let retry_in_ms = wait.as_millis() as u64;
                    tracing::warn!(%err, retry_in_ms, "could not subscribe to plugin state events");
                    tokio::time::sleep(wait).await;
                    wait = (wait * 2).min(SUBSCRIBE_WAIT_MAX);
                }
            }
        }
    }

    async fn current(&self) -> Option<Vec<PluginStatus>> {
        match self.store.current().await {
            Ok(rows) => Some(rows),
            Err(err) => {
                tracing::warn!(%err, "the plugin status rows could not be read");
                None
            }
        }
    }

    async fn consume(&self, subscription: &mut dyn Subscription, delivery: Delivery) {
        let change: PluginChange = match serde_json::from_value(delivery.event.payload.clone()) {
            Ok(change) => change,
            Err(err) => {
                // Events from before T24 carry less; comparing with the registry covers them.
                let topic = delivery.event.topic.as_str();
                tracing::debug!(%err, topic, "a plugin state event was skipped");
                let _ = subscription.ack(delivery.id).await;
                return;
            }
        };
        match self.store.record(&change, Source::Event).await {
            Ok(()) => {
                if let Err(err) = subscription.ack(delivery.id).await {
                    tracing::debug!(%err, plugin = %change.plugin, "a plugin state event was not acknowledged");
                }
            }
            Err(err) => {
                tracing::warn!(%err, plugin = %change.plugin, "a plugin state change was not recorded; it will be retried");
                let _ = subscription.nack(delivery.id, Some(RETRY)).await;
            }
        }
    }

    /// A difference must survive two comparisons, so an event still on its way is not recorded twice.
    async fn compare(&self, seen: &mut Seen) {
        let snapshot: Snapshot = match self.ask(REGISTRY, Value::Null).await {
            Ok(snapshot) => snapshot,
            Err(err) if seen.unanswered => {
                tracing::debug!(%err, "the plugin registry still cannot be read");
                return;
            }
            Err(err) => {
                tracing::warn!(%err, "the plugin registry cannot be read; comparing again once it can");
                seen.unanswered = true;
                return;
            }
        };
        if std::mem::take(&mut seen.unanswered) {
            tracing::info!("the plugin registry can be read again");
        }
        let rows: BTreeMap<&str, &PluginStatus> =
            seen.rows.iter().map(|row| (row.plugin.as_str(), row)).collect();
        let mut behind = BTreeSet::new();
        for entry in &snapshot.plugins {
            if rows.get(entry.plugin.as_str()).is_some_and(|row| row.at >= entry.at) {
                continue;
            }
            if seen.behind.contains(&entry.plugin) {
                self.correct(entry).await;
            } else {
                behind.insert(entry.plugin.clone());
            }
        }
        let registered: BTreeSet<&str> =
            snapshot.plugins.iter().map(|entry| entry.plugin.as_str()).collect();
        let mut missing = BTreeMap::new();
        for row in &seen.rows {
            if row.state.is_none() || registered.contains(row.plugin.as_str()) {
                continue;
            }
            match seen.missing.get(&row.plugin) {
                Some(first) => self.correct(&row.removed(*first)).await,
                None => {
                    missing.insert(row.plugin.clone(), snapshot.at);
                }
            }
        }
        (seen.behind, seen.missing) = (behind, missing);
        if let Err(err) = self.store.checked(Utc::now()).await {
            tracing::warn!(%err, "the plugin status rows could not be marked as compared");
        }
    }

    async fn correct(&self, change: &PluginChange) {
        let state = change.state.map_or("removed", PluginState::as_str);
        tracing::info!(plugin = %change.plugin, state, "a plugin state change was missed; recording it from the registry");
        if let Err(err) = self.store.record(change, Source::Registry).await {
            tracing::warn!(%err, plugin = %change.plugin, "a missed plugin state change was not recorded");
        }
    }

    /// When a row's stay is due to be timed out, unless it is not a stay or has been asked about.
    fn due(&self, row: &PluginStatus, asked: &BTreeMap<Stay, Option<Instant>>) -> Option<Instant> {
        let stay = stay(row)?;
        let elapsed = (Utc::now() - row.since).to_std().unwrap_or_default();
        let due = Instant::now() + self.stuck_after.saturating_sub(elapsed);
        match asked.get(&stay) {
            Some(None) => None,
            Some(Some(retry)) => Some(due.max(*retry)),
            None => Some(due),
        }
    }

    fn next_deadline(&self, seen: &Seen) -> Instant {
        let due = seen.rows.iter().filter_map(|row| self.due(row, &seen.asked)).min();
        due.unwrap_or_else(|| Instant::now() + COMPARE_EVERY)
    }

    async fn time_out(&self, seen: &mut Seen) {
        let now = Instant::now();
        let due: Vec<TimeOut> = seen
            .rows
            .iter()
            .filter(|row| self.due(row, &seen.asked).is_some_and(|due| due <= now + SLACK))
            .map(|row| TimeOut {
                plugin: row.plugin.clone(),
                instance: row.instance,
                since: row.since,
            })
            .collect();
        for asked in due {
            let payload = serde_json::to_value(&asked).unwrap_or_default();
            let again = match self.ask::<Verdict>(TIME_OUT, payload).await {
                Ok(verdict) if verdict.timed_out => {
                    tracing::warn!(plugin = %asked.plugin, since = %asked.since, "a plugin was stuck, so it has been put in error");
                    None
                }
                Ok(verdict) => {
                    let reason = verdict.reason.unwrap_or_default();
                    tracing::info!(plugin = %asked.plugin, reason, "the backend found nothing to time out");
                    None
                }
                Err(err) => {
                    tracing::warn!(%err, plugin = %asked.plugin, "a stuck plugin could not be timed out yet");
                    Some(now + RETRY)
                }
            };
            seen.asked.insert((asked.plugin, asked.instance, asked.since), again);
        }
        let stays: BTreeSet<Stay> = seen.rows.iter().filter_map(stay).collect();
        seen.asked.retain(|stay, _| stays.contains(stay));
    }

    async fn ask<T: DeserializeOwned>(&self, subject: &str, payload: Value) -> Result<T, String> {
        let address = Address::core(SERVICE).map_err(|err| err.to_string())?;
        // With no backend on the fabric the last request is still queued, so none is piled on it.
        if self.services.depth(&address).await.unwrap_or(0) > 0 {
            return Err("no backend is answering core.plugins".into());
        }
        let answer = self
            .services
            .request(&address, subject, payload, DEADLINE)
            .await
            .map_err(|err| err.to_string())?;
        serde_json::from_value(answer)
            .map_err(|err| format!("an unexpected answer to {subject}: {err}"))
    }
}
