//! Your teams' services, on a person's dashboard: every service the teams they are in own or are
//! connected to, worst first by what the plugins that measure services say of each — maturity,
//! delivery, pipelines, reliability and end of life — asked as them, once each for all of them.

use std::collections::BTreeMap;
use std::time::Duration;

use askama::Template;
use doc_plugin_sdk::{Backend, Query};
use serde_json::Value;

use super::{href, render};
use crate::graph::Graph;
use crate::kinds::{Kind, Ref};
use crate::model::Refusal;
use crate::store::Store;

/// The plugins answering `api/readiness` for services.
const MEASURING: [(&str, &str); 5] = [
    ("maturity", "Maturity"),
    ("dora", "Delivery"),
    ("cicd", "Pipelines"),
    ("reliability", "Reliability"),
    ("eol", "End of life"),
];
/// How long a measuring plugin is given to answer.
const ASKING: Duration = Duration::from_secs(5);
/// The most services listed.
const LISTED: usize = 8;

/// One service as the dashboard lists it.
pub struct Row {
    pub href: String,
    pub title: String,
    pub teams: String,
    /// The worst any measuring plugin says, and which said it and why.
    pub word: &'static str,
    pub badge: &'static str,
    pub said: String,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct ServicesFragment {
    rows: Vec<Row>,
    more: usize,
    teams: usize,
}

/// Worst first, as the readiness states are ordered.
fn rank(state: &str) -> (u8, &'static str, &'static str) {
    match state {
        "blocked" => (0, "Blocked", "error"),
        "warning" => (1, "Warning", "degraded"),
        "ready" => (2, "Ready", "ready"),
        _ => (3, "Not known", "unknown"),
    }
}

/// The measuring plugins running now: one that is not would hold the page up to its deadline.
async fn running(backend: &Backend) -> Vec<String> {
    let plugins: Vec<Value> = backend
        .query_all(Query::new("core.plugins").fields(&["id", "state"]))
        .await
        .unwrap_or_default();
    plugins
        .iter()
        .filter(|plugin| plugin["state"] == "running")
        .filter_map(|plugin| plugin["id"].as_str().map(str::to_string))
        .collect()
}

pub async fn services(backend: &Backend, store: &Store<'_>) -> Result<String, Refusal> {
    let caller = backend.caller().ok_or_else(|| Refusal::forbidden("nobody is asking"))?;
    let me = caller.id.clone().filter(|_| caller.kind == "user");
    let me = me.ok_or_else(|| Refusal::forbidden("a dashboard is a person's"))?;
    let graph = Graph::new(store).await?;
    let teams = store.teams_of(&me).await?;
    // Each service once, with the teams it is theirs through.
    let mut found: BTreeMap<String, (String, Vec<String>)> = BTreeMap::new();
    for team in &teams {
        let node = graph.node(&Ref::new(Kind::Team, team.name.clone())).await?;
        let (neighbours, _) = graph.neighbours(&node).await?;
        let mut theirs: Vec<(String, String)> = neighbours
            .iter()
            .filter(|neighbour| neighbour.kind == Kind::Service)
            .map(|neighbour| (neighbour.name.clone(), neighbour.title.clone()))
            .collect();
        for owned in store.owned_by(team.id).await? {
            if owned.kind == Kind::Service {
                theirs.push((owned.name, owned.title));
            }
        }
        let label = match team.title.is_empty() {
            true => team.name.clone(),
            false => team.title.clone(),
        };
        for (name, title) in theirs {
            let held = found.entry(name).or_insert_with(|| (title, Vec::new()));
            if !held.1.contains(&label) {
                held.1.push(label.clone());
            }
        }
    }
    let names: Vec<String> = found.keys().cloned().collect();
    let mut worst: BTreeMap<String, (u8, &'static str, &'static str, String)> = BTreeMap::new();
    if !names.is_empty() {
        let asked: String = names
            .iter()
            .map(|name| {
                format!(
                    "service={}",
                    url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>()
                )
            })
            .collect::<Vec<_>>()
            .join("&");
        let up = running(backend).await;
        let asking = MEASURING.iter().filter(|(plugin, _)| up.iter().any(|id| id == plugin)).map(
            |(plugin, label)| {
                let asked = asked.clone();
                async move {
                    let answer = tokio::time::timeout(
                        ASKING,
                        backend.ask(plugin, "GET", "readiness", Some(&asked), None),
                    )
                    .await;
                    (*label, answer)
                }
            },
        );
        for (label, answer) in futures::future::join_all(asking).await {
            let Ok(Ok((200, body))) = answer else { continue };
            for name in &names {
                let said = &body["services"][name];
                let (order, word, badge) = rank(said["state"].as_str().unwrap_or_default());
                let summary = said["summary"].as_str().unwrap_or_default();
                let sentence = format!("{label}: {summary}");
                let held = worst.get(name).map_or(u8::MAX, |held| held.0);
                if order < held {
                    worst.insert(name.clone(), (order, word, badge, sentence));
                }
            }
        }
    }
    let mut rows: Vec<(u8, Row)> = found
        .into_iter()
        .map(|(name, (title, teams))| {
            let (order, word, badge, said) =
                worst.remove(&name).unwrap_or((3, "Not known", "unknown", String::new()));
            let row = Row {
                href: href(Kind::Service, &name),
                title: if title.is_empty() { name } else { title },
                teams: teams.join(", "),
                word,
                badge,
                said,
            };
            (order, row)
        })
        .collect();
    rows.sort_by_key(|(order, row)| (*order, row.title.to_lowercase()));
    let more = rows.len().saturating_sub(LISTED);
    let rows = rows.into_iter().take(LISTED).map(|(_, row)| row).collect();
    render(&ServicesFragment { rows, more, teams: teams.len() })
}
