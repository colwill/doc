//! Working out what a service sees. Everything that could apply to it is read in one go, the least
//! specific first, so `*` is the default and the service's own value in its own environment is the
//! last word; then any upstream provider is merged in, on whichever side the provider says wins.
//! The answer carries a version, which is its ETag, so a service polling this costs one `304`.
//!
//! That one answer is then written three ways for whoever asked: DOC's own, OpenFeature's, and
//! flagd's. The values are the same; only the shape differs.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

use doc_plugin_sdk::Backend;

use crate::Refusal;
use crate::model::{Precedence, Role, specificity};

/// What flagd's flag definitions are written against. The document names it so anything reading
/// one can check its shape without being told.
pub const FLAGD_SCHEMA: &str = "https://flagd.dev/schema/v0/flags.json";
use crate::providers;
use crate::store::{Entry, Store};

/// What a service is given: its flags, its configuration, and where each value came from.
pub struct Resolved {
    pub service: String,
    pub environment: String,
    pub flags: Map<String, Value>,
    pub config: Map<String, Value>,
    /// Per key, what decided it: `doc`, or the provider's name.
    pub sources: BTreeMap<String, String>,
    pub version: String,
    pub problems: Vec<String>,
}

impl Resolved {
    pub fn payload(&self, refresh_seconds: i64) -> Value {
        json!({
            "service": self.service,
            "environment": self.environment,
            "flags": self.flags,
            "config": self.config,
            "version": self.version,
            "refresh_seconds": refresh_seconds,
            "problems": self.problems,
        })
    }

    /// Where a key's value came from: `doc`, or the provider's name.
    fn source(&self, key: &str) -> String {
        self.sources.get(key).cloned().unwrap_or_else(|| "doc".to_string())
    }

    /// Everything served and the side it is held on. OpenFeature and flagd both know one kind of
    /// flag, so a service reading either is given DOC's configuration among its flags.
    fn everything(&self) -> impl Iterator<Item = (Role, &String, &Value)> {
        self.flags
            .iter()
            .map(|(key, value)| (Role::Flag, key, value))
            .chain(self.config.iter().map(|(key, value)| (Role::Config, key, value)))
    }

    /// A key's value whichever side it is held on: a caller asking by key knows nothing of the
    /// difference between a flag and a setting.
    pub fn held(&self, key: &str) -> Option<&Value> {
        self.flags.get(key).or_else(|| self.config.get(key))
    }

    /// One value as OpenFeature describes an evaluation, in the bulk answer or on its own. Nothing
    /// here is decided by a targeting rule — a value is whatever the service's scope resolves to —
    /// and that is what OpenFeature calls `STATIC`.
    pub fn evaluated(&self, key: &str, value: &Value) -> Value {
        json!({
            "key": key,
            "value": value,
            "reason": "STATIC",
            "variant": variant(value),
            "metadata": { "source": self.source(key) },
        })
    }

    /// OpenFeature's bulk evaluation answer, which its SDKs read as they are.
    pub fn open_feature(&self) -> Value {
        let flags: Vec<Value> =
            self.everything().map(|(_, key, value)| self.evaluated(key, value)).collect();
        json!({
            "flags": flags,
            "metadata": {
                "service": self.service,
                "environment": self.environment,
                "version": self.version,
            },
        })
    }

    /// The same values as a [flagd](https://flagd.dev) flag definition. flagd, and the OpenFeature
    /// providers that evaluate in-process, ask nobody to evaluate anything: they read a document of
    /// flags and work it out themselves. This is that document, already resolved for the service
    /// and the environment it was asked about, so nothing in it needs a targeting rule. A flag that
    /// is not served is not in it and the service falls back to its own default, exactly as it does
    /// everywhere else here.
    pub fn flagd(&self) -> Value {
        let flags: Map<String, Value> = self
            .everything()
            .map(|(role, key, value)| {
                let (variants, default) = variants(value);
                let held = json!({
                    "state": "ENABLED",
                    "variants": variants,
                    "defaultVariant": default,
                    "metadata": { "doc.role": role.as_str(), "doc.source": self.source(key) },
                });
                (key.clone(), held)
            })
            .collect();
        json!({
            "$schema": FLAGD_SCHEMA,
            "flags": flags,
            "metadata": {
                "flagSetId": format!("{}/{}", self.service, self.environment),
                "version": self.version,
            },
        })
    }
}

