//! In-memory fakes used by the endpoint unit tests.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use uuid::Uuid;

use super::repositories::{
    AccessRequestRecord, AuditFilter, AuditRecord, DeclaredPermission, Delegation,
    HealthRepository, IdentityRepository, ListedUser, NewToken, PluginRecord, PluginRepository,
    Repositories, RepositoryError, SettingChange, StoredFeature, StoredPluginFlag, StoredSetting,
    StoredSwitch, TeamRepository, TokenRecord,
};
use crate::identity::{
    Account, ApiToken, Arrival, AuditEntry, Identity, NewUser, Principal, Profile, ServiceAccount,
    TokenOwner, User,
};
use crate::secrets::TokenKind;
use crate::status::plugins::{PluginChange, PluginStatus, PluginStatuses, Source, StatusChange};
use crate::teams::{
    AccountOwner, BY_DEFAULT, BY_PROVIDER, NewTeam, Organisation, Owners, Position, Team,
    TeamChanges, TeamMember, TeamPosition, below_itself,
};

#[derive(Debug, Default)]
pub struct FakeHealth {
    down: AtomicBool,
}

impl FakeHealth {
    pub fn up() -> Arc<Self> {
        Arc::new(Self { down: AtomicBool::new(false) })
    }

    pub fn down() -> Arc<Self> {
        Arc::new(Self { down: AtomicBool::new(true) })
    }

    pub fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }
}

#[async_trait]
impl HealthRepository for FakeHealth {
    async fn ping(&self) -> Result<(), RepositoryError> {
        if self.down.load(Ordering::SeqCst) {
            return Err(RepositoryError::Unavailable("connection refused".into()));
        }
        Ok(())
    }
}

#[derive(Debug, Default)]
struct State {
    users: Vec<User>,
    identities: Vec<Identity>,
    organisations: Vec<Organisation>,
    teams: Vec<Team>,
    members: Vec<TeamMember>,
    positions: Vec<Position>,
    team_positions: Vec<TeamPosition>,
    /// Each chosen identity provider and the organisation it signs people in for.
    providers: Vec<(String, Uuid)>,
    domains: Vec<(String, Uuid)>,
    accounts: Vec<ServiceAccount>,
    /// Each plugin's own service account, as plugin and account.
    plugin_accounts: Vec<(String, Uuid)>,
    /// What each person chose for their dashboard.
    dashboards: Vec<(Uuid, Vec<String>)>,
    tokens: Vec<(ApiToken, Vec<u8>)>,
    plugins: Vec<String>,
    audit: Vec<AuditEntry>,
}

#[derive(Debug, Default)]
pub struct FakeIdentity {
    state: Mutex<State>,
}

impl FakeIdentity {
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn with_user(login: &str) -> (Arc<Self>, User) {
        let identity = Self::empty();
        let user = identity.add_user(login);
        (identity, user)
    }

    /// A user who has signed in, with a `local` account of the same login.
    pub fn add_user(&self, login: &str) -> User {
        let user = self.add_unsigned(login);
        let account =
            Account { provider: "local".into(), external_id: login.into(), login: login.into() };
        let mut state = self.state.lock();
        let identity = new_identity(user.id, &account, "sign-in");
        state.identities.push(identity);
        let now = Utc::now();
        let stored = state.users.iter_mut().find(|stored| stored.id == user.id).expect("added");
        stored.first_signed_in_at = Some(now);
        stored.last_signed_in_at = Some(now);
        let stored = stored.clone();
        linked(&state, stored)
    }

    /// A user an admin made, who has never signed in and has no accounts.
    pub fn add_unsigned(&self, login: &str) -> User {
        let organisation = self.organisation();
        let user = User {
            id: Uuid::now_v7(),
            login: login.into(),
            organisation_id: organisation,
            name: None,
            email: None,
            first_name: None,
            surname: None,
            disabled: false,
            first_signed_in_at: None,
            last_signed_in_at: None,
            linked: Default::default(),
            scopes: None,
        };
        self.state.lock().users.push(user.clone());
        user
    }

    /// The organisation people the helpers make belong to, as the first start makes one: the one
    /// named `default`, made the first time it is asked for.
    pub fn organisation(&self) -> Uuid {
        let mut state = self.state.lock();
        if let Some(found) = state.organisations.iter().find(|found| found.name == "default") {
            return found.id;
        }
        let organisation = Organisation {
            id: Uuid::now_v7(),
            name: "default".into(),
            title: "Default organisation".into(),
            description: String::new(),
            created_at: Utc::now(),
        };
        let id = organisation.id;
        state.organisations.push(organisation);
        id
    }

    pub fn identities_of(&self, user: Uuid) -> Vec<Identity> {
        let state = self.state.lock();
        state.identities.iter().filter(|identity| identity.user_id == user).cloned().collect()
    }

    /// What the first start makes: the default organisation, with Leadership and Product marked
    /// default, and everyone already here placed in them.
    pub fn with_default_teams(&self) -> Vec<Team> {
        let organisation = self.organisation();
        let mut state = self.state.lock();
        let teams: Vec<Team> = [("leadership", "Leadership"), ("product", "Product")]
            .into_iter()
            .map(|(name, title)| Team {
                id: Uuid::now_v7(),
                organisation_id: organisation,
                parent_id: None,
                name: name.into(),
                title: title.into(),
                description: String::new(),
                email: String::new(),
                is_default: true,
                provider: None,
                external_id: None,
                lead_id: None,
                created_at: Utc::now(),
            })
            .collect();
        state.teams.extend(teams.iter().cloned());
        let users: Vec<Uuid> = state.users.iter().map(|user| user.id).collect();
        for team in &teams {
            for user in &users {
                state.members.push(member(team.id, *user, BY_DEFAULT, None));
            }
        }
        teams
    }

    pub fn add_service_account(&self, name: &str) -> ServiceAccount {
        let account = ServiceAccount {
            id: Uuid::now_v7(),
            name: name.into(),
            description: None,
            owner_id: None,
            owner_team_id: None,
            disabled: false,
            created_at: Utc::now(),
        };
        self.state.lock().accounts.push(account.clone());
        account
    }

    /// Issues a token directly, so a test can arrange an expired, revoked or foreign one.
    pub fn give(
        &self,
        secret: &str,
        kind: TokenKind,
        owner: TokenOwner,
        expires_at: Option<DateTime<Utc>>,
        revoked: bool,
    ) -> ApiToken {
        let token = ApiToken {
            id: Uuid::now_v7(),
            kind,
            name: Some("test".into()),
            owner,
            created_at: Utc::now(),
            last_used_at: None,
            expires_at,
            revoked_at: revoked.then(Utc::now),
            scopes: None,
            issued_by: None,
        };
        self.state.lock().tokens.push((token.clone(), crate::identity::token_hash(secret)));
        token
    }

    pub fn set_user_disabled(&self, id: Uuid, disabled: bool) {
        let mut state = self.state.lock();
        if let Some(user) = state.users.iter_mut().find(|user| user.id == id) {
            user.disabled = disabled;
        }
    }

    pub fn set_account_disabled(&self, id: Uuid, disabled: bool) {
        let mut state = self.state.lock();
        if let Some(account) = state.accounts.iter_mut().find(|account| account.id == id) {
            account.disabled = disabled;
        }
    }

    pub fn audit_actions(&self) -> Vec<String> {
        self.state.lock().audit.iter().map(|entry| entry.action.clone()).collect()
    }

    pub fn audit_entries(&self) -> Vec<AuditEntry> {
        self.state.lock().audit.clone()
    }

    fn principal_for(state: &State, owner: &TokenOwner) -> Option<Principal> {
        match owner {
            TokenOwner::User(id) => state
                .users
                .iter()
                .find(|user| user.id == *id)
                .cloned()
                .map(|user| Principal::User(linked(state, user))),
            TokenOwner::ServiceAccount(id) => state
                .accounts
                .iter()
                .find(|account| account.id == *id)
                .cloned()
                .map(Principal::ServiceAccount),
            TokenOwner::Plugin(id) => Some(Principal::Plugin { id: id.clone() }),
        }
    }
}

