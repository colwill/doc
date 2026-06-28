//! The backend API from §5, as plugins call it over `/plugin/v1/*`. Every function takes the plugin
//! its registration token named, never one from the request, so nothing here can be pointed at
//! another plugin's schema, topics, cache or state.

use std::time::Duration;

use doc_cachebus::{CacheBusError, Namespace};
use doc_eventbus::{Event, Topic};
use doc_plugin_protocol::calls::{
    AuditRequest, CacheRequest, CacheResponse, DelegationRequest, DelegationResponse,
    DeprovisionRequest, DeprovisionResponse, IdentityRequest, IdentityResponse, LinkRequest,
    LinkResponse, OffboardRequest, OffboardResponse, OpenRequest, OpenResponse,
    OrganisationRequest, OrganisationResponse, PersonRequest, PublishRequest, PublishResponse,
    RevokeScopedTokenRequest, RevokeScopedTokenResponse, ScopedTokenRequest, ScopedTokenResponse,
    SealRequest, SealResponse, SealedValue, SecretsChangedRequest, SecretsChangedResponse,
    ServiceRequest, ServiceResponse, StateRequest, StateResponse, StatusRequest, TaskRequest,
    TaskResponse, TeamMembersRequest, TeamMembersResponse, TeamRemoveRequest, TeamRemoveResponse,
    TeamRequest, TeamResponse, UserRequest, UserResponse, WriteTeamRequest,
};
use doc_plugin_protocol::data::DataRequest;
use doc_plugin_protocol::{Capability, PluginState};
use doc_servicebus::{Address, Message, ServiceBusError};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::api::AppState;
use crate::api::auth::{Membership, start_session};
use crate::db::repositories::Delegation;
use crate::identity::{Account, Arrival, AuditEntry, Principal, Profile, TokenOwner};
use crate::plugins::{self, TransitionError, settings};
use crate::teams::Team;

/// The TTL a plugin's cache entry gets when it names none.
pub const CACHE_TTL: Duration = Duration::from_secs(24 * 3600);
pub const CACHE_ENTRIES: usize = 10_000;
const MAX_KEY: usize = 256;
const MAX_STATE_BYTES: usize = 1024 * 1024;
const REQUEST_DEADLINE: Duration = Duration::from_secs(10);
const MAX_REQUEST_DEADLINE: Duration = Duration::from_secs(60);

/// A refusal in RFC 9457 terms; the host turns it into the problem the plugin receives.
#[derive(Debug)]
pub struct Refusal {
    pub status: u16,
    pub kind: &'static str,
    pub detail: String,
}

impl Refusal {
    pub fn new(status: u16, kind: &'static str, detail: impl Into<String>) -> Self {
        Self { status, kind, detail: detail.into() }
    }

    fn forbidden(detail: impl Into<String>) -> Self {
        Self::new(403, "forbidden", detail)
    }

    pub fn bad(detail: impl Into<String>) -> Self {
        Self::new(400, "bad-request", detail)
    }

    pub fn unavailable(detail: impl Into<String>) -> Self {
        Self::new(503, "unavailable", detail)
    }
}

impl From<TransitionError> for Refusal {
    fn from(err: TransitionError) -> Self {
        let kind = match err {
            TransitionError::Unknown(_) => "not-registered",
            TransitionError::Illegal { .. } => "illegal-transition",
            TransitionError::Busy(_) => "handover-in-progress",
            TransitionError::Underway(..) => "transition-in-progress",
            TransitionError::Superseded(_) => "superseded",
            TransitionError::Call(_) => "call-failed",
            TransitionError::Needed(..) => "needed",
            TransitionError::Off(_) => "turned-off",
            TransitionError::Follows(..) => "follows-flag",
            TransitionError::Invalid(_) => "invalid",
        };
        Self::new(err.status(), kind, err.to_string())
    }
}

pub fn namespace(plugin: &str) -> Result<Namespace, Refusal> {
    Namespace::new(format!("plugin.{plugin}")).map_err(|err| Refusal::bad(err.to_string()))
}

fn key(key: &str) -> Result<&str, Refusal> {
    match key.is_empty() || key.len() > MAX_KEY {
        true => Err(Refusal::bad(format!("a key is 1 to {MAX_KEY} bytes"))),
        false => Ok(key),
    }
}

fn cache_refusal(err: &CacheBusError) -> Refusal {
    Refusal::unavailable(err.to_string())
}

/// Whom the call being handled is for, when the plugin handed its context token back.
fn acting_for(state: &AppState, plugin: &str, context: Option<&str>) -> Option<Principal> {
    state.plugins.contexts.resolve(plugin, context?)
}

/// The service account a plugin with `service-account` is when it asks as itself, if core has
/// made it one.
async fn own_service_account(state: &AppState, plugin: &str) -> Result<Option<Principal>, Refusal> {
    let capable = state
        .plugins
        .get(plugin)
        .await
        .is_some_and(|entry| entry.manifest.capabilities.contains(&Capability::ServiceAccount));
    if !capable {
        return Ok(None);
    }
    let account = state
        .repos
        .identity
        .plugin_service_account(plugin)
        .await
        .map_err(|err| Refusal::unavailable(err.to_string()))?;
    Ok(account.map(Principal::ServiceAccount))
}

/// Its own collections, the exports of other plugins and `core.*` (§9.1).
pub async fn data(state: &AppState, plugin: &str, request: DataRequest) -> Result<Value, Refusal> {
    crate::data::handle(state, plugin, request).await
}

/// Only into the plugin's own `plugin.<id>.` prefix, so a plugin cannot speak for the platform or
/// for another plugin.
pub async fn publish(
    state: &AppState,
    plugin: &str,
    request: PublishRequest,
) -> Result<PublishResponse, Refusal> {
    let prefix = format!("plugin.{plugin}.");
    if !request.topic.starts_with(&prefix) || request.topic.len() == prefix.len() {
        return Err(Refusal::forbidden(format!("{plugin} may only publish to {prefix}*")));
    }
    let topic = Topic::new(&request.topic).map_err(|err| Refusal::bad(err.to_string()))?;
    let mut event = Event::new(topic, format!("plugin.{plugin}"), request.payload);
    // Keys are global on the bus, so each plugin's are scoped to it: otherwise one plugin could
    // claim a key first and silently swallow another's event.
    if let Some(idempotency) = request.idempotency_key {
        event = event.idempotent(format!("plugin.{plugin}:{idempotency}"));
    }
    let ack = state
        .buses
        .events
        .publish(event)
        .await
        .map_err(|err| Refusal::unavailable(err.to_string()))?;
    Ok(PublishResponse { id: ack.id })
}

/// Made as the principal the context token names. Without a live token there is no one to act for,
/// so the call is refused rather than made as the plugin itself.
pub async fn services(
    state: &AppState,
    plugin: &str,
    context: Option<&str>,
    request: ServiceRequest,
) -> Result<ServiceResponse, Refusal> {
    if context.is_none() {
        return Err(Refusal::forbidden(
            "a Service Bus call needs the context token of the call being handled",
        ));
    }
    let principal = acting_for(state, plugin, context).ok_or_else(|| {
        Refusal::forbidden("this context token was not issued for this call, or its call has ended")
    })?;
    let address = Address::new(&request.address).map_err(|err| Refusal::bad(err.to_string()))?;
    // Only the principal's reference crosses the bus, so a scoped token's limits would not: a call
    // it made is not passed on as its holder. A discovery route answers the plugin itself, as ever.
    if principal.scopes().is_some() && !request.subject.starts_with("discovery/") {
        return Err(Refusal::forbidden(
            "a call made with a scoped token is not passed on to other plugins as its holder",
        ));
    }
    // A `discovery/*` route answers the plugin that asks, whoever that plugin is working for.
    let discovery = request.subject.starts_with("discovery/");
    let reference = match (&principal, discovery) {
        (_, true) => Principal::Plugin { id: plugin.to_string() }.reference(),
        // Asking `api/` as itself, a plugin is its own service account when it has one, and
        // otherwise nobody, who holds nothing.
        (Principal::Plugin { id }, false) if id == plugin => own_service_account(state, plugin)
            .await?
            .map_or_else(|| principal.reference(), |account| account.reference()),
        (_, false) => principal.reference(),
    };
    let mut payload = request.payload;
    // Who relayed it, and the stricter of the guard asked for and the one on the call being
    // handled, so a limit cannot be stepped round by asking another plugin to ask. Core writes
    // both, whatever the payload said.
    let relayed = request.subject.starts_with("api/");
    if let (true, Some(fields)) = (relayed, payload.as_object_mut()) {
        let (_, held) =
            context.map(|token| state.plugins.contexts.relay_of(plugin, token)).unwrap_or_default();
        fields.insert("via".into(), json!(plugin));
        fields.insert("guard".into(), json!(request.guard.max(held)));
    }
    if request.queue {
        let message = Message::new(address, request.subject, payload).as_principal(&reference);
        state
            .buses
            .services
            .send(message)
            .await
            .map_err(|err| Refusal::unavailable(err.to_string()))?;
        return Ok(ServiceResponse { payload: Value::Null });
    }
    let deadline = request
        .deadline_ms
        .map_or(REQUEST_DEADLINE, Duration::from_millis)
        .min(MAX_REQUEST_DEADLINE);
    let answer = state
        .buses
        .services
        .request_as(&address, &request.subject, payload, deadline, Some(&reference))
        .await;
    match answer {
        Ok(payload) => Ok(ServiceResponse { payload }),
        Err(err @ ServiceBusError::NoHandler(_)) => {
            Err(Refusal::new(404, "no-handler", err.to_string()))
        }
        Err(err @ ServiceBusError::DeadlineExceeded(_)) => {
            Err(Refusal::new(504, "deadline", err.to_string()))
        }
        Err(err @ ServiceBusError::Remote { .. }) => {
            Err(Refusal::new(502, "remote-error", err.to_string()))
        }
        Err(err) => Err(Refusal::unavailable(err.to_string())),
    }
}

pub async fn cache(
    state: &AppState,
    plugin: &str,
    request: CacheRequest,
) -> Result<CacheResponse, Refusal> {
    let namespace = namespace(plugin)?;
    let cache = &state.buses.cache;
    let ttl = |ms: Option<u64>| ms.map(Duration::from_millis);
    match request {
        CacheRequest::Get { key: name } => {
            let entry = cache.get(&namespace, key(&name)?).await.map_err(|e| cache_refusal(&e))?;
            Ok(CacheResponse {
                version: entry.as_ref().map(|entry| entry.version),
                value: entry.map(|entry| entry.value),
                applied: true,
            })
        }
        CacheRequest::Set { key: name, value, ttl_ms } => {
            let entry = cache
                .set(&namespace, key(&name)?, value, ttl(ttl_ms))
                .await
                .map_err(|e| cache_refusal(&e))?;
            Ok(CacheResponse { value: None, version: Some(entry.version), applied: true })
        }
        CacheRequest::Delete { key: name } => {
            let removed =
                cache.delete(&namespace, key(&name)?).await.map_err(|e| cache_refusal(&e))?;
            Ok(CacheResponse { value: None, version: None, applied: removed })
        }
        CacheRequest::CompareAndSet { key: name, value, version, ttl_ms } => {
            let written = cache
                .compare_and_set(&namespace, key(&name)?, version, value, ttl(ttl_ms))
                .await
                .map_err(|e| cache_refusal(&e))?;
            Ok(CacheResponse {
                value: None,
                version: written.as_ref().map(|entry| entry.version),
                applied: written.is_some(),
            })
        }
    }
}

