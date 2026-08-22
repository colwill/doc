//! When somebody is away, from their own calendar.
//!
//! A lead records a holiday here, and that is still true. But the thing people actually keep up to
//! date is their calendar: they book leave, they mark themselves sick, and nobody then remembers
//! to tell the rota. So an event on a person's own calendar that says they are away counts exactly
//! as a recorded holiday does — the planner already decides everything through one `away` check,
//! so a turn that falls in one becomes a gap, cover is offered from whoever is left, and the lead
//! is told, with nothing here knowing where it came from.
//!
//! Nothing is copied. `calendar-events` answers `discovery/away` with the spans, and they are read each
//! time rather than mirrored into this plugin's own table, so a holiday somebody cancels stops
//! counting the moment they cancel it.

use chrono::NaiveDate;
use doc_plugin_sdk::Backend;
use uuid::Uuid;

use crate::store::Holiday;

/// Where a span came from, on a `Holiday` that was never recorded by hand.
pub const CALENDAR: &str = "calendar";

/// How far ahead the calendar is asked about; a rota plans at most a year out.
const AHEAD_DAYS: i64 = 400;

/// The spans the people named are away for, as holidays the planner treats like any other.
///
/// A calendar that cannot be reached gives none rather than failing the planning: a rota that
/// still plans, with the holidays a lead recorded, is better than one that stops.
pub async fn from_calendars(backend: &Backend, logins: &[String], from: NaiveDate) -> Vec<Holiday> {
    if logins.is_empty() {
        return Vec::new();
    }
    let to = from + chrono::Duration::days(AHEAD_DAYS);
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("users", &logins.join(","))
        .append_pair("from", &from.to_string())
        .append_pair("to", &to.to_string())
        .finish();
    let answer = backend.discovery("calendar-events", "GET", "away", Some(&query), None).await;
    let body = match answer {
        Ok((200, body)) => body,
        Ok((status, body)) => {
            tracing::debug!(
                status,
                detail = body["detail"].as_str().unwrap_or_default(),
                "the calendar would not say who is away"
            );
            return Vec::new();
        }
        Err(err) => {
            tracing::debug!(%err, "the calendar could not be asked who is away");
            return Vec::new();
        }
    };
    body["away"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|span| {
            let day = |key: &str| span[key].as_str()?.parse::<NaiveDate>().ok();
            let (starts_on, ends_on) = (day("starts_on")?, day("ends_on")?);
            Some(Holiday {
                // The event's own ID, so the same absence is the same row across a replan.
                id: span["event"].as_str().and_then(|id| id.parse().ok()).unwrap_or_else(Uuid::nil),
                user: span["user"].as_str()?.to_string(),
                // Nobody's, since nobody recorded it: the page uses this to offer a delete, and
                // an absence the calendar owns is cancelled in the calendar.
                user_id: Uuid::nil(),
                starts_on,
                ends_on: ends_on.max(starts_on),
                note: match (span["away"].as_str().unwrap_or_default(), span["note"].as_str()) {
                    ("sick", _) => "Off sick".to_string(),
                    (_, Some(title)) if !title.trim().is_empty() => title.to_string(),
                    _ => "Away".to_string(),
                },
                recorded_by: CALENDAR.to_string(),
                source: CALENDAR.to_string(),
            })
        })
        .collect()
}

/// The recorded holidays and the calendar's, together, which is what everything here plans by.
pub async fn everything(
    backend: &Backend,
    recorded: Vec<Holiday>,
    logins: &[String],
    from: NaiveDate,
) -> Vec<Holiday> {
    let mut all = recorded;
    all.extend(from_calendars(backend, logins, from).await);
    all
}
