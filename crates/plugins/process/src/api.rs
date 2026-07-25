//! The plugin's routes. A process is seen by whoever can see its resource in Resource Definitions,
//! asked as them; its owner and admins change it, and its assignees tick off and finish its
//! occurrences. The `occurrences` schedule plans them and publishes `plugin.process.occurrence.*`.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, NaiveDateTime, NaiveTime, Utc};
use chrono_tz::Tz;
use doc_plugin_sdk::{Backend, Caller, Request, Response};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use url::form_urlencoded::byte_serialize;
use uuid::Uuid;

use crate::Refusal;
use crate::cadence::{self, Cadence, LOCAL, Rule};
use crate::store::{Assignee, Item, Occurrence, Process, Store};

type Answer = Result<(u16, Value), Refusal>;

/// How far ahead occurrences are planned; the next one always is, however far off.
const HORIZON_DAYS: i64 = 35;
/// How far back a tick plans occurrences it could not before, such as while the plugin was down.
const LOOKBACK_HOURS: i64 = 48;
const MAX_REMINDER: i64 = 7 * 24 * 60;
const MAX_GRACE: i64 = 30 * 24 * 60;
const MAX_ITEMS: usize = 50;
const MAX_ASSIGNEES: usize = 50;
/// The most calendar events one tick adds for occurrences still without one.
const EVENTS_PER_TICK: usize = 25;

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

fn id_of(text: &str, what: &str) -> Result<Uuid, Refusal> {
    text.parse().map_err(|_| Refusal::missing(format!("there is no such {what}")))
}

fn encoded(text: &str) -> String {
    byte_serialize(text.as_bytes()).collect::<String>().replace('+', "%20")
}

/// The login of whoever a call is for, a person or a service account.
pub fn me(backend: &Backend) -> Result<String, Refusal> {
    match backend.caller() {
        Some(Caller { kind, id: Some(id), label, .. }) if kind == "user" || kind == "service" => {
            Ok(label.clone().unwrap_or_else(|| id.clone()))
        }
        _ => Err(Refusal::forbidden("processes are for people and service accounts")),
    }
}

fn admin(backend: &Backend) -> bool {
    backend.caller().is_some_and(|caller| caller.admin)
}

/// Where people see this plugin's pages in DOC.
pub fn page(path: &str) -> String {
    let base = std::env::var("DOC_PUBLIC_URL").unwrap_or_default();
    format!("{}/p/process/{path}", base.trim_end_matches('/'))
}

/// `kind:name`, with the kind in one spelling.
pub fn normalised(text: &str) -> Result<String, Refusal> {
    let refused = || {
        Refusal::bad(format!(
            "write the resource as kind:name, such as service:card-gateway, not `{text}`"
        ))
    };
    let (kind, name) = text.trim().split_once(':').ok_or_else(refused)?;
    let kind: String =
        kind.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    match (kind.is_empty(), name.trim()) {
        (false, name) if !name.is_empty() => Ok(format!("{kind}:{name}")),
        _ => Err(refused()),
    }
}

/// What Resource Definitions shows the caller, asked as them once for each resource.
pub struct Sight<'a> {
    backend: &'a Backend,
    seen: BTreeMap<String, Result<Value, u16>>,
}

impl<'a> Sight<'a> {
    pub fn new(backend: &'a Backend) -> Self {
        Self { backend, seen: BTreeMap::new() }
    }

    async fn resource(&mut self, resource: &str) -> Result<Value, u16> {
        if let Some(found) = self.seen.get(resource) {
            return found.clone();
        }
        let (kind, name) = resource.split_once(':').unwrap_or_default();
        let path: Vec<String> = name.split('/').map(encoded).collect();
        let route = format!("resources/{kind}/{}", path.join("/"));
        let found = match self.backend.ask("resources", "GET", &route, None, None).await {
            Ok((200, answer)) => Ok(answer),
            Ok((status, _)) => Err(status),
            Err(_) => Err(503),
        };
        self.seen.insert(resource.to_string(), found.clone());
        found
    }

    pub async fn sees(&mut self, resource: &str) -> bool {
        self.resource(resource).await.is_ok()
    }

    async fn member(&mut self, team: &str, login: &str) -> bool {
        let Ok(found) = self.resource(&format!("team:{team}")).await else { return false };
        found["connections"].as_array().into_iter().flatten().any(|connection| {
            connection["kind"] == "User" && connection["name"].as_str() == Some(login)
        })
    }

    pub async fn visible(&mut self, process: Process) -> Result<Process, Refusal> {
        match self.sees(&process.resource).await {
            true => Ok(process),
            false => {
                Err(Refusal::missing("there is no such process, or you cannot see its resource"))
            }
        }
    }

    /// Whether the process is `login`'s to do: by name, through a team, or as its owner when no one else is.
    pub async fn assigned(&mut self, process: &Process, login: &str) -> bool {
        if process.assignees.is_empty() {
            return process.owner == login;
        }
        for assignee in &process.assignees {
            let found = match assignee {
                Assignee::User(user) => user == login,
                Assignee::Team(team) => self.member(team, login).await,
            };
            if found {
                return true;
            }
        }
        false
    }

    pub async fn mine(&mut self, process: &Process, login: &str) -> bool {
        process.owner == login || self.assigned(process, login).await
    }
}

