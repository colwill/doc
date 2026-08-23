//! What the LLM is told: the run's brief, where each source's data goes in DOC, and how to hand it
//! in. One document serves both ways a run is done. Claude, driven by DOC, is told its tools and
//! the sources it can reach; an administrator's own agent is told the HTTP calls and its token.

use askama::Template;
use chrono::{DateTime, Utc};

use crate::settings::{Auth, Config};
use crate::store::{Run, SOURCES};

#[derive(Template)]
#[template(path = "instructions.md", escape = "none")]
pub struct Instructions {
    pub title: String,
    pub id: String,
    pub brief: String,
    pub source_names: String,
    pub source_list: String,
    pub has_confluence: bool,
    pub has_backstage: bool,
    pub has_jira: bool,
    pub has_markdown: bool,
    pub has_mkdocs: bool,
    pub claude: bool,
    /// For Claude: each source it can reach, where, and a hint of its API.
    pub connections: Vec<(String, String, String)>,
    pub hosts: Vec<String>,
    pub api: String,
    pub token: String,
    pub expires: String,
}

/// What stands in for the token wherever it is not being shown for the first and only time.
pub const HIDDEN: &str = "<the token shown when the run was made>";

fn hint(name: &str) -> &'static str {
    match name {
        "confluence" => {
            "its REST API, such as `rest/api/space`, then `rest/api/content?spaceKey=ENG&type=page\
             &expand=body.storage,ancestors&limit=25&start=0`, a page at a time"
        }
        "jira" => "its REST API, such as `rest/api/3/project/search?maxResults=50`",
        "backstage" => {
            "its catalog, such as `api/catalog/entities/by-query?filter=kind=component&limit=100`"
        }
        _ => "",
    }
}

impl Instructions {
    pub fn new(
        run: &Run,
        config: &Config,
        token: Option<&str>,
        expires: Option<DateTime<Utc>>,
    ) -> Self {
        let has = |name: &str| run.sources.iter().any(|source| source == name);
        let shown: Vec<&str> =
            SOURCES.iter().filter(|(name, _)| has(name)).map(|(_, label)| *label).collect();
        let source_names = match shown.as_slice() {
            [] => "other tools".to_string(),
            [only] => (*only).to_string(),
            [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        };
        let connections = config
            .connections
            .iter()
            .filter(|connection| has(connection.name))
            .map(|connection| {
                let signed = match connection.auth {
                    Auth::None => " (without signing in)",
                    _ => "",
                };
                (
                    format!("{}{signed}", connection.name),
                    connection.base.to_string(),
                    hint(connection.name).to_string(),
                )
            })
            .collect();
        Self {
            title: run.title.clone(),
            id: run.id.to_string(),
            brief: match run.brief.trim() {
                "" => "Everything the sources hold that a developer platform should.".to_string(),
                brief => brief.to_string(),
            },
            source_names,
            source_list: run
                .sources
                .iter()
                .map(|source| format!("`{source}`"))
                .collect::<Vec<_>>()
                .join(", "),
            has_confluence: has("confluence"),
            has_backstage: has("backstage"),
            has_jira: has("jira"),
            has_markdown: has("markdown"),
            has_mkdocs: has("mkdocs"),
            claude: run.mode == crate::store::CLAUDE,
            connections,
            hosts: config.hosts.clone(),
            api: config.api_url.clone(),
            token: token.unwrap_or(HIDDEN).to_string(),
            expires: expires.map_or_else(
                || "it is revoked".to_string(),
                |at| at.format("%-d %b %Y %H:%M UTC").to_string(),
            ),
        }
    }

    pub fn text(&self) -> String {
        self.render().unwrap_or_else(|err| format!("The instructions could not be written: {err}"))
    }
}
