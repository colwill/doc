//! Permission parsing, validation and access checks, following MVP.md §6. Core checks `user` and
//! `service` permissions itself and passes `pluginuser` and `pluginservice` through to the plugin,
//! which checks its own. Every rule here is about shape; whether a plugin or a custom permission
//! actually exists is the backend's question, not this crate's.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use thiserror::Error;

/// The platform's own permissions live under this ID, so `plugin:core:user:rw` is a platform admin.
pub const CORE: &str = "core";

/// Every plugin at once, which only `plugin:*:user:ro` names (MVP.md §6, T67): read access to
/// every plugin's pages, including plugins registered later. It never reaches the platform's own
/// permissions and never a plugin's custom ones, so it can open pages and nothing else.
pub const ANY: &str = "*";

const MAX_NAME: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum Error {
    #[error("a permission starts with `plugin:`, not `{0}`")]
    NotAPermission(String),
    #[error("`{0}` is not a valid name")]
    Name(String),
    #[error("wildcards are not permissions: `{0}`")]
    Wildcard(String),
    #[error("`plugin:*:…` is only ever `plugin:*:user:ro`, not `{0}`")]
    AnyIsReadOnly(String),
    #[error("`{0}` is not one of user, service, settings, group, pluginuser, pluginservice")]
    Kind(String),
    #[error(
        "the platform's settings belong to its administrators, so `core` has no settings permission"
    )]
    CoreHasNoSettings,
    #[error("`{0}` is not a scope")]
    Scope(String),
    #[error("a group permission needs a name")]
    UnnamedGroup,
    #[error("a group permission takes no scope")]
    ScopedGroup,
    #[error("`{0}` has segments after the scope")]
    TooLong(String),
    #[error("`{0}` is missing segments")]
    TooShort(String),
    #[error("the platform has no custom permissions")]
    CoreHasNoCustom,
    #[error("groups do not nest, so `{0}` cannot be in a group")]
    NestedGroup(String),
    #[error("a {kind} group cannot hold `{permission}`")]
    WrongMemberKind { kind: MemberKind, permission: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Read,
    Write,
}

impl Access {
    /// Anything that is not a read is treated as a write, so an unknown method fails closed.
    pub fn of(method: &str) -> Self {
        match method {
            "GET" | "HEAD" => Self::Read,
            _ => Self::Write,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    Ro,
    Rw,
    Wo,
}

impl Scope {
    pub fn allows(self, access: Access) -> bool {
        matches!(
            (self, access),
            (Self::Rw, _) | (Self::Ro, Access::Read) | (Self::Wo, Access::Write)
        )
    }

    /// Rule 1: several grants combine to whatever they allow between them, so `ro` and `wo` make `rw`.
    pub fn union(self, other: Self) -> Self {
        if self == other { self } else { Self::Rw }
    }

    /// Whether holding this scope is enough to grant that one, which is what rule 6 turns on.
    pub fn covers(self, other: Self) -> bool {
        [Access::Read, Access::Write]
            .into_iter()
            .all(|access| !other.allows(access) || self.allows(access))
    }
}

impl fmt::Display for Scope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Ro => "ro",
            Self::Rw => "rw",
            Self::Wo => "wo",
        })
    }
}

impl FromStr for Scope {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "ro" => Ok(Self::Ro),
            "rw" => Ok(Self::Rw),
            "wo" => Ok(Self::Wo),
            other => Err(Error::Scope(other.to_string())),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberKind {
    User,
    Service,
}

impl fmt::Display for MemberKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::User => "user",
            Self::Service => "service",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    User {
        scope: Scope,
    },
    Service {
        scope: Scope,
    },
    Group {
        name: String,
    },
    PluginUser {
        name: String,
        scope: Scope,
    },
    PluginService {
        name: String,
        scope: Scope,
    },
    /// Configuring the plugin: its Settings and Features tabs (ADR-0007). Held by users and
    /// service accounts alike, since a deployment may set a plugin up with an account of its own,
    /// and deliberately apart from `user`, because using a plugin and configuring it differ.
    Settings {
        scope: Scope,
    },
}

