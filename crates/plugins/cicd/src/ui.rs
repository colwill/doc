//! The pages at `/p/cicd/…`: the four figures for everything, a service, a team or an
//! organisation, for every stage and each on its own, with their trends; every workflow, the runs
//! behind the figures and each time a default branch broke; and the Pipelines panel on the
//! Catalogue's pages.

use std::collections::BTreeSet;

use askama::Template as Page;
use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, Order, Query, Request, Response};
use serde_json::{Value, json};

use crate::metrics::{self, Chart, Counting, Held, Period, Row, Tile};
use crate::scope::{self, Member, Scope};
use crate::settings::{Definitions, METRICS, Stage};
use crate::store::{Recovery, Run};
use crate::{Refusal, faux, parameter};

pub const PERIODS: [(i64, &str); 5] =
    [(7, "7 days"), (30, "30 days"), (90, "90 days"), (180, "6 months"), (365, "year")];
const DEFAULT_DAYS: i64 = 30;
pub const MAX_DAYS: i64 = 400;
/// The most runs, spells or workflows one listing shows; the API pages through the rest.
const LISTED: usize = 200;
/// Workflows on the overview, the ones failing most.
const WORST: usize = 8;
/// Repositories asked for in one query.
const IN_ONE: usize = 200;
/// How long a source is given to say how its pipeline data stands.
const ASKING: std::time::Duration = std::time::Duration::from_secs(5);
const FAILED: [&str; 3] = ["failure", "timed_out", "startup_failure"];

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
    pub periods: Vec<Choice>,
    /// Faux data from `faux-data`, which the platform says at the top of every page.
    pub faux: bool,
    /// Only the default branch's runs count, which each page says.
    pub default_only: bool,
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
            (Scope::All, "workflows") => "Workflows".to_string(),
            (Scope::All, "runs") => "Workflow runs".to_string(),
            (Scope::All, "failed") => "Failed runs".to_string(),
            (Scope::All, "broken") => "Broken default branches".to_string(),
            (Scope::All, _) => "CI/CD/CT metrics".to_string(),
            (scope, "workflows") => format!("Workflows of {}", scope.label()),
            (scope, "runs") => format!("Workflow runs of {}", scope.label()),
            (scope, "failed") => format!("Failed runs of {}", scope.label()),
            (scope, "broken") => format!("Broken default branches of {}", scope.label()),
            (scope, _) => format!("Pipelines of {}", scope.label()),
        };
        Self {
            tab,
            heading,
            scope,
            days,
            faux: faux::on(backend),
            default_only: Definitions::read(&backend.settings()).default_only,
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
        format!("/p/cicd/{path}?{query}{extra}")
    }

    pub fn overview(&self) -> String {
        self.link("", "")
    }

    pub fn workflows(&self) -> String {
        self.link("workflows", "")
    }

    pub fn runs(&self) -> String {
        self.link("runs", "")
    }

    pub fn failed(&self) -> String {
        self.link("runs", "&failed=1")
    }

    pub fn broken(&self) -> String {
        self.link("broken", "")
    }

    pub fn action(&self) -> String {
        match self.tab {
            "overview" => "/p/cicd/".to_string(),
            "failed" => "/p/cicd/runs".to_string(),
            tab => format!("/p/cicd/{tab}"),
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

    pub fn failed_only(&self) -> bool {
        self.tab == "failed"
    }

    /// Which runs count, in a few words for under a heading.
    pub fn counting(&self) -> &'static str {
        match self.default_only {
            true => "Runs on each repository's default branch.",
            false => "Runs on every branch, pull requests included.",
        }
    }
}

/// One source of workflow runs and how it stands, for a page with nothing to show yet.
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

#[derive(Page)]
#[template(path = "home.html")]
struct Home {
    frame: Frame,
    onboarding: Option<Onboarding>,
    problem: Option<String>,
    about: String,
    tiles: Vec<Tile>,
    charts: Vec<Chart>,
    stages: Vec<Row>,
    worst: Vec<Listed>,
    more_workflows: bool,
    row_kind: &'static str,
    rows: Vec<Row>,
    rows_problem: Option<String>,
    unconnected: Vec<String>,
}

/// A workflow's figures, for a table.
pub struct Listed {
    pub repository: String,
    pub workflow: String,
    pub stage: Stage,
    pub runs: i64,
    pub failed: i64,
    pub cells: Vec<metrics::Cell>,
    pub broken: bool,
}

