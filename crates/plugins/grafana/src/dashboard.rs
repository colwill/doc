//! A dashboard as DOC draws it: its sections, panels, variables and time range. A panel's queries
//! are rebuilt as Grafana's page builds them — variables in, data source resolved, an interval
//! worked out — and sent to `/api/ds/query`, which runs every kind of data source.

use std::collections::BTreeMap;
use std::sync::LazyLock;

use chrono::{
    DateTime, Datelike, Days, DurationRound, Months, NaiveTime, TimeDelta, TimeZone, Utc,
};
use regex::{Captures, Regex};
use serde_json::{Value, json};

use crate::grafana::Datasource;

/// What Grafana stores for a variable set to all of its values.
pub const ALL: &str = "$__all";
/// How many points a chart asks for across its width, unless the panel says.
const POINTS: u64 = 300;
/// The intervals a query is rounded up to, in milliseconds, as Grafana rounds them.
const NICE: [i64; 23] = [
    1_000,
    2_000,
    5_000,
    10_000,
    15_000,
    20_000,
    30_000,
    60_000,
    120_000,
    300_000,
    600_000,
    900_000,
    1_200_000,
    1_800_000,
    3_600_000,
    7_200_000,
    10_800_000,
    21_600_000,
    43_200_000,
    86_400_000,
    172_800_000,
    604_800_000,
    2_592_000_000,
];

/// The time ranges offered, as Grafana writes them and as a person reads them.
pub const RANGES: [(&str, &str); 9] = [
    ("now-15m", "Last 15 minutes"),
    ("now-1h", "Last hour"),
    ("now-3h", "Last 3 hours"),
    ("now-6h", "Last 6 hours"),
    ("now-12h", "Last 12 hours"),
    ("now-24h", "Last 24 hours"),
    ("now-2d", "Last 2 days"),
    ("now-7d", "Last 7 days"),
    ("now-30d", "Last 30 days"),
];

/// `$name`, `${name}`, `${name:format}`, `[[name]]` and `[[name:format]]`.
static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\$(\w+)|\$\{(\w+)(?::([^}]+))?\}|\[\[(\w+)(?::([^\]]+))?\]\]")
        .unwrap_or_else(|err| unreachable!("a fixed pattern is valid: {err}"))
});

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().trim().to_string()
}

/// A value Grafana may keep as a string or a list, as the list it means.
fn values(value: &Value) -> Vec<String> {
    match value {
        Value::String(one) => vec![one.clone()],
        Value::Array(many) => many.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Value::Number(number) => vec![number.to_string()],
        _ => Vec::new(),
    }
}

#[derive(Debug, Clone)]
pub struct Dashboard {
    pub uid: String,
    pub title: String,
    pub folder: String,
    /// Where Grafana shows it, under its own address: `/d/<uid>/<slug>`.
    pub path: String,
    pub from: String,
    pub to: String,
    pub variables: Vec<Variable>,
    pub sections: Vec<Section>,
}

impl Dashboard {
    pub fn read(uid: &str, answer: &Value) -> Result<Self, String> {
        let board = &answer["dashboard"];
        if !board.is_object() {
            return Err("Grafana's answer had no dashboard in it".into());
        }
        let meta = &answer["meta"];
        let title = text(&board["title"]);
        Ok(Self {
            uid: uid.to_string(),
            title: if title.is_empty() { uid.to_string() } else { title },
            folder: meta["folderTitle"].as_str().unwrap_or("General").to_string(),
            path: meta["url"].as_str().map_or_else(|| format!("/d/{uid}"), str::to_string),
            from: board["time"]["from"].as_str().unwrap_or("now-6h").to_string(),
            to: board["time"]["to"].as_str().unwrap_or("now").to_string(),
            variables: board["templating"]["list"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Variable::read)
                .collect(),
            sections: sections(board),
        })
    }

    pub fn panels(&self) -> impl Iterator<Item = &Panel> {
        self.sections.iter().flat_map(|section| section.panels.iter())
    }

    pub fn panel(&self, id: i64) -> Option<&Panel> {
        self.panels().find(|panel| panel.id == id)
    }

    pub fn variable(&self, name: &str) -> Option<&Variable> {
        self.variables.iter().find(|variable| variable.name == name)
    }
}

