//! Every change the plugin makes, and the views it shows, shared by the JSON API and the pages so
//! both check, store, audit and announce in exactly the same way.

use std::collections::{BTreeMap, BTreeSet};

use doc_permissions::{ANY, CORE, Grants, Group, MemberKind, Permission, Subject};
use doc_plugin_sdk::Backend;
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::checks;
use crate::model::{Conditions, GroupRecord, Holder, Refusal, Rule, RuleGrants, parse};
use crate::onboarding;
use crate::store::{Known, Store};

const CHANGED: &str = "plugin.rbac.changed";
const MAX_DESCRIPTION: usize = 500;
const MAX_NAME: usize = 64;
const MAX_VALUE: usize = 256;
const SCOPES: [&str; 3] = ["ro", "rw", "wo"];

pub fn actor(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "platform".into())
}

/// Every change is audited and announced, so core drops the decisions it has cached.
pub async fn record(backend: &Backend, action: &str, subject: &str, detail: Value) {
    if let Err(err) = backend.audit(action, Some(subject), detail).await {
        tracing::warn!(%err, action, "a change was made but could not be audited");
    }
    let change = json!({ "change": action, "subject": subject });
    if let Err(err) = backend.publish(CHANGED, change).await {
        tracing::warn!(%err, action, "core was not told of a change and will notice within a minute");
    }
}

/// The custom permission that makes `plugin:rbac:user:rw` mean every group, rule and attribute
/// rather than only the groups its holder belongs to.
pub const ADMIN: &str = "admin";

/// Whether the caller reaches everything here: a platform admin, or somebody holding
/// `plugin:rbac:pluginuser:admin:rw` beside their `plugin:rbac:user:rw`. Core has already let the
/// request through on that base scope, so anyone else who gets this far is a *scoped* manager.
pub fn full_admin(backend: &Backend) -> bool {
    backend.allows(ADMIN, true)
}

/// Refuses a scoped manager what only a full admin may do: granting anything to anyone directly,
/// rules, attributes. They manage the groups they belong to, and nothing wider.
pub fn require_admin(backend: &Backend) -> Result<(), Refusal> {
    match full_admin(backend) {
        true => Ok(()),
        false => Err(Refusal::forbidden(
            "you manage the groups you belong to; this needs plugin:rbac:pluginuser:admin:rw",
        )),
    }
}

/// The caller as somebody who can belong to a group, when they are a person.
fn acting(backend: &Backend) -> Option<Holder> {
    let caller = backend.caller()?;
    let id: Uuid = caller.id.as_deref()?.parse().ok()?;
    match caller.kind.as_str() {
        "user" => Some(Holder::User { id }),
        _ => None,
    }
}

/// What a caller may do to `group`, or to a new one when none is given. A full admin is bound by
/// nothing, so this answers `None`. A scoped manager may change only a group they already belong
/// to, and what they put in it is capped at their own access, as self-service caps what an owner
/// gives their service account (rule 6): this answers what they hold, to measure that against.
async fn scope(
    backend: &Backend,
    store: &Store<'_>,
    group: Option<&GroupRecord>,
) -> Result<Option<Grants>, Refusal> {
    if full_admin(backend) {
        return Ok(None);
    }
    let holder = acting(backend)
        .ok_or_else(|| Refusal::forbidden("only a person manages the groups they belong to"))?;
    let held = store.grants(&holder).await?;
    if let Some(group) = group {
        let member = held.groups.iter().any(|g| g.plugin == group.plugin && g.name == group.name);
        if !member {
            return Err(Refusal::forbidden(format!(
                "you are not in {}/{}, so you cannot change it; that needs \
                 plugin:rbac:pluginuser:admin:rw",
                group.plugin, group.name
            )));
        }
    }
    Ok(Some(held))
}

/// Refuses a permission beyond what a scoped manager holds themselves.
fn within(held: &Option<Grants>, permission: &Permission) -> Result<(), Refusal> {
    match held {
        Some(held) if !held.may_grant(MemberKind::User, permission) => Err(Refusal::forbidden(
            format!("{permission} goes beyond your own access to {}", permission.plugin),
        )),
        _ => Ok(()),
    }
}

/// Core merged a user who never signed in into another (ADR-0005), so what they were given moves.
pub const MERGED: &str = "platform.iam.user.merged";

#[derive(Deserialize)]
struct Merged {
    from: Uuid,
    into: Uuid,
    #[serde(default)]
    login: String,
}

