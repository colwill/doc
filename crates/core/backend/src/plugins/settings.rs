//! Every plugin's settings and features (ADR-0007): what a value is allowed to be, where one
//! comes from, how a secret is kept, and what happens to the plugin when any of it changes.
//!
//! A value is resolved in one order and only one: what an administrator stored, then the
//! environment (or a file it names), then the plugin's declared default. **What is set here wins**,
//! so a deployment's `DOC_<PLUGIN>_<KEY>` is where a setting starts rather than where it is stuck:
//! the page shows that value in the field, says which variable it came from, and saving stores an
//! override. Clearing the field falls back to the variable again, so a platform that keeps its
//! credentials in Vault, Kubernetes or Docker secrets loses nothing by this. Secrets never come
//! back out: the database holds ciphertext, and only the plugin that declared a secret is ever
//! given its value.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use doc_eventbus::{Event, Topic};
use doc_plugin_protocol::calls::{SettingsChanged, SettingsCheck, SettingsVerdict, SettingsView};
use doc_plugin_protocol::{Capability, Manifest, Secret, Setting, SettingKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::api::AppState;
use crate::db::repositories::{SettingChange, StoredSetting};
use crate::identity::Principal;
use crate::secrets::SealError;

/// Named credentials are kept in the same table as the settings, under a key nothing else can
/// collide with: a declared key may not contain a colon.
const NAMED: &str = "named:";
/// The most credentials one plugin may hold by name.
const MAX_NAMED: usize = 200;

/// The longest a text setting may be, and the longest a list's line may be.
const MAX_TEXT: usize = 4096;
const MAX_LIST: usize = 256;
/// How long a plugin has to say what it thinks of proposed settings before core stops waiting.
pub const CHECK_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

/// Announced when a plugin's settings change, by key and never by value, so anything holding them
/// knows to read them again.
pub fn changed_topic(plugin: &str) -> String {
    format!("platform.plugin.{plugin}.settings")
}

/// Where the value in use came from. Every one of them is editable: a setting the environment
/// starts off is still a setting somebody may change here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Source {
    /// Set here, which beats everything else.
    Stored,
    /// `DOC_<PLUGIN>_<KEY>`, or the file `DOC_<PLUGIN>_<KEY>_FILE` names, with nothing set here.
    Environment,
    /// Nobody has set it anywhere, so it is whatever the plugin declared.
    Default,
}

/// What one setting comes to now, without ever carrying a secret's value.
#[derive(Debug, Clone, Serialize)]
pub struct Current {
    pub key: String,
    pub source: Source,
    /// The value, or `null` for a secret: no answer, page, export or log returns one.
    pub value: Value,
    /// Whether a secret has something set, since that is all anyone is told about one.
    pub set: bool,
    /// A secret sealed under a key the platform no longer has, which must be set again.
    pub unreadable: bool,
    /// The variable that sets this setting in the deployment, when one does: what the value falls
    /// back to if it is cleared here, and what a stored value is overriding. Never its value,
    /// since a variable may hold a secret.
    pub environment: Option<String>,
    pub updated_at: Option<DateTime<Utc>>,
    pub updated_by: Option<String>,
    /// The secret in Secret Storage this points at, rather than a value typed in (FEAT-SECRETS).
    pub from_store: Option<FromStore>,
}

/// One credential a plugin holds by name, as the page is told of it: never its value.
#[derive(Debug, Clone, Serialize)]
pub struct NamedSecret {
    pub name: String,
    /// Sealed under a key the platform no longer has, so it must be set again.
    pub unreadable: bool,
    pub updated_at: DateTime<Utc>,
    pub updated_by: Option<String>,
    /// The secret in Secret Storage this points at, rather than a value typed in.
    pub from_store: Option<FromStore>,
}

/// A secret setting, or a credential held by name, pointed at a secret in Secret Storage
/// (FEAT-SECRETS), as the page is told of it: which one, what the store calls it, and why it
/// gives the plugin nothing when it does not.
#[derive(Debug, Clone, Serialize)]
pub struct FromStore {
    pub secret: Uuid,
    pub label: Option<String>,
    pub problem: Option<String>,
}

/// A credential as a save gives it: typed in, or a secret in Secret Storage to point at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Credential {
    Typed(String),
    Stored(Uuid),
}

/// What a row holds when it points at Secret Storage: which secret, and never its value.
pub fn reference(value: &Value) -> Option<Uuid> {
    value.as_object()?.get("secret")?.as_str()?.parse().ok()
}

fn pointing_at(secret: Uuid) -> Value {
    json!({ "secret": secret })
}

/// The plugin this platform's configuration lets keep secrets, if any.
pub fn store_id(state: &AppState) -> Option<String> {
    state
        .config
        .plugins
        .capabilities
        .iter()
        .find(|(_, allowed)| allowed.contains(&Capability::SecretStore))
        .map(|(plugin, _)| plugin.clone())
}

/// Plugins that pointed at a secret while the store could not be asked, told again once it loads.
static WAITING: LazyLock<parking_lot::Mutex<BTreeSet<String>>> =
    LazyLock::new(|| parking_lot::Mutex::new(BTreeSet::new()));

/// What the store said about some secrets, for one plugin.
#[derive(Debug, Default)]
struct Answered {
    values: BTreeMap<Uuid, Secret<String>>,
    labels: BTreeMap<Uuid, String>,
    problems: BTreeMap<Uuid, String>,
}

impl Answered {
    fn about(&self, secret: Uuid) -> FromStore {
        FromStore {
            secret,
            label: self.labels.get(&secret).cloned(),
            problem: self.problems.get(&secret).cloned(),
        }
    }

    /// How the audit log records a setting pointed at a secret: by name, never by value.
    fn said(&self, value: &Value) -> String {
        let named = reference(value).and_then(|secret| self.labels.get(&secret).cloned());
        format!("from Secret Storage: {}", named.unwrap_or_else(|| "a secret".to_string()))
    }
}

