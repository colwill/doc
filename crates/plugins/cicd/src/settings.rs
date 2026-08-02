//! What CI/CD/CT metrics are configured with: where workflow runs come from, which workflows are
//! continuous integration, delivery and testing, which branches count, and the bands. Teams name
//! their workflows in different ways, so the stages are settings, and the defaults are the common
//! names.

use doc_plugin_sdk::{Feature, Setting, SettingKind, Settings};
use serde::Serialize;
use serde_json::json;

pub const METRICS: &str = "metrics";

pub const SOURCES: &str = "sources";
pub const CD_WORKFLOWS: &str = "cd-workflows";
pub const CI_WORKFLOWS: &str = "ci-workflows";
pub const CT_WORKFLOWS: &str = "ct-workflows";
pub const BRANCHES: &str = "branches";
pub const KEEP_FOR: &str = "keep-for";
pub const SCHEDULE: &str = "recompute-schedule";
pub const RECOMPUTE: &str = "50 2 * * *";

const DAY: f64 = 86_400.0;
const CD: [&str; 6] = ["deploy", "release", "publish", "delivery", "rollout", "cd"];
const CI: [&str; 6] = ["ci", "build", "lint", "check", "compile", "verify"];
const CT: [&str; 11] = [
    "test",
    "tests",
    "testing",
    "e2e",
    "integration",
    "acceptance",
    "regression",
    "smoke",
    "qa",
    "performance",
    "load",
];

pub fn declared() -> Vec<Setting> {
    let band = |key: &str, label: &str, kind: SettingKind, default: f64| {
        Setting::new(key, label, kind).defaulting(json!(default)).grouped("Performance bands")
    };
    let stage = |key: &str, label: &str, default: &[&str], hint: &str| {
        Setting::new(key, label, SettingKind::List)
            .defaulting(json!(default))
            .hinted(hint)
            .grouped("Stages")
    };
    vec![
        Setting::new(SOURCES, "Where workflow runs come from", SettingKind::List)
            .hinted(
                "Plugins that export workflow-runs to cicd, such as github and ghe, and \
                 kubernetes, whose rollouts are delivery runs named rollout <cluster>/<namespace>/\
                 <deployment>. Any that is not running is passed over.",
            )
            .defaulting(json!(["github", "ghe", "kubernetes"]))
            .grouped("Sources"),
        stage(
            CD_WORKFLOWS,
            "Continuous delivery",
            &CD,
            "A workflow whose name or file has one of these words is delivery: it deploys or \
             releases. Checked first. owner/name:deploy.yml names one workflow of one repository.",
        ),
        stage(
            CI_WORKFLOWS,
            "Continuous integration",
            &CI,
            "Words for workflows that build and check each change. Checked next, so Build and \
             test is integration.",
        ),
        stage(
            CT_WORKFLOWS,
            "Continuous testing",
            &CT,
            "Words for workflows that test: end-to-end, integration, regression and the like. \
             A workflow that none of the three names is integration.",
        ),
        Setting::new(BRANCHES, "Runs that count", SettingKind::Choice)
            .one_of(&["default", "all"])
            .defaulting(json!("default"))
            .hinted(
                "default: runs on each repository's default branch, where a failure means the \
                 shared code is broken. all: pull requests and other branches too, where failing \
                 is often a pipeline doing its job. Time to recover is always the default branch.",
            )
            .grouped("Definitions"),
        band("success-elite", "Elite: success rate, percent, at least", SettingKind::Number, 90.0)
            .hinted(
                "Elite follows the benchmarks CircleCI's State of Software Delivery reports \
                 publish: 90% on the default branch, 10 minutes, an hour to recover. The other \
                 bands are steps down from it.",
            ),
        band("success-high", "High: success rate, percent, at least", SettingKind::Number, 80.0),
        band(
            "success-medium",
            "Medium: success rate, percent, at least",
            SettingKind::Number,
            60.0,
        ),
        band("duration-elite", "Elite: duration, at most", SettingKind::Duration, 600.0),
        band("duration-high", "High: duration, at most", SettingKind::Duration, 1_200.0),
        band("duration-medium", "Medium: duration, at most", SettingKind::Duration, 3_600.0),
        band("recovery-elite", "Elite: time to recover, at most", SettingKind::Duration, 3_600.0),
        band("recovery-high", "High: time to recover, at most", SettingKind::Duration, 14_400.0),
        band("recovery-medium", "Medium: time to recover, at most", SettingKind::Duration, DAY),
        band(
            "reruns-elite",
            "Elite: passed on a re-run, percent, at most",
            SettingKind::Number,
            2.0,
        ),
        band("reruns-high", "High: passed on a re-run, percent, at most", SettingKind::Number, 5.0),
        band(
            "reruns-medium",
            "Medium: passed on a re-run, percent, at most",
            SettingKind::Number,
            10.0,
        ),
        Setting::new(KEEP_FOR, "Keep daily figures for", SettingKind::Duration)
            .defaulting(json!(400.0 * DAY))
            .hinted("Long enough to compare a year with the one before.")
            .grouped("Keeping"),
        Setting::new(SCHEDULE, "When to recompute the last 30 days", SettingKind::Cron)
            .defaulting(json!(RECOMPUTE))
            .hinted("A cron expression in UTC. Runs that finished late are counted then.")
            .grouped("Keeping")
            .of_feature(METRICS),
    ]
}

