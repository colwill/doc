//! `apply`: YAML or JSON documents that create or update resources, their connections and
//! connection definitions. All of it is checked before anything is written, in one transaction, so
//! applying the same input twice changes nothing, and a dry run reports what would change.

use std::collections::{BTreeMap, BTreeSet};

use doc_plugin_sdk::Backend;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use doc_plugin_sdk::protocol::calls::{OrganisationRequest, WriteTeamRequest};

use crate::definitions::{Definition, unconnectable};
use crate::kinds::{self, Kind, Ref};
use crate::model::{Node, Refusal, Resource};
use crate::rbac::Rbac;
use crate::store::{Store, ordered};
use doc_plugin_sdk::DataRequest;

const MAX_DOCUMENTS: usize = 2_000;
const MAX_NAME: usize = 200;
const MAX_TITLE: usize = 200;
const MAX_DESCRIPTION: usize = 4_000;
const MAX_METADATA: usize = 64 * 1024;
const DEFINITION: &str = "connectiondefinition";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Document {
    #[serde(default)]
    pub kind: String,
    pub name: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub metadata: Option<Map<String, Value>>,
    /// The owning team's name, or empty for none.
    #[serde(default)]
    pub owner: Option<String>,
    /// A team's organisation, by name: which one it is in is core's, not a connection (T69).
    #[serde(default)]
    pub organisation: Option<String>,
    /// Teams had a Slack channel here until T69; the Slack plugin connects them now. A document
    /// that still names one is taken without it rather than refused.
    #[serde(default, rename = "slack_channel")]
    pub _slack_channel: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub connections: Option<Connections>,
    /// A connection definition's kinds, in order.
    #[serde(default)]
    pub kinds: Option<Vec<String>>,
}

/// `[Team:payments-core]`, or by kind: `{Teams: [payments-core], Repository: acme/card-gateway}`.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Connections {
    Refs(Vec<String>),
    ByKind(BTreeMap<String, Names>),
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum Names {
    One(String),
    Many(Vec<String>),
}

impl Connections {
    /// The kinds a document lists by name, even with none of them: what a sync says it owns.
    fn kinds(&self) -> Vec<Kind> {
        match self {
            Self::Refs(_) => Vec::new(),
            Self::ByKind(map) => map.keys().filter_map(|kind| Kind::parse(kind)).collect(),
        }
    }

    fn refs(&self) -> Result<Vec<Ref>, Refusal> {
        match self {
            Self::Refs(texts) => texts.iter().map(|text| Ref::parse(text)).collect(),
            Self::ByKind(map) => {
                let mut refs = Vec::new();
                for (kind, names) in map {
                    let kind = kinds::kind(kind)?;
                    let names = match names {
                        Names::One(name) => std::slice::from_ref(name),
                        Names::Many(names) => names.as_slice(),
                    };
                    refs.extend(names.iter().map(|name| Ref::new(kind, name.trim())));
                }
                Ok(refs)
            }
        }
    }
}

/// Multi-document YAML, or JSON: one document, or a list of them.
pub fn documents(text: &str) -> Result<Vec<Document>, Refusal> {
    let mut values = Vec::new();
    if text.trim_start().starts_with(['{', '[']) {
        let value: Value = serde_json::from_str(text)
            .map_err(|err| Refusal::bad(format!("the JSON could not be read: {err}")))?;
        values.push(value);
    } else {
        for (index, part) in serde_yaml_ng::Deserializer::from_str(text).enumerate() {
            let value = Value::deserialize(part)
                .map_err(|err| Refusal::bad(format!("YAML document {}: {err}", index + 1)))?;
            values.push(value);
        }
    }
    let mut documents = Vec::new();
    for value in values {
        let items = match value {
            Value::Null => Vec::new(),
            Value::Array(items) => items,
            other => vec![other],
        };
        for item in items {
            let document = serde_json::from_value(item).map_err(|err| {
                Refusal::bad(format!("document {} is not a resource: {err}", documents.len() + 1))
            })?;
            documents.push(document);
        }
    }
    if documents.len() > MAX_DOCUMENTS {
        return Err(Refusal::bad(format!("apply at most {MAX_DOCUMENTS} documents at a time")));
    }
    Ok(documents)
}

