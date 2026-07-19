//! The pages at `/p/water/events/...`: what is coming up, a form for a new event, and each event's
//! page, where teams register and submit for hackathons, people answer game nights and challenges
//! keep a leaderboard.

use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;

use askama::Template;
use chrono::{NaiveDateTime, Utc};
use doc_plugin_sdk::{Backend, Request};

use crate::Refusal;
use crate::api::Sight;
use crate::events::{self, NewEvent, NewSubmission, Winner};
use crate::store::{Event, Store};
use crate::ui::{Flash, Form, Tag, field, form, render, tags, tags_in};

pub struct Line {
    pub href: String,
    pub when: String,
    pub kind: &'static str,
    pub title: String,
    pub team: String,
    pub status: String,
}

pub struct Choice {
    pub value: String,
    pub label: String,
    pub selected: bool,
}

pub struct TeamRow {
    pub team: String,
    pub by: String,
    pub title: String,
    pub html: String,
    pub links: Vec<String>,
}

pub struct Place {
    pub place: u64,
    pub team: String,
    pub note: String,
}

pub struct Score {
    pub place: usize,
    pub entrant: String,
    pub best: String,
    pub entries: usize,
}

#[derive(Template)]
#[template(path = "events.html")]
struct EventsPage {
    flash: Flash,
    writes: bool,
    coming: Vec<Line>,
    past: Vec<Line>,
}

#[derive(Template)]
#[template(path = "event_form.html")]
struct EventForm {
    flash: Flash,
    writes: bool,
    kinds: Vec<Choice>,
    teams: Vec<Choice>,
    fields: BTreeMap<String, String>,
    lower_wins: bool,
    picked: Vec<Tag>,
}

#[derive(Template)]
#[template(path = "event.html")]
struct EventPage {
    flash: Flash,
    /// The section open: `about`, `teams`, `answers` or `leaderboard`, each a page of its own.
    section: &'static str,
    writes: bool,
    event: Event,
    kind: &'static str,
    when: String,
    open: bool,
    organises: bool,
    thread_href: String,
    calendar_href: String,
    registered: Vec<TeamRow>,
    winners: Vec<Place>,
    answers: Vec<(String, String)>,
    counts: (usize, usize, usize),
    board: Vec<Score>,
}

impl EventPage {
    fn url(&self) -> String {
        format!("/p/water/events/{}", self.event.id)
    }

    /// The section after About: a hackathon's teams, who is coming to a game night, or a
    /// challenge's leaderboard.
    fn second(&self) -> Option<(&'static str, &'static str)> {
        match self.event.kind.as_str() {
            "hackathon" => Some(("teams", "Teams")),
            "game-night" => Some(("answers", "Who is coming")),
            "challenge" => Some(("leaderboard", "Leaderboard")),
            _ => None,
        }
    }

    fn sections(&self) -> Vec<(&'static str, &'static str, String)> {
        let url = self.url();
        let mut sections = vec![("about", "About", url.clone())];
        if let Some((key, title)) = self.second() {
            sections.push((key, title, format!("{url}/{key}")));
        }
        sections
    }
}

/// Taking part in an event, or running it, on a page of its own: registering a team, submitting
/// for it, announcing the winners or entering a score. Its own template, apart from the event's,
/// so neither is one render too deep for a worker thread's stack.
#[derive(Template)]
#[template(path = "event_form_page.html")]
struct EventFormPage {
    flash: Flash,
    /// `register`, `submit`, `winners` or `enter`.
    form: &'static str,
    event: Event,
    teams: Vec<Choice>,
    registered: Vec<TeamRow>,
    /// What was typed into the form, when it was refused.
    typed: Form,
}

impl EventFormPage {
    fn url(&self) -> String {
        format!("/p/water/events/{}", self.event.id)
    }

    fn value(&self, name: &str) -> String {
        field(&self.typed, name).unwrap_or_default()
    }
}

/// The section or form a path names, or `None` for no such page.
fn section_named(name: &str) -> Option<&'static str> {
    ["about", "teams", "answers", "leaderboard", "register", "submit", "winners", "enter"]
        .into_iter()
        .find(|known| *known == name)
}