/// The values of these secrets for `plugin`, asked of the store as the platform. The store answers
/// only for secrets shared with that plugin, and says why for the rest.
async fn ask_store(state: &AppState, plugin: &str, secrets: &BTreeSet<Uuid>) -> Answered {
    let mut answered = Answered::default();
    if secrets.is_empty() {
        return answered;
    }
    let refused = |answered: &mut Answered, why: &str| {
        for secret in secrets {
            answered.problems.insert(*secret, why.to_string());
        }
    };
    let Some(store) = store_id(state) else {
        refused(&mut answered, "no plugin keeps secrets on this platform");
        return answered;
    };
    if store == plugin {
        refused(&mut answered, "Secret Storage keeps its own credentials itself");
        return answered;
    }
    let payload = json!({ "plugin": plugin, "secrets": secrets });
    let answer =
        match crate::plugins::routes::ask_internal(state, &store, "resolve", &payload).await {
            Ok(answer) => answer,
            Err(err) => {
                WAITING.lock().insert(plugin.to_string());
                refused(&mut answered, &format!("Secret Storage could not be asked: {err}"));
                return answered;
            }
        };
    for secret in secrets {
        let said = &answer["secrets"][secret.to_string()];
        if let Some(label) = said["label"].as_str() {
            answered.labels.insert(*secret, label.to_string());
        }
        match (said["value"].as_str(), said["problem"].as_str()) {
            (Some(value), _) => {
                answered.values.insert(*secret, Secret::new(value.to_string()));
            }
            (None, problem) => {
                let problem = problem.unwrap_or("Secret Storage has no such secret");
                answered.problems.insert(*secret, problem.to_string());
            }
        }
    }
    answered
}

/// What a plugin's secret settings may point at: the secrets in Secret Storage shared with it.
#[derive(Debug, Clone, Serialize)]
pub struct StoreOffer {
    pub store: String,
    pub secrets: Vec<StoreChoice>,
    /// Why nothing could be offered, when the store could not be asked.
    pub problem: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreChoice {
    pub id: Uuid,
    pub label: String,
    #[serde(default)]
    pub hint: String,
}

/// The secrets a plugin's Settings page may point its secret settings at, for a plugin that has
/// any and is not the store itself.
pub async fn store_offer(
    state: &AppState,
    plugin: &str,
    manifest: &Manifest,
) -> Option<StoreOffer> {
    let wanted = manifest.named_secrets.is_some()
        || manifest.settings.iter().any(|setting| setting.kind.is_secret());
    let store = store_id(state).filter(|store| wanted && store != plugin)?;
    let payload = json!({ "plugin": plugin });
    Some(match crate::plugins::routes::ask_internal(state, &store, "shared", &payload).await {
        Ok(answer) => StoreOffer {
            secrets: serde_json::from_value(answer["secrets"].clone()).unwrap_or_default(),
            store,
            problem: None,
        },
        Err(err) => StoreOffer {
            store,
            secrets: Vec::new(),
            problem: Some(format!("Secret Storage could not be asked: {err}")),
        },
    })
}

/// A plugin's settings and features as they stand.
#[derive(Debug, Clone, Default)]
pub struct Resolved {
    /// Every declared setting, in the order the manifest declares them.
    pub current: Vec<Current>,
    /// Plain values, resolved, for the plugin itself.
    values: BTreeMap<String, Value>,
    /// Secrets, resolved, for the plugin itself and nothing else.
    secrets: BTreeMap<String, Secret<String>>,
    /// The credentials an administrator added by name, for a plugin that holds them that way.
    named: BTreeMap<String, Secret<String>>,
    /// What the page shows of those: their names and when each was last set, never a value.
    pub named_held: Vec<NamedSecret>,
    pub features: BTreeMap<String, bool>,
    /// Required settings with nothing set, which a plugin is told about and a page shows.
    pub missing: Vec<String>,
}

impl Resolved {
    /// What the plugin that declared them is given: values, its own secrets, and its features.
    pub fn for_plugin(self) -> SettingsView {
        SettingsView {
            values: self.values,
            secrets: self.secrets,
            named: self.named,
            features: self.features,
            missing: self.missing,
            instance: Default::default(),
        }
    }

    pub fn feature(&self, name: &str) -> bool {
        self.features.get(name).copied().unwrap_or_default()
    }

    /// A text setting's value, when it is set to anything: what a manifest field that may be
    /// configured — a sign-in's title — uses instead of what the plugin declared.
    pub fn text(&self, key: &str) -> Option<String> {
        let value = self.values.get(key)?.as_str()?.trim().to_string();
        Some(value).filter(|text| !text.is_empty())
    }