pub enum Mode<'a> {
    /// Someone applying a catalogue: anything wrong refuses the whole of it.
    Apply { rbac: &'a Rbac<'a> },
    /// A plugin's sync event: only its own kinds, and what cannot be resolved is left out.
    Sync { publisher: &'a str },
}

impl Mode<'_> {
    fn source(&self) -> &str {
        match self {
            Self::Apply { .. } => "apply",
            Self::Sync { publisher } => publisher,
        }
    }
}

#[derive(Debug, Default)]
pub struct Plan {
    source: String,
    definitions: Vec<Value>,
    /// Organisations and teams, which core keeps (T69): written there first, so the rest of the
    /// plan can point at the rows core made.
    core: Vec<Value>,
    creates: Vec<Value>,
    updates: Vec<Value>,
    owners: Vec<Value>,
    connections: Vec<Value>,
    disconnections: Vec<Value>,
    pub changes: Vec<Value>,
    pub unchanged: usize,
    pub skipped: Vec<String>,
}

impl Plan {
    pub fn changed(&self) -> usize {
        self.changes.len()
    }

    pub fn report(&self, dry_run: bool) -> Value {
        json!({
            "dry_run": dry_run,
            "changed": self.changed(),
            "unchanged": self.unchanged,
            "changes": self.changes,
            "skipped": self.skipped,
        })
    }
}

fn text(field: &str, value: Option<&String>, max: usize) -> Result<Option<String>, String> {
    let Some(value) = value.map(|value| value.trim().to_string()) else { return Ok(None) };
    match value.chars().count() <= max {
        true => Ok(Some(value)),
        false => Err(format!("its {field} is longer than {max} characters")),
    }
}

fn checked_name(name: &str) -> Result<String, String> {
    let name = name.trim();
    let fine = !name.is_empty()
        && name.chars().count() <= MAX_NAME
        && name.starts_with(|c: char| c.is_ascii_alphanumeric())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.:/@+".contains(c));
    match fine {
        true => Ok(name.to_string()),
        false => Err(format!(
            "`{name}` is not a name: up to {MAX_NAME} letters, digits and - _ . : / @ +, \
             starting with a letter or digit"
        )),
    }
}

fn email(address: &str) -> Result<(), String> {
    let fine = address.len() <= 254
        && !address.contains(char::is_whitespace)
        && address
            .split_once('@')
            .is_some_and(|(user, domain)| !user.is_empty() && domain.contains('.'));
    fine.then_some(()).ok_or_else(|| format!("`{address}` is not an email address"))
}

/// A resource document, checked: what it names and the fields it sets.
struct Wanted {
    at: Ref,
    organisation: String,
    title: Option<String>,
    description: Option<String>,
    metadata: Option<Map<String, Value>>,
    owner: Option<String>,
    email: Option<String>,
    connections: Vec<Ref>,
    owned: Vec<Kind>,
}