fn when(event: &Event) -> String {
    let wall = |text: &str| NaiveDateTime::parse_from_str(text, "%Y-%m-%dT%H:%M:%S").ok();
    match (wall(&event.starts_local), wall(&event.ends_local)) {
        (Some(start), Some(end)) if start.date() == end.date() => format!(
            "{} to {} ({})",
            start.format("%a %-d %b %Y, %H:%M"),
            end.format("%H:%M"),
            event.timezone
        ),
        (Some(start), Some(end)) => format!(
            "{} to {} ({})",
            start.format("%a %-d %b %Y, %H:%M"),
            end.format("%a %-d %b %Y, %H:%M"),
            event.timezone
        ),
        _ => format!("{} to {}", event.starts_local, event.ends_local),
    }
}

fn status(event: &Event) -> String {
    match event.status.as_str() {
        "announced" => "Winners announced".into(),
        "closed" => "Closed".into(),
        _ => "Open".into(),
    }
}

fn me(backend: &Backend) -> String {
    backend.caller().and_then(|caller| caller.label.clone()).unwrap_or_default()
}

/// The teams Resource Definitions lists for the viewer, to choose from.
pub async fn teams(backend: &Backend) -> Vec<Choice> {
    match backend.ask("resources", "GET", "resources", Some("kind=team&limit=200"), None).await {
        Ok((200, found)) => found
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|team| {
                let name = team["name"].as_str()?.to_string();
                let title = team["title"].as_str().filter(|title| !title.is_empty());
                let label = title.map_or_else(|| name.clone(), |title| format!("{title} ({name})"));
                Some(Choice { value: name, label, selected: false })
            })
            .collect(),
        _ => Vec::new(),
    }
}

