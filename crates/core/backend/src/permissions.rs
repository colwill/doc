//! Core's half of the permissions model: where a caller's grants come from, how they are cached,
//! and the guard a route is protected by. The RBAC plugin (T26) answers over the Service Bus; until
//! it does, only the bootstrap permissions in configuration apply and every other check is denied.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::extract::{FromRequestParts, OriginalUri};
use doc_cachebus::Namespace;
use doc_eventbus::{ConsumerGroup, TopicFilter};
use doc_permissions::{Access, CORE, Effective, Grants, MemberKind, Permission, Scope};
use doc_servicebus::{Address, ServiceBus};
use http::request::Parts;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::api::AppState;
use crate::api::problem::Problem;
use crate::auth::Auth;
use crate::config::Config;
use crate::identity::Principal;

const NAMESPACE: &str = "core.permissions";
const CACHE_TTL: Duration = Duration::from_secs(60);
const RBAC: &str = "rbac";
const RBAC_DEADLINE: Duration = Duration::from_secs(2);
const CHANGED: &str = "plugin.rbac.changed";
const PROVIDER_STATE: &str = "platform.plugin.rbac.state";
/// A team changing hands, parents or members changes what its people hold (T67).
const TEAMS: &str = "platform.team.>";
const PROVIDER_ROUTE: &str = "effective-permissions";

/// Where a caller's permissions come from. `None` on the state means the platform has no source at
/// all, which is the same as one that answers nothing: bootstrap permissions only.
#[async_trait]
pub trait PermissionSource: Send + Sync {
    /// `teams` are the teams the principal is in and every team above them, which core works out
    /// from its own records; what those hold, their people hold (T67).
    async fn grants(&self, principal: &Principal, teams: &[Uuid]) -> Result<Grants, String>;
}

pub struct RbacSource {
    services: Arc<dyn ServiceBus>,
}

impl RbacSource {
    pub fn new(services: Arc<dyn ServiceBus>) -> Self {
        Self { services }
    }
}

#[async_trait]
impl PermissionSource for RbacSource {
    async fn grants(&self, principal: &Principal, teams: &[Uuid]) -> Result<Grants, String> {
        let payload = json!({ "principal": principal, "teams": teams });
        let reply = ask_rbac(self.services.as_ref(), PROVIDER_ROUTE, payload).await?;
        serde_json::from_value(reply).map_err(|err| err.to_string())
    }
}

/// One of the RBAC plugin's `internal/*` routes, asked as the platform itself.
pub async fn ask_rbac(
    services: &dyn ServiceBus,
    route: &str,
    payload: Value,
) -> Result<Value, String> {
    let address = Address::plugin(RBAC).map_err(|err| err.to_string())?;
    services.request(&address, route, payload, RBAC_DEADLINE).await.map_err(|err| err.to_string())
}

/// Rule 8: configuration grants these before the RBAC plugin exists, so the platform can be set up.
fn bootstrap(config: &Config, principal: &Principal) -> Grants {
    let admin =
        |login: &str| config.bootstrap.admins.iter().any(|name| name.eq_ignore_ascii_case(login));
    let permissions = match principal {
        Principal::User(user) if admin(&user.login) => {
            vec![Permission::user(CORE, Scope::Rw)]
        }
        Principal::ServiceAccount(account)
            if account.name == config.bootstrap.operator
                || account.name == config.bootstrap.frontend =>
        {
            vec![Permission::service(CORE, Scope::Ro)]
        }
        _ => return Grants::default(),
    };
    Grants::new(permissions.into_iter().flatten().collect())
}

/// The principal a Service Bus reference names, as it is now: a disabled account acts for nobody.
pub async fn principal_of(state: &AppState, reference: &str) -> Result<Principal, String> {
    let identity = &state.repos.identity;
    let (kind, id) =
        reference.split_once(':').ok_or_else(|| format!("`{reference}` names nobody"))?;
    let uuid = || id.parse::<Uuid>().map_err(|_| format!("`{reference}` names nobody"));
    let principal = match kind {
        "user" => {
            identity.user_by_id(uuid()?).await.map_err(|err| err.to_string())?.map(Principal::User)
        }
        "service" => identity
            .service_account_by_id(uuid()?)
            .await
            .map_err(|err| err.to_string())?
            .map(Principal::ServiceAccount),
        "plugin" => Some(Principal::Plugin { id: id.to_string() }),
        _ => None,
    };
    match principal {
        Some(principal) if principal.disabled() => {
            Err(format!("{} is disabled", principal.label()))
        }
        Some(principal) => Ok(principal),
        None => Err(format!("{reference} no longer exists")),
    }
}

/// Rule 4 with the caller's kind checked first: only a user can be a platform admin.
pub fn is_admin(held: &Grants, principal: &Principal) -> bool {
    member_kind(principal) == Some(MemberKind::User) && held.is_admin()
}

/// Whether `principal` is a platform admin, worked out the same way as for whoever is asking, but
/// for somebody else entirely — a target a handler is about to act on, say, rather than the caller.
pub async fn is_admin_of(state: &AppState, principal: &Principal) -> bool {
    is_admin(&grants(state, principal).await, principal)
}

