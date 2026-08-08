//! Judging reliability over a period against objectives: availability against the downtime its
//! objective allows, mean time to recover, the longest recovery — from an outage or a restore —
//! against the recovery time objective, and the most data at risk between backups against the
//! recovery point objective. Each is met, at risk, missed, or not measured when there is nothing
//! to measure it by, which a page says rather than guessing.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, Duration, Utc};
use doc_plugin_sdk::{Backend, Order, PluginError, Query};
use futures::StreamExt;
use serde::Serialize;
use serde_json::{Value, json};

use crate::Refusal;
use crate::record::{SAMPLE_SECONDS, midnight, sample_at};
use crate::settings::{Definitions, Targets};
use crate::store::{Backup, Covered, Outage, Restore, Sample, Subject};

const DAY: f64 = 86_400.0;
/// Subjects asked for in one query, and how many are asked about at once.
const IN_ONE: usize = 200;
const AT_ONCE: usize = 8;
const PRIMARY: &str = "#5b00b8";
const FAILED: &str = "#d5281b";
const SECONDARY: &str = "#768692";

/// From `from`, up to but not including `to`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Period {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

impl Period {
    pub fn last(days: i64) -> Self {
        Self::last_seconds(days * 86_400)
    }

    /// The period the page's choices are made of: an hour and six, as well as days.
    pub fn last_seconds(seconds: i64) -> Self {
        let to = Utc::now();
        Self { from: to - Duration::seconds(seconds.max(60)), to }
    }

    pub fn days(&self) -> f64 {
        ((self.to - self.from).num_seconds() as f64 / DAY).max(1.0 / 24.0)
    }

    pub fn seconds(&self) -> i64 {
        (self.to - self.from).num_seconds().max(60)
    }

    /// Short enough to be drawn from telemetry rather than from the days watched.
    pub fn within_a_day(&self) -> bool {
        self.seconds() <= 86_400
    }

    pub fn contains(&self, at: DateTime<Utc>) -> bool {
        self.from <= at && at < self.to
    }

    /// How it is said on a page: "30 days", "6 hours".
    pub fn said(&self) -> String {
        let seconds = self.seconds();
        if seconds < 86_400 {
            let hours = (seconds as f64 / 3_600.0).round() as i64;
            return match hours {
                0 | 1 => "hour".to_string(),
                hours => format!("{hours} hours"),
            };
        }
        let days = self.days().round() as i64;
        match days {
            1 => "24 hours".to_string(),
            365 | 366 => "year".to_string(),
            days => format!("{days} days"),
        }
    }
}

/// Everything a period is judged from, for some subjects.
#[derive(Debug, Clone, Default)]
pub struct Held {
    pub subjects: BTreeMap<String, Subject>,
    /// Those that overlap the period, however early they started.
    pub outages: Vec<Outage>,
    /// Those in the period, and the last before it of each subject.
    pub backups: Vec<Backup>,
    pub restores: Vec<Restore>,
    pub coverage: Vec<Covered>,
    /// The five minutes each subject was watched, for a period short enough to be drawn from
    /// them. Empty for a longer one, which reads the days instead.
    pub samples: Vec<Sample>,
    /// Whether this was gathered from telemetry, so a subject with none of it reads as unwatched
    /// rather than falling back to a day's worth spread evenly over it.
    pub sampled: bool,
}

