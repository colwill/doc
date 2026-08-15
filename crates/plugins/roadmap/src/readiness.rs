//! Whether the services in a release are ready, asked of the plugins that measure them: each
//! answers `GET api/readiness?service=…&until=<day>` (DOC-SPEC §9.2) with `ready`, `warning`,
//! `blocked` or `unknown` and a sentence for each service. They are asked as whoever is looking,
//! so nobody learns through the roadmap what they could not see on the plugin's own page, and only
//! while they are running, since a call to a plugin with no process waits out its deadline.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use chrono::NaiveDate;
use doc_plugin_sdk::{Backend, Query};
use futures::StreamExt;
use serde::Serialize;
use serde_json::Value;

use crate::scope::encoded;
use crate::settings::Definitions;

/// How long a plugin is given to answer.
const ASKING: Duration = Duration::from_secs(5);
/// Questions asked at once.
const AT_ONCE: usize = 8;

/// Worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Blocked,
    Warning,
    Ready,
    Unknown,
}

impl State {
    fn parse(text: Option<&str>) -> Self {
        match text {
            Some("blocked") => Self::Blocked,
            Some("warning") => Self::Warning,
            Some("ready") => Self::Ready,
            _ => Self::Unknown,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Blocked => "Blocked",
            Self::Warning => "Warning",
            Self::Ready => "Ready",
            Self::Unknown => "Not known",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            Self::Blocked => "error",
            Self::Warning => "degraded",
            Self::Ready => "ready",
            Self::Unknown => "unknown",
        }
    }
}

/// What one plugin said of one service.
#[derive(Debug, Clone, Serialize)]
pub struct Signal {
    pub state: State,
    pub summary: String,
    pub href: Option<String>,
}

/// What one plugin said, for one release date.
#[derive(Debug, Clone)]
struct Answer {
    title: String,
    /// It answered with faux data.
    faux: bool,
    problem: Option<String>,
    services: BTreeMap<String, Signal>,
}

/// One plugin's word on one release: the worst of its services, and each service's.
#[derive(Debug, Clone, Serialize)]
pub struct Column {
    pub plugin: String,
    pub title: String,
    pub state: State,
    /// It answered with faux data, which says so beside the answer.
    pub faux: bool,
    pub problem: Option<String>,
    /// Each service's title and what the plugin said of it.
    pub services: Vec<(String, Signal)>,
}

impl Column {
    /// The worst of what it said, in a line, for a table cell's tooltip.
    pub fn said(&self) -> String {
        if let Some(problem) = &self.problem {
            return problem.clone();
        }
        let worst: Vec<String> = self
            .services
            .iter()
            .filter(|(_, signal)| signal.state == self.state)
            .map(|(title, signal)| format!("{title}: {}", signal.summary))
            .collect();
        worst.join(". ")
    }
}

/// A question: the release date asked about, and the plugin asked.
type Asked = (Option<NaiveDate>, String);

/// Everything the plugins said for a page, by release date.
#[derive(Debug, Default)]
pub struct Readiness {
    plugins: Vec<String>,
    asked: BTreeMap<Asked, Answer>,
}

/// A link to a page in DOC, only ever: what another plugin says is not trusted to be a link
/// anywhere else.
fn inside(href: Option<&str>) -> Option<String> {
    href.filter(|href| href.starts_with("/p/") && !href.starts_with("//")).map(str::to_string)
}

fn answered(status: u16, body: &Value, plugin: &str) -> Option<Answer> {
    let refused = |problem: String| Answer {
        title: plugin.to_string(),
        faux: false,
        problem: Some(problem),
        services: BTreeMap::new(),
    };
    match status {
        200 => {}
        // It offers no readiness: it is left out rather than shown as unknown.
        404 => return None,
        401 | 403 => {
            return Some(refused(format!("You cannot see {plugin}, so it was not asked")));
        }
        status => {
            let said = body["detail"].as_str().unwrap_or("no reason given");
            return Some(refused(format!("{plugin} answered {status}: {said}")));
        }
    }
    let services = body["services"]
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, signal)| {
            let signal = Signal {
                state: State::parse(signal["state"].as_str()),
                summary: signal["summary"].as_str().unwrap_or_default().chars().take(500).collect(),
                href: inside(signal["href"].as_str()),
            };
            (name.clone(), signal)
        })
        .collect();
    Some(Answer {
        title: body["title"]
            .as_str()
            .filter(|title| !title.is_empty())
            .unwrap_or(plugin)
            .to_string(),
        faux: body["faux"].is_object(),
        problem: None,
        services,
    })
}

