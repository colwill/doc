//! What the plugin keeps through the data API, and what it reads of core's: users, service
//! accounts, plugins and the permissions they declare, from the `core.*` collections.

use std::collections::{BTreeMap, BTreeSet};

use doc_permissions::{ANY, CORE, Grants, Group, Permission, Subject};
use doc_plugin_sdk::{
    Aggregate, Backend, Collection, Declaration, Field, ListOf, Measure, Order, PluginError, Query,
};
use serde::de::DeserializeOwned;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::model::{
    Assignment, Conditions, GroupRecord, Holder, Refusal, Rule, RuleGrants, SignIn, membership,
};
use crate::offboarding::OffboardingRule;

/// How many IDs one `in` condition carries when labels are looked up.
const CHUNK: usize = 500;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "groups",
            Collection::new()
                .field("id", Field::text().key().describe("`<plugin>/<name>`"))
                .field("plugin", Field::text().required())
                .field("name", Field::text().required())
                .field("kind", Field::text().required().one_of(&["user", "service"]))
                .field("description", Field::text().required().default(json!("")))
                .field("permissions", Field::list(ListOf::Text).required().default(json!([])))
                .index(&["plugin", "name"]),
        )
        .collection(
            "assignments",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("holder_kind", Field::text().required().one_of(&["user", "service", "team"]))
                .field("holder", Field::uuid().required())
                .field("permission", Field::text().required())
                .field("plugin", Field::text().required())
                .field("source", Field::text().required())
                .field("granted_by", Field::text().required())
                .field("granted_at", Field::timestamp().required().default(json!("now")))
                .unique(&["holder_kind", "holder", "permission"])
                .index(&["plugin"])
                .index(&["permission"]),
        )
        .collection(
            // What happens to somebody when the directory they came from says they have left
            // (T67). Core carries each of these out; this only decides which apply.
            "offboarding-rules",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("name", Field::text().required())
                .field("description", Field::text().required().default(json!("")))
                // Which provider's leavers it answers for; empty means any of them.
                .field("provider", Field::text().required().default(json!("")))
                .field("disable", Field::boolean().required().default(json!(true)))
                .field("remove_identity", Field::boolean().required().default(json!(true)))
                .field(
                    "remove_provided_memberships",
                    Field::boolean().required().default(json!(true)),
                )
                .field("remove_memberships", Field::boolean().required().default(json!(false)))
                .field("revoke_tokens", Field::boolean().required().default(json!(true)))
                .field("enabled", Field::boolean().required().default(json!(true)))
                // Listed by name, which the data API sorts by only where there is an index.
                .index(&["name"])
                .index(&["provider"]),
        )
        .collection(
            "attributes",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "holder_kind",
                    Field::text().required().one_of(&["user", "service", "group"]),
                )
                .field("holder", Field::text().required())
                .field("key", Field::text().required())
                .field("value", Field::text().required())
                .unique(&["holder_kind", "holder", "key"]),
        )
        .collection(
            "onboarding-rules",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("name", Field::text().required())
                .field("description", Field::text().required().default(json!("")))
                .field("conditions", Field::json().required())
                .field("grants", Field::json().required())
                .field("enabled", Field::boolean().required().default(json!(true)))
                .unique(&["name"]),
        )
        .collection(
            "sign-ins",
            Collection::new()
                .field("user_id", Field::uuid().key())
                .field("provider", Field::text().required())
                .field("organisations", Field::list(ListOf::Text).required().default(json!([])))
                .field("teams", Field::list(ListOf::Text).required().default(json!([])))
                .field("at", Field::timestamp().required().default(json!("now"))),
        )
}

fn read<T: DeserializeOwned>(record: Map<String, Value>) -> Result<T, Refusal> {
    serde_json::from_value(Value::Object(record))
        .map_err(|err| Refusal::unavailable(format!("a stored record could not be read: {err}")))
}

