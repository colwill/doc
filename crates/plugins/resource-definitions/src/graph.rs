//! Following connections: a definition one level at a time, and a node's neighbours. What `rbac`
//! answers for is asked as the viewer, and narrowed by the path that led to it: under
//! `Team X → Role R`, only X's members who hold R.

use std::collections::{BTreeSet, HashMap};

use serde::Serialize;
use tokio::sync::Mutex;

use crate::definitions::{Definition, above, connectable, derived};
use crate::kinds::{Kind, Ref};
use crate::model::{Neighbour, Node, Refusal};
use crate::rbac::{self, Rbac};
use crate::store::{Link, Store};

pub struct Graph<'a> {
    pub store: &'a Store<'a>,
    pub rbac: Rbac<'a>,
    pub definitions: Vec<Definition>,
    links: Mutex<HashMap<(Kind, String, Kind), Vec<Node>>>,
}

/// What one level holds: `hidden` says why some of it may not be shown.
#[derive(Debug, Default, Serialize)]
pub struct Level {
    pub kind: Option<Kind>,
    pub children: Vec<Branch>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hidden: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Branch {
    pub kind: Kind,
    #[serde(skip)]
    pub reference: String,
    pub name: String,
    pub key: String,
    pub title: String,
    pub derived: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<Level>,
}

impl<'a> Graph<'a> {
    pub async fn new(store: &'a Store<'a>) -> Result<Self, Refusal> {
        Ok(Self {
            store,
            rbac: Rbac::new(store.0),
            definitions: store.definitions().await?,
            links: Mutex::new(HashMap::new()),
        })
    }

    /// Any kind by name: the stored ones here, organisations, teams, users and service accounts
    /// in core, roles in `rbac`.
    pub async fn node(&self, wanted: &Ref) -> Result<Node, Refusal> {
        let missing = || Refusal::missing(format!("there is no {wanted}"));
        match wanted.kind {
            kind if kind.written() => {
                let resource =
                    self.store.resource(kind, &wanted.name).await?.ok_or_else(missing)?;
                Ok(Node {
                    kind,
                    reference: resource.id.to_string(),
                    key: resource.name.clone(),
                    name: resource.name,
                    title: resource.title,
                })
            }
            Kind::User | Kind::ServiceAccount => {
                let found = self.store.core_nodes(std::slice::from_ref(wanted)).await?;
                one(wanted, found.into_iter().map(|(_, node)| node).collect())
            }
            Kind::Role => match self.rbac.role(&wanted.name).await {
                Ok(Some(role)) => Ok(role.node),
                Ok(None) => Err(missing()),
                Err(reason) => {
                    Err(Refusal::forbidden(format!("{wanted} is read from rbac: {reason}")))
                }
            },
            Kind::Permission => Ok(rbac::permission_node(&wanted.name)),
            _ => {
                let (key, value) = wanted.name.split_once('=').ok_or_else(|| {
                    Refusal::bad(format!("an attribute is written key=value, not {}", wanted.name))
                })?;
                Ok(rbac::attribute_node(key, value))
            }
        }
    }

    async fn explicit(&self, parent: &Node, kind: Kind) -> Result<Vec<Node>, Refusal> {
        let key = (parent.kind, parent.reference.clone(), kind);
        if let Some(nodes) = self.links.lock().await.get(&key) {
            return Ok(nodes.clone());
        }
        let found: Vec<Link> = self.store.links(parent, Some(kind)).await?;
        let mut nodes: Vec<Node> = found.iter().map(Link::node).collect();
        for member in self.members(parent, kind).await? {
            if !nodes.iter().any(|node| node.reference == member.reference) {
                nodes.push(member);
            }
        }
        self.links.lock().await.insert(key, nodes.clone());
        Ok(nodes)
    }

    /// Who is in a team, and which teams somebody is in: core keeps both (T69), and they are
    /// shown wherever a connection between the two would be, without anybody drawing the lines.
    async fn members(&self, parent: &Node, kind: Kind) -> Result<Vec<Node>, Refusal> {
        if (parent.kind, kind) == (Kind::User, Kind::Team) {
            let teams = self.store.teams_of(&parent.reference).await?;
            return Ok(teams.iter().map(node_of).collect());
        }
        if (parent.kind, kind) != (Kind::Team, Kind::User) {
            return Ok(Vec::new());
        }
        let mut people = Vec::new();
        for (id, login, _) in self.store.team_members(&parent.reference).await? {
            people.push(Node {
                kind: Kind::User,
                reference: id,
                name: login.clone(),
                title: String::new(),
                key: login,
            });
        }
        let ids: Vec<String> = people.iter().map(|person| person.reference.clone()).collect();
        if let Ok(keys) = self.store.user_keys(&ids).await {
            for person in &mut people {
                if let Some(key) = keys.get(&person.reference) {
                    person.key.clone_from(key);
                }
            }
        }
        Ok(people)
    }

