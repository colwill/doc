//! The pages at `/p/dora/…`: the four metrics for everything, a service, a team or an
//! organisation, with their trends; the deployments and failures behind every figure; teams side
//! by side, when that is turned on; and the Delivery panel on the Catalogue's pages. A metric is
//! never shown for a person.

use std::collections::{BTreeMap, BTreeSet};

use askama::Template as Page;
use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Order, Query, Request, Response};
use serde_json::{Value, json};

use crate::metrics::{self, Band, Chart, Figures, Period, Tile};
use crate::scope::{self, Member, Scope};
use crate::settings::{COMPARE, Definitions, METRICS};
use crate::store::{Counted, Deployment};
use crate::{Refusal, faux, parameter};

pub const PERIODS: [(i64, &str); 5] =
    [(7, "7 days"), (30, "30 days"), (90, "90 days"), (180, "6 months"), (365, "year")];
const DEFAULT_DAYS: i64 = 30;
pub const MAX_DAYS: i64 = 400;
/// The most deployments one listing shows; the API pages through the rest.
const LISTED: usize = 200;
/// How long a source is given to say how its delivery data stands.
const ASKING: std::time::Duration = std::time::Duration::from_secs(5);

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

/// What every page shares: its tab, what it is about and over how long, and links that keep them.
pub struct Frame {
    pub tab: &'static str,
    pub heading: String,
    pub scope: Scope,
    pub days: i64,
    pub compare: bool,
    pub periods: Vec<Choice>,
    /// Faux data from `faux-data`, which the platform says at the top of every page.
    pub faux: bool,
}

/// One of the periods a page can be shown over.
pub struct Choice {
    pub days: i64,
    pub said: &'static str,
    pub chosen: bool,
}

impl Frame {
    fn new(backend: &Backend, tab: &'static str, scope: Scope, days: i64) -> Self {
        let heading = match (&scope, tab) {
            (Scope::All, "failures") => "Failed deployments".to_string(),
            (Scope::All, "deployments") => "Deployments".to_string(),
            (Scope::All, "teams") => "Teams side by side".to_string(),
            (Scope::All, _) => "DORA metrics".to_string(),
            (scope, "failures") => format!("Failed deployments of {}", scope.label()),
            (scope, "deployments") => format!("Deployments of {}", scope.label()),
            (scope, _) => format!("Delivery of {}", scope.label()),
        };
        Self {
            tab,
            heading,
            scope,
            days,
            compare: backend.feature(COMPARE),
            faux: faux::on(backend),
            periods: PERIODS
                .iter()
                .map(|(each, said)| Choice { days: *each, said, chosen: *each == days })
                .collect(),
        }
    }

    fn link(&self, path: &str, extra: &str) -> String {
        let scoped = self.scope.query();
        let mut query = format!("days={}", self.days);
        if !scoped.is_empty() {
            query = format!("{scoped}&{query}");
        }
        format!("/p/dora/{path}?{query}{extra}")
    }

    pub fn overview(&self) -> String {
        self.link("", "")
    }

    pub fn deployments(&self) -> String {
        self.link("deployments", "")
    }

    pub fn failures(&self) -> String {
        self.link("deployments", "&failed=1")
    }

    pub fn teams(&self) -> String {
        format!("/p/dora/teams?days={}", self.days)
    }

