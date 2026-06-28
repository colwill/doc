//! Postgres pool, migrations and the Postgres repository implementations.

use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use sqlx::AssertSqlSafe;
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions};

use chrono::{DateTime, Utc};
use serde_json::Value;
use uuid::Uuid;

use super::repositories::{
    AccessRequestRecord, AuditFilter, AuditRecord, DeclaredPermission, Delegation,
    HealthRepository, IdentityRepository, ListedUser, NewToken, PluginRecord, PluginRepository,
    RepositoryError, SettingChange, StoredFeature, StoredPluginFlag, StoredSetting, StoredSwitch,
    TeamRepository, TokenRecord,
};
use crate::config::Database;
use crate::identity::{
    Account, ApiToken, Arrival, AuditEntry, Identity, NewUser, Principal, Profile, ServiceAccount,
    TokenOwner, User,
};
use crate::secrets::TokenKind;
use crate::status::plugins::{PluginChange, PluginStatus, PluginStatuses, Source, StatusChange};
use crate::status::{Component, Health, HistoryError, StatusHistory};
use crate::teams::{
    AccountOwner, BY_DEFAULT, BY_PROVIDER, NewTeam, Organisation, Owners, Position, Team,
    TeamChanges, TeamMember, TeamPosition,
};
use doc_plugin_protocol::PluginState;

pub const CORE_SCHEMA: &str = "core";
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

fn options(url: &str) -> Result<PgConnectOptions> {
    Ok(PgConnectOptions::from_str(url)
        .context("parsing the database URL")?
        .options([("search_path", CORE_SCHEMA)]))
}

