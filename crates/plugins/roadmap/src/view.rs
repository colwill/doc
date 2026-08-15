//! A scope's releases and what the other plugins say of the services in them: what every page,
//! the API and agents start from.

use std::collections::{BTreeMap, BTreeSet};

use chrono::NaiveDate;
use doc_plugin_sdk::{Backend, Query};
use serde_json::json;

use crate::plan::{self, Held, Issue, Project, Release, Version};
use crate::readiness::{self, Column, Readiness};
use crate::scope::{self, Member, Scope};
use crate::settings::Definitions;
use crate::{Refusal, faux};

/// Far enough to take in any release, and near enough that no date overflows.
const ANY_DAYS: i64 = 100_000;

pub struct View {
    pub releases: Vec<Release>,
    pub readiness: Readiness,
    /// The scope's services.
    pub members: Vec<Member>,
    /// Projects with releases that no service in the Catalogue names.
    pub unnamed: Vec<String>,
}

impl View {
    pub fn columns(&self, release: &Release) -> Vec<Column> {
        match release.version.released {
            true => Vec::new(),
            false => self.readiness.columns(release.due(), &release.services),
        }
    }
}

/// Readiness is asked only of what has not shipped, once per plugin and release date.
async fn asked(backend: &Backend, definitions: &Definitions, releases: &[Release]) -> Readiness {
    let mut wanted: BTreeMap<Option<NaiveDate>, BTreeSet<String>> = BTreeMap::new();
    for release in releases.iter().filter(|release| !release.version.released) {
        wanted.entry(release.due()).or_default().extend(release.names());
    }
    readiness::ask(backend, definitions, wanted).await
}

pub async fn read(backend: &Backend, scope: &Scope, ask: bool) -> Result<View, Refusal> {
    let definitions = Definitions::read(&backend.settings());
    let (members, every) = scope::members(backend, scope).await?;
    let planned = |members: &[Member]| -> BTreeSet<String> {
        members
            .iter()
            .flat_map(|member| member.projects.iter().chain(member.tracked.iter()).cloned())
            .collect()
    };
    let (within, projects) = match scope {
        // Everything read, until the Settings page chooses projects: then those, and any a
        // service names in the Catalogue.
        Scope::All if definitions.tracked.is_empty() || faux::on(backend) => (None, None),
        Scope::All => {
            let mut projects = planned(&every);
            projects.extend(definitions.tracked.keys().cloned());
            (None, Some(projects))
        }
        _ => (
            Some(members.iter().map(|member| member.name.clone()).collect::<BTreeSet<_>>()),
            Some(planned(&members)),
        ),
    };
    let held = plan::held(backend, &definitions, projects.as_ref(), &every).await?;
    let releases = plan::releases(held, &every, within.as_ref(), &definitions, false);
    let named = planned(&every);
    let unnamed: BTreeSet<String> = releases
        .iter()
        .map(|release| release.version.project.clone())
        .filter(|project| !named.contains(project))
        .collect();
    let readiness = match ask {
        true => asked(backend, &definitions, &releases).await,
        false => Readiness::default(),
    };
    Ok(View { releases, readiness, members, unnamed: unnamed.into_iter().collect() })
}

/// One release, with its issues and what the plugins say of its services.
pub async fn one(backend: &Backend, key: &str) -> Result<(Release, Vec<Column>), Refusal> {
    let definitions = Definitions::read(&backend.settings());
    let every = scope::every(backend).await?;
    let (source, id) = key
        .split_once(':')
        .ok_or_else(|| Refusal::bad("a release is named <source>:<id>, as its link has it"))?;
    let held = match faux::on(backend) {
        true => {
            let mut held = faux::held(backend, &every).await?;
            held.versions.retain(|(_, version)| version.id == id);
            held.issues.retain(|(_, issue)| issue.versions.iter().any(|version| version == id));
            held
        }
        false => {
            if !definitions.sources.iter().any(|known| known == source) {
                return Err(Refusal::missing(format!("releases are not read from {source}")));
            }
            let version: Option<Version> = backend.get(&format!("{source}.versions"), id).await?;
            let version =
                version.ok_or_else(|| Refusal::missing(format!("there is no release {key}")))?;
            let issues: Vec<Issue> = backend
                .query_all(
                    Query::new(&format!("{source}.issues"))
                        .filter(json!({ "versions": { "contains": id } })),
                )
                .await?;
            let project: Option<Project> =
                backend.get(&format!("{source}.projects"), version.project.clone()).await?;
            let mut held = Held::default();
            if let Some(project) = project {
                held.projects.insert((source.to_string(), project.key.clone()), project);
            }
            held.issues = issues.into_iter().map(|issue| (source.to_string(), issue)).collect();
            held.versions.push((source.to_string(), version));
            held
        }
    };
    let mut everything = definitions.clone();
    // A release asked for by its link is shown however far off it is.
    everything.ahead_days = ANY_DAYS;
    everything.back_days = ANY_DAYS;
    let release = plan::releases(held, &every, None, &everything, true)
        .into_iter()
        .next()
        .ok_or_else(|| Refusal::missing(format!("there is no release {key}")))?;
    let columns = match release.version.released {
        true => Vec::new(),
        false => {
            let readiness = asked(backend, &definitions, std::slice::from_ref(&release)).await;
            readiness.columns(release.due(), &release.services)
        }
    };
    Ok((release, columns))
}