pub async fn route(
    backend: &Backend,
    request: &Request,
    path: &[&str],
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    let mut sight = Sight::new(backend);
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => list(backend, Flash::default()).await,
        ("GET", ["new"]) => {
            let kind = crate::api::query(request, "kind").unwrap_or_else(|| "game-night".into());
            new_form(backend, vec![("kind".into(), kind)], Flash::default()).await
        }
        ("POST", []) | ("POST", [""]) => {
            let asked = form(request);
            let text = |name: &str| field(&asked, name).unwrap_or_default();
            let wanted = NewEvent {
                kind: text("kind"),
                title: text("title"),
                description: text("description"),
                team: text("team"),
                start: text("start"),
                end: text("end"),
                timezone: field(&asked, "timezone"),
                location: text("location"),
                higher_wins: Some(field(&asked, "lower_wins").is_none()),
                unit: text("unit"),
                tags: tags_in(&asked),
            };
            match events::create(backend, &mut sight, wanted).await {
                Ok(event) => {
                    *moved = Some(format!("/p/water/events/{}", event.id));
                    let notice =
                        "Made. Its discussion has started and it is on the team's calendar.";
                    page(backend, &event.id.to_string(), Flash::done(notice)).await
                }
                Err(refusal) if refusal.status >= 500 => Err(refusal),
                Err(refusal) => new_form(backend, asked, Flash::refused(refusal.detail)).await,
            }
        }
        ("GET", [id]) => page(backend, id, Flash::default()).await,
        ("GET", [id, named]) => match section_named(named) {
            Some(section) => shown(backend, id, section, Form::new(), Flash::default()).await,
            None => Err(Refusal::missing("no such page")),
        },
        ("POST", [id, "registrations"]) => {
            let asked = form(request);
            let team = field(&asked, "team").unwrap_or_default();
            let done = events::register(backend, &mut sight, id, &team).await;
            let done = done.map(|(_, new)| match new {
                true => format!("{team} is registered."),
                false => format!("{team} was registered already."),
            });
            after(backend, id, "teams", "register", asked, done, moved).await
        }
        ("POST", [id, "submissions"]) => {
            let asked = form(request);
            let text = |name: &str| field(&asked, name).unwrap_or_default();
            let wanted = NewSubmission {
                team: text("team"),
                title: text("title"),
                description: text("description"),
                links: text("links").lines().map(str::to_string).collect(),
            };
            let done = events::submit(backend, &mut sight, id, wanted).await;
            let done = done.map(|_| "Submitted.".to_string());
            after(backend, id, "teams", "submit", asked, done, moved).await
        }
        ("POST", [id, "winners"]) => {
            let asked = form(request);
            let winners: Vec<Winner> = (1..=3)
                .filter_map(|place| {
                    let team = field(&asked, &format!("place_{place}"))?;
                    let note = field(&asked, &format!("note_{place}")).unwrap_or_default();
                    Some(Winner { team, place, note })
                })
                .collect();
            let done = events::crown(backend, &mut sight, id, winners).await;
            let done =
                done.map(|_| "The winners are announced, here and in the discussion.".into());
            after(backend, id, "teams", "winners", asked, done, moved).await
        }
        ("POST", [id, "entries"]) => {
            let asked = form(request);
            let score = field(&asked, "score").and_then(|score| score.parse::<f64>().ok());
            let note = field(&asked, "note").unwrap_or_default();
            let done = match score {
                Some(score) => events::enter(backend, id, score, &note)
                    .await
                    .map(|_| "Your score is in.".into()),
                None => Err(Refusal::bad("a score is a number, such as 42 or 3.5")),
            };
            after(backend, id, "leaderboard", "enter", asked, done, moved).await
        }
        ("POST", [id, "rsvp"]) => {
            let response = field(&form(request), "response").unwrap_or_default();
            let flash = match events::rsvp(backend, id, &response).await {
                Ok(_) => Flash::done("Your answer is saved."),
                Err(refusal) => Flash::refused(refusal.detail),
            };
            shown(backend, id, "answers", Form::new(), flash).await
        }
        ("POST", [id, "close"]) => {
            let flash = match events::close(backend, id).await {
                Ok(_) => Flash::done("Closed."),
                Err(refusal) => Flash::refused(refusal.detail),
            };
            page(backend, id, flash).await
        }
        ("POST", [id, "delete"]) => match events::cancel(backend, id).await {
            Ok(event) => {
                *moved = Some("/p/water/events".into());
                let notice = format!("Cancelled {}; its discussion stays.", event.title);
                list(backend, Flash::done(notice)).await
            }
            Err(refusal) => page(backend, id, Flash::refused(refusal.detail)).await,
        },
        _ => Err(Refusal::missing("no such page")),
    }
}

async fn list(backend: &Backend, flash: Flash) -> Result<String, Refusal> {
    let now = Utc::now();
    let (mut coming, mut past) = (Vec::new(), Vec::new());
    for event in Store(backend).events().await? {
        let line = Line {
            href: format!("/p/water/events/{}", event.id),
            when: when(&event),
            kind: events::kind_name(&event.kind),
            title: event.title.clone(),
            team: event.team.clone(),
            status: status(&event),
        };
        match events::instant(&event, &event.ends_local).is_some_and(|end| end < now) {
            true => past.push(line),
            false => coming.push(line),
        }
    }
    past.reverse();
    render(&EventsPage { flash, writes: backend.writes(), coming, past })
}

async fn new_form(backend: &Backend, chosen: Form, flash: Flash) -> Result<String, Refusal> {
    let picked = tags(&tags_in(&chosen));
    let mut fields: BTreeMap<String, String> = chosen.into_iter().collect();
    fields.entry("timezone".into()).or_insert_with(|| "UTC".into());
    let pick = |name: &str| fields.get(name).cloned().unwrap_or_default();
    let (kind, team) = (pick("kind"), pick("team"));
    let kinds = events::KINDS
        .iter()
        .filter(|(value, _)| *value != "hackathon" || events::may_organise_hackathons(backend))
        .map(|(value, shown)| Choice {
            value: (*value).to_string(),
            label: (*shown).to_string(),
            selected: *value == kind,
        })
        .collect();
    let mut teams = teams(backend).await;
    for choice in &mut teams {
        choice.selected = choice.value == team;
    }
    let lower_wins = fields.contains_key("lower_wins");
    render(&EventForm { flash, writes: backend.writes(), kinds, teams, fields, lower_wins, picked })
}