pub fn rule_of(process: &Process) -> Result<Rule, Refusal> {
    let broken = || Refusal::unavailable(format!("{}'s schedule cannot be read", process.title));
    let number = |value: i64| u32::try_from(value).unwrap_or(1);
    Ok(Rule {
        cadence: Cadence::parse(&process.cadence).ok_or_else(broken)?,
        weekday: number(process.weekday),
        month: number(process.month),
        day: number(process.day),
        time: NaiveTime::parse_from_str(&process.at_time, "%H:%M").map_err(|_| broken())?,
        zone: process.timezone.parse().map_err(|_| broken())?,
    })
}

fn zone_of(process: &Process) -> Tz {
    process.timezone.parse().unwrap_or(Tz::UTC)
}

fn instant(text: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(text).ok().map(|at| at.with_timezone(&Utc))
}

/// When an occurrence is due, with the offset of the process's zone.
pub fn due_in(occurrence: &Occurrence, process: &Process) -> String {
    instant(&occurrence.due_at).map_or_else(
        || occurrence.due_at.clone(),
        |at| at.with_timezone(&zone_of(process)).to_rfc3339(),
    )
}

pub fn assignees_text(process: &Process) -> String {
    match process.assignees.is_empty() {
        true => process.owner.clone(),
        false => process.assignees.iter().map(Assignee::shown).collect::<Vec<_>>().join(", "),
    }
}

pub fn occurrence_shown(occurrence: &Occurrence, process: &Process) -> Value {
    json!({
        "id": occurrence.id,
        "process": process.id,
        "title": process.title,
        "resource": process.resource,
        "due": occurrence.due_local,
        "due_at": due_in(occurrence, process),
        "timezone": process.timezone,
        "status": occurrence.status,
        "missed": occurrence.missed_at.is_some(),
        "checklist": occurrence.checklist,
        "note": occurrence.note,
        "finished_by": occurrence.finished_by,
        "finished_at": occurrence.finished_at,
        "reminded_at": occurrence.reminded_at,
        "missed_at": occurrence.missed_at,
        "event": occurrence.event,
        "assignees": assignees_text(process),
        "url": page(&format!("occurrences/{}", occurrence.id)),
    })
}

pub fn process_shown(process: &Process, next: Option<&Occurrence>, missed: usize) -> Value {
    json!({
        "id": process.id,
        "resource": process.resource,
        "title": process.title,
        "description": process.description,
        "cadence": process.cadence,
        "weekday": process.weekday,
        "month": process.month,
        "day": process.day,
        "time": process.at_time,
        "timezone": process.timezone,
        "schedule": rule_of(process).map(|rule| rule.describe()).unwrap_or_default(),
        "duration_minutes": process.duration_minutes,
        "remind_minutes": process.remind_minutes,
        "grace_minutes": process.grace_minutes,
        "owner": process.owner,
        "assignees": process.assignees,
        "checklist": process.checklist,
        "next": next.map(|occurrence| occurrence_shown(occurrence, process)),
        "missed": missed,
        "created_by": process.created_by,
        "created_at": process.created_at,
        "updated_at": process.updated_at,
        "url": page(&format!("processes/{}", process.id)),
    })
}

fn maybe<'de, D: Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(deserializer).map(Some)
}

/// A process as it is asked for; what is left out stays as it was, and `null` clears the reminder.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields, default)]
pub struct Details {
    pub resource: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub cadence: Option<String>,
    pub weekday: Option<Value>,
    pub month: Option<Value>,
    pub day: Option<i64>,
    pub time: Option<String>,
    pub timezone: Option<String>,
    pub duration_minutes: Option<i64>,
    #[serde(deserialize_with = "maybe")]
    pub remind_minutes: Option<Option<i64>>,
    pub grace_minutes: Option<i64>,
    pub owner: Option<String>,
    pub assignees: Option<Vec<Assignee>>,
    pub checklist: Option<Vec<String>>,
}

/// A weekday or month by number from 1, or by the start of its name.
fn numbered(value: &Value, names: &[&str], what: &str) -> Result<i64, Refusal> {
    let refused = || Refusal::bad(format!("{value} is not a {what}"));
    match value {
        Value::Number(number) => number.as_i64().ok_or_else(refused),
        Value::String(text) => {
            let text = text.trim().to_ascii_lowercase();
            if let Ok(number) = text.parse() {
                return Ok(number);
            }
            let found = names
                .iter()
                .position(|name| text.len() >= 3 && name.to_ascii_lowercase().starts_with(&text));
            found.and_then(|index| i64::try_from(index + 1).ok()).ok_or_else(refused)
        }
        _ => Err(refused()),
    }
}

fn login(text: &str, what: &str) -> Result<String, Refusal> {
    let text = text.trim().trim_start_matches('@');
    match text.is_empty() || text.len() > 100 || text.chars().any(char::is_whitespace) {
        true => Err(Refusal::bad(format!("`{text}` is not a login, for the {what}"))),
        false => Ok(text.to_string()),
    }
}