pub fn member_kind(principal: &Principal) -> Option<MemberKind> {
    match principal {
        Principal::User(_) => Some(MemberKind::User),
        Principal::ServiceAccount(_) => Some(MemberKind::Service),
        Principal::Plugin { .. } => None,
    }
}

fn namespace() -> Option<Namespace> {
    Namespace::new(NAMESPACE).ok()
}

/// The teams a person is in and every team above them, and whether any of those is a default
/// team. A service account belongs to no team, and neither does a plugin.
async fn teams_of(state: &AppState, principal: &Principal) -> (Vec<Uuid>, bool) {
    let Principal::User(user) = principal else {
        return (Vec::new(), false);
    };
    let memberships = match state.repos.teams.memberships(user.id).await {
        Ok(memberships) if !memberships.is_empty() => memberships,
        Ok(_) => return (Vec::new(), false),
        Err(err) => {
            tracing::warn!(%err, "a person's teams could not be read, so they hold only their own");
            return (Vec::new(), false);
        }
    };
    let teams = match state.repos.teams.teams().await {
        Ok(teams) => teams,
        Err(err) => {
            tracing::warn!(%err, "the teams could not be read, so they hold only their own");
            return (Vec::new(), false);
        }
    };
    let reach = crate::teams::reach(&teams, &memberships);
    let default = teams.iter().any(|team| team.is_default && reach.contains(&team.id));
    (reach, default)
}

/// What `principal` holds. Through a scoped token that is only what the token's scopes name, at no
/// more than its holder has on each plugin and never as an administrator (FEAT-VACUUM).
pub async fn grants(state: &AppState, principal: &Principal) -> Grants {
    let Some(scopes) = principal.scopes() else { return held(state, principal).await };
    let holder = principal.unscoped();
    let held = held(state, &holder).await;
    narrowed(&held, is_admin(&held, &holder), scopes)
}

/// The scopes a token may name: a plugin's own user permission, never core's, `*`, a group or a
/// custom permission, so a scoped token can do no more than open that plugin's routes.
pub fn scope_of(text: &str) -> Option<Permission> {
    let permission: Permission = text.trim().parse().ok()?;
    let plain = matches!(permission.subject, doc_permissions::Subject::User { .. });
    (plain && !permission.is_platform() && !permission.is_any()).then_some(permission)
}

/// Each scope, at the most its holder has on its plugin: `rw` wanted of someone who only reads
/// gives them `ro`, and a plugin they hold nothing on gives nothing.
pub fn narrowed(held: &Grants, admin: bool, scopes: &[String]) -> Grants {
    let mut permissions = Vec::new();
    for scope in scopes.iter().filter_map(|text| scope_of(text)) {
        let doc_permissions::Subject::User { scope: wanted } = scope.subject else { continue };
        let has = match admin {
            true => Some(Scope::Rw),
            false => held.effective(MemberKind::User, &scope.plugin).scope,
        };
        let granted = match (wanted, has) {
            (_, None) => None,
            (wanted, Some(Scope::Rw)) => Some(wanted),
            (Scope::Rw, Some(has)) => Some(has),
            (wanted, Some(has)) if wanted == has => Some(wanted),
            _ => None,
        };
        if let Some(granted) = granted
            && let Ok(permission) = Permission::user(&scope.plugin, granted)
        {
            permissions.push(permission);
        }
    }
    Grants { permissions, groups: Vec::new(), attributes: held.attributes.clone() }
}

/// Whether a scoped token's scopes name `plugin` at all.
pub fn reaches(scopes: &[String], plugin: &str) -> bool {
    plugin != CORE
        && scopes.iter().filter_map(|text| scope_of(text)).any(|scope| scope.plugin == plugin)
}

/// Bootstrap grants always apply; a source that failed is asked again rather than cached as empty.
async fn held(state: &AppState, principal: &Principal) -> Grants {
    if let Some(cached) = cached(state, principal).await {
        return cached;
    }
    let (teams, in_default) = teams_of(state, principal).await;
    let (mut grants, answered) = match &state.permissions {
        Some(source) => match source.grants(principal, &teams).await {
            Ok(grants) => (grants, true),
            Err(err) => {
                tracing::debug!(%err, principal = %principal.label(), "no permissions from the source");
                (Grants::default(), false)
            }
        },
        None => (Grants::default(), true),
    };
    let extra = bootstrap(&state.config, principal);
    grants.permissions.extend(extra.permissions);
    grants.groups.extend(extra.groups);
    // A default team holds read access to every plugin for as long as it is marked default, so
    // everyone placed in one can open the pages without anyone granting it to them (T67).
    if in_default {
        grants.permissions.push(Permission::any_user_read());
    }
    if answered {
        cache(state, principal, &grants).await;
    }
    grants
}

async fn cached(state: &AppState, principal: &Principal) -> Option<Grants> {
    let entry = state.buses.cache.get(&namespace()?, &principal.reference()).await.ok()??;
    serde_json::from_value(entry.value).ok()
}

