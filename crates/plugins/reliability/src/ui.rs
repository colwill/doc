//! The pages at `/p/reliability/…`: every service of a scope against its objectives, one service
//! with how it is watched, DOC as a whole with its parts and plugins, the outages, backups and
//! restores behind every figure, the form that sets how a service is watched, and the Reliability
//! panel on the Catalogue's pages.

use askama::Template as Page;
use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Request, Response};

use crate::api::{self, Configured};
use crate::metrics::{self, Chart, Judged, Period, Row, Tile};
use crate::scope::Scope;
use crate::settings::{Definitions, PLATFORM, SERVICES, Targets};
use crate::store::{Backup, Outage, Restore, Subject};
use crate::{Refusal, faux, parameter, view};

/// What a page can be shown over, shortest first. The four short ones are drawn from telemetry,
/// which DOC keeps for a week, and the rest from the days watched, which it keeps for as long as
/// it keeps outages.
pub const PERIODS: [(&str, &str, i64); 9] = [
    ("1h", "hour", 3_600),
    ("6h", "6 hours", 6 * 3_600),
    ("12h", "12 hours", 12 * 3_600),
    ("24h", "24 hours", 86_400),
    ("7d", "7 days", 7 * 86_400),
    ("30d", "30 days", 30 * 86_400),
    ("90d", "90 days", 90 * 86_400),
    ("180d", "6 months", 180 * 86_400),
    ("365d", "year", 365 * 86_400),
];
const DEFAULT_DAYS: i64 = 30;
pub const MAX_DAYS: i64 = 400;
/// The most outages, backups or restores one listing shows; the API pages through the rest.
const LISTED: usize = 200;
/// Outages, backups and restores on a service's own page.
const RECENT: usize = 10;

/// The days a page covers: `days=`, or 30.
pub fn days(query: &str) -> Result<i64, Refusal> {
    match parameter(query, "days") {
        None => Ok(DEFAULT_DAYS),
        Some(text) => text
            .parse::<i64>()
            .ok()
            .filter(|days| (1..=MAX_DAYS).contains(days))
            .ok_or_else(|| Refusal::bad(format!("`days` is a number from 1 to {MAX_DAYS}"))),
    }
}

/// How long a page covers, as it is written in a link: one of `PERIODS`, or `<n>d` from the
/// `days=` an older link or the API uses.
#[derive(Debug, Clone)]
pub struct Over {
    pub value: String,
    pub seconds: i64,
}

/// The period a page that is not shown over one covers, so its links carry the usual.
fn a_month() -> Over {
    Over { value: format!("{DEFAULT_DAYS}d"), seconds: DEFAULT_DAYS * 86_400 }
}

/// What a page covers: `over=` from the page's own links, `days=` from an older one or the API,
/// or 30 days.
pub fn over(query: &str) -> Result<Over, Refusal> {
    if let Some(asked) = parameter(query, "over") {
        return match PERIODS.iter().find(|(value, _, _)| *value == asked) {
            Some((value, _, seconds)) => {
                Ok(Over { value: (*value).to_string(), seconds: *seconds })
            }
            None => Err(Refusal::bad(format!(
                "`over` is one of {}",
                PERIODS.iter().map(|(value, _, _)| *value).collect::<Vec<_>>().join(", ")
            ))),
        };
    }
    let days = days(query)?;
    Ok(Over { value: format!("{days}d"), seconds: days * 86_400 })
}

/// What every page shares: its tab, what it is about and over how long, and links that keep them.
pub struct Frame {
    pub tab: &'static str,
    pub heading: String,
    pub scope: Scope,
    /// DOC's own, rather than a scope's services.
    pub doc: bool,
    pub over: Over,
    pub periods: Vec<Choice>,
    /// Faux data from `faux-data`, which the platform says at the top of every page.
    pub faux: bool,
    pub writes: bool,
}

/// One of the periods a page can be shown over.
pub struct Choice {
    pub value: &'static str,
    pub said: &'static str,
    pub chosen: bool,
}

