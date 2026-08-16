//! Faux reliability data, in the shape `reliability` keeps it: outages, backups, restores and time
//! watched, for services and for DOC's own parts (`doc:<component>`) and plugins
//! (`plugin:<id>`), as health checks, reports and DOC's status history would give them. Things
//! failed more often and for longer a year ago.

use std::collections::BTreeMap;

use chrono::{DateTime, Datelike, Duration, Utc};
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::dice::{DAY, Dice, midnight_of, minutes, progress};
use crate::estate::{self, Profile};
use crate::settings::Config;

/// How far before a period outages are made up, so one already going when it starts is there.
const BEFORE_DAYS: i64 = 3;
/// How far before a period backups are made up, so its first gap starts at a backup.
const BACKUPS_BEFORE_DAYS: i64 = 8;
/// DOC's database, whose backups are its recovery point.
const DATABASE: &str = "doc:postgres";

#[derive(Debug, Clone, Serialize)]
pub struct Outage {
    pub id: Uuid,
    pub subject: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: Option<DateTime<Utc>>,
    pub seconds: Option<f64>,
    pub source: String,
    pub detail: Option<String>,
    pub by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Backup {
    pub id: Uuid,
    pub subject: String,
    pub at: DateTime<Utc>,
    pub kind: Option<String>,
    pub note: Option<String>,
    pub by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Restore {
    pub id: Uuid,
    pub subject: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub seconds: f64,
    pub succeeded: bool,
    pub note: Option<String>,
    pub by: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Covered {
    pub id: String,
    pub subject: String,
    pub day: DateTime<Utc>,
    pub seconds: f64,
    pub checks: i64,
    pub failed: i64,
}

/// How reliable something is now. Minutes for outages and restores, hours between backups.
struct Behaviour {
    outages_a_month: f64,
    lasting: (f64, f64),
    backup_every: f64,
    missed_backups: f64,
    restoring: (f64, f64),
    failed_restores: f64,
}

/// One for each profile, best first.
const BEHAVIOURS: [Behaviour; 4] = [
    Behaviour {
        outages_a_month: 0.4,
        lasting: (2.0, 25.0),
        backup_every: 1.0,
        missed_backups: 0.01,
        restoring: (8.0, 30.0),
        failed_restores: 0.0,
    },
    Behaviour {
        outages_a_month: 1.2,
        lasting: (5.0, 90.0),
        backup_every: 6.0,
        missed_backups: 0.04,
        restoring: (25.0, 150.0),
        failed_restores: 0.03,
    },
    Behaviour {
        outages_a_month: 2.5,
        lasting: (15.0, 300.0),
        backup_every: 12.0,
        missed_backups: 0.06,
        restoring: (60.0, 360.0),
        failed_restores: 0.08,
    },
    Behaviour {
        outages_a_month: 5.0,
        lasting: (30.0, 900.0),
        backup_every: 24.0,
        missed_backups: 0.2,
        restoring: (180.0, 720.0),
        failed_restores: 0.2,
    },
];

/// A service as the estate has it; DOC's own parts mostly do well, and its plugins nearly so.
fn profile(config: &Config, subject: &str) -> Profile {
    match subject.split_once(':') {
        Some(("service", service)) => estate::profile(config, service),
        Some((kind, _)) => {
            let share = Dice::seeded(config.seed, subject, -1).unit();
            Profile::of_share(if kind == "doc" { share * 0.6 } else { share * 0.9 })
        }
        None => estate::profile(config, subject),
    }
}

/// Whether a subject has backups of its own: services and DOC's database.
fn backed_up(subject: &str) -> bool {
    subject.starts_with("service:") || subject == DATABASE
}

struct Maker<'a> {
    subject: &'a str,
    seed: u64,
    behaviour: &'static Behaviour,
    now: DateTime<Utc>,
}

impl Maker<'_> {
    /// The outages that started on one day.
    fn outages(&self, index: i64) -> Vec<Outage> {
        let Some(midnight) = midnight_of(index) else { return Vec::new() };
        let (subject, behaviour, now) = (self.subject, self.behaviour, self.now);
        let mut dice = Dice::seeded(self.seed, &format!("{subject}#outages"), index);
        let progress = progress(now, midnight);
        let (slower, riskier) = (1.5 - 0.7 * progress, 1.5 - 0.8 * progress);
        let (source, details): (&str, &[&str]) = match subject.split_once(':').map(|(kind, _)| kind)
        {
            Some("doc") => {
                ("status", &["connection refused", "no leader elected", "no answer within 5s"])
            }
            Some("plugin") => {
                ("status", &["it left the registry", "load failed: its database was unreachable"])
            }
            _ => (
                "check",
                &[
                    "it answered 503",
                    "no answer within 10s",
                    "it could not be connected to",
                    "it answered 500",
                ],
            ),
        };
        (0..dice.count(behaviour.outages_a_month / 30.0 * riskier))
            .filter_map(|_| {
                let started_at = midnight + minutes(dice.between((0.0, 1_440.0)));
                let ended_at = started_at + minutes(dice.spread(behaviour.lasting) * slower);
                let reported = source == "check" && dice.chance(0.2);
                let detail = match reported {
                    true => "reported: the payment provider was failing",
                    false => dice.pick(details),
                };
                let id = Uuid::from_u64_pair(dice.next(), dice.next());
                let over = ended_at <= now;
                (started_at <= now).then(|| Outage {
                    id,
                    subject: subject.to_string(),
                    started_at,
                    ended_at: over.then_some(ended_at),
                    seconds: over.then(|| (ended_at - started_at).num_seconds() as f64),
                    source: if reported { "report".into() } else { source.to_string() },
                    detail: Some(detail.to_string()),
                    by: reported.then(|| crate::ID.to_string()),
                })
            })
            .collect()
    }

    /// The backups taken on one day: one every so many hours, now and then missed.
    fn backups(&self, index: i64) -> Vec<Backup> {
        let Some(midnight) = midnight_of(index) else { return Vec::new() };
        if !backed_up(self.subject) {
            return Vec::new();
        }
        let behaviour = self.behaviour;
        let mut dice = Dice::seeded(self.seed, &format!("{}#backups", self.subject), index);
        let slots = (24.0 / behaviour.backup_every).round().max(1.0) as i64;
        (0..slots)
            .filter_map(|slot| {
                let at = midnight
                    + minutes(
                        60.0 * behaviour.backup_every * slot as f64
                            + 60.0
                            + dice.between((0.0, 10.0)),
                    );
                let missed = dice.chance(behaviour.missed_backups);
                (!missed && at <= self.now).then(|| Backup {
                    id: Uuid::from_u64_pair(dice.next(), dice.next()),
                    subject: self.subject.to_string(),
                    at,
                    kind: Some(if slots > 1 { "incremental" } else { "full" }.to_string()),
                    note: None,
                    by: Some(crate::ID.into()),
                })
            })
            .collect()
    }

    /// A restore drill on the 15th of the month, most months.
    fn restore(&self, index: i64) -> Option<Restore> {
        let midnight = midnight_of(index)?;
        if midnight.day() != 15 || !backed_up(self.subject) {
            return None;
        }
        let mut dice = Dice::seeded(self.seed, &format!("{}#restores", self.subject), index);
        if !dice.chance(0.8) {
            return None;
        }
        let slower = 1.5 - 0.7 * progress(self.now, midnight);
        let started_at = midnight + minutes(600.0);
        let seconds = (dice.spread(self.behaviour.restoring) * slower * 60.0).round();
        let finished_at = started_at + Duration::seconds(seconds as i64);
        (finished_at <= self.now).then(|| Restore {
            id: Uuid::from_u64_pair(dice.next(), dice.next()),
            subject: self.subject.to_string(),
            started_at,
            finished_at,
            seconds,
            succeeded: !dice.chance(self.behaviour.failed_restores),
            note: Some("restore drill".into()),
            by: Some(crate::ID.into()),
        })
    }
}

/// A subject as `reliability` would keep it, watched for longer than any period asked about.
fn subject(key: &str, now: DateTime<Utc>, down: bool) -> Value {
    let (kind, name) = key.split_once(':').unwrap_or(("service", key));
    let kind = if kind == "doc" { "component" } else { kind };
    json!({
        "subject": key,
        "kind": kind,
        "name": name,
        "url": (kind == "service").then(|| format!("https://{name}.faux.example/health")),
        "state": if down { "down" } else { "up" },
        "since": now - Duration::days(800),
        "checked_at": now,
        "status": if down { 503 } else { 200 },
        "latency_ms": 42,
    })
}

/// Everything these subjects would have kept from `from` to `to`: each subject, the outages that
/// overlap the period, the backups in it and the last one before it, the restores started in it,
/// and each day's time watched.
pub fn held(config: &Config, keys: &[String], from: DateTime<Utc>, to: DateTime<Utc>) -> Value {
    let now = Utc::now();
    let first = from.timestamp().div_euclid(DAY);
    let last = to.min(now).timestamp().div_euclid(DAY);
    let (mut outages, mut backups, mut restores, mut coverage) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut subjects = BTreeMap::new();
    for key in keys {
        let maker = Maker {
            subject: key,
            seed: config.seed,
            behaviour: &BEHAVIOURS[profile(config, key).index()],
            now,
        };
        let mut last_before: Option<Backup> = None;
        let mut down = false;
        for index in first - BACKUPS_BEFORE_DAYS..=last {
            for backup in maker.backups(index) {
                match backup.at < from {
                    true => last_before = Some(backup),
                    false if backup.at < to => backups.push(backup),
                    false => {}
                }
            }
            if index < first - BEFORE_DAYS {
                continue;
            }
            for outage in maker.outages(index) {
                if outage.started_at < to && outage.ended_at.is_none_or(|ended| ended >= from) {
                    down |= outage.ended_at.is_none();
                    outages.push(outage);
                }
            }
            restores
                .extend(maker.restore(index).filter(|r| from <= r.started_at && r.started_at < to));
            if index >= first
                && let Some(day) = midnight_of(index)
            {
                let watched = (now.min(day + Duration::days(1)) - day).num_seconds().max(0);
                coverage.push(Covered {
                    id: format!("{key}/{}", day.format("%Y-%m-%d")),
                    subject: key.clone(),
                    day,
                    seconds: watched as f64,
                    checks: watched / 60,
                    failed: 0,
                });
            }
        }
        backups.extend(last_before);
        subjects.insert(key.clone(), subject(key, now, down));
    }
    backups.sort_by_key(|backup| backup.at);
    json!({
        "subjects": subjects,
        "outages": outages,
        "backups": backups,
        "restores": restores,
        "coverage": coverage,
    })
}
