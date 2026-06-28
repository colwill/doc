//! Repository traits the handlers depend on, so endpoints can be tested with in-memory fakes.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use uuid::Uuid;

use doc_background_tasks::TaskStore;
use doc_cron_tasks::CronStore;

use serde_json::Value;

use crate::identity::{
    Account, ApiToken, Arrival, AuditEntry, Identity, NewUser, Principal, Profile, ServiceAccount,
    TokenOwner, User,
};
use crate::secrets::TokenKind;
use crate::status::plugins::PluginStatuses;
use crate::teams::{
    AccountOwner, NewTeam, Organisation, Owners, Position, Team, TeamChanges, TeamMember,
    TeamPosition,
};

#[derive(Debug, thiserror::Error)]
pub enum RepositoryError {
    #[error("storage unavailable: {0}")]
    Unavailable(String),
    /// A change refused because of what is already stored, said so that it can be shown.
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Other(String),
}

#[async_trait]
pub trait HealthRepository: Send + Sync {
    /// Round-trips a trivial query, so readiness reflects the database rather than the pool.
    async fn ping(&self) -> Result<(), RepositoryError>;
}

pub struct NewToken {
    pub kind: TokenKind,
    pub token_hash: Vec<u8>,
    pub name: Option<String>,
    pub owner: TokenOwner,
    pub expires_at: Option<DateTime<Utc>>,
    /// For a scoped token only: the permissions it is limited to.
    pub scopes: Option<Vec<String>>,
    /// The plugin that minted it for its holder, if one did.
    pub issued_by: Option<String>,
}

/// A token and whoever holds it, read together so authentication is one query.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TokenRecord {
    pub token: ApiToken,
    pub principal: Principal,
}

#[async_trait]
pub trait IdentityRepository: Send + Sync {
    async fn service_account_by_name(
        &self,
        name: &str,
    ) -> Result<Option<ServiceAccount>, RepositoryError>;

    async fn create_service_account(
        &self,
        name: &str,
        description: Option<&str>,
        owner: AccountOwner,
    ) -> Result<ServiceAccount, RepositoryError>;

    async fn service_account_by_id(
        &self,
        id: Uuid,
    ) -> Result<Option<ServiceAccount>, RepositoryError>;

    /// The service account `plugin` acts as when it asks as itself, once core has made it one.
    async fn plugin_service_account(
        &self,
        plugin: &str,
    ) -> Result<Option<ServiceAccount>, RepositoryError>;

    /// Records that core made `account` for `plugin` to act as.
    async fn link_plugin_service_account(
        &self,
        plugin: &str,
        account: Uuid,
    ) -> Result<(), RepositoryError>;

    /// The items a person chose for their dashboard, as `plugin/id` in order; `None` until they
    /// have chosen.
    async fn dashboard(&self, user: Uuid) -> Result<Option<Vec<String>>, RepositoryError>;

    async fn set_dashboard(&self, user: Uuid, items: &[String]) -> Result<(), RepositoryError>;

    /// The plugin a service account was made for, if it was made for one.
    async fn service_account_plugin(
        &self,
        account: Uuid,
    ) -> Result<Option<String>, RepositoryError>;

    /// Every account when `owners` is `None`, which is what an identity administrator sees;
    /// otherwise those the user owns and those any of the teams own.
    async fn list_service_accounts(
        &self,
        owners: Option<&Owners>,
    ) -> Result<Vec<ServiceAccount>, RepositoryError>;

    async fn set_service_account_owner(
        &self,
        id: Uuid,
        owner: AccountOwner,
    ) -> Result<Option<ServiceAccount>, RepositoryError>;

    async fn set_service_account_disabled(
        &self,
        id: Uuid,
        disabled: bool,
    ) -> Result<Option<ServiceAccount>, RepositoryError>;

    async fn user_by_id(&self, id: Uuid) -> Result<Option<User>, RepositoryError>;

    async fn list_users(&self) -> Result<Vec<ListedUser>, RepositoryError>;

