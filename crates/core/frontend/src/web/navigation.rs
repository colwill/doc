//! The navigation: the pages on offer to the signed-in user, laid out the way an administrator
//! arranged them, and the page where a `core` writer arranges them. A layout orders, renames,
//! groups and hides pages and can add links of its own, but never shows a page to someone who
//! could not open it.

use askama::Template;
use axum::extract::{Extension, Form, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use http::StatusCode;

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::pages::{Chrome, NavItem, Page};
use crate::backend::{Access, Layout, LayoutEntry};
use crate::session::{self, Signed};

/// Empty rows the arranging page offers for links of the administrator's own.
const SPARE_ROWS: usize = 3;
/// The menu an administrator gives a page to keep it on the bar rather than in the menu it would
/// otherwise fall into. An entry naming no menu at all takes the page's own.
const NO_MENU: &str = "-";
const MAX_LABEL: usize = 60;

/// A page that can be in the navigation, and whether this user may open it.
#[derive(Debug, Clone, PartialEq)]
pub struct Offered {
    pub href: String,
    pub label: String,
    pub description: Option<String>,
    /// The menu it sits in until an administrator places it.
    pub group: Option<String>,
    pub visible: bool,
    /// Counted for access and breadcrumbs, but never drawn as a link in the bar.
    pub hidden: bool,
}

/// One of the platform's own pages.
pub struct CorePage {
    pub label: &'static str,
    pub href: &'static str,
    pub description: &'static str,
    pub group: Option<&'static str>,
}

const fn page(
    label: &'static str,
    href: &'static str,
    description: &'static str,
    group: &'static str,
) -> CorePage {
    CorePage { label, href, description, group: Some(group) }
}

/// The menus, in the order the bar shows them. A group named by a plugin and not here follows
/// them, and an entry in no group at all stands on its own, so a new plugin is never hidden.
pub const GROUPS: [&str; 6] =
    ["Workspace", "Platform", "Technology", "Access", "Watercooler", "Admin"];

/// The menu whose pages are footer links rather than anything in the bar.
pub const HELP: &str = "Help";
/// Everything in this menu is for platform administrators, and nobody else is shown it.
pub const ADMIN: &str = "Admin";

/// Where a group sits in the bar: the menus core knows, then any menu a plugin names of its own,
/// then the entries in no menu at all, with `Admin` last of all.
fn rank(group: Option<&str>) -> usize {
    const UNKNOWN: usize = GROUPS.len();
    match group {
        Some(ADMIN) => usize::MAX,
        Some(group) => GROUPS.iter().position(|known| *known == group).unwrap_or(UNKNOWN),
        None => UNKNOWN + 1,
    }
}

/// The platform's own pages in the bar, offered before any plugin's. The pages a person keeps to
/// themselves — the platform's health, their own tokens and the component gallery — are in the
/// footer instead (`FOOTER_PAGES`), so the bar holds only the work.
pub const CORE_PAGES: [CorePage; 9] = [
    page(
        "People",
        "/users",
        "Everyone DOC knows, each with every account they sign in with, and people to come",
        "Access",
    ),
    page(
        "Teams",
        "/teams",
        "Who works together: organisations, teams inside teams, and their people",
        "Access",
    ),
    page(
        "Service accounts",
        "/service-accounts",
        "Accounts for scripts and services, and their tokens",
        "Access",
    ),
    page("Navbar layout", "/navigation", "What the navigation bar holds, and in what order", ADMIN),
    page("All sections", "/landing", "The cards on the All sections page", ADMIN),
    page("Plugins", "/plugins", "What is loaded, its state and its errors", ADMIN),
    page("Settings", "/settings", "What this platform calls itself", ADMIN),
    page(
        "Set up DOC",
        "/setup",
        "A step at a time: sign-in, plugins, your tools and your data",
        ADMIN,
    ),
    page("Organisations", "/organisations", "Every organisation, and the teams inside it", ADMIN),
];

/// The platform's own footer links, in the order the footer shows them. `Help` pages a plugin
/// declares join them, so help is in the footer wherever it comes from.
pub const FOOTER_PAGES: [CorePage; 3] = [
    CorePage {
        label: "Status",
        href: "/status",
        description: "The health of the platform and everything it runs on",
        group: None,
    },
    CorePage {
        label: "Access tokens",
        href: "/tokens",
        description: "Your personal tokens, for the command line, scripts and MCP clients",
        group: None,
    },
    CorePage {
        label: "Design",
        href: "/design",
        description: "Every component a plugin's pages can use",
        group: Some(HELP),
    },
];

/// Offered after every plugin's, so the audit log is the last thing in the `Admin` menu.
pub const LAST_PAGES: [CorePage; 1] =
    [page("Audit log", "/audit", "Who changed what, and when", ADMIN)];

fn core_offered(pages: &[CorePage], access: &Access) -> Vec<Offered> {
    pages
        .iter()
        .map(|page| Offered {
            href: page.href.to_string(),
            label: page.label.to_string(),
            description: Some(page.description.to_string()),
            group: page.group.map(str::to_string),
            visible: core_visible(page.href, access),
            hidden: false,
        })
        .collect()
}

fn platform(access: &Access) -> bool {
    access.admin || access.plugins.get("core").is_some_and(|core| core.read)
}

fn core_visible(href: &str, access: &Access) -> bool {
    let identities = access.admin || access.plugins.get("rbac").is_some_and(|rbac| rbac.write);
    match href {
        // The `Admin` menu is for platform administrators, and nobody else is shown it. The pages
        // themselves still decide who may open them, so a core reader keeps `/plugins` by its URL.
        "/plugins" | "/audit" | "/navigation" | "/landing" | "/settings" | "/organisations"
        | "/setup" => access.admin,
        "/status" => platform(access),
        "/users" => identities,
        _ => true,
    }
}

/// Every entry a readable plugin offers: by menu, then by the name of each plugin's first entry in
/// that menu, and within one plugin in the order it declares them. So the bar reads the same
/// whatever order plugins registered in, while a plugin with a menu of its own — its sections, say
/// — keeps them in the order it means them to be read. A layout orders them itself, in `placed`.
fn plugin_offered(access: &Access) -> Vec<Offered> {
    let mut plugins: Vec<(usize, String, usize, Offered)> = Vec::new();
    for (id, entry) in access.plugins.iter().filter(|(_, entry)| entry.read) {
        let mut first: std::collections::BTreeMap<Option<String>, String> = Default::default();
        for nav in &entry.nav {
            first.entry(nav.group.clone()).or_insert_with(|| nav.label.clone());
        }
        for (at, nav) in entry.nav.iter().enumerate() {
            let leading = first.get(&nav.group).cloned().unwrap_or_else(|| nav.label.clone());
            let page = Offered {
                href: format!("/p/{id}{}", nav.path),
                label: nav.label.clone(),
                description: nav.description.clone(),
                group: nav.group.clone(),
                visible: true,
                hidden: nav.hidden,
            };
            plugins.push((rank(nav.group.as_deref()), leading, at, page));
        }
    }
    plugins.sort_by_key(|(rank, leading, at, _)| (*rank, leading.clone(), *at));
    plugins.into_iter().map(|(_, _, _, page)| page).collect()
}

/// Whether a page belongs in the footer rather than the bar.
fn helps(page: &Offered) -> bool {
    page.group.as_deref() == Some(HELP)
}

/// Every page this user could be offered in the bar: the platform's, then each plugin's, in the
/// order the menus are shown. The sort is stable, so within a menu the platform's own pages keep
/// the order they are written in and a plugin's keep theirs.
pub fn offered(access: &Access) -> Vec<Offered> {
    let mut offered = core_offered(&CORE_PAGES, access);
    offered.extend(plugin_offered(access).into_iter().filter(|page| !helps(page)));
    offered.extend(core_offered(&LAST_PAGES, access));
    offered.sort_by_key(|page| rank(page.group.as_deref()));
    offered
}

/// Every page the footer can hold: the platform's own, then the `Help` pages any plugin declares.
pub fn footer_offered(access: &Access) -> Vec<Offered> {
    let mut pages = core_offered(&FOOTER_PAGES, access);
    pages.extend(plugin_offered(access).into_iter().filter(helps));
    pages.retain(|page| page.visible);
    pages
}

/// The footer's links. A layout cannot order the footer, but it hides and renames its links the
/// same way it does the bar's, so one page arranges both.
pub fn footer(access: &Access, layout: Option<&Layout>) -> Vec<NavItem> {
    let entry = |href: &str| {
        layout.and_then(|layout| layout.entries.iter().find(|entry| entry.href == href))
    };
    footer_offered(access)
        .into_iter()
        .filter(|page| !entry(&page.href).is_some_and(|entry| entry.hidden))
        .map(|page| NavItem {
            label: entry(&page.href)
                .and_then(|entry| entry.label.clone())
                .unwrap_or_else(|| page.label.clone()),
            href: page.href,
            current: false,
            children: Vec::new(),
            end: false,
            hidden: false,
        })
        .collect()
}

/// A link of the administrator's own: a plugin's page needs that plugin to be readable and running,
/// and any other page decides for itself who may open it.
fn link_visible(href: &str, access: &Access) -> bool {
    let path = href.split(['?', '#']).next().unwrap_or_default();
    match path.strip_prefix("/p/") {
        Some(rest) => {
            let plugin = rest.split('/').next().unwrap_or_default();
            access.plugins.get(plugin).is_some_and(|entry| entry.read && entry.running)
        }
        None => core_visible(path, access),
    }
}

/// Whether `current` is at or below the page `href` points to.
fn under(current: &str, href: &str) -> bool {
    let path = href.split(['?', '#']).next().unwrap_or_default();
    let root = path.trim_end_matches('/');
    match root.is_empty() {
        true => current == "/",
        false => {
            current == root || current.strip_prefix(root).is_some_and(|rest| rest.starts_with('/'))
        }
    }
}

struct Placed {
    href: String,
    label: String,
    group: Option<String>,
    hidden: bool,
}

/// What the layout makes of the pages on offer, in order. A page the layout does not mention
/// comes after the ones it does, so a new plugin shows up before anyone arranges it.
fn placed(offered: &[Offered], layout: Option<&Layout>, access: &Access) -> Vec<Placed> {
    let entries = layout.map_or(&[][..], |layout| &layout.entries[..]);
    let mut shown = Vec::new();
    for entry in entries {
        let page = offered.iter().find(|page| page.href == entry.href);
        let visible = match page {
            Some(page) => page.visible,
            // Nothing offers it any more and nobody named it: a plugin's entry, from while it ran.
            None => entry.label.is_some() && link_visible(&entry.href, access),
        };
        let label = entry.label.clone().or_else(|| page.map(|page| page.label.clone()));
        // A menu is a default the layout may override, as a label is: an entry naming none takes
        // the page's own, so a layout arranged before a page had a menu still follows it, and
        // `NO_MENU` says to keep the page on the bar itself.
        let group = match entry.group.as_deref() {
            Some(NO_MENU) => None,
            Some(group) => Some(group.to_string()),
            None => page.and_then(|page| page.group.clone()),
        };
        // Anything in `Admin` is an administrator's, whether a plugin asked for that menu or a
        // layout put it there; the page itself still decides who may open it by its own URL.
        if group.as_deref() == Some(ADMIN) && !access.admin {
            continue;
        }
        if let (true, false, Some(label)) = (visible, entry.hidden, label) {
            let hidden = page.is_some_and(|page| page.hidden);
            shown.push(Placed { href: entry.href.clone(), label, group, hidden });
        }
    }
    // A page the layout never mentions - a plugin added since it was arranged, or one that has
    // just changed its menu - is put among its menu's pages where the plugins themselves would
    // have it, rather than below everything. An arranged menu keeps the order it was given.
    let offered_at = |href: &str| offered.iter().position(|page| page.href == href);
    for page in offered {
        if page.group.as_deref() == Some(ADMIN) && !access.admin {
            continue;
        }
        if !page.visible || entries.iter().any(|entry| entry.href == page.href) {
            continue;
        }
        let placed = Placed {
            href: page.href.clone(),
            label: page.label.clone(),
            group: page.group.clone(),
            hidden: page.hidden,
        };
        let mates: Vec<usize> = shown
            .iter()
            .enumerate()
            .filter(|(_, other)| other.group == placed.group)
            .map(|(at, _)| at)
            .collect();
        let mine = offered_at(&placed.href);
        let before = mates.iter().copied().find(|at| match offered_at(&shown[*at].href) {
            Some(theirs) => Some(theirs) > mine,
            // A page nothing offers any more keeps whatever place the layout gave it.
            None => false,
        });
        match (before, mates.last()) {
            (Some(at), _) => shown.insert(at, placed),
            (None, Some(last)) => shown.insert(last + 1, placed),
            (None, None) => shown.push(placed),
        }
    }
    shown
}

/// One step of the breadcrumbs; the last, the page itself, has no link.
#[derive(Debug, Clone, PartialEq)]
pub struct Crumb {
    pub label: String,
    pub href: Option<String>,
}

impl Crumb {
    pub fn linked(label: &str, href: &str) -> Self {
        Self { label: label.to_string(), href: Some(href.to_string()) }
    }

    pub fn here(label: &str) -> Self {
        Self { label: label.to_string(), href: None }
    }
}

/// A menu's own page, which lists what is in it. A menu is not a page anywhere else — a plugin
/// names one in its manifest and an administrator can call it anything — so its name becomes the
/// address here, and is read back the same way.
pub fn menu_slug(label: &str) -> String {
    let mut slug = String::new();
    for c in label.trim().to_lowercase().chars() {
        match c.is_ascii_alphanumeric() {
            true => slug.push(c),
            false if !slug.ends_with('-') => slug.push('-'),
            false => {}
        }
    }
    slug.trim_matches('-').to_string()
}

pub fn menu_href(label: &str) -> String {
    format!("/menu/{}", menu_slug(label))
}

/// Where a page sits, from the navigation: home, the menu it is in, the entry nearest it, then the
/// page. `between` goes after the entry, such as the plugin tab the page is under. A step naming
/// what the next one does is left out, so a section's own page reads "Home › Access tokens".
pub fn crumbs(nav: &[NavItem], between: Vec<Crumb>, title: &str) -> Vec<Crumb> {
    let mut trail = vec![Crumb::linked("Home", "/")];
    for item in nav.iter().filter(|item| item.current) {
        match item.is_group() {
            true => {
                // A menu has a page of its own listing what is in it, so the trail leads back up
                // to the whole of Workspace rather than stopping at a word.
                trail.push(Crumb::linked(&item.label, &menu_href(&item.label)));
                let entries = item.children.iter().filter(|child| child.current);
                trail.extend(entries.map(|child| Crumb::linked(&child.label, &child.href)));
            }
            false => trail.push(Crumb::linked(&item.label, &item.href)),
        }
    }
    trail.extend(between);
    trail.push(Crumb::here(title));
    // Said once each: a menu, the page in it, the tab it is on and the page's own heading often
    // share a name, and "Technology > Radar > Radar > Radar" tells nobody anything. The first of
    // each name is kept, since that is the one furthest up the trail, and whatever is left last is
    // the page itself: a plugin headed with its own name, on its Overview tab, reads
    // "CI/CD/CT metrics > Overview", not "Overview > CI/CD/CT metrics".
    let mut kept: Vec<Crumb> = Vec::with_capacity(trail.len());
    for crumb in trail {
        let said = kept.iter().any(|earlier| earlier.label.eq_ignore_ascii_case(&crumb.label));
        if !said {
            kept.push(crumb);
        }
    }
    if let Some(last) = kept.last_mut() {
        last.href = None;
    }
    kept
}

/// A page as the landing page offers it: on a card, in the navigation's order.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub label: String,
    pub href: String,
    pub description: Option<String>,
}

