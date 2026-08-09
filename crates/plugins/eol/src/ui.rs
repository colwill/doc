//! The pages at `/p/eol/…`: what every service, a team's, an organisation's or one service runs and
//! where each release stands; every product in use; every product endoflife.date tracks; one
//! product's whole lifecycle; and the End of life panel on the Catalogue's pages.

use std::collections::{BTreeMap, BTreeSet};

use askama::Template as Page;
use doc_plugin_sdk::{Backend, Request, Response};

use crate::lifecycle::{self, Status, day};
use crate::scope::{self, Member, Scope};
use crate::settings::LIFECYCLE;
use crate::store::Release;
use crate::timeline;
use crate::view::{self, Judged, View};
use crate::{faux, parameter};

/// Rows on the panel, the worst.
const PANEL_ROWS: usize = 5;
/// Releases on a product's timeline, the newest.
const PRODUCT_LANES: usize = 12;

/// What every page shares: its tab, what it is about, and whether its data is faux.
pub struct Frame {
    pub tab: &'static str,
    pub heading: String,
    pub scope: Scope,
    pub faux: bool,
}

impl Frame {
    fn new(backend: &Backend, tab: &'static str, scope: Scope) -> Self {
        let heading = match (&scope, tab) {
            (_, "all") => "All products".to_string(),
            (Scope::All, "products") => "Products in use".to_string(),
            (Scope::All, _) => "End of life".to_string(),
            (scope, "products") => format!("Products {} runs", scope.label()),
            (scope, _) => format!("End of life for {}", scope.label()),
        };
        Self { tab, heading, scope, faux: faux::on(backend) }
    }

    fn link(&self, path: &str) -> String {
        match self.scope.query() {
            query if query.is_empty() => format!("/p/eol/{path}"),
            query => format!("/p/eol/{path}?{query}"),
        }
    }

    pub fn overview(&self) -> String {
        self.link("")
    }

    pub fn products(&self) -> String {
        self.link("products")
    }

    /// Every product endoflife.date tracks, which no scope narrows.
    pub fn all(&self) -> &'static str {
        "/p/eol/all"
    }

    pub fn catalogue(&self) -> Option<String> {
        self.scope.catalogue_href()
    }

    pub fn single(&self) -> bool {
        matches!(self.scope, Scope::Service(_))
    }
}

pub struct Tile {
    pub label: &'static str,
    pub value: String,
    pub badge: Option<&'static str>,
    pub note: String,
}

/// One release in use, and the services that run it.
pub struct InUse {
    pub named: String,
    pub href: String,
    pub status: Status,
    pub why: String,
    pub support: String,
    pub eol: String,
    pub services: Vec<(String, String)>,
}

/// A service and what it runs, in a comparison.
pub struct ByService {
    pub title: String,
    pub href: String,
    pub runs: Vec<(String, Status)>,
    pub worst: Option<Status>,
    pub problems: Vec<String>,
}

#[derive(Page)]
#[template(path = "home.html")]
struct Home {
    frame: Frame,
    problem: Option<String>,
    onboarding: Option<bool>,
    about: String,
    tiles: Vec<Tile>,
    timeline: Option<String>,
    in_use: Vec<InUse>,
    services: Vec<ByService>,
    runs: Vec<Judged>,
    problems: Vec<String>,
    unnamed: Vec<String>,
    /// The repositories read for this service, as the Catalogue names them.
    repositories: Vec<String>,
    /// Whether runtimes are read from repositories' files, so the pages can say what is missing
    /// when they are not.
    reading: bool,
    /// How many repositories Repository Insights has listed the packages of, or nothing where it
    /// is not running; asked only when there is nothing to show, to say why.
    scanned: Option<usize>,
    /// What was found in each repository connected to these services that turned up nothing
    /// endoflife.date tracks, one sentence each: they are connected, and read, and say so.
    found_nothing: Vec<String>,
}

