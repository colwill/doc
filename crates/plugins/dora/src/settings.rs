//! What DORA is configured with (ADR-0007 §4): where delivery data comes from, what counts as a
//! deployment and as a failure, and the performance bands. Organisations deploy and fail in
//! different ways, so every definition is a setting, and the defaults are the common case.

use chrono::Duration;
use doc_plugin_sdk::{Feature, Setting, SettingKind, Settings};
use serde::Serialize;
use serde_json::json;

pub const METRICS: &str = "metrics";
pub const COMPARE: &str = "compare-teams";

pub const SOURCES: &str = "sources";
pub const ROLLOUTS: &str = "rollouts-from";
pub const DEPLOYMENTS_FROM: &str = "deployments-from";
pub const LEAD_FROM: &str = "lead-time-from";
pub const WINDOW: &str = "failure-window";
pub const REVERTS: &str = "revert-signal";
pub const ROLLBACKS: &str = "rollback-signal";
pub const HOTFIXES: &str = "hotfix-signal";
pub const HOTFIX_LABELS: &str = "hotfix-labels";
pub const COUNTERS: &str = "failure-counters";
pub const KEEP_FOR: &str = "keep-for";
pub const SCHEDULE: &str = "recompute-schedule";
pub const RECOMPUTE: &str = "40 2 * * *";

const DAY: f64 = 86_400.0;

pub fn declared() -> Vec<Setting> {
    let band = |key: &str, label: &str, kind: SettingKind, default: f64| {
        Setting::new(key, label, kind).defaulting(json!(default)).grouped("Performance bands")
    };
    vec![
        Setting::new(SOURCES, "Where delivery data comes from", SettingKind::List)
            .hinted(
                "Plugins that export deployments and pull-requests to dora, such as github and \
                 ghe. Any that is not running is passed over.",
            )
            .defaulting(json!(["github", "ghe"]))
            .grouped("Sources"),
        Setting::new(ROLLOUTS, "Where rollouts come from", SettingKind::List)
            .hinted(
                "Plugins that export rollouts to dora, such as kubernetes. A repository one of \
                 them has ever rolled out to production takes its deployments from its finished \
                 production rollouts, and the delivery data above gives only their lead times, \
                 from the commits shipped with the same commit or the pull requests merged since \
                 the rollout before. Any other repository keeps the delivery data's deployments.",
            )
            .defaulting(json!(["kubernetes"]))
            .grouped("Sources"),
        Setting::new(DEPLOYMENTS_FROM, "What counts as a deployment", SettingKind::Choice)
            .one_of(&["prefer-deployments", "deployments", "workflows"])
            .defaulting(json!("prefer-deployments"))
            .hinted(
                "prefer-deployments: a repository's successful deployments to production, or, for \
                 one that never uses GitHub's deployments, successful runs of the workflows that \
                 deploy. deployments or workflows: only that one.",
            )
            .grouped("Definitions"),
        Setting::new(LEAD_FROM, "Change lead time starts at", SettingKind::Choice)
            .one_of(&["commit", "merge"])
            .defaulting(json!("commit"))
            .hinted(
                "commit: each commit a deployment shipped, from when it was committed. merge: each \
                 pull request it shipped, from when it was merged.",
            )
            .grouped("Definitions"),
        Setting::new(WINDOW, "A failure counts against a deployment for", SettingKind::Duration)
            .defaulting(json!(DAY))
            .hinted(
                "A revert, rollback, hotfix or counted incident is put down to the most recent \
                 deployment of the same repository before it, if that was this recent.",
            )
            .grouped("Failures"),
        Setting::new(REVERTS, "A revert is a failure", SettingKind::Boolean)
            .defaulting(json!(true))
            .hinted("A commit or pull request that starts with Revert.")
            .grouped("Failures"),
        Setting::new(ROLLBACKS, "A rollback is a failure", SettingKind::Boolean)
            .defaulting(json!(true))
            .hinted("Deploying a commit older than the one deployed before it.")
            .grouped("Failures"),
        Setting::new(HOTFIXES, "A hotfix is a failure", SettingKind::Boolean)
            .defaulting(json!(true))
            .hinted(
                "A pull request with one of the labels below, or whose title starts with hotfix.",
            )
            .grouped("Failures"),
        Setting::new(HOTFIX_LABELS, "Hotfix labels", SettingKind::List)
            .defaulting(json!(["hotfix"]))
            .grouped("Failures"),
        Setting::new(COUNTERS, "Counters that are failures", SettingKind::List)
            .defaulting(json!(["incidents", "hotfixes"]))
            .hinted(
                "Counted with the increment operation, from an automation or the API, naming a \
                 service or a repository: each one is a failure of the deployment before it.",
            )
            .grouped("Failures"),
        band("frequency-elite", "Elite: deployments a week, at least", SettingKind::Number, 7.0)
            .hinted(
                "The defaults follow DORA's 2023 State of DevOps report; change them to follow a \
                 later one.",
            ),
        band("frequency-high", "High: deployments a week, at least", SettingKind::Number, 1.0),
        band("frequency-medium", "Medium: deployments a week, at least", SettingKind::Number, 0.25),
        band("lead-elite", "Elite: lead time, at most", SettingKind::Duration, DAY),
        band("lead-high", "High: lead time, at most", SettingKind::Duration, 7.0 * DAY),
        band("lead-medium", "Medium: lead time, at most", SettingKind::Duration, 30.0 * DAY),
        band("fail-elite", "Elite: change fail rate, percent, at most", SettingKind::Number, 5.0),
        band("fail-high", "High: change fail rate, percent, at most", SettingKind::Number, 10.0),
        band(
            "fail-medium",
            "Medium: change fail rate, percent, at most",
            SettingKind::Number,
            15.0,
        ),
        band("recovery-elite", "Elite: recovery time, at most", SettingKind::Duration, 3_600.0),
        band("recovery-high", "High: recovery time, at most", SettingKind::Duration, DAY),
        band("recovery-medium", "Medium: recovery time, at most", SettingKind::Duration, 7.0 * DAY),
        Setting::new(KEEP_FOR, "Keep deployments for", SettingKind::Duration)
            .defaulting(json!(400.0 * DAY))
            .hinted("Long enough to compare a year with the one before.")
            .grouped("Keeping"),
        Setting::new(SCHEDULE, "When to recompute the last 30 days", SettingKind::Cron)
            .defaulting(json!(RECOMPUTE))
            .hinted("A cron expression in UTC. Late data is counted then.")
            .grouped("Keeping")
            .of_feature(METRICS),
    ]
}