/// `process` with `details` applied and checked, and whether its schedule changed.
async fn apply(
    sight: &mut Sight<'_>,
    mut process: Process,
    details: Details,
) -> Result<(Process, bool), Refusal> {
    let schedule = |process: &Process| {
        let (cadence, zone, time) = (&process.cadence, &process.timezone, &process.at_time);
        format!("{cadence} {} {} {} {time} {zone}", process.weekday, process.month, process.day)
    };
    let before = schedule(&process);
    if let Some(title) = details.title {
        process.title = title.trim().to_string();
    }
    if process.title.is_empty() || process.title.chars().count() > 200 {
        return Err(Refusal::bad("a process's title is 1 to 200 characters"));
    }
    if let Some(description) = details.description {
        process.description = description.trim().chars().take(5_000).collect();
    }
    if let Some(cadence) = details.cadence {
        let found = Cadence::parse(&cadence).ok_or_else(|| {
            Refusal::bad(format!(
                "`{cadence}` is not a cadence: daily, weekly, monthly, quarterly or yearly"
            ))
        })?;
        process.cadence = found.name().to_string();
    }
    if let Some(weekday) = details.weekday {
        process.weekday = numbered(&weekday, &cadence::WEEKDAYS, "weekday")?;
    }
    if let Some(month) = details.month {
        process.month = numbered(&month, &cadence::MONTHS, "month")?;
    }
    if let Some(day) = details.day {
        process.day = day;
    }
    if let Some(time) = details.time {
        let parsed = NaiveTime::parse_from_str(time.trim(), "%H:%M")
            .or_else(|_| NaiveTime::parse_from_str(time.trim(), "%H:%M:%S"))
            .map_err(|_| Refusal::bad(format!("`{time}` is not a time of day, such as 10:00")))?;
        process.at_time = parsed.format("%H:%M").to_string();
    }
    if let Some(zone) = details.timezone {
        zone.trim().parse::<Tz>().map_err(|_| {
            Refusal::bad(format!("`{zone}` is not a time zone, such as Europe/London"))
        })?;
        process.timezone = zone.trim().to_string();
    }
    if !(1..=31).contains(&process.day) || !(1..=12).contains(&process.month) {
        return Err(Refusal::bad("a day is 1 to 31, and a month 1 to 12"));
    }
    if !(1..=7).contains(&process.weekday) {
        return Err(Refusal::bad("a weekday is 1 for Monday to 7 for Sunday"));
    }
    rule_of(&process)?.check().map_err(Refusal::bad)?;
    if let Some(minutes) = details.duration_minutes {
        if !(5..=1440).contains(&minutes) {
            return Err(Refusal::bad("an occurrence takes 5 to 1440 minutes on the calendar"));
        }
        process.duration_minutes = minutes;
    }
    if let Some(minutes) = details.remind_minutes {
        if minutes.is_some_and(|minutes| !(0..=MAX_REMINDER).contains(&minutes)) {
            return Err(Refusal::bad(format!(
                "a reminder comes 0 to {MAX_REMINDER} minutes before an occurrence is due"
            )));
        }
        process.remind_minutes = minutes;
    }
    if let Some(minutes) = details.grace_minutes {
        if !(1..=MAX_GRACE).contains(&minutes) {
            return Err(Refusal::bad(format!(
                "an occurrence is missed 1 to {MAX_GRACE} minutes after it is due"
            )));
        }
        process.grace_minutes = minutes;
    }
    if let Some(owner) = details.owner {
        process.owner = login(&owner, "owner")?;
    }
    if let Some(assignees) = details.assignees {
        if assignees.len() > MAX_ASSIGNEES {
            return Err(Refusal::bad(format!("a process has up to {MAX_ASSIGNEES} assignees")));
        }
        let mut checked: Vec<Assignee> = Vec::new();
        for assignee in assignees {
            let assignee = match assignee {
                Assignee::User(user) => Assignee::User(login(&user, "assignee")?),
                Assignee::Team(team) => {
                    let team = team.trim().to_string();
                    if !sight.sees(&format!("team:{team}")).await {
                        return Err(Refusal::bad(format!(
                            "there is no team called {team}, or you cannot see it"
                        )));
                    }
                    Assignee::Team(team)
                }
            };
            if !checked.contains(&assignee) {
                checked.push(assignee);
            }
        }
        process.assignees = checked;
    }
    if let Some(checklist) = details.checklist {
        let items: Vec<String> = checklist
            .iter()
            .map(|item| item.trim().to_string())
            .filter(|item| !item.is_empty())
            .collect();
        if items.len() > MAX_ITEMS || items.iter().any(|item| item.chars().count() > 300) {
            return Err(Refusal::bad(format!(
                "a checklist has up to {MAX_ITEMS} items of up to 300 characters"
            )));
        }
        process.checklist = items;
    }
    let rescheduled = schedule(&process) != before;
    Ok((process, rescheduled))
}

fn may_change(backend: &Backend, process: &Process) -> Result<(), Refusal> {
    let me = me(backend)?;
    match admin(backend) || process.owner == me || process.created_by == me {
        true => Ok(()),
        false => Err(Refusal::forbidden(format!(
            "only {}'s owner, {}, or an admin changes it",
            process.title, process.owner
        ))),
    }
}

/// The caller's login, if they may work on the process's occurrences.
async fn may_work(
    backend: &Backend,
    sight: &mut Sight<'_>,
    process: &Process,
) -> Result<String, Refusal> {
    let me = me(backend)?;
    match admin(backend) || process.owner == me || sight.assigned(process, &me).await {
        true => Ok(me),
        false => Err(Refusal::forbidden(format!(
            "only {}'s assignees ({}), its owner and admins work on its occurrences",
            process.title,
            assignees_text(process)
        ))),
    }
}

