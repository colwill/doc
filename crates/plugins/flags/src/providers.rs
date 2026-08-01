//! Reading flags from somewhere else. A platform that already has Unleash, Flagsmith, Runcfg or
//! anything speaking OpenFeature's remote protocol points DOC at it, and DOC serves those flags —
//! and Runcfg's runtime configuration — beside its own, which is what lets a service read one
//! endpoint whatever a team is using behind it.

use std::collections::BTreeMap;
use std::time::Duration;

use doc_plugin_sdk::Backend;
use doc_plugin_sdk::telemetry::sent;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::model::{Role, Upstream};
use crate::store::{Provider, Store};

const TIMEOUT: Duration = Duration::from_secs(8);
/// The longest an upstream answer is kept before it is fetched again, whatever a provider says.
const MAX_CACHE: u64 = 600;

/// What an upstream holds: flags, runtime configuration for the one provider that keeps it, and
/// what it sent that DOC does not know to be either, which is suggested until somebody says.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Values {
    pub flags: BTreeMap<String, Value>,
    pub config: BTreeMap<String, Value>,
    #[serde(default)]
    pub suggested: BTreeMap<String, Value>,
}

impl Values {
    fn flags(flags: BTreeMap<String, Value>) -> Self {
        Self { flags, ..Self::default() }
    }

    /// Takes in each suggestion as what somebody said it is: a flag, a setting, or nothing. One
    /// nobody has said anything about stays a suggestion and is not served. What the provider
    /// itself called a flag or a setting wins over a suggestion of the same name.
    pub fn adopted(mut self, adopted: &BTreeMap<String, String>) -> Self {
        for (name, value) in std::mem::take(&mut self.suggested) {
            let Some(taken) = adopted.get(&name) else {
                self.suggested.insert(name, value);
                continue;
            };
            match Role::named(taken) {
                Some(Role::Flag) => {
                    self.flags.entry(name).or_insert(value);
                }
                Some(Role::Config) => {
                    self.config.entry(name).or_insert(value);
                }
                None => {}
            }
        }
        // What somebody said also moves what the provider itself called a flag or a setting.
        for (name, taken) in adopted {
            let (from, to) = match Role::named(taken) {
                Some(Role::Flag) => (&mut self.config, &mut self.flags),
                Some(Role::Config) => (&mut self.flags, &mut self.config),
                None => {
                    self.flags.remove(name);
                    self.config.remove(name);
                    continue;
                }
            };
            if let Some(value) = from.remove(name) {
                to.entry(name.clone()).or_insert(value);
            }
        }
        self
    }

    /// The suggestions as a provider's record keeps them: each one's name and what it held.
    pub fn pending(&self) -> BTreeMap<String, String> {
        let shown = |value: &Value| match value {
            Value::String(text) => clipped(text),
            other => clipped(&other.to_string()),
        };
        self.suggested.iter().map(|(name, value)| (name.clone(), shown(value))).collect()
    }

    /// `3 flags and 1 setting (version)`: what was read, naming the settings while they are few,
    /// so a provider whose answer was taken in as configuration says so.
    pub fn said(&self) -> String {
        let counted = |count: usize, one: &str, many: &str| match count {
            1 => format!("1 {one}"),
            _ => format!("{count} {many}"),
        };
        let flags = counted(self.flags.len(), "flag", "flags");
        let settings = counted(self.config.len(), "setting", "settings");
        match self.config.len() {
            1..=5 => {
                let names: Vec<&str> = self.config.keys().map(String::as_str).collect();
                format!("{flags} and {settings} ({})", names.join(", "))
            }
            _ => format!("{flags} and {settings}"),
        }
    }
}

/// What an upstream gave, and whether it came from the cache.
pub struct Fetched {
    pub values: Values,
    pub cached: bool,
}

/// One provider's flags for a service, from the Cache Bus if they were fetched recently enough.
pub async fn fetch(
    backend: &Backend,
    provider: &Provider,
    service: &str,
    environment: &str,
) -> Result<Fetched, String> {
    let key = format!("upstream:{}:{service}:{environment}", provider.id);
    if let Ok(Some(cached)) = backend.cache_get(&key).await
        && let Ok(values) = serde_json::from_value::<Values>(cached)
    {
        return Ok(Fetched { values: values.adopted(&provider.adopted), cached: true });
    }
    let values = read(backend, provider, service, environment).await?;
    let ttl = (provider.refresh_seconds.max(5) as u64).min(MAX_CACHE);
    let stored = serde_json::to_value(&values).unwrap_or_else(|_| json!({}));
    let _ = backend.cache_set(&key, stored, Some(Duration::from_secs(ttl))).await;
    let values = values.adopted(&provider.adopted);
    remember(backend, provider, &values).await;
    Ok(Fetched { values, cached: false })
}

