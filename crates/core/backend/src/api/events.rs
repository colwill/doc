//! `GET /api/v1/events/topics`: what is published on this platform. Everything the platform and
//! its plugins announce goes through the Event Bus, which keeps a log per topic, so asking it is
//! asking what there is to subscribe to — no list is kept by hand and nothing is guessed.

use axum::Json;
use axum::extract::State;
use serde::Serialize;

use super::AppState;
use super::problem::Problem;
use crate::permissions::Authorised;

#[derive(Debug, Serialize)]
pub struct Topics {
    pub topics: Vec<Topic>,
}

#[derive(Debug, Serialize)]
pub struct Topic {
    pub topic: String,
    /// How many events the bus still holds for it, which its retention decides.
    pub retained: u64,
    /// How many have ever been published on it.
    pub published: u64,
}

/// Anyone signed in may read them: a topic is the name of something that happens here, and
/// naming one is how an automation, a plugin or a person subscribes to it.
pub async fn topics(_: Authorised, State(state): State<AppState>) -> Result<Json<Topics>, Problem> {
    let mut reported =
        state.buses.events.topics().await.map_err(|err| {
            Problem::unavailable(format!("the event bus could not be asked: {err}"))
        })?;
    reported.sort_by(|a, b| a.topic.as_str().cmp(b.topic.as_str()));
    let topics = reported
        .into_iter()
        .map(|report| Topic {
            topic: report.topic.as_str().to_string(),
            retained: report.retained,
            published: report.published,
        })
        .collect();
    Ok(Json(Topics { topics }))
}

#[cfg(test)]
mod tests {
    use doc_eventbus::{Event, Topic};
    use http::StatusCode;
    use serde_json::json;

    use crate::testing::{ADMIN, get, get_as, plugin_host};

    #[tokio::test]
    async fn the_topics_are_the_ones_the_event_bus_has_seen() {
        let host = plugin_host();
        let publish = |topic: &str| {
            let event =
                Event::new(Topic::new(topic.to_string()).expect("a topic"), "test", json!({}));
            let events = host.state.buses.events.clone();
            async move { events.publish(event).await.expect("published") }
        };
        publish("platform.team.changed").await;
        publish("plugin.kb.document.imported").await;
        publish("plugin.kb.document.imported").await;

        let (status, body, _) = get_as(&host.app, "/api/v1/events/topics", ADMIN).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let listed: Vec<(&str, u64)> = body["topics"]
            .as_array()
            .expect("topics")
            .iter()
            .map(|topic| {
                (
                    topic["topic"].as_str().expect("a name"),
                    topic["published"].as_u64().expect("a count"),
                )
            })
            .collect();
        assert!(listed.contains(&("platform.team.changed", 1)), "{listed:?}");
        assert!(
            listed.contains(&("plugin.kb.document.imported", 2)),
            "the same topic twice is one topic: {listed:?}"
        );
        assert!(listed.windows(2).all(|pair| pair[0].0 <= pair[1].0), "in order: {listed:?}");

        let (status, _, _) = get(&host.app, "/api/v1/events/topics").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "nobody reads them signed out");
    }
}
