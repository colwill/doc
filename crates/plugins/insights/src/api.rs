//! The JSON routes: every repository and its latest summary, a repository's latest scan or any
//! kept one, its scans, asking for a scan (the `scan` operation, for automations) and forgetting a
//! repository. Security findings and dependencies are left out without the `security` permission.

use doc_plugin_sdk::{Backend, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::report::without_security;
use crate::scanner::Scanner;
use crate::store::{self, Repository};
use crate::{Refusal, SECURITY, parameter, ui};

type Answer = Result<Value, Refusal>;

pub async fn handle(backend: &Backend, scanner: &Scanner, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let query = request.query.as_str();
    let answer = match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["api", "repositories"]) => repositories(backend).await,
        ("GET", ["api", "repositories", source, owner, name]) => {
            latest(backend, &id_of(source, owner, name), query).await
        }
        ("GET", ["api", "repositories", source, owner, name, "scans"]) => {
            scans(backend, &id_of(source, owner, name)).await
        }
        ("DELETE", ["api", "repositories", source, owner, name]) => {
            forget(backend, &id_of(source, owner, name)).await
        }
        ("POST", ["api", "scans"]) => {
            return match scan(backend, scanner, request).await {
                Ok(value) => Response::new(202, "application/json", value.to_string()),
                Err(refusal) => refusal.response(),
            };
        }
        _ => return Response::not_found(),
    };
    match answer {
        Ok(value) => Response::json(&value),
        Err(refusal) => refusal.response(),
    }
}

fn id_of(source: &str, owner: &str, name: &str) -> String {
    format!("{source}/{owner}/{name}").to_ascii_lowercase()
}

/// A repository's record as a caller sees it.
fn shown(held: &Repository, security: bool) -> Value {
    let mut summary = held.summary.clone().unwrap_or(Value::Null);
    if !security {
        without_security(&mut summary);
    }
    json!({
        "id": held.id,
        "source": held.source,
        "repository": held.repository,
        "branch": held.branch,
        "state": held.state,
        "why": held.why,
        "wanted_at": held.wanted_at,
        "started_at": held.started_at,
        "finished_at": held.finished_at,
        "commit": held.commit,
        "problem": held.problem,
        "latest": held.latest,
        "summary": summary,
        "page": held.href(),
    })
}

async fn repositories(backend: &Backend) -> Answer {
    let security = backend.allows(SECURITY, false);
    let all = store::repositories(backend).await?;
    Ok(json!({ "repositories": all.iter().map(|held| shown(held, security)).collect::<Vec<_>>() }))
}

async fn held(backend: &Backend, id: &str) -> Result<Repository, Refusal> {
    store::repository(backend, id)
        .await?
        .ok_or_else(|| Refusal::missing(format!("{id} has not been scanned")))
}

async fn latest(backend: &Backend, id: &str, query: &str) -> Answer {
    let held = held(backend, id).await?;
    let security = backend.allows(SECURITY, false);
    let asked = parameter(query, "scan").or_else(|| held.latest.clone());
    let scan = match asked {
        Some(scan) => {
            store::scan(backend, &scan).await?.filter(|scan| scan.repository_id == held.id)
        }
        None => None,
    };
    let scan = scan.map(|scan| {
        let mut shown = json!(scan);
        if !security {
            without_security(&mut shown["summary"]);
            without_security(&mut shown["report"]);
        }
        shown
    });
    Ok(json!({ "repository": shown(&held, security), "scan": scan }))
}

async fn scans(backend: &Backend, id: &str) -> Answer {
    let held = held(backend, id).await?;
    let security = backend.allows(SECURITY, false);
    let history = store::history(backend, &held.id, 500).await?;
    let listed: Vec<Value> = history
        .into_iter()
        .map(|scan| {
            let mut shown = json!(scan);
            if let Some(object) = shown.as_object_mut() {
                object.remove("report");
            }
            if !security {
                without_security(&mut shown["summary"]);
            }
            shown
        })
        .collect();
    Ok(json!({ "scans": listed }))
}

async fn forget(backend: &Backend, id: &str) -> Answer {
    let held = held(backend, id).await?;
    store::forget(backend, &held.id).await?;
    Ok(json!({ "forgotten": held.id }))
}

#[derive(Deserialize)]
struct Asked {
    repository: String,
    #[serde(default)]
    source: Option<String>,
}

/// Asks for a repository to be scanned; the operation automations call.
async fn scan(backend: &Backend, scanner: &Scanner, request: &Request) -> Answer {
    let asked: Asked = request.json().map_err(|err| Refusal::bad(err.to_string()))?;
    let scanning = crate::settings::Scanning::read(&backend.settings());
    let source = asked
        .source
        .filter(|source| !source.trim().is_empty())
        .or_else(|| scanning.first_source().map(str::to_string))
        .ok_or_else(|| Refusal::bad("this plugin's settings name no source"))?;
    let (held, said) = ui::queue(backend, scanner, &source, &asked.repository).await?;
    Ok(
        json!({ "repository": held.repository, "state": held.state, "said": said, "page": held.href() }),
    )
}