/// A product in use, with how many services run it and the worst of its releases in use.
pub struct Used {
    pub label: String,
    pub href: String,
    pub services: usize,
    pub releases: String,
    pub worst: Status,
    pub problem: Option<String>,
}

#[derive(Page)]
#[template(path = "products.html")]
struct Products {
    frame: Frame,
    problem: Option<String>,
    rows: Vec<Used>,
}

pub struct ReleaseRow {
    pub name: String,
    pub released: String,
    pub support: String,
    pub eol: String,
    pub extended: String,
    pub latest: String,
    pub status: Status,
    pub used_by: Vec<(String, String)>,
}

#[derive(Page)]
#[template(path = "product.html")]
struct ProductPage {
    frame: Frame,
    problem: Option<String>,
    label: String,
    link: Option<String>,
    read: String,
    read_problem: Option<String>,
    /// endoflife.date no longer lists it, and this is what it said when it last did.
    dropped: bool,
    timeline: Option<String>,
    rows: Vec<ReleaseRow>,
}

/// A product endoflife.date tracks, on the page of every one.
pub struct Tracked {
    pub label: String,
    pub href: String,
    pub kind: String,
    pub newest: String,
    pub released: String,
    pub before_eol: String,
    pub dropped: bool,
}

pub struct KindChoice {
    pub value: String,
    pub label: String,
    pub chosen: bool,
}

#[derive(Page)]
#[template(path = "all.html")]
struct All {
    frame: Frame,
    lede: String,
    /// Why the copy could not be read last time, or why it never has been.
    unread: Option<String>,
    q: String,
    kinds: Vec<KindChoice>,
    rows: Vec<Tracked>,
    shown: String,
    platform_url: String,
}

#[derive(Page)]
#[template(path = "panel.html")]
struct Panel {
    /// A service's own panel, which need not name the service on each row.
    single: bool,
    message: Option<String>,
    tiles: Vec<Tile>,
    rows: Vec<Judged>,
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

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/');
    let query = request.query.as_str();
    match (request.method.as_str(), path) {
        ("GET", "ui") => overview(backend, query).await,
        ("GET", "ui/products") => products(backend, query).await,
        ("GET", "ui/all") => all(backend, query).await,
        ("GET", "ui/product") => product(backend, query).await,
        ("GET", "ui/panel") => panel(backend, query).await,
        ("GET", path) if path.starts_with("ui/insight/") => {
            insight(backend, path.trim_start_matches("ui/insight/"), query).await
        }
        _ => Response::not_found(),
    }
}

/// A link to somewhere outside DOC, only ever to the web: what a lifecycle file says is not
/// trusted to be one.
pub fn web(url: Option<&str>) -> Option<String> {
    url.filter(|url| url.starts_with("https://") || url.starts_with("http://")).map(str::to_string)
}

fn service_href(name: &str) -> String {
    format!("/p/eol/?{}", scope::encoded(&[("service", name)]))
}

fn dated(date: Option<chrono::NaiveDate>) -> String {
    date.map_or_else(|| "–".to_string(), day)
}

/// How many uses stand where, as tiles: each counts the products run, and says how many services.
pub fn tiles(judged: &[Judged], warn_days: i64) -> Vec<Tile> {
    let months = (warn_days as f64 / 30.4).round() as i64;
    Status::ALL
        .into_iter()
        .map(|status| {
            let these: Vec<&Judged> = judged.iter().filter(|j| j.status == status).collect();
            let mut services: Vec<&str> = these.iter().map(|j| j.service.as_str()).collect();
            services.sort_unstable();
            services.dedup();
            let note = match (status, services.len()) {
                (_, 0) => "None".to_string(),
                (Status::Ending, count) => {
                    format!("Within {months} months, in {}", view_services(count))
                }
                (_, count) => format!("In {}", view_services(count)),
            };
            Tile {
                label: status.word(),
                value: these.len().to_string(),
                badge: (!these.is_empty()).then(|| status.badge()),
                note,
            }
        })
        .collect()
}

fn view_services(count: usize) -> String {
    match count {
        1 => "1 service".to_string(),
        count => format!("{count} services"),
    }
}

