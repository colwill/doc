//! `/p/faux-data/`: which plugins faux data is provided to, and the estate it is made up for —
//! each service the viewer can see in the Catalogue, or the made-up ones where it has none, with
//! its profile and stand-ins.

use askama::Template as Page;
use doc_plugin_sdk::{Backend, Query, Request, Response};
use serde_json::Value;

use crate::estate::{self, Profile};
use crate::settings::{CONSUMERS, Config};

/// One plugin faux data can be provided to.
pub struct Consumer {
    pub id: &'static str,
    pub title: &'static str,
    pub gives: &'static str,
    pub on: bool,
    pub running: bool,
}

/// One service of the estate.
pub struct Service {
    pub name: String,
    pub title: String,
    pub profile: Profile,
    pub repository: String,
    pub project: String,
    pub runs: String,
}

#[derive(Page)]
#[template(path = "home.html")]
struct Home {
    consumers: Vec<Consumer>,
    services: Vec<Service>,
    /// Where the services came from, in a sentence.
    about: String,
    settings: bool,
}

fn drawn<T: Page>(page: &T) -> Response {
    match page.render() {
        Ok(html) => Response::html(html),
        Err(err) => {
            Response::problem(500, "internal", &format!("the page could not be drawn: {err}"))
        }
    }
}

pub async fn handle(backend: &Backend, request: &Request, config: &Config) -> Response {
    match (request.method.as_str(), request.path.trim_end_matches('/')) {
        ("GET", "ui") => home(backend, config).await,
        _ => Response::not_found(),
    }
}

/// The services the viewer can see in the Catalogue, by name and title.
async fn catalogue(backend: &Backend) -> Option<Vec<(String, String)>> {
    let asked = backend.ask("resources", "GET", "resources", Some("kind=service&limit=500"), None);
    let Ok((200, Value::Array(listed))) = asked.await else { return None };
    Some(
        listed
            .iter()
            .filter_map(|resource| {
                let name = resource["name"].as_str()?.to_string();
                let title = resource["title"].as_str().filter(|title| !title.is_empty());
                let title = title.unwrap_or(&name).to_string();
                Some((name, title))
            })
            .collect(),
    )
}

async fn home(backend: &Backend, config: &Config) -> Response {
    let running: Vec<String> = backend
        .query_all::<Value>(Query::new("core.plugins").fields(&["id", "state"]))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|plugin| matches!(plugin["state"].as_str(), Some("running" | "cancelled")))
        .filter_map(|plugin| plugin["id"].as_str().map(str::to_string))
        .collect();
    let consumers = CONSUMERS
        .iter()
        .map(|(id, title, gives)| Consumer {
            id,
            title,
            gives,
            on: config.serving.contains(*id),
            running: running.iter().any(|running| running == id),
        })
        .collect();
    let (listed, about) = match catalogue(backend).await {
        Some(listed) if !listed.is_empty() => {
            (listed, "Each service you can see in the Catalogue.".to_string())
        }
        Some(_) => (
            config.services.clone(),
            "The Catalogue has no services, so these are made up too.".to_string(),
        ),
        None => (
            config.services.clone(),
            "The Catalogue could not be read as you, so these are the services made up where \
             it has none."
                .to_string(),
        ),
    };
    let services = listed
        .into_iter()
        .map(|(name, title)| {
            let profile = estate::profile(config, &name);
            Service {
                repository: estate::repository(&name),
                project: estate::project(&name),
                runs: estate::products(profile).join(", "),
                profile,
                name,
                title,
            }
        })
        .collect();
    let settings = backend.caller().is_some_and(|caller| caller.admin);
    drawn(&Home { consumers, services, about, settings })
}
