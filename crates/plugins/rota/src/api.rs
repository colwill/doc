//! What can be done with rotas, shared by the JSON routes at `api/` and the pages at `ui/`.
//! Anyone signed in sees every rota. A team's lead, the lead of a team above it, or an
//! administrator makes and changes its rotas, picks cover, and records its people's holidays.

use chrono::{Days, NaiveDate, Utc};
use chrono_tz::Tz;
use doc_plugin_sdk::{Backend, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::people::{Directory, Me};
use crate::plan::{self, handover};
use crate::store::{CADENCES, COVER, Holiday, PLANNED, Rota, Shift, Store};

const MAX_TITLE: usize = 100;
const MAX_DESCRIPTION: usize = 1000;
const MAX_MEMBERS: usize = 100;
const MAX_NOTE: usize = 200;
/// However long someone is away, a holiday is at most this many days; a longer absence is more
/// than one.
const MAX_HOLIDAY_DAYS: i64 = 366;

pub type Answer = Result<Value, Refusal>;

pub fn parameter(query: &str, key: &str) -> Option<String> {
    url::form_urlencoded::parse(query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn body<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T, Refusal> {
    let bytes = if request.body.is_empty() { &b"{}"[..] } else { &request.body[..] };
    serde_json::from_slice(bytes)
        .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))
}

pub fn id_of(text: &str, what: &str) -> Result<Uuid, Refusal> {
    text.parse().map_err(|_| Refusal::missing(format!("there is no such {what}")))
}

/// A rota as asked for: new, or everything it should now be.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct RotaInput {
    /// The team's name; a rota never moves to another team.
    pub team: String,
    pub title: String,
    pub description: String,
    pub cadence: String,
    pub starts_on: String,
    pub handover: String,
    pub timezone: String,
    pub members: Vec<String>,
    pub calendar: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct HolidayInput {
    pub user: String,
    pub starts_on: String,
    pub ends_on: String,
    pub note: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct CoverInput {
    /// Who covers it; none gives it back to whoever the rotation gives.
    pub user: Option<String>,
}

fn date(text: &str, what: &str) -> Result<NaiveDate, Refusal> {
    NaiveDate::parse_from_str(text.trim(), "%Y-%m-%d")
        .map_err(|_| Refusal::bad(format!("{what} is a date, such as 2026-10-05")))
}

/// Checks everything about a rota, ready to save.
fn shaped(input: &RotaInput, directory: &Directory, into: &mut Rota) -> Result<(), Refusal> {
    let title = input.title.trim();
    if title.is_empty() || title.chars().count() > MAX_TITLE || title.contains(char::is_control) {
        return Err(Refusal::bad(format!("a rota's title is 1 to {MAX_TITLE} characters")));
    }
    if input.description.chars().count() > MAX_DESCRIPTION {
        return Err(Refusal::bad(format!("a description is at most {MAX_DESCRIPTION} characters")));
    }
    let cadence = input.cadence.trim().to_lowercase();
    if !CADENCES.iter().any(|(name, _)| *name == cadence) {
        return Err(Refusal::bad("a rota runs daily, weekly, monthly or quarterly"));
    }
    let time = if input.handover.trim().is_empty() { "09:00" } else { input.handover.trim() };
    handover(time).ok_or_else(|| Refusal::bad("the handover is a time of day, such as 09:00"))?;
    let zone = if input.timezone.trim().is_empty() { "UTC" } else { input.timezone.trim() };
    zone.parse::<Tz>()
        .map_err(|_| Refusal::bad(format!("`{zone}` is not a time zone, such as Europe/London")))?;
    let mut members: Vec<String> = Vec::new();
    for login in input.members.iter().map(|login| login.trim()).filter(|login| !login.is_empty()) {
        let person = directory
            .person_named(login)
            .filter(|person| directory.is_member(into.team_id, &person.login))
            .ok_or_else(|| Refusal::bad(format!("{login} is not in {}", into.team)))?;
        if !members.contains(&person.login) {
            members.push(person.login.clone());
        }
    }
    if members.len() > MAX_MEMBERS {
        return Err(Refusal::bad(format!("a rota runs through at most {MAX_MEMBERS} people")));
    }
    into.title = title.to_string();
    into.description = input.description.trim().to_string();
    into.cadence = cadence;
    into.starts_on = date(&input.starts_on, "its first day")?;
    into.handover = time.to_string();
    into.timezone = zone.to_string();
    into.members = members;
    if let Some(calendar) = input.calendar {
        into.calendar = calendar;
    }
    Ok(())
}

pub async fn found(store: &Store<'_>, id: Uuid) -> Result<Rota, Refusal> {
    store.rota(id).await?.ok_or_else(|| Refusal::missing("there is no such rota"))
}

fn arranger(me: &Me, directory: &Directory, rota: &Rota) -> Result<(), Refusal> {
    match me.arranges(directory, rota.team_id) {
        true => Ok(()),
        false => Err(Refusal::forbidden(
            "a team's rotas are arranged by its lead, the lead of a team above it, or an \
             administrator",
        )),
    }
}

async fn replan(backend: &Backend, rota: &Rota, directory: &Directory) -> Result<(), Refusal> {
    let now = Utc::now();
    let from = now.date_naive() - Days::new(1);
    let recorded = Store(backend).holidays_from(from).await?;
    let logins = plan::people_of(std::slice::from_ref(rota), directory);
    let holidays = crate::away::everything(backend, recorded, &logins, from).await;
    plan::plan(backend, rota, directory, &holidays, now).await?;
    Ok(())
}

async fn audit(backend: &Backend, action: &str, subject: &str, detail: Value) {
    if let Err(err) = backend.audit(action, Some(subject), detail).await {
        tracing::warn!(%err, action, "a change was not audited");
    }
}

pub async fn create(backend: &Backend, input: RotaInput) -> Result<Rota, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let team = directory
        .team_named(input.team.trim().trim_start_matches("team:"))
        .ok_or_else(|| Refusal::bad(format!("there is no team called {}", input.team.trim())))?;
    let mut rota = Rota {
        id: Uuid::now_v7(),
        team_id: team.id,
        team: team.name.clone(),
        title: String::new(),
        description: String::new(),
        cadence: String::new(),
        starts_on: Utc::now().date_naive(),
        handover: String::new(),
        timezone: String::new(),
        members: Vec::new(),
        calendar: true,
        created_by: me.login.clone(),
    };
    arranger(&me, &directory, &rota)?;
    shaped(&input, &directory, &mut rota)?;
    let store = Store(backend);
    store.save_rota(&rota).await?;
    replan(backend, &rota, &directory).await?;
    audit(
        backend,
        "rota.created",
        &rota.id.to_string(),
        json!({ "team": rota.team, "title": rota.title }),
    )
    .await;
    Ok(rota)
}

pub async fn update(backend: &Backend, id: Uuid, input: RotaInput) -> Result<Rota, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let store = Store(backend);
    let mut rota = found(&store, id).await?;
    arranger(&me, &directory, &rota)?;
    shaped(&input, &directory, &mut rota)?;
    store.save_rota(&rota).await?;
    replan(backend, &rota, &directory).await?;
    audit(
        backend,
        "rota.changed",
        &rota.id.to_string(),
        json!({ "team": rota.team, "title": rota.title }),
    )
    .await;
    Ok(rota)
}

