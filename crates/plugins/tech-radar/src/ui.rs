//! The pages at `/p/radar/...`: the radar and its legend, every entry in a table, each entry's page
//! with its reasoning and timeline, and, for writers, forms to add, edit, move, remove and import.

use askama::Template;
use chrono::{DateTime, Duration, Utc};
use doc_plugin_sdk::{Backend, Request, Response};

use crate::api::{self, query};
use crate::backstage::Import;
use crate::draw::{self, Blip};
use crate::model::{self, Moved, QUADRANTS, RINGS};
use crate::store::{Edit, Entry, Move, NewEntry, Store};
use crate::{Refusal, markdown};

/// A move or an addition is marked on the radar for this long, then the blip is drawn plain.
const FRESH_FOR: Duration = Duration::days(90);

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    pub fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    pub fn refused(detail: impl Into<String>) -> Self {
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

fn day(text: &str) -> String {
    DateTime::parse_from_rfc3339(text)
        .map_or_else(|_| text.to_string(), |at| at.format("%-d %b %Y").to_string())
}

/// Entries numbered as the radar and its legend show them: by quadrant, then ring, then title.
pub fn numbered(mut entries: Vec<Entry>) -> Vec<(usize, Entry)> {
    let place = |entry: &Entry| {
        (
            model::quadrant_index(&entry.quadrant).unwrap_or(usize::MAX),
            model::ring_index(&entry.ring).unwrap_or(usize::MAX),
            entry.title.to_lowercase(),
        )
    };
    entries.sort_by_cached_key(place);
    entries.into_iter().enumerate().map(|(index, entry)| (index + 1, entry)).collect()
}

/// How the entry last moved, while that is still news.
fn fresh(entry: &Entry, now: DateTime<Utc>) -> Moved {
    match DateTime::parse_from_rfc3339(&entry.moved_at) {
        Ok(at) if now - at.with_timezone(&Utc) > FRESH_FOR => Moved::None,
        _ => entry.moved,
    }
}

fn href(entry: &Entry) -> String {
    format!("/p/radar/entries/{}", entry.key)
}

/// The radar, with every entry but `focus` faded when there is one.
fn radar(entries: &[(usize, Entry)], focus: Option<&Entry>, description: &str) -> String {
    let now = Utc::now();
    let blips: Vec<Blip> = entries
        .iter()
        .filter_map(|(number, entry)| {
            Some(Blip {
                number: *number,
                title: entry.title.clone(),
                quadrant: model::quadrant_index(&entry.quadrant)?,
                ring: model::ring_index(&entry.ring)?,
                moved: fresh(entry, now),
                href: href(entry),
                faded: focus.is_some_and(|focus| focus.id != entry.id),
            })
        })
        .collect();
    draw::draw(&blips, description)
}

pub struct Keyed {
    pub number: usize,
    pub title: String,
    pub href: String,
    pub moved: &'static str,
}

pub struct RingKey {
    pub name: &'static str,
    pub start: usize,
    pub entries: Vec<Keyed>,
}

pub struct QuadrantKey {
    pub colour: usize,
    pub name: &'static str,
    pub rings: Vec<RingKey>,
}

#[derive(Template)]
#[template(path = "board.html")]
struct Board {
    svg: String,
    count: usize,
    writes: bool,
    quadrants: Vec<QuadrantKey>,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    flash: Flash,
    writes: bool,
    board: String,
    rings: Vec<(&'static str, &'static str)>,
}

pub struct Row {
    pub number: usize,
    pub title: String,
    pub href: String,
    pub quadrant: &'static str,
    pub ring: &'static str,
    pub moved: &'static str,
    pub changed: String,
}

pub struct Choice {
    pub value: &'static str,
    pub label: &'static str,
    pub selected: bool,
}

#[derive(Template)]
#[template(path = "entries.html")]
struct EntriesPage {
    flash: Flash,
    writes: bool,
    q: String,
    quadrants: Vec<Choice>,
    rings: Vec<Choice>,
    rows: Vec<Row>,
    filtered: bool,
}

#[derive(Template)]
#[template(path = "entry_form.html")]
struct EntryForm {
    flash: Flash,
    writes: bool,
    /// The entry's key when editing one; a new entry also chooses its ring and key.
    editing: Option<String>,
    title: String,
    key: String,
    description: String,
    url: String,
    note: String,
    quadrants: Vec<Choice>,
    rings: Vec<Choice>,
}

pub struct Step {
    pub title: String,
    pub meta: String,
    pub note: String,
}

#[derive(Template)]
#[template(path = "entry.html")]
struct EntryPage {
    flash: Flash,
    writes: bool,
    number: usize,
    key: String,
    title: String,
    quadrant: &'static str,
    ring: &'static str,
    meaning: &'static str,
    url: String,
    description: String,
    moved: &'static str,
    moved_at: String,
    svg: String,
    steps: Vec<Step>,
    rings: Vec<Choice>,
}

#[derive(Template)]
#[template(path = "import.html")]
struct ImportPage {
    flash: Flash,
    writes: bool,
    file: String,
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
    writes: bool,
}

fn quadrant_choices(chosen: &str) -> Vec<Choice> {
    QUADRANTS
        .iter()
        .map(|quadrant| Choice {
            value: quadrant.id,
            label: quadrant.name,
            selected: quadrant.id == chosen,
        })
        .collect()
}

fn ring_choices(chosen: &str) -> Vec<Choice> {
    RINGS
        .iter()
        .map(|ring| Choice { value: ring.id, label: ring.name, selected: ring.id == chosen })
        .collect()
}

fn board_of(backend: &Backend, entries: &[(usize, Entry)]) -> Result<String, Refusal> {
    let now = Utc::now();
    let quadrants = QUADRANTS
        .iter()
        .enumerate()
        .map(|(index, quadrant)| QuadrantKey {
            colour: index + 1,
            name: quadrant.name,
            rings: RINGS
                .iter()
                .filter_map(|ring| {
                    let keyed: Vec<Keyed> = entries
                        .iter()
                        .filter(|(_, entry)| entry.quadrant == quadrant.id && entry.ring == ring.id)
                        .map(|(number, entry)| Keyed {
                            number: *number,
                            title: entry.title.clone(),
                            href: href(entry),
                            moved: fresh(entry, now).words(),
                        })
                        .collect();
                    let start = keyed.first()?.number;
                    Some(RingKey { name: ring.name, start, entries: keyed })
                })
                .collect(),
        })
        .collect();
    render(&Board {
        svg: radar(entries, None, "The tech radar"),
        count: entries.len(),
        writes: backend.writes(),
        quadrants,
    })
}

async fn home(backend: &Backend, flash: Flash) -> Result<String, Refusal> {
    let entries = numbered(Store(backend).entries().await?);
    render(&HomePage {
        flash,
        writes: backend.writes(),
        board: board_of(backend, &entries)?,
        rings: RINGS.iter().map(|ring| (ring.name, ring.meaning)).collect(),
    })
}

async fn list(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let store = Store(backend);
    let q = query(request, "q").unwrap_or_default();
    let quadrant = query(request, "quadrant").unwrap_or_default();
    let ring = query(request, "ring").unwrap_or_default();
    let found = match q.is_empty() {
        true => None,
        false => Some(store.search(&q).await?),
    };
    let now = Utc::now();
    let rows = numbered(store.entries().await?)
        .into_iter()
        .filter(|(_, entry)| quadrant.is_empty() || entry.quadrant == quadrant)
        .filter(|(_, entry)| ring.is_empty() || entry.ring == ring)
        .filter(|(_, entry)| {
            found.as_ref().is_none_or(|found| found.iter().any(|f| f.id == entry.id))
        })
        .map(|(number, entry)| Row {
            number,
            href: href(&entry),
            quadrant: QUADRANTS[model::quadrant_index(&entry.quadrant).unwrap_or(0)].name,
            ring: RINGS[model::ring_index(&entry.ring).unwrap_or(0)].name,
            moved: fresh(&entry, now).words(),
            changed: day(&entry.moved_at),
            title: entry.title,
        })
        .collect();
    render(&EntriesPage {
        flash: Flash::default(),
        writes: backend.writes(),
        filtered: !(q.is_empty() && quadrant.is_empty() && ring.is_empty()),
        q,
        quadrants: quadrant_choices(&quadrant),
        rings: ring_choices(&ring),
        rows,
    })
}

fn new_form(backend: &Backend, asked: &Form, flash: Flash) -> Result<String, Refusal> {
    let text = |name: &str| field(asked, name).unwrap_or_default();
    let ring = field(asked, "ring").unwrap_or_else(|| "assess".into());
    render(&EntryForm {
        flash,
        writes: backend.writes(),
        editing: None,
        title: text("title"),
        key: text("key"),
        description: text("description"),
        url: text("url"),
        note: text("note"),
        quadrants: quadrant_choices(&text("quadrant")),
        rings: ring_choices(&ring),
    })
}

async fn edit_form(
    backend: &Backend,
    id: &str,
    asked: Option<&Form>,
    flash: Flash,
) -> Result<String, Refusal> {
    let entry = Store(backend).found(id).await?;
    let text = |name: &str, stored: &str| match asked {
        Some(asked) => field(asked, name).unwrap_or_default(),
        None => stored.to_string(),
    };
    render(&EntryForm {
        flash,
        writes: backend.writes(),
        title: text("title", &entry.title),
        description: text("description", &entry.description),
        url: text("url", &entry.url),
        note: String::new(),
        quadrants: quadrant_choices(&text("quadrant", &entry.quadrant)),
        rings: Vec::new(),
        key: entry.key.clone(),
        editing: Some(entry.key),
    })
}

async fn entry_page(backend: &Backend, id: &str, flash: Flash) -> Result<String, Refusal> {
    let store = Store(backend);
    let entry = store.found(id).await?;
    let entries = numbered(store.entries().await?);
    let number =
        entries.iter().find(|(_, listed)| listed.id == entry.id).map_or(0, |(number, _)| *number);
    let mut timeline = store.movements(entry.id).await?;
    timeline.reverse();
    let steps = timeline
        .iter()
        .map(|line| {
            let ring = model::ring_name(&line.ring);
            let title = match line.moved {
                Moved::New => format!("Added in {ring}"),
                Moved::In | Moved::Out => format!("Moved to {ring}"),
                Moved::None => format!("Kept in {ring}"),
            };
            Step {
                title,
                meta: format!("{} · {}", line.author, day(&line.at)),
                note: markdown::render(&line.note),
            }
        })
        .collect();
    let ring = model::ring_index(&entry.ring).unwrap_or(0);
    render(&EntryPage {
        flash,
        writes: backend.writes(),
        number,
        svg: radar(&entries, Some(&entry), &format!("{} on the tech radar", entry.title)),
        quadrant: QUADRANTS[model::quadrant_index(&entry.quadrant).unwrap_or(0)].name,
        ring: RINGS[ring].name,
        meaning: RINGS[ring].meaning,
        description: markdown::render(&entry.description),
        moved: fresh(&entry, Utc::now()).words(),
        moved_at: day(&entry.moved_at),
        steps,
        rings: ring_choices(&entry.ring),
        key: entry.key,
        title: entry.title,
        url: entry.url,
    })
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let fragment = matches!(path, ["board"]);
    let mut moved = None;
    match route(backend, request, path, &mut moved).await {
        Ok(html) => match moved {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) if fragment => {
            let text = format!(
                "<p class=\"doc-error-message\" role=\"alert\">{}</p>",
                draw::escape(&refusal.detail)
            );
            Response::new(refusal.status, "text/html; charset=utf-8", text)
        }
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(refusal.detail.clone()), writes: false };
            let html = page.render().unwrap_or_else(|_| draw::escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

/// `moved` is set to the address a page should be shown at when it is not the one asked for.
async fn route(
    backend: &Backend,
    request: &Request,
    path: &[&str],
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => home(backend, Flash::default()).await,
        ("GET", ["board"]) => {
            let entries = numbered(Store(backend).entries().await?);
            board_of(backend, &entries)
        }
        ("GET", ["entries"]) => list(backend, request).await,
        ("GET", ["entries", "new"]) => {
            let asked: Form = ["quadrant", "ring"]
                .into_iter()
                .filter_map(|name| Some((name.to_string(), query(request, name)?)))
                .collect();
            new_form(backend, &asked, Flash::default())
        }
        ("POST", ["entries"]) => {
            let asked = form(request);
            let text = |name: &str| field(&asked, name).unwrap_or_default();
            let wanted = NewEntry {
                title: text("title"),
                quadrant: text("quadrant"),
                ring: text("ring"),
                key: field(&asked, "key"),
                description: text("description"),
                url: text("url"),
                note: text("note"),
            };
            match api::add(backend, wanted).await {
                Ok(entry) => {
                    *moved = Some(href(&entry));
                    let notice = format!("{} is on the radar.", entry.title);
                    entry_page(backend, &entry.key, Flash::done(notice)).await
                }
                Err(refusal) if refusal.status >= 500 => Err(refusal),
                Err(refusal) => new_form(backend, &asked, Flash::refused(refusal.detail)),
            }
        }
        ("GET", ["entries", id]) => entry_page(backend, id, Flash::default()).await,
        ("GET", ["entries", id, "edit"]) => edit_form(backend, id, None, Flash::default()).await,
        ("POST", ["entries", id]) => {
            let asked = form(request);
            let wanted = Edit {
                title: Some(field(&asked, "title").unwrap_or_default()),
                quadrant: Some(field(&asked, "quadrant").unwrap_or_default()),
                description: Some(field(&asked, "description").unwrap_or_default()),
                url: Some(field(&asked, "url").unwrap_or_default()),
            };
            match api::edit(backend, id, wanted).await {
                Ok(entry) => {
                    *moved = Some(href(&entry));
                    entry_page(backend, &entry.key, Flash::done("Saved.")).await
                }
                Err(refusal) if refusal.status >= 500 || refusal.status == 404 => Err(refusal),
                Err(refusal) => {
                    edit_form(backend, id, Some(&asked), Flash::refused(refusal.detail)).await
                }
            }
        }
        ("POST", ["entries", id, "move"]) => {
            let asked = form(request);
            let wanted = Move {
                ring: field(&asked, "ring").unwrap_or_default(),
                note: field(&asked, "note").unwrap_or_default(),
            };
            let flash = match api::move_to(backend, id, wanted).await {
                Ok((entry, Moved::None)) => {
                    Flash::done(format!("Noted: it stays in {}.", model::ring_name(&entry.ring)))
                }
                Ok((entry, _)) => Flash::done(format!(
                    "{} is in {} now.",
                    entry.title,
                    model::ring_name(&entry.ring)
                )),
                Err(refusal) if refusal.status >= 500 || refusal.status == 404 => {
                    return Err(refusal);
                }
                Err(refusal) => Flash::refused(refusal.detail),
            };
            entry_page(backend, id, flash).await
        }
        ("POST", ["entries", id, "delete"]) => match api::remove(backend, id).await {
            Ok(entry) => {
                *moved = Some("/p/radar/".into());
                home(backend, Flash::done(format!("{} is off the radar.", entry.title))).await
            }
            Err(refusal) if refusal.status >= 500 || refusal.status == 404 => Err(refusal),
            Err(refusal) => entry_page(backend, id, Flash::refused(refusal.detail)).await,
        },
        ("GET", ["import"]) => render(&ImportPage {
            flash: Flash::default(),
            writes: backend.writes(),
            file: String::new(),
        }),
        ("POST", ["import"]) => {
            let file = field(&form(request), "file").unwrap_or_default();
            let parsed = serde_json::from_str::<Import>(&file).map_err(|err| {
                Refusal::bad(format!("that is not a radar in Backstage's JSON: {err}"))
            });
            let flash = match parsed {
                Ok(parsed) => match api::import(backend, &parsed).await {
                    Ok(outcome) => {
                        *moved = Some("/p/radar/".into());
                        let notice = format!(
                            "Imported: {} added, {} moved, {} updated and {} unchanged.",
                            outcome.added, outcome.moved, outcome.updated, outcome.unchanged
                        );
                        return home(backend, Flash::done(notice)).await;
                    }
                    Err(refusal) if refusal.status >= 500 => return Err(refusal),
                    Err(refusal) => Flash::refused(refusal.detail),
                },
                Err(refusal) => Flash::refused(refusal.detail),
            };
            render(&ImportPage { flash, writes: backend.writes(), file })
        }
        _ => Err(Refusal::missing("no such page")),
    }
}