impl Subject {
    /// A group permission means membership rather than access, so it has no member kind of its
    /// own; neither has a settings permission, which is held by either kind.
    pub fn member_kind(&self) -> Option<MemberKind> {
        match self {
            Self::User { .. } | Self::PluginUser { .. } => Some(MemberKind::User),
            Self::Service { .. } | Self::PluginService { .. } => Some(MemberKind::Service),
            Self::Group { .. } | Self::Settings { .. } => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Permission {
    pub plugin: String,
    pub subject: Subject,
}

impl Permission {
    pub fn user(plugin: &str, scope: Scope) -> Result<Self, Error> {
        Ok(Self { plugin: name(plugin)?, subject: Subject::User { scope } })
    }

    pub fn service(plugin: &str, scope: Scope) -> Result<Self, Error> {
        Ok(Self { plugin: name(plugin)?, subject: Subject::Service { scope } })
    }

    pub fn group(plugin: &str, group: &str) -> Result<Self, Error> {
        Ok(Self { plugin: name(plugin)?, subject: Subject::Group { name: name(group)? } })
    }

    /// Configuring a plugin: `plugin:<plugin>:settings[:<scope>]` (ADR-0007).
    pub fn settings(plugin: &str, scope: Scope) -> Result<Self, Error> {
        Ok(Self { plugin: name(plugin)?, subject: Subject::Settings { scope } })
    }

    /// Read access to every plugin's pages: `plugin:*:user:ro`.
    pub fn any_user_read() -> Self {
        Self { plugin: ANY.to_string(), subject: Subject::User { scope: Scope::Ro } }
    }

    pub fn is_platform(&self) -> bool {
        self.plugin == CORE
    }

    /// Whether it speaks for every plugin rather than one.
    pub fn is_any(&self) -> bool {
        self.plugin == ANY
    }

    /// Whether it gives `user:ro` on this plugin by naming every plugin. The platform's own
    /// permissions are never included: `core` is opened by holding `plugin:core:…` itself.
    fn any_covers(&self, kind: MemberKind, plugin: &str) -> bool {
        self.is_any()
            && kind == MemberKind::User
            && plugin != CORE
            && matches!(self.subject, Subject::User { .. })
    }
}

fn name(value: &str) -> Result<String, Error> {
    if value.contains('*') || value.contains('>') {
        return Err(Error::Wildcard(value.to_string()));
    }
    let mut chars = value.chars();
    let valid = value.len() <= MAX_NAME
        && chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    valid.then(|| value.to_string()).ok_or_else(|| Error::Name(value.to_string()))
}

fn scope_of(segment: Option<&str>) -> Result<Scope, Error> {
    match segment {
        None => Ok(Scope::Ro),
        Some(value) => value.parse(),
    }
}

impl FromStr for Permission {
    type Err = Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        // The one permission that names every plugin, and it reads only.
        if value == "plugin:*:user:ro" {
            return Ok(Self::any_user_read());
        }
        if value.starts_with("plugin:*:") {
            return Err(Error::AnyIsReadOnly(value.to_string()));
        }
        if value.contains('*') || value.contains('>') {
            return Err(Error::Wildcard(value.to_string()));
        }
        let mut parts = value.split(':');
        match parts.next() {
            Some("plugin") => {}
            other => return Err(Error::NotAPermission(other.unwrap_or_default().to_string())),
        }
        let plugin = name(parts.next().ok_or_else(|| Error::TooShort(value.to_string()))?)?;
        let kind = parts.next().ok_or_else(|| Error::TooShort(value.to_string()))?;
        let custom = |parts: &mut std::str::Split<'_, char>| -> Result<(String, Scope), Error> {
            if plugin == CORE {
                return Err(Error::CoreHasNoCustom);
            }
            let named = name(parts.next().ok_or_else(|| Error::TooShort(value.to_string()))?)?;
            Ok((named, scope_of(parts.next())?))
        };
        let subject = match kind {
            "user" => Subject::User { scope: scope_of(parts.next())? },
            "service" => Subject::Service { scope: scope_of(parts.next())? },
            // The platform's own settings are an administrator's, so `core` has no settings
            // permission of its own: `plugin:core:user:rw` is what opens Admin → Settings.
            "settings" if plugin == CORE => return Err(Error::CoreHasNoSettings),
            "settings" => Subject::Settings { scope: scope_of(parts.next())? },
            "group" => {
                let named = parts.next().filter(|s| !s.is_empty()).ok_or(Error::UnnamedGroup)?;
                if parts.next().is_some() {
                    return Err(Error::ScopedGroup);
                }
                Subject::Group { name: name(named)? }
            }
            "pluginuser" => {
                let (name, scope) = custom(&mut parts)?;
                Subject::PluginUser { name, scope }
            }
            "pluginservice" => {
                let (name, scope) = custom(&mut parts)?;
                Subject::PluginService { name, scope }
            }
            other => return Err(Error::Kind(other.to_string())),
        };
        if parts.next().is_some() {
            return Err(Error::TooLong(value.to_string()));
        }
        Ok(Self { plugin, subject })
    }
}

/// The scope is always written out, so an audit entry or a `403` never leaves it to be inferred.
impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "plugin:{}:", self.plugin)?;
        match &self.subject {
            Subject::User { scope } => write!(f, "user:{scope}"),
            Subject::Service { scope } => write!(f, "service:{scope}"),
            Subject::Settings { scope } => write!(f, "settings:{scope}"),
            Subject::Group { name } => write!(f, "group:{name}"),
            Subject::PluginUser { name, scope } => write!(f, "pluginuser:{name}:{scope}"),
            Subject::PluginService { name, scope } => write!(f, "pluginservice:{name}:{scope}"),
        }
    }
}