/// Queues a background `run` of this plugin. It is started by whoever the call is for, by the
/// plugin itself when it is doing its own work, or by whoever gave the delegation it names.
pub async fn tasks(
    state: &AppState,
    plugin: &str,
    context: Option<&str>,
    request: TaskRequest,
) -> Result<TaskResponse, Refusal> {
    let by = match request.delegation {
        Some(id) => delegator(state, plugin, id).await?,
        // A run outlives the call, and a scoped token's limits do not travel with a task, so one
        // started during such a call is the plugin's own.
        None => acting_for(state, plugin, context)
            .filter(|principal| principal.scopes().is_none())
            .unwrap_or(Principal::Plugin { id: plugin.into() }),
    };
    let chain = context.and_then(|token| state.plugins.contexts.chain(plugin, token));
    let new = doc_background_tasks::NewTask {
        kind: format!("plugin.{plugin}.run"),
        payload: request.payload,
        max_attempts: request.max_attempts.unwrap_or(3).clamp(1, 10),
        started_by: crate::api::tasks::actor(&by),
        chain,
    };
    let task =
        doc_background_tasks::start(state.repos.tasks.as_ref(), state.buses.services.as_ref(), new)
            .await
            .map_err(|err| Refusal::unavailable(err.to_string()))?;
    Ok(TaskResponse { task: task.id })
}

/// Whoever gave `plugin` the delegation `id`, as they are now: a revoked delegation, or one from
/// a disabled or deleted account, starts nothing.
async fn delegator(state: &AppState, plugin: &str, id: Uuid) -> Result<Principal, Refusal> {
    let unavailable =
        |err: crate::db::repositories::RepositoryError| Refusal::unavailable(err.to_string());
    let given = state.repos.plugins.delegation(id).await.map_err(unavailable)?;
    let Some(given) = given.filter(|given| given.plugin == plugin) else {
        return Err(Refusal::new(
            404,
            "no-delegation",
            format!("{plugin} holds no delegation {id}"),
        ));
    };
    if given.revoked_at.is_some() {
        return Err(Refusal::forbidden(format!("delegation {id} was revoked")));
    }
    crate::permissions::principal_of(state, &given.principal).await.map_err(Refusal::forbidden)
}

/// Leave to start tasks later as whoever the call is for, such as an automation running as its
/// owner. Only a person or a service account gives it, during a call they made.
pub async fn delegations(
    state: &AppState,
    plugin: &str,
    context: Option<&str>,
    request: DelegationRequest,
) -> Result<DelegationResponse, Refusal> {
    let unavailable =
        |err: crate::db::repositories::RepositoryError| Refusal::unavailable(err.to_string());
    match request {
        DelegationRequest::Grant { purpose } => {
            let purpose = purpose.trim().to_string();
            if purpose.is_empty() || purpose.chars().count() > 200 {
                return Err(Refusal::bad("say what the delegation is for, in 1 to 200 characters"));
            }
            let principal = acting_for(state, plugin, context)
                .filter(|principal| !matches!(principal, Principal::Plugin { .. }))
                .ok_or_else(|| {
                    Refusal::forbidden(
                        "only a person or a service account can delegate, during a call they made",
                    )
                })?;
            // A delegation acts later as its giver in full, which a scoped token never may.
            if principal.scopes().is_some() {
                return Err(Refusal::forbidden("a scoped token cannot delegate"));
            }
            let given = Delegation {
                id: Uuid::now_v7(),
                plugin: plugin.to_string(),
                principal: principal.reference(),
                purpose: purpose.clone(),
                created_at: chrono::Utc::now(),
                revoked_at: None,
            };
            state.repos.plugins.delegate(&given).await.map_err(unavailable)?;
            let entry = AuditEntry::new("delegation.granted")
                .by(&principal)
                .subject(format!("plugin:{plugin}"))
                .detail(json!({ "delegation": given.id, "purpose": purpose }));
            if let Err(err) = state.repos.identity.record_audit(entry).await {
                tracing::warn!(%err, plugin, "a delegation was not written to the audit log");
            }
            Ok(DelegationResponse { delegation: Some(given.id), revoked: None })
        }
        DelegationRequest::Revoke { delegation } => {
            let revoked = state
                .repos
                .plugins
                .revoke_delegation(plugin, delegation)
                .await
                .map_err(unavailable)?;
            if revoked {
                let entry = AuditEntry::new("delegation.revoked")
                    .by(&Principal::Plugin { id: plugin.into() })
                    .subject(format!("plugin:{plugin}"))
                    .detail(json!({ "delegation": delegation }));
                if let Err(err) = state.repos.identity.record_audit(entry).await {
                    tracing::warn!(%err, plugin, "a revocation was not written to the audit log");
                }
            }
            Ok(DelegationResponse { delegation: Some(delegation), revoked: Some(revoked) })
        }
    }
}

/// A plugin's own settings, secrets and all (ADR-0007). The plugin comes from the registration
/// token on the call, so this can only ever hand a plugin what it declared for itself.
pub async fn settings(
    state: &AppState,
    plugin: &str,
) -> Result<doc_plugin_protocol::calls::SettingsView, Refusal> {
    let manifest = settings::manifest_of(state, plugin)
        .await
        .ok_or_else(|| Refusal::new(404, "not-registered", "this plugin has no manifest"))?;
    let mut view = settings::resolve(state, plugin, &manifest).await.for_plugin();
    view.instance = state.config.instance.for_plugins();
    Ok(view)
}

pub async fn plugin_state(
    state: &AppState,
    plugin: &str,
    request: StateRequest,
) -> Result<StateResponse, Refusal> {
    let store = &state.repos.plugins;
    let unavailable =
        |err: crate::db::repositories::RepositoryError| Refusal::unavailable(err.to_string());
    match request {
        StateRequest::Get { key: name } => {
            let value = store.state_get(plugin, key(&name)?).await.map_err(unavailable)?;
            Ok(StateResponse { value })
        }
        StateRequest::Set { key: name, value } => {
            let size = serde_json::to_vec(&value).map(|bytes| bytes.len()).unwrap_or(usize::MAX);
            if size > MAX_STATE_BYTES {
                return Err(Refusal::bad(format!(
                    "a state value is at most {MAX_STATE_BYTES} bytes"
                )));
            }
            store.state_set(plugin, key(&name)?, &value).await.map_err(unavailable)?;
            Ok(StateResponse { value: None })
        }
        StateRequest::Delete { key: name } => {
            store.state_delete(plugin, key(&name)?).await.map_err(unavailable)?;
            Ok(StateResponse { value: None })
        }
    }
}

/// Recorded under `plugin.<id>.`, so a plugin can add to the audit log but never write an entry
/// that reads as the platform's own.
pub async fn audit(
    state: &AppState,
    plugin: &str,
    context: Option<&str>,
    request: AuditRequest,
) -> Result<Value, Refusal> {
    let action = &request.action;
    let valid = !action.is_empty()
        && action.len() <= 64
        && !action.starts_with('.')
        && !action.ends_with('.')
        && action.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ".-".contains(c));
    if !valid {
        return Err(Refusal::bad("an action is 1 to 64 of a-z, 0-9, `.` and `-`"));
    }
    let mut detail = match request.detail {
        Value::Object(map) => Value::Object(map),
        Value::Null => json!({}),
        other => json!({ "detail": other }),
    };
    if let Some(principal) = acting_for(state, plugin, context) {
        detail["on_behalf_of"] = json!(principal.reference());
    }
    // The plugin that relayed the call being handled, such as Agent Smith working for somebody.
    if let (Some(via), _) =
        context.map(|token| state.plugins.contexts.relay_of(plugin, token)).unwrap_or_default()
    {
        detail["via"] = json!(format!("plugin:{via}"));
    }
    let mut entry = AuditEntry::new(format!("plugin.{plugin}.{action}"))
        .by(&Principal::Plugin { id: plugin.into() })
        .detail(detail);
    if let Some(subject) = request.subject {
        entry = entry.subject(subject);
    }
    state
        .repos
        .identity
        .record_audit(entry)
        .await
        .map_err(|err| Refusal::unavailable(err.to_string()))?;
    Ok(json!({ "recorded": true }))
}

/// A plugin reports its own work stopping, resuming or failing. Loading and unloading are phases
/// the backend runs, so a plugin cannot claim to be in them.
pub async fn status(
    state: &AppState,
    plugin: &str,
    request: StatusRequest,
) -> Result<Value, Refusal> {
    match request.state {
        PluginState::Running | PluginState::Cancelled | PluginState::Error => {
            let now =
                plugins::transition(state, plugin, request.state, request.error.as_deref()).await?;
            Ok(json!({ "state": now }))
        }
        PluginState::Loading | PluginState::Unloading => {
            Err(Refusal::forbidden("loading and unloading are the backend's to set"))
        }
    }
}

/// Whether the plugin holds one of `capabilities`, which the operator allowed it (DOC-SPEC §4.2).
async fn capable(
    state: &AppState,
    plugin: &str,
    capabilities: &[Capability],
) -> Result<(), Refusal> {
    let entry = state.plugins.get(plugin).await.ok_or_else(|| {
        Refusal::new(404, "not-registered", format!("{plugin} is not registered"))
    })?;
    match capabilities.iter().any(|capability| entry.manifest.capabilities.contains(capability)) {
        true => Ok(()),
        false => Err(Refusal::forbidden(match capabilities {
            [Capability::TeamProvider] => format!("{plugin} is not a team provider"),
            [Capability::TeamWriter] => format!("{plugin} does not write teams"),
            [Capability::TokenIssuer] => format!("{plugin} does not issue tokens"),
            [Capability::SecretStore] => format!("{plugin} does not keep secrets"),
            _ => format!("{plugin} is not an identity provider"),
        })),
    }
}

