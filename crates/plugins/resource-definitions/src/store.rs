//! What the plugin keeps through the data API, and what it reads of core's through `core.*`:
//! organisations, teams, users and service accounts, which are connected to but never copied
//! (T69). Organisations and teams were kept here once; `moved_into_core` takes the rows left from
//! then and makes them core's, once.

use std::collections::{BTreeMap, BTreeSet};

use doc_plugin_sdk::protocol::calls::{OrganisationRequest, WriteTeamRequest};
use doc_plugin_sdk::{
    Aggregate, Backend, Collection, DataRequest, Declaration, Field, ListOf, Measure, OnDelete,
    Order, PluginError, Query,
};
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::definitions::Definition;
use crate::kinds::{self, Kind, Ref};
use crate::model::{Node, Refusal, Resource};

/// The one record whose `_version` is the catalogue's, touched by every change to it.
const CATALOGUE: &str = "catalogue";
/// How many IDs one `in` condition carries.
const CHUNK: usize = 500;
const BATCH: usize = 100;

const DEFINITIONS: [(&str, &[&str]); 13] = [
    ("Organisation-to-Users", &["Organisation", "Service", "Team", "Role", "User"]),
    (
        "Organisation-to-ServiceAccounts",
        &["Organisation", "Service", "Repository", "Team", "Role", "ServiceAccount"],
    ),
    ("Service-to-Documentation", &["Service", "Documentation"]),
    ("DocumentationSource-to-Documentation", &["DocumentationSource", "Documentation"]),
    ("Repository-to-Documentation", &["Repository", "Documentation"]),
    ("Repository-to-DocumentationSources", &["Repository", "DocumentationSource"]),
    // Documentation that is not about a piece of software: processes, runbooks, how a team works,
    // what it has agreed. It belongs to whoever keeps it rather than to anything it describes.
    ("Organisation-to-Documentation", &["Organisation", "Documentation"]),
    ("Team-to-Documentation", &["Team", "Documentation"]),
    ("Service-to-CloudResources", &["Service", "CloudResource"]),
    ("Service-to-ServiceAccounts", &["Service", "ServiceAccount"]),
    ("Team-to-Members", &["Team", "User"]),
    ("Role-to-Permissions", &["Role", "Permission"]),
    ("Team-to-CloudResources", &["Team", "CloudResource"]),
];

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "resources",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("kind", Field::text().required())
                .field("name", Field::text().required())
                .field("title", Field::text().required().default(json!("")))
                .field("description", Field::text().required().default(json!("")))
                .field("metadata", Field::json().required().default(json!({})))
                // The owning team, by its ID in core: teams are the platform's, so this points
                // outside this plugin's storage (T69). The field it replaces pointed at a team
                // kept here; a declaration's fields are deprecated before they go, so both are
                // declared and only the new one is written.
                .field("owner_team", Field::uuid())
                .field(
                    "owner",
                    Field::reference("resources").on_delete(OnDelete::Null).deprecated(),
                )
                .field("slack_channel", Field::text().deprecated())
                .field("email", Field::text())
                .field("source", Field::text().required())
                .unique(&["kind", "name"]),
        )
        .collection(
            "connections",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("from_kind", Field::text().required())
                .field("from_ref", Field::text().required())
                .field("to_kind", Field::text().required())
                .field("to_ref", Field::text().required())
                .field("source", Field::text().required())
                .field("created_by", Field::text().required())
                .unique(&["from_kind", "from_ref", "to_kind", "to_ref"])
                .index(&["to_kind", "to_ref"]),
        )
        .collection(
            "connection-definitions",
            Collection::new()
                .field("name", Field::text().key())
                .field("kinds", Field::list(ListOf::Text).required())
                .field("position", Field::integer().required())
                .index(&["position", "name"]),
        )
        // What somebody pinned to the top of this resource's page, in the order they put it in.
        // Only the resources somebody has pinned something to are here.
        .collection(
            PINNED,
            Collection::new()
                .field("resource", Field::text().key())
                .field(
                    "insights",
                    Field::list(ListOf::Text)
                        .required()
                        .default(json!([]))
                        .describe("The insights pinned to it, as plugin:insight, in order"),
                )
                .field("changed_by", Field::text().required().default(json!(""))),
        )
        .collection(
            "catalogue",
            Collection::new()
                .field("id", Field::text().key())
                .field("changed_at", Field::timestamp().required().default(json!("now"))),
        )
}

/// Where what somebody pinned to a resource's page is kept.
pub const PINNED: &str = "pinned";

/// On the first load: the catalogue's record, and the connection definitions it starts with.
pub async fn seed(backend: &Backend) -> Result<(), Refusal> {
    if backend.get::<Value>("catalogue", CATALOGUE).await?.is_some() {
        renamed_verticals(backend).await?;
        added_definitions(backend).await?;
        return moved_into_core(backend).await;
    }
    let mut writes: Vec<DataRequest> = definition_writes(&[]);
    writes.push(DataRequest::upsert("catalogue", &["id"], json!({ "id": CATALOGUE })));
    backend.batch(writes).await?;
    Ok(())
}

/// The definitions to write, leaving out any that are there already: their position is where
/// they come in the list, so the order a catalogue drills down in is the order written here.
fn definition_writes(known: &[Definition]) -> Vec<DataRequest> {
    DEFINITIONS
        .iter()
        .enumerate()
        .filter(|(_, (name, _))| !known.iter().any(|definition| definition.name == *name))
        .map(|(at, (name, kinds))| {
            let values = json!({ "name": name, "kinds": kinds, "position": at + 1 });
            DataRequest::upsert("connection-definitions", &["name"], values)
        })
        .collect()
}

