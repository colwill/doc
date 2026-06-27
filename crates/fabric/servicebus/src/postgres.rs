//! Service Bus in Postgres: the queues, their leases, retries and dead letters kept in tables, so
//! every process of a deployment running without the clusters works the same queue. A message the
//! workers' cron puts on `core.tasks` is then taken by the backend's pool, which is the whole
//! point of it.
//!
//! Request and reply stays direct, in the process that serves the address, exactly as the
//! in-process bus does it: nothing in DOC serves an address one process and calls it from
//! another. A call to an address served somewhere else says so rather than reading as nothing
//! serving it at all, so the day something does, it is a sentence rather than a silence.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use parking_lot::Mutex;
use serde_json::Value;
use sqlx::PgPool;
use tracing::Instrument;
use uuid::Uuid;

use crate::{
    Address, DeadLetter, Lease, Message, QueueSpec, Request, ServiceBus, ServiceBusError,
    ServiceHandler,
};

/// How often a served address looks again when its queue was empty.
const POLL: Duration = Duration::from_millis(500);

#[derive(Clone)]
pub struct PostgresServiceBus {
    pool: PgPool,
    /// What this process answers itself. Request and reply never leaves it.
    handlers: Arc<Mutex<HashMap<Address, Arc<dyn ServiceHandler>>>>,
    /// The name this process is known by, written beside an address it serves.
    name: String,
}

impl PostgresServiceBus {
    pub fn new(pool: PgPool, name: impl Into<String>) -> Self {
        Self { pool, handlers: Arc::new(Mutex::new(HashMap::new())), name: name.into() }
    }

    async fn spec(&self, address: &Address) -> QueueSpec {
        let row: Option<(i32, i64, i64)> = sqlx::query_as(
            "SELECT max_attempts, lease_s, max_depth FROM core.fabric_queue_specs
             WHERE address = $1",
        )
        .bind(address.as_str())
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten();
        let mut spec = QueueSpec::new(address.clone());
        if let Some((max_attempts, lease_s, max_depth)) = row {
            spec.max_attempts = max_attempts.max(1) as u32;
            spec.lease = Duration::from_secs(lease_s.max(1) as u64);
            spec.max_depth = max_depth.max(1) as usize;
        }
        spec
    }

    /// Hands a served address's queued messages to its handler, as the cluster's consumer does.
    fn consume(&self, address: Address) {
        let bus = self.clone();
        tokio::spawn(async move {
            loop {
                let Some(handler) = bus.handlers.lock().get(&address).cloned() else { return };
                match bus.receive(&address).await {
                    Ok(Some(lease)) => {
                        let span = crate::handling(&lease.message);
                        let handled = handler.handle(Request::of(&lease.message));
                        let _ = match handled.instrument(span).await {
                            Ok(_) => bus.ack(lease.id).await,
                            Err(reason) => bus.nack(lease.id, &reason).await,
                        };
                    }
                    _ => tokio::time::sleep(POLL).await,
                }
            }
        });
    }

    async fn served_by(&self, address: &Address) -> Option<String> {
        sqlx::query_as::<_, (String,)>(
            "SELECT served_by FROM core.fabric_services WHERE address = $1",
        )
        .bind(address.as_str())
        .fetch_optional(&self.pool)
        .await
        .ok()
        .flatten()
        .map(|(who,)| who)
    }
}

fn failed(err: &sqlx::Error) -> ServiceBusError {
    ServiceBusError::Unavailable(err.to_string())
}

fn unreadable(err: &serde_json::Error) -> ServiceBusError {
    ServiceBusError::Unavailable(format!("a queued message is unreadable: {err}"))
}

