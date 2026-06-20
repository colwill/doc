//! The plugin's side of the backend API in MVP §5. Every call goes over the one HTTP/3 connection
//! the runtime holds open, and a call made while handling a backend call carries that call's
//! context token — which is what stops a plugin acting for a principal who never called it.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use doc_plugin_protocol::calls::{
    AccessAnswer, AccessRequest, AuditRequest, CacheRequest, CacheResponse, DelegationRequest,
    DelegationResponse, DeprovisionRequest, DeprovisionResponse, IdentityRequest, IdentityResponse,
    LinkRequest, LinkResponse, OffboardRequest, OffboardResponse, OpenRequest, OpenResponse,
    OrganisationRequest, OrganisationResponse, PersonRequest, PublishRequest, PublishResponse,
    RevokeScopedTokenRequest, RevokeScopedTokenResponse, ScopedTokenRequest, ScopedTokenResponse,
    SealRequest, SealResponse, SealedValue, SecretsChangedRequest, SecretsChangedResponse,
    ServiceRequest, ServiceResponse, SettingsView, StateRequest, StateResponse, StatusRequest,
    TaskRequest, TaskResponse, TeamMembersRequest, TeamMembersResponse, TeamRemoveRequest,
    TeamRemoveResponse, TeamRequest, TeamResponse, UserRequest, UserResponse, WriteTeamRequest,
};
use doc_plugin_protocol::data::{Aggregate, DataAnswer, DataRequest, MAX_BATCH, MAX_LIMIT, Query};
use doc_plugin_protocol::{Caller, Guard, PluginState, Secret, backend as paths, header};
use doc_transport::H3Client;
use http::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Map, Value};
use tracing::Instrument;
use uuid::Uuid;

use crate::PluginError;

/// The plugin that keeps feature flags, and its list of the plugins that may read them.
const FLAGS: &str = "flags";
const READER_PLUGINS: &str = "reader-plugins";

/// What the flags plugin holds for a plugin reading as itself: flags and runtime configuration
/// resolved for one service in one environment, DOC's own and its providers' merged.
#[derive(Debug, Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct Flags {
    pub service: String,
    pub environment: String,
    pub flags: Map<String, Value>,
    pub config: Map<String, Value>,
    /// Changes whenever anything in the answer does.
    pub version: String,
    /// Providers that could not be read; what the answer holds is still served.
    pub problems: Vec<String>,
}

impl Flags {
    /// A flag's value, or a setting's where no flag has the key.
    pub fn get(&self, key: &str) -> Option<&Value> {
        let key = key.to_ascii_lowercase();
        self.flags.get(&key).or_else(|| self.config.get(&key))
    }

    /// Whether a flag is on: `true` is, and anything else — off, missing, a value — is not.
    pub fn on(&self, key: &str) -> bool {
        matches!(self.get(key), Some(Value::Bool(true)))
    }
}

const DEADLINE: Duration = Duration::from_secs(30);
/// How often `change` reads and writes again when other writers keep getting there first.
const CHANGE_ATTEMPTS: usize = 5;

/// A span for a call on the backend API, when there is a trace for it to continue.
fn traced(path: &str) -> tracing::Span {
    if !doc_telemetry::in_trace() {
        return tracing::Span::none();
    }
    let function = path.trim_start_matches(paths::PREFIX).trim_start_matches('/');
    tracing::info_span!(
        target: "doc",
        "backend.call",
        otel.name = %format!("backend {function}"),
        otel.kind = "client",
        otel.status_description = tracing::field::Empty,
        http.response.status_code = tracing::field::Empty,
    )
}

/// One page of a query, and the cursor for the next when there is more.
#[derive(Debug, Clone)]
pub struct Page<T> {
    pub records: Vec<T>,
    pub next: Option<String>,
}

fn record<T: DeserializeOwned>(record: Map<String, Value>) -> Result<T, PluginError> {
    serde_json::from_value(Value::Object(record)).map_err(|err| PluginError::encode(&err))
}

pub(crate) struct Inner {
    pub client: H3Client,
    pub token: Secret<String>,
    pub id: String,
    pub version: String,
    /// The plugin's settings as core last gave them (ADR-0007). Read before every `load` and
    /// again whenever core says they changed, so reading one is not a call to the backend.
    pub settings: std::sync::RwLock<Arc<Settings>>,
}

/// A plugin's settings and features, as it was configured (ADR-0007). Every setting the manifest
/// declares is here, at its stored value or its default, so reading one never has to ask whether
/// anybody set it.
#[derive(Debug, Clone, Default)]
pub struct Settings(SettingsView);