pub async fn merged(backend: &Backend, payload: Value) -> Result<(), doc_plugin_sdk::PluginError> {
    let merged: Merged = serde_json::from_value(payload).map_err(|err| {
        doc_plugin_sdk::PluginError::Message(format!("a merge could not be read: {err}"))
    })?;
    let (permissions, attributes) = Store(backend).merge_user(merged.from, merged.into).await?;
    if permissions.is_empty() && attributes.is_empty() {
        return Ok(());
    }
    let detail = json!({
        "from": merged.from, "login": merged.login,
        "permissions": permissions, "attributes": attributes,
    });
    record(backend, "user.merged", &merged.into.to_string(), detail).await;
    Ok(())
}

pub fn described(description: &str) -> Result<String, Refusal> {
    let description = description.trim();
    match description.chars().count() <= MAX_DESCRIPTION {
        true => Ok(description.to_string()),
        false => {
            Err(Refusal::bad(format!("a description is at most {MAX_DESCRIPTION} characters")))
        }
    }
}

pub fn attribute_key(key: &str) -> Result<(), Refusal> {
    let mut chars = key.chars();
    let valid = key.len() <= MAX_NAME
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_.".contains(c));
    valid.then_some(()).ok_or_else(|| {
        Refusal::bad(format!(
            "`{key}` is not an attribute name: a-z, then a-z, 0-9, `-`, `_` or `.`"
        ))
    })
}

/// What a picker may offer a holder in one plugin: its own kind's scopes and declared custom
/// permissions, and group memberships of its kind when it is a principal rather than a group.
pub fn choices(
    known: &Known,
    groups: &[GroupRecord],
    plugin: &str,
    kind: MemberKind,
    memberships: bool,
) -> Vec<String> {
    let (base, custom) = match kind {
        MemberKind::User => ("user", "pluginuser"),
        MemberKind::Service => ("service", "pluginservice"),
    };
    // Every plugin at once is only ever read access for people, so it is the one choice there.
    if plugin == ANY {
        return match kind {
            MemberKind::User => vec![Permission::any_user_read().to_string()],
            MemberKind::Service => Vec::new(),
        };
    }
    let mut choices: Vec<String> =
        SCOPES.iter().map(|scope| format!("plugin:{plugin}:{base}:{scope}")).collect();
    for name in known.customs(plugin, custom) {
        choices
            .extend(SCOPES.iter().map(|scope| format!("plugin:{plugin}:{custom}:{name}:{scope}")));
    }
    if memberships {
        let own = groups.iter().filter(|group| group.plugin == plugin && group.kind == kind);
        choices.extend(own.map(GroupRecord::membership));
    }
    choices
}

/// A group's own name, checked as core checks it, so a form can say so before anything is stored.
pub fn group_name(name: &str) -> Result<String, Refusal> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Refusal::bad("give the group a name, such as deployers"));
    }
    match Group::new(CORE, name, MemberKind::User) {
        Ok(_) => Ok(name.to_string()),
        Err(_) => Err(Refusal::bad(format!(
            "`{name}` is not a name for a group: start with a lower-case letter, then letters, numbers or `-`, such as deployers"
        ))),
    }
}

pub async fn found_group(
    store: &Store<'_>,
    plugin: &str,
    name: &str,
) -> Result<GroupRecord, Refusal> {
    store
        .group(plugin, name)
        .await?
        .ok_or_else(|| Refusal::missing(format!("no group {plugin}/{name}")))
}

/// A group created without permissions is given its plugin's read-only default, stored as such.
pub async fn create_group(
    backend: &Backend,
    store: &Store<'_>,
    plugin: &str,
    name: &str,
    kind: MemberKind,
    description: &str,
    permissions: &[String],
) -> Result<GroupRecord, Refusal> {
    let held = scope(backend, store, None).await?;
    if held.is_some() && kind != MemberKind::User {
        return Err(Refusal::forbidden(
            "you manage groups of people; a group of service accounts needs \
             plugin:rbac:pluginuser:admin:rw",
        ));
    }
    let known = store.known().await?;
    known.exists(plugin)?;
    let mut group = Group::new(plugin, name.trim(), kind)?;
    for text in permissions {
        let permission = parse(text)?;
        known.check(&permission)?;
        within(&held, &permission)?;
        group.insert(permission)?;
    }
    let description = described(description)?;
    if !store.create_group(&group, &description).await? {
        return Err(Refusal::conflict(format!("{}/{} already exists", group.plugin, group.name)));
    }
    // Whoever makes a group without full reach is its first member, so they can go on managing it.
    if held.is_some()
        && let Some(holder) = acting(backend)
    {
        let membership = Permission::group(&group.plugin, &group.name)?;
        store.grant(&holder, &membership, "self-service", &actor(backend)).await?;
    }
    let granted: Vec<String> = group.permissions().iter().map(ToString::to_string).collect();
    let detail = json!({ "kind": group.kind, "permissions": granted, "by": actor(backend) });
    record(backend, "group.created", &format!("{}/{}", group.plugin, group.name), detail).await;
    found_group(store, &group.plugin, &group.name).await
}