/// What these subjects have kept for the period. With faux data, `faux-data`'s instead.
pub async fn held(backend: &Backend, keys: &[String], period: &Period) -> Result<Held, Refusal> {
    if crate::faux::on(backend) {
        return crate::faux::held(backend, keys, period).await;
    }
    let mut found = Held::default();
    for chunk in keys.chunks(IN_ONE) {
        let within = json!({ "gte": period.from, "lt": period.to });
        let subjects: Vec<Subject> = backend
            .query_all(Query::new("subjects").filter(json!({ "subject": { "in": chunk } })))
            .await?;
        found.subjects.extend(subjects.into_iter().map(|held| (held.subject.clone(), held)));
        found.outages.extend(
            backend
                .query_all::<Outage>(Query::new("outages").filter(json!({
                    "subject": { "in": chunk },
                    "started_at": { "lt": period.to },
                    "any": [{ "ended_at": { "is_null": true } }, { "ended_at": { "gte": period.from } }],
                })))
                .await?,
        );
        let filter = json!({ "subject": { "in": chunk }, "at": within });
        found
            .backups
            .extend(backend.query_all::<Backup>(Query::new("backups").filter(filter)).await?);
        let filter = json!({ "subject": { "in": chunk }, "started_at": within });
        found
            .restores
            .extend(backend.query_all::<Restore>(Query::new("restores").filter(filter)).await?);
        let days = json!({ "gte": midnight(period.from), "lt": period.to });
        let filter = json!({ "subject": { "in": chunk }, "day": days });
        found
            .coverage
            .extend(backend.query_all::<Covered>(Query::new("coverage").filter(filter)).await?);
        // An hour, six or twelve is drawn from the telemetry; a day or longer from the days
        // watched, which is all that is kept past a week anyway.
        if period.within_a_day() {
            let from = sample_at(period.from);
            let filter =
                json!({ "subject": { "in": chunk }, "at": { "gte": from, "lt": period.to } });
            found
                .samples
                .extend(backend.query_all::<Sample>(Query::new("samples").filter(filter)).await?);
        }
    }
    found.sampled = period.within_a_day();
    // The last backup before the period, of each subject that has had any, starts its first gap.
    let before: Vec<Result<Option<Backup>, PluginError>> = futures::stream::iter(keys.to_vec())
        .map(|key| async move {
            let asked = Query::new("backups")
                .filter(json!({ "subject": key, "at": { "lt": period.from } }))
                .order(Order::desc("at"))
                .limit(1);
            Ok(backend.query::<Backup>(asked).await?.records.into_iter().next())
        })
        .buffer_unordered(AT_ONCE)
        .collect()
        .await;
    for last in before {
        found.backups.extend(last?);
    }
    found.backups.sort_by_key(|backup| backup.at);
    Ok(found)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Met,
    AtRisk,
    Missed,
    Unknown,
}

impl Verdict {
    pub fn word(self) -> &'static str {
        match self {
            Self::Met => "Met",
            Self::AtRisk => "At risk",
            Self::Missed => "Missed",
            Self::Unknown => "Not measured",
        }
    }

    /// The badge it is shown with: its meaning, since colour alone says nothing.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Met => "ready",
            Self::AtRisk => "degraded",
            Self::Missed => "error",
            Self::Unknown => "unknown",
        }
    }

    /// From how much of an objective is used: met up to `at_risk` of it, at risk up to all of it.
    fn of(used: f64, at_risk: f64) -> Self {
        match used {
            used if used <= at_risk => Self::Met,
            used if used <= 1.0 => Self::AtRisk,
            _ => Self::Missed,
        }
    }

    /// The worst of several, leaving out those not measured unless nothing was.
    fn worst(verdicts: impl Iterator<Item = Self>) -> Self {
        verdicts.filter(|verdict| *verdict != Self::Unknown).max().unwrap_or(Self::Unknown)
    }
}

/// One subject judged over one period.
#[derive(Debug, Clone, Serialize)]
pub struct Judged {
    pub subject: String,
    pub title: String,
    pub targets: Targets,
    /// Seconds of the period since it was first watched.
    pub watched_for: f64,
    pub downtime: f64,
    pub availability: Option<f64>,
    /// How much of the downtime the objective allows has been used, 1 being all of it.
    pub budget_used: Option<f64>,
    /// Outages that started in the period.
    pub outages: usize,
    /// Outages still open now.
    pub open: usize,
    /// Seconds: the mean, and the longest, of the outages that started in the period.
    pub mttr: Option<f64>,
    pub longest: Option<f64>,
    pub restores: usize,
    pub slowest_restore: Option<f64>,
    pub failed_restores: usize,
    /// Seconds: the most data that was ever at risk in the period, since the backup before.
    pub exposure: Option<f64>,
    pub last_backup: Option<DateTime<Utc>>,
    pub backups: usize,
    /// The share of the time it was watched, where it is checked at all.
    pub coverage: Option<f64>,
    pub sla: Verdict,
    pub recovery: Verdict,
    pub rto: Verdict,
    pub rpo: Verdict,
}

impl Judged {
    /// The slowest recovery, from an outage or a restore.
    pub fn worst_recovery(&self) -> Option<f64> {
        match (self.longest, self.slowest_restore) {
            (Some(one), Some(two)) => Some(one.max(two)),
            (one, two) => one.or(two),
        }
    }

    pub fn json(&self) -> Value {
        let round = |value: Option<f64>| value.map(f64::round);
        json!({
            "subject": self.subject,
            "title": self.title,
            "objectives": {
                "availability_percent": self.targets.sla,
                "rto_seconds": self.targets.rto,
                "rpo_seconds": self.targets.rpo,
                "mttr_seconds": self.targets.mttr,
            },
            "availability_percent": self.availability.map(|a| (a * 1_000.0).round() / 1_000.0),
            "downtime_seconds": self.downtime.round(),
            "downtime_allowed_used": self.budget_used.filter(|used| used.is_finite()).map(|used| (used * 100.0).round() / 100.0),
            "outages": self.outages,
            "still_down": self.open,
            "mttr_seconds": round(self.mttr),
            "longest_outage_seconds": round(self.longest),
            "restores": self.restores,
            "slowest_restore_seconds": round(self.slowest_restore),
            "failed_restores": self.failed_restores,
            "most_data_at_risk_seconds": round(self.exposure),
            "last_backup": self.last_backup,
            "backups": self.backups,
            "watched_percent": self.coverage.map(|c| (c * 1_000.0).round() / 10.0),
            "verdicts": { "availability": self.sla, "mttr": self.recovery, "rto": self.rto, "rpo": self.rpo },
        })
    }
}