fn wanted(document: Document, mode: &Mode<'_>) -> Result<Wanted, String> {
    let kind = Kind::parse(&document.kind)
        .ok_or_else(|| format!("`{}` is not a kind of resource", document.kind))?;
    if !kind.written() {
        return Err(match kind {
            Kind::User => "users come from core, once they first sign in".into(),
            Kind::ServiceAccount => {
                "service accounts are created on the service accounts page".into()
            }
            other => format!("{} are kept in rbac", other.plural()),
        });
    }
    // An organisation or a team is the platform's (T69): it holds what core holds, and nothing
    // the catalogue would have to keep beside it.
    if document.organisation.is_some() && kind != Kind::Team {
        return Err("only a team is in an organisation".into());
    }
    if kind.in_core() {
        if document.metadata.is_some() {
            return Err(format!("the platform keeps {}, which have no metadata", kind.plural()));
        }
        if document.owner.as_deref().is_some_and(|owner| !owner.trim().is_empty()) {
            return Err(format!("{} have no owning team", kind.plural()));
        }
    }
    if let Mode::Sync { publisher } = mode
        && !kind.publishers().contains(publisher)
    {
        return Err(format!("{publisher} does not publish {}", kind.plural()));
    }
    let name = checked_name(&document.name)?;
    if document.kinds.is_some() {
        return Err("only a ConnectionDefinition lists kinds".into());
    }
    let email_address = document.email;
    if kind != Kind::Team && email_address.is_some() {
        return Err("only a team has an email address".into());
    }
    let email_address = text("email address", email_address.as_ref(), 254)?;
    if let Some(address) = email_address.as_deref().filter(|address| !address.is_empty()) {
        email(address)?;
    }
    if let Some(metadata) = &document.metadata
        && serde_json::to_vec(metadata).map_or(0, |bytes| bytes.len()) > MAX_METADATA
    {
        return Err(format!("its metadata is over {} KiB", MAX_METADATA / 1024));
    }
    let connections = match &document.connections {
        Some(connections) => connections.refs().map_err(|refusal| refusal.detail)?,
        None => Vec::new(),
    };
    let owned = document.connections.as_ref().map(Connections::kinds).unwrap_or_default();
    Ok(Wanted {
        at: Ref::new(kind, name),
        organisation: document.organisation.unwrap_or_default().trim().to_string(),
        title: text("title", document.title.as_ref(), MAX_TITLE)?,
        description: text("description", document.description.as_ref(), MAX_DESCRIPTION)?,
        metadata: document.metadata,
        owner: document.owner.map(|owner| owner.trim().to_string()),
        email: email_address,
        connections,
        owned,
    })
}

/// Collects what is wrong with an input, so all of it is reported at once.
#[derive(Default)]
struct Problems {
    found: Vec<String>,
    status: Option<u16>,
}

impl Problems {
    fn add(&mut self, at: impl std::fmt::Display, problem: impl std::fmt::Display) {
        self.found.push(format!("{at}: {problem}"));
    }

    fn refuse(&mut self, at: impl std::fmt::Display, refusal: Refusal) {
        self.status.get_or_insert(refusal.status);
        self.add(at, refusal.detail);
    }

    fn into_result(self) -> Result<(), Refusal> {
        if self.found.is_empty() {
            return Ok(());
        }
        let status = self.status.unwrap_or(400);
        Err(Refusal { status, detail: self.found.join("; ") })
    }
}

/// A node that exists already, or that this input creates.
fn stored_node(at: &Ref, id: Uuid) -> Node {
    let name = at.name.clone();
    Node { kind: at.kind, reference: id.to_string(), key: name.clone(), name, title: String::new() }
}

