//! The plugin's routes. People reach events through `api/`, on calendars the Calendar plugin says
//! they can see; other plugins make and change their own events through `discovery/`, and Calendar reads
//! occurrences and feeds there. Every change is published as `plugin.calendar-events.*`.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use doc_plugin_sdk::{Backend, Caller, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::recur::{self, LOCAL, Occurrence, Timing};
use crate::store::{Event, Rsvp, Store};

type Answer = Result<(u16, Value), Refusal>;

const MAX_DAYS: i64 = 400;
const MAX_REMINDER: i64 = 7 * 24 * 60;

pub fn query(request: &Request, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn body<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T, Refusal> {
    let bytes = if request.body.is_empty() { &b"{}"[..] } else { &request.body[..] };
    serde_json::from_slice(bytes)
        .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))
}

fn id_of(text: &str) -> Result<Uuid, Refusal> {
    text.parse().map_err(|_| Refusal::missing("there is no such event"))
}

/// Who a call is for, as `user:<id>`, and their name.
fn principal(backend: &Backend) -> Result<(String, String), Refusal> {
    match backend.caller() {
        Some(Caller { kind, id: Some(id), label, .. }) if kind == "user" || kind == "service" => {
            Ok((format!("{kind}:{id}"), label.clone().unwrap_or_else(|| id.clone())))
        }
        _ => Err(Refusal::forbidden("events are for people and service accounts")),
    }
}

/// Where people see an event in DOC.
pub fn page(id: Uuid) -> String {
    let base = std::env::var("DOC_PUBLIC_URL").unwrap_or_default();
    format!("{}/p/calendar/events/{id}", base.trim_end_matches('/'))
}

/// Which calendars the caller can see, asked of the Calendar plugin as them once each.
struct Sight<'a> {
    backend: &'a Backend,
    seen: BTreeMap<Uuid, Option<Value>>,
}

impl<'a> Sight<'a> {
    fn new(backend: &'a Backend) -> Self {
        Self { backend, seen: BTreeMap::new() }
    }

    async fn calendar(&mut self, id: Uuid) -> Option<Value> {
        if let Some(found) = self.seen.get(&id) {
            return found.clone();
        }
        let found =
            match self.backend.ask("calendar", "GET", &format!("calendars/{id}"), None, None).await
            {
                Ok((200, calendar)) => Some(calendar),
                _ => None,
            };
        self.seen.insert(id, found.clone());
        found
    }

    async fn visible(&mut self, id: Uuid) -> Result<Value, Refusal> {
        self.calendar(id)
            .await
            .ok_or_else(|| Refusal::missing("there is no such calendar, or you cannot see it"))
    }
}

/// `from` and `to`, the next four weeks unless asked otherwise.
fn range(request: &Request) -> Result<(DateTime<Utc>, DateTime<Utc>), Refusal> {
    let parse = |key: &str| -> Result<Option<DateTime<Utc>>, Refusal> {
        query(request, key)
            .map(|text| {
                DateTime::parse_from_rfc3339(&text)
                    .map(|at| at.with_timezone(&Utc))
                    .or_else(|_| {
                        chrono::NaiveDate::parse_from_str(&text, "%Y-%m-%d")
                            .map(|day| day.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc())
                    })
                    .map_err(|_| {
                        Refusal::bad(format!(
                            "`{text}` is not a time, such as 2026-09-21T09:00:00Z"
                        ))
                    })
            })
            .transpose()
    };
    let from = parse("from")?.unwrap_or_else(Utc::now);
    let to = parse("to")?.unwrap_or(from + Duration::days(28));
    if to <= from || (to - from).num_days() > MAX_DAYS {
        return Err(Refusal::bad(format!(
            "`to` comes after `from`, at most {MAX_DAYS} days later"
        )));
    }
    Ok((from, to))
}

