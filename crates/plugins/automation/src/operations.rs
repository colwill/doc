//! Operations other plugins offer automations by name (ADR-0012): which ones the caller can call,
//! found through `core.access`, and what calling one asks of its plugin.

use std::collections::BTreeMap;
use std::time::Duration;

use doc_plugin_sdk::{Backend, Operation};
use serde_json::{Map, Value, json};
use url::form_urlencoded::{Serializer, byte_serialize};

use crate::Refusal;
use crate::conditions::render;
use crate::model::Action;

/// One operation, and the plugin that offers it.
pub struct Offered {
    pub plugin: String,
    pub operation: Operation,
}

impl Offered {
    /// How a form names it: `dora.increment`.
    pub fn key(&self) -> String {
        format!("{}.{}", self.plugin, self.operation.name)
    }
}

/// Every operation the caller can call: those of the plugins they can use that are running now,
/// by plugin and then by name.
pub async fn offered(backend: &Backend) -> Result<Vec<Offered>, Refusal> {
    let access = backend
        .request("core.access", "access", json!({}), Duration::from_secs(10))
        .await
        .map_err(|err| {
            Refusal::unavailable(format!("core could not say what you can use: {err}"))
        })?;
    let mut found = Vec::new();
    for (plugin, held) in access["plugins"].as_object().into_iter().flatten() {
        if held["read"] != true || held["running"] != true {
            continue;
        }
        let operations: Vec<Operation> =
            serde_json::from_value(held["operations"].clone()).unwrap_or_default();
        found.extend(
            operations.into_iter().map(|operation| Offered { plugin: plugin.clone(), operation }),
        );
    }
    found.sort_by(|a, b| {
        a.plugin.cmp(&b.plugin).then_with(|| a.operation.name.cmp(&b.operation.name))
    });
    Ok(found)
}

/// One operation the caller can call, or why not.
pub async fn find(backend: &Backend, plugin: &str, name: &str) -> Result<Operation, Refusal> {
    offered(backend)
        .await?
        .into_iter()
        .find(|offered| offered.plugin == plugin && offered.operation.name == name)
        .map(|offered| offered.operation)
        .ok_or_else(|| {
            Refusal::bad(format!("{plugin} does not offer {name}, or you cannot use {plugin}"))
        })
}

/// Checks the operation actions among `actions` as whoever is saving them: each is offered to
/// them, and each of its required parameters has a value.
pub async fn checked(backend: &Backend, actions: &[Action]) -> Result<(), Refusal> {
    for action in actions {
        let Action::Operation { plugin, operation, params } = action else { continue };
        let declared = find(backend, plugin, operation).await?;
        if let Some(missing) = declared.params.iter().find(|param| {
            param.required && params.get(&param.name).is_none_or(|value| value.trim().is_empty())
        }) {
            return Err(Refusal::bad(format!(
                "{} ({plugin}) needs its {}",
                declared.label, missing.label
            )));
        }
        if let Some(unknown) =
            params.keys().find(|name| !declared.params.iter().any(|param| &param.name == *name))
        {
            return Err(Refusal::bad(format!("{} ({plugin}) takes no {unknown}", declared.label)));
        }
    }
    Ok(())
}

/// What calling it asks of its plugin: the method, the route with its parameters filled in, and
/// the rest of them as the query or the body. Every value is a template, filled from `context`.
pub struct Call {
    pub method: String,
    pub route: String,
    pub query: Option<String>,
    pub body: Option<Value>,
}

pub fn call(operation: &Operation, params: &BTreeMap<String, String>, context: &Value) -> Call {
    let filled: BTreeMap<&str, String> =
        params.iter().map(|(name, template)| (name.as_str(), render(template, context))).collect();
    let mut route = operation.route.clone();
    for (name, value) in &filled {
        let encoded: String =
            byte_serialize(value.as_bytes()).collect::<String>().replace('+', "%20");
        route = route.replace(&format!("{{{name}}}"), &encoded);
    }
    let rest: Vec<(&str, &String)> = filled
        .iter()
        .filter(|(name, _)| !operation.route.contains(&format!("{{{name}}}")))
        .map(|(name, value)| (*name, value))
        .collect();
    let (query, body) = match operation.method.as_str() {
        "GET" | "DELETE" => {
            let mut query = Serializer::new(String::new());
            rest.iter().for_each(|(name, value)| {
                query.append_pair(name, value);
            });
            let query = query.finish();
            ((!query.is_empty()).then_some(query), None)
        }
        _ => {
            let body: Map<String, Value> = rest
                .into_iter()
                .map(|(name, value)| (name.to_string(), Value::String(value.clone())))
                .collect();
            (None, Some(Value::Object(body)))
        }
    };
    Call { method: operation.method.clone(), route, query, body }
}