/// The cards the landing page opens with when nobody has chosen any: the pages people come to DOC
/// for. Any that this platform does not run are simply left out.
pub const DEFAULT_LANDING: [&str; 6] =
    ["/p/kb/", "/p/calendar/", "/p/service-map/", "/p/radar/", "/p/resources/", "/p/automation/"];

/// The landing page's cards: the ones an administrator chose, in their order, or the defaults.
/// A choice that nothing here offers falls back to every page, so the landing page is never bare.
pub fn landing(access: &Access) -> Vec<Card> {
    let all = cards(access);
    let chosen = access
        .navigation
        .as_ref()
        .map(|layout| layout.landing.clone())
        .filter(|landing| !landing.is_empty())
        .unwrap_or_else(|| DEFAULT_LANDING.iter().map(|href| (*href).to_string()).collect());
    let picked: Vec<Card> = chosen
        .iter()
        .filter_map(|href| all.iter().find(|card| card.href == *href).cloned())
        .collect();
    match picked.is_empty() {
        true => all,
        false => picked,
    }
}

/// Every page this user can open, laid out as the navigation is, without its menus.
pub fn cards(access: &Access) -> Vec<Card> {
    let offered = offered(access);
    placed(&offered, access.navigation.as_ref(), access)
        .into_iter()
        .map(|shown| Card {
            description: offered
                .iter()
                .find(|page| page.href == shown.href)
                .and_then(|page| page.description.clone()),
            label: shown.label,
            href: shown.href,
        })
        .collect()
}

