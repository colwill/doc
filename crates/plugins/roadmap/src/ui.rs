//! The pages at `/p/roadmap/…`: every release due for everything, a service, a team or an
//! organisation, on a timeline and in a table with whether its services are ready; what shipped
//! lately; one release with its issues and what each plugin says of each of its services; the form
//! that gives a service its Jira project; and the Roadmap panel on the Catalogue's pages.

use std::collections::BTreeSet;
use std::time::Duration as Wait;

use askama::Template as Page;
use chrono::{Duration, NaiveDate, Utc};
use doc_plugin_sdk::{Backend, Query, Request, Response};
use serde_json::Value;

use crate::plan::{Issue, Progress, Release, Status, day};
use crate::readiness::{self, Column, State};
use crate::scope::{self, Scope};
use crate::settings::{Definitions, ROADMAP};
use crate::timeline::{self, Lane, Mark, Span};
use crate::view::{self, View};
use crate::{Refusal, faux, given, parameter};

/// Releases on the panel, the soonest.
const PANEL_ROWS: usize = 5;
/// Issues listed on a release's page; the API gives them all.
const LISTED_ISSUES: usize = 300;
/// How long a source is given to say how its release data stands.
const ASKING: Wait = Wait::from_secs(5);

/// What every page shares: its tab, what it is about, and whether its data is made up.
pub struct Frame {
    pub tab: &'static str,
    pub heading: String,
    pub scope: Scope,
    /// Faux data from `faux-data`, which the platform says at the top of every page.
    pub faux: bool,
}

impl Frame {
    fn new(backend: &Backend, tab: &'static str, scope: Scope) -> Self {
        let heading = match (&scope, tab) {
            (Scope::All, "released") => "Released lately".to_string(),
            (Scope::All, _) => "Delivery roadmap".to_string(),
            (scope, "released") => format!("Released lately for {}", scope.label()),
            (scope, "project") => format!("{}'s Jira project", scope.label()),
            (scope, _) => format!("Roadmap for {}", scope.label()),
        };
        Self { tab, heading, scope, faux: faux::on(backend) }
    }

    /// Where a service is given its Jira project, when the page is about one.
    pub fn project(&self) -> Option<String> {
        match &self.scope {
            Scope::Service(name) => Some(format!("/p/roadmap/services/{}", given::segment(name))),
            _ => None,
        }
    }

    fn link(&self, path: &str) -> String {
        match self.scope.query() {
            query if query.is_empty() => format!("/p/roadmap/{path}"),
            query => format!("/p/roadmap/{path}?{query}"),
        }
    }

    pub fn overview(&self) -> String {
        self.link("")
    }

    pub fn released(&self) -> String {
        self.link("released")
    }

    pub fn catalogue(&self) -> Option<String> {
        self.scope.catalogue_href()
    }
}

pub struct Tile {
    pub label: &'static str,
    pub value: String,
    pub badge: Option<&'static str>,
    pub note: String,
}

/// One plugin's word on a release, for a table cell.
pub struct Cell {
    pub state: State,
    pub said: String,
}

/// A release as a table shows it.
pub struct Row {
    pub title: String,
    pub href: String,
    pub project: String,
    pub services: Vec<(String, String)>,
    pub due: String,
    pub progress: String,
    pub status: Status,
    pub why: String,
    pub cells: Vec<Cell>,
    pub ready: Option<State>,
}

/// One source of releases and how it stands, for a page with nothing to show yet.
pub struct Source {
    pub id: String,
    pub said: String,
    pub ready: bool,
}

pub struct Onboarding {
    pub on: bool,
    pub admin: bool,
    pub sources: Vec<Source>,
}

/// Somewhere a page sends someone to change something, and what the link says.
pub struct Link {
    pub href: String,
    pub said: &'static str,
}

/// One project the form offers.
pub struct Choice {
    pub key: String,
    pub said: String,
    pub chosen: bool,
}