#[async_trait]
impl IdentityRepository for FakeIdentity {
    async fn service_account_by_name(
        &self,
        name: &str,
    ) -> Result<Option<ServiceAccount>, RepositoryError> {
        Ok(self.state.lock().accounts.iter().find(|account| account.name == name).cloned())
    }

    async fn create_service_account(
        &self,
        name: &str,
        description: Option<&str>,
        owner: AccountOwner,
    ) -> Result<ServiceAccount, RepositoryError> {
        let (owner_id, owner_team_id) = owners_of(owner);
        let account = ServiceAccount {
            id: Uuid::now_v7(),
            name: name.to_string(),
            description: description.map(str::to_string),
            owner_id,
            owner_team_id,
            disabled: false,
            created_at: Utc::now(),
        };
        self.state.lock().accounts.push(account.clone());
        Ok(account)
    }

    async fn service_account_by_id(
        &self,
        id: Uuid,
    ) -> Result<Option<ServiceAccount>, RepositoryError> {
        Ok(self.state.lock().accounts.iter().find(|account| account.id == id).cloned())
    }

    async fn plugin_service_account(
        &self,
        plugin: &str,
    ) -> Result<Option<ServiceAccount>, RepositoryError> {
        let state = self.state.lock();
        let Some((_, id)) = state.plugin_accounts.iter().find(|(held, _)| held == plugin) else {
            return Ok(None);
        };
        Ok(state.accounts.iter().find(|account| account.id == *id).cloned())
    }

    async fn link_plugin_service_account(
        &self,
        plugin: &str,
        account: Uuid,
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        state.plugin_accounts.retain(|(held, _)| held != plugin);
        state.plugin_accounts.push((plugin.to_string(), account));
        Ok(())
    }

    async fn dashboard(&self, user: Uuid) -> Result<Option<Vec<String>>, RepositoryError> {
        let state = self.state.lock();
        Ok(state.dashboards.iter().find(|(held, _)| *held == user).map(|(_, items)| items.clone()))
    }