impl Settings {
    /// The settings core is asking about in a `settings_check`: what they would be if the save it
    /// is about went through, read exactly as the stored ones are.
    pub(crate) fn proposed(check: doc_plugin_protocol::calls::SettingsCheck) -> Self {
        Self(SettingsView {
            values: check.values,
            secrets: check.secrets,
            named: check.named,
            features: check.features,
            missing: Vec::new(),
            instance: check.instance,
        })
    }

    /// Where the platform runs, as its configuration says (`[instance]`): the same for every plugin.
    pub fn instance(&self) -> &doc_plugin_protocol::calls::Instance {
        &self.0.instance
    }

    /// Text, a choice, a URL or a cron expression; empty when it is unset and has no default.
    pub fn text(&self, key: &str) -> String {
        self.0.values.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
    }

    /// The same, but `None` rather than an empty string, for a setting that is optional.
    pub fn some_text(&self, key: &str) -> Option<String> {
        Some(self.text(key)).filter(|text| !text.is_empty())
    }

    pub fn number(&self, key: &str) -> Option<f64> {
        self.0.values.get(key).and_then(Value::as_f64)
    }

    pub fn integer(&self, key: &str) -> Option<i64> {
        self.number(key).map(|number| number as i64)
    }

    pub fn boolean(&self, key: &str) -> bool {
        self.0.values.get(key).and_then(Value::as_bool).unwrap_or_default()
    }

    pub fn list(&self, key: &str) -> Vec<String> {
        self.0
            .values
            .get(key)
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
            .unwrap_or_default()
    }

    /// A `map` setting: each key with its values, in key order.
    pub fn map(&self, key: &str) -> std::collections::BTreeMap<String, Vec<String>> {
        let strings = |values: &Value| -> Vec<String> {
            values
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        };
        self.0
            .values
            .get(key)
            .and_then(Value::as_object)
            .map(|entries| {
                entries.iter().map(|(key, values)| (key.clone(), strings(values))).collect()
            })
            .unwrap_or_default()
    }

    /// A duration setting, which core stores as the seconds it comes to.
    pub fn duration(&self, key: &str) -> Option<Duration> {
        self.number(key).filter(|seconds| *seconds >= 0.0).map(Duration::from_secs_f64)
    }

    /// A secret the plugin declared. Only the plugin that declared it is ever given it, and it
    /// stays in a `Secret` so it cannot reach a log line or an answer by accident.
    pub fn secret(&self, key: &str) -> Option<Secret<String>> {
        self.0.secrets.get(key).cloned()
    }

    /// Whether a feature is on. A feature nobody declared is off.
    pub fn feature(&self, name: &str) -> bool {
        self.0.features.get(name).copied().unwrap_or_default()
    }

    /// A credential this plugin holds by name, for one whose credentials are named at runtime
    /// rather than declared one by one (ADR-0007).
    pub fn named(&self, name: &str) -> Option<Secret<String>> {
        self.0.named.get(name).cloned()
    }

    /// Every credential it holds by name, in name order.
    pub fn named_names(&self) -> Vec<String> {
        self.0.named.keys().cloned().collect()
    }

    /// The setting a deployment variable names, as text, wherever its value came from:
    /// `DOC_INFRA_LINODE_TOKEN` is the `linode-token` setting of the `infra` plugin. For a plugin
    /// that looks its credentials up by variable name, deep in code that has no `Backend` to
    /// hand, this is how those lookups move to declared settings without moving the call sites.
    pub fn by_variable(&self, plugin: &str, variable: &str) -> Option<String> {
        let prefix = format!("DOC_{}_", plugin.to_ascii_uppercase().replace('-', "_"));
        let key = variable.strip_prefix(&prefix)?.to_ascii_lowercase().replace('_', "-");
        self.some_text(&key).or_else(|| self.secret(&key).map(|held| held.expose().clone()))
    }

    /// Required settings nobody has set, which is what a plugin says instead of failing.
    pub fn missing(&self) -> &[String] {
        &self.0.missing
    }

    /// Whether everything required is set, which is what decides if a route can serve at all.
    pub fn configured(&self) -> bool {
        self.0.missing.is_empty()
    }

    /// What a plugin says when it is not configured: "Needs setting: base-url, token".
    pub fn needs(&self) -> String {
        format!("not configured: needs {}", self.0.missing.join(", "))
    }
}

/// Handed to every `Plugin` method. Cloning is cheap; the runtime clones one per call to attach
/// that call's context and caller.
#[derive(Clone)]
pub struct Backend {
    pub(crate) inner: Arc<Inner>,
    context: Option<String>,
    caller: Option<Caller>,
    guard: Option<Guard>,
}

