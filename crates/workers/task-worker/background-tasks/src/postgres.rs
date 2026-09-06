//! The Postgres `TaskStore`. Claiming, cancelling and requeueing are each one statement, so two
//! workers racing for the same task cannot both win.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::store::{Outcome, TaskError, TaskFilter, TaskStore};
use crate::{Actor, NewTask, Task, TaskState};

pub struct PostgresTasks {
    pool: PgPool,
}

impl PostgresTasks {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn failed(err: &sqlx::Error) -> TaskError {
    TaskError::Other(err.to_string())
}

#[derive(sqlx::FromRow)]
struct TaskRow {
    id: Uuid,
    kind: String,
    state: String,
    payload: Value,
    result: Option<Value>,
    error: Option<String>,
    attempts: i32,
    max_attempts: i32,
    cancel_requested: bool,
    started_by_kind: String,
    started_by_id: Option<String>,
    started_by_label: Option<String>,
    chain: Option<Uuid>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
}

impl TryFrom<TaskRow> for Task {
    type Error = TaskError;

    fn try_from(row: TaskRow) -> Result<Self, TaskError> {
        Ok(Self {
            id: row.id,
            kind: row.kind,
            state: row.state.parse()?,
            payload: row.payload,
            result: row.result,
            error: row.error,
            attempts: row.attempts,
            max_attempts: row.max_attempts,
            cancel_requested: row.cancel_requested,
            started_by: Actor {
                kind: row.started_by_kind,
                id: row.started_by_id,
                label: row.started_by_label,
            },
            chain: row.chain,
            created_at: row.created_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        })
    }
}

#[async_trait]
impl TaskStore for PostgresTasks {
    async fn create(&self, new: NewTask) -> Result<Task, TaskError> {
        let row: TaskRow = sqlx::query_as(
            "INSERT INTO core.tasks \
             (id, kind, state, payload, max_attempts, started_by_kind, started_by_id, \
              started_by_label, chain) \
             VALUES ($1, $2, 'queued', $3, $4, $5, $6, $7, $8) \
             RETURNING id, kind, state, payload, result, error, attempts, max_attempts, \
                       cancel_requested, started_by_kind, started_by_id, started_by_label, chain, \
                       created_at, started_at, finished_at",
        )
        .bind(Uuid::now_v7())
        .bind(&new.kind)
        .bind(&new.payload)
        .bind(new.max_attempts)
        .bind(&new.started_by.kind)
        .bind(new.started_by.id.as_deref())
        .bind(new.started_by.label.as_deref())
        .bind(new.chain)
        .fetch_one(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        row.try_into()
    }

    async fn get(&self, id: Uuid) -> Result<Option<Task>, TaskError> {
        let row: Option<TaskRow> = sqlx::query_as(
            "SELECT id, kind, state, payload, result, error, attempts, max_attempts, \
                    cancel_requested, started_by_kind, started_by_id, started_by_label, chain, \
                    created_at, started_at, finished_at \
             FROM core.tasks WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        row.map(Task::try_from).transpose()
    }

    async fn chain(&self, root: Uuid) -> Result<Vec<Task>, TaskError> {
        let rows: Vec<TaskRow> = sqlx::query_as(
            "SELECT id, kind, state, payload, result, error, attempts, max_attempts, \
                    cancel_requested, started_by_kind, started_by_id, started_by_label, chain, \
                    created_at, started_at, finished_at \
             FROM core.tasks WHERE chain = $1 ORDER BY created_at",
        )
        .bind(root)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        rows.into_iter().map(Task::try_from).collect()
    }

    async fn list(&self, filter: TaskFilter) -> Result<Vec<Task>, TaskError> {
        let rows: Vec<TaskRow> = sqlx::query_as(
            "SELECT id, kind, state, payload, result, error, attempts, max_attempts, \
                    cancel_requested, started_by_kind, started_by_id, started_by_label, chain, \
                    created_at, started_at, finished_at \
             FROM core.tasks \
             WHERE ($1::text IS NULL OR kind = $1) \
               AND ($2::text IS NULL OR state = $2) \
               AND ($3::text IS NULL OR (started_by_kind = $3 \
                                         AND started_by_id IS NOT DISTINCT FROM $4)) \
             ORDER BY created_at DESC LIMIT $5",
        )
        .bind(filter.kind.as_deref())
        .bind(filter.state.map(TaskState::as_str))
        .bind(filter.owner.as_ref().map(|owner| owner.kind.as_str()))
        .bind(filter.owner.as_ref().and_then(|owner| owner.id.as_deref()))
        .bind(filter.limit)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        rows.into_iter().map(Task::try_from).collect()
    }

    async fn claim(&self, id: Uuid, lease: Duration) -> Result<Option<Task>, TaskError> {
        let row: Option<TaskRow> = sqlx::query_as(
            "UPDATE core.tasks \
             SET state = 'running', attempts = attempts + 1, started_at = now(), \
                 lease_until = now() + make_interval(secs => $2), updated_at = now() \
             WHERE id = $1 AND cancel_requested = false \
               AND (state = 'queued' \
                    OR (state = 'running' AND (lease_until IS NULL OR lease_until < now()))) \
             RETURNING id, kind, state, payload, result, error, attempts, max_attempts, \
                       cancel_requested, started_by_kind, started_by_id, started_by_label, chain, \
                       created_at, started_at, finished_at",
        )
        .bind(id)
        .bind(lease.as_secs_f64())
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        row.map(Task::try_from).transpose()
    }

    async fn renew(&self, id: Uuid, lease: Duration) -> Result<(), TaskError> {
        sqlx::query(
            "UPDATE core.tasks SET lease_until = now() + make_interval(secs => $2) \
             WHERE id = $1 AND state = 'running'",
        )
        .bind(id)
        .bind(lease.as_secs_f64())
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn finish(&self, id: Uuid, outcome: Outcome) -> Result<(), TaskError> {
        let result = match outcome {
            Outcome::Succeeded(value) => {
                sqlx::query(
                    "UPDATE core.tasks SET state = 'succeeded', result = $2, error = NULL, \
                            finished_at = now(), lease_until = NULL, updated_at = now() \
                     WHERE id = $1",
                )
                .bind(id)
                .bind(value)
                .execute(&self.pool)
                .await
            }
            Outcome::Failed(error) => {
                sqlx::query(
                    "UPDATE core.tasks SET state = 'failed', error = $2, finished_at = now(), \
                            lease_until = NULL, updated_at = now() \
                     WHERE id = $1",
                )
                .bind(id)
                .bind(error)
                .execute(&self.pool)
                .await
            }
            Outcome::Cancelled => {
                sqlx::query(
                    "UPDATE core.tasks SET state = 'cancelled', finished_at = now(), \
                            lease_until = NULL, updated_at = now() \
                     WHERE id = $1",
                )
                .bind(id)
                .execute(&self.pool)
                .await
            }
            Outcome::Retry(error) => {
                sqlx::query(
                    "UPDATE core.tasks SET state = 'queued', error = $2, started_at = NULL, \
                            lease_until = NULL, updated_at = now() \
                     WHERE id = $1",
                )
                .bind(id)
                .bind(error)
                .execute(&self.pool)
                .await
            }
        };
        result.map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn request_cancel(&self, id: Uuid) -> Result<bool, TaskError> {
        let result = sqlx::query(
            "UPDATE core.tasks \
             SET cancel_requested = true, \
                 state = CASE WHEN state = 'queued' THEN 'cancelled' ELSE state END, \
                 finished_at = CASE WHEN state = 'queued' THEN now() ELSE finished_at END, \
                 lease_until = CASE WHEN state = 'queued' THEN NULL ELSE lease_until END, \
                 updated_at = now() \
             WHERE id = $1 AND state IN ('queued', 'running')",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(result.rows_affected() > 0)
    }

    async fn cancel_requested(&self, id: Uuid) -> Result<bool, TaskError> {
        let found: Option<(bool,)> =
            sqlx::query_as("SELECT cancel_requested FROM core.tasks WHERE id = $1")
                .bind(id)
                .fetch_optional(&self.pool)
                .await
                .map_err(|err| failed(&err))?;
        Ok(found.is_some_and(|(requested,)| requested))
    }

    async fn requeue_stranded(&self) -> Result<Vec<(Uuid, String)>, TaskError> {
        sqlx::query_as(
            "UPDATE core.tasks \
             SET state = 'queued', started_at = NULL, lease_until = NULL, updated_at = now() \
             WHERE state = 'running' AND lease_until IS NOT NULL AND lease_until < now() \
             RETURNING id, kind",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn set_kind_paused(&self, kind: &str, paused: bool) -> Result<(), TaskError> {
        sqlx::query(
            "INSERT INTO core.task_kinds (kind, paused) VALUES ($1, $2) \
             ON CONFLICT (kind) DO UPDATE SET paused = EXCLUDED.paused, updated_at = now()",
        )
        .bind(kind)
        .bind(paused)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn paused_kinds(&self) -> Result<Vec<String>, TaskError> {
        let rows: Vec<(String,)> =
            sqlx::query_as("SELECT kind FROM core.task_kinds WHERE paused = true")
                .fetch_all(&self.pool)
                .await
                .map_err(|err| failed(&err))?;
        Ok(rows.into_iter().map(|(kind,)| kind).collect())
    }
}