/// The navigation bar: groups sit where their first entry would, and the entry nearest the
/// current page is the one marked current.
pub fn arrange(
    offered: &[Offered],
    layout: Option<&Layout>,
    access: &Access,
    current: &str,
) -> Vec<NavItem> {
    let shown = placed(offered, layout, access);
    let nearest = shown
        .iter()
        .filter(|entry| under(current, &entry.href))
        .max_by_key(|entry| entry.href.split(['?', '#']).next().unwrap_or_default().len())
        .map(|entry| entry.href.clone());
    let mut items: Vec<NavItem> = Vec::new();
    for entry in shown {
        let item = NavItem {
            current: nearest.as_deref() == Some(entry.href.as_str()),
            label: entry.label,
            href: entry.href,
            children: Vec::new(),
            end: false,
            hidden: entry.hidden,
        };
        let Some(group) = entry.group else {
            items.push(item);
            continue;
        };
        let at = items.iter().position(|existing| existing.is_group() && existing.label == group);
        let menu = match at {
            Some(at) => &mut items[at],
            None => {
                // `Admin` is held at the far end of the bar, away from everyone's own pages.
                let end = group == ADMIN;
                items.push(NavItem {
                    label: group,
                    href: String::new(),
                    current: false,
                    children: Vec::new(),
                    end,
                    hidden: false,
                });
                items.last_mut().expect("just pushed")
            }
        };
        // A menu is current for the pages in it, and for its own page.
        menu.current |= item.current || current == menu_href(&menu.label);
        menu.children.push(item);
    }
    // `Admin` goes last however the layout orders it, since the bar holds it at the far end and a
    // pinned entry in the middle of the row would push everything after it along with it.
    items.sort_by_key(|item| item.end);
    items
}

