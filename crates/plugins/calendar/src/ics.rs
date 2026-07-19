//! Calendars as iCalendar (RFC 5545) for feeds: events keep their recurrence rules and exceptions, and
//! every time zone they use is described by its transitions, so calendar apps expand them the same way.

use std::collections::BTreeSet;

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Offset, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::Value;

/// Folded at 75 octets, as the format asks, without splitting a character.
fn line(out: &mut String, text: &str) {
    let mut width = 0;
    for c in text.chars() {
        if width + c.len_utf8() > 75 {
            out.push_str("\r\n ");
            width = 1;
        }
        out.push(c);
        width += c.len_utf8();
    }
    out.push_str("\r\n");
}

fn escaped(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace(';', "\\;")
        .replace(',', "\\,")
        .replace("\r\n", "\\n")
        .replace('\n', "\\n")
}

fn local(text: &str) -> Option<NaiveDateTime> {
    NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M"))
        .ok()
}

fn stamp(at: NaiveDateTime) -> String {
    at.format("%Y%m%dT%H%M%S").to_string()
}

/// `DTSTART;TZID=Europe/London:20260921T090000`, or the date alone for an all-day event.
fn when(name: &str, value: &str, all_day: bool, zone: &str) -> Option<String> {
    if all_day {
        let date = NaiveDate::parse_from_str(value.get(..10)?, "%Y-%m-%d").ok()?;
        return Some(format!("{name};VALUE=DATE:{}", date.format("%Y%m%d")));
    }
    let at = local(value)?;
    Some(match zone {
        "UTC" | "Etc/UTC" => format!("{name}:{}Z", stamp(at)),
        zone => format!("{name};TZID={zone}:{}", stamp(at)),
    })
}

fn offset(seconds: i32) -> String {
    let sign = if seconds < 0 { '-' } else { '+' };
    let seconds = seconds.abs();
    format!("{sign}{:02}{:02}", seconds / 3600, (seconds % 3600) / 60)
}

fn offset_at(zone: Tz, at: DateTime<Utc>) -> (i32, String) {
    let there = zone.from_utc_datetime(&at.naive_utc());
    (there.offset().fix().local_minus_utc(), there.format("%Z").to_string())
}

/// The zone's transitions around `years`, found day by day and then to the hour.
fn vtimezone(out: &mut String, name: &str, years: (i32, i32)) {
    let Ok(zone) = name.parse::<Tz>() else { return };
    let Some(start) = Utc.with_ymd_and_hms(years.0, 1, 1, 0, 0, 0).single() else { return };
    let Some(end) = Utc.with_ymd_and_hms(years.1 + 1, 1, 1, 0, 0, 0).single() else { return };
    line(out, "BEGIN:VTIMEZONE");
    line(out, &format!("TZID:{name}"));
    let (mut before, mut abbreviation) = offset_at(zone, start);
    let mut day = start;
    let mut changes = 0;
    while day < end {
        let next = day + Duration::days(1);
        let (after, _) = offset_at(zone, next);
        if after != before {
            let mut hour = day;
            while offset_at(zone, hour + Duration::hours(1)).0 == before {
                hour += Duration::hours(1);
            }
            let changed_at = hour + Duration::hours(1);
            let (now, name_now) = offset_at(zone, changed_at);
            let kind = if now > before { "DAYLIGHT" } else { "STANDARD" };
            let wall = changed_at.naive_utc() + Duration::seconds(i64::from(before));
            line(out, &format!("BEGIN:{kind}"));
            line(out, &format!("DTSTART:{}", stamp(wall)));
            line(out, &format!("TZOFFSETFROM:{}", offset(before)));
            line(out, &format!("TZOFFSETTO:{}", offset(now)));
            line(out, &format!("TZNAME:{name_now}"));
            line(out, &format!("END:{kind}"));
            before = now;
            abbreviation = name_now;
            changes += 1;
        }
        day = next;
    }
    if changes == 0 {
        line(out, "BEGIN:STANDARD");
        line(out, &format!("DTSTART:{}", stamp(start.naive_utc())));
        line(out, &format!("TZOFFSETFROM:{}", offset(before)));
        line(out, &format!("TZOFFSETTO:{}", offset(before)));
        line(out, &format!("TZNAME:{abbreviation}"));
        line(out, "END:STANDARD");
    }
    line(out, "END:VTIMEZONE");
}

fn text(event: &Value, key: &str) -> String {
    event[key].as_str().unwrap_or_default().to_string()
}

/// One VCALENDAR named `name`, from the events `calendar-events` answers with.
pub fn calendar(name: &str, events: &[Value]) -> String {
    let mut out = String::new();
    line(&mut out, "BEGIN:VCALENDAR");
    line(&mut out, "VERSION:2.0");
    line(&mut out, "PRODID:-//DOC//Calendar//EN");
    line(&mut out, "CALSCALE:GREGORIAN");
    line(&mut out, "METHOD:PUBLISH");
    line(&mut out, &format!("X-WR-CALNAME:{}", escaped(name)));
    let this_year = Utc::now().year();
    let zones: BTreeSet<String> = events
        .iter()
        .filter(|event| event["all_day"] != true)
        .map(|event| text(event, "timezone"))
        .filter(|zone| !zone.is_empty() && zone != "UTC" && zone != "Etc/UTC")
        .collect();
    for zone in &zones {
        vtimezone(&mut out, zone, (this_year - 1, this_year + 2));
    }
    for event in events {
        let all_day = event["all_day"] == true;
        let zone = text(event, "timezone");
        let (Some(start), Some(end)) = (
            when("DTSTART", &text(event, "start"), all_day, &zone),
            when("DTEND", &text(event, "end"), all_day, &zone),
        ) else {
            continue;
        };
        line(&mut out, "BEGIN:VEVENT");
        line(&mut out, &format!("UID:{}@doc", text(event, "id")));
        let updated = DateTime::parse_from_rfc3339(&text(event, "updated_at"))
            .map(|at| at.with_timezone(&Utc))
            .unwrap_or_else(|_| Utc::now());
        line(&mut out, &format!("DTSTAMP:{}", updated.format("%Y%m%dT%H%M%SZ")));
        line(&mut out, &start);
        line(&mut out, &end);
        if let Some(rule) = event["rrule"].as_str().filter(|rule| !rule.is_empty()) {
            line(&mut out, &format!("RRULE:{rule}"));
        }
        for skipped in event["exdates"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            if let Some(exdate) = when("EXDATE", skipped, all_day, &zone) {
                line(&mut out, &exdate);
            }
        }
        line(&mut out, &format!("SUMMARY:{}", escaped(&text(event, "title"))));
        for (key, field) in
            [("DESCRIPTION", "description"), ("LOCATION", "location"), ("URL", "link")]
        {
            let value = text(event, field);
            if !value.is_empty() {
                let value = if key == "URL" { value } else { escaped(&value) };
                line(&mut out, &format!("{key}:{value}"));
            }
        }
        line(&mut out, &format!("SEQUENCE:{}", event["sequence"].as_i64().unwrap_or(0)));
        let status = if event["cancelled"] == true { "CANCELLED" } else { "CONFIRMED" };
        line(&mut out, &format!("STATUS:{status}"));
        line(&mut out, "END:VEVENT");
    }
    line(&mut out, "END:VCALENDAR");
    out
}