/// Works out every change without making any; anything wrong refuses all of it under `Apply`.
pub async fn plan(
    backend: &Backend,
    store: &Store<'_>,
    documents: Vec<Document>,
    mode: &Mode<'_>,
) -> Result<Plan, Refusal> {
    let mut plan = Plan { source: mode.source().to_string(), ..Plan::default() };
    let mut problems = Problems::default();
    let (definition_documents, resource_documents): (Vec<_>, Vec<_>) =
        documents.into_iter().partition(|document| {
            Kind::parse(&document.kind).is_none() && is_definition(&document.kind)
        });

    let mut definitions = store.definitions().await?;
    for document in definition_documents {
        let at = format!("ConnectionDefinition:{}", document.name.trim());
        if let Mode::Sync { publisher } = mode {
            problems.add(&at, format!("{publisher} may not change connection definitions"));
            continue;
        }
        match definition(document) {
            Ok(definition) => plan_definition(&mut plan, &mut definitions, definition),
            Err(refusal) => problems.refuse(&at, refusal),
        }
    }

    let mut wanted_all: Vec<Wanted> = Vec::new();
    let mut seen = BTreeSet::new();
    for (index, document) in resource_documents.into_iter().enumerate() {
        let at = match Kind::parse(&document.kind) {
            Some(kind) => format!("{kind}:{}", document.name.trim()),
            None => format!("document {}", index + 1),
        };
        match wanted(document, mode) {
            Ok(wanted) if !seen.insert(wanted.at.clone()) => problems.add(&at, "it appears twice"),
            Ok(wanted) => wanted_all.push(wanted),
            Err(problem) => problems.add(&at, problem),
        }
    }

    let mut asked: BTreeSet<Ref> = wanted_all.iter().map(|wanted| wanted.at.clone()).collect();
    for wanted in &wanted_all {
        if let Some(owner) = wanted.owner.as_deref().filter(|owner| !owner.is_empty()) {
            asked.insert(Ref::new(Kind::Team, owner));
        }
        asked.extend(wanted.connections.iter().filter(|at| at.kind.written()).cloned());
    }
    let asked: Vec<Ref> = asked.into_iter().collect();
    let existing: BTreeMap<Ref, Resource> = store
        .existing(&asked)
        .await?
        .into_iter()
        .map(|resource| (Ref::new(resource.kind, resource.name.clone()), resource))
        .collect();
    let mut ids: BTreeMap<Ref, Uuid> =
        existing.iter().map(|(at, resource)| (at.clone(), resource.id)).collect();

    for wanted in &wanted_all {
        // What core keeps is written there, by name; until it answers, the rest of the plan names
        // a new one by an ID of its own, which `execute` swaps for core's.
        let in_core = wanted.at.kind.in_core();
        let written = |id: Uuid, title: &str, description: &str| {
            json!({
                "id": id,
                "at": wanted.at.to_string(),
                "kind": wanted.at.kind,
                "name": wanted.at.name,
                "title": title,
                "description": description,
                "email": wanted.email.clone().unwrap_or_default(),
                "organisation": wanted.organisation,
            })
        };
        match existing.get(&wanted.at) {
            None => {
                let id = Uuid::now_v7();
                ids.insert(wanted.at.clone(), id);
                match in_core {
                    true => plan.core.push(written(
                        id,
                        wanted.title.as_deref().unwrap_or(&wanted.at.name),
                        wanted.description.as_deref().unwrap_or_default(),
                    )),
                    false => plan.creates.push(json!({
                        "id": id,
                        "kind": wanted.at.kind,
                        "name": wanted.at.name,
                        "title": wanted.title.clone().unwrap_or_default(),
                        "description": wanted.description.clone().unwrap_or_default(),
                        "metadata": wanted.metadata.clone().unwrap_or_default(),
                        "email": wanted.email.clone().filter(|value| !value.is_empty()),
                        "source": plan.source,
                    })),
                }
                plan.changes
                    .push(json!({ "action": "created", "resource": wanted.at.to_string() }));
            }
            Some(resource) => {
                let fields = differences(wanted, resource);
                if fields.is_empty() {
                    plan.unchanged += 1;
                    continue;
                }
                match in_core {
                    true => plan.core.push(written(
                        resource.id,
                        wanted.title.as_deref().unwrap_or(&resource.title),
                        wanted.description.as_deref().unwrap_or(&resource.description),
                    )),
                    false => plan.updates.push(json!({
                        "id": resource.id,
                        "title": wanted.title,
                        "description": wanted.description,
                        "metadata": wanted.metadata,
                        "email": wanted.email,
                    })),
                }
                plan.changes.push(json!({
                    "action": "updated",
                    "resource": wanted.at.to_string(),
                    "fields": fields,
                }));
            }
        }
    }

    for wanted in &wanted_all {
        let Some(owner) = wanted.owner.as_deref() else { continue };
        let current = existing.get(&wanted.at).and_then(|resource| resource.owner.as_deref());
        if current.unwrap_or_default() == owner {
            continue;
        }
        let team = match owner {
            "" => None,
            owner => match ids.get(&Ref::new(Kind::Team, owner)) {
                Some(id) => Some(*id),
                None => {
                    let problem = format!("its owner, Team:{owner}, does not exist");
                    match mode {
                        Mode::Apply { .. } => problems.add(&wanted.at, problem),
                        Mode::Sync { .. } => plan.skipped.push(format!("{}: {problem}", wanted.at)),
                    }
                    continue;
                }
            },
        };
        plan.owners.push(json!({ "id": ids.get(&wanted.at), "owner": team }));
        let entry =
            plan.changes.iter_mut().find(|change| change["resource"] == wanted.at.to_string());
        match entry {
            Some(change) if change["action"] == "updated" => {
                if let Some(fields) = change["fields"].as_array_mut() {
                    fields.push(json!("owner"));
                }
            }
            Some(_) => {}
            None => {
                plan.unchanged = plan.unchanged.saturating_sub(1);
                plan.changes.push(json!({
                    "action": "updated",
                    "resource": wanted.at.to_string(),
                    "fields": ["owner"],
                }));
            }
        }
    }

    let people: Vec<Ref> = wanted_all
        .iter()
        .flat_map(|wanted| wanted.connections.iter())
        .filter(|at| matches!(at.kind, Kind::User | Kind::ServiceAccount))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut found: BTreeMap<Ref, Vec<Node>> = BTreeMap::new();
    for (asked, node) in store.core_nodes(&people).await? {
        found.entry(Ref::new(node.kind, asked)).or_default().push(node);
    }

    let mut pairs: Vec<(Ref, Ref, Node, Node)> = Vec::new();
    let mut paired = BTreeSet::new();
    let mut kept: BTreeSet<(Ref, Kind, String)> = BTreeSet::new();
    for wanted in &wanted_all {
        let Some(id) = ids.get(&wanted.at).copied() else { continue };
        let this = stored_node(&wanted.at, id);
        for other in &wanted.connections {
            let resolved = match unconnectable(&definitions, this.kind, other.kind) {
                Some(reason) => Err(Refusal::bad(reason)),
                None => resolve(backend, store, other, &ids, &found, mode).await,
            };
            let node = match resolved {
                Ok(node) => node,
                Err(refusal) => {
                    match mode {
                        Mode::Apply { .. } => {
                            problems.refuse(format!("{} → {other}", wanted.at), refusal)
                        }
                        Mode::Sync { .. } => {
                            plan.skipped
                                .push(format!("{} → {other}: {}", wanted.at, refusal.detail));
                        }
                    }
                    continue;
                }
            };
            kept.insert((wanted.at.clone(), node.kind, node.reference.clone()));
            let (from, to) = ordered(&this, &node);
            if paired.insert((
                from.key().0,
                from.reference.clone(),
                to.key().0,
                to.reference.clone(),
            )) {
                pairs.push((wanted.at.clone(), other.clone(), this.clone(), node));
            }
        }
    }
    problems.into_result()?;

    let asked: Vec<(&Node, &Node)> = pairs.iter().map(|(_, _, a, b)| (a, b)).collect();
    let connected = store.connected(&asked).await?;
    for ((at, other, a, b), already) in pairs.iter().zip(connected) {
        if already {
            plan.unchanged += 1;
            continue;
        }
        let (from, to) = ordered(a, b);
        plan.connections.push(json!({
            "from_kind": from.kind,
            "from_ref": from.reference,
            "to_kind": to.kind,
            "to_ref": to.reference,
        }));
        plan.changes.push(
            json!({ "action": "connected", "resource": at.to_string(), "to": other.to_string() }),
        );
    }
    if let Mode::Sync { publisher } = mode {
        for wanted in &wanted_all {
            let Some(resource) = existing.get(&wanted.at) else { continue };
            let this = stored_node(&wanted.at, resource.id);
            for kind in &wanted.owned {
                for link in store.sourced(&this, *kind, publisher).await? {
                    if kept.contains(&(wanted.at.clone(), link.kind, link.reference.clone())) {
                        continue;
                    }
                    let other = link.node();
                    let (from, to) = ordered(&this, &other);
                    plan.disconnections.push(json!({
                        "from_kind": from.kind,
                        "from_ref": from.reference,
                        "to_kind": to.kind,
                        "to_ref": to.reference,
                    }));
                    let gone = format!("{}:{}", link.kind, link.name);
                    plan.changes.push(
                        json!({ "action": "disconnected", "resource": wanted.at.to_string(), "to": gone }),
                    );
                }
            }
        }
    }
    Ok(plan)
}

