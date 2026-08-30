//! What Grafana answers a panel's queries with — data frames — as what DOC draws: lines over time,
//! bars, figures and tables. A chart carries no code to format it, so units are worked out here:
//! scaled to one unit for a chart's axis, and written out in their own for a figure.

use std::collections::BTreeMap;

use chrono::{TimeZone, Utc};
use serde_json::{Value, json};

/// The brand's purple first, for the measure itself (DOC-SPEC §11.22), then distinct colours.
const COLOURS: [&str; 10] = [
    "#5b00b8", "#005eb8", "#007f3b", "#d5281b", "#768692", "#ed8b00", "#00a499", "#ae2573",
    "#330072", "#ffb81c",
];
/// The most series one chart draws: every one a panel is likely to have, short of page weight.
pub const MOST_SERIES: usize = 200;
/// The most series a chart names in a legend; past it a legend is noise, and hovering names each.
const LEGEND: usize = 12;
/// The most rows a table shows.
pub const MOST_ROWS: usize = 100;
/// The most bars or slices a chart of categories draws.
const MOST_CATEGORIES: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Time,
    Number,
    Text,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
    pub labels: BTreeMap<String, String>,
    /// What the data source says to call it, such as Prometheus' legend.
    pub shown: Option<String>,
    pub unit: Option<String>,
    pub values: Vec<Value>,
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub name: String,
    /// The query it answers, which a geomap layer can be limited to.
    pub ref_id: String,
    pub fields: Vec<Field>,
}

impl Frame {
    fn read(frame: &Value) -> Option<Self> {
        let schema = &frame["schema"];
        let columns = frame["data"]["values"].as_array();
        let fields = schema["fields"]
            .as_array()?
            .iter()
            .enumerate()
            .map(|(index, field)| {
                let kind = match (field["type"].as_str(), field["typeInfo"]["frame"].as_str()) {
                    (Some("time"), _) | (_, Some("time.Time" | "*time.Time")) => Kind::Time,
                    (Some("number"), _) => Kind::Number,
                    _ => Kind::Text,
                };
                let config = &field["config"];
                Field {
                    name: field["name"].as_str().unwrap_or_default().to_string(),
                    kind,
                    labels: field["labels"]
                        .as_object()
                        .into_iter()
                        .flatten()
                        .map(|(key, value)| {
                            (key.clone(), value.as_str().unwrap_or_default().to_string())
                        })
                        .collect(),
                    shown: config["displayNameFromDS"]
                        .as_str()
                        .or_else(|| config["displayName"].as_str())
                        .filter(|shown| !shown.is_empty())
                        .map(str::to_string),
                    unit: config["unit"]
                        .as_str()
                        .filter(|unit| !unit.is_empty())
                        .map(str::to_string),
                    values: columns
                        .and_then(|columns| columns.get(index))
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default(),
                }
            })
            .collect();
        Some(Self {
            name: schema["name"].as_str().unwrap_or_default().to_string(),
            ref_id: schema["refId"].as_str().unwrap_or_default().to_string(),
            fields,
        })
    }

    fn rows(&self) -> usize {
        self.fields.iter().map(|field| field.values.len()).max().unwrap_or_default()
    }
}

/// Everything one panel's queries answered, and what went wrong with any of them.
#[derive(Debug, Default)]
pub struct Answer {
    pub frames: Vec<Frame>,
    pub problems: Vec<String>,
}

/// One number over time, or by row where there is no time.
#[derive(Debug, Clone)]
pub struct Series {
    pub name: String,
    pub unit: Option<String>,
    pub timed: bool,
    pub points: Vec<(i64, Option<f64>)>,
}

fn millis(value: &Value) -> Option<i64> {
    value.as_i64().or_else(|| value.as_f64().map(|at| at as i64))
}