impl Frame {
    fn new(backend: &Backend, tab: &'static str, scope: Scope, doc: bool, over: Over) -> Self {
        let heading = match (&scope, tab, doc) {
            (_, "doc", _) => "DOC's reliability".to_string(),
            (_, "outages", true) => "DOC's outages".to_string(),
            (_, "backups", true) => "DOC's backups and restores".to_string(),
            (Scope::All, "outages", _) => "Outages".to_string(),
            (Scope::All, "backups", _) => "Backups and restores".to_string(),
            (Scope::All, _, _) => "Reliability".to_string(),
            (scope, "outages", _) => format!("Outages of {}", scope.label()),
            (scope, "backups", _) => format!("Backups and restores of {}", scope.label()),
            (scope, "edit", _) => format!("How {} is watched", scope.label()),
            (scope, _, _) => format!("Reliability of {}", scope.label()),
        };
        Self {
            tab,
            heading,
            scope,
            doc,
            faux: faux::on(backend),
            writes: backend.caller().is_some_and(|caller| caller.writes()),
            periods: PERIODS
                .iter()
                .map(|(value, said, _)| Choice { value, said, chosen: *value == over.value })
                .collect(),
            over,
        }
    }

    /// The period this page covers.
    pub fn period(&self) -> Period {
        Period::last_seconds(self.over.seconds)
    }

    fn link(&self, path: &str, doc: bool) -> String {
        let scoped = if doc { "doc=1".to_string() } else { self.scope.query() };
        let mut query = format!("over={}", self.over.value);
        if !scoped.is_empty() {
            query = format!("{scoped}&{query}");
        }
        format!("/p/reliability/{path}?{query}")
    }

    pub fn services(&self) -> String {
        self.link("", false)
    }

    pub fn platform(&self) -> String {
        format!("/p/reliability/doc?over={}", self.over.value)
    }

    pub fn outages(&self) -> String {
        self.link("outages", self.doc)
    }

    pub fn backups(&self) -> String {
        self.link("backups", self.doc)
    }

    pub fn action(&self) -> String {
        match self.tab {
            "services" => "/p/reliability/".to_string(),
            tab => format!("/p/reliability/{tab}"),
        }
    }

    pub fn scope_kind(&self) -> &str {
        self.scope.kind()
    }

    pub fn scope_name(&self) -> &str {
        self.scope.name().unwrap_or_default()
    }

    pub fn catalogue(&self) -> Option<String> {
        self.scope.catalogue_href()
    }

    /// Where a service's watching is changed, when this page is about one.
    pub fn edit(&self) -> Option<String> {
        edit_href(&self.scope)
    }
}

/// The form that sets how a service is watched, when the scope is one service.
fn edit_href(scope: &Scope) -> Option<String> {
    match scope {
        Scope::Service(name) => {
            let segment: String = url::form_urlencoded::byte_serialize(name.as_bytes()).collect();
            Some(format!("/p/reliability/services/{segment}"))
        }
        _ => None,
    }
}

pub struct Onboarding {
    pub admin: bool,
}

#[derive(Page)]
#[template(path = "services.html")]
struct Services {
    frame: Frame,
    onboarding: Option<Onboarding>,
    problem: Option<String>,
    tiles: Vec<Tile>,
    charts: Vec<Chart>,
    rows: Vec<Row>,
    unwatched: Vec<String>,
}

/// A line of the summary of how a service is watched.
pub struct Said {
    pub key: &'static str,
    pub value: String,
}

#[derive(Page)]
#[template(path = "service.html")]
struct Service {
    frame: Frame,
    problem: Option<String>,
    tiles: Vec<Tile>,
    watching: Vec<Said>,
    charts: Vec<Chart>,
    outages: Vec<Listed>,
    backups: Vec<Listed>,
}

#[derive(Page)]
#[template(path = "doc.html")]
struct Doc {
    frame: Frame,
    off: bool,
    problem: Option<String>,
    tiles: Vec<Tile>,
    charts: Vec<Chart>,
    parts: Vec<Row>,
    plugins: Vec<Row>,
    outages: Vec<Listed>,
}

/// One outage, backup or restore in a table.
pub struct Listed {
    pub when: String,
    pub what: String,
    pub about: String,
    pub took: String,
    pub badge: &'static str,
    pub state: String,
    pub detail: String,
}