pub async fn update_group(
    backend: &Backend,
    store: &Store<'_>,
    plugin: &str,
    name: &str,
    description: Option<&str>,
    permissions: Option<&[String]>,
) -> Result<GroupRecord, Refusal> {
    let existing = found_group(store, plugin, name).await?;
    let held = scope(backend, store, Some(&existing)).await?;
    let known = store.known().await?;
    let mut group = Group::new(plugin, name, existing.kind)?;
    for text in permissions.into_iter().flatten() {
        let permission = parse(text)?;
        known.check(&permission)?;
        within(&held, &permission)?;
        group.insert(permission)?;
    }
    let description = description.map(described).transpose()?;
    store.update_group(&group, description.as_deref(), permissions.is_some()).await?;
    let updated = found_group(store, plugin, name).await?;
    let detail = json!({ "permissions": updated.permissions, "by": actor(backend) });
    record(backend, "group.updated", &format!("{plugin}/{name}"), detail).await;
    Ok(updated)
}

pub async fn delete_group(
    backend: &Backend,
    store: &Store<'_>,
    plugin: &str,
    name: &str,
) -> Result<(), Refusal> {
    let group = found_group(store, plugin, name).await?;
    scope(backend, store, Some(&group)).await?;
    if !store.delete_group(&group).await? {
        return Err(Refusal::missing(format!("no group {plugin}/{name}")));
    }
    let detail = json!({ "members": group.members, "by": actor(backend) });
    record(backend, "group.deleted", &format!("{plugin}/{name}"), detail).await;
    Ok(())
}

/// A scoped manager gives or takes only membership of a group they are in; every other assignment,
/// which reaches past their groups, is for a full admin.
async fn allow_assignment(
    backend: &Backend,
    store: &Store<'_>,
    permission: &Permission,
) -> Result<(), Refusal> {
    if full_admin(backend) {
        return Ok(());
    }
    match &permission.subject {
        Subject::Group { name } => {
            let group = found_group(store, &permission.plugin, name).await?;
            scope(backend, store, Some(&group)).await.map(drop)
        }
        _ => require_admin(backend),
    }
}

/// True when the holder did not hold it already.
pub async fn grant(
    backend: &Backend,
    store: &Store<'_>,
    holder: &Holder,
    text: &str,
) -> Result<(Permission, bool), Refusal> {
    let known = store.known().await?;
    checks::holder(store, holder).await?;
    let permission = checks::permission(store, &known, holder, text).await?;
    allow_assignment(backend, store, &permission).await?;
    let added = store.grant(holder, &permission, "api", &actor(backend)).await?;
    if added {
        let detail =
            json!({ "holder": holder, "permission": permission.to_string(), "by": actor(backend) });
        record(backend, "assignment.granted", &holder.key(), detail).await;
    }
    Ok((permission, added))
}

pub async fn revoke(
    backend: &Backend,
    store: &Store<'_>,
    holder: &Holder,
    text: &str,
) -> Result<Permission, Refusal> {
    let permission = parse(text)?;
    allow_assignment(backend, store, &permission).await?;
    if !store.revoke(holder, &permission).await? {
        return Err(Refusal::missing(format!("{} does not hold {permission}", holder.key())));
    }
    let detail =
        json!({ "holder": holder, "permission": permission.to_string(), "by": actor(backend) });
    record(backend, "assignment.revoked", &holder.key(), detail).await;
    Ok(permission)
}

