//! A team's own page: who is in it, the roles it holds and what they grant, the services it owns or
//! serves, and what those depend on. The team itself is the platform's (T69); this page shows what
//! the catalogue knows about it and what it is connected to. Roles, their permissions and their
//! holders are read from `rbac` as the viewer, narrowed to the team's members, as the explorer
//! narrows them.

use std::collections::BTreeMap;

use askama::Template;
use doc_plugin_sdk::Backend;
use serde_json::Value;

use super::{Flash, Panel, href, offered, render};
use crate::graph::Graph;
use crate::kinds::{Kind, Ref};
use crate::model::{Neighbour, Refusal, Resource};
use crate::store::Store;

/// The kinds a team's services lean on, in the order the dependencies tab lists them.
const DEPENDED_ON: [Kind; 5] =
    [Kind::Organisation, Kind::Repository, Kind::CloudResource, Kind::Documentation, Kind::Service];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Overview,
    Members,
    Roles,
    Services,
    Dependencies,
}

impl Tab {
    pub const ALL: [(Tab, &'static str, &'static str); 5] = [
        (Tab::Overview, "overview", "Overview"),
        (Tab::Members, "members", "Members"),
        (Tab::Roles, "roles", "Roles and permissions"),
        (Tab::Services, "services", "Services"),
        (Tab::Dependencies, "dependencies", "Dependencies"),
    ];

    pub fn named(name: Option<&str>) -> Self {
        Self::ALL
            .iter()
            .find(|(_, slug, _)| Some(*slug) == name)
            .map_or(Self::Overview, |(tab, _, _)| *tab)
    }
}

/// A resource shown as a link.
pub struct Named {
    pub label: String,
    pub hint: String,
    pub href: String,
}

pub struct Role {
    pub name: String,
    pub href: String,
    pub permissions: Vec<String>,
    /// The team's members who hold it.
    pub holders: Vec<Named>,
}

/// A service the team is responsible for, and how.
pub struct Service {
    pub named: Named,
    pub owned: bool,
    pub connected: bool,
}

/// What the team's services depend on, of one kind.
pub struct Depended {
    pub plural: &'static str,
    pub items: Vec<(Named, Vec<String>)>,
}

#[derive(Template)]
#[template(path = "team.html")]
pub struct TeamPage {
    pub flash: Flash,
    pub writes: bool,
    pub tab: Tab,
    pub name: String,
    pub title: String,
    pub resource: Option<Resource>,
    pub members: Vec<Named>,
    pub roles: Vec<Role>,
    pub services: Vec<Service>,
    pub depended: Vec<Depended>,
    pub hidden: Vec<String>,
    pub panels: Vec<Panel>,
    pub edit_url: String,
    /// The team's ID in core, where it is kept, its members added and its sub-teams made.
    pub team_id: String,
    /// The organisation it is in, as core has it.
    pub organisation: Option<Named>,
    /// The plugin that provides the team, for one core takes from a directory.
    pub provider: Option<String>,
}

impl TeamPage {
    fn url(&self) -> String {
        href(Kind::Team, &self.name)
    }

    fn tabs(&self) -> Vec<(String, &'static str, bool)> {
        Tab::ALL
            .iter()
            .map(|(tab, slug, label)| {
                let url = match tab {
                    Tab::Overview => self.url(),
                    _ => format!("{}?tab={slug}", self.url()),
                };
                (url, *label, *tab == self.tab)
            })
            .collect()
    }

    fn on(&self, slug: &str) -> bool {
        Tab::ALL.iter().any(|(tab, name, _)| *tab == self.tab && *name == slug)
    }

    /// Every permission the team's roles grant, with the roles that grant it.
    fn permissions(&self) -> Vec<(String, String)> {
        let mut granted: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for role in &self.roles {
            for permission in &role.permissions {
                granted.entry(permission).or_default().push(&role.name);
            }
        }
        granted
            .into_iter()
            .map(|(permission, roles)| (permission.to_string(), roles.join(", ")))
            .collect()
    }

    fn map_url(&self) -> String {
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("team", &self.name)
            .append_pair("depth", "2")
            .finish();
        format!("/p/service-map/map?{query}")
    }
}

fn named(neighbour: &Neighbour) -> Named {
    Named {
        label: neighbour.name.clone(),
        hint: match neighbour.title.as_str() {
            title if title.is_empty() || title == neighbour.name => String::new(),
            title => title.to_string(),
        },
        href: href(neighbour.kind, &neighbour.key),
    }
}

