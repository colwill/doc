//! Outages other plugins see, read from each one's `outages` export at every check: `kubernetes`
//! says a service is down while none of its production pods are available. Each is opened and
//! closed here with that plugin as its source, from the times it gives, and an outage something
//! else already saw over the same time is not counted again.

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, Order, PluginError, Query};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::record;
use crate::settings::Definitions;
use crate::store::{Outage, Subject};
use crate::view;

/// How far back an exported outage is read; one still open is kept open here however old.
const LOOKBACK_DAYS: i64 = 30;

/// An outage as a source exports it.
#[derive(Debug, Deserialize)]
struct Seen {
    service: String,
    started_at: DateTime<Utc>,
    #[serde(default)]
    ended_at: Option<DateTime<Utc>>,
    #[serde(default)]
    detail: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    cluster: Option<String>,
    #[serde(default)]
    namespace: Option<String>,
    #[serde(default)]
    workload: Option<String>,
}

impl Seen {
    fn detail(&self) -> String {
        let place = match (&self.cluster, &self.namespace, &self.workload) {
            (Some(cluster), Some(namespace), Some(workload)) => {
                format!("{cluster}/{namespace}/{workload}")
            }
            _ => String::new(),
        };
        let parts: Vec<&str> =
            [self.environment.as_deref(), Some(place.as_str()), self.detail.as_deref()]
                .into_iter()
                .flatten()
                .filter(|part| !part.is_empty())
                .collect();
        parts.join(": ")
    }
}

/// A service reported about is watched from then on, as one reported through the API is.
async fn watched(backend: &Backend, service: &str, at: DateTime<Utc>) -> Result<(), PluginError> {
    let key = view::key(service);
    let held = backend.get::<Subject>("subjects", key.as_str()).await?;
    if held.as_ref().is_none_or(|held| held.since.is_none()) {
        let mut subject = held.unwrap_or_else(|| Subject::service(service));
        subject.since = Some(at);
        backend.upsert::<Value>("subjects", &["subject"], json!(subject)).await?;
    }
    Ok(())
}

/// Whether some other outage of `subject` covers any of this time already.
async fn overlapped(
    backend: &Backend,
    subject: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<bool, PluginError> {
    let before: Vec<Outage> = backend
        .query_all(
            Query::new("outages")
                .filter(json!({ "subject": subject, "started_at": { "lte": to } })),
        )
        .await?;
    Ok(before.iter().any(|outage| outage.ended_at.is_none_or(|ended| ended >= from)))
}

pub async fn read(backend: &Backend, definitions: &Definitions) -> Result<Value, PluginError> {
    let since = Utc::now() - Duration::days(LOOKBACK_DAYS);
    let (mut opened, mut closed) = (0, 0);
    for source in &definitions.outages_from {
        let seen: Vec<Seen> = match backend
            .query_all(
                Query::new(&format!("{source}.outages"))
                    .filter(json!({ "started_at": { "gte": since } }))
                    .order(Order::asc("started_at")),
            )
            .await
        {
            Ok(seen) => seen,
            // A source that is not running, or exports nothing to reliability, is passed over.
            Err(err) => {
                tracing::debug!(source, %err, "a source's outages could not be read");
                continue;
            }
        };
        if seen.is_empty() {
            continue;
        }
        let ours: Vec<Outage> = backend
            .query_all(
                Query::new("outages")
                    .filter(json!({ "source": source, "started_at": { "gte": since } })),
            )
            .await?;
        for outage in seen {
            let subject = view::key(&outage.service);
            let held = ours
                .iter()
                .find(|held| held.subject == subject && held.started_at == outage.started_at);
            match (held, outage.ended_at) {
                (Some(held), Some(ended)) if held.ended_at.is_none() => {
                    closed += record::close(backend, &subject, ended, Some(source)).await?.len();
                }
                (Some(_), _) => {}
                (None, ended) => {
                    let to = ended.unwrap_or_else(Utc::now);
                    if overlapped(backend, &subject, outage.started_at, to).await? {
                        continue;
                    }
                    watched(backend, &outage.service, outage.started_at).await?;
                    let detail = Some(outage.detail()).filter(|detail| !detail.is_empty());
                    let made =
                        record::open(backend, &subject, outage.started_at, source, detail, None)
                            .await?;
                    if made.is_some() {
                        opened += 1;
                        if let Some(ended) = ended {
                            closed +=
                                record::close(backend, &subject, ended, Some(source)).await?.len();
                        }
                    }
                }
            }
        }
    }
    Ok(json!({ "opened": opened, "closed": closed }))
}
