//! Askama plumbing: the chrome every page shares, the demo gallery and the fallback page.

use askama::Template;
use axum::extract::{Extension, Query, State};
use axum::response::Html;

use super::csrf::Csrf;
use super::error::WebError;
use super::{AppState, VERSION};
use crate::backend::{History, Level, PluginStatus, Status};
use crate::session::Signed;
use serde::Deserialize;

/// What this platform is called beside the logo: the configured name at start-up, and whatever an
/// administrator set in Settings once a page has read it. Pages with somebody signed in take it
/// from their access summary; the rest, such as signing in, show the last one seen.
static INSTANCE: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);
static CONFIGURED: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// The name this service was configured with, which is used until Settings says otherwise.
pub fn set_configured_instance(name: &str) {
    let name = name.trim();
    let _ = CONFIGURED.set((!name.is_empty()).then(|| name.to_string()));
}

/// What the platform's settings say it is called; empty falls back to the configured name.
pub fn set_instance(name: &str) {
    let name = name.trim();
    if let Ok(mut held) = INSTANCE.write() {
        *held = (!name.is_empty()).then(|| name.to_string());
    }
}

pub fn instance() -> Option<String> {
    INSTANCE
        .read()
        .ok()
        .and_then(|held| held.clone())
        .or_else(|| CONFIGURED.get().cloned().flatten())
}

pub struct NavItem {
    pub label: String,
    /// Empty for a group, which only opens its menu.
    pub href: String,
    pub current: bool,
    pub children: Vec<NavItem>,
    /// Kept at the far end of the bar rather than in the run of menus: the `Admin` menu, so an
    /// administrator's own tools are apart from everyone's pages.
    pub end: bool,
    /// Counted for access and breadcrumbs, but never drawn as a link in the bar.
    pub hidden: bool,
}

impl NavItem {
    pub fn is_group(&self) -> bool {
        self.href.is_empty()
    }

    /// A menu's own page, which lists what is in it; empty for anything that is a page already.
    pub fn menu_href(&self) -> String {
        match self.is_group() {
            true => super::navigation::menu_href(&self.label),
            false => String::new(),
        }
    }
}

pub struct Alert {
    pub kind: &'static str,
    pub title: &'static str,
    pub body: String,
}

/// One section of a page that has several, listed in the contents at its left. Each section is a
/// page of its own, so it carries only its own forms, and the header's buttons are that section's.
pub struct Section {
    pub title: String,
    pub href: String,
    pub current: bool,
}

impl Section {
    pub fn new(title: &str, href: String, current: bool) -> Self {
        Self { title: title.to_string(), href, current }
    }
}

pub struct Chrome {
    pub title: String,
    /// What the page calls itself in the header band above its content, which is its title unless
    /// a page says otherwise — "Service status" under the title `Status`, a plugin page's own
    /// first heading. NHS England Digital opens every page this way, and so does this one.
    heading: Option<String>,
    /// The sentence under that heading. A summary with anything but words in it — a name in bold,
    /// a command in code — stays in the page and fills the layout's `page_lede` block instead.
    pub lede: Option<String>,
    /// Whether that band is drawn at all: the landing page has a hero of its own in its place.
    pub header: bool,
    pub instance: Option<String>,
    pub nav: Vec<NavItem>,
    /// The footer's links: the platform's health, your own tokens and help.
    pub footer: Vec<NavItem>,
    /// Whether this viewer is a platform administrator, which the admin pages show their tabs to.
    pub admin: bool,
    /// Where the page sits, shown above its tabs or heading; empty on the landing page.
    pub crumbs: Vec<super::navigation::Crumb>,
    pub alerts: Vec<Alert>,
    pub version: &'static str,
    pub dev_reload: bool,
    /// Who is signed in; the navigation and the sign-out button show only when someone is.
    pub user: Option<String>,
    /// What this page's forms and HTMX requests send back as their CSRF token.
    pub csrf: String,
    /// Whether to answer with the page's own part alone, for a boosted navigation whose browser
    /// already holds the chrome around it.
    pub fragment: bool,
    /// What the plugins are sorted into, so any page can give a plugin its category's pill.
    pub categories: Vec<crate::backend::Category>,
    /// The pill beside the heading, on the pages about one plugin.
    pub category: Option<super::categories::Pill>,
    path: String,
}