    /// Gives users `rbac` names by login their `provider/login`, which names them without doubt.
    async fn keyed(&self, holders: &mut [Node]) {
        let ids: Vec<String> = holders
            .iter()
            .filter(|holder| holder.kind == Kind::User)
            .map(|holder| holder.reference.clone())
            .collect();
        if let Ok(keys) = self.store.user_keys(&ids).await {
            for holder in holders.iter_mut() {
                if let Some(key) = keys.get(&holder.reference) {
                    holder.key.clone_from(key);
                }
            }
        }
    }

    /// What `rbac` says lies between `parent` and `kind`, or why the viewer may not see it.
    async fn answered_by_rbac(&self, parent: &Node, kind: Kind) -> Result<Vec<Node>, String> {
        match (parent.kind, kind) {
            (Kind::Role, _) => {
                let Some(role) = self.rbac.role(&parent.reference).await? else {
                    return Ok(Vec::new());
                };
                Ok(match kind {
                    Kind::User | Kind::ServiceAccount => {
                        let mut holders: Vec<Node> =
                            role.holders.into_iter().filter(|holder| holder.kind == kind).collect();
                        self.keyed(&mut holders).await;
                        holders
                    }
                    Kind::Permission => {
                        role.permissions.iter().map(|p| rbac::permission_node(p)).collect()
                    }
                    Kind::Attribute => role
                        .attributes
                        .iter()
                        .map(|(key, value)| rbac::attribute_node(key, value))
                        .collect(),
                    _ => Vec::new(),
                })
            }
            (Kind::User | Kind::ServiceAccount, Kind::Role | Kind::Attribute) => {
                let held = self.rbac.held(parent.kind, &parent.reference).await?;
                Ok(match kind {
                    Kind::Role => held.roles,
                    _ => held
                        .attributes
                        .iter()
                        .map(|(key, value)| rbac::attribute_node(key, value))
                        .collect(),
                })
            }
            (from, to) => {
                Err(format!("rbac is not asked for {} of {}", to.plural(), from.plural()))
            }
        }
    }

    /// One level down from `parent`, which `path` led to (without it).
    pub async fn children(
        &self,
        path: &[Node],
        parent: &Node,
        kind: Kind,
    ) -> Result<Level, Refusal> {
        if !derived(parent.kind, kind) {
            let children = self.explicit(parent, kind).await?;
            return Ok(level(kind, children, false, None));
        }
        let mut children = match self.answered_by_rbac(parent, kind).await {
            Ok(children) => children,
            Err(reason) => return Ok(level(kind, Vec::new(), true, Some(reason))),
        };
        for ancestor in path {
            if connectable(&self.definitions, ancestor.kind, kind) {
                let keep: BTreeSet<String> = self
                    .explicit(ancestor, kind)
                    .await?
                    .into_iter()
                    .map(|node| node.reference)
                    .collect();
                children.retain(|child| keep.contains(&child.reference));
            }
        }
        Ok(level(kind, children, true, None))
    }

    /// `depth` levels of `definition` below the last node of `path`.
    pub async fn expand(
        &self,
        definition: &Definition,
        path: &[Node],
        depth: usize,
    ) -> Result<Level, Refusal> {
        let Some((parent, above)) = path.split_last() else {
            return Ok(Level::default());
        };
        let next =
            definition.position(parent.kind).and_then(|index| definition.kinds.get(index + 1));
        let Some(&kind) = next.filter(|_| depth > 0) else {
            return Ok(Level::default());
        };
        let mut level = self.children(above, parent, kind).await?;
        let deeper =
            definition.position(kind).is_some_and(|index| index + 1 < definition.kinds.len());
        if depth > 1 && deeper {
            for branch in &mut level.children {
                let mut below = path.to_vec();
                below.push(branch_node(branch));
                branch.next = Some(Box::pin(self.expand(definition, &below, depth - 1)).await?);
            }
        }
        Ok(level)
    }