async fn cache(state: &AppState, principal: &Principal, grants: &Grants) {
    let Some(namespace) = namespace() else { return };
    let Ok(value) = serde_json::to_value(grants) else { return };
    let _ = state.buses.cache.set(&namespace, &principal.reference(), value, Some(CACHE_TTL)).await;
}

pub async fn forget_all(state: &AppState) {
    let Some(namespace) = namespace() else { return };
    let _ = state.buses.cache.clear(&namespace).await;
}

/// Drops every cached decision when an assignment moves or the RBAC plugin changes state.
pub fn watch(state: &AppState) {
    for (name, topic) in [
        ("core.permissions", CHANGED),
        ("core.permissions.provider", PROVIDER_STATE),
        ("core.permissions.teams", TEAMS),
    ] {
        let state = state.clone();
        tokio::spawn(async move {
            let Ok(filter) = TopicFilter::new(topic) else { return };
            let mut subscription =
                match state.buses.events.subscribe(ConsumerGroup::new(name, filter)).await {
                    Ok(subscription) => subscription,
                    Err(err) => {
                        tracing::warn!(%err, "cannot watch {topic}; permissions will only expire");
                        return;
                    }
                };
            while let Some(delivery) = subscription.next().await {
                forget_all(&state).await;
                tracing::info!(%topic, "cached permission decisions dropped");
                let _ = subscription.ack(delivery.id).await;
            }
        });
    }
}

/// A caller that passed the permission check for the route it is on. The plugin comes from the path
/// and the method decides read or write, so a route is guarded by asking a handler for this.
#[derive(Debug, Clone)]
pub struct Authorised {
    pub principal: Principal,
    pub plugin: String,
    pub access: Access,
    /// Rule 4: a platform admin passes every check, including a plugin's own custom ones.
    pub admin: bool,
    pub scope: Option<Scope>,
    /// Core never checks these; they travel to the plugin, which does.
    pub custom: BTreeMap<String, Scope>,
    pub attributes: BTreeMap<String, String>,
}

impl Authorised {
    pub fn allows_custom(&self, name: &str, access: Access) -> bool {
        self.admin || self.custom.get(name).is_some_and(|scope| scope.allows(access))
    }
}

impl FromRequestParts<AppState> for Authorised {
    type Rejection = Problem;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let principal = Auth::from_request_parts(parts, state).await?.0;
        let plugin = plugin_of(&path_of(parts));
        let access = Access::of(parts.method.as_str());
        authorise(state, principal, &plugin, access).await
    }
}

/// `nest` rewrites the URI to the part the inner router matches, so the whole path has to be read
/// back out of the extensions. Reading `parts.uri` here would name every plugin route `core`.
pub(crate) fn path_of(parts: &Parts) -> String {
    parts
        .extensions
        .get::<OriginalUri>()
        .map(|uri| uri.0.path())
        .unwrap_or_else(|| parts.uri.path())
        .to_string()
}

/// Only what a plugin serves is checked against that plugin: `/api/v1/plugins/{id}/api|ui|internal`
/// (T22). `/api/v1/plugins/{id}` and its siblings are the platform administering the registry, so
/// they are core's — otherwise holding a permission in a plugin would let a caller drive it.
const FORWARDED: [&str; 3] = ["api", "ui", "internal"];

pub(crate) fn plugin_of(path: &str) -> String {
    let Some(rest) = path.strip_prefix("/api/v1/plugins/") else { return CORE.to_string() };
    let mut segments = rest.split('/');
    let Some(id) = segments.next().filter(|id| !id.is_empty()) else { return CORE.to_string() };
    match segments.next() {
        Some(next) if FORWARDED.contains(&next) => id.to_string(),
        _ => CORE.to_string(),
    }
}

pub async fn authorise(
    state: &AppState,
    principal: Principal,
    plugin: &str,
    access: Access,
) -> Result<Authorised, Problem> {
    let (authorised, allowed) = assess(state, principal, plugin, access).await;
    if !allowed {
        tracing::info!(
            principal = %authorised.principal.label(),
            plugin,
            ?access,
            "a request was refused by the permission check"
        );
        return Err(denied(plugin, member_kind(&authorised.principal), access));
    }
    Ok(authorised)
}

/// What the guard works out about a caller, and whether it is enough, without refusing anything.
pub async fn assess(
    state: &AppState,
    principal: Principal,
    plugin: &str,
    access: Access,
) -> (Authorised, bool) {
    let held = grants(state, &principal).await;
    let kind = member_kind(&principal);
    let admin = is_admin(&held, &principal);
    let effective = match kind {
        Some(kind) => held.effective(kind, plugin),
        None => Effective::default(),
    };
    let allowed = admin || effective.allows(access);
    let authorised = Authorised {
        principal,
        plugin: plugin.to_string(),
        access,
        admin,
        scope: effective.scope,
        custom: effective.custom,
        attributes: effective.attributes,
    };
    (authorised, allowed)
}