impl Chrome {
    /// Where a narrow screen's breadcrumbs lead instead of the whole trail: the nearest linked step.
    pub fn back(&self) -> Option<&super::navigation::Crumb> {
        self.crumbs.iter().rev().find(|crumb| crumb.href.is_some())
    }

    /// The landing page, whose hero holds a search of its own, so the header leaves it out.
    pub fn on_home(&self) -> bool {
        self.path == "/"
    }

    /// Where this page sits, so the header's search can ask for wherever it is first.
    pub fn path(&self) -> &str {
        &self.path
    }

    pub fn asset(&self, name: &str) -> String {
        super::assets::url(name)
    }

    /// The logo: DOC's beside an instance's own name, and rundoc's when it has none.
    pub fn logo(&self) -> String {
        self.asset(match self.instance {
            Some(_) => "brand/doc.png",
            None => "brand/rundoc.png",
        })
    }

    /// What the logo says, for whoever cannot see it.
    pub fn logo_name(&self) -> &'static str {
        match self.instance {
            Some(_) => "DOC",
            None => "rundoc",
        }
    }

    /// The navigation is filled in by `with_plugins`, from what the signed-in user may open.
    pub fn new(title: impl Into<String>, current: &str) -> Self {
        let nav = Vec::new();
        Self {
            title: title.into(),
            heading: None,
            lede: None,
            header: true,
            instance: instance(),
            nav,
            footer: Vec::new(),
            admin: false,
            crumbs: Vec::new(),
            alerts: Vec::new(),
            version: VERSION,
            dev_reload: super::dev::enabled(),
            user: None,
            csrf: String::new(),
            fragment: false,
            categories: Vec::new(),
            category: None,
            path: current.to_string(),
        }
    }

    /// The pill for a plugin's category, if it is in one.
    pub fn pill(&self, plugin: &str) -> Option<super::categories::Pill> {
        super::categories::pill(&self.categories, plugin)
    }

    /// A page about one plugin, headed with its category's pill. After `with_plugins`, which is
    /// what knows the categories.
    pub fn about_plugin(mut self, plugin: &str) -> Self {
        self.category = self.pill(plugin);
        self
    }

    /// What the header band says, which is the title unless the page was given its own words.
    pub fn heading(&self) -> &str {
        self.heading.as_deref().unwrap_or(&self.title)
    }

    /// Head the page with something other than its title, where the two are not the same thing:
    /// `Status` in the browser's tab, "Service status" on the page.
    pub fn headed(mut self, heading: impl Into<String>) -> Self {
        let heading = heading.into();
        self.heading = (!heading.trim().is_empty()).then_some(heading);
        self
    }

    /// The sentence under the heading, for a summary that is only words.
    pub fn about(mut self, lede: impl Into<String>) -> Self {
        let lede = lede.into();
        self.lede = (!lede.trim().is_empty()).then_some(lede);
        self
    }

    /// No header band: the page opens with something of its own, as the landing page's hero does.
    pub fn bare(mut self) -> Self {
        self.header = false;
        self
    }

    pub fn with_csrf(mut self, csrf: &Csrf) -> Self {
        self.csrf = csrf.0.clone();
        self
    }

    pub fn signed(self, signed: &Signed, csrf: &Csrf) -> Self {
        let mut chrome = self.with_csrf(csrf);
        chrome.user = Some(signed.label());
        chrome.fragment = signed.boosted;
        chrome
    }

    /// The pages the signed-in user may open, including every readable plugin's, from their
    /// cached access and laid out the way an administrator arranged them.
    pub async fn with_plugins(mut self, state: &AppState, signed: &Signed) -> Self {
        use super::navigation::{arrange, footer, offered};
        let access = crate::session::access(state, signed).await;
        // The platform's own name travels with the access summary, so a page that knows who is
        // reading it shows what Settings says, and pages that do not show the last one seen.
        set_instance(&access.settings.instance_name);
        self.instance = instance();
        self.nav = arrange(&offered(&access), access.navigation.as_ref(), &access, &self.path);
        self.footer = footer(&access, access.navigation.as_ref());
        self.admin = access.admin;
        self.categories = access.categories.clone();
        self.crumbs = match self.path.as_str() {
            "/" => Vec::new(),
            _ => super::navigation::crumbs(&self.nav, Vec::new(), &self.title),
        };
        self
    }

    /// A plugin's page names itself with its first heading — which the layout draws in the band
    /// above it, so a plugin is headed like every other page without knowing the band is there —
    /// and places itself with the tab it marks current, and any `doc-trail` steps below that, so
    /// its breadcrumbs go as deep as the page does.
    pub fn within_plugin(mut self, heading: Option<&str>, fragment: &str) -> Self {
        let heading = heading.map(str::to_string);
        if let Some(heading) = &heading {
            self.title = heading.clone();
            self.heading = Some(heading.clone());
        }
        let between = super::plugins::current_tab(fragment)
            .into_iter()
            .chain(super::plugins::trail(fragment))
            .map(|(label, href)| super::navigation::Crumb::linked(&label, &href))
            .collect();
        self.crumbs = super::navigation::crumbs(&self.nav, between, &self.title);
        self
    }
}