    /// A `cron` setting's value, when it is set to something the cron parser takes. A schedule
    /// whose expression is a setting uses this instead of what the manifest declared.
    pub fn cron(&self, key: &str) -> Option<String> {
        let value = self.values.get(key)?.as_str()?.trim().to_string();
        doc_cron_tasks::next_after(&value, Utc::now()).ok()?;
        Some(value)
    }
}

/// `DOC_GITHUB_CLIENT_ID` for `client-id` on `github`, and `..._FILE` for the same through a file,
/// which is how Docker and Kubernetes hand a secret to a process.
pub fn environment_name(plugin: &str, key: &str) -> String {
    let shout = |text: &str| text.to_ascii_uppercase().replace('-', "_");
    format!("DOC_{}_{}", shout(plugin), shout(key))
}

/// The environment as this process sees it. Setting a variable is unsafe from Rust 2024, and this
/// crate forbids `unsafe`, so a test puts its own answers here instead of in the real environment.
#[cfg(test)]
static TEST_ENVIRONMENT: LazyLock<parking_lot::Mutex<BTreeMap<String, String>>> =
    LazyLock::new(|| parking_lot::Mutex::new(BTreeMap::new()));

/// Makes `name` read as `value` for the rest of this test, or as unset with `None`.
#[cfg(test)]
pub fn set_environment_for_test(name: &str, value: Option<&str>) {
    let mut held = TEST_ENVIRONMENT.lock();
    match value {
        Some(value) => held.insert(name.to_string(), value.to_string()),
        None => held.remove(name),
    };
}

fn variable(name: &str) -> Option<String> {
    #[cfg(test)]
    if let Some(value) = TEST_ENVIRONMENT.lock().get(name).cloned() {
        return Some(value);
    }
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// What the environment says a setting is, if it says anything. A file that cannot be read is not
/// the same as one that is not named: it is said so, and the setting is left unset.
fn from_environment(plugin: &str, key: &str) -> Option<String> {
    let name = environment_name(plugin, key);
    if let Some(value) = variable(&name) {
        return Some(value);
    }
    let path = variable(&format!("{name}_FILE"))?;
    match std::fs::read_to_string(&path) {
        Ok(text) => Some(text.trim().to_string()).filter(|text| !text.is_empty()),
        Err(err) => {
            tracing::warn!(plugin, key, path, %err, "the file a setting names could not be read");
            None
        }
    }
}

/// The manifest to build the page from: the registered one, or the last one recorded, so a plugin
/// that is not running can still be configured — which is how a deployment turns one on.
pub async fn manifest_of(state: &AppState, plugin: &str) -> Option<Manifest> {
    if let Some(entry) = state.plugins.get(plugin).await {
        return Some(entry.manifest);
    }
    let records = state.repos.plugins.records().await.ok()?;
    let record = records.into_iter().find(|record| record.id == plugin)?;
    serde_json::from_value(record.manifest).ok()
}

/// Everything a plugin's settings come to: the environment, then what was stored, then defaults.
pub async fn resolve(state: &AppState, plugin: &str, manifest: &Manifest) -> Resolved {
    let stored = state.repos.plugins.plugin_settings(plugin).await.unwrap_or_else(|err| {
        tracing::warn!(plugin, %err, "the plugin's settings could not be read");
        Vec::new()
    });
    let stored: BTreeMap<&str, &StoredSetting> =
        stored.iter().map(|setting| (setting.key.as_str(), setting)).collect();
    let mut resolved =
        Resolved { features: features(state, plugin, manifest).await, ..Default::default() };
    // Every secret pointed at Secret Storage is asked for together, once.
    let secret_keys = secret_keys(manifest);
    let pointed: BTreeSet<Uuid> = stored
        .values()
        .filter(|held| held.key.starts_with(NAMED) || secret_keys.contains(&held.key))
        .filter_map(|held| held.value.as_ref().and_then(reference))
        .collect();
    let answered = ask_store(state, plugin, &pointed).await;

    for setting in &manifest.settings {
        let held = stored.get(setting.key.as_str()).copied();
        // Named only when the deployment actually sets it, since that is what the page says the
        // value falls back to. The variable's own value is read below and never shown.
        let variable = environment_name(plugin, &setting.key);
        let from_env = from_environment(plugin, &setting.key);
        let mut unreadable = false;
        let mut from_store = None;

        // What was set here, if anything: it beats the environment and the default both. A
        // secret pointed at Secret Storage that the store will not give stays pointed there, and
        // unset, rather than falling back to the environment: that would change credential quietly.
        let pointer = held.and_then(|held| held.value.as_ref()).and_then(reference);
        let here: Option<(Value, Option<Secret<String>>)> = match (held, setting.kind.is_secret()) {
            (Some(_), true) if pointer.is_some() => {
                let secret = pointer.unwrap_or_default();
                from_store = Some(answered.about(secret));
                Some((Value::Null, answered.values.get(&secret).cloned()))
            }
            (Some(held), true) => held.sealed.as_ref().and_then(|sealed| {
                match state.settings_keys.open(plugin, &setting.key, sealed) {
                    Ok(secret) => Some((Value::Null, Some(Secret::new(secret)))),
                    Err(err) => {
                        // A secret whose key is gone is still set: it must be set again, and
                        // falling back to the environment would be a silent change of credential.
                        unreadable = matches!(err, SealError::UnknownKey(_));
                        tracing::warn!(plugin, key = %setting.key, %err, "a stored secret could not be read");
                        unreadable.then_some((Value::Null, None))
                    }
                }
            }),
            (Some(held), false) => held.value.clone().map(|value| (value, None)),
            (None, _) => None,
        };

        let (source, value, secret) = match (here, &from_env) {
            (Some((value, secret)), _) => (Source::Stored, value, secret),
            (None, Some(text)) => match setting.kind.is_secret() {
                true => (Source::Environment, Value::Null, Some(Secret::new(text.clone()))),
                false => match parse(setting, &Value::String(text.clone())) {
                    Ok(value) => (Source::Environment, value, None),
                    Err(problem) => {
                        tracing::warn!(
                            plugin,
                            key = %setting.key,
                            %variable,
                            %problem,
                            "the environment sets this to something the plugin did not declare"
                        );
                        (Source::Default, setting.fallback(), None)
                    }
                },
            },
            (None, None) => (Source::Default, setting.fallback(), None),
        };
        // A value is set when there is something to it, wherever it came from: a required
        // setting stored as nothing is still nothing, and says so rather than passing for set.
        let set = match setting.kind.is_secret() {
            true => secret.is_some() || unreadable,
            false => !is_empty(&value),
        };
        resolved.current.push(Current {
            key: setting.key.clone(),
            source,
            value: if setting.kind.is_secret() { Value::Null } else { value.clone() },
            set,
            unreadable,
            environment: from_env.is_some().then_some(variable),
            updated_at: held.map(|held| held.updated_at),
            updated_by: held.and_then(|held| held.updated_by.clone()),
            from_store,
        });
        if setting.required && !set {
            resolved.missing.push(setting.key.clone());
        }
        match secret {
            Some(secret) => {
                resolved.secrets.insert(setting.key.clone(), secret);
            }
            None if !setting.kind.is_secret() => {
                resolved.values.insert(setting.key.clone(), value);
            }
            None => {}
        }
    }

    // Credentials held by name, for a plugin that declares it holds them that way.
    if manifest.named_secrets.is_some() {
        for (key, held) in &stored {
            let Some(name) = key.strip_prefix(NAMED) else { continue };
            if let Some(secret) = held.value.as_ref().and_then(reference) {
                if let Some(value) = answered.values.get(&secret) {
                    resolved.named.insert(name.to_string(), value.clone());
                }
                resolved.named_held.push(NamedSecret {
                    name: name.to_string(),
                    unreadable: false,
                    updated_at: held.updated_at,
                    updated_by: held.updated_by.clone(),
                    from_store: Some(answered.about(secret)),
                });
                continue;
            }
            let Some(sealed) = &held.sealed else { continue };
            let opened = state.settings_keys.open(plugin, key, sealed);
            if let Ok(secret) = &opened {
                resolved.named.insert(name.to_string(), Secret::new(secret.clone()));
            }
            resolved.named_held.push(NamedSecret {
                name: name.to_string(),
                unreadable: matches!(opened, Err(SealError::UnknownKey(_))),
                updated_at: held.updated_at,
                updated_by: held.updated_by.clone(),
                from_store: None,
            });
        }
        resolved.named_held.sort_by(|one, two| one.name.cmp(&two.name));
    }
    resolved
}

/// `^[a-z0-9][a-z0-9_-]{0,63}$`: a credential's name is the plugin's own word for it, and it has
/// to survive being part of a storage key and a form field. The underscore is there because a
/// vendor's own names for its credentials — `project_id`, `client_secret` — usually have one.
pub fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    !name.is_empty()
        && name.len() <= 64
        && chars.next().is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_'))
}

/// Each declared feature, at what somebody set it to or the default the plugin declared.
pub async fn features(
    state: &AppState,
    plugin: &str,
    manifest: &Manifest,
) -> BTreeMap<String, bool> {
    let stored = state.repos.plugins.plugin_features(plugin).await.unwrap_or_else(|err| {
        tracing::warn!(plugin, %err, "the plugin's features could not be read");
        Vec::new()
    });
    manifest
        .features
        .iter()
        .map(|feature| {
            let set = stored.iter().find(|held| held.name == feature.name).map(|held| held.enabled);
            (feature.name.clone(), set.unwrap_or(feature.default))
        })
        .collect()
}

fn is_empty(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(entries) => entries.is_empty(),
        _ => false,
    }
}