pub async fn set_attribute(
    backend: &Backend,
    store: &Store<'_>,
    holder: &Holder,
    key: &str,
    value: &str,
) -> Result<bool, Refusal> {
    require_admin(backend)?;
    checks::holder(store, holder).await?;
    attribute_key(key)?;
    let value = value.trim();
    if value.is_empty() || value.chars().count() > MAX_VALUE {
        return Err(Refusal::bad(format!("an attribute's value is 1 to {MAX_VALUE} characters")));
    }
    let changed = store.set_attribute(holder, key, value, true).await?;
    if changed {
        let detail = json!({ "holder": holder, "key": key, "value": value, "by": actor(backend) });
        record(backend, "attribute.set", &holder.key(), detail).await;
    }
    Ok(changed)
}

pub async fn remove_attribute(
    backend: &Backend,
    store: &Store<'_>,
    holder: &Holder,
    key: &str,
) -> Result<(), Refusal> {
    require_admin(backend)?;
    if !store.remove_attribute(holder, key).await? {
        return Err(Refusal::missing(format!("{} has no attribute {key}", holder.key())));
    }
    let detail = json!({ "holder": holder, "key": key, "by": actor(backend) });
    record(backend, "attribute.removed", &holder.key(), detail).await;
    Ok(())
}

pub fn holder_of(kind: &str, id: &str) -> Result<Holder, Refusal> {
    let id: Uuid = id.parse().map_err(|_| Refusal::bad(format!("`{id}` is not an ID")))?;
    match kind {
        "user" => Ok(Holder::User { id }),
        "service" => Ok(Holder::Service { id }),
        // A team holds permissions of its own, which everybody in it and in the teams inside it
        // holds through it (T67).
        "team" => Ok(Holder::Team { id }),
        _ => Err(Refusal::missing("a principal is a user, a service account or a team")),
    }
}

