//! Releases: Jira's versions, as the source plugins export them, with the issues in them and the
//! services they are for, and where each stands — released, on track, at risk, overdue, planned
//! or not yet scheduled — from its dates and how much of it is done.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, NaiveDate, Utc};
use doc_plugin_sdk::{Backend, PluginError, Query};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::scope::{Member, encoded};
use crate::settings::Definitions;
use crate::{Refusal, faux};

/// Projects asked about in one query.
const IN_ONE: usize = 200;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub project: String,
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub released: bool,
    #[serde(default)]
    pub start_date: Option<NaiveDate>,
    #[serde(default)]
    pub release_date: Option<NaiveDate>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub id: String,
    pub key: String,
    pub project: String,
    #[serde(default)]
    pub summary: Option<String>,
    #[serde(default, rename = "type")]
    pub kind: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub category: String,
    #[serde(default)]
    pub versions: Vec<String>,
    #[serde(default)]
    pub components: Vec<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub updated: Option<DateTime<Utc>>,
    #[serde(default)]
    pub resolved_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub key: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
}

/// What the sources hold for some projects: each record with the source it came from.
#[derive(Debug, Default)]
pub struct Held {
    pub versions: Vec<(String, Version)>,
    pub issues: Vec<(String, Issue)>,
    pub projects: BTreeMap<(String, String), Project>,
}

/// A source's collection, or nothing where the source is not installed or exports nothing here.
pub async fn exported<T: serde::de::DeserializeOwned>(
    backend: &Backend,
    query: Query,
) -> Result<Vec<T>, PluginError> {
    match backend.query_all(query).await {
        Err(err) if matches!(err.problem(), Some((403 | 404, _))) => Ok(Vec::new()),
        answered => answered,
    }
}

/// The releases of these projects, or of every project; with faux data, `faux-data`'s for the
/// services instead, never read.
pub async fn held(
    backend: &Backend,
    definitions: &Definitions,
    projects: Option<&BTreeSet<String>>,
    every: &[Member],
) -> Result<Held, Refusal> {
    if faux::on(backend) {
        return faux::held(backend, every).await;
    }
    let mut held = Held::default();
    for source in &definitions.sources {
        let versions = format!("{source}.versions");
        let issues = format!("{source}.issues");
        let filters: Vec<Value> = match projects {
            None => vec![json!({})],
            Some(projects) => {
                let keys: Vec<&String> = projects.iter().collect();
                keys.chunks(IN_ONE).map(|chunk| json!({ "project": { "in": chunk } })).collect()
            }
        };
        for filter in filters {
            for version in
                exported::<Version>(backend, Query::new(&versions).filter(filter.clone())).await?
            {
                held.versions.push((source.clone(), version));
            }
            let asked = Query::new(&issues).filter(filter).fields(&[
                "id",
                "key",
                "project",
                "summary",
                "type",
                "status",
                "category",
                "versions",
                "components",
                "labels",
                "updated",
                "resolved_at",
                "url",
            ]);
            for issue in exported::<Issue>(backend, asked).await? {
                held.issues.push((source.clone(), issue));
            }
        }
        for project in
            exported::<Project>(backend, Query::new(&format!("{source}.projects"))).await?
        {
            held.projects.insert((source.clone(), project.key.clone()), project);
        }
    }
    Ok(held)
}

/// Where a release stands, worst first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Overdue,
    AtRisk,
    OnTrack,
    Planned,
    Unscheduled,
    Released,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Self::Overdue => "Overdue",
            Self::AtRisk => "At risk",
            Self::OnTrack => "On track",
            Self::Planned => "Planned",
            Self::Unscheduled => "No date",
            Self::Released => "Released",
        }
    }

    /// The badge it is shown with: its meaning, since colour alone says nothing.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Overdue => "error",
            Self::AtRisk => "degraded",
            Self::OnTrack | Self::Released => "ready",
            Self::Planned => "loading",
            Self::Unscheduled => "unknown",
        }
    }

    /// The colour of its bar on the timeline.
    pub fn tone(self) -> &'static str {
        match self {
            Self::Overdue => "error",
            Self::AtRisk => "degraded",
            Self::OnTrack => "ready",
            Self::Planned => "security",
            Self::Unscheduled => "unknown",
            Self::Released => "done",
        }
    }
}