/// What OpenFeature calls the chosen value, which is the value itself for anything but a switch.
fn variant(value: &Value) -> String {
    match value {
        Value::Bool(true) => "on".to_string(),
        Value::Bool(false) => "off".to_string(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

/// flagd holds the values a flag may take by name and then names the one it serves. DOC holds one
/// value, so a switch keeps the `on` and `off` flagd's own documents use, and anything else is the
/// single variant `value`. flagd asks that a flag's variants all be of one type, which they are.
fn variants(value: &Value) -> (Map<String, Value>, String) {
    match value {
        Value::Bool(_) => (
            [("on".to_string(), json!(true)), ("off".to_string(), json!(false))]
                .into_iter()
                .collect(),
            variant(value),
        ),
        held => ([("value".to_string(), held.clone())].into_iter().collect(), "value".to_string()),
    }
}

/// Everything a service reads, DOC's own and its providers'.
pub async fn resolve(
    backend: &Backend,
    service: &str,
    environment: &str,
) -> Result<Resolved, Refusal> {
    let store = Store(backend);
    let mut entries = store.for_service(service, environment).await?;
    // Least specific first, so the service's own value in its own environment is applied last.
    entries.sort_by_key(|entry| specificity(&entry.service, &entry.environment));

    let (mut flags, mut config) = (Map::new(), Map::new());
    let mut sources: BTreeMap<String, String> = BTreeMap::new();
    for entry in &entries {
        let into = match entry.role() {
            Role::Flag => &mut flags,
            Role::Config => &mut config,
        };
        into.insert(entry.key.clone(), entry.value.clone());
        sources.insert(entry.key.clone(), "doc".to_string());
    }

    let mut problems = Vec::new();
    for provider in store.providers().await? {
        if !provider.applies(service, environment) {
            continue;
        }
        match providers::fetch(backend, &provider, service, environment).await {
            Err(problem) => {
                // A provider that cannot be reached must not take a service's flags away: what DOC
                // holds is still served, and the problem is reported beside it.
                problems.push(format!("{}: {problem}", provider.name));
                let _ = store
                    .set_provider(
                        provider.id,
                        json!({ "problem": problem, "checked_at": chrono::Utc::now().to_rfc3339() }),
                    )
                    .await;
            }
            Ok(fetched) => {
                let values = fetched.values;
                let merged = values
                    .flags
                    .into_iter()
                    .map(|held| (Role::Flag, held))
                    .chain(values.config.into_iter().map(|held| (Role::Config, held)));
                for (role, (key, value)) in merged {
                    let into = match role {
                        Role::Flag => &mut flags,
                        Role::Config => &mut config,
                    };
                    if into.contains_key(&key) && provider.precedence() == Precedence::Doc {
                        continue;
                    }
                    into.insert(key.clone(), value);
                    sources.insert(key, provider.name.clone());
                }
                if !fetched.cached {
                    let _ = store
                        .set_provider(
                            provider.id,
                            json!({
                                "problem": Value::Null,
                                "checked_at": chrono::Utc::now().to_rfc3339(),
                            }),
                        )
                        .await;
                }
            }
        }
    }

    let version = version(&flags, &config);
    Ok(Resolved {
        service: service.to_string(),
        environment: environment.to_string(),
        flags,
        config,
        sources,
        version,
        problems,
    })
}

/// A short hash of everything served, which is the ETag a service polls with.
fn version(flags: &Map<String, Value>, config: &Map<String, Value>) -> String {
    let canonical = json!({ "flags": flags, "config": config }).to_string();
    hex::encode(Sha256::digest(canonical.as_bytes()))[..16].to_string()
}

/// One entry as the API describes it.
pub fn shown(entry: &Entry) -> Value {
    json!({
        "id": entry.id,
        "role": entry.role,
        "key": entry.key,
        "service": entry.service,
        "environment": entry.environment,
        "kind": entry.kind,
        "value": entry.value,
        "description": entry.description,
        "enabled": entry.enabled,
        "updated_by": entry.updated_by,
        "updated_at": entry.updated_at,
    })
}

#[cfg(test)]
mod rendered {
    use super::*;

    /// One service's answer, with both sides held and one value from a provider.
    fn resolved() -> Resolved {
        let flags: Map<String, Value> = [
            ("accepting-traffic".to_string(), json!(true)),
            ("legacy-checkout".to_string(), json!(false)),
            ("greeting".to_string(), json!("Hello")),
        ]
        .into_iter()
        .collect();
        let config: Map<String, Value> =
            [("page-size".to_string(), json!(20)), ("limits".to_string(), json!({ "burst": 5 }))]
                .into_iter()
                .collect();
        let sources = [
            ("greeting".to_string(), "doc".to_string()),
            ("page-size".to_string(), "runcfg".to_string()),
        ]
        .into_iter()
        .collect();
        Resolved {
            service: "ledger-api".to_string(),
            environment: "production".to_string(),
            flags,
            config,
            sources,
            version: "2663e9639a17def0".to_string(),
            problems: Vec::new(),
        }
    }

    /// flagd serves a variant by name, so a switch has to be two of them and the one that is on
    /// has to be the one named. A service asking flagd for a boolean reads either.
    #[test]
    fn a_switch_is_flagds_on_and_off() {
        let document = resolved().flagd();
        let on = &document["flags"]["accepting-traffic"];
        assert_eq!(on["variants"], json!({ "on": true, "off": false }));
        assert_eq!(on["defaultVariant"], json!("on"));
        assert_eq!(on["state"], json!("ENABLED"));
        assert_eq!(document["flags"]["legacy-checkout"]["defaultVariant"], json!("off"));
    }

    /// Anything else DOC holds is one value, not a choice between them, so it is the one variant
    /// flagd then serves — whatever its type, an object included.
    #[test]
    fn a_value_is_the_single_variant_flagd_serves() {
        let document = resolved().flagd();
        for (key, held) in [("greeting", json!("Hello")), ("page-size", json!(20))] {
            assert_eq!(document["flags"][key]["variants"], json!({ "value": held }), "{key}");
            assert_eq!(document["flags"][key]["defaultVariant"], json!("value"), "{key}");
        }
        assert_eq!(document["flags"]["limits"]["variants"]["value"], json!({ "burst": 5 }));
    }

    /// flagd knows one kind of flag, so DOC's configuration is served among them and says so, and
    /// a value read from a provider says which one it came from.
    #[test]
    fn the_document_says_what_each_value_is_and_where_it_came_from() {
        let document = resolved().flagd();
        assert_eq!(document["$schema"], json!(FLAGD_SCHEMA));
        assert_eq!(document["metadata"]["flagSetId"], json!("ledger-api/production"));
        assert_eq!(document["metadata"]["version"], json!("2663e9639a17def0"));
        assert_eq!(document["flags"]["greeting"]["metadata"]["doc.role"], json!("flag"));
        assert_eq!(document["flags"]["page-size"]["metadata"]["doc.role"], json!("config"));
        assert_eq!(document["flags"]["page-size"]["metadata"]["doc.source"], json!("runcfg"));
        // Nothing said where it came from, so it is DOC's own.
        assert_eq!(document["flags"]["limits"]["metadata"]["doc.source"], json!("doc"));
    }

    /// flagd's schema takes metadata of strings, numbers and booleans and nothing else: a nested
    /// object here would be refused by whatever reads the document, not by us.
    #[test]
    fn nothing_in_the_metadata_is_deeper_than_flagd_allows() {
        let document = resolved().flagd();
        let held = document["metadata"].as_object().expect("the document's metadata").clone();
        let each = document["flags"].as_object().expect("the flags").values();
        for metadata in each.map(|flag| flag["metadata"].clone()).chain([Value::Object(held)]) {
            for (key, value) in metadata.as_object().expect("metadata") {
                assert!(
                    value.is_string() || value.is_number() || value.is_boolean(),
                    "{key} is {value}"
                );
            }
        }
    }

    /// DOC resolves a service's scope rather than matching a targeting rule, which OpenFeature
    /// calls a static evaluation. The bulk answer holds both sides, since an SDK asks for one.
    #[test]
    fn every_evaluation_is_static_and_names_its_source() {
        let answer = resolved().open_feature();
        let flags = answer["flags"].as_array().expect("the flags");
        assert_eq!(flags.len(), 5);
        for flag in flags {
            assert_eq!(flag["reason"], json!("STATIC"), "{flag}");
            assert!(flag["variant"].is_string(), "{flag}");
            assert!(flag["metadata"]["source"].is_string(), "{flag}");
        }
        assert_eq!(answer["metadata"]["version"], json!("2663e9639a17def0"));
        assert_eq!(answer["metadata"]["service"], json!("ledger-api"));
    }

    /// A key is asked for by name, and a setting answers as readily as a switch.
    #[test]
    fn one_key_answers_from_either_side() {
        let resolved = resolved();
        let evaluated = resolved.evaluated("page-size", resolved.held("page-size").expect("held"));
        assert_eq!(evaluated["value"], json!(20));
        assert_eq!(evaluated["variant"], json!("20"));
        assert_eq!(evaluated["metadata"]["source"], json!("runcfg"));
        assert!(resolved.held("nothing-here").is_none());
    }
}