#[derive(Page)]
#[template(path = "workflows.html")]
struct Workflows {
    frame: Frame,
    problem: Option<String>,
    about: String,
    rows: Vec<Listed>,
    total: usize,
}

pub struct RunRow {
    pub at: String,
    pub repository: String,
    pub workflow: String,
    pub branch: String,
    pub conclusion: String,
    pub badge: &'static str,
    pub rerun: bool,
    pub took: String,
    pub url: Option<String>,
}

#[derive(Page)]
#[template(path = "runs.html")]
struct Runs {
    frame: Frame,
    problem: Option<String>,
    about: String,
    rows: Vec<RunRow>,
    total: usize,
}

pub struct Spell {
    pub broke: String,
    pub repository: String,
    pub workflow: String,
    pub stage: String,
    pub broke_url: Option<String>,
    pub fixed: Option<String>,
    pub fixed_url: Option<String>,
    pub took: String,
    pub failed_runs: i64,
}

#[derive(Page)]
#[template(path = "broken.html")]
struct Broken {
    frame: Frame,
    problem: Option<String>,
    about: String,
    rows: Vec<Spell>,
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
        ("GET", "ui/workflows") => workflows(backend, query).await,
        ("GET", "ui/runs") => runs(backend, query).await,
        ("GET", "ui/broken") => broken(backend, query).await,
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

fn scoped_days(query: &str) -> Result<(Scope, i64), Refusal> {
    Ok((Scope::from_query(query)?, days(query)?))
}

/// Whether anything has been worked out yet, which decides between the figures and what to do
/// to get some.
async fn anything(backend: &Backend) -> Result<bool, Refusal> {
    let found = backend.query::<Value>(Query::new("days").fields(&["id"]).limit(1)).await?;
    Ok(!found.records.is_empty())
}

/// How each running source's pipeline data stands. Only a running one is asked, since a call to
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
        let asked = backend.discovery(id, "GET", "pipelines", None, None);
        let Ok(Ok((200, status))) = tokio::time::timeout(ASKING, asked).await else {
            let said = "It did not say how its pipeline data stands.".to_string();
            found.push(Source { id: id.clone(), said, ready: false });
            continue;
        };
        let synced = status["synced"]["at"]
            .as_str()
            .and_then(|at| DateTime::parse_from_rfc3339(at).ok())
            .map(|at| when(at.with_timezone(&Utc)));
        let (said, ready) = match (status["on"].as_bool(), status["problem"].as_str(), synced) {
            (Some(false), _, _) => ("Its Pipeline data feature is off.".to_string(), false),
            (_, Some(problem), _) => (format!("It cannot read workflow runs: {problem}."), false),
            (_, None, Some(at)) => (format!("Workflow runs were last read in full on {at}."), true),
            (_, None, None) => {
                ("It is reading workflow runs for the first time.".to_string(), true)
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
            "Faux workflow runs for {}, made up by {} for its stand-in repositories: {named}.",
            scope.label(),
            faux::PROVIDER
        );
    }
    let from = definitions.sources.join(" and ");
    match repositories {
        None => format!("Every GitHub Actions workflow run read from {from}."),
        Some(repositories) if repositories.is_empty() => format!(
            "{} has no repository connected in the Catalogue, so there is nothing to measure. \
             Connect one there and its runs count at once.",
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

/// The service or team rows of a comparison, from the figures already read for the period.
fn rows(
    members: &[Member],
    held: &Held,
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
        let theirs = Held {
            days: held
                .days
                .iter()
                .filter(|day| member.repositories.contains(&day.repository))
                .cloned()
                .collect(),
            recoveries: held
                .recoveries
                .iter()
                .filter(|spell| member.repositories.contains(&spell.repository))
                .cloned()
                .collect(),
        };
        let figures = metrics::figures(&theirs, period, Counting::of(definitions));
        let query = scope::encoded(&[(kind, &member.name), ("days", &days.to_string())]);
        rows.push(Row {
            title: member.title.clone(),
            href: Some(format!("/p/cicd/?{query}")),
            runs: figures.runs.to_string(),
            cells: metrics::cells(&figures, &definitions.bands),
        });
    }
    (rows, unconnected)
}

fn listed(workflows: Vec<metrics::Workflow>, definitions: &Definitions) -> Vec<Listed> {
    workflows
        .into_iter()
        .map(|workflow| Listed {
            cells: metrics::cells(&workflow.figures, &definitions.bands),
            runs: workflow.figures.runs,
            failed: workflow.figures.failed,
            repository: workflow.repository,
            workflow: workflow.workflow,
            stage: workflow.stage,
            broken: workflow.broken,
        })
        .collect()
}

async fn overview(backend: &Backend, query: &str) -> Response {
    let (scope, days) = match scoped_days(query) {
        Ok(asked) => asked,
        Err(refusal) => return refusal.response(),
    };
    let definitions = Definitions::read(&backend.settings());
    let mut home = Home {
        frame: Frame::new(backend, "overview", scope, days),
        onboarding: None,
        problem: None,
        about: String::new(),
        tiles: Vec::new(),
        charts: Vec::new(),
        stages: Vec::new(),
        worst: Vec::new(),
        more_workflows: false,
        row_kind: "Service",
        rows: Vec::new(),
        rows_problem: None,
        unconnected: Vec::new(),
    };
    let ready =
        home.frame.faux || (backend.feature(METRICS) && anything(backend).await.unwrap_or(false));
    if !ready {
        home.onboarding = Some(onboarding(backend, &definitions).await);
        return drawn(&home);
    }
    let period = Period::last(days);
    let figured = async {
        let repositories = scope::repositories(backend, &home.frame.scope).await?;
        let now = metrics::held(backend, repositories.as_ref(), &period).await?;
        let before = metrics::held(backend, repositories.as_ref(), &period.before()).await?;
        Ok::<_, Refusal>((repositories, now, before))
    };
    let (repositories, now, before) = match figured.await {
        Ok(figured) => figured,
        Err(refusal) => {
            home.problem = Some(refusal.detail);
            return drawn(&home);
        }
    };
    let counting = Counting::of(&definitions);
    home.about = about(&home.frame.scope, repositories.as_ref(), &definitions, home.frame.faux);
    home.tiles = metrics::tiles(
        &metrics::figures(&now, &period, counting),
        &metrics::figures(&before, &period.before(), counting),
        &period,
        &definitions.bands,
    );
    home.charts = metrics::charts(&now, &period, &definitions);
    home.stages = metrics::stages(&now, &period, &definitions);
    let workflows = metrics::workflows(&now, &period, &definitions);
    home.more_workflows = workflows.len() > WORST;
    home.worst = listed(workflows.into_iter().take(WORST).collect(), &definitions);
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

async fn workflows(backend: &Backend, query: &str) -> Response {
    let (scope, days) = match scoped_days(query) {
        Ok(asked) => asked,
        Err(refusal) => return refusal.response(),
    };
    let definitions = Definitions::read(&backend.settings());
    let mut page = Workflows {
        frame: Frame::new(backend, "workflows", scope, days),
        problem: None,
        about: String::new(),
        rows: Vec::new(),
        total: 0,
    };
    let period = Period::last(days);
    let found = async {
        let repositories = scope::repositories(backend, &page.frame.scope).await?;
        let held = metrics::held(backend, repositories.as_ref(), &period).await?;
        Ok::<_, Refusal>((repositories, held))
    };
    match found.await {
        Ok((repositories, held)) => {
            let workflows = metrics::workflows(&held, &period, &definitions);
            page.total = workflows.len();
            page.about =
                about(&page.frame.scope, repositories.as_ref(), &definitions, page.frame.faux);
            page.rows = listed(workflows.into_iter().take(LISTED).collect(), &definitions);
        }
        Err(refusal) => page.problem = Some(refusal.detail),
    }
    drawn(&page)
}

/// The newest runs of these repositories in the period, from every source, counting as the
/// settings say; with faux data, `faux-data`'s.
pub async fn newest_runs(
    backend: &Backend,
    repositories: Option<&BTreeSet<String>>,
    period: &Period,
    failed_only: bool,
    limit: usize,
) -> Result<Vec<Run>, Refusal> {
    let definitions = Definitions::read(&backend.settings());
    let wanted = |run: &Run| {
        (!failed_only || run.failed()) && (!definitions.default_only || run.default_branch)
    };
    if faux::on(backend) {
        let repositories = repositories.cloned().unwrap_or_default();
        let span = (period.from, period.to);
        let mut found = faux::runs(backend, &repositories, span, definitions.default_only).await?;
        found.reverse();
        found.retain(wanted);
        found.truncate(limit);
        return Ok(found);
    }
    let mut filter = json!({ "finished_at": { "gte": period.from, "lt": period.to } });
    if failed_only {
        filter["conclusion"] = json!({ "in": FAILED });
    }
    if definitions.default_only {
        filter["default_branch"] = json!(true);
    }
    let chunks: Vec<Option<Vec<&String>>> = match repositories {
        None => vec![None],
        Some(repositories) if repositories.is_empty() => return Ok(Vec::new()),
        Some(repositories) => repositories
            .iter()
            .collect::<Vec<_>>()
            .chunks(IN_ONE)
            .map(|chunk| Some(chunk.to_vec()))
            .collect(),
    };
    let mut found: Vec<Run> = Vec::new();
    for source in &definitions.sources {
        for chunk in &chunks {
            let mut filter = filter.clone();
            if let Some(chunk) = chunk {
                filter["repository"] = json!({ "in": chunk });
            }
            let asked = Query::new(&format!("{source}.workflow-runs"))
                .filter(filter)
                .order(Order::desc("finished_at"))
                .limit(u32::try_from(limit).unwrap_or(u32::MAX));
            // A source that is not running, or exports nothing to cicd, is passed over.
            match backend.query::<Run>(asked).await {
                Ok(page) => found.extend(page.records),
                Err(err) => {
                    tracing::info!(source, %err, "a source's workflow runs could not be read")
                }
            }
        }
    }
    found.sort_by_key(|run| std::cmp::Reverse(run.finished_at));
    found.truncate(limit);
    Ok(found)
}

fn run_row(run: &Run) -> RunRow {
    let (conclusion, badge) = match run.conclusion.as_str() {
        "success" => ("Passed", "ready"),
        "failure" => ("Failed", "error"),
        "timed_out" => ("Timed out", "error"),
        "startup_failure" => ("Failed to start", "error"),
        "cancelled" => ("Cancelled", "unknown"),
        "skipped" => ("Skipped", "unknown"),
        other => (other, "unknown"),
    };
    RunRow {
        at: when(run.finished_at),
        repository: run.repository.clone(),
        workflow: run.workflow.clone(),
        branch: run.branch.clone().unwrap_or_default(),
        conclusion: conclusion.to_string(),
        badge,
        rerun: run.succeeded() && run.attempt > 1,
        took: metrics::duration(run.seconds()),
        url: web(run.url.as_deref()),
    }
}

async fn runs(backend: &Backend, query: &str) -> Response {
    let (scope, days) = match scoped_days(query) {
        Ok(asked) => asked,
        Err(refusal) => return refusal.response(),
    };
    let failed_only = parameter(query, "failed").is_some_and(|value| value != "0");
    let definitions = Definitions::read(&backend.settings());
    let tab = if failed_only { "failed" } else { "runs" };
    let mut page = Runs {
        frame: Frame::new(backend, tab, scope, days),
        problem: None,
        about: String::new(),
        rows: Vec::new(),
        total: 0,
    };
    let period = Period::last(days);
    let found = async {
        let repositories = scope::repositories(backend, &page.frame.scope).await?;
        let found =
            newest_runs(backend, repositories.as_ref(), &period, failed_only, LISTED + 1).await?;
        Ok::<_, Refusal>((repositories, found))
    };
    match found.await {
        Ok((repositories, found)) => {
            page.total = found.len();
            page.about =
                about(&page.frame.scope, repositories.as_ref(), &definitions, page.frame.faux);
            page.rows = found.iter().take(LISTED).map(run_row).collect();
        }
        Err(refusal) => page.problem = Some(refusal.detail),
    }
    drawn(&page)
}

fn spell(spell: &Recovery) -> Spell {
    Spell {
        broke: when(spell.broke_at),
        repository: spell.repository.clone(),
        workflow: spell.workflow.clone(),
        stage: Stage::from_key(&spell.stage).unwrap_or(Stage::Ci).short().to_string(),
        broke_url: web(spell.broke_url.as_deref()),
        fixed: spell.fixed_at.map(when),
        fixed_url: web(spell.fixed_url.as_deref()),
        took: spell.seconds.map_or_else(
            || metrics::duration((Utc::now() - spell.broke_at).num_seconds() as f64),
            metrics::duration,
        ),
        failed_runs: spell.failed_runs,
    }
}

/// Still broken first, then the newest.
pub fn ordered(mut spells: Vec<Recovery>) -> Vec<Recovery> {
    spells.sort_by_key(|spell| (spell.fixed_at.is_some(), std::cmp::Reverse(spell.broke_at)));
    spells
}

async fn broken(backend: &Backend, query: &str) -> Response {
    let (scope, days) = match scoped_days(query) {
        Ok(asked) => asked,
        Err(refusal) => return refusal.response(),
    };
    let definitions = Definitions::read(&backend.settings());
    let mut page = Broken {
        frame: Frame::new(backend, "broken", scope, days),
        problem: None,
        about: String::new(),
        rows: Vec::new(),
    };
    let period = Period::last(days);
    let found = async {
        let repositories = scope::repositories(backend, &page.frame.scope).await?;
        let held = metrics::held(backend, repositories.as_ref(), &period).await?;
        Ok::<_, Refusal>((repositories, held))
    };
    match found.await {
        Ok((repositories, held)) => {
            page.about =
                about(&page.frame.scope, repositories.as_ref(), &definitions, page.frame.faux);
            page.rows = ordered(held.recoveries).iter().take(LISTED).map(spell).collect();
        }
        Err(refusal) => page.problem = Some(refusal.detail),
    }
    drawn(&page)
}

/// The Pipelines panel on a service's, team's or organisation's page in the Catalogue: the four
/// tiles for the last 30 days, and the way to the rest.
/// The four figures, by the name each is pinned under.
pub const INSIGHTS: [(&str, &str); 4] = [
    ("success-rate", "Success rate"),
    ("duration", "Duration"),
    ("recover", "Time to recover"),
    ("re-run", "Passed on a re-run"),
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
/// on its own, with why there is none where there is none.
async fn insight(backend: &Backend, which: &str, query: &str) -> Response {
    let Some((_, label)) = INSIGHTS.iter().find(|(id, _)| *id == which) else {
        return Response::not_found();
    };
    let mut tile = InsightTile {
        label: (*label).to_string(),
        value: "—".into(),
        note: String::new(),
        more: "/p/cicd/".into(),
    };
    let scope = match Scope::from_query(query) {
        Ok(Scope::All) | Err(_) => {
            tile.note = "Nothing was named to measure".into();
            return drawn(&tile);
        }
        Ok(scope) => scope,
    };
    tile.more = format!("/p/cicd/?{}", scope.query());
    if !backend.feature(METRICS) && !faux::on(backend) {
        tile.note = "CI/CD/CT metrics are turned off".into();
        return drawn(&tile);
    }
    let period = Period::last(DEFAULT_DAYS);
    let definitions = Definitions::read(&backend.settings());
    let found = async {
        let repositories = scope::repositories(backend, &scope).await?;
        let now = metrics::held(backend, repositories.as_ref(), &period).await?;
        let before = metrics::held(backend, repositories.as_ref(), &period.before()).await?;
        Ok::<_, Refusal>((repositories, now, before))
    };
    match found.await {
        Ok((Some(repositories), _, _)) if repositories.is_empty() => {
            tile.note = "No repository is connected to it".into();
        }
        Ok((_, now, before)) => {
            let counting = Counting::of(&definitions);
            let tiles = metrics::tiles(
                &metrics::figures(&now, &period, counting),
                &metrics::figures(&before, &period.before(), counting),
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
    let more = format!("/p/cicd/?{}", scope.query());
    let mut panel = Panel { message: None, tiles: Vec::new(), more };
    if !backend.feature(METRICS) && !faux::on(backend) {
        panel.message = Some("CI/CD/CT metrics are turned off.".into());
        return drawn(&panel);
    }
    let definitions = Definitions::read(&backend.settings());
    let found = async {
        let repositories = scope::repositories(backend, &scope).await?;
        let now = metrics::held(backend, repositories.as_ref(), &period).await?;
        let before = metrics::held(backend, repositories.as_ref(), &period.before()).await?;
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
            let counting = Counting::of(&definitions);
            panel.tiles = metrics::tiles(
                &metrics::figures(&now, &period, counting),
                &metrics::figures(&before, &period.before(), counting),
                &period,
                &definitions.bands,
            );
        }
        Err(refusal) => panel.message = Some(refusal.detail),
    }
    drawn(&panel)
}