pub async fn pool(config: &Database) -> Result<PgPool> {
    PgPoolOptions::new()
        .max_connections(config.max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(options(config.url.expose())?)
        .await
        .context("connecting to Postgres")
}

/// Waits for Postgres to accept connections, so a restart does not depend on start-up order.
pub async fn pool_with_retry(config: &Database, deadline: Duration) -> Result<PgPool> {
    let started = std::time::Instant::now();
    let mut wait = Duration::from_millis(250);
    loop {
        match pool(config).await {
            Ok(pool) => return Ok(pool),
            Err(err) if started.elapsed() + wait < deadline => {
                tracing::warn!(error = %err, retry_in_ms = wait.as_millis() as u64, "waiting for Postgres");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(Duration::from_secs(5));
            }
            Err(err) => return Err(err),
        }
    }
}

pub async fn migrate(config: &Database) -> Result<()> {
    let pool = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(options(config.migration_url.expose())?)
        .await
        .context("connecting to Postgres as the owner role")?;
    MIGRATOR.run(&pool).await.context("running migrations")?;
    withdraw_plugin_grants(&pool, config.url.expose()).await?;
    pool.close().await;
    Ok(())
}

/// Takes back what the application role could pass on to plugin roles before T62, and so what
/// those roles still hold: `CONNECT` on the core database and `SELECT` on `core_v1`. The role and
/// database names come from configuration, which is why this is not a migration.
async fn withdraw_plugin_grants(owner: &PgPool, app_url: &str) -> Result<()> {
    let app = PgConnectOptions::from_str(app_url).context("parsing the database URL")?;
    let database = quote_ident(app.get_database().unwrap_or(app.get_username()));
    let role = quote_ident(app.get_username());
    let statements = [
        format!("REVOKE GRANT OPTION FOR CONNECT ON DATABASE {database} FROM {role} CASCADE"),
        format!(
            "REVOKE GRANT OPTION FOR SELECT ON ALL TABLES IN SCHEMA core_v1 FROM {role} CASCADE"
        ),
        format!("REVOKE GRANT OPTION FOR USAGE ON SCHEMA core_v1 FROM {role} CASCADE"),
    ];
    for statement in statements {
        sqlx::query(AssertSqlSafe(statement))
            .execute(owner)
            .await
            .context("withdrawing what plugin roles were granted")?;
    }
    Ok(())
}

pub fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

pub struct PostgresHealth {
    pool: PgPool,
}

impl PostgresHealth {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

pub struct PostgresIdentity {
    pool: PgPool,
}

impl PostgresIdentity {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

fn failed(err: &sqlx::Error) -> RepositoryError {
    RepositoryError::Other(err.to_string())
}

/// Whether a write was refused by a unique constraint.
fn duplicate(err: &sqlx::Error) -> bool {
    err.as_database_error().and_then(|db| db.code()).is_some_and(|code| code == "23505")
}

/// The columns of a `User` read from `core.users` as `u`, with each linked account's login.
macro_rules! user_columns {
    () => {
        "u.id, u.login, u.organisation_id, u.name, u.email, u.first_name, u.surname, u.disabled, \
         u.first_signed_in_at, u.last_signed_in_at, \
         COALESCE((SELECT jsonb_object_agg(i.provider, i.login) FROM core.identities i \
                   WHERE i.user_id = u.id), '{}'::jsonb) AS linked"
    };
}

const IDENTITY_COLUMNS: &str = "id, user_id, provider, external_id, login, source, name, email, \
                                first_name, surname, reported_at, created_at, last_used_at";

async fn user_in(
    executor: impl sqlx::PgExecutor<'_>,
    id: Uuid,
) -> Result<Option<User>, RepositoryError> {
    sqlx::query_as(concat!("SELECT ", user_columns!(), " FROM core.users u WHERE u.id = $1"))
        .bind(id)
        .fetch_optional(executor)
        .await
        .map_err(|err| failed(&err))
}

async fn found(executor: impl sqlx::PgExecutor<'_>, id: Uuid) -> Result<User, RepositoryError> {
    user_in(executor, id).await?.ok_or_else(|| RepositoryError::Other(format!("user {id} is gone")))
}

#[async_trait]
impl IdentityRepository for PostgresIdentity {
    async fn service_account_by_name(
        &self,
        name: &str,
    ) -> Result<Option<ServiceAccount>, RepositoryError> {
        sqlx::query_as(
            "SELECT id, name, description, owner_id, owner_team_id, disabled, created_at \
             FROM core.service_accounts WHERE name = $1",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn create_service_account(
        &self,
        name: &str,
        description: Option<&str>,
        owner: AccountOwner,
    ) -> Result<ServiceAccount, RepositoryError> {
        let (user, team) = owner_columns_of(owner);
        sqlx::query_as(
            "INSERT INTO core.service_accounts (id, name, description, owner_id, owner_team_id) \
             VALUES ($1, $2, $3, $4, $5) \
             RETURNING id, name, description, owner_id, owner_team_id, disabled, created_at",
        )
        .bind(Uuid::now_v7())
        .bind(name)
        .bind(description)
        .bind(user)
        .bind(team)
        .fetch_one(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn service_account_by_id(
        &self,
        id: Uuid,
    ) -> Result<Option<ServiceAccount>, RepositoryError> {
        sqlx::query_as(
            "SELECT id, name, description, owner_id, owner_team_id, disabled, created_at \
             FROM core.service_accounts WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn plugin_service_account(
        &self,
        plugin: &str,
    ) -> Result<Option<ServiceAccount>, RepositoryError> {
        sqlx::query_as(
            "SELECT a.id, a.name, a.description, a.owner_id, a.owner_team_id, a.disabled, \
             a.created_at FROM core.plugin_service_accounts p \
             JOIN core.service_accounts a ON a.id = p.account_id WHERE p.plugin = $1",
        )
        .bind(plugin)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn link_plugin_service_account(
        &self,
        plugin: &str,
        account: Uuid,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.plugin_service_accounts (plugin, account_id) VALUES ($1, $2) \
             ON CONFLICT (plugin) DO UPDATE SET account_id = EXCLUDED.account_id",
        )
        .bind(plugin)
        .bind(account)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn dashboard(&self, user: Uuid) -> Result<Option<Vec<String>>, RepositoryError> {
        let items: Option<sqlx::types::Json<Vec<String>>> =
            sqlx::query_scalar("SELECT items FROM core.dashboards WHERE user_id = $1")
                .bind(user)
                .fetch_optional(&self.pool)
                .await
                .map_err(|err| failed(&err))?;
        Ok(items.map(|items| items.0))
    }

    async fn set_dashboard(&self, user: Uuid, items: &[String]) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.dashboards (user_id, items, updated_at) VALUES ($1, $2, now()) \
             ON CONFLICT (user_id) DO UPDATE SET items = EXCLUDED.items, updated_at = now()",
        )
        .bind(user)
        .bind(sqlx::types::Json(items))
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn service_account_plugin(
        &self,
        account: Uuid,
    ) -> Result<Option<String>, RepositoryError> {
        sqlx::query_scalar("SELECT plugin FROM core.plugin_service_accounts WHERE account_id = $1")
            .bind(account)
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| failed(&err))
    }

    async fn list_service_accounts(
        &self,
        owners: Option<&Owners>,
    ) -> Result<Vec<ServiceAccount>, RepositoryError> {
        sqlx::query_as(
            "SELECT id, name, description, owner_id, owner_team_id, disabled, created_at \
             FROM core.service_accounts \
             WHERE $1 OR owner_id = $2 OR owner_team_id = ANY($3) ORDER BY name",
        )
        .bind(owners.is_none())
        .bind(owners.map(|owners| owners.user))
        .bind(owners.map(|owners| owners.teams.clone()).unwrap_or_default())
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn set_service_account_owner(
        &self,
        id: Uuid,
        owner: AccountOwner,
    ) -> Result<Option<ServiceAccount>, RepositoryError> {
        let (user, team) = owner_columns_of(owner);
        sqlx::query_as(
            "UPDATE core.service_accounts \
             SET owner_id = $2, owner_team_id = $3, updated_at = now() WHERE id = $1 \
             RETURNING id, name, description, owner_id, owner_team_id, disabled, created_at",
        )
        .bind(id)
        .bind(user)
        .bind(team)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn set_service_account_disabled(
        &self,
        id: Uuid,
        disabled: bool,
    ) -> Result<Option<ServiceAccount>, RepositoryError> {
        sqlx::query_as(
            "UPDATE core.service_accounts SET disabled = $2, updated_at = now() WHERE id = $1 \
             RETURNING id, name, description, owner_id, owner_team_id, disabled, created_at",
        )
        .bind(id)
        .bind(disabled)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn user_by_id(&self, id: Uuid) -> Result<Option<User>, RepositoryError> {
        user_in(&self.pool, id).await
    }

    async fn list_users(&self) -> Result<Vec<ListedUser>, RepositoryError> {
        sqlx::query_as(concat!(
            "SELECT ",
            user_columns!(),
            ", u.created_at FROM core.users u ORDER BY u.login, u.created_at"
        ))
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn set_user_disabled(
        &self,
        id: Uuid,
        disabled: bool,
    ) -> Result<Option<User>, RepositoryError> {
        let changed =
            sqlx::query("UPDATE core.users SET disabled = $2, updated_at = now() WHERE id = $1")
                .bind(id)
                .bind(disabled)
                .execute(&self.pool)
                .await
                .map_err(|err| failed(&err))?;
        match changed.rows_affected() {
            0 => Ok(None),
            _ => user_in(&self.pool, id).await,
        }
    }

    async fn record_token(&self, token: NewToken) -> Result<bool, RepositoryError> {
        let kind = token.kind.stored_as().ok_or_else(|| {
            RepositoryError::Other(format!("{:?} tokens are not stored", token.kind))
        })?;
        let (user, account, plugin) = match &token.owner {
            TokenOwner::User(id) => (Some(*id), None, None),
            TokenOwner::ServiceAccount(id) => (None, Some(*id), None),
            TokenOwner::Plugin(id) => (None, None, Some(id.clone())),
        };
        let result = sqlx::query(
            "INSERT INTO core.api_tokens \
             (id, kind, token_hash, name, user_id, service_account_id, plugin_id, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) ON CONFLICT (token_hash) DO NOTHING",
        )
        .bind(Uuid::now_v7())
        .bind(kind)
        .bind(&token.token_hash)
        .bind(token.name.as_deref())
        .bind(user)
        .bind(account)
        .bind(plugin)
        .bind(token.expires_at)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(result.rows_affected() > 0)
    }

    async fn register_plugin(&self, id: &str) -> Result<bool, RepositoryError> {
        let result =
            sqlx::query("INSERT INTO core.plugins (id) VALUES ($1) ON CONFLICT DO NOTHING")
                .bind(id)
                .execute(&self.pool)
                .await
                .map_err(|err| failed(&err))?;
        Ok(result.rows_affected() > 0)
    }

    async fn list_plugins(&self) -> Result<Vec<String>, RepositoryError> {
        let rows: Vec<(String,)> = sqlx::query_as("SELECT id FROM core.plugins ORDER BY id")
            .fetch_all(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    async fn token_by_hash(&self, hash: &[u8]) -> Result<Option<TokenRecord>, RepositoryError> {
        let row: Option<TokenRow> = sqlx::query_as(TOKEN_SELECT)
            .bind(hash)
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(row.map(TokenRow::into_record).transpose()?.flatten())
    }

    async fn revoke_issued(
        &self,
        id: Uuid,
        plugin: &str,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "UPDATE core.api_tokens SET revoked_at = now() \
             WHERE id = $1 AND kind = 'scoped' AND issued_by = $2 AND revoked_at IS NULL \
             RETURNING token_hash",
        )
        .bind(id)
        .bind(plugin)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(row.map(|(hash,)| hash))
    }

    async fn touch_token(&self, id: Uuid) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE core.api_tokens SET last_used_at = now() WHERE id = $1")
            .bind(id)
            .execute(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn issue_token(&self, token: NewToken) -> Result<ApiToken, RepositoryError> {
        let kind = token.kind.stored_as().ok_or_else(|| {
            RepositoryError::Other(format!("{:?} tokens are not stored", token.kind))
        })?;
        let (user, account, plugin) = owner_columns(&token.owner);
        let row: TokenColumns = sqlx::query_as(
            "INSERT INTO core.api_tokens \
             (id, kind, token_hash, name, user_id, service_account_id, plugin_id, expires_at, \
              scopes, issued_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) \
             RETURNING id, kind, name, user_id, service_account_id, plugin_id, created_at, \
                       last_used_at, expires_at, revoked_at, scopes, issued_by",
        )
        .bind(Uuid::now_v7())
        .bind(kind)
        .bind(&token.token_hash)
        .bind(token.name.as_deref())
        .bind(user)
        .bind(account)
        .bind(plugin)
        .bind(token.expires_at)
        .bind(token.scopes.as_deref())
        .bind(token.issued_by.as_deref())
        .fetch_one(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        row.into_token()
    }

    async fn list_tokens(
        &self,
        owner: &TokenOwner,
        kind: TokenKind,
    ) -> Result<Vec<ApiToken>, RepositoryError> {
        let stored = kind
            .stored_as()
            .ok_or_else(|| RepositoryError::Other(format!("{kind:?} tokens are not stored")))?;
        let (user, account, plugin) = owner_columns(owner);
        let rows: Vec<TokenColumns> = sqlx::query_as(
            "SELECT id, kind, name, user_id, service_account_id, plugin_id, created_at, \
                    last_used_at, expires_at, revoked_at, scopes, issued_by \
             FROM core.api_tokens \
             WHERE kind = $1 AND revoked_at IS NULL \
               AND user_id IS NOT DISTINCT FROM $2 \
               AND service_account_id IS NOT DISTINCT FROM $3 \
               AND plugin_id IS NOT DISTINCT FROM $4 \
             ORDER BY created_at DESC",
        )
        .bind(stored)
        .bind(user)
        .bind(account)
        .bind(plugin)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        rows.into_iter().map(TokenColumns::into_token).collect()
    }

    async fn revoke_token(
        &self,
        id: Uuid,
        owner: &TokenOwner,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let (user, account, plugin) = owner_columns(owner);
        let row: Option<(Vec<u8>,)> = sqlx::query_as(
            "UPDATE core.api_tokens SET revoked_at = now() \
             WHERE id = $1 AND revoked_at IS NULL \
               AND user_id IS NOT DISTINCT FROM $2 \
               AND service_account_id IS NOT DISTINCT FROM $3 \
               AND plugin_id IS NOT DISTINCT FROM $4 \
             RETURNING token_hash",
        )
        .bind(id)
        .bind(user)
        .bind(account)
        .bind(plugin)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(row.map(|(hash,)| hash))
    }

    async fn user_for_account(
        &self,
        account: &Account,
        profile: &Profile,
        arrival: Arrival,
        organisation: Uuid,
    ) -> Result<(User, bool), RepositoryError> {
        // Two first sign-ins with one account race to insert it; the loser finds the winner's.
        for _ in 0..2 {
            if let Some(user) = self.known_account(account, profile, arrival).await? {
                return Ok((user, false));
            }
            if let Some(user) = self.new_account(account, profile, arrival, organisation).await? {
                return Ok((user, true));
            }
        }
        Err(RepositoryError::Other(format!(
            "{} account {} kept moving",
            account.provider, account.external_id
        )))
    }

    async fn create_user(
        &self,
        user: &NewUser,
        account: Option<&Account>,
    ) -> Result<User, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO core.users (id, login, organisation_id, name, email) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(id)
        .bind(&user.login)
        .bind(user.organisation_id)
        .bind(user.name.as_deref())
        .bind(user.email.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(|err| match foreign(&err) {
            true => conflict("that organisation is gone"),
            false => failed(&err),
        })?;
        place_in_default_teams(&mut tx, id, user.organisation_id).await?;
        if let Some(account) = account {
            insert_identity(&mut tx, id, account, "admin", false).await?.ok_or_else(|| {
                RepositoryError::Conflict(format!(
                    "the {} account {} belongs to someone already",
                    account.provider, account.login
                ))
            })?;
        }
        let user = found(&mut *tx, id).await?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(user)
    }

    async fn identity(
        &self,
        provider: &str,
        external_id: &str,
    ) -> Result<Option<Identity>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {IDENTITY_COLUMNS} FROM core.identities WHERE provider = $1 AND external_id = $2"
        )))
        .bind(provider)
        .bind(external_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn identities(&self, user: Option<Uuid>) -> Result<Vec<Identity>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {IDENTITY_COLUMNS} FROM core.identities \
             WHERE $1::uuid IS NULL OR user_id = $1 ORDER BY created_at, provider"
        )))
        .bind(user)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn attach_identity(
        &self,
        user: Uuid,
        account: &Account,
        source: &str,
    ) -> Result<Identity, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        let identity = insert_identity(&mut tx, user, account, source, false).await?.ok_or_else(|| {
            RepositoryError::Conflict(format!(
                "the {provider} account {} is linked to someone already, or this user has another \
                 {provider} account",
                account.login,
                provider = account.provider,
            ))
        })?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(identity)
    }

    async fn detach_identity(
        &self,
        user: Uuid,
        identity: Uuid,
    ) -> Result<Option<Identity>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "DELETE FROM core.identities WHERE id = $1 AND user_id = $2 RETURNING {IDENTITY_COLUMNS}"
        )))
        .bind(identity)
        .bind(user)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn merge_users(&self, from: Uuid, into: Uuid) -> Result<User, RepositoryError> {
        let conflict = |detail: &str| RepositoryError::Conflict(detail.to_string());
        if from == into {
            return Err(conflict("a user cannot be merged into themselves"));
        }
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        // Locked in a fixed order, so two merges of the same pair cannot deadlock.
        let locked: Vec<(Uuid,)> =
            sqlx::query_as("SELECT id FROM core.users WHERE id = ANY($1) ORDER BY id FOR UPDATE")
                .bind(vec![from, into])
                .fetch_all(&mut *tx)
                .await
                .map_err(|err| failed(&err))?;
        if locked.len() != 2 {
            return Err(conflict("one of the users is gone"));
        }
        let merged = found(&mut *tx, from).await?;
        if merged.organisation_id != found(&mut *tx, into).await?.organisation_id {
            return Err(conflict("they belong to different organisations"));
        }
        if merged.first_signed_in_at.is_some() {
            return Err(RepositoryError::Conflict(format!(
                "{} has signed in, so only an admin can decide between the two",
                merged.login
            )));
        }
        let shared: Option<(String,)> = sqlx::query_as(
            "SELECT a.provider FROM core.identities a \
             JOIN core.identities b ON b.provider = a.provider AND b.user_id = $2 \
             WHERE a.user_id = $1 LIMIT 1",
        )
        .bind(from)
        .bind(into)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        if let Some((provider,)) = shared {
            return Err(RepositoryError::Conflict(format!(
                "both have a {provider} account, so only an admin can decide between them"
            )));
        }
        let moves = [
            "UPDATE core.identities SET user_id = $2 WHERE user_id = $1",
            "UPDATE core.service_accounts SET owner_id = $2, updated_at = now() WHERE owner_id = $1",
            "UPDATE core.api_tokens SET user_id = $2 WHERE user_id = $1",
            "UPDATE core.delegations SET principal = 'user:' || $2::text \
             WHERE principal = 'user:' || $1::text",
            "INSERT INTO core.team_members (team_id, user_id, source, provider, position, created_at) \
             SELECT team_id, $2, source, provider, position, created_at FROM core.team_members \
             WHERE user_id = $1 ON CONFLICT (team_id, user_id) DO NOTHING",
            // They are in every team the other was in by now, so they lead the ones it led.
            "UPDATE core.teams SET lead_id = $2 WHERE lead_id = $1",
            "UPDATE core.users AS u SET name = COALESCE(u.name, f.name), \
                    email = COALESCE(u.email, f.email), updated_at = now() \
             FROM core.users f WHERE f.id = $1 AND u.id = $2",
        ];
        for statement in moves {
            sqlx::query(statement)
                .bind(from)
                .bind(into)
                .execute(&mut *tx)
                .await
                .map_err(|err| failed(&err))?;
        }
        sqlx::query("DELETE FROM core.users WHERE id = $1")
            .bind(from)
            .execute(&mut *tx)
            .await
            .map_err(|err| failed(&err))?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(merged)
    }

    async fn move_user(
        &self,
        id: Uuid,
        organisation: Uuid,
    ) -> Result<Option<User>, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        let moved = sqlx::query(
            "UPDATE core.users SET organisation_id = $2, updated_at = now() WHERE id = $1",
        )
        .bind(id)
        .bind(organisation)
        .execute(&mut *tx)
        .await
        .map_err(|err| match foreign(&err) {
            true => conflict("that organisation is gone"),
            false => failed(&err),
        })?;
        if moved.rows_affected() == 0 {
            return Ok(None);
        }
        sqlx::query(
            "DELETE FROM core.team_members m USING core.teams t \
             WHERE m.team_id = t.id AND m.user_id = $1 AND t.organisation_id <> $2",
        )
        .bind(id)
        .bind(organisation)
        .execute(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        place_in_default_teams(&mut tx, id, organisation).await?;
        let user = found(&mut *tx, id).await?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(Some(user))
    }

    async fn record_sign_in(&self, id: Uuid) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE core.users \
             SET first_signed_in_at = COALESCE(first_signed_in_at, now()), \
                 last_signed_in_at = now(), updated_at = now() \
             WHERE id = $1",
        )
        .bind(id)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn purge_expired_tokens(&self, before: DateTime<Utc>) -> Result<u64, RepositoryError> {
        let result = sqlx::query(
            "DELETE FROM core.api_tokens \
             WHERE (expires_at IS NOT NULL AND expires_at < $1) \
                OR (revoked_at IS NOT NULL AND revoked_at < $1)",
        )
        .bind(before)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(result.rows_affected())
    }

    async fn record_audit(&self, entry: AuditEntry) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.audit_log \
             (id, actor_kind, actor_id, actor_label, action, subject, detail, request_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
        )
        .bind(Uuid::now_v7())
        .bind(&entry.actor_kind)
        .bind(entry.actor_id.as_deref())
        .bind(entry.actor_label.as_deref())
        .bind(&entry.action)
        .bind(entry.subject.as_deref())
        .bind(sqlx::types::Json(&entry.detail))
        .bind(entry.request_id.as_deref())
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn audit_log(&self, filter: &AuditFilter) -> Result<Vec<AuditRecord>, RepositoryError> {
        sqlx::query_as(
            "SELECT at, actor_kind, actor_id, actor_label, action, subject, detail \
             FROM core.audit_log \
             WHERE ($1::text IS NULL OR starts_with(action, $1)) \
               AND ($2::text IS NULL OR actor_label = $2 OR actor_id = $2) \
               AND ($3::timestamptz IS NULL OR at < $3) \
             ORDER BY at DESC LIMIT $4",
        )
        .bind(filter.action.as_deref())
        .bind(filter.actor.as_deref())
        .bind(filter.before)
        .bind(i64::from(filter.limit))
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }
}

impl PostgresIdentity {
    /// The user a known account belongs to, brought up to date, or `None` if nobody has it yet.
    async fn known_account(
        &self,
        account: &Account,
        profile: &Profile,
        arrival: Arrival,
    ) -> Result<Option<User>, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        let held: Option<(Uuid, Uuid, String)> = sqlx::query_as(
            "SELECT id, user_id, login FROM core.identities \
             WHERE provider = $1 AND external_id = $2 FOR UPDATE",
        )
        .bind(&account.provider)
        .bind(&account.external_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        let Some((identity, user, was)) = held else { return Ok(None) };
        sqlx::query(
            "UPDATE core.identities SET login = $2, \
                    last_used_at = CASE WHEN $3 THEN now() ELSE last_used_at END \
             WHERE id = $1",
        )
        .bind(identity)
        .bind(&account.login)
        .bind(arrival == Arrival::SignIn)
        .execute(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        report(&mut tx, identity, profile).await?;
        sqlx::query(
            "UPDATE core.users SET name = COALESCE($2, name), email = COALESCE($3, email), \
                    first_name = COALESCE($4, first_name), surname = COALESCE($5, surname), \
                    login = CASE WHEN login = $6 THEN $7 ELSE login END, updated_at = now() \
             WHERE id = $1",
        )
        .bind(user)
        .bind(profile.name.as_deref())
        .bind(profile.email.as_deref())
        .bind(profile.first_name.as_deref())
        .bind(profile.surname.as_deref())
        .bind(&was)
        .bind(&account.login)
        .execute(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        let user = found(&mut *tx, user).await?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(Some(user))
    }

    /// A new user with the account, in `organisation`, or `None` if someone else's arrived with
    /// it first.
    async fn new_account(
        &self,
        account: &Account,
        profile: &Profile,
        arrival: Arrival,
        organisation: Uuid,
    ) -> Result<Option<User>, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        let id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO core.users (id, login, organisation_id, name, email, first_name, surname) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id)
        .bind(&account.login)
        .bind(organisation)
        .bind(profile.name.as_deref())
        .bind(profile.email.as_deref())
        .bind(profile.first_name.as_deref())
        .bind(profile.surname.as_deref())
        .execute(&mut *tx)
        .await
        .map_err(|err| match foreign(&err) {
            true => conflict("that organisation is gone"),
            false => failed(&err),
        })?;
        let used = arrival == Arrival::SignIn;
        let Some(identity) = insert_identity(&mut tx, id, account, arrival.source(), used).await?
        else {
            return Ok(None);
        };
        report(&mut tx, identity.id, profile).await?;
        place_in_default_teams(&mut tx, id, organisation).await?;
        let user = found(&mut *tx, id).await?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(Some(user))
    }
}

const ORGANISATION_COLUMNS: &str = "id, name, title, description, created_at";
const TEAM_COLUMNS: &str = "id, organisation_id, parent_id, name, title, description, email, \
                            is_default, provider, external_id, lead_id, created_at";
const MEMBER_COLUMNS: &str = "team_id, user_id, source, provider, position, created_at";
const POSITION_COLUMNS: &str =
    "id, organisation_id, name, title, description, responsibilities, created_at";
const TEAM_POSITION_COLUMNS: &str =
    "team_id, name, title, description, added, removed, hidden, created_at";

fn conflict(detail: impl Into<String>) -> RepositoryError {
    RepositoryError::Conflict(detail.into())
}

/// Serialises changes to one organisation's teams, so two of them cannot build a loop between them.
async fn lock_organisation(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
) -> Result<(), RepositoryError> {
    let found: Option<(Uuid,)> =
        sqlx::query_as("SELECT id FROM core.organisations WHERE id = $1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|err| failed(&err))?;
    found.map(drop).ok_or_else(|| conflict("that organisation is gone"))
}

/// The team, with its organisation and then its own row locked, in that order everywhere.
async fn locked_team(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    id: Uuid,
) -> Result<Option<Team>, RepositoryError> {
    let found: Option<(Uuid,)> =
        sqlx::query_as("SELECT organisation_id FROM core.teams WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|err| failed(&err))?;
    let Some((organisation,)) = found else { return Ok(None) };
    lock_organisation(tx, organisation).await?;
    sqlx::query_as(AssertSqlSafe(format!(
        "SELECT {TEAM_COLUMNS} FROM core.teams WHERE id = $1 FOR UPDATE"
    )))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|err| failed(&err))
}

/// The organisation a team is in, or a `Conflict` when it is gone.
async fn organisation_of(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    team: Uuid,
) -> Result<Uuid, RepositoryError> {
    let found: Option<(Uuid,)> =
        sqlx::query_as("SELECT organisation_id FROM core.teams WHERE id = $1")
            .bind(team)
            .fetch_optional(&mut **tx)
            .await
            .map_err(|err| failed(&err))?;
    found.map(|(organisation,)| organisation).ok_or_else(|| conflict("that parent team is gone"))
}

/// Refuses taking the default mark off the last default team of an organisation, holding the rows
/// of its default teams.
async fn keep_a_default(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    team: Uuid,
) -> Result<(), RepositoryError> {
    let defaults: Vec<(Uuid,)> = sqlx::query_as(
        "SELECT id FROM core.teams WHERE is_default \
         AND organisation_id = (SELECT organisation_id FROM core.teams WHERE id = $1) \
         ORDER BY id FOR UPDATE",
    )
    .bind(team)
    .fetch_all(&mut **tx)
    .await
    .map_err(|err| failed(&err))?;
    match defaults.as_slice() {
        [(only,)] if *only == team => Err(conflict(
            "it is its organisation's last default team, and there is always one: mark another \
             default first",
        )),
        _ => Ok(()),
    }
}

#[async_trait]
impl TeamRepository for PostgresIdentity {
    async fn organisations(&self) -> Result<Vec<Organisation>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {ORGANISATION_COLUMNS} FROM core.organisations ORDER BY title, name"
        )))
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn organisation(&self, id: Uuid) -> Result<Option<Organisation>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {ORGANISATION_COLUMNS} FROM core.organisations WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn organisation_named(
        &self,
        name: &str,
    ) -> Result<Option<Organisation>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {ORGANISATION_COLUMNS} FROM core.organisations WHERE name = $1"
        )))
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn create_organisation(
        &self,
        name: &str,
        title: &str,
        description: &str,
    ) -> Result<Organisation, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "INSERT INTO core.organisations (id, name, title, description) \
             VALUES ($1, $2, $3, $4) RETURNING {ORGANISATION_COLUMNS}"
        )))
        .bind(Uuid::now_v7())
        .bind(name)
        .bind(title)
        .bind(description)
        .fetch_one(&self.pool)
        .await
        .map_err(|err| match duplicate(&err) {
            true => conflict(format!("there is an organisation called {name} already")),
            false => failed(&err),
        })
    }

    async fn update_organisation(
        &self,
        id: Uuid,
        name: &str,
        title: &str,
        description: &str,
    ) -> Result<Option<Organisation>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "UPDATE core.organisations SET name = $2, title = $3, description = $4, \
                    updated_at = now() \
             WHERE id = $1 RETURNING {ORGANISATION_COLUMNS}"
        )))
        .bind(id)
        .bind(name)
        .bind(title)
        .bind(description)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| match duplicate(&err) {
            true => conflict(format!("there is an organisation called {name} already")),
            false => failed(&err),
        })
    }

    async fn delete_organisation(&self, id: Uuid) -> Result<Option<Organisation>, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        let found: Option<(Uuid,)> =
            sqlx::query_as("SELECT id FROM core.organisations WHERE id = $1 FOR UPDATE")
                .bind(id)
                .fetch_optional(&mut *tx)
                .await
                .map_err(|err| failed(&err))?;
        if found.is_none() {
            return Ok(None);
        }
        let (teams, people): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM core.teams WHERE organisation_id = $1), \
                    (SELECT count(*) FROM core.users WHERE organisation_id = $1)",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        if teams > 0 {
            return Err(conflict("it still has teams: move or delete them first"));
        }
        if people > 0 {
            return Err(conflict("people belong to it: move them to another organisation first"));
        }
        let deleted = sqlx::query_as(AssertSqlSafe(format!(
            "DELETE FROM core.organisations WHERE id = $1 RETURNING {ORGANISATION_COLUMNS}"
        )))
        .bind(id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(deleted)
    }

    async fn teams(&self) -> Result<Vec<Team>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {TEAM_COLUMNS} FROM core.teams ORDER BY organisation_id, name"
        )))
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn team(&self, id: Uuid) -> Result<Option<Team>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {TEAM_COLUMNS} FROM core.teams WHERE id = $1"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn provided_team(
        &self,
        provider: &str,
        external_id: &str,
    ) -> Result<Option<Team>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {TEAM_COLUMNS} FROM core.teams WHERE provider = $1 AND external_id = $2"
        )))
        .bind(provider)
        .bind(external_id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn create_team(&self, team: &NewTeam) -> Result<Team, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        lock_organisation(&mut tx, team.organisation_id).await?;
        if let Some(parent) = team.parent_id
            && organisation_of(&mut tx, parent).await? != team.organisation_id
        {
            return Err(conflict("a sub-team is in its parent's organisation"));
        }
        let (provider, external_id) = team.provided.clone().unzip();
        let created = sqlx::query_as(AssertSqlSafe(format!(
            "INSERT INTO core.teams (id, organisation_id, parent_id, name, title, description, \
                                     email, is_default, provider, external_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10) RETURNING {TEAM_COLUMNS}"
        )))
        .bind(Uuid::now_v7())
        .bind(team.organisation_id)
        .bind(team.parent_id)
        .bind(&team.name)
        .bind(&team.title)
        .bind(&team.description)
        .bind(&team.email)
        .bind(team.is_default)
        .bind(provider)
        .bind(external_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|err| match duplicate(&err) {
            true => conflict(format!("there is a team called {} there already", team.name)),
            false => failed(&err),
        })?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(created)
    }

    async fn update_team(
        &self,
        id: Uuid,
        changes: &TeamChanges,
    ) -> Result<Option<Team>, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        let Some(team) = locked_team(&mut tx, id).await? else { return Ok(None) };
        if let Some(Some(parent)) = changes.parent {
            if organisation_of(&mut tx, parent).await? != team.organisation_id {
                return Err(conflict("a sub-team is in its parent's organisation"));
            }
            let (below,): (bool,) = sqlx::query_as(
                "WITH RECURSIVE above (id, parent_id) AS ( \
                     SELECT id, parent_id FROM core.teams WHERE id = $1 \
                     UNION SELECT t.id, t.parent_id FROM core.teams t \
                     JOIN above ON t.id = above.parent_id) \
                 SELECT EXISTS (SELECT 1 FROM above WHERE id = $2)",
            )
            .bind(parent)
            .bind(id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|err| failed(&err))?;
            if below {
                return Err(conflict("a team cannot sit inside itself or its own sub-teams"));
            }
        }
        if changes.is_default == Some(false) && team.is_default {
            keep_a_default(&mut tx, id).await?;
        }
        let updated = sqlx::query_as(AssertSqlSafe(format!(
            "UPDATE core.teams SET name = COALESCE($2, name), title = COALESCE($3, title), \
                    description = COALESCE($4, description), email = COALESCE($8, email), \
                    parent_id = CASE WHEN $5 THEN $6 ELSE parent_id END, \
                    is_default = COALESCE($7, is_default), updated_at = now() \
             WHERE id = $1 RETURNING {TEAM_COLUMNS}"
        )))
        .bind(id)
        .bind(changes.name.as_deref())
        .bind(changes.title.as_deref())
        .bind(changes.description.as_deref())
        .bind(changes.parent.is_some())
        .bind(changes.parent.flatten())
        .bind(changes.is_default)
        .bind(changes.email.as_deref())
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| match duplicate(&err) {
            true => conflict("there is a team called that in its organisation already"),
            false => failed(&err),
        })?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(updated)
    }

    async fn delete_team(&self, id: Uuid) -> Result<Option<Team>, RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        let Some(team) = locked_team(&mut tx, id).await? else { return Ok(None) };
        let (children, accounts): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM core.teams WHERE parent_id = $1), \
                    (SELECT count(*) FROM core.service_accounts WHERE owner_team_id = $1)",
        )
        .bind(id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        if children > 0 {
            return Err(conflict("it has sub-teams: move or delete them first"));
        }
        if accounts > 0 {
            return Err(conflict("it owns service accounts: give them to someone else first"));
        }
        if team.is_default {
            keep_a_default(&mut tx, id).await?;
        }
        // Its lead is one of the members going with it: let go of them first.
        sqlx::query("UPDATE core.teams SET lead_id = NULL WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|err| failed(&err))?;
        sqlx::query("DELETE FROM core.teams WHERE id = $1")
            .bind(id)
            .execute(&mut *tx)
            .await
            .map_err(|err| failed(&err))?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(Some(team))
    }

    async fn release_team(&self, id: Uuid) -> Result<Option<Team>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "UPDATE core.teams SET provider = NULL, external_id = NULL, updated_at = now() \
             WHERE id = $1 RETURNING {TEAM_COLUMNS}"
        )))
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn members(&self, team: Option<Uuid>) -> Result<Vec<TeamMember>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {MEMBER_COLUMNS} FROM core.team_members \
             WHERE $1::uuid IS NULL OR team_id = $1 ORDER BY created_at, user_id"
        )))
        .bind(team)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn memberships(&self, user: Uuid) -> Result<Vec<TeamMember>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {MEMBER_COLUMNS} FROM core.team_members WHERE user_id = $1 ORDER BY created_at"
        )))
        .bind(user)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn add_member(
        &self,
        team: Uuid,
        user: Uuid,
        source: &str,
        provider: Option<&str>,
    ) -> Result<bool, RepositoryError> {
        let (same,): (Option<bool>,) = sqlx::query_as(
            "SELECT (SELECT organisation_id FROM core.teams WHERE id = $1) = \
                    (SELECT organisation_id FROM core.users WHERE id = $2)",
        )
        .bind(team)
        .bind(user)
        .fetch_one(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        if same == Some(false) {
            return Err(conflict("they belong to another organisation, and teams hold their own"));
        }
        let added = sqlx::query(
            "INSERT INTO core.team_members (team_id, user_id, source, provider) \
             VALUES ($1, $2, $3, $4) ON CONFLICT DO NOTHING",
        )
        .bind(team)
        .bind(user)
        .bind(source)
        .bind(provider)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(added.rows_affected() > 0)
    }

    async fn remove_member(
        &self,
        team: Uuid,
        user: Uuid,
    ) -> Result<Option<TeamMember>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "DELETE FROM core.team_members WHERE team_id = $1 AND user_id = $2 \
             RETURNING {MEMBER_COLUMNS}"
        )))
        .bind(team)
        .bind(user)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn set_lead(
        &self,
        team: Uuid,
        lead: Option<Uuid>,
    ) -> Result<Option<Team>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "UPDATE core.teams SET lead_id = $2, updated_at = now() WHERE id = $1 \
             RETURNING {TEAM_COLUMNS}"
        )))
        .bind(team)
        .bind(lead)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| match foreign(&err) {
            true => conflict("a team's lead is one of its members"),
            false => failed(&err),
        })
    }

    async fn set_member_position(
        &self,
        team: Uuid,
        user: Uuid,
        position: Option<&str>,
    ) -> Result<Option<TeamMember>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "UPDATE core.team_members SET position = $3 WHERE team_id = $1 AND user_id = $2 \
             RETURNING {MEMBER_COLUMNS}"
        )))
        .bind(team)
        .bind(user)
        .bind(position)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn positions(
        &self,
        organisation: Option<Uuid>,
    ) -> Result<Vec<Position>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {POSITION_COLUMNS} FROM core.positions \
             WHERE $1::uuid IS NULL OR organisation_id = $1 ORDER BY organisation_id, name"
        )))
        .bind(organisation)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn put_position(
        &self,
        organisation: Uuid,
        name: &str,
        title: &str,
        description: &str,
        responsibilities: &[String],
    ) -> Result<Position, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "INSERT INTO core.positions \
                 (id, organisation_id, name, title, description, responsibilities) \
             VALUES ($1, $2, $3, $4, $5, $6) \
             ON CONFLICT (organisation_id, name) DO UPDATE SET title = EXCLUDED.title, \
                 description = EXCLUDED.description, \
                 responsibilities = EXCLUDED.responsibilities, updated_at = now() \
             RETURNING {POSITION_COLUMNS}"
        )))
        .bind(Uuid::now_v7())
        .bind(organisation)
        .bind(name)
        .bind(title)
        .bind(description)
        .bind(responsibilities)
        .fetch_one(&self.pool)
        .await
        .map_err(|err| match foreign(&err) {
            true => conflict("that organisation is gone"),
            false => failed(&err),
        })
    }

    async fn delete_position(
        &self,
        organisation: Uuid,
        name: &str,
    ) -> Result<Option<Position>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "DELETE FROM core.positions WHERE organisation_id = $1 AND name = $2 \
             RETURNING {POSITION_COLUMNS}"
        )))
        .bind(organisation)
        .bind(name)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn team_positions(
        &self,
        team: Option<Uuid>,
    ) -> Result<Vec<TeamPosition>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "SELECT {TEAM_POSITION_COLUMNS} FROM core.team_positions \
             WHERE $1::uuid IS NULL OR team_id = $1 ORDER BY team_id, name"
        )))
        .bind(team)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn put_team_position(&self, change: &TeamPosition) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.team_positions \
                 (team_id, name, title, description, added, removed, hidden) \
             VALUES ($1, $2, $3, $4, $5, $6, $7) \
             ON CONFLICT (team_id, name) DO UPDATE SET title = EXCLUDED.title, \
                 description = EXCLUDED.description, added = EXCLUDED.added, \
                 removed = EXCLUDED.removed, hidden = EXCLUDED.hidden, updated_at = now()",
        )
        .bind(change.team_id)
        .bind(&change.name)
        .bind(&change.title)
        .bind(&change.description)
        .bind(&change.added)
        .bind(&change.removed)
        .bind(change.hidden)
        .execute(&self.pool)
        .await
        .map_err(|err| match foreign(&err) {
            true => conflict("that team is gone"),
            false => failed(&err),
        })?;
        Ok(())
    }

    async fn delete_team_position(&self, team: Uuid, name: &str) -> Result<bool, RepositoryError> {
        let deleted =
            sqlx::query("DELETE FROM core.team_positions WHERE team_id = $1 AND name = $2")
                .bind(team)
                .bind(name)
                .execute(&self.pool)
                .await
                .map_err(|err| failed(&err))?;
        Ok(deleted.rows_affected() > 0)
    }

    async fn vacate_position(
        &self,
        teams: &[Uuid],
        name: &str,
    ) -> Result<Vec<TeamMember>, RepositoryError> {
        sqlx::query_as(AssertSqlSafe(format!(
            "UPDATE core.team_members SET position = NULL \
             WHERE team_id = ANY($1) AND position = $2 RETURNING {MEMBER_COLUMNS}"
        )))
        .bind(teams)
        .bind(name)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn providers(&self) -> Result<Vec<(String, Uuid)>, RepositoryError> {
        sqlx::query_as(
            "SELECT provider, organisation_id FROM core.organisation_providers ORDER BY provider",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn set_providers(
        &self,
        organisation: Uuid,
        providers: &[String],
    ) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        lock_organisation(&mut tx, organisation).await?;
        let taken: Option<(String,)> = sqlx::query_as(
            "SELECT provider FROM core.organisation_providers \
             WHERE provider = ANY($1) AND organisation_id <> $2 LIMIT 1",
        )
        .bind(providers)
        .bind(organisation)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        if let Some((provider,)) = taken {
            return Err(conflict(format!(
                "another organisation signs in with {provider}, and each provider serves one"
            )));
        }
        sqlx::query(
            "DELETE FROM core.organisation_providers \
             WHERE organisation_id = $1 AND NOT (provider = ANY($2))",
        )
        .bind(organisation)
        .bind(providers)
        .execute(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        sqlx::query(
            "INSERT INTO core.organisation_providers (provider, organisation_id) \
             SELECT unnest($2::text[]), $1 ON CONFLICT (provider) DO NOTHING",
        )
        .bind(organisation)
        .bind(providers)
        .execute(&mut *tx)
        .await
        .map_err(|err| match duplicate(&err) {
            true => conflict("another organisation chose one of them at the same moment"),
            false => failed(&err),
        })?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn domains(&self) -> Result<Vec<(String, Uuid)>, RepositoryError> {
        sqlx::query_as(
            "SELECT domain, organisation_id FROM core.organisation_domains ORDER BY domain",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))
    }

    async fn set_domains(
        &self,
        organisation: Uuid,
        domains: &[String],
    ) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        lock_organisation(&mut tx, organisation).await?;
        let taken: Option<(String,)> = sqlx::query_as(
            "SELECT domain FROM core.organisation_domains \
             WHERE domain = ANY($1) AND organisation_id <> $2 LIMIT 1",
        )
        .bind(domains)
        .bind(organisation)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        if let Some((domain,)) = taken {
            return Err(conflict(format!(
                "another organisation approved {domain}, and each domain belongs to one"
            )));
        }
        sqlx::query(
            "DELETE FROM core.organisation_domains \
             WHERE organisation_id = $1 AND NOT (domain = ANY($2))",
        )
        .bind(organisation)
        .bind(domains)
        .execute(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        sqlx::query(
            "INSERT INTO core.organisation_domains (domain, organisation_id) \
             SELECT unnest($2::text[]), $1 ON CONFLICT (domain) DO NOTHING",
        )
        .bind(organisation)
        .bind(domains)
        .execute(&mut *tx)
        .await
        .map_err(|err| match duplicate(&err) {
            true => conflict("another organisation approved one of them at the same moment"),
            false => failed(&err),
        })?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn set_provided_members(
        &self,
        team: Uuid,
        provider: &str,
        users: &[Uuid],
    ) -> Result<(Vec<Uuid>, Vec<Uuid>), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        let removed: Vec<(Uuid,)> = sqlx::query_as(
            "DELETE FROM core.team_members \
             WHERE team_id = $1 AND source = $2 AND provider = $3 AND NOT (user_id = ANY($4)) \
             RETURNING user_id",
        )
        .bind(team)
        .bind(BY_PROVIDER)
        .bind(provider)
        .bind(users)
        .fetch_all(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        // Someone who is not a user yet is left out: the provider makes them first.
        let added: Vec<(Uuid,)> = sqlx::query_as(
            "INSERT INTO core.team_members (team_id, user_id, source, provider) \
             SELECT $1, users.id, $2, $3 FROM core.users WHERE users.id = ANY($4) \
             AND users.organisation_id = (SELECT organisation_id FROM core.teams WHERE id = $1) \
             ON CONFLICT DO NOTHING RETURNING user_id",
        )
        .bind(team)
        .bind(BY_PROVIDER)
        .bind(provider)
        .bind(users)
        .fetch_all(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        tx.commit().await.map_err(|err| failed(&err))?;
        let ids = |rows: Vec<(Uuid,)>| rows.into_iter().map(|(id,)| id).collect();
        Ok((ids(added), ids(removed)))
    }
}

/// Everyone is placed in every default team of their organisation as they are made (ADR-0004).
async fn place_in_default_teams(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: Uuid,
    organisation: Uuid,
) -> Result<(), RepositoryError> {
    sqlx::query(
        "INSERT INTO core.team_members (team_id, user_id, source) \
         SELECT id, $1, $2 FROM core.teams WHERE is_default AND organisation_id = $3 \
         ON CONFLICT DO NOTHING",
    )
    .bind(user)
    .bind(BY_DEFAULT)
    .bind(organisation)
    .execute(&mut **tx)
    .await
    .map_err(|err| failed(&err))?;
    Ok(())
}

/// Keeps what a provider last reported of the person behind an identity; what it leaves out stays.
async fn report(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    identity: Uuid,
    profile: &Profile,
) -> Result<(), RepositoryError> {
    if profile.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "UPDATE core.identities SET name = COALESCE($2, name), email = COALESCE($3, email), \
                first_name = COALESCE($4, first_name), surname = COALESCE($5, surname), \
                reported_at = now() \
         WHERE id = $1",
    )
    .bind(identity)
    .bind(profile.name.as_deref())
    .bind(profile.email.as_deref())
    .bind(profile.first_name.as_deref())
    .bind(profile.surname.as_deref())
    .execute(&mut **tx)
    .await
    .map_err(|err| failed(&err))?;
    Ok(())
}

/// Whether a write was refused because something it names is gone.
fn foreign(err: &sqlx::Error) -> bool {
    err.as_database_error().and_then(|db| db.code()).is_some_and(|code| code == "23503")
}

fn owner_columns_of(owner: AccountOwner) -> (Option<Uuid>, Option<Uuid>) {
    match owner {
        AccountOwner::Platform => (None, None),
        AccountOwner::User(user) => (Some(user), None),
        AccountOwner::Team(team) => (None, Some(team)),
    }
}

/// `None` when the account belongs to someone, or the user has one with its provider already.
async fn insert_identity(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    user: Uuid,
    account: &Account,
    source: &str,
    used: bool,
) -> Result<Option<Identity>, RepositoryError> {
    let inserted = sqlx::query_as(AssertSqlSafe(format!(
        "INSERT INTO core.identities \
         (id, user_id, provider, external_id, login, source, last_used_at) \
         VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $7 THEN now() END) \
         RETURNING {IDENTITY_COLUMNS}"
    )))
    .bind(Uuid::now_v7())
    .bind(user)
    .bind(&account.provider)
    .bind(&account.external_id)
    .bind(&account.login)
    .bind(source)
    .bind(used)
    .fetch_one(&mut **tx)
    .await;
    match inserted {
        Ok(identity) => Ok(Some(identity)),
        Err(err) if duplicate(&err) => Ok(None),
        Err(err) => Err(failed(&err)),
    }
}

const TOKEN_SELECT: &str = "SELECT t.id, t.kind, t.name, t.user_id, t.service_account_id, \
                                   t.plugin_id, t.created_at, t.last_used_at, t.expires_at, \
                                   t.revoked_at, t.scopes, t.issued_by, \
                                   u.login, u.name AS user_name, u.organisation_id, u.first_name, \
                                   u.surname, \
                                   u.email, u.disabled AS user_disabled, u.first_signed_in_at, \
                                   u.last_signed_in_at, \
                                   (SELECT jsonb_object_agg(i.provider, i.login) \
                                    FROM core.identities i WHERE i.user_id = u.id) AS linked, \
                                   s.name AS account_name, s.description AS account_description, \
                                   s.owner_id, s.owner_team_id, s.disabled AS account_disabled, \
                                   s.created_at AS account_created_at \
                            FROM core.api_tokens t \
                            LEFT JOIN core.users u ON u.id = t.user_id \
                            LEFT JOIN core.service_accounts s ON s.id = t.service_account_id \
                            WHERE t.token_hash = $1";

fn owner_columns(owner: &TokenOwner) -> (Option<Uuid>, Option<Uuid>, Option<String>) {
    match owner {
        TokenOwner::User(id) => (Some(*id), None, None),
        TokenOwner::ServiceAccount(id) => (None, Some(*id), None),
        TokenOwner::Plugin(id) => (None, None, Some(id.clone())),
    }
}

fn kind_from_stored(kind: &str) -> Result<TokenKind, RepositoryError> {
    Ok(match kind {
        "session" => TokenKind::Session,
        "personal" => TokenKind::Personal,
        "service" => TokenKind::Service,
        "plugin-registration" => TokenKind::PluginRegistration,
        "operator" => TokenKind::Operator,
        "scoped" => TokenKind::Scoped,
        other => return Err(RepositoryError::Other(format!("unknown token kind {other}"))),
    })
}

#[derive(sqlx::FromRow)]
struct TokenColumns {
    id: Uuid,
    kind: String,
    name: Option<String>,
    user_id: Option<Uuid>,
    service_account_id: Option<Uuid>,
    plugin_id: Option<String>,
    created_at: DateTime<Utc>,
    last_used_at: Option<DateTime<Utc>>,
    expires_at: Option<DateTime<Utc>>,
    revoked_at: Option<DateTime<Utc>>,
    scopes: Option<Vec<String>>,
    issued_by: Option<String>,
}

impl TokenColumns {
    fn owner(&self) -> Result<TokenOwner, RepositoryError> {
        match (self.user_id, self.service_account_id, &self.plugin_id) {
            (Some(id), _, _) => Ok(TokenOwner::User(id)),
            (_, Some(id), _) => Ok(TokenOwner::ServiceAccount(id)),
            (_, _, Some(id)) => Ok(TokenOwner::Plugin(id.clone())),
            _ => Err(RepositoryError::Other("a token row has no owner".into())),
        }
    }

    fn into_token(self) -> Result<ApiToken, RepositoryError> {
        Ok(ApiToken {
            id: self.id,
            kind: kind_from_stored(&self.kind)?,
            name: self.name.clone(),
            owner: self.owner()?,
            created_at: self.created_at,
            last_used_at: self.last_used_at,
            expires_at: self.expires_at,
            revoked_at: self.revoked_at,
            scopes: self.scopes.clone(),
            issued_by: self.issued_by.clone(),
        })
    }
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    #[sqlx(flatten)]
    token: TokenColumns,
    login: Option<String>,
    user_name: Option<String>,
    organisation_id: Option<Uuid>,
    first_name: Option<String>,
    surname: Option<String>,
    email: Option<String>,
    user_disabled: Option<bool>,
    first_signed_in_at: Option<DateTime<Utc>>,
    last_signed_in_at: Option<DateTime<Utc>>,
    linked: Option<sqlx::types::Json<std::collections::BTreeMap<String, String>>>,
    account_name: Option<String>,
    account_description: Option<String>,
    owner_id: Option<Uuid>,
    owner_team_id: Option<Uuid>,
    account_disabled: Option<bool>,
    account_created_at: Option<DateTime<Utc>>,
}

impl TokenRow {
    /// A token whose owner row has been deleted authenticates nobody, so it reads as absent.
    fn into_record(self) -> Result<Option<TokenRecord>, RepositoryError> {
        let owner = self.token.owner()?;
        let principal = match &owner {
            TokenOwner::User(id) => Principal::User(User {
                id: *id,
                login: self.login.unwrap_or_default(),
                organisation_id: self.organisation_id.unwrap_or_default(),
                first_name: self.first_name,
                surname: self.surname,
                name: self.user_name,
                email: self.email,
                disabled: self.user_disabled.unwrap_or(true),
                first_signed_in_at: self.first_signed_in_at,
                last_signed_in_at: self.last_signed_in_at,
                linked: self.linked.map(|linked| linked.0).unwrap_or_default(),
                scopes: None,
            }),
            TokenOwner::ServiceAccount(id) => {
                let Some(name) = self.account_name else { return Ok(None) };
                Principal::ServiceAccount(ServiceAccount {
                    id: *id,
                    name,
                    description: self.account_description,
                    owner_id: self.owner_id,
                    owner_team_id: self.owner_team_id,
                    disabled: self.account_disabled.unwrap_or(true),
                    created_at: self.account_created_at.unwrap_or_else(Utc::now),
                })
            }
            TokenOwner::Plugin(id) => Principal::Plugin { id: id.clone() },
        };
        Ok(Some(TokenRecord { token: self.token.into_token()?, principal }))
    }
}

const PING_TIMEOUT: Duration = Duration::from_secs(2);

#[async_trait]
impl HealthRepository for PostgresHealth {
    async fn ping(&self) -> Result<(), RepositoryError> {
        let query = sqlx::query("SELECT 1").execute(&self.pool);
        match tokio::time::timeout(PING_TIMEOUT, query).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(err)) => Err(RepositoryError::Unavailable(err.to_string())),
            Err(_) => Err(RepositoryError::Unavailable(format!(
                "no answer within {}s",
                PING_TIMEOUT.as_secs()
            ))),
        }
    }
}

pub struct PostgresStatusHistory {
    pool: PgPool,
}

impl PostgresStatusHistory {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[async_trait]
impl StatusHistory for PostgresStatusHistory {
    async fn record(&self, component: &Component) -> Result<(), HistoryError> {
        let nodes = serde_json::to_value(&component.nodes)
            .map_err(|err| HistoryError::Other(err.to_string()))?;
        sqlx::query(
            "INSERT INTO core.status_history (at, kind, name, state, detail, latency_ms, nodes) \
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(component.checked_at)
        .bind(&component.kind)
        .bind(&component.name)
        .bind(component.state.as_str())
        .bind(component.detail.as_deref())
        .bind(component.latency_ms.map(|ms| ms as i32))
        .bind(nodes)
        .execute(&self.pool)
        .await
        .map_err(|err| HistoryError::Other(err.to_string()))?;
        Ok(())
    }

    async fn prune(&self, before: DateTime<Utc>) -> Result<u64, HistoryError> {
        let result = sqlx::query("DELETE FROM core.status_history WHERE at < $1")
            .bind(before)
            .execute(&self.pool)
            .await
            .map_err(|err| HistoryError::Other(err.to_string()))?;
        Ok(result.rows_affected())
    }

    async fn since(&self, since: DateTime<Utc>) -> Result<Vec<Component>, HistoryError> {
        let rows: Vec<Check> = sqlx::query_as(
            "SELECT at, kind, name, state, detail, latency_ms, nodes \
             FROM core.status_history WHERE at >= $1 ORDER BY at",
        )
        .bind(since)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| HistoryError::Other(err.to_string()))?;
        Ok(rows.into_iter().map(checked).collect())
    }

    async fn between(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<Component>, HistoryError> {
        let rows: Vec<Check> = sqlx::query_as(
            "SELECT at, kind, name, state, detail, latency_ms, nodes \
             FROM core.status_history WHERE at >= $1 AND at < $2 ORDER BY at, id",
        )
        .bind(from)
        .bind(to)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| HistoryError::Other(err.to_string()))?;
        Ok(rows.into_iter().map(checked).collect())
    }
}

type Check = (DateTime<Utc>, String, String, String, Option<String>, Option<i32>, Value);

fn checked((at, kind, name, state, detail, latency_ms, nodes): Check) -> Component {
    Component {
        kind,
        name,
        state: word(state).unwrap_or(Health::Unknown),
        detail,
        nodes: serde_json::from_value(nodes).unwrap_or_default(),
        checked_at: at,
        latency_ms: latency_ms.map(|ms| ms.max(0) as u64),
    }
}

pub struct PostgresPluginStatus {
    pool: PgPool,
}

impl PostgresPluginStatus {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(sqlx::FromRow)]
struct PluginStatusRow {
    plugin: String,
    version: String,
    classification: String,
    instance: Uuid,
    state: Option<String>,
    error: Option<String>,
    since: DateTime<Utc>,
    at: DateTime<Utc>,
    registered_at: DateTime<Utc>,
    last_error: Option<String>,
    last_error_at: Option<DateTime<Utc>>,
    checked_at: DateTime<Utc>,
}

/// The kebab-case word an enum is stored as, read back through its serde name.
fn word<T: serde::de::DeserializeOwned>(text: String) -> Result<T, RepositoryError> {
    serde_json::from_value(Value::String(text))
        .map_err(|err| RepositoryError::Other(err.to_string()))
}

impl TryFrom<PluginStatusRow> for PluginStatus {
    type Error = RepositoryError;

    fn try_from(row: PluginStatusRow) -> Result<Self, RepositoryError> {
        Ok(Self {
            plugin: row.plugin,
            version: row.version,
            classification: word(row.classification)?,
            instance: row.instance,
            state: row.state.map(word).transpose()?,
            error: row.error,
            since: row.since,
            at: row.at,
            registered_at: row.registered_at,
            last_error: row.last_error,
            last_error_at: row.last_error_at,
            checked_at: row.checked_at,
        })
    }
}

#[async_trait]
impl PluginStatuses for PostgresPluginStatus {
    async fn current(&self) -> Result<Vec<PluginStatus>, RepositoryError> {
        let rows: Vec<PluginStatusRow> = sqlx::query_as(
            "SELECT plugin, version, classification, instance, state, error, since, at, \
                    registered_at, last_error, last_error_at, checked_at \
             FROM core.plugin_status ORDER BY plugin",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        rows.into_iter().map(PluginStatus::try_from).collect()
    }

    async fn record(&self, change: &PluginChange, source: Source) -> Result<(), RepositoryError> {
        let state = change.state.map(PluginState::as_str);
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        sqlx::query(
            "INSERT INTO core.plugin_status_history \
               (plugin, version, instance, state, error, at, source) \
             VALUES ($1, $2, $3, COALESCE($4, 'removed'), $5, $6, $7) \
             ON CONFLICT ON CONSTRAINT plugin_status_history_once DO NOTHING",
        )
        .bind(&change.plugin)
        .bind(&change.version)
        .bind(change.instance)
        .bind(state)
        .bind(change.error.as_deref())
        .bind(change.at)
        .bind(source.as_str())
        .execute(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        sqlx::query(
            "INSERT INTO core.plugin_status AS current \
               (plugin, version, classification, instance, state, error, since, at, \
                registered_at, last_error, last_error_at, checked_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $6, \
                     CASE WHEN $6::text IS NULL THEN NULL ELSE $8 END, now()) \
             ON CONFLICT (plugin) DO UPDATE SET \
               version = EXCLUDED.version, classification = EXCLUDED.classification, \
               instance = EXCLUDED.instance, state = EXCLUDED.state, error = EXCLUDED.error, \
               since = EXCLUDED.since, at = EXCLUDED.at, registered_at = EXCLUDED.registered_at, \
               last_error = COALESCE(EXCLUDED.error, current.last_error), \
               last_error_at = CASE WHEN EXCLUDED.error IS NULL THEN current.last_error_at \
                                    ELSE EXCLUDED.at END, \
               checked_at = now() \
             WHERE current.at <= EXCLUDED.at",
        )
        .bind(&change.plugin)
        .bind(&change.version)
        .bind(change.classification.as_str())
        .bind(change.instance)
        .bind(state)
        .bind(change.error.as_deref())
        .bind(change.since)
        .bind(change.at)
        .bind(change.registered_at)
        .execute(&mut *tx)
        .await
        .map_err(|err| failed(&err))?;
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn checked(&self, at: DateTime<Utc>) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE core.plugin_status SET checked_at = $1")
            .bind(at)
            .execute(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn history(
        &self,
        plugin: &str,
        limit: u32,
    ) -> Result<Vec<StatusChange>, RepositoryError> {
        let rows: Vec<(String, Uuid, String, Option<String>, DateTime<Utc>, String)> =
            sqlx::query_as(
                "SELECT version, instance, state, error, at, source \
                 FROM core.plugin_status_history WHERE plugin = $1 \
                 ORDER BY at DESC, id DESC LIMIT $2",
            )
            .bind(plugin)
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(rows
            .into_iter()
            .map(|(version, instance, state, error, at, source)| StatusChange {
                version,
                instance,
                state,
                error,
                at,
                source,
            })
            .collect())
    }

    async fn changes(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, StatusChange)>, RepositoryError> {
        let rows: Vec<(String, String, Uuid, String, Option<String>, DateTime<Utc>, String)> =
            sqlx::query_as(
                "SELECT plugin, version, instance, state, error, at, source \
                 FROM core.plugin_status_history WHERE at >= $1 AND at < $2 ORDER BY at, id",
            )
            .bind(from)
            .bind(to)
            .fetch_all(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(rows
            .into_iter()
            .map(|(plugin, version, instance, state, error, at, source)| {
                (plugin, StatusChange { version, instance, state, error, at, source })
            })
            .collect())
    }

    async fn prune(&self, before: DateTime<Utc>) -> Result<u64, RepositoryError> {
        let result = sqlx::query("DELETE FROM core.plugin_status_history WHERE at < $1")
            .bind(before)
            .execute(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(result.rows_affected())
    }
}

/// The registry of plugins, their versions, delegations and state.
pub struct PostgresPlugins {
    pool: PgPool,
}

impl PostgresPlugins {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }
}

#[derive(sqlx::FromRow)]
struct PluginRow {
    id: String,
    version: Option<String>,
    classification: Option<String>,
    state: Option<String>,
    address: Option<String>,
    binary_sha256: Option<String>,
    manifest: Value,
    error: Option<String>,
    registered_at: Option<DateTime<Utc>>,
    last_seen_at: Option<DateTime<Utc>>,
}

impl From<PluginRow> for PluginRecord {
    fn from(row: PluginRow) -> Self {
        Self {
            id: row.id,
            version: row.version.unwrap_or_default(),
            classification: row.classification.unwrap_or_default(),
            state: row.state.unwrap_or_default(),
            address: row.address.unwrap_or_default(),
            binary_sha256: row.binary_sha256.unwrap_or_default(),
            manifest: row.manifest,
            error: row.error,
            registered_at: row.registered_at,
            last_seen_at: row.last_seen_at,
        }
    }
}

#[async_trait]
impl PluginRepository for PostgresPlugins {
    async fn record_registration(&self, record: &PluginRecord) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.plugins \
               (id, version, classification, state, address, binary_sha256, manifest, error, \
                registered_at, last_seen_at, updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, NULL, now(), now(), now()) \
             ON CONFLICT (id) DO UPDATE SET \
               version = EXCLUDED.version, classification = EXCLUDED.classification, \
               state = EXCLUDED.state, address = EXCLUDED.address, \
               binary_sha256 = EXCLUDED.binary_sha256, manifest = EXCLUDED.manifest, \
               error = NULL, registered_at = now(), last_seen_at = now(), updated_at = now()",
        )
        .bind(&record.id)
        .bind(&record.version)
        .bind(&record.classification)
        .bind(&record.state)
        .bind(&record.address)
        .bind(&record.binary_sha256)
        .bind(&record.manifest)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn version_hash(
        &self,
        plugin: &str,
        version: &str,
    ) -> Result<Option<String>, RepositoryError> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT binary_sha256 FROM core.plugin_versions WHERE plugin = $1 AND version = $2",
        )
        .bind(plugin)
        .bind(version)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(row.map(|(hash,)| hash))
    }

    /// The hash is overwritten on a conflict: registration has already refused a different one
    /// unless `plugins.allow_rebuilds` let it through, and then the rebuild is the version now.
    async fn record_version(
        &self,
        plugin: &str,
        version: &str,
        hash: &str,
        manifest: &Value,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.plugin_versions (plugin, version, binary_sha256, manifest) \
             VALUES ($1, $2, $3, $4) \
             ON CONFLICT (plugin, version) DO UPDATE SET \
               binary_sha256 = EXCLUDED.binary_sha256, manifest = EXCLUDED.manifest, \
               last_seen_at = now()",
        )
        .bind(plugin)
        .bind(version)
        .bind(hash)
        .bind(manifest)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    /// Replaces the set, so a permission dropped from the manifest stops being grantable.
    async fn record_permissions(
        &self,
        plugin: &str,
        permissions: &[DeclaredPermission],
    ) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        sqlx::query("DELETE FROM core.plugin_permissions WHERE plugin = $1")
            .bind(plugin)
            .execute(&mut *tx)
            .await
            .map_err(|err| failed(&err))?;
        for permission in permissions {
            sqlx::query(
                "INSERT INTO core.plugin_permissions (plugin, kind, name) VALUES ($1, $2, $3) \
                 ON CONFLICT DO NOTHING",
            )
            .bind(plugin)
            .bind(&permission.kind)
            .bind(&permission.name)
            .execute(&mut *tx)
            .await
            .map_err(|err| failed(&err))?;
        }
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn permissions(&self, plugin: &str) -> Result<Vec<DeclaredPermission>, RepositoryError> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT kind, name FROM core.plugin_permissions WHERE plugin = $1 ORDER BY kind, name",
        )
        .bind(plugin)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(rows.into_iter().map(|(kind, name)| DeclaredPermission { kind, name }).collect())
    }

    async fn set_state(
        &self,
        plugin: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE core.plugins SET state = $2, error = $3, updated_at = now() WHERE id = $1",
        )
        .bind(plugin)
        .bind(state)
        .bind(error)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn touch(&self, plugin: &str) -> Result<(), RepositoryError> {
        sqlx::query("UPDATE core.plugins SET last_seen_at = now() WHERE id = $1")
            .bind(plugin)
            .execute(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn deregister(&self, plugin: &str) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE core.plugins \
             SET state = NULL, address = NULL, error = NULL, updated_at = now() WHERE id = $1",
        )
        .bind(plugin)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn save_handover(
        &self,
        plugin: &str,
        state: Option<&Value>,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "UPDATE core.plugins SET handover = $2, handover_at = now(), updated_at = now() \
             WHERE id = $1",
        )
        .bind(plugin)
        .bind(state)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn handover(&self, plugin: &str) -> Result<Option<Value>, RepositoryError> {
        let row: Option<(Option<Value>,)> =
            sqlx::query_as("SELECT handover FROM core.plugins WHERE id = $1")
                .bind(plugin)
                .fetch_optional(&self.pool)
                .await
                .map_err(|err| failed(&err))?;
        Ok(row.and_then(|(state,)| state))
    }

    async fn delegate(&self, delegation: &Delegation) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.delegations (id, plugin, principal, purpose, created_at) \
             VALUES ($1, $2, $3, $4, $5)",
        )
        .bind(delegation.id)
        .bind(&delegation.plugin)
        .bind(&delegation.principal)
        .bind(&delegation.purpose)
        .bind(delegation.created_at)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn delegation(&self, id: Uuid) -> Result<Option<Delegation>, RepositoryError> {
        type Row = (Uuid, String, String, String, DateTime<Utc>, Option<DateTime<Utc>>);
        let row: Option<Row> = sqlx::query_as(
            "SELECT id, plugin, principal, purpose, created_at, revoked_at \
             FROM core.delegations WHERE id = $1",
        )
        .bind(id)
        .fetch_optional(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(row.map(|(id, plugin, principal, purpose, created_at, revoked_at)| Delegation {
            id,
            plugin,
            principal,
            purpose,
            created_at,
            revoked_at,
        }))
    }

    async fn revoke_delegation(&self, plugin: &str, id: Uuid) -> Result<bool, RepositoryError> {
        let result = sqlx::query(
            "UPDATE core.delegations SET revoked_at = now() \
             WHERE id = $1 AND plugin = $2 AND revoked_at IS NULL",
        )
        .bind(id)
        .bind(plugin)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(result.rows_affected() > 0)
    }

    async fn records(&self) -> Result<Vec<PluginRecord>, RepositoryError> {
        let rows: Vec<PluginRow> = sqlx::query_as(
            "SELECT id, version, classification, state, address, binary_sha256, manifest, error, \
                    registered_at, last_seen_at \
             FROM core.plugins ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(rows.into_iter().map(PluginRecord::from).collect())
    }

    async fn state_get(&self, plugin: &str, key: &str) -> Result<Option<Value>, RepositoryError> {
        let row: Option<(Value,)> =
            sqlx::query_as("SELECT value FROM core.plugin_state WHERE plugin = $1 AND key = $2")
                .bind(plugin)
                .bind(key)
                .fetch_optional(&self.pool)
                .await
                .map_err(|err| failed(&err))?;
        Ok(row.map(|(value,)| value))
    }

    async fn state_set(
        &self,
        plugin: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.plugin_state (plugin, key, value) VALUES ($1, $2, $3) \
             ON CONFLICT (plugin, key) DO UPDATE SET value = EXCLUDED.value, updated_at = now()",
        )
        .bind(plugin)
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn state_delete(&self, plugin: &str, key: &str) -> Result<bool, RepositoryError> {
        let result = sqlx::query("DELETE FROM core.plugin_state WHERE plugin = $1 AND key = $2")
            .bind(plugin)
            .bind(key)
            .execute(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(result.rows_affected() > 0)
    }

    async fn navigation(&self) -> Result<Option<Value>, RepositoryError> {
        let row: Option<(Value,)> = sqlx::query_as("SELECT layout FROM core.navigation")
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(row.map(|(layout,)| layout))
    }

    async fn set_navigation(&self, layout: &Value) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.navigation (id, layout, updated_at) VALUES (true, $1, now()) \
             ON CONFLICT (id) DO UPDATE SET layout = EXCLUDED.layout, updated_at = now()",
        )
        .bind(layout)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn settings(&self) -> Result<Option<Value>, RepositoryError> {
        let row: Option<(Value,)> = sqlx::query_as("SELECT settings FROM core.settings")
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(row.map(|(settings,)| settings))
    }

    async fn set_settings(&self, settings: &Value) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.settings (id, settings, updated_at) VALUES (true, $1, now()) \
             ON CONFLICT (id) DO UPDATE SET settings = EXCLUDED.settings, updated_at = now()",
        )
        .bind(settings)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn setup(&self) -> Result<Option<Value>, RepositoryError> {
        let row: Option<(Value,)> = sqlx::query_as("SELECT setup FROM core.setup")
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(row.map(|(setup,)| setup))
    }

    async fn set_setup(&self, setup: &Value) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.setup (id, setup, updated_at) VALUES (true, $1, now()) \
             ON CONFLICT (id) DO UPDATE SET setup = EXCLUDED.setup, updated_at = now()",
        )
        .bind(setup)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn plugin_settings(&self, plugin: &str) -> Result<Vec<StoredSetting>, RepositoryError> {
        let rows: Vec<SettingRow> = sqlx::query_as(SETTING_COLUMNS)
            .bind(plugin)
            .fetch_all(&self.pool)
            .await
            .map_err(|err| failed(&err))?;
        Ok(rows.into_iter().map(SettingRow::into_stored).collect())
    }

    /// One transaction, so a save that fails part way through changes nothing.
    async fn set_plugin_settings(
        &self,
        plugin: &str,
        changes: &[SettingChange],
        by: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        for change in changes {
            match change {
                SettingChange::Clear { key } => {
                    sqlx::query("DELETE FROM core.plugin_settings WHERE plugin = $1 AND key = $2")
                        .bind(plugin)
                        .bind(key)
                        .execute(&mut *tx)
                        .await
                        .map_err(|err| failed(&err))?;
                }
                SettingChange::Set { key, value } => {
                    sqlx::query(
                        "INSERT INTO core.plugin_settings \
                           (plugin, key, value, secret, nonce, key_id, updated_at, updated_by) \
                         VALUES ($1, $2, $3, NULL, NULL, NULL, now(), $4) \
                         ON CONFLICT (plugin, key) DO UPDATE SET \
                           value = EXCLUDED.value, secret = NULL, nonce = NULL, key_id = NULL, \
                           updated_at = now(), updated_by = EXCLUDED.updated_by",
                    )
                    .bind(plugin)
                    .bind(key)
                    .bind(value)
                    .bind(by)
                    .execute(&mut *tx)
                    .await
                    .map_err(|err| failed(&err))?;
                }
                SettingChange::Seal { key, sealed } => {
                    sqlx::query(
                        "INSERT INTO core.plugin_settings \
                           (plugin, key, value, secret, nonce, key_id, updated_at, updated_by) \
                         VALUES ($1, $2, NULL, $3, $4, $5, now(), $6) \
                         ON CONFLICT (plugin, key) DO UPDATE SET \
                           value = NULL, secret = EXCLUDED.secret, nonce = EXCLUDED.nonce, \
                           key_id = EXCLUDED.key_id, updated_at = now(), \
                           updated_by = EXCLUDED.updated_by",
                    )
                    .bind(plugin)
                    .bind(key)
                    .bind(&sealed.ciphertext)
                    .bind(&sealed.nonce)
                    .bind(&sealed.key_id)
                    .bind(by)
                    .execute(&mut *tx)
                    .await
                    .map_err(|err| failed(&err))?;
                }
            }
        }
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn plugin_secrets(&self) -> Result<Vec<(String, StoredSetting)>, RepositoryError> {
        let rows: Vec<SettingRow> = sqlx::query_as(
            "SELECT plugin, key, value, secret, nonce, key_id, updated_at, updated_by \
             FROM core.plugin_settings WHERE secret IS NOT NULL ORDER BY plugin, key",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(rows.into_iter().map(|row| (row.plugin.clone(), row.into_stored())).collect())
    }

    async fn plugin_features(&self, plugin: &str) -> Result<Vec<StoredFeature>, RepositoryError> {
        let rows: Vec<(String, bool, DateTime<Utc>, Option<String>)> = sqlx::query_as(
            "SELECT name, enabled, updated_at, updated_by FROM core.plugin_features \
             WHERE plugin = $1 ORDER BY name",
        )
        .bind(plugin)
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(rows
            .into_iter()
            .map(|(name, enabled, updated_at, updated_by)| StoredFeature {
                name,
                enabled,
                updated_at,
                updated_by,
            })
            .collect())
    }

    async fn set_plugin_features(
        &self,
        plugin: &str,
        changes: &[(String, bool)],
        by: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let mut tx = self.pool.begin().await.map_err(|err| failed(&err))?;
        for (name, enabled) in changes {
            sqlx::query(
                "INSERT INTO core.plugin_features (plugin, name, enabled, updated_at, updated_by) \
                 VALUES ($1, $2, $3, now(), $4) \
                 ON CONFLICT (plugin, name) DO UPDATE SET enabled = EXCLUDED.enabled, \
                   updated_at = now(), updated_by = EXCLUDED.updated_by",
            )
            .bind(plugin)
            .bind(name)
            .bind(enabled)
            .bind(by)
            .execute(&mut *tx)
            .await
            .map_err(|err| failed(&err))?;
        }
        tx.commit().await.map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn plugin_switches(&self) -> Result<Vec<StoredSwitch>, RepositoryError> {
        let rows: Vec<(String, DateTime<Utc>, Option<String>)> = sqlx::query_as(
            "SELECT plugin, updated_at, updated_by FROM core.plugin_switches ORDER BY plugin",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(rows
            .into_iter()
            .map(|(plugin, updated_at, updated_by)| StoredSwitch { plugin, updated_at, updated_by })
            .collect())
    }

    async fn set_plugin_switch(
        &self,
        plugin: &str,
        on: bool,
        by: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let query = match on {
            true => sqlx::query("DELETE FROM core.plugin_switches WHERE plugin = $1").bind(plugin),
            false => sqlx::query(
                "INSERT INTO core.plugin_switches (plugin, updated_at, updated_by) \
                 VALUES ($1, now(), $2) \
                 ON CONFLICT (plugin) DO UPDATE SET updated_at = now(), \
                   updated_by = EXCLUDED.updated_by",
            )
            .bind(plugin)
            .bind(by),
        };
        query.execute(&self.pool).await.map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn plugin_flags(&self) -> Result<Vec<StoredPluginFlag>, RepositoryError> {
        let rows: Vec<(String, String, DateTime<Utc>, Option<String>)> = sqlx::query_as(
            "SELECT plugin, flag, updated_at, updated_by FROM core.plugin_flags ORDER BY plugin",
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(rows
            .into_iter()
            .map(|(plugin, flag, updated_at, updated_by)| StoredPluginFlag {
                plugin,
                flag,
                updated_at,
                updated_by,
            })
            .collect())
    }

    async fn set_plugin_flag(
        &self,
        plugin: &str,
        flag: Option<&str>,
        by: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let query = match flag {
            None => sqlx::query("DELETE FROM core.plugin_flags WHERE plugin = $1").bind(plugin),
            Some(flag) => sqlx::query(
                "INSERT INTO core.plugin_flags (plugin, flag, updated_at, updated_by) \
                 VALUES ($1, $2, now(), $3) \
                 ON CONFLICT (plugin) DO UPDATE SET flag = EXCLUDED.flag, updated_at = now(), \
                   updated_by = EXCLUDED.updated_by",
            )
            .bind(plugin)
            .bind(flag)
            .bind(by),
        };
        query.execute(&self.pool).await.map_err(|err| failed(&err))?;
        Ok(())
    }

    async fn access_requests(
        &self,
        target: &str,
    ) -> Result<Vec<AccessRequestRecord>, RepositoryError> {
        sqlx::query_as(ACCESS_BY_TARGET)
            .bind(target)
            .fetch_all(&self.pool)
            .await
            .map_err(|err| failed(&err))
    }

    async fn access_request_for(
        &self,
        requester: &str,
        target: &str,
        setting: &str,
    ) -> Result<Option<AccessRequestRecord>, RepositoryError> {
        sqlx::query_as(ACCESS_BY_REQUESTER)
            .bind(requester)
            .bind(target)
            .bind(setting)
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| failed(&err))
    }

    async fn access_request(
        &self,
        id: Uuid,
    ) -> Result<Option<AccessRequestRecord>, RepositoryError> {
        sqlx::query_as(ACCESS_BY_ID)
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .map_err(|err| failed(&err))
    }

    async fn put_access_request(
        &self,
        record: &AccessRequestRecord,
    ) -> Result<(), RepositoryError> {
        sqlx::query(
            "INSERT INTO core.access_requests \
               (id, requester, target, setting, reason, state, created_at, decided_at, decided_by) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (id) DO UPDATE SET reason = EXCLUDED.reason, state = EXCLUDED.state, \
               created_at = EXCLUDED.created_at, decided_at = EXCLUDED.decided_at, \
               decided_by = EXCLUDED.decided_by",
        )
        .bind(record.id)
        .bind(&record.requester)
        .bind(&record.target)
        .bind(&record.setting)
        .bind(&record.reason)
        .bind(&record.state)
        .bind(record.created_at)
        .bind(record.decided_at)
        .bind(&record.decided_by)
        .execute(&self.pool)
        .await
        .map_err(|err| failed(&err))?;
        Ok(())
    }
}

const ACCESS_BY_TARGET: &str = "SELECT id, requester, target, setting, reason, state, created_at, decided_at, decided_by FROM core.access_requests \
                                WHERE target = $1 ORDER BY created_at DESC";
const ACCESS_BY_REQUESTER: &str = "SELECT id, requester, target, setting, reason, state, created_at, decided_at, decided_by FROM core.access_requests \
                                   WHERE requester = $1 AND target = $2 AND setting = $3";
const ACCESS_BY_ID: &str = "SELECT id, requester, target, setting, reason, state, created_at, decided_at, decided_by FROM core.access_requests WHERE id = $1";

const SETTING_COLUMNS: &str = "SELECT plugin, key, value, secret, nonce, key_id, updated_at, \
                               updated_by FROM core.plugin_settings WHERE plugin = $1 ORDER BY key";

#[derive(sqlx::FromRow)]
struct SettingRow {
    plugin: String,
    key: String,
    value: Option<Value>,
    secret: Option<Vec<u8>>,
    nonce: Option<Vec<u8>>,
    key_id: Option<String>,
    updated_at: DateTime<Utc>,
    updated_by: Option<String>,
}

impl SettingRow {
    fn into_stored(self) -> StoredSetting {
        let sealed = match (self.secret, self.nonce, self.key_id) {
            (Some(ciphertext), Some(nonce), Some(key_id)) => {
                Some(crate::secrets::Sealed { key_id, nonce, ciphertext })
            }
            _ => None,
        };
        StoredSetting {
            key: self.key,
            value: self.value,
            sealed,
            updated_at: self.updated_at,
            updated_by: self.updated_by,
        }
    }
}