/// One page as the arranging page lists it.
#[derive(Debug)]
pub struct Row {
    pub href: String,
    /// The page's own name, or empty for a link of the administrator's own.
    pub offered: String,
    pub label: String,
    pub group: String,
    pub shown: bool,
    /// Whether anything offers it now; a link that nothing offers can be removed.
    pub known: bool,
    /// Whether the footer holds it, where it can be hidden or renamed but not ordered.
    pub footer: bool,
    /// Whether it was already in the saved layout. Such a row keeps whatever name it has, even
    /// none, so an arrangement is never held hostage by a page that has stopped being offered.
    pub kept: bool,
}

/// One thing the bar holds at its top level: a menu, or a page that stands on its own. Putting
/// these in order is putting the bar in order, which the pages below follow.
pub struct Top {
    /// The menu's name, or the page's address for one that stands alone.
    pub key: String,
    pub label: String,
    pub menu: bool,
}

#[derive(Template)]
#[template(path = "navigation.html")]
pub struct NavigationPage {
    pub chrome: Chrome,
    pub tops: Vec<Top>,
    pub rows: Vec<Row>,
    pub spare: Vec<usize>,
    pub groups: Vec<String>,
    pub error: Option<String>,
    pub saved: bool,
}

impl NavigationPage {
    /// Where the row is, counted from one, since the order is changed by dragging rather than by
    /// typing a number between two others.
    fn order(index: &usize) -> usize {
        index + 1
    }
}

/// The saved entries first, in their order, then every page on offer that they leave out.
/// Every page the arranging page lists: the bar's, then the footer's, which a layout hides and
/// renames but cannot order.
fn arrangeable(access: &Access) -> Vec<(Offered, bool)> {
    let mut pages: Vec<(Offered, bool)> =
        offered(access).into_iter().map(|page| (page, false)).collect();
    pages.extend(footer_offered(access).into_iter().map(|page| (page, true)));
    pages
}

fn rows(pages: &[(Offered, bool)], layout: &Layout) -> Vec<Row> {
    let found = |href: &str| pages.iter().find(|(page, _)| page.href == href);
    let mut rows: Vec<Row> = layout
        .entries
        .iter()
        .map(|entry| {
            let page = found(&entry.href);
            Row {
                href: entry.href.clone(),
                offered: page.map(|(page, _)| page.label.clone()).unwrap_or_default(),
                label: entry.label.clone().unwrap_or_default(),
                group: entry.group.clone().unwrap_or_default(),
                shown: !entry.hidden,
                known: page.is_some(),
                footer: page.is_some_and(|(_, footer)| *footer),
                kept: true,
            }
        })
        .collect();
    for (page, footer) in pages {
        if !layout.entries.iter().any(|entry| entry.href == page.href) {
            rows.push(Row {
                href: page.href.clone(),
                offered: page.label.clone(),
                label: String::new(),
                group: page.group.clone().unwrap_or_default(),
                shown: true,
                known: true,
                footer: *footer,
                kept: false,
            });
        }
    }
    rows
}

/// The menu a row's page ends up in: what the row says, then what the page asks for, and nothing
/// at all for a row kept on the bar itself.
fn menu_of(pages: &[(Offered, bool)], row: &Row) -> Option<String> {
    match row.group.as_str() {
        NO_MENU => None,
        "" => pages
            .iter()
            .find(|(page, _)| page.href == row.href)
            .and_then(|(page, _)| page.group.clone()),
        group => Some(group.to_string()),
    }
}