/// How many of a release's issues are to do, in progress and done.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Progress {
    pub to_do: usize,
    pub in_progress: usize,
    pub done: usize,
    pub total: usize,
}

impl Progress {
    fn of<'a>(issues: impl Iterator<Item = &'a Issue>) -> Self {
        let mut progress = Self::default();
        for issue in issues {
            progress.total += 1;
            match issue.category.as_str() {
                "done" => progress.done += 1,
                "indeterminate" => progress.in_progress += 1,
                _ => progress.to_do += 1,
            }
        }
        progress
    }

    /// The share done, 0 to 1.
    pub fn share(&self) -> Option<f64> {
        (self.total > 0).then(|| self.done as f64 / self.total as f64)
    }

    pub fn said(&self) -> String {
        match self.total {
            0 => "No issues yet".to_string(),
            total => format!("{} of {total} done", self.done),
        }
    }
}

/// A release, the services it is for, and where it stands.
#[derive(Debug, Clone)]
pub struct Release {
    /// `<source>:<id>`, which names it in DOC.
    pub key: String,
    pub version: Version,
    pub project: String,
    pub project_url: Option<String>,
    /// Each service's name and title.
    pub services: Vec<(String, String)>,
    pub progress: Progress,
    pub status: Status,
    pub why: String,
    pub issues: Vec<Issue>,
}

pub fn day(date: NaiveDate) -> String {
    date.format("%-d %b %Y").to_string()
}

fn in_days(days: i64) -> String {
    match days {
        0 => "today".to_string(),
        1 => "tomorrow".to_string(),
        -1 => "yesterday".to_string(),
        days if days > 0 => format!("in {days} days"),
        days => format!("{} days ago", -days),
    }
}

impl Release {
    /// What it is called on a page: its project and its name, `PAY 2.4`.
    pub fn title(&self) -> String {
        format!("{} {}", self.version.project, self.version.name)
    }

    pub fn href(&self) -> String {
        format!("/p/roadmap/release?{}", encoded(&[("id", &self.key)]))
    }

    pub fn due(&self) -> Option<NaiveDate> {
        self.version.release_date
    }

    pub fn due_said(&self) -> String {
        self.due().map_or_else(|| "No date".to_string(), day)
    }

    pub fn names(&self) -> BTreeSet<String> {
        self.services.iter().map(|(name, _)| name.clone()).collect()
    }

    pub fn json(&self, with_issues: bool) -> Value {
        let mut value = json!({
            "id": self.key,
            "title": self.title(),
            "project": self.version.project,
            "project_name": self.project,
            "name": self.version.name,
            "description": self.version.description,
            "released": self.version.released,
            "start_date": self.version.start_date,
            "release_date": self.version.release_date,
            "status": self.status,
            "said": self.why,
            "progress": self.progress,
            "services": self.services.iter().map(|(name, _)| name).collect::<Vec<_>>(),
            "url": self.version.url,
        });
        if with_issues {
            value["issues"] = json!(self.issues);
        }
        value
    }
}