#[async_trait]
impl ServiceBus for PostgresServiceBus {
    async fn register_queue(&self, spec: QueueSpec) -> Result<(), ServiceBusError> {
        sqlx::query(
            "INSERT INTO core.fabric_queue_specs (address, max_attempts, lease_s, max_depth)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (address) DO UPDATE SET
                 max_attempts = excluded.max_attempts,
                 lease_s = excluded.lease_s,
                 max_depth = excluded.max_depth",
        )
        .bind(spec.address.as_str())
        .bind(spec.max_attempts as i32)
        .bind(spec.lease.as_secs().max(1) as i64)
        .bind(spec.max_depth as i64)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn serve(
        &self,
        address: Address,
        handler: Arc<dyn ServiceHandler>,
    ) -> Result<(), ServiceBusError> {
        sqlx::query(
            "INSERT INTO core.fabric_services (address, served_by, since)
             VALUES ($1, $2, now())
             ON CONFLICT (address) DO UPDATE SET served_by = excluded.served_by, since = now()",
        )
        .bind(address.as_str())
        .bind(&self.name)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        let first = self.handlers.lock().insert(address.clone(), handler).is_none();
        if first {
            self.consume(address);
        }
        Ok(())
    }

    async fn request_as(
        &self,
        address: &Address,
        subject: &str,
        payload: Value,
        deadline: Duration,
        principal: Option<&str>,
    ) -> Result<Value, ServiceBusError> {
        let handler = self.handlers.lock().get(address).cloned();
        let Some(handler) = handler else {
            return match self.served_by(address).await {
                Some(elsewhere) => Err(ServiceBusError::Unavailable(format!(
                    "{address} is served by {elsewhere}, and without the fabric clusters a \
                     request is only answered inside the process that serves it"
                ))),
                None => Err(ServiceBusError::NoHandler(address.clone())),
            };
        };
        let request = Request {
            subject: subject.to_string(),
            payload,
            principal: principal.map(str::to_string),
            correlation_id: None,
            trace: doc_telemetry::traceparent(),
        };
        match tokio::time::timeout(deadline, handler.handle(request)).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(message)) => Err(ServiceBusError::Remote { address: address.clone(), message }),
            Err(_) => Err(ServiceBusError::DeadlineExceeded(address.clone())),
        }
    }

    async fn send(&self, mut message: Message) -> Result<Uuid, ServiceBusError> {
        drop(crate::sending(&mut message));
        let spec = self.spec(&message.to).await;
        let depth = self.depth(&message.to).await?;
        if depth >= spec.max_depth {
            return Err(ServiceBusError::Unavailable(format!("{} is full", message.to)));
        }
        let id = message.id;
        let body = serde_json::to_value(&message).map_err(|err| unreadable(&err))?;
        sqlx::query(
            "INSERT INTO core.fabric_queue (id, address, message, attempts) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(message.to.as_str())
        .bind(&body)
        .bind(message.attempts as i32)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(id)
    }

    /// One statement, so two processes reaching for the same queue cannot both take the same
    /// message: the row is chosen and leased together, and a row somebody else is holding is
    /// skipped rather than waited for.
    async fn receive(&self, address: &Address) -> Result<Option<Lease>, ServiceBusError> {
        let spec = self.spec(address).await;
        let lease = Uuid::now_v7();
        let row: Option<(Value, i32)> = sqlx::query_as(
            "UPDATE core.fabric_queue SET
                 lease = $2,
                 leased_until = now() + make_interval(secs => $3::bigint)
             WHERE id = (
                 SELECT id FROM core.fabric_queue
                 WHERE address = $1
                   AND available_at <= now()
                   AND (leased_until IS NULL OR leased_until <= now())
                 ORDER BY queued_at
                 FOR UPDATE SKIP LOCKED
                 LIMIT 1
             )
             RETURNING message, attempts",
        )
        .bind(address.as_str())
        .bind(lease)
        .bind(spec.lease.as_secs().max(1) as i64)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        let Some((body, attempts)) = row else { return Ok(None) };
        let mut message: Message = serde_json::from_value(body).map_err(|err| unreadable(&err))?;
        message.attempts = attempts.max(0) as u32;
        Ok(Some(Lease { id: lease, message }))
    }

    async fn ack(&self, lease: Uuid) -> Result<(), ServiceBusError> {
        sqlx::query("DELETE FROM core.fabric_queue WHERE lease = $1")
            .bind(lease)
            .execute(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(())
    }

    /// Counts the attempt and puts the message back, or dead-letters it once its queue's attempts
    /// are used up — the in-process bus's rule, with the queue's own spec.
    async fn nack(&self, lease: Uuid, reason: &str) -> Result<(), ServiceBusError> {
        let row: Option<(Uuid, String, Value, i32)> = sqlx::query_as(
            "UPDATE core.fabric_queue SET
                 attempts = attempts + 1, lease = NULL, leased_until = NULL
             WHERE lease = $1
             RETURNING id, address, message, attempts",
        )
        .bind(lease)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        let Some((id, address, body, attempts)) = row else { return Ok(()) };
        let address = Address::new(address)?;
        let spec = self.spec(&address).await;
        if (attempts.max(0) as u32) < spec.max_attempts {
            return Ok(());
        }
        let mut message: Message = serde_json::from_value(body).map_err(|err| unreadable(&err))?;
        message.attempts = attempts.max(0) as u32;
        let letter = DeadLetter {
            message,
            reason: format!("{reason} after {} attempts", spec.max_attempts),
            at: Utc::now(),
        };
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        sqlx::query("DELETE FROM core.fabric_queue WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|err| failed(&err))?;
        sqlx::query(
            "INSERT INTO core.fabric_dead_letters (id, address, letter, at) VALUES ($1, $2, $3, $4)",
        )
        .bind(id)
        .bind(address.as_str())
        .bind(serde_json::to_value(&letter).map_err(|err| unreadable(&err))?)
        .bind(letter.at)
        .execute(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn dead_letters(&self, address: &Address) -> Result<Vec<DeadLetter>, ServiceBusError> {
        let rows: Vec<(Value,)> = sqlx::query_as(
            "SELECT letter FROM core.fabric_dead_letters WHERE address = $1 ORDER BY at",
        )
        .bind(address.as_str())
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        rows.into_iter()
            .map(|(letter,)| serde_json::from_value(letter).map_err(|err| unreadable(&err)))
            .collect()
    }

    async fn depth(&self, address: &Address) -> Result<usize, ServiceBusError> {
        let (depth,): (i64,) = sqlx::query_as(
            "SELECT count(*) FROM core.fabric_queue
             WHERE address = $1 AND available_at <= now()
               AND (leased_until IS NULL OR leased_until <= now())",
        )
        .bind(address.as_str())
        .fetch_one(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(depth.max(0) as usize)
    }
}
