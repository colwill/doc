//! The Postgres `CronStore`. The claim is one statement with `FOR UPDATE SKIP LOCKED`, which is
//! what makes a run happen once however many replicas are looking for it.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use sqlx::PgPool;

use crate::CronTask;
use crate::store::{CronError, CronStore};

pub struct PostgresCron {
    pool: PgPool,
}

impl PostgresCron {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn failed(err: &sqlx::Error) -> CronError {
    CronError::Other(err.to_string())
}

#[derive(sqlx::FromRow)]
struct CronRow {
    name: String,
    schedule: String,
    description: Option<String>,
    paused: bool,
    next_run_at: Option<DateTime<Utc>>,
    last_run_at: Option<DateTime<Utc>>,
    last_state: Option<String>,
    last_error: Option<String>,
}

impl From<CronRow> for CronTask {
    fn from(row: CronRow) -> Self {
        Self {
            name: row.name,
            schedule: row.schedule,
            description: row.description,
            paused: row.paused,
            next_run_at: row.next_run_at,
            last_run_at: row.last_run_at,
            last_state: row.last_state,
            last_error: row.last_error,
        }
    }
}

#[async_trait]
impl CronStore for PostgresCron {
    async fn upsert(
        &self,
        name: &str,
        schedule: &str,
        description: Option<&str>,
        next_run_at: DateTime<Utc>,
    ) -> Result<CronTask, CronError> {
        let row: CronRow = sqlx::query_as(
            "INSERT INTO core.cron_tasks (name, schedule, description, next_run_at) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (name) DO UPDATE \
             SET schedule = EXCLUDED.schedule, description = EXCLUDED.description, \
                 updated_at = now() \
             RETURNING name, schedule, description, paused, next_run_at, last_run_at, \
                       last_state, last_error",
        )
        .bind(name)
        .bind(schedule)
        .bind(description)
        .bind(next_run_at)
        .fetch_one(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(row.into())
    }

    async fn list(&self) -> Result<Vec<CronTask>, CronError> {
        let rows: Vec<CronRow> = sqlx::query_as(
            "SELECT name, schedule, description, paused, next_run_at, last_run_at, \
                    last_state, last_error \
             FROM core.cron_tasks ORDER BY name",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(rows.into_iter().map(CronTask::from).collect())
    }

    async fn get(&self, name: &str) -> Result<Option<CronTask>, CronError> {
        let row: Option<CronRow> = sqlx::query_as(
            "SELECT name, schedule, description, paused, next_run_at, last_run_at, \
                    last_state, last_error \
             FROM core.cron_tasks WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(row.map(CronTask::from))
    }

    async fn set_paused(&self, name: &str, paused: bool) -> Result<bool, CronError> {
        let result = sqlx::query(
            "UPDATE core.cron_tasks SET paused = $2, updated_at = now() WHERE name = $1",
        )
        .bind(name)
        .bind(paused)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(result.rows_affected() > 0)
    }

    async fn claim_due(
        &self,
        worker: &str,
        lease: Duration,
        known: &[String],
        prefixes: &[String],
    ) -> Result<Option<CronTask>, CronError> {
        let row: Option<CronRow> = sqlx::query_as(
            "UPDATE core.cron_tasks SET claimed_by = $1, \
                    claimed_until = now() + make_interval(secs => $2), updated_at = now() \
             WHERE name = ( \
                 SELECT name FROM core.cron_tasks \
                 WHERE paused = false AND next_run_at <= now() \
                   AND (name = ANY($3) OR EXISTS ( \
                       SELECT 1 FROM unnest($4::text[]) AS prefix WHERE starts_with(name, prefix))) \
                   AND (claimed_until IS NULL OR claimed_until < now()) \
                 ORDER BY next_run_at LIMIT 1 FOR UPDATE SKIP LOCKED) \
             RETURNING name, schedule, description, paused, next_run_at, last_run_at, \
                       last_state, last_error",
        )
        .bind(worker)
        .bind(lease.as_secs_f64())
        .bind(known)
        .bind(prefixes)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(row.map(CronTask::from))
    }

    async fn prune(&self, prefix: &str, keep: &[String]) -> Result<u64, CronError> {
        let result = sqlx::query(
            "DELETE FROM core.cron_tasks WHERE starts_with(name, $1) AND NOT (name = ANY($2))",
        )
        .bind(prefix)
        .bind(keep)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(result.rows_affected())
    }

    async fn finish(
        &self,
        name: &str,
        next_run_at: DateTime<Utc>,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), CronError> {
        sqlx::query(
            "UPDATE core.cron_tasks \
             SET last_run_at = now(), last_state = $2, last_error = $3, next_run_at = $4, \
                 claimed_by = NULL, claimed_until = NULL, updated_at = now() \
             WHERE name = $1",
        )
        .bind(name)
        .bind(state)
        .bind(error)
        .bind(next_run_at)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }
}