/// With the `token-issuer` capability: a scoped token for the person this call is for, which the
/// plugin hands to their agent. Only during a call a person made, never through a scoped token.
pub async fn scoped_token(
    state: &AppState,
    plugin: &str,
    context: Option<&str>,
    request: ScopedTokenRequest,
) -> Result<ScopedTokenResponse, Refusal> {
    capable(state, plugin, &[Capability::TokenIssuer]).await?;
    let principal = acting_for(state, plugin, context)
        .filter(|principal| matches!(principal, Principal::User(_)))
        .ok_or_else(|| {
            Refusal::forbidden("a scoped token is minted for a person, during their call")
        })?;
    let asked = crate::scoped::NewScopedToken {
        name: request.name,
        scopes: request.scopes,
        expires_in_minutes: request.expires_in_minutes,
    };
    let minted =
        crate::scoped::mint(state, &principal, &asked, Some(plugin)).await.map_err(|problem| {
            let detail = problem.detail_text().unwrap_or(&problem.title).to_string();
            Refusal::new(problem.status.as_u16(), problem.kind, detail)
        })?;
    Ok(ScopedTokenResponse {
        id: minted.token.id,
        token: minted.secret,
        scopes: minted.token.scopes.clone().unwrap_or_default(),
        expires_at: minted.token.expires_at.unwrap_or_else(chrono::Utc::now),
    })
}

/// Revokes a scoped token the plugin minted, for whomever it minted it.
pub async fn revoke_scoped_token(
    state: &AppState,
    plugin: &str,
    request: RevokeScopedTokenRequest,
) -> Result<RevokeScopedTokenResponse, Refusal> {
    capable(state, plugin, &[Capability::TokenIssuer]).await?;
    let hash = state
        .repos
        .identity
        .revoke_issued(request.id, plugin)
        .await
        .map_err(|err| Refusal::unavailable(err.to_string()))?;
    let revoked = hash.is_some();
    if let Some(hash) = hash {
        crate::auth::forget(state, &hash).await;
        let entry = AuditEntry::new("auth.token.scoped.revoked")
            .by(&Principal::Plugin { id: plugin.to_string() })
            .subject(request.id.to_string());
        let _ = state.repos.identity.record_audit(entry).await;
    }
    Ok(RevokeScopedTokenResponse { revoked })
}

/// What a plugin's own sealed values are bound to beside its ID. No setting's can collide with it,
/// since a setting's key has no colon and a named credential's starts `named:`.
fn sealed_label(label: &str) -> Result<String, Refusal> {
    let label = label.trim();
    match label.is_empty() || label.chars().count() > 200 {
        true => {
            Err(Refusal::bad("a label names what the value belongs to, in 1 to 200 characters"))
        }
        false => Ok(format!("sealed:{label}")),
    }
}

fn base64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unbase64(text: &str) -> Result<Vec<u8>, Refusal> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD
        .decode(text.as_bytes())
        .map_err(|_| Refusal::bad("that is not a value this platform sealed"))
}

/// With the `secret-store` capability: a value sealed under the settings key, bound to the plugin
/// and to the label naming its record (FEAT-SECRETS). The key never leaves core.
pub async fn seal(
    state: &AppState,
    plugin: &str,
    request: SealRequest,
) -> Result<SealResponse, Refusal> {
    capable(state, plugin, &[Capability::SecretStore]).await?;
    let label = sealed_label(&request.label)?;
    if !state.settings_keys.available() {
        return Err(Refusal::unavailable(
            "this platform has no settings key yet, so nothing can be sealed: run the bootstrap",
        ));
    }
    let sealed = state
        .settings_keys
        .seal(plugin, &label, request.value.expose())
        .map_err(|err| Refusal::unavailable(err.to_string()))?;
    Ok(SealResponse {
        sealed: SealedValue {
            key_id: sealed.key_id,
            nonce: base64(&sealed.nonce),
            ciphertext: base64(&sealed.ciphertext),
        },
    })
}

/// With the `secret-store` capability: a value `seal` made, opened for the plugin that sealed it
/// under the label it was sealed with, and whether a newer key should seal it again.
pub async fn open(
    state: &AppState,
    plugin: &str,
    request: OpenRequest,
) -> Result<OpenResponse, Refusal> {
    capable(state, plugin, &[Capability::SecretStore]).await?;
    let label = sealed_label(&request.label)?;
    let sealed = crate::secrets::Sealed {
        key_id: request.sealed.key_id.clone(),
        nonce: unbase64(&request.sealed.nonce)?,
        ciphertext: unbase64(&request.sealed.ciphertext)?,
    };
    let value = state.settings_keys.open(plugin, &label, &sealed).map_err(|err| match err {
        crate::secrets::SealError::UnknownKey(_) => Refusal::new(409, "conflict", err.to_string()),
        crate::secrets::SealError::NoKey(_) => Refusal::unavailable(err.to_string()),
        _ => Refusal::bad("that value does not open here: it was sealed for something else"),
    })?;
    let stale = state.settings_keys.current_id().is_some_and(|current| current != sealed.key_id);
    Ok(OpenResponse { value: doc_plugin_protocol::Secret::new(value), stale })
}

/// With the `secret-store` capability: some of its secrets changed, so every plugin whose settings
/// point at one is told to read them again.
pub async fn secrets_changed(
    state: &AppState,
    plugin: &str,
    request: SecretsChangedRequest,
) -> Result<SecretsChangedResponse, Refusal> {
    capable(state, plugin, &[Capability::SecretStore]).await?;
    let told = settings::secrets_changed(state, &request.secrets, request.loaded).await;
    Ok(SecretsChangedResponse { told })
}

/// With the `team-writer` capability: somebody added by email address for the person the call is
/// for, who is held to exactly what `POST /api/v1/people` holds them to. Never for the plugin
/// itself, since adding people is a decision somebody has to be accountable for.
pub async fn add_person(
    state: &AppState,
    plugin: &str,
    context: Option<&str>,
    request: PersonRequest,
) -> Result<Value, Refusal> {
    capable(state, plugin, &[Capability::TeamWriter]).await?;
    let by = acting_for(state, plugin, context)
        .filter(|who| !matches!(who, Principal::Plugin { .. }))
        .ok_or_else(|| {
            Refusal::forbidden("somebody is added for whoever is asking, never for the plugin")
        })?;
    let asked = crate::api::people::NewPerson {
        email: request.email,
        name: request.name,
        team: request.team,
        organisation: request.organisation,
        login: request.login,
    };
    let (_, answer) = crate::api::people::add(state, &by, asked).await.map_err(|problem| {
        let detail = problem.detail_text().unwrap_or(&problem.title).to_string();
        Refusal::new(problem.status.as_u16(), problem.kind, detail)
    })?;
    Ok(answer)
}

/// Identity providers only, and only for accounts of their own, so one provider cannot sign in,
/// link or make anyone's account with another.
async fn own_account(
    state: &AppState,
    plugin: &str,
    provider: &str,
    external_id: &str,
    login: &str,
) -> Result<Account, Refusal> {
    own_account_as(state, plugin, &[Capability::IdentityProvider], provider, external_id, login)
        .await
}

async fn own_account_as(
    state: &AppState,
    plugin: &str,
    capabilities: &[Capability],
    provider: &str,
    external_id: &str,
    login: &str,
) -> Result<Account, Refusal> {
    capable(state, plugin, capabilities).await?;
    if provider != plugin {
        return Err(Refusal::forbidden(format!("{plugin} may only speak for its own accounts")));
    }
    if external_id.is_empty() || login.is_empty() {
        return Err(Refusal::bad("an account needs an external ID and a login"));
    }
    Ok(Account { provider: provider.into(), external_id: external_id.into(), login: login.into() })
}

fn refusal(problem: &crate::api::problem::Problem) -> Refusal {
    let detail = problem.detail_text().unwrap_or(&problem.title).to_string();
    Refusal::new(problem.status.as_u16(), problem.kind, detail)
}

/// The organisation that signs people in with `provider`, which each provider serves one of.
async fn organisation_of(state: &AppState, provider: &str) -> Result<Option<Uuid>, Refusal> {
    let chosen = state.repos.teams.providers().await.map_err(|err| unavailable(&err))?;
    Ok(chosen
        .into_iter()
        .find(|(chosen, _)| chosen == provider)
        .map(|(_, organisation)| organisation))
}

/// Signs in whoever the account belongs to, found by its immutable ID alone, or a new user with it
/// in the organisation that signs in with this provider. Nobody signs in with another
/// organisation's providers (ADR-0005).
pub async fn identity(
    state: &AppState,
    plugin: &str,
    request: IdentityRequest,
) -> Result<IdentityResponse, Refusal> {
    let account =
        own_account(state, plugin, &request.provider, &request.external_id, &request.login).await?;
    let held = state
        .repos
        .identity
        .identity(&account.provider, &account.external_id)
        .await
        .map_err(|err| unavailable(&err))?;
    let holder = match &held {
        Some(held) => {
            state.repos.identity.user_by_id(held.user_id).await.map_err(|err| unavailable(&err))?
        }
        None => None,
    };
    // A DOC password somebody was given when they were added works whatever providers their
    // organisation chose, so they can sign in before its SSO is set up (FEAT-PEOPLE). Everything
    // else signs in only with its own organisation's providers (ADR-0005).
    let organisation = match (&holder, plugin == crate::api::people::PASSWORDS) {
        (Some(holder), true) => holder.organisation_id,
        _ => organisation_of(state, plugin)
            .await?
            .ok_or_else(|| Refusal::forbidden(format!("no organisation signs in with {plugin}")))?,
    };
    let foreign = || {
        Refusal::forbidden(format!(
            "that account's person belongs to another organisation, and signs in with its own \
             providers, not {plugin}"
        ))
    };
    if holder.is_some_and(|holder| holder.organisation_id != organisation) {
        return Err(foreign());
    }
    let membership = Membership { organisations: request.organisations, teams: request.teams };
    let profile = Profile {
        name: request.name,
        email: request.email,
        first_name: request.first_name,
        surname: request.surname,
    };
    let (user, _) = state
        .repos
        .identity
        .user_for_account(&account, &profile, Arrival::SignIn, organisation)
        .await
        .map_err(|err| unavailable(&err))?;
    if user.organisation_id != organisation {
        return Err(foreign());
    }
    if user.disabled {
        return Err(Refusal::forbidden("this account is disabled"));
    }
    let started = start_session(state, &user, plugin, &membership)
        .await
        .map_err(|problem| refusal(&problem))?;
    Ok(IdentityResponse {
        user_id: user.id,
        session_token: started.secret,
        expires_at: started.expires_at,
        first: started.first,
    })
}