fn is_definition(kind: &str) -> bool {
    let kind: String = kind.chars().filter(char::is_ascii_alphanumeric).collect();
    kind.eq_ignore_ascii_case(DEFINITION)
}

fn definition(document: Document) -> Result<Definition, Refusal> {
    let extra = document.title.is_some()
        || document.description.is_some()
        || document.metadata.is_some()
        || document.owner.is_some()
        || document.organisation.is_some()
        || document.connections.is_some();
    if extra {
        return Err(Refusal::bad("a connection definition has only a name and its kinds"));
    }
    let listed = document.kinds.ok_or_else(|| Refusal::bad("it lists no kinds"))?;
    let kinds = listed.iter().map(|kind| kinds::kind(kind)).collect::<Result<Vec<_>, _>>()?;
    Definition::new(&document.name, kinds)
}

fn plan_definition(plan: &mut Plan, definitions: &mut Vec<Definition>, definition: Definition) {
    let text = definition.to_string();
    let row = json!({ "name": definition.name, "kinds": definition.kinds, "ord": plan.definitions.len() });
    match definitions.iter_mut().find(|known| known.name == definition.name) {
        Some(known) if known.kinds == definition.kinds => plan.unchanged += 1,
        Some(known) => {
            *known = definition;
            plan.definitions.push(row);
            plan.changes.push(json!({ "action": "updated", "definition": text }));
        }
        None => {
            definitions.push(definition);
            plan.definitions.push(row);
            plan.changes.push(json!({ "action": "created", "definition": text }));
        }
    }
}