/// A definition added to this list after a catalogue was made — connecting a page to the source
/// it came from, say — is written the next time the plugin loads. One that somebody has changed
/// or removed since is left as they left it: this adds what is missing and nothing else.
async fn added_definitions(backend: &Backend) -> Result<(), Refusal> {
    let known = Store(backend).definitions().await?;
    let writes = definition_writes(&known);
    if writes.is_empty() {
        return Ok(());
    }
    backend.batch(writes).await?;
    Ok(())
}

/// One page of a listing: what is left after `offset`, at most `limit` of it. A listing is read
/// whole and then cut, because the kinds kept in core and the kinds kept here are put in one
/// order only once they are together.
fn page<T>(found: Vec<T>, limit: i64, offset: usize) -> Vec<T> {
    found.into_iter().skip(offset).take(usize::try_from(limit).unwrap_or_default()).collect()
}

/// Organisations were called verticals. Rows kept from then are rewritten once, so filters on the
/// kind find them; the old names still parse, so anything else that says "vertical" keeps working.
async fn renamed_verticals(backend: &Backend) -> Result<(), Refusal> {
    const OLD: &str = "Vertical";
    const NEW: &str = "Organisation";
    let store = Store(backend);
    let mut writes = Vec::new();
    let resources = Query::new("resources").filter(json!({ "kind": OLD })).fields(&["id"]);
    for resource in store.all(resources).await? {
        let key = resource.get("id").cloned().unwrap_or_default();
        writes.push(DataRequest::update("resources", key, json!({ "kind": NEW })));
    }
    for field in ["from_kind", "to_kind"] {
        let connections = Query::new("connections").filter(json!({ field: OLD })).fields(&["id"]);
        for connection in store.all(connections).await? {
            let key = connection.get("id").cloned().unwrap_or_default();
            writes.push(DataRequest::update("connections", key, json!({ field: NEW })));
        }
    }
    for definition in store.all(Query::new("connection-definitions")).await? {
        let kinds: Vec<String> = definition
            .get("kinds")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if !kinds.iter().any(|kind| kind == OLD) {
            continue;
        }
        let name = text(&definition, "name");
        let kinds: Vec<&str> =
            kinds.iter().map(|kind| if kind == OLD { NEW } else { kind.as_str() }).collect();
        writes.push(DataRequest::delete("connection-definitions", name.clone()));
        writes.push(DataRequest::insert(
            "connection-definitions",
            json!({
                "name": name.replace(OLD, NEW),
                "kinds": kinds,
                "position": definition.get("position").cloned().unwrap_or(json!(0)),
            }),
        ));
    }
    if writes.is_empty() {
        return Ok(());
    }
    tracing::info!(writes = writes.len(), "verticals renamed organisations");
    writes.push(touch());
    for batch in writes.chunks(BATCH) {
        backend.batch(batch.to_vec()).await?;
    }
    Ok(())
}

/// Where core keeps a kind the catalogue shows but does not keep (T69).
fn core_collection(kind: Kind) -> Option<&'static str> {
    match kind {
        Kind::Organisation => Some("core.organisations"),
        Kind::Team => Some("core.teams"),
        _ => None,
    }
}

/// What the catalogue says a resource came from, when it came from core.
pub const PLATFORM: &str = "the platform";

/// A name core will take: lowercase, with anything else it does not allow turned into `-`.
fn core_name(name: &str) -> String {
    let lowered: String = name
        .to_lowercase()
        .chars()
        .map(|c| match c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c) {
            true => c,
            false => '-',
        })
        .collect();
    let trimmed = lowered.trim_start_matches(|c: char| !c.is_ascii_alphanumeric());
    trimmed.chars().take(64).collect()
}

fn cut(text: &str, most: usize) -> String {
    text.chars().take(most).collect()
}