pub async fn found_process(
    store: &Store<'_>,
    sight: &mut Sight<'_>,
    id: &str,
) -> Result<Process, Refusal> {
    let process = store
        .process(id_of(id, "process")?)
        .await?
        .ok_or_else(|| Refusal::missing("there is no such process"))?;
    sight.visible(process).await
}

pub async fn found_occurrence(
    store: &Store<'_>,
    sight: &mut Sight<'_>,
    id: &str,
) -> Result<(Occurrence, Process), Refusal> {
    let occurrence = store
        .occurrence(id_of(id, "occurrence")?)
        .await?
        .ok_or_else(|| Refusal::missing("there is no such occurrence"))?;
    let process = store
        .process(occurrence.process)
        .await?
        .ok_or_else(|| Refusal::missing("there is no such occurrence"))?;
    let process = sight.visible(process).await.map_err(|_| {
        Refusal::missing("there is no such occurrence, or you cannot see its resource")
    })?;
    Ok((occurrence, process))
}

fn fresh(text: &str) -> Item {
    Item { text: text.to_string(), done: false, by: None, at: None }
}

/// The process's checklist, keeping whatever was already ticked off.
fn merged(process: &Process, old: &[Item]) -> Vec<Item> {
    process
        .checklist
        .iter()
        .map(|text| {
            old.iter().find(|item| &item.text == text).cloned().unwrap_or_else(|| fresh(text))
        })
        .collect()
}

fn titled(process: &Process, occurrence: &Occurrence) -> String {
    match occurrence.status.as_str() {
        "pending" => process.title.clone(),
        status => format!("{} ({status})", process.title),
    }
}

/// What `calendar-events` is asked to show for an occurrence.
fn event_details(process: &Process, occurrence: &Occurrence, new: bool) -> Value {
    let start = NaiveDateTime::parse_from_str(&occurrence.due_local, LOCAL).unwrap_or_default();
    let end = start + Duration::minutes(process.duration_minutes);
    let mut lines = Vec::new();
    if !process.description.is_empty() {
        lines.push(process.description.clone());
    }
    if !process.checklist.is_empty() {
        lines.push(String::from("Checklist:"));
        lines.extend(process.checklist.iter().map(|item| format!("- {item}")));
    }
    let mut asked = json!({
        "title": titled(process, occurrence),
        "start": occurrence.due_local,
        "end": end.format(LOCAL).to_string(),
        "timezone": process.timezone,
        "link": page(&format!("occurrences/{}", occurrence.id)),
        "description": lines.join("\n"),
        "attendees": process.assignees,
    });
    if new {
        asked["on"] = json!(process.resource);
        asked["resource"] = json!(process.resource);
    }
    asked
}

fn refused_by(status: u16, answer: &Value) -> Refusal {
    let detail = answer["detail"].as_str().unwrap_or("no reason given");
    Refusal::unavailable(format!("calendar-events answered {status}: {detail}"))
}