/// The plugins named in the settings that are running now.
async fn running(backend: &Backend, definitions: &Definitions) -> Vec<String> {
    let running: BTreeSet<String> = backend
        .query_all::<Value>(Query::new("core.plugins").fields(&["id", "state"]))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|plugin| matches!(plugin["state"].as_str(), Some("running" | "cancelled")))
        .filter_map(|plugin| plugin["id"].as_str().map(str::to_string))
        .collect();
    definitions.readiness.iter().filter(|id| running.contains(*id)).cloned().collect()
}

/// Asks each plugin about the services due on each day: one question per plugin and day.
pub async fn ask(
    backend: &Backend,
    definitions: &Definitions,
    wanted: BTreeMap<Option<NaiveDate>, BTreeSet<String>>,
) -> Readiness {
    let plugins = running(backend, definitions).await;
    let mut questions = Vec::new();
    for (until, services) in wanted.iter().filter(|(_, services)| !services.is_empty()) {
        for plugin in &plugins {
            let mut pairs: Vec<(&str, String)> =
                services.iter().map(|service| ("service", service.clone())).collect();
            if let Some(until) = until {
                pairs.push(("until", until.to_string()));
            }
            let pairs: Vec<(&str, &str)> =
                pairs.iter().map(|(key, value)| (*key, value.as_str())).collect();
            questions.push((*until, plugin.clone(), encoded(&pairs)));
        }
    }
    let answers: Vec<(Asked, Option<Answer>)> = futures::stream::iter(questions)
        .map(|(until, plugin, query)| async move {
            let asked = backend.ask(&plugin, "GET", "readiness", Some(&query), None);
            let answer = match tokio::time::timeout(ASKING, asked).await {
                Ok(Ok((status, body))) => answered(status, &body, &plugin),
                Ok(Err(err)) => Some(Answer {
                    title: plugin.clone(),
                    faux: false,
                    problem: Some(format!("{plugin} could not be asked: {err}")),
                    services: BTreeMap::new(),
                }),
                Err(_) => Some(Answer {
                    title: plugin.clone(),
                    faux: false,
                    problem: Some(format!("{plugin} did not answer in time")),
                    services: BTreeMap::new(),
                }),
            };
            ((until, plugin), answer)
        })
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    let asked: BTreeMap<Asked, Answer> =
        answers.into_iter().filter_map(|(key, answer)| Some((key, answer?))).collect();
    let plugins = plugins
        .into_iter()
        .filter(|plugin| asked.keys().any(|(_, asked)| asked == plugin))
        .collect();
    Readiness { plugins, asked }
}

impl Readiness {
    /// What each plugin said of these services for a release due on `until`, in the settings'
    /// order. A plugin's state is the worst of its services', leaving out those it could not judge
    /// unless it could judge none.
    pub fn columns(&self, until: Option<NaiveDate>, services: &[(String, String)]) -> Vec<Column> {
        self.plugins
            .iter()
            .filter_map(|plugin| {
                let answer = self.asked.get(&(until, plugin.clone()))?;
                let said: Vec<(String, Signal)> = services
                    .iter()
                    .filter_map(|(name, title)| {
                        Some((title.clone(), answer.services.get(name)?.clone()))
                    })
                    .collect();
                let state = match &answer.problem {
                    Some(_) => State::Unknown,
                    None => said
                        .iter()
                        .map(|(_, signal)| signal.state)
                        .filter(|state| *state != State::Unknown)
                        .min()
                        .unwrap_or(State::Unknown),
                };
                Some(Column {
                    plugin: plugin.clone(),
                    title: answer.title.clone(),
                    state,
                    faux: answer.faux,
                    problem: answer.problem.clone(),
                    services: said,
                })
            })
            .collect()
    }

    /// The plugins asked, by their titles and whether they answered with faux data, for a
    /// table's headings.
    pub fn titles(&self) -> Vec<(String, bool)> {
        self.plugins
            .iter()
            .map(|plugin| {
                self.asked.iter().find(|((_, asked), _)| asked == plugin).map_or_else(
                    || (plugin.clone(), false),
                    |(_, answer)| (answer.title.clone(), answer.faux),
                )
            })
            .collect()
    }

    /// Why any plugin could not be asked, once each.
    pub fn problems(&self) -> Vec<String> {
        let mut problems: Vec<String> =
            self.asked.values().filter_map(|answer| answer.problem.clone()).collect();
        problems.sort();
        problems.dedup();
        problems
    }
}

/// The worst any plugin said of a release, where any could judge it.
pub fn worst(columns: &[Column]) -> Option<State> {
    columns.iter().map(|column| column.state).filter(|state| *state != State::Unknown).min()
}