/// Asks the same question the guard asks, for a handler whose route is not the plugin's own — a
/// core route that an `rbac` permission opens, for instance.
pub async fn holds(state: &AppState, principal: &Principal, plugin: &str, access: Access) -> bool {
    let held = grants(state, principal).await;
    let Some(kind) = member_kind(principal) else { return false };
    is_admin(&held, principal) || held.effective(kind, plugin).allows(access)
}

fn denied(plugin: &str, kind: Option<MemberKind>, access: Access) -> Problem {
    let subject = match kind {
        Some(MemberKind::Service) => "service",
        _ => "user",
    };
    let scope = match access {
        Access::Read => Scope::Ro,
        Access::Write => Scope::Rw,
    };
    Problem::forbidden(format!("needs plugin:{plugin}:{subject}:{scope}")).with("plugin", plugin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::memory::{FakeHealth, FakeIdentity};
    use crate::fabric::Buses;
    use crate::identity::TokenOwner;
    use crate::secrets::TokenKind;
    use crate::testing::{get_as, post_json};
    use axum::routing::get;
    use axum::{Json, Router};
    use http::StatusCode;
    use serde_json::Value;

    const USER: &str = "doc_ses_user";
    const SERVICE: &str = "doc_svc_account";

    /// Holds the source's answer as raw JSON, so a malformed permission reaches the parser exactly
    /// as one from the RBAC plugin would.
    struct FakeSource(Value);

    #[async_trait]
    impl PermissionSource for FakeSource {
        async fn grants(&self, _: &Principal, _: &[Uuid]) -> Result<Grants, String> {
            serde_json::from_value(self.0.clone()).map_err(|err| err.to_string())
        }
    }

    struct Harness {
        app: Router,
        identity: Arc<FakeIdentity>,
    }

    fn harness(config: Config, source: Option<Value>) -> Harness {
        let identity = FakeIdentity::empty();
        let repos = crate::testing::repositories(FakeHealth::up(), identity.clone());
        let mut state = AppState::new(config, repos, Buses::in_memory());
        if let Some(grants) = source {
            state = state.with_permissions(Arc::new(FakeSource(grants)));
        }
        let routes = Router::new()
            .route("/guarded", get(report).post(report))
            .route("/plugins/{id}/api/thing", get(report).post(report));
        Harness { app: Router::new().nest("/api/v1", routes).with_state(state), identity }
    }

    fn granting(permissions: &[&str]) -> Option<Value> {
        Some(json!({ "permissions": permissions }))
    }

    /// A source that answers for teams: what each team holds, its people hold (T67).
    struct TeamSource(std::sync::Mutex<BTreeMap<Uuid, Vec<String>>>);

    #[async_trait]
    impl PermissionSource for TeamSource {
        async fn grants(&self, _: &Principal, teams: &[Uuid]) -> Result<Grants, String> {
            let held = self.0.lock().expect("not poisoned");
            let permissions: Vec<String> =
                teams.iter().filter_map(|team| held.get(team)).flatten().cloned().collect();
            serde_json::from_value(json!({ "permissions": permissions }))
                .map_err(|err| err.to_string())
        }
    }

    async fn report(caller: Authorised) -> Json<Value> {
        Json(json!({
            "plugin": caller.plugin,
            "admin": caller.admin,
            "custom": caller.custom,
            "attributes": caller.attributes,
        }))
    }

    impl Harness {
        fn user(self, login: &str) -> Self {
            let user = self.identity.add_user(login);
            self.identity.give(USER, TokenKind::Session, TokenOwner::User(user.id), None, false);
            self
        }

        fn service(self, name: &str) -> Self {
            let account = self.identity.add_service_account(name);
            let owner = TokenOwner::ServiceAccount(account.id);
            self.identity.give(SERVICE, TokenKind::Service, owner, None, false);
            self
        }

        async fn read(&self, path: &str, token: &str) -> StatusCode {
            get_as(&self.app, path, token).await.0
        }

        async fn write(&self, path: &str, token: &str) -> StatusCode {
            post_json(&self.app, path, Some(token), json!({})).await.0
        }
    }

    #[tokio::test]
    async fn every_scope_allows_exactly_the_access_it_names() {
        for (scope, read, write) in [
            ("ro", StatusCode::OK, StatusCode::FORBIDDEN),
            ("rw", StatusCode::OK, StatusCode::OK),
            ("wo", StatusCode::FORBIDDEN, StatusCode::OK),
        ] {
            let held = format!("plugin:core:user:{scope}");
            let app = harness(Config::default(), granting(&[&held])).user("ada");
            assert_eq!(app.read("/api/v1/guarded", USER).await, read, "{scope} read");
            assert_eq!(app.write("/api/v1/guarded", USER).await, write, "{scope} write");
        }
    }

    #[tokio::test]
    async fn the_default_scope_is_read_only() {
        let app = harness(Config::default(), granting(&["plugin:core:user"])).user("ada");
        assert_eq!(app.read("/api/v1/guarded", USER).await, StatusCode::OK);
        assert_eq!(app.write("/api/v1/guarded", USER).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn each_principal_kind_is_checked_against_its_own_permission() {
        let app = harness(Config::default(), granting(&["plugin:core:service:rw"])).service("bot");
        assert_eq!(app.read("/api/v1/guarded", SERVICE).await, StatusCode::OK);

        let app = harness(Config::default(), granting(&["plugin:core:service:rw"])).user("ada");
        assert_eq!(
            app.read("/api/v1/guarded", USER).await,
            StatusCode::FORBIDDEN,
            "a user must not be let in by a service permission"
        );

        let app = harness(Config::default(), granting(&["plugin:core:user:rw"])).service("bot");
        assert_eq!(
            app.read("/api/v1/guarded", SERVICE).await,
            StatusCode::FORBIDDEN,
            "a service account must not be let in by a user permission"
        );
    }

    #[tokio::test]
    async fn a_group_grants_what_it_holds() {
        let source = Some(json!({
            "permissions": ["plugin:kb:group:editors"],
            "groups": [{
                "plugin": "kb",
                "name": "editors",
                "kind": "user",
                "permissions": ["plugin:kb:user:rw"],
            }],
        }));
        let app = harness(Config::default(), source).user("ada");
        assert_eq!(app.write("/api/v1/plugins/kb/api/thing", USER).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn an_empty_group_still_grants_the_read_only_default() {
        let source = Some(json!({
            "permissions": ["plugin:kb:group:readers"],
            "groups": [{ "plugin": "kb", "name": "readers", "kind": "user" }],
        }));
        let app = harness(Config::default(), source).user("ada");
        assert_eq!(app.read("/api/v1/plugins/kb/api/thing", USER).await, StatusCode::OK);
        assert_eq!(app.write("/api/v1/plugins/kb/api/thing", USER).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn scopes_from_a_direct_grant_and_a_group_combine() {
        let source = Some(json!({
            "permissions": ["plugin:kb:user:ro", "plugin:kb:group:writers"],
            "groups": [{
                "plugin": "kb",
                "name": "writers",
                "kind": "user",
                "permissions": ["plugin:kb:user:wo"],
            }],
        }));
        let app = harness(Config::default(), source).user("ada");
        assert_eq!(app.read("/api/v1/plugins/kb/api/thing", USER).await, StatusCode::OK);
        assert_eq!(
            app.write("/api/v1/plugins/kb/api/thing", USER).await,
            StatusCode::OK,
            "ro and wo together are rw"
        );
    }

    #[tokio::test]
    async fn a_group_of_the_other_member_kind_grants_nothing() {
        let source = Some(json!({
            "permissions": ["plugin:kb:group:bots"],
            "groups": [{
                "plugin": "kb",
                "name": "bots",
                "kind": "service",
                "permissions": ["plugin:kb:service:rw"],
            }],
        }));
        let app = harness(Config::default(), source).user("ada");
        assert_eq!(app.read("/api/v1/plugins/kb/api/thing", USER).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_group_grants_on_every_plugin_it_reaches_not_only_the_one_it_is_listed_under() {
        let source = Some(json!({
            "permissions": ["plugin:kb:group:deployers"],
            "groups": [{
                "plugin": "kb",
                "name": "deployers",
                "kind": "user",
                "permissions": ["plugin:kb:user:ro", "plugin:water:user:rw"],
            }],
        }));
        let app = harness(Config::default(), source).user("ada");
        assert_eq!(app.read("/api/v1/plugins/kb/api/thing", USER).await, StatusCode::OK);
        assert_eq!(
            app.write("/api/v1/plugins/kb/api/thing", USER).await,
            StatusCode::FORBIDDEN,
            "it holds only read on the plugin it is listed under"
        );
        assert_eq!(
            app.write("/api/v1/plugins/water/api/thing", USER).await,
            StatusCode::OK,
            "membership is read wherever the group's permissions point"
        );
    }

    #[tokio::test]
    async fn a_group_grants_nothing_on_a_plugin_it_holds_nothing_for() {
        let source = Some(json!({
            "permissions": ["plugin:kb:group:deployers"],
            "groups": [{
                "plugin": "kb",
                "name": "deployers",
                "kind": "user",
                "permissions": ["plugin:water:user:rw"],
            }],
        }));
        let app = harness(Config::default(), source).user("ada");
        assert_eq!(
            app.read("/api/v1/plugins/kb/api/thing", USER).await,
            StatusCode::FORBIDDEN,
            "being listed under kb is not itself access to kb"
        );
        assert_eq!(app.write("/api/v1/plugins/water/api/thing", USER).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn membership_of_an_unknown_group_grants_nothing() {
        let app = harness(Config::default(), granting(&["plugin:kb:group:ghosts"])).user("ada");
        assert_eq!(app.read("/api/v1/plugins/kb/api/thing", USER).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn bootstrap_permissions_apply_with_no_source_at_all() {
        let mut config = Config::default();
        config.bootstrap.admins = vec!["ada".into()];
        let app = harness(config.clone(), None).user("ada");
        assert_eq!(app.write("/api/v1/guarded", USER).await, StatusCode::OK);

        let app = harness(config, None).service("operator");
        assert_eq!(app.read("/api/v1/guarded", SERVICE).await, StatusCode::OK);
        assert_eq!(
            app.write("/api/v1/guarded", SERVICE).await,
            StatusCode::FORBIDDEN,
            "the operator account is read-only"
        );
    }

    #[tokio::test]
    async fn a_platform_admin_passes_every_check() {
        let app = harness(Config::default(), granting(&["plugin:core:user:rw"])).user("ada");
        let (status, body, _) =
            post_json(&app.app, "/api/v1/plugins/kb/api/thing", Some(USER), json!({})).await;
        assert_eq!(status, StatusCode::OK, "an admin holds nothing for kb and still passes");
        assert_eq!(body["admin"], true, "the plugin is told, so its own checks pass too");
    }

    #[tokio::test]
    async fn custom_permissions_and_attributes_are_passed_through_unchecked() {
        let source = Some(json!({
            "permissions": ["plugin:kb:user:ro", "plugin:kb:pluginuser:imports:rw"],
            "attributes": { "team": "payments-core" },
        }));
        let app = harness(Config::default(), source).user("ada");
        let (status, body, _) = get_as(&app.app, "/api/v1/plugins/kb/api/thing", USER).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["plugin"], "kb");
        assert_eq!(body["admin"], false);
        assert_eq!(body["custom"]["imports"], "rw", "core passes it on without checking it");
        assert_eq!(body["attributes"]["team"], "payments-core");
    }

    #[tokio::test]
    async fn a_permission_for_one_plugin_does_not_open_another() {
        let app = harness(Config::default(), granting(&["plugin:kb:user:rw"])).user("ada");
        assert_eq!(app.read("/api/v1/plugins/kb/api/thing", USER).await, StatusCode::OK);
        assert_eq!(app.read("/api/v1/plugins/infra/api/thing", USER).await, StatusCode::FORBIDDEN);
        assert_eq!(app.read("/api/v1/guarded", USER).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn malformed_permissions_are_rejected_and_grant_nothing() {
        for bad in [
            "plugin:core:user:*",
            "plugin:kb:group",
            "plugin:kb:group:editors:rw",
            "plugin:core:pluginuser:imports",
            "plugin:kb:owner:rw",
            "plugin:kb:user:admin",
            "kb:user:rw",
            "plugin:KB:user:rw",
        ] {
            let app =
                harness(Config::default(), granting(&[bad, "plugin:core:user:rw"])).user("ada");
            assert_eq!(
                app.read("/api/v1/guarded", USER).await,
                StatusCode::FORBIDDEN,
                "{bad:?} must be refused, and take the whole answer with it"
            );
        }
    }

    #[tokio::test]
    async fn a_well_formed_answer_through_the_same_path_is_accepted() {
        let app = harness(Config::default(), granting(&["plugin:core:user:rw"])).user("ada");
        assert_eq!(app.read("/api/v1/guarded", USER).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn with_no_permission_source_every_check_fails_closed() {
        let app = harness(Config::default(), None).user("ada");
        assert_eq!(app.read("/api/v1/guarded", USER).await, StatusCode::FORBIDDEN);
        assert_eq!(app.write("/api/v1/guarded", USER).await, StatusCode::FORBIDDEN);
        assert_eq!(app.read("/api/v1/plugins/kb/api/thing", USER).await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_plugin_token_holds_nothing_of_its_own() {
        let app = harness(Config::default(), granting(&["plugin:core:user:rw"]));
        app.identity.give(
            "doc_reg_kb",
            TokenKind::PluginRegistration,
            TokenOwner::Plugin("kb".into()),
            None,
            false,
        );
        assert_eq!(app.read("/api/v1/guarded", "doc_reg_kb").await, StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn an_unauthenticated_request_is_refused_before_the_permission_check() {
        let app = harness(Config::default(), granting(&["plugin:core:user:rw"])).user("ada");
        assert_eq!(app.read("/api/v1/guarded", "doc_ses_nosuch").await, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_refusal_names_the_permission_that_was_needed() {
        let app = harness(Config::default(), granting(&[])).user("ada");
        let (status, body, content_type) =
            post_json(&app.app, "/api/v1/plugins/kb/api/thing", Some(USER), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(content_type.as_deref(), Some("application/problem+json"));
        assert_eq!(body["detail"], "needs plugin:kb:user:rw");
        assert_eq!(body["plugin"], "kb");
    }

    /// The RBAC plugin as core reaches it over the Service Bus, answering whatever it holds now.
    struct Provider(parking_lot::Mutex<Value>);

    #[async_trait]
    impl doc_servicebus::ServiceHandler for Provider {
        async fn handle(&self, request: doc_servicebus::Request) -> Result<Value, String> {
            assert_eq!(request.subject, PROVIDER_ROUTE);
            assert_eq!(request.payload["principal"]["kind"], "user");
            Ok(self.0.lock().clone())
        }
    }

    fn provider(grants: Value) -> Arc<Provider> {
        Arc::new(Provider(parking_lot::Mutex::new(grants)))
    }

    async fn over_the_bus(provider: Option<Arc<Provider>>) -> (Harness, AppState) {
        let identity = FakeIdentity::empty();
        let repos = crate::testing::repositories(FakeHealth::up(), identity.clone());
        let buses = Buses::in_memory();
        let source = Arc::new(RbacSource::new(buses.services.clone()));
        let state = AppState::new(Config::default(), repos, buses).with_permissions(source);
        if let Some(provider) = provider {
            state.buses.services.serve(Address::plugin(RBAC).unwrap(), provider).await.unwrap();
        }
        let routes = Router::new().route("/plugins/{id}/api/thing", get(report).post(report));
        let app = Router::new().nest("/api/v1", routes).with_state(state.clone());
        (Harness { app, identity }, state)
    }

    const THING: &str = "/api/v1/plugins/kb/api/thing";

    #[tokio::test]
    async fn core_follows_the_rbac_plugin_including_what_its_groups_grant() {
        let grants = json!({
            "permissions": ["plugin:kb:group:editors"],
            "groups": [{
                "plugin": "kb",
                "name": "editors",
                "kind": "user",
                "permissions": ["plugin:kb:user:rw"],
            }],
        });
        let (app, _) = over_the_bus(Some(provider(grants))).await;
        let app = app.user("ada");
        assert_eq!(app.write(THING, USER).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn with_the_rbac_plugin_stopped_core_fails_closed_until_it_answers_again() {
        let (app, state) = over_the_bus(None).await;
        let app = app.user("ada");
        assert_eq!(app.read(THING, USER).await, StatusCode::FORBIDDEN);

        let answer = provider(json!({ "permissions": ["plugin:kb:user:ro"] }));
        state.buses.services.serve(Address::plugin(RBAC).unwrap(), answer).await.unwrap();
        assert_eq!(
            app.read(THING, USER).await,
            StatusCode::OK,
            "a lookup that failed is asked again rather than cached as a refusal"
        );
    }

    #[tokio::test]
    async fn cached_decisions_go_when_the_rbac_plugin_changes_state() {
        let answer = provider(json!({ "permissions": ["plugin:kb:user:ro"] }));
        let (app, state) = over_the_bus(Some(answer.clone())).await;
        let app = app.user("ada");
        watch(&state);
        assert_eq!(app.read(THING, USER).await, StatusCode::OK);
        *answer.0.lock() = json!({});
        assert_eq!(app.read(THING, USER).await, StatusCode::OK, "the decision is cached");

        let topic = doc_eventbus::Topic::new(PROVIDER_STATE).unwrap();
        let event = doc_eventbus::Event::new(topic, "core.plugins", json!({ "state": "error" }));
        state.buses.events.publish(event).await.unwrap();
        let mut status = StatusCode::OK;
        for _ in 0..100 {
            status = app.read(THING, USER).await;
            if status == StatusCode::FORBIDDEN {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(status, StatusCode::FORBIDDEN, "the stopped plugin's answer is not kept");
    }

    /// A team's grants are part of everyone's answer in it, so a membership or a parent changing
    /// must drop the cached decisions, not wait for them to expire (T67).
    #[tokio::test]
    async fn cached_decisions_go_when_a_team_changes() {
        let answer = provider(json!({ "permissions": ["plugin:kb:user:ro"] }));
        let (app, state) = over_the_bus(Some(answer.clone())).await;
        let app = app.user("ada");
        watch(&state);
        assert_eq!(app.read(THING, USER).await, StatusCode::OK);
        *answer.0.lock() = json!({});
        assert_eq!(app.read(THING, USER).await, StatusCode::OK, "the decision is cached");

        let topic = doc_eventbus::Topic::new("platform.team.changed").unwrap();
        let moved = json!({ "id": Uuid::new_v4(), "members": { "removed": ["ada"] } });
        let event = doc_eventbus::Event::new(topic, "core.teams", moved);
        state.buses.events.publish(event).await.unwrap();
        let mut status = StatusCode::OK;
        for _ in 0..100 {
            status = app.read(THING, USER).await;
            if status == StatusCode::FORBIDDEN {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(status, StatusCode::FORBIDDEN, "what the team gave is asked for again");
    }

    /// Administering the registry is core's, not the plugin's: a caller who holds a permission in
    /// `kb` must not be able to unload it by reaching `/api/v1/plugins/kb/state`.
    #[test]
    fn only_forwarded_routes_are_named_after_their_plugin() {
        for forwarded in [
            "/api/v1/plugins/kb/api/search",
            "/api/v1/plugins/kb/ui",
            "/api/v1/plugins/kb/ui/page",
            "/api/v1/plugins/kb/internal/reindex",
        ] {
            assert_eq!(plugin_of(forwarded), "kb", "{forwarded}");
        }
        for administered in [
            "/api/v1/plugins",
            "/api/v1/plugins/kb",
            "/api/v1/plugins/kb/state",
            "/api/v1/plugins/kb/permissions",
            "/api/v1/status",
            "/api/v1/plugins/",
        ] {
            assert_eq!(plugin_of(administered), CORE, "{administered}");
        }
    }

    #[tokio::test]
    async fn a_person_holds_what_their_teams_and_the_teams_above_them_hold() {
        use crate::db::repositories::TeamRepository;
        use crate::teams::{BY_HAND, NewTeam};

        let identity = FakeIdentity::empty();
        let ada = identity.add_user("ada");
        identity.give(USER, TokenKind::Session, TokenOwner::User(ada.id), None, false);
        let organisation = identity.organisation();
        let above = identity
            .create_team(&NewTeam {
                organisation_id: organisation,
                parent_id: None,
                name: "platform".into(),
                title: "Platform".into(),
                description: String::new(),
                email: String::new(),
                is_default: false,
                provided: None,
            })
            .await
            .expect("a team");
        let below = identity
            .create_team(&NewTeam {
                organisation_id: organisation,
                parent_id: Some(above.id),
                name: "tools".into(),
                title: "Tools".into(),
                description: String::new(),
                email: String::new(),
                is_default: false,
                provided: None,
            })
            .await
            .expect("a team inside it");
        identity.add_member(below.id, ada.id, BY_HAND, None).await.expect("in the team");

        let held = BTreeMap::from([(above.id, vec!["plugin:kb:user:rw".to_string()])]);
        let source = Arc::new(TeamSource(std::sync::Mutex::new(held)));
        let repos = crate::testing::repositories(FakeHealth::up(), identity.clone());
        let state = AppState::new(Config::default(), repos, Buses::in_memory())
            .with_permissions(source.clone());
        let routes = Router::new().route("/plugins/{id}/api/thing", get(report).post(report));
        let app: Router = Router::new().nest("/api/v1", routes).with_state(state.clone());

        let (status, _, _) =
            post_json(&app, "/api/v1/plugins/kb/api/thing", Some(USER), json!({})).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "the team above the one she is in holds it, and access flows down"
        );

        // Taken out of the team, she holds nothing again — once the cached decision is dropped.
        identity.remove_member(below.id, ada.id).await.expect("taken out");
        forget_all(&state).await;
        let (status, _, _) =
            post_json(&app, "/api/v1/plugins/kb/api/thing", Some(USER), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "out of the team, out of what it held");
    }

    #[tokio::test]
    async fn read_access_to_every_plugin_opens_pages_and_nothing_else() {
        let harness = harness(Config::default(), granting(&["plugin:*:user:ro"])).user("ada");
        let app = &harness.app;

        let (status, _, _) = get_as(app, "/api/v1/plugins/kb/api/thing", USER).await;
        assert_eq!(status, StatusCode::OK, "every plugin's pages, including ones added later");
        let (status, _, _) = get_as(app, "/api/v1/plugins/water/api/thing", USER).await;
        assert_eq!(status, StatusCode::OK, "a plugin nobody named");

        let (status, _, _) =
            post_json(app, "/api/v1/plugins/kb/api/thing", Some(USER), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "reading only");

        let (status, _, _) = get_as(app, "/api/v1/guarded", USER).await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "the platform's own pages are not a plugin's, so it never opens them"
        );

        let (_, body, _) = get_as(app, "/api/v1/plugins/kb/api/thing", USER).await;
        assert_eq!(body["admin"], false, "it never makes anyone a platform administrator");
        assert_eq!(body["custom"], json!({}), "and never a plugin's own custom permissions");
    }

    #[tokio::test]
    async fn only_read_access_to_every_plugin_can_be_written_that_way() {
        for refused in ["plugin:*:user:rw", "plugin:*:service:ro", "plugin:*:group:editors"] {
            let harness = harness(Config::default(), granting(&[refused])).user("ada");
            let (status, _, _) = get_as(&harness.app, "/api/v1/plugins/kb/api/thing", USER).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{refused} is not a permission");
        }
    }

    #[tokio::test]
    async fn a_default_team_gives_its_people_read_access_to_every_plugin() {
        let identity = FakeIdentity::empty();
        let ada = identity.add_user("ada");
        identity.give(USER, TokenKind::Session, TokenOwner::User(ada.id), None, false);
        identity.with_default_teams();

        let repos = crate::testing::repositories(FakeHealth::up(), identity.clone());
        let state = AppState::new(Config::default(), repos, Buses::in_memory());
        let routes = Router::new().route("/plugins/{id}/api/thing", get(report).post(report));
        let app: Router = Router::new().nest("/api/v1", routes).with_state(state);

        let (status, _, _) = get_as(&app, "/api/v1/plugins/kb/api/thing", USER).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "everyone is placed in the default teams, and a default team reads every plugin"
        );
        let (status, _, _) =
            post_json(&app, "/api/v1/plugins/kb/api/thing", Some(USER), json!({})).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "reading only");
    }
}