/// A `map` as it is written anywhere: an object of lists (or of comma-separated text), or text of
/// `KEY=one,two` entries, one to a line or separated by `;`, as a form or a variable gives it.
fn entries(proposed: &Value) -> Result<Vec<(String, Vec<String>)>, String> {
    let split = |text: &str| -> Vec<String> {
        text.split(',')
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect()
    };
    match proposed {
        Value::Object(entries) => entries
            .iter()
            .map(|(key, values)| match values {
                Value::Array(items) => items
                    .iter()
                    .map(|item| match item {
                        Value::String(text) => Ok(text.trim().to_string()),
                        other => Err(format!("{key} takes text, not {}", shape(other))),
                    })
                    .collect::<Result<Vec<_>, _>>()
                    .map(|values| (key.trim().to_string(), values)),
                Value::String(text) => Ok((key.trim().to_string(), split(text))),
                Value::Null => Ok((key.trim().to_string(), Vec::new())),
                other => Err(format!("{key} takes a list, not {}", shape(other))),
            })
            .collect(),
        Value::String(text) => Ok(text
            .split(['\n', ';'])
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(|entry| match entry.split_once('=') {
                Some((key, values)) => (key.trim().to_string(), split(values)),
                None => (entry.to_string(), Vec::new()),
            })
            .collect()),
        other => Err(format!("this takes KEY=one,two entries, not {}", shape(other))),
    }
}

/// Checks a proposed value against what the plugin declared, and returns it in the shape it will
/// be stored in: a duration as seconds, a list as an array of lines, a number as a number.
pub fn parse(setting: &Setting, proposed: &Value) -> Result<Value, String> {
    let text = |value: &Value| -> Result<String, String> {
        match value {
            Value::String(text) => Ok(text.trim().to_string()),
            other => Err(format!("this takes text, not {}", shape(other))),
        }
    };
    let limited = |text: &str| -> Result<(), String> {
        if text.chars().count() > MAX_TEXT {
            return Err(format!("this is at most {MAX_TEXT} characters"));
        }
        if let Some(pattern) = &setting.pattern {
            let regex = regex::Regex::new(pattern).map_err(|_| {
                "this setting's pattern is not usable, so nothing can be saved against it"
                    .to_string()
            })?;
            if !regex.is_match(text) {
                return Err(format!("this must match {pattern}"));
            }
        }
        Ok(())
    };
    match setting.kind {
        SettingKind::Text => {
            let text = text(proposed)?;
            limited(&text)?;
            length(setting, text.chars().count() as f64, "characters")?;
            Ok(Value::String(text))
        }
        SettingKind::Secret => {
            let text = text(proposed)?;
            if text.chars().count() > MAX_TEXT {
                return Err(format!("this is at most {MAX_TEXT} characters"));
            }
            Ok(Value::String(text))
        }
        SettingKind::Number => {
            let number = match proposed {
                Value::Number(number) => number.as_f64().ok_or("this is not a number")?,
                Value::String(text) => {
                    text.trim().parse::<f64>().map_err(|_| "this is not a number".to_string())?
                }
                other => return Err(format!("this takes a number, not {}", shape(other))),
            };
            within(setting, number)?;
            Ok(json!(number))
        }
        SettingKind::Boolean => match proposed {
            Value::Bool(on) => Ok(Value::Bool(*on)),
            Value::String(text) => match text.trim() {
                "true" | "on" | "yes" => Ok(Value::Bool(true)),
                "false" | "off" | "no" | "" => Ok(Value::Bool(false)),
                _ => Err("this is on or off".to_string()),
            },
            other => Err(format!("this is on or off, not {}", shape(other))),
        },
        SettingKind::Choice => {
            let text = text(proposed)?;
            // Choices the plugin offers from a route change as what it knows does, so any text
            // is kept and the plugin passes over what it no longer offers.
            if setting.one_of.is_empty() && setting.choices.is_some() {
                limited(&text)?;
                return Ok(Value::String(text));
            }
            match setting.one_of.contains(&text) {
                true => Ok(Value::String(text)),
                false => Err(format!("this is one of {}", setting.one_of.join(", "))),
            }
        }
        SettingKind::Map => {
            let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (key, values) in entries(proposed)? {
                if key.is_empty() {
                    if values.is_empty() {
                        continue;
                    }
                    return Err(format!(
                        "{} is given to nothing: write KEY=one,two",
                        values.join(", ")
                    ));
                }
                limited(&key)?;
                let held = map.entry(key).or_default();
                for value in values.into_iter().filter(|value| !value.is_empty()) {
                    if value.chars().count() > MAX_TEXT {
                        return Err(format!("a value is at most {MAX_TEXT} characters"));
                    }
                    if !held.contains(&value) {
                        held.push(value);
                    }
                }
            }
            if map.len() > MAX_LIST {
                return Err(format!("this is at most {MAX_LIST} entries"));
            }
            if map.values().map(Vec::len).sum::<usize>() > MAX_LIST {
                return Err(format!("this gives at most {MAX_LIST} values in all"));
            }
            Ok(json!(map))
        }
        SettingKind::List => {
            let lines: Vec<String> = match proposed {
                Value::Array(items) => items
                    .iter()
                    .map(|item| match item {
                        Value::String(text) => Ok(text.trim().to_string()),
                        other => Err(format!("a list takes text, not {}", shape(other))),
                    })
                    .collect::<Result<_, _>>()?,
                // A form sends one box, so the lines in it are the list — and a variable in a
                // deployment is one line, so commas separate there as they always have.
                Value::String(text) => {
                    text.split(['\n', ',']).map(|line| line.trim().to_string()).collect()
                }
                other => Err(format!("this takes a list, not {}", shape(other)))?,
            };
            let lines: Vec<String> = lines.into_iter().filter(|line| !line.is_empty()).collect();
            if lines.len() > MAX_LIST {
                return Err(format!("this is at most {MAX_LIST} lines"));
            }
            for line in &lines {
                limited(line)?;
            }
            Ok(json!(lines))
        }
        SettingKind::Url => {
            let text = text(proposed)?;
            limited(&text)?;
            let url = url::Url::parse(&text).map_err(|err| format!("this is not a URL: {err}"))?;
            match url.scheme() {
                "http" | "https" => Ok(Value::String(text)),
                scheme => Err(format!("a URL here is http or https, not {scheme}")),
            }
        }
        SettingKind::Duration => {
            let seconds = match proposed {
                Value::Number(number) => number.as_f64().ok_or("this is not a span of time")?,
                Value::String(text) => span(text.trim())?,
                other => return Err(format!("this takes a span of time, not {}", shape(other))),
            };
            within(setting, seconds)?;
            Ok(json!(seconds))
        }
        SettingKind::Cron => {
            let text = text(proposed)?;
            doc_cron_tasks::next_after(&text, Utc::now()).map_err(|err| err.to_string())?;
            Ok(Value::String(text))
        }
    }
}

