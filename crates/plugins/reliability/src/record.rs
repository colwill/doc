//! Writing down what is seen: an outage opened once however many things notice it and closed
//! once, each announced for automations, and the time each subject was watched added to its day.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, DataRequest, PluginError, Query};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ID;
use crate::store::{Covered, Outage, Sample};

const BATCH: usize = 100;

/// Opens an outage of `subject` from `started_at`, unless one is open already, whatever noticed
/// it: two sources seeing the same outage count its downtime once.
pub async fn open(
    backend: &Backend,
    subject: &str,
    started_at: DateTime<Utc>,
    source: &str,
    detail: Option<String>,
    by: Option<String>,
) -> Result<Option<Outage>, PluginError> {
    if !still_open(backend, subject).await?.is_empty() {
        return Ok(None);
    }
    let outage = Outage {
        id: Uuid::new_v4(),
        subject: subject.to_string(),
        started_at,
        ended_at: None,
        seconds: None,
        source: source.to_string(),
        detail: detail.map(|detail| detail.chars().take(500).collect()),
        by,
    };
    backend.insert::<Value>("outages", json!(outage)).await?;
    let topic = format!("plugin.{ID}.outage.started");
    backend.publish_once(&topic, announced(&outage), &format!("started:{}", outage.id)).await?;
    Ok(Some(outage))
}

pub async fn still_open(backend: &Backend, subject: &str) -> Result<Vec<Outage>, PluginError> {
    backend
        .query_all(
            Query::new("outages")
                .filter(json!({ "subject": subject, "ended_at": { "is_null": true } })),
        )
        .await
}

/// Closes `subject`'s open outages at `ended_at`: only those `source` opened, when it names one,
/// so a passing check never ends an outage somebody reported.
pub async fn close(
    backend: &Backend,
    subject: &str,
    ended_at: DateTime<Utc>,
    source: Option<&str>,
) -> Result<Vec<Outage>, PluginError> {
    let mut closed = Vec::new();
    for mut outage in still_open(backend, subject).await? {
        if source.is_some_and(|source| source != outage.source) {
            continue;
        }
        let ended_at = ended_at.max(outage.started_at);
        outage.ended_at = Some(ended_at);
        outage.seconds = Some(outage.lasted(ended_at));
        let set = json!({ "ended_at": ended_at, "seconds": outage.seconds });
        backend.update::<Value>("outages", outage.id.to_string(), set, None).await?;
        let topic = format!("plugin.{ID}.outage.ended");
        backend.publish_once(&topic, announced(&outage), &format!("ended:{}", outage.id)).await?;
        closed.push(outage);
    }
    Ok(closed)
}

fn announced(outage: &Outage) -> Value {
    let (kind, name) = outage.subject.split_once(':').unwrap_or(("service", &outage.subject));
    json!({
        "outage": outage.id,
        "subject": outage.subject,
        "kind": kind,
        "name": name,
        "started_at": outage.started_at,
        "ended_at": outage.ended_at,
        "seconds": outage.seconds,
        "source": outage.source,
        "detail": outage.detail,
    })
}

pub fn midnight(at: DateTime<Utc>) -> DateTime<Utc> {
    at.date_naive().and_hms_opt(0, 0, 0).map_or(at, |day| day.and_utc())
}

/// How finely watching is kept as telemetry. The shortest period a page can be shown over is an
/// hour, and twelve points across it is what reads as a line rather than a scribble.
pub const SAMPLE_SECONDS: i64 = 300;

/// The five minutes `at` falls in, from.
pub fn sample_at(at: DateTime<Utc>) -> DateTime<Utc> {
    let started = at.timestamp().div_euclid(SAMPLE_SECONDS) * SAMPLE_SECONDS;
    DateTime::from_timestamp(started, 0).unwrap_or(at)
}

/// Time watched and checks made, by subject and five minutes, waiting to be added to what is
/// kept. It is added twice over: as telemetry, which keeps the five minutes and is thrown away
/// after a week, and rolled up into the day, which is kept for as long as outages are. A year of
/// days is small; a year of five minutes is not, and belongs somewhere that keeps telemetry.
#[derive(Debug, Default)]
pub struct Watching(BTreeMap<(String, DateTime<Utc>), (f64, i64, i64)>);