/// What Grafana would call a field: its data source's name, its labels, its frame's or its own.
fn display_name(field: &Field, frame: &Frame, alone: bool) -> String {
    if let Some(shown) = &field.shown {
        return shown.clone();
    }
    let metric = field.labels.get("__name__");
    let labels: Vec<(&String, &String)> =
        field.labels.iter().filter(|(key, _)| *key != "__name__").collect();
    if let [(_, value)] = labels.as_slice()
        && metric.is_none()
    {
        return (*value).clone();
    }
    if !labels.is_empty() {
        let written: Vec<String> =
            labels.iter().map(|(key, value)| format!("{key}=\"{value}\"")).collect();
        let named = metric.cloned().unwrap_or_else(|| match field.name.as_str() {
            "Value" | "value" => String::new(),
            name => name.to_string(),
        });
        return format!("{named}{{{}}}", written.join(", "));
    }
    if let Some(metric) = metric {
        return metric.clone();
    }
    match (frame.name.is_empty(), field.name.as_str()) {
        (false, "Value" | "value") => frame.name.clone(),
        (false, _) if alone => frame.name.clone(),
        _ => field.name.clone(),
    }
}

impl Answer {
    /// Grafana's answer to `/api/ds/query`, leaving out the queries only there for others.
    pub fn read(answer: &Value, hidden: &[String]) -> Self {
        let mut read = Self::default();
        for (ref_id, result) in answer["results"].as_object().into_iter().flatten() {
            if hidden.contains(ref_id) {
                continue;
            }
            if let Some(error) = result["error"].as_str().filter(|error| !error.is_empty()) {
                read.problems.push(format!("Query {ref_id}: {error}"));
            }
            let frames = result["frames"].as_array().into_iter().flatten().filter_map(Frame::read);
            read.frames.extend(frames.map(|mut frame| {
                if frame.ref_id.is_empty() {
                    frame.ref_id.clone_from(ref_id);
                }
                frame
            }));
        }
        read
    }

    pub fn is_empty(&self) -> bool {
        self.frames.iter().all(|frame| frame.rows() == 0)
    }

    pub fn series(&self) -> Vec<Series> {
        let mut found = Vec::new();
        for frame in &self.frames {
            let time = frame.fields.iter().find(|field| field.kind == Kind::Time);
            let numbers: Vec<&Field> =
                frame.fields.iter().filter(|field| field.kind == Kind::Number).collect();
            for field in &numbers {
                let points = field
                    .values
                    .iter()
                    .enumerate()
                    .map(|(row, value)| {
                        let at = time.and_then(|time| time.values.get(row)).and_then(millis);
                        let at = at.unwrap_or_else(|| i64::try_from(row).unwrap_or_default());
                        (at, value.as_f64().filter(|value| value.is_finite()))
                    })
                    .collect();
                found.push(Series {
                    name: display_name(field, frame, numbers.len() == 1),
                    unit: field.unit.clone(),
                    timed: time.is_some(),
                    points,
                });
            }
        }
        found
    }

    /// A frame's text column as categories and its numbers as bars, for bars not over time.
    fn categories(&self) -> Option<(Vec<String>, Vec<Series>)> {
        let frame = self.frames.iter().find(|frame| {
            frame.fields.iter().any(|field| field.kind == Kind::Text)
                && frame.fields.iter().any(|field| field.kind == Kind::Number)
        })?;
        let names = frame.fields.iter().find(|field| field.kind == Kind::Text)?;
        let labels: Vec<String> =
            names.values.iter().take(MOST_CATEGORIES).map(cell_text).collect();
        let numbers: Vec<&Field> =
            frame.fields.iter().filter(|field| field.kind == Kind::Number).collect();
        let series = numbers
            .iter()
            .map(|field| Series {
                name: display_name(field, frame, numbers.len() == 1),
                unit: field.unit.clone(),
                timed: false,
                points: field
                    .values
                    .iter()
                    .take(MOST_CATEGORIES)
                    .enumerate()
                    .map(|(row, value)| (i64::try_from(row).unwrap_or_default(), value.as_f64()))
                    .collect(),
            })
            .collect();
        Some((labels, series))
    }
}

pub fn cell_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One figure from a series, as a panel's calculation asks: the latest, the mean, the highest…
pub fn reduce(points: &[(i64, Option<f64>)], calculation: &str) -> Option<f64> {
    let present: Vec<f64> = points.iter().filter_map(|(_, value)| *value).collect();
    let sum: f64 = present.iter().sum();
    let highest = present.iter().copied().reduce(f64::max);
    let lowest = present.iter().copied().reduce(f64::min);
    match calculation {
        "last" => points.last().and_then(|(_, value)| *value),
        "first" => points.first().and_then(|(_, value)| *value),
        "firstNotNull" => present.first().copied(),
        "mean" | "avg" if !present.is_empty() => Some(sum / present.len() as f64),
        "max" => highest,
        "min" => lowest,
        "sum" | "total" if !present.is_empty() => Some(sum),
        "count" => Some(points.len() as f64),
        "range" => highest.zip(lowest).map(|(highest, lowest)| highest - lowest),
        "delta" | "diff" => present.last().zip(present.first()).map(|(last, first)| last - first),
        "mean" | "avg" | "sum" | "total" => None,
        _ => present.last().copied(),
    }
}

