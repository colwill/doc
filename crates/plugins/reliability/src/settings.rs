//! What reliability is judged against: the objectives every service has unless it sets its own,
//! DOC's own objectives to the teams that use it, how services are checked, and how DOC's status
//! history is read. Each objective is a setting, so a platform promises what it can keep.

use doc_plugin_sdk::{Feature, Setting, SettingKind, Settings};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub const SERVICES: &str = "services";
pub const PLATFORM: &str = "platform";

pub const SLA: &str = "sla";
pub const RTO: &str = "rto";
pub const RPO: &str = "rpo";
pub const MTTR: &str = "mttr";
pub const DOC_SLA: &str = "doc-sla";
pub const DOC_RTO: &str = "doc-rto";
pub const DOC_RPO: &str = "doc-rpo";
pub const DOC_MTTR: &str = "doc-mttr";
pub const DEGRADED: &str = "degraded-is-down";
pub const AT_RISK: &str = "at-risk";
pub const FAILURES: &str = "failures-before-down";
pub const TIMEOUT: &str = "check-timeout";
pub const ALLOWED: &str = "allowed-hosts";
pub const OUTAGES_FROM: &str = "outages-from";
pub const PROBE_SCHEDULE: &str = "check-schedule";
pub const PROBE: &str = "* * * * *";
pub const PLATFORM_SCHEDULE: &str = "platform-schedule";
pub const READ: &str = "*/5 * * * *";
pub const KEEP_FOR: &str = "keep-for";
pub const TELEMETRY_FOR: &str = "telemetry-for";
/// The most telemetry DOC keeps about any service or plugin without somewhere else keeping it
/// (DOC-SPEC §9.19). DOC is a platform for seeing how things stand, not a time-series database.
pub const TELEMETRY_CAP_DAYS: i64 = 7;

const HOUR: f64 = 3_600.0;
const DAY: f64 = 86_400.0;

pub fn declared() -> Vec<Setting> {
    let percent = |key: &str, label: &str, default: f64, group: &str| {
        Setting::new(key, label, SettingKind::Number)
            .defaulting(json!(default))
            .between(0.0, 100.0)
            .grouped(group)
    };
    let seconds = |key: &str, label: &str, default: f64, group: &str| {
        Setting::new(key, label, SettingKind::Duration).defaulting(json!(default)).grouped(group)
    };
    vec![
        percent(SLA, "Availability objective, percent", 99.9, "Service objectives").hinted(
            "What every service is held to unless it sets its own: the share of the time it is up.",
        ),
        seconds(RTO, "Recovery time objective", 4.0 * HOUR, "Service objectives")
            .hinted("The longest an outage, or a restore from backup, may take to recover from."),
        seconds(RPO, "Recovery point objective", DAY, "Service objectives")
            .hinted("The most data that may be lost: the longest there may be between backups."),
        seconds(MTTR, "Mean time to recover, at most", HOUR, "Service objectives")
            .hinted("How long outages may take to recover from, on average."),
        percent(DOC_SLA, "Availability objective, percent", 99.5, "DOC's objectives").hinted(
            "What DOC promises the teams that use it. DOC is up while its database, buses, \
             backend and frontend all are.",
        ),
        seconds(DOC_RTO, "Recovery time objective", HOUR, "DOC's objectives"),
        seconds(DOC_RPO, "Recovery point objective", DAY, "DOC's objectives").hinted(
            "Measured from the backups of DOC's database recorded here, by whatever takes them.",
        ),
        seconds(DOC_MTTR, "Mean time to recover, at most", 30.0 * 60.0, "DOC's objectives"),
        Setting::new(DEGRADED, "Degraded counts as down", SettingKind::Boolean)
            .defaulting(json!(false))
            .hinted(
                "A bus with a node down still works, so by default it is up. Turn this on to \
                 count any degraded part of DOC as an outage.",
            )
            .grouped("DOC's objectives")
            .of_feature(PLATFORM),
        percent(AT_RISK, "At risk from, percent of an objective", 75.0, "Judging").hinted(
            "An objective is at risk once this much of it is used: of the downtime the \
             availability objective allows, or of the time the others allow.",
        ),
        Setting::new(FAILURES, "Failed checks before a service is down", SettingKind::Number)
            .defaulting(json!(2))
            .between(1.0, 10.0)
            .hinted("So a single slow answer is not an outage. The outage starts at the first.")
            .grouped("Checks")
            .of_feature(SERVICES),
        seconds(TIMEOUT, "A check gives up after", 10.0, "Checks")
            .between(1.0, 25.0)
            .of_feature(SERVICES),
        Setting::new(ALLOWED, "Hosts checks may also reach", SettingKind::List)
            .hinted(
                "Checks never reach loopback, link-local or cloud metadata addresses, since a \
                 check URL is anybody's to set. Hosts listed here, such as localhost, may be \
                 checked anyway.",
            )
            .grouped("Checks")
            .of_feature(SERVICES),
        Setting::new(OUTAGES_FROM, "Where outages come from", SettingKind::List)
            .defaulting(json!(["kubernetes"]))
            .hinted(
                "Plugins that export outages to reliability, read at every check: kubernetes \
                 says a service is down while none of its pods are available in production. An \
                 outage something else already saw is counted once.",
            )
            .grouped("Checks")
            .of_feature(SERVICES),
        Setting::new(PROBE_SCHEDULE, "How often to check", SettingKind::Cron)
            .defaulting(json!(PROBE))
            .hinted("A cron expression in UTC.")
            .grouped("Checks")
            .of_feature(SERVICES),
        Setting::new(PLATFORM_SCHEDULE, "How often to read DOC's status", SettingKind::Cron)
            .defaulting(json!(READ))
            .hinted("A cron expression in UTC. Each read carries on from the last.")
            .grouped("DOC's objectives")
            .of_feature(PLATFORM),
        seconds(KEEP_FOR, "Keep outages, backups and restores for", 400.0 * DAY, "Keeping")
            .hinted("Long enough to compare a year with the one before."),
        seconds(TELEMETRY_FOR, "Keep telemetry for", 7.0 * DAY, "Keeping").hinted(
            "The five minutes at a time each service and plugin is watched in, which the hour \
             and six-hour views are drawn from. A week is the most DOC keeps, whatever is set \
             here, until a plugin that takes telemetry somewhere that keeps it — Grafana, \
             InfluxDB — is onboarded. The days watched are kept for as long as outages are \
             either way, so the longer views lose nothing.",
        ),
    ]
}