#[derive(Page)]
#[template(path = "home.html")]
struct Home {
    frame: Frame,
    onboarding: Option<Onboarding>,
    problem: Option<String>,
    about: String,
    tiles: Vec<Tile>,
    timeline: Option<String>,
    titles: Vec<(String, bool)>,
    rows: Vec<Row>,
    problems: Vec<String>,
    unmapped: Vec<String>,
    /// Choosing or changing a service's Jira project, for someone who can.
    given: Option<Link>,
}

#[derive(Page)]
#[template(path = "released.html")]
struct Released {
    frame: Frame,
    problem: Option<String>,
    rows: Vec<Row>,
    days: i64,
}

pub struct IssueRow {
    pub key: String,
    pub url: Option<String>,
    pub summary: String,
    pub kind: String,
    pub status: String,
    pub badge: &'static str,
}

#[derive(Page)]
#[template(path = "release.html")]
struct ReleasePage {
    frame: Frame,
    problem: Option<String>,
    title: String,
    project: String,
    project_url: Option<String>,
    jira_url: Option<String>,
    description: Option<String>,
    start: String,
    due: String,
    status: Status,
    why: String,
    progress: Progress,
    services: Vec<(String, String)>,
    columns: Vec<Column>,
    released: bool,
    issues: Vec<IssueRow>,
    more_issues: usize,
}

#[derive(Page)]
#[template(path = "project.html")]
struct ProjectPage {
    frame: Frame,
    name: String,
    problem: Option<String>,
    saved: bool,
    admin: bool,
    /// The plugins releases are read from, for an admin to connect when none has read a project.
    sources: Vec<String>,
    /// No choice first, then every project read, then any it is given that none has read.
    choices: Vec<Choice>,
    /// The projects it is given now, when there are several: choosing one replaces them.
    several: Vec<String>,
    component: String,
    label: String,
    /// Its Roadmap section in the Catalogue.
    back: String,
}

#[derive(Page)]
#[template(path = "panel.html")]
struct Panel {
    message: Option<String>,
    rows: Vec<Row>,
    more: String,
    /// Choosing a Jira project for a service that has none, for someone who can.
    choose: Option<String>,
    /// Changing the Jira project of a service that has one, for someone who can.
    change: Option<String>,
}

fn drawn<T: Page>(page: &T) -> Response {
    match page.render() {
        Ok(html) => Response::html(html),
        Err(err) => {
            Response::problem(500, "internal", &format!("the page could not be drawn: {err}"))
        }
    }
}

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/');
    let query = request.query.as_str();
    match (request.method.as_str(), path) {
        ("GET", "ui") => overview(backend, query).await,
        ("GET", "ui/released") => released(backend, query).await,
        ("GET", "ui/release") => release(backend, query).await,
        ("GET", "ui/panel") => panel(backend, query).await,
        ("GET", path) if path.starts_with("ui/insight/") => {
            insight(backend, path.trim_start_matches("ui/insight/"), query).await
        }
        ("GET", path) if path.starts_with("ui/services/") => {
            project(backend, path.trim_start_matches("ui/services/"), None).await
        }
        ("POST", path) if path.starts_with("ui/services/") => {
            let posted = url::form_urlencoded::parse(&request.body).into_owned().collect();
            project(backend, path.trim_start_matches("ui/services/"), Some(posted)).await
        }
        _ => Response::not_found(),
    }
}

/// A link to somewhere outside DOC, only ever to the web: what a source exports is not trusted
/// to be one.
pub fn web(url: Option<&str>) -> Option<String> {
    url.filter(|url| url.starts_with("https://") || url.starts_with("http://")).map(str::to_string)
}

fn service_links(release: &Release) -> Vec<(String, String)> {
    release
        .services
        .iter()
        .map(|(name, title)| {
            (title.clone(), format!("/p/roadmap/?{}", scope::encoded(&[("service", name)])))
        })
        .collect()
}