    async fn set_user_disabled(
        &self,
        id: Uuid,
        disabled: bool,
    ) -> Result<Option<User>, RepositoryError>;

    /// Returns whether the token was new; re-running the bootstrap must not issue a second row.
    async fn record_token(&self, token: NewToken) -> Result<bool, RepositoryError>;

    async fn register_plugin(&self, id: &str) -> Result<bool, RepositoryError>;

    async fn list_plugins(&self) -> Result<Vec<String>, RepositoryError>;

    async fn token_by_hash(&self, hash: &[u8]) -> Result<Option<TokenRecord>, RepositoryError>;

    /// Revokes a scoped token `plugin` minted, answering its hash if it was still in force.
    async fn revoke_issued(
        &self,
        id: Uuid,
        plugin: &str,
    ) -> Result<Option<Vec<u8>>, RepositoryError>;

    async fn touch_token(&self, id: Uuid) -> Result<(), RepositoryError>;

    async fn issue_token(&self, token: NewToken) -> Result<ApiToken, RepositoryError>;

    async fn list_tokens(
        &self,
        owner: &TokenOwner,
        kind: TokenKind,
    ) -> Result<Vec<ApiToken>, RepositoryError>;

    /// Scoped to the owner so one caller cannot revoke another's token by guessing an ID, and
    /// returns the revoked token's hash so the caller can drop it from the cache.
    async fn revoke_token(
        &self,
        id: Uuid,
        owner: &TokenOwner,
    ) -> Result<Option<Vec<u8>>, RepositoryError>;

    /// The user an account belongs to, made with the account in `organisation` when there is none,
    /// and whether it was made. The account's login and what it reported are brought up to date, so
    /// is the user's profile, and the user's own login follows the account's while it matches.
    /// `Arrival::SignIn` records the account's use.
    async fn user_for_account(
        &self,
        account: &Account,
        profile: &Profile,
        arrival: Arrival,
        organisation: Uuid,
    ) -> Result<(User, bool), RepositoryError>;

    /// A user made before the person signs in, with an account an admin attached if one is given.
    /// A `Conflict` if that account belongs to someone already.
    async fn create_user(
        &self,
        user: &NewUser,
        account: Option<&Account>,
    ) -> Result<User, RepositoryError>;

    async fn identity(
        &self,
        provider: &str,
        external_id: &str,
    ) -> Result<Option<Identity>, RepositoryError>;

    /// One user's identities, or everyone's when `user` is `None`, oldest first.
    async fn identities(&self, user: Option<Uuid>) -> Result<Vec<Identity>, RepositoryError>;

    /// A `Conflict` if the account belongs to anyone, or the user has one with its provider.
    async fn attach_identity(
        &self,
        user: Uuid,
        account: &Account,
        source: &str,
    ) -> Result<Identity, RepositoryError>;

    async fn detach_identity(
        &self,
        user: Uuid,
        identity: Uuid,
    ) -> Result<Option<Identity>, RepositoryError>;

    /// Moves what `from` has to `into` and deletes `from`, returning it as it was: its identities,
    /// the service accounts it owns, its tokens, the delegations it gave and the teams it is in, and
    /// its name and email address where `into` has none. A `Conflict` if `from` has ever signed in,
    /// if either is gone, or if both have an account with the same provider.
    async fn merge_users(&self, from: Uuid, into: Uuid) -> Result<User, RepositoryError>;

    /// Moves someone to another organisation: out of their old organisation's teams and into the
    /// new one's default teams. A `Conflict` if the organisation is gone.
    async fn move_user(
        &self,
        id: Uuid,
        organisation: Uuid,
    ) -> Result<Option<User>, RepositoryError>;

    async fn record_sign_in(&self, id: Uuid) -> Result<(), RepositoryError>;

    /// Removes tokens that expired or were revoked long enough ago to be of no further interest.
    async fn purge_expired_tokens(&self, before: DateTime<Utc>) -> Result<u64, RepositoryError>;

    async fn record_audit(&self, entry: AuditEntry) -> Result<(), RepositoryError>;

