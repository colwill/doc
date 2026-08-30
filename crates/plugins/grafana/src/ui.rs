//! The pages at `/p/grafana/...`: the dashboards; a dashboard with its time range and variables
//! to choose and a card per panel that draws itself as it comes in; the figures behind a chart;
//! and a panel on each service's page in the Catalogue with the dashboards given that service.

use std::collections::BTreeMap;

use askama::Template;
use doc_plugin_sdk::{Backend, Request, Response};

use crate::dashboard::{ALL, Dashboard, Panel, RANGES, Range, Scope, Variable};
use crate::frames::{self, Answer, MOST_SERIES, Series, Table};
use crate::grafana::{self, Datasource, Grafana};
use crate::settings::{Config, RULES};
use crate::view::{self, connected};
use crate::{Refusal, parameter};

/// The most panels of a dashboard a service's page shows; the rest are a link away.
const PREVIEWED: usize = 4;

/// The kinds of panel DOC draws. Any other is left to Grafana's own page.
const DRAWN: [&str; 13] = [
    "timeseries",
    "graph",
    "trend",
    "barchart",
    "bargauge",
    "stat",
    "singlestat",
    "gauge",
    "piechart",
    "grafana-piechart-panel",
    "table",
    "table-old",
    "geomap",
];

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn plural(count: usize, word: &str) -> String {
    match count {
        1 => format!("1 {word}"),
        _ => format!("{count} {word}s"),
    }
}

/// One choice in a drop-down.
pub struct Pick {
    pub value: String,
    pub label: String,
    pub chosen: bool,
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    problem: String,
}

pub struct Row {
    pub title: String,
    pub href: String,
    pub folder: String,
    pub tags: Vec<String>,
    pub services: String,
    /// Chosen on the Settings page, but not among the dashboards Grafana lists.
    pub missing: bool,
}

#[derive(Template)]
#[template(path = "home.html")]
struct Home {
    connected: bool,
    problem: Option<String>,
    settles: bool,
    rules: Vec<&'static str>,
    /// Every dashboard the account can see is listed, since none are chosen, so it can be searched.
    every: bool,
    q: String,
    rows: Vec<Row>,
    count: String,
}

pub struct Input {
    pub id: String,
    pub name: String,
    pub label: String,
    pub picks: Vec<Pick>,
    /// The value typed, for a variable with nothing to choose from.
    pub free: Option<String>,
}

pub struct Card {
    pub title: String,
    pub href: String,
}

