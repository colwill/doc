//! What a page, a call or an agent is shown: a scope's services, each judged by its own
//! objectives over a period, or DOC — as a whole, part by part and plugin by plugin.

use doc_plugin_sdk::{Backend, Query};
use serde_json::{Value, json};

use crate::Refusal;
use crate::metrics::{self, Held, Judged, Period};
use crate::platform::{self, COMPONENTS};
use crate::scope::{self, Member, Scope};
use crate::settings::Definitions;

pub fn key(service: &str) -> String {
    format!("service:{service}")
}

/// A scope's services judged over a period.
pub struct Services {
    pub members: Vec<Member>,
    pub held: Held,
    pub judged: Vec<Judged>,
}

impl Services {
    /// Each service with the objectives it is held to, for charts.
    pub fn objectives(&self, definitions: &Definitions) -> Vec<(String, crate::settings::Targets)> {
        self.members
            .iter()
            .map(|member| {
                let key = key(&member.name);
                let targets = self
                    .held
                    .subjects
                    .get(&key)
                    .map_or(definitions.services, |held| held.targets(definitions.services));
                (key, targets)
            })
            .collect()
    }
}

pub async fn services(
    backend: &Backend,
    scope: &Scope,
    period: &Period,
) -> Result<Services, Refusal> {
    let definitions = Definitions::read(&backend.settings());
    let members = scope::services(backend, scope).await?;
    let keys: Vec<String> = members.iter().map(|member| key(&member.name)).collect();
    let held = metrics::held(backend, &keys, period).await?;
    let judged = members
        .iter()
        .map(|member| {
            let key = key(&member.name);
            let targets = held
                .subjects
                .get(&key)
                .map_or(definitions.services, |held| held.targets(definitions.services));
            metrics::judge(&key, &member.title, targets, &held, period, definitions.at_risk)
        })
        .collect();
    Ok(Services { members, held, judged })
}

/// DOC judged over a period.
pub struct Doc {
    pub whole: Judged,
    /// What the whole was judged from, as the one subject `doc`.
    pub whole_held: Held,
    pub held: Held,
    pub parts: Vec<Judged>,
    pub plugins: Vec<Judged>,
}

pub async fn doc(backend: &Backend, period: &Period) -> Result<Doc, Refusal> {
    let definitions = Definitions::read(&backend.settings());
    let parts: Vec<(String, String)> = COMPONENTS
        .iter()
        .map(|(name, title)| (format!("doc:{name}"), (*title).to_string()))
        .collect();
    // Only plugins that have run here: the configuration names some that never have.
    let registered: Vec<Value> = backend
        .query_all(
            Query::new("core.plugins")
                .filter(json!({ "registered_at": { "is_null": false } }))
                .fields(&["id", "display_name"]),
        )
        .await?;
    let mut plugins: Vec<(String, String)> = registered
        .iter()
        .filter_map(|plugin| {
            let id = plugin["id"].as_str()?;
            let title =
                plugin["display_name"].as_str().filter(|title| !title.is_empty()).unwrap_or(id);
            Some((format!("plugin:{id}"), title.to_string()))
        })
        .collect();
    plugins.sort_by(|one, two| one.1.cmp(&two.1));
    let keys: Vec<String> = parts.iter().chain(&plugins).map(|(key, _)| key.clone()).collect();
    let held = metrics::held(backend, &keys, period).await?;
    let part_keys: Vec<String> = parts.iter().map(|(key, _)| key.clone()).collect();
    let (whole, whole_held) = metrics::composite(&held, period, &definitions, &part_keys);
    let judged = |listed: &[(String, String)]| -> Vec<Judged> {
        listed
            .iter()
            .map(|(key, title)| {
                metrics::judge(key, title, definitions.doc, &held, period, definitions.at_risk)
            })
            .collect()
    };
    Ok(Doc { parts: judged(&parts), plugins: judged(&plugins), whole, whole_held, held })
}

/// What a subject is called on a page, from its key.
pub fn titled(subject: &str) -> String {
    match subject.split_once(':') {
        Some(("doc", name)) => format!("DOC's {}", platform::titled(name).to_ascii_lowercase()),
        Some(("plugin", id)) => format!("DOC's {id} plugin"),
        Some((_, name)) => name.to_string(),
        None => subject.to_string(),
    }
}