    async fn set_dashboard(&self, user: Uuid, items: &[String]) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        state.dashboards.retain(|(held, _)| *held != user);
        state.dashboards.push((user, items.to_vec()));
        Ok(())
    }

    async fn service_account_plugin(
        &self,
        account: Uuid,
    ) -> Result<Option<String>, RepositoryError> {
        let state = self.state.lock();
        Ok(state
            .plugin_accounts
            .iter()
            .find(|(_, id)| *id == account)
            .map(|(held, _)| held.clone()))
    }

    async fn list_service_accounts(
        &self,
        owners: Option<&Owners>,
    ) -> Result<Vec<ServiceAccount>, RepositoryError> {
        Ok(self
            .state
            .lock()
            .accounts
            .iter()
            .filter(|account| {
                owners.is_none_or(|owners| {
                    account.owner_id == Some(owners.user)
                        || account.owner_team_id.is_some_and(|team| owners.teams.contains(&team))
                })
            })
            .cloned()
            .collect())
    }

    async fn set_service_account_owner(
        &self,
        id: Uuid,
        owner: AccountOwner,
    ) -> Result<Option<ServiceAccount>, RepositoryError> {
        let mut state = self.state.lock();
        let Some(account) = state.accounts.iter_mut().find(|account| account.id == id) else {
            return Ok(None);
        };
        (account.owner_id, account.owner_team_id) = owners_of(owner);
        Ok(Some(account.clone()))
    }

    async fn set_service_account_disabled(
        &self,
        id: Uuid,
        disabled: bool,
    ) -> Result<Option<ServiceAccount>, RepositoryError> {
        let mut state = self.state.lock();
        let Some(account) = state.accounts.iter_mut().find(|account| account.id == id) else {
            return Ok(None);
        };
        account.disabled = disabled;
        Ok(Some(account.clone()))
    }

    async fn user_by_id(&self, id: Uuid) -> Result<Option<User>, RepositoryError> {
        let state = self.state.lock();
        Ok(state.users.iter().find(|user| user.id == id).cloned().map(|user| linked(&state, user)))
    }

    async fn list_users(&self) -> Result<Vec<ListedUser>, RepositoryError> {
        let state = self.state.lock();
        Ok(state
            .users
            .iter()
            .cloned()
            .map(|user| ListedUser {
                created_at: user.first_signed_in_at.unwrap_or_default(),
                user: linked(&state, user),
            })
            .collect())
    }

    async fn set_user_disabled(
        &self,
        id: Uuid,
        disabled: bool,
    ) -> Result<Option<User>, RepositoryError> {
        let mut state = self.state.lock();
        let Some(user) = state.users.iter_mut().find(|user| user.id == id) else {
            return Ok(None);
        };
        user.disabled = disabled;
        let user = user.clone();
        Ok(Some(linked(&state, user)))
    }

    async fn record_token(&self, token: NewToken) -> Result<bool, RepositoryError> {
        let mut state = self.state.lock();
        if state.tokens.iter().any(|(_, hash)| hash == &token.token_hash) {
            return Ok(false);
        }
        let hash = token.token_hash.clone();
        state.tokens.push((into_api_token(token), hash));
        Ok(true)
    }

    async fn register_plugin(&self, id: &str) -> Result<bool, RepositoryError> {
        let mut state = self.state.lock();
        if state.plugins.iter().any(|plugin| plugin == id) {
            return Ok(false);
        }
        state.plugins.push(id.to_string());
        Ok(true)
    }

    async fn list_plugins(&self) -> Result<Vec<String>, RepositoryError> {
        Ok(self.state.lock().plugins.clone())
    }

    async fn token_by_hash(&self, hash: &[u8]) -> Result<Option<TokenRecord>, RepositoryError> {
        let state = self.state.lock();
        let Some((token, _)) = state.tokens.iter().find(|(_, stored)| stored == hash) else {
            return Ok(None);
        };
        let Some(principal) = Self::principal_for(&state, &token.owner) else {
            return Ok(None);
        };
        Ok(Some(TokenRecord { token: token.clone(), principal }))
    }

    async fn revoke_issued(
        &self,
        id: Uuid,
        plugin: &str,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let mut state = self.state.lock();
        let found = state.tokens.iter_mut().find(|(token, _)| {
            token.id == id
                && token.kind == TokenKind::Scoped
                && token.issued_by.as_deref() == Some(plugin)
                && token.revoked_at.is_none()
        });
        Ok(found.map(|(token, hash)| {
            token.revoked_at = Some(Utc::now());
            hash.clone()
        }))
    }

    async fn touch_token(&self, id: Uuid) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        if let Some((token, _)) = state.tokens.iter_mut().find(|(token, _)| token.id == id) {
            token.last_used_at = Some(Utc::now());
        }
        Ok(())
    }

    async fn issue_token(&self, token: NewToken) -> Result<ApiToken, RepositoryError> {
        let hash = token.token_hash.clone();
        let token = into_api_token(token);
        self.state.lock().tokens.push((token.clone(), hash));
        Ok(token)
    }

    async fn list_tokens(
        &self,
        owner: &TokenOwner,
        kind: TokenKind,
    ) -> Result<Vec<ApiToken>, RepositoryError> {
        Ok(self
            .state
            .lock()
            .tokens
            .iter()
            .filter(|(token, _)| &token.owner == owner && token.kind == kind)
            .filter(|(token, _)| token.revoked_at.is_none())
            .map(|(token, _)| token.clone())
            .collect())
    }

    async fn revoke_token(
        &self,
        id: Uuid,
        owner: &TokenOwner,
    ) -> Result<Option<Vec<u8>>, RepositoryError> {
        let mut state = self.state.lock();
        let Some((token, hash)) =
            state.tokens.iter_mut().find(|(token, _)| token.id == id && &token.owner == owner)
        else {
            return Ok(None);
        };
        if token.revoked_at.is_some() {
            return Ok(None);
        }
        token.revoked_at = Some(Utc::now());
        Ok(Some(hash.clone()))
    }

    async fn user_for_account(
        &self,
        account: &Account,
        profile: &Profile,
        arrival: Arrival,
        organisation: Uuid,
    ) -> Result<(User, bool), RepositoryError> {
        let mut state = self.state.lock();
        let held = state.identities.iter_mut().find(|identity| {
            identity.provider == account.provider && identity.external_id == account.external_id
        });
        if let Some(identity) = held {
            let was = std::mem::replace(&mut identity.login, account.login.clone());
            if arrival == Arrival::SignIn {
                identity.last_used_at = Some(Utc::now());
            }
            report(identity, profile);
            let id = identity.user_id;
            let user = state.users.iter_mut().find(|user| user.id == id).ok_or_else(gone)?;
            if user.login == was {
                user.login = account.login.clone();
            }
            user.name = profile.name.clone().or(user.name.take());
            user.email = profile.email.clone().or(user.email.take());
            user.first_name = profile.first_name.clone().or(user.first_name.take());
            user.surname = profile.surname.clone().or(user.surname.take());
            let user = user.clone();
            return Ok((linked(&state, user), false));
        }
        if !state.organisations.iter().any(|found| found.id == organisation) {
            return Err(RepositoryError::Conflict("that organisation is gone".into()));
        }
        let user = User {
            id: Uuid::now_v7(),
            login: account.login.clone(),
            organisation_id: organisation,
            name: profile.name.clone(),
            email: profile.email.clone(),
            first_name: profile.first_name.clone(),
            surname: profile.surname.clone(),
            disabled: false,
            first_signed_in_at: None,
            last_signed_in_at: None,
            linked: Default::default(),
            scopes: None,
        };
        let mut identity = new_identity(user.id, account, arrival.source());
        if arrival == Arrival::SignIn {
            identity.last_used_at = Some(Utc::now());
        }
        report(&mut identity, profile);
        state.users.push(user.clone());
        state.identities.push(identity);
        place_in_default_teams(&mut state, user.id, organisation);
        Ok((linked(&state, user), true))
    }

    async fn create_user(
        &self,
        new: &NewUser,
        account: Option<&Account>,
    ) -> Result<User, RepositoryError> {
        let mut state = self.state.lock();
        if let Some(account) = account
            && holder(&state, account).is_some()
        {
            return Err(taken(account));
        }
        if !state.organisations.iter().any(|found| found.id == new.organisation_id) {
            return Err(RepositoryError::Conflict("that organisation is gone".into()));
        }
        let user = User {
            id: Uuid::now_v7(),
            login: new.login.clone(),
            organisation_id: new.organisation_id,
            name: new.name.clone(),
            email: new.email.clone(),
            first_name: None,
            surname: None,
            disabled: false,
            first_signed_in_at: None,
            last_signed_in_at: None,
            linked: Default::default(),
            scopes: None,
        };
        state.users.push(user.clone());
        if let Some(account) = account {
            state.identities.push(new_identity(user.id, account, "admin"));
        }
        place_in_default_teams(&mut state, user.id, new.organisation_id);
        Ok(linked(&state, user))
    }

    async fn identity(
        &self,
        provider: &str,
        external_id: &str,
    ) -> Result<Option<Identity>, RepositoryError> {
        let state = self.state.lock();
        Ok(state
            .identities
            .iter()
            .find(|identity| identity.provider == provider && identity.external_id == external_id)
            .cloned())
    }

    async fn identities(&self, user: Option<Uuid>) -> Result<Vec<Identity>, RepositoryError> {
        let state = self.state.lock();
        Ok(state
            .identities
            .iter()
            .filter(|identity| user.is_none_or(|user| identity.user_id == user))
            .cloned()
            .collect())
    }

    async fn attach_identity(
        &self,
        user: Uuid,
        account: &Account,
        source: &str,
    ) -> Result<Identity, RepositoryError> {
        let mut state = self.state.lock();
        let same_provider = state
            .identities
            .iter()
            .any(|identity| identity.user_id == user && identity.provider == account.provider);
        if holder(&state, account).is_some() || same_provider {
            return Err(taken(account));
        }
        let identity = new_identity(user, account, source);
        state.identities.push(identity.clone());
        Ok(identity)
    }

    async fn detach_identity(
        &self,
        user: Uuid,
        identity: Uuid,
    ) -> Result<Option<Identity>, RepositoryError> {
        let mut state = self.state.lock();
        let Some(index) =
            state.identities.iter().position(|held| held.id == identity && held.user_id == user)
        else {
            return Ok(None);
        };
        Ok(Some(state.identities.remove(index)))
    }

    async fn merge_users(&self, from: Uuid, into: Uuid) -> Result<User, RepositoryError> {
        let conflict = |detail: &str| RepositoryError::Conflict(detail.to_string());
        let mut state = self.state.lock();
        if from == into {
            return Err(conflict("a user cannot be merged into themselves"));
        }
        let Some(merged) = state.users.iter().find(|user| user.id == from).cloned() else {
            return Err(conflict("one of the users is gone"));
        };
        let Some(kept) = state.users.iter().find(|user| user.id == into) else {
            return Err(conflict("one of the users is gone"));
        };
        if kept.organisation_id != merged.organisation_id {
            return Err(conflict("they belong to different organisations"));
        }
        if merged.first_signed_in_at.is_some() {
            return Err(conflict("they have signed in"));
        }
        let providers = |user: Uuid| -> Vec<String> {
            state
                .identities
                .iter()
                .filter(|identity| identity.user_id == user)
                .map(|identity| identity.provider.clone())
                .collect()
        };
        let theirs = providers(into);
        if providers(from).iter().any(|provider| theirs.contains(provider)) {
            return Err(conflict("both have an account with the same provider"));
        }
        let merged = linked(&state, merged);
        for identity in state.identities.iter_mut().filter(|identity| identity.user_id == from) {
            identity.user_id = into;
        }
        for account in state.accounts.iter_mut().filter(|account| account.owner_id == Some(from)) {
            account.owner_id = Some(into);
        }
        for (token, _) in state.tokens.iter_mut() {
            if token.owner == TokenOwner::User(from) {
                token.owner = TokenOwner::User(into);
            }
        }
        let theirs: Vec<Uuid> = state
            .members
            .iter()
            .filter(|held| held.user_id == into)
            .map(|held| held.team_id)
            .collect();
        state.members.retain(|held| held.user_id != from || !theirs.contains(&held.team_id));
        for held in state.members.iter_mut().filter(|held| held.user_id == from) {
            held.user_id = into;
        }
        for team in state.teams.iter_mut().filter(|team| team.lead_id == Some(from)) {
            team.lead_id = Some(into);
        }
        if let Some(user) = state.users.iter_mut().find(|user| user.id == into) {
            user.name = user.name.take().or(merged.name.clone());
            user.email = user.email.take().or(merged.email.clone());
        }
        state.users.retain(|user| user.id != from);
        Ok(merged)
    }

    async fn move_user(
        &self,
        id: Uuid,
        organisation: Uuid,
    ) -> Result<Option<User>, RepositoryError> {
        let mut state = self.state.lock();
        if !state.organisations.iter().any(|found| found.id == organisation) {
            return Err(RepositoryError::Conflict("that organisation is gone".into()));
        }
        let Some(user) = state.users.iter_mut().find(|user| user.id == id) else {
            return Ok(None);
        };
        user.organisation_id = organisation;
        let user = user.clone();
        let elsewhere: Vec<Uuid> = state
            .teams
            .iter()
            .filter(|team| team.organisation_id != organisation)
            .map(|team| team.id)
            .collect();
        state.members.retain(|held| held.user_id != id || !elsewhere.contains(&held.team_id));
        unled(&mut state);
        place_in_default_teams(&mut state, id, organisation);
        Ok(Some(linked(&state, user)))
    }

    async fn record_sign_in(&self, id: Uuid) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        if let Some(user) = state.users.iter_mut().find(|user| user.id == id) {
            let now = Utc::now();
            user.first_signed_in_at.get_or_insert(now);
            user.last_signed_in_at = Some(now);
        }
        Ok(())
    }

    async fn purge_expired_tokens(&self, before: DateTime<Utc>) -> Result<u64, RepositoryError> {
        let mut state = self.state.lock();
        let before_count = state.tokens.len();
        state.tokens.retain(|(token, _)| {
            token.expires_at.is_none_or(|expiry| expiry > before)
                && token.revoked_at.is_none_or(|revoked| revoked > before)
        });
        Ok((before_count - state.tokens.len()) as u64)
    }

    async fn record_audit(&self, entry: AuditEntry) -> Result<(), RepositoryError> {
        self.state.lock().audit.push(entry);
        Ok(())
    }

    async fn audit_log(&self, filter: &AuditFilter) -> Result<Vec<AuditRecord>, RepositoryError> {
        let entries = self.state.lock().audit.clone();
        let now = Utc::now();
        let count = entries.len();
        let records = entries.into_iter().enumerate().map(|(index, entry)| AuditRecord {
            at: now - chrono::Duration::seconds((count - index) as i64),
            actor_kind: entry.actor_kind,
            actor_id: entry.actor_id,
            actor_label: entry.actor_label,
            action: entry.action,
            subject: entry.subject,
            detail: entry.detail,
        });
        let mut records: Vec<AuditRecord> = records
            .filter(|record| {
                filter.action.as_deref().is_none_or(|prefix| record.action.starts_with(prefix))
            })
            .filter(|record| {
                filter.actor.as_deref().is_none_or(|actor| {
                    record.actor_label.as_deref() == Some(actor)
                        || record.actor_id.as_deref() == Some(actor)
                })
            })
            .filter(|record| filter.before.is_none_or(|before| record.at < before))
            .collect();
        records.reverse();
        records.truncate(filter.limit as usize);
        Ok(records)
    }
}

