//! Planning: the shifts a rota gives from now to a little way ahead, whom the rotation puts on
//! each, which of those are away, and the calendar events that say so.
//!
//! A shift the rotation gives someone who is on holiday for any of it is a gap: nobody is on until
//! the lead picks cover, and the lead is told once. Someone the lead picked stays picked, and a
//! holiday booked over their shift later marks it as a clash for the lead to look at, rather than
//! quietly undoing what the lead decided. Shifts that have ended are history and never change.

use chrono::{DateTime, Days, Months, NaiveDate, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;
use doc_plugin_sdk::Backend;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::people::Directory;
use crate::store::{COVER, GAP, Holiday, PLANNED, Rota, Shift, Store};

/// How many turns ahead, counting the one running now, are planned.
pub fn horizon(cadence: &str) -> i64 {
    match cadence {
        "daily" => 14,
        "weekly" => 8,
        "monthly" => 4,
        _ => 2,
    }
}

pub fn zone(rota: &Rota) -> Tz {
    rota.timezone.parse().unwrap_or(Tz::UTC)
}

pub fn handover(text: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(text.trim(), "%H:%M").ok()
}

/// The day turn `period` starts on.
fn day_of(rota: &Rota, period: i64) -> Option<NaiveDate> {
    let start = rota.starts_on;
    let count = u32::try_from(period.unsigned_abs()).ok()?;
    let forward = period >= 0;
    let days = |days: u64| match forward {
        true => start.checked_add_days(Days::new(days)),
        false => start.checked_sub_days(Days::new(days)),
    };
    let months = |months: u32| match forward {
        true => start.checked_add_months(Months::new(months)),
        false => start.checked_sub_months(Months::new(months)),
    };
    match rota.cadence.as_str() {
        "daily" => days(u64::from(count)),
        "weekly" => days(u64::from(count) * 7),
        "monthly" => months(count),
        _ => months(count.checked_mul(3)?),
    }
}

/// When turn `period` starts: its day, at the handover time where the rota runs.
pub fn starts(rota: &Rota, period: i64) -> Option<DateTime<Utc>> {
    let day = day_of(rota, period)?;
    let time = handover(&rota.handover).unwrap_or(NaiveTime::MIN);
    let local = day.and_time(time);
    let zone = zone(rota);
    // A handover in a clock change's gap happens as the clocks go forward.
    let at = zone
        .from_local_datetime(&local)
        .earliest()
        .or_else(|| zone.from_local_datetime(&(local + chrono::Duration::hours(1))).earliest())?;
    Some(at.with_timezone(&Utc))
}

/// The turn running at `at`; before the rota starts, a negative one.
pub fn period_at(rota: &Rota, at: DateTime<Utc>) -> i64 {
    let days = (at.date_naive() - rota.starts_on).num_days();
    let mut period = match rota.cadence.as_str() {
        "daily" => days,
        "weekly" => days.div_euclid(7),
        "monthly" => days.div_euclid(30),
        _ => days.div_euclid(91),
    };
    for _ in 0..64 {
        match (starts(rota, period), starts(rota, period + 1)) {
            (Some(from), _) if from > at => period -= 1,
            (_, Some(next)) if next <= at => period += 1,
            _ => break,
        }
    }
    period
}

/// Whether the holiday covers any of `from`..`to` where the rota runs.
fn overlaps(holiday: &Holiday, rota: &Rota, from: DateTime<Utc>, to: DateTime<Utc>) -> bool {
    let zone = zone(rota);
    let edge = |day: NaiveDate| {
        zone.from_local_datetime(&day.and_time(NaiveTime::MIN))
            .earliest()
            .map(|at| at.with_timezone(&Utc))
    };
    let (Some(away), Some(back)) =
        (edge(holiday.starts_on), holiday.ends_on.succ_opt().and_then(edge))
    else {
        return false;
    };
    away < to && back > from
}

pub fn away(
    holidays: &[Holiday],
    rota: &Rota,
    login: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> bool {
    holidays.iter().any(|holiday| holiday.user == login && overlaps(holiday, rota, from, to))
}

/// Who could cover a shift: the rota's people who are not away for it, in the order the rotation
/// would reach them next, and anyone else in the team after them.
pub fn cover_for(
    rota: &Rota,
    shift: &Shift,
    directory: &Directory,
    holidays: &[Holiday],
) -> Vec<String> {
    let members = serving(rota, directory);
    let mut ordered: Vec<String> = Vec::new();
    if !members.is_empty() {
        let len = members.len() as i64;
        for step in 1..=len {
            ordered.push(members[(shift.period + step).rem_euclid(len) as usize].clone());
        }
    }
    for (person, _) in directory.members(rota.team_id) {
        if !ordered.contains(&person.login) {
            ordered.push(person.login.clone());
        }
    }
    ordered
        .into_iter()
        .filter(|login| shift.assignee.as_deref() != Some(login.as_str()))
        .filter(|login| !away(holidays, rota, login, shift.starts_at, shift.ends_at))
        .collect()
}

/// The rota's people who are still in its team, in its order.
pub fn serving(rota: &Rota, directory: &Directory) -> Vec<String> {
    rota.members.iter().filter(|login| directory.is_member(rota.team_id, login)).cloned().collect()
}

/// What changed in one run of the planner, for the answer and the log.
#[derive(Debug, Default)]
pub struct Planned {
    pub made: usize,
    pub changed: usize,
    pub gaps: usize,
    pub dropped: usize,
}

/// Brings `rota`'s shifts from now to its horizon up to date, then its calendar events, and tells
/// the lead of any gap or clash they have not heard of.
pub async fn plan(
    backend: &Backend,
    rota: &Rota,
    directory: &Directory,
    holidays: &[Holiday],
    now: DateTime<Utc>,
) -> Result<Planned, Refusal> {
    let store = Store(backend);
    let mut done = Planned::default();
    let members = serving(rota, directory);
    let first = period_at(rota, now).max(0);
    let last = first + horizon(&rota.cadence);
    let existing = store.shifts_from(rota.id, now).await?;
    for shift in &existing {
        let beyond = shift.period < first || shift.period >= last;
        // A turn the rota no longer has: its start or cadence moved, or it has nobody on it.
        let moved = starts(rota, shift.period) != Some(shift.starts_at);
        if (beyond && shift.starts_at > now)
            || moved
            || (members.is_empty() && shift.starts_at > now)
        {
            unsync(backend, shift).await;
            store.delete_shift(shift.id).await?;
            done.dropped += 1;
        }
    }
    if members.is_empty() {
        return Ok(done);
    }
    let existing = store.shifts_from(rota.id, now).await?;
    for period in first..last {
        let (Some(starts_at), Some(ends_at)) = (starts(rota, period), starts(rota, period + 1))
        else {
            continue;
        };
        let planned = members[period.rem_euclid(members.len() as i64) as usize].clone();
        let held = existing.iter().find(|shift| shift.period == period).cloned();
        let mut shift = held.clone().unwrap_or(Shift {
            id: Uuid::now_v7(),
            rota: rota.id,
            period,
            starts_at,
            ends_at,
            planned: None,
            assignee: None,
            state: PLANNED.into(),
            clash: false,
            flagged: false,
            user_event: None,
            user_event_for: None,
            team_event: None,
            synced: None,
        });
        shift.planned = Some(planned.clone());
        let covered = shift.state == COVER
            && shift
                .assignee
                .as_deref()
                .is_some_and(|login| directory.is_member(rota.team_id, login));
        if covered {
            let login = shift.assignee.clone().unwrap_or_default();
            shift.clash = away(holidays, rota, &login, starts_at, ends_at);
        } else if away(holidays, rota, &planned, starts_at, ends_at) {
            shift.state = GAP.into();
            shift.assignee = None;
            shift.clash = false;
        } else {
            shift.state = PLANNED.into();
            shift.assignee = Some(planned);
            shift.clash = false;
        }
        if !shift.wants_cover() {
            shift.flagged = false;
        }
        let tell = shift.wants_cover() && !shift.flagged;
        if tell {
            shift.flagged = flag(backend, rota, &shift, directory).await;
        }
        if shift.wants_cover() {
            done.gaps += 1;
        }
        if rota.calendar {
            sync(backend, rota, directory, &mut shift).await;
        } else if shift.user_event.is_some() || shift.team_event.is_some() {
            unsync(backend, &shift).await;
            (shift.user_event, shift.user_event_for, shift.team_event, shift.synced) =
                (None, None, None, None);
        }
        let changed = match &held {
            None => {
                done.made += 1;
                true
            }
            Some(before) => {
                let differs =
                    serde_json::to_value(before).ok() != serde_json::to_value(&shift).ok();
                done.changed += usize::from(differs);
                differs
            }
        };
        if changed {
            store.save_shift(&shift).await?;
        }
    }
    Ok(done)
}

/// Tells the team's lead that a shift needs cover, answering whether they were told.
async fn flag(backend: &Backend, rota: &Rota, shift: &Shift, directory: &Directory) -> bool {
    let when = local(rota, shift.starts_at);
    let (title, body) = match shift.state.as_str() {
        GAP => (
            format!("{}: nobody is on from {when}", rota.title),
            format!(
                "{} is away then. Pick someone to cover it.",
                shift.planned.as_deref().unwrap_or("Whoever it was")
            ),
        ),
        _ => (
            format!(
                "{}: {} is away during their shift from {when}",
                rota.title,
                shift.assignee.as_deref().unwrap_or("someone")
            ),
            "They were picked to cover it, but have a holiday for some of it.".to_string(),
        ),
    };
    let url = format!("/p/rota/rotas/{}", rota.id);
    let payload = json!({
        "rota": rota.id, "team": rota.team, "shift": shift.id, "state": shift.state,
        "planned": shift.planned, "assignee": shift.assignee,
        "starts_at": shift.starts_at, "ends_at": shift.ends_at, "url": url,
    });
    if let Err(err) = backend.publish("plugin.rota.shift.uncovered", payload).await {
        tracing::warn!(%err, "a gap was not announced");
    }
    let Some(lead) = directory.lead_for(rota.team_id) else {
        tracing::info!(rota = %rota.id, "a gap has no lead to tell");
        return true;
    };
    match backend.notify(&lead.id.to_string(), &title, &body, Some(&url)).await {
        Ok(()) => true,
        Err(err) => {
            tracing::warn!(%err, "the lead was not told of a gap");
            false
        }
    }
}

pub fn local(rota: &Rota, at: DateTime<Utc>) -> String {
    at.with_timezone(&zone(rota)).format("%a %-d %b %H:%M").to_string()
}

fn wall(rota: &Rota, at: DateTime<Utc>) -> String {
    at.with_timezone(&zone(rota)).naive_local().format("%Y-%m-%dT%H:%M:%S").to_string()
}

/// Puts the shift on the team's calendar, and on the person's while someone is on it.
async fn sync(backend: &Backend, rota: &Rota, directory: &Directory, shift: &mut Shift) {
    let team_title =
        directory.team(rota.team_id).map_or_else(|| rota.team.clone(), |team| team.title.clone());
    let on = shift.assignee.clone();
    let fingerprint = format!(
        "{}|{team_title}|{}|{}|{}|{:?}",
        rota.title, rota.timezone, shift.starts_at, shift.ends_at, on
    );
    if shift.synced.as_deref() == Some(fingerprint.as_str())
        && shift.team_event.is_some()
        && (on.is_none() || shift.user_event.is_some())
    {
        return;
    }
    // A calendar link is a whole URL, so one is given only where DOC knows its own address.
    let base = std::env::var("DOC_PUBLIC_URL").unwrap_or_default();
    let link = (!base.is_empty())
        .then(|| format!("{}/p/rota/rotas/{}", base.trim_end_matches('/'), rota.id));
    let details = |title: String, description: String| {
        let mut said = json!({
            "title": title,
            "description": format!("{description} Its shifts are at /p/rota/rotas/{}.", rota.id),
            "start": wall(rota, shift.starts_at),
            "end": wall(rota, shift.ends_at),
            "timezone": rota.timezone,
            "resource": format!("team:{}", rota.team),
        });
        if let Some(link) = &link {
            said["link"] = json!(link);
        }
        said
    };
    let team_line = match &on {
        Some(login) => format!("{}: {login}", rota.title),
        None => format!("{}: nobody yet (a gap)", rota.title),
    };
    let team_says = details(team_line, format!("{team_title}'s {} rota.", rota.title));
    shift.team_event =
        placed(backend, shift.team_event, &format!("team:{}", rota.team), team_says).await;
    // Someone else is on now: the last person's event goes, and the new person's comes.
    if shift.user_event_for != on
        && let Some(event) = shift.user_event.take()
    {
        remove(backend, event).await;
        shift.user_event_for = None;
    }
    if let Some(login) = &on {
        let says = details(
            format!("{} ({team_title})", rota.title),
            format!("Your turn on {team_title}'s {} rota.", rota.title),
        );
        shift.user_event = placed(backend, shift.user_event, &format!("user:{login}"), says).await;
        shift.user_event_for = shift.user_event.map(|_| login.clone());
    }
    shift.synced = Some(fingerprint);
}

/// Changes the event, or makes it where there is none or it has gone; its ID, if it is there.
async fn placed(backend: &Backend, event: Option<Uuid>, on: &str, details: Value) -> Option<Uuid> {
    if let Some(event) = event {
        match backend
            .discovery(
                "calendar-events",
                "PATCH",
                &format!("events/{event}"),
                None,
                Some(details.clone()),
            )
            .await
        {
            Ok((200, _)) => return Some(event),
            Ok((404, _)) => {}
            Ok((status, answer)) => {
                tracing::warn!(status, detail = %answer["detail"], "a shift's event was not changed");
                return Some(event);
            }
            Err(err) => {
                tracing::warn!(%err, "a shift's event was not changed");
                return Some(event);
            }
        }
    }
    let mut asked = details;
    asked["on"] = json!(on);
    match backend.discovery("calendar-events", "POST", "events", None, Some(asked)).await {
        Ok((201, made)) => made["id"].as_str().and_then(|id| id.parse().ok()),
        Ok((status, answer)) => {
            tracing::warn!(status, on, detail = %answer["detail"], "a shift is not on a calendar");
            None
        }
        Err(err) => {
            tracing::warn!(%err, on, "a shift is not on a calendar");
            None
        }
    }
}

async fn remove(backend: &Backend, event: Uuid) {
    let route = format!("events/{event}/delete");
    if let Err(err) = backend.discovery("calendar-events", "POST", &route, None, None).await {
        tracing::warn!(%err, "a shift's event was not taken off its calendar");
    }
}

/// Takes a shift off every calendar it is on.
pub async fn unsync(backend: &Backend, shift: &Shift) {
    for event in [shift.user_event, shift.team_event].into_iter().flatten() {
        remove(backend, event).await;
    }
}

/// Everyone the rotas given turn to, each once: who the calendar is asked about.
pub fn people_of(rotas: &[Rota], directory: &Directory) -> Vec<String> {
    let mut logins: Vec<String> = rotas.iter().flat_map(|rota| serving(rota, directory)).collect();
    logins.sort();
    logins.dedup();
    logins
}

/// Plans every rota, as the schedule does, or the ones of `team` only.
pub async fn plan_all(backend: &Backend, team: Option<Uuid>) -> Result<Value, Refusal> {
    let store = Store(backend);
    let directory = Directory::read(backend).await?;
    let now = Utc::now();
    let recorded = store.holidays_from(now.date_naive() - Days::new(1)).await?;
    let rotas = match team {
        Some(team) => store.rotas_of(team).await?,
        None => store.rotas().await?,
    };
    // Everyone any of these rotas turns to, so the calendar is asked once for the whole run.
    let logins = people_of(&rotas, &directory);
    let holidays =
        crate::away::everything(backend, recorded, &logins, now.date_naive() - Days::new(1)).await;
    let (mut made, mut changed, mut gaps, mut failed) = (0, 0, 0, 0);
    for rota in &rotas {
        match plan(backend, rota, &directory, &holidays, now).await {
            Ok(done) => {
                made += done.made;
                changed += done.changed;
                gaps += done.gaps;
            }
            Err(refusal) => {
                failed += 1;
                tracing::warn!(rota = %rota.id, detail = %refusal.detail, "a rota was not planned");
            }
        }
    }
    Ok(
        json!({ "rotas": rotas.len(), "made": made, "changed": changed, "gaps": gaps, "failed": failed }),
    )
}