fn calendars_of(request: &Request) -> Result<Vec<Uuid>, Refusal> {
    let listed =
        query(request, "calendars").or_else(|| query(request, "calendar")).unwrap_or_default();
    listed
        .split(',')
        .filter(|id| !id.trim().is_empty())
        .map(|id| {
            id.trim().parse().map_err(|_| Refusal::bad(format!("`{id}` is not a calendar's ID")))
        })
        .collect()
}

fn timing(event: &Event) -> Result<(Timing<'_>, Tz), Refusal> {
    let zone: Tz = event
        .timezone
        .parse()
        .map_err(|_| Refusal::unavailable("a stored event has no time zone"))?;
    let start = recur::wall(&event.starts_local, false).map_err(Refusal::unavailable)?;
    let end = recur::wall(&event.ends_local, false).map_err(Refusal::unavailable)?;
    Ok((Timing { zone, start, end, rule: event.rrule.as_deref(), skipped: &event.exdates }, zone))
}

fn occurrence_shown(event: &Event, occurrence: &Occurrence) -> Value {
    let (start, end) = match event.all_day {
        true => (
            occurrence.start.format("%Y-%m-%d").to_string(),
            occurrence.end.format("%Y-%m-%d").to_string(),
        ),
        false => (occurrence.start.to_rfc3339(), occurrence.end.to_rfc3339()),
    };
    json!({
        "event": event.id,
        "calendar": event.calendar,
        "occurrence": occurrence.local.format(LOCAL).to_string(),
        "title": event.title,
        "start": start,
        "end": end,
        "all_day": event.all_day,
        "away": event.away,
        "timezone": event.timezone,
        "location": event.location,
        "link": event.link,
        "recurring": event.rrule.is_some(),
        "resource": event.resource,
        "source_plugin": event.source_plugin,
    })
}

