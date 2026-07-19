//! Hackathons, game nights and challenges. Each is organised by a team, starts a discussion thread
//! and goes on the team's calendar through `calendar-events`. Hackathons need the `hackathon`
//! permission, and are team-based: teams register, submit and are announced as winners.

use chrono::{DateTime, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use doc_plugin_sdk::{Backend, Caller};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::api::{self, NewReply, NewThread, Sight};
use crate::markdown;
use crate::store::{Entry, Event, Registration, Store, Submission};

pub const KINDS: [(&str, &str); 3] =
    [("hackathon", "Hackathon"), ("game-night", "Game night"), ("challenge", "Challenge")];

const LOCAL: &str = "%Y-%m-%dT%H:%M:%S";
const MAX_LINKS: usize = 5;

pub fn kind_name(kind: &str) -> &'static str {
    KINDS.iter().find(|(name, _)| *name == kind).map_or("Event", |(_, shown)| shown)
}

fn login(backend: &Backend) -> Result<String, Refusal> {
    match backend.caller() {
        Some(Caller { kind, id: Some(id), label, .. }) if kind == "user" || kind == "service" => {
            Ok(label.clone().unwrap_or_else(|| id.clone()))
        }
        _ => Err(Refusal::forbidden("events are for people and service accounts")),
    }
}

fn admin(backend: &Backend) -> bool {
    backend.caller().is_some_and(|caller| caller.admin)
}

/// Whether the caller holds `hackathon`, which names an ability, at any scope.
pub fn may_organise_hackathons(backend: &Backend) -> bool {
    backend.caller().is_some_and(|caller| caller.admin || caller.custom.contains_key("hackathon"))
}

/// Whether `login` is one of the team's members in Resource Definitions.
pub async fn member(sight: &mut Sight<'_>, team: &str, login: &str) -> bool {
    let Ok(found) = sight.resource(&format!("team:{team}")).await else { return false };
    found["connections"].as_array().into_iter().flatten().any(|connection| {
        connection["kind"] == "User" && connection["name"].as_str() == Some(login)
    })
}

async fn team_of(sight: &mut Sight<'_>, team: &str) -> Result<String, Refusal> {
    let team = team.trim().trim_start_matches("team:").to_string();
    match sight.resource(&format!("team:{team}")).await {
        Ok(_) if !team.is_empty() => Ok(team),
        _ => Err(Refusal::bad(format!("there is no team called {team}, or you cannot see it"))),
    }
}

/// A wall-clock time, from `2026-10-22T09:00` or with seconds.
fn wall(text: &str) -> Result<NaiveDateTime, Refusal> {
    NaiveDateTime::parse_from_str(text.trim(), LOCAL)
        .or_else(|_| NaiveDateTime::parse_from_str(text.trim(), "%Y-%m-%dT%H:%M"))
        .map_err(|_| Refusal::bad(format!("`{text}` is not a time, such as 2026-10-22T09:00")))
}

pub fn instant(event: &Event, local: &str) -> Option<DateTime<Utc>> {
    let zone: Tz = event.timezone.parse().ok()?;
    let at = NaiveDateTime::parse_from_str(local, LOCAL).ok()?;
    zone.from_local_datetime(&at).earliest().map(|at| at.with_timezone(&Utc))
}

pub fn page(event: &Event) -> String {
    api::page(&format!("events/{}", event.id))
}