/// What is added to one bucket: seconds watched, checks made, checks failed.
type Counted = (f64, i64, i64);

impl Watching {
    /// `subject` was watched from `from` to `to`, split at each bucket's edge, and checked once
    /// at `to`.
    pub fn add(&mut self, subject: &str, from: DateTime<Utc>, to: DateTime<Utc>, failed: bool) {
        let mut at = from.min(to);
        while at < to {
            let next = (sample_at(at) + Duration::seconds(SAMPLE_SECONDS)).min(to);
            let held = self.0.entry((subject.to_string(), sample_at(at))).or_default();
            held.0 += (next - at).num_milliseconds() as f64 / 1_000.0;
            at = next;
        }
        let held = self.0.entry((subject.to_string(), sample_at(to))).or_default();
        held.1 += 1;
        held.2 += i64::from(failed);
    }

    /// The same counts rolled up into the days they fall in.
    fn by_day(&self) -> BTreeMap<(String, DateTime<Utc>), Counted> {
        let mut days: BTreeMap<(String, DateTime<Utc>), Counted> = BTreeMap::new();
        for ((subject, at), (seconds, checks, failed)) in &self.0 {
            let held = days.entry((subject.clone(), midnight(*at))).or_default();
            held.0 += seconds;
            held.1 += checks;
            held.2 += failed;
        }
        days
    }

    /// Adds everything to the five minutes and the days already kept.
    pub async fn flush(self, backend: &Backend) -> Result<(), PluginError> {
        let days: Vec<_> = self.by_day().into_iter().collect();
        for chunk in days.chunks(BATCH) {
            let ids: Vec<String> = chunk
                .iter()
                .map(|((subject, day), _)| format!("{subject}/{}", day.format("%Y-%m-%d")))
                .collect();
            let held: Vec<Covered> = backend
                .query_all(Query::new("coverage").filter(json!({ "id": { "in": ids } })))
                .await?;
            let held: BTreeMap<String, Covered> =
                held.into_iter().map(|covered| (covered.id.clone(), covered)).collect();
            let writes = chunk
                .iter()
                .zip(&ids)
                .map(|(((subject, day), (seconds, checks, failed)), id)| {
                    let before = held.get(id);
                    let covered = Covered {
                        id: id.clone(),
                        subject: subject.clone(),
                        day: *day,
                        seconds: (before.map_or(0.0, |b| b.seconds) + seconds).min(86_400.0),
                        checks: before.map_or(0, |b| b.checks) + checks,
                        failed: before.map_or(0, |b| b.failed) + failed,
                    };
                    DataRequest::upsert("coverage", &["id"], json!(covered))
                })
                .collect();
            backend.batch(writes).await?;
        }
        let samples: Vec<_> = self.0.into_iter().collect();
        for chunk in samples.chunks(BATCH) {
            let ids: Vec<String> = chunk
                .iter()
                .map(|((subject, at), _)| format!("{subject}/{}", at.timestamp()))
                .collect();
            let held: Vec<Sample> = backend
                .query_all(Query::new("samples").filter(json!({ "id": { "in": ids } })))
                .await?;
            let held: BTreeMap<String, Sample> =
                held.into_iter().map(|sample| (sample.id.clone(), sample)).collect();
            let writes = chunk
                .iter()
                .zip(&ids)
                .map(|(((subject, at), (seconds, checks, failed)), id)| {
                    let before = held.get(id);
                    let sample = Sample {
                        id: id.clone(),
                        subject: subject.clone(),
                        at: *at,
                        seconds: (before.map_or(0.0, |b| b.seconds) + seconds)
                            .min(SAMPLE_SECONDS as f64),
                        checks: before.map_or(0, |b| b.checks) + checks,
                        failed: before.map_or(0, |b| b.failed) + failed,
                    };
                    DataRequest::upsert("samples", &["id"], json!(sample))
                })
                .collect();
            backend.batch(writes).await?;
        }
        Ok(())
    }
}