fn differences(wanted: &Wanted, resource: &Resource) -> Vec<&'static str> {
    let differs = |wanted: &Option<String>, current: &str| {
        wanted.as_deref().is_some_and(|wanted| wanted != current)
    };
    let mut fields = Vec::new();
    if differs(&wanted.title, &resource.title) {
        fields.push("title");
    }
    if differs(&wanted.description, &resource.description) {
        fields.push("description");
    }
    if wanted.metadata.as_ref().is_some_and(|metadata| *metadata != resource.metadata) {
        fields.push("metadata");
    }
    if differs(&wanted.email, resource.email.as_deref().unwrap_or_default()) {
        fields.push("email");
    }
    fields
}

/// The other end of a connection a document asks for.
async fn resolve(
    backend: &Backend,
    store: &Store<'_>,
    other: &Ref,
    ids: &BTreeMap<Ref, Uuid>,
    people: &BTreeMap<Ref, Vec<Node>>,
    mode: &Mode<'_>,
) -> Result<Node, Refusal> {
    match other.kind {
        kind if kind.written() => match ids.get(other) {
            Some(id) => Ok(stored_node(other, *id)),
            None => Err(Refusal::missing(format!("there is no {other}"))),
        },
        Kind::User | Kind::ServiceAccount => {
            let nodes = people.get(other).map(Vec::as_slice).unwrap_or_default();
            let node = match nodes {
                [] if other.kind == Kind::User => {
                    return Err(Refusal::missing(format!("{} has not signed in yet", other.name)));
                }
                [] => return Err(Refusal::missing(format!("there is no {other}"))),
                [node] => node.clone(),
                _ => {
                    return Err(Refusal::conflict(format!(
                        "more than one user is called {}: name one as provider/login",
                        other.name
                    )));
                }
            };
            if node.kind == Kind::ServiceAccount {
                match mode {
                    Mode::Apply { .. } => crate::ops::may_assign(backend, store, &node).await?,
                    Mode::Sync { .. } => {
                        return Err(Refusal::forbidden("service accounts are assigned by people"));
                    }
                }
            }
            Ok(node)
        }
        Kind::Role => {
            let Mode::Apply { rbac } = mode else {
                return Err(Refusal::forbidden("roles are related by people, who can see them"));
            };
            match rbac.role(&other.name).await {
                Ok(Some(role)) => Ok(role.node),
                Ok(None) => Err(Refusal::missing(format!("rbac has no role {}", other.name))),
                Err(reason) => {
                    Err(Refusal::forbidden(format!("roles are read from rbac as you: {reason}")))
                }
            }
        }
        _ => Err(Refusal::bad(format!(
            "{} come from rbac and cannot be related by hand",
            other.kind.plural()
        ))),
    }
}

