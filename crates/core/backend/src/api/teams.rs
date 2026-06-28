//! Organisations and teams (ADR-0004). Anyone signed in can see them. Identity managers, who hold
//! `plugin:rbac:user:rw` as for users and service accounts, change them. A team a plugin provides
//! is that plugin's to change, apart from its default mark and the people added to it in DOC.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use axum::response::IntoResponse;
use doc_eventbus::{Event, Topic};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::iam::{AccountView, manages_identity, reach_of};
use super::problem::Problem;
use crate::auth::Auth;
use crate::identity::{AuditEntry, Principal};
use crate::teams::{BY_HAND, BY_PROVIDER, NewTeam, Organisation, Team, TeamChanges, TeamMember};

const MAX_NAME: usize = 64;
const MAX_TITLE: usize = 100;
const MAX_DESCRIPTION: usize = 500;
const MAX_EMAIL: usize = 254;

#[derive(Debug, Deserialize)]
pub struct OrganisationBody {
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
}

#[derive(Debug, Default, Deserialize)]
pub struct OrganisationPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TeamBody {
    pub organisation: Uuid,
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub parent: Option<Uuid>,
    #[serde(default)]
    pub default: bool,
}

/// `parent` absent leaves it; `null` makes the team top-level.
#[derive(Debug, Default, Deserialize)]
pub struct TeamPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default, deserialize_with = "present")]
    pub parent: Option<Option<Uuid>>,
    #[serde(default)]
    pub default: Option<bool>,
}

/// A field that is present, even as `null`, is `Some`.
fn present<'de, D>(deserializer: D) -> Result<Option<Option<Uuid>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Uuid>::deserialize(deserializer).map(Some)
}

#[derive(Debug, Deserialize)]
pub struct MemberBody {
    pub user: Uuid,
}

/// The identity providers an organisation's people sign in with.
#[derive(Debug, Deserialize)]
pub struct ProvidersBody {
    pub providers: Vec<String>,
}

/// The email domains an organisation approves (FEAT-PEOPLE).
#[derive(Debug, Deserialize)]
pub struct DomainsBody {
    pub domains: Vec<String>,
}

/// An identity provider as an organisation's page offers it: what it is called, whether it is
/// running, and which organisation chose it, if any, since each serves one.
#[derive(Debug, Serialize)]
pub struct ProviderChoice {
    pub id: String,
    pub title: String,
    pub running: bool,
    pub organisation: Option<Uuid>,
}

#[derive(Debug, Serialize)]
pub struct OrganisationView {
    #[serde(flatten)]
    pub organisation: Organisation,
    pub teams: usize,
}

/// A team as lists show it, with its organisation's name, how many people are in it and who
/// leads it.
#[derive(Debug, Clone, Serialize)]
pub struct TeamView {
    #[serde(flatten)]
    pub team: Team,
    pub organisation: String,
    pub members: usize,
    pub lead: Option<LeadView>,
}

/// Who leads a team, by name, so an org chart needs no one call per team.
#[derive(Debug, Clone, Serialize)]
pub struct LeadView {
    pub login: String,
    pub name: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct MemberView {
    pub user_id: Uuid,
    pub login: String,
    pub name: Option<String>,
    pub source: String,
    pub provider: Option<String>,
    /// The name of the position they hold here.
    pub position: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A team with everything around it. Its service accounts are listed only for someone who manages
/// them.
#[derive(Debug, Serialize)]
pub struct TeamDetail {
    #[serde(flatten)]
    pub team: Team,
    pub organisation: Organisation,
    /// From the top of the organisation down to its parent.
    pub ancestors: Vec<Team>,
    pub sub_teams: Vec<Team>,
    pub members: Vec<MemberView>,
    pub service_accounts: Option<Vec<AccountView>>,
    /// The positions it has, as it inherits and changes them (FEAT-TEAMS).
    pub positions: Vec<super::positions::TeamPositionView>,
    /// Whether whoever asked may change its lead and positions.
    pub arranges: bool,
}

pub(super) async fn manager(state: &AppState, principal: &Principal) -> Result<(), Problem> {
    match manages_identity(state, principal).await {
        true => Ok(()),
        false => Err(Problem::forbidden("needs plugin:rbac:user:rw")),
    }
}

/// Lower-case letters, digits, `-`, `_` and `.`, starting with a letter or a digit.
pub fn slug(value: &str, what: &str) -> Result<String, Problem> {
    let value = value.trim();
    let mut chars = value.chars();
    let valid = value.len() <= MAX_NAME
        && chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c));
    match valid {
        true => Ok(value.to_string()),
        false => Err(Problem::bad_request(format!(
            "{what} is 1 to {MAX_NAME} of a-z, 0-9, `-`, `_` and `.`, starting with a letter or a digit"
        ))),
    }
}

pub fn title(value: &str) -> Result<String, Problem> {
    let value = value.trim();
    match !value.is_empty()
        && value.chars().count() <= MAX_TITLE
        && !value.contains(char::is_control)
    {
        true => Ok(value.to_string()),
        false => Err(Problem::bad_request(format!("a title is 1 to {MAX_TITLE} characters"))),
    }
}

pub fn description(value: &str) -> Result<String, Problem> {
    let value = value.trim();
    match value.chars().count() <= MAX_DESCRIPTION {
        true => Ok(value.to_string()),
        false => Err(Problem::bad_request(format!(
            "a description is at most {MAX_DESCRIPTION} characters"
        ))),
    }
}

/// A team's contact address, or empty for none. Enough of a check to catch a typed mistake; what
/// a mail server will take is its own business.
pub fn contact(value: &str) -> Result<String, Problem> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(String::new());
    }
    let fine = value.chars().count() <= MAX_EMAIL
        && !value.contains(char::is_whitespace)
        && value
            .split_once('@')
            .is_some_and(|(user, domain)| !user.is_empty() && domain.contains('.'));
    match fine {
        true => Ok(value.to_string()),
        false => Err(Problem::bad_request(format!(
            "an email address is at most {MAX_EMAIL} characters, such as team@example.com"
        ))),
    }
}

/// Changes to organisations and teams, for plugins and frontends: `platform.organisation.*` and
/// `platform.team.*`. A bus that is down fails nothing that succeeded.
pub(crate) async fn announce(state: &AppState, event: &str, detail: Value) {
    let name = format!("platform.{event}");
    let Ok(topic) = Topic::new(name.clone()) else { return };
    if let Err(err) =
        state.buses.events.publish(Event::new(topic, crate::fabric::SOURCE, detail)).await
    {
        tracing::warn!(%err, topic = %name, "could not announce a change to teams");
    }
}

pub(super) async fn audit(
    state: &AppState,
    by: &Principal,
    action: &str,
    subject: Uuid,
    detail: Value,
) {
    let entry = AuditEntry::new(action).by(by).subject(subject.to_string()).detail(detail);
    let _ = state.repos.identity.record_audit(entry).await;
}

pub(super) async fn organisation(state: &AppState, id: Uuid) -> Result<Organisation, Problem> {
    state.repos.teams.organisation(id).await?.ok_or_else(|| Problem::not_found("organisation"))
}

pub(super) async fn team(state: &AppState, id: Uuid) -> Result<Team, Problem> {
    state.repos.teams.team(id).await?.ok_or_else(|| Problem::not_found("team"))
}

/// A provider's team is changed by that provider, apart from what DOC alone decides.
fn made_in_doc(team: &Team) -> Result<(), Problem> {
    match &team.provider {
        Some(provider) => Err(Problem::conflict(format!(
            "{provider} provides this team, so it changes there; DOC keeps its default mark and \
             the people added here"
        ))),
        None => Ok(()),
    }
}

