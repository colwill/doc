//! The JSON routes: what the Settings page offers — the vendor accounts that let this plugin
//! through, and the dashboards and the Catalogue's services — and the dashboards, a dashboard's
//! panels and variables, and what a panel's queries answer, for scripts and agents.

use std::collections::BTreeMap;

use doc_plugin_sdk::{Backend, Choice, Choices, Request, Response};
use serde_json::{Value, json};

use crate::dashboard::Scope;
use crate::grafana::{self, Grafana};
use crate::settings::Config;
use crate::view::{self, connected};
use crate::{Refusal, parameter};

type Answer = Result<Value, Refusal>;

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let config = Config::read(&backend.settings());
    let query = request.query.as_str();
    let answer = match (request.method.as_str(), path) {
        ("GET", ["settings", "accounts"]) => account_choices(backend).await,
        ("GET", ["settings", "dashboards"]) => dashboard_choices(backend, &config).await,
        ("GET", ["dashboards"]) => dashboards(backend, &config, query).await,
        ("GET", ["dashboards", uid]) => match view::uid(uid) {
            Ok(uid) => dashboard(backend, &config, uid).await,
            Err(refusal) => Err(refusal),
        },
        ("GET", ["dashboards", uid, "panels", id]) => match view::uid(uid) {
            Ok(uid) => panel(backend, &config, uid, id, query).await,
            Err(refusal) => Err(refusal),
        },
        _ => Err(Refusal::missing("no such route")),
    };
    match answer {
        Ok(value) => Response::json(&value),
        Err(refusal) => refusal.response(),
    }
}

/// The proxied accounts in Secret Storage that let this plugin through.
async fn account_choices(backend: &Backend) -> Answer {
    let choices = grafana::accounts(backend)
        .await?
        .into_iter()
        .map(|account| {
            let title = if account.title.is_empty() { &account.name } else { &account.title };
            Choice::new(&account.name, title).hinted(&account.address)
        })
        .collect();
    Ok(json!(Choices { choices, ..Choices::default() }))
}

/// Every dashboard the account sees, and the Catalogue's services, asked as the viewer.
async fn dashboard_choices(backend: &Backend, config: &Config) -> Answer {
    let choices = match config.account.as_deref() {
        Some(account) => Grafana::new(backend, account)
            .search("")
            .await?
            .into_iter()
            .map(|found| Choice::new(&found.uid, &found.title).hinted(&found.folder))
            .collect(),
        None => Vec::new(),
    };
    let listed = match backend
        .ask("resources", "GET", "resources", Some("kind=service&limit=500"), None)
        .await
    {
        Ok((200, Value::Array(listed))) => listed,
        Ok((status, body)) => {
            let said = body["detail"].as_str().unwrap_or("it gave no reason");
            return Err(Refusal {
                status,
                detail: format!("the Catalogue would not list its services: {said}"),
            });
        }
        Err(err) => {
            return Err(Refusal::unavailable(format!("the Catalogue could not be asked: {err}")));
        }
    };
    let mut values: Vec<Choice> = listed
        .iter()
        .filter_map(|service| {
            let name = service["name"].as_str()?;
            let title = service["title"].as_str().filter(|title| !title.is_empty()).unwrap_or(name);
            Some(Choice::new(name, title))
        })
        .collect();
    values.sort_by(|one, two| one.value.cmp(&two.value));
    Ok(json!(Choices {
        choices,
        values,
        choices_label: "Grafana dashboard".into(),
        values_label: "Services".into(),
    }))
}

async fn dashboards(backend: &Backend, config: &Config, query: &str) -> Answer {
    let grafana = Grafana::new(backend, connected(config)?);
    let q = parameter(query, "q").unwrap_or_default();
    let found = grafana.search(&q).await?;
    let shown = found
        .iter()
        .filter(|held| config.dashboards.is_empty() || config.dashboards.contains_key(&held.uid))
        .map(|held| {
            json!({
                "uid": held.uid, "title": held.title, "folder": held.folder, "tags": held.tags,
                "services": config.dashboards.get(&held.uid).cloned().unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({ "dashboards": shown }))
}

async fn dashboard(backend: &Backend, config: &Config, uid: &str) -> Answer {
    let grafana = Grafana::new(backend, connected(config)?);
    let board = view::board(&grafana, uid).await?;
    let sections: Vec<Value> = board
        .sections
        .iter()
        .map(|section| {
            let panels: Vec<Value> = section
                .panels
                .iter()
                .map(|panel| json!({ "id": panel.id, "title": panel.title, "type": panel.kind }))
                .collect();
            json!({ "title": section.title, "panels": panels })
        })
        .collect();
    let variables: Vec<Value> = board
        .variables
        .iter()
        .filter(|variable| !variable.hidden)
        .map(|variable| {
            let options: Vec<Value> = variable
                .options
                .iter()
                .map(|(text, value)| json!({ "text": text, "value": value }))
                .collect();
            json!({
                "name": variable.name, "label": variable.label, "type": variable.kind,
                "current": variable.current, "options": options,
            })
        })
        .collect();
    Ok(json!({
        "uid": board.uid, "title": board.title, "folder": board.folder,
        "time": { "from": board.from, "to": board.to },
        "services": config.dashboards.get(uid).cloned().unwrap_or_default(),
        "variables": variables, "sections": sections,
    }))
}

/// What a panel's queries answer, as series: the same `range` and `var-<name>` as its page.
async fn panel(backend: &Backend, config: &Config, uid: &str, id: &str, query: &str) -> Answer {
    let grafana = Grafana::new(backend, connected(config)?);
    let board = view::board(&grafana, uid).await?;
    let panel = view::panel(&grafana, &board, id).await?;
    let scope = Scope::new(&board, query, &BTreeMap::new());
    let answer = view::answer(&grafana, &scope, &panel).await.map_err(Refusal::bad)?;
    let series: Vec<Value> = answer
        .series()
        .iter()
        .map(|series| {
            json!({
                "name": series.name, "unit": series.unit, "timed": series.timed,
                "points": series.points,
            })
        })
        .collect();
    Ok(json!({
        "panel": { "id": panel.id, "title": panel.title, "type": panel.kind, "unit": panel.unit() },
        "range": {
            "from": scope.range.from, "to": scope.range.to,
            "start": scope.range.start, "end": scope.range.end,
        },
        "variables": scope.values,
        "series": series,
        "problems": answer.problems,
    }))
}