/// The section a change was made in, saying so, with the address bar there; or, when it was
/// refused, its form again with what was typed and why.
async fn after(
    backend: &Backend,
    id: &str,
    section: &'static str,
    form: &'static str,
    typed: Form,
    done: Result<String, Refusal>,
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    match done {
        Ok(notice) => {
            *moved = Some(format!("/p/water/events/{id}/{section}"));
            shown(backend, id, section, Form::new(), Flash::done(notice)).await
        }
        Err(refusal) if refusal.status >= 500 => Err(refusal),
        Err(refusal) => shown(backend, id, form, typed, Flash::refused(refusal.detail)).await,
    }
}

async fn page(backend: &Backend, id: &str, flash: Flash) -> Result<String, Refusal> {
    shown(backend, id, "about", Form::new(), flash).await
}

/// An event's section or form, boxed: every route draws one, and a debug build that held the
/// future in place in each of them would outgrow a worker thread's stack.
fn shown<'a>(
    backend: &'a Backend,
    id: &'a str,
    section: &'static str,
    typed: Form,
    flash: Flash,
) -> Pin<Box<dyn Future<Output = Result<String, Refusal>> + Send + 'a>> {
    Box::pin(drawn(backend, id, section, typed, flash))
}

async fn drawn(
    backend: &Backend,
    id: &str,
    section: &'static str,
    typed: Form,
    flash: Flash,
) -> Result<String, Refusal> {
    let store = Store(backend);
    let event = events::found(&store, id).await?;
    let admin = backend.caller().is_some_and(|caller| caller.admin);
    let organises = backend.writes() && (admin || event.organiser == me(backend));
    let mut registered = Vec::new();
    let submissions = store.submissions(event.id).await?;
    for registration in store.registrations(event.id).await? {
        let submission = submissions.iter().find(|submission| submission.team == registration.team);
        registered.push(TeamRow {
            by: registration.registered_by.clone(),
            title: submission.map(|submission| submission.title.clone()).unwrap_or_default(),
            html: submission.map(|submission| submission.html.clone()).unwrap_or_default(),
            links: submission.map(|submission| submission.links.clone()).unwrap_or_default(),
            team: registration.team,
        });
    }
    let winners = event
        .winners
        .iter()
        .map(|winner| Place {
            place: winner["place"].as_u64().unwrap_or_default(),
            team: winner["team"].as_str().unwrap_or_default().to_string(),
            note: winner["note"].as_str().unwrap_or_default().to_string(),
        })
        .collect();
    let answers = match event.kind.as_str() {
        "game-night" => events::answers(backend, &event).await,
        _ => Vec::new(),
    };
    let count = |response: &str| answers.iter().filter(|(_, given)| given == response).count();
    let counts = (count("yes"), count("maybe"), count("no"));
    let entries = store.entries(event.id).await?;
    let board = events::leaderboard(&event, &entries)
        .into_iter()
        .enumerate()
        .map(|(index, (entrant, best, entries))| {
            let unit =
                if event.unit.is_empty() { String::new() } else { format!(" {}", event.unit) };
            Score { place: index + 1, entrant, best: format!("{best}{unit}"), entries }
        })
        .collect();
    let teams = match event.kind.as_str() {
        "hackathon" => teams(backend).await,
        _ => Vec::new(),
    };
    if matches!(section, "register" | "submit" | "winners" | "enter") {
        return render(&EventFormPage { flash, form: section, event, teams, registered, typed });
    }
    render(&EventPage {
        flash,
        section,
        writes: backend.writes(),
        kind: events::kind_name(&event.kind),
        when: when(&event),
        open: event.status == "open",
        organises,
        thread_href: event
            .thread
            .map_or_else(String::new, |thread| format!("/p/water/threads/{thread}")),
        calendar_href: event.calendar_event.map_or_else(String::new, |calendar_event| {
            format!("/p/calendar/events/{calendar_event}")
        }),
        registered,
        winners,
        counts,
        answers,
        board,
        event,
    })
}