fn owners_of(owner: AccountOwner) -> (Option<Uuid>, Option<Uuid>) {
    match owner {
        AccountOwner::Platform => (None, None),
        AccountOwner::User(user) => (Some(user), None),
        AccountOwner::Team(team) => (None, Some(team)),
    }
}

fn member(team: Uuid, user: Uuid, source: &str, provider: Option<&str>) -> TeamMember {
    TeamMember {
        team_id: team,
        user_id: user,
        source: source.to_string(),
        provider: provider.map(str::to_string),
        position: None,
        created_at: Utc::now(),
    }
}

/// A lead is one of the team's members, so someone who leaves stops leading it.
fn unled(state: &mut State) {
    let State { teams, members, .. } = state;
    for team in teams.iter_mut() {
        if let Some(lead) = team.lead_id
            && !members.iter().any(|held| held.team_id == team.id && held.user_id == lead)
        {
            team.lead_id = None;
        }
    }
}

fn place_in_default_teams(state: &mut State, user: Uuid, organisation: Uuid) {
    let defaults: Vec<Uuid> = state
        .teams
        .iter()
        .filter(|team| team.is_default && team.organisation_id == organisation)
        .map(|team| team.id)
        .collect();
    for team in defaults {
        if !state.members.iter().any(|held| held.team_id == team && held.user_id == user) {
            state.members.push(member(team, user, BY_DEFAULT, None));
        }
    }
}

fn conflict(detail: &str) -> RepositoryError {
    RepositoryError::Conflict(detail.to_string())
}

/// Refuses taking the default mark off the last default team of an organisation.
fn keep_a_default(state: &State, team: Uuid) -> Result<(), RepositoryError> {
    let organisation =
        state.teams.iter().find(|found| found.id == team).map(|found| found.organisation_id);
    let defaults: Vec<Uuid> = state
        .teams
        .iter()
        .filter(|team| team.is_default && Some(team.organisation_id) == organisation)
        .map(|team| team.id)
        .collect();
    match defaults.as_slice() {
        [only] if *only == team => Err(conflict("it is the last default team")),
        _ => Ok(()),
    }
}

#[async_trait]
impl TeamRepository for FakeIdentity {
    async fn organisations(&self) -> Result<Vec<Organisation>, RepositoryError> {
        let mut organisations = self.state.lock().organisations.clone();
        organisations.sort_by(|a, b| (&a.title, &a.name).cmp(&(&b.title, &b.name)));
        Ok(organisations)
    }

    async fn organisation(&self, id: Uuid) -> Result<Option<Organisation>, RepositoryError> {
        let state = self.state.lock();
        Ok(state.organisations.iter().find(|organisation| organisation.id == id).cloned())
    }

    async fn organisation_named(
        &self,
        name: &str,
    ) -> Result<Option<Organisation>, RepositoryError> {
        let state = self.state.lock();
        Ok(state.organisations.iter().find(|organisation| organisation.name == name).cloned())
    }

    async fn create_organisation(
        &self,
        name: &str,
        title: &str,
        description: &str,
    ) -> Result<Organisation, RepositoryError> {
        let mut state = self.state.lock();
        if state.organisations.iter().any(|organisation| organisation.name == name) {
            return Err(conflict("there is an organisation called that already"));
        }
        let organisation = Organisation {
            id: Uuid::now_v7(),
            name: name.into(),
            title: title.into(),
            description: description.into(),
            created_at: Utc::now(),
        };
        state.organisations.push(organisation.clone());
        Ok(organisation)
    }

    async fn update_organisation(
        &self,
        id: Uuid,
        name: &str,
        title: &str,
        description: &str,
    ) -> Result<Option<Organisation>, RepositoryError> {
        let mut state = self.state.lock();
        if state
            .organisations
            .iter()
            .any(|organisation| organisation.name == name && organisation.id != id)
        {
            return Err(conflict("there is an organisation called that already"));
        }
        let Some(organisation) = state.organisations.iter_mut().find(|found| found.id == id) else {
            return Ok(None);
        };
        (organisation.name, organisation.title, organisation.description) =
            (name.into(), title.into(), description.into());
        Ok(Some(organisation.clone()))
    }

    async fn delete_organisation(&self, id: Uuid) -> Result<Option<Organisation>, RepositoryError> {
        let mut state = self.state.lock();
        if state.teams.iter().any(|team| team.organisation_id == id) {
            return Err(conflict("it still has teams"));
        }
        if state.users.iter().any(|user| user.organisation_id == id) {
            return Err(conflict("people belong to it"));
        }
        let Some(index) = state.organisations.iter().position(|found| found.id == id) else {
            return Ok(None);
        };
        Ok(Some(state.organisations.remove(index)))
    }

    async fn teams(&self) -> Result<Vec<Team>, RepositoryError> {
        Ok(self.state.lock().teams.clone())
    }

    async fn team(&self, id: Uuid) -> Result<Option<Team>, RepositoryError> {
        Ok(self.state.lock().teams.iter().find(|team| team.id == id).cloned())
    }

    async fn provided_team(
        &self,
        provider: &str,
        external_id: &str,
    ) -> Result<Option<Team>, RepositoryError> {
        let state = self.state.lock();
        Ok(state
            .teams
            .iter()
            .find(|team| {
                team.provider.as_deref() == Some(provider)
                    && team.external_id.as_deref() == Some(external_id)
            })
            .cloned())
    }

    async fn create_team(&self, new: &NewTeam) -> Result<Team, RepositoryError> {
        let mut state = self.state.lock();
        if !state.organisations.iter().any(|organisation| organisation.id == new.organisation_id) {
            return Err(conflict("that organisation is gone"));
        }
        if let Some(parent) = new.parent_id {
            let parent = state.teams.iter().find(|team| team.id == parent);
            let parent = parent.ok_or_else(|| conflict("that parent team is gone"))?;
            if parent.organisation_id != new.organisation_id {
                return Err(conflict("a sub-team is in its parent's organisation"));
            }
        }
        let taken = state.teams.iter().any(|team| {
            (team.organisation_id == new.organisation_id && team.name == new.name)
                || new.provided.as_ref().is_some_and(|(provider, external_id)| {
                    team.provider.as_ref() == Some(provider)
                        && team.external_id.as_ref() == Some(external_id)
                })
        });
        if taken {
            return Err(conflict("there is a team called that there already"));
        }
        let (provider, external_id) = new.provided.clone().unzip();
        let team = Team {
            id: Uuid::now_v7(),
            organisation_id: new.organisation_id,
            parent_id: new.parent_id,
            name: new.name.clone(),
            title: new.title.clone(),
            description: new.description.clone(),
            email: new.email.clone(),
            is_default: new.is_default,
            provider,
            external_id,
            lead_id: None,
            created_at: Utc::now(),
        };
        state.teams.push(team.clone());
        Ok(team)
    }

