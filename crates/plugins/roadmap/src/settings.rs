//! What the delivery roadmap is drawn from and judged by: the Jira projects it tracks and the
//! services each is for, the plugins releases are read from, the plugins asked whether each
//! service in a release is ready, and when a release is behind.

use std::collections::BTreeMap;

use doc_plugin_sdk::{Feature, Setting, SettingKind, Settings};
use serde_json::json;

pub const ROADMAP: &str = "roadmap";

pub const PROJECTS: &str = "projects";
/// Where the Settings page asks what projects and services can be chosen.
pub const PROJECT_CHOICES: &str = "settings/projects";
pub const SOURCES: &str = "sources";
pub const READINESS: &str = "readiness-from";
pub const BEHIND: &str = "behind-by";
pub const AHEAD: &str = "ahead";
pub const BACK: &str = "released-within";

const DAY: f64 = 86_400.0;

pub fn declared() -> Vec<Setting> {
    vec![
        Setting::new(PROJECTS, "Projects tracked", SettingKind::Map)
            .choices_from(PROJECT_CHOICES)
            .hinted(
                "The Jira projects the roadmap shows, from those jira and jira-dc read, each with \
                 the services its releases are for, so they are asked whether they are ready and \
                 the releases show on their pages. A project a service is given from its Roadmap \
                 section in the Catalogue is tracked as well. With none chosen here, every \
                 project read is shown.",
            )
            .grouped("Projects"),
        Setting::new(SOURCES, "Where releases come from", SettingKind::List)
            .defaulting(json!(["jira", "jira-dc"]))
            .hinted(
                "Plugins that export versions and issues to roadmap, such as jira and jira-dc. \
                 Any that is not running is passed over.",
            )
            .grouped("Sources"),
        Setting::new(READINESS, "Ask whether services are ready", SettingKind::List)
            .defaulting(json!(["cicd", "reliability", "eol", "dora"]))
            .hinted(
                "Plugins asked about each service in a release, in this order: whether its \
                 pipelines pass, whether it keeps its objectives, whether what it runs is still \
                 supported on the release date, and how its deployments go. Any plugin that \
                 answers api/readiness can be named. Each is asked as whoever is looking.",
            )
            .grouped("Readiness"),
        Setting::new(BEHIND, "At risk when behind by, percent", SettingKind::Number)
            .defaulting(json!(15.0))
            .between(0.0, 100.0)
            .hinted(
                "A release with a start and a release date is at risk when the share of its \
                 issues done trails the share of its time gone by more than this.",
            )
            .grouped("Judging"),
        Setting::new(AHEAD, "Show releases due within", SettingKind::Duration)
            .defaulting(json!(365.0 * DAY))
            .hinted("Unreleased releases due further ahead are left off the roadmap.")
            .grouped("Showing"),
        Setting::new(BACK, "Show releases shipped within", SettingKind::Duration)
            .defaulting(json!(90.0 * DAY))
            .grouped("Showing"),
    ]
}

pub fn features() -> Vec<Feature> {
    vec![
        Feature::new(
            ROADMAP,
            "Delivery roadmap",
            "Every release planned in Jira for each service, team and organisation: when it is \
             due, how far along it is, and whether the services in it are ready — their \
             pipelines, reliability, end of life and delivery, from the plugins that measure them.",
        )
        .needs(&["jira", "jira-dc"], "release-data"),
    ]
}

#[derive(Debug, Clone)]
pub struct Definitions {
    /// Each project chosen on the Settings page, its key upper case, with the services it is for.
    pub tracked: BTreeMap<String, Vec<String>>,
    pub sources: Vec<String>,
    pub readiness: Vec<String>,
    pub behind: f64,
    pub ahead_days: i64,
    pub back_days: i64,
}

fn days(settings: &Settings, key: &str, default: i64) -> i64 {
    settings
        .duration(key)
        .map_or(default, |span| i64::try_from(span.as_secs() / 86_400).unwrap_or(default))
}

fn ids(settings: &Settings, key: &str) -> Vec<String> {
    let mut seen = Vec::new();
    for id in settings.list(key) {
        let id = id.trim().to_ascii_lowercase();
        if !id.is_empty() && !seen.contains(&id) {
            seen.push(id);
        }
    }
    seen
}

impl Definitions {
    pub fn read(settings: &Settings) -> Self {
        let mut tracked: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (project, services) in settings.map(PROJECTS) {
            let project = project.trim().to_ascii_uppercase();
            if project.is_empty() {
                continue;
            }
            let held = tracked.entry(project).or_default();
            for service in services.into_iter().map(|service| service.trim().to_string()) {
                if !service.is_empty() && !held.contains(&service) {
                    held.push(service);
                }
            }
        }
        Self {
            tracked,
            sources: ids(settings, SOURCES),
            readiness: ids(settings, READINESS),
            behind: settings.number(BEHIND).unwrap_or(15.0).clamp(0.0, 100.0),
            ahead_days: days(settings, AHEAD, 365),
            back_days: days(settings, BACK, 90),
        }
    }
}
