//! When an event happens. Times are kept as wall-clock times in the event's own zone, so a weekly
//! 09:00 in London stays 09:00 through daylight saving, and exceptions name the wall-clock start
//! they skip.

use chrono::{DateTime, Duration, NaiveDate, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use rrule::RRuleSet;

/// The most occurrences one event gives for one question.
const MAX_OCCURRENCES: u16 = 2000;

pub const LOCAL: &str = "%Y-%m-%dT%H:%M:%S";

/// A wall-clock time, from `2026-09-21T09:00`, `2026-09-21T09:00:00`, or a date for all day.
pub fn wall(text: &str, all_day: bool) -> Result<NaiveDateTime, String> {
    let text = text.trim();
    if all_day {
        let day = NaiveDate::parse_from_str(text.get(..10).unwrap_or(text), "%Y-%m-%d")
            .map_err(|_| format!("`{text}` is not a date, such as 2026-09-21"))?;
        return Ok(day.and_hms_opt(0, 0, 0).unwrap_or_default());
    }
    NaiveDateTime::parse_from_str(text, LOCAL)
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M"))
        .map_err(|_| format!("`{text}` is not a time, such as 2026-09-21T09:00"))
}

/// The instant a wall-clock time means in `zone`; in a gap made by daylight saving, the time after it.
pub fn instant(zone: Tz, at: NaiveDateTime) -> DateTime<Tz> {
    zone.from_local_datetime(&at)
        .earliest()
        .or_else(|| zone.from_local_datetime(&(at + Duration::hours(1))).earliest())
        .unwrap_or_else(|| zone.from_utc_datetime(&at))
}

pub struct Timing<'a> {
    pub zone: Tz,
    pub start: NaiveDateTime,
    pub end: NaiveDateTime,
    pub rule: Option<&'a str>,
    pub skipped: &'a [String],
}

/// One occurrence: when it starts and ends, and the wall-clock start that names it.
#[derive(Debug, Clone)]
pub struct Occurrence {
    pub start: DateTime<Tz>,
    pub end: DateTime<Tz>,
    pub local: NaiveDateTime,
}

fn set(timing: &Timing<'_>) -> Result<RRuleSet, String> {
    let zone = timing.zone.name();
    let mut text = format!("DTSTART;TZID={zone}:{}", timing.start.format("%Y%m%dT%H%M%S"));
    if let Some(rule) = timing.rule {
        text.push_str(&format!("\nRRULE:{rule}"));
    }
    for skipped in timing.skipped {
        if let Ok(at) = NaiveDateTime::parse_from_str(skipped, LOCAL) {
            text.push_str(&format!("\nEXDATE;TZID={zone}:{}", at.format("%Y%m%dT%H%M%S")));
        }
    }
    text.parse::<RRuleSet>().map_err(|err| format!("the recurrence rule is not one: {err}"))
}

/// Every occurrence that overlaps `from` to `to`.
pub fn between(
    timing: &Timing<'_>,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Occurrence>, String> {
    let length = timing.end - timing.start;
    let occurrence = |start: DateTime<Tz>| {
        let local = start.naive_local();
        Occurrence { start, end: instant(timing.zone, local + length), local }
    };
    let Some(_) = timing.rule else {
        let only = occurrence(instant(timing.zone, timing.start));
        let overlaps = only.start.with_timezone(&Utc) < to && only.end.with_timezone(&Utc) > from;
        return Ok(if overlaps { vec![only] } else { Vec::new() });
    };
    let zone = rrule::Tz::Tz(timing.zone);
    let after = (from - length).with_timezone(&zone);
    let before = to.with_timezone(&zone);
    let found = set(timing)?.after(after).before(before).all(MAX_OCCURRENCES);
    Ok(found
        .dates
        .into_iter()
        .map(|start| occurrence(start.with_timezone(&timing.zone)))
        .filter(|occurrence| {
            occurrence.start.with_timezone(&Utc) < to && occurrence.end.with_timezone(&Utc) > from
        })
        .collect())
}

/// When the last occurrence ends, if the rule ends at all, found by expanding it once when saved.
pub fn last_end(timing: &Timing<'_>) -> Result<Option<DateTime<Utc>>, String> {
    let Some(rule) = timing.rule else {
        return Ok(Some(instant(timing.zone, timing.end).with_timezone(&Utc)));
    };
    let upper = rule.to_ascii_uppercase();
    if !upper.contains("COUNT=") && !upper.contains("UNTIL=") {
        set(timing)?;
        return Ok(None);
    }
    let found = set(timing)?.all(MAX_OCCURRENCES);
    if found.limited {
        return Err(format!("an event recurs at most {MAX_OCCURRENCES} times, or without end"));
    }
    let length = timing.end - timing.start;
    Ok(found
        .dates
        .last()
        .map(|start| instant(timing.zone, start.naive_local() + length).with_timezone(&Utc)))
}
