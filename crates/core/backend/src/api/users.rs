//! People and the accounts they sign in with (ADR-0005). Identity managers, who hold
//! `plugin:rbac:user:rw` as for service accounts, make users before they first sign in, link and
//! unlink their accounts, and merge a user who never signed in into one who has. Everyone sees their
//! own accounts, and links more by signing in to each provider from their account page.

use std::collections::HashMap;
use std::time::Duration;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use doc_cachebus::Namespace;
use doc_plugin_protocol::calls::{LINK_ROUTE, LinkStart, LinkStarted};
use doc_secret::Secret;
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::auth::identity_providers;
use super::iam::{announce, manages_identity};
use super::problem::Problem;
use crate::auth::{Auth, forget_all};
use crate::identity::{Account, AuditEntry, Identity, NewUser, Principal, User, token_hash};
use crate::permissions;
use crate::plugins::routes::ask_internal;
use crate::teams::Organisation;

const LINKS: &str = "core.links";
/// Long enough to sign in to a provider, short enough that a stray ticket soon goes.
const LINK_TTL: Duration = Duration::from_secs(600);
const MAX_LOGIN: usize = 64;
const MAX_TEXT: usize = 256;

/// A user with the organisation they belong to, every account linked to them, oldest first, and
/// the teams they are in.
#[derive(Debug, Serialize)]
pub struct UserView {
    #[serde(flatten)]
    pub user: User,
    pub organisation: Option<Organisation>,
    pub identities: Vec<Identity>,
    pub teams: Vec<TeamOf>,
}

/// A team someone is in, and how they came to be.
#[derive(Debug, Serialize)]
pub struct TeamOf {
    pub id: Uuid,
    pub name: String,
    pub title: String,
    pub source: String,
}