/// Outages merged where they overlap, as `(start, end)`, with `None` for one still open.
fn merged(
    mut spans: Vec<(DateTime<Utc>, Option<DateTime<Utc>>)>,
) -> Vec<(DateTime<Utc>, Option<DateTime<Utc>>)> {
    spans.sort_by_key(|(start, _)| *start);
    let mut out: Vec<(DateTime<Utc>, Option<DateTime<Utc>>)> = Vec::new();
    for (start, end) in spans {
        match out.last_mut() {
            Some((_, held)) if held.is_none_or(|held| start <= held) => {
                *held = match (*held, end) {
                    (Some(one), Some(two)) => Some(one.max(two)),
                    _ => None,
                };
            }
            _ => out.push((start, end)),
        }
    }
    out
}

/// Seconds `subject` was watched from `start` to `end`, from its days watched: a day only partly
/// inside in proportion to the part of it that could be watched, from `since` until now.
fn covered(held: &Held, subject: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> f64 {
    if held.sampled {
        return sampled(held, subject, start, end);
    }
    let now = Utc::now();
    let since = held.subjects.get(subject).and_then(|held| held.since);
    held.coverage
        .iter()
        .filter(|c| c.subject == subject)
        .map(|c| {
            let day_end = c.day + Duration::days(1);
            let from = since.map_or(c.day, |since| since.max(c.day));
            let inside = (day_end.min(end) - from.max(start)).num_seconds().max(0) as f64;
            let length = (day_end.min(now) - from).num_seconds().max(1) as f64;
            c.seconds * (inside / length).min(1.0)
        })
        .sum::<f64>()
        + 0.0
}

/// Seconds `subject` was watched from `start` to `end`, from its telemetry: each five minutes in
/// proportion to the part of it inside, so a window that does not fall on a boundary is not
/// credited with the whole of the five minutes at each end.
fn sampled(held: &Held, subject: &str, start: DateTime<Utc>, end: DateTime<Utc>) -> f64 {
    let length = Duration::seconds(SAMPLE_SECONDS);
    held.samples
        .iter()
        .filter(|sample| sample.subject == subject)
        .map(|sample| {
            let over = sample.at + length;
            let inside = (over.min(end) - sample.at.max(start)).num_seconds().max(0) as f64;
            sample.seconds * (inside / SAMPLE_SECONDS as f64).min(1.0)
        })
        .sum::<f64>()
        + 0.0
}

/// Judges `subject` over the period by its objectives.
pub fn judge(
    subject: &str,
    title: &str,
    targets: Targets,
    held: &Held,
    period: &Period,
    at_risk: f64,
) -> Judged {
    let now = Utc::now();
    let end = period.to.min(now);
    let since = held.subjects.get(subject).and_then(|held| held.since);
    let start = since.map(|since| since.max(period.from));
    let span = start.map_or(0.0, |start| (end - start).num_seconds().max(0) as f64);
    // Something checked was watched only while the checks came: a gap counts neither way.
    let checked = held
        .subjects
        .get(subject)
        .is_some_and(|held| held.url.is_some() || held.kind == "component");
    let covered = start.map_or(0.0, |start| covered(held, subject, start, end));
    let coverage = (checked && span > 0.0).then(|| (covered / span).min(1.0));
    let watched_for = if checked { covered.min(span) } else { span };
    let theirs: Vec<&Outage> = held.outages.iter().filter(|o| o.subject == subject).collect();
    let spans = merged(theirs.iter().map(|o| (o.started_at, o.ended_at)).collect());
    let downtime: f64 = start.map_or(0.0, |start| {
        spans
            .iter()
            .map(|(from, to)| {
                let (from, to) = ((*from).max(start), to.unwrap_or(now).min(end));
                (to - from).num_seconds().max(0) as f64
            })
            .sum::<f64>()
            .min(watched_for)
            // An empty sum of floats is -0.0.
            + 0.0
    });
    let availability = (watched_for > 0.0).then(|| (1.0 - downtime / watched_for) * 100.0);
    let allowed = (1.0 - targets.sla / 100.0) * watched_for;
    let budget_used = (watched_for > 0.0).then(|| match allowed > 0.0 {
        true => downtime / allowed,
        false if downtime > 0.0 => f64::INFINITY,
        false => 0.0,
    });
    let started: Vec<&&Outage> = theirs.iter().filter(|o| period.contains(o.started_at)).collect();
    let lasted: Vec<f64> = started.iter().map(|o| o.lasted(now)).collect();
    let mttr = (!lasted.is_empty()).then(|| lasted.iter().sum::<f64>() / lasted.len() as f64);
    let longest = lasted.iter().copied().reduce(f64::max);
    let restores: Vec<&Restore> = held
        .restores
        .iter()
        .filter(|r| r.subject == subject && period.contains(r.started_at))
        .collect();
    let slowest_restore = restores.iter().map(|r| r.seconds).reduce(f64::max);
    let failed_restores = restores.iter().filter(|r| !r.succeeded).count();

    let backups: Vec<DateTime<Utc>> =
        held.backups.iter().filter(|b| b.subject == subject).map(|b| b.at).collect();
    let mut exposure: Option<f64> = None;
    let mut previous: Option<DateTime<Utc>> = None;
    // Each gap between backups that reaches into the period, the last running to its end.
    for at in backups.iter().copied().filter(|at| *at < end).chain([end]) {
        if let Some(before) = previous
            && at >= period.from
        {
            let gap = (at - before).num_seconds().max(0) as f64;
            exposure = Some(exposure.map_or(gap, |held| held.max(gap)));
        }
        previous = Some(at);
    }
    let in_period = backups.iter().filter(|at| period.contains(**at)).count();

    let recovery = match mttr {
        None => Verdict::Met,
        Some(mean) => Verdict::of(mean / targets.mttr.max(1.0), at_risk),
    };
    let worst = match (longest, slowest_restore) {
        (Some(one), Some(two)) => Some(one.max(two)),
        (one, two) => one.or(two),
    };
    Judged {
        subject: subject.to_string(),
        title: title.to_string(),
        targets,
        watched_for,
        downtime,
        availability,
        budget_used,
        outages: started.len(),
        open: theirs.iter().filter(|o| o.ended_at.is_none()).count(),
        mttr,
        longest,
        restores: restores.len(),
        slowest_restore,
        failed_restores,
        exposure,
        last_backup: backups.iter().copied().filter(|at| *at < end).max(),
        backups: in_period,
        coverage,
        sla: budget_used.map_or(Verdict::Unknown, |used| Verdict::of(used, at_risk)),
        recovery,
        rto: match (worst, failed_restores) {
            (_, failed) if failed > 0 => Verdict::Missed,
            (Some(worst), _) => Verdict::of(worst / targets.rto.max(1.0), at_risk),
            (None, _) => Verdict::Unknown,
        },
        rpo: exposure
            .map_or(Verdict::Unknown, |gap| Verdict::of(gap / targets.rpo.max(1.0), at_risk)),
    }
}

/// DOC as one: down whenever any part of it is, with the backups and restores of its database;
/// and what it was judged from, as the one subject `doc`, for its charts.
pub fn composite(
    held: &Held,
    period: &Period,
    definitions: &Definitions,
    parts: &[String],
) -> (Judged, Held) {
    let ours = |subject: &str| parts.iter().any(|part| part == subject);
    let spans = merged(
        held.outages
            .iter()
            .filter(|o| ours(&o.subject))
            .map(|o| (o.started_at, o.ended_at))
            .collect(),
    );
    let as_doc =
        |subject: &str| if ours(subject) { "doc".to_string() } else { subject.to_string() };
    let since = parts.iter().filter_map(|part| held.subjects.get(part)?.since).min();
    // DOC was watched while all of its parts were: as long as the least watched of them.
    let start = since.map_or(period.from, |since| since.max(period.from));
    let least = parts
        .iter()
        .filter(|part| held.subjects.contains_key(*part))
        .map(|part| (covered(held, part, start, period.to), part))
        .min_by(|one, two| one.0.total_cmp(&two.0))
        .map(|(_, part)| part.clone());
    let mut whole = Held {
        sampled: held.sampled,
        // DOC was watched as long as the least watched of its parts, so its telemetry is that
        // part's under DOC's name, exactly as its days watched below are.
        samples: held
            .samples
            .iter()
            .filter(|sample| least.as_deref() == Some(sample.subject.as_str()))
            .map(|sample| Sample { subject: "doc".into(), ..sample.clone() })
            .collect(),
        subjects: BTreeMap::from([(
            "doc".to_string(),
            Subject {
                subject: "doc".into(),
                kind: "component".into(),
                since,
                ..Subject::default()
            },
        )]),
        outages: spans
            .into_iter()
            .map(|(started_at, ended_at)| Outage {
                id: uuid::Uuid::nil(),
                subject: "doc".into(),
                started_at,
                ended_at,
                seconds: None,
                source: "status".into(),
                detail: None,
                by: None,
            })
            .collect(),
        backups: held.backups.iter().filter(|b| ours(&b.subject)).cloned().collect(),
        restores: held.restores.iter().filter(|r| ours(&r.subject)).cloned().collect(),
        coverage: held
            .coverage
            .iter()
            .filter(|c| least.as_deref() == Some(c.subject.as_str()))
            .map(|c| Covered { subject: "doc".into(), ..c.clone() })
            .collect(),
    };
    for backup in &mut whole.backups {
        backup.subject = as_doc(&backup.subject);
    }
    for restore in &mut whole.restores {
        restore.subject = as_doc(&restore.subject);
    }
    let judged = judge("doc", "DOC", definitions.doc, &whole, period, definitions.at_risk);
    (judged, whole)
}

/// A span of time as people say it: `3d 4h`, `5h 12m`, `42m`, `30s`.
pub fn duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as i64;
    let (days, hours, minutes) = (total / 86_400, total % 86_400 / 3_600, total % 3_600 / 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{total}s"),
        (0, 0, minutes) => format!("{minutes}m"),
        (0, hours, 0) => format!("{hours}h"),
        (0, hours, minutes) => format!("{hours}h {minutes}m"),
        (days, 0, _) => format!("{days}d"),
        (days, hours, _) => format!("{days}d {hours}h"),
    }
}