/// Organisations and teams were kept here until T69; core keeps them now. Every row left from then
/// becomes a core organisation or team of the same name, or joins the one already there;
/// everything that pointed at the old row points at core's, and the row goes. It happens once,
/// because afterwards there are none of those rows left to move.
async fn moved_into_core(backend: &Backend) -> Result<(), Refusal> {
    let store = Store(backend);
    let kinds = [Kind::Organisation.name(), Kind::Team.name()];
    let stored =
        store.all(Query::new("resources").filter(json!({ "kind": { "in": kinds } }))).await?;
    if stored.is_empty() {
        return Ok(());
    }
    let (mut moved, mut writes) = (BTreeMap::new(), Vec::new());
    let (organisations, teams): (Vec<_>, Vec<_>) =
        stored.iter().partition(|row| text(row, "kind") == Kind::Organisation.name());
    // Which organisation each team was in, from its connections, so it joins the same one.
    let mut organisation_of: BTreeMap<String, String> = BTreeMap::new();
    for connection in store.all(Query::new("connections")).await? {
        if text(&connection, "from_kind") == Kind::Organisation.name()
            && text(&connection, "to_kind") == Kind::Team.name()
        {
            organisation_of.insert(text(&connection, "to_ref"), text(&connection, "from_ref"));
        }
    }
    // A team kept here named no organisation — the old definitions put none next to a team — so
    // one that names none joins the organisation the platform made first, which is the one the
    // bootstrap made and where its default teams are.
    let organisations_in_core = store.all(Query::new("core.organisations")).await?;
    let fallback = organisations_in_core
        .iter()
        .min_by_key(|row| text(row, "created_at"))
        .map(|row| text(row, "name"))
        .unwrap_or_default();
    // A name the platform already has is the same organisation or team, which is the whole point
    // of the move: it joins that one and is left as core has it.
    let already = |rows: &[Map<String, Value>], name: &str, organisation: &str| {
        let of_name: Vec<&Map<String, Value>> =
            rows.iter().filter(|row| text(row, "name") == name).collect();
        let here = of_name.iter().find(|row| text(row, "organisation_id") == organisation);
        here.or_else(|| of_name.first()).map(|row| text(row, "id"))
    };
    let teams_in_core = store.all(Query::new("core.teams")).await?;
    let mut named: BTreeMap<String, String> = BTreeMap::new();
    for row in &organisations {
        let name = core_name(&text(row, "name"));
        if let Some(id) = already(&organisations_in_core, &name, "") {
            tracing::info!(organisation = %name, "an organisation moved into one the platform has");
            moved.insert(text(row, "id"), id);
            named.insert(text(row, "id"), name);
            continue;
        }
        let title = match text(row, "title") {
            title if title.is_empty() => name.clone(),
            title => cut(&title, 100),
        };
        let request = OrganisationRequest {
            name: name.clone(),
            title,
            description: cut(&text(row, "description"), 500),
        };
        let made = backend.write_organisation(request).await?;
        tracing::info!(organisation = %name, made = made.created, "an organisation moved into core");
        moved.insert(text(row, "id"), made.organisation_id.to_string());
        named.insert(text(row, "id"), name);
    }
    for row in &teams {
        let name = core_name(&text(row, "name"));
        let of_organisation = organisation_of
            .get(&text(row, "id"))
            .and_then(|was| moved.get(was))
            .cloned()
            .unwrap_or_default();
        if let Some(id) = already(&teams_in_core, &name, &of_organisation) {
            tracing::info!(team = %name, "a team moved into one the platform has");
            moved.insert(text(row, "id"), id);
            continue;
        }
        let title = match text(row, "title") {
            title if title.is_empty() => name.clone(),
            title => cut(&title, 100),
        };
        let organisation = organisation_of
            .get(&text(row, "id"))
            .and_then(|was| named.get(was))
            .cloned()
            .unwrap_or_else(|| fallback.clone());
        let request = WriteTeamRequest {
            organisation,
            name: name.clone(),
            title,
            description: cut(&text(row, "description"), 500),
            email: text(row, "email"),
            parent: None,
        };
        let made = backend.write_team(request).await?;
        tracing::info!(team = %name, made = made.created, "a team moved into core");
        moved.insert(text(row, "id"), made.team_id.to_string());
    }

    // Everything that pointed at a row now points at core's. Two rows that became one team would
    // make one connection twice, so what is already there is left alone.
    let connections = store.all(Query::new("connections")).await?;
    let ends = |row: &Map<String, Value>| {
        let end = |side: &str| {
            let reference = text(row, &format!("{side}_ref"));
            moved.get(&reference).cloned().unwrap_or(reference)
        };
        (text(row, "from_kind"), end("from"), text(row, "to_kind"), end("to"))
    };
    let mut kept: BTreeSet<(String, String, String, String)> = connections
        .iter()
        .filter(|row| {
            let (from, to) = (text(row, "from_ref"), text(row, "to_ref"));
            !moved.contains_key(&from) && !moved.contains_key(&to)
        })
        .map(ends)
        .collect();
    for row in &connections {
        let (from, to) = (text(row, "from_ref"), text(row, "to_ref"));
        if !moved.contains_key(&from) && !moved.contains_key(&to) {
            continue;
        }
        let pointing = ends(row);
        let id = text(row, "id");
        if !kept.insert(pointing.clone()) {
            writes.push(DataRequest::delete("connections", id));
            continue;
        }
        let (from_kind, from_ref, to_kind, to_ref) = pointing;
        writes.push(DataRequest::update(
            "connections",
            id,
            json!({
                "from_kind": from_kind, "from_ref": from_ref,
                "to_kind": to_kind, "to_ref": to_ref,
            }),
        ));
    }
    // An owning team is now named by its ID in core, in the field that replaces `owner`. The old
    // one is emptied as it is copied, so nothing points at a row that is about to go.
    for row in store.all(Query::new("resources").fields(&["id", "owner"])).await? {
        if let Some(team) = moved.get(&text(&row, "owner")) {
            let write = json!({ "owner_team": team, "owner": null });
            writes.push(DataRequest::update("resources", text(&row, "id"), write));
        }
    }
    for row in &stored {
        writes.push(DataRequest::delete("resources", text(row, "id")));
    }
    writes.push(touch());
    tracing::info!(
        organisations = organisations.len(),
        teams = teams.len(),
        writes = writes.len(),
        "organisations and teams moved from the catalogue into the platform"
    );
    store.run(writes).await
}

/// The write that moves the catalogue's version on, which goes in the same batch as the change.
fn touch() -> DataRequest {
    DataRequest::update(
        CATALOGUE,
        CATALOGUE,
        json!({ "changed_at": chrono::Utc::now().to_rfc3339() }),
    )
}