/// A calculation in the words a figure is noted with.
pub fn calculated(calculation: &str) -> &'static str {
    match calculation {
        "first" | "firstNotNull" => "First in the range",
        "mean" | "avg" => "Mean over the range",
        "max" => "Highest in the range",
        "min" => "Lowest in the range",
        "sum" | "total" => "Total over the range",
        "count" => "Values in the range",
        "range" => "Highest less lowest",
        "delta" | "diff" => "Change over the range",
        _ => "Latest",
    }
}

/// How numbers in a unit are written: into the unit's base, then the step suiting the largest.
#[derive(Debug, Clone)]
pub struct Scale {
    factor: f64,
    step: f64,
    pub suffix: String,
    /// What the axis is called, or nothing for a plain number.
    pub axis: String,
}

impl Scale {
    pub fn apply(&self, value: f64) -> f64 {
        value * self.factor / self.step
    }
}

const BINARY: [(&str, f64); 6] = [
    ("B", 1.0),
    ("KiB", 1024.0),
    ("MiB", 1_048_576.0),
    ("GiB", 1_073_741_824.0),
    ("TiB", 1_099_511_627_776.0),
    ("PiB", 1_125_899_906_842_624.0),
];
const DECIMAL: [(&str, f64); 6] =
    [("B", 1.0), ("kB", 1e3), ("MB", 1e6), ("GB", 1e9), ("TB", 1e12), ("PB", 1e15)];
const SECONDS: [(&str, f64); 7] = [
    ("ns", 1e-9),
    ("µs", 1e-6),
    ("ms", 1e-3),
    ("s", 1.0),
    ("min", 60.0),
    ("h", 3_600.0),
    ("days", 86_400.0),
];
const SHORT: [(&str, f64); 5] = [("", 1.0), ("K", 1e3), ("Mil", 1e6), ("Bil", 1e9), ("Tri", 1e12)];

/// A Grafana unit, such as `bytes` or `ms`, scaled for numbers up to `largest`.
pub fn scale(unit: &str, largest: f64, chart: bool) -> Scale {
    let flat = |suffix: &str, axis: &str| Scale {
        factor: 1.0,
        step: 1.0,
        suffix: suffix.to_string(),
        axis: axis.to_string(),
    };
    let laddered = |ladder: &[(&str, f64)], factor: f64, per: &str| {
        let largest = largest.abs() * factor;
        let (name, step) = ladder
            .iter()
            .rev()
            .find(|(_, step)| largest >= *step)
            .or_else(|| ladder.iter().find(|(_, step)| *step == 1.0))
            .copied()
            .unwrap_or(("", 1.0));
        let name = format!("{name}{per}");
        let suffix = if name.is_empty() { String::new() } else { format!(" {name}") };
        Scale { factor, step, suffix, axis: name }
    };
    match unit {
        "percent" => flat("%", "%"),
        "percentunit" => Scale { factor: 100.0, step: 1.0, suffix: "%".into(), axis: "%".into() },
        "bytes" | "binbytes" => laddered(&BINARY, 1.0, ""),
        "decbytes" => laddered(&DECIMAL, 1.0, ""),
        "bits" | "binbits" => laddered(&BINARY, 0.125, ""),
        "Bps" | "binBps" => laddered(&BINARY, 1.0, "/s"),
        "ns" => laddered(&SECONDS, 1e-9, ""),
        "µs" | "us" => laddered(&SECONDS, 1e-6, ""),
        "ms" => laddered(&SECONDS, 1e-3, ""),
        "s" | "dtdurations" => laddered(&SECONDS, 1.0, ""),
        "m" => laddered(&SECONDS, 60.0, ""),
        "h" => laddered(&SECONDS, 3_600.0, ""),
        "d" => laddered(&SECONDS, 86_400.0, ""),
        "reqps" => flat(" req/s", "Requests per second"),
        "rps" => flat(" reads/s", "Reads per second"),
        "wps" => flat(" writes/s", "Writes per second"),
        "iops" => flat(" IO/s", "I/O operations per second"),
        "ops" => flat(" ops/s", "Operations per second"),
        "reqpm" => flat(" req/min", "Requests per minute"),
        "celsius" => flat(" °C", "°C"),
        "hertz" => flat(" Hz", "Hz"),
        "" | "short" | "none" | "locale" if chart => flat("", ""),
        "" | "short" | "none" | "locale" => laddered(&SHORT, 1.0, ""),
        other => match other.split_once(':') {
            Some(("suffix", suffix)) => flat(&format!(" {suffix}"), suffix),
            Some(("prefix", prefix)) => flat("", prefix),
            _ => flat(&format!(" {other}"), other),
        },
    }
}