pub fn row(release: &Release, columns: &[Column]) -> Row {
    Row {
        title: release.title(),
        href: release.href(),
        project: release.project.clone(),
        services: service_links(release),
        due: release.due_said(),
        progress: release.progress.said(),
        status: release.status,
        why: release.why.clone(),
        cells: columns
            .iter()
            .map(|column| Cell { state: column.state, said: column.said() })
            .collect(),
        ready: readiness::worst(columns),
    }
}

fn plural(count: usize, one: &str, many: &str) -> String {
    match count {
        1 => format!("1 {one}"),
        count => format!("{count} {many}"),
    }
}

fn tiles(view: &View, upcoming: &[&Release], back_days: i64) -> Vec<Tile> {
    let count = |status: Status| upcoming.iter().filter(|release| release.status == status).count();
    let not_ready = upcoming
        .iter()
        .filter(|release| {
            matches!(
                readiness::worst(&view.columns(release)),
                Some(State::Blocked | State::Warning)
            )
        })
        .count();
    let blocked = upcoming
        .iter()
        .filter(|release| readiness::worst(&view.columns(release)) == Some(State::Blocked))
        .count();
    let shipped = view.releases.iter().filter(|release| release.version.released).count();
    let badge = |count: usize, badge: &'static str| (count > 0).then_some(badge);
    vec![
        Tile {
            label: "Coming up",
            value: upcoming.len().to_string(),
            badge: None,
            note: format!("{} not scheduled", count(Status::Unscheduled)),
        },
        Tile {
            label: "Overdue",
            value: count(Status::Overdue).to_string(),
            badge: badge(count(Status::Overdue), "error"),
            note: "Past their date, not released".into(),
        },
        Tile {
            label: "At risk",
            value: count(Status::AtRisk).to_string(),
            badge: badge(count(Status::AtRisk), "degraded"),
            note: "Behind for the time gone".into(),
        },
        Tile {
            label: "Services not ready",
            value: not_ready.to_string(),
            badge: badge(not_ready, if blocked > 0 { "error" } else { "degraded" }),
            note: format!("{} blocked", plural(blocked, "release", "releases")),
        },
        Tile {
            label: "Released",
            value: shipped.to_string(),
            badge: None,
            note: format!("In the last {back_days} days"),
        },
    ]
}

/// The releases coming up on a timeline: from start to release date, coloured by where each
/// stands, with its date marked; an overdue one runs on to today.
fn drawn_timeline(upcoming: &[&Release], today: NaiveDate) -> Option<String> {
    let dated: Vec<&&Release> = upcoming.iter().filter(|release| release.due().is_some()).collect();
    if dated.is_empty() {
        return None;
    }
    let last = dated.iter().filter_map(|release| release.due()).max().unwrap_or(today);
    let first = dated
        .iter()
        .filter_map(|release| release.version.start_date.or(release.due()))
        .min()
        .unwrap_or(today)
        .max(today - Duration::days(90));
    let window = (
        first.min(today) - Duration::days(7),
        last.max(today + Duration::days(30)) + Duration::days(14),
    );
    let lanes: Vec<Lane> = dated
        .iter()
        .map(|release| {
            let due = release.due().unwrap_or(today);
            let start = release.version.start_date.unwrap_or(due - Duration::days(14)).min(due);
            let end = if release.status == Status::Overdue { today } else { due };
            Lane {
                label: release.title(),
                href: Some(release.href()),
                spans: vec![Span {
                    from: start,
                    to: Some(end),
                    tone: release.status.tone(),
                    said: format!("{}: {}", release.status.word(), release.why),
                }],
                marks: vec![Mark {
                    at: due,
                    tone: release.status.tone(),
                    said: format!("Due {}", day(due)),
                }],
            }
        })
        .collect();
    let described =
        "Each release coming up, from its start to its release date, coloured by where it stands";
    Some(timeline::drawn(&lanes, window, today, described))
}