impl Backend {
    pub(crate) fn new(inner: Arc<Inner>) -> Self {
        Self { inner, context: None, caller: None, guard: None }
    }

    pub(crate) fn for_call(&self, context: Option<String>, caller: Option<Caller>) -> Self {
        Self { inner: self.inner.clone(), context, caller, guard: None }
    }

    /// The same backend, limiting every call it relays to other plugins by `guard`, as well as
    /// by whatever guard the call being handled already carries, which core keeps either way.
    pub fn guarded(&self, guard: Guard) -> Self {
        Self { guard: Some(guard), ..self.clone() }
    }

    pub fn id(&self) -> &str {
        &self.inner.id
    }

    pub fn version(&self) -> &str {
        &self.inner.version
    }

    /// Who the backend is acting for, when this is a forwarded call rather than a lifecycle one.
    pub fn caller(&self) -> Option<&Caller> {
        self.caller.as_ref()
    }

    /// The plugin's own permission check: core has already checked `user` and `service` access.
    pub fn allows(&self, permission: &str, write: bool) -> bool {
        self.caller.as_ref().is_some_and(|caller| caller.allows(permission, write))
    }

    /// Whether the caller may write to this plugin, so a page shows editing controls only to them.
    pub fn writes(&self) -> bool {
        self.caller.as_ref().is_some_and(Caller::writes)
    }

    /// Refuses with the problem the backend would return, so a handler can use `?`.
    pub fn require(&self, permission: &str, write: bool) -> Result<(), PluginError> {
        match self.allows(permission, write) {
            true => Ok(()),
            false => Err(PluginError::Forbidden(format!(
                "plugin:{}:pluginuser:{permission}",
                self.inner.id
            ))),
        }
    }

    pub fn attribute(&self, key: &str) -> Option<&str> {
        self.caller.as_ref()?.attributes.get(key).map(String::as_str)
    }

