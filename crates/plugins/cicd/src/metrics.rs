//! The four figures over a period — success rate, duration, time to recover and runs that passed
//! only when re-run — for every stage together and each on its own, with their bands, the period
//! before to compare with, and their trends. Worked out from the daily figures and broken spells
//! in the period, so a median is a median of runs rather than of days.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Datelike, Duration, Utc};
use doc_plugin_sdk::{Backend, Order, Query};
use serde::Serialize;
use serde_json::{Value, json};

use crate::Refusal;
use crate::compute::{median, midnight, percentile};
use crate::settings::{Bands, Definitions, Stage};
use crate::store::{Day, Recovery};

const DAY: f64 = 86_400.0;
/// Repositories asked for in one query.
const IN_ONE: usize = 200;
const PRIMARY: &str = "#5b00b8";
const FAILED: &str = "#d5281b";
const SECONDARY: &str = "#768692";
const TESTING: &str = "#007f3b";

/// From `from`, up to but not including `to`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Period {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

impl Period {
    pub fn last(days: i64) -> Self {
        let to = Utc::now();
        Self { from: to - Duration::days(days), to }
    }

    pub fn days(&self) -> f64 {
        ((self.to - self.from).num_seconds() as f64 / DAY).max(1.0 / 24.0)
    }

    /// The same length of time just before it, to compare with.
    pub fn before(&self) -> Self {
        Self { from: self.from - (self.to - self.from), to: self.from }
    }

    pub fn contains(&self, at: DateTime<Utc>) -> bool {
        self.from <= at && at < self.to
    }

    /// The first midnight in it: a day's figures count in the period its midnight is in, so no day
    /// counts twice in a period and the one before.
    pub fn first_day(&self) -> DateTime<Utc> {
        let day = midnight(self.from);
        if day < self.from { day + Duration::days(1) } else { day }
    }

    pub fn holds_day(&self, day: DateTime<Utc>) -> bool {
        self.first_day() <= day && day < self.to
    }

    /// How it is said on a page: "30 days".
    pub fn said(&self) -> String {
        let days = self.days().round() as i64;
        match days {
            1 => "day".to_string(),
            365 | 366 => "year".to_string(),
            days => format!("{days} days"),
        }
    }
}

/// What the figures of a period are worked out from.
#[derive(Debug, Clone, Default)]
pub struct Held {
    pub days: Vec<Day>,
    pub recoveries: Vec<Recovery>,
}

