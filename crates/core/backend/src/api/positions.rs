//! A team's lead and the positions people hold in it (FEAT-TEAMS). An organisation defines its
//! positions and what each is responsible for; identity managers change them. Every team inherits
//! them through the teams above it, and its lead — or the lead of a team above it — changes, hides
//! or adds to them there, and says who holds which. Anyone signed in can see all of it.
//!
//! Someone holding a position a change takes away from their team is left holding none, and the
//! answer says who, rather than the change being refused: a team rearranging itself should not
//! have to empty a position by hand first.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path, State};
use http::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::AppState;
use super::iam::manages_identity;
use super::problem::Problem;
use super::teams::{announce, audit, description, manager, organisation, slug, team, title};
use crate::auth::Auth;
use crate::identity::Principal;
use crate::teams::{EffectivePosition, Position, Team, TeamMember, TeamPosition, chain, effective};

const MAX_RESPONSIBILITY: usize = 200;
const MAX_RESPONSIBILITIES: usize = 30;

#[derive(Debug, Deserialize)]
pub struct PositionBody {
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub responsibilities: Vec<String>,
}

/// What one team changes about a position. Everything left out is inherited; a position nothing
/// above the team has is the team's own and needs a title.
#[derive(Debug, Default, Deserialize)]
pub struct TeamPositionBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub added: Vec<String>,
    #[serde(default)]
    pub removed: Vec<String>,
    #[serde(default)]
    pub hidden: Option<bool>,
}

