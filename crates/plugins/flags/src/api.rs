//! The API at `/api/v1/plugins/flags/api/…`. A running service reads `evaluate`, or one of the two
//! protocols it already has a client for: OpenFeature's `ofrep/v1/evaluate/flags`, which its SDKs
//! speak without knowing anything about DOC, and `flagd/v0/flags.json`, the flag definition flagd
//! and the in-process OpenFeature providers read instead of asking for an evaluation. Everything
//! else is for whoever manages the flags.

use doc_plugin_sdk::{Backend, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::model::{ANY, Kind, MAX_DESCRIPTION, Precedence, Role, UPSTREAMS, Upstream, key, scope};
use crate::store::{Provider, Store, Writing};
use crate::{DEFAULT_ENVIRONMENT, READER_PLUGINS, REFRESH, Refusal, evaluate, providers};

pub fn query(request: &Request, name: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn ok(value: &Value) -> Result<Response, Refusal> {
    Ok(Response::json(value))
}

/// Who a call is about: the service and environment it named, or the platform's defaults.
pub fn asked_about(backend: &Backend, request: &Request, body: Option<&Value>) -> (String, String) {
    let from_body = |field: &str| {
        body.and_then(|body| {
            body["context"][field].as_str().or_else(|| body[field].as_str()).map(str::to_string)
        })
    };
    let service = query(request, "service")
        .or_else(|| from_body("service"))
        .or_else(|| from_body("targetingKey"))
        .unwrap_or_else(|| ANY.to_string());
    let environment = query(request, "environment")
        .or_else(|| from_body("environment"))
        .unwrap_or_else(|| default_environment(backend));
    (service.to_ascii_lowercase(), environment.to_ascii_lowercase())
}

pub fn default_environment(backend: &Backend) -> String {
    backend.settings().some_text(DEFAULT_ENVIRONMENT).unwrap_or_else(|| "production".to_string())
}

fn refresh(backend: &Backend) -> i64 {
    backend.settings().integer(REFRESH).unwrap_or(30).clamp(5, 3_600)
}

/// What a caller sent, as a form field or a JSON body.
#[derive(Debug, Deserialize)]
struct Written {
    #[serde(default)]
    role: Option<String>,
    key: String,
    #[serde(default)]
    service: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    value: Value,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    enabled: Option<bool>,
}

impl Written {
    fn into_writing(self) -> Result<Writing, Refusal> {
        let role = self
            .role
            .as_deref()
            .map(|role| Role::named(role).ok_or_else(|| Refusal::bad("a role is flag or config")))
            .transpose()?
            .unwrap_or(Role::Flag);
        let kind = self
            .kind
            .as_deref()
            .map(|kind| {
                Kind::named(kind)
                    .ok_or_else(|| Refusal::bad("a kind is boolean, string, number or json"))
            })
            .transpose()?
            .unwrap_or(Kind::Boolean);
        let value = match (&self.value, kind) {
            (Value::String(written), kind) if kind != Kind::String => {
                kind.read(written).map_err(Refusal::bad)?
            }
            (value, kind) if kind.holds(value) => value.clone(),
            (Value::Null, Kind::Boolean) => Value::Bool(false),
            (value, kind) => {
                return Err(Refusal::bad(format!("{value} is not a {}", kind.as_str())));
            }
        };
        Ok(Writing {
            role,
            key: key(&self.key).map_err(Refusal::bad)?,
            service: scope(self.service.as_deref().unwrap_or(ANY), "a service")
                .map_err(Refusal::bad)?,
            environment: scope(self.environment.as_deref().unwrap_or(ANY), "an environment")
                .map_err(Refusal::bad)?,
            kind,
            value,
            description: self
                .description
                .unwrap_or_default()
                .trim()
                .chars()
                .take(MAX_DESCRIPTION)
                .collect(),
            enabled: self.enabled.unwrap_or(true),
        })
    }
}

pub async fn handle(backend: &Backend, request: Request) -> Response {
    let ofrep = request.path.trim_start_matches("api/").starts_with("ofrep/");
    match route(backend, &request).await {
        Ok(response) => response,
        // An OpenFeature SDK reads a refusal by its `errorCode`; problem+json means nothing to it.
        Err(refusal) if ofrep => ofrep_refusal(&refusal),
        Err(refusal) => refusal.response(),
    }
}

/// An answer that is polled: it carries the resolved version as its ETag, and a caller that
/// already holds that version is told so rather than sent the whole thing again.
fn tagged(body: &Value, version: &str, request: &Request) -> Response {
    let etag = format!("\"{version}\"");
    let known = request.headers.get("if-none-match").map(|held| held.trim_matches('"'));
    if known == Some(version) {
        return Response::new(304, "application/json", Vec::new()).with_header("etag", &etag);
    }
    Response::new(200, "application/json", body.to_string())
        .with_header("etag", &etag)
        .with_header("cache-control", "no-cache")
}

/// A refusal in OpenFeature's own shape, which is what its SDKs read: an `errorCode` they know,
/// the detail beside it, and the key where a caller asked about one flag.
fn ofrep_problem(status: u16, code: &str, detail: &str, key: Option<&str>) -> Response {
    let mut body = json!({ "errorCode": code, "errorDetails": detail });
    if let Some(key) = key {
        body["key"] = json!(key);
    }
    Response::new(status, "application/json", body.to_string())
}

/// What anything refused on an OFREP route becomes. The protocol has no answer for a platform that
/// cannot reach its own store, so what DOC calls a 503 is the protocol's 500, with the cause kept.
fn ofrep_refusal(refusal: &Refusal) -> Response {
    let (status, code) = match refusal.status {
        400 => (400, "INVALID_CONTEXT"),
        404 => (404, "FLAG_NOT_FOUND"),
        status if status < 500 => (status, "GENERAL"),
        _ => (500, "GENERAL"),
    };
    ofrep_problem(status, code, &refusal.detail, None)
}

/// The evaluation context a caller sent. Every OpenFeature SDK sends one, so a body that is not
/// JSON is the protocol's `PARSE_ERROR` rather than something to shrug at.
fn ofrep_context(request: &Request) -> Result<Value, Response> {
    match request.body.is_empty() {
        true => Ok(Value::Null),
        false => request.json::<Value>().map_err(|err| {
            ofrep_problem(400, "PARSE_ERROR", &format!("that is not JSON: {err}"), None)
        }),
    }
}

async fn route(backend: &Backend, request: &Request) -> Result<Response, Refusal> {
    let store = Store(backend);
    let path = request.path.trim_start_matches("api/").trim_end_matches('/');
    let parts: Vec<&str> = path.split('/').collect();
    match (request.method.as_str(), parts.as_slice()) {
        // What the platform itself reads, as the service it names, for the plugins it turns on
        // and off by a flag. Only the platform asks it.
        ("POST", ["internal", "platform"]) => {
            if backend.caller().is_none_or(|caller| caller.kind != "platform") {
                return Err(Refusal::forbidden("internal routes answer core alone"));
            }
            let body: Value = request.json().unwrap_or_default();
            let service = body["service"].as_str().unwrap_or("doc").to_ascii_lowercase();
            let environment = default_environment(backend);
            let resolved = evaluate::resolve(backend, &service, &environment).await?;
            ok(&resolved.payload(refresh(backend)))
        }
        // What another plugin reads as itself: as the service named after it unless it names
        // another, for the plugins the requestable list of readers names. One left out is told so
        // in a way the SDK recognises, and asks to be added.
        ("GET", ["discovery", "evaluate"]) => {
            let asking = match backend.caller() {
                Some(caller) if caller.kind == "plugin" => caller.id.clone().unwrap_or_default(),
                _ => return Err(Refusal::forbidden("discovery routes are for plugins")),
            };
            if !backend.settings().list(READER_PLUGINS).contains(&asking) {
                let detail = format!(
                    "{asking} is not one of the plugins that read flags; it asks to be added to \
                     {READER_PLUGINS}"
                );
                return Ok(Response::problem(403, "not-listed", &detail));
            }
            let service = query(request, "service").unwrap_or(asking);
            let service = scope(&service, "a service").map_err(Refusal::bad)?;
            let environment = query(request, "environment")
                .map(|environment| scope(&environment, "an environment"))
                .transpose()
                .map_err(Refusal::bad)?
                .unwrap_or_else(|| default_environment(backend));
            let resolved = evaluate::resolve(backend, &service, &environment).await?;
            ok(&resolved.payload(refresh(backend)))
        }
        // What a running service reads. The ETag makes a poll cost a `304` and nothing else.
        ("GET", ["evaluate"]) => {
            let (service, environment) = asked_about(backend, request, None);
            let resolved = evaluate::resolve(backend, &service, &environment).await?;
            Ok(tagged(&resolved.payload(refresh(backend)), &resolved.version, request))
        }
        // OpenFeature's remote evaluation protocol, so any OpenFeature SDK can read DOC directly.
        // Its bulk answer is a static evaluation of everything one service sees, which is what the
        // ETag is of: polling it costs a `304`, the same as `evaluate`.
        ("POST", ["ofrep", "v1", "evaluate", "flags"]) => {
            let body = match ofrep_context(request) {
                Ok(body) => body,
                Err(refused) => return Ok(refused),
            };
            let (service, environment) = asked_about(backend, request, Some(&body));
            let resolved = evaluate::resolve(backend, &service, &environment).await?;
            Ok(tagged(&resolved.open_feature(), &resolved.version, request))
        }
        ("POST", ["ofrep", "v1", "evaluate", "flags", wanted]) => {
            let body = match ofrep_context(request) {
                Ok(body) => body,
                Err(refused) => return Ok(refused),
            };
            let (service, environment) = asked_about(backend, request, Some(&body));
            let resolved = evaluate::resolve(backend, &service, &environment).await?;
            let wanted = wanted.to_ascii_lowercase();
            match resolved.held(&wanted) {
                Some(value) => ok(&resolved.evaluated(&wanted, value)),
                None => Ok(ofrep_problem(
                    404,
                    "FLAG_NOT_FOUND",
                    &format!("{service} has no flag called {wanted}"),
                    Some(&wanted),
                )),
            }
        }
        // What flagd reads. It evaluates for itself rather than asking, so it is given the flag
        // definition for one service in one environment; an in-process OpenFeature provider that
        // polls a URL reads the same document. The ETag makes that poll a `304` too.
        ("GET", ["flagd", "v0", "flags.json"]) => {
            let (service, environment) = asked_about(backend, request, None);
            let resolved = evaluate::resolve(backend, &service, &environment).await?;
            Ok(tagged(&resolved.flagd(), &resolved.version, request))
        }
        ("GET", ["entries"]) => {
            let role = query(request, "role").and_then(|role| Role::named(&role));
            let entries = store
                .entries(role, query(request, "service").as_deref(), query(request, "q").as_deref())
                .await?;
            ok(&json!({ "entries": entries.iter().map(evaluate::shown).collect::<Vec<_>>() }))
        }
        ("POST", ["entries"]) => {
            let written: Written = request.json().map_err(|err| Refusal::bad(err.to_string()))?;
            let writing = written.into_writing()?;
            guarded(backend, &writing.environment)?;
            let entry = store.write(&writing, &crate::who(backend)).await?;
            announce(backend, &entry.key, &entry.service, &entry.environment).await;
            ok(&evaluate::shown(&entry))
        }
        ("GET", ["entries", id]) => {
            let entry = found(&store, id).await?;
            ok(&evaluate::shown(&entry))
        }
        ("PATCH", ["entries", id]) => {
            let entry = found(&store, id).await?;
            guarded(backend, &entry.environment)?;
            let body: Value = request.json().map_err(|err| Refusal::bad(err.to_string()))?;
            let mut set = serde_json::Map::new();
            if let Some(enabled) = body.get("enabled").and_then(Value::as_bool) {
                set.insert("enabled".into(), json!(enabled));
            }
            if let Some(description) = body.get("description").and_then(Value::as_str) {
                set.insert("description".into(), json!(description.trim()));
            }
            if let Some(value) = body.get("value") {
                let kind = entry.kind();
                let value = match value {
                    Value::String(written) if kind != Kind::String => {
                        kind.read(written).map_err(Refusal::bad)?
                    }
                    value if kind.holds(value) => value.clone(),
                    value => {
                        return Err(Refusal::bad(format!("{value} is not a {}", kind.as_str())));
                    }
                };
                set.insert("value".into(), value);
            }
            if set.is_empty() {
                return Err(Refusal::bad("name what to change: value, enabled or description"));
            }
            set.insert("updated_by".into(), json!(crate::who(backend)));
            let changed = store
                .set(entry.id, Value::Object(set))
                .await?
                .ok_or_else(|| Refusal::missing("there is no such flag"))?;
            announce(backend, &changed.key, &changed.service, &changed.environment).await;
            ok(&evaluate::shown(&changed))
        }
        ("DELETE", ["entries", id]) => {
            let entry = found(&store, id).await?;
            guarded(backend, &entry.environment)?;
            store.remove(entry.id).await?;
            announce(backend, &entry.key, &entry.service, &entry.environment).await;
            Ok(Response::new(204, "application/json", Vec::new()))
        }
        ("GET", ["providers"]) => {
            let providers = store.providers().await?;
            ok(&json!({ "providers": providers.iter().map(shown_provider).collect::<Vec<_>>() }))
        }
        ("POST", ["providers"]) => {
            let body: Value = request.json().map_err(|err| Refusal::bad(err.to_string()))?;
            let values = provider_values(&body)?;
            guarded(backend, values["environment"].as_str().unwrap_or(ANY))?;
            let provider = store.write_provider(values).await?;
            ok(&shown_provider(&provider))
        }
        ("DELETE", ["providers", id]) => {
            let id =
                Uuid::parse_str(id).map_err(|_| Refusal::bad("a provider is named by its id"))?;
            if let Some(provider) = store.provider(id).await? {
                guarded(backend, &provider.environment)?;
            }
            match store.remove_provider(id).await? {
                true => Ok(Response::new(204, "application/json", Vec::new())),
                false => Err(Refusal::missing("there is no such provider")),
            }
        }
        // Reads one provider now, without the cache, and says how many flags it holds.
        ("POST", ["providers", id, "check"]) => {
            let id =
                Uuid::parse_str(id).map_err(|_| Refusal::bad("a provider is named by its id"))?;
            let provider = store
                .provider(id)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such provider"))?;
            let environment = default_environment(backend);
            match providers::check(backend, &provider, ANY, &environment).await {
                Ok(held) => {
                    let _ = store
                        .set_provider(provider.id, json!({ "suggested": held.pending() }))
                        .await;
                    ok(&json!({
                        "provider": provider.name,
                        "flags": held.flags.len(),
                        "settings": held.config.len(),
                        "suggested": held.pending(),
                    }))
                }
                Err(problem) => Err(Refusal::bad(problem)),
            }
        }
        // Says what a field the provider sends is, so it is served as that from now on.
        ("POST", ["providers", id, "adopt"]) => {
            let body: Value = request.json().map_err(|err| Refusal::bad(err.to_string()))?;
            let field = body["field"].as_str().unwrap_or_default();
            let taken = body["as"].as_str().unwrap_or_default();
            let (provider, _) = adopt(backend, id, field, taken).await?;
            ok(&shown_provider(&provider))
        }
        ("GET", ["services"]) => ok(&json!({ "services": store.services().await? })),
        _ => Ok(Response::not_found()),
    }
}

/// Refuses a change in `environment` that the call's guard does not allow. A call limited to
/// development and test, such as a runbook Agent Smith runs there, never changes production, and
/// `*` is every environment, production among them (FEAT-AGENT).
fn guarded(backend: &Backend, environment: &str) -> Result<(), Refusal> {
    let Some(caller) = backend.caller() else { return Ok(()) };
    let named = Some(environment.trim()).filter(|named| *named != ANY && !named.is_empty());
    if caller.may_change(named) {
        return Ok(());
    }
    let what = named.map_or_else(|| "every environment".to_string(), str::to_string);
    Err(Refusal::forbidden(format!(
        "this call may not change flags in {what}: whoever made it limited it to development and \
         test, as a runbook run there is"
    )))
}

/// Every change is announced, so a service that would rather be told than poll can subscribe, and
/// an automation can act on one.
pub async fn announce(backend: &Backend, key: &str, service: &str, environment: &str) {
    let _ = backend
        .publish(
            "plugin.flags.entry.changed",
            json!({ "key": key, "service": service, "environment": environment }),
        )
        .await;
    let _ = backend.publish("plugin.flags.ui.entries", json!({ "key": key })).await;
}

pub fn shown_provider(provider: &Provider) -> Value {
    json!({
        "id": provider.id,
        "name": provider.name,
        "kind": provider.kind,
        "url": provider.url,
        "environment": provider.environment,
        "services": provider.services,
        "precedence": provider.precedence,
        "credential": provider.credential,
        "credentials": provider.credentials(),
        "configs": provider.configs,
        "suggested": provider.suggested,
        "adopted": provider.adopted,
        "enabled": provider.enabled,
        "refresh_seconds": provider.refresh_seconds,
        "checked_at": provider.checked_at,
        "problem": provider.problem,
    })
}

pub fn provider_values(body: &Value) -> Result<Value, Refusal> {
    let name = scope(body["name"].as_str().unwrap_or_default(), "a name").map_err(Refusal::bad)?;
    if name == ANY {
        return Err(Refusal::bad("a provider needs a name"));
    }
    let kind = Upstream::named(body["kind"].as_str().unwrap_or_default()).ok_or_else(|| {
        let kinds: Vec<&str> = UPSTREAMS.iter().map(|kind| kind.as_str()).collect();
        Refusal::bad(format!("a provider is one of {}", kinds.join(", ")))
    })?;
    let url = match (body["url"].as_str().unwrap_or_default().trim(), kind.default_url()) {
        ("", Some(hosted)) => hosted.to_string(),
        (written, _) => written.to_string(),
    };
    if url::Url::parse(&url).is_err() {
        return Err(Refusal::bad("a provider's URL is a URL"));
    }
    let credential = credentials(kind, body)?;
    let configs = configs(kind, &body["configs"])?;
    let precedence = Precedence::named(body["precedence"].as_str().unwrap_or("doc"))
        .ok_or_else(|| Refusal::bad("precedence is doc or upstream"))?;
    let services: Vec<String> = match &body["services"] {
        Value::Array(named) => named
            .iter()
            .filter_map(Value::as_str)
            .map(|service| scope(service, "a service"))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Refusal::bad)?,
        Value::String(named) => named
            .split(',')
            .map(str::trim)
            .filter(|named| !named.is_empty())
            .map(|service| scope(service, "a service"))
            .collect::<Result<Vec<_>, _>>()
            .map_err(Refusal::bad)?,
        _ => Vec::new(),
    };
    Ok(json!({
        "name": name,
        "kind": kind.as_str(),
        "url": url.trim_end_matches('/'),
        "environment": scope(body["environment"].as_str().unwrap_or(ANY), "an environment")
            .map_err(Refusal::bad)?,
        "services": services,
        "precedence": precedence.as_str(),
        "credential": credential,
        "configs": configs,
        "enabled": body["enabled"].as_bool().unwrap_or(true),
        "refresh_seconds": body["refresh_seconds"].as_i64().unwrap_or(60).clamp(5, 3_600),
    }))
}

/// The credentials a provider names, in the order its kind reads them: `credentials` as a list, or
/// `credential` as one name or several separated by commas. They are kept as that one text.
fn credentials(kind: Upstream, body: &Value) -> Result<Option<String>, Refusal> {
    let named: Vec<String> = match &body["credentials"] {
        Value::Array(named) => {
            named.iter().map(|one| one.as_str().unwrap_or_default().into()).collect()
        }
        _ => body["credential"].as_str().unwrap_or_default().split(',').map(String::from).collect(),
    };
    let named: Vec<String> = named.iter().map(|one| one.trim().to_ascii_lowercase()).collect();
    let slots = kind.credentials();
    for (at, slot) in slots.iter().enumerate() {
        if slot.required && named.get(at).is_none_or(String::is_empty) {
            return Err(Refusal::bad(format!("{} is read with its {}", kind.label(), slot.label)));
        }
    }
    let named: Vec<String> = named.into_iter().filter(|one| !one.is_empty()).collect();
    if named.len() > slots.len() {
        let wanted: Vec<&str> = slots.iter().map(|slot| slot.label).collect();
        return Err(Refusal::bad(format!("{} is read with: {}", kind.label(), wanted.join(", "))));
    }
    Ok((!named.is_empty()).then(|| named.join(", ")))
}

/// Says what a field a provider sends is — `flag`, `config` or `ignored` — or `forget`s what was
/// said, so it is suggested again the next time the provider is read. Answers the provider as it
/// now is and a sentence saying what changed.
pub async fn adopt(
    backend: &Backend,
    id: &str,
    field: &str,
    taken: &str,
) -> Result<(Provider, String), Refusal> {
    let store = Store(backend);
    let id = Uuid::parse_str(id).map_err(|_| Refusal::bad("a provider is named by its id"))?;
    let provider =
        store.provider(id).await?.ok_or_else(|| Refusal::missing("there is no such provider"))?;
    guarded(backend, &provider.environment)?;
    let field = field.trim().to_ascii_lowercase();
    let name = &provider.name;
    let mut adopted = provider.adopted.clone();
    let mut suggested = provider.suggested.clone();
    let said = match taken {
        "flag" | "config" | "ignored" => {
            if !suggested.contains_key(&field) && !adopted.contains_key(&field) {
                return Err(Refusal::missing(format!("{name} has not sent `{field}`")));
            }
            adopted.insert(field.clone(), taken.to_string());
            suggested.remove(&field);
            match taken {
                "flag" => format!("{field} from {name} is served as a flag."),
                "config" => format!("{field} from {name} is served as a setting."),
                _ => format!("{field} from {name} is ignored."),
            }
        }
        "forget" => {
            if adopted.remove(&field).is_none() {
                return Err(Refusal::missing(format!("nothing was said about `{field}`")));
            }
            format!("{field} is suggested again the next time {name} is read.")
        }
        _ => {
            return Err(Refusal::bad(
                "a field is taken in as flag, config or ignored, or forgotten",
            ));
        }
    };
    let provider = store
        .set_provider(id, json!({ "adopted": adopted, "suggested": suggested }))
        .await?
        .ok_or_else(|| Refusal::missing("there is no such provider"))?;
    announce(backend, &field, ANY, &provider.environment).await;
    Ok((provider, said))
}

/// The configurations a provider reads by name, as a list or separated by commas. A kind that
/// holds none keeps none, whatever was sent.
fn configs(kind: Upstream, sent: &Value) -> Result<Vec<String>, Refusal> {
    const MAX_CONFIGS: usize = 20;
    if !kind.reads_configs() {
        return Ok(Vec::new());
    }
    let named: Vec<&str> = match sent {
        Value::Array(named) => named.iter().filter_map(Value::as_str).collect(),
        Value::String(named) => named.split(',').collect(),
        _ => Vec::new(),
    };
    let mut configs: Vec<String> = Vec::new();
    for name in named.into_iter().map(str::trim).filter(|name| !name.is_empty()) {
        if name.len() > 128 || name.chars().any(char::is_control) {
            return Err(Refusal::bad(format!("`{name}` is not a configuration's name")));
        }
        if !configs.iter().any(|held| held == name) {
            configs.push(name.to_string());
        }
    }
    if configs.len() > MAX_CONFIGS {
        return Err(Refusal::bad(format!("a provider reads at most {MAX_CONFIGS} configurations")));
    }
    Ok(configs)
}

pub async fn found(store: &Store<'_>, id: &str) -> Result<crate::store::Entry, Refusal> {
    let id = Uuid::parse_str(id).map_err(|_| Refusal::bad("a flag is named by its id"))?;
    store.entry(id).await?.ok_or_else(|| Refusal::missing("there is no such flag"))
}