async fn announce(backend: &Backend, topic: &str, payload: Value) {
    if let Err(err) = backend.publish(topic, payload).await {
        tracing::warn!(%err, %topic, "an event's news was not published");
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewEvent {
    pub kind: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub team: String,
    pub start: String,
    pub end: String,
    #[serde(default)]
    pub timezone: Option<String>,
    #[serde(default)]
    pub location: String,
    #[serde(default)]
    pub higher_wins: Option<bool>,
    /// More resources for its discussion to be tagged with, beside the organising team.
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub unit: String,
}

pub async fn found(store: &Store<'_>, id: &str) -> Result<Event, Refusal> {
    let id: Uuid = id.parse().map_err(|_| Refusal::missing("there is no such event"))?;
    store.event(id).await?.ok_or_else(|| Refusal::missing("there is no such event"))
}

/// Makes an event with its discussion thread and its place on the organising team's calendar.
pub async fn create(
    backend: &Backend,
    sight: &mut Sight<'_>,
    asked: NewEvent,
) -> Result<Event, Refusal> {
    let me = login(backend)?;
    let kind = KINDS
        .iter()
        .map(|(name, _)| *name)
        .find(|name| *name == asked.kind.trim())
        .ok_or_else(|| Refusal::bad("an event is a hackathon, a game-night or a challenge"))?;
    if kind == "hackathon" && !may_organise_hackathons(backend) {
        return Err(Refusal::forbidden("needs plugin:water:pluginuser:hackathon"));
    }
    let title = asked.title.trim().to_string();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(Refusal::bad("an event's title is 1 to 200 characters"));
    }
    let team = team_of(sight, &asked.team).await?;
    if !admin(backend) && !member(sight, &team, &me).await {
        return Err(Refusal::forbidden(format!("only members of {team} organise events for it")));
    }
    let zone = asked.timezone.as_deref().map(str::trim).filter(|zone| !zone.is_empty());
    let zone = zone.unwrap_or("UTC");
    zone.parse::<Tz>()
        .map_err(|_| Refusal::bad(format!("`{zone}` is not a time zone, such as Europe/London")))?;
    let (start, end) = (wall(&asked.start)?, wall(&asked.end)?);
    if end <= start {
        return Err(Refusal::bad("an event ends after it starts"));
    }
    let description = asked.description.trim().to_string();
    if description.chars().count() > 20_000 {
        return Err(Refusal::bad("a description is up to 20,000 characters"));
    }
    let mut event = Event {
        id: Uuid::now_v7(),
        kind: kind.to_string(),
        html: markdown::render(&description).html,
        description,
        title,
        team,
        organiser: me.clone(),
        starts_local: start.format(LOCAL).to_string(),
        ends_local: end.format(LOCAL).to_string(),
        timezone: zone.to_string(),
        location: asked.location.trim().chars().take(300).collect(),
        thread: None,
        calendar_event: None,
        higher_wins: asked.higher_wins.unwrap_or(true),
        unit: asked.unit.trim().chars().take(30).collect(),
        status: "open".into(),
        winners: Vec::new(),
        created_at: String::new(),
        updated_at: String::new(),
    };
    let store = Store(backend);
    store.save_event(&event).await?;
    let link = format!("/p/water/events/{}", event.id);
    let opening = format!(
        "{} for /team:{}, from {} to {} ({}). Everything about it is at [its page]({link}).\n\n{}",
        kind_name(kind),
        event.team,
        start.format("%a %-d %b %Y, %H:%M"),
        end.format("%a %-d %b %Y, %H:%M"),
        event.timezone,
        event.description
    );
    let thread = NewThread { title: event.title.clone(), body: opening, tags: asked.tags };
    event.thread = Some(api::start(backend, sight, thread).await?.id);
    let asked = json!({
        "on": format!("team:{}", event.team),
        "title": format!("{}: {}", kind_name(kind), event.title),
        "start": event.starts_local,
        "end": event.ends_local,
        "timezone": event.timezone,
        "location": event.location,
        "link": page(&event),
        "description": event.description,
        "resource": format!("team:{}", event.team),
    });
    match backend.discovery("calendar-events", "POST", "events", None, Some(asked)).await {
        Ok((201, made)) => {
            event.calendar_event = made["id"].as_str().and_then(|id| id.parse().ok())
        }
        Ok((status, answer)) => {
            tracing::warn!(status, detail = %answer["detail"], "an event is not on its team's calendar")
        }
        Err(err) => tracing::warn!(%err, "an event is not on its team's calendar"),
    }
    store.save_event(&event).await?;
    let starts_at = instant(&event, &event.starts_local).map(|at| at.to_rfc3339());
    let payload = json!({
        "event": event.id,
        "kind": event.kind,
        "title": event.title,
        "team": event.team,
        "organiser": event.organiser,
        "starts_at": starts_at,
        "url": page(&event),
    });
    announce(backend, "plugin.water.event.created", payload).await;
    Ok(event)
}

fn organises(backend: &Backend, event: &Event) -> Result<String, Refusal> {
    let me = login(backend)?;
    match admin(backend) || event.organiser == me {
        true => Ok(me),
        false => Err(Refusal::forbidden(format!(
            "only {}, who organises it, or an admin does that",
            event.organiser
        ))),
    }
}

fn open(event: &Event, kind: &str) -> Result<(), Refusal> {
    if event.kind != kind {
        return Err(Refusal::bad(format!("that is for a {}", kind_name(kind).to_lowercase())));
    }
    match event.status.as_str() {
        "open" => Ok(()),
        _ => Err(Refusal::bad(format!("{} is closed", event.title))),
    }
}

/// Registers a team for a hackathon; any of its members may.
pub async fn register(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    team: &str,
) -> Result<(Event, bool), Refusal> {
    let me = login(backend)?;
    let store = Store(backend);
    let event = found(&store, id).await?;
    open(&event, "hackathon")?;
    let team = team_of(sight, team).await?;
    if !admin(backend) && !member(sight, &team, &me).await {
        return Err(Refusal::forbidden(format!("only members of {team} register it")));
    }
    let added = store.register(event.id, &team, &me).await?;
    if added {
        let payload = json!({ "event": event.id, "title": event.title, "team": team, "by": me, "url": page(&event) });
        announce(backend, "plugin.water.hackathon.registered", payload).await;
    }
    Ok((event, added))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewSubmission {
    pub team: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub links: Vec<String>,
}

/// A registered team's submission, made or replaced by one of its members.
pub async fn submit(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    asked: NewSubmission,
) -> Result<Event, Refusal> {
    let me = login(backend)?;
    let store = Store(backend);
    let event = found(&store, id).await?;
    open(&event, "hackathon")?;
    let team = asked.team.trim().trim_start_matches("team:").to_string();
    let registered = store.registrations(event.id).await?;
    if !registered.iter().any(|registration| registration.team == team) {
        return Err(Refusal::bad(format!("{team} has not registered for {}", event.title)));
    }
    if !admin(backend) && !member(sight, &team, &me).await {
        return Err(Refusal::forbidden(format!("only members of {team} submit for it")));
    }
    let title = asked.title.trim().to_string();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(Refusal::bad("a submission's title is 1 to 200 characters"));
    }
    let links: Vec<String> = asked
        .links
        .iter()
        .map(|link| link.trim().to_string())
        .filter(|link| !link.is_empty())
        .collect();
    let web = |link: &String| link.starts_with("https://") || link.starts_with("http://");
    if links.len() > MAX_LINKS || !links.iter().all(web) {
        return Err(Refusal::bad(format!(
            "a submission has up to {MAX_LINKS} http or https links"
        )));
    }
    let description = asked.description.trim().to_string();
    let submission = Submission {
        event: event.id,
        team: team.clone(),
        title,
        html: markdown::render(&description).html,
        description,
        links,
        submitted_by: me,
        updated_at: String::new(),
    };
    store.submit(&submission).await?;
    let payload = json!({ "event": event.id, "title": event.title, "team": team, "submission": submission.title, "url": page(&event) });
    announce(backend, "plugin.water.hackathon.submitted", payload).await;
    Ok(event)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Winner {
    pub team: String,
    pub place: u32,
    #[serde(default)]
    pub note: String,
}

/// Announces a hackathon's winners, closing it, and says so in its discussion.
pub async fn crown(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    winners: Vec<Winner>,
) -> Result<Event, Refusal> {
    let store = Store(backend);
    let mut event = found(&store, id).await?;
    organises(backend, &event)?;
    if event.kind != "hackathon" || event.status == "announced" {
        return Err(Refusal::bad("winners are announced once, for a hackathon"));
    }
    let registered: Vec<String> = store
        .registrations(event.id)
        .await?
        .into_iter()
        .map(|registration| registration.team)
        .collect();
    if winners.is_empty() || winners.len() > registered.len() {
        return Err(Refusal::bad("name at least one winner, from the registered teams"));
    }
    let mut named: Vec<Value> = Vec::new();
    for winner in &winners {
        let team = winner.team.trim().trim_start_matches("team:").to_string();
        if !registered.contains(&team) || !(1..=10).contains(&winner.place) {
            return Err(Refusal::bad(format!(
                "each winner is a registered team with a place from 1 to 10, not {team}"
            )));
        }
        let note: String = winner.note.trim().chars().take(300).collect();
        named.push(json!({ "team": team, "place": winner.place, "note": note }));
    }
    named.sort_by_key(|winner| winner["place"].as_u64());
    event.winners = named.clone();
    event.status = "announced".into();
    store.save_event(&event).await?;
    if let Some(thread) = event.thread {
        let lines: Vec<String> = named
            .iter()
            .map(|winner| {
                let note = winner["note"].as_str().filter(|note| !note.is_empty());
                let note = note.map(|note| format!(": {note}")).unwrap_or_default();
                format!(
                    "{}. /team:{}{note}",
                    winner["place"],
                    winner["team"].as_str().unwrap_or_default()
                )
            })
            .collect();
        let body = format!("The winners of {} are:\n\n{}", event.title, lines.join("\n"));
        if let Err(refusal) = api::reply(
            backend,
            sight,
            &thread.to_string(),
            NewReply { body, tags: Vec::new(), parent: None },
        )
        .await
        {
            tracing::warn!(detail = %refusal.detail, "the winners were not posted to the discussion");
        }
    }
    let payload = json!({ "event": event.id, "title": event.title, "team": event.team, "winners": named, "url": page(&event) });
    announce(backend, "plugin.water.hackathon.announced", payload).await;
    Ok(event)
}

/// Adds a score to a challenge.
pub async fn enter(backend: &Backend, id: &str, score: f64, note: &str) -> Result<Event, Refusal> {
    let me = login(backend)?;
    let store = Store(backend);
    let event = found(&store, id).await?;
    open(&event, "challenge")?;
    if !score.is_finite() {
        return Err(Refusal::bad("a score is a number"));
    }
    let entry = Entry {
        id: Uuid::now_v7(),
        event: event.id,
        entrant: me.clone(),
        score,
        note: note.trim().chars().take(300).collect(),
        created_at: String::new(),
    };
    store.enter(&entry).await?;
    let payload = json!({ "event": event.id, "title": event.title, "entrant": me, "score": score, "url": page(&event) });
    announce(backend, "plugin.water.challenge.entered", payload).await;
    Ok(event)
}

/// Each entrant's best score, the leader first.
pub fn leaderboard(event: &Event, entries: &[Entry]) -> Vec<(String, f64, usize)> {
    let mut best: Vec<(String, f64, usize)> = Vec::new();
    for entry in entries {
        match best.iter_mut().find(|(entrant, _, _)| *entrant == entry.entrant) {
            Some((_, score, count)) => {
                *count += 1;
                let better =
                    if event.higher_wins { entry.score > *score } else { entry.score < *score };
                if better {
                    *score = entry.score;
                }
            }
            None => best.push((entry.entrant.clone(), entry.score, 1)),
        }
    }
    best.sort_by(|a, b| {
        let order = a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal);
        if event.higher_wins { order.reverse() } else { order }
    });
    best
}

/// Stops registrations, submissions and entries.
pub async fn close(backend: &Backend, id: &str) -> Result<Event, Refusal> {
    let store = Store(backend);
    let mut event = found(&store, id).await?;
    organises(backend, &event)?;
    if event.status == "open" {
        event.status = "closed".into();
        store.save_event(&event).await?;
    }
    Ok(event)
}

/// Cancels an event, taking it off the team's calendar; its discussion stays.
pub async fn cancel(backend: &Backend, id: &str) -> Result<Event, Refusal> {
    let store = Store(backend);
    let event = found(&store, id).await?;
    organises(backend, &event)?;
    if let Some(calendar_event) = event.calendar_event {
        let route = format!("events/{calendar_event}/delete");
        if let Err(err) = backend.discovery("calendar-events", "POST", &route, None, None).await {
            tracing::warn!(%err, "a cancelled event is still on its team's calendar");
        }
    }
    store.delete_event(event.id).await?;
    Ok(event)
}

/// Who has answered a game night's calendar event, and how.
pub async fn answers(backend: &Backend, event: &Event) -> Vec<(String, String)> {
    let Some(calendar_event) = event.calendar_event else { return Vec::new() };
    match backend
        .discovery("calendar-events", "GET", &format!("events/{calendar_event}"), None, None)
        .await
    {
        Ok((200, shown)) => shown["rsvps"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|rsvp| {
                let text = |key: &str| rsvp[key].as_str().unwrap_or_default().to_string();
                (text("who"), text("response"))
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Answers a game night's invitation, as the caller, on its calendar event.
pub async fn rsvp(backend: &Backend, id: &str, response: &str) -> Result<Event, Refusal> {
    let event = found(&Store(backend), id).await?;
    let calendar_event = match (event.kind.as_str(), event.calendar_event) {
        ("game-night", Some(calendar_event)) => calendar_event,
        _ => return Err(Refusal::bad("answers are for game nights on a calendar")),
    };
    let asked = json!({ "event": calendar_event, "response": response });
    match backend.ask("calendar-events", "POST", "rsvps", None, Some(asked)).await {
        Ok((200, _)) => Ok(event),
        Ok((status, answer)) => Err(Refusal {
            status,
            detail: answer["detail"].as_str().unwrap_or("calendar-events refused it").to_string(),
        }),
        Err(err) => Err(Refusal::unavailable(format!("calendar-events could not be asked: {err}"))),
    }
}

pub fn registrations_shown(registrations: &[Registration]) -> Value {
    json!(registrations.iter().map(|registration| json!({ "team": registration.team, "by": registration.registered_by, "at": registration.created_at })).collect::<Vec<_>>())
}

pub fn event_shown(event: &Event) -> Value {
    json!({
        "id": event.id,
        "kind": event.kind,
        "title": event.title,
        "description": event.description,
        "html": event.html,
        "team": event.team,
        "organiser": event.organiser,
        "start": event.starts_local,
        "end": event.ends_local,
        "timezone": event.timezone,
        "starts_at": instant(event, &event.starts_local).map(|at| at.to_rfc3339()),
        "location": event.location,
        "thread": event.thread,
        "calendar_event": event.calendar_event,
        "higher_wins": event.higher_wins,
        "unit": event.unit,
        "status": event.status,
        "winners": event.winners,
        "url": page(event),
    })
}
