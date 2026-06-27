//! Cache Bus in Postgres: the same TTLs, versions and eviction as the in-process one, in a table,
//! so every process of a deployment running without the clusters shares one cache.

use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::PgPool;

use crate::{CacheBus, CacheBusError, Entry, Namespace, NamespaceSpec};

const DEFAULT_MAX_ENTRIES: i64 = 100_000;

#[derive(Clone)]
pub struct PostgresCacheBus {
    pool: PgPool,
}

impl PostgresCacheBus {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn failed(err: &sqlx::Error) -> CacheBusError {
    CacheBusError::Unavailable(err.to_string())
}

fn seconds(ttl: Duration) -> i64 {
    ttl.as_secs().min(i64::MAX as u64) as i64
}

/// An entry is gone the moment it expires, whoever reads it, so a row that outlives its TTL is
/// never returned and is tidied away by the next write to its namespace.
fn entry(value: Value, version: i64, expires_at: Option<DateTime<Utc>>) -> Entry {
    Entry { value, version: version.max(0) as u64, expires_at }
}

impl PostgresCacheBus {
    async fn spec(&self, namespace: &Namespace) -> Result<(Option<i64>, i64), CacheBusError> {
        let row: Option<(Option<i64>, i64)> = sqlx::query_as(
            "SELECT default_ttl_s, max_entries FROM core.fabric_cache_specs WHERE namespace = $1",
        )
        .bind(namespace.as_str())
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(row.unwrap_or((None, DEFAULT_MAX_ENTRIES)))
    }

    /// Drops what has expired, then the oldest of what is left while the namespace is over its
    /// limit — the in-process bus's eviction, written as two statements.
    async fn evict(&self, namespace: &Namespace, max_entries: i64) -> Result<(), CacheBusError> {
        sqlx::query(
            "DELETE FROM core.fabric_cache
             WHERE namespace = $1 AND expires_at IS NOT NULL AND expires_at <= now()",
        )
        .bind(namespace.as_str())
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        sqlx::query(
            "DELETE FROM core.fabric_cache WHERE (namespace, key) IN (
                 SELECT namespace, key FROM core.fabric_cache WHERE namespace = $1
                 ORDER BY written_at DESC, key DESC OFFSET $2
             )",
        )
        .bind(namespace.as_str())
        .bind(max_entries)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }
}

#[async_trait]
impl CacheBus for PostgresCacheBus {
    async fn register_namespace(&self, spec: NamespaceSpec) -> Result<(), CacheBusError> {
        sqlx::query(
            "INSERT INTO core.fabric_cache_specs (namespace, default_ttl_s, max_entries)
             VALUES ($1, $2, $3)
             ON CONFLICT (namespace) DO UPDATE
                 SET default_ttl_s = excluded.default_ttl_s, max_entries = excluded.max_entries",
        )
        .bind(spec.namespace.as_str())
        .bind(spec.default_ttl.map(seconds))
        .bind(spec.max_entries as i64)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn get(&self, namespace: &Namespace, key: &str) -> Result<Option<Entry>, CacheBusError> {
        let row: Option<(Value, i64, Option<DateTime<Utc>>)> = sqlx::query_as(
            "SELECT value, version, expires_at FROM core.fabric_cache
             WHERE namespace = $1 AND key = $2 AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(namespace.as_str())
        .bind(key)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(row.map(|(value, version, expires_at)| entry(value, version, expires_at)))
    }

    async fn set(
        &self,
        namespace: &Namespace,
        key: &str,
        value: Value,
        ttl: Option<Duration>,
    ) -> Result<Entry, CacheBusError> {
        let (default_ttl, max_entries) = self.spec(namespace).await?;
        let ttl = ttl.map(seconds).or(default_ttl);
        let row: (Value, i64, Option<DateTime<Utc>>) = sqlx::query_as(
            "INSERT INTO core.fabric_cache (namespace, key, value, version, expires_at, written_at)
             VALUES ($1, $2, $3, 1, CASE WHEN $4::bigint IS NULL THEN NULL
                                         ELSE now() + make_interval(secs => $4::bigint) END, now())
             ON CONFLICT (namespace, key) DO UPDATE SET
                 value = excluded.value,
                 -- An entry that had already expired starts again at one, as a fresh one does.
                 version = CASE WHEN core.fabric_cache.expires_at IS NOT NULL
                                 AND core.fabric_cache.expires_at <= now()
                                THEN 1 ELSE core.fabric_cache.version + 1 END,
                 expires_at = excluded.expires_at,
                 written_at = now()
             RETURNING value, version, expires_at",
        )
        .bind(namespace.as_str())
        .bind(key)
        .bind(&value)
        .bind(ttl)
        .fetch_one(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        self.evict(namespace, max_entries).await?;
        Ok(entry(row.0, row.1, row.2))
    }

    async fn delete(&self, namespace: &Namespace, key: &str) -> Result<bool, CacheBusError> {
        let done = sqlx::query(
            "DELETE FROM core.fabric_cache
             WHERE namespace = $1 AND key = $2 AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(namespace.as_str())
        .bind(key)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(done.rows_affected() > 0)
    }

    async fn compare_and_set(
        &self,
        namespace: &Namespace,
        key: &str,
        expected: Option<u64>,
        value: Value,
        ttl: Option<Duration>,
    ) -> Result<Option<Entry>, CacheBusError> {
        let (default_ttl, _) = self.spec(namespace).await?;
        let ttl = ttl.map(seconds).or(default_ttl);
        // One statement, so two processes writing the same key cannot both believe they won: the
        // insert takes the key only if nobody holds it, and the update only if the version matches.
        let row: Option<(Value, i64, Option<DateTime<Utc>>)> = match expected {
            None => sqlx::query_as(
                "INSERT INTO core.fabric_cache (namespace, key, value, version, expires_at, written_at)
                 VALUES ($1, $2, $3, 1, CASE WHEN $4::bigint IS NULL THEN NULL
                                             ELSE now() + make_interval(secs => $4::bigint) END, now())
                 ON CONFLICT (namespace, key) DO UPDATE SET
                     value = excluded.value, version = 1,
                     expires_at = excluded.expires_at, written_at = now()
                 WHERE core.fabric_cache.expires_at IS NOT NULL
                   AND core.fabric_cache.expires_at <= now()
                 RETURNING value, version, expires_at",
            )
            .bind(namespace.as_str())
            .bind(key)
            .bind(&value)
            .bind(ttl)
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| failed(&err))?,
            Some(version) => sqlx::query_as(
                "UPDATE core.fabric_cache SET
                     value = $3,
                     version = version + 1,
                     expires_at = CASE WHEN $4::bigint IS NULL THEN NULL
                                       ELSE now() + make_interval(secs => $4::bigint) END,
                     written_at = now()
                 WHERE namespace = $1 AND key = $2 AND version = $5
                   AND (expires_at IS NULL OR expires_at > now())
                 RETURNING value, version, expires_at",
            )
            .bind(namespace.as_str())
            .bind(key)
            .bind(&value)
            .bind(ttl)
            .bind(version as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| failed(&err))?,
        };
        Ok(row.map(|(value, version, expires_at)| entry(value, version, expires_at)))
    }

    async fn clear(&self, namespace: &Namespace) -> Result<usize, CacheBusError> {
        let done = sqlx::query("DELETE FROM core.fabric_cache WHERE namespace = $1")
            .bind(namespace.as_str())
            .execute(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(done.rows_affected() as usize)
    }
}
