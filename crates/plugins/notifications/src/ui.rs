//! The pages at `/p/notifications/...`: the bell (an HTMX fragment embedded in every page's
//! header) and the full history, with mark as read, archive and delete.

use askama::Template;
use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Request, Response};
use uuid::Uuid;

use crate::Refusal;
use crate::store::{BELL, Notification, PAGE, Store};

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

fn when(text: &str) -> String {
    DateTime::parse_from_rfc3339(text).map_or_else(
        |_| text.to_string(),
        |at| at.with_timezone(&Utc).format("%-d %b, %H:%M").to_string(),
    )
}

/// The bell's short form: the time for one made today, the date for anything older.
fn short_when(text: &str) -> String {
    let Ok(at) = DateTime::parse_from_rfc3339(text) else {
        return text.to_string();
    };
    let at = at.with_timezone(&Utc);
    match at.date_naive() == Utc::now().date_naive() {
        true => at.format("%H:%M").to_string(),
        false => at.format("%-d %b").to_string(),
    }
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn excerpt(text: &str, max: usize) -> String {
    let text = text.trim();
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut short: String = text.chars().take(max).collect();
    short.push('…');
    short
}

fn query(request: &Request, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn writes(backend: &Backend) -> bool {
    backend.caller().is_some_and(|caller| caller.writes())
}

/// Tells every open bell to refresh itself.
pub async fn announce(backend: &Backend) {
    if let Err(err) =
        backend.publish("plugin.notifications.ui.changed", serde_json::json!({})).await
    {
        tracing::warn!(%err, "a change was stored but not announced");
    }
}

/// What made a notification, as its badge says it: `software-templates` is "Software templates".
/// Empty for one made before notifications knew who sent them.
fn source(id: &str) -> String {
    let mut words = id.trim().replace(['-', '_', '.'], " ");
    if let Some(first) = words.get(..1) {
        words.replace_range(..1, &first.to_uppercase());
    }
    words
}

#[derive(Template)]
#[template(path = "bell_row.html")]
struct BellRow {
    id: String,
    title: String,
    text: String,
    when: String,
    short: String,
    source: String,
    writes: bool,
}

/// Rendered eagerly, not left as structs the template renders nested: askama HTML-escapes a
/// plain `{{ expr }}`, so a row's own markup has to already be a trusted `String` by the time the
/// page that holds it is rendered, injected with `|safe`.
fn bell_row(n: &Notification, writes: bool) -> Result<String, Refusal> {
    render(&BellRow {
        id: n.id.to_string(),
        title: n.title.clone(),
        text: excerpt(&n.body, 200),
        when: when(&n.created_at),
        short: short_when(&n.created_at),
        source: source(&n.source),
        writes,
    })
}

#[derive(Template)]
#[template(path = "badge.html")]
struct Badge {
    unread: usize,
}

/// The small unread count beside the bell icon, which core renders unconditionally (so the icon
/// itself never flashes on navigation) and this fills in a moment later.
pub async fn badge(backend: &Backend) -> Result<String, Refusal> {
    render(&Badge { unread: Store(backend).unread().await? })
}

#[derive(Template)]
#[template(path = "panel.html")]
struct Panel {
    rows: Vec<String>,
}

/// The dropdown's contents, fetched into it while it is closed and invisible, so this one never
/// causes anything the user can see to flash either.
pub async fn panel(backend: &Backend) -> Result<String, Refusal> {
    let recent = Store(backend).recent(BELL).await?;
    let writes = writes(backend);
    let rows = recent.iter().map(|n| bell_row(n, writes)).collect::<Result<_, _>>()?;
    render(&Panel { rows })
}

/// One notification as a dashboard lists it.
struct Glance {
    href: String,
    title: String,
    from: String,
    when: String,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct InboxItem {
    waiting: usize,
    rows: Vec<Glance>,
}

/// The inbox on a person's dashboard: how many are waiting, and the newest of them, each opening
/// where it is read.
async fn inbox_item(backend: &Backend) -> Result<String, Refusal> {
    let store = Store(backend);
    let waiting = store.unread().await?;
    let rows = store
        .recent(5)
        .await?
        .iter()
        .map(|n| Glance {
            href: format!("/p/notifications/?show={}", n.id),
            title: n.title.clone(),
            from: source(&n.source),
            when: when(&n.created_at),
        })
        .collect();
    render(&InboxItem { waiting, rows })
}

#[derive(Template)]
#[template(path = "list_row.html")]
struct ListRow {
    id: String,
    title: String,
    text: String,
    url: String,
    when: String,
    source: String,
    unread: bool,
    archived: bool,
    writes: bool,
    shown: bool,
}

fn list_row(n: &Notification, writes: bool) -> Result<String, Refusal> {
    shown_row(n, writes, false)
}

/// A row, `shown` when it is the one the bell was followed to.
fn shown_row(n: &Notification, writes: bool, shown: bool) -> Result<String, Refusal> {
    render(&ListRow {
        id: n.id.to_string(),
        source: source(&n.source),
        title: n.title.clone(),
        text: n.body.clone(),
        url: n.url.clone(),
        when: when(&n.created_at),
        unread: n.read_at.is_none(),
        archived: n.archived_at.is_some(),
        writes,
        shown,
    })
}

fn list_rows(notifications: &[Notification], writes: bool) -> Result<Vec<String>, Refusal> {
    notifications.iter().map(|n| list_row(n, writes)).collect()
}

/// How many pages of the inbox are read looking for the one the bell was followed to.
const SEARCHED: usize = 10;

#[derive(Template)]
#[template(path = "list.html")]
struct List {
    error: Option<String>,
    archived: bool,
    rows: Vec<String>,
    more: Option<String>,
}

/// The inbox, or its archive. `show`, which the bell links with, is one notification to open it at:
/// pages are read until it is among them, so it is in its place in the list rather than lifted out
/// of it, and following it there is reading it.
async fn list_page(
    backend: &Backend,
    archived: bool,
    show: Option<String>,
) -> Result<String, Refusal> {
    let store = Store(backend);
    let (mut notifications, mut next) = store.list(archived, PAGE, None).await?;
    let show = show.and_then(|id| Uuid::parse_str(&id).ok());
    if let Some(show) = show {
        let mut pages = 1;
        while pages < SEARCHED && !notifications.iter().any(|n| n.id == show) {
            let Some(after) = next.take() else { break };
            let (more, after) = store.list(archived, PAGE, Some(after)).await?;
            notifications.extend(more);
            next = after;
            pages += 1;
        }
        if let Some(found) = notifications.iter_mut().find(|n| n.id == show && n.read_at.is_none())
        {
            *found = store.mark_read(&show.to_string()).await?;
            announce(backend).await;
        }
    }
    let writes = writes(backend);
    let rows = notifications
        .iter()
        .map(|n| shown_row(n, writes, Some(n.id) == show))
        .collect::<Result<_, _>>()?;
    render(&List { error: None, archived, rows, more: next })
}

#[derive(Template)]
#[template(path = "rows.html")]
struct Rows {
    rows: Vec<String>,
    more: Option<String>,
    archived: bool,
}

async fn rows_page(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let archived = query(request, "archived").as_deref() == Some("true");
    let after = query(request, "after");
    let store = Store(backend);
    let (notifications, next) = store.list(archived, PAGE, after).await?;
    let writes = writes(backend);
    render(&Rows { rows: list_rows(&notifications, writes)?, more: next, archived })
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let fragment = !matches!(path, [] | [""] | ["archived"]);
    match route(backend, request, path).await {
        Ok(html) => Response::html(html),
        Err(refusal) if fragment => {
            let text = format!(
                "<p class=\"doc-error-message\" role=\"alert\">{}</p>",
                escape(&refusal.detail)
            );
            Response::new(refusal.status, "text/html; charset=utf-8", text)
        }
        Err(refusal) => refusal.response(),
    }
}

async fn route(backend: &Backend, request: &Request, path: &[&str]) -> Result<String, Refusal> {
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => list_page(backend, false, query(request, "show")).await,
        ("GET", ["archived"]) => list_page(backend, true, None).await,
        ("GET", ["badge"]) => badge(backend).await,
        ("GET", ["panel"]) => panel(backend).await,
        ("GET", ["dashboard", "inbox"]) => inbox_item(backend).await,
        ("GET", ["rows"]) => rows_page(backend, request).await,
        ("POST", ["items", id, "read"]) => {
            let n = Store(backend).mark_read(id).await?;
            announce(backend).await;
            // From the bell, the bell is drawn again without it (it lists only what is unread),
            // and the count beside the icon with it, rather than waiting for the change to be
            // announced back.
            match query(request, "from").as_deref() {
                Some("bell") => Ok(format!(
                    "{}<span id=\"doc-notifications-badge\" hx-swap-oob=\"innerHTML\">{}</span>",
                    panel(backend).await?,
                    badge(backend).await?
                )),
                _ => list_row(&n, writes(backend)),
            }
        }
        ("POST", ["items", id, "archive"]) => {
            Store(backend).archive(id).await?;
            announce(backend).await;
            Ok(String::new())
        }
        ("POST", ["items", id, "delete"]) => {
            Store(backend).remove(id).await?;
            announce(backend).await;
            Ok(String::new())
        }
        _ => Err(Refusal::missing("no such route")),
    }
}