/// An account as an admin gives it: its provider, the provider's ID for it, and its login if that
/// differs from the ID.
#[derive(Debug, Deserialize)]
pub struct AccountBody {
    pub provider: String,
    pub external_id: String,
    #[serde(default)]
    pub login: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct NewUserBody {
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub account: Option<AccountBody>,
    /// The organisation they belong to; without it, the one their account's provider signs in for,
    /// or the only one there is.
    #[serde(default)]
    pub organisation: Option<Uuid>,
}

/// Whether someone is disabled, and the organisation they belong to.
#[derive(Debug, Default, Deserialize)]
pub struct UserPatch {
    #[serde(default)]
    pub disabled: Option<bool>,
    #[serde(default)]
    pub organisation: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct MergeBody {
    pub from: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct LinkBody {
    pub provider: String,
    #[serde(default)]
    pub return_to: Option<String>,
}

/// What a link ticket stands for: linking an account with `provider` to `user`.
#[derive(Debug, Serialize, Deserialize)]
pub struct Ticket {
    pub user: Uuid,
    pub provider: String,
}

/// What linking did: the identity, now the user's, and a user it brought in by merging.
pub struct Linked {
    pub identity: Identity,
    pub merged: Option<User>,
    /// Whether anything changed, which is not so when the account was the user's already.
    pub new: bool,
}

async fn manager(state: &AppState, principal: &Principal) -> Result<(), Problem> {
    match manages_identity(state, principal).await {
        true => Ok(()),
        false => Err(Problem::forbidden("needs plugin:rbac:user:rw")),
    }
}

/// `plugin:rbac:user:rw` reaches every user, but must not reach further than whoever holds it: an
/// identity manager who is not themselves a platform admin may not disable or move one. A platform
/// admin, who already passes `manager` on `is_admin` alone, always passes this too.
async fn protect_admin(state: &AppState, by: &Principal, target: &User) -> Result<(), Problem> {
    if !permissions::is_admin_of(state, &Principal::User(target.clone())).await {
        return Ok(());
    }
    if permissions::is_admin_of(state, by).await {
        return Ok(());
    }
    Err(Problem::forbidden(format!(
        "{} is a platform admin; only another admin can do that to them",
        target.login
    )))
}

fn me(principal: &Principal) -> Result<&User, Problem> {
    principal.as_user().ok_or_else(|| Problem::forbidden("only a person has linked accounts"))
}

async fn user(state: &AppState, id: Uuid) -> Result<User, Problem> {
    state.repos.identity.user_by_id(id).await?.ok_or_else(|| Problem::not_found("user"))
}

async fn view(state: &AppState, user: User) -> Result<UserView, Problem> {
    let identities = state.repos.identity.identities(Some(user.id)).await?;
    let memberships = state.repos.teams.memberships(user.id).await?;
    let mut teams = Vec::with_capacity(memberships.len());
    if !memberships.is_empty() {
        let known = state.repos.teams.teams().await?;
        for held in memberships {
            if let Some(team) = known.iter().find(|team| team.id == held.team_id) {
                let (name, title) = (team.name.clone(), team.title.clone());
                teams.push(TeamOf { id: team.id, name, title, source: held.source });
            }
        }
    }
    let organisation = state.repos.teams.organisation(user.organisation_id).await?;
    Ok(UserView { user, organisation, identities, teams })
}

/// A login or an account's ID: something to type, with no spaces.
fn handle(value: &str, what: &str, max: usize) -> Result<String, Problem> {
    let value = value.trim();
    let valid = !value.is_empty()
        && value.chars().count() <= max
        && !value.chars().any(|c| c.is_whitespace() || c.is_control());
    match valid {
        true => Ok(value.to_string()),
        false => {
            Err(Problem::bad_request(format!("{what} is 1 to {max} characters, with no spaces")))
        }
    }
}

fn optional(value: Option<String>, what: &str) -> Result<Option<String>, Problem> {
    let Some(value) = value.map(|value| value.trim().to_string()).filter(|value| !value.is_empty())
    else {
        return Ok(None);
    };
    if value.chars().count() > MAX_TEXT || value.chars().any(char::is_control) {
        return Err(Problem::bad_request(format!("{what} is at most {MAX_TEXT} characters")));
    }
    Ok(Some(value))
}

/// Any plugin the platform knows, running or not, so accounts can be given before their provider
/// is started.
async fn known_provider(state: &AppState, provider: &str) -> Result<bool, Problem> {
    let configured = state.config.plugins.ids.iter().any(|id| id == provider);
    if configured || state.plugins.get(provider).await.is_some() {
        return Ok(true);
    }
    Ok(state.repos.identity.list_plugins().await?.iter().any(|known| known == provider))
}

/// Where someone belongs when nobody said: with the organisation that signs in with their account's
/// provider, or in the only organisation there is.
async fn organisation_for(state: &AppState, account: Option<&Account>) -> Result<Uuid, Problem> {
    if let Some(account) = account {
        let chosen = state.repos.teams.providers().await?;
        if let Some((_, organisation)) =
            chosen.into_iter().find(|(provider, _)| provider == &account.provider)
        {
            return Ok(organisation);
        }
    }
    match state.repos.teams.organisations().await?.as_slice() {
        [only] => Ok(only.id),
        _ => Err(Problem::bad_request("choose the organisation they belong to")),
    }
}

async fn account_of(state: &AppState, body: AccountBody) -> Result<Account, Problem> {
    let provider = body.provider.trim().to_string();
    if !known_provider(state, &provider).await? {
        return Err(Problem::bad_request(format!("there is no provider called {provider}")));
    }
    let external_id = handle(&body.external_id, "an account's ID", MAX_TEXT)?;
    let login = match body.login.as_deref().map(str::trim).filter(|login| !login.is_empty()) {
        Some(login) => handle(login, "a login", MAX_LOGIN)?,
        None => external_id.clone(),
    };
    Ok(Account { provider, external_id, login })
}

/// Only a path on this site, so a link cannot finish on someone else's page.
fn local_path(path: &str) -> bool {
    path.starts_with('/') && !path.starts_with("//") && !path.contains('\\')
}

pub async fn list(
    State(state): State<AppState>,
    auth: Auth,
) -> Result<Json<Vec<UserView>>, Problem> {
    manager(&state, &auth.0).await?;
    let mut held: HashMap<Uuid, Vec<Identity>> = HashMap::new();
    for identity in state.repos.identity.identities(None).await? {
        held.entry(identity.user_id).or_default().push(identity);
    }
    let organisations = state.repos.teams.organisations().await?;
    let users = state.repos.identity.list_users().await?;
    Ok(Json(
        users
            .into_iter()
            .map(|listed| UserView {
                identities: held.remove(&listed.user.id).unwrap_or_default(),
                organisation: organisations
                    .iter()
                    .find(|organisation| organisation.id == listed.user.organisation_id)
                    .cloned(),
                user: listed.user,
                teams: Vec::new(),
            })
            .collect(),
    ))
}

/// A user made before the person signs in, so they can be given access ahead of time. With an
/// account, their first sign-in with it finds everything they were given.
pub async fn create(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<NewUserBody>,
) -> Result<impl IntoResponse, Problem> {
    manager(&state, &auth.0).await?;
    let account = match body.account {
        Some(account) => Some(account_of(&state, account).await?),
        None => None,
    };
    let organisation = match body.organisation {
        Some(id) => {
            let found = state.repos.teams.organisation(id).await?;
            found.ok_or_else(|| Problem::bad_request("there is no such organisation"))?.id
        }
        None => organisation_for(&state, account.as_ref()).await?,
    };
    let new = NewUser {
        login: handle(&body.login, "a login", MAX_LOGIN)?,
        organisation_id: organisation,
        name: optional(body.name, "a name")?,
        email: optional(body.email, "an email address")?,
    };
    if let Some(account) = &account
        && state.repos.identity.identity(&account.provider, &account.external_id).await?.is_some()
    {
        return Err(Problem::conflict(format!(
            "that {} account is linked to someone already",
            account.provider
        )));
    }
    let user = state.repos.identity.create_user(&new, account.as_ref()).await?;
    let detail = json!({ "login": user.login, "account": account.as_ref().map(|a| &a.provider) });
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("iam.user.created")
                .by(&auth.0)
                .subject(user.id.to_string())
                .detail(detail),
        )
        .await;
    announce(&state, "user.created", json!({ "id": user.id, "login": user.login })).await;
    Ok((StatusCode::CREATED, Json(view(&state, user).await?)))
}

/// Anyone's for an identity manager; otherwise only your own.
pub async fn show(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<UserView>, Problem> {
    if !auth.0.as_user().is_some_and(|me| me.id == id) {
        manager(&state, &auth.0).await?;
    }
    Ok(Json(view(&state, user(&state, id).await?).await?))
}

/// Disables or enables someone, or moves them to another organisation: out of their old one's
/// teams and into the new one's default teams.
pub async fn update(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<UserPatch>,
) -> Result<Json<UserView>, Problem> {
    manager(&state, &auth.0).await?;
    if body.disabled.is_none() && body.organisation.is_none() {
        return Err(Problem::bad_request(
            "change whether they are disabled, or their organisation",
        ));
    }
    let person = user(&state, id).await?;
    protect_admin(&state, &auth.0, &person).await?;
    if let Some(disabled) = body.disabled {
        if auth.0.as_user().is_some_and(|me| me.id == id) {
            return Err(Problem::conflict("an account cannot disable itself"));
        }
        let changed = state
            .repos
            .identity
            .set_user_disabled(id, disabled)
            .await?
            .ok_or_else(|| Problem::not_found("user"))?;
        forget_all(&state).await;
        let action = if disabled { "disabled" } else { "enabled" };
        let entry =
            AuditEntry::new(format!("iam.user.{action}")).by(&auth.0).subject(id.to_string());
        let _ = state.repos.identity.record_audit(entry).await;
        let event = json!({ "id": id, "login": changed.login });
        announce(&state, &format!("user.{action}"), event).await;
    }
    if let Some(organisation) = body.organisation
        && person.organisation_id != organisation
    {
        let target = state
            .repos
            .teams
            .organisation(organisation)
            .await?
            .ok_or_else(|| Problem::not_found("organisation"))?;
        state
            .repos
            .identity
            .move_user(id, organisation)
            .await?
            .ok_or_else(|| Problem::not_found("user"))?;
        forget_all(&state).await;
        let entry = AuditEntry::new("iam.user.moved")
            .by(&auth.0)
            .subject(id.to_string())
            .detail(json!({ "organisation": target.name }));
        let _ = state.repos.identity.record_audit(entry).await;
        announce(&state, "user.moved", json!({ "id": id, "organisation": organisation })).await;
    }
    Ok(Json(view(&state, user(&state, id).await?).await?))
}

/// An admin's decision to link an account, which is the only way besides the person signing in
/// to it that an existing user gains one (ADR-0005).
pub async fn attach(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<AccountBody>,
) -> Result<impl IntoResponse, Problem> {
    manager(&state, &auth.0).await?;
    let target = user(&state, id).await?;
    let account = account_of(&state, body).await?;
    let linked = link_account(&state, &target, &account, "admin", &auth.0).await?;
    let status = if linked.new { StatusCode::CREATED } else { StatusCode::OK };
    let merged = linked.merged.map(|merged| merged.id);
    Ok((status, Json(json!({ "identity": linked.identity, "merged": merged }))))
}

/// An admin may take away any account, even the last; the person keeps any session they hold.
pub async fn detach(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, identity)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, Problem> {
    manager(&state, &auth.0).await?;
    unlink(&state, id, identity, &auth.0).await
}

pub async fn merge(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<MergeBody>,
) -> Result<Json<UserView>, Problem> {
    manager(&state, &auth.0).await?;
    let into = user(&state, id).await?;
    let from = user(&state, body.from).await?;
    merge_into(&state, &from, &into, &auth.0).await?;
    Ok(Json(view(&state, user(&state, id).await?).await?))
}

/// Your accounts, and every identity provider running now, which your account page offers to link.
pub async fn mine(State(state): State<AppState>, auth: Auth) -> Result<Json<Value>, Problem> {
    let me = me(&auth.0)?;
    let identities = state.repos.identity.identities(Some(me.id)).await?;
    let providers = identity_providers(&state).await;
    Ok(Json(json!({ "identities": identities, "providers": providers })))
}

/// Starts linking an account with `provider`: core gives the provider a ticket for you, and you
/// are sent to sign in there. The ticket never leaves core and the provider.
pub async fn start_link(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<LinkBody>,
) -> Result<impl IntoResponse, Problem> {
    let me = me(&auth.0)?;
    let provider = body.provider.trim();
    if !identity_providers(&state).await.iter().any(|running| running.id == provider) {
        return Err(Problem::not_found("identity provider"));
    }
    // The principal may have come from the cache, from before an account was linked.
    if user(&state, me.id).await?.linked.contains_key(provider) {
        return Err(Problem::conflict(format!("you have a {provider} account linked already")));
    }
    let ticket = issue_ticket(&state, me.id, provider).await?;
    let return_to = body.return_to.filter(|path| local_path(path));
    let start = serde_json::to_value(LinkStart { ticket, return_to })
        .map_err(|err| Problem::internal(err.to_string()))?;
    let answer = ask_internal(&state, provider, LINK_ROUTE, &start).await.map_err(|err| {
        Problem::unavailable(format!("{provider} could not start linking: {err}"))
    })?;
    let started: LinkStarted = serde_json::from_value(answer).map_err(|err| {
        Problem::unavailable(format!("{provider} did not say where to sign in: {err}"))
    })?;
    if !(started.location.starts_with("https://") || started.location.starts_with("http://")) {
        return Err(Problem::unavailable(format!("{provider} did not say where to sign in")));
    }
    Ok((StatusCode::CREATED, Json(json!({ "location": started.location }))))
}

/// Unlinks one of your accounts, as long as another is left to sign in with.
pub async fn unlink_mine(
    State(state): State<AppState>,
    auth: Auth,
    Path(identity): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    let me = me(&auth.0)?;
    let held = state.repos.identity.identities(Some(me.id)).await?;
    if !held.iter().any(|mine| mine.id == identity) {
        return Err(Problem::not_found("linked account"));
    }
    if held.len() < 2 {
        return Err(Problem::conflict("it is the only account you can sign in with"));
    }
    unlink(&state, me.id, identity, &auth.0).await
}

async fn unlink(
    state: &AppState,
    user: Uuid,
    identity: Uuid,
    by: &Principal,
) -> Result<StatusCode, Problem> {
    let removed = state
        .repos
        .identity
        .detach_identity(user, identity)
        .await?
        .ok_or_else(|| Problem::not_found("linked account"))?;
    // Cached principals carry their linked accounts.
    forget_all(state).await;
    let detail = json!({ "provider": removed.provider, "login": removed.login });
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("iam.user.identity.unlinked")
                .by(by)
                .subject(user.to_string())
                .detail(detail.clone()),
        )
        .await;
    let mut event = detail;
    event["id"] = json!(user);
    announce(state, "user.identity.unlinked", event).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn issue_ticket(
    state: &AppState,
    user: Uuid,
    provider: &str,
) -> Result<Secret<String>, Problem> {
    let ticket = crate::secrets::link_ticket()
        .map_err(|err| Problem::internal(format!("issuing a link ticket: {err}")))?;
    let namespace = Namespace::new(LINKS).map_err(|err| Problem::internal(err.to_string()))?;
    let stands_for = json!(Ticket { user, provider: provider.to_string() });
    state
        .buses
        .cache
        .set(&namespace, &hex::encode(token_hash(ticket.expose())), stands_for, Some(LINK_TTL))
        .await
        .map_err(|err| Problem::unavailable(format!("the link could not be started: {err}")))?;
    Ok(ticket)
}

/// What a ticket stands for, once: redeeming it takes it, so a second use finds nothing.
pub async fn redeem_ticket(state: &AppState, ticket: &str) -> Result<Option<Ticket>, String> {
    let namespace = Namespace::new(LINKS).map_err(|err| err.to_string())?;
    let key = hex::encode(token_hash(ticket));
    let Some(entry) = state.buses.cache.get(&namespace, &key).await.map_err(|e| e.to_string())?
    else {
        return Ok(None);
    };
    if !state.buses.cache.delete(&namespace, &key).await.map_err(|err| err.to_string())? {
        return Ok(None);
    }
    Ok(serde_json::from_value(entry.value).ok())
}

/// Links an account to `user`. An account of someone who has never signed in brings its whole
/// record over by merging it; one that anyone else has signed in with is refused, for an admin to
/// decide. An account that is the user's already changes nothing.
pub async fn link_account(
    state: &AppState,
    user: &User,
    account: &Account,
    source: &str,
    by: &Principal,
) -> Result<Linked, Problem> {
    let identities = &state.repos.identity;
    if let Some(held) = identities.identity(&account.provider, &account.external_id).await? {
        if held.user_id == user.id {
            return Ok(Linked { identity: held, merged: None, new: false });
        }
        let holder = identities
            .user_by_id(held.user_id)
            .await?
            .ok_or_else(|| Problem::conflict("that account's user has just gone"))?;
        if holder.first_signed_in_at.is_some() {
            return Err(Problem::conflict(format!(
                "that {} account belongs to {}, who has signed in with it, so an admin decides \
                 between them",
                account.provider, holder.login
            )));
        }
        let merged = merge_into(state, &holder, user, by).await?;
        let identity =
            identities.identity(&account.provider, &account.external_id).await?.ok_or_else(
                || Problem::conflict("that account was unlinked while it was being linked"),
            )?;
        return Ok(Linked { identity, merged: Some(merged), new: true });
    }
    if let Some(login) = user_linked(state, user.id, &account.provider).await? {
        return Err(Problem::conflict(format!(
            "{} has a {} account linked already, {login}, which has to be unlinked first",
            user.login, account.provider
        )));
    }
    let identity = identities.attach_identity(user.id, account, source).await?;
    forget_all(state).await;
    let detail = json!({ "provider": account.provider, "login": account.login, "source": source });
    let _ = identities
        .record_audit(
            AuditEntry::new("iam.user.identity.linked")
                .by(by)
                .subject(user.id.to_string())
                .detail(detail),
        )
        .await;
    let event = json!({ "id": user.id, "provider": account.provider, "login": account.login });
    announce(state, "user.identity.linked", event).await;
    Ok(Linked { identity, merged: None, new: true })
}

/// The login of the user's account with `provider`, read fresh rather than from a cached principal.
async fn user_linked(
    state: &AppState,
    user: Uuid,
    provider: &str,
) -> Result<Option<String>, Problem> {
    let held = state.repos.identity.identities(Some(user)).await?;
    Ok(held.into_iter().find(|identity| identity.provider == provider).map(|found| found.login))
}

/// Moves everything `from` has to `into`, then removes `from`. Only a user who has never signed in
/// can be merged away. Plugins move what they hold for them on `platform.iam.user.merged`.
pub async fn merge_into(
    state: &AppState,
    from: &User,
    into: &User,
    by: &Principal,
) -> Result<User, Problem> {
    if from.first_signed_in_at.is_some() {
        return Err(Problem::conflict(format!(
            "{} has signed in, so their record cannot be merged into another",
            from.login
        )));
    }
    let merged = state.repos.identity.merge_users(from.id, into.id).await?;
    forget_all(state).await;
    let _ = state
        .repos
        .identity
        .record_audit(
            AuditEntry::new("iam.user.merged")
                .by(by)
                .subject(into.id.to_string())
                .detail(json!({ "from": from.id, "login": merged.login })),
        )
        .await;
    let event = json!({ "from": from.id, "into": into.id, "login": merged.login });
    announce(state, "user.merged", event).await;
    Ok(merged)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use async_trait::async_trait;
    use doc_eventbus::{ConsumerGroup, TopicFilter};
    use doc_permissions::Grants;
    use doc_plugin_protocol::calls::{IdentityRequest, LinkRequest, UserRequest};
    use doc_plugin_protocol::{Capability, Manifest, PluginState, RegisterRequest};

    use super::*;
    use crate::api::router;
    use crate::config::Config;
    use crate::db::repositories::IdentityRepository;
    use crate::identity::TokenOwner;
    use crate::permissions::PermissionSource;
    use crate::plugins::client::Answer;
    use crate::plugins::{self, api as plugin_api};
    use crate::secrets::TokenKind;
    use crate::testing::{ADMIN, Host, delete_as, get_as, patch_json, plugin_host_with, post_json};

    /// Grants each named principal whatever permissions they are given here, so a test can make an
    /// identity manager who is not an admin, an admin who holds nothing else, and so on.
    struct Source(BTreeMap<String, Vec<String>>);

    #[async_trait]
    impl PermissionSource for Source {
        async fn grants(&self, principal: &Principal, _teams: &[Uuid]) -> Result<Grants, String> {
            let held = self.0.get(&principal.label()).cloned().unwrap_or_default();
            serde_json::from_value(json!({ "permissions": held })).map_err(|err| err.to_string())
        }
    }

    const ADA: &str = "doc_ses_ada";
    const LOCATION: &str = "https://github.example/login/oauth/authorize?state=s";

    /// `tester` is a platform admin, and so an identity manager; `ada` is anyone else.
    fn host() -> (Host, User) {
        let mut config = Config::default();
        config.bootstrap.admins = vec!["tester".into()];
        config.plugins.ids = vec!["hello".into(), "github".into(), "ghe".into(), "local".into()];
        for provider in ["github", "ghe"] {
            config.plugins.capabilities.insert(provider.into(), vec![Capability::IdentityProvider]);
        }
        let host = plugin_host_with(config);
        let ada = host.identity.add_user("ada");
        host.identity.give(ADA, TokenKind::Session, TokenOwner::User(ada.id), None, false);
        (host, ada)
    }

    async fn running(host: &Host, manifest: Manifest) {
        let id = manifest.id.clone();
        let request = RegisterRequest {
            manifest: Manifest { version: "1.0.0".into(), ..manifest },
            address: format!("plugin-{id}:4440"),
            binary_sha256: "a".repeat(64),
            started_at: None,
        };
        let principal = Principal::Plugin { id: id.clone() };
        plugins::register(&host.state, &principal, request).await.expect("registered");
        assert_eq!(host.settle(&id).await, Some(PluginState::Running));
    }

    async fn providers(host: &Host) {
        for id in ["github", "ghe"] {
            crate::testing::identity_provider(host, id).await;
        }
    }

    fn octocat(external_id: &str, login: &str) -> IdentityRequest {
        IdentityRequest {
            provider: "github".into(),
            external_id: external_id.into(),
            login: login.into(),
            ..IdentityRequest::default()
        }
    }

    fn account(provider: &str, external_id: &str, login: &str) -> Account {
        Account { provider: provider.into(), external_id: external_id.into(), login: login.into() }
    }

    /// Starts linking an account with `provider` for whoever `token` is, and returns the ticket
    /// the provider was handed.
    async fn start(host: &Host, token: &str, provider: &str) -> String {
        host.plugin.answer_with(Answer::json(StatusCode::OK, &json!({ "location": LOCATION })));
        let asked = json!({ "provider": provider, "return_to": "/account" });
        let (status, body, _) = post_json(&host.app, "/api/v1/me/links", Some(token), asked).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["location"], LOCATION);
        assert!(body.get("ticket").is_none(), "the ticket goes to the provider alone");
        let (forwarded, caller) = host.plugin.requests().pop().expect("the provider was asked");
        assert_eq!(forwarded.path, "internal/link");
        assert_eq!(caller.kind, "platform");
        let start: Value = serde_json::from_slice(&forwarded.body).expect("json");
        assert_eq!(start["return_to"], "/account");
        start["ticket"].as_str().expect("a ticket").to_string()
    }

    fn linking(ticket: &str, provider: &str, external_id: &str, login: &str) -> LinkRequest {
        LinkRequest {
            ticket: Secret::new(ticket.to_string()),
            provider: provider.into(),
            external_id: external_id.into(),
            login: login.into(),
            name: None,
            email: None,
        }
    }

    #[tokio::test]
    async fn a_user_made_before_they_sign_in_is_the_one_they_sign_in_as() {
        let (host, _) = host();
        providers(&host).await;
        let made = json!({ "login": "dev", "name": "Dev Eloper",
                           "account": { "provider": "github", "external_id": "7", "login": "dev" } });
        let (status, body, _) = post_json(&host.app, "/api/v1/users", Some(ADMIN), made).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        let id = body["id"].as_str().expect("an id").to_string();
        assert_eq!(body["first_signed_in_at"], Value::Null);
        assert_eq!(body["identities"][0]["source"], "admin");
        assert_eq!(body["organisation"]["name"], "default", "the organisation github signs in for");
        assert!(host.identity.audit_actions().contains(&"iam.user.created".to_string()));

        let signed_in = crate::testing::sign_in(&host, "github", "7", "dev").await;
        assert_eq!(
            signed_in.user_id.to_string(),
            id,
            "the same user, with whatever they were given"
        );
        assert!(signed_in.first);
        let user = host.identity.user_by_id(signed_in.user_id).await.unwrap().expect("known");
        assert_eq!(user.name.as_deref(), Some("Dev Eloper"));
    }

    #[tokio::test]
    async fn sign_in_goes_by_the_account_s_immutable_id_and_never_its_login() {
        let (host, _) = host();
        providers(&host).await;
        let first = plugin_api::identity(&host.state, "github", octocat("583231", "octocat"))
            .await
            .expect("signed in");
        assert!(first.first);

        let renamed = plugin_api::identity(&host.state, "github", octocat("583231", "octo-cat"))
            .await
            .expect("signed in");
        assert_eq!(renamed.user_id, first.user_id, "a renamed login is the same person");
        assert!(!renamed.first);
        let user = host.identity.user_by_id(first.user_id).await.unwrap().expect("known");
        assert_eq!(user.login, "octo-cat", "the user's login follows the account they came with");
        assert_eq!(user.linked.get("github").map(String::as_str), Some("octo-cat"));

        let reused = plugin_api::identity(&host.state, "github", octocat("999", "octocat"))
            .await
            .expect("signed in");
        assert_ne!(reused.user_id, first.user_id, "someone who takes up an old login is not them");
    }

    #[tokio::test]
    async fn only_identity_managers_make_and_list_users() {
        let (host, ada) = host();
        let made = json!({ "login": "newcomer" });
        let (status, _, _) = post_json(&host.app, "/api/v1/users", Some(ADA), made).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) = get_as(&host.app, "/api/v1/users", ADA).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, body, _) = get_as(&host.app, &format!("/api/v1/users/{}", ada.id), ADA).await;
        assert_eq!(status, StatusCode::OK, "anyone may see their own record");
        assert_eq!(body["identities"][0]["provider"], "local");
        let tester = host.identity.identity("local", "tester").await.unwrap().expect("known");
        let path = format!("/api/v1/users/{}", tester.user_id);
        let (status, _, _) = get_as(&host.app, &path, ADA).await;
        assert_eq!(status, StatusCode::FORBIDDEN);