fn shape(value: &Value) -> &'static str {
    match value {
        Value::Null => "nothing",
        Value::Bool(_) => "on or off",
        Value::Number(_) => "a number",
        Value::String(_) => "text",
        Value::Array(_) => "a list",
        Value::Object(_) => "a record",
    }
}

fn within(setting: &Setting, number: f64) -> Result<(), String> {
    if setting.min.is_some_and(|min| number < min) {
        return Err(format!("this is at least {}", setting.min.unwrap_or_default()));
    }
    if setting.max.is_some_and(|max| number > max) {
        return Err(format!("this is at most {}", setting.max.unwrap_or_default()));
    }
    Ok(())
}

fn length(setting: &Setting, measured: f64, unit: &str) -> Result<(), String> {
    if setting.min.is_some_and(|min| measured < min) {
        return Err(format!("this is at least {} {unit}", setting.min.unwrap_or_default()));
    }
    if setting.max.is_some_and(|max| measured > max) {
        return Err(format!("this is at most {} {unit}", setting.max.unwrap_or_default()));
    }
    Ok(())
}

/// `45s`, `5m`, `2h`, `7d`, or plain seconds, as the seconds it comes to.
fn span(text: &str) -> Result<f64, String> {
    let wrong = || "this is a span of time, such as 30s, 5m, 2h or 7d".to_string();
    if let Ok(seconds) = text.parse::<f64>() {
        return Ok(seconds);
    }
    let (number, unit) = text.split_at(text.len().saturating_sub(1));
    let number: f64 = number.parse().map_err(|_| wrong())?;
    let each = match unit {
        "s" => 1.0,
        "m" => 60.0,
        "h" => 3600.0,
        "d" => 86_400.0,
        _ => return Err(wrong()),
    };
    Ok(number * each)
}

#[derive(Debug, thiserror::Error)]
pub enum SaveError {
    /// Each key the values were wrong for, with what is wrong with it.
    #[error("some of these settings cannot be saved")]
    Problems(BTreeMap<String, String>),
    /// The plugin looked at them and refused, such as a credential its server would not take.
    #[error("{0}")]
    Refused(String),
    #[error("no settings key has been created, so a secret cannot be stored")]
    NoKey,
    #[error("{0}")]
    Storage(String),
}

/// What a save did, for the audit entry and the answer.
#[derive(Debug, Default)]
pub struct Saved {
    pub keys: Vec<String>,
    pub features: Vec<String>,
    /// What the plugin said when it was happy, such as which account a token belongs to.
    pub message: Option<String>,
}

/// One setting and what it is to become: `None` puts it back to the plugin's default, and for a
/// secret, takes it away.
type Proposed = Vec<(Setting, Option<Value>)>;

/// Turns what was typed into what will be stored, refusing the lot if any of it is wrong. A
/// setting the deployment also sets in the environment is taken like any other: what is stored
/// here wins, and clearing it falls back to the variable.
fn proposed(
    manifest: &Manifest,
    plugin: &str,
    values: &BTreeMap<String, Value>,
) -> Result<Proposed, BTreeMap<String, String>> {
    let mut problems = BTreeMap::new();
    let mut changes = Vec::new();
    for (key, proposed) in values {
        let Some(setting) = manifest.settings.iter().find(|setting| &setting.key == key) else {
            problems.insert(key.clone(), format!("{plugin} has no setting called {key}"));
            continue;
        };
        // A secret pointed at Secret Storage is stored as that pointer; the store is asked about
        // it before anything is kept.
        if setting.kind.is_secret()
            && let Some(secret) = reference(proposed)
        {
            changes.push((setting.clone(), Some(pointing_at(secret))));
            continue;
        }
        // Nothing at all clears it: back to the environment if the deployment sets it, and
        // otherwise to the plugin's default; for a secret, gone.
        if proposed.is_null() || (is_empty(proposed) && !setting.required) {
            changes.push((setting.clone(), None));
            continue;
        }
        match parse(setting, proposed) {
            Ok(value) => changes.push((setting.clone(), Some(value))),
            Err(problem) => {
                problems.insert(key.clone(), problem);
            }
        }
    }
    match problems.is_empty() {
        true => Ok(changes),
        false => Err(problems),
    }
}