impl Serialize for Permission {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Permission {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// A group is a role: a named set of permissions of one member kind, which may reach across
/// plugins. Groups are defined by the RBAC plugin but validated here, because enforcement stays
/// in core.
///
/// `plugin` is the one it is **listed under**, which gives it its name — `kb/editors` — and is
/// where its membership permission lives, `plugin:kb:group:editors`. What it *grants* is not
/// confined to that plugin: a group listed under `kb` may hold `plugin:water:user:rw` too, and
/// membership of it is read wherever its permissions point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "GroupWire")]
pub struct Group {
    pub plugin: String,
    pub name: String,
    pub kind: MemberKind,
    permissions: Vec<Permission>,
}

#[derive(Deserialize)]
struct GroupWire {
    plugin: String,
    name: String,
    kind: MemberKind,
    #[serde(default)]
    permissions: Vec<Permission>,
}

impl TryFrom<GroupWire> for Group {
    type Error = Error;

    fn try_from(wire: GroupWire) -> Result<Self, Error> {
        let mut group = Group::new(&wire.plugin, &wire.name, wire.kind)?;
        for permission in wire.permissions {
            group.insert(permission)?;
        }
        Ok(group)
    }
}

impl Group {
    pub fn new(plugin: &str, group: &str, kind: MemberKind) -> Result<Self, Error> {
        Ok(Self { plugin: name(plugin)?, name: name(group)?, kind, permissions: Vec::new() })
    }

    pub fn insert(&mut self, permission: Permission) -> Result<(), Error> {
        // Configuring a plugin is held by either kind, so a role of either may hold it.
        if matches!(permission.subject, Subject::Settings { .. }) {
            self.permissions.push(permission);
            return Ok(());
        }
        match permission.subject.member_kind() {
            None => return Err(Error::NestedGroup(permission.to_string())),
            Some(kind) if kind != self.kind => {
                return Err(Error::WrongMemberKind {
                    kind: self.kind,
                    permission: permission.to_string(),
                });
            }
            Some(_) => {}
        }
        self.permissions.push(permission);
        Ok(())
    }

    pub fn with(mut self, permission: Permission) -> Result<Self, Error> {
        self.insert(permission)?;
        Ok(self)
    }

    /// Everything the group grants, across every plugin it reaches. An empty group still grants
    /// the read-only default of the plugin it is listed under, which is what gives every plugin a
    /// default role for groups, users and services.
    pub fn permissions(&self) -> Vec<Permission> {
        if !self.permissions.is_empty() {
            return self.permissions.clone();
        }
        let subject = match self.kind {
            MemberKind::User => Subject::User { scope: Scope::Ro },
            MemberKind::Service => Subject::Service { scope: Scope::Ro },
        };
        vec![Permission { plugin: self.plugin.clone(), subject }]
    }