/// Availability to as many places as it takes to say something: 99.95%, not 100% for a blip.
pub fn percent(value: f64) -> String {
    // An empty sum of floats is -0.0, which is still nothing.
    let value = if value == 0.0 { 0.0 } else { value };
    let places = match value {
        value if value >= 99.99 => 3,
        value if value >= 99.0 => 2,
        _ => 1,
    };
    let text = format!("{value:.places$}");
    let text = text.trim_end_matches('0').trim_end_matches('.');
    format!("{text}%")
}

pub fn plural(count: usize, word: &str) -> String {
    match count {
        1 => format!("1 {word}"),
        count => format!("{count} {word}s"),
    }
}

/// One headline, its verdict and what is behind it.
#[derive(Debug, Clone)]
pub struct Tile {
    pub label: &'static str,
    pub value: String,
    pub verdict: Verdict,
    pub note: String,
}

/// The four tiles for one subject.
pub fn tiles(judged: &Judged, period: &Period) -> Vec<Tile> {
    let targets = judged.targets;
    let span = period.said();
    let outages = match (judged.outages, judged.open) {
        (0, _) => format!("No outages in the last {span}."),
        (count, 0) => format!("{} in the last {span}.", plural(count, "outage")),
        (count, open) => {
            format!("{} in the last {span}, {open} still going.", plural(count, "outage"))
        }
    };
    let watched = judged
        .coverage
        .map(|coverage| format!(" Watched {} of the time.", percent(coverage * 100.0)))
        .unwrap_or_default();
    vec![
        Tile {
            label: "Availability",
            value: judged.availability.map_or_else(|| "Not watched".into(), percent),
            verdict: judged.sla,
            note: match judged.availability {
                None => "Nothing checks it and no outage has been reported, so there is nothing to go on.".into(),
                Some(_) => {
                    let allowed = duration((1.0 - targets.sla / 100.0) * judged.watched_for);
                    let used = match judged.downtime > 0.0 {
                        true => format!("down {} of the {allowed} it may be", duration(judged.downtime)),
                        false => format!("never down, of the {allowed} it may be"),
                    };
                    format!("Objective {}: {used}. {outages}{watched}", percent(targets.sla))
                }
            },
        },
        Tile {
            label: "Mean time to recover",
            value: judged.mttr.map_or_else(|| "No outages".into(), duration),
            verdict: judged.recovery,
            note: match judged.mttr {
                None => format!("Objective {}. Nothing to recover from.", duration(targets.mttr)),
                Some(_) => format!("Objective {}. {outages}", duration(targets.mttr)),
            },
        },
        Tile {
            label: "Recovery time",
            value: judged.worst_recovery().map_or_else(|| "Not tested".into(), duration),
            verdict: judged.rto,
            note: {
                let restored = match (judged.restores, judged.slowest_restore) {
                    (0, _) | (_, None) => "no restore was tried".to_string(),
                    (count, Some(slowest)) => format!(
                        "the slowest of {} took {}{}",
                        plural(count, "restore"),
                        duration(slowest),
                        match judged.failed_restores {
                            0 => String::new(),
                            failed => format!(", and {failed} failed"),
                        }
                    ),
                };
                let outage = judged
                    .longest
                    .map_or_else(|| "no outage".to_string(), |longest| format!("the longest outage took {}", duration(longest)));
                format!("Objective {}: {outage}, and {restored}.", duration(targets.rto))
            },
        },
        Tile {
            label: "Recovery point",
            value: judged.exposure.map_or_else(|| "No backups".into(), duration),
            verdict: judged.rpo,
            note: match (judged.exposure, judged.last_backup) {
                (Some(_), Some(last)) => format!(
                    "Objective {}: the most data at risk, between backups. The last was {} ago; {} in the last {span}.",
                    duration(targets.rpo),
                    duration((period.to.min(Utc::now()) - last).num_seconds() as f64),
                    plural(judged.backups, "backup")
                ),
                _ => format!(
                    "Objective {}. No backup is recorded, so the data at risk is unknown.",
                    duration(targets.rpo)
                ),
            },
        },
    ]
}