#[derive(Template)]
#[template(path = "page.html")]
pub struct Page {
    pub chrome: Chrome,
    pub heading: String,
    pub message: String,
}

impl Page {
    pub fn within(chrome: Chrome, heading: impl Into<String>, message: impl Into<String>) -> Self {
        let heading = heading.into();
        Self { chrome: chrome.headed(&heading), heading, message: message.into() }
    }

    pub fn new(title: impl Into<String>, message: impl Into<String>) -> Self {
        let title = title.into();
        let chrome = Chrome::new(title.clone(), "");
        Self { heading: title, chrome, message: message.into() }
    }

    pub fn render_html(&self) -> Result<Html<String>, WebError> {
        Ok(Html(self.render()?))
    }
}

#[derive(Template)]
#[template(path = "design.html")]
pub struct Design {
    pub chrome: Chrome,
    pub backend: String,
    pub version: &'static str,
}

/// The component gallery: every `doc-*` class a plugin's pages may use.
pub async fn design(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    let mut chrome =
        Chrome::new("Design", "/design").signed(&signed, &csrf).with_plugins(&state, &signed).await;
    chrome.alerts = vec![
        Alert { kind: "error", title: "Error", body: "The archive plugin failed to load.".into() },
        Alert { kind: "success", title: "Success", body: "The identity plugin is ready.".into() },
        Alert { kind: "info", title: "Important", body: "The scheduler is still loading.".into() },
    ];
    let page = Design { chrome, backend: state.backend.base_url().to_string(), version: VERSION };
    Ok(Html(page.render()?))
}

#[derive(Template)]
#[template(path = "status.html")]
pub struct StatusPage {
    pub chrome: Chrome,
    pub panel: StatusPanel,
    pub charts: StatusCharts,
}

/// The cards, which refresh themselves when a status or plugin state changes (T33).
#[derive(Template)]
#[template(path = "status_panel.html")]
pub struct StatusPanel {
    pub backend: Level,
    pub backend_error: Option<String>,
    pub status: Option<Status>,
    /// What the plugins are sorted into, for the pill beside each in the Plugins card.
    pub categories: Vec<crate::backend::Category>,
    /// How the Plugins card is sorted, which its rows are already in.
    pub sort: PluginSort,
}

/// A column of the status page's Plugins card that it can be sorted by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SortColumn {
    #[default]
    Name,
    Category,
    State,
}

impl SortColumn {
    fn named(name: &str) -> Option<Self> {
        match name {
            "name" => Some(Self::Name),
            "category" => Some(Self::Category),
            "state" => Some(Self::State),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Name => "name",
            Self::Category => "category",
            Self::State => "state",
        }
    }
}

/// How the Plugins card is sorted: a column, A to Z or Z to A. It travels in the address, so the
/// card's live refresh keeps it and a link to the page shows it the same way. Anything the page
/// does not know is taken as the default, by name from A to Z, rather than refused.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PluginSort {
    pub column: SortColumn,
    pub descending: bool,
}

/// The address's `sort` and `order`, as they came.
#[derive(Debug, Default, Deserialize)]
pub struct SortQuery {
    #[serde(default)]
    sort: String,
    #[serde(default)]
    order: String,
}

impl From<SortQuery> for PluginSort {
    fn from(query: SortQuery) -> Self {
        Self {
            column: SortColumn::named(&query.sort).unwrap_or_default(),
            descending: query.order == "desc",
        }
    }
}

impl PluginSort {
    /// The address's query for this sort.
    pub fn query(self) -> String {
        let order = if self.descending { "desc" } else { "asc" };
        format!("sort={}&order={order}", self.column.name())
    }

    /// What pressing a column's heading sorts by: the other way round when it is the column
    /// sorted by already, and A to Z when it is another.
    fn pressed(self, column: SortColumn) -> Self {
        match self.column == column {
            true => Self { column, descending: !self.descending },
            false => Self { column, descending: false },
        }
    }

