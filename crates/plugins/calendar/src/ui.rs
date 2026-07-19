//! The plugin's pages at `/p/calendar/...`: month, week and agenda views of one calendar or of all
//! of a person's, event pages with RSVP, event forms for writers, feeds, and a resource-page panel.

use std::collections::BTreeMap;

use askama::Template;
use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Timelike, Utc};
use chrono_tz::Tz;
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::api::{self, Sight, found, occurrences, personal, query, theirs};
use crate::store::{Calendar, Store};

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    fn refused(detail: impl Into<String>) -> Self {
        Self { notice: None, error: Some(detail.into()) }
    }
}

type Form = Vec<(String, String)>;

fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> Option<String> {
    form.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The grid draws in quarter hours, which is as fine as anybody books.
const SLOTS: u16 = 96;
/// How many events a month cell shows before it says how many more there are.
const CHIPS: usize = 3;

/// How many colours there are for the calendars in somebody's own view. Their own calendar is
/// drawn in the platform's colour, and each team and subscription takes the next tone along.
const TONES: usize = 8;

/// What one calendar's events are called and coloured in a view.
pub struct Shade {
    pub name: String,
    /// 0 for the platform's own colour, 1 to `TONES` for a calendar of its own.
    pub tone: u8,
}

pub struct Item {
    pub time: String,
    /// The time it starts, on its own, for a month cell where there is no room for both.
    pub from: String,
    pub title: String,
    pub href: String,
    pub calendar: String,
    /// Which colour its calendar has in this view.
    pub tone: u8,
    pub location: String,
    pub all_day: bool,
    /// Quarter hours from midnight, for the day and week grids.
    pub start: u16,
    pub span: u16,
    /// Side by side with what it overlaps: which lane it is in, of how many.
    pub lane: u16,
    pub lanes: u16,
}

pub struct Day {
    /// The whole day, for the list: `Mon 1 Sep`.
    pub label: String,
    /// The weekday on its own, for a column heading: `Mon`.
    pub weekday: String,
    pub date: String,
    pub number: u32,
    pub in_range: bool,
    pub today: bool,
    pub items: Vec<Item>,
}

impl Day {
    /// What a month cell shows, and how many it leaves for the day view to show.
    pub fn chips(&self) -> Vec<&Item> {
        self.items.iter().take(CHIPS).collect()
    }

    pub fn more(&self) -> usize {
        self.items.len().saturating_sub(CHIPS)
    }

    pub fn all_day(&self) -> Vec<&Item> {
        self.items.iter().filter(|item| item.all_day).collect()
    }

    pub fn timed(&self) -> Vec<&Item> {
        self.items.iter().filter(|item| !item.all_day).collect()
    }
}

/// Where an occurrence sits on one day: the time as it is written, and the quarter hours it
/// covers, so the grid can place it.
struct Placed {
    day: NaiveDate,
    time: String,
    from: String,
    all_day: bool,
    start: u16,
    span: u16,
}

/// Events that overlap stand side by side, as they do in a diary: each one is given a lane, and
/// everything that overlaps it, directly or through another event, is given the same number of
/// lanes so the columns line up.
fn lay_out(items: &mut [Item]) {
    let mut timed: Vec<usize> = (0..items.len()).filter(|&i| !items[i].all_day).collect();
    timed.sort_by_key(|&i| (items[i].start, u16::MAX - items[i].span));
    let mut cluster: Vec<usize> = Vec::new();
    let mut lanes: Vec<u16> = Vec::new(); // where each lane is free from
    let mut ends: u16 = 0;
    let close = |cluster: &mut Vec<usize>, items: &mut [Item], lanes: usize| {
        let lanes = u16::try_from(lanes).unwrap_or(1).max(1);
        for &i in cluster.iter() {
            items[i].lanes = lanes;
        }
        cluster.clear();
    };
    for &i in &timed {
        if items[i].start >= ends && !cluster.is_empty() {
            close(&mut cluster, items, lanes.len());
            lanes.clear();
        }
        let lane = match lanes.iter().position(|free| *free <= items[i].start) {
            Some(lane) => lane,
            None => {
                lanes.push(0);
                lanes.len() - 1
            }
        };
        lanes[lane] = items[i].start + items[i].span;
        items[i].lane = u16::try_from(lane).unwrap_or(0);
        ends = ends.max(items[i].start + items[i].span);
        cluster.push(i);
    }
    close(&mut cluster, items, lanes.len());
}

pub struct Choice {
    pub id: Uuid,
    pub name: String,
    pub selected: bool,
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

#[derive(Template)]
#[template(path = "view.html")]
struct ViewPage {
    flash: Flash,
    writes: bool,
    title: String,
    here: String,
    mode: String,
    zone: String,
    heading: String,
    date: String,
    weeks: Vec<Vec<Day>>,
    days: Vec<Day>,
    hours: Vec<String>,
    previous: String,
    next: String,
    today: String,
    calendar: Option<Calendar>,
    subscribed: bool,
    /// The calendars the view draws, with their colours, for the key above it.
    calendars: Vec<Shade>,
}

#[derive(Template)]
#[template(path = "calendars.html")]
struct CalendarsPage {
    flash: Flash,
    writes: bool,
    calendars: Vec<Value>,
}

#[derive(Template)]
#[template(path = "feeds.html")]
struct FeedsPage {
    flash: Flash,
    feeds: Vec<Value>,
}

/// Getting a new feed's URL, on a page of its own.
#[derive(Template)]
#[template(path = "feed_new.html")]
struct NewFeedPage {
    flash: Flash,
    choices: Vec<Choice>,
    chosen: String,
}

#[derive(Template)]
#[template(path = "event.html")]
struct EventPage {
    flash: Flash,
    id: String,
    title: String,
    calendar_name: String,
    location: String,
    link: String,
    description: String,
    resource: String,
    made_by: String,
    when: String,
    repeats: String,
    upcoming: Vec<(String, String)>,
    rsvps: Vec<(String, String, String)>,
    attendees: String,
    editable: bool,
    calendar_href: String,
}

#[derive(Template)]
#[template(path = "event_form.html")]
struct EventForm {
    flash: Flash,
    writes: bool,
    action: String,
    heading: String,
    choices: Vec<Choice>,
    fields: BTreeMap<&'static str, String>,
    all_day: bool,
    /// Why they will not be working, where the event says so: `holiday`, `sick` or `other`.
    away: String,
    repeat: String,
    reminder: String,
}

#[derive(Template)]
#[template(path = "panel.html")]
struct PanelFragment {
    writes: bool,
    calendar: Calendar,
    items: Vec<(String, Item)>,
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let fragment = matches!(path, ["panel"] | ["dashboard", ..]);
    let mut moved = None;
    match route(backend, request, path, &mut moved).await {
        Ok(html) => match moved {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) if fragment => {
            let text = format!(
                "<p class=\"doc-error-message\" role=\"alert\">{}</p>",
                escape(&refusal.detail)
            );
            Response::new(refusal.status, "text/html; charset=utf-8", text)
        }
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(refusal.detail.clone()) };
            let html = page.render().unwrap_or_else(|_| escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

/// A page for `path`. `moved` is where the address bar goes when a form lands somewhere other
/// than where it was sent from.
async fn route(
    backend: &Backend,
    request: &Request,
    path: &[&str],
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    let store = Store(backend);
    let mut sight = Sight::new(backend);
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => view(backend, &mut sight, request, None, Flash::default()).await,
        ("GET", ["c", id]) => {
            let calendar = found(&store, &mut sight, id).await?;
            view(backend, &mut sight, request, Some(calendar), Flash::default()).await
        }
        ("GET", ["calendars"]) => calendars(backend, &mut sight, Flash::default()).await,
        ("POST", ["subscribe"]) => {
            let form = form(request);
            let id = field(&form, "calendar").unwrap_or_default();
            let calendar = found(&store, &mut sight, &id).await?;
            let on = field(&form, "subscribed").as_deref() != Some("false");
            let (me, _) = api::principal(backend)?;
            let by_default = api::teams_of(backend, &me).await?.contains(&calendar.resource);
            store.subscribe(&me, calendar.id, on, by_default).await?;
            let notice = match (on, by_default) {
                (true, true) => format!(
                    "{} is back in your calendar and your feed, as your team's.",
                    calendar.name
                ),
                (true, false) => format!(
                    "You subscribe to {} now; its events are in your calendar and your feed.",
                    calendar.name
                ),
                (false, true) => format!(
                    "{} is out of your calendar, though you are still in the team.",
                    calendar.name
                ),
                (false, false) => format!("You no longer subscribe to {}.", calendar.name),
            };
            match field(&form, "back").as_deref() {
                Some("calendar") => {
                    view(backend, &mut sight, request, Some(calendar), Flash::done(notice)).await
                }
                _ => calendars(backend, &mut sight, Flash::done(notice)).await,
            }
        }
        ("GET", ["feeds"]) => feeds(backend, Flash::default()).await,
        ("GET", ["feeds", "new"]) => {
            new_feed(backend, &mut sight, String::new(), Flash::default()).await
        }
        ("POST", ["feeds"]) => {
            let calendar = field(&form(request), "calendar");
            let chosen = calendar.clone().unwrap_or_default();
            let asked = json!({ "calendar": calendar.filter(|id| id != "all") });
            match api::feeds_add(backend, &mut sight, asked).await {
                Ok(_) => {
                    *moved = Some("/p/calendar/feeds".into());
                    let flash = Flash::done(
                        "Here is its secret URL. Add it to a calendar app as a subscription.",
                    );
                    feeds(backend, flash).await
                }
                Err(refusal) => {
                    new_feed(backend, &mut sight, chosen, Flash::refused(refusal.detail)).await
                }
            }
        }
        ("POST", ["feeds", id, "rotate"]) => {
            let flash = match api::feeds_rotate(backend, id).await {
                Ok(_) => Flash::done(
                    "A new secret URL; the old one no longer works, so give the new one to your calendar app.",
                ),
                Err(refusal) => Flash::refused(refusal.detail),
            };
            feeds(backend, flash).await
        }
        ("GET", ["events", "new"]) => {
            let calendar = query(request, "calendar").and_then(|id| id.parse().ok());
            let date =
                query(request, "date").unwrap_or_else(|| Utc::now().format("%Y-%m-%d").to_string());
            new_event(backend, &mut sight, calendar, &date, Flash::default()).await
        }
        ("POST", ["events"]) => {
            let form = form(request);
            match asked_event(&form) {
                Ok(asked) => {
                    match backend.ask("calendar-events", "POST", "events", None, Some(asked)).await
                    {
                        Ok((201, made)) => {
                            event(
                                backend,
                                made["id"].as_str().unwrap_or_default(),
                                Flash::done("Saved."),
                            )
                            .await
                        }
                        Ok((_, refused)) => {
                            let calendar = field(&form, "calendar").and_then(|id| id.parse().ok());
                            let date = field(&form, "start").unwrap_or_default();
                            new_event(
                                backend,
                                &mut sight,
                                calendar,
                                date.get(..10).unwrap_or_default(),
                                Flash::refused(detail(&refused)),
                            )
                            .await
                        }
                        Err(err) => Err(Refusal::unavailable(format!(
                            "calendar-events could not be asked: {err}"
                        ))),
                    }
                }
                Err(refusal) => {
                    let calendar = field(&form, "calendar").and_then(|id| id.parse().ok());
                    new_event(backend, &mut sight, calendar, "", Flash::refused(refusal.detail))
                        .await
                }
            }
        }
        ("GET", ["events", id]) => event(backend, id, Flash::default()).await,
        ("GET", ["events", id, "edit"]) => {
            edit_event(backend, &mut sight, id, Flash::default()).await
        }
        ("POST", ["events", id]) => {
            let form = form(request);
            let mut asked = asked_event(&form)?;
            if let Some(fields) = asked.as_object_mut() {
                fields.remove("calendar");
            }
            match backend
                .ask("calendar-events", "PATCH", &format!("events/{id}"), None, Some(asked))
                .await
            {
                Ok((200, _)) => event(backend, id, Flash::done("Saved.")).await,
                Ok((_, refused)) => {
                    edit_event(backend, &mut sight, id, Flash::refused(detail(&refused))).await
                }
                Err(err) => {
                    Err(Refusal::unavailable(format!("calendar-events could not be asked: {err}")))
                }
            }
        }
        ("POST", ["events", id, "delete"]) => {
            let shown = event_json(backend, id).await?;
            match backend
                .ask("calendar-events", "DELETE", &format!("events/{id}"), None, None)
                .await
            {
                Ok((204, _)) => {
                    let calendar =
                        found(&store, &mut sight, shown["calendar"].as_str().unwrap_or_default())
                            .await?;
                    let notice =
                        format!("Deleted {}.", shown["title"].as_str().unwrap_or("the event"));
                    view(backend, &mut sight, request, Some(calendar), Flash::done(notice)).await
                }
                Ok((_, refused)) => event(backend, id, Flash::refused(detail(&refused))).await,
                Err(err) => {
                    Err(Refusal::unavailable(format!("calendar-events could not be asked: {err}")))
                }
            }
        }
        ("POST", ["rsvp"]) => {
            let form = form(request);
            let id = field(&form, "event").unwrap_or_default();
            let asked = json!({
                "event": id,
                "occurrence": field(&form, "occurrence"),
                "response": field(&form, "response").unwrap_or_default(),
            });
            let flash = match backend
                .ask("calendar-events", "POST", "rsvps", None, Some(asked))
                .await
            {
                Ok((200, _)) => Flash::done("Your answer is saved."),
                Ok((_, refused)) => Flash::refused(detail(&refused)),
                Err(err) => Flash::refused(format!("calendar-events could not be asked: {err}")),
            };
            event(backend, &id, flash).await
        }
        ("GET", ["panel"]) => panel(backend, &mut sight, request).await,
        ("GET", ["dashboard", "upcoming"]) => upcoming(backend).await,
        _ => Err(Refusal::missing("no such page")),
    }
}

fn detail(refused: &Value) -> String {
    refused["detail"].as_str().unwrap_or("it was refused").to_string()
}

fn zone_of(text: Option<&str>, fallback: &str) -> Tz {
    text.and_then(|zone| zone.parse().ok()).or_else(|| fallback.parse().ok()).unwrap_or(Tz::UTC)
}

/// An occurrence's day or days and its time, in the zone the page is drawn in.
/// Which quarter hour of the day a time is in.
fn slot_of(at: &DateTime<Tz>) -> u16 {
    u16::try_from(at.hour() * 4 + at.minute() / 15).unwrap_or(0).min(SLOTS - 1)
}

fn placed(occurrence: &Value, zone: Tz) -> Vec<Placed> {
    let start = occurrence["start"].as_str().unwrap_or_default();
    let end = occurrence["end"].as_str().unwrap_or_default();
    if occurrence["all_day"] == true {
        let (Ok(first), Ok(after)) = (
            NaiveDate::parse_from_str(start, "%Y-%m-%d"),
            NaiveDate::parse_from_str(end, "%Y-%m-%d"),
        ) else {
            return Vec::new();
        };
        return first
            .iter_days()
            .take_while(|day| *day < after)
            .take(62)
            .map(|day| Placed {
                day,
                time: "All day".to_string(),
                from: "All day".to_string(),
                all_day: true,
                start: 0,
                span: SLOTS,
            })
            .collect();
    }
    let (Ok(start), Ok(end)) =
        (DateTime::parse_from_rfc3339(start), DateTime::parse_from_rfc3339(end))
    else {
        return Vec::new();
    };
    let (start, end) = (start.with_timezone(&zone), end.with_timezone(&zone));
    let time = format!("{}–{}", start.format("%H:%M"), end.format("%H:%M"));
    let from = slot_of(&start);
    // An event that runs past midnight is shown to the end of the day it started; the grid draws
    // one day to a column, and the rest of it is in the next day's own column.
    let until = match end.date_naive() > start.date_naive() {
        true => SLOTS,
        false => slot_of(&end).max(from + 1),
    };
    vec![Placed {
        day: start.date_naive(),
        time,
        from: start.format("%H:%M").to_string(),
        all_day: false,
        start: from,
        span: until - from,
    }]
}

fn item(occurrence: &Value, at: Placed, shades: &BTreeMap<String, Shade>) -> Item {
    let shade = occurrence["calendar"].as_str().and_then(|id| shades.get(id));
    Item {
        time: at.time,
        from: at.from,
        all_day: at.all_day,
        start: at.start,
        span: at.span,
        lane: 0,
        lanes: 1,
        title: occurrence["title"].as_str().unwrap_or_default().to_string(),
        href: format!("/p/calendar/events/{}", occurrence["event"].as_str().unwrap_or_default()),
        calendar: shade.map(|shade| shade.name.clone()).unwrap_or_default(),
        tone: shade.map_or(0, |shade| shade.tone),
        location: occurrence["location"].as_str().unwrap_or_default().to_string(),
    }
}

/// A colour for each calendar in a view: the person's own keeps the platform's, and everything
/// else, a team's calendar above all, takes one of its own so its events are told apart at a
/// glance. One calendar on its own is drawn in the platform's colour.
fn shades(calendars: &[Calendar]) -> BTreeMap<String, Shade> {
    let mut next: u8 = 0;
    calendars
        .iter()
        .map(|calendar| {
            let tone = match calendars.len() > 1 && calendar.owner.is_none() {
                true => {
                    next = next % u8::try_from(TONES).unwrap_or(8) + 1;
                    next
                }
                false => 0,
            };
            (calendar.id.to_string(), Shade { name: calendar.name.clone(), tone })
        })
        .collect()
}

/// A month, a week or the next thirty days of one calendar, or of all of a person's.
async fn view(
    backend: &Backend,
    sight: &mut Sight<'_>,
    request: &Request,
    calendar: Option<Calendar>,
    flash: Flash,
) -> Result<String, Refusal> {
    let mode = query(request, "view")
        .filter(|mode| matches!(mode.as_str(), "month" | "week" | "day" | "agenda"))
        .unwrap_or_else(|| "month".into());
    let calendars = match &calendar {
        Some(one) => vec![one.clone()],
        None => theirs(backend, sight).await?,
    };
    let fallback = match &calendar {
        Some(one) => one.timezone.clone(),
        None => personal(backend).await?.timezone,
    };
    let zone = zone_of(query(request, "tz").as_deref(), &fallback);
    let today = Utc::now().with_timezone(&zone).date_naive();
    let date = query(request, "date")
        .and_then(|date| NaiveDate::parse_from_str(&date, "%Y-%m-%d").ok())
        .unwrap_or(today);
    let (first, last, previous, next, heading) = match mode.as_str() {
        "day" => (
            date,
            date,
            date - Duration::days(1),
            date + Duration::days(1),
            date.format("%A %-d %B %Y").to_string(),
        ),
        "week" => {
            let monday = date - Duration::days(i64::from(date.weekday().num_days_from_monday()));
            (
                monday,
                monday + Duration::days(6),
                monday - Duration::days(7),
                monday + Duration::days(7),
                format!("Week of {}", monday.format("%-d %B %Y")),
            )
        }
        "agenda" => (
            date,
            date + Duration::days(29),
            date - Duration::days(30),
            date + Duration::days(30),
            format!("From {}", date.format("%-d %B %Y")),
        ),
        _ => {
            let start = date.with_day(1).unwrap_or(date);
            let grid = start - Duration::days(i64::from(start.weekday().num_days_from_monday()));
            let following = if start.month() == 12 {
                NaiveDate::from_ymd_opt(start.year() + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(start.year(), start.month() + 1, 1)
            }
            .unwrap_or(start);
            let before = (start - Duration::days(1)).with_day(1).unwrap_or(start);
            (grid, grid + Duration::days(41), before, following, start.format("%B %Y").to_string())
        }
    };
    let from = zone
        .from_local_datetime(&first.and_hms_opt(0, 0, 0).unwrap_or_default())
        .earliest()
        .map(|at| at.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let to = zone
        .from_local_datetime(&(last + Duration::days(1)).and_hms_opt(0, 0, 0).unwrap_or_default())
        .earliest()
        .map(|at| at.with_timezone(&Utc))
        .unwrap_or_else(Utc::now);
    let listed = occurrences(backend, &calendars, &from.to_rfc3339(), &to.to_rfc3339()).await?;
    let shades = shades(&calendars);
    let mut by_day: BTreeMap<NaiveDate, Vec<Item>> = BTreeMap::new();
    for occurrence in &listed {
        for at in placed(occurrence, zone) {
            let day = at.day;
            by_day.entry(day).or_default().push(item(occurrence, at, &shades));
        }
    }
    for items in by_day.values_mut() {
        items.sort_by_key(|item| (!item.all_day, item.start, item.title.clone()));
        lay_out(items);
    }
    let in_range = |day: NaiveDate| match mode.as_str() {
        "month" => day.month() == date.month(),
        _ => true,
    };
    // The hours down the side of the day and week grids, labelled on the hour.
    let hours: Vec<String> = (0..24).map(|hour| format!("{hour:02}:00")).collect();
    let days: Vec<Day> = first
        .iter_days()
        .take_while(|day| *day <= last)
        .map(|day| Day {
            label: day.format("%a %-d %b").to_string(),
            weekday: day.format("%a").to_string(),
            date: day.format("%Y-%m-%d").to_string(),
            number: day.day(),
            in_range: in_range(day),
            today: day == today,
            items: by_day.remove(&day).unwrap_or_default(),
        })
        .collect();
    let (weeks, days) = match mode.as_str() {
        "day" => (Vec::new(), days),
        "month" => {
            let mut weeks: Vec<Vec<Day>> = Vec::new();
            for day in days {
                match weeks.last_mut().filter(|week| week.len() < 7) {
                    Some(week) => week.push(day),
                    None => weeks.push(vec![day]),
                }
            }
            (weeks, Vec::new())
        }
        "agenda" => (Vec::new(), days.into_iter().filter(|day| !day.items.is_empty()).collect()),
        _ => (Vec::new(), days),
    };
    let here = match &calendar {
        Some(one) => format!("/p/calendar/c/{}", one.id),
        None => "/p/calendar/".into(),
    };
    let (me, _) = api::principal(backend)?;
    let subscribed = match &calendar {
        Some(one) => api::following(backend, &me).await?.contains(&one.id),
        None => false,
    };
    let fmt = |day: NaiveDate| day.format("%Y-%m-%d").to_string();
    render(&ViewPage {
        flash,
        writes: backend.writes(),
        title: calendar
            .as_ref()
            .map_or_else(|| "Your calendar".to_string(), |one| one.name.clone()),
        here,
        zone: zone.name().to_string(),
        heading,
        date: fmt(date),
        weeks,
        days,
        hours,
        previous: fmt(previous),
        next: fmt(next),
        today: fmt(today),
        mode,
        subscribed,
        calendars: calendars
            .iter()
            .filter_map(|calendar| shades.get(&calendar.id.to_string()))
            .map(|shade| Shade { name: shade.name.clone(), tone: shade.tone })
            .collect(),
        calendar,
    })
}

async fn calendars(
    backend: &Backend,
    sight: &mut Sight<'_>,
    flash: Flash,
) -> Result<String, Refusal> {
    let listed = api::visible_calendars(backend, sight).await?;
    render(&CalendarsPage { flash, writes: backend.writes(), calendars: listed })
}

async fn feeds(backend: &Backend, flash: Flash) -> Result<String, Refusal> {
    let (me, _) = api::principal(backend)?;
    let store = Store(backend);
    let names: BTreeMap<Uuid, String> =
        store.calendars().await?.into_iter().map(|calendar| (calendar.id, calendar.name)).collect();
    let listed: Vec<Value> =
        store.feeds(&me).await?.iter().map(|feed| api::feed_shown(feed, &names)).collect();
    render(&FeedsPage { flash, feeds: listed })
}

async fn new_feed(
    backend: &Backend,
    sight: &mut Sight<'_>,
    chosen: String,
    flash: Flash,
) -> Result<String, Refusal> {
    let choices = choices(backend, sight, None).await?;
    render(&NewFeedPage { flash, choices, chosen })
}

/// The calendars someone may put events on: their own, and each they can see.
async fn choices(
    backend: &Backend,
    sight: &mut Sight<'_>,
    selected: Option<Uuid>,
) -> Result<Vec<Choice>, Refusal> {
    let mut listed = vec![personal(backend).await?];
    for calendar in Store(backend).calendars().await? {
        if calendar.owner.is_none() && sight.sees(&calendar).await {
            listed.push(calendar);
        }
    }
    Ok(listed
        .into_iter()
        .map(|calendar| Choice {
            selected: Some(calendar.id) == selected,
            id: calendar.id,
            name: calendar.name,
        })
        .collect())
}

async fn new_event(
    backend: &Backend,
    sight: &mut Sight<'_>,
    calendar: Option<Uuid>,
    date: &str,
    flash: Flash,
) -> Result<String, Refusal> {
    let choices = choices(backend, sight, calendar).await?;
    let zone = match calendar {
        Some(id) => Store(backend).calendar(id).await?.map(|calendar| calendar.timezone),
        None => None,
    }
    .unwrap_or_else(|| "UTC".into());
    let date =
        if date.is_empty() { Utc::now().format("%Y-%m-%d").to_string() } else { date.to_string() };
    let fields = BTreeMap::from([
        ("start", format!("{date}T09:00")),
        ("end", format!("{date}T10:00")),
        ("timezone", zone),
    ]);
    render(&EventForm {
        flash,
        writes: backend.writes(),
        action: "/p/calendar/events".into(),
        heading: "New event".into(),
        choices,
        fields,
        all_day: false,
        away: String::new(),
        repeat: String::new(),
        reminder: String::new(),
    })
}

async fn event_json(backend: &Backend, id: &str) -> Result<Value, Refusal> {
    match backend.ask("calendar-events", "GET", &format!("events/{id}"), None, None).await {
        Ok((200, event)) => Ok(event),
        Ok((404, _)) => {
            Err(Refusal::missing("there is no such event, or you cannot see its calendar"))
        }
        Ok((status, refused)) => Err(Refusal { status, detail: detail(&refused) }),
        Err(err) => Err(Refusal::unavailable(format!("calendar-events could not be asked: {err}"))),
    }
}

async fn edit_event(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    flash: Flash,
) -> Result<String, Refusal> {
    let shown = event_json(backend, id).await?;
    let text = |key: &str| shown[key].as_str().unwrap_or_default().to_string();
    let all_day = shown["all_day"] == true;
    let fields = BTreeMap::from([
        ("title", text("title")),
        ("description", text("description")),
        ("location", text("location")),
        ("link", text("link")),
        ("start", text("start").chars().take(if all_day { 10 } else { 16 }).collect()),
        ("end", text("end").chars().take(if all_day { 10 } else { 16 }).collect()),
        ("timezone", text("timezone")),
        ("rrule", text("rrule")),
    ]);
    let calendar = shown["calendar"].as_str().and_then(|id| id.parse().ok());
    render(&EventForm {
        flash,
        writes: backend.writes(),
        action: format!("/p/calendar/events/{id}"),
        heading: format!("Edit {}", text("title")),
        choices: choices(backend, sight, calendar).await?,
        fields,
        all_day,
        away: shown["away"].as_str().unwrap_or_default().to_string(),
        repeat: "rule".into(),
        reminder: shown["reminder_minutes"]
            .as_i64()
            .map(|minutes| minutes.to_string())
            .unwrap_or_default(),
    })
}

/// What the event form asks `calendar-events` for.
fn asked_event(form: &Form) -> Result<Value, Refusal> {
    let all_day = field(form, "all_day").is_some();
    let rule = match field(form, "repeat").as_deref() {
        Some(freq @ ("DAILY" | "WEEKLY" | "MONTHLY" | "YEARLY")) => {
            let mut rule = format!("FREQ={freq}");
            if let Some(count) = field(form, "count").and_then(|count| count.parse::<u32>().ok()) {
                rule.push_str(&format!(";COUNT={count}"));
            }
            Some(rule)
        }
        Some("rule") => Some(field(form, "rrule").unwrap_or_default()),
        _ => Some(String::new()),
    };
    let mut asked = json!({
        "away": field(form, "away").unwrap_or_default(),
        "calendar": field(form, "calendar"),
        "title": field(form, "title").unwrap_or_default(),
        "description": field(form, "description").unwrap_or_default(),
        "location": field(form, "location").unwrap_or_default(),
        "link": field(form, "link").unwrap_or_default(),
        "all_day": all_day,
        "timezone": field(form, "timezone").unwrap_or_else(|| "UTC".into()),
        "start": field(form, "start").ok_or_else(|| Refusal::bad("say when it starts"))?,
        "rrule": rule,
    });
    if let Some(end) = field(form, "end") {
        asked["end"] = json!(end);
    }
    if let Some(minutes) = field(form, "reminder").and_then(|minutes| minutes.parse::<i64>().ok()) {
        asked["reminder_minutes"] = json!(minutes);
    }
    Ok(asked)
}

async fn event(backend: &Backend, id: &str, flash: Flash) -> Result<String, Refusal> {
    let shown = event_json(backend, id).await?;
    let text = |key: &str| shown[key].as_str().unwrap_or_default().to_string();
    let all_day = shown["all_day"] == true;
    let when = match all_day {
        true => format!("All day, {} to {}", text("start"), text("end")),
        false => {
            let wall = |key: &str| {
                chrono::NaiveDateTime::parse_from_str(&text(key), "%Y-%m-%dT%H:%M:%S").ok()
            };
            match (wall("start"), wall("end")) {
                (Some(start), Some(end)) if start.date() == end.date() => format!(
                    "{} to {} ({})",
                    start.format("%a %-d %b %Y, %H:%M"),
                    end.format("%H:%M"),
                    text("timezone")
                ),
                (Some(start), Some(end)) => format!(
                    "{} to {} ({})",
                    start.format("%a %-d %b %Y, %H:%M"),
                    end.format("%a %-d %b %Y, %H:%M"),
                    text("timezone")
                ),
                _ => format!("{} to {} ({})", text("start"), text("end"), text("timezone")),
            }
        }
    };
    let repeats = shown["rrule"].as_str().map(|rule| {
        let skipped = shown["exdates"].as_array().map_or(0, Vec::len);
        match skipped {
            0 => rule.to_string(),
            n => format!("{rule}, except {n} time{}", if n == 1 { "" } else { "s" }),
        }
    });
    let calendar = text("calendar");
    let now = Utc::now();
    let asked = format!(
        "calendars={calendar}&from={}&to={}",
        now.format("%Y-%m-%dT%H:%M:%SZ"),
        (now + Duration::days(180)).format("%Y-%m-%dT%H:%M:%SZ")
    );
    let upcoming =
        match backend.ask("calendar-events", "GET", "occurrences", Some(&asked), None).await {
            Ok((200, found)) => found["occurrences"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|occurrence| occurrence["event"].as_str() == Some(id))
                .take(6)
                .map(|occurrence| {
                    let named = occurrence["occurrence"].as_str().unwrap_or_default().to_string();
                    let start = occurrence["start"].as_str().unwrap_or_default();
                    let shown = DateTime::parse_from_rfc3339(start)
                        .map(|at| at.format("%a %-d %b %Y, %H:%M").to_string())
                        .unwrap_or_else(|_| start.to_string());
                    (named.clone(), shown)
                })
                .collect(),
            _ => Vec::new(),
        };
    let rsvps = shown["rsvps"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|rsvp| {
            let occurrence = match rsvp["occurrence"].as_str().filter(|at| !at.is_empty()) {
                Some(at) => chrono::NaiveDateTime::parse_from_str(at, "%Y-%m-%dT%H:%M:%S")
                    .map_or_else(
                        |_| at.to_string(),
                        |at| at.format("%a %-d %b %Y, %H:%M").to_string(),
                    ),
                None => "every time".to_string(),
            };
            (
                rsvp["who"].as_str().unwrap_or_default().to_string(),
                rsvp["response"].as_str().unwrap_or_default().to_string(),
                occurrence,
            )
        })
        .collect();
    let attendees: Vec<String> = shown["attendees"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|who| {
            who["user"]
                .as_str()
                .map(str::to_string)
                .or_else(|| who["team"].as_str().map(|team| format!("team {team}")))
        })
        .collect();
    let made_by = match shown["source_plugin"].as_str() {
        Some(plugin) => format!("the {plugin} plugin"),
        None => text("created_by"),
    };
    render(&EventPage {
        flash,
        id: id.to_string(),
        title: text("title"),
        calendar_name: text("calendar_name"),
        location: text("location"),
        link: text("link").chars().filter(|_| text("link").starts_with("http")).collect(),
        description: text("description"),
        resource: text("resource"),
        made_by,
        editable: backend.writes() && shown["source_plugin"].is_null(),
        calendar_href: format!("/p/calendar/c/{calendar}"),
        when,
        repeats: repeats.unwrap_or_default(),
        upcoming,
        rsvps,
        attendees: attendees.join(", "),
    })
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct UpcomingFragment {
    items: Vec<(String, Item)>,
}

/// The week ahead on a person's dashboard: what is on their own calendar, their teams' and the ones
/// they subscribed to, soonest first, in their own calendar's zone.
async fn upcoming(backend: &Backend) -> Result<String, Refusal> {
    let (principal, _) = api::principal(backend)?;
    let calendars = api::mine(backend, &principal).await?;
    let now = Utc::now();
    let week = (now + Duration::days(7)).to_rfc3339();
    let listed = occurrences(backend, &calendars, &now.to_rfc3339(), &week).await?;
    let zone = calendars.first().map_or(Tz::UTC, |calendar| zone_of(None, &calendar.timezone));
    let shades = shades(&calendars);
    let items = listed
        .iter()
        .take(6)
        .flat_map(|occurrence| {
            placed(occurrence, zone).into_iter().take(1).map(|at| {
                let label = at.day.format("%a %-d %b").to_string();
                (label, item(occurrence, at, &shades))
            })
        })
        .collect();
    render(&UpcomingFragment { items })
}

async fn panel(
    backend: &Backend,
    sight: &mut Sight<'_>,
    request: &Request,
) -> Result<String, Refusal> {
    let resource = api::normalised(
        &query(request, "resource").ok_or_else(|| Refusal::bad("name the resource"))?,
    )?;
    let (_, label) = api::principal(backend)?;
    let calendar = api::ensure(backend, sight, &resource, &label).await?;
    let now = Utc::now();
    let listed = occurrences(
        backend,
        std::slice::from_ref(&calendar),
        &now.to_rfc3339(),
        &(now + Duration::days(14)).to_rfc3339(),
    )
    .await?;
    let zone = zone_of(None, &calendar.timezone);
    let shades = shades(std::slice::from_ref(&calendar));
    let items = listed
        .iter()
        .take(8)
        .flat_map(|occurrence| {
            placed(occurrence, zone).into_iter().take(1).map(|at| {
                let label = at.day.format("%a %-d %b").to_string();
                (label, item(occurrence, at, &shades))
            })
        })
        .collect();
    render(&PanelFragment { writes: backend.writes(), calendar, items })
}