    /// What it grants on one plugin, which is all that resolving access to that plugin may use.
    pub fn permissions_for(&self, plugin: &str) -> Vec<Permission> {
        self.permissions().into_iter().filter(|held| held.plugin == plugin).collect()
    }

    /// Every plugin it grants on, in the order its permissions were written, the one it is listed
    /// under first. A group with no permissions reaches the plugin it is listed under alone.
    pub fn plugins(&self) -> Vec<String> {
        let mut plugins = vec![self.plugin.clone()];
        for permission in self.permissions() {
            if !plugins.contains(&permission.plugin) {
                plugins.push(permission.plugin.clone());
            }
        }
        plugins
    }
}

/// What a principal holds: its own permissions, the groups those memberships point at, and the
/// attributes plugins are given alongside their custom permissions.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Grants {
    pub permissions: Vec<Permission>,
    pub groups: Vec<Group>,
    pub attributes: BTreeMap<String, String>,
}

impl Grants {
    pub fn new(permissions: Vec<Permission>) -> Self {
        Self { permissions, ..Self::default() }
    }

    pub fn with_group(mut self, group: Group) -> Self {
        self.groups.push(group);
        self
    }

    pub fn with_attribute(mut self, key: &str, value: &str) -> Self {
        self.attributes.insert(key.to_string(), value.to_string());
        self
    }

    /// Rule 1: direct permissions plus those of every group the principal belongs to in this
    /// plugin, with the scopes combined. A membership whose group is unknown grants nothing.
    pub fn effective(&self, kind: MemberKind, plugin: &str) -> Effective {
        let mut effective = Effective::default();
        // `plugin:*:user:ro` opens every plugin's pages, so it counts here as `user:ro` on this
        // one. It is a floor, not a ceiling: anything held on the plugin itself adds to it.
        if self.permissions.iter().any(|p| p.any_covers(kind, plugin)) {
            effective.add(
                kind,
                &Permission::user(plugin, Scope::Ro).unwrap_or_else(|_| Permission {
                    plugin: plugin.to_string(),
                    subject: Subject::User { scope: Scope::Ro },
                }),
            );
        }
        // Membership is read whatever plugin it is listed under, since a group may grant on
        // plugins other than that one; only what it grants *here* is added.
        for permission in &self.permissions {
            match &permission.subject {
                Subject::Group { name } => {
                    let found = self.groups.iter().find(|g| {
                        g.plugin == permission.plugin && g.name == *name && g.kind == kind
                    });
                    for granted in found.into_iter().flat_map(|group| group.permissions_for(plugin))
                    {
                        effective.add(kind, &granted);
                    }
                }
                _ if permission.plugin == plugin => effective.add(kind, permission),
                _ => {}
            }
        }
        effective.attributes.clone_from(&self.attributes);
        effective
    }

    /// Rule 4: a user holding `plugin:core:user:rw` is a platform admin, and every check passes.
    pub fn is_admin(&self) -> bool {
        self.effective(MemberKind::User, CORE).scope == Some(Scope::Rw)
    }

    /// Rule 6: an owner may grant a service account only what its own access already covers, and
    /// granting group membership means covering everything that group holds.
    pub fn may_grant(&self, kind: MemberKind, permission: &Permission) -> bool {
        match &permission.subject {
            // A group may reach several plugins, so each of its permissions is weighed against
            // what the granter holds on that permission's own plugin.
            Subject::Group { name } => self
                .groups
                .iter()
                .find(|g| g.plugin == permission.plugin && g.name == *name)
                .is_some_and(|group| {
                    group
                        .permissions()
                        .iter()
                        .all(|granted| self.effective(kind, &granted.plugin).covers(granted))
                }),
            _ => self.effective(kind, &permission.plugin).covers(permission),
        }
    }
}

/// A principal's resolved access to one plugin.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Effective {
    /// What core checks, from `user` or `service` permissions. `None` denies everything.
    pub scope: Option<Scope>,
    /// The plugin's own permissions, which core passes on without checking.
    pub custom: BTreeMap<String, Scope>,
    pub attributes: BTreeMap<String, String>,
    /// Configuring the plugin, from `settings` permissions (ADR-0007). Apart from `scope`,
    /// because writing to a plugin is not leave to reconfigure it.
    pub settings: Option<Scope>,
}