    async fn update_team(
        &self,
        id: Uuid,
        changes: &TeamChanges,
    ) -> Result<Option<Team>, RepositoryError> {
        let mut state = self.state.lock();
        let Some(team) = state.teams.iter().find(|team| team.id == id).cloned() else {
            return Ok(None);
        };
        if let Some(Some(parent)) = changes.parent {
            let found = state.teams.iter().find(|candidate| candidate.id == parent);
            let found = found.ok_or_else(|| conflict("that parent team is gone"))?;
            if found.organisation_id != team.organisation_id {
                return Err(conflict("a sub-team is in its parent's organisation"));
            }
            if below_itself(&state.teams, id, parent) {
                return Err(conflict("a team cannot sit inside itself or its own sub-teams"));
            }
        }
        if let Some(name) = &changes.name
            && state.teams.iter().any(|other| {
                other.id != id
                    && other.organisation_id == team.organisation_id
                    && &other.name == name
            })
        {
            return Err(conflict("there is a team called that in its organisation already"));
        }
        if changes.is_default == Some(false) && team.is_default {
            keep_a_default(&state, id)?;
        }
        let stored =
            state.teams.iter_mut().find(|team| team.id == id).ok_or_else(|| conflict("gone"))?;
        if let Some(name) = &changes.name {
            stored.name = name.clone();
        }
        if let Some(title) = &changes.title {
            stored.title = title.clone();
        }
        if let Some(description) = &changes.description {
            stored.description = description.clone();
        }
        if let Some(email) = &changes.email {
            stored.email = email.clone();
        }
        if let Some(parent) = changes.parent {
            stored.parent_id = parent;
        }
        if let Some(is_default) = changes.is_default {
            stored.is_default = is_default;
        }
        Ok(Some(stored.clone()))
    }

    async fn delete_team(&self, id: Uuid) -> Result<Option<Team>, RepositoryError> {
        let mut state = self.state.lock();
        let Some(team) = state.teams.iter().find(|team| team.id == id).cloned() else {
            return Ok(None);
        };
        if state.teams.iter().any(|child| child.parent_id == Some(id)) {
            return Err(conflict("it has sub-teams"));
        }
        if state.accounts.iter().any(|account| account.owner_team_id == Some(id)) {
            return Err(conflict("it owns service accounts"));
        }
        if team.is_default {
            keep_a_default(&state, id)?;
        }
        state.teams.retain(|found| found.id != id);
        state.members.retain(|held| held.team_id != id);
        state.team_positions.retain(|change| change.team_id != id);
        Ok(Some(team))
    }

    async fn release_team(&self, id: Uuid) -> Result<Option<Team>, RepositoryError> {
        let mut state = self.state.lock();
        let Some(team) = state.teams.iter_mut().find(|team| team.id == id) else {
            return Ok(None);
        };
        (team.provider, team.external_id) = (None, None);
        Ok(Some(team.clone()))
    }

    async fn members(&self, team: Option<Uuid>) -> Result<Vec<TeamMember>, RepositoryError> {
        let state = self.state.lock();
        Ok(state
            .members
            .iter()
            .filter(|held| team.is_none_or(|team| held.team_id == team))
            .cloned()
            .collect())
    }

    async fn memberships(&self, user: Uuid) -> Result<Vec<TeamMember>, RepositoryError> {
        let state = self.state.lock();
        Ok(state.members.iter().filter(|held| held.user_id == user).cloned().collect())
    }

    async fn add_member(
        &self,
        team: Uuid,
        user: Uuid,
        source: &str,
        provider: Option<&str>,
    ) -> Result<bool, RepositoryError> {
        let mut state = self.state.lock();
        let theirs =
            state.users.iter().find(|found| found.id == user).map(|found| found.organisation_id);
        let its =
            state.teams.iter().find(|found| found.id == team).map(|found| found.organisation_id);
        if theirs.is_some() && its.is_some() && theirs != its {
            return Err(conflict("they belong to another organisation"));
        }
        if state.members.iter().any(|held| held.team_id == team && held.user_id == user) {
            return Ok(false);
        }
        state.members.push(member(team, user, source, provider));
        Ok(true)
    }

    async fn remove_member(
        &self,
        team: Uuid,
        user: Uuid,
    ) -> Result<Option<TeamMember>, RepositoryError> {
        let mut state = self.state.lock();
        let Some(index) =
            state.members.iter().position(|held| held.team_id == team && held.user_id == user)
        else {
            return Ok(None);
        };
        let removed = state.members.remove(index);
        unled(&mut state);
        Ok(Some(removed))
    }

    async fn set_lead(
        &self,
        team: Uuid,
        lead: Option<Uuid>,
    ) -> Result<Option<Team>, RepositoryError> {
        let mut state = self.state.lock();
        if let Some(lead) = lead
            && !state.members.iter().any(|held| held.team_id == team && held.user_id == lead)
        {
            return Err(conflict("a team's lead is one of its members"));
        }
        let Some(stored) = state.teams.iter_mut().find(|found| found.id == team) else {
            return Ok(None);
        };
        stored.lead_id = lead;
        Ok(Some(stored.clone()))
    }

    async fn set_member_position(
        &self,
        team: Uuid,
        user: Uuid,
        position: Option<&str>,
    ) -> Result<Option<TeamMember>, RepositoryError> {
        let mut state = self.state.lock();
        let held =
            state.members.iter_mut().find(|held| held.team_id == team && held.user_id == user);
        Ok(held.map(|held| {
            held.position = position.map(str::to_string);
            held.clone()
        }))
    }

    async fn positions(
        &self,
        organisation: Option<Uuid>,
    ) -> Result<Vec<Position>, RepositoryError> {
        let state = self.state.lock();
        let mut found: Vec<Position> = state
            .positions
            .iter()
            .filter(|position| organisation.is_none_or(|id| position.organisation_id == id))
            .cloned()
            .collect();
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(found)
    }

    async fn put_position(
        &self,
        organisation: Uuid,
        name: &str,
        title: &str,
        description: &str,
        responsibilities: &[String],
    ) -> Result<Position, RepositoryError> {
        let mut state = self.state.lock();
        if !state.organisations.iter().any(|found| found.id == organisation) {
            return Err(conflict("that organisation is gone"));
        }
        let held = state
            .positions
            .iter_mut()
            .find(|position| position.organisation_id == organisation && position.name == name);
        if let Some(position) = held {
            position.title = title.to_string();
            position.description = description.to_string();
            position.responsibilities = responsibilities.to_vec();
            return Ok(position.clone());
        }
        let position = Position {
            id: Uuid::now_v7(),
            organisation_id: organisation,
            name: name.to_string(),
            title: title.to_string(),
            description: description.to_string(),
            responsibilities: responsibilities.to_vec(),
            created_at: Utc::now(),
        };
        state.positions.push(position.clone());
        Ok(position)
    }

    async fn delete_position(
        &self,
        organisation: Uuid,
        name: &str,
    ) -> Result<Option<Position>, RepositoryError> {
        let mut state = self.state.lock();
        let Some(index) = state
            .positions
            .iter()
            .position(|position| position.organisation_id == organisation && position.name == name)
        else {
            return Ok(None);
        };
        Ok(Some(state.positions.remove(index)))
    }