        let (status, body, _) = get_as(&host.app, "/api/v1/users", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        let logins: Vec<&str> =
            body.as_array().unwrap().iter().filter_map(|user| user["login"].as_str()).collect();
        assert!(logins.contains(&"ada") && logins.contains(&"tester"), "{logins:?}");
    }

    /// `plugin:rbac:user:rw` reaches every user, but must not reach further than whoever holds it.
    #[tokio::test]
    async fn an_identity_manager_who_is_not_an_admin_may_not_touch_a_platform_admin() {
        let (mut host, _ada) = host();
        let bob = host.identity.add_user("bob");
        let tester =
            host.identity.identity("local", "tester").await.unwrap().expect("known").user_id;
        let held = BTreeMap::from([("ada".to_string(), vec!["plugin:rbac:user:rw".to_string()])]);
        host.state = host.state.clone().with_permissions(Arc::new(Source(held)));
        host.app = router(host.state.clone());

        // ada, an identity manager but not an admin, may see tester's page but not act on it
        let (status, _, _) = get_as(&host.app, &format!("/api/v1/users/{tester}"), ADA).await;
        assert_eq!(status, StatusCode::OK, "an identity manager may see anyone");
        let (status, body, _) = patch_json(
            &host.app,
            &format!("/api/v1/users/{tester}"),
            ADA,
            json!({ "disabled": true }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body["detail"].as_str().unwrap_or_default().contains("platform admin"), "{body}");
        let (status, body, _) = patch_json(
            &host.app,
            &format!("/api/v1/users/{tester}"),
            ADA,
            json!({ "organisation": Uuid::now_v7() }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "an admin cannot be moved either, {body}");

        // an ordinary user is unaffected: ada manages one exactly as an identity manager always has
        let path = format!("/api/v1/users/{}", bob.id);
        let (status, body, _) =
            patch_json(&host.app, &path, ADA, json!({ "disabled": true })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["disabled"], true);
    }

    #[tokio::test]
    async fn a_new_user_needs_a_login_a_known_provider_and_an_account_nobody_has() {
        let (host, _) = host();
        for (made, expected) in [
            (json!({ "login": "two words" }), StatusCode::BAD_REQUEST),
            (json!({ "login": "" }), StatusCode::BAD_REQUEST),
            (
                json!({ "login": "x", "account": { "provider": "nowhere", "external_id": "1" } }),
                StatusCode::BAD_REQUEST,
            ),
            (
                json!({ "login": "x", "account": { "provider": "local", "external_id": "ada" } }),
                StatusCode::CONFLICT,
            ),
        ] {
            let (status, body, _) =
                post_json(&host.app, "/api/v1/users", Some(ADMIN), made.clone()).await;
            assert_eq!(status, expected, "{made}: {body}");
        }
        let made = json!({ "login": "early", "account": { "provider": "github", "external_id": "7",
                                                          "login": "early-bird" } });
        let (status, body, _) = post_json(&host.app, "/api/v1/users", Some(ADMIN), made).await;
        assert_eq!(status, StatusCode::CREATED, "a provider that is not running yet will do");
        assert_eq!(body["linked"]["github"], "early-bird");
    }

    #[tokio::test]
    async fn linking_passes_a_one_use_ticket_between_core_and_the_provider_alone() {
        let (host, ada) = host();
        providers(&host).await;
        let ticket = start(&host, ADA, "github").await;

        let wrong = plugin_api::link(&host.state, "ghe", linking(&ticket, "ghe", "5", "ada"))
            .await
            .expect_err("a ticket is for one provider");
        assert_eq!(wrong.status, 403);

        let ticket = start(&host, ADA, "github").await;
        let linked =
            plugin_api::link(&host.state, "github", linking(&ticket, "github", "5", "ada"))
                .await
                .expect("linked");
        assert_eq!(linked.user_id, ada.id);
        assert_eq!(linked.merged, None);
        let again = plugin_api::link(&host.state, "github", linking(&ticket, "github", "5", "ada"))
            .await
            .expect_err("a ticket is used once");
        assert_eq!(again.kind, "wrong-ticket");

        let (status, body, _) = get_as(&host.app, "/api/v1/me/identities", ADA).await;
        assert_eq!(status, StatusCode::OK);
        let providers: Vec<&str> = body["identities"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|identity| identity["provider"].as_str())
            .collect();
        assert_eq!(providers, ["local", "github"], "still one user, with both accounts");
        let offered: Vec<&str> =
            body["providers"].as_array().unwrap().iter().filter_map(|p| p["id"].as_str()).collect();
        assert_eq!(offered, ["ghe", "github"]);
        let asked = json!({ "provider": "github" });
        let (status, _, _) = post_json(&host.app, "/api/v1/me/links", Some(ADA), asked).await;
        assert_eq!(status, StatusCode::CONFLICT, "one account per provider");
        let asked = json!({ "provider": "hello" });
        let (status, _, _) = post_json(&host.app, "/api/v1/me/links", Some(ADA), asked).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "only a running identity provider links");
    }

    #[tokio::test]
    async fn linking_the_account_of_someone_who_never_signed_in_brings_their_record_over() {
        let (host, ada) = host();
        providers(&host).await;
        let early = host
            .identity
            .create_user(
                &NewUser {
                    login: "ada-early".into(),
                    organisation_id: host.identity.organisation(),
                    name: Some("Ada".into()),
                    email: None,
                },
                Some(&account("github", "5", "ada")),
            )
            .await
            .expect("made");
        let owner = crate::teams::AccountOwner::User(early.id);
        let owned = host.identity.create_service_account("deployer", None, owner).await;
        let owned = owned.expect("made");
        let filter = TopicFilter::new("platform.iam.user.merged").unwrap();
        let mut merged = host
            .state
            .buses
            .events
            .subscribe(ConsumerGroup::new("test.merged", filter))
            .await
            .unwrap();

        let ticket = start(&host, ADA, "github").await;
        let linked =
            plugin_api::link(&host.state, "github", linking(&ticket, "github", "5", "ada"))
                .await
                .expect("linked");
        assert_eq!(linked.merged, Some(early.id));
        assert!(host.identity.user_by_id(early.id).await.unwrap().is_none(), "the record goes");
        let ada_now = host.identity.user_by_id(ada.id).await.unwrap().expect("still here");
        assert_eq!(ada_now.linked.get("github").map(String::as_str), Some("ada"));
        assert_eq!(ada_now.name.as_deref(), Some("Ada"), "a gap in the profile is filled");
        let account = host.identity.service_account_by_id(owned.id).await.unwrap().expect("kept");
        assert_eq!(account.owner_id, Some(ada.id), "what they owned comes over");

        let event = merged.next().await.expect("announced").event.payload;
        assert_eq!(event["from"], early.id.to_string());
        assert_eq!(event["into"], ada.id.to_string(), "plugins move what they hold on this");
        assert!(host.identity.audit_actions().contains(&"iam.user.merged".to_string()));
    }

    #[tokio::test]
    async fn an_account_someone_has_signed_in_with_is_never_taken_over() {
        let (host, ada) = host();
        providers(&host).await;
        plugin_api::identity(&host.state, "github", octocat("583231", "octocat"))
            .await
            .expect("octocat signed in");

        let ticket = start(&host, ADA, "github").await;
        let refused = plugin_api::link(
            &host.state,
            "github",
            linking(&ticket, "github", "583231", "octocat"),
        )
        .await
        .expect_err("refused");
        assert_eq!(refused.status, 409, "{}", refused.detail);

        let attached = json!({ "provider": "github", "external_id": "583231" });
        let path = format!("/api/v1/users/{}/identities", ada.id);
        let (status, _, _) = post_json(&host.app, &path, Some(ADMIN), attached).await;
        assert_eq!(status, StatusCode::CONFLICT, "not even by an admin; they merge or unlink");
        assert!(
            !host.identity.user_by_id(ada.id).await.unwrap().unwrap().linked.contains_key("github")
        );
    }

    #[tokio::test]
    async fn an_admin_links_and_unlinks_accounts_one_per_provider() {
        let (host, ada) = host();
        let path = format!("/api/v1/users/{}/identities", ada.id);
        let attached = json!({ "provider": "github", "external_id": "1", "login": "ada" });
        let (status, body, _) = post_json(&host.app, &path, Some(ADMIN), attached.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["identity"]["source"], "admin");
        let (status, _, _) = post_json(&host.app, &path, Some(ADMIN), attached).await;
        assert_eq!(status, StatusCode::OK, "linking it again changes nothing");
        let other = json!({ "provider": "github", "external_id": "2" });
        let (status, _, _) = post_json(&host.app, &path, Some(ADMIN), other).await;
        assert_eq!(status, StatusCode::CONFLICT, "one account per provider");
        let (status, _, _) = post_json(
            &host.app,
            &path,
            Some(ADA),
            json!({ "provider": "ghe", "external_id": "3" }),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "nobody links their own by saying so");

        let github = host.identity.identity("github", "1").await.unwrap().expect("linked");
        let (status, _, _) = delete_as(&host.app, &format!("{path}/{}", github.id), ADMIN).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        assert!(host.identity.identity("github", "1").await.unwrap().is_none());
        assert!(host.identity.audit_actions().contains(&"iam.user.identity.unlinked".to_string()));
    }

    #[tokio::test]
    async fn you_unlink_your_own_accounts_but_never_the_last() {
        let (host, ada) = host();
        let local = host.identity.identities_of(ada.id).pop().expect("local");
        let path = format!("/api/v1/me/identities/{}", local.id);
        let (status, body, _) = delete_as(&host.app, &path, ADA).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        host.identity
            .attach_identity(ada.id, &account("github", "1", "ada"), "link")
            .await
            .unwrap();
        let (status, _, _) = delete_as(&host.app, &path, ADA).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let left: Vec<String> =
            host.identity.identities_of(ada.id).into_iter().map(|held| held.provider).collect();
        assert_eq!(left, ["github"]);

        let tester = host.identity.identity("local", "tester").await.unwrap().expect("known");
        let path = format!("/api/v1/me/identities/{}", tester.id);
        let (status, _, _) = delete_as(&host.app, &path, ADA).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "only your own");
    }