/// How many of a group met an objective.
fn tallied(judged: &[Judged], pick: fn(&Judged) -> Verdict) -> (usize, usize, Verdict) {
    let known: Vec<Verdict> = judged.iter().map(pick).filter(|v| *v != Verdict::Unknown).collect();
    let met = known.iter().filter(|v| **v == Verdict::Met).count();
    (met, known.len(), Verdict::worst(known.into_iter()))
}

/// The four tiles for a group of services, each judged by its own objectives.
pub fn group_tiles(judged: &[Judged], period: &Period) -> Vec<Tile> {
    let span = period.said();
    let of = |(met, known, _): (usize, usize, Verdict)| match known {
        0 => "None measured".to_string(),
        known => format!("{met} of {known}"),
    };
    let unmeasured = |known: usize, what: &str| match judged.len() - known {
        0 => String::new(),
        left => format!(" {} {what}.", plural(left, "service")),
    };
    let lowest = judged
        .iter()
        .filter_map(|j| Some((j.availability?, j)))
        .min_by(|one, two| one.0.total_cmp(&two.0))
        .map(|(availability, j)| {
            format!(
                "Lowest: {} at {} against {}.",
                j.title,
                percent(availability),
                percent(j.targets.sla)
            )
        })
        .unwrap_or_default();
    let sla = tallied(judged, |j| j.sla);
    let recovery = tallied(judged, |j| j.recovery);
    let rto = tallied(judged, |j| j.rto);
    let rpo = tallied(judged, |j| j.rpo);
    let outages: usize = judged.iter().map(|j| j.outages).sum();
    let lasted: Vec<f64> = judged.iter().filter_map(|j| Some(j.mttr? * j.outages as f64)).collect();
    let mean = (outages > 0).then(|| lasted.iter().sum::<f64>() / outages as f64);
    let slowest = judged
        .iter()
        .filter_map(|j| Some((j.worst_recovery()? / j.targets.rto.max(1.0), j)))
        .max_by(|one, two| one.0.total_cmp(&two.0))
        .and_then(|(_, j)| {
            Some(format!(
                "Slowest: {} took {} against {}.",
                j.title,
                duration(j.worst_recovery()?),
                duration(j.targets.rto)
            ))
        })
        .unwrap_or_default();
    let riskiest = judged
        .iter()
        .filter_map(|j| Some((j.exposure? / j.targets.rpo.max(1.0), j)))
        .max_by(|one, two| one.0.total_cmp(&two.0))
        .and_then(|(_, j)| {
            Some(format!(
                "Most at risk: {}, {} between backups against {}.",
                j.title,
                duration(j.exposure?),
                duration(j.targets.rpo)
            ))
        })
        .unwrap_or_default();
    vec![
        Tile {
            label: "Availability objective met",
            value: of(sla),
            verdict: sla.2,
            note: format!("{lowest}{}", unmeasured(sla.1, "not watched")),
        },
        Tile {
            label: "Mean time to recover",
            value: mean.map_or_else(|| "No outages".into(), duration),
            verdict: recovery.2,
            note: format!(
                "{} in the last {span}; {} within its objective.",
                plural(outages, "outage"),
                of(recovery)
            ),
        },
        Tile {
            label: "Recovery time objective met",
            value: of(rto),
            verdict: rto.2,
            note: format!("{slowest}{}", unmeasured(rto.1, "not tested by an outage or a restore")),
        },
        Tile {
            label: "Recovery point objective met",
            value: of(rpo),
            verdict: rpo.2,
            note: format!("{riskiest}{}", unmeasured(rpo.1, "with no backup recorded")),
        },
    ]
}

