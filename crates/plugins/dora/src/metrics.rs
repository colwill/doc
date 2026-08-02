//! The four metrics over a period, each with its band and the period before it to compare with,
//! and their trend day by day or week by week. Worked out from the deployments in the period
//! rather than from daily sums, so a median is a real median and every figure can be traced to
//! the deployments behind it.

use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, Duration, TimeZone, Utc};
use doc_plugin_sdk::{Backend, Order, Query};
use serde::Serialize;
use serde_json::{Value, json};

use crate::Refusal;
use crate::compute::{median, percentile};
use crate::settings::Bands;
use crate::store::Deployment;

const DAY: f64 = 86_400.0;
/// Repositories asked for in one query.
const IN_ONE: usize = 200;
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

/// The deployments of these repositories in the period, oldest first; `None` is every repository.
/// With faux data, `faux-data`'s delivery worked out here instead, never read or written.
pub async fn deployments(
    backend: &Backend,
    repositories: Option<&BTreeSet<String>>,
    period: &Period,
) -> Result<Vec<Deployment>, Refusal> {
    if crate::faux::on(backend) {
        let definitions = crate::settings::Definitions::read(&backend.settings());
        let repositories = repositories.cloned().unwrap_or_default();
        return crate::faux::deployments(backend, &repositories, period, &definitions).await;
    }
    let window = json!({ "gte": period.from, "lt": period.to });
    let asked =
        |filter: Value| Query::new("deployments").filter(filter).order(Order::asc("deployed_at"));
    let Some(repositories) = repositories else {
        return Ok(backend.query_all(asked(json!({ "deployed_at": window }))).await?);
    };
    let names: Vec<&String> = repositories.iter().collect();
    let mut found: Vec<Deployment> = Vec::new();
    for chunk in names.chunks(IN_ONE) {
        let filter = json!({ "repository": { "in": chunk }, "deployed_at": window });
        found.extend(backend.query_all::<Deployment>(asked(filter)).await?);
    }
    found.sort_by_key(|deployment| deployment.deployed_at);
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

/// The four metrics over one period.
#[derive(Debug, Clone, Serialize)]
pub struct Figures {
    pub deployments: usize,
    pub per_week: f64,
    /// Seconds, over every change shipped in the period.
    pub lead_time: Option<Spread>,
    pub failed: usize,
    /// Percent of deployments that caused a failure.
    pub change_fail_rate: Option<f64>,
    /// Seconds, over the failures that have recovered.
    pub recovery: Option<Spread>,
    pub unrecovered: usize,
}

pub fn figures(deployments: &[Deployment], period: &Period) -> Figures {
    let count = deployments.len();
    let leads: Vec<f64> = deployments.iter().flat_map(|d| d.lead_times.iter().copied()).collect();
    let failed = deployments.iter().filter(|d| d.failed).count();
    let recoveries: Vec<f64> = deployments.iter().filter_map(|d| d.recovery_seconds).collect();
    Figures {
        deployments: count,
        per_week: count as f64 / period.days() * 7.0,
        lead_time: spread(&leads),
        failed,
        change_fail_rate: (count > 0).then(|| failed as f64 / count as f64 * 100.0),
        recovery: spread(&recoveries),
        unrecovered: deployments.iter().filter(|d| d.failed && d.recovered_at.is_none()).count(),
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
    pub deployment_frequency: Option<Band>,
    pub lead_time: Option<Band>,
    pub change_fail_rate: Option<Band>,
    pub recovery: Option<Band>,
}

impl Figures {
    pub fn bands(&self, bands: &Bands) -> Banded {
        Banded {
            deployment_frequency: (self.deployments > 0)
                .then(|| Band::at_least(self.per_week, bands.frequency)),
            lead_time: self.lead_time.map(|lead| Band::at_most(lead.median, bands.lead)),
            change_fail_rate: self.change_fail_rate.map(|rate| Band::at_most(rate, bands.fail)),
            recovery: self.recovery.map(|recovery| Band::at_most(recovery.median, bands.recovery)),
        }
    }

    /// The whole of it as the API and agents are given it.
    pub fn json(&self, bands: &Bands) -> Value {
        json!({
            "deployments": self.deployments,
            "deployments_per_week": rounded(self.per_week),
            "lead_time": self.lead_time.map(|s| json!({ "median_seconds": s.median.round(), "p90_seconds": s.p90.round(), "changes": s.count })),
            "failed_deployments": self.failed,
            "change_fail_rate_percent": self.change_fail_rate.map(rounded),
            "recovery": self.recovery.map(|s| json!({ "median_seconds": s.median.round(), "p90_seconds": s.p90.round(), "recovered": s.count })),
            "not_yet_recovered": self.unrecovered,
            "bands": self.bands(bands),
        })
    }
}

fn rounded(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

fn one_place(value: f64) -> String {
    let text = format!("{value:.1}");
    text.strip_suffix(".0").map(str::to_string).unwrap_or(text)
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

/// How often, in whichever unit reads best: a day, a week or a month.
pub fn frequency(figures: &Figures) -> String {
    let per_day = figures.per_week / 7.0;
    match figures.deployments {
        0 => "None".to_string(),
        _ if per_day >= 1.0 => format!("{} a day", one_place(per_day)),
        _ if figures.per_week >= 1.0 => format!("{} a week", one_place(figures.per_week)),
        _ => format!("{} a month", one_place(per_day * 30.0)),
    }
}

pub fn percent(rate: f64) -> String {
    format!("{}%", one_place(rate))
}

/// One headline number, its band and what it was before.
#[derive(Debug, Clone)]
pub struct Tile {
    pub label: &'static str,
    pub value: String,
    pub band: Option<Band>,
    pub note: String,
}

pub fn tiles(now: &Figures, before: &Figures, period: &Period, bands: &Bands) -> Vec<Tile> {
    let banded = now.bands(bands);
    let span = period.said();
    let was = |text: Option<String>| {
        text.map(|text| format!(" The {span} before: {text}.")).unwrap_or_default()
    };
    vec![
        Tile {
            label: "Deployment frequency",
            value: frequency(now),
            band: banded.deployment_frequency,
            note: format!(
                "{} to production in the last {span}.{}",
                plural(now.deployments, "deployment"),
                was((before.deployments > 0).then(|| frequency(before)))
            ),
        },
        Tile {
            label: "Change lead time",
            value: now.lead_time.map_or_else(|| "None yet".into(), |lead| duration(lead.median)),
            band: banded.lead_time,
            note: match now.lead_time {
                Some(lead) => format!(
                    "Median of {}; 90th percentile {}.{}",
                    plural(lead.count, "change"),
                    duration(lead.p90),
                    was(before.lead_time.map(|lead| duration(lead.median)))
                ),
                None => "Nothing shipped with its commits known.".into(),
            },
        },
        Tile {
            label: "Change fail rate",
            value: now.change_fail_rate.map_or_else(|| "None yet".into(), percent),
            band: banded.change_fail_rate,
            note: match now.deployments {
                0 => "No deployments to fail.".into(),
                count => format!(
                    "{} of {count} caused a failure.{}",
                    now.failed,
                    was(before.change_fail_rate.map(percent))
                ),
            },
        },
        Tile {
            label: "Failed deployment recovery time",
            value: match (now.recovery, now.failed) {
                (Some(recovery), _) => duration(recovery.median),
                (None, 0) => "No failures".into(),
                (None, _) => "Not yet".into(),
            },
            band: banded.recovery,
            note: match (now.recovery, now.unrecovered) {
                (Some(recovery), 0) => format!(
                    "Median of {}.{}",
                    plural(recovery.count, "recovery"),
                    was(before.recovery.map(|recovery| duration(recovery.median)))
                ),
                (Some(recovery), open) => format!(
                    "Median of {}; {open} not recovered yet.",
                    plural(recovery.count, "recovery")
                ),
                (None, 0) => "Nothing to recover from.".into(),
                (None, open) => format!("{} not recovered yet.", plural(open, "failure")),
            },
        },
    ]
}

pub fn plural(count: usize, word: &str) -> String {
    match (count, word) {
        (1, word) => format!("1 {word}"),
        (count, "recovery") => format!("{count} recoveries"),
        (count, word) => format!("{count} {word}s"),
    }
}

/// A chart as a canvas carries it: its title, what it shows for someone who cannot see it, and
/// its Chart.js configuration.
#[derive(Debug, Clone)]
pub struct Chart {
    pub title: &'static str,
    pub described: String,
    pub config: String,
}

/// The period cut into days, or into weeks from Monday for anything over a fortnight: a day's
/// fail rate is all or nothing, and says little.
fn buckets(period: &Period) -> (Vec<DateTime<Utc>>, Duration, bool) {
    let weekly = period.days() > 14.0;
    let day = |at: DateTime<Utc>| {
        Utc.with_ymd_and_hms(at.year(), at.month(), at.day(), 0, 0, 0).single().unwrap_or(at)
    };
    let mut start = day(period.from);
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

fn hours(seconds: Option<f64>) -> Option<f64> {
    seconds.map(|seconds| (seconds / 3_600.0 * 10.0).round() / 10.0)
}

/// The four trend charts: deployments (and those that failed), lead time, fail rate and recovery.
pub fn charts(deployments: &[Deployment], period: &Period) -> Vec<Chart> {
    let (starts, step, weekly) = buckets(period);
    let mut grouped: Vec<Vec<&Deployment>> = vec![Vec::new(); starts.len()];
    for deployment in deployments {
        if let Some(index) = starts.iter().rposition(|start| *start <= deployment.deployed_at)
            && deployment.deployed_at < starts[index] + step
        {
            grouped[index].push(deployment);
        }
    }
    let labels: Vec<String> = starts
        .iter()
        .map(|start| match weekly {
            true => format!("w/c {}", start.format("%-d %b")),
            false => start.format("%-d %b").to_string(),
        })
        .collect();
    let fine: Vec<usize> = grouped.iter().map(|g| g.iter().filter(|d| !d.failed).count()).collect();
    let failed: Vec<usize> =
        grouped.iter().map(|g| g.iter().filter(|d| d.failed).count()).collect();
    let leads = |g: &Vec<&Deployment>| -> Vec<f64> {
        g.iter().flat_map(|d| d.lead_times.iter().copied()).collect()
    };
    let lead_median: Vec<Option<f64>> = grouped.iter().map(|g| hours(median(&leads(g)))).collect();
    let lead_p90: Vec<Option<f64>> =
        grouped.iter().map(|g| hours(percentile(&leads(g), 90.0))).collect();
    let rate: Vec<Option<f64>> = grouped
        .iter()
        .map(|g| {
            (!g.is_empty()).then(|| {
                rounded(g.iter().filter(|d| d.failed).count() as f64 / g.len() as f64 * 100.0)
            })
        })
        .collect();
    let recovery: Vec<Option<f64>> = grouped
        .iter()
        .map(|g| hours(median(&g.iter().filter_map(|d| d.recovery_seconds).collect::<Vec<_>>())))
        .collect();
    let each = if weekly { "week" } else { "day" };
    // Deployments are counted, so their axis is in whole numbers.
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
    let line = |label: &str, data: &Vec<Option<f64>>, colour: &str, dashed: bool| {
        json!({
            "label": label, "data": data, "borderColor": colour, "backgroundColor": colour,
            "spanGaps": true, "tension": 0, "pointRadius": 2,
            "borderDash": if dashed { json!([4, 4]) } else { json!([]) },
        })
    };
    vec![
        Chart {
            title: "Deployments",
            described: format!("Deployments to production each {each}, and how many of them caused a failure"),
            config: json!({
                "type": "bar",
                "data": { "labels": labels, "datasets": [
                    { "label": "Deployed", "data": fine, "backgroundColor": PRIMARY },
                    { "label": "Caused a failure", "data": failed, "backgroundColor": FAILED },
                ]},
                "options": options("deployments", true),
            })
            .to_string(),
        },
        Chart {
            title: "Change lead time",
            described: format!("The median and 90th percentile change lead time each {each}, in hours"),
            config: json!({
                "type": "line",
                "data": { "labels": labels, "datasets": [
                    line("Median", &lead_median, PRIMARY, false),
                    line("90th percentile", &lead_p90, SECONDARY, true),
                ]},
                "options": options("hours", false),
            })
            .to_string(),
        },
        Chart {
            title: "Change fail rate",
            described: format!("The percentage of deployments that caused a failure each {each}"),
            config: json!({
                "type": "line",
                "data": { "labels": labels, "datasets": [line("Change fail rate", &rate, FAILED, false)] },
                "options": options("percent", false),
            })
            .to_string(),
        },
        Chart {
            title: "Failed deployment recovery time",
            described: format!("The median time to recover from a failed deployment each {each}, in hours"),
            config: json!({
                "type": "line",
                "data": { "labels": labels, "datasets": [line("Median", &recovery, PRIMARY, false)] },
                "options": options("hours", false),
            })
            .to_string(),
        },
    ]
}