/// What the plugin is asked about, and what a **Test connection** asks with: the settings as they
/// would be if this save went through, secrets and all.
async fn ask(
    state: &AppState,
    plugin: &str,
    manifest: &Manifest,
    changes: &[(Setting, Option<Value>)],
    named: &NamedChanges,
    features: &BTreeMap<String, bool>,
    pointed: &Answered,
) -> Result<SettingsVerdict, String> {
    let Some(entry) = state.plugins.get(plugin).await else {
        // A plugin that is not running cannot object; the settings are stored for its next load.
        return Ok(SettingsVerdict::ok());
    };
    if !entry.state.serves_requests() {
        return Ok(SettingsVerdict::ok());
    }
    let resolved = resolve(state, plugin, manifest).await;
    let mut check = SettingsCheck {
        values: resolved.values.clone(),
        secrets: resolved.secrets.clone(),
        named: resolved.named.clone(),
        features: features.clone(),
        instance: state.config.instance.for_plugins(),
    };
    for (setting, value) in changes {
        let from_store = value.as_ref().and_then(reference);
        match (setting.kind.is_secret(), value) {
            (true, Some(_)) if from_store.is_some() => {
                match from_store.and_then(|secret| pointed.values.get(&secret)) {
                    Some(secret) => check.secrets.insert(setting.key.clone(), secret.clone()),
                    None => check.secrets.remove(&setting.key),
                };
            }
            (true, Some(Value::String(secret))) => {
                check.secrets.insert(setting.key.clone(), Secret::new(secret.clone()));
            }
            (true, _) => {
                check.secrets.remove(&setting.key);
            }
            (false, Some(value)) => {
                check.values.insert(setting.key.clone(), value.clone());
            }
            (false, None) => {
                check.values.insert(setting.key.clone(), setting.fallback());
            }
        }
    }
    // Credentials held by name are asked about as they would be too, so a plugin tries the one
    // being added rather than the one it replaces.
    for (name, value) in named {
        let given = match value {
            Some(Credential::Typed(secret)) => Some(Secret::new(secret.clone())),
            Some(Credential::Stored(secret)) => pointed.values.get(secret).cloned(),
            None => None,
        };
        match given {
            Some(secret) => check.named.insert(name.clone(), secret),
            None => check.named.remove(name),
        };
    }
    let context = crate::plugins::own_context(state, plugin, CHECK_DEADLINE)
        .map_err(|err| err.to_string())?;
    match entry.client.settings_check(&check, context.token()).await {
        Ok(verdict) => Ok(verdict),
        // A plugin that does not answer this call has no opinion, which is the common case.
        Err(crate::plugins::client::CallError::Refused(_, 404, _)) => Ok(SettingsVerdict::ok()),
        Err(err) => Err(err.detail()),
    }
}

/// A **Test connection**: the plugin's opinion of these settings, with nothing stored.
pub async fn test(
    state: &AppState,
    plugin: &str,
    manifest: &Manifest,
    values: &BTreeMap<String, Value>,
) -> Result<SettingsVerdict, SaveError> {
    let changes = proposed(manifest, plugin, values).map_err(SaveError::Problems)?;
    let named = NamedChanges::new();
    let pointed = pointed_at(state, plugin, &changes, &named).await?;
    let features = features(state, plugin, manifest).await;
    ask(state, plugin, manifest, &changes, &named, &features, &pointed)
        .await
        .map_err(SaveError::Refused)
}

/// Credentials a plugin holds by name, as a save asks for them: a value or a secret in Secret
/// Storage sets one, and `None` takes it away.
pub type NamedChanges = BTreeMap<String, Option<Credential>>;

/// Asks the store about every secret a save points at, and refuses the save for any it will not
/// give this plugin, so a setting is never left pointing at nothing.
async fn pointed_at(
    state: &AppState,
    plugin: &str,
    changes: &Proposed,
    named: &NamedChanges,
) -> Result<Answered, SaveError> {
    let mut secrets = BTreeSet::new();
    let mut asked: Vec<(String, Uuid)> = Vec::new();
    for (setting, value) in changes {
        if let Some(secret) =
            value.as_ref().and_then(reference).filter(|_| setting.kind.is_secret())
        {
            secrets.insert(secret);
            asked.push((setting.key.clone(), secret));
        }
    }
    for (name, credential) in named {
        if let Some(Credential::Stored(secret)) = credential {
            secrets.insert(*secret);
            asked.push((name.clone(), *secret));
        }
    }
    let answered = ask_store(state, plugin, &secrets).await;
    let problems: BTreeMap<String, String> = asked
        .into_iter()
        .filter_map(|(key, secret)| answered.problems.get(&secret).map(|why| (key, why.clone())))
        .collect();
    match problems.is_empty() {
        true => Ok(answered),
        false => Err(SaveError::Problems(problems)),
    }
}

