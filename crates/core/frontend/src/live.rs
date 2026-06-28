//! Live updates: the frontend follows status, plugin state, task and plugin UI topics on the Event
//! Bus and streams each browser what its user may see, as server-sent events on `/events`.

use std::convert::Infallible;
use std::future::Future;
use std::time::Duration;

use axum::extract::State;
use axum::response::sse::{Event as Sse, KeepAlive};
use axum::response::{IntoResponse, Response};
use doc_eventbus::{ConsumerGroup, EventBus, TopicFilter};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::session::{self, Signed};
use crate::web::AppState;

const BACKLOG: usize = 256;
/// A plugin marks a topic as a UI update by publishing it under `plugin.<id>.ui.`.
const FOLLOWED: [&str; 4] =
    ["platform.status.>", "platform.plugin.*.state", "platform.task.>", "plugin.*.ui.>"];

/// One change, and what decides who may see it.
#[derive(Debug, Clone)]
pub struct Change {
    pub kind: &'static str,
    pub plugin: Option<String>,
    pub started_by: Option<String>,
    pub payload: Value,
}

impl Change {
    fn of(topic: &str, payload: Value) -> Option<Self> {
        let parts: Vec<&str> = topic.split('.').collect();
        let (kind, plugin) = match parts.as_slice() {
            ["platform", "status", ..] => ("status", None),
            ["platform", "plugin", id, "state"] => ("plugin-state", Some((*id).to_string())),
            ["platform", "task", ..] => ("task", None),
            ["plugin", id, "ui", ..] => ("plugin-ui", Some((*id).to_string())),
            _ => return None,
        };
        let started_by = payload["started_by"]["id"].as_str().map(str::to_string);
        Some(Self { kind, plugin, started_by, payload })
    }
}

#[derive(Clone)]
pub struct Hub(broadcast::Sender<Change>);

impl Hub {
    /// Each process follows under its own group, so every replica hears every change.
    pub fn start(events: &std::sync::Arc<dyn EventBus>, instance: &str) -> Self {
        let (sender, _) = broadcast::channel(BACKLOG);
        for filter in FOLLOWED {
            let events = events.clone();
            let sender = sender.clone();
            let name = format!("frontend.live.{instance}.{}", filter.replace(['*', '>'], "_"));
            tokio::spawn(async move {
                let Ok(filter) = TopicFilter::new(filter) else { return };
                let mut group = ConsumerGroup::new(name, filter);
                group.from_start = false;
                let mut subscription = match events.subscribe(group).await {
                    Ok(subscription) => subscription,
                    Err(err) => {
                        tracing::warn!(%err, "live updates will not include this topic");
                        return;
                    }
                };
                while let Some(delivery) = subscription.next().await {
                    let topic = delivery.event.topic.as_str().to_string();
                    if let Some(change) = Change::of(&topic, delivery.event.payload) {
                        let _ = sender.send(change);
                    }
                    let _ = subscription.ack(delivery.id).await;
                }
            });
        }
        Self(sender)
    }
}

/// Status and every plugin's state are platform business; a task is its starter's, a UI update
/// its plugin readers'.
async fn visible(state: &AppState, signed: &Signed, change: &Change) -> bool {
    let access = session::access(state, signed).await;
    let reads = |plugin: &str| access.admin || access.plugins.get(plugin).is_some_and(|a| a.read);
    let me = signed.me.get("id").and_then(Value::as_str);
    match change.kind {
        "status" => reads("core"),
        "plugin-state" => reads("core") || change.plugin.as_deref().is_some_and(reads),
        "task" => reads("core") || (me.is_some() && change.started_by.as_deref() == me),
        _ => change.plugin.as_deref().is_some_and(reads),
    }
}

pub async fn events(State(state): State<AppState>, signed: Signed) -> Response {
    let mut changes = state.live.0.subscribe();
    let stream = async_stream(move |sender| async move {
        loop {
            let change = match changes.recv().await {
                Ok(change) => change,
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    let reload = Sse::default()
                        .event("lagged")
                        .data(json!({ "missed": missed }).to_string());
                    if sender.send(Ok(reload)).await.is_err() {
                        return;
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => return,
            };
            if !visible(&state, &signed, &change).await {
                continue;
            }
            let mut data = change.payload.clone();
            if let (Some(plugin), Some(object)) = (&change.plugin, data.as_object_mut()) {
                object.insert("plugin".into(), json!(plugin));
            }
            let event = Sse::default().event(change.kind).data(data.to_string());
            if sender.send(Ok(event)).await.is_err() {
                return;
            }
        }
    });
    axum::response::Sse::new(stream)
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// A stream fed by a task, which ends when the browser goes and the channel closes.
fn async_stream<F, Fut>(work: F) -> tokio_stream::wrappers::ReceiverStream<Result<Sse, Infallible>>
where
    F: FnOnce(tokio::sync::mpsc::Sender<Result<Sse, Infallible>>) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let (sender, receiver) = tokio::sync::mpsc::channel(32);
    tokio::spawn(work(sender));
    tokio_stream::wrappers::ReceiverStream::new(receiver)
}