    async fn team_positions(
        &self,
        team: Option<Uuid>,
    ) -> Result<Vec<TeamPosition>, RepositoryError> {
        let state = self.state.lock();
        Ok(state
            .team_positions
            .iter()
            .filter(|change| team.is_none_or(|id| change.team_id == id))
            .cloned()
            .collect())
    }

    async fn put_team_position(&self, change: &TeamPosition) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        if !state.teams.iter().any(|found| found.id == change.team_id) {
            return Err(conflict("that team is gone"));
        }
        state
            .team_positions
            .retain(|held| !(held.team_id == change.team_id && held.name == change.name));
        state.team_positions.push(change.clone());
        Ok(())
    }

    async fn delete_team_position(&self, team: Uuid, name: &str) -> Result<bool, RepositoryError> {
        let mut state = self.state.lock();
        let before = state.team_positions.len();
        state.team_positions.retain(|held| !(held.team_id == team && held.name == name));
        Ok(state.team_positions.len() < before)
    }

    async fn vacate_position(
        &self,
        teams: &[Uuid],
        name: &str,
    ) -> Result<Vec<TeamMember>, RepositoryError> {
        let mut state = self.state.lock();
        let mut vacated = Vec::new();
        for held in &mut state.members {
            if teams.contains(&held.team_id) && held.position.as_deref() == Some(name) {
                held.position = None;
                vacated.push(held.clone());
            }
        }
        Ok(vacated)
    }

    async fn providers(&self) -> Result<Vec<(String, Uuid)>, RepositoryError> {
        Ok(self.state.lock().providers.clone())
    }

    async fn set_providers(
        &self,
        organisation: Uuid,
        providers: &[String],
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        if !state.organisations.iter().any(|found| found.id == organisation) {
            return Err(conflict("that organisation is gone"));
        }
        let taken = state
            .providers
            .iter()
            .any(|(provider, chosen)| *chosen != organisation && providers.contains(provider));
        if taken {
            return Err(conflict("another organisation signs in with one of them"));
        }
        state.providers.retain(|(_, chosen)| *chosen != organisation);
        state.providers.extend(providers.iter().map(|provider| (provider.clone(), organisation)));
        Ok(())
    }

    async fn domains(&self) -> Result<Vec<(String, Uuid)>, RepositoryError> {
        let mut domains = self.state.lock().domains.clone();
        domains.sort();
        Ok(domains)
    }

    async fn set_domains(
        &self,
        organisation: Uuid,
        domains: &[String],
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        if !state.organisations.iter().any(|found| found.id == organisation) {
            return Err(conflict("that organisation is gone"));
        }
        let taken = state
            .domains
            .iter()
            .find(|(domain, chosen)| *chosen != organisation && domains.contains(domain));
        if let Some((domain, _)) = taken {
            let said =
                format!("another organisation approved {domain}, and each domain belongs to one");
            return Err(conflict(&said));
        }
        state.domains.retain(|(_, chosen)| *chosen != organisation);
        state.domains.extend(domains.iter().map(|domain| (domain.clone(), organisation)));
        Ok(())
    }

    async fn set_provided_members(
        &self,
        team: Uuid,
        provider: &str,
        users: &[Uuid],
    ) -> Result<(Vec<Uuid>, Vec<Uuid>), RepositoryError> {
        let mut state = self.state.lock();
        let mut removed = Vec::new();
        state.members.retain(|held| {
            let theirs = held.team_id == team
                && held.source == BY_PROVIDER
                && held.provider.as_deref() == Some(provider);
            if theirs && !users.contains(&held.user_id) {
                removed.push(held.user_id);
                return false;
            }
            true
        });
        let mut added = Vec::new();
        let its =
            state.teams.iter().find(|found| found.id == team).map(|found| found.organisation_id);
        for user in users {
            let known = state
                .users
                .iter()
                .any(|found| found.id == *user && Some(found.organisation_id) == its);
            let there =
                state.members.iter().any(|held| held.team_id == team && held.user_id == *user);
            if known && !there {
                state.members.push(member(team, *user, BY_PROVIDER, Some(provider)));
                added.push(*user);
            }
        }
        unled(&mut state);
        Ok((added, removed))
    }
}

/// The user with their linked accounts, as `core.identities` holds them.
fn linked(state: &State, mut user: User) -> User {
    user.linked = state
        .identities
        .iter()
        .filter(|identity| identity.user_id == user.id)
        .map(|identity| (identity.provider.clone(), identity.login.clone()))
        .collect();
    user
}

fn holder(state: &State, account: &Account) -> Option<Uuid> {
    state
        .identities
        .iter()
        .find(|identity| {
            identity.provider == account.provider && identity.external_id == account.external_id
        })
        .map(|identity| identity.user_id)
}

fn new_identity(user: Uuid, account: &Account, source: &str) -> Identity {
    Identity {
        id: Uuid::now_v7(),
        user_id: user,
        provider: account.provider.clone(),
        external_id: account.external_id.clone(),
        login: account.login.clone(),
        source: source.to_string(),
        name: None,
        email: None,
        first_name: None,
        surname: None,
        reported_at: None,
        created_at: Utc::now(),
        last_used_at: None,
    }
}

/// Keeps what a provider last reported; what it leaves out stays as it was.
fn report(identity: &mut Identity, profile: &Profile) {
    if profile.is_empty() {
        return;
    }
    identity.name = profile.name.clone().or(identity.name.take());
    identity.email = profile.email.clone().or(identity.email.take());
    identity.first_name = profile.first_name.clone().or(identity.first_name.take());
    identity.surname = profile.surname.clone().or(identity.surname.take());
    identity.reported_at = Some(Utc::now());
}

fn taken(account: &Account) -> RepositoryError {
    RepositoryError::Conflict(format!(
        "the {} account {} is linked to someone already",
        account.provider, account.login
    ))
}

fn gone() -> RepositoryError {
    RepositoryError::Other("the user an identity belongs to is gone".into())
}

fn into_api_token(token: NewToken) -> ApiToken {
    ApiToken {
        id: Uuid::now_v7(),
        kind: token.kind,
        name: token.name,
        owner: token.owner,
        created_at: Utc::now(),
        last_used_at: None,
        expires_at: token.expires_at,
        revoked_at: None,
        scopes: token.scopes,
        issued_by: token.issued_by,
    }
}

impl Repositories {
    pub fn in_memory() -> Self {
        let identity = FakeIdentity::empty();
        Self {
            health: FakeHealth::up(),
            identity: identity.clone(),
            teams: identity,
            plugins: FakePlugins::empty(),
            plugin_status: FakePluginStatus::empty(),
            tasks: doc_background_tasks::store::MemoryTasks::new(),
            cron: doc_cron_tasks::MemoryCron::new(),
            status_history: FakeStatusHistory::empty(),
            data: crate::data::memory::MemoryData::new(),
        }
    }
}

/// Status checks in memory, as the workers would have recorded them.
#[derive(Default)]
pub struct FakeStatusHistory {
    checks: Mutex<Vec<crate::status::Component>>,
}

impl FakeStatusHistory {
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

#[async_trait]
impl crate::status::StatusHistory for FakeStatusHistory {
    async fn record(
        &self,
        component: &crate::status::Component,
    ) -> Result<(), crate::status::HistoryError> {
        self.checks.lock().push(component.clone());
        Ok(())
    }

    async fn prune(&self, before: DateTime<Utc>) -> Result<u64, crate::status::HistoryError> {
        let mut checks = self.checks.lock();
        let kept = checks.len();
        checks.retain(|check| check.checked_at >= before);
        Ok((kept - checks.len()) as u64)
    }