async fn views(state: &AppState, teams: Vec<Team>) -> Result<Vec<TeamView>, Problem> {
    let organisations: HashMap<Uuid, String> = state
        .repos
        .teams
        .organisations()
        .await?
        .into_iter()
        .map(|organisation| (organisation.id, organisation.name))
        .collect();
    let mut counts: HashMap<Uuid, usize> = HashMap::new();
    for held in state.repos.teams.members(None).await? {
        *counts.entry(held.team_id).or_default() += 1;
    }
    let mut leads: HashMap<Uuid, LeadView> = HashMap::new();
    for lead in teams.iter().filter_map(|team| team.lead_id) {
        if leads.contains_key(&lead) {
            continue;
        }
        if let Some(user) = state.repos.identity.user_by_id(lead).await? {
            leads.insert(lead, LeadView { login: user.login, name: user.name });
        }
    }
    Ok(teams
        .into_iter()
        .map(|team| TeamView {
            organisation: organisations.get(&team.organisation_id).cloned().unwrap_or_default(),
            members: counts.get(&team.id).copied().unwrap_or_default(),
            lead: team.lead_id.and_then(|lead| leads.get(&lead).cloned()),
            team,
        })
        .collect())
}

pub async fn list_organisations(
    State(state): State<AppState>,
    _auth: Auth,
) -> Result<Json<Vec<OrganisationView>>, Problem> {
    let teams = state.repos.teams.teams().await?;
    let organisations = state.repos.teams.organisations().await?;
    Ok(Json(
        organisations
            .into_iter()
            .map(|organisation| OrganisationView {
                teams: teams.iter().filter(|team| team.organisation_id == organisation.id).count(),
                organisation,
            })
            .collect(),
    ))
}

pub async fn create_organisation(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<OrganisationBody>,
) -> Result<impl IntoResponse, Problem> {
    manager(&state, &auth.0).await?;
    let name = slug(&body.name, "a name")?;
    let (title, description) = (title(&body.title)?, description(&body.description)?);
    let made = state.repos.teams.create_organisation(&name, &title, &description).await?;
    audit(&state, &auth.0, "organisation.created", made.id, json!({ "name": made.name })).await;
    announce(&state, "organisation.created", json!({ "id": made.id, "name": made.name })).await;
    Ok((StatusCode::CREATED, Json(made)))
}

/// Every identity provider running now or chosen by an organisation, with who chose it.
async fn choices(state: &AppState) -> Result<Vec<ProviderChoice>, Problem> {
    let running = super::auth::identity_providers(state).await;
    let chosen = state.repos.teams.providers().await?;
    let mut choices: Vec<ProviderChoice> = running
        .into_iter()
        .map(|offered| ProviderChoice {
            organisation: chosen
                .iter()
                .find(|(provider, _)| provider == &offered.id)
                .map(|(_, organisation)| *organisation),
            id: offered.id,
            title: offered.title,
            running: true,
        })
        .collect();
    for (provider, organisation) in chosen {
        if !choices.iter().any(|choice| choice.id == provider) {
            let title = provider.clone();
            let organisation = Some(organisation);
            choices.push(ProviderChoice { id: provider, title, running: false, organisation });
        }
    }
    choices.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(choices)
}