/// Keeps what a provider suggests on its record, where the Providers page offers it, writing only
/// when that has changed.
async fn remember(backend: &Backend, provider: &Provider, values: &Values) {
    let pending = values.pending();
    if pending != provider.suggested {
        let _ = Store(backend).set_provider(provider.id, json!({ "suggested": pending })).await;
    }
}

/// The credentials the provider names, as the Settings page holds them, in the order its kind
/// reads them. One it names that is not there is a problem, not a request sent without it.
fn credentials(
    backend: &Backend,
    provider: &Provider,
    kind: Upstream,
) -> Result<Vec<String>, String> {
    let named = provider.credentials();
    let slots = kind.credentials();
    if let Some(slot) = slots.iter().skip(named.len()).find(|slot| slot.required) {
        return Err(format!("{} is read with its {}", kind.label(), slot.label));
    }
    named
        .iter()
        .zip(slots)
        .map(|(name, slot)| {
            backend
                .settings()
                .named(name)
                .map(|secret| secret.expose().trim().to_string())
                .ok_or_else(|| {
                    format!("its {} names `{name}`, which is not on the Settings page", slot.label)
                })
        })
        .collect()
}

async fn read(
    backend: &Backend,
    provider: &Provider,
    service: &str,
    environment: &str,
) -> Result<Values, String> {
    let kind = provider
        .kind()
        .ok_or_else(|| format!("`{}` is not a provider DOC reads", provider.kind))?;
    let client = reqwest::Client::builder()
        .timeout(TIMEOUT)
        .user_agent(concat!("doc-flags/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| format!("the client could not be made: {err}"))?;
    let held = credentials(backend, provider, kind)?;
    let token = held.first().cloned();
    let base = provider.url.trim_end_matches('/');
    match kind {
        Upstream::Unleash => {
            let url = format!("{base}/api/client/features");
            let mut asking = client.get(&url).header("accept", "application/json");
            if let Some(token) = token {
                asking = asking.header("authorization", token);
            }
            let answer = asking.send().await;
            sent("unleash", "features", &answer);
            unleash(&read_json(answer, &url, "Unleash").await?).map(Values::flags)
        }
        Upstream::Flagsmith => {
            let url = format!("{base}/api/v1/flags/");
            let mut asking = client.get(&url).header("accept", "application/json");
            if let Some(token) = token {
                asking = asking.header("x-environment-key", token);
            }
            let answer = asking.send().await;
            sent("flagsmith", "flags", &answer);
            flagsmith(&read_json(answer, &url, "Flagsmith").await?).map(Values::flags)
        }
        Upstream::Ofrep => {
            let url = format!("{base}/ofrep/v1/evaluate/flags");
            let context = json!({
                "context": { "targetingKey": service, "service": service, "environment": environment }
            });
            let mut asking = client.post(&url).json(&context);
            if let Some(token) = token {
                asking = asking.bearer_auth(token);
            }
            let answer = asking.send().await;
            sent("ofrep", "evaluate", &answer);
            ofrep(&read_json(answer, &url, "The OFREP provider").await?).map(Values::flags)
        }
        Upstream::Runcfg => {
            let [project, id, secret] = held.as_slice() else {
                return Err(
                    "Runcfg is read with its Project ID, Client ID and Client secret".into()
                );
            };
            let shaped = project
                .chars()
                .all(|letter| letter.is_ascii_alphanumeric() || matches!(letter, '-' | '_'));
            if project.is_empty() || !shaped {
                return Err("its Project ID is not a Runcfg project ID".into());
            }
            let project = format!("{base}/api/v1/sdk/project/{project}");
            let asking = |url: &str| {
                client
                    .get(url)
                    .header("accept", "application/json")
                    .header("x-client-id", id)
                    .header("x-client-secret", secret)
            };
            let url = format!("{project}/flags");
            let answer = asking(&url).send().await;
            sent("runcfg", "flags", &answer);
            let mut values = runcfg_values(&read_json(answer, &url, "Runcfg").await?)?;
            for name in &provider.configs {
                let mut url = url::Url::parse(&format!("{project}/config"))
                    .map_err(|err| format!("its URL is not a URL: {err}"))?;
                url.query_pairs_mut().append_pair("name", name);
                let answer = asking(url.as_str()).send().await;
                sent("runcfg", "config", &answer);
                if let Ok(answered) = &answer
                    && answered.status() == reqwest::StatusCode::NOT_FOUND
                {
                    return Err(format!("Runcfg holds no configuration called {name}"));
                }
                let body = read_json(answer, url.as_str(), "Runcfg").await?;
                values.config.insert(name.to_ascii_lowercase(), runcfg_config(name, &body)?);
            }
            Ok(values)
        }
        Upstream::Http => {
            let mut asking = client.get(base).header("accept", "application/json");
            if let Some(token) = token {
                asking = asking.bearer_auth(token);
            }
            let answer = asking.send().await;
            sent("flags-http", "get", &answer);
            plain(&read_json(answer, base, "The endpoint").await?).map(Values::flags)
        }
    }
}

async fn read_json(
    answer: Result<reqwest::Response, reqwest::Error>,
    url: &str,
    who: &str,
) -> Result<Value, String> {
    let answer = answer.map_err(|err| format!("{who} could not be reached: {err}"))?;
    let status = answer.status();
    if !status.is_success() {
        // What it said about it, when it said it as `{"error": …}` or `{"message": …}`.
        let body: Value = answer.json().await.unwrap_or_default();
        let said = body["error"].as_str().or_else(|| body["message"].as_str());
        return Err(match said.map(str::trim).filter(|said| !said.is_empty()) {
            Some(said) => format!("{who} answered {status} to {url}: {}", clipped(said)),
            None => format!("{who} answered {status} to {url}"),
        });
    }
    answer.json().await.map_err(|err| format!("{who} did not answer JSON: {err}"))
}

/// What a provider said, cut short enough to sit in one line of a page.
fn clipped(said: &str) -> String {
    const MOST: usize = 200;
    match said.char_indices().nth(MOST) {
        Some((at, _)) => format!("{}…", &said[..at]),
        None => said.to_string(),
    }
}

/// Unleash's client API: every enabled toggle, with a variant's payload where it has one.
fn unleash(body: &Value) -> Result<BTreeMap<String, Value>, String> {
    let features = body["features"].as_array().ok_or("Unleash answered without any features")?;
    let mut values = BTreeMap::new();
    for feature in features {
        let Some(name) = feature["name"].as_str() else { continue };
        let enabled = feature["enabled"].as_bool().unwrap_or(false);
        let payload = feature["variants"]
            .as_array()
            .and_then(|variants| variants.first())
            .and_then(|variant| variant["payload"]["value"].as_str());
        let value = match (enabled, payload) {
            (true, Some(payload)) => Value::String(payload.to_string()),
            (enabled, _) => Value::Bool(enabled),
        };
        values.insert(name.to_ascii_lowercase(), value);
    }
    Ok(values)
}

/// Flagsmith's environment flags: its value when it has one, and whether it is on when it has not.
fn flagsmith(body: &Value) -> Result<BTreeMap<String, Value>, String> {
    let flags = match body {
        Value::Array(flags) => flags.clone(),
        other => other["results"].as_array().cloned().unwrap_or_default(),
    };
    let mut values = BTreeMap::new();
    for flag in flags {
        let Some(name) = flag["feature"]["name"].as_str() else { continue };
        let enabled = flag["enabled"].as_bool().unwrap_or(false);
        let value = match (&flag["feature_state_value"], enabled) {
            (Value::Null, enabled) => Value::Bool(enabled),
            (value, _) => value.clone(),
        };
        values.insert(name.to_ascii_lowercase(), value);
    }
    Ok(values)
}

/// OpenFeature's remote evaluation protocol, which is what DOC serves as well.
fn ofrep(body: &Value) -> Result<BTreeMap<String, Value>, String> {
    let flags = body["flags"].as_array().ok_or("the provider answered without any flags")?;
    let mut values = BTreeMap::new();
    for flag in flags {
        let Some(key) = flag["key"].as_str() else { continue };
        values.insert(key.to_ascii_lowercase(), flag["value"].clone());
    }
    Ok(values)
}

/// What Runcfg answered for a project. Its answer is what exists, so nothing in it is refused for
/// being in a shape DOC did not know. What it sends is made of entries — `{"key", "value",
/// "value_type", "enabled"}` — whether each is a field of the answer named after its key, or in a
/// `flags` list or map, or a few levels down; its configurations come beside them. A field that is
/// none of those is data too, but whether it is a flag or a setting is somebody's to say, so it is
/// suggested rather than served. A field of secrets is never read.
fn runcfg_values(body: &Value) -> Result<Values, String> {
    const DEPTH: usize = 3;
    let body = unwrapped(body);
    let mut values = Values::default();
    let fields = match &body {
        Value::Array(_) => {
            entries(&mut values, &body)?;
            return Ok(values);
        }
        Value::Object(fields) => fields,
        _ => return Err(format!("Runcfg answered its flags as {}", shape(&body))),
    };
    if let Some(said) = said(&body) {
        return Err(format!("Runcfg answered: {}", clipped(said)));
    }
    for (name, held) in fields {
        match bare(name).as_str() {
            "flags" => entries(&mut values, held)?,
            "configs" | "configurations" => values.config.extend(runcfg_configs(held)),
            "secrets" | "secret" => {}
            _ if is_entry(held) => entry(&mut values, Some(name), held),
            _ => {
                let flags = deep(held, "flags", DEPTH);
                let configs =
                    deep(held, "configs", DEPTH).or_else(|| deep(held, "configurations", DEPTH));
                if flags.is_none() && configs.is_none() {
                    values.suggested.insert(name.to_ascii_lowercase(), held.clone());
                    continue;
                }
                if let Some(flags) = flags {
                    entries(&mut values, flags)?;
                }
                if let Some(configs) = configs {
                    values.config.extend(runcfg_configs(configs));
                }
            }
        }
    }
    Ok(values)
}

/// Whether a field is one of Runcfg's entries: typed by `value_type`, or a `key` that is on or off.
fn is_entry(held: &Value) -> bool {
    named(held, "value_type").is_some_and(Value::is_string)
        || (named(held, "key").is_some_and(Value::is_string)
            && named(held, "enabled").is_some_and(Value::is_boolean))
}

/// A list of Runcfg's entries, or a map of them by key.
fn entries(values: &mut Values, held: &Value) -> Result<(), String> {
    match held {
        Value::Array(listed) => listed.iter().for_each(|one| entry(values, None, one)),
        Value::Object(mapped) => mapped
            .iter()
            .filter(|(name, _)| !matches!(bare(name).as_str(), "secrets" | "secret"))
            .for_each(|(name, one)| entry(values, Some(name), one)),
        Value::Null => {}
        other => return Err(format!("Runcfg answered its flags as {}", shape(other))),
    }
    Ok(())
}

/// One of Runcfg's entries, served by its `value_type`: `bool`, `int`, `float` and `string` as a
/// flag, `json` and `yaml` as a setting. One that is off is left out, so a service falls back to
/// its own default as Runcfg's SDKs do, and a secret is never served or shown. The value is in
/// `value`, or in the field of its type (`value_bool`); one with none is a switch, and on. A plain
/// value under a name is a flag of that value.
fn entry(values: &mut Values, mapped: Option<&str>, one: &Value) {
    let key = named(one, "key").and_then(Value::as_str).or(mapped);
    let Some(key) = key.map(str::to_ascii_lowercase) else { return };
    if !one.is_object() {
        values.flags.insert(key, one.clone());
        return;
    }
    let kind = named(one, "value_type").and_then(Value::as_str).unwrap_or_default();
    let kind = kind.to_ascii_lowercase();
    let marked = |field: &str| named(one, field).and_then(Value::as_bool).unwrap_or(false);
    let secret = kind.contains("secret")
        || kind == "encrypted"
        || ["secret", "encrypted", "sensitive"].into_iter().any(marked);
    if secret || !named(one, "enabled").and_then(Value::as_bool).unwrap_or(true) {
        return;
    }
    let typed = |field: &str| {
        named(one, field).or_else(|| named(one, "value")).filter(|held| !held.is_null())
    };
    if matches!(kind.as_str(), "json" | "yaml" | "object" | "config" | "configuration") {
        let setting = match typed("value") {
            Some(Value::String(written)) => {
                serde_yaml_ng::from_str(written).unwrap_or_else(|_| json!(written))
            }
            Some(held) => held.clone(),
            None => return,
        };
        values.config.insert(key, setting);
        return;
    }
    let flag = match kind.as_str() {
        "bool" | "boolean" => typed("value_bool").and_then(Value::as_bool).map(Value::Bool),
        "int" | "integer" => typed("value_int").and_then(Value::as_i64).map(|n| json!(n)),
        "float" | "double" | "number" => {
            typed("value_float").and_then(Value::as_f64).map(|n| json!(n))
        }
        "string" | "text" => typed("value_string").and_then(Value::as_str).map(|s| json!(s)),
        _ => None,
    };
    let flag = flag.or_else(|| typed("value").cloned()).unwrap_or(Value::Bool(true));
    values.flags.insert(key, flag);
}

/// Runcfg's configurations when it sends them beside its flags: a list of them by `name`, or a
/// map by name, each one's YAML under `content` served as the value it describes.
fn runcfg_configs(held: &Value) -> BTreeMap<String, Value> {
    let listed: Vec<(Option<&str>, &Value)> = match held {
        Value::Array(configs) => configs.iter().map(|config| (None, config)).collect(),
        Value::Object(configs) => {
            configs.iter().map(|(name, config)| (Some(name.as_str()), config)).collect()
        }
        _ => Vec::new(),
    };
    let mut values = BTreeMap::new();
    for (mapped, config) in listed {
        let name = named(config, "name").or_else(|| named(config, "key")).and_then(Value::as_str);
        let Some(name) = name.or(mapped).map(str::to_ascii_lowercase) else { continue };
        let content = match config {
            Value::Object(_) => named(config, "content").or_else(|| named(config, "value")),
            _ => Some(config),
        };
        let value = match content {
            Some(Value::String(written)) => {
                serde_yaml_ng::from_str(written).unwrap_or_else(|_| json!(written))
            }
            Some(other) => other.clone(),
            None => config.clone(),
        };
        values.insert(name, value);
    }
    values
}

/// One of Runcfg's configurations: YAML under `content`, served as the value it describes.
fn runcfg_config(name: &str, body: &Value) -> Result<Value, String> {
    let body = unwrapped(body);
    let content = named(&body, "content")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Runcfg answered {name} as {}", shape(&body)))?;
    serde_yaml_ng::from_str(content).map_err(|err| format!("Runcfg's {name} is not YAML: {err}"))
}

/// A field, or the first field of that name a few levels into the objects it holds.
fn deep<'a>(body: &'a Value, field: &str, depth: usize) -> Option<&'a Value> {
    named(body, field).or_else(|| match depth {
        0 => None,
        _ => body.as_object()?.values().find_map(|inner| deep(inner, field, depth - 1)),
    })
}