fn text(record: &Map<String, Value>, field: &str) -> String {
    record.get(field).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn lower_contains(haystack: &str, needle: &str) -> bool {
    haystack.to_lowercase().contains(&needle.to_lowercase())
}

/// How well `text` answers a lowered search, lower being better: the whole of it, how it starts,
/// how one of its words starts, anywhere in it, and last its letters in order with others between,
/// as `cgw` is in `card-gateway`. One letter only matches how something or a word in it starts.
fn text_closeness(text: &str, lowered: &str) -> Option<u32> {
    let text = text.to_lowercase();
    if text.is_empty() {
        return None;
    }
    let words = text.split(|c: char| !c.is_alphanumeric()).filter(|word| !word.is_empty());
    let long = lowered.chars().count() > 1;
    match () {
        _ if text == lowered => Some(0),
        _ if text.starts_with(lowered) => Some(10),
        _ if words.clone().any(|word| word.starts_with(lowered)) => Some(20),
        _ if long && text.contains(lowered) => Some(30),
        _ if long => {
            let (mut wanted, mut first, mut last) = (lowered.chars().peekable(), None, 0);
            for (at, c) in text.chars().enumerate() {
                if wanted.peek() == Some(&c) {
                    wanted.next();
                    first.get_or_insert(at);
                    last = at;
                }
            }
            let spread = (last + 1 - first?).saturating_sub(lowered.chars().count());
            wanted.peek().is_none().then(|| 40 + spread.min(40) as u32)
        }
        _ => None,
    }
}

/// How well a resource answers a search, by its name, its title a little behind, or its kind:
/// `d` finds every Documentation page, just after whatever is named with a d.
pub fn closeness(search: &str, name: &str, title: &str, kind: Kind) -> Option<u32> {
    let lowered = search.trim().to_lowercase();
    if lowered.is_empty() {
        return Some(100);
    }
    let by_kind = kinds::words(kind).any(|word| word.starts_with(&lowered)).then_some(15);
    let by_title = text_closeness(title, &lowered).map(|close| close + 1);
    [text_closeness(name, &lowered), by_title, by_kind].into_iter().flatten().min()
}

/// A connection is kept with its higher level first, so each pair has one spelling.
pub fn ordered<'n>(a: &'n Node, b: &'n Node) -> (&'n Node, &'n Node) {
    if a.kind <= b.kind { (a, b) } else { (b, a) }
}

fn between(from: &Node, to: &Node) -> Value {
    json!({ "from_kind": from.kind, "from_ref": from.reference, "to_kind": to.kind, "to_ref": to.reference })
}

/// A connection, as the neighbour it leads to.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Link {
    pub kind: Kind,
    pub reference: String,
    pub name: String,
    pub title: String,
    pub key: String,
}

impl Link {
    pub fn node(&self) -> Node {
        Node {
            kind: self.kind,
            reference: self.reference.clone(),
            name: self.name.clone(),
            title: self.title.clone(),
            key: self.key.clone(),
        }
    }
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    async fn all(&self, query: Query) -> Result<Vec<Map<String, Value>>, Refusal> {
        Ok(self.0.query_all(query).await?)
    }

    /// Records of `collection` whose `field` is one of `values`, a chunk of them at a time.
    async fn among(
        &self,
        collection: &str,
        field: &str,
        values: &[String],
        extra: Value,
    ) -> Result<Vec<Map<String, Value>>, Refusal> {
        let mut found = Vec::new();
        for chunk in values.chunks(CHUNK) {
            let mut filter = extra.clone();
            filter[field] = json!({ "in": chunk });
            found.extend(self.all(Query::new(collection).filter(filter)).await?);
        }
        Ok(found)
    }

    pub async fn run(&self, writes: Vec<DataRequest>) -> Result<(), Refusal> {
        for batch in writes.chunks(BATCH) {
            self.0.batch(batch.to_vec()).await?;
        }
        Ok(())
    }

    pub async fn version(&self) -> Result<i64, Refusal> {
        let catalogue: Option<Map<String, Value>> = self.0.get("catalogue", CATALOGUE).await?;
        Ok(catalogue.and_then(|record| record.get("_version")?.as_i64()).unwrap_or_default())
    }