pub async fn show_organisation(
    State(state): State<AppState>,
    _auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, Problem> {
    let organisation = organisation(&state, id).await?;
    let teams: Vec<Team> = state
        .repos
        .teams
        .teams()
        .await?
        .into_iter()
        .filter(|team| team.organisation_id == id)
        .collect();
    let teams = views(&state, teams).await?;
    let providers = choices(&state).await?;
    let domains: Vec<String> = state
        .repos
        .teams
        .domains()
        .await?
        .into_iter()
        .filter(|(_, by)| *by == id)
        .map(|(domain, _)| domain)
        .collect();
    Ok(Json(json!({
        "organisation": organisation,
        "teams": teams,
        "providers": providers,
        "domains": domains,
    })))
}

/// Chooses the email domains an organisation approves: somebody with an address in one can be
/// added by an administrator or a team lead, and sign in with a DOC password (FEAT-PEOPLE). A
/// domain belongs to one organisation, so one another approved is refused until it lets go.
pub async fn set_domains(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<DomainsBody>,
) -> Result<Json<Value>, Problem> {
    manager(&state, &auth.0).await?;
    organisation(&state, id).await?;
    let mut domains = body
        .domains
        .iter()
        .filter(|domain| !domain.trim().is_empty())
        .map(|domain| super::people::domain(domain))
        .collect::<Result<Vec<_>, _>>()?;
    domains.sort();
    domains.dedup();
    state.repos.teams.set_domains(id, &domains).await?;
    let detail = json!({ "domains": domains });
    audit(&state, &auth.0, "organisation.domains.changed", id, detail).await;
    announce(&state, "organisation.changed", json!({ "id": id, "domains": domains })).await;
    Ok(Json(json!({ "domains": domains })))
}

/// Chooses the identity providers an organisation's people sign in with. Each provider serves one
/// organisation, so one another has chosen is refused until it lets go.
pub async fn set_providers(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<ProvidersBody>,
) -> Result<Json<Value>, Problem> {
    manager(&state, &auth.0).await?;
    organisation(&state, id).await?;
    let mut providers: Vec<String> =
        body.providers.iter().map(|provider| provider.trim().to_string()).collect();
    providers.sort();
    providers.dedup();
    for provider in &providers {
        let known = state.config.plugins.ids.contains(provider)
            || state.plugins.get(provider).await.is_some();
        if !known {
            return Err(Problem::bad_request(format!("there is no provider called {provider}")));
        }
    }
    state.repos.teams.set_providers(id, &providers).await?;
    let detail = json!({ "providers": providers });
    audit(&state, &auth.0, "organisation.providers.changed", id, detail).await;
    announce(&state, "organisation.changed", json!({ "id": id, "providers": providers })).await;
    Ok(Json(json!({ "providers": providers })))
}

pub async fn update_organisation(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<OrganisationPatch>,
) -> Result<Json<Organisation>, Problem> {
    manager(&state, &auth.0).await?;
    let found = organisation(&state, id).await?;
    let name = match &body.name {
        Some(name) => slug(name, "a name")?,
        None => found.name.clone(),
    };
    let title = match &body.title {
        Some(given) => title(given)?,
        None => found.title.clone(),
    };
    let description = match &body.description {
        Some(given) => description(given)?,
        None => found.description.clone(),
    };
    let updated = state
        .repos
        .teams
        .update_organisation(id, &name, &title, &description)
        .await?
        .ok_or_else(|| Problem::not_found("organisation"))?;
    audit(&state, &auth.0, "organisation.changed", id, json!({ "name": updated.name })).await;
    announce(&state, "organisation.changed", json!({ "id": id, "name": updated.name })).await;
    Ok(Json(updated))
}

pub async fn delete_organisation(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    manager(&state, &auth.0).await?;
    let deleted = state
        .repos
        .teams
        .delete_organisation(id)
        .await?
        .ok_or_else(|| Problem::not_found("organisation"))?;
    audit(&state, &auth.0, "organisation.deleted", id, json!({ "name": deleted.name })).await;
    announce(&state, "organisation.deleted", json!({ "id": id, "name": deleted.name })).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn list_teams(
    State(state): State<AppState>,
    _auth: Auth,
) -> Result<Json<Vec<TeamView>>, Problem> {
    let teams = state.repos.teams.teams().await?;
    Ok(Json(views(&state, teams).await?))
}

pub async fn create_team(
    State(state): State<AppState>,
    auth: Auth,
    Json(body): Json<TeamBody>,
) -> Result<impl IntoResponse, Problem> {
    manager(&state, &auth.0).await?;
    organisation(&state, body.organisation).await?;
    if let Some(parent) = body.parent {
        team(&state, parent).await?;
    }
    let new = NewTeam {
        organisation_id: body.organisation,
        parent_id: body.parent,
        name: slug(&body.name, "a name")?,
        title: title(&body.title)?,
        description: description(&body.description)?,
        email: contact(&body.email)?,
        is_default: body.default,
        provided: None,
    };
    let made = state.repos.teams.create_team(&new).await?;
    let detail = json!({ "name": made.name, "organisation": made.organisation_id });
    audit(&state, &auth.0, "team.created", made.id, detail.clone()).await;
    let mut event = detail;
    event["id"] = json!(made.id);
    announce(&state, "team.created", event).await;
    Ok((StatusCode::CREATED, Json(made)))
}

pub async fn show_team(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<TeamDetail>, Problem> {
    let found = team(&state, id).await?;
    let teams = state.repos.teams.teams().await?;
    let mut ancestors = Vec::new();
    let mut next = found.parent_id;
    while let Some(parent) = next.and_then(|id| teams.iter().find(|team| team.id == id)) {
        if ancestors.iter().any(|seen: &Team| seen.id == parent.id) {
            break;
        }
        ancestors.insert(0, parent.clone());
        next = parent.parent_id;
    }
    let sub_teams = teams.iter().filter(|team| team.parent_id == Some(id)).cloned().collect();
    let held = state.repos.teams.members(Some(id)).await?;
    let mut members = Vec::with_capacity(held.len());
    for membership in held {
        let Some(user) = state.repos.identity.user_by_id(membership.user_id).await? else {
            continue;
        };
        members.push(MemberView {
            user_id: user.id,
            login: user.login,
            name: user.name,
            source: membership.source,
            provider: membership.provider,
            position: membership.position,
            created_at: membership.created_at,
        });
    }
    members.sort_by_key(|member| member.login.to_lowercase());
    let manages = match auth.0.as_user() {
        Some(user) => reach_of(&state, user.id).await?.contains(&id),
        None => false,
    } || manages_identity(&state, &auth.0).await;
    let service_accounts = match manages {
        true => {
            let owners = crate::teams::Owners { user: Uuid::nil(), teams: vec![id] };
            let accounts = state.repos.identity.list_service_accounts(Some(&owners)).await?;
            Some(accounts.into_iter().map(AccountView::from).collect())
        }
        false => None,
    };
    let organisation = organisation(&state, found.organisation_id).await?;
    let positions = super::positions::team_positions(&state, &found).await?;
    let arranges = super::positions::arranges(&state, &auth.0, &found).await;
    Ok(Json(TeamDetail {
        team: found,
        organisation,
        ancestors,
        sub_teams,
        members,
        service_accounts,
        positions,
        arranges,
    }))
}

pub async fn update_team(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<TeamPatch>,
) -> Result<Json<Team>, Problem> {
    manager(&state, &auth.0).await?;
    let found = team(&state, id).await?;
    let changes_provided = body.name.is_some()
        || body.title.is_some()
        || body.description.is_some()
        || body.parent.is_some();
    if changes_provided {
        made_in_doc(&found)?;
    }
    if let Some(Some(parent)) = body.parent {
        team(&state, parent).await?;
    }
    let changes = TeamChanges {
        name: body.name.as_deref().map(|name| slug(name, "a name")).transpose()?,
        title: body.title.as_deref().map(title).transpose()?,
        description: body.description.as_deref().map(description).transpose()?,
        email: body.email.as_deref().map(contact).transpose()?,
        parent: body.parent,
        is_default: body.default,
    };
    let updated = state
        .repos
        .teams
        .update_team(id, &changes)
        .await?
        .ok_or_else(|| Problem::not_found("team"))?;
    let detail = json!({
        "name": updated.name, "parent": updated.parent_id, "default": updated.is_default,
    });
    audit(&state, &auth.0, "team.changed", id, detail.clone()).await;
    let mut event = detail;
    event["id"] = json!(id);
    crate::permissions::forget_all(&state).await;
    announce(&state, "team.changed", event).await;
    Ok(Json(updated))
}

pub async fn delete_team(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<StatusCode, Problem> {
    manager(&state, &auth.0).await?;
    made_in_doc(&team(&state, id).await?)?;
    let deleted =
        state.repos.teams.delete_team(id).await?.ok_or_else(|| Problem::not_found("team"))?;
    audit(&state, &auth.0, "team.deleted", id, json!({ "name": deleted.name })).await;
    crate::permissions::forget_all(&state).await;
    announce(&state, "team.deleted", json!({ "id": id, "name": deleted.name })).await;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn add_member(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<MemberBody>,
) -> Result<impl IntoResponse, Problem> {
    manager(&state, &auth.0).await?;
    team(&state, id).await?;
    let user = state
        .repos
        .identity
        .user_by_id(body.user)
        .await?
        .ok_or_else(|| Problem::not_found("user"))?;
    if !state.repos.teams.add_member(id, user.id, BY_HAND, None).await? {
        return Ok((StatusCode::OK, Json(json!({ "added": false }))));
    }
    changed_members(&state, &auth.0, id, "team.member.added", user.id, &user.login).await;
    Ok((StatusCode::CREATED, Json(json!({ "added": true }))))
}

pub async fn remove_member(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, user)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, Problem> {
    manager(&state, &auth.0).await?;
    team(&state, id).await?;
    let held: Option<TeamMember> =
        state.repos.teams.members(Some(id)).await?.into_iter().find(|held| held.user_id == user);
    let held = held.ok_or_else(|| Problem::not_found("member"))?;
    if held.source == BY_PROVIDER {
        let provider = held.provider.unwrap_or_default();
        return Err(Problem::conflict(format!(
            "{provider} put them in this team, so they leave it there"
        )));
    }
    state.repos.teams.remove_member(id, user).await?;
    let login = state.repos.identity.user_by_id(user).await?.map(|user| user.login);
    let login = login.unwrap_or_default();
    changed_members(&state, &auth.0, id, "team.member.removed", user, &login).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Audits a change of members and announces it as `platform.team.changed`.
async fn changed_members(
    state: &AppState,
    by: &Principal,
    team: Uuid,
    action: &str,
    user: Uuid,
    login: &str,
) {
    audit(state, by, action, team, json!({ "user": user, "login": login })).await;
    let change = if action.ends_with("added") { "added" } else { "removed" };
    // What a team holds, its people hold, so the answer to "may they?" changes with its members.
    // The event does this too, on a platform whose buses reach every node; this is the near one.
    crate::permissions::forget_all(state).await;
    announce(state, "team.changed", json!({ "id": team, "members": { change: [user] } })).await;
}

#[cfg(test)]
mod tests {
    use doc_plugin_protocol::calls::{
        OrganisationRequest, TeamMembersRequest, TeamRemoveRequest, TeamRequest, UserRequest,
        WriteTeamRequest,
    };
    use doc_plugin_protocol::{Capability, Manifest, PluginState, RegisterRequest};

    use super::*;
    use crate::config::Config;
    use crate::db::repositories::{IdentityRepository, TeamRepository};
    use crate::identity::{Account, NewUser, TokenOwner};
    use crate::plugins::{self, api as plugin_api};
    use crate::secrets::TokenKind;
    use crate::testing::{ADMIN, Host, delete_as, get_as, patch_json, plugin_host_with, post_json};

    const ADA: &str = "doc_ses_ada";
    const BOB: &str = "doc_ses_bob";
    const EVE: &str = "doc_ses_eve";

    /// `tester` administers everything; `ada`, `bob` and `eve` are anyone else.
    fn host() -> (Host, [crate::identity::User; 3]) {
        let mut config = Config::default();
        config.bootstrap.admins = vec!["tester".into()];
        config.plugins.ids =
            vec!["hello".into(), "github".into(), "ghe".into(), "resources".into()];
        for provider in ["ghe", "hello"] {
            config.plugins.capabilities.insert(provider.into(), vec![Capability::IdentityProvider]);
        }
        config
            .plugins
            .capabilities
            .insert("github".into(), vec![Capability::TeamProvider, Capability::IdentityProvider]);
        config.plugins.capabilities.insert("resources".into(), vec![Capability::TeamWriter]);
        let host = plugin_host_with(config);
        let people = [("ada", ADA), ("bob", BOB), ("eve", EVE)].map(|(login, token)| {
            let user = host.identity.add_user(login);
            host.identity.give(token, TokenKind::Session, TokenOwner::User(user.id), None, false);
            user
        });
        (host, people)
    }

    async fn running(host: &Host, id: &str, capabilities: Vec<Capability>) {
        let manifest = Manifest {
            id: id.into(),
            version: "1.0.0".into(),
            capabilities,
            ..Manifest::default()
        };
        let request = RegisterRequest {
            manifest,
            address: format!("plugin-{id}:4440"),
            binary_sha256: "a".repeat(64),
            started_at: None,
        };
        let principal = Principal::Plugin { id: id.into() };
        plugins::register(&host.state, &principal, request).await.expect("registered");
        assert_eq!(host.settle(id).await, Some(PluginState::Running));
    }

    async fn github(host: &Host) {
        let capabilities = vec![Capability::TeamProvider, Capability::IdentityProvider];
        running(host, "github", capabilities).await;
        let organisation = host.identity.organisation();
        host.identity.set_providers(organisation, &["github".to_string()]).await.expect("chosen");
    }

    async fn made(host: &Host, path: &str, body: Value) -> String {
        let (status, made, _) = post_json(&host.app, path, Some(ADMIN), body.clone()).await;
        assert_eq!(status, StatusCode::CREATED, "{path} {body}: {made}");
        made["id"].as_str().expect("an id").to_string()
    }

    /// Moves everyone but the admin into `organisation`, whose teams they can then be in.
    async fn join(host: &Host, organisation: &str) {
        for listed in host.identity.list_users().await.unwrap() {
            if listed.user.login == "tester" {
                continue;
            }
            let path = format!("/api/v1/users/{}", listed.user.id);
            let moving = json!({ "organisation": organisation });
            let (status, body, _) = patch_json(&host.app, &path, ADMIN, moving).await;
            assert_eq!(status, StatusCode::OK, "{body}");
            assert_eq!(body["organisation"]["id"], organisation);
        }
    }

    async fn organisation_and_teams(host: &Host) -> (String, String, String) {
        let organisation = made(
            host,
            "/api/v1/organisations",
            json!({ "name": "acme", "title": "Acme", "description": "Where we work" }),
        )
        .await;
        join(host, &organisation).await;
        let platform = made(
            host,
            "/api/v1/teams",
            json!({ "organisation": organisation, "name": "platform", "title": "Platform" }),
        )
        .await;
        let qa = made(
            host,
            "/api/v1/teams",
            json!({ "organisation": organisation, "name": "qa", "title": "QA", "parent": platform }),
        )
        .await;
        (organisation, platform, qa)
    }

    #[tokio::test]
    async fn identity_managers_make_organisations_and_teams_that_everyone_sees() {
        let (host, [ada, ..]) = host();
        let (organisation, platform, qa) = organisation_and_teams(&host).await;

        let (status, body, _) = get_as(&host.app, &format!("/api/v1/teams/{qa}"), ADA).await;
        assert_eq!(status, StatusCode::OK, "anyone signed in sees teams");
        assert_eq!(body["organisation"]["name"], "acme");
        assert_eq!(body["ancestors"][0]["id"], platform.as_str());
        assert_eq!(body["service_accounts"], Value::Null, "ada manages none of its accounts");
        let (_, body, _) = get_as(&host.app, &format!("/api/v1/teams/{platform}"), ADA).await;
        assert_eq!(body["sub_teams"][0]["name"], "qa");
        let (_, listed, _) = get_as(&host.app, "/api/v1/organisations", ADA).await;
        assert_eq!(listed[0]["teams"], 2);

        let refused = [
            ("/api/v1/organisations", json!({ "name": "other", "title": "Other" })),
            ("/api/v1/teams", json!({ "organisation": organisation, "name": "x", "title": "X" })),
        ];
        for (path, body) in refused {
            let (status, _, _) = post_json(&host.app, path, Some(ADA), body).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
        }
        let path = format!("/api/v1/teams/{platform}/members");
        let (status, _, _) =
            post_json(&host.app, &path, Some(ADA), json!({ "user": ada.id })).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "nobody adds themselves");

        for (body, expected) in [
            (json!({ "name": "Not A Slug", "title": "X" }), StatusCode::BAD_REQUEST),
            (json!({ "name": "acme", "title": "Again" }), StatusCode::CONFLICT),
            (json!({ "name": "blank", "title": " " }), StatusCode::BAD_REQUEST),
        ] {
            let (status, answer, _) =
                post_json(&host.app, "/api/v1/organisations", Some(ADMIN), body.clone()).await;
            assert_eq!(status, expected, "{body}: {answer}");
        }
        let taken = json!({ "organisation": organisation, "name": "qa", "title": "Again" });
        let (status, _, _) = post_json(&host.app, "/api/v1/teams", Some(ADMIN), taken).await;
        assert_eq!(status, StatusCode::CONFLICT, "a name is unique within its organisation");
        assert!(host.identity.audit_actions().contains(&"team.created".to_string()));
    }

    #[tokio::test]
    async fn a_team_never_sits_below_itself_or_outside_its_organisation() {
        let (host, _) = host();
        let (organisation, platform, qa) = organisation_and_teams(&host).await;
        let nightly = made(
            &host,
            "/api/v1/teams",
            json!({ "organisation": organisation, "name": "nightly", "title": "Nightly", "parent": qa }),
        )
        .await;
        for parent in [&nightly, &qa, &platform] {
            let (status, body, _) = patch_json(
                &host.app,
                &format!("/api/v1/teams/{platform}"),
                ADMIN,
                json!({ "parent": parent }),
            )
            .await;
            assert_eq!(status, StatusCode::CONFLICT, "{parent}: {body}");
        }
        let elsewhere =
            made(&host, "/api/v1/organisations", json!({ "name": "other", "title": "Other" }))
                .await;
        let stranger = made(
            &host,
            "/api/v1/teams",
            json!({ "organisation": elsewhere, "name": "stranger", "title": "Stranger" }),
        )
        .await;
        let (status, _, _) = patch_json(
            &host.app,
            &format!("/api/v1/teams/{nightly}"),
            ADMIN,
            json!({ "parent": stranger }),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "a sub-team is in its parent's organisation");
        let crossing =
            json!({ "organisation": elsewhere, "name": "late", "title": "Late", "parent": qa });
        let (status, _, _) = post_json(&host.app, "/api/v1/teams", Some(ADMIN), crossing).await;
        assert_eq!(status, StatusCode::CONFLICT);

        let (status, body, _) = patch_json(
            &host.app,
            &format!("/api/v1/teams/{nightly}"),
            ADMIN,
            json!({ "parent": null }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["parent_id"], Value::Null, "null makes it top-level");
        let (status, _, _) =
            delete_as(&host.app, &format!("/api/v1/teams/{platform}"), ADMIN).await;
        assert_eq!(status, StatusCode::CONFLICT, "a team with sub-teams stays");
        let path = format!("/api/v1/organisations/{organisation}");
        let (status, _, _) = delete_as(&host.app, &path, ADMIN).await;
        assert_eq!(status, StatusCode::CONFLICT, "an organisation with teams stays");
    }

    #[tokio::test]
    async fn everyone_is_placed_in_every_default_team_as_they_arrive() {
        let (host, [ada, ..]) = host();
        let defaults = host.identity.with_default_teams();
        for team in &defaults {
            let members = host.identity.members(Some(team.id)).await.unwrap();
            assert!(members.iter().any(|held| held.user_id == ada.id), "those already here too");
        }

        github(&host).await;
        let newcomer = crate::testing::sign_in(&host, "github", "1", "newcomer").await.user_id;

        let organisation = host.identity.organisation();
        let early = NewUser {
            login: "early".into(),
            organisation_id: organisation,
            name: None,
            email: None,
        };
        let early = host.identity.create_user(&early, None).await.unwrap();
        let described = UserRequest {
            provider: "github".into(),
            external_id: "7".into(),
            login: "octo".into(),
            ..UserRequest::default()
        };
        let provided = plugin_api::users(&host.state, "github", described).await.unwrap().user_id;

        for user in [newcomer, early.id, provided] {
            let teams: Vec<Uuid> = host
                .identity
                .memberships(user)
                .await
                .unwrap()
                .into_iter()
                .filter(|held| held.source == "default")
                .map(|held| held.team_id)
                .collect();
            assert_eq!(teams.len(), 2, "{user} is in Leadership and Product");
        }
        let (_, body, _) = get_as(&host.app, &format!("/api/v1/users/{newcomer}"), ADMIN).await;
        let names: Vec<&str> = body["teams"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|team| team["name"].as_str())
            .collect();
        assert_eq!(names, ["leadership", "product"]);
    }

    #[tokio::test]
    async fn there_is_always_a_default_team() {
        let (host, _) = host();
        let [leadership, product] = host.identity.with_default_teams().try_into().unwrap();
        let path = |team: &Team| format!("/api/v1/teams/{}", team.id);
        let unmark = json!({ "default": false });

        let (status, _, _) = patch_json(&host.app, &path(&leadership), ADMIN, unmark.clone()).await;
        assert_eq!(status, StatusCode::OK, "one of two can go");
        let (status, body, _) = patch_json(&host.app, &path(&product), ADMIN, unmark.clone()).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, _, _) = delete_as(&host.app, &path(&product), ADMIN).await;
        assert_eq!(status, StatusCode::CONFLICT, "nor deleted while it is the last");

        let organisation = leadership.organisation_id;
        let everyone = made(
            &host,
            "/api/v1/teams",
            json!({ "organisation": organisation, "name": "everyone", "title": "Everyone", "default": true }),
        )
        .await;
        let (status, _, _) = delete_as(&host.app, &path(&product), ADMIN).await;
        assert_eq!(status, StatusCode::NO_CONTENT, "with another default, admins delete it");
        let (status, _, _) = delete_as(&host.app, &path(&leadership), ADMIN).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (status, _, _) =
            delete_as(&host.app, &format!("/api/v1/teams/{everyone}"), ADMIN).await;
        assert_eq!(status, StatusCode::CONFLICT, "the last one stays");
    }

    #[tokio::test]
    async fn a_provider_and_an_admin_never_undo_each_others_changes() {
        let (host, [ada, bob, eve]) = host();
        github(&host).await;
        let acme =
            made(&host, "/api/v1/organisations", json!({ "name": "acme", "title": "Acme" })).await;
        join(&host, &acme).await;
        let provide = |key: &str, name: &str, parent: Option<&str>| TeamRequest {
            organisation: "acme".into(),
            external_id: key.into(),
            name: name.into(),
            title: name.to_uppercase(),
            description: String::new(),
            parent: parent.map(str::to_string),
        };
        let platform =
            plugin_api::teams(&host.state, "github", provide("acme/platform", "platform", None))
                .await
                .expect("made");
        assert!(platform.created);
        let again =
            plugin_api::teams(&host.state, "github", provide("acme/platform", "platform", None))
                .await
                .expect("found");
        assert_eq!((again.team_id, again.created), (platform.team_id, false));
        running(&host, "hello", Vec::new()).await;
        let refused = plugin_api::teams(&host.state, "hello", provide("x", "x", None)).await;
        assert_eq!(refused.expect_err("not a team provider").status, 403);

        let syncing =
            |users: Vec<Uuid>| TeamMembersRequest { external_id: "acme/platform".into(), users };
        plugin_api::team_members(&host.state, "github", syncing(vec![ada.id, bob.id]))
            .await
            .expect("synced");
        let path = format!("/api/v1/teams/{}", platform.team_id);
        let (status, _, _) = post_json(
            &host.app,
            &format!("{path}/members"),
            Some(ADMIN),
            json!({ "user": eve.id }),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "an admin adds someone to a provider's team");

        let synced = plugin_api::team_members(&host.state, "github", syncing(vec![ada.id]))
            .await
            .expect("synced");
        assert_eq!((synced.added, synced.removed), (vec![], vec![bob.id]));
        let members: Vec<Uuid> = host
            .identity
            .members(Some(platform.team_id))
            .await
            .unwrap()
            .into_iter()
            .map(|held| held.user_id)
            .collect();
        assert!(members.contains(&eve.id), "the sync leaves the admin's addition alone");

        let (status, body, _) =
            delete_as(&host.app, &format!("{path}/members/{}", ada.id), ADMIN).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, _, _) =
            patch_json(&host.app, &path, ADMIN, json!({ "title": "Renamed" })).await;
        assert_eq!(status, StatusCode::CONFLICT, "the provider names its own teams");
        let (status, _, _) = delete_as(&host.app, &path, ADMIN).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, body, _) =
            patch_json(&host.app, &path, ADMIN, json!({ "default": true })).await;
        assert_eq!(status, StatusCode::OK, "DOC decides what is default: {body}");

        let organisation = host.identity.organisation_named("acme").await.unwrap().unwrap();
        let hand = made(
            &host,
            "/api/v1/teams",
            json!({ "organisation": organisation.id, "name": "design", "title": "Design" }),
        )
        .await;
        let clash =
            plugin_api::teams(&host.state, "github", provide("acme/design", "design", None)).await;
        assert_eq!(clash.expect_err("the name is taken").status, 409);
        let foreign = plugin_api::team_members(
            &host.state,
            "github",
            TeamMembersRequest { external_id: "design".into(), users: vec![ada.id] },
        )
        .await;
        assert_eq!(foreign.expect_err("not its team").status, 404);
        assert!(host.identity.members(Some(hand.parse().unwrap())).await.unwrap().is_empty());

        let removed = plugin_api::team_remove(
            &host.state,
            "github",
            TeamRemoveRequest { external_id: "acme/platform".into() },
        )
        .await
        .expect("removed");
        assert!(removed.released && !removed.removed, "eve was added in DOC, so it stays");
        let kept = host.identity.team(platform.team_id).await.unwrap().expect("still there");
        assert_eq!(kept.provider, None, "now a team made in DOC");
        let left: Vec<Uuid> = host
            .identity
            .members(Some(platform.team_id))
            .await
            .unwrap()
            .into_iter()
            .map(|held| held.user_id)
            .collect();
        assert_eq!(left, [eve.id], "only the provider's memberships went");

        plugin_api::teams(&host.state, "github", provide("acme/tools", "tools", None))
            .await
            .unwrap();
        let gone = plugin_api::team_remove(
            &host.state,
            "github",
            TeamRemoveRequest { external_id: "acme/tools".into() },
        )
        .await
        .expect("removed");
        assert!(gone.removed, "nothing made in DOC depended on it");
    }

    #[tokio::test]
    async fn a_team_owns_service_accounts_that_its_members_and_sub_teams_manage() {
        let (host, [ada, bob, _]) = host();
        let (_, platform, qa) = organisation_and_teams(&host).await;
        for (team, user) in [(&platform, ada.id), (&qa, bob.id)] {
            let path = format!("/api/v1/teams/{team}/members");
            let (status, _, _) =
                post_json(&host.app, &path, Some(ADMIN), json!({ "user": user })).await;
            assert_eq!(status, StatusCode::CREATED);
        }

        let asked = json!({ "name": "platform-bot", "team": qa });
        let (status, _, _) =
            post_json(&host.app, "/api/v1/service-accounts", Some(ADA), asked).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "ada is above QA, not in it");
        let asked = json!({ "name": "platform-bot", "team": platform });
        let (status, body, _) =
            post_json(&host.app, "/api/v1/service-accounts", Some(EVE), asked.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "eve is in neither: {body}");
        let (status, body, _) =
            post_json(&host.app, "/api/v1/service-accounts", Some(ADA), asked).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["owner_team_id"], platform.as_str());
        assert_eq!(body["owner_id"], Value::Null);
        let account = body["id"].as_str().unwrap().to_string();

        let path = format!("/api/v1/service-accounts/{account}");
        let (status, _, _) = get_as(&host.app, &path, BOB).await;
        assert_eq!(status, StatusCode::OK, "a sub-team's member manages what the team above owns");
        let (status, _, _) =
            post_json(&host.app, &format!("{path}/tokens"), Some(BOB), json!({ "name": "ci" }))
                .await;
        assert_eq!(status, StatusCode::CREATED);
        let (status, _, _) = get_as(&host.app, &path, EVE).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (_, listed, _) = get_as(&host.app, "/api/v1/service-accounts", BOB).await;
        assert_eq!(listed[0]["name"], "platform-bot");
        let (_, listed, _) = get_as(&host.app, "/api/v1/service-accounts", EVE).await;
        assert_eq!(listed, json!([]));
        let (_, team, _) = get_as(&host.app, &format!("/api/v1/teams/{platform}"), BOB).await;
        assert_eq!(team["service_accounts"][0]["name"], "platform-bot");

        let (status, _, _) =
            delete_as(&host.app, &format!("/api/v1/teams/{platform}"), ADMIN).await;
        assert_eq!(status, StatusCode::CONFLICT, "a team that owns accounts stays");
        let owner = format!("{path}/owner");
        let (status, _, _) =
            crate::testing::put_json(&host.app, &owner, BOB, json!({ "user": ada.id })).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "only an admin gives it to someone else");
        let (status, body, _) =
            crate::testing::put_json(&host.app, &owner, BOB, json!({ "user": bob.id })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            (body["owner_id"].as_str(), &body["owner_team_id"]),
            (Some(bob.id.to_string().as_str()), &Value::Null)
        );
        let (status, _, _) = get_as(&host.app, &path, ADA).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "no longer the team's");
        assert!(
            host.identity
                .audit_actions()
                .contains(&"iam.service-account.owner.changed".to_string())
        );
    }

    #[tokio::test]
    async fn merging_brings_team_memberships_over() {
        let (host, [ada, ..]) = host();
        let (acme, platform, _) = organisation_and_teams(&host).await;
        let early = host
            .identity
            .create_user(
                &NewUser {
                    login: "early".into(),
                    organisation_id: acme.parse().unwrap(),
                    name: None,
                    email: None,
                },
                Some(&Account {
                    provider: "github".into(),
                    external_id: "9".into(),
                    login: "early".into(),
                }),
            )
            .await
            .unwrap();
        let path = format!("/api/v1/teams/{platform}/members");
        post_json(&host.app, &path, Some(ADMIN), json!({ "user": early.id })).await;
        let merge = format!("/api/v1/users/{}/merge", ada.id);
        let (status, body, _) =
            post_json(&host.app, &merge, Some(ADMIN), json!({ "from": early.id })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["teams"][0]["name"], "platform");
    }

    #[tokio::test]
    async fn each_organisation_chooses_its_sign_in_providers_and_each_serves_one() {
        let (host, _) = host();
        github(&host).await;
        let acme =
            made(&host, "/api/v1/organisations", json!({ "name": "acme", "title": "Acme" })).await;
        let default = host.identity.organisation();
        let choose = |providers: Value| json!({ "providers": providers });

        let path = format!("/api/v1/organisations/{acme}/providers");
        let (status, body, _) =
            crate::testing::put_json(&host.app, &path, ADMIN, choose(json!(["github"]))).await;
        assert_eq!(status, StatusCode::CONFLICT, "the default organisation has it: {body}");
        let (status, _, _) =
            crate::testing::put_json(&host.app, &path, ADA, choose(json!([]))).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        let (status, _, _) =
            crate::testing::put_json(&host.app, &path, ADMIN, choose(json!(["nowhere"]))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let away = format!("/api/v1/organisations/{default}/providers");
        let (status, _, _) =
            crate::testing::put_json(&host.app, &away, ADMIN, choose(json!([]))).await;
        assert_eq!(status, StatusCode::OK, "it lets go");
        let (status, body, _) =
            crate::testing::put_json(&host.app, &path, ADMIN, choose(json!(["github"]))).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, shown, _) = get_as(&host.app, &format!("/api/v1/organisations/{acme}"), ADA).await;
        let github =
            shown["providers"].as_array().unwrap().iter().find(|p| p["id"] == "github").cloned();
        assert_eq!(github.expect("offered")["organisation"], acme.as_str());
        assert!(
            host.identity.audit_actions().contains(&"organisation.providers.changed".to_string())
        );
    }

    #[tokio::test]
    async fn people_sign_in_with_their_own_organisations_providers_and_join_its_default_teams() {
        let (host, [ada, ..]) = host();
        let [leadership, _] = host.identity.with_default_teams().try_into().unwrap();
        github(&host).await;
        let acme =
            made(&host, "/api/v1/organisations", json!({ "name": "acme", "title": "Acme" })).await;
        let everyone = made(
            &host,
            "/api/v1/teams",
            json!({ "organisation": acme, "name": "everyone", "title": "Everyone", "default": true }),
        )
        .await;
        let acme_id: Uuid = acme.parse().unwrap();
        running(&host, "ghe", vec![Capability::IdentityProvider]).await;
        host.identity.set_providers(acme_id, &["ghe".to_string()]).await.expect("chosen");

        let newcomer = crate::testing::sign_in(&host, "ghe", "17", "newcomer").await;
        let user = host.identity.user_by_id(newcomer.user_id).await.unwrap().expect("made");
        assert_eq!(user.organisation_id, acme_id, "they join the organisation ghe signs in for");
        let teams: Vec<Uuid> = host
            .identity
            .memberships(user.id)
            .await
            .unwrap()
            .into_iter()
            .map(|held| held.team_id)
            .collect();
        assert_eq!(teams, [everyone.parse::<Uuid>().unwrap()], "its default teams, not another's");

        // ada is the default organisation's; an account of hers with acme's provider signs nobody in.
        let account =
            Account { provider: "ghe".into(), external_id: "99".into(), login: "ada".into() };
        host.identity.attach_identity(ada.id, &account, "link").await.unwrap();
        let request = doc_plugin_protocol::calls::IdentityRequest {
            provider: "ghe".into(),
            external_id: "99".into(),
            login: "ada".into(),
            ..Default::default()
        };
        let refused = plugin_api::identity(&host.state, "ghe", request).await.expect_err("refused");
        assert_eq!(refused.status, 403, "{}", refused.detail);

        running(&host, "hello", vec![Capability::IdentityProvider]).await;
        let request = doc_plugin_protocol::calls::IdentityRequest {
            provider: "hello".into(),
            external_id: "1".into(),
            login: "stray".into(),
            ..Default::default()
        };
        let refused = plugin_api::identity(&host.state, "hello", request).await;
        assert_eq!(refused.expect_err("no organisation signs in with it").status, 403);
        assert!(
            host.identity
                .members(Some(leadership.id))
                .await
                .unwrap()
                .iter()
                .all(|held| held.user_id != newcomer.user_id)
        );
    }

    #[tokio::test]
    async fn people_are_in_their_own_organisations_teams_and_move_with_them() {
        let (host, [ada, ..]) = host();
        let [leadership, _] = host.identity.with_default_teams().try_into().unwrap();
        let acme =
            made(&host, "/api/v1/organisations", json!({ "name": "acme", "title": "Acme" })).await;
        let everyone = made(
            &host,
            "/api/v1/teams",
            json!({ "organisation": acme, "name": "everyone", "title": "Everyone", "default": true }),
        )
        .await;
        let path = format!("/api/v1/teams/{everyone}/members");
        let (status, body, _) =
            post_json(&host.app, &path, Some(ADMIN), json!({ "user": ada.id })).await;
        assert_eq!(status, StatusCode::CONFLICT, "ada is not acme's: {body}");

        let (status, _, _) = delete_as(
            &host.app,
            &format!("/api/v1/organisations/{}", host.identity.organisation()),
            ADMIN,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "people belong to it, and so do its teams");

        let moving = json!({ "organisation": acme });
        let (status, _, _) =
            patch_json(&host.app, &format!("/api/v1/users/{}", ada.id), ADA, moving.clone()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "nobody moves themselves");
        let (status, body, _) =
            patch_json(&host.app, &format!("/api/v1/users/{}", ada.id), ADMIN, moving).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let names: Vec<&str> = body["teams"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|team| team["name"].as_str())
            .collect();
        assert_eq!(
            names,
            ["everyone"],
            "out of the old organisation's teams, into the new one's default"
        );
        assert!(
            !host
                .identity
                .members(Some(leadership.id))
                .await
                .unwrap()
                .iter()
                .any(|held| held.user_id == ada.id)
        );
        assert!(host.identity.audit_actions().contains(&"iam.user.moved".to_string()));
    }

    #[tokio::test]
    async fn a_provider_makes_users_in_the_organisation_it_names_or_signs_in_for() {
        let (host, _) = host();
        github(&host).await;
        let acme =
            made(&host, "/api/v1/organisations", json!({ "name": "acme", "title": "Acme" })).await;
        let described = |external_id: &str, organisation: Option<&str>| UserRequest {
            provider: "github".into(),
            external_id: external_id.into(),
            login: format!("person-{external_id}"),
            organisation: organisation.map(str::to_string),
            ..UserRequest::default()
        };
        let named =
            plugin_api::users(&host.state, "github", described("1", Some("acme"))).await.unwrap();
        let user = host.identity.user_by_id(named.user_id).await.unwrap().unwrap();
        assert_eq!(user.organisation_id.to_string(), acme);
        let unnamed = plugin_api::users(&host.state, "github", described("2", None)).await.unwrap();
        let user = host.identity.user_by_id(unnamed.user_id).await.unwrap().unwrap();
        assert_eq!(
            user.organisation_id,
            host.identity.organisation(),
            "the one github signs in for"
        );
        let unknown =
            plugin_api::users(&host.state, "github", described("3", Some("nowhere"))).await;
        assert_eq!(unknown.expect_err("no such organisation").status, 404);
    }

    /// A provider of both teams and sign-ins should not have to be told twice which organisation
    /// it works for, so a team that names none is in the one that signs in with it (T68).
    #[tokio::test]
    async fn a_team_provider_that_names_no_organisation_gets_the_one_it_signs_in_for() {
        let (host, _) = host();
        github(&host).await;
        let provide = |organisation: &str| TeamRequest {
            organisation: organisation.into(),
            external_id: "acme/platform".into(),
            name: "platform".into(),
            title: "Platform".into(),
            description: String::new(),
            parent: None,
        };
        let made = plugin_api::teams(&host.state, "github", provide("")).await.expect("made");
        let team = host.identity.team(made.team_id).await.unwrap().expect("a team");
        assert_eq!(
            team.organisation_id,
            host.identity.organisation(),
            "the one github signs in for"
        );

        let elsewhere = made_organisation(&host, "beta").await;
        let moved = plugin_api::teams(&host.state, "github", provide("beta")).await;
        assert_eq!(
            moved.expect_err("teams do not move between organisations").status,
            409,
            "and naming another one afterwards does not move it out of {elsewhere}"
        );
    }

    /// The Catalogue keeps no teams of its own any more (T69): it makes core's, as itself while
    /// it moves what it held, and for a person only if they administer identity.
    #[tokio::test]
    async fn a_team_writer_makes_organisations_and_teams_in_doc() {
        let (host, [ada, ..]) = host();
        running(&host, "resources", vec![Capability::TeamWriter]).await;
        let organisation = plugin_api::write_organisation(
            &host.state,
            "resources",
            None,
            OrganisationRequest {
                name: "payments".into(),
                title: "Payments".into(),
                description: "Taking card payments.".into(),
            },
        )
        .await
        .expect("an organisation");
        assert!(organisation.created);

        let write = |title: &str, email: &str, parent: Option<&str>| WriteTeamRequest {
            organisation: "payments".into(),
            name: "payments-core".into(),
            title: title.into(),
            description: String::new(),
            email: email.into(),
            parent: parent.map(str::to_string),
        };
        let made = plugin_api::write_team(
            &host.state,
            "resources",
            None,
            write("Payments Core", "core@acme.example", None),
        )
        .await
        .expect("a team");
        assert!(made.created);
        let team = host.identity.team(made.team_id).await.unwrap().expect("the team");
        assert_eq!(team.organisation_id.to_string(), organisation.organisation_id.to_string());
        assert_eq!(team.email, "core@acme.example");
        assert_eq!(team.provider, None, "a team made in DOC, which admins can change");

        // Writing it again is how the Catalogue applies the same document twice: one team, brought
        // up to date rather than refused.
        let again = plugin_api::write_team(
            &host.state,
            "resources",
            None,
            write("Payments core", "core@acme.example", None),
        )
        .await
        .expect("the same team");
        assert!(!again.created);
        assert_eq!(again.team_id, made.team_id);
        let team = host.identity.team(made.team_id).await.unwrap().expect("the team");
        assert_eq!(team.title, "Payments core");

        // A person asking must administer identity themselves: a team decides who holds what.
        let theirs = host.state.plugins.contexts.issue(
            "resources",
            Principal::User(ada.clone()),
            std::time::Duration::from_secs(60),
        );
        let theirs = theirs.expect("a context");
        let refused = plugin_api::write_team(
            &host.state,
            "resources",
            Some(theirs.token()),
            write("Theirs", "", None),
        )
        .await;
        assert_eq!(refused.expect_err("not an identity manager").status, 403);
    }

    #[tokio::test]
    async fn only_a_team_writer_writes_teams_and_a_provider_keeps_its_own() {
        let (host, _) = host();
        github(&host).await;
        running(&host, "resources", vec![Capability::TeamWriter]).await;
        let writing = WriteTeamRequest {
            name: "platform".into(),
            title: "Platform".into(),
            ..WriteTeamRequest::default()
        };
        let refused = plugin_api::write_team(&host.state, "github", None, writing.clone()).await;
        assert_eq!(refused.expect_err("github only provides teams").status, 403);

        let provided = plugin_api::teams(
            &host.state,
            "github",
            TeamRequest {
                organisation: String::new(),
                external_id: "acme/platform".into(),
                name: "platform".into(),
                title: "Platform".into(),
                description: "From GitHub.".into(),
                parent: None,
            },
        )
        .await
        .expect("provided");
        let written = plugin_api::write_team(
            &host.state,
            "resources",
            None,
            WriteTeamRequest {
                title: "Renamed by the catalogue".into(),
                description: "Not this either.".into(),
                email: "platform@acme.example".into(),
                ..writing
            },
        )
        .await
        .expect("the team github provides");
        assert_eq!(written.team_id, provided.team_id, "one team, not two of the same name");
        let team = host.identity.team(provided.team_id).await.unwrap().expect("the team");
        assert_eq!(team.title, "Platform", "what the provider says stays");
        assert_eq!(team.description, "From GitHub.");
        assert_eq!(team.email, "platform@acme.example", "an address is DOC's to keep");
    }

    /// Ada leads Platform; Bob is in QA below it; Eve is in neither.
    async fn led(host: &Host, people: &[crate::identity::User; 3]) -> (String, String, String) {
        let (organisation, platform, qa) = organisation_and_teams(host).await;
        let [ada, bob, _] = people;
        for (team, user) in [(&platform, ada.id), (&qa, bob.id), (&qa, ada.id)] {
            let path = format!("/api/v1/teams/{team}/members");
            let (status, _, _) =
                post_json(&host.app, &path, Some(ADMIN), json!({ "user": user })).await;
            assert_eq!(status, StatusCode::CREATED);
        }
        let lead = format!("/api/v1/teams/{platform}/lead");
        let (status, body, _) =
            crate::testing::put_json(&host.app, &lead, ADA, json!({ "user": ada.id })).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "nobody makes themselves lead: {body}");
        let (status, body, _) =
            crate::testing::put_json(&host.app, &lead, ADMIN, json!({ "user": ada.id })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["lead_id"], json!(ada.id));
        (organisation, platform, qa)
    }

    #[tokio::test]
    async fn a_team_inherits_its_organisations_positions_and_changes_them_for_itself_and_below() {
        let (host, people) = host();
        let (organisation, platform, qa) = led(&host, &people).await;
        let put = |path: String, token: &'static str, body: Value| {
            let app = host.app.clone();
            async move { crate::testing::put_json(&app, &path, token, body).await }
        };
        let engineer = format!("/api/v1/organisations/{organisation}/positions/engineer");
        let defined = json!({
            "title": "Software Engineer",
            "responsibilities": ["Writes code", "Reviews code", "Is on call"],
        });
        let (status, _, _) = put(engineer.clone(), ADA, defined.clone()).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "a lead changes their team, not the organisation"
        );
        let (status, _, _) = put(engineer.clone(), ADMIN, defined).await;
        assert_eq!(status, StatusCode::CREATED);

        // Platform's lead changes it for Platform, and so for QA below it.
        let changed = json!({ "title": "Platform Engineer", "removed": ["Is on call"], "added": ["Runs the platform"] });
        let (status, body, _) =
            put(format!("/api/v1/teams/{platform}/positions/engineer"), ADA, changed).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // QA adds one of its own, and its lead is Platform's too.
        let own = json!({ "title": "QA Engineer", "added": ["Tests releases"] });
        let (status, body, _) = put(format!("/api/v1/teams/{qa}/positions/qa"), ADA, own).await;
        assert_eq!(status, StatusCode::OK, "the lead of a team above arranges it: {body}");
        let nameless = json!({ "added": ["Anything"] });
        let (status, _, _) =
            put(format!("/api/v1/teams/{qa}/positions/nobody"), ADA, nameless).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "a position of its own needs a title");
        let (status, _, _) =
            put(format!("/api/v1/teams/{platform}/positions/qa"), BOB, json!({ "hidden": true }))
                .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "Bob leads nothing");

        let (_, body, _) = get_as(&host.app, &format!("/api/v1/teams/{qa}/positions"), EVE).await;
        let positions = body.as_array().expect("positions");
        let engineer = positions.iter().find(|held| held["name"] == "engineer").expect("inherited");
        assert_eq!(engineer["title"], "Platform Engineer");
        assert_eq!(
            engineer["responsibilities"],
            json!(["Writes code", "Reviews code", "Runs the platform"])
        );
        assert_eq!(engineer["origin"], "organisation");
        assert_eq!(engineer["changed_by"], json!(["platform"]));
        assert_eq!(engineer["changed_here"], false);
        let qa_engineer = positions.iter().find(|held| held["name"] == "qa").expect("its own");
        assert_eq!(qa_engineer["origin"], "qa");
        let (_, body, _) = get_as(&host.app, &format!("/api/v1/teams/{platform}"), EVE).await;
        assert_eq!(body["arranges"], false);
        assert!(
            !body["positions"].as_array().unwrap().iter().any(|held| held["name"] == "qa"),
            "a team's own position is not its parent's"
        );
    }

    #[tokio::test]
    async fn a_position_taken_away_leaves_whoever_held_it_holding_none() {
        let (host, people) = host();
        let [_, bob, eve] = &people;
        let (organisation, platform, qa) = led(&host, &people).await;
        let engineer = format!("/api/v1/organisations/{organisation}/positions/engineer");
        let (status, _, _) =
            crate::testing::put_json(&host.app, &engineer, ADMIN, json!({ "title": "Engineer" }))
                .await;
        assert_eq!(status, StatusCode::CREATED);

        let holds = format!("/api/v1/teams/{qa}/members/{}/position", bob.id);
        let (status, body, _) =
            crate::testing::put_json(&host.app, &holds, ADA, json!({ "position": "engineer" }))
                .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _, _) =
            crate::testing::put_json(&host.app, &holds, ADA, json!({ "position": "astronaut" }))
                .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "only a position the team has");
        let stranger = format!("/api/v1/teams/{qa}/members/{}/position", eve.id);
        let (status, _, _) =
            crate::testing::put_json(&host.app, &stranger, ADA, json!({ "position": "engineer" }))
                .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "Eve is not in QA");

        // Platform hides it, so QA below has it no more and Bob holds nothing.
        let hide = format!("/api/v1/teams/{platform}/positions/engineer");
        let (status, body, _) =
            crate::testing::put_json(&host.app, &hide, ADA, json!({ "hidden": true })).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["vacated"], json!([{ "team": qa, "user": bob.id }]));
        let (_, body, _) = get_as(&host.app, &format!("/api/v1/teams/{qa}"), BOB).await;
        let held = body["members"].as_array().unwrap().iter().find(|held| held["login"] == "bob");
        assert_eq!(held.unwrap()["position"], Value::Null);

        // A lead is one of the team's members, and stops leading it on leaving.
        let lead = format!("/api/v1/teams/{qa}/lead");
        let (status, _, _) =
            crate::testing::put_json(&host.app, &lead, ADA, json!({ "user": eve.id })).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (status, _, _) =
            crate::testing::put_json(&host.app, &lead, ADA, json!({ "user": bob.id })).await;
        assert_eq!(status, StatusCode::OK);
        let leaving = format!("/api/v1/teams/{qa}/members/{}", bob.id);
        let (status, _, _) = delete_as(&host.app, &leaving, ADMIN).await;
        assert_eq!(status, StatusCode::NO_CONTENT);
        let (_, body, _) = get_as(&host.app, &format!("/api/v1/teams/{qa}"), ADA).await;
        assert_eq!(body["lead_id"], Value::Null);
    }

    async fn made_organisation(host: &Host, name: &str) -> String {
        made(host, "/api/v1/organisations", json!({ "name": name, "title": name.to_uppercase() }))
            .await
    }
}