/// Links the account to whoever asked core for the ticket, for the provider the ticket was for.
pub async fn link(
    state: &AppState,
    plugin: &str,
    request: LinkRequest,
) -> Result<LinkResponse, Refusal> {
    let account =
        own_account(state, plugin, &request.provider, &request.external_id, &request.login).await?;
    let wrong = || Refusal::new(403, "wrong-ticket", "this link was not started here, or was used");
    let ticket = crate::api::users::redeem_ticket(state, request.ticket.expose())
        .await
        .map_err(Refusal::unavailable)?
        .ok_or_else(wrong)?;
    if ticket.provider != plugin {
        return Err(wrong());
    }
    let user = state
        .repos
        .identity
        .user_by_id(ticket.user)
        .await
        .map_err(|err| Refusal::unavailable(err.to_string()))?
        .ok_or_else(|| Refusal::new(404, "not-found", "the user this link was for is gone"))?;
    if user.disabled {
        return Err(Refusal::forbidden("this account is disabled"));
    }
    let by = Principal::User(user.clone());
    let linked = crate::api::users::link_account(state, &user, &account, "link", &by)
        .await
        .map_err(|problem| refusal(&problem))?;
    Ok(LinkResponse { user_id: user.id, merged: linked.merged.map(|merged| merged.id) })
}

/// The user an account of the provider's belongs to, made with it if nobody has it yet, so a
/// directory's members can be given access before they first sign in.
pub async fn users(
    state: &AppState,
    plugin: &str,
    request: UserRequest,
) -> Result<UserResponse, Refusal> {
    let providers = [Capability::IdentityProvider, Capability::TeamProvider];
    let account = own_account_as(
        state,
        plugin,
        &providers,
        &request.provider,
        &request.external_id,
        &request.login,
    )
    .await?;
    let organisation = match &request.organisation {
        Some(name) => {
            let found = state
                .repos
                .teams
                .organisation_named(name)
                .await
                .map_err(|err| unavailable(&err))?;
            found.map(|organisation| organisation.id).ok_or_else(|| {
                Refusal::new(404, "not-found", format!("there is no organisation called {name}"))
            })?
        }
        None => organisation_of(state, plugin).await?.ok_or_else(|| {
            let detail = format!(
                "no organisation signs in with {plugin}, so name the organisation its people join"
            );
            Refusal::new(409, "conflict", detail)
        })?,
    };
    let profile = Profile {
        name: request.name,
        email: request.email,
        first_name: request.first_name,
        surname: request.surname,
    };
    let (user, created) = state
        .repos
        .identity
        .user_for_account(&account, &profile, Arrival::Provider, organisation)
        .await
        .map_err(|err| unavailable(&err))?;
    if created {
        let by = Principal::Plugin { id: plugin.to_string() };
        let entry = AuditEntry::new("iam.user.created")
            .by(&by)
            .subject(user.id.to_string())
            .detail(json!({ "login": user.login, "account": plugin }));
        let _ = state.repos.identity.record_audit(entry).await;
        let event = json!({ "id": user.id, "login": user.login });
        crate::api::iam::announce(state, "user.created", event).await;
    }
    Ok(UserResponse { user_id: user.id, created })
}

fn unavailable(err: &crate::db::repositories::RepositoryError) -> Refusal {
    match err {
        crate::db::repositories::RepositoryError::Conflict(detail) => {
            Refusal::new(409, "conflict", detail.clone())
        }
        other => Refusal::unavailable(other.to_string()),
    }
}

/// One of the provider's teams, by its key there.
async fn provided(state: &AppState, plugin: &str, external_id: &str) -> Result<Team, Refusal> {
    state
        .repos
        .teams
        .provided_team(plugin, external_id)
        .await
        .map_err(|err| unavailable(&err))?
        .ok_or_else(|| {
            Refusal::new(404, "not-found", format!("{plugin} has no team keyed {external_id}"))
        })
}

fn checked(result: Result<String, crate::api::problem::Problem>) -> Result<String, Refusal> {
    result.map_err(|problem| refusal(&problem))
}

/// Team providers only: makes or brings up to date one of the provider's own teams, in an
/// organisation that already exists, below another of its own teams if it names a parent.
pub async fn teams(
    state: &AppState,
    plugin: &str,
    request: TeamRequest,
) -> Result<TeamResponse, Refusal> {
    capable(state, plugin, &[Capability::TeamProvider]).await?;
    let key = request.external_id.trim().to_string();
    if key.is_empty() || key.len() > 256 {
        return Err(Refusal::bad("a team needs its provider's key, of at most 256 bytes"));
    }
    let name = checked(crate::api::teams::slug(&request.name, "a name"))?;
    let title = checked(crate::api::teams::title(&request.title))?;
    let description = checked(crate::api::teams::description(&request.description))?;
    // Naming no organisation means the one that signs in with this provider, as making a user
    // does (§9.9), so a provider of both needs no second name for the same organisation.
    let organisation = match request.organisation.trim() {
        "" => {
            let id = organisation_of(state, plugin).await?.ok_or_else(|| {
                let detail = format!(
                    "no organisation signs in with {plugin}, so name the organisation its teams are in"
                );
                Refusal::new(409, "conflict", detail)
            })?;
            state
                .repos
                .teams
                .organisation(id)
                .await
                .map_err(|err| unavailable(&err))?
                .ok_or_else(|| Refusal::new(404, "not-found", "that organisation is gone"))?
        }
        named => state
            .repos
            .teams
            .organisation_named(named)
            .await
            .map_err(|err| unavailable(&err))?
            .ok_or_else(|| {
                let detail = format!("there is no organisation called {named}");
                Refusal::new(404, "not-found", detail)
            })?,
    };
    let parent = match &request.parent {
        Some(parent) => Some(provided(state, plugin, parent).await?),
        None => None,
    };
    if parent.as_ref().is_some_and(|parent| parent.organisation_id != organisation.id) {
        return Err(Refusal::new(409, "conflict", "a sub-team is in its parent's organisation"));
    }
    let parent_id = parent.map(|parent| parent.id);
    let by = Principal::Plugin { id: plugin.to_string() };
    let existing =
        state.repos.teams.provided_team(plugin, &key).await.map_err(|err| unavailable(&err))?;
    if let Some(existing) = existing {
        if existing.organisation_id != organisation.id {
            let detail = "that team is in another organisation, and teams do not move between them";
            return Err(Refusal::new(409, "conflict", detail));
        }
        let unchanged = existing.name == name
            && existing.title == title
            && existing.description == description
            && existing.parent_id == parent_id;
        if !unchanged {
            let changes = crate::teams::TeamChanges {
                name: Some(name),
                title: Some(title),
                description: Some(description),
                email: None,
                parent: Some(parent_id),
                is_default: None,
            };
            let updated = state
                .repos
                .teams
                .update_team(existing.id, &changes)
                .await
                .map_err(|err| unavailable(&err))?;
            if let Some(updated) = updated {
                let detail = json!({ "id": updated.id, "name": updated.name, "provider": plugin });
                crate::api::teams::announce(state, "team.changed", detail).await;
            }
        }
        return Ok(TeamResponse { team_id: existing.id, created: false });
    }
    let new = crate::teams::NewTeam {
        organisation_id: organisation.id,
        parent_id,
        name,
        title,
        description,
        email: String::new(),
        is_default: false,
        provided: Some((plugin.to_string(), key)),
    };
    let made = state.repos.teams.create_team(&new).await.map_err(|err| unavailable(&err))?;
    let entry = AuditEntry::new("team.created")
        .by(&by)
        .subject(made.id.to_string())
        .detail(json!({ "name": made.name, "organisation": organisation.name }));
    let _ = state.repos.identity.record_audit(entry).await;
    let event = json!({ "id": made.id, "name": made.name, "organisation": organisation.id });
    crate::api::teams::announce(state, "team.created", event).await;
    Ok(TeamResponse { team_id: made.id, created: true })
}

/// The person a `team-writer` plugin is acting for, if it is acting for anybody. Teams decide who
/// holds what, so a plugin may only make them for somebody who administers identity themselves;
/// with nobody asking, the plugin writes as itself, which is how it moves records it kept before
/// into core (T69).
async fn writing_teams(
    state: &AppState,
    plugin: &str,
    context: Option<&str>,
) -> Result<Principal, Refusal> {
    capable(state, plugin, &[Capability::TeamWriter]).await?;
    // A plugin's own calls — loading, a schedule, moving what it kept into core — are the
    // plugin's; only a person or a service account asking through it is held to what they may do.
    let acting = acting_for(state, plugin, context);
    let Some(principal) = acting.filter(|who| !matches!(who, Principal::Plugin { .. })) else {
        return Ok(Principal::Plugin { id: plugin.to_string() });
    };
    match crate::api::iam::manages_identity(state, &principal).await {
        true => Ok(principal),
        false => Err(Refusal::forbidden(
            "organisations and teams are changed by whoever administers identity",
        )),
    }
}

/// With the `team-writer` capability: an organisation made in DOC, by name, made if there is none.
pub async fn write_organisation(
    state: &AppState,
    plugin: &str,
    context: Option<&str>,
    request: OrganisationRequest,
) -> Result<OrganisationResponse, Refusal> {
    let by = writing_teams(state, plugin, context).await?;
    let name = checked(crate::api::teams::slug(&request.name, "a name"))?;
    let title = checked(crate::api::teams::title(&request.title))?;
    let description = checked(crate::api::teams::description(&request.description))?;
    let existing =
        state.repos.teams.organisation_named(&name).await.map_err(|err| unavailable(&err))?;
    if let Some(existing) = existing {
        if existing.title != title || existing.description != description {
            let changed = state
                .repos
                .teams
                .update_organisation(existing.id, &name, &title, &description)
                .await
                .map_err(|err| unavailable(&err))?;
            if let Some(changed) = changed {
                let detail = json!({ "id": changed.id, "name": changed.name, "by": plugin });
                crate::api::teams::announce(state, "organisation.changed", detail).await;
            }
        }
        return Ok(OrganisationResponse { organisation_id: existing.id, created: false });
    }
    let made = state
        .repos
        .teams
        .create_organisation(&name, &title, &description)
        .await
        .map_err(|err| unavailable(&err))?;
    let entry = AuditEntry::new("organisation.created")
        .by(&by)
        .subject(made.id.to_string())
        .detail(json!({ "name": made.name, "plugin": plugin }));
    let _ = state.repos.identity.record_audit(entry).await;
    crate::api::teams::announce(
        state,
        "organisation.created",
        json!({ "id": made.id, "name": made.name }),
    )
    .await;
    Ok(OrganisationResponse { organisation_id: made.id, created: true })
}