/// A number with about four significant figures, as a chart is sent it.
fn rounded(value: f64) -> f64 {
    if value == 0.0 || !value.is_finite() {
        return value;
    }
    let digits = (3 - value.abs().log10().floor() as i32).clamp(0, 8);
    let power = 10_f64.powi(digits);
    (value * power).round() / power
}

/// A figure in its unit, with the panel's decimals or as many as read well.
pub fn figure(value: f64, unit: &str, decimals: Option<usize>) -> String {
    let scale = scale(unit, value, false);
    let shown = scale.apply(value);
    // A whole number stays whole, even when arithmetic has left a trace on it.
    let whole = (shown - shown.round()).abs() < 1e-9 * shown.abs().max(1.0);
    let decimals = decimals.unwrap_or(match shown.abs() {
        _ if whole => 0,
        big if big >= 100.0 => 0,
        middling if middling >= 10.0 => 1,
        _ => 2,
    });
    format!("{shown:.decimals$}{}", scale.suffix)
}

/// A moment on a chart's axis, written as finely as the range needs.
fn moment(millis: i64, span: i64) -> String {
    let Some(at) = Utc.timestamp_millis_opt(millis).single() else { return millis.to_string() };
    let written = match span {
        ..=7_200_000 => "%H:%M:%S",
        7_200_001..=86_400_000 => "%H:%M",
        86_400_001..=2_678_400_000 => "%-d %b %H:%M",
        _ => "%-d %b %Y",
    };
    at.format(written).to_string()
}

/// A moment in a table, written out whole.
pub fn when(millis: i64) -> String {
    Utc.timestamp_millis_opt(millis)
        .single()
        .map_or_else(|| millis.to_string(), |at| at.format("%Y-%m-%d %H:%M:%S").to_string())
}

fn colour(index: usize) -> &'static str {
    COLOURS[index % COLOURS.len()]
}

fn options(axis: &str, legend: bool, x: &Value) -> Value {
    json!({
        "animation": false,
        "interaction": { "mode": "index", "intersect": false },
        "scales": {
            "x": x,
            "y": { "title": { "display": !axis.is_empty(), "text": axis } },
        },
        "plugins": { "legend": { "display": legend, "position": "bottom" } },
    })
}

/// Lines or bars over time: every moment any series has, with a gap where one has nothing.
pub fn over_time(series: &[Series], unit: &str, span: i64, bars: bool) -> Value {
    let shown: Vec<&Series> = series.iter().take(MOST_SERIES).collect();
    let mut moments: Vec<i64> =
        shown.iter().flat_map(|series| series.points.iter().map(|(at, _)| *at)).collect();
    moments.sort_unstable();
    moments.dedup();
    let largest = shown
        .iter()
        .flat_map(|series| series.points.iter().filter_map(|(_, value)| *value))
        .fold(0.0_f64, |most, value| most.max(value.abs()));
    let scale = scale(unit, largest, true);
    let datasets: Vec<Value> = shown
        .iter()
        .enumerate()
        .map(|(index, series)| {
            let by_moment: BTreeMap<i64, Option<f64>> = series.points.iter().copied().collect();
            let data: Vec<Option<f64>> = moments
                .iter()
                .map(|at| {
                    by_moment.get(at).copied().flatten().map(|value| rounded(scale.apply(value)))
                })
                .collect();
            json!({
                "label": series.name, "data": data,
                "borderColor": colour(index), "backgroundColor": colour(index),
                "spanGaps": true, "tension": 0, "pointRadius": 0, "borderWidth": 2,
            })
        })
        .collect();
    let labels: Vec<String> = moments.iter().map(|at| moment(*at, span)).collect();
    let x = json!({
        "ticks": { "maxTicksLimit": 8, "maxRotation": 0 },
        "title": { "display": true, "text": "Time (UTC)" },
    });
    json!({
        "type": if bars { "bar" } else { "line" },
        "data": { "labels": labels, "datasets": datasets },
        "options": options(&scale.axis, (2..=LEGEND).contains(&datasets.len()), &x),
    })
}