    pub fn action(&self) -> String {
        match self.tab {
            "overview" => "/p/dora/".to_string(),
            "teams" => "/p/dora/teams".to_string(),
            _ => "/p/dora/deployments".to_string(),
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

    pub fn failures_only(&self) -> bool {
        self.tab == "failures"
    }
}

/// One source of delivery data and how it stands, for a page with nothing to show yet.
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

/// A service's or team's four figures in a row, each with its band.
pub struct Row {
    pub title: String,
    pub href: String,
    pub cells: Vec<Cell>,
}

pub struct Cell {
    pub value: String,
    pub band: Option<Band>,
}

#[derive(Page)]
#[template(path = "home.html")]
struct Home {
    frame: Frame,
    row_kind: &'static str,
    onboarding: Option<Onboarding>,
    problem: Option<String>,
    about: String,
    tiles: Vec<Tile>,
    charts: Vec<Chart>,
    signals: String,
    rows: Vec<Row>,
    rows_problem: Option<String>,
    unconnected: Vec<String>,
    counted: Vec<(String, i64)>,
}

pub struct Listed {
    pub at: String,
    pub repository: String,
    pub environment: String,
    pub sha: String,
    pub url: Option<String>,
    pub commits: i64,
    pub lead: String,
    pub failed: bool,
    pub failure: String,
    pub failure_url: Option<String>,
    pub recovery: String,
}

#[derive(Page)]
#[template(path = "deployments.html")]
struct Deployments {
    frame: Frame,
    problem: Option<String>,
    about: String,
    rows: Vec<Listed>,
    total: usize,
}

#[derive(Page)]
#[template(path = "teams.html")]
struct Teams {
    frame: Frame,
    row_kind: &'static str,
    problem: Option<String>,
    rows: Vec<Row>,
    unconnected: Vec<String>,
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

pub async fn handle(backend: &Backend, request: &Request) -> Response {
    let path = request.path.trim_end_matches('/');
    let query = request.query.as_str();
    match (request.method.as_str(), path) {
        ("GET", "ui") => overview(backend, query).await,
        ("GET", "ui/deployments") => deployments(backend, query).await,
        ("GET", "ui/teams") => teams(backend, query).await,
        ("GET", "ui/panel") => panel(backend, query).await,
        ("GET", path) if path.starts_with("ui/insight/") => {
            insight(backend, path.trim_start_matches("ui/insight/"), query).await
        }
        _ => Response::not_found(),
    }
}

/// A link to somewhere outside DOC, only ever to the web: what a source exports is not trusted
/// to be one.
fn web(url: Option<&str>) -> Option<String> {
    url.filter(|url| url.starts_with("https://") || url.starts_with("http://")).map(str::to_string)
}

fn when(at: DateTime<Utc>) -> String {
    at.format("%-d %b %Y, %H:%M").to_string()
}

/// Whether anything has been worked out yet, which decides between the metrics and what to do
/// to get some.
async fn anything(backend: &Backend) -> Result<bool, Refusal> {
    let found = backend.query::<Value>(Query::new("deployments").fields(&["id"]).limit(1)).await?;
    Ok(!found.records.is_empty())
}

/// How each running source's delivery data stands. Only a running one is asked, since a call to
/// a plugin with no process waits out its whole deadline, and none is waited on for long.
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
        let asked = backend.discovery(id, "GET", "delivery", None, None);
        let Ok(Ok((200, status))) = tokio::time::timeout(ASKING, asked).await else {
            let said = "It did not say how its delivery data stands.".to_string();
            found.push(Source { id: id.clone(), said, ready: false });
            continue;
        };
        let synced = status["synced"]["at"]
            .as_str()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| when(at.with_timezone(&Utc)));
        let (said, ready) = match (status["on"].as_bool(), status["problem"].as_str(), synced) {
            (Some(false), _, _) => ("Its Delivery data feature is off.".to_string(), false),
            (_, Some(problem), _) => (format!("It cannot read delivery data: {problem}."), false),
            (_, None, Some(at)) => (format!("Delivery data was last read in full on {at}."), true),
            (_, None, None) => {
                ("It is reading delivery data for the first time.".to_string(), true)
            }
        };
        found.push(Source { id: id.clone(), said, ready });
    }
    found
}

async fn onboarding(backend: &Backend, definitions: &Definitions) -> Onboarding {
    Onboarding {
        on: backend.feature(METRICS),
        admin: backend.caller().is_some_and(|caller| caller.admin),
        sources: sources(backend, definitions).await,
    }
}