pub fn features() -> Vec<Feature> {
    vec![
        Feature::new(
            SERVICES,
            "Service reliability",
            "Checks each service's health URL every minute, and takes outages, backups and \
             restores reported by automations and the API, to judge each service's availability, \
             time to recover, recovery time and recovery point against its objectives.",
        ),
        Feature::new(
            PLATFORM,
            "DOC's reliability",
            "Reads DOC's own status history — its database, buses, backend, frontend and every \
             plugin — to judge DOC against the objectives it promises the teams that use it.",
        )
        .on(),
    ]
}

/// What something is held to. Seconds for the three times, percent for availability.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Targets {
    pub sla: f64,
    pub rto: f64,
    pub rpo: f64,
    pub mttr: f64,
}

/// Every definition, read once from the settings.
#[derive(Debug, Clone, PartialEq)]
pub struct Definitions {
    pub services: Targets,
    pub doc: Targets,
    pub degraded_is_down: bool,
    /// The fraction of an objective from which it is at risk.
    pub at_risk: f64,
    pub failures: i64,
    pub timeout: f64,
    pub allowed: Vec<String>,
    pub outages_from: Vec<String>,
    pub keep_days: i64,
    /// What the settings ask telemetry be kept for. What is actually kept is this or a week,
    /// whichever is less, until something else is keeping it: see `Definitions::telemetry_days`.
    pub telemetry_asked_days: i64,
}

impl Definitions {
    pub fn read(settings: &Settings) -> Self {
        let number =
            |key: &str, default: f64| settings.number(key).filter(|n| *n >= 0.0).unwrap_or(default);
        Self {
            services: Targets {
                sla: number(SLA, 99.9).min(100.0),
                rto: number(RTO, 4.0 * HOUR),
                rpo: number(RPO, DAY),
                mttr: number(MTTR, HOUR),
            },
            doc: Targets {
                sla: number(DOC_SLA, 99.5).min(100.0),
                rto: number(DOC_RTO, HOUR),
                rpo: number(DOC_RPO, DAY),
                mttr: number(DOC_MTTR, 30.0 * 60.0),
            },
            degraded_is_down: settings.boolean(DEGRADED),
            at_risk: (number(AT_RISK, 75.0) / 100.0).clamp(0.0, 1.0),
            failures: settings.integer(FAILURES).unwrap_or(2).clamp(1, 10),
            timeout: number(TIMEOUT, 10.0).clamp(1.0, 25.0),
            allowed: settings
                .list(ALLOWED)
                .iter()
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
            outages_from: settings
                .list(OUTAGES_FROM)
                .iter()
                .map(|source| source.trim().to_ascii_lowercase())
                .filter(|source| !source.is_empty())
                .collect(),
            keep_days: (number(KEEP_FOR, 400.0 * DAY) / DAY).ceil().max(1.0) as i64,
            telemetry_asked_days: (number(TELEMETRY_FOR, 7.0 * DAY) / DAY).ceil().max(1.0) as i64,
        }
    }

    /// How long telemetry is actually kept for. A week is the cap unless something is taking it
    /// somewhere that keeps it, in which case what the settings ask for stands: what is here has
    /// stopped being the only copy, so keeping less of it is no longer the kindness.
    pub fn telemetry_days(&self, exported: bool) -> i64 {
        match exported {
            true => self.telemetry_asked_days,
            false => self.telemetry_asked_days.min(TELEMETRY_CAP_DAYS),
        }
    }
}
