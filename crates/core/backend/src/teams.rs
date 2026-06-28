//! Organisations and teams (ADR-0004): who works together, in core, so that plugins can drive them
//! and RBAC can grant through them. A team is in one organisation and can sit inside another team
//! of it. Its members are users, never service accounts.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Organisation {
    pub id: Uuid,
    /// Unique, and what URLs and plugins name it by.
    pub name: String,
    pub title: String,
    pub description: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Team {
    pub id: Uuid,
    pub organisation_id: Uuid,
    pub parent_id: Option<Uuid>,
    /// Unique within its organisation.
    pub name: String,
    pub title: String,
    pub description: String,
    /// Where to write to the team; empty for none (ADR-0004, settled in T69).
    pub email: String,
    /// Everyone is placed in every default team as they are made.
    #[serde(rename = "default")]
    pub is_default: bool,
    /// The plugin that provides the team, which alone changes it; `None` for one made in DOC.
    pub provider: Option<String>,
    /// The provider's key for the team, such as `acme/platform`.
    pub external_id: Option<String>,
    /// One of its members, who looks after it (FEAT-TEAMS): its positions, and who holds them.
    pub lead_id: Option<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// How someone came to be in a team: added by hand, placed in a default team, or by its provider.
pub const BY_HAND: &str = "admin";
pub const BY_DEFAULT: &str = "default";
pub const BY_PROVIDER: &str = "provider";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct TeamMember {
    pub team_id: Uuid,
    pub user_id: Uuid,
    /// `admin`, `default` or `provider`.
    pub source: String,
    /// The plugin that added them, when `source` is `provider`.
    pub provider: Option<String>,
    /// The name of the position they hold in this team, one of those it has.
    pub position: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct NewTeam {
    pub organisation_id: Uuid,
    pub parent_id: Option<Uuid>,
    pub name: String,
    pub title: String,
    pub description: String,
    pub email: String,
    pub is_default: bool,
    /// The plugin that provides it and its key there.
    pub provided: Option<(String, String)>,
}

/// What an update changes; `None` leaves a field as it is, and `parent: Some(None)` makes a team
/// top-level.
#[derive(Debug, Clone, Default)]
pub struct TeamChanges {
    pub name: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub email: Option<String>,
    pub parent: Option<Option<Uuid>>,
    pub is_default: Option<bool>,
}

/// Who owns a service account (ADR-0004): the platform, one user, or a team whose members manage it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountOwner {
    Platform,
    User(Uuid),
    Team(Uuid),
}

/// The service accounts someone may manage without administering identity: their own, and those of
/// every team they reach.
#[derive(Debug, Clone)]
pub struct Owners {
    pub user: Uuid,
    pub teams: Vec<Uuid>,
}

/// Every team a person is in, and every team above those: what the teams hold flows down to them.
pub fn reach(teams: &[Team], memberships: &[TeamMember]) -> Vec<Uuid> {
    let mut reached: Vec<Uuid> = Vec::new();
    for membership in memberships {
        let mut next = Some(membership.team_id);
        while let Some(id) = next {
            if reached.contains(&id) {
                break;
            }
            reached.push(id);
            next = teams.iter().find(|team| team.id == id).and_then(|team| team.parent_id);
        }
    }
    reached
}

/// Whether making `parent` the parent of `team` would put the team below itself.
pub fn below_itself(teams: &[Team], team: Uuid, parent: Uuid) -> bool {
    let mut next = Some(parent);
    let mut seen = 0;
    while let Some(id) = next {
        if id == team || seen > teams.len() {
            return true;
        }
        seen += 1;
        next = teams.iter().find(|candidate| candidate.id == id).and_then(|found| found.parent_id);
    }
    false
}

/// A position people hold across an organisation (FEAT-TEAMS), such as Senior Software Engineer,
/// and what it is responsible for. Every team has it unless the team, or one above it, changes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct Position {
    pub id: Uuid,
    pub organisation_id: Uuid,
    /// Unique in its organisation, and never changed, since teams and members name it.
    pub name: String,
    pub title: String,
    pub description: String,
    pub responsibilities: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// One team's change to a position it inherits, or a position of its own when nothing above it has
/// one of that name. `None` keeps what comes from above.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct TeamPosition {
    pub team_id: Uuid,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// Responsibilities added to the inherited ones, and inherited ones dropped.
    pub added: Vec<String>,
    pub removed: Vec<String>,
    /// `Some(true)` hides an inherited position here and below; `Some(false)` shows it again.
    pub hidden: Option<bool>,
    pub created_at: DateTime<Utc>,
}

/// A position as one team has it, once everything above it has had its say.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectivePosition {
    pub name: String,
    pub title: String,
    pub description: String,
    pub responsibilities: Vec<String>,
    /// A hidden position is not held in this team; it is listed so that it can be shown again.
    pub hidden: bool,
    /// Where it was defined: `organisation`, or the name of the team that made it.
    pub origin: String,
    /// The teams, from the top down, that changed it on the way.
    pub changed_by: Vec<String>,
    /// Whether this team itself changed or made it.
    pub changed_here: bool,
}

/// The positions a team has: the organisation's, changed by each team from the top of the
/// organisation down to it. `chain` is those teams in that order, each with its own changes.
pub fn effective(
    organisation: &[Position],
    chain: &[(&Team, Vec<TeamPosition>)],
) -> Vec<EffectivePosition> {
    let mut positions: Vec<EffectivePosition> = organisation
        .iter()
        .map(|position| EffectivePosition {
            name: position.name.clone(),
            title: position.title.clone(),
            description: position.description.clone(),
            responsibilities: position.responsibilities.clone(),
            hidden: false,
            origin: "organisation".into(),
            changed_by: Vec::new(),
            changed_here: false,
        })
        .collect();
    let last = chain.len().saturating_sub(1);
    for (depth, (team, changes)) in chain.iter().enumerate() {
        let here = depth == last;
        for change in changes {
            match positions.iter_mut().find(|position| position.name == change.name) {
                Some(position) => {
                    if let Some(title) = &change.title {
                        position.title.clone_from(title);
                    }
                    if let Some(description) = &change.description {
                        position.description.clone_from(description);
                    }
                    position.responsibilities.retain(|held| !change.removed.contains(held));
                    for added in &change.added {
                        if !position.responsibilities.contains(added) {
                            position.responsibilities.push(added.clone());
                        }
                    }
                    if let Some(hidden) = change.hidden {
                        position.hidden = hidden;
                    }
                    position.changed_by.push(team.name.clone());
                    position.changed_here |= here;
                }
                // Nothing above has one of that name: it is this team's own, if it says what it is.
                None => {
                    let Some(title) = &change.title else { continue };
                    positions.push(EffectivePosition {
                        name: change.name.clone(),
                        title: title.clone(),
                        description: change.description.clone().unwrap_or_default(),
                        responsibilities: change.added.clone(),
                        hidden: change.hidden.unwrap_or(false),
                        origin: team.name.clone(),
                        changed_by: Vec::new(),
                        changed_here: here,
                    });
                }
            }
        }
    }
    positions.sort_by_key(|position| position.title.to_lowercase());
    positions
}

/// The teams from the top of `team`'s organisation down to `team` itself.
pub fn chain(teams: &[Team], team: Uuid) -> Vec<&Team> {
    let mut found = Vec::new();
    let mut next = Some(team);
    while let Some(id) = next {
        let Some(at) = teams.iter().find(|candidate| candidate.id == id) else { break };
        if found.iter().any(|seen: &&Team| seen.id == at.id) {
            break;
        }
        found.insert(0, at);
        next = at.parent_id;
    }
    found
}