/// The releases in use, each once, with the services that run it, worst first.
fn in_use(judged: &[Judged]) -> Vec<InUse> {
    let mut grouped: BTreeMap<(Status, String, String), InUse> = BTreeMap::new();
    for each in judged {
        let named = each.named();
        let entry = grouped
            .entry((each.status, each.product.clone(), named.clone()))
            .or_insert_with(|| InUse {
                named,
                href: each.href(),
                status: each.status,
                why: each.why.clone(),
                support: dated(each.release.as_ref().and_then(|r| r.support_ends)),
                eol: dated(each.release.as_ref().and_then(|r| r.eol)),
                services: Vec::new(),
            });
        let link = (each.title.clone(), service_href(&each.service));
        if !entry.services.contains(&link) {
            entry.services.push(link);
        }
    }
    grouped.into_values().collect()
}

fn by_service(view: &View) -> Vec<ByService> {
    view.members
        .iter()
        .filter(|member| !member.used.is_empty() || !member.problems.is_empty())
        .map(|member| {
            let theirs = view.of(&member.name);
            ByService {
                title: member.title.clone(),
                href: service_href(&member.name),
                runs: theirs.iter().map(|judged| (judged.named(), judged.status)).collect(),
                worst: theirs.iter().map(|judged| judged.status).min(),
                problems: member.problems.clone(),
            }
        })
        .collect()
}

/// The releases in use on a timeline, each once.
fn drawn_timeline(judged: &[Judged], today: chrono::NaiveDate, described: &str) -> Option<String> {
    let mut seen = BTreeMap::new();
    for each in judged {
        if let Some(release) = &each.release {
            seen.entry(each.named()).or_insert((each.href(), release));
        }
    }
    if seen.is_empty() {
        return None;
    }
    let releases: Vec<&Release> = seen.values().map(|(_, release)| *release).collect();
    let window = view::window(&releases, today);
    let lanes: Vec<timeline::Lane> = seen
        .into_iter()
        .map(|(named, (href, release))| view::lane(named, Some(href), release, today, true))
        .collect();
    Some(timeline::drawn(&lanes, window, today, described))
}

fn about(scope: &Scope, view: &View, faux: bool) -> String {
    if faux {
        return format!(
            "Faux products and release cycles for {}, made up by {}: nothing here is read from \
             the Catalogue's metadata or endoflife.date.",
            scope.label(),
            faux::PROVIDER
        );
    }
    let how = format!(
        "read from the repositories connected to it in the Catalogue and from its {} metadata, \
         with release cycles from endoflife.date",
        scope::PRODUCTS,
    );
    if matches!(scope, Scope::Service(_)) {
        return format!("What it runs is {how}.");
    }
    let known = view.members.iter().filter(|member| !member.used.is_empty()).count();
    let discovered = view.members.iter().filter(|member| member.discovered()).count();
    format!(
        "What each service runs is {how}. What {known} of {} {} is known, {discovered} of them \
         from their repositories.",
        view_services(view.members.len()),
        if known == 1 { "runs" } else { "run" },
    )
}