/// With the `team-writer` capability: a team made in DOC, by name in its organisation. A team a
/// provider keeps stays as the provider has it; only its address is written here.
pub async fn write_team(
    state: &AppState,
    plugin: &str,
    context: Option<&str>,
    request: WriteTeamRequest,
) -> Result<TeamResponse, Refusal> {
    let by = writing_teams(state, plugin, context).await?;
    let name = checked(crate::api::teams::slug(&request.name, "a name"))?;
    let title = checked(crate::api::teams::title(&request.title))?;
    let description = checked(crate::api::teams::description(&request.description))?;
    let email = checked(crate::api::teams::contact(&request.email))?;
    let teams = state.repos.teams.teams().await.map_err(|err| unavailable(&err))?;
    // A team that names no organisation is the one of that name there already, wherever it is:
    // a plugin bringing a team up to date should not have to know which organisation it joined.
    let named = request.organisation.trim();
    let found = match named.is_empty() {
        true => {
            let same: Vec<&Team> = teams.iter().filter(|team| team.name == name).collect();
            match same.as_slice() {
                [] => None,
                [only] => Some((*only).clone()),
                _ => {
                    let detail =
                        format!("more than one team is called {name}, so name the organisation");
                    return Err(Refusal::new(409, "conflict", detail));
                }
            }
        }
        false => {
            let organisation = in_organisation(state, plugin, named).await?;
            teams
                .iter()
                .find(|team| team.organisation_id == organisation && team.name == name)
                .cloned()
        }
    };
    let organisation = match &found {
        Some(team) => team.organisation_id,
        None => in_organisation(state, plugin, named).await?,
    };
    let here = |wanted: &str| {
        teams
            .iter()
            .find(|team| team.organisation_id == organisation && team.name == wanted)
            .cloned()
    };
    let parent = match request.parent.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
        Some(wanted) => Some(here(wanted).ok_or_else(|| {
            Refusal::new(404, "not-found", format!("there is no team called {wanted} to sit in"))
        })?),
        None => None,
    };
    let parent_id = parent.map(|parent| parent.id);
    if let Some(existing) = found {
        let provided = existing.provider.is_some();
        let changes = crate::teams::TeamChanges {
            name: None,
            title: (!provided && existing.title != title).then(|| title.clone()),
            description: (!provided && existing.description != description)
                .then(|| description.clone()),
            email: (existing.email != email).then(|| email.clone()),
            parent: (!provided && parent_id.is_some() && existing.parent_id != parent_id)
                .then_some(parent_id),
            is_default: None,
        };
        let changed = changes.title.is_some()
            || changes.description.is_some()
            || changes.email.is_some()
            || changes.parent.is_some();
        if changed {
            let updated = state
                .repos
                .teams
                .update_team(existing.id, &changes)
                .await
                .map_err(|err| unavailable(&err))?;
            if let Some(updated) = updated {
                let entry = AuditEntry::new("team.changed")
                    .by(&by)
                    .subject(updated.id.to_string())
                    .detail(json!({ "name": updated.name, "plugin": plugin }));
                let _ = state.repos.identity.record_audit(entry).await;
                let detail = json!({ "id": updated.id, "name": updated.name, "by": plugin });
                crate::api::teams::announce(state, "team.changed", detail).await;
            }
        }
        return Ok(TeamResponse { team_id: existing.id, created: false });
    }
    let new = crate::teams::NewTeam {
        organisation_id: organisation,
        parent_id,
        name,
        title,
        description,
        email,
        is_default: false,
        provided: None,
    };
    let made = state.repos.teams.create_team(&new).await.map_err(|err| unavailable(&err))?;
    let entry = AuditEntry::new("team.created")
        .by(&by)
        .subject(made.id.to_string())
        .detail(json!({ "name": made.name, "organisation": organisation, "plugin": plugin }));
    let _ = state.repos.identity.record_audit(entry).await;
    let event = json!({ "id": made.id, "name": made.name, "organisation": organisation });
    crate::api::teams::announce(state, "team.created", event).await;
    Ok(TeamResponse { team_id: made.id, created: true })
}

/// The organisation a written team is in: the one named, else the one that signs in with the
/// plugin, else the only one there is.
async fn in_organisation(state: &AppState, plugin: &str, named: &str) -> Result<Uuid, Refusal> {
    if !named.trim().is_empty() {
        let found = state
            .repos
            .teams
            .organisation_named(named.trim())
            .await
            .map_err(|err| unavailable(&err))?;
        return found.map(|organisation| organisation.id).ok_or_else(|| {
            Refusal::new(404, "not-found", format!("there is no organisation called {named}"))
        });
    }
    if let Some(organisation) = organisation_of(state, plugin).await? {
        return Ok(organisation);
    }
    let organisations = state.repos.teams.organisations().await.map_err(|err| unavailable(&err))?;
    match organisations.as_slice() {
        [only] => Ok(only.id),
        _ => Err(Refusal::new(
            409,
            "conflict",
            "there is more than one organisation, so name the one it is in",
        )),
    }
}

/// Identity providers only: somebody is gone from the directory this plugin speaks for. Core does
/// nothing to them here beyond saying so — what follows is for the offboarding rules to decide,
/// which is why this only finds the person and announces it (T67).
pub async fn deprovision(
    state: &AppState,
    plugin: &str,
    request: DeprovisionRequest,
) -> Result<DeprovisionResponse, Refusal> {
    capable(state, plugin, &[Capability::IdentityProvider]).await?;
    let found = state
        .repos
        .identity
        .identity(plugin, &request.external_id)
        .await
        .map_err(|err| unavailable(&err))?;
    // A directory may speak of people this platform never knew, which is not an error.
    let Some(identity) = found else { return Ok(DeprovisionResponse::default()) };
    let user =
        state.repos.identity.user_by_id(identity.user_id).await.map_err(|err| unavailable(&err))?;
    let login = user.map(|user| user.login);
    let by = Principal::Plugin { id: plugin.to_string() };
    let detail = json!({
        "provider": plugin,
        "external_id": request.external_id,
        "reason": request.reason,
        "login": login,
    });
    let entry = AuditEntry::new("iam.user.deprovisioned")
        .by(&by)
        .subject(identity.user_id.to_string())
        .detail(detail.clone());
    let _ = state.repos.identity.record_audit(entry).await;
    let mut event = detail;
    if let Some(object) = event.as_object_mut() {
        object.insert("user".into(), json!(identity.user_id));
    }
    crate::api::teams::announce(state, "iam.user.deprovisioned", event).await;
    Ok(DeprovisionResponse { user_id: Some(identity.user_id), login })
}

/// What an offboarding rule decided, carried out: each part only if it was asked for, each one
/// audited, and the cached permission decisions dropped so nothing it took away outlives it.
pub async fn offboard(
    state: &AppState,
    plugin: &str,
    request: OffboardRequest,
) -> Result<OffboardResponse, Refusal> {
    capable(state, plugin, &[Capability::Offboarding]).await?;
    let by = Principal::Plugin { id: plugin.to_string() };
    let user = state
        .repos
        .identity
        .user_by_id(request.user)
        .await
        .map_err(|err| unavailable(&err))?
        .ok_or_else(|| Refusal::new(404, "not-found", "there is no such user"))?;
    let mut done = OffboardResponse::default();

    if request.disable {
        let changed = state
            .repos
            .identity
            .set_user_disabled(user.id, true)
            .await
            .map_err(|err| unavailable(&err))?;
        done.disabled = changed.is_some_and(|user| user.disabled);
    }

    if let Some(provider) = request.remove_identity.as_deref() {
        let held = state
            .repos
            .identity
            .identities(Some(user.id))
            .await
            .map_err(|err| unavailable(&err))?;
        for identity in held.iter().filter(|identity| identity.provider == provider) {
            let gone = state
                .repos
                .identity
                .detach_identity(user.id, identity.id)
                .await
                .map_err(|err| unavailable(&err))?;
            done.identities_removed += usize::from(gone.is_some());
        }
    }

    let memberships =
        state.repos.teams.memberships(user.id).await.map_err(|err| unavailable(&err))?;
    for membership in &memberships {
        let by_provider = request
            .remove_provided_memberships
            .as_deref()
            .is_some_and(|provider| membership.provider.as_deref() == Some(provider));
        if !(request.remove_memberships || by_provider) {
            continue;
        }
        let gone = state
            .repos
            .teams
            .remove_member(membership.team_id, user.id)
            .await
            .map_err(|err| unavailable(&err))?;
        done.memberships_removed += usize::from(gone.is_some());
    }

    if request.revoke_tokens {
        let owner = TokenOwner::User(user.id);
        for kind in [crate::secrets::TokenKind::Session, crate::secrets::TokenKind::Personal] {
            let tokens = state
                .repos
                .identity
                .list_tokens(&owner, kind)
                .await
                .map_err(|err| unavailable(&err))?;
            for token in tokens.iter().filter(|token| token.revoked_at.is_none()) {
                let hash = state
                    .repos
                    .identity
                    .revoke_token(token.id, &owner)
                    .await
                    .map_err(|err| unavailable(&err))?;
                if let Some(hash) = hash {
                    crate::auth::forget(state, &hash).await;
                    done.tokens_revoked += 1;
                }
            }
        }
    }

    let detail = json!({
        "login": user.login,
        "reason": request.reason,
        "disabled": done.disabled,
        "identities_removed": done.identities_removed,
        "memberships_removed": done.memberships_removed,
        "tokens_revoked": done.tokens_revoked,
    });
    let entry = AuditEntry::new("iam.user.offboarded")
        .by(&by)
        .subject(user.id.to_string())
        .detail(detail.clone());
    let _ = state.repos.identity.record_audit(entry).await;
    // What they held is no longer theirs, so nothing cached may say otherwise.
    crate::permissions::forget_all(state).await;
    let mut event = detail;
    if let Some(object) = event.as_object_mut() {
        object.insert("user".into(), json!(user.id));
    }
    crate::api::teams::announce(state, "iam.user.offboarded", event).await;
    Ok(done)
}

/// Team providers only: who is in one of its teams. Only the memberships it made change.
pub async fn team_members(
    state: &AppState,
    plugin: &str,
    request: TeamMembersRequest,
) -> Result<TeamMembersResponse, Refusal> {
    capable(state, plugin, &[Capability::TeamProvider]).await?;
    let team = provided(state, plugin, &request.external_id).await?;
    let (added, removed) = state
        .repos
        .teams
        .set_provided_members(team.id, plugin, &request.users)
        .await
        .map_err(|err| unavailable(&err))?;
    if !added.is_empty() || !removed.is_empty() {
        let members = json!({ "added": added, "removed": removed });
        let entry = AuditEntry::new("team.members.synced")
            .by(&Principal::Plugin { id: plugin.to_string() })
            .subject(team.id.to_string())
            .detail(members.clone());
        let _ = state.repos.identity.record_audit(entry).await;
        let event = json!({ "id": team.id, "members": members });
        crate::api::teams::announce(state, "team.changed", event).await;
    }
    Ok(TeamMembersResponse { added, removed })
}