    /// Newest first; `action` matches as a prefix, and `actor` a label or an ID.
    async fn audit_log(&self, filter: &AuditFilter) -> Result<Vec<AuditRecord>, RepositoryError>;
}

/// Organisations, teams and who is in them (ADR-0004). Users made through `IdentityRepository`
/// are placed in every default team in the same step, so these share storage with it.
#[async_trait]
pub trait TeamRepository: Send + Sync {
    async fn organisations(&self) -> Result<Vec<Organisation>, RepositoryError>;

    async fn organisation(&self, id: Uuid) -> Result<Option<Organisation>, RepositoryError>;

    async fn organisation_named(&self, name: &str)
    -> Result<Option<Organisation>, RepositoryError>;

    /// A `Conflict` if the name is taken.
    async fn create_organisation(
        &self,
        name: &str,
        title: &str,
        description: &str,
    ) -> Result<Organisation, RepositoryError>;

    /// A `Conflict` if the name is taken.
    async fn update_organisation(
        &self,
        id: Uuid,
        name: &str,
        title: &str,
        description: &str,
    ) -> Result<Option<Organisation>, RepositoryError>;

    /// A `Conflict` while it has teams.
    async fn delete_organisation(&self, id: Uuid) -> Result<Option<Organisation>, RepositoryError>;

    /// Every team, in every organisation.
    async fn teams(&self) -> Result<Vec<Team>, RepositoryError>;

    async fn team(&self, id: Uuid) -> Result<Option<Team>, RepositoryError>;

    async fn provided_team(
        &self,
        provider: &str,
        external_id: &str,
    ) -> Result<Option<Team>, RepositoryError>;

    /// A `Conflict` if the name is taken in the organisation, the parent is in another one, or the
    /// provider's key is taken.
    async fn create_team(&self, team: &NewTeam) -> Result<Team, RepositoryError>;

    /// A `Conflict` if the name is taken, the new parent is in another organisation or below the
    /// team itself, or it would take the default mark off the last default team.
    async fn update_team(
        &self,
        id: Uuid,
        changes: &TeamChanges,
    ) -> Result<Option<Team>, RepositoryError>;

    /// A `Conflict` while it has sub-teams or owns service accounts, or if it is the last default
    /// team. Its memberships go with it.
    async fn delete_team(&self, id: Uuid) -> Result<Option<Team>, RepositoryError>;

    /// Makes a provider's team one made in DOC, when the provider stops providing it but something
    /// made in DOC depends on it.
    async fn release_team(&self, id: Uuid) -> Result<Option<Team>, RepositoryError>;

    /// Everyone in one team, or in every team when `team` is `None`.
    async fn members(&self, team: Option<Uuid>) -> Result<Vec<TeamMember>, RepositoryError>;

    /// The teams someone is in, and how they came to be.
    async fn memberships(&self, user: Uuid) -> Result<Vec<TeamMember>, RepositoryError>;

    /// Whether they were added; someone in the team already stays as they came to be.
    async fn add_member(
        &self,
        team: Uuid,
        user: Uuid,
        source: &str,
        provider: Option<&str>,
    ) -> Result<bool, RepositoryError>;

    async fn remove_member(
        &self,
        team: Uuid,
        user: Uuid,
    ) -> Result<Option<TeamMember>, RepositoryError>;

    /// Makes the memberships `provider` made in `team` exactly `users`, leaving everyone else's, and
    /// returns who it added and who it removed. Someone of another organisation is left out.
    async fn set_provided_members(
        &self,
        team: Uuid,
        provider: &str,
        users: &[Uuid],
    ) -> Result<(Vec<Uuid>, Vec<Uuid>), RepositoryError>;

    /// Makes one of a team's members its lead, or leaves it without one. A `Conflict` if they are
    /// not in the team.
    async fn set_lead(
        &self,
        team: Uuid,
        lead: Option<Uuid>,
    ) -> Result<Option<Team>, RepositoryError>;

