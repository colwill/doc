//! The plugin's routes. `api/*` and `ui/*` sit behind core's read or write check on `rbac` (rule
//! 5), and `internal/*` answers core alone: the permission provider and rule 6's self-service.

use doc_permissions::{Grants, MemberKind, Subject};
use doc_plugin_sdk::{Backend, Request, Response};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::model::{Holder, Refusal, parse};
use crate::ops;
use crate::store::Store;
use crate::{checks, ui};

type Answer = Result<(u16, Value), Refusal>;

pub async fn handle(backend: &Backend, request: Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    match segments.as_slice() {
        ["internal", route @ ..] => {
            if backend.caller().is_none_or(|caller| caller.kind != "platform") {
                return Refusal::forbidden("internal routes answer core alone").response();
            }
            match internal(backend, &request, route).await {
                Ok(value) => Response::json(&value),
                Err(refusal) => refusal.internal(),
            }
        }
        ["ui", route @ ..] => ui::handle(backend, &request, route).await,
        ["api", route @ ..] => match api(backend, &request, route).await {
            Ok((204, _)) => Response::new(204, "application/json", Vec::new()),
            Ok((status, value)) => Response::new(
                status,
                "application/json",
                serde_json::to_vec(&value).unwrap_or_default(),
            ),
            Err(refusal) => refusal.response(),
        },
        _ => Refusal::missing("no such route").response(),
    }
}

fn ok<T: Serialize>(value: T) -> Answer {
    Ok((200, json!(value)))
}

fn body<T: DeserializeOwned>(request: &Request) -> Result<T, Refusal> {
    serde_json::from_slice(&request.body)
        .map_err(|err| Refusal::bad(format!("the body is not what this route takes: {err}")))
}

