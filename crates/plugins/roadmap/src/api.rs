//! The JSON routes: a scope's releases with where each stands and whether its services are ready,
//! and one release with its issues.

use doc_plugin_sdk::{Backend, Choice, Choices, Query, Request, Response};
use serde_json::{Value, json};

use crate::plan::{self, Project};
use crate::readiness;
use crate::scope::{self, Scope};
use crate::settings::{self, Definitions};
use crate::view;
use crate::{Refusal, faux, parameter};

type Answer = Result<Value, Refusal>;

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let query = request.query.as_str();
    let answer = match (request.method.as_str(), path.as_str()) {
        (_, "api/mcp") => return crate::mcp::handle(backend, request).await,
        ("GET", "api/releases") => releases(backend, query).await,
        ("GET", route) if route == format!("api/{}", settings::PROJECT_CHOICES) => {
            project_choices(backend).await
        }
        ("GET", route) if route.starts_with("api/releases/") => {
            let key = decoded(&route["api/releases/".len()..]);
            release(backend, &key).await
        }
        _ => return Response::not_found(),
    };
    match answer {
        Ok(value) => Response::json(&value),
        Err(refusal) => refusal.response(),
    }
}

/// What **Projects tracked** on the Settings page offers: every project the sources read, and
/// every service in the Catalogue, asked as whoever is looking.
async fn project_choices(backend: &Backend) -> Answer {
    let definitions = Definitions::read(&backend.settings());
    let mut choices: Vec<Choice> = Vec::new();
    for source in &definitions.sources {
        let projects: Vec<Project> =
            plan::exported(backend, Query::new(&format!("{source}.projects"))).await?;
        for project in projects {
            let key = project.key.to_ascii_uppercase();
            match choices.iter_mut().find(|choice| choice.value == key) {
                Some(held) => held.hint = format!("{}, {source}", held.hint),
                None => {
                    let name = if project.name.is_empty() { &key } else { &project.name };
                    choices.push(Choice::new(&key, &format!("{name} ({key})")).hinted(source));
                }
            }
        }
    }
    choices.sort_by_key(|choice| choice.label.to_lowercase());
    let values = scope::catalogued(backend)
        .await?
        .into_iter()
        .map(|service| Choice::new(&service.name, &service.title))
        .collect();
    Ok(json!(Choices {
        choices,
        values,
        choices_label: "Jira project".into(),
        values_label: "Services".into(),
    }))
}

fn decoded(text: &str) -> String {
    url::form_urlencoded::parse(format!("x={text}").as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

fn flag(query: &str, name: &str) -> Option<bool> {
    parameter(query, name).map(|value| !matches!(value.as_str(), "0" | "false" | "no"))
}

/// Unreleased releases, or with `released=true` those shipped lately; with each one's readiness
/// unless `readiness=false`.
async fn releases(backend: &Backend, query: &str) -> Answer {
    let scope = Scope::from_query(query)?;
    let shipped = flag(query, "released").unwrap_or(false);
    let ask = !shipped && flag(query, "readiness").unwrap_or(true);
    let view = view::read(backend, &scope, ask).await?;
    let listed: Vec<Value> = view
        .releases
        .iter()
        .filter(|release| release.version.released == shipped)
        .map(|release| {
            let columns = view.columns(release);
            let mut value = release.json(false);
            value["readiness"] = json!({ "worst": readiness::worst(&columns), "plugins": columns });
            value
        })
        .collect();
    Ok(json!({
        "scope": { "kind": scope.kind(), "name": scope.name() },
        "releases": listed,
        "faux": faux::said(),
    }))
}

async fn release(backend: &Backend, key: &str) -> Answer {
    let (release, columns) = view::one(backend, key).await?;
    let mut value = release.json(true);
    value["readiness"] = json!({ "worst": readiness::worst(&columns), "plugins": columns });
    value["faux"] = faux::said();
    Ok(value)
}