pub struct Part {
    pub title: String,
    pub cards: Vec<Card>,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct Page {
    title: String,
    lede: String,
    services: String,
    open: Option<String>,
    action: String,
    ranges: Vec<Pick>,
    inputs: Vec<Input>,
    parts: Vec<Part>,
}

pub struct Chart {
    pub config: String,
    pub described: String,
}

pub struct Figure {
    pub label: String,
    pub value: String,
    pub note: String,
}

pub enum Drawn {
    Chart(Chart),
    Map(Chart),
    Figures(Vec<Figure>),
    Table(Table),
    Text(Vec<String>),
    Nothing(String),
}

#[derive(Template)]
#[template(path = "panel.html")]
struct PanelView {
    description: String,
    drawn: Drawn,
    problems: Vec<String>,
    figures: Option<String>,
    /// What the figures are behind: a chart or a map.
    pictured: &'static str,
    open: Option<String>,
}

#[derive(Template)]
#[template(path = "figures.html")]
struct Figures {
    title: String,
    board_title: String,
    board_href: String,
    lede: String,
    table: Option<Table>,
    problems: Vec<String>,
}

pub struct Previewed {
    pub title: String,
    pub href: String,
    pub cards: Vec<Card>,
    pub more: String,
    pub problem: Option<String>,
}

#[derive(Template)]
#[template(path = "service.html")]
struct ServicePanel {
    connected: bool,
    settles: bool,
    boards: Vec<Previewed>,
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    // A card or a panel loads into a page that is already drawn, so a refusal says so in place.
    let fragment = matches!(path, ["panel"] | ["d", _, "panels", _]);
    match route(backend, request, path).await {
        Ok(html) => Response::html(html),
        Err(refusal) if fragment => Response::html(format!(
            "<p class=\"doc-error-message\" role=\"alert\">{}</p>",
            escape(&refusal.detail)
        )),
        Err(refusal) => {
            let page = Blank { problem: refusal.detail.clone() };
            let html = page.render().unwrap_or_else(|_| escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

async fn route(backend: &Backend, request: &Request, path: &[&str]) -> Result<String, Refusal> {
    let config = Config::read(&backend.settings());
    let query = request.query.as_str();
    match (request.method.as_str(), path) {
        ("GET", []) => home(backend, &config, query).await,
        ("GET", ["panel"]) => service(backend, &config, query).await,
        ("GET", ["d", uid]) => page(backend, &config, view::uid(uid)?, query).await,
        ("GET", ["d", uid, "panels", id]) => {
            panel(backend, &config, view::uid(uid)?, id, query).await
        }
        ("GET", ["d", uid, "panels", id, "figures"]) => {
            figures(backend, &config, view::uid(uid)?, id, query).await
        }
        _ => Err(Refusal::missing("there is no such page")),
    }
}

async fn home(backend: &Backend, config: &Config, query: &str) -> Result<String, Refusal> {
    let q = parameter(query, "q").unwrap_or_default();
    let every = config.dashboards.is_empty();
    let mut page = Home {
        connected: config.account.is_some(),
        problem: None,
        settles: view::settles(backend),
        rules: RULES.to_vec(),
        every,
        q: q.clone(),
        rows: Vec::new(),
        count: String::new(),
    };
    let Some(account) = config.account.as_deref() else { return render(&page) };
    let searched = if every { q.as_str() } else { "" };
    match Grafana::new(backend, account).search(searched).await {
        Err(refusal) => page.problem = Some(refusal.detail),
        Ok(found) => {
            let row = |uid: &str, services: &[String]| {
                let held = found.iter().find(|found| found.uid == uid);
                Row {
                    title: held.map_or_else(|| uid.to_string(), |held| held.title.clone()),
                    href: format!("/p/grafana/d/{uid}"),
                    folder: held.map(|held| held.folder.clone()).unwrap_or_default(),
                    tags: held.map(|held| held.tags.clone()).unwrap_or_default(),
                    services: services.join(", "),
                    missing: held.is_none(),
                }
            };
            page.rows = match every {
                true => found.iter().map(|held| row(&held.uid, &[])).collect(),
                false => {
                    config.dashboards.iter().map(|(uid, services)| row(uid, services)).collect()
                }
            };
        }
    }
    page.count = plural(page.rows.len(), "dashboard");
    render(&page)
}

/// The time ranges to choose from, the chosen one marked, with the dashboard's own if it differs.
fn ranges(board: &Dashboard, chosen: &Range) -> Vec<Pick> {
    let mut picks: Vec<Pick> = RANGES
        .iter()
        .map(|(from, label)| Pick {
            value: (*from).to_string(),
            label: (*label).to_string(),
            chosen: chosen.to == "now" && chosen.from == *from,
        })
        .collect();
    let saved = match board.to.as_str() {
        "now" => board.from.clone(),
        to => format!("{}~{to}", board.from),
    };
    if !picks.iter().any(|pick| pick.value == saved) {
        let chosen = chosen.value() == saved;
        picks.insert(0, Pick { value: saved, label: "As the dashboard is saved".into(), chosen });
    }
    if !picks.iter().any(|pick| pick.chosen) {
        picks.insert(0, Pick { value: chosen.value(), label: chosen.words(), chosen: true });
    }
    picks
}

/// A variable as the page offers it: a drop-down of its values, or a box to type one in.
fn input(variable: &Variable, scope: &Scope<'_>, list: &[Datasource]) -> Input {
    let chosen = scope.values.get(&variable.name).cloned().unwrap_or_default();
    let options: Vec<(String, String)> = match variable.kind.as_str() {
        "datasource" => list
            .iter()
            .filter(|held| held.kind == variable.query)
            .map(|held| (held.name.clone(), held.uid.clone()))
            .collect(),
        _ => variable.options.clone(),
    };
    let mut picks: Vec<Pick> = options
        .iter()
        .map(|(shown, value)| Pick {
            value: value.clone(),
            label: if value == ALL { "All".into() } else { shown.clone() },
            chosen: chosen.contains(value) || chosen.contains(shown),
        })
        .collect();
    // A value chosen in Grafana that is not among the options stays chosen rather than lost.
    if !picks.is_empty()
        && !picks.iter().any(|pick| pick.chosen)
        && let Some(first) = chosen.first()
    {
        picks.insert(0, Pick { value: first.clone(), label: chosen.join(" + "), chosen: true });
    }
    let free = picks.is_empty().then(|| chosen.join(","));
    Input {
        id: format!("grafana-var-{}", variable.name),
        name: format!("var-{}", variable.name),
        label: variable.label.clone(),
        picks,
        free,
    }
}

/// What a panel is called, with the variables it names put in, as Grafana's own page shows it.
fn titled(panel: &Panel, scope: &Scope<'_>) -> String {
    match panel.title.is_empty() {
        true => "A panel with no title".into(),
        false => scope.words(&panel.title),
    }
}

fn cards(uid: &str, panels: &[&Panel], scope: &Scope<'_>) -> Vec<Card> {
    let query = scope.query();
    panels
        .iter()
        .map(|panel| Card {
            title: titled(panel, scope),
            href: format!("/p/grafana/d/{uid}/panels/{}?{query}", panel.id),
        })
        .collect()
}

async fn page(
    backend: &Backend,
    config: &Config,
    uid: &str,
    query: &str,
) -> Result<String, Refusal> {
    let grafana = Grafana::new(backend, connected(config)?);
    let board = view::board(&grafana, uid).await?;
    let scope = Scope::new(&board, query, &BTreeMap::new());
    let shown: Vec<&Variable> =
        board.variables.iter().filter(|variable| !variable.hidden).collect();
    let list = match shown.iter().any(|variable| variable.kind == "datasource") {
        true => grafana.datasources().await.unwrap_or_default(),
        false => Vec::new(),
    };
    let parts = board
        .sections
        .iter()
        .map(|section| Part {
            title: section.title.clone(),
            cards: cards(uid, &section.panels.iter().collect::<Vec<_>>(), &scope),
        })
        .collect();
    let open = grafana::address(backend, config)
        .await
        .map(|address| format!("{address}{}?{}", board.path, scope.grafana_query()));
    let page = Page {
        lede: format!(
            "In {} · {} · {}",
            board.folder,
            plural(board.panels().count(), "panel"),
            scope.range.words()
        ),
        services: config
            .dashboards
            .get(uid)
            .map(|services| services.join(", "))
            .unwrap_or_default(),
        open,
        action: format!("/p/grafana/d/{uid}"),
        ranges: ranges(&board, &scope.range),
        inputs: shown.iter().map(|variable| input(variable, &scope, &list)).collect(),
        parts,
        title: board.title.clone(),
    };
    render(&page)
}

/// What a panel says beneath its heading, from its own text, a paragraph a block.
fn paragraphs(panel: &Panel, scope: &Scope<'_>) -> Vec<String> {
    let html = panel.options["mode"].as_str() == Some("html");
    let mut text = scope.words(&panel.content);
    if html {
        let mut plain = String::with_capacity(text.len());
        let mut inside = false;
        for c in text.chars() {
            match c {
                '<' => inside = true,
                '>' => inside = false,
                c if !inside => plain.push(c),
                _ => {}
            }
        }
        text = plain;
    }
    text.split("\n\n")
        .map(|block| {
            block
                .lines()
                .map(|line| line.trim().trim_start_matches('#').trim().replace(['*', '`'], ""))
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|paragraph| !paragraph.is_empty())
        .take(20)
        .collect()
}

/// The unit a panel's numbers are in: its own, or else the first its data source gave.
fn unit_of(panel: &Panel, series: &[Series]) -> String {
    match panel.unit() {
        "" => series.iter().find_map(|series| series.unit.clone()).unwrap_or_default(),
        unit => unit.to_string(),
    }
}

/// A panel's answer drawn the way its kind is meant to be read.
fn drawn(panel: &Panel, answer: &Answer, range: &Range) -> Drawn {
    if answer.is_empty() {
        return Drawn::Nothing("Nothing in this range.".into());
    }
    let series = answer.series();
    let timed: Vec<Series> = series.iter().filter(|series| series.timed).cloned().collect();
    let unit = unit_of(panel, &series);
    let calculation = panel.calculation();
    let described = format!(
        "{}: {} {}",
        if panel.title.is_empty() { "A panel" } else { &panel.title },
        match series.len() {
            1 => "one series".to_string(),
            count => format!("{count} series"),
        },
        range.over()
    );
    let chart = |config: serde_json::Value| {
        Drawn::Chart(Chart { config: config.to_string(), described: described.clone() })
    };
    let table = || {
        frames::table(answer, &unit, panel.decimals())
            .map_or_else(|| Drawn::Nothing("Nothing in this range.".into()), Drawn::Table)
    };
    match panel.kind.as_str() {
        "timeseries" | "graph" | "trend" if !timed.is_empty() => {
            chart(frames::over_time(&timed, &unit, range.millis(), false))
        }
        "barchart" => match frames::by_category(answer, &unit) {
            Some(config) => chart(config),
            None if !timed.is_empty() => {
                chart(frames::over_time(&timed, &unit, range.millis(), true))
            }
            None => table(),
        },
        "bargauge" | "piechart" | "grafana-piechart-panel" => {
            let pie = panel.kind != "bargauge";
            frames::reduced(&series, calculation, &unit, pie).map_or_else(table, chart)
        }
        "geomap" => match crate::geomap::map(panel, answer, &unit) {
            Ok((config, places)) => Drawn::Map(Chart {
                config: config.to_string(),
                described: format!(
                    "{}: {} {}",
                    if panel.title.is_empty() { "A map" } else { &panel.title },
                    plural(places, "place"),
                    range.over()
                ),
            }),
            Err(why) => Drawn::Nothing(why),
        },
        "stat" | "singlestat" | "gauge" => {
            let alone = series.len() == 1;
            let figures: Vec<Figure> = series
                .iter()
                .take(MOST_SERIES)
                .filter_map(|series| {
                    let value = frames::reduce(&series.points, calculation)?;
                    Some(Figure {
                        label: match alone {
                            true => frames::calculated(calculation).to_string(),
                            false => series.name.clone(),
                        },
                        value: frames::figure(value, &unit, panel.decimals()),
                        note: match alone {
                            true => String::new(),
                            false => frames::calculated(calculation).to_string(),
                        },
                    })
                })
                .collect();
            match figures.is_empty() {
                true => Drawn::Nothing("No value in this range.".into()),
                false => Drawn::Figures(figures),
            }
        }
        _ => table(),
    }
}

/// Whether a panel reuses another panel's results (`-- Dashboard --`), which has no query to run.
fn mirrors(panel: &Panel) -> bool {
    let named = panel.datasource["uid"].as_str().or_else(|| panel.datasource.as_str());
    named == Some("-- Dashboard --")
}

/// A Grafana kind of panel in words, such as `state timeline`.
fn kind_words(kind: &str) -> String {
    match kind {
        "" => "this kind of".into(),
        kind => kind.replace(['-', '_'], " "),
    }
}

async fn panel(
    backend: &Backend,
    config: &Config,
    uid: &str,
    id: &str,
    query: &str,
) -> Result<String, Refusal> {
    let grafana = Grafana::new(backend, connected(config)?);
    let board = view::board(&grafana, uid).await?;
    let panel = view::panel(&grafana, &board, id).await?;
    let scope = Scope::new(&board, query, &BTreeMap::new());
    let mut view = PanelView {
        description: scope.words(&panel.description),
        drawn: Drawn::Nothing(String::new()),
        problems: Vec::new(),
        figures: None,
        pictured: "chart",
        open: None,
    };
    let left_to_grafana = || async {
        grafana::address(backend, config).await.map(|address| {
            format!("{address}{}?{}&viewPanel={}", board.path, scope.grafana_query(), panel.id)
        })
    };
    if panel.kind == "text" {
        view.drawn = Drawn::Text(paragraphs(&panel, &scope));
        return render(&view);
    }
    if !DRAWN.contains(&panel.kind.as_str()) {
        view.drawn = Drawn::Nothing(format!(
            "DOC does not draw {} panels; Grafana's own page does.",
            kind_words(&panel.kind)
        ));
        view.open = left_to_grafana().await;
        return render(&view);
    }
    if mirrors(&panel) {
        view.drawn = Drawn::Nothing(
            "This panel shows another panel's results, which only Grafana's own page draws.".into(),
        );
        view.open = left_to_grafana().await;
        return render(&view);
    }
    match view::answer(&grafana, &scope, &panel).await {
        Err(why) => view.problems.push(why),
        Ok(answer) => {
            view.drawn = drawn(&panel, &answer, &scope.range);
            view.problems.clone_from(&answer.problems);
            if let Drawn::Chart(_) | Drawn::Map(_) = view.drawn {
                view.pictured = if panel.kind == "geomap" { "map" } else { "chart" };
                view.figures = Some(format!(
                    "/p/grafana/d/{uid}/panels/{}/figures?{}",
                    panel.id,
                    scope.query()
                ));
            }
        }
    }
    render(&view)
}

async fn figures(
    backend: &Backend,
    config: &Config,
    uid: &str,
    id: &str,
    query: &str,
) -> Result<String, Refusal> {
    let grafana = Grafana::new(backend, connected(config)?);
    let board = view::board(&grafana, uid).await?;
    let panel = view::panel(&grafana, &board, id).await?;
    let scope = Scope::new(&board, query, &BTreeMap::new());
    let mut page = Figures {
        title: titled(&panel, &scope),
        board_title: board.title.clone(),
        board_href: format!("/p/grafana/d/{uid}?{}", scope.query()),
        lede: match panel.kind.as_str() {
            "geomap" => format!("Every place the map draws, {}.", scope.range.over()),
            _ => format!("Every figure the chart draws, {}.", scope.range.over()),
        },
        table: None,
        problems: Vec::new(),
    };
    match view::answer(&grafana, &scope, &panel).await {
        Err(why) => page.problems.push(why),
        Ok(answer) => {
            let series = answer.series();
            let unit = unit_of(&panel, &series);
            page.problems.clone_from(&answer.problems);
            page.table = match series.is_empty() || panel.kind == "geomap" {
                true => frames::table(&answer, &unit, panel.decimals()),
                false => Some(frames::every_point(&series, &unit, panel.decimals())),
            };
        }
    }
    render(&page)
}

/// A service page's panel: each dashboard given the service, its first panels drawn for it.
async fn service(backend: &Backend, config: &Config, query: &str) -> Result<String, Refusal> {
    let resource = parameter(query, "resource").unwrap_or_default();
    let service = resource
        .split_once(':')
        .filter(|(kind, _)| kind.eq_ignore_ascii_case("service"))
        .map(|(_, name)| name.to_string())
        .ok_or_else(|| Refusal::bad("the panel is for a service: resource=service:<name>"))?;
    let mut panel = ServicePanel {
        connected: config.account.is_some(),
        settles: view::settles(backend),
        boards: Vec::new(),
    };
    let Some(account) = config.account.as_deref() else { return render(&panel) };
    let grafana = Grafana::new(backend, account);
    for uid in config.for_service(&service) {
        let board = match view::board(&grafana, &uid).await {
            Ok(board) => board,
            Err(refusal) => {
                panel.boards.push(Previewed {
                    title: uid.clone(),
                    href: format!("/p/grafana/d/{uid}"),
                    cards: Vec::new(),
                    more: String::new(),
                    problem: Some(refusal.detail),
                });
                continue;
            }
        };
        let mut given = BTreeMap::new();
        if board.variable(&config.service_variable).is_some() {
            given.insert(config.service_variable.clone(), service.clone());
        }
        let scope = Scope::new(&board, "", &given);
        let asked = scope.query();
        let drawable: Vec<&Panel> = board.panels().filter(|panel| panel.kind != "text").collect();
        let shown: Vec<&Panel> = drawable.iter().take(PREVIEWED).copied().collect();
        panel.boards.push(Previewed {
            title: board.title.clone(),
            href: format!("/p/grafana/d/{uid}?{asked}"),
            cards: cards(&uid, &shown, &scope),
            more: match drawable.len().saturating_sub(shown.len()) {
                0 => String::new(),
                _ => format!("The whole dashboard, {}", plural(board.panels().count(), "panel")),
            },
            problem: None,
        });
    }
    render(&panel)
}