/// How each running source's release data stands. Only a running one is asked, and none is
/// waited on for long.
async fn sources(backend: &Backend, definitions: &Definitions) -> Vec<Source> {
    let running: Vec<String> = backend
        .query_all::<Value>(Query::new("core.plugins").fields(&["id", "state"]))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|plugin| matches!(plugin["state"].as_str(), Some("running" | "cancelled")))
        .filter_map(|plugin| plugin["id"].as_str().map(str::to_string))
        .collect();
    let mut found = Vec::new();
    for id in definitions.sources.iter().filter(|id| running.contains(id)) {
        let asked = backend.discovery(id, "GET", "releases", None, None);
        let Ok(Ok((200, status))) = tokio::time::timeout(ASKING, asked).await else {
            let said = "It did not say how its release data stands.".to_string();
            found.push(Source { id: id.clone(), said, ready: false });
            continue;
        };
        let (said, ready) = match (
            status["on"].as_bool(),
            status["problem"].as_str(),
            status["synced"]["at"].as_str(),
        ) {
            (Some(false), _, _) => ("Its Release data feature is off.".to_string(), false),
            (_, Some(problem), _) => (format!("It cannot read releases: {problem}."), false),
            (_, None, Some(_)) => ("Releases have been read.".to_string(), true),
            (_, None, None) => ("It is reading releases for the first time.".to_string(), true),
        };
        found.push(Source { id: id.clone(), said, ready });
    }
    found
}

/// What a page shows, in a sentence; `projects` are those a service is given, when it is about one.
fn about(scope: &Scope, faux: bool, tracking: bool, projects: &BTreeSet<String>) -> String {
    match (faux, scope) {
        (true, scope) => format!(
            "Faux releases and issues for {}, made up by {}; whether its services are ready is \
             asked of the other plugins as usual, and each column says whose data it is.",
            scope.label(),
            faux::PROVIDER
        ),
        (false, Scope::All) if tracking => "The releases of the Jira projects tracked on the \
             Settings page, and of every project a service is given in the Catalogue."
            .to_string(),
        (false, Scope::All) => "Every release planned in the Jira projects read, for the \
             services each project is given to, in the Catalogue or on the Settings page."
            .to_string(),
        (false, Scope::Service(name)) if projects.is_empty() => {
            format!("{name} has no Jira project yet, so the roadmap has no releases for it.")
        }
        (false, Scope::Service(name)) => format!(
            "The releases planned in Jira for {name}, from {}.",
            projects.iter().cloned().collect::<Vec<_>>().join(" and ")
        ),
        (false, scope) => {
            format!("The releases planned in Jira for the services of {}.", scope.label())
        }
    }
}

async fn overview(backend: &Backend, query: &str) -> Response {
    let scope = match Scope::from_query(query) {
        Ok(scope) => scope,
        Err(refusal) => return refusal.response(),
    };
    let definitions = Definitions::read(&backend.settings());
    let mut page = Home {
        frame: Frame::new(backend, "overview", scope),
        onboarding: None,
        problem: None,
        about: String::new(),
        tiles: Vec::new(),
        timeline: None,
        titles: Vec::new(),
        rows: Vec::new(),
        problems: Vec::new(),
        unmapped: Vec::new(),
        given: None,
    };
    let view = match view::read(backend, &page.frame.scope, true).await {
        Ok(view) => view,
        Err(refusal) => {
            page.problem = Some(refusal.detail);
            return drawn(&page);
        }
    };
    if view.releases.is_empty() && !page.frame.faux && page.frame.scope == Scope::All {
        page.onboarding = Some(Onboarding {
            on: backend.feature(ROADMAP),
            admin: backend.caller().is_some_and(|caller| caller.admin),
            sources: sources(backend, &definitions).await,
        });
        return drawn(&page);
    }
    let today = Utc::now().date_naive();
    let upcoming: Vec<&Release> =
        view.releases.iter().filter(|release| !release.version.released).collect();
    let projects: BTreeSet<String> = view
        .members
        .iter()
        .flat_map(|member| member.projects.iter().chain(member.tracked.iter()).cloned())
        .collect();
    page.about =
        about(&page.frame.scope, page.frame.faux, !definitions.tracked.is_empty(), &projects);
    page.given = page.frame.project().filter(|_| backend.writes()).map(|href| Link {
        href,
        said: match view.members.iter().all(|member| member.projects.is_empty()) {
            true => "Choose its Jira project",
            false => "Change its Jira project",
        },
    });
    page.tiles = tiles(&view, &upcoming, definitions.back_days);
    page.timeline = drawn_timeline(&upcoming, today);
    page.titles = view.readiness.titles();
    page.rows = upcoming.iter().map(|release| row(release, &view.columns(release))).collect();
    page.problems = view.readiness.problems();
    page.unmapped = view.unnamed.clone();
    drawn(&page)
}