async fn overview(backend: &Backend, query: &str) -> Response {
    let scope = match Scope::from_query(query) {
        Ok(scope) => scope,
        Err(refusal) => return refusal.response(),
    };
    let frame = Frame::new(backend, "overview", scope);
    let mut page = Home {
        problem: None,
        onboarding: None,
        about: String::new(),
        tiles: Vec::new(),
        timeline: None,
        in_use: Vec::new(),
        services: Vec::new(),
        runs: Vec::new(),
        problems: Vec::new(),
        unnamed: Vec::new(),
        repositories: Vec::new(),
        reading: backend.feature(crate::settings::REPOSITORIES),
        scanned: None,
        found_nothing: Vec::new(),
        frame,
    };
    let view = match view::read(backend, &page.frame.scope).await {
        Ok(view) => view,
        Err(refusal) => {
            page.problem = Some(refusal.detail);
            return drawn(&page);
        }
    };
    if !view.anything() && view.members.iter().all(|member| member.problems.is_empty()) {
        page.onboarding = Some(backend.feature(LIFECYCLE));
        page.found_nothing = found_nothing(backend, &view.members).await;
        page.scanned = match crate::packages::scanning(backend).await {
            crate::packages::Scanning::Absent => None,
            crate::packages::Scanning::Listed(count) => Some(count),
        };
        return drawn(&page);
    }
    let warn_days = crate::settings::Definitions::read(&backend.settings()).warn_days;
    page.about = about(&page.frame.scope, &view, page.frame.faux);
    page.tiles = tiles(&view.judged, warn_days);
    page.timeline = drawn_timeline(
        &view.judged,
        view.today,
        "Each release in use, from its release to its end of life: active support, then security fixes only, then any extended support",
    );
    if page.frame.single() {
        page.runs = view.judged.clone();
        page.problems = view.members.iter().flat_map(|member| member.problems.clone()).collect();
        page.repositories =
            view.members.iter().flat_map(|member| member.repositories.clone()).collect();
    } else {
        page.in_use = in_use(&view.judged);
        page.services = by_service(&view);
        let unknown: Vec<Member> = view
            .members
            .iter()
            .filter(|member| member.used.is_empty() && member.problems.is_empty())
            .cloned()
            .collect();
        // A service whose repository is connected and read is not asked to connect one: what was
        // found in it is said instead.
        page.found_nothing = found_nothing(backend, &unknown).await;
        let connected: BTreeSet<String> = crate::repositories::connected(backend, None)
            .await
            .into_iter()
            .flat_map(|held| held.services)
            .collect();
        page.unnamed = unknown
            .iter()
            .filter(|member| !connected.contains(&member.name))
            .map(|member| member.title.clone())
            .collect();
    }
    drawn(&page)
}

async fn products(backend: &Backend, query: &str) -> Response {
    let scope = match Scope::from_query(query) {
        Ok(scope) => scope,
        Err(refusal) => return refusal.response(),
    };
    let frame = Frame::new(backend, "products", scope);
    let view = match view::read(backend, &frame.scope).await {
        Ok(view) => view,
        Err(refusal) => {
            return drawn(&Products { frame, problem: Some(refusal.detail), rows: Vec::new() });
        }
    };
    let mut grouped: BTreeMap<String, Vec<&Judged>> = BTreeMap::new();
    for each in &view.judged {
        grouped.entry(each.product.clone()).or_default().push(each);
    }
    let mut rows: Vec<Used> = grouped
        .into_iter()
        .map(|(product, uses)| {
            let mut services: Vec<&str> = uses.iter().map(|each| each.service.as_str()).collect();
            services.sort_unstable();
            services.dedup();
            let mut releases: Vec<String> = uses
                .iter()
                .map(|each| {
                    each.release
                        .as_ref()
                        .map(|release| release.name.clone())
                        .or_else(|| each.version.clone())
                        .unwrap_or_else(|| "no version".into())
                })
                .collect();
            releases.sort_unstable();
            releases.dedup();
            Used {
                label: uses[0].label.clone(),
                href: uses[0].href(),
                services: services.len(),
                releases: releases.join(", "),
                worst: uses.iter().map(|each| each.status).min().unwrap_or(Status::Unknown),
                problem: view.products.get(&product).and_then(|product| product.problem.clone()),
            }
        })
        .collect();
    rows.sort_by(|one, two| (one.worst, &one.label).cmp(&(two.worst, &two.label)));
    drawn(&Products { frame, problem: None, rows })
}