/// Every occurrence on these calendars between `from` and `to`, soonest first.
async fn occurrences(
    store: &Store<'_>,
    calendars: &[Uuid],
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> Result<Vec<Value>, Refusal> {
    let mut found: Vec<(DateTime<Utc>, Value)> = Vec::new();
    for event in store.around(calendars, &from.to_rfc3339(), &to.to_rfc3339()).await? {
        let (timing, _) = timing(&event)?;
        for occurrence in recur::between(&timing, from, to).map_err(Refusal::unavailable)? {
            found.push((
                occurrence.start.with_timezone(&Utc),
                occurrence_shown(&event, &occurrence),
            ));
        }
    }
    found.sort_by_key(|(start, _)| *start);
    Ok(found.into_iter().map(|(_, shown)| shown).collect())
}

/// An event as its stored times and rule, with who has answered, for pages and feeds.
fn event_shown(event: &Event, rsvps: &[Rsvp]) -> Value {
    let (start, end) = match event.all_day {
        true => (
            event.starts_local.get(..10).unwrap_or_default().to_string(),
            event.ends_local.get(..10).unwrap_or_default().to_string(),
        ),
        false => (event.starts_local.clone(), event.ends_local.clone()),
    };
    json!({
        "id": event.id,
        "calendar": event.calendar,
        "calendar_resource": event.calendar_resource,
        "title": event.title,
        "description": event.description,
        "location": event.location,
        "link": event.link,
        "all_day": event.all_day,
        "away": event.away,
        "timezone": event.timezone,
        "start": start,
        "end": end,
        "rrule": event.rrule,
        "exdates": event.exdates,
        "reminder_minutes": event.reminder_minutes,
        "attendees": event.attendees,
        "resource": event.resource,
        "source_plugin": event.source_plugin,
        "created_by": event.created_by_label,
        "sequence": event.sequence,
        "updated_at": event.updated_at,
        "url": page(event.id),
        "rsvps": rsvps.iter().filter(|rsvp| rsvp.event == event.id).map(|rsvp| json!({ "who": rsvp.label, "occurrence": rsvp.occurrence, "response": rsvp.response })).collect::<Vec<_>>(),
    })
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Details {
    pub calendar: Option<Uuid>,
    /// For another plugin: the resource whose calendar it goes on, as `kind:name`.
    pub on: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub link: Option<String>,
    pub all_day: Option<bool>,
    /// `holiday`, `sick` or `other` to say the person whose calendar it is will not be working.
    pub away: Option<String>,
    pub timezone: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub rrule: Option<String>,
    pub exdates: Option<Vec<String>>,
    pub reminder_minutes: Option<i64>,
    pub attendees: Option<Vec<Value>>,
    pub resource: Option<String>,
}

fn normalised_resource(text: &str) -> Result<String, Refusal> {
    match text.trim().split_once(':') {
        Some((kind, name)) if !name.trim().is_empty() => {
            let kind: String = kind
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .collect::<String>()
                .to_ascii_lowercase();
            Ok(format!("{kind}:{}", name.trim()))
        }
        _ => Err(Refusal::bad(format!("write the resource as kind:name, not `{text}`"))),
    }
}

/// A time as a wall-clock time in `zone`: given as one, or as an instant with an offset.
fn local_in(zone: Tz, text: &str, all_day: bool) -> Result<String, Refusal> {
    if !all_day && let Ok(at) = DateTime::parse_from_rfc3339(text.trim()) {
        return Ok(at.with_timezone(&zone).naive_local().format(LOCAL).to_string());
    }
    Ok(recur::wall(text, all_day).map_err(Refusal::bad)?.format(LOCAL).to_string())
}

/// `event` with `details` applied and everything checked, its first start and last end worked out.
fn apply(mut event: Event, details: Details, new: bool) -> Result<Event, Refusal> {
    if let Some(title) = details.title {
        event.title = title.trim().to_string();
    }
    if event.title.is_empty() || event.title.chars().count() > 200 {
        return Err(Refusal::bad("an event's title is 1 to 200 characters"));
    }
    if let Some(description) = details.description {
        event.description = description.trim().chars().take(10_000).collect();
    }
    if let Some(location) = details.location {
        event.location = location.trim().chars().take(500).collect();
    }
    if let Some(link) = details.link {
        let link = link.trim().to_string();
        if !link.is_empty() && !(link.starts_with("https://") || link.starts_with("http://")) {
            return Err(Refusal::bad("a link is an http or https URL"));
        }
        event.link = link;
    }
    if let Some(away) = &details.away {
        let away = away.trim().to_ascii_lowercase();
        if !matches!(away.as_str(), "" | "holiday" | "sick" | "other") {
            return Err(Refusal::bad(
                "somebody is away for a holiday, because they are sick, or for something else",
            ));
        }
        // Only on a person's own calendar: a meeting in a team's calendar is not anybody being
        // away, and letting it say so would take people off rotas they are on.
        if !away.is_empty() && !event.calendar_resource.starts_with("user:") {
            return Err(Refusal::bad(
                "only an event on somebody's own calendar can say they are away",
            ));
        }
        event.away = away;
    }
    if let Some(all_day) = details.all_day {
        event.all_day = all_day;
    }
    if let Some(zone) = details.timezone {
        zone.trim().parse::<Tz>().map_err(|_| {
            Refusal::bad(format!("`{zone}` is not a time zone, such as Europe/London"))
        })?;
        event.timezone = zone.trim().to_string();
    }
    let zone: Tz =
        event.timezone.parse().map_err(|_| Refusal::bad("the event's time zone is not one"))?;
    if let Some(start) = details.start {
        event.starts_local = local_in(zone, &start, event.all_day)?;
    }
    if let Some(end) = details.end {
        event.ends_local = local_in(zone, &end, event.all_day)?;
    } else if new {
        let start = recur::wall(&event.starts_local, false).map_err(Refusal::bad)?;
        let length = if event.all_day { Duration::days(1) } else { Duration::hours(1) };
        event.ends_local = (start + length).format(LOCAL).to_string();
    }
    if let Some(rule) = details.rrule {
        let rule = rule.trim().trim_start_matches("RRULE:").to_string();
        event.rrule = (!rule.is_empty()).then_some(rule);
    }
    if let Some(skipped) = details.exdates {
        event.exdates = skipped
            .iter()
            .map(|at| local_in(zone, at, event.all_day))
            .collect::<Result<Vec<_>, _>>()?;
    }
    if let Some(minutes) = details.reminder_minutes {
        if !(0..=MAX_REMINDER).contains(&minutes) {
            return Err(Refusal::bad(format!(
                "a reminder comes 0 to {MAX_REMINDER} minutes before"
            )));
        }
        event.reminder_minutes = Some(minutes);
    }
    if let Some(attendees) = details.attendees {
        let valid = attendees.iter().all(|who| who["user"].is_string() || who["team"].is_string());
        if !valid || attendees.len() > 200 {
            return Err(Refusal::bad(
                "attendees are up to 200 of {\"user\": login} or {\"team\": name}",
            ));
        }
        event.attendees = attendees;
    }
    if let Some(resource) = details.resource {
        event.resource =
            (!resource.trim().is_empty()).then(|| normalised_resource(&resource)).transpose()?;
    }
    let start = recur::wall(&event.starts_local, false).map_err(Refusal::bad)?;
    let end = recur::wall(&event.ends_local, false).map_err(Refusal::bad)?;
    if end <= start {
        return Err(Refusal::bad("an event ends after it starts"));
    }
    let timing = Timing { zone, start, end, rule: event.rrule.as_deref(), skipped: &event.exdates };
    event.until_utc = recur::last_end(&timing).map_err(Refusal::bad)?.map(|at| at.to_rfc3339());
    event.starts_utc = recur::instant(zone, start).with_timezone(&Utc).to_rfc3339();
    if !new {
        event.sequence += 1;
    }
    Ok(event)
}

fn blank(
    calendar: Uuid,
    calendar_resource: String,
    created_by: String,
    label: String,
    source: Option<String>,
) -> Event {
    Event {
        id: Uuid::now_v7(),
        calendar,
        calendar_resource,
        title: String::new(),
        description: String::new(),
        location: String::new(),
        link: String::new(),
        all_day: false,
        away: String::new(),
        timezone: "UTC".into(),
        starts_local: String::new(),
        ends_local: String::new(),
        rrule: None,
        exdates: Vec::new(),
        starts_utc: String::new(),
        until_utc: None,
        reminder_minutes: None,
        attendees: Vec::new(),
        resource: None,
        source_plugin: source,
        created_by,
        created_by_label: label,
        sequence: 0,
        created_at: String::new(),
        updated_at: String::new(),
    }
}

async fn announce(backend: &Backend, what: &str, event: &Event) {
    let payload = json!({
        "event": event.id,
        "calendar": event.calendar,
        "calendar_resource": event.calendar_resource,
        "title": event.title,
        "start": event.starts_local,
        "timezone": event.timezone,
        "resource": event.resource,
        "source_plugin": event.source_plugin,
        // So anything that cares who is working — rotas, recurring processes — hears it without
        // having to read the calendar itself.
        "away": event.away,
        "url": page(event.id),
    });
    if let Err(err) =
        backend.publish(&format!("plugin.calendar-events.event.{what}"), payload).await
    {
        tracing::warn!(%err, event = %event.id, "an event change was not published");
    }
}

pub async fn handle(backend: &Backend, request: Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let answer = match segments.as_slice() {
        ["discovery", route @ ..] => discovery(backend, &request, route).await,
        ["api", route @ ..] => api(backend, &request, route).await,
        _ => Err(Refusal::missing("no such route")),
    };
    match answer {
        Ok((204, _)) => Response::new(204, "application/json", Vec::new()),
        Ok((status, value)) => Response::new(
            status,
            "application/json",
            serde_json::to_vec(&value).unwrap_or_default(),
        ),
        Err(refusal) => refusal.response(),
    }
}

async fn api(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let store = Store(backend);
    let mut sight = Sight::new(backend);
    match (request.method.as_str(), path) {
        ("GET", ["occurrences"]) => {
            let (from, to) = range(request)?;
            let mut calendars = Vec::new();
            for id in calendars_of(request)? {
                sight.visible(id).await?;
                calendars.push(id);
            }
            let listed = occurrences(&store, &calendars, from, to).await?;
            Ok((200, json!({ "from": from, "to": to, "occurrences": listed })))
        }
        ("GET", ["events", id]) => {
            let event = store
                .event(id_of(id)?)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such event"))?;
            let calendar = sight.visible(event.calendar).await?;
            let rsvps = store.rsvps(&[event.id]).await?;
            let mut shown = event_shown(&event, &rsvps);
            shown["calendar_name"] = calendar["name"].clone();
            Ok((200, shown))
        }
        ("POST", ["events"]) => {
            let (me, label) = principal(backend)?;
            let details: Details = body(request)?;
            let calendar_id = details
                .calendar
                .ok_or_else(|| Refusal::bad("name the calendar the event goes on"))?;
            let calendar = sight.visible(calendar_id).await?;
            let resource = calendar["resource"].as_str().unwrap_or_default().to_string();
            let event = apply(blank(calendar_id, resource, me, label, None), details, true)?;
            store.save(&event).await?;
            announce(backend, "created", &event).await;
            Ok((201, event_shown(&event, &[])))
        }
        ("PATCH" | "DELETE", ["events", id]) => {
            let event = store
                .event(id_of(id)?)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such event"))?;
            sight.visible(event.calendar).await?;
            if let Some(plugin) = &event.source_plugin {
                return Err(Refusal::forbidden(format!(
                    "{plugin} made this event, and changes it"
                )));
            }
            if request.method == "DELETE" {
                store.delete(event.id).await?;
                announce(backend, "deleted", &event).await;
                return Ok((204, Value::Null));
            }
            let event = apply(event, body(request)?, false)?;
            store.save(&event).await?;
            announce(backend, "updated", &event).await;
            Ok((200, event_shown(&event, &store.rsvps(&[event.id]).await?)))
        }
        ("POST", ["rsvps"]) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Answered {
                event: Uuid,
                #[serde(default)]
                occurrence: Option<String>,
                response: String,
            }
            let (me, label) = principal(backend)?;
            let asked: Answered = body(request)?;
            if !matches!(asked.response.as_str(), "yes" | "no" | "maybe") {
                return Err(Refusal::bad("answer yes, no or maybe"));
            }
            let event = store
                .event(asked.event)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such event"))?;
            sight.visible(event.calendar).await?;
            let occurrence = match asked.occurrence.filter(|at| !at.trim().is_empty()) {
                Some(at) => {
                    let zone: Tz = event.timezone.parse().unwrap_or(Tz::UTC);
                    local_in(zone, &at, event.all_day)?
                }
                None => String::new(),
            };
            let rsvp = Rsvp {
                event: event.id,
                occurrence,
                principal: me,
                label,
                response: asked.response,
                updated_at: String::new(),
            };
            store.answer(&rsvp).await?;
            let payload = json!({ "event": event.id, "occurrence": rsvp.occurrence, "who": rsvp.label, "response": rsvp.response, "calendar_resource": event.calendar_resource, "source_plugin": event.source_plugin });
            let _ = backend.publish("plugin.calendar-events.rsvp.changed", payload).await;
            Ok((200, event_shown(&event, &store.rsvps(&[event.id]).await?)))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

/// When the people named are not working, from the events on their own calendars that say so.
///
/// This is the one place anything else asks. A rota deciding whether a turn falls to somebody who
/// is away, and a recurring process deciding whether the person it falls to will be there, both
/// read the same spans from here rather than keeping a copy of the calendar that drifts from it.
/// Spans come back as whole days, because that is the question being asked: somebody off from
/// Tuesday afternoon is not working on Tuesday.
async fn away(store: &Store<'_>, request: &Request) -> Answer {
    let logins: Vec<String> = query(request, "users")
        .or_else(|| query(request, "user"))
        .unwrap_or_default()
        .split(',')
        .map(|login| login.trim().to_string())
        .filter(|login| !login.is_empty())
        .collect();
    if logins.is_empty() {
        return Err(Refusal::bad("name at least one person, as `users`"));
    }
    let (from, to) = range(request)?;
    // A person's own calendar is their own resource; nothing else says somebody is away.
    let calendars: Vec<String> = logins.iter().map(|login| format!("user:{login}")).collect();
    let mut spans: Vec<Value> = Vec::new();
    for event in store.away_on(&calendars, &from.to_rfc3339(), &to.to_rfc3339()).await? {
        let (timing, zone) = timing(&event)?;
        let login = event.calendar_resource.strip_prefix("user:").unwrap_or_default().to_string();
        for occurrence in recur::between(&timing, from, to).map_err(Refusal::unavailable)? {
            // The days it touches where they are, not where the server is.
            let starts_on = occurrence.start.with_timezone(&zone).date_naive();
            let ends = occurrence.end.with_timezone(&zone);
            // An event ending at midnight ends the day before: it does not take that day.
            let ends_on = match ends.time() == chrono::NaiveTime::MIN {
                true => ends.date_naive().pred_opt().unwrap_or(starts_on).max(starts_on),
                false => ends.date_naive(),
            };
            spans.push(json!({
                "user": login,
                "starts_on": starts_on,
                "ends_on": ends_on,
                "away": event.away,
                "note": event.title,
                "event": event.id,
            }));
        }
    }
    spans.sort_by(|one, two| {
        (one["user"].as_str(), one["starts_on"].as_str())
            .cmp(&(two["user"].as_str(), two["starts_on"].as_str()))
    });
    Ok((200, json!({ "away": spans })))
}

/// Other plugins: Calendar reading occurrences and feeds, and others keeping their own events.
async fn discovery(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let plugin = match backend.caller() {
        Some(Caller { kind, id: Some(id), .. }) if kind == "plugin" => id.clone(),
        _ => return Err(Refusal::forbidden("discovery routes are for plugins")),
    };
    let store = Store(backend);
    match (request.method.as_str(), path) {
        // Any plugin may ask who is away: it says only that somebody is not working and when,
        // which is what a rota or a process needs and no more of their calendar than that.
        ("GET", ["away"]) => away(&store, request).await,
        ("GET", ["occurrences"]) if plugin == "calendar" => {
            let (from, to) = range(request)?;
            let listed = occurrences(&store, &calendars_of(request)?, from, to).await?;
            Ok((200, json!({ "occurrences": listed })))
        }
        ("GET", ["events"]) if plugin == "calendar" => {
            let events = store.on(&calendars_of(request)?).await?;
            Ok((
                200,
                json!({ "events": events.iter().map(|event| event_shown(event, &[])).collect::<Vec<_>>() }),
            ))
        }
        ("POST", ["calendars", id, "delete"]) if plugin == "calendar" => {
            store
                .delete_calendar(id.parse().map_err(|_| Refusal::bad("not a calendar's ID"))?)
                .await?;
            Ok((204, Value::Null))
        }
        ("POST", ["events"]) => {
            let details: Details = body(request)?;
            let on = details.on.clone().ok_or_else(|| {
                Refusal::bad("name the resource whose calendar the event goes on, as `on`")
            })?;
            let resource = normalised_resource(&on)?;
            let (kind, name) = resource.split_once(':').unwrap_or_default();
            let (status, calendar) = backend
                .discovery(
                    "calendar",
                    "GET",
                    &format!("calendars/for/{kind}/{name}"),
                    // Calendar decides whom a person's own calendar takes events from by the
                    // plugin that made them, which only this plugin can say.
                    Some(&format!("for={plugin}")),
                    None,
                )
                .await
                .map_err(|err| {
                    Refusal::unavailable(format!("the Calendar plugin could not be asked: {err}"))
                })?;
            if status != 200 {
                return Err(Refusal::unavailable(format!(
                    "the Calendar plugin answered {status}: {}",
                    calendar["detail"]
                )));
            }
            let calendar_id: Uuid = calendar["id"]
                .as_str()
                .and_then(|id| id.parse().ok())
                .ok_or_else(|| Refusal::unavailable("the Calendar plugin gave no calendar"))?;
            let details = Details {
                resource: details.resource.clone().or(Some(resource.clone())),
                ..details
            };
            let blank = blank(
                calendar_id,
                resource,
                format!("plugin:{plugin}"),
                plugin.clone(),
                Some(plugin.clone()),
            );
            let event = apply(blank, details, true)?;
            store.save(&event).await?;
            announce(backend, "created", &event).await;
            Ok((201, event_shown(&event, &[])))
        }
        ("PATCH" | "POST", ["events", id]) | ("POST", ["events", id, "delete"]) => {
            let event = store
                .event(id_of(id)?)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such event"))?;
            if event.source_plugin.as_deref() != Some(plugin.as_str()) {
                return Err(Refusal::forbidden("a plugin changes only the events it made"));
            }
            if path.last() == Some(&"delete") {
                store.delete(event.id).await?;
                announce(backend, "deleted", &event).await;
                return Ok((204, Value::Null));
            }
            let event = apply(event, body(request)?, false)?;
            store.save(&event).await?;
            announce(backend, "updated", &event).await;
            Ok((200, event_shown(&event, &[])))
        }
        ("GET", ["events", id]) => {
            let event = store
                .event(id_of(id)?)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such event"))?;
            if event.source_plugin.as_deref() != Some(plugin.as_str()) && plugin != "calendar" {
                return Err(Refusal::forbidden("a plugin reads only the events it made"));
            }
            Ok((200, event_shown(&event, &store.rsvps(&[event.id]).await?)))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

/// Publishes `reminder.due` for every occurrence whose reminder time has come in the last few minutes.
pub async fn remind(backend: &Backend) -> Result<Value, Refusal> {
    let store = Store(backend);
    let now = Utc::now();
    let lookback = Duration::minutes(5);
    let horizon = now + Duration::minutes(MAX_REMINDER) + Duration::minutes(1);
    let mut sent = Vec::new();
    for event in store.reminded(&(now - lookback).to_rfc3339(), &horizon.to_rfc3339()).await? {
        let lead = Duration::minutes(event.reminder_minutes.unwrap_or_default());
        let (timing, _) = timing(&event)?;
        let window =
            recur::between(&timing, now - lookback + lead, now + lead + Duration::seconds(1))
                .map_err(Refusal::unavailable)?;
        for occurrence in window {
            let due = occurrence.start.with_timezone(&Utc) - lead;
            if due > now
                || due < now - lookback
                || occurrence.start.with_timezone(&Utc) < now - lookback + lead
            {
                continue;
            }
            let named = occurrence.local.format(LOCAL).to_string();
            if !store.remind(event.id, &named).await? {
                continue;
            }
            let payload = json!({
                "event": event.id,
                "occurrence": named,
                "calendar": event.calendar_resource,
                "calendar_id": event.calendar,
                "title": event.title,
                "starts_at": occurrence.start.to_rfc3339(),
                "location": event.location,
                "link": event.link,
                "resource": event.resource,
                "url": page(event.id),
            });
            match backend.publish("plugin.calendar-events.reminder.due", payload).await {
                Ok(_) => sent.push(json!({ "event": event.id, "occurrence": named })),
                Err(err) => tracing::warn!(%err, event = %event.id, "a reminder was not published"),
            }
        }
    }
    Ok(json!({ "reminded": sent }))
}