/// What counts as a failure, in one sentence, and what is missing for a fuller picture.
fn signals(definitions: &Definitions) -> String {
    let mut counted = Vec::new();
    if definitions.reverts {
        counted.push("a revert".to_string());
    }
    if definitions.rollbacks {
        counted.push("a rollback".to_string());
    }
    if definitions.hotfixes {
        counted.push("a hotfix".to_string());
    }
    if !definitions.counters.is_empty() {
        counted.push(format!("a count of {}", definitions.counters.join(" or ")));
    }
    let window = metrics::duration(definitions.window_seconds as f64);
    let listed = match counted.as_slice() {
        [] => return "Nothing is counted as a failure: every failure signal is turned off.".into(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} or {last}", rest.join(", ")),
    };
    format!(
        "A deployment caused a failure when {listed} came within {window} after it. No incident \
         tracker is connected, so incidents are only those counted through the increment operation."
    )
}

/// The service or team rows of a comparison, from the deployments already read for the period.
fn rows(
    members: &[Member],
    deployments: &[Deployment],
    (period, days): (&Period, i64),
    definitions: &Definitions,
    kind: &str,
) -> (Vec<Row>, Vec<String>) {
    let mut rows = Vec::new();
    let mut unconnected = Vec::new();
    for member in members {
        if member.repositories.is_empty() {
            unconnected.push(member.name.clone());
            continue;
        }
        let theirs: Vec<Deployment> = deployments
            .iter()
            .filter(|deployment| member.repositories.contains(&deployment.repository))
            .cloned()
            .collect();
        let figures = metrics::figures(&theirs, period);
        let banded = figures.bands(&definitions.bands);
        let query = scope::encoded(&[(kind, &member.name), ("days", &days.to_string())]);
        let cell = |value: String, band: Option<Band>| Cell { value, band };
        rows.push(Row {
            title: member.title.clone(),
            href: format!("/p/dora/?{query}"),
            cells: vec![
                cell(metrics::frequency(&figures), banded.deployment_frequency),
                cell(lead(&figures), banded.lead_time),
                cell(
                    figures.change_fail_rate.map_or_else(|| "–".into(), metrics::percent),
                    banded.change_fail_rate,
                ),
                cell(recovery(&figures), banded.recovery),
            ],
        });
    }
    (rows, unconnected)
}

fn lead(figures: &Figures) -> String {
    figures.lead_time.map_or_else(|| "–".into(), |lead| metrics::duration(lead.median))
}

fn recovery(figures: &Figures) -> String {
    match (figures.recovery, figures.failed, figures.deployments) {
        (Some(recovery), _, _) => metrics::duration(recovery.median),
        (None, _, 0) => "–".into(),
        (None, 0, _) => "No failures".into(),
        (None, _, _) => "Not yet".into(),
    }
}