/// Team providers only: a team it no longer has. Its memberships go; the team goes too unless
/// something made in DOC depends on it, in which case it stays as a team made in DOC.
pub async fn team_remove(
    state: &AppState,
    plugin: &str,
    request: TeamRemoveRequest,
) -> Result<TeamRemoveResponse, Refusal> {
    capable(state, plugin, &[Capability::TeamProvider]).await?;
    let found = state
        .repos
        .teams
        .provided_team(plugin, &request.external_id)
        .await
        .map_err(|err| unavailable(&err))?;
    let Some(team) = found else { return Ok(TeamRemoveResponse::default()) };
    let teams = state.repos.teams.teams().await.map_err(|err| unavailable(&err))?;
    let below: Vec<&Team> = teams.iter().filter(|child| child.parent_id == Some(team.id)).collect();
    if below.iter().any(|child| child.provider.as_deref() == Some(plugin)) {
        let detail = "remove its sub-teams first";
        return Err(Refusal::new(409, "conflict", detail));
    }
    state
        .repos
        .teams
        .set_provided_members(team.id, plugin, &[])
        .await
        .map_err(|err| unavailable(&err))?;
    let staying =
        state.repos.teams.members(Some(team.id)).await.map_err(|err| unavailable(&err))?;
    let owners = crate::teams::Owners { user: Uuid::nil(), teams: vec![team.id] };
    let accounts = state
        .repos
        .identity
        .list_service_accounts(Some(&owners))
        .await
        .map_err(|err| unavailable(&err))?;
    let by = Principal::Plugin { id: plugin.to_string() };
    if !staying.is_empty() || !below.is_empty() || !accounts.is_empty() || team.is_default {
        state.repos.teams.release_team(team.id).await.map_err(|err| unavailable(&err))?;
        let entry = AuditEntry::new("team.released")
            .by(&by)
            .subject(team.id.to_string())
            .detail(json!({ "name": team.name }));
        let _ = state.repos.identity.record_audit(entry).await;
        let event = json!({ "id": team.id, "name": team.name, "provider": Value::Null });
        crate::api::teams::announce(state, "team.changed", event).await;
        return Ok(TeamRemoveResponse { removed: false, released: true });
    }
    state.repos.teams.delete_team(team.id).await.map_err(|err| unavailable(&err))?;
    let entry = AuditEntry::new("team.deleted")
        .by(&by)
        .subject(team.id.to_string())
        .detail(json!({ "name": team.name }));
    let _ = state.repos.identity.record_audit(entry).await;
    crate::api::teams::announce(state, "team.deleted", json!({ "id": team.id, "name": team.name }))
        .await;
    Ok(TeamRemoveResponse { removed: true, released: false })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use doc_eventbus::{ConsumerGroup, TopicFilter};
    use doc_plugin_protocol::{Manifest, RegisterRequest};
    use doc_servicebus::{Request, ServiceHandler};

    use super::*;
    use crate::config::Config;
    use crate::testing::{Host, plugin_host, plugin_host_with};

    const ADA_TOKEN: &str = "doc_ses_ada";

    const DAY: Duration = Duration::from_secs(86_400);

    fn manifest(id: &str) -> Manifest {
        Manifest { id: id.into(), version: "1.0.0".into(), ..Manifest::default() }
    }

    async fn running(host: &Host, manifest: Manifest) {
        let id = manifest.id.clone();
        let principal = Principal::Plugin { id: id.clone() };
        let request = RegisterRequest {
            manifest,
            address: format!("plugin-{id}:4440"),
            binary_sha256: "a".repeat(64),
            started_at: None,
        };
        plugins::register(&host.state, &principal, request).await.expect("registered");
        assert_eq!(host.settle(&id).await, Some(PluginState::Running));
    }

    fn published(topic: &str) -> PublishRequest {
        PublishRequest { topic: topic.into(), payload: json!({ "n": 1 }), idempotency_key: None }
    }

    fn ask(address: &str, queue: bool) -> ServiceRequest {
        ServiceRequest {
            address: address.into(),
            subject: "ping".into(),
            payload: json!({}),
            deadline_ms: Some(1_000),
            queue,
            guard: None,
        }
    }

    struct Echo;

    #[async_trait]
    impl ServiceHandler for Echo {
        async fn handle(&self, request: Request) -> Result<Value, String> {
            Ok(json!({ "principal": request.principal }))
        }
    }

    #[tokio::test]
    async fn registration_gives_a_plugin_its_own_cache() {
        let host = plugin_host();
        running(&host, manifest("hello")).await;
        let set = CacheRequest::Set { key: "k".into(), value: json!(1), ttl_ms: None };
        assert!(cache(&host.state, "hello", set).await.expect("its namespace exists").applied);
    }

    #[tokio::test]
    async fn a_plugin_publishes_only_under_its_own_prefix() {
        let host = plugin_host();
        publish(&host.state, "hello", published("plugin.hello.greeted")).await.expect("its own");
        for topic in [
            "platform.backend.started",
            "plugin.kb.greeted",
            "plugin.hellox.greeted",
            "plugin.hello",
            "plugin.hello.",
        ] {
            let refusal = publish(&host.state, "hello", published(topic)).await.expect_err(topic);
            assert_eq!(refusal.status, 403, "{topic}");
        }

        let filter = TopicFilter::new("plugin.>").expect("filter");
        let mut seen =
            host.state.buses.events.subscribe(ConsumerGroup::new("t", filter)).await.unwrap();
        let delivery = seen.next().await.expect("the one that was allowed");
        assert_eq!(delivery.event.topic.as_str(), "plugin.hello.greeted");
        assert_eq!(delivery.event.source, "plugin.hello");
    }

    /// Keys are global on the bus. Scoped per plugin, one plugin claiming a key first cannot turn
    /// another's publish into a silent duplicate.
    #[tokio::test]
    async fn one_plugins_idempotency_key_cannot_swallow_anothers_event() {
        let host = plugin_host();
        let keyed = |topic: &str| PublishRequest {
            idempotency_key: Some("nightly".into()),
            ..published(topic)
        };
        let first = publish(&host.state, "rbac", keyed("plugin.rbac.synced")).await.unwrap();
        let second = publish(&host.state, "hello", keyed("plugin.hello.synced")).await.unwrap();
        assert_ne!(first.id, second.id);
        let again = publish(&host.state, "hello", keyed("plugin.hello.synced")).await.unwrap();
        assert_eq!(again.id, second.id, "the same plugin repeating its key is still a duplicate");
    }

    #[tokio::test]
    async fn a_service_request_is_made_as_whoever_the_context_names() {
        let host = plugin_host();
        let echo = Address::core("echo").expect("address");
        host.state.buses.services.serve(echo, Arc::new(Echo)).await.expect("serving");
        let ada = host.identity.add_user("ada");
        let context = host
            .state
            .plugins
            .contexts
            .issue("hello", Principal::User(ada.clone()), DAY)
            .expect("issued");
        let answer = services(&host.state, "hello", Some(context.token()), ask("core.echo", false))
            .await
            .expect("answered");
        assert_eq!(answer.payload["principal"], format!("user:{}", ada.id));
    }

    #[tokio::test]
    async fn a_discovery_request_is_made_as_the_plugin_itself_whoever_it_works_for() {
        let host = plugin_host();
        let echo = Address::core("echo").expect("address");
        host.state.buses.services.serve(echo, Arc::new(Echo)).await.expect("serving");
        let ada = host.identity.add_user("ada");
        let context =
            host.state.plugins.contexts.issue("kb", Principal::User(ada), DAY).expect("issued");
        let mut discovery = ask("core.echo", false);
        discovery.subject = "discovery/archive-links".into();
        let answer =
            services(&host.state, "kb", Some(context.token()), discovery).await.expect("answered");
        assert_eq!(answer.payload["principal"], "plugin:kb");
    }

    #[tokio::test]
    async fn a_service_request_needs_a_live_context_issued_to_this_plugin() {
        let host = plugin_host();
        let echo = Address::core("echo").expect("address");
        host.state.buses.services.serve(echo, Arc::new(Echo)).await.expect("serving");
        let contexts = &host.state.plugins.contexts;
        let as_hello = || Principal::Plugin { id: "hello".into() };
        let theirs = contexts.issue("rbac", as_hello(), DAY).expect("issued");
        let ended = contexts.issue("hello", as_hello(), DAY).expect("issued").token().to_string();
        let expired = contexts.issue("hello", as_hello(), Duration::ZERO).expect("issued");

        for (why, token) in [
            ("no context", None),
            ("a made-up one", Some("doc_ctx_made-up")),
            ("another plugin's", Some(theirs.token())),
            ("one whose call has ended", Some(ended.as_str())),
            ("an expired one", Some(expired.token())),
        ] {
            let refusal = services(&host.state, "hello", token, ask("core.echo", false))
                .await
                .expect_err(why);
            assert_eq!(refusal.status, 403, "{why}");
        }
    }

    #[tokio::test]
    async fn a_queued_message_carries_the_principal_too() {
        let host = plugin_host();
        let context = host
            .state
            .plugins
            .contexts
            .issue("hello", Principal::Plugin { id: "hello".into() }, DAY)
            .expect("issued");
        services(&host.state, "hello", Some(context.token()), ask("core.jobs", true))
            .await
            .expect("queued");
        let jobs = Address::core("jobs").expect("address");
        let lease = host.state.buses.services.receive(&jobs).await.unwrap().expect("a message");
        assert_eq!(lease.message.principal.as_deref(), Some("plugin:hello"));
    }

    #[tokio::test]
    async fn a_queued_message_reaches_whoever_serves_its_address() {
        struct Recorder(parking_lot::Mutex<Vec<Request>>);

        #[async_trait]
        impl ServiceHandler for Recorder {
            async fn handle(&self, request: Request) -> Result<Value, String> {
                self.0.lock().push(request);
                Ok(Value::Null)
            }
        }

        let host = plugin_host();
        let recorder = Arc::new(Recorder(parking_lot::Mutex::new(Vec::new())));
        let address = Address::plugin("automation").expect("address");
        host.state.buses.services.serve(address, recorder.clone()).await.expect("serving");
        let contexts = &host.state.plugins.contexts;
        let context =
            contexts.issue("hello", Principal::Plugin { id: "hello".into() }, DAY).unwrap();
        services(&host.state, "hello", Some(context.token()), ask("plugin.automation", true))
            .await
            .expect("queued");
        for _ in 0..40 {
            if !recorder.0.lock().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let received = recorder.0.lock();
        assert_eq!(received.len(), 1, "delivered once, without anyone asking for it");
        assert_eq!(received[0].subject, "ping");
        assert_eq!(received[0].principal.as_deref(), Some("plugin:hello"));
    }

    #[tokio::test]
    async fn each_plugin_has_a_cache_of_its_own() {
        let host = plugin_host();
        running(&host, manifest("hello")).await;
        running(&host, manifest("rbac")).await;
        let set =
            |key: &str| CacheRequest::Set { key: key.into(), value: json!("hi"), ttl_ms: None };
        let get = |key: &str| CacheRequest::Get { key: key.into() };

        let written = cache(&host.state, "hello", set("k")).await.unwrap();
        assert!(cache(&host.state, "rbac", get("k")).await.unwrap().value.is_none());
        let read = cache(&host.state, "hello", get("k")).await.unwrap();
        assert_eq!(read.value, Some(json!("hi")));
        assert_eq!(read.version, written.version);

        let swap = |version: Option<u64>| CacheRequest::CompareAndSet {
            key: "k".into(),
            value: json!("bye"),
            version,
            ttl_ms: None,
        };
        assert!(cache(&host.state, "hello", swap(written.version)).await.unwrap().applied);
        assert!(!cache(&host.state, "hello", swap(written.version)).await.unwrap().applied);
        assert!(!cache(&host.state, "hello", swap(None)).await.unwrap().applied, "k exists");

        let removed = CacheRequest::Delete { key: "k".into() };
        assert!(cache(&host.state, "hello", removed).await.unwrap().applied);
    }

    #[tokio::test]
    async fn a_task_runs_this_plugin_started_by_whoever_the_call_is_for() {
        let host = plugin_host();
        let ada = host.identity.add_user("ada");
        let context = host
            .state
            .plugins
            .contexts
            .issue("hello", Principal::User(ada.clone()), DAY)
            .expect("issued");
        let request = |max| TaskRequest {
            payload: json!({ "name": "ada" }),
            max_attempts: max,
            delegation: None,
        };

        let queued = tasks(&host.state, "hello", Some(context.token()), request(Some(50))).await;
        let task = host.state.repos.tasks.get(queued.unwrap().task).await.unwrap().expect("a task");
        assert_eq!(task.kind, "plugin.hello.run");
        assert_eq!(task.payload, json!({ "name": "ada" }));
        assert_eq!(task.max_attempts, 10, "attempts are bounded");
        assert_eq!(
            (task.started_by.kind.as_str(), task.started_by.id),
            ("user", Some(ada.id.to_string()))
        );

        let own = tasks(&host.state, "hello", None, request(None)).await.unwrap();
        let task = host.state.repos.tasks.get(own.task).await.unwrap().expect("a task");
        assert_eq!(task.started_by.id.as_deref(), Some("hello"), "its own work is its own");
    }

    #[tokio::test]
    async fn a_delegation_starts_tasks_later_as_whoever_gave_it_until_it_is_revoked() {
        let host = plugin_host();
        let ada = host.identity.add_user("ada");
        let contexts = &host.state.plugins.contexts;
        let context = contexts.issue("hello", Principal::User(ada.clone()), DAY).expect("issued");
        let grant = |purpose: &str| DelegationRequest::Grant { purpose: purpose.into() };
        let given = delegations(&host.state, "hello", Some(context.token()), grant("automation 1"))
            .await
            .expect("granted")
            .delegation
            .expect("an id");
        assert!(host.identity.audit_actions().contains(&"delegation.granted".to_string()));
        drop(context);

        let later = |delegation| TaskRequest {
            payload: json!({ "run": 1 }),
            max_attempts: None,
            delegation: Some(delegation),
        };
        let queued = tasks(&host.state, "hello", None, later(given)).await.expect("queued");
        let task = host.state.repos.tasks.get(queued.task).await.unwrap().expect("a task");
        assert_eq!(
            (task.started_by.kind.as_str(), task.started_by.id),
            ("user", Some(ada.id.to_string())),
            "started as ada, with no call of hers in progress"
        );

        let refused =
            tasks(&host.state, "other", None, later(given)).await.expect_err("not its own");
        assert_eq!(refused.status, 404, "another plugin cannot use it");
        host.identity.set_user_disabled(ada.id, true);
        let refused = tasks(&host.state, "hello", None, later(given)).await.expect_err("disabled");
        assert_eq!(refused.status, 403, "{}", refused.detail);
        host.identity.set_user_disabled(ada.id, false);

        let revoke = || DelegationRequest::Revoke { delegation: given };
        let other = delegations(&host.state, "other", None, revoke()).await.expect("answered");
        assert_eq!(other.revoked, Some(false), "only the plugin it was given to revokes it");
        let revoked = delegations(&host.state, "hello", None, revoke()).await.expect("answered");
        assert_eq!(revoked.revoked, Some(true));
        let refused = tasks(&host.state, "hello", None, later(given)).await.expect_err("revoked");
        assert_eq!(refused.status, 403);
        assert!(refused.detail.contains("revoked"), "{}", refused.detail);
    }

    #[tokio::test]
    async fn only_a_person_or_service_account_can_delegate_during_their_own_call() {
        let host = plugin_host();
        let grant = || DelegationRequest::Grant { purpose: "automation 1".into() };
        let refused = delegations(&host.state, "hello", None, grant()).await.expect_err("no call");
        assert_eq!(refused.status, 403);
        let contexts = &host.state.plugins.contexts;
        let own = contexts.issue("hello", Principal::Plugin { id: "hello".into() }, DAY).unwrap();
        let refused = delegations(&host.state, "hello", Some(own.token()), grant())
            .await
            .expect_err("itself");
        assert_eq!(refused.status, 403, "a plugin cannot delegate to itself");

        let bot = host.identity.add_service_account("docs-bot");
        let theirs = contexts.issue("hello", Principal::ServiceAccount(bot), DAY).unwrap();
        let other = delegations(&host.state, "other", Some(theirs.token()), grant()).await;
        assert!(other.is_err(), "a context works only for the plugin it was issued to");
        let empty = DelegationRequest::Grant { purpose: " ".into() };
        let refused = delegations(&host.state, "hello", Some(theirs.token()), empty).await;
        assert_eq!(refused.expect_err("no purpose").status, 400);
        let given = delegations(&host.state, "hello", Some(theirs.token()), grant()).await;
        assert!(given.expect("a service account can").delegation.is_some());
    }

    #[tokio::test]
    async fn work_queued_during_a_task_joins_its_chain_and_the_first_task_says_how_all_of_it_went()
    {
        use doc_background_tasks::{Outcome, TaskState};
        let host = plugin_host();
        let ada = host.identity.add_user("ada");
        let ask =
            || TaskRequest { payload: json!({ "batch": 1 }), max_attempts: None, delegation: None };
        let outside = host.state.plugins.contexts.issue("hello", Principal::User(ada.clone()), DAY);
        let first =
            tasks(&host.state, "hello", Some(outside.expect("issued").token()), ask()).await;
        let first = first.expect("queued").task;
        let during =
            host.state.plugins.contexts.issue_in("hello", Principal::User(ada.clone()), DAY, first);
        let during = during.expect("issued");
        let second =
            tasks(&host.state, "hello", Some(during.token()), ask()).await.expect("queued").task;
        let chained = host.state.repos.tasks.get(second).await.unwrap().expect("a task");
        assert_eq!(chained.chain, Some(first), "queued while the first ran, so part of its run");
        let theirs = host.state.plugins.contexts.chain("rbac", during.token());
        assert_eq!(theirs, None, "another plugin's context says nothing");

        let session = crate::secrets::TokenKind::Session;
        host.identity.give(ADA_TOKEN, session, TokenOwner::User(ada.id), None, false);
        let shown = |id: Uuid| {
            let app = crate::api::router(host.state.clone());
            async move {
                crate::testing::get_as(&app, &format!("/api/v1/tasks/{id}"), ADA_TOKEN).await.1
            }
        };
        let tasks_store = &host.state.repos.tasks;
        tasks_store.finish(first, Outcome::Succeeded(json!({}))).await.expect("finished");
        let body = shown(first).await;
        assert_eq!(body["chained"]["state"], "running", "the second batch is still queued: {body}");
        assert_eq!(
            (body["chained"]["tasks"].clone(), body["chained"]["finished"].clone()),
            (json!(2), json!(1))
        );
        tasks_store
            .finish(second, Outcome::Failed("the second batch broke".into()))
            .await
            .expect("finished");
        let body = shown(first).await;
        assert_eq!(
            body["state"],
            TaskState::Succeeded.as_str(),
            "the first task itself did its part"
        );
        assert_eq!(body["chained"]["state"], "failed");
        assert_eq!(body["chained"]["error"], "the second batch broke");
        assert!(
            shown(second).await.get("chained").is_none(),
            "only the first task speaks for the chain"
        );
    }

    #[tokio::test]
    async fn state_round_trips_and_belongs_to_one_plugin() {
        let host = plugin_host();
        let set = StateRequest::Set { key: "checkpoint".into(), value: json!({ "at": 5 }) };
        plugin_state(&host.state, "hello", set).await.expect("stored");
        let get = || StateRequest::Get { key: "checkpoint".into() };
        let read = plugin_state(&host.state, "hello", get()).await.unwrap();
        assert_eq!(read.value, Some(json!({ "at": 5 })));
        assert!(plugin_state(&host.state, "rbac", get()).await.unwrap().value.is_none());

        let delete = StateRequest::Delete { key: "checkpoint".into() };
        plugin_state(&host.state, "hello", delete).await.expect("deleted");
        assert!(plugin_state(&host.state, "hello", get()).await.unwrap().value.is_none());
    }

    #[tokio::test]
    async fn state_keys_and_values_are_bounded() {
        let host = plugin_host();
        let huge = StateRequest::Set { key: "k".into(), value: json!("x".repeat(MAX_STATE_BYTES)) };
        assert_eq!(plugin_state(&host.state, "hello", huge).await.expect_err("big").status, 400);
        for key in [String::new(), "k".repeat(MAX_KEY + 1)] {
            let get = StateRequest::Get { key };
            assert_eq!(plugin_state(&host.state, "hello", get).await.expect_err("key").status, 400);
        }
    }

    #[tokio::test]
    async fn an_audit_entry_is_filed_under_the_plugin_and_names_whom_it_acted_for() {
        let host = plugin_host();
        let ada = host.identity.add_user("ada");
        let context = host
            .state
            .plugins
            .contexts
            .issue("hello", Principal::User(ada.clone()), DAY)
            .expect("issued");
        let request = AuditRequest {
            action: "greeted".into(),
            subject: Some("ada".into()),
            detail: json!({ "n": 1 }),
        };
        audit(&host.state, "hello", Some(context.token()), request).await.expect("recorded");
        let entry = host.identity.audit_entries().pop().expect("an entry");
        assert_eq!(entry.action, "plugin.hello.greeted");
        assert_eq!(
            (entry.actor_kind.as_str(), entry.actor_id.as_deref()),
            ("plugin", Some("hello"))
        );
        assert_eq!(entry.detail["on_behalf_of"], format!("user:{}", ada.id));
        assert_eq!(entry.detail["n"], 1);
    }

    #[tokio::test]
    async fn a_plugin_cannot_write_an_entry_that_reads_as_the_platforms() {
        let host = plugin_host();
        let request = |action: &str| AuditRequest {
            action: action.into(),
            subject: None,
            detail: Value::Null,
        };
        audit(&host.state, "hello", None, request("iam.token.created")).await.expect("recorded");
        assert_eq!(
            host.identity.audit_actions().last().map(String::as_str),
            Some("plugin.hello.iam.token.created")
        );
        for bad in ["", "Greeted", ".x", "x.", "has space", &"x".repeat(65)] {
            let refusal = audit(&host.state, "hello", None, request(bad)).await.expect_err(bad);
            assert_eq!(refusal.status, 400, "{bad:?}");
        }
    }

    #[tokio::test]
    async fn a_plugin_reports_its_own_work_stopping_resuming_and_failing() {
        let host = plugin_host();
        running(&host, manifest("hello")).await;
        let report =
            |state, error: Option<&str>| StatusRequest { state, error: error.map(Into::into) };
        status(&host.state, "hello", report(PluginState::Cancelled, None)).await.unwrap();
        assert_eq!(host.state_of("hello").await, Some(PluginState::Cancelled));
        status(&host.state, "hello", report(PluginState::Running, None)).await.unwrap();
        status(&host.state, "hello", report(PluginState::Error, Some("disk full"))).await.unwrap();
        assert_eq!(host.error_of("hello").await.as_deref(), Some("disk full"));

        let refusal = status(&host.state, "hello", report(PluginState::Cancelled, None)).await;
        assert_eq!(refusal.expect_err("error cannot become cancelled").status, 409);
    }

    #[tokio::test]
    async fn a_plugin_cannot_claim_to_be_loading_or_unloading() {
        let host = plugin_host();
        running(&host, manifest("hello")).await;
        for state in [PluginState::Loading, PluginState::Unloading] {
            let request = StatusRequest { state, error: None };
            let refusal = status(&host.state, "hello", request).await.expect_err("refused");
            assert_eq!(refusal.status, 403);
        }
        assert_eq!(host.state_of("hello").await, Some(PluginState::Running));
    }

    fn github() -> Host {
        let mut config = Config::default();
        config.plugins.ids = vec!["hello".into(), "github".into()];
        config.plugins.capabilities.insert("github".into(), vec![Capability::IdentityProvider]);
        plugin_host_with(config)
    }

    fn octocat(provider: &str) -> IdentityRequest {
        IdentityRequest {
            provider: provider.into(),
            external_id: "583231".into(),
            login: "octocat".into(),
            name: Some("The Octocat".into()),
            email: None,
            organisations: vec!["github".into()],
            teams: vec!["github/octo-team".into()],
            ..IdentityRequest::default()
        }
    }

    /// The organisation people sign in to with `provider`, as an admin chooses on its page.
    async fn chosen(host: &Host, provider: &str) {
        use crate::db::repositories::TeamRepository;
        let organisation = host.identity.organisation();
        host.identity.set_providers(organisation, &[provider.to_string()]).await.expect("chosen");
    }

    /// A host where `github` signs people in and `rbac` may act on those who have left.
    fn offboarding() -> Host {
        let mut config = Config::default();
        config.plugins.ids = vec!["hello".into(), "github".into(), "rbac".into()];
        config.plugins.capabilities.insert("github".into(), vec![Capability::IdentityProvider]);
        config.plugins.capabilities.insert("rbac".into(), vec![Capability::Offboarding]);
        plugin_host_with(config)
    }

    #[tokio::test]
    async fn a_provider_says_somebody_has_left_and_core_only_announces_it() {
        let host = offboarding();
        let provider =
            Manifest { capabilities: vec![Capability::IdentityProvider], ..manifest("github") };
        running(&host, provider).await;
        chosen(&host, "github").await;
        let signed_in =
            identity(&host.state, "github", octocat("github")).await.expect("signed in");

        let filter = TopicFilter::new("platform.iam.user.deprovisioned").unwrap();
        let group = ConsumerGroup::new("test.offboarding", filter);
        let mut announced = host.state.buses.events.subscribe(group).await.unwrap();

        let gone = DeprovisionRequest { external_id: "583231".into(), reason: Some("left".into()) };
        let answer = deprovision(&host.state, "github", gone).await.expect("taken");
        assert_eq!(answer.user_id, Some(signed_in.user_id));
        assert_eq!(answer.login.as_deref(), Some("octocat"));

        let event = announced.next().await.expect("announced").event.payload;
        assert_eq!(event["provider"], "github");
        assert_eq!(event["reason"], "left");

        let principal = crate::auth::authenticate(&host.state, signed_in.session_token.expose())
            .await
            .expect("the session still works");
        assert!(!principal.disabled(), "core itself does nothing to them: the rules decide");

        // A directory may speak of people this platform never knew.
        let stranger = DeprovisionRequest { external_id: "nobody".into(), reason: None };
        let answer = deprovision(&host.state, "github", stranger).await.expect("not an error");
        assert_eq!(answer.user_id, None);

        // And only an identity provider may say it.
        running(&host, manifest("hello")).await;
        let refusal = deprovision(
            &host.state,
            "hello",
            DeprovisionRequest { external_id: "583231".into(), reason: None },
        )
        .await
        .expect_err("refused");
        assert_eq!(refusal.status, 403);
    }

    #[tokio::test]
    async fn offboarding_takes_away_what_a_rule_asks_for() {
        use crate::db::repositories::TeamRepository;

        let host = offboarding();
        let provider =
            Manifest { capabilities: vec![Capability::IdentityProvider], ..manifest("github") };
        running(&host, provider).await;
        chosen(&host, "github").await;
        let offboarder =
            Manifest { capabilities: vec![Capability::Offboarding], ..manifest("rbac") };
        running(&host, offboarder).await;
        let signed_in =
            identity(&host.state, "github", octocat("github")).await.expect("signed in");
        let user = signed_in.user_id;
        let teams = host.identity.with_default_teams();
        assert!(
            !host.identity.memberships(user).await.expect("read").is_empty(),
            "everyone is placed in the default teams"
        );

        let asked = OffboardRequest {
            user,
            disable: true,
            remove_identity: Some("github".into()),
            remove_memberships: true,
            revoke_tokens: true,
            reason: Some("left the company".into()),
            ..OffboardRequest::default()
        };
        let done = offboard(&host.state, "rbac", asked).await.expect("offboarded");
        assert!(done.disabled);
        assert_eq!(done.identities_removed, 1, "the account they signed in with is gone");
        assert_eq!(done.memberships_removed, teams.len());
        assert!(done.tokens_revoked >= 1, "their session no longer works");

        assert!(
            crate::auth::authenticate(&host.state, signed_in.session_token.expose()).await.is_err(),
            "the session was revoked, so it is refused at once"
        );
        assert!(host.identity.memberships(user).await.expect("read").is_empty());
        assert!(
            host.state.repos.identity.identity("github", "583231").await.expect("read").is_none(),
            "signing in with that account again makes a new user, not this one"
        );

        // Only a plugin allowed to may do it, and only to somebody who exists.
        let refusal = offboard(
            &host.state,
            "github",
            OffboardRequest { user, disable: true, ..OffboardRequest::default() },
        )
        .await
        .expect_err("refused");
        assert_eq!(refusal.status, 403, "signing people in is not offboarding them");
        let refusal = offboard(
            &host.state,
            "rbac",
            OffboardRequest { user: Uuid::now_v7(), ..OffboardRequest::default() },
        )
        .await
        .expect_err("refused");
        assert_eq!(refusal.status, 404);
    }

    #[tokio::test]
    async fn only_an_identity_provider_may_sign_users_in() {
        let host = github();
        running(&host, manifest("hello")).await;
        let refusal = identity(&host.state, "hello", octocat("hello")).await.expect_err("refused");
        assert_eq!(refusal.status, 403);
    }

    #[tokio::test]
    async fn an_identity_provider_signs_in_its_own_users() {
        let host = github();
        let provider =
            Manifest { capabilities: vec![Capability::IdentityProvider], ..manifest("github") };
        running(&host, provider).await;
        chosen(&host, "github").await;
        let filter = TopicFilter::new("platform.iam.user.signed-in").unwrap();
        let group = ConsumerGroup::new("test.onboarding", filter);
        let mut announced = host.state.buses.events.subscribe(group).await.unwrap();
        let signed_in =
            identity(&host.state, "github", octocat("github")).await.expect("signed in");
        let principal = crate::auth::authenticate(&host.state, signed_in.session_token.expose())
            .await
            .expect("the session works");
        let user = principal.as_user().expect("a user");
        assert_eq!(user.login, "octocat");
        assert_eq!(user.linked.get("github").map(String::as_str), Some("octocat"));
        assert_eq!(user.id, signed_in.user_id);
        assert!(signed_in.first, "the frontend offers the accounts plugins need after this");

        let event = announced.next().await.expect("the sign-in is announced").event.payload;
        assert_eq!(event["provider"], "github");
        assert_eq!(event["first"], true);
        assert_eq!(event["organisations"], json!(["github"]), "onboarding rules match on these");
        assert_eq!(event["teams"], json!(["github/octo-team"]));
    }

    #[tokio::test]
    async fn the_sign_in_page_is_offered_each_organisations_running_identity_providers() {
        let host = github();
        let app = crate::api::router(host.state.clone());
        let (status, body, _) = crate::testing::get(&app, "/api/v1/auth/providers").await;
        assert_eq!(status, http::StatusCode::OK, "asked before anyone has signed in");
        assert_eq!(body, json!({ "organisations": [] }));

        running(&host, manifest("hello")).await;
        let signs_in = Some(doc_plugin_protocol::SignIn::new(
            "GitHub",
            doc_plugin_protocol::SignInKind::Redirect,
        ));
        let provider = Manifest {
            capabilities: vec![Capability::IdentityProvider],
            sign_in: signs_in,
            ..manifest("github")
        };
        running(&host, provider).await;
        let (_, body, _) = crate::testing::get(&app, "/api/v1/auth/providers").await;
        assert_eq!(
            body["organisations"],
            json!([]),
            "a provider no organisation chose is not offered"
        );

        chosen(&host, "github").await;
        let (_, body, _) = crate::testing::get(&app, "/api/v1/auth/providers").await;
        assert_eq!(body["organisations"][0]["name"], "default");
        assert_eq!(
            body["organisations"][0]["providers"],
            json!([{ "id": "github", "title": "GitHub", "kind": "redirect" }]),
            "hello signs nobody in"
        );
    }

    #[tokio::test]
    async fn an_identity_provider_cannot_sign_in_another_providers_users() {
        let host = github();
        let provider =
            Manifest { capabilities: vec![Capability::IdentityProvider], ..manifest("github") };
        running(&host, provider).await;
        let refusal = identity(&host.state, "github", octocat("local")).await.expect_err("refused");
        assert_eq!(refusal.status, 403);
    }
}
