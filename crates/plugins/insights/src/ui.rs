//! The pages at `/p/insights/…`: every repository and how its latest scan stands, a page for each
//! repository with a tab for each part of what ccc found, and the Insights panel on a repository's
//! page in the Catalogue. Security findings and dependencies need the `security` permission.

use askama::Template as Page;
use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Query, Request, Response};
use serde_json::{Value, json};

use crate::scanner::{Scanner, Tools};
use crate::settings::{ADVISORIES, SCANS, Scanning};
use crate::store::{self, Named, Repository, Scan, Wanted};
use crate::{ID, Refusal, SECURITY, parameter};

const PRIMARY: &str = "#5b00b8";
const FAILED: &str = "#d5281b";
const SECONDARY: &str = "#768692";
/// The most scans a trend is drawn from, and the most rows one table shows.
const HISTORY: u32 = 30;
const SHOWN: usize = 200;
/// How long a source is given to say how its delivery data stands.
const ASKING: std::time::Duration = std::time::Duration::from_secs(5);

pub struct Tile {
    pub label: String,
    pub value: String,
    pub note: String,
}

pub struct Chart {
    pub title: String,
    pub described: String,
    pub config: String,
}

#[derive(Default)]
pub struct Cell {
    pub text: String,
    pub href: Option<String>,
    pub badge: Option<&'static str>,
    pub mono: bool,
    pub numeric: bool,
    pub hint: Option<String>,
}

impl Cell {
    fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), ..Self::default() }
    }

    fn mono(text: impl Into<String>) -> Self {
        Self { text: text.into(), mono: true, ..Self::default() }
    }

    fn number(value: u64) -> Self {
        Self { text: thousands(value), numeric: true, ..Self::default() }
    }

    fn badge(text: &str, modifier: &'static str) -> Self {
        Self { text: text.to_string(), badge: Some(modifier), ..Self::default() }
    }

    fn linked(mut self, href: Option<String>) -> Self {
        self.href = href;
        self
    }

    fn hinted(mut self, hint: impl Into<String>) -> Self {
        let hint = hint.into();
        self.hint = (!hint.is_empty()).then_some(hint);
        self
    }
}

pub struct Header {
    pub label: &'static str,
    pub numeric: bool,
}

pub struct Table {
    pub caption: Option<String>,
    pub headers: Vec<Header>,
    pub rows: Vec<Vec<Cell>>,
}

impl Table {
    /// Headers ending in `#` are for numbers, and are right-aligned without it.
    fn new(headers: &[&'static str]) -> Self {
        let headers = headers
            .iter()
            .map(|label| match label.strip_suffix('#') {
                Some(label) => Header { label, numeric: true },
                None => Header { label, numeric: false },
            })
            .collect();
        Self { caption: None, headers, rows: Vec::new() }
    }

    fn captioned(mut self, total: usize) -> Self {
        if total > self.rows.len() {
            self.caption =
                Some(format!("The first {} of {}", self.rows.len(), thousands(total as u64)));
        }
        self
    }
}

pub enum Section {
    Heading(String),
    Text(String),
    Hint(String),
    Empty(String),
    Problem(String),
    Tiles(Vec<Tile>),
    Charts(Vec<Chart>),
    Summary(Vec<(String, Cell)>),
    Table(Table),
}

/// How a repository stands, in a line that refreshes itself when a scan starts or ends.
pub struct Status {
    pub id: String,
    pub query: String,
    pub modifier: &'static str,
    pub word: &'static str,
    pub said: String,
    pub newer: Option<String>,
    pub problem: Option<String>,
}

pub struct Tab {
    pub label: &'static str,
    pub href: String,
    pub current: bool,
}

pub struct Frame {
    pub repository: String,
    pub id: String,
    pub tabs: Vec<Tab>,
    pub writes: bool,
}

#[derive(Page)]
#[template(path = "repository.html")]
struct RepositoryPage {
    frame: Frame,
    status: Status,
    sections: Vec<Section>,
}

#[derive(Page)]
#[template(path = "status.html")]
struct StatusPage {
    status: Status,
}

pub struct Row {
    pub href: String,
    pub repository: String,
    pub source: String,
    pub modifier: &'static str,
    pub word: &'static str,
    pub cells: Vec<Cell>,
    pub problem: Option<String>,
}

pub struct Listing {
    pub headers: Vec<Header>,
    pub rows: Vec<Row>,
}

#[derive(Page)]
#[template(path = "repositories.html")]
struct ListingPage {
    listing: Listing,
}

pub struct ScanForm {
    pub sources: Vec<String>,
    pub source: String,
    pub repository: String,
    pub problem: Option<String>,
    pub said: Option<String>,
}

#[derive(Page)]
#[template(path = "form.html")]
struct FormPage {
    form: ScanForm,
}

pub struct Source {
    pub id: String,
    pub said: String,
    pub ready: bool,
}

pub struct Onboarding {
    pub on: bool,
    pub admin: bool,
    pub problem: Option<String>,
    pub sources: Vec<Source>,
}

#[derive(Page)]
#[template(path = "home.html")]
struct Home {
    onboarding: Onboarding,
    writes: bool,
    listing: Listing,
    branch: String,
}

/// Asking for a scan, on a page of its own.
#[derive(Page)]
#[template(path = "scan_new.html")]
struct NewScan {
    form: ScanForm,
}

#[derive(Page)]
#[template(path = "panel.html")]
struct Panel {
    message: Option<String>,
    tiles: Vec<Tile>,
    more: String,
}

fn drawn<T: Page>(page: &T) -> Response {
    match page.render() {
        Ok(html) => Response::html(html),
        Err(err) => {
            Response::problem(500, "internal", &format!("the page could not be drawn: {err}"))
        }
    }
}

pub async fn handle(backend: &Backend, scanner: &Scanner, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/');
    let segments: Vec<&str> = path.split('/').collect();
    let query = request.query.as_str();
    let answered = match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["ui"]) => home(backend, scanner).await,
        ("GET", ["ui", "repositories"]) => {
            listing(backend).await.map(|listing| drawn(&ListingPage { listing }))
        }
        ("GET", ["ui", "scans", "new"]) => new_scan(backend).await,
        ("POST", ["ui", "scans"]) => asked(backend, scanner, request).await,
        ("GET", ["ui", "panel"]) => panel(backend, query).await,
        ("GET", ["ui", "insight", which]) => insight(backend, which, query).await,
        ("GET", ["ui", "r", source, owner, name]) => {
            repository_page(backend, &id_of(source, owner, name), query).await
        }
        ("GET", ["ui", "r", source, owner, name, "status"]) => {
            let held = held(backend, &id_of(source, owner, name)).await;
            held.map(|held| drawn(&StatusPage { status: status(&held, parameter(query, "scan")) }))
        }
        ("POST", ["ui", "r", source, owner, name, "scan"]) => {
            again(backend, scanner, &id_of(source, owner, name)).await
        }
        _ => return Response::not_found(),
    };
    answered.unwrap_or_else(|refusal| refusal.response())
}

