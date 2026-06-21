//! Event Bus in Postgres: the log, its retention, and each consumer group's place in it kept in
//! tables, so every process of a deployment running without the clusters publishes to and reads
//! from one bus. What the workers' probes announce then reaches the backend, and what the backend
//! announces reaches anything else that has the database.
//!
//! Delivery is at least once, as it is everywhere else: an event is handed out, and stays handed
//! out until it is acknowledged or its lease runs out. A group shares one place in the log, so an
//! event goes to one member of it.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::{
    ConsumerGroup, Delivery, Event, EventBus, EventBusError, PublishAck, Subscription, Topic,
    TopicFilter, TopicReport, TopicSpec,
};

/// How often a subscription with nothing waiting looks again.
const POLL: Duration = Duration::from_millis(250);
/// How often retention is applied. Every publish would be wasteful and none would be a leak.
const PRUNE_EVERY: Duration = Duration::from_secs(60);

#[derive(Clone)]
pub struct PostgresEventBus {
    pool: PgPool,
}

fn failed(err: &sqlx::Error) -> EventBusError {
    EventBusError::Unavailable(err.to_string())
}

fn unreadable(err: &serde_json::Error) -> EventBusError {
    EventBusError::Other(format!("a published event is unreadable: {err}"))
}

/// A topic filter as a Postgres regular expression: `*` is one segment, `>` is the rest of them.
/// Matching in the database keeps finding the next event for a group to one statement.
fn pattern(filter: &TopicFilter) -> String {
    let mut regex = String::from("^");
    let segments: Vec<&str> = filter.as_str().split('.').collect();
    for (at, segment) in segments.iter().enumerate() {
        if at > 0 {
            regex.push_str("\\.");
        }
        match *segment {
            ">" => regex.push_str(".+"),
            "*" => regex.push_str("[^.]+"),
            plain => regex.push_str(plain),
        }
    }
    regex.push('$');
    regex
}

impl PostgresEventBus {
    /// Starts the bus and the pruning that keeps each topic to what it was registered for.
    pub fn new(pool: PgPool) -> Self {
        let bus = Self { pool };
        let pruning = bus.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(PRUNE_EVERY).await;
                if let Err(err) = pruning.prune().await {
                    tracing::warn!(%err, "the event log could not be pruned");
                }
            }
        });
        bus
    }

    /// Applies every registered topic spec: what is older than its age, and what is beyond its
    /// count, goes. An event no spec matches is kept, as it is in the clusters.
    async fn prune(&self) -> Result<(), EventBusError> {
        let specs: Vec<(String, i64, i64)> =
            sqlx::query_as("SELECT filter, max_events, max_age_s FROM core.fabric_topic_specs")
                .fetch_all(&self.pool)
                .await
                .map_err(|err| failed(&err))?;
        for (filter, max_events, max_age_s) in specs {
            let Ok(filter) = TopicFilter::new(filter) else { continue };
            let regex = pattern(&filter);
            sqlx::query(
                "DELETE FROM core.fabric_events
                 WHERE topic ~ $1 AND published_at < now() - make_interval(secs => $2::bigint)",
            )
            .bind(&regex)
            .bind(max_age_s.max(1))
            .execute(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
            sqlx::query(
                "DELETE FROM core.fabric_events WHERE sequence IN (
                     SELECT sequence FROM core.fabric_events WHERE topic ~ $1
                     ORDER BY sequence DESC OFFSET $2
                 )",
            )
            .bind(&regex)
            .bind(max_events.max(1))
            .execute(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        }
        Ok(())
    }
}