/// The daily figures and broken spells of these repositories in the period; `None` is every
/// repository. With faux data, `faux-data`'s runs worked out here instead, never read or written.
pub async fn held(
    backend: &Backend,
    repositories: Option<&BTreeSet<String>>,
    period: &Period,
) -> Result<Held, Refusal> {
    if crate::faux::on(backend) {
        let definitions = Definitions::read(&backend.settings());
        let repositories = repositories.cloned().unwrap_or_default();
        return crate::faux::held(backend, &repositories, period, &definitions).await;
    }
    let days = json!({ "gte": period.first_day(), "lt": period.to });
    let broke = json!({ "gte": period.from, "lt": period.to });
    let Some(repositories) = repositories else {
        return Ok(Held {
            days: backend.query_all(Query::new("days").filter(json!({ "day": days }))).await?,
            recoveries: backend
                .query_all(
                    Query::new("recoveries")
                        .filter(json!({ "broke_at": broke }))
                        .order(Order::asc("broke_at")),
                )
                .await?,
        });
    };
    let names: Vec<&String> = repositories.iter().collect();
    let mut found = Held::default();
    for chunk in names.chunks(IN_ONE) {
        let filter = json!({ "repository": { "in": chunk }, "day": days });
        found.days.extend(backend.query_all::<Day>(Query::new("days").filter(filter)).await?);
        let filter = json!({ "repository": { "in": chunk }, "broke_at": broke });
        found
            .recoveries
            .extend(backend.query_all::<Recovery>(Query::new("recoveries").filter(filter)).await?);
    }
    found.recoveries.sort_by_key(|spell| spell.broke_at);
    Ok(found)
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Spread {
    pub median: f64,
    pub p90: f64,
    pub count: usize,
}

fn spread(values: &[f64]) -> Option<Spread> {
    Some(Spread { median: median(values)?, p90: percentile(values, 90.0)?, count: values.len() })
}

/// The four figures over one period, for every stage or one.
#[derive(Debug, Clone, Serialize)]
pub struct Figures {
    pub runs: i64,
    pub succeeded: i64,
    pub failed: i64,
    pub cancelled: i64,
    pub per_day: f64,
    /// Percent of the runs that passed or failed which passed.
    pub success_rate: Option<f64>,
    /// Seconds, over the runs that passed.
    pub duration: Option<Spread>,
    pub reruns: i64,
    /// Percent of the runs that passed which needed another attempt.
    pub rerun_rate: Option<f64>,
    /// Times a workflow broke on the default branch in the period.
    pub broke: usize,
    /// Seconds, over those that were fixed.
    pub recovery: Option<Spread>,
    pub still_broken: usize,
}

/// Which of a period's figures count: every stage or one, and the default branch or every branch.
#[derive(Debug, Clone, Copy)]
pub struct Counting {
    pub default_only: bool,
    pub stage: Option<Stage>,
}

impl Counting {
    pub fn of(definitions: &Definitions) -> Self {
        Self { default_only: definitions.default_only, stage: None }
    }

    pub fn stage(self, stage: Stage) -> Self {
        Self { stage: Some(stage), ..self }
    }

    fn day(&self, day: &Day) -> bool {
        (!self.default_only || day.default_branch)
            && self.stage.is_none_or(|stage| day.stage == stage.key())
    }

    fn spell(&self, spell: &Recovery) -> bool {
        self.stage.is_none_or(|stage| spell.stage == stage.key())
    }
}

pub fn figures(held: &Held, period: &Period, counting: Counting) -> Figures {
    let days: Vec<&Day> = held.days.iter().filter(|day| counting.day(day)).collect();
    let spells: Vec<&Recovery> =
        held.recoveries.iter().filter(|spell| counting.spell(spell)).collect();
    let sum = |each: fn(&Day) -> i64| days.iter().map(|day| each(day)).sum::<i64>();
    let (runs, succeeded, failed) = (sum(|d| d.runs), sum(|d| d.succeeded), sum(|d| d.failed));
    let reruns = sum(|d| d.reruns);
    let durations: Vec<f64> = days.iter().flat_map(|day| day.durations.iter().copied()).collect();
    let recoveries: Vec<f64> = spells.iter().filter_map(|spell| spell.seconds).collect();
    let ended = succeeded + failed;
    Figures {
        runs,
        succeeded,
        failed,
        cancelled: sum(|d| d.cancelled),
        per_day: runs as f64 / period.days(),
        success_rate: (ended > 0).then(|| succeeded as f64 / ended as f64 * 100.0),
        duration: spread(&durations),
        reruns,
        rerun_rate: (succeeded > 0).then(|| reruns as f64 / succeeded as f64 * 100.0),
        broke: spells.len(),
        recovery: spread(&recoveries),
        still_broken: spells.iter().filter(|spell| spell.fixed_at.is_none()).count(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Band {
    Elite,
    High,
    Medium,
    Low,
}

impl Band {
    pub fn word(self) -> &'static str {
        match self {
            Self::Elite => "Elite",
            Self::High => "High",
            Self::Medium => "Medium",
            Self::Low => "Low",
        }
    }

    /// The badge it is shown with: its meaning, since colour alone says nothing.
    pub fn badge(self) -> &'static str {
        match self {
            Self::Elite | Self::High => "ready",
            Self::Medium => "degraded",
            Self::Low => "error",
        }
    }

    fn at_least(value: f64, bands: [f64; 3]) -> Self {
        match value {
            value if value >= bands[0] => Self::Elite,
            value if value >= bands[1] => Self::High,
            value if value >= bands[2] => Self::Medium,
            _ => Self::Low,
        }
    }

    fn at_most(value: f64, bands: [f64; 3]) -> Self {
        match value {
            value if value <= bands[0] => Self::Elite,
            value if value <= bands[1] => Self::High,
            value if value <= bands[2] => Self::Medium,
            _ => Self::Low,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Banded {
    pub success_rate: Option<Band>,
    pub duration: Option<Band>,
    pub recovery: Option<Band>,
    pub reruns: Option<Band>,
}

impl Figures {
    pub fn bands(&self, bands: &Bands) -> Banded {
        Banded {
            success_rate: self.success_rate.map(|rate| Band::at_least(rate, bands.success)),
            duration: self.duration.map(|spread| Band::at_most(spread.median, bands.duration)),
            recovery: self.recovery.map(|spread| Band::at_most(spread.median, bands.recovery)),
            reruns: self.rerun_rate.map(|rate| Band::at_most(rate, bands.reruns)),
        }
    }

    /// The whole of it as the API and agents are given it.
    pub fn json(&self, bands: &Bands) -> Value {
        let spread = |spread: Option<Spread>, counted: &str| {
            spread.map(|s| json!({ "median_seconds": s.median.round(), "p90_seconds": s.p90.round(), counted: s.count }))
        };
        json!({
            "runs": self.runs,
            "runs_per_day": rounded(self.per_day),
            "succeeded": self.succeeded,
            "failed": self.failed,
            "cancelled": self.cancelled,
            "success_rate_percent": self.success_rate.map(rounded),
            "duration": spread(self.duration, "runs"),
            "passed_on_a_rerun": self.reruns,
            "rerun_rate_percent": self.rerun_rate.map(rounded),
            "broke": self.broke,
            "time_to_recover": spread(self.recovery, "recovered"),
            "still_broken": self.still_broken,
            "bands": self.bands(bands),
        })
    }
}

pub fn rounded(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn one_place(value: f64) -> String {
    let text = format!("{value:.1}");
    text.strip_suffix(".0").map(str::to_string).unwrap_or(text)
}

/// A span of time as people say it: `3d 4h`, `5h 12m`, `8m 30s`, `42s`.
pub fn duration(seconds: f64) -> String {
    let total = seconds.max(0.0).round() as i64;
    let (days, hours, minutes, left) =
        (total / 86_400, total % 86_400 / 3_600, total % 3_600 / 60, total % 60);
    match (days, hours, minutes) {
        (0, 0, 0) => format!("{total}s"),
        (0, 0, minutes) if minutes < 10 && left > 0 => format!("{minutes}m {left}s"),
        (0, 0, minutes) => format!("{minutes}m"),
        (0, hours, 0) => format!("{hours}h"),
        (0, hours, minutes) => format!("{hours}h {minutes}m"),
        (days, 0, _) => format!("{days}d"),
        (days, hours, _) => format!("{days}d {hours}h"),
    }
}

pub fn percent(rate: f64) -> String {
    format!("{}%", one_place(rate))
}

/// How many runs a day, a week or a month, whichever reads best.
pub fn often(figures: &Figures) -> String {
    match figures.runs {
        0 => "no runs".to_string(),
        _ if figures.per_day >= 1.0 => format!("{} runs a day", one_place(figures.per_day)),
        _ if figures.per_day * 7.0 >= 1.0 => {
            format!("{} runs a week", one_place(figures.per_day * 7.0))
        }
        _ => format!("{} runs a month", one_place(figures.per_day * 30.0)),
    }
}

pub fn plural(count: usize, word: &str) -> String {
    match (count, word) {
        (1, word) => format!("1 {word}"),
        (count, "recovery") => format!("{count} recoveries"),
        (count, word) => format!("{count} {word}s"),
    }
}

/// One headline number, its band and what it was before.
#[derive(Debug, Clone)]
pub struct Tile {
    pub label: &'static str,
    pub value: String,
    pub band: Option<Band>,
    pub note: String,
}

pub fn recovered(figures: &Figures) -> String {
    match (figures.recovery, figures.broke) {
        (Some(recovery), _) => duration(recovery.median),
        (None, 0) => "No failure".into(),
        (None, _) => "Not yet".into(),
    }
}

pub fn tiles(now: &Figures, before: &Figures, period: &Period, bands: &Bands) -> Vec<Tile> {
    let banded = now.bands(bands);
    let span = period.said();
    let was = |text: Option<String>| {
        text.map(|text| format!(" The {span} before: {text}.")).unwrap_or_default()
    };
    vec![
        Tile {
            label: "Success rate",
            value: now.success_rate.map_or_else(|| "No runs".into(), percent),
            band: banded.success_rate,
            note: match now.succeeded + now.failed {
                0 => format!("Nothing passed or failed in the last {span}."),
                ended => format!(
                    "{} of {ended} passed; {}.{}",
                    now.succeeded,
                    often(now),
                    was(before.success_rate.map(percent))
                ),
            },
        },
        Tile {
            label: "Duration",
            value: now.duration.map_or_else(|| "None yet".into(), |spread| duration(spread.median)),
            band: banded.duration,
            note: match now.duration {
                Some(spread) => format!(
                    "Median of {} that passed; 90th percentile {}.{}",
                    plural(spread.count, "run"),
                    duration(spread.p90),
                    was(before.duration.map(|spread| duration(spread.median)))
                ),
                None => "No run passed to time.".into(),
            },
        },
        Tile {
            label: "Time to recover",
            value: recovered(now),
            band: banded.recovery,
            note: match (now.recovery, now.still_broken) {
                (Some(recovery), 0) => format!(
                    "Median of {}, from a default branch breaking to passing again.{}",
                    plural(recovery.count, "recovery"),
                    was(before.recovery.map(|recovery| duration(recovery.median)))
                ),
                (Some(recovery), open) => format!(
                    "Median of {}; {open} still broken.",
                    plural(recovery.count, "recovery")
                ),
                (None, 0) => "No workflow failed on a default branch.".into(),
                (None, open) => format!("{} still broken.", plural(open, "workflow")),
            },
        },
        Tile {
            label: "Passed on a re-run",
            value: now.rerun_rate.map_or_else(|| "None yet".into(), percent),
            band: banded.reruns,
            note: match now.succeeded {
                0 => "No run passed.".into(),
                passed => format!(
                    "{} of {passed} passed only when run again, which is often a flaky test.{}",
                    now.reruns,
                    was(before.rerun_rate.map(percent))
                ),
            },
        },
    ]
}

/// A stage's four figures in a row, each with its band, or a service's or team's.
pub struct Row {
    pub title: String,
    pub href: Option<String>,
    pub runs: String,
    pub cells: Vec<Cell>,
}

pub struct Cell {
    pub value: String,
    pub band: Option<Band>,
}

pub fn cells(figures: &Figures, bands: &Bands) -> Vec<Cell> {
    let banded = figures.bands(bands);
    let dash = || "–".to_string();
    vec![
        Cell { value: figures.success_rate.map_or_else(dash, percent), band: banded.success_rate },
        Cell {
            value: figures.duration.map_or_else(dash, |spread| duration(spread.median)),
            band: banded.duration,
        },
        Cell {
            value: match figures.runs {
                0 => dash(),
                _ => recovered(figures),
            },
            band: banded.recovery,
        },
        Cell { value: figures.rerun_rate.map_or_else(dash, percent), band: banded.reruns },
    ]
}

/// Each stage's row, for a stage that ran at all.
pub fn stages(held: &Held, period: &Period, definitions: &Definitions) -> Vec<Row> {
    Stage::ALL
        .into_iter()
        .filter_map(|stage| {
            let figures = figures(held, period, Counting::of(definitions).stage(stage));
            (figures.runs > 0 || figures.broke > 0).then(|| Row {
                title: format!("{} ({})", stage.word(), stage.short()),
                href: None,
                runs: figures.runs.to_string(),
                cells: cells(&figures, &definitions.bands),
            })
        })
        .collect()
}

/// One workflow of one repository over the period.
pub struct Workflow {
    pub repository: String,
    pub workflow: String,
    pub stage: Stage,
    pub figures: Figures,
    pub broken: bool,
}

/// Every workflow that ran, the ones failing most first.
pub fn workflows(held: &Held, period: &Period, definitions: &Definitions) -> Vec<Workflow> {
    let mut grouped: BTreeMap<(String, i64), Held> = BTreeMap::new();
    for day in &held.days {
        let key = (day.repository.clone(), day.workflow_id);
        grouped.entry(key).or_default().days.push(day.clone());
    }
    for spell in &held.recoveries {
        let key = (spell.repository.clone(), spell.workflow_id);
        grouped.entry(key).or_default().recoveries.push(spell.clone());
    }
    let counting = Counting::of(definitions);
    let mut found: Vec<Workflow> = grouped
        .into_iter()
        .filter_map(|((repository, _), theirs)| {
            let named = theirs.days.last().map(|day| (day.workflow.clone(), day.stage.clone()));
            let named = named.or_else(|| {
                theirs.recoveries.last().map(|spell| (spell.workflow.clone(), spell.stage.clone()))
            });
            let (workflow, stage) = named?;
            let figures = figures(&theirs, period, counting);
            (figures.runs > 0 || figures.broke > 0).then(|| Workflow {
                repository,
                workflow,
                stage: Stage::from_key(&stage).unwrap_or(Stage::Ci),
                broken: figures.still_broken > 0,
                figures,
            })
        })
        .collect();
    found.sort_by(|one, two| {
        (two.broken, two.figures.failed, two.figures.runs).cmp(&(
            one.broken,
            one.figures.failed,
            one.figures.runs,
        ))
    });
    found
}

/// A chart as a canvas carries it: its title, what it shows for someone who cannot see it, and
/// its Chart.js configuration.
#[derive(Debug, Clone)]
pub struct Chart {
    pub title: &'static str,
    pub described: String,
    pub config: String,
}

/// The period cut into days, or into weeks from Monday for anything over a fortnight.
fn buckets(period: &Period) -> (Vec<DateTime<Utc>>, Duration, bool) {
    let weekly = period.days() > 14.0;
    let mut start = period.first_day();
    let step = match weekly {
        true => {
            start -= Duration::days(i64::from(start.weekday().num_days_from_monday()));
            Duration::weeks(1)
        }
        false => Duration::days(1),
    };
    let mut starts = Vec::new();
    while start < period.to {
        starts.push(start);
        start += step;
    }
    (starts, step, weekly)
}

fn in_hours(seconds: Option<f64>) -> Option<f64> {
    seconds.map(|seconds| (seconds / 3_600.0 * 10.0).round() / 10.0)
}

fn in_minutes(seconds: Option<f64>) -> Option<f64> {
    seconds.map(|seconds| (seconds / 60.0 * 10.0).round() / 10.0)
}

/// Runs by how they ended, success rate and duration by stage, and time to recover.
pub fn charts(held: &Held, period: &Period, definitions: &Definitions) -> Vec<Chart> {
    let (starts, step, weekly) = buckets(period);
    let mut grouped: Vec<Held> = vec![Held::default(); starts.len()];
    let index = |at: DateTime<Utc>| {
        starts.iter().rposition(|start| *start <= at).filter(|found| at < starts[*found] + step)
    };
    for day in &held.days {
        if let Some(found) = index(day.day) {
            grouped[found].days.push(day.clone());
        }
    }
    for spell in &held.recoveries {
        if let Some(found) = index(spell.broke_at) {
            grouped[found].recoveries.push(spell.clone());
        }
    }
    let part = Period { from: period.from, to: period.from + step };
    let counting = Counting::of(definitions);
    let every: Vec<Figures> = grouped.iter().map(|g| figures(g, &part, counting)).collect();
    let by_stage = |stage: Stage| -> Vec<Figures> {
        grouped.iter().map(|g| figures(g, &part, counting.stage(stage))).collect()
    };
    let labels: Vec<String> = starts
        .iter()
        .map(|start| match weekly {
            true => format!("w/c {}", start.format("%-d %b")),
            false => start.format("%-d %b").to_string(),
        })
        .collect();
    let each = if weekly { "week" } else { "day" };
    let options = |unit: &str, stacked: bool| {
        json!({
            "animation": false,
            "interaction": { "mode": "index", "intersect": false },
            "scales": {
                "x": { "stacked": stacked },
                "y": {
                    "stacked": stacked, "beginAtZero": true,
                    "ticks": if stacked { json!({ "precision": 0 }) } else { json!({}) },
                    "title": { "display": true, "text": unit },
                },
            },
            "plugins": { "legend": { "position": "bottom" } },
        })
    };
    let line = |label: &str, data: Vec<Option<f64>>, colour: &str, dashed: bool| {
        json!({
            "label": label, "data": data, "borderColor": colour, "backgroundColor": colour,
            "spanGaps": true, "tension": 0, "pointRadius": 2,
            "borderDash": if dashed { json!([4, 4]) } else { json!([]) },
        })
    };
    let colours =
        [(Stage::Ci, PRIMARY, false), (Stage::Cd, SECONDARY, true), (Stage::Ct, TESTING, false)];
    let staged = |value: fn(&Figures) -> Option<f64>| -> Vec<Value> {
        colours
            .iter()
            .map(|(stage, colour, dashed)| {
                let data = by_stage(*stage).iter().map(value).collect();
                line(stage.word(), data, colour, *dashed)
            })
            .collect()
    };
    let passed: Vec<i64> = every.iter().map(|f| f.succeeded).collect();
    let failed: Vec<i64> = every.iter().map(|f| f.failed).collect();
    let cancelled: Vec<i64> = every.iter().map(|f| f.cancelled).collect();
    let recovery: Vec<Option<f64>> =
        every.iter().map(|f| in_hours(f.recovery.map(|s| s.median))).collect();
    vec![
        Chart {
            title: "Runs",
            described: format!("Workflow runs each {each}, by whether they passed, failed or were cancelled"),
            config: json!({
                "type": "bar",
                "data": { "labels": labels, "datasets": [
                    { "label": "Passed", "data": passed, "backgroundColor": PRIMARY },
                    { "label": "Failed", "data": failed, "backgroundColor": FAILED },
                    { "label": "Cancelled", "data": cancelled, "backgroundColor": SECONDARY },
                ]},
                "options": options("runs", true),
            })
            .to_string(),
        },
        Chart {
            title: "Success rate",
            described: format!("The percentage of runs that passed each {each}, for integration, delivery and testing"),
            config: json!({
                "type": "line",
                "data": { "labels": labels, "datasets": staged(|f| f.success_rate.map(rounded)) },
                "options": options("percent", false),
            })
            .to_string(),
        },
        Chart {
            title: "Duration",
            described: format!("The median time a run that passed took each {each}, in minutes, for integration, delivery and testing"),
            config: json!({
                "type": "line",
                "data": { "labels": labels, "datasets": staged(|f| in_minutes(f.duration.map(|s| s.median))) },
                "options": options("minutes", false),
            })
            .to_string(),
        },
        Chart {
            title: "Time to recover",
            described: format!("The median time from a default branch breaking to passing again, by the {each} it broke, in hours"),
            config: json!({
                "type": "line",
                "data": { "labels": labels, "datasets": [line("Median", recovery, FAILED, false)] },
                "options": options("hours", false),
            })
            .to_string(),
        },
    ]
}