    async fn post<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
    ) -> Result<R, PluginError> {
        self.call(path, body, DEADLINE).await
    }

    /// Every call carries the plugin's registration token: it is what tells the backend which
    /// plugin is speaking, on lifecycle calls as much as on the backend API.
    pub(crate) async fn call<T: Serialize, R: DeserializeOwned>(
        &self,
        path: &str,
        body: &T,
        deadline: Duration,
    ) -> Result<R, PluginError> {
        let payload =
            Bytes::from(serde_json::to_vec(body).map_err(|err| PluginError::encode(&err))?);
        let authorization = format!("Bearer {}", self.inner.token.expose());
        let mut headers: Vec<(&str, &str)> =
            vec![("content-type", "application/json"), ("authorization", &authorization)];
        if let Some(context) = &self.context {
            headers.push((header::CONTEXT, context));
        }
        let span = traced(path);
        let parent = span.in_scope(doc_telemetry::traceparent);
        if let Some(parent) = &parent {
            headers.push((header::TRACEPARENT, parent));
        }
        let call = self.inner.client.call(Method::POST, path, &headers, payload);
        let answered = tokio::time::timeout(deadline, call).instrument(span.clone()).await;
        let (status, bytes) = answered
            .map_err(|_| PluginError::Deadline)
            .and_then(|answered| answered.map_err(|err| PluginError::Unreachable(err.to_string())))
            .inspect_err(|err| {
                span.record("otel.status_description", tracing::field::display(err));
            })?;
        span.record("http.response.status_code", status.as_u16());
        if !status.is_success() {
            if status.is_server_error() {
                span.record("otel.status_description", status.as_str());
            }
            return Err(PluginError::Refused {
                status: status.as_u16(),
                body: String::from_utf8_lossy(&bytes).into_owned(),
            });
        }
        if bytes.is_empty() {
            return serde_json::from_slice(b"null").map_err(|err| PluginError::encode(&err));
        }
        serde_json::from_slice(&bytes).map_err(|err| PluginError::encode(&err))
    }

    /// This plugin's settings and features as core last gave them (ADR-0007). Cheap: the runtime
    /// reads them before each `load` and again whenever core says they changed.
    pub fn settings(&self) -> Arc<Settings> {
        self.inner.settings.read().map(|held| held.clone()).unwrap_or_default()
    }

    /// Whether one of this plugin's features is on, which is the check a route or a piece of work
    /// belonging to a feature makes for itself.
    pub fn feature(&self, name: &str) -> bool {
        self.settings().feature(name)
    }

    /// Reads the settings from core again and keeps them. The runtime does this for a plugin; a
    /// plugin only calls it if it wants them fresher than the last change it was told about.
    pub async fn refresh_settings(&self) -> Result<Arc<Settings>, PluginError> {
        let view: SettingsView = self.post(paths::SETTINGS, &Value::Null).await?;
        let settings = Arc::new(Settings(view));
        if let Ok(mut held) = self.inner.settings.write() {
            *held = settings.clone();
        }
        Ok(settings)
    }

    /// One request to the data API (DOC-SPEC §9.1); the methods below cover each operation.
    pub async fn data(&self, request: &DataRequest) -> Result<DataAnswer, PluginError> {
        self.post(paths::DATA, request).await
    }

    /// A record by its key, from this plugin's collections, an export or `core.*`.
    pub async fn get<T: DeserializeOwned>(
        &self,
        collection: &str,
        key: impl Into<Value>,
    ) -> Result<Option<T>, PluginError> {
        let answer = self.data(&DataRequest::get(collection, key)).await?;
        answer.record.map(record).transpose()
    }

    pub async fn query<T: DeserializeOwned>(&self, query: Query) -> Result<Page<T>, PluginError> {
        let answer = self.data(&DataRequest::Query(query)).await?;
        let records =
            answer.records.unwrap_or_default().into_iter().map(record).collect::<Result<_, _>>()?;
        Ok(Page { records, next: answer.next })
    }

    /// Every record the query matches, a page at a time; keep it for collections known to be small.
    pub async fn query_all<T: DeserializeOwned>(
        &self,
        query: Query,
    ) -> Result<Vec<T>, PluginError> {
        let mut query = query.limit(MAX_LIMIT);
        let mut all = Vec::new();
        loop {
            let page = self.query(query.clone()).await?;
            all.extend(page.records);
            match page.next {
                Some(next) => query = query.after(Some(next)),
                None => return Ok(all),
            }
        }
    }

    pub async fn aggregate(
        &self,
        aggregate: Aggregate,
    ) -> Result<Vec<Map<String, Value>>, PluginError> {
        Ok(self.data(&DataRequest::Aggregate(aggregate)).await?.groups.unwrap_or_default())
    }

    /// The record as stored, with its key, defaults and system fields filled in.
    pub async fn insert<T: DeserializeOwned>(
        &self,
        collection: &str,
        values: Value,
    ) -> Result<T, PluginError> {
        let answer = self.data(&DataRequest::insert(collection, values)).await?;
        record(answer.record.unwrap_or_default())
    }

    /// `None` when there is no such record; with a `version`, only if the record is still at it.
    pub async fn update<T: DeserializeOwned>(
        &self,
        collection: &str,
        key: impl Into<Value>,
        set: Value,
        version: Option<i64>,
    ) -> Result<Option<T>, PluginError> {
        let mut request = DataRequest::update(collection, key, set);
        if let Some(version) = version {
            request = request.at_version(version);
        }
        self.data(&request).await?.record.map(record).transpose()
    }

    /// Updates a record from what it holds now, again whenever another writer gets there first.
    pub async fn change(
        &self,
        collection: &str,
        key: impl Into<Value>,
        change: impl Fn(&Map<String, Value>) -> Option<Value>,
    ) -> Result<Option<Map<String, Value>>, PluginError> {
        let key = key.into();
        for _ in 0..CHANGE_ATTEMPTS {
            let Some(current) = self.get::<Map<String, Value>>(collection, key.clone()).await?
            else {
                return Ok(None);
            };
            let Some(set) = change(&current) else { return Ok(None) };
            let version = current.get("_version").and_then(Value::as_i64);
            match self.update(collection, key.clone(), set, version).await {
                Err(err) if err.is_version_conflict() => continue,
                answered => return answered,
            }
        }
        Err(PluginError::Message(format!("{collection} {key} kept changing; try again")))
    }

    /// Inserts, or updates the record whose `on` fields match; true when it was inserted.
    pub async fn upsert<T: DeserializeOwned>(
        &self,
        collection: &str,
        on: &[&str],
        values: Value,
    ) -> Result<(T, bool), PluginError> {
        let answer = self.data(&DataRequest::upsert(collection, on, values)).await?;
        Ok((record(answer.record.unwrap_or_default())?, answer.created.unwrap_or_default()))
    }

    /// Whether there was a record to delete; with a `version`, only if the record is still at it.
    pub async fn delete(
        &self,
        collection: &str,
        key: impl Into<Value>,
        version: Option<i64>,
    ) -> Result<bool, PluginError> {
        let mut request = DataRequest::delete(collection, key);
        if let Some(version) = version {
            request = request.at_version(version);
        }
        Ok(self.data(&request).await?.deleted.unwrap_or_default())
    }

    /// Up to 100 writes in one transaction: all of them happen, or none do.
    pub async fn batch(&self, writes: Vec<DataRequest>) -> Result<Vec<DataAnswer>, PluginError> {
        Ok(self.data(&DataRequest::Batch { writes }).await?.results.unwrap_or_default())
    }

    /// Deletes every record `filter` matches, a batch at a time rather than in one transaction.
    pub async fn delete_where(
        &self,
        collection: &str,
        key: &str,
        filter: Value,
    ) -> Result<usize, PluginError> {
        let mut deleted = 0;
        loop {
            let found = Query::new(collection)
                .filter(filter.clone())
                .fields(&[key])
                .limit(MAX_BATCH as u32);
            let page = self.query::<Map<String, Value>>(found).await?;
            let writes: Vec<DataRequest> = page
                .records
                .iter()
                .filter_map(|record| record.get(key).cloned())
                .map(|key| DataRequest::delete(collection, key))
                .collect();
            if writes.is_empty() {
                return Ok(deleted);
            }
            deleted += writes.len();
            self.batch(writes).await?;
        }
    }

    /// Publishes to this plugin's own `plugin.<id>.*` prefix; the backend refuses anything else.
    pub async fn publish(&self, topic: &str, payload: Value) -> Result<Uuid, PluginError> {
        let request = PublishRequest { topic: topic.to_string(), payload, idempotency_key: None };
        let response: PublishResponse = self.post(paths::EVENTS, &request).await?;
        Ok(response.id)
    }

    pub async fn publish_once(
        &self,
        topic: &str,
        payload: Value,
        key: &str,
    ) -> Result<Uuid, PluginError> {
        let request = PublishRequest {
            topic: topic.to_string(),
            payload,
            idempotency_key: Some(key.to_string()),
        };
        let response: PublishResponse = self.post(paths::EVENTS, &request).await?;
        Ok(response.id)
    }

    /// A Service Bus request, made as the principal of the call being handled.
    pub async fn request(
        &self,
        address: &str,
        subject: &str,
        payload: Value,
        deadline: Duration,
    ) -> Result<Value, PluginError> {
        let request = ServiceRequest {
            address: address.to_string(),
            subject: subject.to_string(),
            payload,
            deadline_ms: Some(deadline.as_millis() as u64),
            queue: false,
            guard: self.guard,
        };
        let response: ServiceResponse = self.post(paths::SERVICES, &request).await?;
        Ok(response.payload)
    }

    pub async fn send(
        &self,
        address: &str,
        subject: &str,
        payload: Value,
    ) -> Result<(), PluginError> {
        let request = ServiceRequest {
            address: address.to_string(),
            subject: subject.to_string(),
            payload,
            deadline_ms: None,
            queue: true,
            guard: self.guard,
        };
        let _: ServiceResponse = self.post(paths::SERVICES, &request).await?;
        Ok(())
    }

    /// Queues a POST of `body` to another plugin's `discovery/<route>`, delivered even if it is busy now.
    pub async fn queue(&self, plugin: &str, route: &str, body: Value) -> Result<(), PluginError> {
        let payload = serde_json::json!({ "method": "POST", "body": body });
        self.send(&format!("plugin.{plugin}"), &format!("discovery/{route}"), payload).await
    }

    /// Puts a notification in `user`'s inbox: the header bell and `/p/notifications/`. Fire and
    /// forget, like `queue`, so a slow or momentarily busy `notifications` never holds up the
    /// call telling it something happened.
    pub async fn notify(
        &self,
        user: &str,
        title: &str,
        body: &str,
        url: Option<&str>,
    ) -> Result<(), PluginError> {
        self.queue(
            "notifications",
            "notify",
            serde_json::json!({ "user": user, "title": title, "body": body, "url": url.unwrap_or("") }),
        )
        .await
    }

    /// Asks an administrator to add this plugin to `plugin`'s requestable list `setting`, such as
    /// `github`'s `archive-plugins`. The first ask notifies whoever may change `plugin`'s settings;
    /// asking again only says where it stands, so it is safe to call every time access is refused.
    pub async fn request_access(
        &self,
        plugin: &str,
        setting: &str,
        reason: &str,
    ) -> Result<AccessAnswer, PluginError> {
        let request = AccessRequest {
            plugin: plugin.to_string(),
            setting: setting.to_string(),
            reason: reason.to_string(),
        };
        self.post(paths::ACCESS_REQUESTS, &request).await
    }

    /// The feature flags this plugin reads as itself, from the flags plugin: as the service named
    /// after this plugin unless `service` names another (`doc` is the platform's own), in the
    /// platform's default environment unless `environment` names one. Flags for every service and
    /// providers' flags apply as they do to any service.
    ///
    /// The flags plugin answers only the plugins its requestable `reader-plugins` list names. One
    /// that is not listed yet is refused, and this asks whoever may change the flags plugin's
    /// settings to add it — which only says where the request stands after the first time, so it
    /// is safe on every call. To read flags as the person a call is for instead, where their own
    /// access to the flags plugin decides, ask its `evaluate` route with [`Backend::ask`].
    pub async fn flags(
        &self,
        service: Option<&str>,
        environment: Option<&str>,
    ) -> Result<Flags, PluginError> {
        let plain = |name: &str| {
            !name.is_empty()
                && name.len() <= 120
                && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_./*".contains(c))
        };
        let mut query = Vec::new();
        for (field, named) in [("service", service), ("environment", environment)] {
            let Some(named) = named else { continue };
            if !plain(named) {
                return Err(PluginError::from(format!("`{named}` is not a flag's {field}")));
            }
            query.push(format!("{field}={named}"));
        }
        let query = (!query.is_empty()).then(|| query.join("&"));
        let (status, body) =
            self.discovery(FLAGS, "GET", "evaluate", query.as_deref(), None).await?;
        match status {
            200 => serde_json::from_value(body)
                .map_err(|err| PluginError::from(format!("the flags plugin answered: {err}"))),
            403 if body["type"].as_str().is_some_and(|kind| kind.ends_with("not-listed")) => {
                let reason = "It reads feature flags as itself, to decide what it does.";
                if let Err(err) = self.request_access(FLAGS, READER_PLUGINS, reason).await {
                    tracing::warn!(%err, "access to the flags plugin could not be asked for");
                }
                Err(PluginError::from(
                    "the flags plugin does not let this plugin read flags yet; whoever may change \
                     its settings has been asked to",
                ))
            }
            status => {
                let detail = body["detail"].as_str().unwrap_or_default();
                Err(PluginError::from(format!("the flags plugin answered {status}: {detail}")))
            }
        }
    }

    /// Another plugin's `api/<route>` as this call's principal: its status and body, refusals too.
    pub async fn ask(
        &self,
        plugin: &str,
        method: &str,
        route: &str,
        query: Option<&str>,
        body: Option<Value>,
    ) -> Result<(u16, Value), PluginError> {
        self.relayed(plugin, &format!("api/{route}"), method, query, body).await
    }

    /// Another plugin's `discovery/<route>`, asked as this plugin itself; that route decides whom it serves.
    pub async fn discovery(
        &self,
        plugin: &str,
        method: &str,
        route: &str,
        query: Option<&str>,
        body: Option<Value>,
    ) -> Result<(u16, Value), PluginError> {
        self.relayed(plugin, &format!("discovery/{route}"), method, query, body).await
    }

    async fn relayed(
        &self,
        plugin: &str,
        subject: &str,
        method: &str,
        query: Option<&str>,
        body: Option<Value>,
    ) -> Result<(u16, Value), PluginError> {
        let payload = serde_json::json!({ "method": method, "query": query, "body": body });
        let mut answer =
            self.request(&format!("plugin.{plugin}"), subject, payload, DEADLINE).await?;
        let status = answer["status"].as_u64().and_then(|status| u16::try_from(status).ok());
        let status = status.ok_or_else(|| PluginError::from("the relayed answer has no status"))?;
        Ok((status, answer["body"].take()))
    }

    pub async fn cache_get(&self, key: &str) -> Result<Option<Value>, PluginError> {
        let response: CacheResponse =
            self.post(paths::CACHE, &CacheRequest::Get { key: key.to_string() }).await?;
        Ok(response.value)
    }

    pub async fn cache_set(
        &self,
        key: &str,
        value: Value,
        ttl: Option<Duration>,
    ) -> Result<u64, PluginError> {
        let request = CacheRequest::Set {
            key: key.to_string(),
            value,
            ttl_ms: ttl.map(|ttl| ttl.as_millis() as u64),
        };
        let response: CacheResponse = self.post(paths::CACHE, &request).await?;
        Ok(response.version.unwrap_or_default())
    }

    pub async fn cache_delete(&self, key: &str) -> Result<(), PluginError> {
        let _: CacheResponse =
            self.post(paths::CACHE, &CacheRequest::Delete { key: key.to_string() }).await?;
        Ok(())
    }

    /// Returns false when the version had moved on, which is a lost race rather than a failure.
    /// `version` of `None` writes only if the key does not exist yet.
    pub async fn cache_compare_and_set(
        &self,
        key: &str,
        value: Value,
        version: Option<u64>,
        ttl: Option<Duration>,
    ) -> Result<bool, PluginError> {
        let request = CacheRequest::CompareAndSet {
            key: key.to_string(),
            value,
            version,
            ttl_ms: ttl.map(|ttl| ttl.as_millis() as u64),
        };
        let response: CacheResponse = self.post(paths::CACHE, &request).await?;
        Ok(response.applied)
    }

    /// Queues a background `run` of this plugin and returns the task it can be followed by.
    pub async fn task(&self, payload: Value) -> Result<Uuid, PluginError> {
        let request = TaskRequest { payload, max_attempts: None, delegation: None };
        let response: TaskResponse = self.post(paths::TASKS, &request).await?;
        Ok(response.task)
    }

    /// As `task`, tried at most `max_attempts` times: once, for work that must not be done twice.
    pub async fn task_tried(&self, payload: Value, max_attempts: i32) -> Result<Uuid, PluginError> {
        let request = TaskRequest { payload, max_attempts: Some(max_attempts), delegation: None };
        let response: TaskResponse = self.post(paths::TASKS, &request).await?;
        Ok(response.task)
    }

    /// Queues a run started by whoever gave `delegation`, checked as they are now.
    pub async fn task_as(
        &self,
        delegation: Uuid,
        payload: Value,
        max_attempts: Option<i32>,
    ) -> Result<Uuid, PluginError> {
        let request = TaskRequest { payload, max_attempts, delegation: Some(delegation) };
        let response: TaskResponse = self.post(paths::TASKS, &request).await?;
        Ok(response.task)
    }

    /// Leave to start runs later as whoever this call is for; only their own call can give it.
    pub async fn delegate(&self, purpose: &str) -> Result<Uuid, PluginError> {
        let request = DelegationRequest::Grant { purpose: purpose.to_string() };
        let response: DelegationResponse = self.post(paths::DELEGATIONS, &request).await?;
        response.delegation.ok_or_else(|| PluginError::from("the backend gave no delegation"))
    }

    /// Whether the delegation was still in force.
    pub async fn revoke(&self, delegation: Uuid) -> Result<bool, PluginError> {
        let request = DelegationRequest::Revoke { delegation };
        let response: DelegationResponse = self.post(paths::DELEGATIONS, &request).await?;
        Ok(response.revoked.unwrap_or(false))
    }

    pub async fn state_get(&self, key: &str) -> Result<Option<Value>, PluginError> {
        let response: StateResponse =
            self.post(paths::STATE, &StateRequest::Get { key: key.to_string() }).await?;
        Ok(response.value)
    }

    pub async fn state_set(&self, key: &str, value: Value) -> Result<(), PluginError> {
        let _: StateResponse =
            self.post(paths::STATE, &StateRequest::Set { key: key.to_string(), value }).await?;
        Ok(())
    }

    pub async fn state_delete(&self, key: &str) -> Result<(), PluginError> {
        let _: StateResponse =
            self.post(paths::STATE, &StateRequest::Delete { key: key.to_string() }).await?;
        Ok(())
    }

    pub async fn audit(
        &self,
        action: &str,
        subject: Option<&str>,
        detail: Value,
    ) -> Result<(), PluginError> {
        let request = AuditRequest {
            action: action.to_string(),
            subject: subject.map(str::to_string),
            detail,
        };
        let _: Value = self.post(paths::AUDIT, &request).await?;
        Ok(())
    }

    /// Reports a lifecycle state or an error. The runtime does this around `load` and `unload`;
    /// a plugin calls it itself when it decides it is cancelled or in error.
    pub async fn set_state(
        &self,
        state: PluginState,
        error: Option<&str>,
    ) -> Result<(), PluginError> {
        let request = StatusRequest { state, error: error.map(str::to_string) };
        let _: Value = self.post(paths::STATUS, &request).await?;
        Ok(())
    }

    /// Identity providers only: creates or updates a user and starts a session for them.
    pub async fn identity(
        &self,
        request: IdentityRequest,
    ) -> Result<IdentityResponse, PluginError> {
        self.post(paths::IDENTITY, &request).await
    }

    /// Identity providers only: links an account to whoever asked core for the request's ticket,
    /// which a sign-in started from their account page carries through (ADR-0005).
    pub async fn link(&self, request: LinkRequest) -> Result<LinkResponse, PluginError> {
        self.post(paths::IDENTITY_LINK, &request).await
    }

    /// Identity and team providers only: the user one of the provider's accounts belongs to, made
    /// with it if there is none, so people can be given access before they first sign in.
    pub async fn provide_user(&self, request: UserRequest) -> Result<UserResponse, PluginError> {
        self.post(paths::USERS, &request).await
    }

    /// Team providers only: makes or updates one of the provider's teams in core (ADR-0004).
    pub async fn provide_team(&self, request: TeamRequest) -> Result<TeamResponse, PluginError> {
        self.post(paths::TEAMS, &request).await
    }

    /// With the `team-writer` capability: an organisation made in DOC, made if there is none of
    /// that name (T69). When a person is asking, they must administer identity themselves.
    pub async fn write_organisation(
        &self,
        request: OrganisationRequest,
    ) -> Result<OrganisationResponse, PluginError> {
        self.post(paths::ORGANISATION_WRITE, &request).await
    }

    /// With the `team-writer` capability: a team made in DOC, made if its organisation has none of
    /// that name. A team a provider keeps is left alone apart from its address.
    pub async fn write_team(&self, request: WriteTeamRequest) -> Result<TeamResponse, PluginError> {
        self.post(paths::TEAM_WRITE, &request).await
    }

    /// With the `token-issuer` capability: a scoped token for whoever this call is for, limited to
    /// `scopes` they hold, for minutes (FEAT-VACUUM). Only during a call a person made.
    pub async fn scoped_token(
        &self,
        request: ScopedTokenRequest,
    ) -> Result<ScopedTokenResponse, PluginError> {
        self.post(paths::SCOPED_TOKENS, &request).await
    }

    /// Revokes a scoped token this plugin minted; whether it was still in force.
    pub async fn revoke_scoped_token(&self, id: Uuid) -> Result<bool, PluginError> {
        let response: RevokeScopedTokenResponse =
            self.post(paths::SCOPED_TOKEN_REVOKE, &RevokeScopedTokenRequest { id }).await?;
        Ok(response.revoked)
    }

    /// Team providers only: who is in one of its teams, leaving people added in DOC alone.
    pub async fn provide_members(
        &self,
        request: TeamMembersRequest,
    ) -> Result<TeamMembersResponse, PluginError> {
        self.post(paths::TEAM_MEMBERS, &request).await
    }

    /// Identity providers only: somebody is gone from the directory this plugin speaks for. Core
    /// says so and nothing more; the offboarding rules decide what follows (T67).
    pub async fn deprovision(
        &self,
        request: DeprovisionRequest,
    ) -> Result<DeprovisionResponse, PluginError> {
        self.post(paths::DEPROVISION, &request).await
    }

    /// With the `offboarding` capability: what to do about somebody who has left — disable
    /// them, take away an account, their team memberships or their tokens.
    pub async fn offboard(
        &self,
        request: OffboardRequest,
    ) -> Result<OffboardResponse, PluginError> {
        self.post(paths::OFFBOARD, &request).await
    }

    /// Team providers only: a team the provider no longer has.
    pub async fn remove_team(
        &self,
        request: TeamRemoveRequest,
    ) -> Result<TeamRemoveResponse, PluginError> {
        self.post(paths::TEAM_REMOVE, &request).await
    }

    /// With the `secret-store` capability: `value` sealed under the platform's settings key and
    /// bound to this plugin and `label`, so it opens for those two and nothing else (FEAT-SECRETS).
    pub async fn seal(
        &self,
        label: &str,
        value: &Secret<String>,
    ) -> Result<SealedValue, PluginError> {
        let request = SealRequest { label: label.to_string(), value: value.clone() };
        let response: SealResponse = self.post(paths::SEAL, &request).await?;
        Ok(response.sealed)
    }

    /// A value `seal` sealed, and whether it was sealed under an older key and wants sealing again.
    pub async fn open(
        &self,
        label: &str,
        sealed: &SealedValue,
    ) -> Result<(Secret<String>, bool), PluginError> {
        let request = OpenRequest { label: label.to_string(), sealed: sealed.clone() };
        let response: OpenResponse = self.post(paths::OPEN, &request).await?;
        Ok((response.value, response.stale))
    }

    /// With the `secret-store` capability: tells core which secrets changed, so every plugin whose
    /// settings point at one reads them again; `loaded` also tells the ones that could not reach it.
    pub async fn secrets_changed(
        &self,
        secrets: Vec<Uuid>,
        loaded: bool,
    ) -> Result<Vec<String>, PluginError> {
        let request = SecretsChangedRequest { secrets, loaded };
        let response: SecretsChangedResponse = self.post(paths::SECRETS_CHANGED, &request).await?;
        Ok(response.told)
    }

    /// With the `team-writer` capability, during a person's call: somebody added by email address,
    /// under the rules `POST /api/v1/people` has (FEAT-PEOPLE). Answers what that route answers.
    pub async fn add_person(&self, request: PersonRequest) -> Result<Value, PluginError> {
        self.post(paths::PEOPLE, &request).await
    }
}