pub fn features() -> Vec<Feature> {
    vec![
        Feature::new(
            METRICS,
            "DORA metrics",
            "Deployment frequency, change lead time, change fail rate and failed deployment \
             recovery time, for every service, team and organisation, from GitHub's delivery data.",
        )
        .needs(&["github", "ghe"], "delivery-data"),
        Feature::new(
            COMPARE,
            "Compare teams",
            "A Teams tab putting every team's four metrics side by side.",
        )
        .warning(
            "DORA's own guidance is that these metrics are for a team to improve itself, not for \
             ranking teams against each other: comparisons invite gaming the numbers. They are \
             never shown for a person.",
        ),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum From {
    PreferDeployments,
    Deployments,
    Workflows,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum LeadFrom {
    Commit,
    Merge,
}

/// The thresholds of each band, better to worse: at least for frequency, at most for the rest.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Bands {
    pub frequency: [f64; 3],
    pub lead: [f64; 3],
    pub fail: [f64; 3],
    pub recovery: [f64; 3],
}

/// Every definition, read once from the settings.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Definitions {
    pub sources: Vec<String>,
    pub rollouts: Vec<String>,
    pub from: From,
    pub lead_from: LeadFrom,
    pub window_seconds: i64,
    pub reverts: bool,
    pub rollbacks: bool,
    pub hotfixes: bool,
    pub hotfix_labels: Vec<String>,
    pub counters: Vec<String>,
    pub bands: Bands,
    pub keep_days: i64,
}

impl Definitions {
    pub fn read(settings: &Settings) -> Self {
        let seconds =
            |key: &str, default: f64| settings.number(key).filter(|n| *n >= 0.0).unwrap_or(default);
        let three = |keys: [&str; 3], defaults: [f64; 3]| {
            [
                seconds(keys[0], defaults[0]),
                seconds(keys[1], defaults[1]),
                seconds(keys[2], defaults[2]),
            ]
        };
        let lower = |key: &str| {
            settings.list(key).iter().map(|item| item.trim().to_ascii_lowercase()).collect()
        };
        Self {
            sources: lower(SOURCES),
            rollouts: lower(ROLLOUTS),
            from: match settings.text(DEPLOYMENTS_FROM).as_str() {
                "deployments" => From::Deployments,
                "workflows" => From::Workflows,
                _ => From::PreferDeployments,
            },
            lead_from: match settings.text(LEAD_FROM).as_str() {
                "merge" => LeadFrom::Merge,
                _ => LeadFrom::Commit,
            },
            window_seconds: seconds(WINDOW, DAY) as i64,
            reverts: settings.boolean(REVERTS),
            rollbacks: settings.boolean(ROLLBACKS),
            hotfixes: settings.boolean(HOTFIXES),
            hotfix_labels: lower(HOTFIX_LABELS),
            counters: lower(COUNTERS),
            bands: Bands {
                frequency: three(
                    ["frequency-elite", "frequency-high", "frequency-medium"],
                    [7.0, 1.0, 0.25],
                ),
                lead: three(
                    ["lead-elite", "lead-high", "lead-medium"],
                    [DAY, 7.0 * DAY, 30.0 * DAY],
                ),
                fail: three(["fail-elite", "fail-high", "fail-medium"], [5.0, 10.0, 15.0]),
                recovery: three(
                    ["recovery-elite", "recovery-high", "recovery-medium"],
                    [3_600.0, DAY, 7.0 * DAY],
                ),
            },
            keep_days: (seconds(KEEP_FOR, 400.0 * DAY) / DAY).ceil().max(1.0) as i64,
        }
    }

    pub fn window(&self) -> Duration {
        Duration::seconds(self.window_seconds)
    }

    /// What the stored records depend on, so a change to any of it has them worked out again.
    /// The bands only change how the records are shown, so they are left out.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let counted = json!([
            self.sources,
            self.rollouts,
            self.from,
            self.lead_from,
            self.window_seconds,
            self.reverts,
            self.rollbacks,
            self.hotfixes,
            self.hotfix_labels,
            self.counters,
        ]);
        hex::encode(Sha256::digest(counted.to_string().as_bytes()))
    }
}