#[derive(Page)]
#[template(path = "listing.html")]
struct Listing {
    frame: Frame,
    problem: Option<String>,
    rows: Vec<Listed>,
    total: usize,
    empty: &'static str,
}

#[derive(Page)]
#[template(path = "edit.html")]
struct Edit {
    frame: Frame,
    name: String,
    problem: Option<String>,
    saved: bool,
    url: String,
    sla: String,
    rto: String,
    rpo: String,
    mttr: String,
    defaults: Targets,
    checks_on: bool,
}

impl Edit {
    pub fn default_sla(&self) -> String {
        metrics::percent(self.defaults.sla)
    }

    pub fn default_time(&self, seconds: &f64) -> String {
        metrics::duration(*seconds)
    }
}

#[derive(Page)]
#[template(path = "panel.html")]
struct Panel {
    message: Option<String>,
    tiles: Vec<Tile>,
    more: String,
    /// The form that sets how the service is watched, for someone who can change it.
    edit: Option<String>,
}

fn drawn<T: Page>(page: &T) -> Response {
    match page.render() {
        Ok(html) => Response::html(html),
        Err(err) => {
            Response::problem(500, "internal", &format!("the page could not be drawn: {err}"))
        }
    }
}

fn form(request: &Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &[(String, String)], name: &str) -> Option<String> {
    form.iter().find(|(key, _)| key == name).map(|(_, value)| value.trim().to_string())
}

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/');
    let query = request.query.as_str();
    let segments: Vec<&str> = path.split('/').collect();
    match (request.method.as_str(), segments.as_slice()) {
        ("GET", ["ui"]) => overview(backend, query).await,
        ("GET", ["ui", "doc"]) => doc(backend, query).await,
        ("GET", ["ui", "outages"]) => outages(backend, query).await,
        ("GET", ["ui", "backups"]) => backups(backend, query).await,
        ("GET", ["ui", "services", name]) => edit(backend, name, None).await,
        ("POST", ["ui", "services", name]) => edit(backend, name, Some(form(request))).await,
        ("GET", ["ui", "panel"]) => panel(backend, query).await,
        ("GET", ["ui", "dashboard"]) => dashboard(backend).await,
        ("GET", ["ui", "insight", which]) => insight(backend, which, query).await,
        _ => Response::not_found(),
    }
}

fn when(at: DateTime<Utc>) -> String {
    at.format("%-d %b %Y, %H:%M").to_string()
}

fn scoped_over(query: &str) -> Result<(Scope, Over), Refusal> {
    Ok((Scope::from_query(query)?, over(query)?))
}

fn is_doc(query: &str) -> bool {
    parameter(query, "doc").is_some_and(|value| value != "0" && value != "false")
}

fn rows(
    judged: &[Judged],
    subjects: &std::collections::BTreeMap<String, Subject>,
    over: &Over,
    link: bool,
) -> Vec<Row> {
    judged
        .iter()
        .map(|judged| {
            let name =
                judged.subject.split_once(':').map_or(judged.subject.as_str(), |(_, name)| name);
            Row {
                title: judged.title.clone(),
                href: link.then(|| {
                    format!(
                        "/p/reliability/?{}",
                        crate::scope::encoded(&[("service", name), ("over", &over.value)])
                    )
                }),
                now: metrics::now(judged, subjects.get(&judged.subject)),
                cells: metrics::cells(judged),
            }
        })
        .collect()
}

fn outage_row(outage: &Outage) -> Listed {
    let now = Utc::now();
    let (badge, state) = match outage.ended_at {
        Some(_) => ("ready", "Recovered".to_string()),
        None => ("error", "Still down".to_string()),
    };
    let source = match outage.source.as_str() {
        "check" => "a failed health check".to_string(),
        "status" => "DOC's status".to_string(),
        "report" => format!("reported by {}", outage.by.as_deref().unwrap_or("someone")),
        // A plugin whose outages are read, such as kubernetes.
        plugin => plugin.to_string(),
    };
    Listed {
        when: when(outage.started_at),
        what: view::titled(&outage.subject),
        about: format!("From {source}"),
        took: metrics::duration(outage.lasted(now)),
        badge,
        state,
        detail: outage.detail.clone().unwrap_or_default(),
    }
}