/// Deletes the rota, taking every shift still to come off the calendars.
pub async fn delete(backend: &Backend, id: Uuid) -> Result<Rota, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let store = Store(backend);
    let rota = found(&store, id).await?;
    arranger(&me, &directory, &rota)?;
    for shift in store.shifts_from(rota.id, Utc::now()).await? {
        plan::unsync(backend, &shift).await;
    }
    store.delete_rota(rota.id).await?;
    audit(
        backend,
        "rota.deleted",
        &rota.id.to_string(),
        json!({ "team": rota.team, "title": rota.title }),
    )
    .await;
    Ok(rota)
}

/// Puts someone on a shift instead of whoever the rotation gives, or gives it back to them.
pub async fn cover(
    backend: &Backend,
    shift: Uuid,
    input: CoverInput,
) -> Result<(Rota, Shift), Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let store = Store(backend);
    let mut found_shift =
        store.shift(shift).await?.ok_or_else(|| Refusal::missing("there is no such shift"))?;
    let rota = found(&store, found_shift.rota).await?;
    arranger(&me, &directory, &rota)?;
    if found_shift.ends_at <= Utc::now() {
        return Err(Refusal::bad("that shift is over"));
    }
    let chosen = input.user.as_deref().map(str::trim).filter(|login| !login.is_empty());
    match chosen {
        Some(login) => {
            let person = directory
                .person_named(login)
                .filter(|person| directory.is_member(rota.team_id, &person.login))
                .ok_or_else(|| Refusal::bad(format!("{login} is not in {}", rota.team)))?;
            found_shift.state = COVER.into();
            found_shift.assignee = Some(person.login.clone());
        }
        None => {
            found_shift.state = PLANNED.into();
            found_shift.assignee = found_shift.planned.clone();
        }
    }
    found_shift.flagged = false;
    store.save_shift(&found_shift).await?;
    replan(backend, &rota, &directory).await?;
    let saved = store.shift(shift).await?.unwrap_or(found_shift);
    let detail = json!({ "rota": rota.id, "assignee": saved.assignee, "state": saved.state });
    audit(backend, "rota.shift.covered", &shift.to_string(), detail.clone()).await;
    if let Err(err) = backend.publish("plugin.rota.shift.assigned", detail).await {
        tracing::warn!(%err, "a pick of cover was not announced");
    }
    Ok((rota, saved))
}