/// A row of a dashboard and the panels in it. The panels before the first row have no title.
#[derive(Debug, Clone)]
pub struct Section {
    pub title: String,
    pub panels: Vec<Panel>,
}

fn grid(panel: &Value) -> (i64, i64) {
    let at = &panel["gridPos"];
    (at["y"].as_i64().unwrap_or_default(), at["x"].as_i64().unwrap_or_default())
}

/// The sections in Grafana's order: old-style rows, or a flat list in which a row starts one.
fn sections(board: &Value) -> Vec<Section> {
    let flat = board["panels"].as_array().cloned().unwrap_or_default();
    let mut found: Vec<Section> = Vec::new();
    if flat.is_empty() {
        for row in board["rows"].as_array().into_iter().flatten() {
            let titled = row["showTitle"].as_bool().unwrap_or(false);
            found.push(Section {
                title: if titled { text(&row["title"]) } else { String::new() },
                panels: row["panels"].as_array().into_iter().flatten().map(Panel::read).collect(),
            });
        }
    } else {
        let mut flat = flat;
        flat.sort_by_key(grid);
        found.push(Section { title: String::new(), panels: Vec::new() });
        for panel in &flat {
            if panel["type"].as_str() == Some("row") {
                let mut inside: Vec<Value> =
                    panel["panels"].as_array().cloned().unwrap_or_default();
                inside.sort_by_key(grid);
                found.push(Section {
                    title: text(&panel["title"]),
                    panels: inside.iter().map(Panel::read).collect(),
                });
            } else if let Some(last) = found.last_mut() {
                last.panels.push(Panel::read(panel));
            }
        }
    }
    found.retain(|section| !section.panels.is_empty());
    found
}

#[derive(Debug, Clone)]
pub struct Panel {
    pub id: i64,
    pub title: String,
    pub kind: String,
    pub description: String,
    pub datasource: Value,
    pub targets: Vec<Value>,
    /// `fieldConfig.defaults`: the unit and decimals every field starts from.
    pub defaults: Value,
    pub options: Value,
    /// What Grafana's own page does to the answer before drawing it.
    pub transformations: Value,
    pub max_points: Option<u64>,
    /// The shortest interval the panel's queries may use, such as `1m`.
    pub interval: String,
    /// What a text panel says.
    pub content: String,
    /// A library panel's uid, for a panel the dashboard holds only a reference to.
    pub library: Option<String>,
}

impl Panel {
    fn read(value: &Value) -> Self {
        let library = value["libraryPanel"]["uid"]
            .as_str()
            .filter(|_| value.get("targets").is_none())
            .map(str::to_string);
        let content = value["options"]["content"].as_str().or_else(|| value["content"].as_str());
        Self {
            id: value["id"].as_i64().unwrap_or_default(),
            title: text(&value["title"]),
            kind: value["type"].as_str().unwrap_or_default().to_string(),
            description: text(&value["description"]),
            datasource: value["datasource"].clone(),
            targets: value["targets"].as_array().cloned().unwrap_or_default(),
            defaults: value["fieldConfig"]["defaults"].clone(),
            options: value["options"].clone(),
            transformations: value["transformations"].clone(),
            max_points: value["maxDataPoints"].as_u64(),
            interval: value["interval"].as_str().unwrap_or_default().to_string(),
            content: content.unwrap_or_default().to_string(),
            library,
        }
    }

    /// The panel with its library panel's model in place of the reference, keeping where it is.
    pub fn with_library(&self, model: &Value) -> Self {
        let mut panel = Self::read(model);
        panel.id = self.id;
        panel.library = None;
        if panel.title.is_empty() {
            panel.title.clone_from(&self.title);
        }
        panel
    }

    pub fn unit(&self) -> &str {
        self.defaults["unit"].as_str().unwrap_or_default()
    }

    pub fn decimals(&self) -> Option<usize> {
        self.defaults["decimals"]
            .as_u64()
            .and_then(|decimals| usize::try_from(decimals.min(8)).ok())
    }

    /// How a figure is worked out from a series, such as `lastNotNull` or `mean`.
    pub fn calculation(&self) -> &str {
        self.options["reduceOptions"]["calcs"][0].as_str().unwrap_or("lastNotNull")
    }
}

