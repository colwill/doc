//! Teams, their leads and their people, from core (FEAT-TEAMS). A team's rotas are arranged, and
//! its people's holidays recorded, by its lead or the lead of a team above it, or an administrator.

use std::collections::BTreeMap;

use doc_plugin_sdk::{Backend, Caller, Query};
use serde::Deserialize;
use uuid::Uuid;

use crate::Refusal;

#[derive(Debug, Clone, Deserialize)]
pub struct Team {
    pub id: Uuid,
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    pub name: String,
    pub title: String,
    #[serde(default)]
    pub lead_id: Option<Uuid>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Person {
    pub id: Uuid,
    pub login: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub disabled: bool,
}

impl Person {
    pub fn label(&self) -> String {
        match &self.name {
            Some(name) if !name.is_empty() => format!("{} ({name})", self.login),
            _ => self.login.clone(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Membership {
    pub team_id: Uuid,
    pub user_id: Uuid,
    #[serde(default)]
    pub position: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct TeamPosition {
    team_id: Uuid,
    name: String,
    title: String,
}

/// Everything core says about teams and people, read once for a request or a run.
pub struct Directory {
    pub teams: Vec<Team>,
    pub people: BTreeMap<Uuid, Person>,
    pub memberships: Vec<Membership>,
    positions: Vec<TeamPosition>,
}

fn unavailable(err: impl std::fmt::Display) -> Refusal {
    Refusal::unavailable(format!("teams could not be read from core: {err}"))
}

impl Directory {
    pub async fn read(backend: &Backend) -> Result<Self, Refusal> {
        let teams: Vec<Team> = backend
            .query_all(Query::new("core.teams").fields(&[
                "id",
                "parent_id",
                "name",
                "title",
                "lead_id",
            ]))
            .await
            .map_err(unavailable)?;
        let people: Vec<Person> = backend
            .query_all(Query::new("core.users").fields(&["id", "login", "name", "disabled"]))
            .await
            .map_err(unavailable)?;
        let memberships: Vec<Membership> = backend
            .query_all(Query::new("core.team-members").fields(&["team_id", "user_id", "position"]))
            .await
            .map_err(unavailable)?;
        // An older core has no positions to give; a team simply has none then.
        let positions: Vec<TeamPosition> = backend
            .query_all(Query::new("core.team-positions").fields(&["team_id", "name", "title"]))
            .await
            .unwrap_or_default();
        Ok(Self {
            teams,
            people: people.into_iter().map(|person| (person.id, person)).collect(),
            memberships,
            positions,
        })
    }

    pub fn team(&self, id: Uuid) -> Option<&Team> {
        self.teams.iter().find(|team| team.id == id)
    }

    pub fn team_named(&self, name: &str) -> Option<&Team> {
        self.teams.iter().find(|team| team.name.eq_ignore_ascii_case(name))
    }

    pub fn person_named(&self, login: &str) -> Option<&Person> {
        self.people.values().find(|person| person.login.eq_ignore_ascii_case(login))
    }

    /// The team's people, in login order, with the title of the position each holds there.
    pub fn members(&self, team: Uuid) -> Vec<(&Person, Option<String>)> {
        let mut found: Vec<(&Person, Option<String>)> = self
            .memberships
            .iter()
            .filter(|held| held.team_id == team)
            .filter_map(|held| {
                let person = self.people.get(&held.user_id)?;
                let title = held.position.as_ref().map(|name| {
                    self.positions
                        .iter()
                        .find(|position| position.team_id == team && &position.name == name)
                        .map_or_else(|| name.clone(), |position| position.title.clone())
                });
                Some((person, title))
            })
            .filter(|(person, _)| !person.disabled)
            .collect();
        found.sort_by_key(|(person, _)| person.login.to_lowercase());
        found
    }

    pub fn is_member(&self, team: Uuid, login: &str) -> bool {
        self.members(team).iter().any(|(person, _)| person.login == login)
    }

    /// The team and every team above it.
    fn chain(&self, team: Uuid) -> Vec<&Team> {
        let mut found: Vec<&Team> = Vec::new();
        let mut next = Some(team);
        while let Some(id) = next {
            let Some(at) = self.team(id) else { break };
            if found.iter().any(|seen| seen.id == at.id) {
                break;
            }
            found.push(at);
            next = at.parent_id;
        }
        found
    }

    /// Whether `user` leads the team or a team above it.
    pub fn leads(&self, user: Uuid, team: Uuid) -> bool {
        self.chain(team).iter().any(|above| above.lead_id == Some(user))
    }

    /// The teams `user` arranges: each they lead and every team inside those.
    pub fn led_by(&self, user: Uuid) -> Vec<&Team> {
        let mut led: Vec<&Team> =
            self.teams.iter().filter(|team| self.leads(user, team.id)).collect();
        led.sort_by_key(|team| team.title.to_lowercase());
        led
    }

    /// The lead of the team, or failing that of the nearest team above it: who is told of a gap.
    pub fn lead_for(&self, team: Uuid) -> Option<&Person> {
        self.chain(team).iter().find_map(|above| above.lead_id.and_then(|id| self.people.get(&id)))
    }

    /// The teams someone is in.
    pub fn teams_of(&self, user: Uuid) -> Vec<&Team> {
        self.memberships
            .iter()
            .filter(|held| held.user_id == user)
            .filter_map(|held| self.team(held.team_id))
            .collect()
    }
}

/// Who is asking: a person's ID and login, and whether they administer the platform.
#[derive(Debug, Clone)]
pub struct Me {
    pub id: Option<Uuid>,
    pub login: String,
    pub admin: bool,
}

impl Me {
    pub fn of(backend: &Backend) -> Result<Self, Refusal> {
        match backend.caller() {
            Some(Caller { kind, id: Some(id), label, admin, .. }) if kind == "user" => Ok(Self {
                id: id.parse().ok(),
                login: label.clone().unwrap_or_else(|| id.clone()),
                admin: *admin,
            }),
            Some(Caller { kind, id: Some(id), label, admin, .. }) if kind == "service" => {
                Ok(Self {
                    id: None,
                    login: label.clone().unwrap_or_else(|| id.clone()),
                    admin: *admin,
                })
            }
            _ => Err(Refusal::forbidden("rotas are for people and service accounts")),
        }
    }

    /// Whether they arrange `team`'s rotas and record its people's holidays.
    pub fn arranges(&self, directory: &Directory, team: Uuid) -> bool {
        self.admin || self.id.is_some_and(|id| directory.leads(id, team))
    }

    /// Whether they record holidays for `person`: they lead a team the person is in.
    pub fn records_for(&self, directory: &Directory, person: Uuid) -> bool {
        self.admin
            || self.id.is_some_and(|id| {
                directory.teams_of(person).iter().any(|team| directory.leads(id, team.id))
            })
    }
}