/// One subject's figures in a row, each with its verdict.
pub struct Row {
    pub title: String,
    pub href: Option<String>,
    pub now: (&'static str, &'static str),
    pub cells: Vec<Cell>,
}

pub struct Cell {
    pub value: String,
    /// None where the objective does not apply.
    pub verdict: Option<Verdict>,
}

/// How a subject stands now: down with an outage open, up or unknown by its checks, or not watched.
pub fn now(judged: &Judged, subject: Option<&Subject>) -> (&'static str, &'static str) {
    match (judged.open, subject) {
        (open, _) if open > 0 => ("Down", "error"),
        (_, Some(held)) if held.state == "up" => ("Up", "ready"),
        (_, Some(held)) if held.url.is_some() || held.kind != "service" => ("Unknown", "unknown"),
        (_, Some(_)) => ("Reported", "unknown"),
        (_, None) => ("Not watched", "unknown"),
    }
}

/// Whether a subject keeps data of its own that backups are for: a service, or DOC's database,
/// which holds every plugin's data too.
pub fn keeps_data(subject: &str) -> bool {
    subject.starts_with("service:") || subject == crate::platform::DATABASE
}

pub fn cells(judged: &Judged) -> Vec<Cell> {
    let recovery = match (judged.worst_recovery(), judged.failed_restores) {
        (_, 1) => "A restore failed".into(),
        (_, failed) if failed > 1 => format!("{failed} restores failed"),
        (Some(worst), _) => duration(worst),
        (None, _) => "Not tested".into(),
    };
    vec![
        Cell {
            value: judged.availability.map_or_else(|| "–".into(), percent),
            verdict: Some(judged.sla),
        },
        Cell {
            value: judged.mttr.map_or_else(|| "No outages".into(), duration),
            verdict: Some(judged.recovery),
        },
        Cell { value: recovery, verdict: Some(judged.rto) },
        match keeps_data(&judged.subject) {
            true => Cell {
                value: judged.exposure.map_or_else(|| "No backups".into(), duration),
                verdict: Some(judged.rpo),
            },
            false => Cell { value: "–".into(), verdict: None },
        },
    ]
}