fn backup_row(backup: &Backup) -> Listed {
    Listed {
        when: when(backup.at),
        what: view::titled(&backup.subject),
        about: match &backup.kind {
            Some(kind) if kind.starts_with(['a', 'e', 'i', 'o', 'u']) => {
                format!("An {kind} backup")
            }
            Some(kind) => format!("A {kind} backup"),
            None => "A backup".to_string(),
        },
        took: String::new(),
        badge: "ready",
        state: "Backed up".into(),
        detail: backup.note.clone().unwrap_or_default(),
    }
}

fn restore_row(restore: &Restore) -> Listed {
    let (badge, state) = match restore.succeeded {
        true => ("ready", "Restored"),
        false => ("error", "Failed"),
    };
    Listed {
        when: when(restore.started_at),
        what: view::titled(&restore.subject),
        about: "A restore from backup".into(),
        took: metrics::duration(restore.seconds),
        badge,
        state: state.to_string(),
        detail: restore.note.clone().unwrap_or_default(),
    }
}

async fn overview(backend: &Backend, query: &str) -> Response {
    let (scope, over) = match scoped_over(query) {
        Ok(asked) => asked,
        Err(refusal) => return refusal.response(),
    };
    if let Scope::Service(name) = &scope {
        return service(backend, name.clone(), over).await;
    }
    let definitions = Definitions::read(&backend.settings());
    let mut page = Services {
        frame: Frame::new(backend, "services", scope, false, over),
        onboarding: None,
        problem: None,
        tiles: Vec::new(),
        charts: Vec::new(),
        rows: Vec::new(),
        unwatched: Vec::new(),
    };
    if !backend.feature(SERVICES) && !page.frame.faux {
        page.onboarding =
            Some(Onboarding { admin: backend.caller().is_some_and(|caller| caller.admin) });
    }
    let period = page.frame.period();
    match view::services(backend, &page.frame.scope, &period).await {
        Ok(services) => {
            let watched: Vec<Judged> = services
                .judged
                .iter()
                .filter(|j| services.held.subjects.contains_key(&j.subject))
                .cloned()
                .collect();
            page.unwatched = services
                .judged
                .iter()
                .filter(|j| !services.held.subjects.contains_key(&j.subject))
                .map(|j| j.title.clone())
                .collect();
            page.tiles = metrics::group_tiles(&watched, &period);
            let objectives: Vec<(String, Targets)> = services
                .objectives(&definitions)
                .into_iter()
                .filter(|(key, _)| services.held.subjects.contains_key(key))
                .collect();
            page.charts = metrics::charts(&services.held, &period, &objectives);
            page.rows = rows(&watched, &services.held.subjects, &page.frame.over, true);
        }
        Err(refusal) => page.problem = Some(refusal.detail),
    }
    drawn(&page)
}

fn watching(subject: Option<&Subject>, targets: Targets) -> Vec<Said> {
    let own = |value: Option<f64>, said: String| match value {
        Some(_) => said,
        None => format!("{said}, the default"),
    };
    let mut said = vec![match subject.and_then(|subject| subject.url.clone()) {
        Some(url) => Said { key: "Health URL", value: url },
        None => Said { key: "Health URL", value: "None: only reported outages count".into() },
    }];
    if let Some(subject) = subject
        && let Some(checked) = subject.checked_at
        && subject.url.is_some()
    {
        let answer = match (subject.status, &subject.problem) {
            (_, Some(problem)) => problem.clone(),
            (Some(status), None) => format!("it answered {status}"),
            (None, None) => "no answer".into(),
        };
        let latency = subject.latency_ms.map(|ms| format!(" in {ms} ms")).unwrap_or_default();
        said.push(Said {
            key: "Last checked",
            value: format!("{}: {answer}{latency}", when(checked)),
        });
    }
    let held = subject.cloned().unwrap_or_default();
    said.push(Said {
        key: "Availability objective",
        value: own(held.sla, metrics::percent(targets.sla)),
    });
    said.push(Said {
        key: "Mean time to recover",
        value: own(held.mttr, metrics::duration(targets.mttr)),
    });
    said.push(Said {
        key: "Recovery time objective",
        value: own(held.rto, metrics::duration(targets.rto)),
    });
    said.push(Said {
        key: "Recovery point objective",
        value: own(held.rpo, metrics::duration(targets.rpo)),
    });
    said
}