/// endoflife.date's categories in words: one of them, and several.
fn kind_words(category: &str) -> (String, String) {
    let (one, several) = match category {
        "lang" => ("Language", "Languages"),
        "framework" => ("Framework", "Frameworks"),
        "database" => ("Database", "Databases"),
        "os" => ("Operating system", "Operating systems"),
        "app" => ("Application", "Applications"),
        "server-app" => ("Server software", "Server software"),
        "service" => ("Cloud service", "Cloud services"),
        "device" => ("Device", "Devices"),
        "standard" => ("Standard", "Standards"),
        other => {
            let mut letters = other.chars();
            let word = letters
                .next()
                .map(|first| first.to_uppercase().chain(letters).collect::<String>())
                .unwrap_or_default();
            return (word.clone(), word);
        }
    };
    (one.to_string(), several.to_string())
}

/// Where this DOC is, for the example on the page; the development address where none is given.
fn platform_url() -> String {
    std::env::var("DOC_PUBLIC_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string())
}

/// Every product endoflife.date tracks, as DOC's copy of it holds them, narrowed by a search and
/// a kind: for teams to look up what they are choosing, not only what is already run.
async fn all(backend: &Backend, query: &str) -> Response {
    let q = parameter(query, "q").unwrap_or_default();
    let kind = parameter(query, "kind").unwrap_or_default();
    let mut page = All {
        frame: Frame::new(backend, "all", Scope::All),
        lede: String::new(),
        unread: None,
        q: q.clone(),
        kinds: Vec::new(),
        rows: Vec::new(),
        shown: String::new(),
        platform_url: platform_url(),
    };
    let every = match crate::mirror::every(backend).await {
        Ok(every) => every,
        Err(refusal) => {
            page.unread = Some(refusal.detail);
            return drawn(&page);
        }
    };
    let copy = crate::mirror::held(backend).await;
    let when = |at: chrono::DateTime<chrono::Utc>| at.format("%-d %b %Y, %H:%M").to_string();
    page.lede = format!(
        "Every product endoflife.date tracks — {} languages, frameworks, databases, operating \
         systems and more — with each one's newest release and how many of its releases have not \
         reached their end of life. DOC keeps its own copy, read again each night in one request, \
         so nothing here waits on endoflife.date or stops when it cannot be reached.{}",
        every.iter().filter(|product| !product.dropped()).count(),
        match copy.read_at {
            Some(at) if !page.frame.faux => format!(" The copy was read on {}.", when(at)),
            _ => String::new(),
        }
    );
    if !page.frame.faux {
        page.unread = match (&copy.problem, copy.read_at, copy.tried_at) {
            (Some(problem), Some(_), Some(tried)) => Some(format!(
                "The last read, on {}, failed, so this is the copy read before: {problem}",
                when(tried)
            )),
            (Some(problem), None, _) => {
                Some(format!("endoflife.date has not been read yet: {problem}"))
            }
            (None, None, _) => Some(
                "endoflife.date has not been read yet. It is read when End of life loads, and \
                 each night."
                    .into(),
            ),
            _ => None,
        };
    }
    let mut kinds: Vec<(String, String)> = every
        .iter()
        .filter_map(|product| product.category.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|category| {
            let (_, several) = kind_words(&category);
            (several, category)
        })
        .collect();
    kinds.sort();
    page.kinds = kinds
        .into_iter()
        .map(|(label, value)| KindChoice { chosen: value == kind, value, label })
        .collect();
    let wanted = q.to_ascii_lowercase();
    let today = lifecycle::today();
    page.rows = every
        .iter()
        .filter(|product| kind.is_empty() || product.category.as_deref() == Some(kind.as_str()))
        .filter(|product| {
            wanted.is_empty()
                || product.product.contains(&wanted)
                || product.label.to_ascii_lowercase().contains(&wanted)
                || product.aliases.iter().any(|alias| alias.to_ascii_lowercase().contains(&wanted))
        })
        .map(|product| {
            let newest = product.releases.first();
            let before_eol =
                product.releases.iter().filter(|release| !release.ended_by(today)).count();
            Tracked {
                label: product.label.clone(),
                href: format!("/p/eol/product?{}", scope::encoded(&[("name", &product.product)])),
                kind: product.category.as_deref().map_or_else(|| "–".into(), |c| kind_words(c).0),
                newest: newest.map_or_else(|| "–".into(), Release::title),
                released: dated(newest.and_then(|release| release.released)),
                before_eol: format!("{before_eol} of {}", product.releases.len()),
                dropped: product.dropped(),
            }
        })
        .collect();
    page.shown = match (page.rows.len(), q.is_empty() && kind.is_empty()) {
        (_, true) => String::new(),
        (0, false) => "Nothing matches that.".into(),
        (1, false) => "1 product matches.".into(),
        (count, false) => format!("{count} products match."),
    };
    drawn(&page)
}

async fn product(backend: &Backend, query: &str) -> Response {
    let frame = Frame::new(backend, "product", Scope::All);
    let mut page = ProductPage {
        frame,
        problem: None,
        label: String::new(),
        link: None,
        read: String::new(),
        read_problem: None,
        dropped: false,
        timeline: None,
        rows: Vec::new(),
    };
    let Some(name) = parameter(query, "name") else {
        page.problem = Some("Name a product, such as ?name=nodejs.".into());
        return drawn(&page);
    };
    let found = match crate::api::lifecycle_of(backend, &name).await {
        Ok(found) => found,
        Err(refusal) => {
            page.problem = Some(refusal.detail);
            return drawn(&page);
        }
    };
    let (product, used_by) = found;
    // A product nothing runs is one of all of them, not one in use.
    if used_by.is_empty() {
        page.frame.tab = "all";
    }
    let today = lifecycle::today();
    let warn_days = crate::settings::Definitions::read(&backend.settings()).warn_days;
    page.frame.heading = product.label.clone();
    page.label = product.label.clone();
    page.link = web(product.link.as_deref());
    let when = |at: chrono::DateTime<chrono::Utc>| at.format("%-d %b %Y, %H:%M").to_string();
    page.dropped = product.dropped();
    if lifecycle::is_custom(&product.product) || page.frame.faux {
        page.read = product.read_at.map(when).unwrap_or_default();
        page.read_problem = product.problem.clone();
    } else {
        // endoflife.date's products are as current as the copy, whenever each last changed.
        let copy = crate::mirror::held(backend).await;
        page.read = copy.read_at.or(product.read_at).map(when).unwrap_or_default();
        page.read_problem = copy.problem.or_else(|| product.problem.clone());
    }
    let shown: Vec<&Release> = product.releases.iter().take(PRODUCT_LANES).collect();
    if !shown.is_empty() {
        let window = view::window(&shown, today);
        let lanes: Vec<timeline::Lane> = shown
            .iter()
            .map(|release| view::lane(release.title(), None, release, today, false))
            .collect();
        let described =
            format!("Each release of {}, from its release to its end of life", product.label);
        page.timeline = Some(timeline::drawn(&lanes, window, today, &described));
    }
    page.rows = product
        .releases
        .iter()
        .map(|release| ReleaseRow {
            name: release.title(),
            released: dated(release.released),
            support: match (release.support_ends, release.support_ended) {
                (Some(ends), _) => day(ends),
                (None, true) => "Ended".into(),
                (None, false) => "–".into(),
            },
            eol: match (release.eol, release.ended) {
                (Some(eol), _) => day(eol),
                (None, true) => "Ended".into(),
                (None, false) => "Not announced".into(),
            },
            extended: dated(release.extended_ends),
            latest: release.latest.clone().unwrap_or_else(|| "–".into()),
            status: release.status(today, warn_days),
            used_by: used_by
                .iter()
                .filter(|judged| judged.release.as_ref().is_some_and(|r| r.name == release.name))
                .map(|judged| (judged.title.clone(), service_href(&judged.service)))
                .collect(),
        })
        .collect();
    drawn(&page)
}

/// What may be pinned: how much of what it runs is past its end of life, or nearly there. The
/// other statuses are counts of things that are fine, which is not what anybody pins.
pub const INSIGHTS: [(&str, Status); 2] = [("ended", Status::Ended), ("ending", Status::Ending)];

#[derive(Page)]
#[template(path = "insight.html")]
struct InsightTile {
    label: String,
    value: String,
    note: String,
    more: String,
}

/// One status's count, for pinning to the top of a resource's page: how many of the products it
/// runs are there, and in how many of its services.
async fn insight(backend: &Backend, which: &str, query: &str) -> Response {
    let Some((_, status)) = INSIGHTS.iter().find(|(id, _)| *id == which) else {
        return Response::not_found();
    };
    let mut tile = InsightTile {
        label: status.word().to_string(),
        value: "—".into(),
        note: String::new(),
        more: "/p/eol/".into(),
    };
    let scope = match Scope::from_query(query) {
        Ok(scope) => scope,
        Err(_) => {
            tile.note = "Nothing was named to look at".into();
            return drawn(&tile);
        }
    };
    tile.more = match scope.query() {
        query if query.is_empty() => "/p/eol/".to_string(),
        query => format!("/p/eol/?{query}"),
    };
    match view::read(backend, &scope).await {
        Err(refusal) => tile.note = refusal.detail,
        Ok(view) if view.judged.is_empty() => {
            tile.note = "What it runs is not known yet".into();
        }
        Ok(view) => {
            let warn_days = crate::settings::Definitions::read(&backend.settings()).warn_days;
            let tiles = tiles(&view.judged, warn_days);
            if let Some(found) = tiles.into_iter().find(|tile| tile.label == status.word()) {
                tile.value = found.value;
                tile.note = found.note;
            }
        }
    }
    drawn(&tile)
}

/// A sentence for each repository connected to one of `members` that was read and turned up
/// nothing endoflife.date tracks.
async fn found_nothing(backend: &Backend, members: &[Member]) -> Vec<String> {
    let files = backend.feature(crate::settings::REPOSITORIES);
    let named: BTreeSet<&str> = members.iter().map(|member| member.name.as_str()).collect();
    crate::repositories::connected(backend, None)
        .await
        .iter()
        .filter(|held| held.services.iter().any(|service| named.contains(service.as_str())))
        .map(|held| crate::repositories::nothing_found(held, files))
        .collect()
}

async fn panel(backend: &Backend, query: &str) -> Response {
    let scope = match Scope::from_query(query) {
        Ok(scope) => scope,
        Err(refusal) => return refusal.response(),
    };
    let more = match scope.query() {
        query if query.is_empty() => "/p/eol/".to_string(),
        query => format!("/p/eol/?{query}"),
    };
    let single = matches!(scope, Scope::Service(_));
    let mut page = Panel { single, message: None, tiles: Vec::new(), rows: Vec::new(), more };
    match view::read(backend, &scope).await {
        Err(refusal) => page.message = Some(refusal.detail),
        Ok(view) if view.judged.is_empty() => {
            let found = found_nothing(backend, &view.members).await;
            page.message = Some(match scope {
                _ if !found.is_empty() => format!(
                    "{} Name what it runs in its {} metadata, such as rust@1.80,postgresql@16.",
                    found.join(" "),
                    scope::PRODUCTS
                ),
                Scope::Service(_) => format!(
                    "What this service runs is not known yet. Connect it to its repository in \
                     the Catalogue and what that uses is read once Repository Insights has \
                     scanned it, or name what it runs in its {} metadata, such as \
                     nodejs@20,postgresql@16.",
                    scope::PRODUCTS
                ),
                _ => "What its services run is not known yet: none is connected to a repository \
                      Repository Insights has scanned, or names what it runs in its metadata."
                    .into(),
            });
        }
        Ok(view) => {
            let warn_days = crate::settings::Definitions::read(&backend.settings()).warn_days;
            page.tiles = tiles(&view.judged, warn_days)
                .into_iter()
                .filter(|tile| tile.value != "0" || tile.label == Status::Ended.word())
                .collect();
            page.rows = view
                .judged
                .into_iter()
                .filter(|judged| judged.status != Status::Supported)
                .take(PANEL_ROWS)
                .collect();
        }
    }
    drawn(&page)
}