/// The organisations and teams a plan writes, made or brought up to date in core, answering what
/// core calls each one now: a new one was planned under an ID of this plugin's making (T69).
async fn write_in_core(
    store: &Store<'_>,
    plan: &Plan,
) -> Result<BTreeMap<String, String>, Refusal> {
    let mut answered = BTreeMap::new();
    let field = |row: &Value, name: &str| row[name].as_str().unwrap_or_default().to_string();
    // Organisations first, whatever order the documents came in: a team joins one by name.
    let organisation_first = |row: &&Value| field(row, "kind") != Kind::Organisation.name();
    let mut ordered: Vec<&Value> = plan.core.iter().collect();
    ordered.sort_by_key(organisation_first);
    for row in ordered {
        let planned = field(row, "id");
        let id = match Kind::parse(&field(row, "kind")) {
            Some(Kind::Organisation) => {
                let request = OrganisationRequest {
                    name: field(row, "name"),
                    title: field(row, "title"),
                    description: field(row, "description"),
                };
                store.0.write_organisation(request).await?.organisation_id
            }
            _ => {
                let request = WriteTeamRequest {
                    organisation: field(row, "organisation"),
                    name: field(row, "name"),
                    title: field(row, "title"),
                    description: field(row, "description"),
                    email: field(row, "email"),
                    parent: None,
                };
                store.0.write_team(request).await?.team_id
            }
        };
        answered.insert(planned, id.to_string());
    }
    Ok(answered)
}

/// Writes a plan up to 100 changes at a time, each batch all or nothing, then moves the version on.
pub async fn execute(store: &Store<'_>, plan: &Plan, by: &str) -> Result<(), Refusal> {
    if plan.changed() == 0 {
        return Ok(());
    }
    // What core keeps goes first, since everything else may point at it.
    let in_core = write_in_core(store, plan).await?;
    let swapped = |value: &Value| match value.as_str().and_then(|text| in_core.get(text)) {
        Some(id) => json!(id),
        None => value.clone(),
    };
    let mut writes = Vec::new();
    if !plan.definitions.is_empty() {
        let known = store.definitions().await?;
        let last = store.last_position().await?;
        for row in &plan.definitions {
            let name = row["name"].as_str().unwrap_or_default();
            match known.iter().any(|definition| definition.name == name) {
                true => writes.push(DataRequest::update(
                    "connection-definitions",
                    name,
                    json!({ "kinds": row["kinds"] }),
                )),
                false => writes.push(DataRequest::insert(
                    "connection-definitions",
                    json!({ "name": name, "kinds": row["kinds"], "position": last + row["ord"].as_i64().unwrap_or(0) + 1 }),
                )),
            }
        }
    }
    writes.extend(plan.creates.iter().map(|row| DataRequest::insert("resources", row.clone())));
    for row in &plan.updates {
        let mut set = Map::new();
        for field in ["title", "description", "metadata"] {
            if !row[field].is_null() {
                set.insert(field.into(), row[field].clone());
            }
        }
        match row["email"].as_str() {
            Some("") => set.insert("email".into(), Value::Null),
            Some(value) => set.insert("email".into(), json!(value)),
            None => None,
        };
        let id = row["id"].as_str().unwrap_or_default();
        writes.push(DataRequest::update("resources", id, Value::Object(set)));
    }
    for row in &plan.owners {
        let id = row["id"].as_str().unwrap_or_default();
        let owner = swapped(&row["owner"]);
        writes.push(DataRequest::update("resources", id, json!({ "owner_team": owner })));
    }
    for row in &plan.connections {
        let mut values = row.clone();
        values["from_ref"] = swapped(&row["from_ref"]);
        values["to_ref"] = swapped(&row["to_ref"]);
        values["source"] = json!(plan.source);
        values["created_by"] = json!(by);
        writes.push(DataRequest::insert("connections", values));
    }
    for row in &plan.disconnections {
        let query = doc_plugin_sdk::Query::new("connections").filter(row.clone()).fields(&["id"]);
        let found: Vec<Value> = store.0.query_all(query).await?;
        for connection in found {
            writes.push(DataRequest::delete("connections", connection["id"].clone()));
        }
    }
    writes.push(DataRequest::update(
        "catalogue",
        "catalogue",
        json!({ "changed_at": chrono::Utc::now().to_rfc3339() }),
    ));
    store.run(writes).await.map_err(|refusal| match refusal.status {
        400 | 409 => Refusal::conflict(format!(
            "not all of it was applied, because the catalogue changed meanwhile; apply it again ({})",
            refusal.detail
        )),
        _ => refusal,
    })
}