#[derive(Debug, Clone)]
pub struct Variable {
    pub name: String,
    pub label: String,
    pub kind: String,
    /// Not offered on the page: Grafana hides it, or it is not something to choose.
    pub hidden: bool,
    pub all_value: String,
    pub current: Vec<String>,
    /// What can be chosen, as `(text, value)`.
    pub options: Vec<(String, String)>,
    /// For a data source variable, the kind of data source it chooses among.
    pub query: String,
}

impl Variable {
    fn read(value: &Value) -> Option<Self> {
        let name = value["name"].as_str()?.to_string();
        let kind = value["type"].as_str().unwrap_or("query").to_string();
        let query = match &value["query"] {
            Value::String(query) => query.clone(),
            Value::Object(query) => {
                query.get("query").and_then(Value::as_str).unwrap_or_default().to_string()
            }
            _ => String::new(),
        };
        let mut options: Vec<(String, String)> = value["options"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|option| (values(&option["text"]).join(" + "), values(&option["value"]).join(",")))
            .collect();
        if options.is_empty() && matches!(kind.as_str(), "custom" | "interval") {
            options = listed(&query);
        }
        let all = value["includeAll"].as_bool().unwrap_or(false);
        if all && !options.iter().any(|(_, value)| value == ALL) {
            options.insert(0, ("All".to_string(), ALL.to_string()));
        }
        let mut current = values(&value["current"]["value"]);
        if kind == "constant" {
            current = vec![query.clone()];
        }
        if current.is_empty()
            && let Some((_, first)) = options.first()
        {
            current = vec![first.clone()];
        }
        let label = text(&value["label"]);
        Some(Self {
            label: if label.is_empty() { name.clone() } else { label },
            hidden: value["hide"].as_i64() == Some(2)
                || matches!(kind.as_str(), "constant" | "adhoc"),
            all_value: value["allValue"].as_str().unwrap_or_default().to_string(),
            name,
            kind,
            current,
            options,
            query,
        })
    }
}

/// A custom variable's values as written: `a, b, c` or `text : value`, with `\,` inside one.
fn listed(query: &str) -> Vec<(String, String)> {
    let mut found = Vec::new();
    for part in query.replace("\\,", "\u{0}").split(',') {
        let part = part.replace('\u{0}', ",");
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (shown, value) = part.split_once(" : ").unwrap_or((part, part));
        found.push((shown.trim().to_string(), value.trim().to_string()));
    }
    found
}

/// The time a dashboard covers: as Grafana writes it, and as moments.
#[derive(Debug, Clone)]
pub struct Range {
    pub from: String,
    pub to: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
}

impl Range {
    fn of(from: &str, to: &str, now: DateTime<Utc>) -> Option<Self> {
        let start = moment(from, now, false)?;
        let end = moment(to, now, true)?;
        (start < end).then(|| Self { from: from.to_string(), to: to.to_string(), start, end })
    }

    /// The range asked for — Grafana's `from` and `to`, or `range` — else the dashboard's own.
    pub fn asked(query: &str, board: &Dashboard) -> Self {
        let now = Utc::now();
        let asked = |name: &str| crate::parameter(query, name);
        let written = match (asked("from"), asked("range")) {
            (Some(from), _) => Some((from, asked("to").unwrap_or_else(|| "now".to_string()))),
            (None, Some(range)) => Some(match range.split_once('~') {
                Some((from, to)) => (from.to_string(), to.to_string()),
                None => (range, "now".to_string()),
            }),
            (None, None) => None,
        };
        written
            .and_then(|(from, to)| Self::of(&from, &to, now))
            .or_else(|| Self::of(&board.from, &board.to, now))
            .or_else(|| Self::of("now-6h", "now", now))
            .unwrap_or_else(|| Self {
                from: "now-6h".into(),
                to: "now".into(),
                start: now - TimeDelta::hours(6),
                end: now,
            })
    }

    /// How the page's `range` writes it.
    pub fn value(&self) -> String {
        match self.to.as_str() {
            "now" => self.from.clone(),
            to => format!("{}~{to}", self.from),
        }
    }

    pub fn millis(&self) -> i64 {
        (self.end - self.start).num_milliseconds().max(1_000)
    }

    /// What a figure was taken over, in words: `over the last hour`, or from one moment to another.
    pub fn over(&self) -> String {
        let words = self.words();
        match words.starts_with("Last ") {
            true => format!("over the {}", words.to_lowercase()),
            false => format!("from {words}"),
        }
    }