async fn released(backend: &Backend, query: &str) -> Response {
    let scope = match Scope::from_query(query) {
        Ok(scope) => scope,
        Err(refusal) => return refusal.response(),
    };
    let definitions = Definitions::read(&backend.settings());
    let frame = Frame::new(backend, "released", scope);
    let view = match view::read(backend, &frame.scope, false).await {
        Ok(view) => view,
        Err(refusal) => {
            let page = Released {
                frame,
                problem: Some(refusal.detail),
                rows: Vec::new(),
                days: definitions.back_days,
            };
            return drawn(&page);
        }
    };
    let mut shipped: Vec<&Release> =
        view.releases.iter().filter(|release| release.version.released).collect();
    shipped.sort_by_key(|release| std::cmp::Reverse(release.due()));
    let rows = shipped.iter().map(|release| row(release, &[])).collect();
    drawn(&Released { frame, problem: None, rows, days: definitions.back_days })
}

fn issue_row(issue: &Issue) -> IssueRow {
    IssueRow {
        key: issue.key.clone(),
        url: web(issue.url.as_deref()),
        summary: issue.summary.clone().unwrap_or_default(),
        kind: issue.kind.clone().unwrap_or_default(),
        status: issue.status.clone().unwrap_or_else(|| issue.category.clone()),
        badge: match issue.category.as_str() {
            "done" => "ready",
            "indeterminate" => "loading",
            _ => "unknown",
        },
    }
}

async fn release(backend: &Backend, query: &str) -> Response {
    let frame = Frame::new(backend, "release", Scope::All);
    let mut page = ReleasePage {
        frame,
        problem: None,
        title: String::new(),
        project: String::new(),
        project_url: None,
        jira_url: None,
        description: None,
        start: String::new(),
        due: String::new(),
        status: Status::Unscheduled,
        why: String::new(),
        progress: Progress::default(),
        services: Vec::new(),
        columns: Vec::new(),
        released: false,
        issues: Vec::new(),
        more_issues: 0,
    };
    let Some(key) = parameter(query, "id") else {
        page.problem = Some("Name a release with ?id=, as its link does.".into());
        return drawn(&page);
    };
    let (release, columns) = match view::one(backend, &key).await {
        Ok(found) => found,
        Err(refusal) => {
            page.problem = Some(refusal.detail);
            return drawn(&page);
        }
    };
    page.frame.heading = release.title();
    page.title = release.title();
    page.project = release.project.clone();
    page.project_url = web(release.project_url.as_deref());
    page.jira_url = web(release.version.url.as_deref());
    page.description = release.version.description.clone().filter(|text| !text.trim().is_empty());
    page.start = release.version.start_date.map_or_else(|| "Not set".to_string(), day);
    page.due = release.due_said();
    page.status = release.status;
    page.why = release.why.clone();
    page.progress = release.progress;
    page.services = service_links(&release);
    page.columns = columns;
    page.released = release.version.released;
    let mut issues = release.issues.clone();
    issues.sort_by(|one, two| {
        let rank = |issue: &Issue| match issue.category.as_str() {
            "indeterminate" => 0,
            "new" => 1,
            "done" => 3,
            _ => 2,
        };
        let number = |issue: &Issue| {
            let (project, number) = issue.key.rsplit_once('-').unwrap_or((&issue.key, ""));
            (project.to_string(), number.parse::<u64>().unwrap_or(u64::MAX))
        };
        (rank(one), number(one)).cmp(&(rank(two), number(two)))
    });
    page.more_issues = issues.len().saturating_sub(LISTED_ISSUES);
    page.issues = issues.iter().take(LISTED_ISSUES).map(issue_row).collect();
    drawn(&page)
}