fn text(record: &Map<String, Value>, field: &str) -> String {
    record.get(field).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// The same, for a value that has already been built up.
fn text_of(value: &Value, field: &str) -> String {
    value.get(field).and_then(Value::as_str).unwrap_or_default().to_string()
}

/// A write refused because the record it would make already exists.
fn exists(err: &PluginError) -> bool {
    err.is_duplicate()
}

fn group_id(plugin: &str, name: &str) -> String {
    format!("{plugin}/{name}")
}

/// The plugins that exist and the custom permissions they declare, as core registered them.
pub struct Known {
    pub plugins: BTreeSet<String>,
    custom: BTreeSet<(String, String, String)>,
}

impl Known {
    /// Every plugin a permission can name: `core` and each registered one.
    pub fn names(&self) -> Vec<String> {
        std::iter::once(CORE.to_string()).chain(self.plugins.iter().cloned()).collect()
    }

    /// The custom permissions a plugin declares for `pluginuser` or `pluginservice`.
    pub fn customs(&self, plugin: &str, kind: &str) -> Vec<String> {
        self.custom
            .iter()
            .filter(|(owner, declared, _)| owner == plugin && declared == kind)
            .map(|(_, _, name)| name.clone())
            .collect()
    }

    pub fn exists(&self, plugin: &str) -> Result<(), Refusal> {
        // `*` is every plugin at once, including ones registered later, so nothing to look up.
        match plugin == CORE || plugin == ANY || self.plugins.contains(plugin) {
            true => Ok(()),
            false => Err(Refusal::bad(format!("no plugin called {plugin} is registered"))),
        }
    }

    /// Every plugin a permission can name, with `*` for all of them at once.
    pub fn namable(&self) -> Vec<String> {
        std::iter::once(ANY.to_string()).chain(self.names()).collect()
    }

    pub fn check(&self, permission: &Permission) -> Result<(), Refusal> {
        let plugin = &permission.plugin;
        self.exists(plugin)?;
        let custom = match &permission.subject {
            Subject::PluginUser { name, .. } => Some(("pluginuser", name)),
            Subject::PluginService { name, .. } => Some(("pluginservice", name)),
            _ => None,
        };
        match custom {
            Some((kind, name))
                if !self.custom.contains(&(plugin.clone(), kind.to_string(), name.clone())) =>
            {
                Err(Refusal::bad(format!("{plugin} declares no {kind} permission called {name}")))
            }
            _ => Ok(()),
        }
    }
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    async fn all(&self, query: Query) -> Result<Vec<Map<String, Value>>, Refusal> {
        Ok(self.0.query_all(query).await?)
    }

    pub async fn known(&self) -> Result<Known, Refusal> {
        let plugins = self.all(Query::new("core.plugins").fields(&["id"])).await?;
        let declared = self.all(Query::new("core.plugin-permissions")).await?;
        Ok(Known {
            plugins: plugins.iter().map(|plugin| text(plugin, "id")).collect(),
            custom: declared
                .iter()
                .map(|row| (text(row, "plugin"), text(row, "kind"), text(row, "name")))
                .collect(),
        })
    }

    /// The holder's name, or `None` when no such user, service account or group exists.
    pub async fn label(&self, holder: &Holder) -> Result<Option<String>, Refusal> {
        let (collection, key, field) = match holder {
            Holder::User { id } => ("core.users", id.to_string(), "login"),
            Holder::Service { id } => ("core.service-accounts", id.to_string(), "name"),
            Holder::Team { id } => ("core.teams", id.to_string(), "title"),
            Holder::Group { plugin, name } => ("groups", group_id(plugin, name), "id"),
        };
        let found: Option<Map<String, Value>> = self.0.get(collection, key).await?;
        Ok(found.map(|record| text(&record, field)))
    }

    /// Labels for many users and service accounts at once, by `(kind, id)`.
    async fn labels(
        &self,
        holders: &BTreeSet<(String, String)>,
    ) -> Result<BTreeMap<(String, String), String>, Refusal> {
        let mut labels = BTreeMap::new();
        for (kind, collection, field) in
            [("user", "core.users", "login"), ("service", "core.service-accounts", "name")]
        {
            let ids: Vec<&String> =
                holders.iter().filter(|(k, _)| k == kind).map(|(_, id)| id).collect();
            for chunk in ids.chunks(CHUNK) {
                let query = Query::new(collection)
                    .filter(json!({ "id": { "in": chunk } }))
                    .fields(&["id", field]);
                for row in self.all(query).await? {
                    labels.insert((kind.to_string(), text(&row, "id")), text(&row, field));
                }
            }
        }
        Ok(labels)
    }

    /// Users with this login, one per identity provider that has signed someone in under it.
    pub async fn users(&self, login: &str) -> Result<Vec<Value>, Refusal> {
        let everyone = self
            .all(Query::new("core.users").fields(&["id", "provider", "login", "name", "disabled"]))
            .await?;
        let mut found: Vec<Map<String, Value>> = everyone
            .into_iter()
            .filter(|user| text(user, "login").eq_ignore_ascii_case(login))
            .collect();
        found.sort_by_key(|user| text(user, "provider"));
        Ok(found.into_iter().map(Value::Object).collect())
    }

    /// Everyone who has signed in, by login, for the people page.
    pub async fn people(&self) -> Result<Vec<Value>, Refusal> {
        let mut users = self
            .all(Query::new("core.users").fields(&["id", "login", "provider", "disabled"]))
            .await?;
        users.sort_by_key(|user| (text(user, "login").to_lowercase(), text(user, "provider")));
        let mut accounts = self
            .all(
                Query::new("core.service-accounts").fields(&["id", "name", "owner_id", "disabled"]),
            )
            .await?;
        accounts.sort_by_key(|account| text(account, "name").to_lowercase());
        let mut people: Vec<Value> = users
            .into_iter()
            .take(1000)
            .map(|user| {
                json!({
                    "kind": "user", "id": user.get("id"), "name": user.get("login"),
                    "provider": user.get("provider"), "disabled": user.get("disabled"),
                })
            })
            .collect();
        people.extend(accounts.into_iter().take(1000).map(|account| {
            json!({
                "kind": "service", "id": account.get("id"), "name": account.get("name"),
                "owner_id": account.get("owner_id"), "disabled": account.get("disabled"),
            })
        }));
        Ok(people)
    }

    /// Every team, by title, for the people page. A team holds permissions like a person does, so
    /// it has a page of its own (T67).
    pub async fn teams(&self) -> Result<Vec<Value>, Refusal> {
        let mut teams = self
            .all(Query::new("core.teams").fields(&[
                "id",
                "name",
                "title",
                "parent_id",
                "default",
                "provider",
            ]))
            .await?;
        teams.sort_by_key(|team| text(team, "title").to_lowercase());
        Ok(teams.into_iter().take(1000).map(Value::Object).collect())
    }

    /// The people directly in a team, by login; the teams inside it are not counted here.
    pub async fn team_members(&self, team: Uuid) -> Result<Vec<Value>, Refusal> {
        let rows = self
            .all(
                Query::new("core.team-members")
                    .filter(json!({ "team_id": team.to_string() }))
                    .fields(&["user_id", "source", "provider"]),
            )
            .await?;
        let ids: Vec<String> = rows.iter().map(|row| text(row, "user_id")).collect();
        let logins = self.logins(&ids).await?;
        let mut people: Vec<Value> = rows
            .iter()
            .map(|row| {
                let id = text(row, "user_id");
                json!({
                    "id": id, "login": logins.get(&id).cloned().unwrap_or_default(),
                    "source": row.get("source"), "provider": row.get("provider"),
                })
            })
            .collect();
        people.sort_by_key(|person| text_of(person, "login").to_lowercase());
        Ok(people)
    }

    /// The teams somebody is directly in, by title.
    pub async fn teams_of(&self, user: Uuid) -> Result<Vec<Value>, Refusal> {
        let rows = self
            .all(
                Query::new("core.team-members")
                    .filter(json!({ "user_id": user.to_string() }))
                    .fields(&["team_id", "source", "provider"]),
            )
            .await?;
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        let ids: Vec<String> = rows.iter().map(|row| text(row, "team_id")).collect();
        let mut titles = BTreeMap::new();
        for chunk in ids.chunks(CHUNK) {
            let query = Query::new("core.teams")
                .filter(json!({ "id": { "in": chunk } }))
                .fields(&["id", "title"]);
            for team in self.all(query).await? {
                titles.insert(text(&team, "id"), text(&team, "title"));
            }
        }
        let mut teams: Vec<Value> = rows
            .iter()
            .map(|row| {
                let id = text(row, "team_id");
                json!({
                    "id": id, "title": titles.get(&id).cloned().unwrap_or_default(),
                    "source": row.get("source"), "provider": row.get("provider"),
                })
            })
            .collect();
        teams.sort_by_key(|team| text_of(team, "title").to_lowercase());
        Ok(teams)
    }

    /// Logins for many users at once, by ID.
    async fn logins(&self, ids: &[String]) -> Result<BTreeMap<String, String>, Refusal> {
        let mut logins = BTreeMap::new();
        for chunk in ids.chunks(CHUNK) {
            let query = Query::new("core.users")
                .filter(json!({ "id": { "in": chunk } }))
                .fields(&["id", "login"]);
            for user in self.all(query).await? {
                logins.insert(text(&user, "id"), text(&user, "login"));
            }
        }
        Ok(logins)
    }

    pub async fn service_account(&self, name: &str) -> Result<Option<Uuid>, Refusal> {
        let query = Query::new("core.service-accounts")
            .filter(json!({ "name": name }))
            .fields(&["id"])
            .limit(1);
        let page = self.0.query::<Map<String, Value>>(query).await?;
        Ok(page.records.first().and_then(|row| text(row, "id").parse().ok()))
    }

    /// How many principals are in each group, by its membership permission.
    async fn member_counts(
        &self,
        memberships: &[String],
    ) -> Result<BTreeMap<String, i64>, Refusal> {
        let mut counts = BTreeMap::new();
        for chunk in memberships.chunks(CHUNK) {
            let counted = Aggregate::new("assignments")
                .filter(json!({ "permission": { "in": chunk } }))
                .group_by("permission")
                .measure("members", Measure::Count("*".into()));
            for group in self.0.aggregate(counted).await? {
                counts.insert(
                    text(&group, "permission"),
                    group.get("members").and_then(Value::as_i64).unwrap_or(0),
                );
            }
        }
        Ok(counts)
    }

    async fn records(&self, groups: Vec<Map<String, Value>>) -> Result<Vec<GroupRecord>, Refusal> {
        let memberships: Vec<String> = groups
            .iter()
            .map(|group| membership(&text(group, "plugin"), &text(group, "name")))
            .collect();
        let counts = self.member_counts(&memberships).await?;
        groups
            .into_iter()
            .map(|mut group| {
                let members = counts
                    .get(&membership(&text(&group, "plugin"), &text(&group, "name")))
                    .copied();
                group.insert("members".into(), json!(members.unwrap_or(0)));
                if let Some(Value::Array(permissions)) = group.get_mut("permissions") {
                    permissions.sort_by_key(|permission| {
                        permission.as_str().unwrap_or_default().to_string()
                    });
                }
                read(group)
            })
            .collect()
    }

    pub async fn groups(&self, plugin: Option<&str>) -> Result<Vec<GroupRecord>, Refusal> {
        let mut query = Query::new("groups").order(Order::asc("plugin")).order(Order::asc("name"));
        if let Some(plugin) = plugin {
            query = query.filter(json!({ "plugin": plugin }));
        }
        let groups = self.all(query).await?;
        self.records(groups).await
    }

    pub async fn group(&self, plugin: &str, name: &str) -> Result<Option<GroupRecord>, Refusal> {
        let Some(group) =
            self.0.get::<Map<String, Value>>("groups", group_id(plugin, name)).await?
        else {
            return Ok(None);
        };
        Ok(self.records(vec![group]).await?.into_iter().next())
    }

    /// False when a group of that name already exists, in which case nothing is written.
    pub async fn create_group(&self, group: &Group, description: &str) -> Result<bool, Refusal> {
        let permissions: Vec<String> =
            group.permissions().iter().map(Permission::to_string).collect();
        let values = json!({
            "id": group_id(&group.plugin, &group.name), "plugin": group.plugin, "name": group.name,
            "kind": group.kind, "description": description, "permissions": permissions,
        });
        match self.0.insert::<Value>("groups", values).await {
            Ok(_) => Ok(true),
            Err(err) if exists(&err) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// Replaces the description and, when given, the whole set of permissions.
    pub async fn update_group(
        &self,
        group: &Group,
        description: Option<&str>,
        replace: bool,
    ) -> Result<(), Refusal> {
        let mut set = Map::new();
        if let Some(description) = description {
            set.insert("description".into(), json!(description));
        }
        if replace {
            let permissions: Vec<String> =
                group.permissions().iter().map(Permission::to_string).collect();
            set.insert("permissions".into(), json!(permissions));
        }
        let key = group_id(&group.plugin, &group.name);
        let _: Option<Value> = self.0.update("groups", key, Value::Object(set), None).await?;
        Ok(())
    }

    /// Removes the group with its memberships and attributes; false when it did not exist.
    pub async fn delete_group(&self, record: &GroupRecord) -> Result<bool, Refusal> {
        let key = group_id(&record.plugin, &record.name);
        self.0
            .delete_where("assignments", "id", json!({ "permission": record.membership() }))
            .await?;
        self.0
            .delete_where("attributes", "id", json!({ "holder_kind": "group", "holder": key }))
            .await?;
        Ok(self.0.delete("groups", key, None).await?)
    }

    async fn assignments(&self, filter: Value) -> Result<Vec<Assignment>, Refusal> {
        let rows = self.all(Query::new("assignments").filter(filter)).await?;
        let holders: BTreeSet<(String, String)> =
            rows.iter().map(|row| (text(row, "holder_kind"), text(row, "holder"))).collect();
        let labels = self.labels(&holders).await?;
        rows.into_iter()
            .map(|row| {
                let (kind, id) = (text(&row, "holder_kind"), text(&row, "holder"));
                read(Map::from_iter([
                    ("holder".to_string(), json!({ "kind": kind, "id": id })),
                    ("label".to_string(), json!(labels.get(&(kind, id)))),
                    ("permission".to_string(), json!(text(&row, "permission"))),
                    ("source".to_string(), json!(text(&row, "source"))),
                    ("granted_by".to_string(), json!(text(&row, "granted_by"))),
                    ("granted_at".to_string(), json!(text(&row, "granted_at"))),
                ]))
            })
            .collect()
    }

    pub async fn assignments_of(&self, holder: &Holder) -> Result<Vec<Assignment>, Refusal> {
        let mut found = self
            .assignments(json!({ "holder_kind": holder.kind(), "holder": holder.key() }))
            .await?;
        found.sort_by(|a, b| a.permission.cmp(&b.permission));
        Ok(found)
    }

    pub async fn assignments_in(&self, plugin: &str) -> Result<Vec<Assignment>, Refusal> {
        let mut found = self.assignments(json!({ "plugin": plugin })).await?;
        found.sort_by(|a, b| (&a.label, &a.permission).cmp(&(&b.label, &b.permission)));
        Ok(found)
    }

    pub async fn members(&self, record: &GroupRecord) -> Result<Vec<Assignment>, Refusal> {
        let mut found = self.assignments(json!({ "permission": record.membership() })).await?;
        found.sort_by(|a, b| a.label.cmp(&b.label));
        Ok(found)
    }

    /// True when the holder did not already have it; a group's permissions are part of the group.
    pub async fn grant(
        &self,
        holder: &Holder,
        permission: &Permission,
        source: &str,
        by: &str,
    ) -> Result<bool, Refusal> {
        let text = permission.to_string();
        if let Holder::Group { plugin, name } = holder {
            let added = self
                .0
                .change("groups", group_id(plugin, name), |group| {
                    let mut permissions = group.get("permissions")?.as_array()?.clone();
                    if permissions.iter().any(|held| held.as_str() == Some(&text)) {
                        return None;
                    }
                    permissions.push(json!(text));
                    Some(json!({ "permissions": permissions }))
                })
                .await?;
            return Ok(added.is_some());
        }
        let values = json!({
            "holder_kind": holder.kind(), "holder": holder.key(), "permission": text,
            "plugin": permission.plugin, "source": source, "granted_by": by,
        });
        match self.0.insert::<Value>("assignments", values).await {
            Ok(_) => Ok(true),
            Err(err) if exists(&err) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    pub async fn revoke(&self, holder: &Holder, permission: &Permission) -> Result<bool, Refusal> {
        let text = permission.to_string();
        if let Holder::Group { plugin, name } = holder {
            let removed = self
                .0
                .change("groups", group_id(plugin, name), |group| {
                    let permissions = group.get("permissions")?.as_array()?;
                    let kept: Vec<&Value> =
                        permissions.iter().filter(|held| held.as_str() != Some(&text)).collect();
                    (kept.len() < permissions.len()).then(|| json!({ "permissions": kept }))
                })
                .await?;
            return Ok(removed.is_some());
        }
        let filter =
            json!({ "holder_kind": holder.kind(), "holder": holder.key(), "permission": text });
        Ok(self.0.delete_where("assignments", "id", filter).await? > 0)
    }

    /// What core checks for a user or service account: its permissions and groups, and attributes.
    pub async fn grants(&self, holder: &Holder) -> Result<Grants, Refusal> {
        let held = self
            .all(
                Query::new("assignments")
                    .filter(json!({ "holder_kind": holder.kind(), "holder": holder.key() })),
            )
            .await?;
        let permissions: Vec<String> = held.iter().map(|row| text(row, "permission")).collect();
        let mut groups = Vec::new();
        for permission in &permissions {
            let Some((plugin, name)) =
                permission.strip_prefix("plugin:").and_then(|rest| rest.split_once(":group:"))
            else {
                continue;
            };
            if let Some(group) =
                self.0.get::<Map<String, Value>>("groups", group_id(plugin, name)).await?
            {
                groups.push(json!({
                    "plugin": group.get("plugin"), "name": group.get("name"),
                    "kind": group.get("kind"), "permissions": group.get("permissions"),
                }));
            }
        }
        let grants = json!({
            "permissions": permissions,
            "groups": groups,
            "attributes": self.attributes(holder).await?,
        });
        serde_json::from_value(grants)
            .map_err(|err| Refusal::unavailable(format!("stored grants are not valid: {err}")))
    }

    pub async fn attributes(&self, holder: &Holder) -> Result<BTreeMap<String, String>, Refusal> {
        let rows = self
            .all(
                Query::new("attributes")
                    .filter(json!({ "holder_kind": holder.kind(), "holder": holder.key() })),
            )
            .await?;
        Ok(rows.iter().map(|row| (text(row, "key"), text(row, "value"))).collect())
    }

    /// True when the value was new or changed; `replace` false leaves an existing value alone.
    pub async fn set_attribute(
        &self,
        holder: &Holder,
        key: &str,
        value: &str,
        replace: bool,
    ) -> Result<bool, Refusal> {
        let at = json!({ "holder_kind": holder.kind(), "holder": holder.key(), "key": key });
        let found = self
            .0
            .query::<Map<String, Value>>(Query::new("attributes").filter(at.clone()).limit(1))
            .await?;
        match found.records.into_iter().next() {
            Some(existing) if !replace || text(&existing, "value") == value => Ok(false),
            Some(existing) => {
                let set = json!({ "value": value });
                let updated: Option<Value> =
                    self.0.update("attributes", text(&existing, "id"), set, None).await?;
                Ok(updated.is_some())
            }
            None => {
                let mut values = at;
                values["value"] = json!(value);
                match self.0.insert::<Value>("attributes", values).await {
                    Ok(_) => Ok(true),
                    Err(err) if exists(&err) => Ok(false),
                    Err(err) => Err(err.into()),
                }
            }
        }
    }

    /// Brings everything a user holds over to another when core merges them (ADR-0005): each
    /// permission and group the other lacks, and each attribute they have no value for. What the
    /// other already has stays as it is, and the rest goes. Running it again changes nothing.
    pub async fn merge_user(
        &self,
        from: Uuid,
        into: Uuid,
    ) -> Result<(Vec<String>, BTreeMap<String, String>), Refusal> {
        let (merged, kept) = (Holder::User { id: from }, Holder::User { id: into });
        let held: BTreeSet<String> =
            self.assignments_of(&kept).await?.into_iter().map(|held| held.permission).collect();
        let theirs = json!({ "holder_kind": "user", "holder": merged.key() });
        let mut permissions = Vec::new();
        for row in self.all(Query::new("assignments").filter(theirs.clone())).await? {
            let (id, permission) = (text(&row, "id"), text(&row, "permission"));
            if held.contains(&permission) {
                self.0.delete("assignments", id, None).await?;
                continue;
            }
            let moved = json!({ "holder": kept.key() });
            let _: Option<Value> = self.0.update("assignments", id, moved, None).await?;
            permissions.push(permission);
        }
        let values = self.attributes(&kept).await?;
        let mut attributes = BTreeMap::new();
        for row in self.all(Query::new("attributes").filter(theirs)).await? {
            let (id, key) = (text(&row, "id"), text(&row, "key"));
            if values.contains_key(&key) {
                self.0.delete("attributes", id, None).await?;
                continue;
            }
            let moved = json!({ "holder": kept.key() });
            let _: Option<Value> = self.0.update("attributes", id, moved, None).await?;
            attributes.insert(key, text(&row, "value"));
        }
        self.0.delete("sign-ins", from.to_string(), None).await?;
        Ok((permissions, attributes))
    }

    pub async fn remove_attribute(&self, holder: &Holder, key: &str) -> Result<bool, Refusal> {
        let filter = json!({ "holder_kind": holder.kind(), "holder": holder.key(), "key": key });
        Ok(self.0.delete_where("attributes", "id", filter).await? > 0)
    }

    /// The plugins that sign people in, offered when writing an offboarding rule (T67).
    pub async fn identity_providers(&self) -> Result<Vec<String>, Refusal> {
        let plugins = self.all(Query::new("core.plugins")).await?;
        Ok(plugins
            .iter()
            .filter(|plugin| {
                plugin.get("capabilities").and_then(Value::as_array).is_some_and(|held| {
                    held.iter().any(|c| c.as_str() == Some("identity-provider"))
                })
            })
            .map(|plugin| text(plugin, "id"))
            .collect())
    }

    /// The offboarding rules, in the order they are shown (T67).
    pub async fn offboarding_rules(&self) -> Result<Vec<OffboardingRule>, Refusal> {
        Ok(self.0.query_all(Query::new("offboarding-rules").order(Order::asc("name"))).await?)
    }

    pub async fn offboarding_rule(&self, id: Uuid) -> Result<Option<OffboardingRule>, Refusal> {
        Ok(self.0.get("offboarding-rules", id.to_string()).await?)
    }

    pub async fn add_offboarding_rule(&self, rule: Value) -> Result<OffboardingRule, Refusal> {
        Ok(self.0.insert("offboarding-rules", rule).await?)
    }

    pub async fn set_offboarding_rule(
        &self,
        id: Uuid,
        change: Value,
    ) -> Result<Option<OffboardingRule>, Refusal> {
        Ok(self.0.update("offboarding-rules", id.to_string(), change, None).await?)
    }

    pub async fn delete_offboarding_rule(&self, id: Uuid) -> Result<bool, Refusal> {
        Ok(self.0.delete("offboarding-rules", id.to_string(), None).await?)
    }

    pub async fn rules(&self) -> Result<Vec<Rule>, Refusal> {
        Ok(self.0.query_all(Query::new("onboarding-rules").order(Order::asc("name"))).await?)
    }

    pub async fn rule(&self, id: Uuid) -> Result<Option<Rule>, Refusal> {
        Ok(self.0.get("onboarding-rules", id.to_string()).await?)
    }

    /// `None` when another rule already has the name.
    pub async fn create_rule(
        &self,
        name: &str,
        description: &str,
        conditions: &Conditions,
        grants: &RuleGrants,
        enabled: bool,
    ) -> Result<Option<Rule>, Refusal> {
        let values = json!({
            "name": name, "description": description, "conditions": conditions,
            "grants": grants, "enabled": enabled,
        });
        match self.0.insert("onboarding-rules", values).await {
            Ok(rule) => Ok(Some(rule)),
            Err(err) if exists(&err) => Ok(None),
            Err(err) => Err(err.into()),
        }
    }

    pub async fn update_rule(&self, rule: &Rule) -> Result<Option<Rule>, Refusal> {
        let set = json!({
            "name": rule.name, "description": rule.description, "conditions": rule.conditions,
            "grants": rule.grants, "enabled": rule.enabled,
        });
        Ok(self.0.update("onboarding-rules", rule.id.to_string(), set, None).await?)
    }

    pub async fn delete_rule(&self, id: Uuid) -> Result<bool, Refusal> {
        Ok(self.0.delete("onboarding-rules", id.to_string(), None).await?)
    }

    pub async fn record_sign_in(&self, sign_in: &SignIn) -> Result<(), Refusal> {
        let values = json!({
            "user_id": sign_in.user, "provider": sign_in.provider,
            "organisations": sign_in.organisations, "teams": sign_in.teams,
            "at": chrono::Utc::now().to_rfc3339(),
        });
        let _: (Value, bool) = self.0.upsert("sign-ins", &["user_id"], values).await?;
        Ok(())
    }

    /// The user's last sign-in, or one with no provider for a user who has not signed in since.
    pub async fn sign_in(&self, user: Uuid) -> Result<Option<SignIn>, Refusal> {
        let Some(found) = self.0.get::<Map<String, Value>>("core.users", user.to_string()).await?
        else {
            return Ok(None);
        };
        let last = self.0.get::<Map<String, Value>>("sign-ins", user.to_string()).await?;
        let list = |field: &str| {
            last.as_ref().and_then(|last| last.get(field).cloned()).unwrap_or(json!([]))
        };
        Ok(Some(read(Map::from_iter([
            ("user".to_string(), json!(user)),
            ("login".to_string(), json!(text(&found, "login"))),
            (
                "provider".to_string(),
                json!(last.as_ref().map(|last| text(last, "provider")).unwrap_or_default()),
            ),
            ("organisations".to_string(), list("organisations")),
            ("teams".to_string(), list("teams")),
        ]))?))
    }
}