/// What an answer said went wrong, when it is `{"error": …}` or `{"message": …}` and nothing else.
fn said(body: &Value) -> Option<&str> {
    let fields = body.as_object()?;
    let said = named(body, "error").or_else(|| named(body, "message"))?.as_str()?;
    (fields.len() == 1).then_some(said)
}

/// An answer that is JSON written as a JSON string, read as the JSON it holds.
fn unwrapped(body: &Value) -> Value {
    match body {
        Value::String(written) => serde_json::from_str(written).unwrap_or_else(|_| body.clone()),
        other => other.clone(),
    }
}

/// A field whatever its case: `value_type`, `valueType` and `ValueType` are the same field.
fn named<'a>(object: &'a Value, field: &str) -> Option<&'a Value> {
    let wanted = bare(field);
    object.as_object()?.iter().find(|(name, _)| bare(name) == wanted).map(|(_, value)| value)
}

/// A field's name as `named` compares it: lower case, without underscores.
fn bare(name: &str) -> String {
    name.replace('_', "").to_ascii_lowercase()
}

/// What an answer looked like, without what it holds, for when it is not what was expected.
fn shape(body: &Value) -> String {
    match body {
        Value::Object(fields) => {
            if let Some(said) =
                named(body, "error").or_else(|| named(body, "message")).and_then(Value::as_str)
            {
                return format!("an error: {}", clipped(said));
            }
            let names: Vec<&str> = fields.keys().map(String::as_str).take(12).collect();
            match names.is_empty() {
                true => "an empty object".to_string(),
                false => format!("an object of {}", names.join(", ")),
            }
        }
        Value::Array(items) => format!("a list of {}", items.len()),
        Value::String(_) => "text that is not JSON".to_string(),
        Value::Number(_) => "a number".to_string(),
        Value::Bool(_) => "true or false".to_string(),
        Value::Null => "null".to_string(),
    }
}

/// A plain JSON object of keys and values, or one under `flags`.
fn plain(body: &Value) -> Result<BTreeMap<String, Value>, String> {
    let object = match &body["flags"] {
        Value::Object(flags) => flags.clone(),
        _ => body.as_object().cloned().ok_or("the endpoint did not answer a JSON object")?,
    };
    Ok(object.into_iter().map(|(key, value)| (key.to_ascii_lowercase(), value)).collect())
}

/// Reads a provider once, without the cache, for **Test connection** and the Providers page.
pub async fn check(
    backend: &Backend,
    provider: &Provider,
    service: &str,
    environment: &str,
) -> Result<Values, String> {
    read(backend, provider, service, environment)
        .await
        .map(|values| values.adopted(&provider.adopted))
}