    /// What is pinned to the top of a resource's page, as `plugin:insight`, in order.
    pub async fn pinned(&self, at: &str) -> Result<Vec<String>, Refusal> {
        let held: Option<Map<String, Value>> = self.0.get(PINNED, at.to_string()).await?;
        let Some(held) = held else { return Ok(Vec::new()) };
        Ok(held
            .get("insights")
            .and_then(Value::as_array)
            .map(|pinned| pinned.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default())
    }

    /// Keeps the whole row for one resource: what is pinned and the order it is read in. Pinning
    /// nothing takes the row away rather than leaving an empty one behind.
    pub async fn set_pinned(&self, at: &str, insights: &[String], by: &str) -> Result<(), Refusal> {
        if insights.is_empty() {
            self.0.delete(PINNED, at.to_string(), None).await?;
            return Ok(());
        }
        let values = json!({ "resource": at, "insights": insights, "changed_by": by });
        let _: (Value, bool) = self.0.upsert(PINNED, &["resource"], values).await?;
        Ok(())
    }

    pub async fn definitions(&self) -> Result<Vec<Definition>, Refusal> {
        let query = Query::new("connection-definitions")
            .order(Order::asc("position"))
            .order(Order::asc("name"));
        let stored = self.all(query).await?;
        Ok(stored
            .into_iter()
            .map(|row| Definition {
                name: text(&row, "name"),
                kinds: serde_json::from_value(row.get("kinds").cloned().unwrap_or_default())
                    .unwrap_or_default(),
            })
            .collect())
    }

    /// Where the last connection definition sits, so a new one goes after it.
    pub async fn last_position(&self) -> Result<i64, Refusal> {
        let highest = Aggregate::new("connection-definitions")
            .measure("last", Measure::Max("position".into()));
        let groups = self.0.aggregate(highest).await?;
        Ok(groups.first().and_then(|group| group.get("last")?.as_i64()).unwrap_or(0))
    }

    pub async fn definition(&self, name: &str) -> Result<Definition, Refusal> {
        let definitions = self.definitions().await?;
        definitions
            .into_iter()
            .find(|definition| definition.name.eq_ignore_ascii_case(name))
            .ok_or_else(|| Refusal::missing(format!("there is no connection definition {name}")))
    }

    pub async fn delete_definition(&self, name: &str) -> Result<bool, Refusal> {
        if self.0.get::<Value>("connection-definitions", name).await?.is_none() {
            return Ok(false);
        }
        self.0.batch(vec![DataRequest::delete("connection-definitions", name), touch()]).await?;
        Ok(true)
    }

    /// Resources from their records, with each owner named rather than by its ID.
    async fn resources_of(
        &self,
        records: Vec<Map<String, Value>>,
    ) -> Result<Vec<Resource>, Refusal> {
        let owners: Vec<String> = records
            .iter()
            .map(|record| text(record, "owner_team"))
            .filter(|owner| !owner.is_empty())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // An owner is a team, which the platform keeps (T69).
        let named: BTreeMap<String, String> = self
            .among("core.teams", "id", &owners, json!({}))
            .await?
            .iter()
            .map(|owner| (text(owner, "id"), text(owner, "name")))
            .collect();
        records
            .into_iter()
            .map(|mut record| {
                let owner = named.get(&text(&record, "owner_team")).cloned();
                record.insert("owner".into(), json!(owner));
                serde_json::from_value(Value::Object(record)).map_err(|err| {
                    Refusal::unavailable(format!("a stored resource could not be read: {err}"))
                })
            })
            .collect()
    }

    /// Organisations and teams as the catalogue shows them, read from core and never copied
    /// here (T69). Neither has metadata or an owner, because core keeps neither.
    async fn kept_in_core(&self, kind: Kind) -> Result<Vec<Resource>, Refusal> {
        let Some(collection) = core_collection(kind) else { return Ok(Vec::new()) };
        let rows = self.all(Query::new(collection)).await?;
        Ok(rows
            .iter()
            .filter_map(|row| {
                let at = row
                    .get("created_at")
                    .and_then(Value::as_str)
                    .and_then(|at| at.parse::<chrono::DateTime<chrono::Utc>>().ok())
                    .unwrap_or_default();
                Some(Resource {
                    id: text(row, "id").parse().ok()?,
                    kind,
                    name: text(row, "name"),
                    title: text(row, "title"),
                    description: text(row, "description"),
                    metadata: Map::new(),
                    owner: None,
                    email: Some(text(row, "email")).filter(|email| !email.is_empty()),
                    source: PLATFORM.to_string(),
                    created_at: at,
                    updated_at: at,
                })
            })
            .collect())
    }

    /// One of core's own rows, by ID, for the few things the catalogue shows that core alone
    /// knows: which organisation a team is in, and which plugin provides it.
    pub async fn core_row(
        &self,
        kind: Kind,
        id: &str,
    ) -> Result<Option<Map<String, Value>>, Refusal> {
        let Some(collection) = core_collection(kind).filter(|_| !id.is_empty()) else {
            return Ok(None);
        };
        let found = self.all(Query::new(collection).filter(json!({ "id": id })).limit(1)).await?;
        Ok(found.into_iter().next())
    }

    pub async fn resource(&self, kind: Kind, name: &str) -> Result<Option<Resource>, Refusal> {
        if core_collection(kind).is_some() {
            let found = self.kept_in_core(kind).await?;
            return Ok(found.into_iter().find(|resource| resource.name == name));
        }
        let query = Query::new("resources").filter(json!({ "kind": kind, "name": name })).limit(1);
        let found = self.0.query::<Map<String, Value>>(query).await?.records;
        Ok(self.resources_of(found).await?.into_iter().next())
    }

    /// The people in a team, with how each came to be there. Teams and their members are core's
    /// (T64, T69); the catalogue shows them and keeps no list of its own.
    pub async fn team_members(&self, team: &str) -> Result<Vec<(String, String, String)>, Refusal> {
        let members = self
            .all(
                Query::new("core.team-members")
                    .filter(json!({ "team_id": team }))
                    .fields(&["user_id", "source", "provider"]),
            )
            .await?;
        let ids: Vec<String> = members.iter().map(|member| text(member, "user_id")).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let people = self
            .all(
                Query::new("core.users")
                    .filter(json!({ "id": { "in": ids } }))
                    .fields(&["id", "login", "name"]),
            )
            .await?;
        let mut found: Vec<(String, String, String)> = members
            .iter()
            .filter_map(|member| {
                let id = text(member, "user_id");
                let person = people.iter().find(|person| text(person, "id") == id)?;
                let how = match (text(member, "source").as_str(), text(member, "provider")) {
                    ("provider", provider) if !provider.is_empty() => {
                        format!("put there by {provider}")
                    }
                    ("provider", _) => "put there by a provider".to_string(),
                    ("default", _) => "everybody joins this team".to_string(),
                    _ => "added by hand".to_string(),
                };
                Some((id, text(person, "login"), how))
            })
            .collect();
        found.sort_by_key(|(_, login, _)| login.to_lowercase());
        Ok(found)
    }

    /// The teams somebody is in, as core keeps them (T69), for the other side of a team's
    /// members: a person's page shows the teams they are in without anybody drawing them.
    pub async fn teams_of(&self, user: &str) -> Result<Vec<Resource>, Refusal> {
        let held = self
            .all(
                Query::new("core.team-members")
                    .filter(json!({ "user_id": user }))
                    .fields(&["team_id"]),
            )
            .await?;
        let ids: Vec<String> = held.iter().map(|row| text(row, "team_id")).collect();
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let teams = self.kept_in_core(Kind::Team).await?;
        Ok(teams.into_iter().filter(|team| ids.contains(&team.id.to_string())).collect())
    }

    /// The resources that name `owner`, such as a team, as their owner.
    pub async fn owned_by(&self, owner: Uuid) -> Result<Vec<Resource>, Refusal> {
        let query = Query::new("resources").filter(json!({ "owner_team": owner }));
        let found = self.all(query).await?;
        self.resources_of(found).await
    }

    /// The resources among `refs` that exist, by kind and name: the stored ones, and the
    /// organisations and teams core keeps.
    pub async fn existing(&self, refs: &[Ref]) -> Result<Vec<Resource>, Refusal> {
        let mut by_kind: BTreeMap<Kind, Vec<String>> = BTreeMap::new();
        for wanted in refs {
            by_kind.entry(wanted.kind).or_default().push(wanted.name.clone());
        }
        let (mut found, mut theirs) = (Vec::new(), Vec::new());
        for (kind, names) in by_kind {
            if core_collection(kind).is_some() {
                let kept = self.kept_in_core(kind).await?;
                theirs.extend(kept.into_iter().filter(|resource| names.contains(&resource.name)));
                continue;
            }
            found.extend(self.among("resources", "name", &names, json!({ "kind": kind })).await?);
        }
        let mut resources = self.resources_of(found).await?;
        resources.append(&mut theirs);
        Ok(resources)
    }

    pub async fn resources(
        &self,
        kind: Kind,
        text: Option<&str>,
        limit: i64,
        offset: usize,
    ) -> Result<Vec<Resource>, Refusal> {
        if core_collection(kind).is_some() {
            let mut found = self.kept_in_core(kind).await?;
            if let Some(wanted) = text {
                found.retain(|resource| {
                    lower_contains(&resource.name, wanted)
                        || lower_contains(&resource.title, wanted)
                });
            }
            found.sort_by(|a, b| a.name.cmp(&b.name));
            return Ok(page(found, limit, offset));
        }
        let mut found = self.all(Query::new("resources").filter(json!({ "kind": kind }))).await?;
        if let Some(wanted) = text {
            found.retain(|row| {
                lower_contains(&self::text(row, "name"), wanted)
                    || lower_contains(&self::text(row, "title"), wanted)
            });
        }
        found.sort_by_key(|row| self::text(row, "name"));
        self.resources_of(page(found, limit, offset)).await
    }

    /// Users who have signed in and service accounts, as nodes; neither is disabled.
    async fn people(&self, kind: Kind) -> Result<Vec<Node>, Refusal> {
        let (collection, fields): (&str, &[&str]) = match kind {
            Kind::User => ("core.users", &["id", "login", "name", "provider"]),
            Kind::ServiceAccount => ("core.service-accounts", &["id", "name", "description"]),
            _ => return Ok(Vec::new()),
        };
        let found = self
            .all(Query::new(collection).filter(json!({ "disabled": false })).fields(fields))
            .await?;
        Ok(found
            .iter()
            .map(|row| match kind {
                Kind::User => Node {
                    kind,
                    reference: text(row, "id"),
                    name: text(row, "login"),
                    title: text(row, "name"),
                    key: format!("{}/{}", text(row, "provider"), text(row, "login")),
                },
                _ => Node {
                    kind,
                    reference: text(row, "id"),
                    name: text(row, "name"),
                    title: text(row, "description"),
                    key: text(row, "name"),
                },
            })
            .collect())
    }

    /// Users or service accounts from core, which are never copied here.
    pub async fn principals(
        &self,
        kind: Kind,
        text: Option<&str>,
        limit: i64,
        offset: usize,
    ) -> Result<Vec<Node>, Refusal> {
        let mut found = self.people(kind).await?;
        if let Some(wanted) = text {
            found.retain(|node| {
                lower_contains(&node.name, wanted)
                    || (kind == Kind::User && lower_contains(&node.title, wanted))
            });
        }
        found.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(page(found, limit, offset))
    }

    /// Best matches first: the exact name, then names starting with `text`, then the rest.
    /// Resources of `kinds` ranked by how well they answer `text` (see `closeness`), best first.
    /// Empty text lists them all, by kind then name, for choosing from before anything is typed.
    pub async fn search(
        &self,
        text: &str,
        kinds: &[Kind],
        limit: i64,
    ) -> Result<Vec<Node>, Refusal> {
        let mut found: Vec<(u32, Node)> = Vec::new();
        let stored: Vec<String> =
            kinds.iter().filter(|kind| kind.stored()).map(|kind| kind.name().to_string()).collect();
        for row in self.among("resources", "kind", &stored, json!({})).await? {
            let Ok(kind) = serde_json::from_value::<Kind>(json!(self::text(&row, "kind"))) else {
                continue;
            };
            let (name, title) = (self::text(&row, "name"), self::text(&row, "title"));
            if let Some(close) = closeness(text, &name, &title, kind) {
                let reference = self::text(&row, "id");
                found.push((close, Node { kind, reference, key: name.clone(), name, title }));
            }
        }
        for kind in [Kind::Organisation, Kind::Team].into_iter().filter(|kind| kinds.contains(kind))
        {
            for resource in self.kept_in_core(kind).await? {
                if let Some(close) = closeness(text, &resource.name, &resource.title, kind) {
                    let node = Node {
                        kind,
                        reference: resource.id.to_string(),
                        key: resource.name.clone(),
                        name: resource.name,
                        title: resource.title,
                    };
                    found.push((close, node));
                }
            }
        }
        for kind in
            [Kind::User, Kind::ServiceAccount].into_iter().filter(|kind| kinds.contains(kind))
        {
            for node in self.people(kind).await? {
                // A service account's title is its description, which is not what it is called.
                let title = if kind == Kind::User { node.title.as_str() } else { "" };
                if let Some(close) = closeness(text, &node.name, title, kind) {
                    found.push((close, node));
                }
            }
        }
        found.sort_by(|(a_close, a), (b_close, b)| {
            a_close
                .cmp(b_close)
                .then_with(|| a.kind.cmp(&b.kind))
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });
        found.truncate(limit.max(0) as usize);
        Ok(found.into_iter().map(|(_, node)| node).collect())
    }

    pub async fn counts(&self) -> Result<BTreeMap<Kind, i64>, Refusal> {
        let counted = Aggregate::new("resources")
            .group_by("kind")
            .measure("count", Measure::Count("*".into()));
        let mut counts = BTreeMap::new();
        for group in self.0.aggregate(counted).await? {
            if let Ok(kind) =
                serde_json::from_value::<Kind>(group.get("kind").cloned().unwrap_or_default())
            {
                counts.insert(kind, group.get("count").and_then(Value::as_i64).unwrap_or(0));
            }
        }
        for (kind, collection) in
            [(Kind::User, "core.users"), (Kind::ServiceAccount, "core.service-accounts")]
        {
            let live = Aggregate::new(collection)
                .filter(json!({ "disabled": false }))
                .measure("count", Measure::Count("*".into()));
            let total = self.0.aggregate(live).await?;
            counts.insert(
                kind,
                total.first().and_then(|group| group.get("count")?.as_i64()).unwrap_or(0),
            );
        }
        for kind in [Kind::Organisation, Kind::Team] {
            let counted = Aggregate::new(core_collection(kind).unwrap_or_default())
                .measure("count", Measure::Count("*".into()));
            let total = self.0.aggregate(counted).await?;
            counts.insert(
                kind,
                total.first().and_then(|group| group.get("count")?.as_i64()).unwrap_or(0),
            );
        }
        Ok(counts)
    }

    /// Users by `login` or `provider/login` (so maybe several), and service accounts by name.
    pub async fn core_nodes(&self, refs: &[Ref]) -> Result<Vec<(String, Node)>, Refusal> {
        let mut found = Vec::new();
        if refs.iter().any(|wanted| wanted.kind == Kind::User) {
            let everyone = self
                .all(Query::new("core.users").fields(&["id", "login", "name", "provider"]))
                .await?;
            for wanted in refs.iter().filter(|wanted| wanted.kind == Kind::User) {
                for row in &everyone {
                    let key = format!("{}/{}", text(row, "provider"), text(row, "login"));
                    if text(row, "login") == wanted.name || key == wanted.name {
                        let node = Node {
                            kind: Kind::User,
                            reference: text(row, "id"),
                            name: text(row, "login"),
                            title: text(row, "name"),
                            key,
                        };
                        found.push((wanted.name.clone(), node));
                    }
                }
            }
        }
        for kind in [Kind::Organisation, Kind::Team] {
            let names: Vec<&Ref> = refs.iter().filter(|wanted| wanted.kind == kind).collect();
            if names.is_empty() {
                continue;
            }
            for resource in self.kept_in_core(kind).await? {
                if names.iter().any(|wanted| wanted.name == resource.name) {
                    let node = Node {
                        kind,
                        reference: resource.id.to_string(),
                        key: resource.name.clone(),
                        name: resource.name.clone(),
                        title: resource.title.clone(),
                    };
                    found.push((resource.name, node));
                }
            }
        }
        let accounts: Vec<String> = refs
            .iter()
            .filter(|wanted| wanted.kind == Kind::ServiceAccount)
            .map(|wanted| wanted.name.clone())
            .collect();
        for row in self.among("core.service-accounts", "name", &accounts, json!({})).await? {
            let node = Node {
                kind: Kind::ServiceAccount,
                reference: text(&row, "id"),
                name: text(&row, "name"),
                title: text(&row, "description"),
                key: text(&row, "name"),
            };
            found.push((text(&row, "name"), node));
        }
        Ok(found)
    }

    /// Users' `provider/login` by core ID, for holders `rbac` names only by ID and login.
    pub async fn user_keys(&self, ids: &[String]) -> Result<BTreeMap<String, String>, Refusal> {
        let found = self.among("core.users", "id", ids, json!({})).await?;
        Ok(found
            .iter()
            .map(|row| {
                (text(row, "id"), format!("{}/{}", text(row, "provider"), text(row, "login")))
            })
            .collect())
    }

    pub async fn service_account_owner(&self, id: &str) -> Result<Option<Uuid>, Refusal> {
        let account: Option<Map<String, Value>> = self.0.get("core.service-accounts", id).await?;
        Ok(account.and_then(|account| text(&account, "owner_id").parse().ok()))
    }

    /// Names and titles for the other ends of connections, leaving out ends that are gone.
    async fn labelled(&self, ends: Vec<(Kind, String)>) -> Result<Vec<Link>, Refusal> {
        let wanted = |kind: Kind| -> Vec<String> {
            ends.iter()
                .filter(|(end, _)| *end == kind)
                .map(|(_, reference)| reference.clone())
                .collect()
        };
        let stored: Vec<String> = ends
            .iter()
            .filter(|(kind, _)| kind.stored())
            .map(|(_, reference)| reference.clone())
            .collect();
        let mut named: BTreeMap<(Kind, String), (String, String, String)> = BTreeMap::new();
        for row in self.among("resources", "id", &stored, json!({})).await? {
            if let Ok(kind) = serde_json::from_value::<Kind>(json!(text(&row, "kind"))) {
                named.insert(
                    (kind, text(&row, "id")),
                    (text(&row, "name"), text(&row, "title"), text(&row, "name")),
                );
            }
        }
        for kind in [Kind::Organisation, Kind::Team] {
            let collection = core_collection(kind).unwrap_or_default();
            for row in self.among(collection, "id", &wanted(kind), json!({})).await? {
                let name = text(&row, "name");
                named.insert((kind, text(&row, "id")), (name.clone(), text(&row, "title"), name));
            }
        }
        for row in self.among("core.users", "id", &wanted(Kind::User), json!({})).await? {
            let key = format!("{}/{}", text(&row, "provider"), text(&row, "login"));
            named.insert(
                (Kind::User, text(&row, "id")),
                (text(&row, "login"), text(&row, "name"), key),
            );
        }
        for row in self
            .among("core.service-accounts", "id", &wanted(Kind::ServiceAccount), json!({}))
            .await?
        {
            let name = text(&row, "name");
            named.insert(
                (Kind::ServiceAccount, text(&row, "id")),
                (name.clone(), text(&row, "description"), name),
            );
        }
        for role in wanted(Kind::Role) {
            named.insert((Kind::Role, role.clone()), (role.clone(), String::new(), role));
        }
        Ok(ends
            .into_iter()
            .filter_map(|(kind, reference)| {
                let (name, title, key) = named.get(&(kind, reference.clone()))?.clone();
                Some(Link { kind, reference, name, title, key })
            })
            .collect())
    }

    /// The other ends of `node`'s stored connections, whichever way round they were kept.
    async fn ends(
        &self,
        node: &Node,
        other: Option<Kind>,
        source: Option<&str>,
    ) -> Result<Vec<(Kind, String)>, Refusal> {
        let mut ends = Vec::new();
        for (this, that) in [("from", "to"), ("to", "from")] {
            let mut filter =
                json!({ format!("{this}_kind"): node.kind, format!("{this}_ref"): node.reference });
            if let Some(other) = other {
                filter[format!("{that}_kind")] = json!(other);
            }
            if let Some(source) = source {
                filter["source"] = json!(source);
            }
            for row in self.all(Query::new("connections").filter(filter)).await? {
                if let Ok(kind) =
                    serde_json::from_value::<Kind>(json!(text(&row, &format!("{that}_kind"))))
                {
                    ends.push((kind, text(&row, &format!("{that}_ref"))));
                }
            }
        }
        Ok(ends)
    }

    /// Every connection of `node`, whichever way it was stored, optionally only to `other`.
    pub async fn links(&self, node: &Node, other: Option<Kind>) -> Result<Vec<Link>, Refusal> {
        let ends = self.ends(node, other, None).await?;
        let mut links = self.labelled(ends).await?;
        links.sort_by(|a, b| (a.kind, &a.name).cmp(&(b.kind, &b.name)));
        Ok(links)
    }

    /// The other ends of `node`'s connections to `kind` that `source` made.
    pub async fn sourced(
        &self,
        node: &Node,
        kind: Kind,
        source: &str,
    ) -> Result<Vec<Link>, Refusal> {
        let ends = self.ends(node, Some(kind), Some(source)).await?;
        self.labelled(ends).await
    }

    async fn connection(&self, from: &Node, to: &Node) -> Result<Option<String>, Refusal> {
        let query = Query::new("connections").filter(between(from, to)).fields(&["id"]).limit(1);
        let found = self.0.query::<Map<String, Value>>(query).await?.records;
        Ok(found.first().map(|row| text(row, "id")))
    }

    /// Which of `pairs` are already connected, in the order they were asked.
    pub async fn connected(&self, pairs: &[(&Node, &Node)]) -> Result<Vec<bool>, Refusal> {
        let mut connected = Vec::with_capacity(pairs.len());
        for (a, b) in pairs {
            let (from, to) = ordered(a, b);
            connected.push(self.connection(from, to).await?.is_some());
        }
        Ok(connected)
    }

    /// True when the connection is new.
    pub async fn connect(
        &self,
        a: &Node,
        b: &Node,
        source: &str,
        by: &str,
    ) -> Result<bool, Refusal> {
        let (from, to) = ordered(a, b);
        let mut values = between(from, to);
        values["source"] = json!(source);
        values["created_by"] = json!(by);
        match self.0.batch(vec![DataRequest::insert("connections", values), touch()]).await {
            Ok(_) => Ok(true),
            Err(err) if taken(&err) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// True when there was such a connection.
    pub async fn disconnect(&self, a: &Node, b: &Node) -> Result<bool, Refusal> {
        let (from, to) = ordered(a, b);
        let Some(id) = self.connection(from, to).await? else { return Ok(false) };
        self.0.batch(vec![DataRequest::delete("connections", id), touch()]).await?;
        Ok(true)
    }

    /// Takes its connections with it; resources it owned keep going without an owner.
    pub async fn delete(&self, resource: &Resource) -> Result<(), Refusal> {
        let reference = resource.id.to_string();
        for this in ["from", "to"] {
            let filter =
                json!({ format!("{this}_kind"): resource.kind, format!("{this}_ref"): reference });
            self.0.delete_where("connections", "id", filter).await?;
        }
        self.0.batch(vec![DataRequest::delete("resources", reference), touch()]).await?;
        Ok(())
    }

    /// Drops what `publisher` stopped publishing: the resource if it made it, else its connections.
    pub async fn forget(&self, kind: Kind, name: &str, publisher: &str) -> Result<bool, Refusal> {
        let Some(resource) = self.resource(kind, name).await? else { return Ok(false) };
        if resource.source == publisher {
            self.delete(&resource).await?;
            return Ok(true);
        }
        let reference = resource.id.to_string();
        let mut removed = 0;
        for this in ["from", "to"] {
            let filter = json!({
                format!("{this}_kind"): kind, format!("{this}_ref"): reference, "source": publisher,
            });
            removed += self.0.delete_where("connections", "id", filter).await?;
        }
        if removed > 0 {
            self.0.batch(vec![touch()]).await?;
        }
        Ok(removed > 0)
    }
}

/// A write refused because what it would make already exists.
pub fn taken(err: &PluginError) -> bool {
    err.is_duplicate()
}