/// What may be pinned, by the name each is pinned under. "Coming up" and "Released" are counts
/// of things going to plan, which nobody pins; these are the ones somebody acts on.
pub const INSIGHTS: [(&str, &str); 3] =
    [("overdue", "Overdue"), ("at-risk", "At risk"), ("not-ready", "Services not ready")];

#[derive(Page)]
#[template(path = "insight.html")]
struct InsightTile {
    label: String,
    value: String,
    note: String,
    more: String,
}

/// One of them, for pinning to the top of a resource's page: the same figure the roadmap shows,
/// on its own.
async fn insight(backend: &Backend, which: &str, query: &str) -> Response {
    let Some((_, label)) = INSIGHTS.iter().find(|(id, _)| *id == which) else {
        return Response::not_found();
    };
    let mut tile = InsightTile {
        label: (*label).to_string(),
        value: "—".into(),
        note: String::new(),
        more: "/p/roadmap/".into(),
    };
    let scope = match Scope::from_query(query) {
        Ok(scope) => scope,
        Err(_) => {
            tile.note = "Nothing was named to look at".into();
            return drawn(&tile);
        }
    };
    tile.more = match scope.query() {
        query if query.is_empty() => "/p/roadmap/".to_string(),
        query => format!("/p/roadmap/?{query}"),
    };
    match view::read(backend, &scope, true).await {
        Err(refusal) => tile.note = refusal.detail,
        Ok(view) => {
            let upcoming: Vec<&Release> =
                view.releases.iter().filter(|release| !release.version.released).collect();
            let back_days = Definitions::read(&backend.settings()).back_days;
            let tiles = tiles(&view, &upcoming, back_days);
            match tiles.into_iter().find(|tile| tile.label == *label) {
                Some(found) => {
                    tile.value = found.value;
                    tile.note = found.note;
                }
                None => tile.note = "Nothing to measure yet".into(),
            }
        }
    }
    drawn(&tile)
}

async fn panel(backend: &Backend, query: &str) -> Response {
    let scope = match Scope::from_query(query) {
        Ok(scope) => scope,
        Err(refusal) => return refusal.response(),
    };
    let more = match scope.query() {
        query if query.is_empty() => "/p/roadmap/".to_string(),
        query => format!("/p/roadmap/?{query}"),
    };
    let mut page = Panel { message: None, rows: Vec::new(), more, choose: None, change: None };
    match view::read(backend, &scope, true).await {
        Err(refusal) => page.message = Some(refusal.detail),
        Ok(view) => {
            let upcoming: Vec<&Release> = view
                .releases
                .iter()
                .filter(|release| !release.version.released)
                .take(PANEL_ROWS)
                .collect();
            let projects: BTreeSet<String> = view
                .members
                .iter()
                .flat_map(|member| member.projects.iter().chain(member.tracked.iter()).cloned())
                .collect();
            let form = match &scope {
                Scope::Service(name) if backend.writes() => {
                    Some(format!("/p/roadmap/services/{}", given::segment(name)))
                }
                _ => None,
            };
            match view.members.iter().all(|member| member.projects.is_empty()) {
                true => page.choose = form,
                false => page.change = form,
            }
            if upcoming.is_empty() {
                page.message = Some(match &scope {
                    Scope::Service(name) if projects.is_empty() => {
                        format!("{name} has no Jira project yet, so there are no releases to show.")
                    }
                    Scope::Service(_) => format!(
                        "No release of {} is coming up.",
                        projects.iter().cloned().collect::<Vec<_>>().join(" or ")
                    ),
                    _ => "No release is coming up.".to_string(),
                });
            }
            page.rows =
                upcoming.iter().map(|release| row(release, &view.columns(release))).collect();
        }
    }
    drawn(&page)
}