fn id_of(source: &str, owner: &str, name: &str) -> String {
    format!("{source}/{owner}/{name}").to_ascii_lowercase()
}

async fn held(backend: &Backend, id: &str) -> Result<Repository, Refusal> {
    store::repository(backend, id)
        .await?
        .ok_or_else(|| Refusal::missing(format!("{id} has not been scanned")))
}

fn when(at: DateTime<Utc>) -> String {
    at.format("%-d %b %Y, %H:%M").to_string()
}

fn thousands(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::new();
    for (at, digit) in digits.chars().enumerate() {
        if at > 0 && (digits.len() - at).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn number(value: &Value) -> u64 {
    value.as_u64().unwrap_or_default()
}

fn list(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}

fn names(value: &Value) -> String {
    list(value).iter().map(text).collect::<Vec<_>>().join(", ")
}

/// ccc's change set calls a repository with no `.ccc/map.json` one service, `.`.
fn service(name: &str) -> String {
    match name {
        "." => "The whole repository".into(),
        name => name.to_string(),
    }
}

fn services_named(value: &Value) -> String {
    list(value).iter().map(|name| service(&text(name))).collect::<Vec<_>>().join(", ")
}

fn short(commit: &str) -> &str {
    commit.get(..10).unwrap_or(commit)
}

/// A link to somewhere outside DOC, only ever to the web.
fn web(url: Option<&str>) -> Option<String> {
    url.filter(|url| url.starts_with("https://") || url.starts_with("http://")).map(str::to_string)
}

/// Where a scan's files are on the source's web pages, when it says.
struct Links {
    web: Option<url::Url>,
    commit: String,
}

impl Links {
    fn new(web: Option<&str>, commit: &str) -> Self {
        let web = web.and_then(|web| url::Url::parse(web).ok());
        let known = commit.len() >= 7 && commit.chars().all(|c| c.is_ascii_hexdigit());
        Self { web: web.filter(|_| known), commit: commit.to_string() }
    }

    fn file(&self, file: &str, line: Option<u64>) -> Option<String> {
        let mut url = self.web.clone()?;
        url.path_segments_mut().ok()?.push("blob").push(&self.commit).extend(file.split('/'));
        if let Some(line) = line.filter(|line| *line > 0) {
            url.set_fragment(Some(&format!("L{line}")));
        }
        Some(url.to_string())
    }

    fn commit(&self, commit: &str) -> Option<String> {
        let mut url = self.web.clone()?;
        url.path_segments_mut().ok()?.push("commit").push(commit);
        Some(url.to_string())
    }

    /// `file:line`, linked to the line.
    fn place(&self, row: &Value) -> Cell {
        let file = text(&row["file"]);
        let line = row["line"].as_u64();
        let said = match line {
            Some(line) if line > 0 => format!("{file}:{line}"),
            _ => file.clone(),
        };
        Cell::mono(said).linked(self.file(&file, line))
    }
}

fn state_badge(state: &str) -> (&'static str, &'static str) {
    match state {
        store::QUEUED => ("loading", "Queued"),
        store::SCANNING => ("loading", "Scanning"),
        store::SCANNED => ("ready", "Scanned"),
        store::FAILED => ("error", "Failed"),
        _ => ("unknown", "Unknown"),
    }
}

fn status(held: &Repository, shown: Option<String>) -> Status {
    let (modifier, word) = state_badge(&held.state);
    let why = held.why.as_deref().map(|why| format!(" {why}.")).unwrap_or_default();
    let said = match held.state.as_str() {
        store::QUEUED => format!("Waiting to be scanned.{why}"),
        store::SCANNING => format!(
            "Being scanned since {}.{why}",
            held.started_at.map(when).unwrap_or_else(|| "a moment ago".into())
        ),
        _ => match (held.finished_at, &held.commit) {
            (Some(at), Some(commit)) => {
                format!("Last scanned on {at} at {}.", short(commit), at = when(at))
            }
            (Some(at), None) => format!("Last tried on {}.", when(at)),
            _ => "Not scanned yet.".into(),
        },
    };
    let newer = match (&shown, &held.latest) {
        (Some(shown), Some(latest)) if shown != latest => Some(held.href()),
        _ => None,
    };
    let problem = held.problem.clone().filter(|_| held.state == store::FAILED);
    let query = shown.map(|scan| format!("?scan={scan}")).unwrap_or_default();
    Status { id: held.id.clone(), query, modifier, word, said, newer, problem }
}

/// Whether sources announcing merges are reading them, for a page with nothing to show yet.
async fn sources(backend: &Backend, scanning: &Scanning) -> Vec<Source> {
    let running: Vec<String> = backend
        .query_all::<Value>(Query::new("core.plugins").fields(&["id", "state"]))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|plugin| matches!(plugin["state"].as_str(), Some("running" | "cancelled")))
        .filter_map(|plugin| plugin["id"].as_str().map(str::to_string))
        .collect();
    let mut found = Vec::new();
    for id in scanning.sources.iter().filter(|id| running.contains(id)) {
        let asked = backend.discovery(id, "GET", "delivery", None, None);
        let Ok(Ok((200, stands))) = tokio::time::timeout(ASKING, asked).await else {
            let said = "It did not say whether it reads merged pull requests.".to_string();
            found.push(Source { id: id.clone(), said, ready: false });
            continue;
        };
        let (said, ready) = match (stands["on"].as_bool(), stands["problem"].as_str()) {
            (Some(false), _) => (
                "Its Delivery data feature is off, so no merge is heard from it.".to_string(),
                false,
            ),
            (_, Some(problem)) => {
                (format!("It cannot read merged pull requests: {problem}."), false)
            }
            _ => ("Merged pull requests are heard from it.".to_string(), true),
        };
        found.push(Source { id: id.clone(), said, ready });
    }
    found
}

async fn home(backend: &Backend, scanner: &Scanner) -> Result<Response, Refusal> {
    let scanning = Scanning::read(&backend.settings());
    let tools: Tools = scanner.tools().await;
    let onboarding = Onboarding {
        on: backend.feature(SCANS),
        admin: backend.caller().is_some_and(|caller| caller.admin),
        problem: tools.version.err(),
        sources: sources(backend, &scanning).await,
    };
    Ok(drawn(&Home {
        onboarding,
        writes: backend.writes(),
        listing: listing(backend).await?,
        branch: scanning.branch,
    }))
}

async fn new_scan(backend: &Backend) -> Result<Response, Refusal> {
    if !backend.writes() {
        return Err(Refusal::forbidden("asking for a scan needs write access to Insights"));
    }
    let scanning = Scanning::read(&backend.settings());
    let form = ScanForm {
        source: scanning.first_source().unwrap_or("github").to_string(),
        sources: scanning.sources.clone(),
        repository: String::new(),
        problem: None,
        said: None,
    };
    Ok(drawn(&NewScan { form }))
}

async fn listing(backend: &Backend) -> Result<Listing, Refusal> {
    let security = backend.allows(SECURITY, false);
    let mut labels =
        vec!["Commit", "Scanned", "Lines#", "Functions#", "Lint warnings#", "Untested#"];
    if security {
        labels.push("Security, high#");
    }
    let headers = Table::new(&labels).headers;
    let rows = store::repositories(backend)
        .await?
        .into_iter()
        .map(|held| {
            let (modifier, word) = state_badge(&held.state);
            let summary = held.summary.clone().unwrap_or(Value::Null);
            let links = Links::new(held.web.as_deref(), held.commit.as_deref().unwrap_or_default());
            let scanned = held.summary.is_some();
            let figure = |key: &str| match scanned {
                true => Cell::number(number(&summary[key])),
                false => Cell { numeric: true, ..Cell::text("–") },
            };
            let mut cells = vec![
                match &held.commit {
                    Some(commit) => Cell::mono(short(commit)).linked(links.commit(commit)),
                    None => Cell::text("–"),
                },
                Cell::text(
                    held.finished_at.filter(|_| scanned).map(when).unwrap_or_else(|| "–".into()),
                ),
                figure("lines"),
                figure("functions"),
                figure("lint_warnings"),
                figure("untested"),
            ];
            if security {
                cells.push(match scanned && summary["security"]["available"] == true {
                    true => Cell::number(number(&summary["security"]["high"])),
                    false => Cell { numeric: true, ..Cell::text("–") },
                });
            }
            Row {
                href: held.href(),
                repository: held.repository.clone(),
                source: held.source.clone(),
                modifier,
                word,
                cells,
                problem: held.problem.clone().filter(|_| held.state == store::FAILED),
            }
        })
        .collect();
    Ok(Listing { headers, rows })
}

/// Somebody asked for a repository by name, from the home page's form.
async fn asked(
    backend: &Backend,
    scanner: &Scanner,
    request: &Request,
) -> Result<Response, Refusal> {
    let body = String::from_utf8_lossy(&request.body).to_string();
    let scanning = Scanning::read(&backend.settings());
    let repository = parameter(&body, "repository").unwrap_or_default();
    let source = parameter(&body, "source")
        .or_else(|| scanning.first_source().map(str::to_string))
        .unwrap_or_else(|| "github".into());
    let mut form = ScanForm {
        sources: scanning.sources.clone(),
        source: source.clone(),
        repository: repository.clone(),
        problem: None,
        said: None,
    };
    match queue(backend, scanner, &source, &repository).await {
        Ok((held, said)) => {
            form.repository = String::new();
            form.said = Some(format!("{}: {said}", held.repository));
        }
        Err(refusal) if refusal.status < 500 => form.problem = Some(refusal.detail),
        Err(refusal) => return Err(refusal),
    }
    Ok(drawn(&FormPage { form }))
}

/// Queues `repository` from `source` as the caller, who has written to the plugin to get here.
pub async fn queue(
    backend: &Backend,
    scanner: &Scanner,
    source: &str,
    repository: &str,
) -> Result<(Repository, &'static str), Refusal> {
    if !backend.feature(SCANS) {
        return Err(Refusal::bad(
            "Repository insights are off: turn them on from the Plugins page",
        ));
    }
    let scanning = Scanning::read(&backend.settings());
    if !scanning.sources.iter().any(|known| known.eq_ignore_ascii_case(source.trim())) {
        return Err(Refusal::bad(format!(
            "{source} is not one of the sources in this plugin's settings ({})",
            scanning.sources.join(", ")
        )));
    }
    let named = Named::new(source, repository).map_err(Refusal::bad)?;
    if !scanning.wants(&named.repository) {
        return Err(Refusal::bad(format!(
            "{} is not one of the repositories this plugin's settings scan",
            named.repository
        )));
    }
    let who = crate::who(backend);
    let wanted = Wanted { why: format!("Asked for by {who}"), trigger: None, at: None };
    let asked = store::want(backend, &named, &scanning.branch, wanted).await?;
    scanner.wake();
    let _ =
        backend.publish(&format!("plugin.{ID}.ui.repositories"), json!({ "id": named.id() })).await;
    let held = held(backend, &named.id()).await?;
    Ok((held, asked.said()))
}

/// "Scan again", from a repository's page: answers with its status line.
async fn again(backend: &Backend, scanner: &Scanner, id: &str) -> Result<Response, Refusal> {
    let held = held(backend, id).await?;
    let (held, _) = queue(backend, scanner, &held.source, &held.repository).await?;
    Ok(drawn(&StatusPage { status: status(&held, held.latest.clone()) }))
}

/// What may be pinned about a repository, by the name each is pinned under and the figure in the
/// scan it reads.
pub const INSIGHTS: [(&str, &str, &str); 4] = [
    ("untested", "Untested functions", "untested"),
    ("lint-warnings", "Lint warnings", "lint_warnings"),
    ("lines", "Lines of code", "lines"),
    ("functions", "Functions", "functions"),
];

#[derive(Page)]
#[template(path = "insight.html")]
struct InsightTile {
    label: String,
    value: String,
    note: String,
    more: String,
}

/// One figure from the last scan, for pinning to the top of a repository's page.
async fn insight(backend: &Backend, which: &str, query: &str) -> Result<Response, Refusal> {
    let Some((_, label, key)) = INSIGHTS.iter().find(|(id, _, _)| *id == which) else {
        return Ok(Response::not_found());
    };
    let mut tile = InsightTile {
        label: (*label).to_string(),
        value: "—".into(),
        note: String::new(),
        more: "/p/insights/".into(),
    };
    let resource = parameter(query, "resource").unwrap_or_default();
    let name =
        resource.split_once(':').map_or(resource.as_str(), |(_, name)| name).to_ascii_lowercase();
    let held = store::repositories(backend)
        .await?
        .into_iter()
        .find(|held| held.repository.eq_ignore_ascii_case(&name) && held.summary.is_some());
    let Some(held) = held else {
        tile.note = "Not scanned yet".into();
        return Ok(drawn(&tile));
    };
    let summary = held.summary.clone().unwrap_or_default();
    tile.value = thousands(number(&summary[*key]));
    tile.more = held.href();
    tile.note = match which {
        "untested" => {
            format!("Of the {} ranked as worth a test", thousands(number(&summary["testable"])))
        }
        _ => held.finished_at.map(|at| format!("Scanned {}", when(at))).unwrap_or_default(),
    };
    Ok(drawn(&tile))
}

async fn panel(backend: &Backend, query: &str) -> Result<Response, Refusal> {
    let resource = parameter(query, "resource").unwrap_or_default();
    let name =
        resource.split_once(':').map_or(resource.as_str(), |(_, name)| name).to_ascii_lowercase();
    let held = store::repositories(backend)
        .await?
        .into_iter()
        .find(|held| held.repository.eq_ignore_ascii_case(&name) && held.summary.is_some());
    let Some(held) = held else {
        return Ok(drawn(&Panel {
            message: Some(
                "Not scanned yet: it is scanned the next time a pull request is merged into it."
                    .into(),
            ),
            tiles: Vec::new(),
            more: "/p/insights/".into(),
        }));
    };
    let summary = held.summary.clone().unwrap_or_default();
    let tile = |label: &str, key: &str| Tile {
        label: label.into(),
        value: thousands(number(&summary[key])),
        note: String::new(),
    };
    let mut tiles = vec![
        tile("Lines of code", "lines"),
        tile("Functions", "functions"),
        tile("Lint warnings", "lint_warnings"),
    ];
    tiles.push(Tile {
        label: "Untested functions".into(),
        value: thousands(number(&summary["untested"])),
        note: format!("of {} ranked", thousands(number(&summary["testable"]))),
    });
    let note = held.finished_at.map(|at| format!("Scanned {}", when(at))).unwrap_or_default();
    tiles[0].note = note;
    Ok(drawn(&Panel { message: None, tiles, more: held.href() }))
}

const TABS: [(&str, &str); 7] = [
    ("overview", "Overview"),
    ("changes", "Changes"),
    ("hotspots", "Hotspots"),
    ("services", "Services"),
    ("lints", "Lints"),
    ("tests", "Tests"),
    ("security", "Security"),
];

async fn repository_page(backend: &Backend, id: &str, query: &str) -> Result<Response, Refusal> {
    let held = held(backend, id).await?;
    let security = backend.allows(SECURITY, false);
    let tab = parameter(query, "tab")
        .and_then(|asked| TABS.iter().find(|(tab, _)| *tab == asked).map(|(tab, _)| *tab))
        .unwrap_or("overview");
    if tab == "security" && !security {
        return Err(Refusal::forbidden(format!("that needs plugin:{ID}:pluginuser:{SECURITY}:ro")));
    }
    let asked = parameter(query, "scan");
    let shown_id = asked.clone().or_else(|| held.latest.clone());
    let scan = match &shown_id {
        Some(scan) => {
            store::scan(backend, scan).await?.filter(|scan| scan.repository_id == held.id)
        }
        None => None,
    };
    if asked.is_some() && scan.is_none() {
        return Err(Refusal::missing("there is no such scan of this repository"));
    }
    let chosen = asked.as_ref().map(|scan| format!("&scan={scan}")).unwrap_or_default();
    let tabs = TABS
        .iter()
        .filter(|(each, _)| *each != "security" || security)
        .map(|(each, label)| Tab {
            label,
            href: match *each {
                "overview" => format!("{}{}", held.href(), chosen.replacen('&', "?", 1)),
                each => format!("{}?tab={each}{chosen}", held.href()),
            },
            current: *each == tab,
        })
        .collect();
    let frame = Frame {
        repository: held.repository.clone(),
        id: held.id.clone(),
        tabs,
        writes: backend.writes(),
    };
    let status = status(&held, scan.as_ref().map(|scan| scan.id.to_string()));
    let Some(scan) = scan else {
        let said = match held.state.as_str() {
            store::FAILED => "The scan failed, so there is nothing to show yet.",
            _ => "Nothing to show until its first scan is done.",
        };
        let sections = vec![Section::Empty(said.into())];
        return Ok(drawn(&RepositoryPage { frame, status, sections }));
    };
    let links = Links::new(held.web.as_deref(), &scan.commit);
    let report = &scan.report;
    let sections = match tab {
        "changes" => changes(report, &scan, &links),
        "hotspots" => hotspots(report, &links),
        "services" => services(report),
        "lints" => lints(report, &links),
        "tests" => tests(report, &links),
        "security" => security_sections(backend, report, &links),
        _ => {
            let history = store::history(backend, &held.id, HISTORY).await?;
            overview(&held, &scan, &history, &links, security)
        }
    };
    Ok(drawn(&RepositoryPage { frame, status, sections }))
}

/// `+12 since the scan before`, or what else fits.
fn delta(now: u64, before: Option<u64>) -> String {
    match before {
        None => "The first scan".into(),
        Some(before) if before == now => "Same as the scan before".into(),
        Some(before) if now > before => {
            format!("{} more than the scan before", thousands(now - before))
        }
        Some(before) => format!("{} fewer than the scan before", thousands(before - now)),
    }
}

fn overview(
    held: &Repository,
    scan: &Scan,
    history: &[Scan],
    links: &Links,
    security: bool,
) -> Vec<Section> {
    let summary = &scan.summary;
    let before =
        history.iter().find(|each| each.finished_at < scan.finished_at).map(|each| &each.summary);
    let figure = |key: &str| number(&summary[key]);
    let was = |key: &str| before.map(|before| number(&before[key]));
    let mut tiles = vec![
        Tile {
            label: "Lines of code".into(),
            value: thousands(figure("lines")),
            note: delta(figure("lines"), was("lines")),
        },
        Tile {
            label: "Functions".into(),
            value: thousands(figure("functions")),
            note: delta(figure("functions"), was("functions")),
        },
        Tile {
            label: "Lint warnings".into(),
            value: thousands(figure("lint_warnings")),
            note: delta(figure("lint_warnings"), was("lint_warnings")),
        },
        Tile {
            label: "Untested functions".into(),
            value: thousands(figure("untested")),
            note: format!("Of the {} ccc ranks as worth a test", thousands(figure("testable"))),
        },
    ];
    let security_summary = &summary["security"];
    tiles.push(match security && security_summary["available"] == true {
        true => Tile {
            label: "Security findings, high".into(),
            value: thousands(number(&security_summary["high"])),
            note: format!(
                "{} medium, {} low",
                thousands(number(&security_summary["medium"])),
                thousands(number(&security_summary["low"]))
            ),
        },
        false => Tile {
            label: "Call cycles".into(),
            value: thousands(figure("cycles")),
            note: "Functions that end up calling themselves".into(),
        },
    });
    let mut sections = vec![Section::Tiles(tiles)];

    let mut oldest_first: Vec<&Scan> = history.iter().collect();
    oldest_first.reverse();
    if oldest_first.len() >= 2 {
        let labels: Vec<String> = oldest_first
            .iter()
            .map(|each| each.finished_at.format("%-d %b %H:%M").to_string())
            .collect();
        let series = |key: &str| -> Vec<u64> {
            oldest_first.iter().map(|each| number(&each.summary[key])).collect()
        };
        let line = |label: &str, data: Vec<u64>, colour: &str, dashed: bool| {
            json!({
                "label": label, "data": data, "borderColor": colour, "backgroundColor": colour,
                "tension": 0, "pointRadius": 2,
                "borderDash": if dashed { json!([4, 4]) } else { json!([]) },
            })
        };
        let options = |unit: &str| {
            json!({
                "animation": false,
                "interaction": { "mode": "index", "intersect": false },
                "scales": { "y": { "beginAtZero": true, "ticks": { "precision": 0 }, "title": { "display": true, "text": unit } } },
                "plugins": { "legend": { "position": "bottom" } },
            })
        };
        sections.push(Section::Charts(vec![
            Chart {
                title: "Size".into(),
                described: "Lines of code and functions at each scan".into(),
                config: json!({
                    "type": "line",
                    "data": { "labels": labels, "datasets": [
                        line("Lines of code", series("lines"), PRIMARY, false),
                    ]},
                    "options": options("lines"),
                })
                .to_string(),
            },
            Chart {
                title: "What needs attention".into(),
                described: "Lint warnings and untested functions at each scan".into(),
                config: json!({
                    "type": "line",
                    "data": { "labels": labels, "datasets": [
                        line("Lint warnings", series("lint_warnings"), FAILED, false),
                        line("Untested functions", series("untested"), SECONDARY, true),
                    ]},
                    "options": options("functions"),
                })
                .to_string(),
            },
        ]));
    }

    let report = &scan.report;
    let mut languages =
        Table::new(&["Language", "Files#", "Functions#", "Lines#", "Average complexity#"]);
    for language in list(&report["languages"]) {
        languages.rows.push(vec![
            Cell::text(text(&language["language"])),
            Cell::number(number(&language["files"])),
            Cell::number(number(&language["funcs"])),
            Cell::number(number(&language["lines"])),
            Cell {
                numeric: true,
                ..Cell::text(format!(
                    "{:.1}",
                    language["avg_complexity"].as_f64().unwrap_or_default()
                ))
            },
        ]);
    }
    if !languages.rows.is_empty() {
        sections.push(Section::Heading("Languages".into()));
        sections.push(Section::Table(languages));
    }

    let trigger = scan.trigger.clone().unwrap_or(Value::Null);
    let why = Cell::text(scan.why.clone().unwrap_or_else(|| "–".into()))
        .linked(web(trigger["url"].as_str()))
        .hinted(text(&trigger["title"]));
    let compared = match &scan.base_commit {
        Some(base) => Cell::mono(short(base)).linked(links.commit(base)),
        None => {
            Cell::text("Nothing: this is the first scan, or the one before could not be fetched")
        }
    };
    let took =
        scan.took_ms.map(|ms| format!("{:.1}s", ms as f64 / 1000.0)).unwrap_or_else(|| "–".into());
    sections.push(Section::Heading("This scan".into()));
    sections.push(Section::Summary(vec![
        ("Commit".into(), Cell::mono(short(&scan.commit)).linked(links.commit(&scan.commit))),
        ("Compared with".into(), compared),
        ("Branch".into(), Cell::mono(&scan.branch)),
        ("Scanned".into(), Cell::text(when(scan.finished_at))),
        ("Took".into(), Cell::text(took)),
        ("Why".into(), why),
        ("Scanned with".into(), Cell::text(scan.ccc.clone().unwrap_or_else(|| "ccc".into()))),
    ]));

    if history.len() > 1 {
        let mut scans = Table::new(&[
            "Scanned",
            "Commit",
            "Why",
            "Lines#",
            "Functions#",
            "Lint warnings#",
            "Untested#",
        ]);
        for each in history {
            let current = each.id == scan.id;
            let at = Cell::text(when(each.finished_at))
                .linked((!current).then(|| format!("{}?scan={}", held.href(), each.id)))
                .hinted(if current { "shown" } else { "" });
            scans.rows.push(vec![
                at,
                Cell::mono(short(&each.commit)).linked(links.commit(&each.commit)),
                Cell::text(each.why.clone().unwrap_or_default()),
                Cell::number(number(&each.summary["lines"])),
                Cell::number(number(&each.summary["functions"])),
                Cell::number(number(&each.summary["lint_warnings"])),
                Cell::number(number(&each.summary["untested"])),
            ]);
        }
        sections.push(Section::Heading("Scans".into()));
        sections.push(Section::Table(scans));
    }
    sections
}

fn changes(report: &Value, scan: &Scan, links: &Links) -> Vec<Section> {
    let changes = &report["changes"];
    if changes["available"] != true {
        let reason = text(&changes["reason"]);
        return match &scan.base_commit {
            None => vec![
                Section::Empty(
                    "There is no scan before this one to compare it with. From the next scan on, \
                     this tab shows what changed since the one before: files, functions, whether \
                     tests reach them and which services they affect."
                        .into(),
                ),
                Section::Hint(format!("ccc said: {reason}")),
            ],
            Some(_) => {
                vec![Section::Empty(format!("ccc could not work out what changed: {reason}"))]
            }
        };
    }
    let counts = &changes["counts"];
    let base = text(&changes["base_sha"]);
    let mut sections = vec![
        Section::Text(format!(
            "What changed between {} and {}: everything merged into {} between the two scans.",
            short(scan.base_commit.as_deref().unwrap_or(&base)),
            short(&scan.commit),
            scan.branch
        )),
        Section::Tiles(vec![
            Tile {
                label: "Files changed".into(),
                value: thousands(number(&counts["changed_files"])),
                note: String::new(),
            },
            Tile {
                label: "Functions changed".into(),
                value: thousands(number(&counts["changed_functions"])),
                note: String::new(),
            },
            Tile {
                label: "Changed and untested".into(),
                value: thousands(number(&counts["untested"])),
                note: "No test reaches them".into(),
            },
            Tile {
                label: "Services to test".into(),
                value: thousands(list(&changes["services_to_test"]).len() as u64),
                note: services_named(&changes["services_to_test"]),
            },
        ]),
    ];
    let functions = list(&changes["changed_functions"]);
    if !functions.is_empty() {
        let mut table = Table::new(&["Function", "Where", "Tested", "Tested by", "Called from"]);
        for function in functions.iter().take(SHOWN) {
            let lines = list(&function["lines"]);
            let line = lines.first().and_then(Value::as_u64);
            let file = text(&function["file"]);
            let place = match (line, lines.get(1).and_then(Value::as_u64)) {
                (Some(from), Some(to)) => format!("{file}:{from}-{to}"),
                _ => file.clone(),
            };
            table.rows.push(vec![
                Cell::mono(text(&function["function"])),
                Cell::mono(place).linked(links.file(&file, line)),
                match function["tested"] == true {
                    true => Cell::badge("Yes", "ready"),
                    false => Cell::badge("No", "degraded"),
                },
                Cell::text(names(&function["tested_by"])),
                Cell::text(names(&function["called_from"])),
            ]);
        }
        sections.push(Section::Heading("Changed functions".into()));
        sections
            .push(Section::Table(table.captioned(number(&counts["changed_functions"]) as usize)));
    }
    let files = list(&changes["changed_files"]);
    if !files.is_empty() {
        let mut table = Table::new(&["Change", "File", "Services"]);
        for file in files.iter().take(SHOWN) {
            let path = text(&file["path"]);
            let status = text(&file["status"]);
            let modifier = match status.as_str() {
                "added" => "ready",
                "deleted" => "error",
                _ => "unknown",
            };
            let href = (status != "deleted").then(|| links.file(&path, None)).flatten();
            table.rows.push(vec![
                Cell::badge(&status, modifier),
                Cell::mono(path).linked(href),
                Cell::text(services_named(&file["services"])),
            ]);
        }
        sections.push(Section::Heading("Changed files".into()));
        sections.push(Section::Table(table.captioned(number(&counts["changed_files"]) as usize)));
    }
    let impact = list(&changes["impact"]);
    if !impact.is_empty() {
        let mut table = Table::new(&["Service", "Why", "Through"]);
        for each in impact {
            let path: Vec<String> = list(&each["path"]).iter().map(|s| service(&text(s))).collect();
            table.rows.push(vec![
                Cell::text(service(&text(&each["service"]))),
                Cell::text(text(&each["reason"])),
                Cell::mono(path.join(" → ")),
            ]);
        }
        sections.push(Section::Heading("Services affected".into()));
        sections.push(Section::Table(table));
    }
    sections
}

fn hotspots(report: &Value, links: &Links) -> Vec<Section> {
    let hot = &report["hot"];
    let mut sections = vec![Section::Hint(
        "From the shape of the call graph, not from running anything: a function called from many \
         places or with many decision points is where a change reaches furthest."
            .into(),
    )];
    let complex = list(&report["complexity"]["functions"]);
    if !complex.is_empty() {
        let mut table = Table::new(&[
            "Function",
            "Where",
            "Complexity#",
            "Lines#",
            "Parameters#",
            "Loop depth#",
        ]);
        for row in complex.iter().take(25) {
            table.rows.push(vec![
                Cell::mono(text(&row["function"])),
                links.place(row),
                Cell::number(number(&row["complexity"])),
                Cell::number(number(&row["lines"])),
                Cell::number(number(&row["params"])),
                Cell::number(number(&row["loop_depth"])),
            ]);
        }
        sections.push(Section::Heading("Most complex".into()));
        sections.push(Section::Table(table));
    }
    let called = list(&hot["most_called"]);
    if !called.is_empty() {
        let mut table = Table::new(&["Function", "Where", "Callers#", "Call sites#"]);
        for row in called {
            table.rows.push(vec![
                Cell::mono(text(&row["name"])),
                links.place(row),
                Cell::number(number(&row["callers"])),
                Cell::number(number(&row["call_sites"])),
            ]);
        }
        sections.push(Section::Heading("Most called".into()));
        sections.push(Section::Table(table));
    }
    let widest = list(&hot["widest"]);
    if !widest.is_empty() {
        let mut table = Table::new(&["Function", "Where", "Calls#"]);
        for row in widest {
            table.rows.push(vec![
                Cell::mono(text(&row["name"])),
                links.place(row),
                Cell::number(number(&row["calls"])),
            ]);
        }
        sections.push(Section::Heading("Calls the most".into()));
        sections.push(Section::Table(table));
    }
    let chains = list(&hot["deepest_chains"]);
    if !chains.is_empty() {
        let mut table = Table::new(&["Depth#", "Chain", "Starts at"]);
        for chain in chains {
            let steps: Vec<String> =
                list(&chain["chain"]).iter().map(|step| text(&step["name"])).collect();
            let start = list(&chain["chain"]).first().cloned().unwrap_or(Value::Null);
            table.rows.push(vec![
                Cell::number(number(&chain["depth"])),
                Cell::mono(steps.join(" → ")),
                links.place(&start),
            ]);
        }
        sections.push(Section::Heading("Deepest call chains".into()));
        sections.push(Section::Table(table));
    }
    let cycles = list(&hot["cycles"]);
    if !cycles.is_empty() {
        let mut table = Table::new(&["Functions#", "Which"]);
        for cycle in cycles {
            let members: Vec<String> =
                list(&cycle["members"]).iter().map(|m| text(&m["name"])).collect();
            table
                .rows
                .push(vec![Cell::number(number(&cycle["size"])), Cell::mono(members.join(", "))]);
        }
        sections.push(Section::Heading("Call cycles".into()));
        sections.push(Section::Hint(
            "Functions that call each other round in a loop: mutual recursion.".into(),
        ));
        sections.push(Section::Table(table.captioned(number(&hot["cycles_total"]) as usize)));
    }
    sections
}

fn services(report: &Value) -> Vec<Section> {
    let services = &report["services"];
    let source = text(&services["source"]);
    let mut said = format!("Where these services come from: {source}.");
    if source.contains("no .ccc/map.json") {
        said.push_str(
            " A repository can say where its services start and end in .ccc/map.json, but needs \
             nothing of the kind to be scanned.",
        );
    }
    let mut sections = vec![Section::Hint(said)];
    let listed = list(&services["services"]);
    if listed.is_empty() {
        sections.push(Section::Empty("ccc found no services.".into()));
    } else {
        let mut table = Table::new(&["Service", "Files#", "Functions#", "Paths"]);
        for service in listed {
            table.rows.push(vec![
                Cell::text(text(&service["name"])),
                Cell::number(number(&service["files"])),
                Cell::number(number(&service["funcs"])),
                Cell::mono(names(&service["globs"])),
            ]);
        }
        sections.push(Section::Table(table));
    }
    let edges = list(&services["edges"]);
    if !edges.is_empty() {
        let mut table = Table::new(&["From", "To", "Found", "Symbols#", "Such as"]);
        for edge in edges.iter().take(SHOWN) {
            let found = match (edge["detected"] == true, edge["declared"] == true) {
                (true, true) => Cell::badge("Declared and found", "ready"),
                (true, false) => Cell::badge("Found in code", "ready"),
                (false, true) => Cell::badge("Declared, not found", "unknown"),
                (false, false) => Cell::badge("Unknown", "unknown"),
            };
            table.rows.push(vec![
                Cell::text(text(&edge["from"])),
                Cell::text(text(&edge["to"])),
                found,
                Cell::number(number(&edge["count"])),
                Cell::mono(names(&edge["symbols"])),
            ]);
        }
        sections.push(Section::Heading("Calls between services".into()));
        sections.push(Section::Table(table.captioned(edges.len())));
    }
    let unassigned = number(&services["unassigned"]);
    if unassigned > 0 {
        let some: Vec<String> =
            list(&services["unassigned_files"]).iter().take(5).map(text).collect();
        sections.push(Section::Hint(format!(
            "{} files belong to no service, such as {}.",
            thousands(unassigned),
            some.join(", ")
        )));
    }
    sections
}

fn severity(word: &str) -> Cell {
    match word {
        "warn" => Cell::badge("Warning", "degraded"),
        "info" => Cell::badge("Note", "unknown"),
        "high" => Cell::badge("High", "error"),
        "critical" => Cell::badge("Critical", "error"),
        "medium" | "moderate" => Cell::badge("Medium", "degraded"),
        "low" => Cell::badge("Low", "unknown"),
        other => Cell::badge(other, "unknown"),
    }
}

fn lints(report: &Value, links: &Links) -> Vec<Section> {
    let lints = &report["lints"];
    let mut sections = vec![Section::Hint(
        "Syntax-level heuristics with no type or data-flow information: each names what it \
         measured, so check it in the source before acting on it."
            .into(),
    )];
    let findings = list(&lints["findings"]);
    if findings.is_empty() {
        sections.push(Section::Empty("ccc found nothing to lint.".into()));
        return sections;
    }
    let mut rules = Table::new(&["Rule", "Kind", "Found#", "What it looks for"]);
    for rule in list(&lints["rules"]) {
        let name = text(&rule["rule"]);
        let found = number(&lints["by_rule"][&name]);
        if found == 0 {
            continue;
        }
        rules.rows.push(vec![
            Cell::mono(&name),
            severity(&text(&rule["severity"])),
            Cell::number(found),
            Cell::text(text(&rule["what"])).hinted(text(&rule["limits"])),
        ]);
    }
    sections.push(Section::Table(rules));
    let mut table = Table::new(&["Kind", "Rule", "Where", "Function", "Finding"]);
    for finding in findings.iter().take(SHOWN) {
        table.rows.push(vec![
            severity(&text(&finding["severity"])),
            Cell::mono(text(&finding["rule"])),
            links.place(finding),
            Cell::mono(text(&finding["function"])),
            Cell::text(text(&finding["message"])).hinted(text(&finding["hint"])),
        ]);
    }
    sections.push(Section::Heading("Findings".into()));
    sections.push(Section::Table(table.captioned(findings.len())));
    if lints["truncated"] == true {
        sections.push(Section::Hint("ccc lists at most 400 findings, warnings first.".into()));
    }
    sections
}

fn tests(report: &Value, links: &Links) -> Vec<Section> {
    let tests = &report["tests"];
    let summary = &tests["summary"];
    let mut sections = vec![
        Section::Tiles(vec![
            Tile { label: "Functions ranked".into(), value: thousands(number(&summary["functions"])), note: "By structural risk".into() },
            Tile { label: "No test reaches them".into(), value: thousands(number(&summary["untested"])), note: String::new() },
        ]),
        Section::Hint(
            "Covered means a test calls the function, matched through its type, file, an import or \
             a qualifier. It does not mean its behaviour is asserted."
                .into(),
        ),
    ];
    if let Some(kinds) = summary["by_kind"].as_object().filter(|kinds| !kinds.is_empty()) {
        let mut table = Table::new(&["Kind of test ccc suggests", "Functions#"]);
        for (kind, count) in kinds {
            table.rows.push(vec![Cell::text(kind.clone()), Cell::number(number(count))]);
        }
        sections.push(Section::Table(table));
    }
    let targets = list(&tests["targets"]);
    if !targets.is_empty() {
        let mut table =
            Table::new(&["Function", "Where", "Suggested test", "Covered", "Priority#"]);
        for target in targets {
            table.rows.push(vec![
                Cell::mono(text(&target["function"])),
                links.place(target),
                Cell::text(text(&target["kind"])),
                match target["covered"] == true {
                    true => Cell::badge("Yes", "ready").hinted(names(&target["covered_by"])),
                    false => Cell::badge("No", "degraded"),
                },
                Cell::number(number(&target["priority"])),
            ]);
        }
        sections.push(Section::Heading("Where a test would help most".into()));
        sections.push(Section::Table(table));
    }
    sections
}

fn security_sections(backend: &Backend, report: &Value, links: &Links) -> Vec<Section> {
    let mut sections = vec![Section::Heading("Security findings".into())];
    let security = &report["security"];
    if security["available"] != true {
        sections.push(Section::Problem(format!(
            "ccc's security scan did not run: {}",
            text(&security["reason"])
        )));
    } else {
        let counts = &security["counts"];
        sections.push(Section::Tiles(vec![
            Tile {
                label: "High".into(),
                value: thousands(number(&counts["high"])),
                note: String::new(),
            },
            Tile {
                label: "Medium".into(),
                value: thousands(number(&counts["medium"])),
                note: String::new(),
            },
            Tile {
                label: "Low".into(),
                value: thousands(number(&counts["low"])),
                note: String::new(),
            },
            Tile {
                label: "Files scanned".into(),
                value: thousands(number(&security["files_scanned"])),
                note: String::new(),
            },
        ]));
        sections.push(Section::Hint(
            "Every finding is a match on the syntax, with no data flow behind it: confirm it in the \
             source. Test files are left out, since they carry fixtures rather than leaks."
                .into(),
        ));
        let findings = list(&security["findings"]);
        if findings.is_empty() {
            sections.push(Section::Empty("ccc's rules matched nothing.".into()));
        } else {
            let mut table = Table::new(&["Severity", "Rule", "Where", "Finding", "Evidence"]);
            for finding in findings.iter().take(SHOWN) {
                table.rows.push(vec![
                    severity(&text(&finding["severity"])),
                    Cell::mono(text(&finding["rule"])).hinted(text(&finding["cwe"])),
                    links.place(finding),
                    Cell::text(text(&finding["message"])).hinted(text(&finding["hint"])),
                    Cell::mono(text(&finding["evidence"])),
                ]);
            }
            sections.push(Section::Table(table.captioned(number(&security["total"]) as usize)));
        }
    }

    sections.push(Section::Heading("Dependencies".into()));
    let dependencies = &report["dependencies"];
    if dependencies["available"] != true {
        sections.push(Section::Problem(format!(
            "ccc's audit did not run: {}",
            text(&dependencies["reason"])
        )));
        return sections;
    }
    let lockfiles = names(&dependencies["lockfiles"]);
    sections.push(Section::Summary(vec![
        ("Packages".into(), Cell::text(thousands(number(&dependencies["packages"])))),
        ("Direct".into(), Cell::text(thousands(number(&dependencies["direct"])))),
        (
            "Lockfiles".into(),
            Cell::mono(if lockfiles.is_empty() { "None found".into() } else { lockfiles }),
        ),
    ]));
    if dependencies["assessed"] != true {
        let said = match (backend.feature(ADVISORIES), dependencies["error"].as_str()) {
            (_, Some(error)) => format!("They were not checked against OSV: {error}."),
            (false, None) => "They were not checked against OSV: Dependency advisories are off in this plugin's settings.".into(),
            (true, None) => "They were not checked against OSV at this scan.".into(),
        };
        sections.push(Section::Hint(said));
    } else if list(&dependencies["findings"]).is_empty() {
        sections.push(Section::Empty(format!(
            "No known advisory against any of the {} packages.",
            thousands(number(&dependencies["packages"]))
        )));
    } else {
        let mut table = Table::new(&["Severity", "Package", "Reach", "Advisory", "Fixed in"]);
        for finding in list(&dependencies["findings"]).iter().take(SHOWN) {
            let reach = match (finding["direct"] == true, finding["dev"] == true) {
                (true, false) => "Direct",
                (true, true) => "Direct, development only",
                (false, false) => "Transitive",
                (false, true) => "Transitive, development only",
            };
            table.rows.push(vec![
                severity(&text(&finding["severity"])),
                Cell::mono(format!("{}@{}", text(&finding["name"]), text(&finding["version"])))
                    .hinted(text(&finding["ecosystem"])),
                Cell::text(reach).hinted(text(&finding["lockfile"])),
                Cell::mono(text(&finding["id"]))
                    .linked(web(finding["url"].as_str()))
                    .hinted(text(&finding["summary"])),
                Cell::text(match text(&finding["fixed"]) {
                    fixed if fixed.is_empty() => "No fix published".into(),
                    fixed => fixed,
                }),
            ]);
        }
        sections.push(Section::Table(table.captioned(number(&dependencies["total"]) as usize)));
    }
    sections
}