    /// Puts the plugins in this order. A plugin in no category comes after those in one whichever
    /// way round, and plugins that sort alike stay in name order.
    fn apply(self, plugins: &mut [PluginStatus], categories: &[crate::backend::Category]) {
        let key = |plugin: &PluginStatus| -> Option<String> {
            match self.column {
                SortColumn::Name => Some(plugin.id.to_lowercase()),
                SortColumn::Category => super::categories::pill(categories, &plugin.id)
                    .map(|pill| pill.name.to_lowercase()),
                SortColumn::State => Some(StatusPanel::lifecycle(plugin).to_lowercase()),
            }
        };
        plugins.sort_by(|a, b| {
            let first = match (key(a), key(b)) {
                (Some(a), Some(b)) if self.descending => b.cmp(&a),
                (Some(a), Some(b)) => a.cmp(&b),
                (Some(_), None) => std::cmp::Ordering::Less,
                (None, Some(_)) => std::cmp::Ordering::Greater,
                (None, None) => std::cmp::Ordering::Equal,
            };
            first.then_with(|| a.id.to_lowercase().cmp(&b.id.to_lowercase()))
        });
    }
}

impl StatusPanel {
    fn pill(&self, plugin: &str) -> Option<super::categories::Pill> {
        super::categories::pill(&self.categories, plugin)
    }

    fn column(name: &str) -> SortColumn {
        SortColumn::named(name).unwrap_or_default()
    }

    /// The query a column's heading leads to.
    fn sorted_by(&self, column: &str) -> String {
        self.sort.pressed(Self::column(column)).query()
    }

    /// `aria-sort` for the column sorted by, and nothing for the others.
    fn aria_sort(&self, column: &str) -> Option<&'static str> {
        let way = if self.sort.descending { "descending" } else { "ascending" };
        (self.sort.column == Self::column(column)).then_some(way)
    }

    /// What pressing a column's heading does, for a screen reader, which the arrows show.
    fn sort_hint(&self, column: &str) -> &'static str {
        match self.sort.pressed(Self::column(column)).descending {
            true => ", sort Z to A",
            false => ", sort A to Z",
        }
    }

    /// How long ago the backend ran these checks, since the page does not refresh itself yet.
    pub fn checked(when: &chrono::DateTime<chrono::Utc>) -> String {
        let seconds = (chrono::Utc::now() - *when).num_seconds().max(0);
        match seconds {
            0..=4 => "Checked just now".to_string(),
            5..=59 => format!("Checked {seconds} seconds ago"),
            60..=3599 => format!("Checked {} minutes ago", seconds / 60),
            _ => format!("Checked at {}", when.format("%H:%M UTC")),
        }
    }

    /// The badge modifier for a level, so the template stays free of match arms.
    pub fn badge(level: &Level) -> &'static str {
        match level {
            Level::Up => "up",
            Level::Degraded => "degraded",
            Level::Down => "down",
            Level::Unknown => "unknown",
        }
    }

    pub fn label(level: &Level) -> &'static str {
        match level {
            Level::Up => "Up",
            Level::Degraded => "Degraded",
            Level::Down => "Down",
            Level::Unknown => "Unknown",
        }
    }

    /// A plugin's own lifecycle word, which says more than its level: `Loading` and `Cancelled`
    /// are both degraded, but not for the same reason.
    pub fn lifecycle(plugin: &PluginStatus) -> String {
        match plugin.lifecycle.as_deref() {
            Some(state) => {
                let mut letters = state.chars();
                letters
                    .next()
                    .map(|first| first.to_uppercase().chain(letters).collect())
                    .unwrap_or_default()
            }
            None => "Not running".into(),
        }
    }
}

/// An unreachable backend is reported as a down component, not an error page, since that is the
/// answer the reader came for; a caller without `core` read access is told so instead.
async fn panel(
    state: &AppState,
    signed: &Signed,
    sort: PluginSort,
) -> Result<StatusPanel, WebError> {
    let backend = state.backend.clone();
    let token = signed.token().to_string();
    let answer = state.immediate.run(async move { Ok::<_, String>(backend.status(&token).await) });
    let failure = match answer.await {
        Ok(Ok(mut status)) => {
            let categories = crate::session::access(state, signed).await.categories;
            sort.apply(&mut status.plugins, &categories);
            return Ok(StatusPanel {
                backend: Level::Up,
                backend_error: None,
                status: Some(status),
                categories,
                sort,
            });
        }
        Ok(Err(err)) if err.status() == Some(403) => return Err(WebError::Backend(err)),
        Ok(Err(err)) => err.to_string(),
        Err(err) => err.to_string(),
    };
    tracing::warn!(err = %failure, "the backend did not answer the status request");
    Ok(StatusPanel {
        backend: Level::Down,
        backend_error: Some(failure),
        status: None,
        categories: Vec::new(),
        sort,
    })
}