/// What the bar holds at its top level, in the order these rows put it: each menu where its first
/// page falls, and each page that stands on its own. `Admin` is left out, since it is always last.
fn tops(pages: &[(Offered, bool)], rows: &[Row]) -> Vec<Top> {
    let mut tops: Vec<Top> = Vec::new();
    for row in rows.iter().filter(|row| row.shown && !row.footer && !row.href.is_empty()) {
        let (key, label, menu) = match menu_of(pages, row) {
            Some(group) => (group.clone(), group, true),
            None => {
                let label = match row.label.is_empty() {
                    true => row.offered.clone(),
                    false => row.label.clone(),
                };
                (row.href.clone(), label, false)
            }
        };
        if key == ADMIN || tops.iter().any(|top| top.key == key) {
            continue;
        }
        tops.push(Top { key, label, menu });
    }
    tops
}

/// The caller's access, when they may arrange the navigation.
pub(super) async fn writes(state: &AppState, signed: &Signed) -> Option<Access> {
    let access = session::access(state, signed).await;
    let writes = access.admin || access.plugins.get("core").is_some_and(|core| core.write);
    writes.then_some(access)
}

pub(super) fn forbidden() -> Response {
    let page = Page::new("Forbidden", "Arranging the navigation needs plugin:core:user:rw.");
    match page.render_html() {
        Ok(html) => (StatusCode::FORBIDDEN, html).into_response(),
        Err(err) => err.into_response(),
    }
}

async fn render(
    state: &AppState,
    signed: &Signed,
    csrf: &Csrf,
    pages: &[(Offered, bool)],
    rows: Vec<Row>,
    error: Option<String>,
    saved: bool,
) -> Result<Response, WebError> {
    let chrome = Chrome::new("Navbar layout", "/navigation")
        .signed(signed, csrf)
        .with_plugins(state, signed)
        .await;
    // The menus core knows are always on offer, so a page can be moved into one that is empty.
    let mut groups: Vec<String> = rows
        .iter()
        .map(|row| row.group.clone())
        .filter(|group| !group.is_empty())
        .chain(GROUPS.iter().map(|group| (*group).to_string()))
        .chain(std::iter::once(NO_MENU.to_string()))
        .collect();
    groups.sort();
    groups.dedup();
    let spare = (rows.len()..rows.len() + SPARE_ROWS).collect();
    let status = if error.is_some() { StatusCode::BAD_REQUEST } else { StatusCode::OK };
    let tops = tops(pages, &rows);
    let page = NavigationPage { chrome, tops, rows, spare, groups, error, saved };
    Ok((status, Html(page.render()?)).into_response())
}

#[derive(Debug, serde::Deserialize)]
pub struct Saved {
    #[serde(default)]
    saved: Option<String>,
}

impl Saved {
    pub(super) fn is_saved(&self) -> bool {
        self.saved.is_some()
    }
}

pub async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Query(saved): Query<Saved>,
) -> Result<Response, WebError> {
    let Some(access) = writes(&state, &signed).await else {
        return Ok(forbidden());
    };
    let layout = state.backend.navigation(signed.token()).await?;
    let pages = arrangeable(&access);
    let rows = rows(&pages, &layout);
    render(&state, &signed, &csrf, &pages, rows, None, saved.saved.is_some()).await
}

struct Submitted {
    order: usize,
    at: usize,
    row: Row,
}

fn field<'a>(form: &'a [(String, String)], name: &str, at: usize) -> Option<&'a str> {
    let key = format!("{name}.{at}");
    form.iter().find(|(field, _)| *field == key).map(|(_, value)| value.trim())
}

/// The rows as sent, in the order asked for; rows with no page, or marked removed, are dropped.
fn submitted(form: &[(String, String)]) -> Result<Vec<Row>, String> {
    let mut rows = Vec::new();
    let indices =
        form.iter().filter_map(|(name, _)| name.strip_prefix("href.")?.parse::<usize>().ok());
    for at in indices {
        let href = field(form, "href", at).unwrap_or_default().to_string();
        if href.is_empty() || field(form, "remove", at).is_some() {
            continue;
        }
        let label = field(form, "label", at).unwrap_or_default().to_string();
        let offered = field(form, "offered", at).unwrap_or_default().to_string();
        // Only a link being written here needs a name. One already in the layout keeps what it
        // has, so a page that has stopped being offered cannot block the whole arrangement.
        let kept = field(form, "kept", at).is_some();
        if offered.is_empty() && label.is_empty() && !kept {
            return Err(format!("Give {href} a label: nothing else names it."));
        }
        if label.chars().count() > MAX_LABEL {
            return Err(format!("A label is at most {MAX_LABEL} characters: {label:?} is longer."));
        }
        let order = match field(form, "order", at).unwrap_or_default() {
            "" => usize::MAX,
            text => {
                text.parse().map_err(|_| format!("The order of {href} is not a whole number."))?
            }
        };
        let row = Row {
            href,
            known: !offered.is_empty(),
            offered,
            label,
            group: field(form, "group", at).unwrap_or_default().to_string(),
            shown: field(form, "shown", at).is_some(),
            footer: false,
            kept,
        };
        rows.push(Submitted { order, at, row });
    }
    rows.sort_by_key(|submitted| (submitted.order, submitted.at));
    Ok(rows.into_iter().map(|submitted| submitted.row).collect())
}