/// A chart as a canvas carries it: its title, what it shows for someone who cannot see it, and
/// its Chart.js configuration.
#[derive(Debug, Clone)]
pub struct Chart {
    pub title: &'static str,
    pub described: String,
    pub config: String,
}

/// What the points of a chart are: a week each from Monday for a period over a fortnight, a day
/// each down to a day, and a share of an hour below that — always about a dozen points, which is
/// what reads as a line rather than as a scribble or as two dots.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Grain {
    Weekly,
    Daily,
    /// Within a day, drawn from telemetry: the step in seconds.
    Fine(i64),
}

impl Grain {
    /// What one point is called, for "outages a day".
    fn each(self) -> &'static str {
        match self {
            Self::Weekly => "week",
            Self::Daily => "day",
            Self::Fine(step) if step < 3_600 => "five minutes",
            Self::Fine(3_600) => "hour",
            Self::Fine(_) => "two hours",
        }
    }

    fn label(self, at: DateTime<Utc>) -> String {
        match self {
            Self::Weekly => format!("w/c {}", at.format("%-d %b")),
            Self::Daily => at.format("%-d %b").to_string(),
            Self::Fine(_) => at.format("%H:%M").to_string(),
        }
    }
}

/// The period cut into points: weeks from Monday over a fortnight, days down to a day, and
/// within a day a step that leaves about a dozen of them, on the five minutes telemetry is kept
/// in so a point is whole samples rather than parts of them.
fn buckets(period: &Period) -> (Vec<DateTime<Utc>>, Duration, Grain) {
    let seconds = period.seconds();
    let (grain, mut start, step) = match () {
        () if period.days() > 14.0 => {
            let mut start = midnight(period.from);
            start -= Duration::days(i64::from(start.weekday().num_days_from_monday()));
            (Grain::Weekly, start, Duration::weeks(1))
        }
        () if seconds > 86_400 => (Grain::Daily, midnight(period.from), Duration::days(1)),
        () => {
            // A dozen points, rounded up to whole five minutes, so an hour steps by five, six
            // hours by half an hour, twelve by an hour and a day by two.
            let wanted = (seconds as f64 / 12.0 / SAMPLE_SECONDS as f64).ceil().max(1.0) as i64;
            let step = wanted * SAMPLE_SECONDS;
            (Grain::Fine(step), sample_at(period.from), Duration::seconds(step))
        }
    };
    let mut starts = Vec::new();
    while start < period.to {
        starts.push(start);
        start += step;
    }
    (starts, step, grain)
}