pub async fn page(
    backend: &Backend,
    store: &Store<'_>,
    name: &str,
    tab: Tab,
    flash: Flash,
) -> Result<String, Refusal> {
    let graph = Graph::new(store).await?;
    let team = graph.node(&Ref::new(Kind::Team, name.to_string())).await?;
    let resource = store.resource(Kind::Team, name).await?;
    let (neighbours, mut hidden) = graph.neighbours(&team).await?;
    let of = |kind: Kind| neighbours.iter().filter(move |neighbour| neighbour.kind == kind);

    // Who is in the team is core's to say; anyone connected to it in the catalogue as well is
    // listed beside them rather than twice.
    let mut members: Vec<Named> = store
        .team_members(&team.reference)
        .await?
        .into_iter()
        .map(|(id, login, how)| Named { label: login, hint: how, href: format!("/users/{id}") })
        .collect();
    let known: Vec<String> = members.iter().map(|member| member.label.clone()).collect();
    members.extend(
        of(Kind::User)
            .filter(|person| !known.contains(&person.name))
            .map(named)
            .filter(|person| !known.contains(&person.label)),
    );

    // The roles it holds: the ones it is connected to here, and the ones `rbac` grants the team
    // itself (T67), which is where a team's own access lives.
    let mut wanted: Vec<String> = of(Kind::Role).map(|neighbour| neighbour.name.clone()).collect();
    let mut roles = Vec::new();
    if matches!(tab, Tab::Overview | Tab::Roles) {
        match graph.rbac.held(Kind::Team, &team.reference).await {
            Ok(held) => {
                for role in held.roles {
                    if !wanted.contains(&role.name) {
                        wanted.push(role.name);
                    }
                }
            }
            Err(reason) => hidden.push(reason),
        }
        for role_name in &wanted {
            let role = graph.node(&Ref::new(Kind::Role, role_name.clone())).await?;
            let path = [team.clone()];
            let permissions = graph.children(&path, &role, Kind::Permission).await?;
            let holders = graph.children(&path, &role, Kind::User).await?;
            hidden.extend(permissions.hidden.clone());
            hidden.extend(holders.hidden.clone());
            roles.push(Role {
                name: role.name.clone(),
                href: href(Kind::Role, &role.key),
                permissions: permissions
                    .children
                    .iter()
                    .map(|branch| branch.name.clone())
                    .collect(),
                holders: holders
                    .children
                    .iter()
                    .map(|branch| Named {
                        label: branch.name.clone(),
                        hint: String::new(),
                        href: href(branch.kind, &branch.key),
                    })
                    .collect(),
            });
        }
    }

    let mut services: BTreeMap<String, Service> = BTreeMap::new();
    for neighbour in of(Kind::Service) {
        services.insert(
            neighbour.name.clone(),
            Service { named: named(neighbour), owned: false, connected: true },
        );
    }
    if let Some(resource) = &resource {
        for owned in store.owned_by(resource.id).await? {
            if owned.kind != Kind::Service {
                continue;
            }
            let entry = services.entry(owned.name.clone()).or_insert_with(|| Service {
                named: Named {
                    hint: match owned.title.as_str() {
                        title if title == owned.name => String::new(),
                        title => title.to_string(),
                    },
                    href: href(Kind::Service, &owned.name),
                    label: owned.name.clone(),
                },
                owned: false,
                connected: false,
            });
            entry.owned = true;
        }
    }

    let mut depended = Vec::new();
    if tab == Tab::Dependencies {
        // The team's own repositories and cloud resources, then what each of its services uses.
        let mut found: BTreeMap<Kind, BTreeMap<String, (Named, Vec<String>)>> = BTreeMap::new();
        for neighbour in neighbours.iter().filter(|n| DEPENDED_ON.contains(&n.kind)) {
            if neighbour.kind != Kind::Service {
                found
                    .entry(neighbour.kind)
                    .or_default()
                    .entry(neighbour.key.clone())
                    .or_insert_with(|| (named(neighbour), vec![format!("the team {name}")]));
            }
        }
        for service in services.values() {
            let node = graph.node(&Ref::new(Kind::Service, service.named.label.clone())).await?;
            let (around, reasons) = graph.neighbours(&node).await?;
            hidden.extend(reasons);
            for neighbour in around.iter().filter(|n| DEPENDED_ON.contains(&n.kind)) {
                if neighbour.kind == Kind::Service && services.contains_key(&neighbour.name) {
                    continue;
                }
                let via = found
                    .entry(neighbour.kind)
                    .or_default()
                    .entry(neighbour.key.clone())
                    .or_insert_with(|| (named(neighbour), Vec::new()));
                via.1.push(service.named.label.clone());
            }
        }
        for kind in DEPENDED_ON {
            if let Some(items) = found.remove(&kind) {
                depended
                    .push(Depended { plural: kind.plural(), items: items.into_values().collect() });
            }
        }
    }

    hidden.sort();
    hidden.dedup();
    let panels = match tab {
        // A team's page shows panels but pins nothing: pinning is a resource's own arrangement.
        Tab::Overview => {
            offered(backend, Kind::Team, &Ref::new(Kind::Team, name.to_string())).await.0
        }
        _ => Vec::new(),
    };
    // Which organisation it is in, and whether a directory keeps it: both core's to say.
    let (mut organisation, mut provider) = (None, None);
    if let Some(row) = store.core_row(Kind::Team, &team.reference).await? {
        provider = row.get("provider").and_then(Value::as_str).map(str::to_string);
        let of_organisation =
            row.get("organisation_id").and_then(Value::as_str).unwrap_or_default();
        if let Some(found) = store.core_row(Kind::Organisation, of_organisation).await? {
            let label = found.get("title").and_then(Value::as_str).unwrap_or_default().to_string();
            let name = found.get("name").and_then(Value::as_str).unwrap_or_default().to_string();
            organisation = Some(Named {
                label: match label.is_empty() {
                    true => name.clone(),
                    false => label,
                },
                hint: String::new(),
                href: href(Kind::Organisation, &name),
            });
        }
    }
    render(&TeamPage {
        flash,
        writes: backend.writes(),
        tab,
        name: team.name.clone(),
        title: team.title.clone(),
        resource,
        members,
        roles,
        services: services.into_values().collect(),
        depended,
        hidden,
        panels,
        team_id: team.reference.clone(),
        organisation,
        provider,
        edit_url: format!(
            "/p/resources/apply?{}",
            url::form_urlencoded::Serializer::new(String::new())
                .append_pair("from", &format!("Team:{name}"))
                .finish()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn person(login: &str) -> Named {
        Named {
            label: login.into(),
            hint: String::new(),
            href: format!("/p/resources/r/user/{login}"),
        }
    }

    fn page(tab: Tab) -> TeamPage {
        TeamPage {
            flash: Flash::default(),
            writes: false,
            tab,
            name: "payments-core".into(),
            title: "Payments core".into(),
            resource: None,
            team_id: "018f-1".into(),
            organisation: None,
            provider: None,
            members: vec![person("ada"), person("grace")],
            roles: vec![
                Role {
                    name: "deployers".into(),
                    href: "/p/resources/r/role/deployers".into(),
                    permissions: vec!["plugin:infra:user:rw".into(), "plugin:core:user:ro".into()],
                    holders: vec![person("ada")],
                },
                Role {
                    name: "readers".into(),
                    href: "/p/resources/r/role/readers".into(),
                    permissions: vec!["plugin:core:user:ro".into()],
                    holders: Vec::new(),
                },
            ],
            services: vec![Service {
                named: Named {
                    label: "card-gateway".into(),
                    hint: String::new(),
                    href: "/p/resources/r/service/card-gateway".into(),
                },
                owned: true,
                connected: false,
            }],
            depended: Vec::new(),
            hidden: Vec::new(),
            panels: Vec::new(),
            edit_url: String::new(),
        }
    }

    #[test]
    fn the_overview_is_the_default_and_every_section_has_a_tab() {
        assert_eq!(Tab::named(None), Tab::Overview);
        assert_eq!(Tab::named(Some("roles")), Tab::Roles);
        let html = page(Tab::Overview).render().expect("rendered");
        for tab in ["Members", "Roles and permissions", "Services", "Dependencies"] {
            assert!(html.contains(&format!(">{tab}</a>")), "{tab}");
        }
        assert!(
            html.contains(
                r#"href="/p/resources/r/team/payments-core" aria-current="page">Overview"#
            )
        );
        assert!(html.contains(">2</a>"), "the member count links to the members");
    }

    #[test]
    fn permissions_are_gathered_from_every_role_with_the_roles_that_grant_them() {
        let page = page(Tab::Roles);
        assert_eq!(
            page.permissions(),
            [
                ("plugin:core:user:ro".to_string(), "deployers, readers".to_string()),
                ("plugin:infra:user:rw".to_string(), "deployers".to_string()),
            ]
        );
        let html = page.render().expect("rendered");
        assert!(html.contains("None of the team's members."), "readers is held by no member");
        assert!(page.map_url().starts_with("/p/service-map/map?team=payments-core"));
    }
}