/// Records a holiday for someone in a team the caller leads, and plans the rotas they are on
/// again, so any shift of theirs it covers becomes a gap for their lead.
pub async fn add_holiday(backend: &Backend, input: HolidayInput) -> Result<Holiday, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let person = directory
        .person_named(input.user.trim())
        .ok_or_else(|| Refusal::bad(format!("nobody signs in as {}", input.user.trim())))?;
    if !me.records_for(&directory, person.id) {
        return Err(Refusal::forbidden(
            "holidays are recorded by the lead of a team the person is in, or an administrator",
        ));
    }
    let starts_on = date(&input.starts_on, "the first day away")?;
    let ends_on = date(&input.ends_on, "the last day away")?;
    if ends_on < starts_on {
        return Err(Refusal::bad("the last day away is on or after the first"));
    }
    if (ends_on - starts_on).num_days() >= MAX_HOLIDAY_DAYS {
        return Err(Refusal::bad(
            "a holiday is at most a year; record a longer absence as more than one",
        ));
    }
    if input.note.chars().count() > MAX_NOTE {
        return Err(Refusal::bad(format!("a note is at most {MAX_NOTE} characters")));
    }
    let holiday = Holiday {
        source: String::new(),
        id: Uuid::now_v7(),
        user: person.login.clone(),
        user_id: person.id,
        starts_on,
        ends_on,
        note: input.note.trim().to_string(),
        recorded_by: me.login.clone(),
    };
    Store(backend).add_holiday(&holiday).await?;
    replan_for(backend, &directory, person.id).await?;
    audit(
        backend,
        "rota.holiday.recorded",
        &holiday.id.to_string(),
        json!({ "user": holiday.user, "starts_on": holiday.starts_on, "ends_on": holiday.ends_on }),
    )
    .await;
    Ok(holiday)
}

pub async fn delete_holiday(backend: &Backend, id: Uuid) -> Result<Holiday, Refusal> {
    let me = Me::of(backend)?;
    let directory = Directory::read(backend).await?;
    let store = Store(backend);
    let holiday =
        store.holiday(id).await?.ok_or_else(|| Refusal::missing("there is no such holiday"))?;
    if !me.records_for(&directory, holiday.user_id) {
        return Err(Refusal::forbidden(
            "holidays are recorded by the lead of a team the person is in, or an administrator",
        ));
    }
    store.delete_holiday(id).await?;
    replan_for(backend, &directory, holiday.user_id).await?;
    audit(backend, "rota.holiday.deleted", &id.to_string(), json!({ "user": holiday.user })).await;
    Ok(holiday)
}

/// Plans again every rota of every team the person is in.
async fn replan_for(backend: &Backend, directory: &Directory, person: Uuid) -> Result<(), Refusal> {
    let store = Store(backend);
    let now = Utc::now();
    let from = now.date_naive() - Days::new(1);
    let recorded = store.holidays_from(from).await?;
    let mut theirs = Vec::new();
    for team in directory.teams_of(person) {
        theirs.extend(store.rotas_of(team.id).await?);
    }
    let logins = plan::people_of(&theirs, directory);
    let holidays = crate::away::everything(backend, recorded, &logins, from).await;
    for rota in &theirs {
        plan::plan(backend, rota, directory, &holidays, now).await?;
    }
    Ok(())
}