    /// Everything one step away, stored or answered by `rbac`, with what the viewer may not see.
    pub async fn neighbours(&self, node: &Node) -> Result<(Vec<Neighbour>, Vec<String>), Refusal> {
        let mut neighbours: BTreeSet<Neighbour> = self
            .store
            .links(node, None)
            .await?
            .iter()
            .map(|link| neighbour(&self.definitions, node, &link.node(), ""))
            .collect();
        // What somebody drew stands for itself: a neighbour that is already a connection is not
        // shown again as one worked out from ownership, membership or rbac.
        let drawn: BTreeSet<(Kind, String)> =
            neighbours.iter().map(|other| (other.kind, other.key.clone())).collect();
        let mut hidden = BTreeSet::new();
        let asked: &[Kind] = match node.kind {
            Kind::Role => &[Kind::User, Kind::ServiceAccount, Kind::Permission, Kind::Attribute],
            Kind::User | Kind::ServiceAccount => &[Kind::Role, Kind::Attribute],
            _ => &[],
        };
        let add = |other: &Node, via: &'static str, into: &mut BTreeSet<Neighbour>| {
            if !drawn.contains(&(other.kind, other.key.clone())) {
                into.insert(neighbour(&self.definitions, node, other, via));
            }
        };
        for kind in asked {
            match self.answered_by_rbac(node, *kind).await {
                Ok(found) => {
                    for other in &found {
                        add(other, "rbac", &mut neighbours);
                    }
                }
                Err(reason) => {
                    hidden.insert(reason);
                }
            }
        }
        // Ownership is kept as a field rather than a connection, but a team and what it owns are
        // neighbours by any reading of the word: a map of a team should hold its services. They
        // are marked as derived, since nobody drew them.
        if let Ok(id) = node.reference.parse::<uuid::Uuid>() {
            for owned in self.store.owned_by(id).await? {
                add(&node_of(&owned), "owner", &mut neighbours);
            }
        }
        for member in self.members(node, Kind::User).await? {
            add(&member, "member", &mut neighbours);
        }
        for team in self.members(node, Kind::Team).await? {
            add(&team, "member", &mut neighbours);
        }
        if node.kind.stored()
            && let Some(resource) = self.store.resource(node.kind, &node.name).await?
            && let Some(owner) = resource.owner
            && let Some(team) = self.store.resource(Kind::Team, &owner).await?
        {
            add(&node_of(&team), "owner", &mut neighbours);
        }
        Ok((neighbours.into_iter().collect(), hidden.into_iter().collect()))
    }
}

fn one(wanted: &Ref, mut found: Vec<Node>) -> Result<Node, Refusal> {
    if found.len() > 1 {
        return Err(Refusal::conflict(format!(
            "more than one {} is called {}: name one as provider/login",
            wanted.kind, wanted.name
        )));
    }
    found.pop().ok_or_else(|| Refusal::missing(format!("there is no {wanted}")))
}

fn level(kind: Kind, children: Vec<Node>, derived: bool, hidden: Option<String>) -> Level {
    let children = children
        .into_iter()
        .map(|node| Branch {
            kind: node.kind,
            reference: node.reference,
            name: node.name,
            key: node.key,
            title: node.title,
            derived,
            next: None,
        })
        .collect();
    Level { kind: Some(kind), children, hidden }
}

fn branch_node(branch: &Branch) -> Node {
    Node {
        kind: branch.kind,
        reference: branch.reference.clone(),
        name: branch.name.clone(),
        title: branch.title.clone(),
        key: branch.key.clone(),
    }
}

/// A stored resource as a node of the graph.
fn node_of(resource: &crate::model::Resource) -> Node {
    Node {
        kind: resource.kind,
        reference: resource.id.to_string(),
        name: resource.name.clone(),
        title: resource.title.clone(),
        key: resource.name.clone(),
    }
}

/// Which way a neighbour lies: the definitions decide, because they are what says which kind is
/// above which. Only where nothing says — a pair no definition puts next to each other, reached
/// through ownership or `rbac` — does the declared order of the kinds settle it.
fn neighbour(
    definitions: &[Definition],
    from: &Node,
    other: &Node,
    via: &'static str,
) -> Neighbour {
    let direction = match above(definitions, other.kind, from.kind) {
        Some(true) => "in",
        Some(false) => "out",
        None if other.kind > from.kind => "out",
        None => "in",
    };
    Neighbour {
        kind: other.kind,
        name: other.name.clone(),
        key: other.key.clone(),
        title: other.title.clone(),
        direction,
        derived: !via.is_empty(),
        via,
    }
}
