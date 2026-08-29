//! Who is who, from core: organisations, teams and their leads, people, service accounts and
//! plugins. It decides who manages an owner's secrets and accounts, and whom an allowance covers.

use std::collections::BTreeMap;

use doc_plugin_sdk::{Backend, Query};
use serde::Deserialize;
use uuid::Uuid;

use crate::Refusal;
use crate::store::or_default;

#[derive(Debug, Clone, Deserialize)]
pub struct Organisation {
    pub id: Uuid,
    pub name: String,
    #[serde(default, deserialize_with = "or_default")]
    pub title: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Team {
    pub id: Uuid,
    #[serde(default)]
    pub parent_id: Option<Uuid>,
    pub name: String,
    #[serde(default, deserialize_with = "or_default")]
    pub title: String,
    #[serde(default)]
    pub lead_id: Option<Uuid>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Person {
    pub id: Uuid,
    pub login: String,
    #[serde(default)]
    pub organisation_id: Option<Uuid>,
    #[serde(default, deserialize_with = "or_default")]
    pub disabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct Membership {
    team_id: Uuid,
    user_id: Uuid,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServiceAccount {
    pub id: Uuid,
    pub name: String,
    #[serde(default, deserialize_with = "or_default")]
    pub disabled: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Plugin {
    pub id: String,
    #[serde(default, deserialize_with = "or_default")]
    pub display_name: String,
}

impl Plugin {
    pub fn shown(&self) -> String {
        match self.display_name.is_empty() {
            true => self.id.clone(),
            false => format!("{} ({})", self.display_name, self.id),
        }
    }
}

/// Everything core says about who is who, read once for a request or a run.
pub struct Directory {
    pub organisations: Vec<Organisation>,
    pub teams: Vec<Team>,
    pub people: BTreeMap<Uuid, Person>,
    memberships: Vec<Membership>,
    pub services: Vec<ServiceAccount>,
    pub plugins: Vec<Plugin>,
}

fn unavailable(err: impl std::fmt::Display) -> Refusal {
    Refusal::unavailable(format!("who is who could not be read from core: {err}"))
}

impl Directory {
    pub async fn read(backend: &Backend) -> Result<Self, Refusal> {
        let organisations: Vec<Organisation> = backend
            .query_all(Query::new("core.organisations").fields(&["id", "name", "title"]))
            .await
            .map_err(unavailable)?;
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
            .query_all(Query::new("core.users").fields(&[
                "id",
                "login",
                "organisation_id",
                "disabled",
            ]))
            .await
            .map_err(unavailable)?;
        let memberships: Vec<Membership> = backend
            .query_all(Query::new("core.team-members").fields(&["team_id", "user_id"]))
            .await
            .map_err(unavailable)?;
        let services: Vec<ServiceAccount> = backend
            .query_all(Query::new("core.service-accounts").fields(&["id", "name", "disabled"]))
            .await
            .map_err(unavailable)?;
        let mut plugins: Vec<Plugin> = backend
            .query_all(Query::new("core.plugins").fields(&["id", "display_name"]))
            .await
            .map_err(unavailable)?;
        plugins.sort_by(|one, two| one.id.cmp(&two.id));
        Ok(Self {
            organisations,
            teams,
            people: people.into_iter().map(|person| (person.id, person)).collect(),
            memberships,
            services,
            plugins,
        })
    }

    pub fn team(&self, id: Uuid) -> Option<&Team> {
        self.teams.iter().find(|team| team.id == id)
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

    /// Whether `user` leads the team or a team above it, which is who arranges it.
    pub fn arranges(&self, user: Uuid, team: Uuid) -> bool {
        self.chain(team).iter().any(|above| above.lead_id == Some(user))
    }

    /// The lead of the team, or of the nearest team above it that has one.
    pub fn lead_for(&self, team: Uuid) -> Option<Uuid> {
        self.chain(team).iter().find_map(|above| above.lead_id)
    }

    /// Whether `user` is in the team or in a team inside it.
    pub fn within(&self, user: Uuid, team: Uuid) -> bool {
        self.memberships
            .iter()
            .filter(|held| held.user_id == user)
            .any(|held| self.chain(held.team_id).iter().any(|above| above.id == team))
    }

    /// What an owner or an allowance's `who` is called on a page.
    pub fn label(&self, reference: &str) -> String {
        let Some((kind, id)) = reference.split_once(':') else { return reference.to_string() };
        let uuid = id.parse::<Uuid>().ok();
        let named = match kind {
            "organisation" => uuid
                .and_then(|id| self.organisations.iter().find(|found| found.id == id))
                .map(|found| format!("{} (everyone in it)", titled(&found.title, &found.name))),
            "team" => uuid.and_then(|id| self.team(id)).map(|team| {
                let title = titled(&team.title, &team.name);
                format!("{title} (team)")
            }),
            "user" => uuid.and_then(|id| self.people.get(&id)).map(|person| person.login.clone()),
            "service" => uuid
                .and_then(|id| self.services.iter().find(|found| found.id == id))
                .map(|found| format!("{} (service account)", found.name)),
            "plugin" => self.plugins.iter().find(|found| found.id == id).map(|found| {
                match found.display_name.is_empty() {
                    true => format!("the {} plugin", found.id),
                    false => format!("{} (the {} plugin)", found.display_name, found.id),
                }
            }),
            _ => None,
        };
        named.unwrap_or_else(|| reference.to_string())
    }

    /// Whose a secret or an account is, as a page says it.
    pub fn owner_label(&self, reference: &str) -> String {
        match reference.split_once(':') {
            Some(("team", _)) => format!("{} (team)", self.owner_name(reference)),
            _ => self.owner_name(reference),
        }
    }

    /// An owner's name without saying what kind it is, for a secret's label.
    pub fn owner_name(&self, reference: &str) -> String {
        let Some((kind, id)) = reference.split_once(':') else { return reference.to_string() };
        let uuid = id.parse::<Uuid>().ok();
        let named = match kind {
            "organisation" => uuid
                .and_then(|id| self.organisations.iter().find(|found| found.id == id))
                .map(|found| titled(&found.title, &found.name)),
            "team" => uuid.and_then(|id| self.team(id)).map(|team| titled(&team.title, &team.name)),
            "user" => uuid.and_then(|id| self.people.get(&id)).map(|person| person.login.clone()),
            _ => None,
        };
        named.unwrap_or_else(|| reference.to_string())
    }
}

fn titled(title: &str, name: &str) -> String {
    match title.trim().is_empty() {
        true => name.to_string(),
        false => title.to_string(),
    }
}

/// Who is asking, as far as managing and asking go.
#[derive(Debug, Clone)]
pub struct Me {
    /// `user`, `service`, `plugin` or `platform`.
    pub kind: String,
    pub id: Option<Uuid>,
    pub plugin: Option<String>,
    pub login: String,
    /// A platform administrator, or somebody with this plugin's custom `admin` permission.
    pub admin: bool,
    pub writes: bool,
}

impl Me {
    pub fn of(backend: &Backend) -> Self {
        let Some(caller) = backend.caller() else {
            return Self {
                kind: "platform".into(),
                id: None,
                plugin: None,
                login: "Secret Storage".into(),
                admin: false,
                writes: false,
            };
        };
        let id = caller.id.as_deref().and_then(|id| id.parse().ok());
        Self {
            kind: caller.kind.clone(),
            id,
            plugin: (caller.kind == "plugin").then(|| caller.id.clone()).flatten(),
            login: caller.label.clone().or_else(|| caller.id.clone()).unwrap_or_default(),
            admin: caller.admin || backend.allows(crate::ADMIN, true),
            writes: backend.writes(),
        }
    }

    /// `user:<id>`, `service:<id>` or `plugin:<id>`, as an allowance or a token names them.
    pub fn reference(&self) -> String {
        match (self.kind.as_str(), self.id, &self.plugin) {
            ("user", Some(id), _) => format!("user:{id}"),
            ("service", Some(id), _) => format!("service:{id}"),
            ("plugin", _, Some(plugin)) => format!("plugin:{plugin}"),
            _ => self.kind.clone(),
        }
    }

    pub fn is_person(&self) -> bool {
        self.kind == "user" && self.id.is_some()
    }

    /// Whether they manage what `owner` owns: an organisation's is an administrator's, a team's
    /// whoever arranges it, and a person's their own.
    pub fn manages(&self, directory: &Directory, owner: &str) -> bool {
        if self.admin {
            return true;
        }
        let (Some((kind, id)), Some(me)) =
            (owner.split_once(':'), self.id.filter(|_| self.is_person()))
        else {
            return false;
        };
        let Ok(id) = id.parse::<Uuid>() else { return false };
        match kind {
            "team" => directory.arranges(me, id),
            "user" => me == id,
            _ => false,
        }
    }

    /// The owners they may store a secret or onboard an account for, as `(reference, label)`.
    pub fn owners(&self, directory: &Directory) -> Vec<(String, String)> {
        let mut owners = Vec::new();
        if let (true, Some(me)) = (self.is_person(), self.id) {
            owners.push((format!("user:{me}"), format!("Yourself ({})", self.login)));
        }
        let mut teams: Vec<&Team> = directory
            .teams
            .iter()
            .filter(|team| self.manages(directory, &format!("team:{}", team.id)))
            .collect();
        teams.sort_by_key(|team| titled(&team.title, &team.name).to_lowercase());
        owners.extend(teams.into_iter().map(|team| {
            (format!("team:{}", team.id), directory.label(&format!("team:{}", team.id)))
        }));
        if self.admin {
            for organisation in &directory.organisations {
                let title = titled(&organisation.title, &organisation.name);
                owners.push((
                    format!("organisation:{}", organisation.id),
                    format!("{title} (organisation)"),
                ));
            }
        }
        owners
    }

    /// Whether an allowance naming `who` covers them.
    pub fn covered(&self, directory: &Directory, who: &str) -> bool {
        let Some((kind, id)) = who.split_once(':') else { return false };
        match (kind, self.kind.as_str()) {
            ("plugin", "plugin") => self.plugin.as_deref() == Some(id),
            ("service", "service") | ("user", "user") => {
                self.id.is_some_and(|me| id.parse::<Uuid>().is_ok_and(|named| named == me))
            }
            ("team", "user") => match (self.id, id.parse::<Uuid>()) {
                (Some(me), Ok(team)) => directory.within(me, team),
                _ => false,
            },
            ("organisation", "user") => match (self.id, id.parse::<Uuid>()) {
                (Some(me), Ok(organisation)) => directory
                    .people
                    .get(&me)
                    .is_some_and(|person| person.organisation_id == Some(organisation)),
                _ => false,
            },
            _ => false,
        }
    }
}