async fn service(backend: &Backend, name: String, over: Over) -> Response {
    let scope = Scope::Service(name.clone());
    let mut page = Service {
        frame: Frame::new(backend, "services", scope, false, over),
        problem: None,
        tiles: Vec::new(),
        watching: Vec::new(),
        charts: Vec::new(),
        outages: Vec::new(),
        backups: Vec::new(),
    };
    let period = page.frame.period();
    match view::services(backend, &page.frame.scope, &period).await {
        Ok(services) => {
            let Some(judged) = services.judged.first() else {
                page.problem = Some(format!("{name} is not in the Catalogue"));
                return drawn(&page);
            };
            let key = view::key(&name);
            let subject = services.held.subjects.get(&key);
            page.tiles = metrics::tiles(judged, &period);
            page.watching = watching(subject, judged.targets);
            page.charts =
                metrics::charts(&services.held, &period, &[(key.clone(), judged.targets)]);
            let mut outages: Vec<&Outage> = services.held.outages.iter().collect();
            outages.sort_by_key(|o| (o.ended_at.is_some(), std::cmp::Reverse(o.started_at)));
            page.outages = outages.into_iter().take(RECENT).map(outage_row).collect();
            page.backups = recent(&services.held.backups, &services.held.restores, &period, RECENT);
        }
        Err(refusal) => page.problem = Some(refusal.detail),
    }
    drawn(&page)
}

/// Backups and restores in the period, newest first.
fn recent(backups: &[Backup], restores: &[Restore], period: &Period, limit: usize) -> Vec<Listed> {
    let mut found: Vec<(DateTime<Utc>, Listed)> = backups
        .iter()
        .filter(|backup| period.contains(backup.at))
        .map(|backup| (backup.at, backup_row(backup)))
        .chain(restores.iter().map(|restore| (restore.started_at, restore_row(restore))))
        .collect();
    found.sort_by_key(|(at, _)| std::cmp::Reverse(*at));
    found.into_iter().take(limit).map(|(_, listed)| listed).collect()
}

async fn doc(backend: &Backend, query: &str) -> Response {
    let over = match over(query) {
        Ok(over) => over,
        Err(refusal) => return refusal.response(),
    };
    let mut page = Doc {
        frame: Frame::new(backend, "doc", Scope::All, true, over),
        off: !backend.feature(PLATFORM) && !faux::on(backend),
        problem: None,
        tiles: Vec::new(),
        charts: Vec::new(),
        parts: Vec::new(),
        plugins: Vec::new(),
        outages: Vec::new(),
    };
    let period = page.frame.period();
    match view::doc(backend, &period).await {
        Ok(doc) => {
            page.tiles = metrics::tiles(&doc.whole, &period);
            page.charts =
                metrics::charts(&doc.whole_held, &period, &[("doc".into(), doc.whole.targets)]);
            page.parts = rows(&doc.parts, &doc.held.subjects, &page.frame.over, false);
            page.plugins = rows(&doc.plugins, &doc.held.subjects, &page.frame.over, false);
            let mut outages: Vec<&Outage> = doc.held.outages.iter().collect();
            outages.sort_by_key(|o| (o.ended_at.is_some(), std::cmp::Reverse(o.started_at)));
            page.outages = outages.into_iter().take(RECENT).map(outage_row).collect();
        }
        Err(refusal) => page.problem = Some(refusal.detail),
    }
    drawn(&page)
}

async fn outages(backend: &Backend, query: &str) -> Response {
    let (scope, over) = match scoped_over(query) {
        Ok(asked) => asked,
        Err(refusal) => return refusal.response(),
    };
    let doc = is_doc(query);
    let mut page = Listing {
        frame: Frame::new(backend, "outages", scope, doc, over),
        problem: None,
        rows: Vec::new(),
        total: 0,
        empty: "No outages in this period.",
    };
    let period = page.frame.period();
    match api::listed_outages(backend, query, &period).await {
        Ok(found) => {
            page.total = found.len();
            page.rows = found.iter().take(LISTED).map(outage_row).collect();
        }
        Err(refusal) => page.problem = Some(refusal.detail),
    }
    drawn(&page)
}