/// Bars for a frame's categories, where a bar chart is not over time.
pub fn by_category(answer: &Answer, unit: &str) -> Option<Value> {
    let (labels, series) = answer.categories()?;
    let largest = series
        .iter()
        .flat_map(|series| series.points.iter().filter_map(|(_, value)| *value))
        .fold(0.0_f64, |most, value| most.max(value.abs()));
    let scale = scale(unit, largest, true);
    let datasets: Vec<Value> = series
        .iter()
        .take(MOST_SERIES)
        .enumerate()
        .map(|(index, series)| {
            let data: Vec<Option<f64>> = series
                .points
                .iter()
                .map(|(_, value)| value.map(|value| rounded(scale.apply(value))))
                .collect();
            json!({ "label": series.name, "data": data, "backgroundColor": colour(index) })
        })
        .collect();
    Some(json!({
        "type": "bar",
        "data": { "labels": labels, "datasets": datasets },
        "options": options(&scale.axis, (2..=LEGEND).contains(&datasets.len()), &json!({})),
    }))
}

/// One figure per series, as bars side by side or as slices of a whole.
pub fn reduced(series: &[Series], calculation: &str, unit: &str, pie: bool) -> Option<Value> {
    let figures: Vec<(String, f64)> = series
        .iter()
        .take(MOST_CATEGORIES)
        .filter_map(|series| Some((series.name.clone(), reduce(&series.points, calculation)?)))
        .collect();
    if figures.is_empty() {
        return None;
    }
    let largest = figures.iter().fold(0.0_f64, |most, (_, value)| most.max(value.abs()));
    let scale = scale(unit, largest, true);
    let labels: Vec<&String> = figures.iter().map(|(name, _)| name).collect();
    let data: Vec<f64> = figures.iter().map(|(_, value)| rounded(scale.apply(*value))).collect();
    let colours: Vec<&str> = (0..figures.len()).map(colour).collect();
    let dataset =
        json!({ "label": calculated(calculation), "data": data, "backgroundColor": colours });
    Some(match pie {
        true => json!({
            "type": "doughnut",
            "data": { "labels": labels, "datasets": [dataset] },
            "options": { "animation": false, "plugins": { "legend": { "position": "bottom" } } },
        }),
        false => {
            let mut options = options(
                &scale.axis,
                false,
                &json!({ "title": { "display": !scale.axis.is_empty(), "text": scale.axis } }),
            );
            options["indexAxis"] = json!("y");
            options["scales"]["y"] = json!({});
            json!({
                "type": "bar",
                "data": { "labels": labels, "datasets": [dataset] },
                "options": options,
            })
        }
    })
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub numeric: bool,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<String>>,
    /// Rows there were beyond those shown.
    pub more: usize,
}

/// A cell as a table shows it: a moment written out, a number in its unit, anything else as it is.
fn cell(field: &Field, row: usize, unit: &str, decimals: Option<usize>) -> String {
    match (field.kind, field.values.get(row).unwrap_or(&Value::Null)) {
        (_, Value::Null) => String::new(),
        (Kind::Time, value) => millis(value).map_or_else(|| cell_text(value), when),
        (Kind::Number, value) => value.as_f64().map_or_else(
            || cell_text(value),
            |number| figure(number, field.unit.as_deref().unwrap_or(unit), decimals),
        ),
        (Kind::Text, value) => cell_text(value),
    }
}