/// Where a scope's figures come from, in a sentence.
fn about(
    scope: &Scope,
    repositories: Option<&BTreeSet<String>>,
    definitions: &Definitions,
    faux: bool,
) -> String {
    if faux {
        let named = repositories
            .map(|repositories| repositories.iter().cloned().collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        return format!(
            "Faux deployments for {}, made up by {} for its stand-in repositories: {named}.",
            scope.label(),
            faux::PROVIDER
        );
    }
    let from = definitions.sources.join(" and ");
    match repositories {
        None => format!("Every production deployment read from {from}."),
        Some(repositories) if repositories.is_empty() => format!(
            "{} has no repository connected in the Catalogue, so there is nothing to measure. \
             Connect one there and its deployments count at once.",
            scope.label()
        ),
        Some(repositories) => {
            let named: Vec<&str> = repositories.iter().map(String::as_str).collect();
            let counted = match named.len() {
                1 => "the repository".to_string(),
                count => format!("the {count} repositories"),
            };
            format!(
                "From {counted} connected to {} in the Catalogue: {}.",
                scope.label(),
                named.join(", ")
            )
        }
    }
}

/// What was counted in the period, by counter: for a scope, what was counted against its
/// repositories or its own name.
async fn counted(
    backend: &Backend,
    scope: &Scope,
    repositories: Option<&BTreeSet<String>>,
    period: &Period,
) -> Result<Vec<(String, i64)>, Refusal> {
    let window = json!({ "gte": period.from, "lt": period.to });
    let mut found: BTreeMap<uuid::Uuid, Counted> = BTreeMap::new();
    if faux::on(backend) {
        let repositories = repositories.cloned().unwrap_or_default();
        let counted = faux::counted(backend, &repositories, period).await?;
        found.extend(counted.into_iter().map(|c| (c.id, c)));
        return Ok(totals(found));
    }
    let mut read = |counted: Vec<Counted>| {
        for count in counted {
            found.insert(count.id, count);
        }
    };
    match repositories {
        None => {
            read(backend.query_all(Query::new("counters").filter(json!({ "at": window }))).await?)
        }
        Some(repositories) => {
            if !repositories.is_empty() {
                let names: Vec<&String> = repositories.iter().collect();
                let filter = json!({ "repository": { "in": names }, "at": window });
                read(backend.query_all(Query::new("counters").filter(filter)).await?);
            }
            if let Some(name) = scope.name() {
                let filter = json!({ "service": name, "at": window });
                read(backend.query_all(Query::new("counters").filter(filter)).await?);
            }
        }
    }
    Ok(totals(found))
}

fn totals(found: BTreeMap<uuid::Uuid, Counted>) -> Vec<(String, i64)> {
    let mut totals: BTreeMap<String, i64> = BTreeMap::new();
    for count in found.into_values() {
        *totals.entry(count.counter).or_default() += count.amount;
    }
    totals.into_iter().collect()
}

async fn overview(backend: &Backend, query: &str) -> Response {
    let (scope, days) = match (Scope::from_query(query), days(query)) {
        (Ok(scope), Ok(days)) => (scope, days),
        (Err(refusal), _) | (_, Err(refusal)) => return refusal.response(),
    };
    let definitions = Definitions::read(&backend.settings());
    let frame = Frame::new(backend, "overview", scope, days);
    let mut home = Home {
        frame,
        row_kind: "Service",
        onboarding: None,
        problem: None,
        about: String::new(),
        tiles: Vec::new(),
        charts: Vec::new(),
        signals: signals(&definitions),
        rows: Vec::new(),
        rows_problem: None,
        unconnected: Vec::new(),
        counted: Vec::new(),
    };
    let ready =
        home.frame.faux || (backend.feature(METRICS) && anything(backend).await.unwrap_or(false));
    if !ready {
        home.onboarding = Some(onboarding(backend, &definitions).await);
        return drawn(&home);
    }
    let period = Period::last(days);
    let repositories = match scope::repositories(backend, &home.frame.scope).await {
        Ok(repositories) => repositories,
        Err(refusal) => {
            home.problem = Some(refusal.detail);
            return drawn(&home);
        }
    };
    let figured = async {
        let now = metrics::deployments(backend, repositories.as_ref(), &period).await?;
        let before = metrics::deployments(backend, repositories.as_ref(), &period.before()).await?;
        let counted = counted(backend, &home.frame.scope, repositories.as_ref(), &period).await?;
        Ok::<_, Refusal>((now, before, counted))
    };
    let (now, before, counted) = match figured.await {
        Ok(figured) => figured,
        Err(refusal) => {
            home.problem = Some(refusal.detail);
            return drawn(&home);
        }
    };
    home.about = about(&home.frame.scope, repositories.as_ref(), &definitions, home.frame.faux);
    home.tiles = metrics::tiles(
        &metrics::figures(&now, &period),
        &metrics::figures(&before, &period.before()),
        &period,
        &definitions.bands,
    );
    home.charts = metrics::charts(&now, &period);
    home.counted = counted;
    if home.frame.scope == Scope::All {
        match scope::every(backend, "service").await {
            Ok(members) => {
                (home.rows, home.unconnected) =
                    rows(&members, &now, (&period, days), &definitions, "service");
            }
            Err(refusal) => home.rows_problem = Some(refusal.detail),
        }
    }
    drawn(&home)
}

fn listed(deployment: &Deployment) -> Listed {
    Listed {
        at: when(deployment.deployed_at),
        repository: deployment.repository.clone(),
        environment: deployment.environment.clone(),
        sha: deployment.sha.chars().take(7).collect(),
        url: web(deployment.url.as_deref()),
        commits: deployment.commits,
        lead: deployment.lead_median.map_or_else(|| "–".into(), metrics::duration),
        failed: deployment.failed,
        failure: match deployment.failure.as_deref() {
            Some("revert") => "A revert".to_string(),
            Some("rollback") => "A rollback".to_string(),
            Some("hotfix") => "A hotfix".to_string(),
            Some(counter) => format!("Counted as {counter}"),
            None => String::new(),
        },
        failure_url: web(deployment.failure_url.as_deref()),
        recovery: match (deployment.failed, deployment.recovery_seconds) {
            (false, _) => String::new(),
            (true, Some(seconds)) => metrics::duration(seconds),
            (true, None) => "Not yet".to_string(),
        },
    }
}

async fn deployments(backend: &Backend, query: &str) -> Response {
    let (scope, days) = match (Scope::from_query(query), days(query)) {
        (Ok(scope), Ok(days)) => (scope, days),
        (Err(refusal), _) | (_, Err(refusal)) => return refusal.response(),
    };
    let failed_only = parameter(query, "failed").is_some_and(|value| value != "0");
    let definitions = Definitions::read(&backend.settings());
    let tab = if failed_only { "failures" } else { "deployments" };
    let mut page = Deployments {
        frame: Frame::new(backend, tab, scope, days),
        problem: None,
        about: String::new(),
        rows: Vec::new(),
        total: 0,
    };
    let period = Period::last(days);
    let found = async {
        let repositories = scope::repositories(backend, &page.frame.scope).await?;
        let found = metrics::deployments(backend, repositories.as_ref(), &period).await?;
        Ok::<_, Refusal>((repositories, found))
    };
    match found.await {
        Ok((repositories, found)) => {
            let mut found: Vec<Deployment> =
                found.into_iter().filter(|deployment| !failed_only || deployment.failed).collect();
            found.reverse();
            page.total = found.len();
            page.about =
                about(&page.frame.scope, repositories.as_ref(), &definitions, page.frame.faux);
            page.rows = found.iter().take(LISTED).map(listed).collect();
        }
        Err(refusal) => page.problem = Some(refusal.detail),
    }
    drawn(&page)
}

async fn teams(backend: &Backend, query: &str) -> Response {
    let days = match days(query) {
        Ok(days) => days,
        Err(refusal) => return refusal.response(),
    };
    let definitions = Definitions::read(&backend.settings());
    let mut page = Teams {
        frame: Frame::new(backend, "teams", Scope::All, days),
        row_kind: "Team",
        problem: None,
        rows: Vec::new(),
        unconnected: Vec::new(),
    };
    if !backend.feature(COMPARE) {
        page.problem = Some(
            "Comparing teams is turned off. It is turned on from this plugin's Features tab, \
             beside DORA's warning about it."
                .into(),
        );
        return drawn(&page);
    }
    let period = Period::last(days);
    let found = async {
        let members = scope::every(backend, "team").await?;
        let theirs: BTreeSet<String> =
            members.iter().flat_map(|member| member.repositories.iter().cloned()).collect();
        let deployments = metrics::deployments(backend, Some(&theirs), &period).await?;
        Ok::<_, Refusal>((members, deployments))
    };
    match found.await {
        Ok((members, deployments)) => {
            (page.rows, page.unconnected) =
                rows(&members, &deployments, (&period, days), &definitions, "team");
        }
        Err(refusal) => page.problem = Some(refusal.detail),
    }
    drawn(&page)
}

/// The Delivery panel on a service's, team's or organisation's page in the Catalogue: the four
/// tiles for the last 30 days, and the way to the rest.
/// The four metrics, by the name each is pinned under.
pub const INSIGHTS: [(&str, &str); 4] = [
    ("frequency", "Deployment frequency"),
    ("lead-time", "Change lead time"),
    ("fail-rate", "Change fail rate"),
    ("recovery", "Failed deployment recovery"),
];

#[derive(Page)]
#[template(path = "insight.html")]
struct InsightTile {
    label: String,
    value: String,
    note: String,
    more: String,
}

/// One of the four, for pinning to the top of a resource's page: the same figure the panel shows,
/// on its own. A metric that cannot be worked out yet says so in the tile rather than going
/// blank, because a tile with nothing in it reads as a fault rather than as nothing to measure.
async fn insight(backend: &Backend, which: &str, query: &str) -> Response {
    let Some((_, label)) = INSIGHTS.iter().find(|(id, _)| *id == which) else {
        return Response::not_found();
    };
    let mut tile = InsightTile {
        label: (*label).to_string(),
        value: "—".into(),
        note: String::new(),
        more: "/p/dora/".into(),
    };
    let scope = match Scope::from_query(query) {
        Ok(Scope::All) | Err(_) => {
            tile.note = "Nothing was named to measure".into();
            return drawn(&tile);
        }
        Ok(scope) => scope,
    };
    tile.more = format!("/p/dora/?{}", scope.query());
    if !backend.feature(METRICS) && !faux::on(backend) {
        tile.note = "DORA metrics are turned off".into();
        return drawn(&tile);
    }
    let period = Period::last(DEFAULT_DAYS);
    let definitions = Definitions::read(&backend.settings());
    let found = async {
        let repositories = scope::repositories(backend, &scope).await?;
        let now = metrics::deployments(backend, repositories.as_ref(), &period).await?;
        let before = metrics::deployments(backend, repositories.as_ref(), &period.before()).await?;
        Ok::<_, Refusal>((repositories, now, before))
    };
    match found.await {
        Ok((Some(repositories), _, _)) if repositories.is_empty() => {
            tile.note = "No repository is connected to it".into();
        }
        Ok((_, now, before)) => {
            let tiles = metrics::tiles(
                &metrics::figures(&now, &period),
                &metrics::figures(&before, &period.before()),
                &period,
                &definitions.bands,
            );
            if let Some(found) = tiles.into_iter().find(|tile| tile.label == *label) {
                tile.value = found.value;
                tile.note = found.note;
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
    let more = format!("/p/dora/?{}", scope.query());
    let mut panel = Panel { message: None, tiles: Vec::new(), more };
    if !backend.feature(METRICS) && !faux::on(backend) {
        panel.message = Some("DORA metrics are turned off.".into());
        return drawn(&panel);
    }
    let definitions = Definitions::read(&backend.settings());
    let found = async {
        let repositories = scope::repositories(backend, &scope).await?;
        let now = metrics::deployments(backend, repositories.as_ref(), &period).await?;
        let before = metrics::deployments(backend, repositories.as_ref(), &period.before()).await?;
        Ok::<_, Refusal>((repositories, now, before))
    };
    match found.await {
        Ok((Some(repositories), _, _)) if repositories.is_empty() => {
            panel.message = Some(format!(
                "{} has no repository connected in the Catalogue, so there is nothing to measure yet.",
                scope.label()
            ));
        }
        Ok((_, now, before)) => {
            panel.tiles = metrics::tiles(
                &metrics::figures(&now, &period),
                &metrics::figures(&before, &period.before()),
                &period,
                &definitions.bands,
            );
        }
        Err(refusal) => panel.message = Some(refusal.detail),
    }
    drawn(&panel)
}

/// The newest deployments first, for the API: those of a scope in a period.
pub async fn newest(
    backend: &Backend,
    repositories: Option<&BTreeSet<String>>,
    period: &Period,
    failed_only: bool,
    limit: usize,
) -> Result<Vec<Deployment>, Refusal> {
    if faux::on(backend) {
        let mut found = metrics::deployments(backend, repositories, period).await?;
        found.retain(|deployment| !failed_only || deployment.failed);
        found.reverse();
        found.truncate(limit);
        return Ok(found);
    }
    let mut filter = json!({ "deployed_at": { "gte": period.from, "lt": period.to } });
    if failed_only {
        filter["failed"] = json!(true);
    }
    if let Some(repositories) = repositories {
        if repositories.is_empty() {
            return Ok(Vec::new());
        }
        filter["repository"] = json!({ "in": repositories });
    }
    let asked = Query::new("deployments")
        .filter(filter)
        .order(Order::desc("deployed_at"))
        .limit(u32::try_from(limit).unwrap_or(u32::MAX));
    Ok(backend.query::<Deployment>(asked).await?.records)
}