/// Puts an occurrence on its resource's calendar.
async fn give_event(
    backend: &Backend,
    process: &Process,
    occurrence: &Occurrence,
) -> Result<(), Refusal> {
    let asked = event_details(process, occurrence, true);
    let (status, made) =
        backend.discovery("calendar-events", "POST", "events", None, Some(asked)).await.map_err(
            |err| Refusal::unavailable(format!("calendar-events could not be asked: {err}")),
        )?;
    if status != 201 {
        return Err(refused_by(status, &made));
    }
    let event: Uuid = made["id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| Refusal::unavailable("calendar-events gave no event"))?;
    if !Store(backend).claim_event(occurrence.id, event).await? {
        drop_event(backend, event).await;
    }
    Ok(())
}

async fn update_event(backend: &Backend, process: &Process, occurrence: &Occurrence) {
    let Some(event) = occurrence.event else { return };
    let asked = event_details(process, occurrence, false);
    match backend
        .discovery("calendar-events", "PATCH", &format!("events/{event}"), None, Some(asked))
        .await
    {
        Ok((200, _)) => {}
        Ok((status, answer)) => {
            let refusal = refused_by(status, &answer);
            tracing::warn!(%event, detail = %refusal.detail, "an occurrence's event was not updated");
        }
        Err(err) => tracing::warn!(%err, %event, "an occurrence's event was not updated"),
    }
}

async fn drop_event(backend: &Backend, event: Uuid) {
    let route = format!("events/{event}/delete");
    match backend.discovery("calendar-events", "POST", &route, None, None).await {
        Ok((204 | 404, _)) => {}
        Ok((status, answer)) => {
            let refusal = refused_by(status, &answer);
            tracing::warn!(%event, detail = %refusal.detail, "an occurrence's event was not deleted");
        }
        Err(err) => tracing::warn!(%err, %event, "an occurrence's event was not deleted"),
    }
}

/// Publishes `plugin.process.occurrence.<what>` once, answering whether it went out.
async fn announce(
    backend: &Backend,
    what: &str,
    process: &Process,
    occurrence: &Occurrence,
    extra: Value,
) -> bool {
    let mut payload = json!({
        "resource": process.resource,
        "process": process.title,
        "process_id": process.id,
        "occurrence": occurrence.id,
        "due_at": due_in(occurrence, process),
        "assignees": assignees_text(process),
        "owner": process.owner,
        "url": page(&format!("occurrences/{}", occurrence.id)),
    });
    if let (Some(payload), Value::Object(extra)) = (payload.as_object_mut(), extra) {
        payload.extend(extra);
    }
    let topic = format!("plugin.process.occurrence.{what}");
    let key = format!("{what}:{}", occurrence.id);
    match backend.publish_once(&topic, payload, &key).await {
        Ok(_) => true,
        Err(err) => {
            tracing::warn!(%err, occurrence = %occurrence.id, %topic, "a notice was not published");
            false
        }
    }
}

/// The occurrences the process should have by now and over the coming weeks that it lacks.
fn unplanned(
    process: &Process,
    planned: &BTreeSet<(Uuid, String)>,
    now: DateTime<Utc>,
) -> Result<Vec<Occurrence>, Refusal> {
    let rule = rule_of(process)?;
    let plan_from = instant(&process.plan_from).unwrap_or(now);
    let from = plan_from.max(now - Duration::hours(LOOKBACK_HOURS));
    let mut dues = rule.upcoming(from, now + Duration::days(HORIZON_DAYS), 0);
    for next in rule.upcoming(plan_from.max(now), now, 1) {
        if !dues.iter().any(|due| due.local == next.local) {
            dues.push(next);
        }
    }
    Ok(dues
        .into_iter()
        .filter(|due| !planned.contains(&(process.id, due.named())))
        .map(|due| Occurrence {
            id: Uuid::now_v7(),
            process: process.id,
            due_local: due.named(),
            due_at: due.at.with_timezone(&Utc).to_rfc3339(),
            status: "pending".into(),
            checklist: process.checklist.iter().map(|text| fresh(text)).collect(),
            event: None,
            note: String::new(),
            finished_by: None,
            finished_at: None,
            reminded_at: None,
            missed_at: None,
            created_at: String::new(),
            updated_at: String::new(),
        })
        .collect())
}

/// Adds missing occurrences with their events, none in a day, week or month that already has one.
async fn plan(
    backend: &Backend,
    process: &Process,
    planned: &BTreeSet<(Uuid, String)>,
    now: DateTime<Utc>,
) -> Result<usize, Refusal> {
    let store = Store(backend);
    let rule = rule_of(process)?;
    let mut wanted = Vec::new();
    for occurrence in unplanned(process, planned, now)? {
        let due = NaiveDateTime::parse_from_str(&occurrence.due_local, LOCAL)
            .map_err(|_| Refusal::unavailable("an occurrence's time cannot be read"))?;
        let (first, next) = rule.period(due.date());
        let bound = |day: chrono::NaiveDate| day.and_time(NaiveTime::MIN).format(LOCAL).to_string();
        if !store.occupied(process.id, &bound(first), &bound(next)).await? {
            wanted.push(occurrence);
        }
    }
    if wanted.is_empty() {
        return Ok(0);
    }
    let added = store.add(&wanted).await?;
    for occurrence in wanted.iter().filter(|occurrence| added.contains(&occurrence.id)) {
        if let Err(refusal) = give_event(backend, process, occurrence).await {
            tracing::warn!(process = %process.id, detail = %refusal.detail, "an occurrence is not on its calendar yet");
            break;
        }
    }
    Ok(added.len())
}

async fn planned_for(
    store: &Store<'_>,
    process: &Process,
) -> Result<BTreeSet<(Uuid, String)>, Refusal> {
    let occurrences = store.occurrences_of(process.id).await?;
    Ok(occurrences.into_iter().map(|occurrence| (process.id, occurrence.due_local)).collect())
}

pub async fn create(
    backend: &Backend,
    sight: &mut Sight<'_>,
    details: Details,
) -> Result<Process, Refusal> {
    let me = me(backend)?;
    let asked = details
        .resource
        .as_deref()
        .ok_or_else(|| Refusal::bad("name the resource the process is for, as kind:name"))?;
    let resource = normalised(asked)?;
    if !sight.sees(&resource).await {
        return Err(Refusal::missing(format!("there is no {resource}, or you cannot see it")));
    }
    let now = Utc::now();
    let blank = Process {
        id: Uuid::now_v7(),
        resource,
        title: String::new(),
        description: String::new(),
        cadence: "weekly".into(),
        weekday: 1,
        month: 1,
        day: 1,
        at_time: "09:00".into(),
        timezone: "UTC".into(),
        duration_minutes: 30,
        remind_minutes: Some(60),
        grace_minutes: 24 * 60,
        owner: me.clone(),
        assignees: Vec::new(),
        checklist: Vec::new(),
        plan_from: now.to_rfc3339(),
        created_by: me,
        created_at: String::new(),
        updated_at: String::new(),
    };
    let (process, _) = apply(sight, blank, Details { resource: None, ..details }).await?;
    let store = Store(backend);
    store.save_process(&process).await?;
    plan(backend, &process, &BTreeSet::new(), now).await?;
    store.process(process.id).await?.ok_or_else(|| Refusal::missing("the process has gone"))
}

/// Changes a process. A new schedule replans the occurrences to come; anything else updates them.
pub async fn change(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    details: Details,
) -> Result<Process, Refusal> {
    let store = Store(backend);
    let process = found_process(&store, sight, id).await?;
    may_change(backend, &process)?;
    if let Some(resource) = &details.resource
        && normalised(resource)? != process.resource
    {
        return Err(Refusal::bad("a process stays on its resource; make a new one for another"));
    }
    let (mut process, rescheduled) =
        apply(sight, process, Details { resource: None, ..details }).await?;
    let now = Utc::now();
    if rescheduled {
        process.plan_from = now.to_rfc3339();
    }
    store.save_process(&process).await?;
    for occurrence in store.occurrences_of(process.id).await? {
        let coming = instant(&occurrence.due_at).is_some_and(|due| due > now);
        if occurrence.status != "pending" || !coming {
            continue;
        }
        if rescheduled {
            if let Some(event) = occurrence.event {
                drop_event(backend, event).await;
            }
            store.delete_occurrence(occurrence.id).await?;
            continue;
        }
        let checklist = merged(&process, &occurrence.checklist);
        store.set_checklist(occurrence.id, &checklist).await?;
        update_event(backend, &process, &occurrence).await;
    }
    plan(backend, &process, &planned_for(&store, &process).await?, now).await?;
    store.process(process.id).await?.ok_or_else(|| Refusal::missing("the process has gone"))
}

/// Deletes a process, its occurrences and their calendar events.
pub async fn remove(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
) -> Result<Process, Refusal> {
    let store = Store(backend);
    let process = found_process(&store, sight, id).await?;
    may_change(backend, &process)?;
    for occurrence in store.occurrences_of(process.id).await? {
        if let Some(event) = occurrence.event {
            drop_event(backend, event).await;
        }
    }
    store.delete_process(process.id).await?;
    Ok(process)
}

fn still_open(occurrence: &Occurrence) -> Result<(), Refusal> {
    match occurrence.status.as_str() {
        "pending" | "missed" => Ok(()),
        status => Err(Refusal::conflict(format!("this occurrence is {status} already"))),
    }
}

/// Ticks off, or unticks, item `number` of an occurrence's checklist, counting from 1.
pub async fn tick(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    number: usize,
    done: bool,
) -> Result<(Occurrence, Process), Refusal> {
    let store = Store(backend);
    let (occurrence, process) = found_occurrence(&store, sight, id).await?;
    let me = may_work(backend, sight, &process).await?;
    still_open(&occurrence)?;
    let index = number.checked_sub(1).filter(|index| *index < occurrence.checklist.len());
    let Some(index) = index else {
        return Err(Refusal::bad(format!(
            "the checklist has items 1 to {}",
            occurrence.checklist.len()
        )));
    };
    let item = Item {
        text: occurrence.checklist[index].text.clone(),
        done,
        by: done.then(|| me.clone()),
        at: done.then(|| Utc::now().to_rfc3339()),
    };
    if !store.tick(occurrence.id, index, &item).await? {
        return Err(Refusal::conflict("this occurrence was finished meanwhile"));
    }
    let occurrence =
        store.occurrence(occurrence.id).await?.ok_or_else(|| Refusal::missing("it has gone"))?;
    Ok((occurrence, process))
}

/// Marks an occurrence done once its checklist is ticked off, or ticking off the rest with `all`.
pub async fn complete(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    all: bool,
    note: Option<String>,
) -> Result<(Occurrence, Process), Refusal> {
    let store = Store(backend);
    let (occurrence, process) = found_occurrence(&store, sight, id).await?;
    let me = may_work(backend, sight, &process).await?;
    still_open(&occurrence)?;
    let left: Vec<&str> = occurrence
        .checklist
        .iter()
        .filter(|item| !item.done)
        .map(|item| item.text.as_str())
        .collect();
    if !left.is_empty() && !all {
        return Err(Refusal::bad(format!(
            "tick off the rest of the checklist first, or mark them all done: {}",
            left.join("; ")
        )));
    }
    let now = Utc::now().to_rfc3339();
    let mut finished = occurrence.clone();
    for item in finished.checklist.iter_mut().filter(|item| !item.done) {
        (item.done, item.by, item.at) = (true, Some(me.clone()), Some(now.clone()));
    }
    finished.status = "done".into();
    finished.finished_by = Some(me.clone());
    if let Some(note) = note {
        finished.note = note.trim().chars().take(1_000).collect();
    }
    if !store.finish(&finished).await? {
        return Err(Refusal::conflict("this occurrence was finished meanwhile"));
    }
    update_event(backend, &process, &finished).await;
    let late = finished.missed_at.is_some();
    announce(backend, "done", &process, &finished, json!({ "by": me, "late": late })).await;
    let occurrence =
        store.occurrence(occurrence.id).await?.ok_or_else(|| Refusal::missing("it has gone"))?;
    Ok((occurrence, process))
}

/// Skips an occurrence, saying why.
pub async fn skip(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    note: &str,
) -> Result<(Occurrence, Process), Refusal> {
    let store = Store(backend);
    let (occurrence, process) = found_occurrence(&store, sight, id).await?;
    let me = may_work(backend, sight, &process).await?;
    still_open(&occurrence)?;
    let note: String = note.trim().chars().take(1_000).collect();
    if note.is_empty() {
        return Err(Refusal::bad("say why it is skipped"));
    }
    let mut skipped = occurrence.clone();
    skipped.status = "skipped".into();
    skipped.finished_by = Some(me.clone());
    skipped.note = note.clone();
    if !store.finish(&skipped).await? {
        return Err(Refusal::conflict("this occurrence was finished meanwhile"));
    }
    update_event(backend, &process, &skipped).await;
    announce(backend, "skipped", &process, &skipped, json!({ "by": me, "note": note })).await;
    let occurrence =
        store.occurrence(occurrence.id).await?.ok_or_else(|| Refusal::missing("it has gone"))?;
    Ok((occurrence, process))
}

/// The processes the caller can see, on one resource or all, or only their own.
pub async fn listed(
    backend: &Backend,
    sight: &mut Sight<'_>,
    resource: Option<&str>,
    mine: bool,
) -> Result<Vec<Process>, Refusal> {
    let store = Store(backend);
    let candidates = match resource {
        Some(resource) => {
            let resource = normalised(resource)?;
            if !sight.sees(&resource).await {
                return Err(Refusal::missing(format!(
                    "there is no {resource}, or you cannot see it"
                )));
            }
            store.on(&resource).await?
        }
        None => store.processes().await?,
    };
    let login = me(backend)?;
    let mut found = Vec::new();
    for process in candidates {
        if !sight.sees(&process.resource).await {
            continue;
        }
        if mine && !sight.mine(&process, &login).await {
            continue;
        }
        found.push(process);
    }
    Ok(found)
}

/// Each process's next open occurrence, and how many of its occurrences were missed lately.
pub async fn outlook(
    store: &Store<'_>,
    processes: &[Process],
) -> Result<BTreeMap<Uuid, (Option<Occurrence>, usize)>, Refusal> {
    let now = Utc::now();
    let ids: Vec<Uuid> = processes.iter().map(|process| process.id).collect();
    let from = (now - Duration::days(90)).to_rfc3339();
    let to = (now + Duration::days(400)).to_rfc3339();
    let mut found: BTreeMap<Uuid, (Option<Occurrence>, usize)> = BTreeMap::new();
    for occurrence in store.between(&ids, &from, &to).await? {
        let entry = found.entry(occurrence.process).or_default();
        if occurrence.missed_at.is_some() {
            entry.1 += 1;
        }
        if occurrence.status == "pending" && entry.0.is_none() {
            entry.0 = Some(occurrence);
        }
    }
    Ok(found)
}

/// Open occurrences of these processes: missed or overdue ones, and those due within `days`.
pub async fn to_do(
    store: &Store<'_>,
    processes: &[Process],
    days: i64,
) -> Result<Vec<Occurrence>, Refusal> {
    let now = Utc::now();
    let ids: Vec<Uuid> = processes.iter().map(|process| process.id).collect();
    let from = (now - Duration::days(30)).to_rfc3339();
    let to = (now + Duration::days(days)).to_rfc3339();
    Ok(store
        .between(&ids, &from, &to)
        .await?
        .into_iter()
        .filter(|occurrence| matches!(occurrence.status.as_str(), "pending" | "missed"))
        .collect())
}

fn shown_list(
    processes: &[Process],
    outlook: &BTreeMap<Uuid, (Option<Occurrence>, usize)>,
) -> Vec<Value> {
    processes
        .iter()
        .map(|process| {
            let (next, missed) = outlook.get(&process.id).cloned().unwrap_or_default();
            process_shown(process, next.as_ref(), missed)
        })
        .collect()
}

pub async fn handle(backend: &Backend, request: Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let answer = match segments.as_slice() {
        ["ui", route @ ..] => return crate::ui::handle(backend, &request, route).await,
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
        ("GET", ["processes"]) => {
            let mine = query(request, "mine").is_some_and(|mine| mine == "true");
            let resource = query(request, "resource");
            let found = listed(backend, &mut sight, resource.as_deref(), mine).await?;
            Ok((200, json!(shown_list(&found, &outlook(&store, &found).await?))))
        }
        ("POST", ["processes"]) => {
            let process = create(backend, &mut sight, body(request)?).await?;
            let found = std::slice::from_ref(&process);
            Ok((201, json!(shown_list(found, &outlook(&store, found).await?).pop())))
        }
        ("GET", ["processes", id]) => {
            let process = found_process(&store, &mut sight, id).await?;
            let found = std::slice::from_ref(&process);
            let mut shown =
                shown_list(found, &outlook(&store, found).await?).pop().unwrap_or_default();
            let occurrences = store.occurrences_of(process.id).await?;
            shown["occurrences"] = json!(
                occurrences
                    .iter()
                    .map(|occurrence| occurrence_shown(occurrence, &process))
                    .collect::<Vec<_>>()
            );
            Ok((200, shown))
        }
        ("GET", ["processes", id, "schedule"]) => {
            let process = found_process(&store, &mut sight, id).await?;
            let count = query(request, "count").and_then(|count| count.parse().ok()).unwrap_or(8);
            if !(1..=24).contains(&count) {
                return Err(Refusal::bad("ask for 1 to 24 of the times it is due"));
            }
            let now = Utc::now();
            let dues = rule_of(&process)?.upcoming(now, now, count);
            let shown: Vec<String> = dues.iter().map(|due| due.at.to_rfc3339()).collect();
            Ok((200, json!({ "schedule": rule_of(&process)?.describe(), "due": shown })))
        }
        ("PATCH", ["processes", id]) => {
            let process = change(backend, &mut sight, id, body(request)?).await?;
            let found = std::slice::from_ref(&process);
            Ok((200, json!(shown_list(found, &outlook(&store, found).await?).pop())))
        }
        ("DELETE", ["processes", id]) => {
            remove(backend, &mut sight, id).await?;
            Ok((204, Value::Null))
        }
        ("GET", ["occurrences"]) => {
            let mine = query(request, "mine").is_some_and(|mine| mine == "true");
            let resource = query(request, "resource");
            let days = query(request, "days").and_then(|days| days.parse().ok()).unwrap_or(14);
            if !(1..=400).contains(&days) {
                return Err(Refusal::bad("ask for 1 to 400 days ahead"));
            }
            let processes = listed(backend, &mut sight, resource.as_deref(), mine).await?;
            let by_id: BTreeMap<Uuid, &Process> =
                processes.iter().map(|process| (process.id, process)).collect();
            let shown: Vec<Value> = to_do(&store, &processes, days)
                .await?
                .iter()
                .filter_map(|occurrence| {
                    by_id
                        .get(&occurrence.process)
                        .map(|process| occurrence_shown(occurrence, process))
                })
                .collect();
            Ok((200, json!(shown)))
        }
        ("GET", ["occurrences", id]) => {
            let (occurrence, process) = found_occurrence(&store, &mut sight, id).await?;
            Ok((200, occurrence_shown(&occurrence, &process)))
        }
        ("POST", ["occurrences", id, "tick"]) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Ticked {
                item: usize,
                #[serde(default)]
                done: Option<bool>,
            }
            let asked: Ticked = body(request)?;
            let done = asked.done.unwrap_or(true);
            let (occurrence, process) = tick(backend, &mut sight, id, asked.item, done).await?;
            Ok((200, occurrence_shown(&occurrence, &process)))
        }
        ("POST", ["occurrences", id, "complete"]) => {
            #[derive(Deserialize, Default)]
            #[serde(deny_unknown_fields, default)]
            struct Completed {
                all: bool,
                note: Option<String>,
            }
            let asked: Completed = body(request)?;
            let (occurrence, process) =
                complete(backend, &mut sight, id, asked.all, asked.note).await?;
            Ok((200, occurrence_shown(&occurrence, &process)))
        }
        ("POST", ["occurrences", id, "skip"]) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Skipped {
                note: String,
            }
            let asked: Skipped = body(request)?;
            let (occurrence, process) = skip(backend, &mut sight, id, &asked.note).await?;
            Ok((200, occurrence_shown(&occurrence, &process)))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}

/// Plans occurrences, gives events to those without, and publishes reminders and missed notices.
pub async fn sweep(backend: &Backend) -> Result<Value, Refusal> {
    let store = Store(backend);
    let now = Utc::now();
    let processes = store.processes().await?;
    let since = now - Duration::hours(LOOKBACK_HOURS + 24);
    let planned: BTreeSet<(Uuid, String)> =
        store.planned(&since.to_rfc3339()).await?.into_iter().collect();
    let mut added = 0;
    for process in &processes {
        match plan(backend, process, &planned, now).await {
            Ok(count) => added += count,
            Err(refusal) => {
                tracing::warn!(process = %process.id, detail = %refusal.detail, "occurrences were not planned");
            }
        }
    }
    let by_id: BTreeMap<Uuid, &Process> =
        processes.iter().map(|process| (process.id, process)).collect();
    let mut failing = BTreeSet::new();
    for occurrence in store.without_event(&now.to_rfc3339()).await?.iter().take(EVENTS_PER_TICK) {
        let Some(process) = by_id.get(&occurrence.process) else { continue };
        if failing.contains(&process.id) {
            continue;
        }
        if let Err(refusal) = give_event(backend, process, occurrence).await {
            tracing::warn!(process = %process.id, detail = %refusal.detail, "an occurrence is not on its calendar yet");
            failing.insert(process.id);
        }
    }
    let (mut reminded, mut missed) = (Vec::new(), Vec::new());
    let until = now + Duration::minutes(MAX_REMINDER + 1);
    // Who will not be there, over the days these occurrences fall on. Asked once for the sweep.
    let away = crate::away::Away::read(
        backend,
        &processes,
        now.date_naive(),
        until.date_naive() + chrono::Days::new(1),
    )
    .await;
    let mut uncovered = Vec::new();
    for occurrence in store.pending_before(&until.to_rfc3339()).await? {
        let (Some(process), Some(due)) =
            (by_id.get(&occurrence.process), instant(&occurrence.due_at))
        else {
            continue;
        };
        if now >= due + Duration::minutes(process.grace_minutes) {
            if announce(backend, "missed", process, &occurrence, json!({})).await
                && store.miss(occurrence.id).await?
            {
                let flagged = Occurrence { status: "missed".into(), ..occurrence.clone() };
                update_event(backend, process, &flagged).await;
                missed.push(occurrence.id);
            }
            continue;
        }
        let due_soon =
            process.remind_minutes.is_some_and(|lead| now >= due - Duration::minutes(lead));
        // Who it falls to that will not be there, from their own calendars.
        let short = away.among(process, due.date_naive());
        if due_soon
            && occurrence.reminded_at.is_none()
            && announce(backend, "reminder", process, &occurrence, json!({ "away": short })).await
            && store.remind(occurrence.id).await?
        {
            reminded.push(occurrence.id);
        }
        // Nobody it falls to will be there. Said once per occurrence, as soon as it is close
        // enough to remind about, so somebody has time to pick it up.
        if due_soon
            && away.nobody_left(process, due.date_naive())
            && announce(backend, "uncovered", process, &occurrence, json!({ "away": short })).await
        {
            uncovered.push(occurrence.id);
        }
    }
    Ok(json!({
        "planned": added,
        "reminded": reminded,
        "missed": missed,
        "uncovered": uncovered,
    }))
}