/// The form that gives a service its Jira project; with `posted`, saving it first.
async fn project(backend: &Backend, name: &str, posted: Option<Vec<(String, String)>>) -> Response {
    let definitions = Definitions::read(&backend.settings());
    let mut page = ProjectPage {
        frame: Frame::new(backend, "project", Scope::Service(name.to_string())),
        name: name.to_string(),
        problem: None,
        saved: false,
        admin: backend.caller().is_some_and(|caller| caller.admin),
        sources: definitions.sources.clone(),
        choices: Vec::new(),
        several: Vec::new(),
        component: String::new(),
        label: String::new(),
        back: format!("/p/resources/r/service/{}?section=panel-roadmap", given::segment(name)),
    };
    let field = |posted: &[(String, String)], key: &str| {
        posted.iter().find(|(named, _)| named == key).map(|(_, value)| value.trim().to_string())
    };
    let (before, projects) =
        match (given::given(backend, name).await, given::projects(backend).await) {
            (Ok(before), Ok(projects)) => (before, projects),
            (Err(refusal), _) | (_, Err(refusal)) => {
                page.problem = Some(refusal.detail);
                return drawn(&page);
            }
        };
    // What was sent, kept on the form when saving it is refused.
    let mut asked = None;
    if let Some(posted) = &posted {
        let (project, component, label) = (
            field(posted, "project").unwrap_or_default().to_ascii_uppercase(),
            field(posted, "component").unwrap_or_default(),
            field(posted, "label").unwrap_or_default(),
        );
        let offered = project.is_empty()
            || before.projects.contains(&project)
            || projects.iter().any(|known| known.key.eq_ignore_ascii_case(&project));
        let saved = match offered {
            true => given::give(backend, name, &project, &component, &label).await,
            false => Err(Refusal::bad(format!("{project} is not a Jira project that was read"))),
        };
        match saved {
            Ok(()) => page.saved = true,
            Err(refusal) => {
                page.problem = Some(refusal.detail);
                asked = Some((project, component, label));
            }
        }
    }
    let now = match page.saved {
        true => match given::given(backend, name).await {
            Ok(now) => now,
            Err(refusal) => {
                page.problem = Some(refusal.detail);
                return drawn(&page);
            }
        },
        false => before,
    };
    let (chosen, component, label) = asked.unwrap_or_else(|| {
        (now.projects.first().cloned().unwrap_or_default(), now.component, now.label)
    });
    (page.component, page.label) = (component, label);
    if now.projects.len() > 1 {
        page.several = now.projects.clone();
    }
    page.choices.push(Choice {
        key: String::new(),
        said: "None".into(),
        chosen: chosen.is_empty(),
    });
    for project in &projects {
        let key = project.key.to_ascii_uppercase();
        page.choices.push(Choice {
            said: match project.name.is_empty() {
                true => key.clone(),
                false => format!("{} ({key})", project.name),
            },
            chosen: key == chosen,
            key,
        });
    }
    // A project it is given that no source has read is still offered, so saving keeps it.
    for key in &now.projects {
        if !page.choices.iter().any(|choice| choice.key == *key) {
            page.choices.push(Choice {
                key: key.clone(),
                said: format!("{key} (not found in Jira)"),
                chosen: *key == chosen,
            });
        }
    }
    drawn(&page)
}
