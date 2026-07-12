//! Roles, their holders and permissions, and principals' attributes, asked of `rbac` as the person
//! viewing, so nobody sees more of them here than `rbac` itself would show them.

use std::collections::{BTreeMap, HashMap};

use doc_plugin_sdk::Backend;
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use crate::kinds::Kind;
use crate::model::Node;

/// `None` when `rbac` has no such thing; an error says why the viewer may not see it.
type Answer = Result<Option<Value>, String>;

pub struct Rbac<'a> {
    backend: &'a Backend,
    answers: Mutex<HashMap<String, Answer>>,
}

#[derive(Debug, Clone)]
pub struct Role {
    pub node: Node,
    pub permissions: Vec<String>,
    pub holders: Vec<Node>,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default)]
pub struct Held {
    pub roles: Vec<Node>,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Group {
    plugin: String,
    name: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    permissions: Vec<String>,
}

#[derive(Deserialize)]
struct Holder {
    kind: String,
    id: String,
}

#[derive(Deserialize)]
struct Member {
    holder: Holder,
    label: Option<String>,
}

#[derive(Deserialize)]
struct GroupDetail {
    group: Group,
    #[serde(default)]
    members: Vec<Member>,
    #[serde(default)]
    attributes: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct Principal {
    #[serde(default)]
    groups: Vec<Group>,
    #[serde(default)]
    attributes: BTreeMap<String, String>,
}

pub fn role_node(plugin: &str, name: &str, description: &str) -> Node {
    let reference = format!("{plugin}/{name}");
    Node {
        kind: Kind::Role,
        reference: reference.clone(),
        name: reference.clone(),
        title: description.into(),
        key: reference,
    }
}

/// `plugin/name`, as a role is named here.
pub fn split_role(name: &str) -> Option<(&str, &str)> {
    let (plugin, group) = name.split_once('/')?;
    let fine = |part: &str| {
        !part.is_empty()
            && part.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    };
    (fine(plugin) && fine(group)).then_some((plugin, group))
}

impl<'a> Rbac<'a> {
    pub fn new(backend: &'a Backend) -> Self {
        Self { backend, answers: Mutex::new(HashMap::new()) }
    }

    async fn get(&self, route: &str) -> Answer {
        if let Some(answer) = self.answers.lock().await.get(route) {
            return answer.clone();
        }
        let answer = match self.backend.ask("rbac", "GET", route, None, None).await {
            Ok((200, body)) => Ok(Some(body)),
            Ok((404, _)) => Ok(None),
            Ok((status, body)) => {
                let detail =
                    body["detail"].as_str().map_or_else(|| body.to_string(), str::to_string);
                Err(match status {
                    401 | 403 => detail,
                    _ => format!("rbac answered {status}: {detail}"),
                })
            }
            Err(err) => Err(format!("rbac could not be asked: {err}")),
        };
        self.answers.lock().await.insert(route.to_string(), answer.clone());
        answer
    }

    pub async fn role(&self, name: &str) -> Result<Option<Role>, String> {
        let Some((plugin, group)) = split_role(name) else { return Ok(None) };
        let Some(body) = self.get(&format!("groups/{plugin}/{group}")).await? else {
            return Ok(None);
        };
        let detail: GroupDetail = serde_json::from_value(body)
            .map_err(|err| format!("rbac's answer for {name} was not understood: {err}"))?;
        let holders = detail
            .members
            .into_iter()
            .filter_map(|member| {
                let kind = match member.holder.kind.as_str() {
                    "user" => Kind::User,
                    "service" => Kind::ServiceAccount,
                    // A team holds roles as a person does (T67), and is a resource here (T69).
                    "team" => Kind::Team,
                    _ => return None,
                };
                let name = member.label.unwrap_or_else(|| member.holder.id.clone());
                let key = name.clone();
                Some(Node { kind, reference: member.holder.id, name, title: String::new(), key })
            })
            .collect();
        let group = detail.group;
        Ok(Some(Role {
            node: role_node(&group.plugin, &group.name, &group.description),
            permissions: group.permissions,
            holders,
            attributes: detail.attributes,
        }))
    }

    pub async fn roles(&self) -> Result<Vec<Node>, String> {
        let body = self.get("groups").await?.unwrap_or_default();
        let groups: Vec<Group> = serde_json::from_value(body)
            .map_err(|err| format!("rbac's list of roles was not understood: {err}"))?;
        Ok(groups
            .iter()
            .map(|group| role_node(&group.plugin, &group.name, &group.description))
            .collect())
    }

    /// The roles a user, service account or team holds and its attributes, by core ID.
    pub async fn held(&self, kind: Kind, id: &str) -> Result<Held, String> {
        let holder = match kind {
            Kind::User => "user",
            Kind::ServiceAccount => "service",
            Kind::Team => "team",
            _ => return Ok(Held::default()),
        };
        let Some(body) = self.get(&format!("principals/{holder}/{id}")).await? else {
            return Ok(Held::default());
        };
        let principal: Principal = serde_json::from_value(body)
            .map_err(|err| format!("rbac's answer for a principal was not understood: {err}"))?;
        let roles = principal
            .groups
            .iter()
            .map(|group| role_node(&group.plugin, &group.name, &group.description))
            .collect();
        Ok(Held { roles, attributes: principal.attributes })
    }
}

pub fn attribute_node(key: &str, value: &str) -> Node {
    let name = format!("{key}={value}");
    Node {
        kind: Kind::Attribute,
        reference: name.clone(),
        key: name.clone(),
        name,
        title: String::new(),
    }
}

pub fn permission_node(permission: &str) -> Node {
    Node {
        kind: Kind::Permission,
        reference: permission.to_string(),
        name: permission.to_string(),
        title: String::new(),
        key: permission.to_string(),
    }
}