#[async_trait]
impl EventBus for PostgresEventBus {
    async fn register_topic(&self, spec: TopicSpec) -> Result<(), EventBusError> {
        sqlx::query(
            "INSERT INTO core.fabric_topic_specs (filter, max_events, max_age_s)
             VALUES ($1, $2, $3)
             ON CONFLICT (filter) DO UPDATE
                 SET max_events = excluded.max_events, max_age_s = excluded.max_age_s",
        )
        .bind(spec.filter.as_str())
        .bind(spec.max_events as i64)
        .bind(spec.max_age.as_secs().max(1) as i64)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn publish(&self, event: Event) -> Result<PublishAck, EventBusError> {
        let body = serde_json::to_value(&event).map_err(|err| unreadable(&err))?;
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        // An idempotency key that was used before gives back the event it was used for, rather
        // than a second copy of it.
        let row: Option<(i64, Uuid)> = sqlx::query_as(
            "INSERT INTO core.fabric_events (id, topic, idempotency_key, event)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (idempotency_key) WHERE idempotency_key IS NOT NULL DO NOTHING
             RETURNING sequence, id",
        )
        .bind(event.id)
        .bind(event.topic.as_str())
        .bind(event.idempotency_key.as_deref())
        .bind(&body)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        let ack = match row {
            Some((sequence, id)) => {
                sqlx::query(
                    "INSERT INTO core.fabric_topic_counts (topic, published) VALUES ($1, 1)
                     ON CONFLICT (topic) DO UPDATE
                         SET published = core.fabric_topic_counts.published + 1",
                )
                .bind(event.topic.as_str())
                .execute(&mut *tx)
                .await
                .map_err(|err| failed(&err))?;
                PublishAck { id, offset: sequence.max(0) as u64, duplicate: false }
            }
            None => {
                let (sequence, id): (i64, Uuid) = sqlx::query_as(
                    "SELECT sequence, id FROM core.fabric_events WHERE idempotency_key = $1",
                )
                .bind(event.idempotency_key.as_deref())
                .fetch_one(&mut *tx)
                .await
                .map_err(|err| failed(&err))?;
                PublishAck { id, offset: sequence.max(0) as u64, duplicate: true }
            }
        };
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(ack)
    }

    async fn subscribe(
        &self,
        group: ConsumerGroup,
    ) -> Result<Box<dyn Subscription>, EventBusError> {
        // A group starts where it was, and a new one at the oldest event kept or at the next one
        // published, as it asked.
        let start: i64 = match group.from_start {
            true => 0,
            false => {
                sqlx::query_as::<_, (Option<i64>,)>("SELECT max(sequence) FROM core.fabric_events")
                    .fetch_one(&self.pool)
                    .await
                    .map_err(|err| failed(&err))?
                    .0
                    .unwrap_or(0)
            }
        };
        sqlx::query(
            "INSERT INTO core.fabric_event_groups (name, filter, at_sequence)
             VALUES ($1, $2, $3)
             ON CONFLICT (name) DO UPDATE SET filter = excluded.filter, updated_at = now()",
        )
        .bind(&group.name)
        .bind(group.filter.as_str())
        .bind(start)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(Box::new(PostgresSubscription {
            pool: self.pool.clone(),
            regex: pattern(&group.filter),
            group,
        }))
    }

    async fn topics(&self) -> Result<Vec<TopicReport>, EventBusError> {
        let rows: Vec<(String, i64, Option<i64>)> = sqlx::query_as(
            "SELECT counts.topic, coalesce(kept.retained, 0), counts.published
             FROM core.fabric_topic_counts counts
             LEFT JOIN (
                 SELECT topic, count(*) AS retained FROM core.fabric_events GROUP BY topic
             ) kept ON kept.topic = counts.topic
             ORDER BY counts.topic",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(rows
            .into_iter()
            .filter_map(|(topic, retained, published)| {
                Some(TopicReport {
                    topic: Topic::new(topic).ok()?,
                    retained: retained.max(0) as u64,
                    published: published.unwrap_or_default().max(0) as u64,
                })
            })
            .collect())
    }
}

struct PostgresSubscription {
    pool: PgPool,
    group: ConsumerGroup,
    regex: String,
}

impl PostgresSubscription {
    /// One turn: a redelivery that is due, or the next event in the log for this group. Both are
    /// taken under a lock on the group's row, so two members of it never take the same event.
    async fn take(&self) -> Result<Option<Delivery>, EventBusError> {
        let lease = self.group.lease.as_secs().max(1) as i64;
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        let locked: Option<(i64,)> = sqlx::query_as(
            "SELECT at_sequence FROM core.fabric_event_groups WHERE name = $1 FOR UPDATE",
        )
        .bind(&self.group.name)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        let Some((at_sequence,)) = locked else { return Ok(None) };

        // Anything handed out before whose lease has run out goes again, before anything new.
        let again: Option<(Uuid, i32, i64)> = sqlx::query_as(
            "UPDATE core.fabric_event_deliveries SET
                 attempt = attempt + 1,
                 leased_until = now() + make_interval(secs => $2::bigint)
             WHERE id = (
                 SELECT deliveries.id FROM core.fabric_event_deliveries deliveries
                 WHERE deliveries.group_name = $1
                   AND deliveries.available_at <= now()
                   AND deliveries.leased_until <= now()
                 ORDER BY deliveries.sequence
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
             )
             RETURNING id, attempt, sequence",
        )
        .bind(&self.group.name)
        .bind(lease)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        if let Some((id, attempt, sequence)) = again {
            let kept: Option<(Value,)> =
                sqlx::query_as("SELECT event FROM core.fabric_events WHERE sequence = $1")
                    .bind(sequence)
                    .fetch_optional(&mut *tx)
                    .await
                    .map_err(|err| failed(&err))?;
            // The event it was for may have been pruned meanwhile; then there is nothing to hand
            // out and the delivery goes with it.
            let Some((body,)) = kept else {
                sqlx::query("DELETE FROM core.fabric_event_deliveries WHERE id = $1")
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .map_err(|err| failed(&err))?;
                tx.commit().await.map_err(|err| failed(&err))?;
                return Ok(None);
            };
            tx.commit().await.map_err(|err| failed(&err))?;
            let event: Event = serde_json::from_value(body).map_err(|err| unreadable(&err))?;
            return Ok(Some(Delivery { id, event, attempt: attempt.max(1) as u32 }));
        }

        let (highest,): (i64,) =
            sqlx::query_as("SELECT coalesce(max(sequence), 0) FROM core.fabric_events")
                .fetch_one(&mut *tx)
                .await
                .map_err(|err| failed(&err))?;
        let next: Option<(i64, Value)> = sqlx::query_as(
            "SELECT sequence, event FROM core.fabric_events
             WHERE sequence > $1 AND topic ~ $2
             ORDER BY sequence LIMIT 1",
        )
        .bind(at_sequence)
        .bind(&self.regex)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        let Some((sequence, body)) = next else {
            // Nothing here matches this group's filter, and nothing already published ever will,
            // so the group steps over it rather than reading the same events again every turn.
            sqlx::query(
                "UPDATE core.fabric_event_groups SET at_sequence = greatest($2, at_sequence)
                 WHERE name = $1",
            )
            .bind(&self.group.name)
            .bind(highest)
            .execute(&mut *tx)
            .await
            .map_err(|err| failed(&err))?;
            tx.commit().await.map_err(|err| failed(&err))?;
            return Ok(None);
        };
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO core.fabric_event_deliveries
                 (id, group_name, sequence, attempt, available_at, leased_until)
             VALUES ($1, $2, $3, 1, now(), now() + make_interval(secs => $4::bigint))",
        )
        .bind(id)
        .bind(&self.group.name)
        .bind(sequence)
        .bind(lease)
        .execute(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        sqlx::query(
            "UPDATE core.fabric_event_groups SET at_sequence = $2, updated_at = now()
             WHERE name = $1",
        )
        .bind(&self.group.name)
        .bind(sequence)
        .execute(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        tx.commit().await.map_err(|err| failed(&err))?;
        let event: Event = serde_json::from_value(body).map_err(|err| unreadable(&err))?;
        Ok(Some(Delivery { id, event, attempt: 1 }))
    }
}

#[async_trait]
impl Subscription for PostgresSubscription {
    async fn next(&mut self) -> Option<Delivery> {
        loop {
            match self.take().await {
                Ok(Some(delivery)) => return Some(delivery),
                Ok(None) => tokio::time::sleep(POLL).await,
                Err(err) => {
                    tracing::warn!(%err, group = %self.group.name, "the event bus could not be read");
                    tokio::time::sleep(POLL).await;
                }
            }
        }
    }

    async fn ack(&mut self, delivery: Uuid) -> Result<(), EventBusError> {
        sqlx::query("DELETE FROM core.fabric_event_deliveries WHERE id = $1")
            .bind(delivery)
            .execute(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn nack(&mut self, delivery: Uuid, after: Option<Duration>) -> Result<(), EventBusError> {
        let after = after.unwrap_or_default().as_secs().min(i64::MAX as u64) as i64;
        sqlx::query(
            "UPDATE core.fabric_event_deliveries SET
                 available_at = now() + make_interval(secs => $2::bigint),
                 leased_until = now() + make_interval(secs => $2::bigint)
             WHERE id = $1",
        )
        .bind(delivery)
        .bind(after)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }
}