/// Checks the values, asks the plugin, then stores all of them or none (ADR-0007).
pub async fn save(
    state: &AppState,
    plugin: &str,
    manifest: &Manifest,
    values: &BTreeMap<String, Value>,
    named: &NamedChanges,
    by: &Principal,
) -> Result<Saved, SaveError> {
    let changes = proposed(manifest, plugin, values).map_err(SaveError::Problems)?;
    let asked_named = named;
    let named = named_changes(state, plugin, manifest, named).await?;
    if changes.is_empty() && named.is_empty() {
        return Ok(Saved::default());
    }
    let pointed = pointed_at(state, plugin, &changes, asked_named).await?;
    let features = features(state, plugin, manifest).await;
    let verdict = ask(state, plugin, manifest, &changes, asked_named, &features, &pointed)
        .await
        .map_err(SaveError::Refused)?;
    if !verdict.problems.is_empty() {
        return Err(SaveError::Problems(verdict.problems));
    }
    if let Some(problem) = verdict.problem {
        return Err(SaveError::Refused(problem));
    }

    let mut stored = Vec::new();
    let mut audited = serde_json::Map::new();
    for (setting, value) in &changes {
        let key = setting.key.clone();
        match (setting.kind.is_secret(), value) {
            (true, Some(value)) if reference(value).is_some() => {
                audited.insert(key.clone(), json!(pointed.said(value)));
                stored.push(SettingChange::Set { key, value: value.clone() });
            }
            (true, Some(Value::String(secret))) => {
                if !state.settings_keys.available() {
                    return Err(SaveError::NoKey);
                }
                let sealed = state
                    .settings_keys
                    .seal(plugin, &key, secret)
                    .map_err(|err| SaveError::Storage(err.to_string()))?;
                stored.push(SettingChange::Seal { key: key.clone(), sealed });
                // A secret is recorded as changed and never as what it was changed to.
                audited.insert(key, json!("changed"));
            }
            (true, _) => {
                stored.push(SettingChange::Clear { key: key.clone() });
                audited.insert(key, json!("cleared"));
            }
            (false, Some(value)) => {
                stored.push(SettingChange::Set { key: key.clone(), value: value.clone() });
                audited.insert(key, value.clone());
            }
            (false, None) => {
                stored.push(SettingChange::Clear { key: key.clone() });
                audited.insert(key, Value::Null);
            }
        }
    }
    let mut stored = stored;
    for change in &named {
        match change {
            SettingChange::Clear { key } => {
                audited.insert(key.clone(), json!("cleared"));
            }
            SettingChange::Set { key, value } => {
                audited.insert(key.clone(), json!(pointed.said(value)));
            }
            other => {
                audited.insert(other.key().to_string(), json!("changed"));
            }
        }
        stored.push(change.clone());
    }

    let label = by.label();
    state
        .repos
        .plugins
        .set_plugin_settings(plugin, &stored, Some(&label))
        .await
        .map_err(|err| SaveError::Storage(err.to_string()))?;

    let keys: Vec<String> = stored.iter().map(|change| change.key().to_string()).collect();
    let detail = json!({ "keys": keys, "values": Value::Object(audited) });
    crate::plugins::audit(state, by, "plugin.settings.changed", plugin, detail).await;
    // A schedule whose expression is one of these settings now runs to the new one.
    if manifest
        .schedules
        .iter()
        .any(|schedule| schedule.setting.as_ref().is_some_and(|setting| keys.contains(setting)))
    {
        let configured = resolve(state, plugin, manifest).await;
        crate::plugins::reschedule(state, plugin, &manifest.schedules, &configured).await;
    }
    tell(state, plugin, manifest, &keys, &[]).await;
    Ok(Saved { keys, features: Vec::new(), message: verdict.message })
}

/// Checks the credentials a save adds or takes away by name, and seals the ones it adds.
async fn named_changes(
    state: &AppState,
    plugin: &str,
    manifest: &Manifest,
    wanted: &NamedChanges,
) -> Result<Vec<SettingChange>, SaveError> {
    if wanted.is_empty() {
        return Ok(Vec::new());
    }
    let mut problems = BTreeMap::new();
    if manifest.named_secrets.is_none() {
        problems.insert("named".to_string(), format!("{plugin} does not hold credentials by name"));
        return Err(SaveError::Problems(problems));
    }
    let held = state.repos.plugins.plugin_settings(plugin).await.unwrap_or_default();
    let existing = held.iter().filter(|held| held.key.starts_with(NAMED)).count();
    let adding = wanted.iter().filter(|(_, value)| value.is_some()).count();
    if existing + adding > MAX_NAMED {
        problems.insert(
            "named".to_string(),
            format!("a plugin holds at most {MAX_NAMED} credentials by name"),
        );
        return Err(SaveError::Problems(problems));
    }

    let mut changes = Vec::new();
    for (name, value) in wanted {
        if !valid_name(name) {
            let said = "a name is up to 64 of a-z, 0-9, - and _, starting with a letter or a digit";
            problems.insert(name.clone(), said.to_string());
            continue;
        }
        let key = format!("{NAMED}{name}");
        let typed = match value {
            Some(Credential::Stored(secret)) => {
                changes.push(SettingChange::Set { key, value: pointing_at(*secret) });
                continue;
            }
            Some(Credential::Typed(typed)) => Some(typed.trim()).filter(|typed| !typed.is_empty()),
            None => None,
        };
        match typed {
            None => changes.push(SettingChange::Clear { key }),
            Some(secret) => {
                if !state.settings_keys.available() {
                    return Err(SaveError::NoKey);
                }
                match state.settings_keys.seal(plugin, &key, secret) {
                    Ok(sealed) => changes.push(SettingChange::Seal { key, sealed }),
                    Err(err) => return Err(SaveError::Storage(err.to_string())),
                }
            }
        }
    }
    match problems.is_empty() {
        true => Ok(changes),
        false => Err(SaveError::Problems(problems)),
    }
}

/// Turning features on and off, which also decides which of the plugin's schedules exist.
pub async fn set_features(
    state: &AppState,
    plugin: &str,
    manifest: &Manifest,
    wanted: &BTreeMap<String, bool>,
    by: &Principal,
) -> Result<Saved, SaveError> {
    let mut problems = BTreeMap::new();
    let mut changes = Vec::new();
    let held = features(state, plugin, manifest).await;
    for (name, on) in wanted {
        if !manifest.features.iter().any(|feature| &feature.name == name) {
            problems.insert(name.clone(), format!("{plugin} has no feature called {name}"));
            continue;
        }
        if held.get(name) != Some(on) {
            changes.push((name.clone(), *on));
        }
    }
    if !problems.is_empty() {
        return Err(SaveError::Problems(problems));
    }
    if changes.is_empty() {
        return Ok(Saved::default());
    }
    let label = by.label();
    state
        .repos
        .plugins
        .set_plugin_features(plugin, &changes, Some(&label))
        .await
        .map_err(|err| SaveError::Storage(err.to_string()))?;

    let names: Vec<String> = changes.iter().map(|(name, _)| name.clone()).collect();
    let detail = json!({ "features": changes.iter().map(|(name, on)| json!({ "feature": name, "enabled": on })).collect::<Vec<_>>() });
    crate::plugins::audit(state, by, "plugin.features.changed", plugin, detail).await;
    // A schedule of a feature that has just gone off stops existing, and one of a feature that
    // has come on starts, so nothing runs for a feature nobody wants.
    let configured = resolve(state, plugin, manifest).await;
    crate::plugins::reschedule(state, plugin, &manifest.schedules, &configured).await;
    tell(state, plugin, manifest, &[], &names).await;
    Ok(Saved { keys: Vec::new(), features: names, message: None })
}