    /// The range in words, for a hint or a choice.
    pub fn words(&self) -> String {
        if self.to == "now"
            && let Some((_, label)) = RANGES.iter().find(|(from, _)| *from == self.from)
        {
            return label.to_string();
        }
        format!(
            "{} to {} UTC",
            self.start.format("%-d %b %Y %H:%M"),
            self.end.format("%-d %b %Y %H:%M")
        )
    }
}

/// A moment as Grafana writes one (`now-6h`, `now-1d/d`, millis, a date); an end rounds up.
fn moment(text: &str, now: DateTime<Utc>, end: bool) -> Option<DateTime<Utc>> {
    let text = text.trim();
    if let Some(rest) = text.strip_prefix("now") {
        let (offset, round) = match rest.split_once('/') {
            Some((offset, round)) => (offset, Some(round)),
            None => (rest, None),
        };
        let mut at = now;
        if !offset.is_empty() {
            let (sign, body) = offset.split_at(1);
            let split = body.find(|c: char| !c.is_ascii_digit())?;
            let (count, unit) = body.split_at(split);
            let count: i64 = count.parse().ok()?;
            at = match sign {
                "-" => shifted(at, -count, unit)?,
                "+" => shifted(at, count, unit)?,
                _ => return None,
            };
        }
        return match round {
            Some(unit) => rounded(at, unit, end),
            None => Some(at),
        };
    }
    if let Ok(millis) = text.parse::<i64>() {
        return Utc.timestamp_millis_opt(millis).single();
    }
    DateTime::parse_from_rfc3339(text).ok().map(|at| at.with_timezone(&Utc))
}

fn shifted(at: DateTime<Utc>, count: i64, unit: &str) -> Option<DateTime<Utc>> {
    let months = |months: i64| {
        let span = Months::new(u32::try_from(months.unsigned_abs()).ok()?);
        match months < 0 {
            true => at.checked_sub_months(span),
            false => at.checked_add_months(span),
        }
    };
    match unit {
        "s" => at.checked_add_signed(TimeDelta::try_seconds(count)?),
        "m" => at.checked_add_signed(TimeDelta::try_minutes(count)?),
        "h" => at.checked_add_signed(TimeDelta::try_hours(count)?),
        "d" => at.checked_add_signed(TimeDelta::try_days(count)?),
        "w" => at.checked_add_signed(TimeDelta::try_weeks(count)?),
        "M" => months(count),
        "y" => months(count.checked_mul(12)?),
        _ => None,
    }
}

fn rounded(at: DateTime<Utc>, unit: &str, end: bool) -> Option<DateTime<Utc>> {
    let midnight = |day: chrono::NaiveDate| Utc.from_utc_datetime(&day.and_time(NaiveTime::MIN));
    let start = match unit {
        "s" => at.duration_trunc(TimeDelta::seconds(1)).ok()?,
        "m" => at.duration_trunc(TimeDelta::minutes(1)).ok()?,
        "h" => at.duration_trunc(TimeDelta::hours(1)).ok()?,
        "d" => midnight(at.date_naive()),
        "w" => {
            let back = u64::from(at.weekday().num_days_from_monday());
            midnight(at.date_naive().checked_sub_days(Days::new(back))?)
        }
        "M" => Utc.with_ymd_and_hms(at.year(), at.month(), 1, 0, 0, 0).single()?,
        "y" => Utc.with_ymd_and_hms(at.year(), 1, 1, 0, 0, 0).single()?,
        _ => return None,
    };
    match end {
        true => Some(shifted(start, 1, unit)? - TimeDelta::milliseconds(1)),
        false => Some(start),
    }
}

/// A span as a panel's minimum interval writes it, such as `30s`, `1m` or `>5m`.
fn span_millis(text: &str) -> Option<i64> {
    let text = text.trim().trim_start_matches('>');
    let split = text.find(|c: char| !c.is_ascii_digit())?;
    let (count, unit) = text.split_at(split);
    let count: i64 = count.parse().ok()?;
    let each = match unit {
        "ms" => 1,
        "s" => 1_000,
        "m" => 60_000,
        "h" => 3_600_000,
        "d" => 86_400_000,
        "w" => 604_800_000,
        "y" => 31_536_000_000,
        _ => return None,
    };
    count.checked_mul(each)
}