/// The answer as a table: labelled series a column per label, else the first frame and its like.
pub fn table(answer: &Answer, unit: &str, decimals: Option<usize>) -> Option<Table> {
    let filled: Vec<&Frame> = answer.frames.iter().filter(|frame| frame.rows() > 0).collect();
    let first = filled.first()?;
    if let Some(table) = labelled(&filled, unit, decimals) {
        return Some(table);
    }
    let names: Vec<&str> = first.fields.iter().map(|field| field.name.as_str()).collect();
    let alike: Vec<&&Frame> = filled
        .iter()
        .filter(|frame| {
            frame.fields.iter().map(|field| field.name.as_str()).eq(names.iter().copied())
        })
        .collect();
    let columns: Vec<Column> = first
        .fields
        .iter()
        .map(|field| Column {
            name: field.shown.clone().unwrap_or_else(|| field.name.clone()),
            numeric: field.kind == Kind::Number,
        })
        .collect();
    let mut rows = Vec::new();
    let mut total: usize = 0;
    for frame in alike {
        for row in 0..frame.rows() {
            total += 1;
            if rows.len() < MOST_ROWS {
                rows.push(
                    frame.fields.iter().map(|field| cell(field, row, unit, decimals)).collect(),
                );
            }
        }
    }
    Some(Table { columns, more: total.saturating_sub(rows.len()), rows })
}

fn labelled(frames: &[&Frame], unit: &str, decimals: Option<usize>) -> Option<Table> {
    let number = |frame: &Frame| -> Option<usize> {
        let numbers: Vec<usize> = (0..frame.fields.len())
            .filter(|index| frame.fields[*index].kind == Kind::Number)
            .collect();
        let others = frame.fields.iter().any(|field| field.kind == Kind::Text);
        match (numbers.as_slice(), others) {
            ([index], false) => Some(*index),
            _ => None,
        }
    };
    let numbers: Vec<usize> = frames.iter().map(|frame| number(frame)).collect::<Option<_>>()?;
    let keys: std::collections::BTreeSet<&String> = frames
        .iter()
        .zip(&numbers)
        .flat_map(|(frame, index)| frame.fields[*index].labels.keys())
        .filter(|key| *key != "__name__")
        .collect();
    if keys.is_empty() {
        return None;
    }
    let timed =
        frames.iter().any(|frame| frame.fields.iter().any(|field| field.kind == Kind::Time));
    let mut columns = Vec::new();
    if timed {
        columns.push(Column { name: "Time".into(), numeric: false });
    }
    columns.extend(keys.iter().map(|key| Column { name: (*key).clone(), numeric: false }));
    columns.push(Column { name: "Value".into(), numeric: true });
    let mut rows = Vec::new();
    let mut total: usize = 0;
    for (frame, index) in frames.iter().zip(&numbers) {
        let value = &frame.fields[*index];
        let time = frame.fields.iter().find(|field| field.kind == Kind::Time);
        for row in 0..frame.rows() {
            total += 1;
            if rows.len() >= MOST_ROWS {
                continue;
            }
            let mut cells = Vec::with_capacity(columns.len());
            if timed {
                cells.push(time.map(|time| cell(time, row, unit, decimals)).unwrap_or_default());
            }
            cells
                .extend(keys.iter().map(|key| value.labels.get(*key).cloned().unwrap_or_default()));
            cells.push(cell(value, row, unit, decimals));
            rows.push(cells);
        }
    }
    Some(Table { columns, more: total.saturating_sub(rows.len()), rows })
}

/// Every point a chart over time draws, a row per moment and a column per series.
pub fn every_point(series: &[Series], unit: &str, decimals: Option<usize>) -> Table {
    let shown: Vec<&Series> = series.iter().take(MOST_SERIES).collect();
    let mut moments: Vec<i64> =
        shown.iter().flat_map(|series| series.points.iter().map(|(at, _)| *at)).collect();
    moments.sort_unstable();
    moments.dedup();
    let timed = shown.iter().any(|series| series.timed);
    let mut columns =
        vec![Column { name: if timed { "Time (UTC)" } else { "Row" }.into(), numeric: false }];
    columns.extend(shown.iter().map(|series| Column { name: series.name.clone(), numeric: true }));
    let lookups: Vec<BTreeMap<i64, Option<f64>>> =
        shown.iter().map(|series| series.points.iter().copied().collect()).collect();
    let rows = moments
        .iter()
        .map(|at| {
            let mut cells = vec![if timed { when(*at) } else { (at + 1).to_string() }];
            for (series, lookup) in shown.iter().zip(&lookups) {
                let unit = series.unit.as_deref().unwrap_or(unit);
                cells.push(
                    lookup
                        .get(at)
                        .copied()
                        .flatten()
                        .map(|value| figure(value, unit, decimals))
                        .unwrap_or_default(),
                );
            }
            cells
        })
        .collect();
    Table { columns, rows, more: 0 }
}