    async fn since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<crate::status::Component>, crate::status::HistoryError> {
        let mut checks: Vec<_> =
            self.checks.lock().iter().filter(|check| check.checked_at >= since).cloned().collect();
        checks.sort_by_key(|check| check.checked_at);
        Ok(checks)
    }
    async fn between(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<crate::status::Component>, crate::status::HistoryError> {
        let mut checks: Vec<_> = self
            .checks
            .lock()
            .iter()
            .filter(|check| from <= check.checked_at && check.checked_at < to)
            .cloned()
            .collect();
        checks.sort_by_key(|check| check.checked_at);
        Ok(checks)
    }
}

#[derive(Debug, Default)]
struct PluginState {
    records: Vec<PluginRecord>,
    versions: Vec<(String, String, String)>,
    permissions: Vec<(String, DeclaredPermission)>,
    stored: std::collections::BTreeMap<(String, String), serde_json::Value>,
    handovers: std::collections::BTreeMap<String, serde_json::Value>,
    delegations: Vec<Delegation>,
    navigation: Option<serde_json::Value>,
    settings: Option<serde_json::Value>,
    setup: Option<serde_json::Value>,
    plugin_settings: std::collections::BTreeMap<(String, String), StoredSetting>,
    plugin_features: std::collections::BTreeMap<(String, String), StoredFeature>,
    plugin_switches: std::collections::BTreeMap<String, StoredSwitch>,
    plugin_flags: std::collections::BTreeMap<String, StoredPluginFlag>,
    access_requests: Vec<AccessRequestRecord>,
}

#[derive(Debug, Default)]
pub struct FakePlugins {
    state: Mutex<PluginState>,
}

impl FakePlugins {
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Pretends this version was seen before, which is what the binary-hash conflict needs.
    pub fn remember(&self, plugin: &str, version: &str, hash: &str) {
        self.state.lock().versions.push((plugin.into(), version.into(), hash.into()));
    }

    pub fn record(&self, plugin: &str) -> Option<PluginRecord> {
        self.state.lock().records.iter().find(|record| record.id == plugin).cloned()
    }
}

#[async_trait]
impl PluginRepository for FakePlugins {
    async fn record_registration(&self, record: &PluginRecord) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        state.records.retain(|existing| existing.id != record.id);
        state.records.push(record.clone());
        Ok(())
    }

    async fn version_hash(
        &self,
        plugin: &str,
        version: &str,
    ) -> Result<Option<String>, RepositoryError> {
        Ok(self
            .state
            .lock()
            .versions
            .iter()
            .find(|(id, seen, _)| id == plugin && seen == version)
            .map(|(_, _, hash)| hash.clone()))
    }

    async fn record_version(
        &self,
        plugin: &str,
        version: &str,
        hash: &str,
        _manifest: &serde_json::Value,
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        state.versions.retain(|(id, seen, _)| !(id == plugin && seen == version));
        state.versions.push((plugin.into(), version.into(), hash.into()));
        Ok(())
    }

    async fn record_permissions(
        &self,
        plugin: &str,
        permissions: &[DeclaredPermission],
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        state.permissions.retain(|(id, _)| id != plugin);
        for permission in permissions {
            state.permissions.push((plugin.to_string(), permission.clone()));
        }
        Ok(())
    }

    async fn permissions(&self, plugin: &str) -> Result<Vec<DeclaredPermission>, RepositoryError> {
        Ok(self
            .state
            .lock()
            .permissions
            .iter()
            .filter(|(id, _)| id == plugin)
            .map(|(_, permission)| permission.clone())
            .collect())
    }

    async fn set_state(
        &self,
        plugin: &str,
        state: &str,
        error: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let mut held = self.state.lock();
        if let Some(record) = held.records.iter_mut().find(|record| record.id == plugin) {
            record.state = state.to_string();
            record.error = error.map(str::to_string);
        }
        Ok(())
    }

    async fn touch(&self, plugin: &str) -> Result<(), RepositoryError> {
        let mut held = self.state.lock();
        if let Some(record) = held.records.iter_mut().find(|record| record.id == plugin) {
            record.last_seen_at = Some(Utc::now());
        }
        Ok(())
    }

    async fn deregister(&self, plugin: &str) -> Result<(), RepositoryError> {
        let mut held = self.state.lock();
        if let Some(record) = held.records.iter_mut().find(|record| record.id == plugin) {
            record.state = String::new();
            record.address = String::new();
            record.error = None;
        }
        Ok(())
    }

    async fn records(&self) -> Result<Vec<PluginRecord>, RepositoryError> {
        Ok(self.state.lock().records.clone())
    }

    async fn save_handover(
        &self,
        plugin: &str,
        state: Option<&serde_json::Value>,
    ) -> Result<(), RepositoryError> {
        let mut held = self.state.lock();
        match state {
            Some(state) => held.handovers.insert(plugin.to_string(), state.clone()),
            None => held.handovers.remove(plugin),
        };
        Ok(())
    }

    async fn handover(&self, plugin: &str) -> Result<Option<serde_json::Value>, RepositoryError> {
        Ok(self.state.lock().handovers.get(plugin).cloned())
    }

    async fn delegate(&self, delegation: &Delegation) -> Result<(), RepositoryError> {
        self.state.lock().delegations.push(delegation.clone());
        Ok(())
    }

    async fn delegation(&self, id: Uuid) -> Result<Option<Delegation>, RepositoryError> {
        Ok(self.state.lock().delegations.iter().find(|given| given.id == id).cloned())
    }

    async fn revoke_delegation(&self, plugin: &str, id: Uuid) -> Result<bool, RepositoryError> {
        let mut state = self.state.lock();
        let live = state
            .delegations
            .iter_mut()
            .find(|given| given.id == id && given.plugin == plugin && given.revoked_at.is_none());
        Ok(live.map(|given| given.revoked_at = Some(Utc::now())).is_some())
    }

    async fn state_get(
        &self,
        plugin: &str,
        key: &str,
    ) -> Result<Option<serde_json::Value>, RepositoryError> {
        Ok(self.state.lock().stored.get(&(plugin.to_string(), key.to_string())).cloned())
    }

