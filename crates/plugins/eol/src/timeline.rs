//! A timeline drawn as SVG on the server: one lane a row, bars from one day to another and marks
//! on a day, with today across them all. Pages carry no script or inline style, so the shapes are
//! placed by their attributes and coloured by `doc-gantt` classes in DOC's stylesheet; every
//! timeline sits beside a table that says the same in words.

use std::fmt::Write as _;

use chrono::{Datelike, Duration, NaiveDate};

const WIDTH: f64 = 960.0;
const LABELS: f64 = 210.0;
const RIGHT: f64 = 12.0;
const AXIS: f64 = 30.0;
const ROW: f64 = 26.0;
const BAR: f64 = 14.0;
const LABEL_CHARS: usize = 28;

/// A stretch of time on a lane; with no end, it runs past the right edge.
pub struct Span {
    pub from: NaiveDate,
    pub to: Option<NaiveDate>,
    /// `ready`, `security`, `degraded`, `error`, `unknown`, `muted` or `done`.
    pub tone: &'static str,
    pub said: String,
}

/// One day on a lane, such as a release date.
pub struct Mark {
    pub at: NaiveDate,
    pub tone: &'static str,
    pub said: String,
}

pub struct Lane {
    pub label: String,
    pub href: Option<String>,
    pub spans: Vec<Span>,
    pub marks: Vec<Mark>,
}

pub fn escaped(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
    out
}

fn shortened(text: &str) -> String {
    match text.chars().count() > LABEL_CHARS {
        true => format!("{}…", text.chars().take(LABEL_CHARS - 1).collect::<String>()),
        false => text.to_string(),
    }
}

/// Where the ticks go: months for half a year or less, quarters for a couple of years, years
/// beyond.
fn ticks(from: NaiveDate, to: NaiveDate) -> Vec<(NaiveDate, String)> {
    let days = (to - from).num_days();
    let step = match days {
        ..=200 => 1,
        201..=800 => 3,
        _ => 12,
    };
    let month = |day: NaiveDate| day.checked_add_months(chrono::Months::new(1));
    let mut at = NaiveDate::from_ymd_opt(from.year(), from.month(), 1).unwrap_or(from);
    while at < from || !at.month0().is_multiple_of(step) {
        match month(at) {
            Some(next) if next <= to => at = next,
            _ => return Vec::new(),
        }
    }
    let mut out = Vec::new();
    while at <= to {
        let said = match step {
            12 => at.format("%Y").to_string(),
            _ => at.format("%b %Y").to_string(),
        };
        out.push((at, said));
        match at.checked_add_months(chrono::Months::new(step)) {
            Some(next) => at = next,
            None => break,
        }
    }
    out
}

/// The lanes drawn over `from`..`to`, with `today` marked; `described` is what a screen reader
/// is told the picture shows.
pub fn drawn(
    lanes: &[Lane],
    (from, to): (NaiveDate, NaiveDate),
    today: NaiveDate,
    described: &str,
) -> String {
    let span = (to - from).num_days().max(1) as f64;
    let inner = WIDTH - LABELS - RIGHT;
    let x =
        |day: NaiveDate| LABELS + ((day - from).num_days() as f64 / span * inner).clamp(0.0, inner);
    let height = AXIS + ROW * lanes.len() as f64 + 8.0;
    let mut svg = String::new();
    let _ = write!(
        svg,
        r#"<svg class="doc-gantt" viewBox="0 0 {WIDTH} {height}" width="100%" role="img" aria-label="{}" xmlns="http://www.w3.org/2000/svg">"#,
        escaped(described)
    );
    for (at, said) in ticks(from, to) {
        let left = x(at);
        let _ = write!(
            svg,
            r#"<line class="doc-gantt__tick" x1="{left:.1}" y1="{}" x2="{left:.1}" y2="{height}"/><text class="doc-gantt__tick-label" x="{:.1}" y="14">{}</text>"#,
            AXIS - 6.0,
            left + 3.0,
            escaped(&said)
        );
    }
    for (index, lane) in lanes.iter().enumerate() {
        let top = AXIS + ROW * index as f64;
        let middle = top + ROW / 2.0;
        if index % 2 == 1 {
            let _ = write!(
                svg,
                r#"<rect class="doc-gantt__stripe" x="0" y="{top:.1}" width="{WIDTH}" height="{ROW}"/>"#
            );
        }
        let label = escaped(&shortened(&lane.label));
        let text = format!(
            r#"<text class="doc-gantt__label" x="4" y="{:.1}"><title>{}</title>{label}</text>"#,
            middle + 4.5,
            escaped(&lane.label)
        );
        match &lane.href {
            Some(href) => {
                let _ = write!(svg, r#"<a href="{}">{text}</a>"#, escaped(href));
            }
            None => svg.push_str(&text),
        }
        for bar in &lane.spans {
            let end = bar.to.unwrap_or(to + Duration::days(1));
            if end < from || bar.from > to {
                continue;
            }
            let (left, right) = (x(bar.from.max(from)), x(end.min(to)));
            let _ = write!(
                svg,
                r#"<rect class="doc-gantt__bar doc-gantt__bar--{}" x="{left:.1}" y="{:.1}" width="{:.1}" height="{BAR}" rx="3"><title>{}</title></rect>"#,
                bar.tone,
                middle - BAR / 2.0,
                (right - left).max(2.0),
                escaped(&bar.said)
            );
        }
        for mark in &lane.marks {
            if mark.at < from || mark.at > to {
                continue;
            }
            let (centre, size) = (x(mark.at), 7.0);
            let _ = write!(
                svg,
                r#"<polygon class="doc-gantt__mark doc-gantt__mark--{}" points="{:.1},{:.1} {:.1},{:.1} {:.1},{:.1} {:.1},{:.1}"><title>{}</title></polygon>"#,
                mark.tone,
                centre,
                middle - size,
                centre + size,
                middle,
                centre,
                middle + size,
                centre - size,
                middle,
                escaped(&mark.said)
            );
        }
    }
    if today >= from && today <= to {
        let now = x(today);
        let _ = write!(
            svg,
            r#"<line class="doc-gantt__today" x1="{now:.1}" y1="{}" x2="{now:.1}" y2="{height}"/><text class="doc-gantt__today-label" x="{:.1}" y="{}">Today</text>"#,
            AXIS - 12.0,
            now + 3.0,
            AXIS - 2.0
        );
    }
    svg.push_str("</svg>");
    svg
}