pub fn query(request: &Request, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.into_owned())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewGroup {
    plugin: String,
    name: String,
    kind: MemberKind,
    #[serde(default)]
    description: String,
    #[serde(default)]
    permissions: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupChange {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    permissions: Option<Vec<String>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AssignmentBody {
    holder: Holder,
    permission: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AttributeBody {
    holder: Holder,
    key: String,
    #[serde(default)]
    value: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleTest {
    user: Uuid,
}

async fn api(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let store = Store(backend);
    match (request.method.as_str(), path) {
        ("GET", ["groups"]) => ok(store.groups(query(request, "plugin").as_deref()).await?),
        ("POST", ["groups"]) => {
            let new: NewGroup = body(request)?;
            let group = ops::create_group(
                backend,
                &store,
                &new.plugin,
                &new.name,
                new.kind,
                &new.description,
                &new.permissions,
            )
            .await?;
            Ok((201, json!(group)))
        }
        ("GET", ["groups", plugin, name]) => {
            let group = ops::found_group(&store, plugin, name).await?;
            let members = store.members(&group).await?;
            let holder = Holder::Group { plugin: (*plugin).into(), name: (*name).into() };
            let attributes = store.attributes(&holder).await?;
            ok(json!({ "group": group, "members": members, "attributes": attributes }))
        }
        ("PATCH", ["groups", plugin, name]) => {
            let change: GroupChange = body(request)?;
            let description = change.description.as_deref();
            let permissions = change.permissions.as_deref();
            ok(ops::update_group(backend, &store, plugin, name, description, permissions).await?)
        }
        ("DELETE", ["groups", plugin, name]) => {
            ops::delete_group(backend, &store, plugin, name).await?;
            Ok((204, Value::Null))
        }
        ("POST", ["assignments"]) => {
            let assignment: AssignmentBody = body(request)?;
            let (permission, added) =
                ops::grant(backend, &store, &assignment.holder, &assignment.permission).await?;
            let granted =
                json!({ "holder": assignment.holder, "permission": permission.to_string() });
            Ok((if added { 201 } else { 200 }, granted))
        }
        ("POST", ["assignments", "revoke"]) => {
            let assignment: AssignmentBody = body(request)?;
            ops::revoke(backend, &store, &assignment.holder, &assignment.permission).await?;
            Ok((204, Value::Null))
        }
        ("POST", ["attributes"]) => {
            let attribute: AttributeBody = body(request)?;
            let value = attribute.value.as_deref().unwrap_or_default();
            ops::set_attribute(backend, &store, &attribute.holder, &attribute.key, value).await?;
            ok(json!({ "holder": attribute.holder, "key": attribute.key, "value": value.trim() }))
        }
        ("POST", ["attributes", "remove"]) => {
            let attribute: AttributeBody = body(request)?;
            ops::remove_attribute(backend, &store, &attribute.holder, &attribute.key).await?;
            Ok((204, Value::Null))
        }
        ("GET", ["plugins", plugin]) => {
            store.known().await?.exists(plugin)?;
            let groups = store.groups(Some(plugin)).await?;
            let assignments = store.assignments_in(plugin).await?;
            ok(json!({ "plugin": plugin, "groups": groups, "assignments": assignments }))
        }
        ("GET", ["users"]) => {
            let login = query(request, "login").ok_or_else(|| Refusal::bad("name a `login`"))?;
            ok(store.users(&login).await?)
        }
        ("GET", ["principals", kind, id]) => {
            ok(ops::principal(&store, &ops::holder_of(kind, id)?).await?)
        }
        ("GET", ["offboarding-rules"]) => ok(store.offboarding_rules().await?),
        ("GET", ["offboarding-rules", id]) => {
            let id: Uuid = id.parse().map_err(|_| Refusal::bad("a rule is named by its ID"))?;
            let rule = store
                .offboarding_rule(id)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such rule"))?;
            ok(rule)
        }
        ("GET", ["rules"]) => ok(store.rules().await?),
        ("POST", ["rules"]) => {
            Ok((201, json!(ops::create_rule(backend, &store, body(request)?).await?)))
        }
        ("GET", ["rules", id]) => ok(ops::rule(&store, id).await?),
        ("PATCH", ["rules", id]) => {
            ok(ops::update_rule(backend, &store, id, body(request)?).await?)
        }
        ("DELETE", ["rules", id]) => {
            ops::delete_rule(backend, &store, id).await?;
            Ok((204, Value::Null))
        }
        ("POST", ["rules", id, "test"]) => {
            let test: RuleTest = body(request)?;
            ok(ops::test_rule(&store, &ops::rule(&store, id).await?, test.user).await?)
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

async fn internal(backend: &Backend, request: &Request, path: &[&str]) -> Result<Value, Refusal> {
    let store = Store(backend);
    match path {
        ["effective-permissions"] => effective(&store, body(request)?).await,
        ["principal"] => ops::principal(&store, &body::<HolderBody>(request)?.holder).await,
        ["self-service", "grant"] => self_service_grant(backend, &store, body(request)?).await,
        ["self-service", "revoke"] => self_service_revoke(backend, &store, body(request)?).await,
        _ => Err(Refusal::missing("no such route")),
    }
}

#[derive(Deserialize)]
struct HolderBody {
    holder: Holder,
}

#[derive(Deserialize)]
struct Lookup {
    principal: CorePrincipal,
    /// The teams this principal is in and every team above those, which core works out from its
    /// own records and sends, since it holds them (T67). Their grants are added to the person's.
    #[serde(default)]
    teams: Vec<Uuid>,
}

/// Core's own principal, of which only users and service accounts hold anything.
#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
enum CorePrincipal {
    User {
        id: Uuid,
    },
    ServiceAccount {
        id: Uuid,
    },
    #[serde(other)]
    Other,
}

async fn effective(store: &Store<'_>, lookup: Lookup) -> Result<Value, Refusal> {
    let holder = match lookup.principal {
        CorePrincipal::User { id } => Holder::User { id },
        CorePrincipal::ServiceAccount { id } => Holder::Service { id },
        CorePrincipal::Other => return Ok(json!(Grants::default())),
    };
    let mut grants = store.grants(&holder).await?;
    // A person holds what their teams hold. Only a person: a service account belongs to nobody's
    // team, and core sends none for one.
    if matches!(holder, Holder::User { .. }) {
        for team in lookup.teams {
            let held = store.grants(&Holder::Team { id: team }).await?;
            grants.permissions.extend(held.permissions);
            grants.groups.extend(held.groups);
            // Attributes stay a person's own: a team's are the team's.
        }
    }
    Ok(json!(grants))
}

#[derive(Deserialize)]
struct SelfService {
    account: Uuid,
    permission: String,
    by: String,
    #[serde(default)]
    grants: Grants,
    #[serde(default)]
    unlimited: bool,
}

/// An owner grants no more than their own access, as core computed it.
async fn self_service_grant(
    backend: &Backend,
    store: &Store<'_>,
    request: SelfService,
) -> Result<Value, Refusal> {
    let holder = Holder::Service { id: request.account };
    let known = store.known().await?;
    checks::holder(store, &holder).await?;
    let permission = checks::permission(store, &known, &holder, &request.permission).await?;
    if !request.unlimited {
        let mut held = request.grants;
        if let Subject::Group { name } = &permission.subject
            && let Some(group) = store.group(&permission.plugin, name).await?
        {
            held.groups.push(group.group()?);
        }
        if !held.may_grant(MemberKind::User, &permission) {
            return Err(Refusal::forbidden(format!(
                "{permission} exceeds your own access to {}",
                permission.plugin
            )));
        }
    }
    let added = store.grant(&holder, &permission, "self-service", &request.by).await?;
    if added {
        let detail = json!({ "permission": permission.to_string(), "by": request.by });
        ops::record(backend, "self-service.granted", &request.account.to_string(), detail).await;
    }
    Ok(json!({ "holder": holder, "permission": permission.to_string(), "added": added }))
}

async fn self_service_revoke(
    backend: &Backend,
    store: &Store<'_>,
    request: SelfService,
) -> Result<Value, Refusal> {
    let holder = Holder::Service { id: request.account };
    let permission = parse(&request.permission)?;
    if !store.revoke(&holder, &permission).await? {
        return Err(Refusal::missing(format!("the account does not hold {permission}")));
    }
    let detail = json!({ "permission": permission.to_string(), "by": request.by });
    ops::record(backend, "self-service.revoked", &request.account.to_string(), detail).await;
    Ok(json!({ "holder": holder, "permission": permission.to_string() }))
}