/// The bar's entries as arranged, keeping the landing page's cards, which the one layout holds too.
/// The top level as the menus table sends it back: each menu's name, or a standalone page's
/// address, in the order asked for.
fn menus(form: &[(String, String)]) -> Vec<String> {
    let mut asked: Vec<(usize, usize, String)> = Vec::new();
    let indices =
        form.iter().filter_map(|(name, _)| name.strip_prefix("top.")?.parse::<usize>().ok());
    for at in indices {
        let Some(key) = field(form, "top", at).filter(|key| !key.is_empty()) else { continue };
        let order = field(form, "toporder", at)
            .and_then(|order| order.parse::<usize>().ok())
            .unwrap_or(usize::MAX);
        asked.push((order, at, key.to_string()));
    }
    asked.sort_by_key(|(order, at, _)| (*order, *at));
    asked.into_iter().map(|(_, _, key)| key).collect()
}

/// The rows with each menu's pages kept together and the menus in the order the top table asks
/// for, so the bar follows it. Anything that table does not name keeps its place after the rest.
fn by_menu(pages: &[(Offered, bool)], rows: Vec<Row>, menus: &[String]) -> Vec<Row> {
    if menus.is_empty() {
        return rows;
    }
    let mut rows = rows;
    // Stable, so pages keep the order they were given within their own menu.
    rows.sort_by_key(|row| {
        let key = menu_of(pages, row).unwrap_or_else(|| row.href.clone());
        menus.iter().position(|menu| *menu == key).unwrap_or(menus.len())
    });
    rows
}

/// What was arranged, as a layout. A name or a menu is written down only where it differs from
/// what the page itself says: a layout holds the decisions somebody made, not a copy of every
/// default, so a page that is later renamed or moved to another menu by its plugin follows along
/// instead of being held to what it was called the day the bar was arranged.
fn layout_of(pages: &[(Offered, bool)], rows: &[Row], landing: Vec<String>) -> Layout {
    let text = |text: &str| Some(text.to_string()).filter(|text| !text.is_empty());
    let offered = |href: &str| pages.iter().map(|(page, _)| page).find(|page| page.href == href);
    Layout {
        entries: rows
            .iter()
            .map(|row| {
                let page = offered(&row.href);
                let named = page.is_some_and(|page| page.label == row.label);
                let same_menu =
                    page.is_some_and(|page| page.group.clone().unwrap_or_default() == row.group);
                LayoutEntry {
                    href: row.href.clone(),
                    label: match named {
                        true => None,
                        false => text(&row.label),
                    },
                    group: match same_menu {
                        true => None,
                        false => text(&row.group),
                    },
                    hidden: !row.shown,
                }
            })
            .collect(),
        landing,
    }
}

