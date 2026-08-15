//! The Jira project a service is given, chosen from the projects the source plugins have read and
//! kept in the Catalogue as its `jira/project-key`, with `jira/component` and `jira/label` for a
//! project several services share. Written as whoever saves it, so the Catalogue decides whether
//! they may.

use std::collections::BTreeMap;

use doc_plugin_sdk::{Backend, Query};
use serde_json::{Map, Value, json};

use crate::plan::{self, Project};
use crate::scope::{self, COMPONENT, LABEL, PROJECT_KEY};
use crate::settings::Definitions;
use crate::{Refusal, faux};

/// Every project the source plugins have read, by name, each once.
pub async fn projects(backend: &Backend) -> Result<Vec<Project>, Refusal> {
    let mut found: BTreeMap<String, Project> = BTreeMap::new();
    if faux::on(backend) {
        let every = scope::every(backend).await?;
        for ((_, key), project) in faux::held(backend, &every).await?.projects {
            found.entry(key).or_insert(project);
        }
    } else {
        for source in &Definitions::read(&backend.settings()).sources {
            let query = Query::new(&format!("{source}.projects"));
            for project in plan::exported::<Project>(backend, query).await? {
                found.entry(project.key.to_ascii_uppercase()).or_insert(project);
            }
        }
    }
    let mut projects: Vec<Project> = found.into_values().collect();
    projects.sort_by_key(|project| (project.name.to_lowercase(), project.key.clone()));
    Ok(projects)
}

/// What a service's metadata says now, and the rest of it, which saving keeps as it is.
pub struct Given {
    pub projects: Vec<String>,
    pub component: String,
    pub label: String,
    metadata: Map<String, Value>,
}

/// A service's metadata, read from the Catalogue as whoever is looking.
pub async fn given(backend: &Backend, service: &str) -> Result<Given, Refusal> {
    let route = format!("resources/service/{}", segment(service));
    let body = match backend.ask("resources", "GET", &route, None, None).await {
        Ok((200, body)) => body,
        Ok((status, body)) => return Err(refused(status, &body)),
        Err(err) => {
            return Err(Refusal::unavailable(format!("the Catalogue could not be asked: {err}")));
        }
    };
    let Some(metadata) = body["resource"]["metadata"].as_object().cloned() else {
        return Err(Refusal::missing(format!("{service} is not in the Catalogue")));
    };
    let text = |key: &str| match metadata.get(key) {
        Some(Value::String(text)) => text.trim().to_string(),
        Some(Value::Array(items)) => {
            items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(",")
        }
        _ => String::new(),
    };
    let projects = text(PROJECT_KEY)
        .split(',')
        .map(|key| key.trim().to_ascii_uppercase())
        .filter(|key| !key.is_empty())
        .collect();
    Ok(Given { projects, component: text(COMPONENT), label: text(LABEL), metadata })
}

/// Gives a service one project, or none when `project` is empty, leaving the rest of its metadata
/// as it is.
pub async fn give(
    backend: &Backend,
    service: &str,
    project: &str,
    component: &str,
    label: &str,
) -> Result<(), Refusal> {
    let mut metadata = given(backend, service).await?.metadata;
    let project = project.trim().to_ascii_uppercase();
    let wanted = [
        (PROJECT_KEY, project.as_str()),
        // Narrowing means nothing without a project to narrow.
        (COMPONENT, if project.is_empty() { "" } else { component.trim() }),
        (LABEL, if project.is_empty() { "" } else { label.trim() }),
    ];
    for (key, value) in wanted {
        match value.is_empty() {
            true => metadata.remove(key),
            false => metadata.insert(key.to_string(), Value::String(value.to_string())),
        };
    }
    let document = json!([{ "kind": "Service", "name": service, "metadata": metadata }]);
    match backend.ask("resources", "POST", "apply", None, Some(document)).await {
        Ok((200 | 201, _)) => Ok(()),
        Ok((status, body)) => Err(refused(status, &body)),
        Err(err) => Err(Refusal::unavailable(format!("the Catalogue could not be asked: {err}"))),
    }
}

fn refused(status: u16, body: &Value) -> Refusal {
    let said = body["detail"].as_str().unwrap_or("it refused");
    match status {
        401 | 403 => Refusal::forbidden(format!(
            "a service's Jira project is kept in the Catalogue, which says: {said}"
        )),
        404 => Refusal::missing(format!("the Catalogue says: {said}")),
        400 | 409 | 422 => Refusal::bad(format!("the Catalogue says: {said}")),
        _ => Refusal::unavailable(format!("the Catalogue answered {status}: {said}")),
    }
}

/// A name as one segment of a path: the Catalogue decodes `%xx`, and never `+`.
pub fn segment(name: &str) -> String {
    url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>().replace('+', "%20")
}