/// What turning a plugin on from the plugins page switches: its own features that work from other
/// plugins', and the features of theirs they work from (FEAT-DORA).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Enabling {
    /// The plugin's own features, which are off or whose needs nobody meets yet.
    pub features: Vec<String>,
    /// Other plugins' features turned on with them, as `(plugin, feature)`.
    pub with: Vec<(String, String)>,
    /// A need no running, configured plugin can meet, in words, which is why none of it is offered.
    pub blocked: Option<String>,
}

/// Whether another plugin could meet a need now: it serves requests, declares the feature and has
/// every required setting, so turning the feature on starts something rather than an error.
async fn could_meet(state: &AppState, plugin: &str, feature: &str) -> Option<Manifest> {
    let entry = state.plugins.get(plugin).await.filter(|entry| entry.state.serves_requests())?;
    let manifest = entry.manifest;
    if !manifest.features.iter().any(|declared| declared.name == feature) {
        return None;
    }
    resolve(state, plugin, &manifest).await.missing.is_empty().then_some(manifest)
}

/// What enabling `plugin` would switch, or `None` when every feature that works from another
/// plugin's is already on and has what it needs. A feature with a warning is never switched this
/// way: it asks to be turned on knowingly, on its own tab.
pub async fn enabling(state: &AppState, plugin: &str, manifest: &Manifest) -> Option<Enabling> {
    let own = features(state, plugin, manifest).await;
    let mut offer = Enabling::default();
    for feature in manifest.features.iter().filter(|f| !f.needs.is_empty() && f.warning.is_empty())
    {
        let mut unmet = Vec::new();
        for need in &feature.needs {
            let mut met = false;
            for other in &need.plugins {
                if let Some(theirs) = manifest_of(state, other).await {
                    met |= features(state, other, &theirs).await.get(&need.feature) == Some(&true);
                }
            }
            if !met {
                unmet.push(need);
            }
        }
        if own.get(&feature.name) == Some(&true) && unmet.is_empty() {
            continue;
        }
        offer.features.push(feature.name.clone());
        for need in unmet {
            let mut able = Vec::new();
            for other in &need.plugins {
                if could_meet(state, other, &need.feature).await.is_some() {
                    able.push((other.clone(), need.feature.clone()));
                }
            }
            if able.is_empty() {
                offer.blocked = Some(format!(
                    "needs {} running and configured, for its {} feature",
                    need.plugins.join(" or "),
                    need.feature
                ));
            }
            for pair in able {
                if !offer.with.contains(&pair) {
                    offer.with.push(pair);
                }
            }
        }
    }
    (!offer.features.is_empty()).then_some(offer)
}

/// Announces the change and tells the plugin, which reloads itself unless it does better. Neither
/// carries a value: the plugin reads its settings back over its own connection.
async fn tell(
    state: &AppState,
    plugin: &str,
    manifest: &Manifest,
    keys: &[String],
    features: &[String],
) {
    if let Ok(topic) = Topic::new(changed_topic(plugin)) {
        let payload = json!({ "plugin": plugin, "keys": keys, "features": features });
        let event = Event::new(topic, crate::plugins::SOURCE, payload);
        if let Err(err) = state.buses.events.publish(event).await {
            tracing::warn!(plugin, %err, "the settings change was not announced");
        }
    }
    let Some(entry) = state.plugins.get(plugin).await.filter(|entry| entry.state.serves_requests())
    else {
        return;
    };
    let changed = SettingsChanged { keys: keys.to_vec(), features: features.to_vec() };
    let context = match crate::plugins::own_context(state, plugin, CHECK_DEADLINE) {
        Ok(context) => context,
        Err(err) => {
            tracing::warn!(plugin, %err, "the plugin was not told its settings changed");
            return;
        }
    };
    if let Err(err) = entry.client.settings_changed(&changed, context.token()).await {
        tracing::warn!(
            plugin,
            version = %manifest.version,
            error = %err.detail(),
            "the plugin was not told its settings changed; it will see them at its next load"
        );
    }
}

/// Tells every plugin whose settings point at one of `secrets` that they changed, and, when the
/// store has just loaded, every plugin that could not reach it before (FEAT-SECRETS). Answers the
/// plugins told.
pub async fn secrets_changed(state: &AppState, secrets: &[Uuid], loaded: bool) -> Vec<String> {
    let waiting: BTreeSet<String> = match loaded {
        true => std::mem::take(&mut *WAITING.lock()),
        false => BTreeSet::new(),
    };
    let records = match state.repos.plugins.records().await {
        Ok(records) => records,
        Err(err) => {
            tracing::warn!(%err, "the plugins using changed secrets could not be found");
            return Vec::new();
        }
    };
    let mut told = Vec::new();
    for record in records {
        let Some(manifest) = manifest_of(state, &record.id).await else { continue };
        let secret_keys = secret_keys(&manifest);
        let rows = state.repos.plugins.plugin_settings(&record.id).await.unwrap_or_default();
        let pointing: Vec<String> =
            rows.iter()
                .filter(|row| row.key.starts_with(NAMED) || secret_keys.contains(&row.key))
                .filter(|row| {
                    row.value.as_ref().and_then(reference).is_some_and(|secret| {
                        secrets.contains(&secret) || waiting.contains(&record.id)
                    })
                })
                .map(|row| row.key.clone())
                .collect();
        if pointing.is_empty() {
            continue;
        }
        tell(state, &record.id, &manifest, &pointing, &[]).await;
        told.push(record.id);
    }
    told
}

/// The keys of every secret a plugin declares, so a rotation knows which setting each row is.
pub fn secret_keys(manifest: &Manifest) -> BTreeSet<String> {
    manifest
        .settings
        .iter()
        .filter(|setting| setting.kind.is_secret())
        .map(|setting| setting.key.clone())
        .collect()
}