    /// The position someone holds in a team, or none. `None` if they are not in it.
    async fn set_member_position(
        &self,
        team: Uuid,
        user: Uuid,
        position: Option<&str>,
    ) -> Result<Option<TeamMember>, RepositoryError>;

    /// The positions an organisation defines, or every organisation's when `None`.
    async fn positions(&self, organisation: Option<Uuid>)
    -> Result<Vec<Position>, RepositoryError>;

    /// Makes or changes the organisation's position of that name.
    async fn put_position(
        &self,
        organisation: Uuid,
        name: &str,
        title: &str,
        description: &str,
        responsibilities: &[String],
    ) -> Result<Position, RepositoryError>;

    async fn delete_position(
        &self,
        organisation: Uuid,
        name: &str,
    ) -> Result<Option<Position>, RepositoryError>;

    /// What one team, or every team when `None`, changes about the positions it inherits.
    async fn team_positions(
        &self,
        team: Option<Uuid>,
    ) -> Result<Vec<TeamPosition>, RepositoryError>;

    /// Replaces what a team changes about one position.
    async fn put_team_position(&self, change: &TeamPosition) -> Result<(), RepositoryError>;

    /// Whether the team changed that position.
    async fn delete_team_position(&self, team: Uuid, name: &str) -> Result<bool, RepositoryError>;

    /// Takes a position away from everyone who holds it in these teams, returning who they were.
    async fn vacate_position(
        &self,
        teams: &[Uuid],
        name: &str,
    ) -> Result<Vec<TeamMember>, RepositoryError>;

    /// Which organisation each chosen identity provider signs people in for.
    async fn providers(&self) -> Result<Vec<(String, Uuid)>, RepositoryError>;

    /// The identity providers an organisation's people sign in with, replacing what it chose. A
    /// `Conflict` if another organisation has one of them, since a provider serves one.
    async fn set_providers(
        &self,
        organisation: Uuid,
        providers: &[String],
    ) -> Result<(), RepositoryError>;

    /// Which organisation approved each email domain (FEAT-PEOPLE).
    async fn domains(&self) -> Result<Vec<(String, Uuid)>, RepositoryError>;

    /// The email domains an organisation approves, replacing what it approved. A `Conflict` if
    /// another organisation approved one of them, since a domain belongs to one.
    async fn set_domains(
        &self,
        organisation: Uuid,
        domains: &[String],
    ) -> Result<(), RepositoryError>;
}

/// A user with when core first recorded them, as `core.users` shows it.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ListedUser {
    #[sqlx(flatten)]
    pub user: User,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default)]
pub struct AuditFilter {
    pub action: Option<String>,
    pub actor: Option<String>,
    pub before: Option<DateTime<Utc>>,
    pub limit: u32,
}

#[derive(Debug, Clone, serde::Serialize, sqlx::FromRow)]
pub struct AuditRecord {
    pub at: DateTime<Utc>,
    pub actor_kind: String,
    pub actor_id: Option<String>,
    pub actor_label: Option<String>,
    pub action: String,
    pub subject: Option<String>,
    pub detail: Value,
}

/// A plugin's leave to start tasks as `principal` (`user:<id>` or `service:<id>`) until revoked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegation {
    pub id: Uuid,
    pub plugin: String,
    pub principal: String,
    pub purpose: String,
    pub created_at: DateTime<Utc>,
    pub revoked_at: Option<DateTime<Utc>>,
}

/// One row of `core.plugins`, as registration leaves it.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PluginRecord {
    pub id: String,
    pub version: String,
    pub classification: String,
    pub state: String,
    pub address: String,
    pub binary_sha256: String,
    pub manifest: Value,
    pub error: Option<String>,
    pub registered_at: Option<DateTime<Utc>>,
    pub last_seen_at: Option<DateTime<Utc>>,
}

/// A permission a plugin has by existing (`user`, `service`) or declares (`pluginuser`,
/// `pluginservice`), which is what the RBAC plugin offers as grantable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclaredPermission {
    pub kind: String,
    pub name: String,
}