/// The holidays the caller may see: their own, and those of the people in teams they lead.
pub async fn visible_holidays(
    backend: &Backend,
    me: &Me,
    directory: &Directory,
) -> Result<Vec<Holiday>, Refusal> {
    let today = Utc::now().date_naive();
    let recorded = Store(backend).holidays_from(today).await?;
    // Whoever they may see absences for: themselves, and the people in the teams they lead.
    let mut logins: Vec<String> = std::iter::once(me.login.clone())
        .chain(
            directory
                .people
                .values()
                .filter(|person| me.records_for(directory, person.id))
                .map(|person| person.login.clone()),
        )
        .collect();
    logins.sort();
    logins.dedup();
    let all = crate::away::everything(backend, recorded, &logins, today).await;
    Ok(all
        .into_iter()
        .filter(|holiday| {
            holiday.user == me.login
                || me.records_for(directory, holiday.user_id)
                // A calendar's carries nobody's ID, so it is judged by whose calendar it is.
                || (holiday.source == crate::away::CALENDAR
                    && directory
                        .people
                        .values()
                        .find(|person| person.login == holiday.user)
                        .is_some_and(|person| me.records_for(directory, person.id)))
        })
        .collect())
}

fn shift_shown(shift: &Shift) -> Value {
    json!({
        "id": shift.id, "period": shift.period, "starts_at": shift.starts_at,
        "ends_at": shift.ends_at, "planned": shift.planned, "assignee": shift.assignee,
        "state": shift.state, "clash": shift.clash,
    })
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let answered = route(backend, request, path).await;
    match answered {
        Ok(Value::Null) => Response::new(204, "application/json", Vec::new()),
        Ok(value) => Response::json(&value),
        Err(refusal) => refusal.response(),
    }
}

async fn route(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let store = Store(backend);
    match (request.method.as_str(), path) {
        ("GET", ["rotas"]) => {
            let rotas = match parameter(&request.query, "team") {
                Some(team) => {
                    let directory = Directory::read(backend).await?;
                    let team = directory.team_named(&team).ok_or_else(|| {
                        Refusal::missing(format!("there is no team called {team}"))
                    })?;
                    store.rotas_of(team.id).await?
                }
                None => store.rotas().await?,
            };
            Ok(json!({ "rotas": rotas }))
        }
        ("POST", ["rotas"]) => Ok(json!(create(backend, body(request)?).await?)),
        ("GET", ["rotas", id]) => {
            let rota = found(&store, id_of(id, "rota")?).await?;
            let shifts = store.shifts_from(rota.id, Utc::now()).await?;
            Ok(
                json!({ "rota": rota, "shifts": shifts.iter().map(shift_shown).collect::<Vec<_>>() }),
            )
        }
        ("PUT", ["rotas", id]) => {
            Ok(json!(update(backend, id_of(id, "rota")?, body(request)?).await?))
        }
        ("DELETE", ["rotas", id]) => {
            delete(backend, id_of(id, "rota")?).await?;
            Ok(Value::Null)
        }
        ("POST", ["shifts", id, "cover"]) => {
            let (_, shift) = cover(backend, id_of(id, "shift")?, body(request)?).await?;
            Ok(shift_shown(&shift))
        }
        ("GET", ["me", "shifts"]) => {
            let me = Me::of(backend)?;
            let shifts = store.shifts_of(&me.login, Utc::now()).await?;
            Ok(json!({ "shifts": shifts.iter().map(shift_shown).collect::<Vec<_>>() }))
        }
        ("GET", ["holidays"]) => {
            let me = Me::of(backend)?;
            let directory = Directory::read(backend).await?;
            Ok(json!({ "holidays": visible_holidays(backend, &me, &directory).await? }))
        }
        ("POST", ["holidays"]) => Ok(json!(add_holiday(backend, body(request)?).await?)),
        ("DELETE", ["holidays", id]) => {
            delete_holiday(backend, id_of(id, "holiday")?).await?;
            Ok(Value::Null)
        }
        _ => Err(Refusal::missing("no such route")),
    }
}