pub async fn save(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Form(form): Form<Vec<(String, String)>>,
) -> Result<Response, WebError> {
    let Some(access) = writes(&state, &signed).await else {
        return Ok(forbidden());
    };
    let pages = arrangeable(&access);
    let reset = form.iter().any(|(name, _)| name == "reset");
    let rows = match (reset, submitted(&form)) {
        (true, _) => Vec::new(),
        (false, Ok(rows)) => by_menu(&pages, rows, &menus(&form)),
        (false, Err(problem)) => {
            let layout = state.backend.navigation(signed.token()).await?;
            let rows = rows(&pages, &layout);
            return render(&state, &signed, &csrf, &pages, rows, Some(problem), false).await;
        }
    };
    let landing = state.backend.navigation(signed.token()).await?.landing;
    match state.backend.set_navigation(signed.token(), &layout_of(&pages, &rows, landing)).await {
        Ok(_) => {}
        Err(err) if err.status() == Some(400) => {
            return render(&state, &signed, &csrf, &pages, rows, Some(err.detail()), false).await;
        }
        Err(err) => return Err(err.into()),
    }
    session::forget_access(&state).await;
    Ok(Redirect::to("/navigation?saved=1").into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{NavEntry, PluginAccess};

    /// A plugin as these tests describe it: its ID, then each entry's label, path and menu.
    type Plugin<'a> = (&'a str, &'a [(&'a str, &'a str, Option<&'a str>)]);

    fn access(admin: bool, readable: &[Plugin<'_>]) -> Access {
        let plugins = readable
            .iter()
            .map(|(id, nav)| {
                let nav = nav.iter().map(|(label, path, group)| NavEntry {
                    label: (*label).into(),
                    path: (*path).into(),
                    description: None,
                    group: group.map(Into::into),
                    hidden: false,
                });
                (
                    (*id).to_string(),
                    PluginAccess {
                        read: true,
                        running: true,
                        nav: nav.collect(),
                        ..PluginAccess::default()
                    },
                )
            })
            .collect();
        Access { admin, plugins, ..Access::default() }
    }

    fn entry(href: &str, label: Option<&str>, group: Option<&str>) -> LayoutEntry {
        LayoutEntry {
            href: href.into(),
            label: label.map(Into::into),
            group: group.map(Into::into),
            hidden: false,
        }
    }

    fn labels(items: &[NavItem]) -> Vec<String> {
        items
            .iter()
            .map(|item| match item.is_group() {
                true => format!(
                    "{}[{}]",
                    item.label,
                    item.children
                        .iter()
                        .map(|child| child.label.as_str())
                        .collect::<Vec<_>>()
                        .join(",")
                ),
                false => item.label.clone(),
            })
            .collect()
    }

    #[test]
    fn with_no_layout_every_page_falls_into_the_menu_it_names() {
        let access = access(true, &[("rbac", &[("Access control", "/", Some("Access"))])]);
        let items = arrange(&offered(&access), None, &access, "/plugins");
        assert_eq!(
            labels(&items),
            [
                "Access[People,Teams,Service accounts,Access control]",
                "Admin[Navbar layout,All sections,Plugins,Settings,Set up DOC,Organisations,Audit log]"
            ],
            "a plugin joins the menu it names, and `Admin` is last whatever else there is"
        );
        let footer: Vec<String> =
            footer(&access, None).into_iter().map(|item| item.label).collect();
        assert_eq!(
            footer,
            ["Status", "Access tokens", "Design"],
            "the platform's health, your own tokens and help are footer links, not bar entries"
        );
        let admin = items.last().expect("the admin menu");
        assert!(admin.current, "the menu holding the page being looked at is current");
        assert!(admin.children[2].current, "and the page itself within it");
        assert!(items.iter().all(|item| item.href != "/"), "the logo leads home, not the bar");
    }

    #[test]
    fn an_access_menu_gathers_pages_renames_them_and_adds_a_link_of_its_own() {
        let access = access(true, &[("rbac", &[("Access control", "/", Some("Access"))])]);
        let layout = Layout {
            entries: vec![
                entry("/", None, None),
                entry("/p/rbac/people", Some("Users"), Some("Access")),
                entry("/tokens", Some("Access tokens"), Some("Access")),
                entry("/service-accounts", None, None),
                entry("/p/rbac/", None, None),
                entry("/teams", None, Some(NO_MENU)),
                LayoutEntry { hidden: true, ..entry("/status", None, None) },
            ],
            ..Layout::default()
        };
        let items = arrange(&offered(&access), Some(&layout), &access, "/p/rbac/people");
        assert_eq!(
            labels(&items),
            [
                "Access[Users,Access tokens,People,Service accounts,Access control]",
                "Teams",
                "Admin[Navbar layout,All sections,Plugins,Settings,Set up DOC,Organisations,Audit log]"
            ],
            "an entry naming no menu takes the page's own, so a layout arranged before menus \
             existed follows them; `-` keeps Teams on the bar; core's own People page, which this \
             layout never mentions, goes among the Access pages where core offers it rather than \
             below them; the old Overview entry, which nothing offers now, is left out; and \
             `/tokens`, which core no longer offers, is a link of the administrator's own"
        );
        let menu = &items[0];
        assert!(menu.current, "the menu holding the current page is current");
        assert!(menu.children[0].current, "the nearest page is current, not the plugin's root");
        assert!(!menu.children[3].current);
    }

    #[test]
    fn a_layout_never_shows_a_page_its_viewer_cannot_open() {
        let admin = access(true, &[("rbac", &[("Access control", "/", Some("Access"))])]);
        let layout = Layout {
            entries: vec![
                entry("/status", None, Some("Platform")),
                entry("/p/rbac/", None, Some("Access")),
                entry("/p/rbac/people", Some("Users"), Some("Access")),
                entry("/p/gone/", None, None),
            ],
            ..Layout::default()
        };
        let plain = access(false, &[]);
        let items = arrange(&offered(&plain), Some(&layout), &plain, "/");
        assert_eq!(
            labels(&items),
            ["Access[Teams,Service accounts]"],
            "the `Admin` menu is for platform administrators, and nobody else is shown it"
        );

        let items = arrange(&offered(&admin), Some(&layout), &admin, "/");
        assert!(
            !labels(&items).iter().any(|label| label.contains("gone")),
            "an unnamed entry nothing offers any more is left out"
        );
    }

    #[test]
    fn a_link_to_a_plugin_that_stopped_goes_with_it() {
        let mut access = access(true, &[("hello", &[("Hello", "/", Some("Help"))])]);
        let layout = Layout {
            entries: vec![entry("/p/hello/greetings", Some("Greetings"), None)],
            ..Layout::default()
        };
        let shown =
            |access: &Access| labels(&arrange(&offered(access), Some(&layout), access, "/"));
        assert!(shown(&access).contains(&"Greetings".to_string()));

        let hello = access.plugins.get_mut("hello").expect("hello");
        hello.running = false;
        hello.nav.clear();
        assert!(!shown(&access).iter().any(|label| label == "Greetings" || label == "Hello"));
    }

    #[test]
    fn breadcrumbs_follow_the_navigation_down_to_the_page() {
        let access = access(true, &[("rbac", &[("Access control", "/", Some("Access"))])]);
        let layout = Layout {
            entries: vec![
                entry("/p/rbac/", None, Some("Access")),
                entry("/tokens", Some("Access tokens"), Some("Access")),
            ],
            ..Layout::default()
        };
        let labels = |crumbs: &[Crumb]| {
            crumbs
                .iter()
                .map(|crumb| match &crumb.href {
                    Some(href) => format!("{}({href})", crumb.label),
                    None => crumb.label.clone(),
                })
                .collect::<Vec<_>>()
        };

        let nav = arrange(&offered(&access), Some(&layout), &access, "/p/rbac/principals/user/1");
        let people = vec![Crumb::linked("People and accounts", "/p/rbac/people")];
        assert_eq!(
            labels(&crumbs(&nav, people, "t20admin")),
            [
                "Home(/)",
                // A menu leads to its own page, which lists what is in it.
                "Access(/menu/access)",
                "Access control(/p/rbac/)",
                "People and accounts(/p/rbac/people)",
                "t20admin"
            ]
        );

        let nav = arrange(&offered(&access), Some(&layout), &access, "/tokens");
        assert_eq!(
            labels(&crumbs(&nav, Vec::new(), "Access tokens")),
            ["Home(/)", "Access(/menu/access)", "Access tokens"],
            "a section's own page is not named twice"
        );

        let nav = arrange(&offered(&access), None, &access, "/search");
        assert_eq!(labels(&crumbs(&nav, Vec::new(), "Search")), ["Home(/)", "Search"]);

        // A menu, the page in it, the tab it is on and its heading often share a name; the trail
        // says it once, at the depth it was reached.
        let seen = tests::access(false, &[("radar", &[("Radar", "/", Some("Technology"))])]);
        let nav = arrange(&offered(&seen), None, &seen, "/p/radar/");
        let tab = vec![Crumb::linked("Radar", "/p/radar/")];
        assert_eq!(
            labels(&crumbs(&nav, tab, "Radar")),
            ["Home(/)", "Technology(/menu/technology)", "Radar"],
            "the plugin's own tab and heading repeat its navigation entry, so they are not said again"
        );

        // Headed with its own name on a tab called something else, the tab is where the page is.
        let seen =
            tests::access(false, &[("cicd", &[("CI/CD/CT metrics", "/", Some("Platform"))])]);
        let nav = arrange(&offered(&seen), None, &seen, "/p/cicd/");
        let tab = vec![Crumb::linked("Overview", "/p/cicd/")];
        assert_eq!(
            labels(&crumbs(&nav, tab, "CI/CD/CT metrics")),
            ["Home(/)", "Platform(/menu/platform)", "CI/CD/CT metrics(/p/cicd/)", "Overview"],
        );
    }

    #[test]
    fn the_arranging_form_is_read_in_the_order_asked_for() {
        let form: Vec<(String, String)> = [
            ("href.0", "/"),
            ("offered.0", "Overview"),
            ("order.0", "30"),
            ("shown.0", "on"),
            ("href.1", "/tokens"),
            ("offered.1", "Access tokens"),
            ("order.1", "10"),
            ("group.1", "Access"),
            ("href.2", "/p/rbac/people"),
            ("label.2", "Users"),
            ("order.2", "20"),
            ("group.2", "Access"),
            ("shown.2", "on"),
            ("href.3", ""),
            ("href.4", "/old"),
            ("label.4", "Old"),
            ("remove.4", "on"),
        ]
        .iter()
        .map(|(name, value)| ((*name).to_string(), (*value).to_string()))
        .collect();
        // A page left where its plugin puts it, and called what its plugin calls it, is written
        // down as neither, so renaming it there later reaches everyone who arranged the bar.
        let offered = vec![(
            Offered {
                href: "/p/radar/".into(),
                label: "Radar".into(),
                description: None,
                group: Some("Technology".into()),
                visible: true,
                hidden: false,
            },
            false,
        )];
        let left_alone: Vec<Row> = vec![Row {
            href: "/p/radar/".into(),
            offered: "Radar".into(),
            label: "Radar".into(),
            group: "Technology".into(),
            shown: true,
            known: true,
            footer: false,
            kept: false,
        }];
        assert_eq!(
            layout_of(&offered, &left_alone, Vec::new()).entries,
            vec![entry("/p/radar/", None, None)],
            "what was not changed is not written down"
        );

        // Nothing offers these pages in this test, so every name and menu is a decision.
        let layout = layout_of(&[], &submitted(&form).unwrap(), Vec::new());
        assert_eq!(
            layout.entries,
            vec![
                LayoutEntry { hidden: true, ..entry("/tokens", None, Some("Access")) },
                entry("/p/rbac/people", Some("Users"), Some("Access")),
                entry("/", None, None),
            ]
        );
    }

    #[test]
    fn a_menu_is_drawn_as_a_disclosure_holding_its_pages() {
        let access = access(true, &[("rbac", &[("Access control", "/", Some("Access"))])]);
        let layout = Layout {
            entries: vec![
                entry("/p/rbac/people", Some("Users"), Some("Access")),
                entry("/tokens", Some("Access tokens"), Some("Access")),
            ],
            ..Layout::default()
        };
        let mut chrome = Chrome::new("Access tokens", "/tokens");
        chrome.user = Some("admin".into());
        chrome.nav = arrange(&offered(&access), Some(&layout), &access, "/tokens");
        let html = Page::within(chrome, "Access tokens", "").render().unwrap();
        let menu = &html[html.find("doc-menu__details").expect("a menu")..];
        let menu = &menu[..menu.find("</details>").expect("closed")];
        assert!(menu.contains(r#"aria-current="true">Access</summary>"#), "{menu}");
        assert!(
            menu.contains(r#"<a class="doc-menu__link" href="/p/rbac/people">Users</a>"#),
            "{menu}"
        );
        assert!(menu.contains(r#"href="/tokens" aria-current="page">Access tokens</a>"#), "{menu}");
    }

    #[test]
    fn a_link_of_ones_own_needs_a_label_but_one_already_arranged_does_not() {
        let form: Vec<(String, String)> =
            vec![("href.0".into(), "/p/rbac/people".into()), ("shown.0".into(), "on".into())];
        assert!(submitted(&form).unwrap_err().contains("label"));

        let mut kept = form.clone();
        kept.push(("kept.0".into(), "1".into()));
        let held = submitted(&kept).expect("a row already in the layout keeps what it has");
        assert_eq!(
            held[0].href, "/p/rbac/people",
            "a page that stopped being offered is held on to, so one plugin being down cannot \
             block the whole arrangement"
        );

        let layout = Layout {
            entries: vec![entry("/status", None, None), entry("/users", None, None)],
            ..Layout::default()
        };
        let pages = arrangeable(&access(true, &[]));
        let arranged = rows(&pages, &layout);
        let status = arranged.iter().find(|row| row.href == "/status").expect("status is listed");
        assert!(
            status.footer && status.offered == "Status",
            "a page the footer holds is listed by its own name, to be hidden or renamed there"
        );
        assert!(arranged.iter().any(|row| row.href == "/users" && !row.footer));
    }
}