impl Effective {
    fn add(&mut self, kind: MemberKind, permission: &Permission) {
        // Settings are held by either kind, so this is checked before the kind-matched arms.
        if let Subject::Settings { scope } = &permission.subject {
            self.settings = Some(self.settings.map_or(*scope, |held| held.union(*scope)));
            return;
        }
        match (&permission.subject, kind) {
            (Subject::User { scope }, MemberKind::User)
            | (Subject::Service { scope }, MemberKind::Service) => {
                self.scope = Some(self.scope.map_or(*scope, |held| held.union(*scope)));
            }
            (Subject::PluginUser { name, scope }, MemberKind::User)
            | (Subject::PluginService { name, scope }, MemberKind::Service) => {
                self.custom
                    .entry(name.clone())
                    .and_modify(|held| *held = held.union(*scope))
                    .or_insert(*scope);
            }
            _ => {}
        }
    }

    pub fn allows(&self, access: Access) -> bool {
        self.scope.is_some_and(|scope| scope.allows(access))
    }

    /// Whether the holder may see the plugin's settings (`Read`) or change them (`Write`).
    pub fn allows_settings(&self, access: Access) -> bool {
        self.settings.is_some_and(|scope| scope.allows(access))
    }

    /// Plugins call this for their own permissions; core never checks them itself.
    pub fn allows_custom(&self, name: &str, access: Access) -> bool {
        self.custom.get(name).is_some_and(|scope| scope.allows(access))
    }