#[derive(Page)]
#[template(path = "dashboard.html")]
struct DashboardFragment {
    problem: Option<String>,
    rows: Vec<Listed>,
}

/// Outages still going on, on a person's dashboard: on the services they can see, newest first,
/// from any that started in the last month.
async fn dashboard(backend: &Backend) -> Response {
    let mut page = DashboardFragment { problem: None, rows: Vec::new() };
    match api::listed_outages(backend, "", &Period::last(30)).await {
        Ok(found) => {
            page.rows = found
                .iter()
                .filter(|outage| outage.ended_at.is_none())
                .take(6)
                .map(outage_row)
                .collect();
        }
        Err(refusal) => page.problem = Some(refusal.detail),
    }
    drawn(&page)
}

async fn backups(backend: &Backend, query: &str) -> Response {
    let (scope, over) = match scoped_over(query) {
        Ok(asked) => asked,
        Err(refusal) => return refusal.response(),
    };
    let doc = is_doc(query);
    let mut page = Listing {
        frame: Frame::new(backend, "backups", scope, doc, over),
        problem: None,
        rows: Vec::new(),
        total: 0,
        empty: "No backup or restore was recorded in this period.",
    };
    let period = page.frame.period();
    let held = match doc {
        true => view::doc(backend, &period).await.map(|doc| doc.held),
        false => {
            view::services(backend, &page.frame.scope, &period).await.map(|services| services.held)
        }
    };
    match held {
        Ok(held) => {
            let found = recent(&held.backups, &held.restores, &period, usize::MAX);
            page.total = found.len();
            page.rows = found.into_iter().take(LISTED).collect();
        }
        Err(refusal) => page.problem = Some(refusal.detail),
    }
    drawn(&page)
}

fn written(value: Option<f64>, as_time: bool) -> String {
    match (value, as_time) {
        (None, _) => String::new(),
        (Some(seconds), true) => metrics::duration(seconds),
        (Some(percent), false) => format!("{percent}"),
    }
}

/// The form that sets how a service is watched; with `posted`, saving it first.
async fn edit(backend: &Backend, name: &str, posted: Option<Vec<(String, String)>>) -> Response {
    let definitions = Definitions::read(&backend.settings());
    let name = name.to_string();
    let mut page = Edit {
        frame: Frame::new(backend, "edit", Scope::Service(name.clone()), false, a_month()),
        name: name.clone(),
        problem: None,
        saved: false,
        url: String::new(),
        sla: String::new(),
        rto: String::new(),
        rpo: String::new(),
        mttr: String::new(),
        defaults: definitions.services,
        checks_on: backend.feature(SERVICES),
    };
    if let Some(posted) = posted {
        let text = |key: &str| field(&posted, key).map(serde_json::Value::String);
        let asked = Configured {
            url: field(&posted, "url"),
            sla: text("sla"),
            rto: text("rto"),
            rpo: text("rpo"),
            mttr: text("mttr"),
        };
        (page.url, page.sla, page.rto, page.rpo, page.mttr) = (
            field(&posted, "url").unwrap_or_default(),
            field(&posted, "sla").unwrap_or_default(),
            field(&posted, "rto").unwrap_or_default(),
            field(&posted, "rpo").unwrap_or_default(),
            field(&posted, "mttr").unwrap_or_default(),
        );
        let stop = field(&posted, "stop").is_some();
        let done = match stop {
            true => api::stop(backend, &name).await,
            false => api::configure(backend, &name, asked).await,
        };
        match done {
            Ok(_) if stop => {
                (page.url, page.sla, page.rto, page.rpo, page.mttr) = Default::default();
                page.saved = true;
            }
            Ok(_) => page.saved = true,
            Err(refusal) => page.problem = Some(refusal.detail),
        }
        return drawn(&page);
    }
    if let Err(refusal) = crate::scope::visible(backend, &name).await {
        page.problem = Some(refusal.detail);
        return drawn(&page);
    }
    let key = view::key(&name);
    match backend.get::<Subject>("subjects", key.as_str()).await {
        Ok(Some(subject)) => {
            page.url = subject.url.unwrap_or_default();
            page.sla = written(subject.sla, false);
            page.rto = written(subject.rto, true);
            page.rpo = written(subject.rpo, true);
            page.mttr = written(subject.mttr, true);
        }
        Ok(None) => {}
        Err(err) => page.problem = Some(err.to_string()),
    }
    drawn(&page)
}