/// Where a release stands today, and why, in a sentence.
fn judged(
    version: &Version,
    progress: &Progress,
    today: NaiveDate,
    behind: f64,
) -> (Status, String) {
    let done = match progress.total {
        0 => "no issues in it yet".to_string(),
        _ => progress.said(),
    };
    if version.released {
        return match version.release_date {
            Some(date) => (Status::Released, format!("Released on {}; {done}", day(date))),
            None => (Status::Released, format!("Released; {done}")),
        };
    }
    let Some(due) = version.release_date else {
        return (Status::Unscheduled, format!("No release date yet; {done}"));
    };
    let days = (due - today).num_days();
    if days < 0 {
        return (
            Status::Overdue,
            format!("Due {} ({}) and not released; {done}", day(due), in_days(days)),
        );
    }
    if let Some(start) = version.start_date
        && start > today
    {
        return (Status::Planned, format!("Starts {}, due {}", day(start), day(due)));
    }
    if version.start_date.is_none() && progress.done + progress.in_progress == 0 {
        return (
            Status::Planned,
            format!("Due {} ({}); nothing started yet", day(due), in_days(days)),
        );
    }
    if let (Some(start), Some(share)) = (version.start_date, progress.share())
        && due > start
    {
        let gone = (today - start).num_days() as f64 / (due - start).num_days() as f64;
        if (gone - share) * 100.0 > behind {
            return (
                Status::AtRisk,
                format!(
                    "{:.0}% done with {:.0}% of its time gone; due {} ({})",
                    share * 100.0,
                    gone * 100.0,
                    day(due),
                    in_days(days)
                ),
            );
        }
    }
    (Status::OnTrack, format!("Due {} ({}); {done}", day(due), in_days(days)))
}

/// Which releases to show: unreleased ones due within `ahead_days` or with no date, and those
/// released in the last `back_days`.
fn shown(version: &Version, today: NaiveDate, definitions: &Definitions) -> bool {
    match (version.released, version.release_date) {
        (true, Some(date)) => date >= today - Duration::days(definitions.back_days),
        (true, None) => false,
        (false, Some(date)) => date <= today + Duration::days(definitions.ahead_days),
        (false, None) => true,
    }
}

/// Every release of what is held, for the services it is for; with `within`, only those that are
/// for one of these services. Issues are kept with each release where `with_issues` says.
pub fn releases(
    held: Held,
    every: &[Member],
    within: Option<&BTreeSet<String>>,
    definitions: &Definitions,
    with_issues: bool,
) -> Vec<Release> {
    let today = Utc::now().date_naive();
    let mut by_version: BTreeMap<(String, String), Vec<Issue>> = BTreeMap::new();
    for (source, issue) in held.issues {
        for version in &issue.versions {
            by_version.entry((source.clone(), version.clone())).or_default().push(issue.clone());
        }
    }
    let mut found = Vec::new();
    for (source, version) in held.versions {
        if !shown(&version, today, definitions) {
            continue;
        }
        let issues = by_version.remove(&(source.clone(), version.id.clone())).unwrap_or_default();
        let components: BTreeSet<String> =
            issues.iter().flat_map(|issue| issue.components.iter().cloned()).collect();
        let labels: BTreeSet<String> =
            issues.iter().flat_map(|issue| issue.labels.iter().cloned()).collect();
        let services: Vec<(String, String)> = every
            .iter()
            .filter(|member| member.owns(&version.project, &components, &labels))
            .map(|member| (member.name.clone(), member.title.clone()))
            .collect();
        if let Some(within) = within
            && !services.iter().any(|(name, _)| within.contains(name))
        {
            continue;
        }
        let progress = Progress::of(issues.iter());
        let (status, why) = judged(&version, &progress, today, definitions.behind);
        let project = held.projects.get(&(source.clone(), version.project.clone()));
        found.push(Release {
            key: format!("{source}:{}", version.id),
            project: project
                .map_or_else(|| version.project.clone(), |project| project.name.clone()),
            project_url: project.and_then(|project| project.url.clone()),
            services,
            progress,
            status,
            why,
            issues: if with_issues { issues } else { Vec::new() },
            version,
        });
    }
    found.sort_by(|one, two| {
        (one.version.released, one.due().is_none(), one.due(), one.title()).cmp(&(
            two.version.released,
            two.due().is_none(),
            two.due(),
            two.title(),
        ))
    });
    found
}
