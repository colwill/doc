//! The JSON routes under `api/`: clusters, workloads, rollouts and outages as last read, and the
//! choices the Settings page offers for environments.

use doc_plugin_sdk::{Backend, Choice, Choices, Order, Query, Request, Response};
use serde_json::{Value, json};

use crate::Refusal;
use crate::settings::Config;
use crate::store::{ClusterRecord, NamespaceRecord, OutageRecord, Rollout, WorkloadRecord};

type Answer = Result<Value, Refusal>;

const MAX_LIMIT: u32 = 500;

pub fn parameter(query: &str, key: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// A filter from whichever of `keys` the query string gives.
fn filtered(query: &str, keys: &[&str]) -> Value {
    let mut filter = serde_json::Map::new();
    for key in keys {
        if let Some(value) = parameter(query, key) {
            filter.insert((*key).to_string(), json!(value));
        }
    }
    Value::Object(filter)
}

fn limit(query: &str) -> Result<u32, Refusal> {
    match parameter(query, "limit") {
        None => Ok(100),
        Some(text) => match text.parse::<u32>() {
            Ok(limit) if (1..=MAX_LIMIT).contains(&limit) => Ok(limit),
            _ => Err(Refusal::bad(format!("limit is a number from 1 to {MAX_LIMIT}"))),
        },
    }
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let query = request.query.as_str();
    let answered = match (request.method.as_str(), path) {
        ("GET", ["settings", "environments"]) => environment_choices(backend).await,
        ("GET", ["clusters"]) => clusters(backend).await,
        ("GET", ["clusters", name]) => cluster(backend, name).await,
        ("GET", ["workloads"]) => workloads(backend, query).await,
        ("GET", ["rollouts"]) => rollouts(backend, query).await,
        ("GET", ["outages"]) => outages(backend, query).await,
        _ => Err(Refusal::missing("no such route")),
    };
    match answered {
        Ok(value) => Response::json(&value),
        Err(refusal) => refusal.response(),
    }
}

/// Every cluster, and every namespace read in one, to give an environment to.
async fn environment_choices(backend: &Backend) -> Answer {
    let config = Config::read(&backend.settings());
    let namespaces: Vec<NamespaceRecord> = backend
        .query_all(Query::new("namespaces").order(Order::asc("id")))
        .await
        .unwrap_or_default();
    let mut choices: Vec<Choice> = config
        .clusters
        .iter()
        .map(|cluster| Choice::new(cluster, cluster).hinted("the whole cluster"))
        .collect();
    choices.extend(
        namespaces
            .iter()
            .filter(|namespace| config.clusters.contains(&namespace.cluster))
            .map(|namespace| Choice::new(&namespace.id, &namespace.id).hinted("one namespace")),
    );
    let values = config.names.iter().map(|name| Choice::new(name, name)).collect();
    Ok(json!(Choices {
        choices,
        values,
        choices_label: "Cluster or cluster/namespace".into(),
        values_label: "Environment".into(),
    }))
}

async fn clusters(backend: &Backend) -> Answer {
    let clusters: Vec<ClusterRecord> =
        backend.query_all(Query::new("clusters").order(Order::asc("id"))).await?;
    Ok(json!({ "clusters": clusters }))
}

async fn cluster(backend: &Backend, name: &str) -> Answer {
    let cluster: ClusterRecord = backend
        .get("clusters", name)
        .await?
        .ok_or_else(|| Refusal::missing(format!("there is no cluster called {name}")))?;
    let of = |collection: &str| {
        Query::new(collection).filter(json!({ "cluster": name })).order(Order::asc("id"))
    };
    let namespaces: Vec<NamespaceRecord> = backend.query_all(of("namespaces")).await?;
    let workloads: Vec<WorkloadRecord> = backend.query_all(of("workloads")).await?;
    Ok(json!({ "cluster": cluster, "namespaces": namespaces, "workloads": workloads }))
}

async fn workloads(backend: &Backend, query: &str) -> Answer {
    let filter = filtered(query, &["cluster", "namespace", "service", "environment", "state"]);
    let workloads: Vec<WorkloadRecord> =
        backend.query_all(Query::new("workloads").filter(filter).order(Order::asc("id"))).await?;
    Ok(json!({ "workloads": workloads }))
}

async fn rollouts(backend: &Backend, query: &str) -> Answer {
    let filter = filtered(
        query,
        &["cluster", "namespace", "workload", "service", "repository", "environment", "state"],
    );
    let found = backend
        .query::<Rollout>(
            Query::new("rollouts")
                .filter(filter)
                .order(Order::desc("started_at"))
                .limit(limit(query)?),
        )
        .await?;
    Ok(json!({ "rollouts": found.records }))
}

async fn outages(backend: &Backend, query: &str) -> Answer {
    let filter = filtered(query, &["cluster", "service", "environment"]);
    let found = backend
        .query::<OutageRecord>(
            Query::new("outages")
                .filter(filter)
                .order(Order::desc("started_at"))
                .limit(limit(query)?),
        )
        .await?;
    Ok(json!({ "outages": found.records }))
}