/// The Reliability panel on a service's, team's or organisation's page in the Catalogue: the
/// four tiles for the last 30 days, and the way to the rest.
/// The four objectives, by the name each is pinned under.
pub const INSIGHTS: [(&str, &str); 4] = [
    ("availability", "Availability"),
    ("mttr", "Mean time to recover"),
    ("rto", "Recovery time"),
    ("rpo", "Recovery point"),
];

#[derive(Page)]
#[template(path = "insight.html")]
struct InsightTile {
    label: String,
    value: String,
    note: String,
    more: String,
}

/// One of the four, for pinning to the top of a resource's page. Only a service has one of its
/// own: a team's or an organisation's is a summary of everything under it, which is a panel's job
/// rather than a single number's.
async fn insight(backend: &Backend, which: &str, query: &str) -> Response {
    let Some((_, label)) = INSIGHTS.iter().find(|(id, _)| *id == which) else {
        return Response::not_found();
    };
    let mut tile = InsightTile {
        label: (*label).to_string(),
        value: "—".into(),
        note: String::new(),
        more: "/p/reliability/".into(),
    };
    let scope = match Scope::from_query(query) {
        Ok(Scope::All) | Err(_) => {
            tile.note = "Nothing was named to measure".into();
            return drawn(&tile);
        }
        Ok(scope) => scope,
    };
    tile.more = format!("/p/reliability/?{}", scope.query());
    if !backend.feature(SERVICES) && !faux::on(backend) {
        tile.note = "Service reliability is turned off".into();
        return drawn(&tile);
    }
    let period = Period::last(DEFAULT_DAYS);
    match view::services(backend, &scope, &period).await {
        Ok(services) => {
            let watched = services
                .judged
                .iter()
                .find(|judged| services.held.subjects.contains_key(&judged.subject));
            match watched {
                None => {
                    tile.note = "Nothing watches it yet".into();
                }
                Some(judged) => {
                    let tiles = metrics::tiles(judged, &period);
                    if let Some(found) = tiles.into_iter().find(|tile| tile.label == *label) {
                        tile.value = found.value;
                        tile.note = found.note;
                    }
                }
            }
        }
        Err(refusal) => tile.note = refusal.detail,
    }
    drawn(&tile)
}

async fn panel(backend: &Backend, query: &str) -> Response {
    let scope = match Scope::from_query(query) {
        Ok(Scope::All) => return Refusal::bad("name the resource the panel is on").response(),
        Ok(scope) => scope,
        Err(refusal) => return refusal.response(),
    };
    let period = Period::last(DEFAULT_DAYS);
    let more = format!("/p/reliability/?{}", scope.query());
    let mut panel = Panel { message: None, tiles: Vec::new(), more, edit: None };
    if !backend.feature(SERVICES) && !faux::on(backend) {
        panel.message = Some("Service reliability is turned off.".into());
        return drawn(&panel);
    }
    match view::services(backend, &scope, &period).await {
        Ok(services) => {
            panel.edit = edit_href(&scope).filter(|_| backend.writes());
            let watched: Vec<Judged> = services
                .judged
                .iter()
                .filter(|j| services.held.subjects.contains_key(&j.subject))
                .cloned()
                .collect();
            match (&scope, watched.as_slice()) {
                (_, []) => {
                    panel.message = Some(format!(
                        "Nothing watches {} yet: give it a health URL, or report its outages and backups.",
                        scope.label()
                    ));
                }
                (Scope::Service(_), [judged]) => panel.tiles = metrics::tiles(judged, &period),
                _ => panel.tiles = metrics::group_tiles(&watched, &period),
            }
        }
        Err(refusal) => panel.message = Some(refusal.detail),
    }
    drawn(&panel)
}