/// The step a query takes: the range over the points asked, no finer than the panel allows.
fn interval_millis(range: &Range, points: u64, least: &str) -> i64 {
    let points = i64::try_from(points.max(1)).unwrap_or(i64::MAX);
    let wanted = (range.millis() / points).max(span_millis(least).unwrap_or_default()).max(1_000);
    NICE.iter().copied().find(|nice| *nice >= wanted).unwrap_or(wanted)
}

/// Where a panel's data comes from, once its variables are put in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Source {
    Found {
        uid: String,
        kind: String,
    },
    /// Each query names its own.
    Mixed,
    /// Grafana's list of data sources is needed to say which.
    Unknown,
    Refused(String),
}

/// The body for `/api/ds/query`, and the queries only there for others, not drawn.
#[derive(Debug, Clone)]
pub struct Asking {
    pub body: Value,
    pub hidden: Vec<String>,
}

pub enum Built {
    Ready(Asking),
    /// The data sources must be listed first.
    Unknown,
    Refused(String),
}

/// What a page has chosen: the range, and a value for each variable.
#[derive(Debug, Clone)]
pub struct Scope<'a> {
    board: &'a Dashboard,
    pub range: Range,
    pub values: BTreeMap<String, Vec<String>>,
}

fn is_expression(datasource: &Value) -> bool {
    let uid = datasource["uid"].as_str().or_else(|| datasource.as_str()).unwrap_or_default();
    matches!(uid, "__expr__" | "-100") || datasource["type"].as_str() == Some("__expr__")
}

fn letter(index: usize) -> String {
    let letters = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut name = String::from(char::from(letters[index % letters.len()]));
    if index >= letters.len() {
        name.push_str(&(index / letters.len()).to_string());
    }
    name
}

impl<'a> Scope<'a> {
    /// Values from the query's `var-<name>`, then those `given`, then what the dashboard saved.
    pub fn new(board: &'a Dashboard, query: &str, given: &BTreeMap<String, String>) -> Self {
        let asked: Vec<(String, String)> =
            url::form_urlencoded::parse(query.as_bytes()).into_owned().collect();
        let mut values = BTreeMap::new();
        for variable in &board.variables {
            let key = format!("var-{}", variable.name);
            let from_query: Vec<String> = asked
                .iter()
                .filter(|(name, _)| *name == key)
                .map(|(_, value)| value.clone())
                .collect();
            let chosen = match (variable.kind.as_str(), from_query.is_empty()) {
                ("constant", _) | (_, true) => match given.get(&variable.name) {
                    Some(value) if variable.kind != "constant" => vec![value.clone()],
                    _ => variable.current.clone(),
                },
                _ => from_query,
            };
            values.insert(variable.name.clone(), chosen);
        }
        Self { board, range: Range::asked(query, board), values }
    }

    fn pairs(&self, from: &str, to: Option<&str>) -> String {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        match to {
            Some(to) => query.append_pair("from", from).append_pair("to", to),
            None => query.append_pair("range", from),
        };
        for variable in self.board.variables.iter().filter(|variable| !variable.hidden) {
            for value in self.values.get(&variable.name).into_iter().flatten() {
                query.append_pair(&format!("var-{}", variable.name), value);
            }
        }
        query.finish()
    }

    /// The query a link to this view carries, so a panel draws what its page chose.
    pub fn query(&self) -> String {
        self.pairs(&self.range.value(), None)
    }

    /// The same, as Grafana's own page reads it.
    pub fn grafana_query(&self) -> String {
        self.pairs(&self.range.from, Some(&self.range.to))
    }