/// Availability, outages, time to recover and data at risk, day by day or week by week, for the
/// subjects given with their objectives.
pub fn charts(held: &Held, period: &Period, subjects: &[(String, Targets)]) -> Vec<Chart> {
    let (starts, step, grain) = buckets(period);
    let now = Utc::now();
    let keys: BTreeSet<&str> = subjects.iter().map(|(key, _)| key.as_str()).collect();
    let labels: Vec<String> = starts.iter().map(|start| grain.label(*start)).collect();
    let mut availability = Vec::new();
    let mut outages = Vec::new();
    let mut recovery = Vec::new();
    let mut exposure = Vec::new();
    for start in &starts {
        let part = Period { from: (*start).max(period.from), to: (*start + step).min(period.to) };
        if part.from >= part.to || part.from > now {
            availability.push(None);
            outages.push(0);
            recovery.push(None);
            exposure.push(None);
            continue;
        }
        let judged: Vec<Judged> = subjects
            .iter()
            .map(|(key, targets)| judge(key, key, *targets, held, &part, 0.0))
            .collect();
        let watched: f64 = judged.iter().map(|j| j.watched_for).sum();
        let down: f64 = judged.iter().map(|j| j.downtime).sum();
        availability
            .push((watched > 0.0).then(|| ((1.0 - down / watched) * 100_000.0).round() / 1_000.0));
        outages.push(judged.iter().map(|j| j.outages).sum::<usize>());
        let lasted: Vec<f64> = held
            .outages
            .iter()
            .filter(|o| keys.contains(o.subject.as_str()) && part.contains(o.started_at))
            .map(|o| o.lasted(now) / 60.0)
            .collect();
        recovery.push(
            (!lasted.is_empty())
                .then(|| (lasted.iter().sum::<f64>() / lasted.len() as f64 * 10.0).round() / 10.0),
        );
        exposure.push(
            judged
                .iter()
                .filter_map(|j| j.exposure)
                .reduce(f64::max)
                .map(|gap| (gap / 3_600.0 * 10.0).round() / 10.0),
        );
    }
    let each = grain.each();
    let options = |unit: &str, stacked: bool| {
        json!({
            "animation": false,
            "interaction": { "mode": "index", "intersect": false },
            "scales": { "y": {
                "beginAtZero": !unit.starts_with("percent"),
                "ticks": if stacked { json!({ "precision": 0 }) } else { json!({}) },
                "title": { "display": true, "text": unit },
            } },
            "plugins": { "legend": { "position": "bottom" } },
        })
    };
    let line = |label: &str, data: Value, colour: &str, dashed: bool| {
        json!({
            "label": label, "data": data, "borderColor": colour, "backgroundColor": colour,
            "spanGaps": true, "tension": 0, "pointRadius": if dashed { 0 } else { 2 },
            "borderDash": if dashed { json!([4, 4]) } else { json!([]) },
        })
    };
    // With one subject, its objective is drawn beside it.
    let objective = |value: f64| json!(vec![value; starts.len()]);
    let only = (subjects.len() == 1).then(|| subjects[0].1);
    let mut availability_lines = vec![line("Availability", json!(availability), PRIMARY, false)];
    let mut exposure_lines = vec![line("Most data at risk", json!(exposure), PRIMARY, false)];
    if let Some(targets) = only {
        availability_lines.push(line("Objective", objective(targets.sla), SECONDARY, true));
        exposure_lines.push(line(
            "Objective",
            objective((targets.rpo / 3_600.0 * 10.0).round() / 10.0),
            SECONDARY,
            true,
        ));
    }
    vec![
        Chart {
            title: "Availability",
            described: format!("The percentage of the time it was up each {each}"),
            config: json!({
                "type": "line",
                "data": { "labels": labels, "datasets": availability_lines },
                "options": options("percent up", false),
            })
            .to_string(),
        },
        Chart {
            title: "Outages",
            described: format!("How many outages started each {each}"),
            config: json!({
                "type": "bar",
                "data": { "labels": labels, "datasets": [{ "label": "Outages", "data": outages, "backgroundColor": FAILED }] },
                "options": options("outages", true),
            })
            .to_string(),
        },
        Chart {
            title: "Time to recover",
            described: format!("The mean time the outages that started each {each} took to recover, in minutes"),
            config: json!({
                "type": "line",
                "data": { "labels": labels, "datasets": [line("Mean", json!(recovery), FAILED, false)] },
                "options": options("minutes", false),
            })
            .to_string(),
        },
        Chart {
            title: "Data at risk",
            described: format!("The most time between two backups each {each}, in hours: what a failure then would have lost"),
            config: json!({
                "type": "line",
                "data": { "labels": labels, "datasets": exposure_lines },
                "options": options("hours", false),
            })
            .to_string(),
        },
    ]
}