pub async fn status(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Query(sort): Query<SortQuery>,
) -> Result<Html<String>, WebError> {
    let panel = panel(&state, &signed, sort.into()).await?;
    let charts = charts(&state, &signed).await;
    let chrome = Chrome::new("Status", "/status")
        .headed("Service status")
        .signed(&signed, &csrf)
        .with_plugins(&state, &signed)
        .await;
    Ok(Html(StatusPage { chrome, panel, charts }.render()?))
}

const CHART_HOURS: u32 = 6;

/// The dashboard's charts, each a Chart.js configuration its canvas carries in `data-chart`.
#[derive(Template)]
#[template(path = "status_charts.html")]
pub struct StatusCharts {
    pub latency: String,
    pub lag: String,
    pub problem: Option<String>,
}

/// One line per series over shared minute labels, so points checked seconds apart line up.
fn chart(
    series: &std::collections::BTreeMap<String, Vec<(String, u64)>>,
    labels: &[String],
    unit: &str,
) -> String {
    let datasets: Vec<serde_json::Value> = series
        .iter()
        .map(|(name, points)| {
            let at: std::collections::HashMap<&str, u64> =
                points.iter().map(|(label, value)| (label.as_str(), *value)).collect();
            let data: Vec<Option<u64>> = labels.iter().map(|label| at.get(label.as_str()).copied()).collect();
            serde_json::json!({ "label": name, "data": data, "spanGaps": true, "pointRadius": 0, "tension": 0.2 })
        })
        .collect();
    serde_json::json!({
        "type": "line",
        "data": { "labels": labels, "datasets": datasets },
        "options": {
            "animation": false,
            "interaction": { "mode": "index", "intersect": false },
            "scales": { "y": { "beginAtZero": true, "title": { "display": true, "text": unit } } },
            "plugins": { "legend": { "position": "bottom" } },
        },
    })
    .to_string()
}

impl StatusCharts {
    fn of(history: &History) -> Self {
        let label = |at: &chrono::DateTime<chrono::Utc>| at.format("%H:%M").to_string();
        let mut labels: Vec<String> =
            history.checks.iter().map(|check| label(&check.checked_at)).collect();
        labels.dedup();
        let mut latency = std::collections::BTreeMap::<String, Vec<(String, u64)>>::new();
        let mut lag = std::collections::BTreeMap::<String, Vec<(String, u64)>>::new();
        for check in &history.checks {
            let at = label(&check.checked_at);
            if let Some(ms) = check.latency_ms {
                latency.entry(check.name.clone()).or_default().push((at.clone(), ms));
            }
            for node in &check.nodes {
                if let Some(behind) = node.replication_lag {
                    lag.entry(node.address.clone()).or_default().push((at.clone(), behind));
                }
            }
        }
        Self {
            latency: chart(&latency, &labels, "milliseconds"),
            lag: chart(&lag, &labels, "log entries behind the leader"),
            problem: history
                .checks
                .is_empty()
                .then(|| "No checks have been recorded yet.".to_string()),
        }
    }
}

async fn charts(state: &AppState, signed: &Signed) -> StatusCharts {
    match state.backend.status_history(signed.token(), CHART_HOURS).await {
        Ok(history) => StatusCharts::of(&history),
        Err(err) => StatusCharts {
            latency: String::new(),
            lag: String::new(),
            problem: Some(format!("The history could not be read: {}", err.detail())),
        },
    }
}

pub async fn status_charts(
    State(state): State<AppState>,
    signed: Signed,
) -> Result<Html<String>, WebError> {
    Ok(Html(charts(&state, &signed).await.render()?))
}

pub async fn status_panel(
    State(state): State<AppState>,
    signed: Signed,
    Query(sort): Query<SortQuery>,
) -> Result<Html<String>, WebError> {
    Ok(Html(panel(&state, &signed, sort.into()).await?.render()?))
}