    #[tokio::test]
    async fn an_admin_merges_a_user_who_never_signed_in_but_not_one_who_has() {
        let (host, ada) = host();
        let early = host.identity.add_unsigned("ada-early");
        let path = format!("/api/v1/users/{}/merge", ada.id);
        let (status, body, _) =
            post_json(&host.app, &path, Some(ADA), json!({ "from": early.id })).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        let (status, body, _) =
            post_json(&host.app, &path, Some(ADMIN), json!({ "from": early.id })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["id"], ada.id.to_string());
        assert!(host.identity.user_by_id(early.id).await.unwrap().is_none());

        let tester = host.identity.identity("local", "tester").await.unwrap().expect("known");
        let (status, _, _) =
            post_json(&host.app, &path, Some(ADMIN), json!({ "from": tester.user_id })).await;
        assert_eq!(status, StatusCode::CONFLICT, "someone who has signed in is never merged away");
        let (status, _, _) =
            post_json(&host.app, &path, Some(ADMIN), json!({ "from": ada.id })).await;
        assert_eq!(status, StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn plugins_and_the_frontend_see_which_accounts_the_caller_has_linked() {
        let (host, ada) = host();
        providers(&host).await;
        let needs = Manifest {
            id: "hello".into(),
            linked_accounts: vec!["github".into()],
            ..Manifest::default()
        };
        running(&host, needs).await;
        host.identity
            .attach_identity(ada.id, &account("github", "1", "ada-gh"), "link")
            .await
            .unwrap();
        forget_all(&host.state).await;

        let (status, body, _) = get_as(&host.app, "/api/v1/me/access", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["plugins"]["hello"]["links"], json!(["github"]));
        assert_eq!(body["linked"], json!({ "local": "tester" }));

        host.plugin.answer_with(Answer::json(StatusCode::OK, &json!({})));
        let (status, _, _) = get_as(&host.app, "/api/v1/plugins/hello/api/anything", ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        let (_, caller) = host.plugin.requests().pop().expect("forwarded");
        assert_eq!(caller.linked.get("local").map(String::as_str), Some("tester"));

        let principal = crate::auth::authenticate(&host.state, ADA).await.expect("ada");
        let caller = plugins::runs::caller_of(&permissions::Authorised {
            principal,
            plugin: "hello".into(),
            access: doc_permissions::Access::Read,
            admin: false,
            scope: None,
            custom: Default::default(),
            attributes: Default::default(),
        });
        assert_eq!(caller.linked.get("github").map(String::as_str), Some("ada-gh"));
    }

    #[tokio::test]
    async fn a_provider_makes_users_for_its_own_accounts_before_they_sign_in() {
        let (host, _) = host();
        providers(&host).await;
        running(&host, Manifest { id: "hello".into(), ..Manifest::default() }).await;
        let described = |provider: &str| UserRequest {
            provider: provider.into(),
            external_id: "42".into(),
            login: "newcomer".into(),
            name: Some("New Comer".into()),
            ..UserRequest::default()
        };
        let made =
            plugin_api::users(&host.state, "github", described("github")).await.expect("made");
        assert!(made.created);
        let again =
            plugin_api::users(&host.state, "github", described("github")).await.expect("found");
        assert_eq!((again.user_id, again.created), (made.user_id, false));
        let user = host.identity.user_by_id(made.user_id).await.unwrap().expect("known");
        assert_eq!(user.first_signed_in_at, None, "made, not signed in");
        let held = host.identity.identity("github", "42").await.unwrap().expect("with its account");
        assert_eq!(held.source, "provider");

        let other = plugin_api::users(&host.state, "github", described("ghe")).await;
        assert_eq!(other.expect_err("only its own accounts").status, 403);
        let hello = plugin_api::users(&host.state, "hello", described("hello")).await;
        assert_eq!(hello.expect_err("only identity providers").status, 403);

        let signed_in = plugin_api::identity(&host.state, "github", octocat("42", "newcomer"))
            .await
            .expect("signed in");
        assert_eq!(signed_in.user_id, made.user_id, "their first sign-in finds them");
        assert!(signed_in.first);
    }

    #[tokio::test]
    async fn each_account_keeps_what_its_provider_last_reported_and_the_user_the_latest() {
        let (host, _) = host();
        providers(&host).await;
        let reporting = |email: Option<&str>, first_name: Option<&str>| IdentityRequest {
            provider: "github".into(),
            external_id: "583231".into(),
            login: "octocat".into(),
            name: Some("Mona Octocat".into()),
            email: email.map(str::to_string),
            first_name: first_name.map(str::to_string),
            surname: Some("Octocat".into()),
            ..IdentityRequest::default()
        };
        let first = plugin_api::identity(
            &host.state,
            "github",
            reporting(Some("mona@example.com"), Some("Mona")),
        )
        .await
        .expect("signed in");
        let held = host.identity.identity("github", "583231").await.unwrap().expect("known");
        assert_eq!(held.email.as_deref(), Some("mona@example.com"));
        assert_eq!(
            (held.first_name.as_deref(), held.surname.as_deref()),
            (Some("Mona"), Some("Octocat"))
        );
        assert!(held.reported_at.is_some());

        plugin_api::identity(&host.state, "github", reporting(Some("mona@octo.example"), None))
            .await
            .expect("signed in again");
        let user = host.identity.user_by_id(first.user_id).await.unwrap().expect("known");
        assert_eq!(user.email.as_deref(), Some("mona@octo.example"), "the latest report wins");
        assert_eq!(user.first_name.as_deref(), Some("Mona"), "what it left out stays");
        let (status, body, _) =
            get_as(&host.app, &format!("/api/v1/users/{}", first.user_id), ADMIN).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["identities"][0]["email"], "mona@octo.example");
        assert_eq!(body["first_name"], "Mona");
    }
}