#[derive(Debug, Deserialize)]
pub struct LeadBody {
    pub user: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct HeldBody {
    pub position: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Holder {
    pub user_id: Uuid,
    pub login: String,
    pub name: Option<String>,
}

/// A position as a team has it, with who holds it there and what this team changed of it.
#[derive(Debug, Serialize)]
pub struct TeamPositionView {
    #[serde(flatten)]
    pub position: EffectivePosition,
    pub holders: Vec<Holder>,
    pub change: Option<TeamPosition>,
}

fn responsibilities(given: &[String]) -> Result<Vec<String>, Problem> {
    let mut kept: Vec<String> = Vec::new();
    for responsibility in given {
        let responsibility = responsibility.trim();
        if responsibility.is_empty() || kept.iter().any(|held| held == responsibility) {
            continue;
        }
        if responsibility.chars().count() > MAX_RESPONSIBILITY
            || responsibility.contains(char::is_control)
        {
            return Err(Problem::bad_request(format!(
                "a responsibility is one line of at most {MAX_RESPONSIBILITY} characters"
            )));
        }
        kept.push(responsibility.to_string());
    }
    match kept.len() <= MAX_RESPONSIBILITIES {
        true => Ok(kept),
        false => Err(Problem::bad_request(format!(
            "a position has at most {MAX_RESPONSIBILITIES} responsibilities"
        ))),
    }
}

/// Everything that decides an organisation's positions, read once.
pub(crate) struct Arrangement {
    pub teams: Vec<Team>,
    pub positions: Vec<Position>,
    pub changes: Vec<TeamPosition>,
}

impl Arrangement {
    pub(crate) async fn of(state: &AppState, organisation: Uuid) -> Result<Self, Problem> {
        let teams: Vec<Team> = state
            .repos
            .teams
            .teams()
            .await?
            .into_iter()
            .filter(|team| team.organisation_id == organisation)
            .collect();
        let positions = state.repos.teams.positions(Some(organisation)).await?;
        let changes = state
            .repos
            .teams
            .team_positions(None)
            .await?
            .into_iter()
            .filter(|change| teams.iter().any(|team| team.id == change.team_id))
            .collect();
        Ok(Self { teams, positions, changes })
    }

    /// The positions `team` has.
    pub(crate) fn positions_of(&self, team: Uuid) -> Vec<EffectivePosition> {
        let chain: Vec<(&Team, Vec<TeamPosition>)> = chain(&self.teams, team)
            .into_iter()
            .map(|above| {
                let changes = self.changes.iter().filter(|change| change.team_id == above.id);
                (above, changes.cloned().collect())
            })
            .collect();
        effective(&self.positions, &chain)
    }

    /// Whether someone in `team` can hold `name`.
    fn holdable(&self, team: Uuid, name: &str) -> bool {
        self.positions_of(team).iter().any(|position| position.name == name && !position.hidden)
    }
}

/// Leaves everyone in `organisation` whose team no longer has the position they held holding none,
/// and returns who they were.
async fn settle(state: &AppState, organisation: Uuid) -> Result<Vec<TeamMember>, Problem> {
    let arranged = Arrangement::of(state, organisation).await?;
    let mut gone: Vec<(Uuid, String)> = Vec::new();
    for held in state.repos.teams.members(None).await? {
        let Some(name) = &held.position else { continue };
        if arranged.teams.iter().any(|team| team.id == held.team_id)
            && !arranged.holdable(held.team_id, name)
            && !gone.contains(&(held.team_id, name.clone()))
        {
            gone.push((held.team_id, name.clone()));
        }
    }
    let mut vacated = Vec::new();
    for (team, name) in gone {
        vacated.extend(state.repos.teams.vacate_position(&[team], &name).await?);
    }
    Ok(vacated)
}

fn vacated(held: &[TeamMember]) -> Value {
    json!(
        held.iter()
            .map(|held| json!({ "team": held.team_id, "user": held.user_id }))
            .collect::<Vec<_>>()
    )
}

/// An identity manager, or the lead of `team` or of a team above it.
pub(crate) async fn arranges(state: &AppState, principal: &Principal, team: &Team) -> bool {
    if manages_identity(state, principal).await {
        return true;
    }
    let Some(user) = principal.as_user() else { return false };
    let Ok(teams) = state.repos.teams.teams().await else { return false };
    chain(&teams, team.id).iter().any(|above| above.lead_id == Some(user.id))
}

async fn arranger(state: &AppState, principal: &Principal, team: &Team) -> Result<(), Problem> {
    match arranges(state, principal, team).await {
        true => Ok(()),
        false => Err(Problem::forbidden(
            "a team is arranged by its lead, the lead of a team above it, or an identity manager",
        )),
    }
}

pub async fn list_organisation_positions(
    State(state): State<AppState>,
    _auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<Position>>, Problem> {
    organisation(&state, id).await?;
    Ok(Json(state.repos.teams.positions(Some(id)).await?))
}

/// Makes the organisation's position of that name, or changes it.
pub async fn put_organisation_position(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, name)): Path<(Uuid, String)>,
    Json(body): Json<PositionBody>,
) -> Result<(StatusCode, Json<Position>), Problem> {
    manager(&state, &auth.0).await?;
    organisation(&state, id).await?;
    let name = slug(&name, "a position's name")?;
    let existed = state.repos.teams.positions(Some(id)).await?.iter().any(|held| held.name == name);
    let saved = state
        .repos
        .teams
        .put_position(
            id,
            &name,
            &title(&body.title)?,
            &description(&body.description)?,
            &responsibilities(&body.responsibilities)?,
        )
        .await?;
    let action = if existed { "position.changed" } else { "position.created" };
    audit(&state, &auth.0, action, id, json!({ "position": name })).await;
    announce(&state, "organisation.changed", json!({ "id": id, "positions": [name] })).await;
    let status = if existed { StatusCode::OK } else { StatusCode::CREATED };
    Ok((status, Json(saved)))
}

pub async fn delete_organisation_position(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<Json<Value>, Problem> {
    manager(&state, &auth.0).await?;
    organisation(&state, id).await?;
    state
        .repos
        .teams
        .delete_position(id, &name)
        .await?
        .ok_or_else(|| Problem::not_found("position"))?;
    // A team that only changed it has nothing left to change; one that gave it a title of its own
    // keeps it as its own position.
    let arranged = Arrangement::of(&state, id).await?;
    for change in arranged.changes.iter().filter(|change| change.name == name) {
        if change.title.is_none() {
            state.repos.teams.delete_team_position(change.team_id, &name).await?;
        }
    }
    let held = settle(&state, id).await?;
    audit(&state, &auth.0, "position.deleted", id, json!({ "position": name })).await;
    announce(&state, "organisation.changed", json!({ "id": id, "positions": [name] })).await;
    Ok(Json(json!({ "vacated": vacated(&held) })))
}

pub(super) async fn team_positions(
    state: &AppState,
    team: &Team,
) -> Result<Vec<TeamPositionView>, Problem> {
    let arranged = Arrangement::of(state, team.organisation_id).await?;
    let members = state.repos.teams.members(Some(team.id)).await?;
    let mut people: HashMap<Uuid, Holder> = HashMap::new();
    for held in members.iter().filter(|held| held.position.is_some()) {
        if let Some(user) = state.repos.identity.user_by_id(held.user_id).await? {
            people.insert(user.id, Holder { user_id: user.id, login: user.login, name: user.name });
        }
    }
    Ok(arranged
        .positions_of(team.id)
        .into_iter()
        .map(|position| {
            let mut holders: Vec<Holder> = members
                .iter()
                .filter(|held| held.position.as_deref() == Some(position.name.as_str()))
                .filter_map(|held| people.remove(&held.user_id))
                .collect();
            holders.sort_by_key(|holder| holder.login.to_lowercase());
            let change = arranged
                .changes
                .iter()
                .find(|change| change.team_id == team.id && change.name == position.name)
                .cloned();
            TeamPositionView { position, holders, change }
        })
        .collect())
}

pub async fn list_team_positions(
    State(state): State<AppState>,
    _auth: Auth,
    Path(id): Path<Uuid>,
) -> Result<Json<Vec<TeamPositionView>>, Problem> {
    let found = team(&state, id).await?;
    Ok(Json(team_positions(&state, &found).await?))
}

/// Replaces what the team changes about one position; a change of nothing is no change at all.
pub async fn put_team_position(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, name)): Path<(Uuid, String)>,
    Json(body): Json<TeamPositionBody>,
) -> Result<Json<Value>, Problem> {
    let found = team(&state, id).await?;
    arranger(&state, &auth.0, &found).await?;
    let name = slug(&name, "a position's name")?;
    let arranged = Arrangement::of(&state, found.organisation_id).await?;
    let above = match found.parent_id {
        Some(parent) => arranged.positions_of(parent),
        None => effective(&arranged.positions, &[]),
    };
    let inherited = above.iter().any(|position| position.name == name);
    let change = TeamPosition {
        team_id: id,
        name: name.clone(),
        title: body.title.as_deref().map(title).transpose()?,
        description: body.description.as_deref().map(description).transpose()?,
        added: responsibilities(&body.added)?,
        removed: responsibilities(&body.removed)?,
        hidden: body.hidden,
        created_at: chrono::Utc::now(),
    };
    if !inherited && change.title.is_none() {
        return Err(Problem::bad_request(format!(
            "nothing above this team has a position called {name}, so give it a title to make it \
             this team's own"
        )));
    }
    let nothing = change.title.is_none()
        && change.description.is_none()
        && change.added.is_empty()
        && change.removed.is_empty()
        && change.hidden.is_none();
    match nothing {
        true => {
            state.repos.teams.delete_team_position(id, &name).await?;
        }
        false => state.repos.teams.put_team_position(&change).await?,
    }
    let held = settle(&state, found.organisation_id).await?;
    audit(&state, &auth.0, "team.position.changed", id, json!({ "position": name })).await;
    announce(&state, "team.changed", json!({ "id": id, "positions": [name] })).await;
    Ok(Json(json!({ "vacated": vacated(&held) })))
}