pub fn features() -> Vec<Feature> {
    vec![
        Feature::new(
            METRICS,
            "CI/CD/CT metrics",
            "How often each service's pipelines succeed, how long they take, how quickly a broken \
             default branch is fixed and how often a run only passes when re-run, for continuous \
             integration, delivery and testing alike, from GitHub Actions.",
        )
        .needs(&["github", "ghe"], "pipeline-data"),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Ci,
    Cd,
    Ct,
}

impl Stage {
    pub const ALL: [Self; 3] = [Self::Ci, Self::Cd, Self::Ct];

    pub fn key(self) -> &'static str {
        match self {
            Self::Ci => "ci",
            Self::Cd => "cd",
            Self::Ct => "ct",
        }
    }

    pub fn from_key(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|stage| stage.key() == key)
    }

    pub fn short(self) -> &'static str {
        match self {
            Self::Ci => "CI",
            Self::Cd => "CD",
            Self::Ct => "CT",
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Ci => "Integration",
            Self::Cd => "Delivery",
            Self::Ct => "Testing",
        }
    }
}

/// The words each stage is known by, lower case.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Stages {
    pub cd: Vec<String>,
    pub ci: Vec<String>,
    pub ct: Vec<String>,
}

impl Stages {
    /// Delivery, then integration, then testing; a workflow none of them names is integration.
    /// An entry is a word of the workflow's name or file, or `owner/name:<name or file>` for one
    /// repository's workflow.
    pub fn of(&self, repository: &str, workflow: &str, path: &str) -> Stage {
        let file = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
        let stem = file.rsplit_once('.').map_or(file.as_str(), |(stem, _)| stem).to_string();
        let name = workflow.to_ascii_lowercase();
        let words: Vec<&str> = name
            .split(|c: char| !c.is_ascii_alphanumeric())
            .chain(stem.split(|c: char| !c.is_ascii_alphanumeric()))
            .filter(|word| !word.is_empty())
            .collect();
        let names = |entries: &[String]| {
            entries.iter().any(|entry| match entry.split_once(':') {
                Some((only, wanted)) => {
                    only.eq_ignore_ascii_case(repository)
                        && [name.as_str(), file.as_str(), stem.as_str()].contains(&wanted.trim())
                }
                None => words.contains(&entry.as_str()),
            })
        };
        match () {
            () if names(&self.cd) => Stage::Cd,
            () if names(&self.ci) => Stage::Ci,
            () if names(&self.ct) => Stage::Ct,
            () => Stage::Ci,
        }
    }
}

/// The thresholds of each band, better to worse: at least for success, at most for the rest.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Bands {
    pub success: [f64; 3],
    pub duration: [f64; 3],
    pub recovery: [f64; 3],
    pub reruns: [f64; 3],
}

/// Every definition, read once from the settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Definitions {
    pub sources: Vec<String>,
    pub stages: Stages,
    /// Only the default branch's runs, rather than every branch's.
    pub default_only: bool,
    pub bands: Bands,
    pub keep_days: i64,
}

impl Definitions {
    pub fn read(settings: &Settings) -> Self {
        let number =
            |key: &str, default: f64| settings.number(key).filter(|n| *n >= 0.0).unwrap_or(default);
        let three = |keys: [&str; 3], defaults: [f64; 3]| {
            [
                number(keys[0], defaults[0]),
                number(keys[1], defaults[1]),
                number(keys[2], defaults[2]),
            ]
        };
        let lower = |key: &str| -> Vec<String> {
            settings
                .list(key)
                .iter()
                .map(|item| item.trim().to_ascii_lowercase())
                .filter(|item| !item.is_empty())
                .collect()
        };
        Self {
            sources: lower(SOURCES),
            stages: Stages {
                cd: lower(CD_WORKFLOWS),
                ci: lower(CI_WORKFLOWS),
                ct: lower(CT_WORKFLOWS),
            },
            default_only: settings.text(BRANCHES) != "all",
            bands: Bands {
                success: three(
                    ["success-elite", "success-high", "success-medium"],
                    [90.0, 80.0, 60.0],
                ),
                duration: three(
                    ["duration-elite", "duration-high", "duration-medium"],
                    [600.0, 1_200.0, 3_600.0],
                ),
                recovery: three(
                    ["recovery-elite", "recovery-high", "recovery-medium"],
                    [3_600.0, 14_400.0, DAY],
                ),
                reruns: three(["reruns-elite", "reruns-high", "reruns-medium"], [2.0, 5.0, 10.0]),
            },
            keep_days: (number(KEEP_FOR, 400.0 * DAY) / DAY).ceil().max(1.0) as i64,
        }
    }

    /// What the stored figures depend on, so a change to it has them worked out again. Which
    /// branches count and the bands only change how they are read, so they are left out.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let counted = json!([self.sources, self.stages]);
        hex::encode(Sha256::digest(counted.to_string().as_bytes()))
    }
}