/// What a user or service account holds directly, through groups, and what that allows per plugin.
pub async fn principal(store: &Store<'_>, holder: &Holder) -> Result<Value, Refusal> {
    let Some(kind) = holder.member_kind() else {
        return Err(Refusal::bad("a group is shown under groups/<plugin>/<name>"));
    };
    let label = checks::holder(store, holder).await?;
    let grants = store.grants(holder).await?;
    // Where their access reaches: the plugins their own permissions name, and the ones every
    // group they are in grants on, which need not be the plugin that group is listed under.
    let mut plugins: BTreeSet<String> =
        grants.permissions.iter().map(|held| held.plugin.clone()).collect();
    for group in &grants.groups {
        plugins.extend(group.permissions().into_iter().map(|held| held.plugin));
    }
    let access: BTreeMap<String, Value> = plugins
        .into_iter()
        .map(|plugin| {
            let effective = grants.effective(kind, &plugin);
            (plugin, json!({ "scope": effective.scope, "custom": effective.custom }))
        })
        .collect();
    Ok(json!({
        "holder": holder,
        "label": label,
        "assignments": store.assignments_of(holder).await?,
        "groups": grants.groups,
        "attributes": grants.attributes,
        "access": access,
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleChange {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub conditions: Option<Conditions>,
    #[serde(default)]
    pub grants: Option<RuleGrants>,
    #[serde(default)]
    pub enabled: Option<bool>,
}

fn rule_name(name: &str) -> Result<String, Refusal> {
    let name = name.trim();
    match (1..=MAX_NAME).contains(&name.chars().count()) {
        true => Ok(name.to_string()),
        false => Err(Refusal::bad(format!("a rule's name is 1 to {MAX_NAME} characters"))),
    }
}

/// A rule is not written on a team a provider reported: its teams are kept here now, and a team
/// holds permissions itself, so it is granted on the team's own page (T68).
fn check_conditions(conditions: &Conditions) -> Result<(), Refusal> {
    match conditions.names_a_team() {
        true => Err(Refusal::bad(
            "a rule is not written on a provider's team any more; grant the team itself, on its \
             page under people and teams",
        )),
        false => Ok(()),
    }
}

/// Rules onboard users, so they grant only user permissions and groups of users.
async fn check_rule(store: &Store<'_>, grants: &RuleGrants) -> Result<(), Refusal> {
    let known = store.known().await?;
    for group in &grants.groups {
        let record = found_group(store, &group.plugin, &group.name)
            .await
            .map_err(|refusal| Refusal::bad(refusal.detail))?;
        if record.kind != MemberKind::User {
            return Err(Refusal::bad(format!(
                "{}/{} is a group of service accounts, and rules onboard users",
                group.plugin, group.name
            )));
        }
    }
    for text in &grants.permissions {
        let permission = parse(text)?;
        known.check(&permission)?;
        match permission.subject {
            Subject::User { .. } | Subject::PluginUser { .. } => {}
            Subject::Group { .. } => return Err(Refusal::bad("list groups under `groups`")),
            _ => return Err(Refusal::bad(format!("a rule onboards users, not {permission}"))),
        }
    }
    grants.attributes.keys().try_for_each(|key| attribute_key(key))
}

pub async fn rule(store: &Store<'_>, id: &str) -> Result<Rule, Refusal> {
    let id: Uuid = id.parse().map_err(|_| Refusal::bad(format!("`{id}` is not an ID")))?;
    store.rule(id).await?.ok_or_else(|| Refusal::missing(format!("no rule {id}")))
}

pub async fn create_rule(
    backend: &Backend,
    store: &Store<'_>,
    change: RuleChange,
) -> Result<Rule, Refusal> {
    require_admin(backend)?;
    let name = rule_name(change.name.as_deref().unwrap_or_default())?;
    let description = described(change.description.as_deref().unwrap_or_default())?;
    let conditions = change.conditions.unwrap_or_default();
    let grants = change.grants.unwrap_or_default();
    check_conditions(&conditions)?;
    check_rule(store, &grants).await?;
    let rule = store
        .create_rule(&name, &description, &conditions, &grants, change.enabled.unwrap_or(true))
        .await?
        .ok_or_else(|| Refusal::conflict(format!("a rule called {name} already exists")))?;
    record(
        backend,
        "rule.created",
        &rule.id.to_string(),
        json!({ "rule": rule, "by": actor(backend) }),
    )
    .await;
    Ok(rule)
}

pub async fn update_rule(
    backend: &Backend,
    store: &Store<'_>,
    id: &str,
    change: RuleChange,
) -> Result<Rule, Refusal> {
    require_admin(backend)?;
    let mut rule = rule(store, id).await?;
    if let Some(name) = &change.name {
        rule.name = rule_name(name)?;
    }
    if let Some(description) = &change.description {
        rule.description = described(description)?;
    }
    if let Some(grants) = change.grants {
        check_rule(store, &grants).await?;
        rule.grants = grants;
    }
    if let Some(conditions) = change.conditions {
        check_conditions(&conditions)?;
        rule.conditions = conditions;
    }
    rule.enabled = change.enabled.unwrap_or(rule.enabled);
    if store.rules().await?.iter().any(|other| other.id != rule.id && other.name == rule.name) {
        return Err(Refusal::conflict(format!("a rule called {} already exists", rule.name)));
    }
    let updated =
        store.update_rule(&rule).await?.ok_or_else(|| Refusal::missing(format!("no rule {id}")))?;
    record(
        backend,
        "rule.updated",
        &updated.id.to_string(),
        json!({ "rule": updated, "by": actor(backend) }),
    )
    .await;
    Ok(updated)
}

pub async fn delete_rule(backend: &Backend, store: &Store<'_>, id: &str) -> Result<Rule, Refusal> {
    require_admin(backend)?;
    let rule = rule(store, id).await?;
    if !store.delete_rule(rule.id).await? {
        return Err(Refusal::missing(format!("no rule {id}")));
    }
    record(
        backend,
        "rule.deleted",
        &rule.id.to_string(),
        json!({ "name": rule.name, "by": actor(backend) }),
    )
    .await;
    Ok(rule)
}

/// Whether the rule matches the user's last sign-in and attributes, and what it would add now.
pub async fn test_rule(store: &Store<'_>, rule: &Rule, user: Uuid) -> Result<Value, Refusal> {
    let mut rule = rule.clone();
    let sign_in =
        store.sign_in(user).await?.ok_or_else(|| Refusal::missing(format!("no user {user}")))?;
    let attributes = store.attributes(&Holder::User { id: user }).await?;
    let matches = rule.conditions.matches(&sign_in, &attributes);
    rule.enabled = true;
    let adds = match matches {
        true => json!(
            onboarding::apply(store, &sign_in, std::slice::from_ref(&rule), false, true).await?
        ),
        false => Value::Null,
    };
    Ok(
        json!({ "rule": rule.name, "user": sign_in.login, "sign_in": sign_in, "matches": matches, "would_add": adds }),
    )
}