/// Takes back what the team changed about a position, or removes a position of its own.
pub async fn delete_team_position(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, name)): Path<(Uuid, String)>,
) -> Result<Json<Value>, Problem> {
    let found = team(&state, id).await?;
    arranger(&state, &auth.0, &found).await?;
    if !state.repos.teams.delete_team_position(id, &name).await? {
        return Err(Problem::not_found("position change"));
    }
    let held = settle(&state, found.organisation_id).await?;
    audit(&state, &auth.0, "team.position.reverted", id, json!({ "position": name })).await;
    announce(&state, "team.changed", json!({ "id": id, "positions": [name] })).await;
    Ok(Json(json!({ "vacated": vacated(&held) })))
}

/// Makes one of the team's members its lead, or leaves it without one.
pub async fn set_lead(
    State(state): State<AppState>,
    auth: Auth,
    Path(id): Path<Uuid>,
    Json(body): Json<LeadBody>,
) -> Result<Json<Team>, Problem> {
    let found = team(&state, id).await?;
    arranger(&state, &auth.0, &found).await?;
    if let Some(user) = body.user {
        let members = state.repos.teams.members(Some(id)).await?;
        if !members.iter().any(|held| held.user_id == user) {
            return Err(Problem::conflict("a team's lead is one of its members: add them first"));
        }
    }
    let updated = state
        .repos
        .teams
        .set_lead(id, body.user)
        .await?
        .ok_or_else(|| Problem::not_found("team"))?;
    audit(&state, &auth.0, "team.lead.changed", id, json!({ "lead": body.user })).await;
    announce(&state, "team.changed", json!({ "id": id, "lead": body.user })).await;
    Ok(Json(updated))
}

/// The position someone holds in the team: one it has and does not hide, or none.
pub async fn set_member_position(
    State(state): State<AppState>,
    auth: Auth,
    Path((id, user)): Path<(Uuid, Uuid)>,
    Json(body): Json<HeldBody>,
) -> Result<Json<TeamMember>, Problem> {
    let found = team(&state, id).await?;
    arranger(&state, &auth.0, &found).await?;
    let position = body.position.as_deref().map(str::trim).filter(|name| !name.is_empty());
    if let Some(name) = position {
        let arranged = Arrangement::of(&state, found.organisation_id).await?;
        if !arranged.holdable(id, name) {
            return Err(Problem::bad_request(format!("this team has no position called {name}")));
        }
    }
    let held = state
        .repos
        .teams
        .set_member_position(id, user, position)
        .await?
        .ok_or_else(|| Problem::not_found("member"))?;
    let detail = json!({ "user": user, "position": position });
    audit(&state, &auth.0, "team.member.position", id, detail.clone()).await;
    announce(&state, "team.changed", json!({ "id": id, "member_position": detail })).await;
    Ok(Json(held))
}