    /// `text` with its variables put in for a data source of `kind`; Grafana's own `$__` are left.
    pub fn interpolate(&self, text: &str, kind: &str) -> String {
        REFERENCE
            .replace_all(text, |found: &Captures<'_>| {
                let name = found.get(1).or_else(|| found.get(2)).or_else(|| found.get(4));
                let name = name.map(|name| name.as_str()).unwrap_or_default();
                let format = found.get(3).or_else(|| found.get(5)).map(|format| format.as_str());
                self.built_in(name)
                    .or_else(|| self.formatted(name, format, kind))
                    .unwrap_or_else(|| found[0].to_string())
            })
            .into_owned()
    }

    /// `text` with its variables as people read them, as in a title: `All`, not every value.
    pub fn words(&self, text: &str) -> String {
        REFERENCE
            .replace_all(text, |found: &Captures<'_>| {
                let name = found.get(1).or_else(|| found.get(2)).or_else(|| found.get(4));
                let name = name.map(|name| name.as_str()).unwrap_or_default();
                self.built_in(name)
                    .or_else(|| self.formatted(name, Some("text"), ""))
                    .unwrap_or_else(|| found[0].to_string())
            })
            .into_owned()
    }

    fn built_in(&self, name: &str) -> Option<String> {
        match name {
            "__from" => Some(self.range.start.timestamp_millis().to_string()),
            "__to" => Some(self.range.end.timestamp_millis().to_string()),
            "__dashboard" => Some(self.board.title.clone()),
            _ => None,
        }
    }

    fn formatted(&self, name: &str, format: Option<&str>, kind: &str) -> Option<String> {
        let variable = self.board.variable(name)?;
        let chosen = self.values.get(name)?;
        let all = chosen.iter().any(|value| value == ALL);
        if all && !variable.all_value.is_empty() {
            return Some(variable.all_value.clone());
        }
        let values: Vec<String> = match all {
            true => variable
                .options
                .iter()
                .map(|(_, value)| value.clone())
                .filter(|value| value != ALL)
                .collect(),
            false => chosen.clone(),
        };
        if format == Some("text") && all {
            return Some("All".to_string());
        }
        if format == Some("text") {
            let shown = values.iter().map(|value| {
                variable
                    .options
                    .iter()
                    .find(|(_, held)| held == value)
                    .map_or_else(|| value.clone(), |(shown, _)| shown.clone())
            });
            return Some(shown.collect::<Vec<_>>().join(" + "));
        }
        if all && values.is_empty() {
            let every = if matches!(kind, "prometheus" | "loki") { ".*" } else { "*" };
            return Some(every.to_string());
        }
        Some(shaped(&values, format, kind, all || values.len() > 1))
    }

    /// Where `datasource` reads from; with an empty `list`, a name is trusted as a uid.
    pub fn source(&self, datasource: &Value, list: Option<&[Datasource]>) -> Source {
        let (written, kind) = match datasource {
            Value::Null => (String::new(), String::new()),
            Value::String(written) => (written.clone(), String::new()),
            Value::Object(held) => (
                held.get("uid").and_then(Value::as_str).unwrap_or_default().to_string(),
                held.get("type").and_then(Value::as_str).unwrap_or_default().to_string(),
            ),
            _ => return Source::Refused("its data source is not one Grafana names".into()),
        };
        let named = self.interpolate(&written, "");
        let by_variable = named != written;
        match named.as_str() {
            "" | "default" => match list {
                None => Source::Unknown,
                Some(list) => list.iter().find(|held| held.default).map_or_else(
                    || {
                        Source::Refused(
                            "it names no data source, and DOC could not find Grafana's default"
                                .into(),
                        )
                    },
                    |held| Source::Found { uid: held.uid.clone(), kind: held.kind.clone() },
                ),
            },
            "-- Mixed --" => Source::Mixed,
            "-- Dashboard --" => Source::Refused(
                "it shows another panel's results, which only Grafana's own page can draw".into(),
            ),
            "-- Grafana --" | "grafana" => {
                Source::Found { uid: "grafana".into(), kind: "datasource".into() }
            }
            "__expr__" | "-100" => {
                Source::Found { uid: "__expr__".into(), kind: "__expr__".into() }
            }
            unset if unset.starts_with('$') || unset.starts_with("[[") => Source::Refused(format!(
                "its data source is the variable {unset}, which has no value"
            )),
            named if !kind.is_empty() && !by_variable => {
                Source::Found { uid: named.to_string(), kind }
            }
            named => match list {
                None => Source::Unknown,
                Some(list) => {
                    match list.iter().find(|held| held.uid == named || held.name == named) {
                        Some(held) => {
                            Source::Found { uid: held.uid.clone(), kind: held.kind.clone() }
                        }
                        None if list.is_empty() => Source::Found { uid: named.to_string(), kind },
                        None => {
                            Source::Refused(format!("Grafana has no data source called {named}"))
                        }
                    }
                }
            },
        }
    }

    /// The call that runs a panel's queries; a query's own data source wins where it must.
    pub fn build(&self, panel: &Panel, list: Option<&[Datasource]>) -> Built {
        if panel.targets.is_empty() {
            return Built::Refused("it has no queries".into());
        }
        let points = panel.max_points.unwrap_or(POINTS).clamp(10, 1_000);
        let interval = interval_millis(&self.range, points, &panel.interval);
        let expressions = panel.targets.iter().any(|target| is_expression(&target["datasource"]));
        let panel_source = self.source(&panel.datasource, list);
        let mut queries = Vec::new();
        let mut hidden = Vec::new();
        for (index, target) in panel.targets.iter().enumerate() {
            let ref_id = target["refId"]
                .as_str()
                .filter(|ref_id| !ref_id.is_empty())
                .map_or_else(|| letter(index), str::to_string);
            if target["hide"].as_bool().unwrap_or(false) {
                hidden.push(ref_id.clone());
                if !expressions {
                    continue;
                }
            }
            let own = &target["datasource"];
            let own_wins = is_expression(own)
                || panel_source == Source::Mixed
                || (panel.datasource.is_null() && !own.is_null());
            let source = match own_wins {
                true => self.source(own, list),
                false => panel_source.clone(),
            };
            let (uid, kind) = match source {
                Source::Found { uid, kind } => (uid, kind),
                Source::Unknown => return Built::Unknown,
                Source::Mixed => {
                    return Built::Refused("one of its queries names no data source".into());
                }
                Source::Refused(why) => return Built::Refused(why),
            };
            let mut query = self.filled(target, &kind);
            query["refId"] = json!(ref_id);
            query["datasource"] = json!({ "uid": uid, "type": kind });
            query["intervalMs"] = json!(interval);
            query["maxDataPoints"] = json!(points);
            queries.push(query);
        }
        if queries.is_empty() {
            return Built::Refused("every query in it is hidden".into());
        }
        Built::Ready(Asking {
            body: json!({
                "queries": queries,
                "from": self.range.start.timestamp_millis().to_string(),
                "to": self.range.end.timestamp_millis().to_string(),
            }),
            hidden,
        })
    }

    /// A query with every string in it interpolated, but for what names it.
    fn filled(&self, value: &Value, kind: &str) -> Value {
        match value {
            Value::String(text) => Value::String(self.interpolate(text, kind)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|item| self.filled(item, kind)).collect())
            }
            Value::Object(held) => Value::Object(
                held.iter()
                    .map(|(key, item)| match key.as_str() {
                        "refId" | "datasource" => (key.clone(), item.clone()),
                        _ => (key.clone(), self.filled(item, kind)),
                    })
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

/// Values put into a query as `format` asks, or by default as a regex or a glob for many.
fn shaped(values: &[String], format: Option<&str>, kind: &str, many: bool) -> String {
    let each = |separator: &str, shape: &dyn Fn(&String) -> String| {
        values.iter().map(shape).collect::<Vec<_>>().join(separator)
    };
    let one = values.first().cloned().unwrap_or_default();
    match format {
        Some("raw" | "csv") => values.join(","),
        Some("pipe") => values.join("|"),
        Some("json") => serde_json::to_string(values).unwrap_or_default(),
        Some("regex") if values.len() == 1 => escaped(&one, false),
        Some("regex") => format!("({})", each("|", &|value| escaped(value, false))),
        Some("glob") if values.len() == 1 => one,
        Some("glob") => format!("{{{}}}", values.join(",")),
        Some("singlequote") => each(",", &|value| format!("'{}'", value.replace('\'', "\\'"))),
        Some("doublequote") => each(",", &|value| format!("\"{}\"", value.replace('"', "\\\""))),
        Some("sqlstring") => each(",", &|value| format!("'{}'", value.replace('\'', "''"))),
        Some("percentencode") => {
            url::form_urlencoded::byte_serialize(values.join(",").as_bytes()).collect()
        }
        _ if !many => one,
        _ if matches!(kind, "prometheus" | "loki") => {
            format!("({})", each("|", &|value| escaped(value, true)))
        }
        _ => format!("{{{}}}", values.join(",")),
    }
}

/// A value made literal in a regular expression; `quoted` escapes it again for a PromQL string.
fn escaped(value: &str, quoted: bool) -> String {
    let slash = if quoted { "\\\\" } else { "\\" };
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push_str(slash);
        }
        out.push(c);
    }
    out
}