    async fn state_set(
        &self,
        plugin: &str,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<(), RepositoryError> {
        self.state.lock().stored.insert((plugin.to_string(), key.to_string()), value.clone());
        Ok(())
    }

    async fn state_delete(&self, plugin: &str, key: &str) -> Result<bool, RepositoryError> {
        Ok(self.state.lock().stored.remove(&(plugin.to_string(), key.to_string())).is_some())
    }

    async fn navigation(&self) -> Result<Option<serde_json::Value>, RepositoryError> {
        Ok(self.state.lock().navigation.clone())
    }

    async fn set_navigation(&self, layout: &serde_json::Value) -> Result<(), RepositoryError> {
        self.state.lock().navigation = Some(layout.clone());
        Ok(())
    }

    async fn settings(&self) -> Result<Option<serde_json::Value>, RepositoryError> {
        Ok(self.state.lock().settings.clone())
    }

    async fn set_settings(&self, settings: &serde_json::Value) -> Result<(), RepositoryError> {
        self.state.lock().settings = Some(settings.clone());
        Ok(())
    }

    async fn setup(&self) -> Result<Option<serde_json::Value>, RepositoryError> {
        Ok(self.state.lock().setup.clone())
    }

    async fn set_setup(&self, setup: &serde_json::Value) -> Result<(), RepositoryError> {
        self.state.lock().setup = Some(setup.clone());
        Ok(())
    }

    async fn plugin_settings(&self, plugin: &str) -> Result<Vec<StoredSetting>, RepositoryError> {
        let state = self.state.lock();
        Ok(state
            .plugin_settings
            .iter()
            .filter(|((held, _), _)| held == plugin)
            .map(|(_, setting)| setting.clone())
            .collect())
    }

    async fn set_plugin_settings(
        &self,
        plugin: &str,
        changes: &[SettingChange],
        by: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        for change in changes {
            let at = (plugin.to_string(), change.key().to_string());
            match change {
                SettingChange::Clear { .. } => {
                    state.plugin_settings.remove(&at);
                }
                SettingChange::Set { key, value } => {
                    state.plugin_settings.insert(
                        at,
                        StoredSetting {
                            key: key.clone(),
                            value: Some(value.clone()),
                            sealed: None,
                            updated_at: Utc::now(),
                            updated_by: by.map(str::to_string),
                        },
                    );
                }
                SettingChange::Seal { key, sealed } => {
                    state.plugin_settings.insert(
                        at,
                        StoredSetting {
                            key: key.clone(),
                            value: None,
                            sealed: Some(sealed.clone()),
                            updated_at: Utc::now(),
                            updated_by: by.map(str::to_string),
                        },
                    );
                }
            }
        }
        Ok(())
    }

    async fn plugin_secrets(&self) -> Result<Vec<(String, StoredSetting)>, RepositoryError> {
        let state = self.state.lock();
        Ok(state
            .plugin_settings
            .iter()
            .filter(|(_, setting)| setting.is_secret())
            .map(|((plugin, _), setting)| (plugin.clone(), setting.clone()))
            .collect())
    }

    async fn plugin_features(&self, plugin: &str) -> Result<Vec<StoredFeature>, RepositoryError> {
        let state = self.state.lock();
        Ok(state
            .plugin_features
            .iter()
            .filter(|((held, _), _)| held == plugin)
            .map(|(_, feature)| feature.clone())
            .collect())
    }

    async fn set_plugin_features(
        &self,
        plugin: &str,
        changes: &[(String, bool)],
        by: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        for (name, enabled) in changes {
            state.plugin_features.insert(
                (plugin.to_string(), name.clone()),
                StoredFeature {
                    name: name.clone(),
                    enabled: *enabled,
                    updated_at: Utc::now(),
                    updated_by: by.map(str::to_string),
                },
            );
        }
        Ok(())
    }

    async fn plugin_switches(&self) -> Result<Vec<StoredSwitch>, RepositoryError> {
        Ok(self.state.lock().plugin_switches.values().cloned().collect())
    }

    async fn set_plugin_switch(
        &self,
        plugin: &str,
        on: bool,
        by: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        match on {
            true => {
                state.plugin_switches.remove(plugin);
            }
            false => {
                let switch = StoredSwitch {
                    plugin: plugin.to_string(),
                    updated_at: Utc::now(),
                    updated_by: by.map(str::to_string),
                };
                state.plugin_switches.insert(plugin.to_string(), switch);
            }
        }
        Ok(())
    }

    async fn plugin_flags(&self) -> Result<Vec<StoredPluginFlag>, RepositoryError> {
        Ok(self.state.lock().plugin_flags.values().cloned().collect())
    }

    async fn set_plugin_flag(
        &self,
        plugin: &str,
        flag: Option<&str>,
        by: Option<&str>,
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        match flag {
            None => {
                state.plugin_flags.remove(plugin);
            }
            Some(flag) => {
                let held = StoredPluginFlag {
                    plugin: plugin.to_string(),
                    flag: flag.to_string(),
                    updated_at: Utc::now(),
                    updated_by: by.map(str::to_string),
                };
                state.plugin_flags.insert(plugin.to_string(), held);
            }
        }
        Ok(())
    }

    async fn access_requests(
        &self,
        target: &str,
    ) -> Result<Vec<AccessRequestRecord>, RepositoryError> {
        let mut found: Vec<AccessRequestRecord> = self
            .state
            .lock()
            .access_requests
            .iter()
            .filter(|request| request.target == target)
            .cloned()
            .collect();
        found.sort_by_key(|request| std::cmp::Reverse(request.created_at));
        Ok(found)
    }

    async fn access_request_for(
        &self,
        requester: &str,
        target: &str,
        setting: &str,
    ) -> Result<Option<AccessRequestRecord>, RepositoryError> {
        Ok(self
            .state
            .lock()
            .access_requests
            .iter()
            .find(|request| {
                request.requester == requester
                    && request.target == target
                    && request.setting == setting
            })
            .cloned())
    }

    async fn access_request(
        &self,
        id: Uuid,
    ) -> Result<Option<AccessRequestRecord>, RepositoryError> {
        Ok(self.state.lock().access_requests.iter().find(|request| request.id == id).cloned())
    }

    async fn put_access_request(
        &self,
        record: &AccessRequestRecord,
    ) -> Result<(), RepositoryError> {
        let mut state = self.state.lock();
        state.access_requests.retain(|request| request.id != record.id);
        state.access_requests.push(record.clone());
        Ok(())
    }
}

/// `core.plugin_status` in memory, with the same rule as Postgres for which change wins.
#[derive(Default)]
pub struct FakePluginStatus {
    rows: Mutex<std::collections::BTreeMap<String, PluginStatus>>,
    history: Mutex<Vec<(PluginChange, Source)>>,
}

impl FakePluginStatus {
    pub fn empty() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn history(&self) -> Vec<(PluginChange, Source)> {
        self.history.lock().clone()
    }
}

#[async_trait]
impl PluginStatuses for FakePluginStatus {
    async fn current(&self) -> Result<Vec<PluginStatus>, RepositoryError> {
        Ok(self.rows.lock().values().cloned().collect())
    }

    async fn record(&self, change: &PluginChange, source: Source) -> Result<(), RepositoryError> {
        let mut history = self.history.lock();
        let once = |(seen, _): &(PluginChange, Source)| {
            (&seen.plugin, seen.instance, seen.state, seen.at)
                == (&change.plugin, change.instance, change.state, change.at)
        };
        if !history.iter().any(once) {
            history.push((change.clone(), source));
        }
        let mut rows = self.rows.lock();
        let previous = rows.get(&change.plugin);
        if previous.is_some_and(|row| row.at > change.at) {
            return Ok(());
        }
        let (last_error, last_error_at) = match &change.error {
            Some(error) => (Some(error.clone()), Some(change.at)),
            None => {
                previous.map_or((None, None), |row| (row.last_error.clone(), row.last_error_at))
            }
        };
        let row = PluginStatus {
            plugin: change.plugin.clone(),
            version: change.version.clone(),
            classification: change.classification,
            instance: change.instance,
            state: change.state,
            error: change.error.clone(),
            since: change.since,
            at: change.at,
            registered_at: change.registered_at,
            last_error,
            last_error_at,
            checked_at: Utc::now(),
        };
        rows.insert(change.plugin.clone(), row);
        Ok(())
    }

    async fn checked(&self, at: DateTime<Utc>) -> Result<(), RepositoryError> {
        for row in self.rows.lock().values_mut() {
            row.checked_at = at;
        }
        Ok(())
    }

    async fn history(
        &self,
        plugin: &str,
        limit: u32,
    ) -> Result<Vec<StatusChange>, RepositoryError> {
        let mut kept: Vec<StatusChange> = self
            .history
            .lock()
            .iter()
            .filter(|(change, _)| change.plugin == plugin)
            .map(|(change, source)| StatusChange {
                version: change.version.clone(),
                instance: change.instance,
                state: change
                    .state
                    .map_or("removed", doc_plugin_protocol::PluginState::as_str)
                    .into(),
                error: change.error.clone(),
                at: change.at,
                source: source.as_str().into(),
            })
            .collect();
        kept.sort_by_key(|change| std::cmp::Reverse(change.at));
        kept.truncate(limit as usize);
        Ok(kept)
    }

    async fn changes(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<(String, StatusChange)>, RepositoryError> {
        let mut kept: Vec<(String, StatusChange)> = self
            .history
            .lock()
            .iter()
            .filter(|(change, _)| from <= change.at && change.at < to)
            .map(|(change, source)| {
                let state =
                    change.state.map_or("removed", doc_plugin_protocol::PluginState::as_str);
                (
                    change.plugin.clone(),
                    StatusChange {
                        version: change.version.clone(),
                        instance: change.instance,
                        state: state.into(),
                        error: change.error.clone(),
                        at: change.at,
                        source: source.as_str().into(),
                    },
                )
            })
            .collect();
        kept.sort_by_key(|(_, change)| change.at);
        Ok(kept)
    }

    async fn prune(&self, before: DateTime<Utc>) -> Result<u64, RepositoryError> {
        let mut history = self.history.lock();
        let kept = history.len();
        history.retain(|(change, _)| change.at >= before);
        Ok((kept - history.len()) as u64)
    }
}