    fn covers(&self, permission: &Permission) -> bool {
        match &permission.subject {
            Subject::Service { scope } | Subject::User { scope } => {
                self.scope.is_some_and(|held| held.covers(*scope))
            }
            Subject::PluginService { name, scope } | Subject::PluginUser { name, scope } => {
                self.custom.get(name).is_some_and(|held| held.covers(*scope))
            }
            Subject::Settings { scope } => self.settings.is_some_and(|held| held.covers(*scope)),
            Subject::Group { .. } => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn permission(text: &str) -> Permission {
        text.parse().unwrap_or_else(|err| panic!("{text} is not a permission: {err:?}"))
    }

    /// What someone holds, as core works it out for them.
    fn holding(permissions: &[&str]) -> Grants {
        Grants::new(permissions.iter().map(|text| permission(text)).collect())
    }

    /// Asked as `rbac` asks it when an owner grants their service account something (rule 6):
    /// the granter is a person, weighed as one.
    fn may_grant(held: &Grants, text: &str) -> bool {
        held.may_grant(MemberKind::User, &permission(text))
    }

    fn group(plugin: &str, name: &str, kind: MemberKind, permissions: &[&str]) -> Group {
        permissions.iter().fold(Group::new(plugin, name, kind).expect("a group"), |group, text| {
            group.with(permission(text)).expect("a permission the group may hold")
        })
    }

    /// A scope grants what it allows and nothing more, on its own plugin and no other.
    #[test]
    fn a_granter_gives_no_more_than_their_own_scope_on_that_plugin() {
        let writer = holding(&["plugin:kb:user:rw"]);
        for scope in ["ro", "rw", "wo"] {
            assert!(may_grant(&writer, &format!("plugin:kb:service:{scope}")), "rw covers {scope}");
        }
        let reader = holding(&["plugin:kb:user:ro"]);
        assert!(may_grant(&reader, "plugin:kb:service:ro"));
        assert!(!may_grant(&reader, "plugin:kb:service:rw"), "reading is not leave to write");
        assert!(!may_grant(&reader, "plugin:kb:service:wo"));
        assert!(!may_grant(&writer, "plugin:dora:service:ro"), "nothing held on dora");
        assert!(!may_grant(&holding(&[]), "plugin:kb:service:ro"), "nothing held at all");
    }

    /// A plugin's own permissions and its settings are each their own thing to hold: writing to a
    /// plugin is neither leave to give out one of its custom permissions nor to configure it.
    #[test]
    fn custom_permissions_and_settings_need_holding_themselves() {
        let writer = holding(&["plugin:kb:user:rw"]);
        assert!(!may_grant(&writer, "plugin:kb:pluginservice:export:ro"));
        assert!(!may_grant(&writer, "plugin:kb:settings:ro"));

        let exporter = holding(&["plugin:kb:pluginuser:export:rw", "plugin:kb:settings:ro"]);
        assert!(may_grant(&exporter, "plugin:kb:pluginservice:export:ro"));
        assert!(may_grant(&exporter, "plugin:kb:pluginservice:export:rw"));
        assert!(!may_grant(&exporter, "plugin:kb:pluginservice:import:ro"), "another custom one");
        assert!(may_grant(&exporter, "plugin:kb:settings:ro"));
        assert!(!may_grant(&exporter, "plugin:kb:settings:rw"), "reading settings is not writing");
    }

    /// Access held through membership of a group counts, on every plugin the group reaches, but
    /// only when the granter is a member and the group is known.
    #[test]
    fn access_held_through_a_group_counts_as_held() {
        let editors =
            group("kb", "editors", MemberKind::User, &["plugin:kb:user:rw", "plugin:dora:user:ro"]);
        let member = holding(&["plugin:kb:group:editors"]).with_group(editors.clone());
        assert!(may_grant(&member, "plugin:kb:service:rw"));
        assert!(may_grant(&member, "plugin:dora:service:ro"), "a group reaches other plugins");
        assert!(!may_grant(&member, "plugin:dora:service:rw"));

        let unknown = holding(&["plugin:kb:group:editors"]);
        assert!(
            !may_grant(&unknown, "plugin:kb:service:ro"),
            "a group nobody defined grants nothing"
        );

        let outsider = holding(&[]).with_group(editors);
        assert!(!may_grant(&outsider, "plugin:kb:service:ro"), "knowing a group is not belonging");
    }

    /// Giving a service account a group is giving it everything the group carries, so the granter
    /// must cover all of it, on each plugin it reaches. `rbac` passes the group being granted along
    /// with what the granter holds, so naming it must not count as belonging to it.
    #[test]
    fn a_group_is_granted_only_when_all_it_carries_is_covered() {
        let bots = || {
            group(
                "kb",
                "bots",
                MemberKind::Service,
                &["plugin:kb:service:rw", "plugin:dora:service:ro"],
            )
        };
        let kb_only = holding(&["plugin:kb:user:rw"]).with_group(bots());
        assert!(!may_grant(&kb_only, "plugin:kb:group:bots"), "dora is not covered");

        let both = holding(&["plugin:kb:user:rw", "plugin:dora:user:ro"]).with_group(bots());
        assert!(may_grant(&both, "plugin:kb:group:bots"));

        let reader = holding(&["plugin:kb:user:ro", "plugin:dora:user:ro"]).with_group(bots());
        assert!(!may_grant(&reader, "plugin:kb:group:bots"), "the group writes to kb");

        let undefined = holding(&["plugin:kb:user:rw", "plugin:dora:user:rw"]);
        assert!(
            !may_grant(&undefined, "plugin:kb:group:bots"),
            "a group nobody defined is refused"
        );
    }

    /// Reading every plugin is a floor of `ro` on each, so it lets someone give read access to any
    /// plugin and nothing beyond it — and never to the platform itself.
    #[test]
    fn reading_every_plugin_grants_reading_and_no_more() {
        let everywhere = holding(&["plugin:*:user:ro"]);
        assert!(may_grant(&everywhere, "plugin:kb:service:ro"));
        assert!(may_grant(&everywhere, "plugin:dora:service:ro"));
        assert!(!may_grant(&everywhere, "plugin:kb:service:rw"));
        assert!(!may_grant(&everywhere, "plugin:core:service:ro"), "the platform is not a plugin");
    }
}