/// A plugin's request to join another plugin's requestable list setting (DOC-SPEC §9.15).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AccessRequestRecord {
    pub id: Uuid,
    pub requester: String,
    pub target: String,
    pub setting: String,
    pub reason: String,
    /// `pending`, `approved` or `denied`.
    pub state: String,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
    pub decided_by: Option<String>,
}

/// One setting as it is stored (ADR-0007): a plain value, or a secret nothing but the settings key
/// can turn back into one. Which of the two it is follows the plugin's declaration, not this row.
#[derive(Debug, Clone)]
pub struct StoredSetting {
    pub key: String,
    pub value: Option<Value>,
    pub sealed: Option<crate::secrets::Sealed>,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<String>,
}

impl StoredSetting {
    pub fn is_secret(&self) -> bool {
        self.sealed.is_some()
    }
}

/// What one save does to one setting. A save applies every change or none of them.
#[derive(Debug, Clone)]
pub enum SettingChange {
    Set {
        key: String,
        value: Value,
    },
    Seal {
        key: String,
        sealed: crate::secrets::Sealed,
    },
    /// Back to the plugin's default, and for a secret, gone.
    Clear {
        key: String,
    },
}

impl SettingChange {
    pub fn key(&self) -> &str {
        match self {
            Self::Set { key, .. } | Self::Seal { key, .. } | Self::Clear { key } => key,
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoredFeature {
    pub name: String,
    pub enabled: bool,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<String>,
}

/// A plugin somebody turned off, and who and when.
#[derive(Debug, Clone)]
pub struct StoredSwitch {
    pub plugin: String,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<String>,
}

/// The flag a plugin is turned on and off by, and who chose it and when.
#[derive(Debug, Clone)]
pub struct StoredPluginFlag {
    pub plugin: String,
    pub flag: String,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<String>,
}

#[async_trait]
pub trait PluginRepository: Send + Sync {
    async fn record_registration(&self, record: &PluginRecord) -> Result<(), RepositoryError>;

    /// The binary hash this version was first seen with, if it has been seen at all.
    async fn version_hash(
        &self,
        plugin: &str,
        version: &str,
    ) -> Result<Option<String>, RepositoryError>;

    async fn record_version(
        &self,
        plugin: &str,
        version: &str,
        hash: &str,
        manifest: &Value,
    ) -> Result<(), RepositoryError>;

    async fn record_permissions(
        &self,
        plugin: &str,
        permissions: &[DeclaredPermission],
    ) -> Result<(), RepositoryError>;

    async fn permissions(&self, plugin: &str) -> Result<Vec<DeclaredPermission>, RepositoryError>;

    async fn set_state(
        &self,
        plugin: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), RepositoryError>;

    async fn touch(&self, plugin: &str) -> Result<(), RepositoryError>;

    /// Clears what registration filled in. There is no `removed` state, so a row with no state is
    /// how `core.plugins` says a known plugin is not currently registered.
    async fn deregister(&self, plugin: &str) -> Result<(), RepositoryError>;

    async fn records(&self) -> Result<Vec<PluginRecord>, RepositoryError>;

    /// What the last `unload` returned; `None` clears it, because the next `load` gets exactly that.
    async fn save_handover(
        &self,
        plugin: &str,
        state: Option<&Value>,
    ) -> Result<(), RepositoryError>;

    async fn handover(&self, plugin: &str) -> Result<Option<Value>, RepositoryError>;

    async fn delegate(&self, delegation: &Delegation) -> Result<(), RepositoryError>;

    async fn delegation(&self, id: Uuid) -> Result<Option<Delegation>, RepositoryError>;

    /// Whether it was in force; only the plugin it was given to can revoke it.
    async fn revoke_delegation(&self, plugin: &str, id: Uuid) -> Result<bool, RepositoryError>;

    async fn state_get(&self, plugin: &str, key: &str) -> Result<Option<Value>, RepositoryError>;

    async fn state_set(
        &self,
        plugin: &str,
        key: &str,
        value: &Value,
    ) -> Result<(), RepositoryError>;

    async fn state_delete(&self, plugin: &str, key: &str) -> Result<bool, RepositoryError>;

    /// How an administrator arranged the navigation, or `None` for the order pages are offered in.
    async fn navigation(&self) -> Result<Option<Value>, RepositoryError>;

    async fn set_navigation(&self, layout: &Value) -> Result<(), RepositoryError>;

    /// What an administrator set for the whole platform, or `None` for the configured defaults.
    async fn settings(&self) -> Result<Option<Value>, RepositoryError>;

    async fn set_settings(&self, settings: &Value) -> Result<(), RepositoryError>;

    /// How far the first setup has got, or `None` while nobody has opened it.
    async fn setup(&self) -> Result<Option<Value>, RepositoryError>;

    async fn set_setup(&self, setup: &Value) -> Result<(), RepositoryError>;

    /// One plugin's stored settings (ADR-0007). A setting nobody has set has no row, so what comes
    /// back is what was set, not everything the plugin declares.
    async fn plugin_settings(&self, plugin: &str) -> Result<Vec<StoredSetting>, RepositoryError>;

    /// Applies every change or none of them, so a save that is refused part way through leaves the
    /// plugin configured as it was.
    async fn set_plugin_settings(
        &self,
        plugin: &str,
        changes: &[SettingChange],
        by: Option<&str>,
    ) -> Result<(), RepositoryError>;

    /// Every stored secret, plugin by plugin, for re-encrypting them under a new key.
    async fn plugin_secrets(&self) -> Result<Vec<(String, StoredSetting)>, RepositoryError>;

    /// The features somebody has turned on or off; the rest are at their declared defaults.
    async fn plugin_features(&self, plugin: &str) -> Result<Vec<StoredFeature>, RepositoryError>;

    async fn set_plugin_features(
        &self,
        plugin: &str,
        changes: &[(String, bool)],
        by: Option<&str>,
    ) -> Result<(), RepositoryError>;

    /// The plugins somebody has turned off; every other plugin is on.
    async fn plugin_switches(&self) -> Result<Vec<StoredSwitch>, RepositoryError>;

    /// Turns a plugin off, or on again, which forgets that it was ever off.
    async fn set_plugin_switch(
        &self,
        plugin: &str,
        on: bool,
        by: Option<&str>,
    ) -> Result<(), RepositoryError>;

    /// The flags plugins are turned on and off by; every other plugin is switched by hand.
    async fn plugin_flags(&self) -> Result<Vec<StoredPluginFlag>, RepositoryError>;

    /// Has a plugin follow a flag, or stop following one.
    async fn set_plugin_flag(
        &self,
        plugin: &str,
        flag: Option<&str>,
        by: Option<&str>,
    ) -> Result<(), RepositoryError>;

    /// Requests to join `target`'s requestable settings, newest first.
    async fn access_requests(
        &self,
        target: &str,
    ) -> Result<Vec<AccessRequestRecord>, RepositoryError>;

    /// The one request `requester` has made for `target`'s `setting`, if any.
    async fn access_request_for(
        &self,
        requester: &str,
        target: &str,
        setting: &str,
    ) -> Result<Option<AccessRequestRecord>, RepositoryError>;

    async fn access_request(
        &self,
        id: Uuid,
    ) -> Result<Option<AccessRequestRecord>, RepositoryError>;

    /// Stores a request as given, replacing the one with its ID.
    async fn put_access_request(&self, record: &AccessRequestRecord)
    -> Result<(), RepositoryError>;
}

#[derive(Clone)]
pub struct Repositories {
    pub health: Arc<dyn HealthRepository>,
    pub identity: Arc<dyn IdentityRepository>,
    pub teams: Arc<dyn TeamRepository>,
    pub plugins: Arc<dyn PluginRepository>,
    pub plugin_status: Arc<dyn PluginStatuses>,
    pub tasks: Arc<dyn TaskStore>,
    pub cron: Arc<dyn CronStore>,
    pub status_history: Arc<dyn crate::status::StatusHistory>,
    pub data: Arc<dyn crate::data::store::DataStore>,
}
