//! Plugin UIs: `/p/<plugin>/<path>` is the plugin's `ui/<path>` route, fetched as the signed-in
//! user. HTMX requests get the bare fragment, page loads get it inside the layout.
//!
//! A plugin can also have a name of its own — `rbac.rundoc.sh` for `rundoc.sh/p/rbac/` — which the
//! DNS plugin puts in the zone for every plugin it finds. `by_host` is what makes those names
//! arrive somewhere: see it for why they redirect rather than serve the page where they stand.

use std::sync::OnceLock;

use askama::Template;
use axum::body::{Bytes, to_bytes};
use axum::extract::{Extension, Path, Request, State};
use axum::middleware::Next;
use axum::response::{Html, IntoResponse, Redirect, Response};
use http::header::CONTENT_TYPE;
use http::{HeaderName, HeaderValue, StatusCode};
use serde_json::Value;

use super::AppState;
use super::csrf::Csrf;
use super::pages::{Chrome, Page};
use crate::backend::Forwarded;
use crate::session::{Signed, to_sign_in};

/// As much as the backend takes for a plugin, so a zipped repository or an archive of docs gets
/// through to the plugin that reads it.
const MAX_BODY: usize = 16 * 1024 * 1024;
/// A plugin's answer naming the plugin whose faux data it shows (DOC-SPEC §9.2).
const FAUX_HEADER: &str = "x-doc-faux";

/// Where the platform is reached from outside, and the host part of it, worked out once. Unset
/// leaves everything as it was: nothing is redirected and no plugin has a name of its own.
fn public() -> Option<&'static (String, String)> {
    static PUBLIC: OnceLock<Option<(String, String)>> = OnceLock::new();
    PUBLIC
        .get_or_init(|| {
            let url = std::env::var("DOC_PUBLIC_URL").ok()?;
            let url = url.trim().trim_end_matches('/').to_string();
            let host = url.parse::<http::Uri>().ok()?.host()?.to_ascii_lowercase();
            (!host.is_empty()).then_some((url, host))
        })
        .as_ref()
}

/// The plugin a request's host names, where it names one: `rbac.rundoc.sh` is `rbac`, and the
/// platform's own host is nobody. A label is a plugin ID, so the same characters an ID may have.
fn plugin_of(host: &str, public_host: &str) -> Option<String> {
    let host = host.split(':').next()?.trim_end_matches('.').to_ascii_lowercase();
    let label = host.strip_suffix(public_host)?.strip_suffix('.')?;
    let named = !label.is_empty()
        && !label.contains('.')
        && label.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    named.then(|| label.to_string())
}

/// A plugin's own name, sent to that plugin's page on the platform's own host.
///
/// This redirects rather than serving the page where it stands, and that is the whole of the
/// reason: every cookie the frontend sets is host-only (see `cookie.rs`), so a session on
/// `rundoc.sh` is not sent to `rbac.rundoc.sh`. Serving the app under each plugin's name would
/// mean signing in again for every one of them, or widening the session to the whole domain so
/// that any plugin's name could act with it. A redirect costs one hop and changes neither.
pub async fn by_host(request: Request, next: Next) -> Response {
    let Some((base, public_host)) = public() else { return next.run(request).await };
    let host = request
        .headers()
        .get(http::header::HOST)
        .and_then(|host| host.to_str().ok())
        .map(str::to_string)
        .or_else(|| request.uri().host().map(str::to_string));
    let Some(plugin) = host.as_deref().and_then(|host| plugin_of(host, public_host)) else {
        return next.run(request).await;
    };
    let path = request.uri().path().trim_start_matches('/');
    let query = request.uri().query().map(|query| format!("?{query}")).unwrap_or_default();
    // Permanent, and kept a permanent redirect for POST too: a form sent to a plugin's own name
    // is repeated to the page it belongs on rather than quietly becoming a GET.
    Redirect::permanent(&format!("{base}/p/{plugin}/{path}{query}")).into_response()
}

#[derive(Template)]
#[template(path = "plugin.html")]
pub struct PluginPage {
    pub chrome: Chrome,
    /// Whose faux data the page shows, said above everything else on it.
    pub faux: Option<Faux>,
    pub fragment: String,
    /// What the page's heading row offered beside its heading, drawn in the header band.
    pub actions: Option<String>,
    /// The accounts the plugin needs that the viewer has not linked, each offered to link.
    pub missing: Vec<super::me::Offer>,
    /// This page, to come back to after linking.
    pub here: String,
}

/// The providers `plugin` needs an account with that the viewer has not linked, and can link now.
async fn missing(state: &AppState, signed: &Signed, plugin: &str) -> Vec<super::me::Offer> {
    let access = crate::session::access(state, signed).await;
    let Some(entry) = access.plugins.get(plugin) else { return Vec::new() };
    let label = super::me::plugin_label(plugin, entry);
    entry
        .links
        .iter()
        .filter(|provider| !access.linked.contains_key(*provider))
        .filter(|provider| access.plugins.get(*provider).is_some_and(|provider| provider.running))
        .map(|provider| super::me::Offer {
            provider: provider.clone(),
            name: super::auth::provider_name(provider),
            needed_by: vec![label.clone()],
        })
        .collect()
}

/// A page showing faux data (FIX-FAUX-DATA): which plugin made it up, which plugin shows it, and
/// what the viewer may open of the first. Said by the platform so no plugin words it differently,
/// and only for a provider that is a registered plugin, so a page cannot put any text it likes in
/// the banner.
#[derive(Template)]
#[template(path = "faux.html")]
pub struct Faux {
    pub provider: String,
    pub plugin: String,
    /// The viewer may open the provider's page, and its Settings page.
    pub readable: bool,
    pub settings: bool,
    /// A resource panel, which gets one line rather than a banner.
    pub compact: bool,
}

async fn faux(
    state: &AppState,
    signed: &Signed,
    plugin: &str,
    answer: &Forwarded,
    compact: bool,
) -> Option<Faux> {
    let provider = answer
        .headers
        .iter()
        .find(|(name, _)| name == FAUX_HEADER)
        .map(|(_, value)| value.trim().to_string())?;
    let access = crate::session::access(state, signed).await;
    let entry = access.plugins.get(&provider)?;
    Some(Faux {
        readable: entry.read && entry.running,
        settings: entry.settings,
        provider,
        plugin: plugin.to_string(),
        compact,
    })
}

/// A resource panel is asked for with the resource it is on.
fn panel(uri: &http::Uri) -> bool {
    url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .any(|(name, _)| name == "resource")
}

fn forwardable(name: &str) -> bool {
    matches!(name, "accept" | "accept-language" | "content-type") || name.starts_with("hx-")
}

fn returnable(name: &str) -> bool {
    matches!(
        name,
        "content-type"
            | "cache-control"
            | "vary"
            | "content-disposition"
            | "x-content-type-options"
    ) || name.starts_with("hx-")
}

pub async fn root(
    state: State<AppState>,
    signed: Signed,
    csrf: Extension<Csrf>,
    Path(plugin): Path<String>,
    request: Request,
) -> Response {
    show(state, signed, csrf, plugin, String::new(), request).await
}

pub async fn below(
    state: State<AppState>,
    signed: Signed,
    csrf: Extension<Csrf>,
    Path((plugin, path)): Path<(String, String)>,
    request: Request,
) -> Response {
    show(state, signed, csrf, plugin, path, request).await
}

/// An ambient fragment such as the header bell asks with this, so a plugin that is not running,
/// not reached, or otherwise refuses just leaves its slot empty rather than showing an error
/// where the user never asked to open anything.
fn quiet(uri: &http::Uri) -> bool {
    url::form_urlencoded::parse(uri.query().unwrap_or_default().as_bytes())
        .any(|(name, value)| name == "quiet" && value != "0" && value != "false")
}

async fn show(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    plugin: String,
    path: String,
    request: Request,
) -> Response {
    let (parts, body) = request.into_parts();
    let htmx = parts.headers.contains_key("hx-request");
    let failing = Failing {
        state: &state,
        signed: &signed,
        csrf: &csrf,
        uri: &parts.uri,
        htmx,
        quiet: quiet(&parts.uri),
    };
    let Ok(body) = to_bytes(body, MAX_BODY).await else {
        return failing.respond(413, "That is more than a page may send.").await;
    };
    let query = parts.uri.query().map(|query| format!("?{query}")).unwrap_or_default();
    let target = format!("api/v1/plugins/{plugin}/ui/{path}{query}");
    let headers = parts
        .headers
        .iter()
        .filter(|(name, _)| forwardable(name.as_str()))
        .filter_map(|(name, value)| Some((name.to_string(), value.to_str().ok()?.to_string())))
        .collect();
    let answer = match state
        .backend
        .forward(signed.token(), parts.method.clone(), &target, headers, body)
        .await
    {
        Ok(answer) => answer,
        Err(err) => {
            let message = format!("{plugin} could not be reached: {err}");
            return failing.respond(502, &message).await;
        }
    };
    let html = answer
        .headers
        .iter()
        .any(|(name, value)| name == "content-type" && value.starts_with("text/html"));
    match answer.status {
        200..=299 if htmx && html => {
            let answer = reheaded(answer);
            let faux = faux(&state, &signed, &plugin, &answer, panel(&parts.uri)).await;
            match faux.and_then(|faux| faux.render().ok()) {
                Some(banner) => passed_on(prefixed(answer, &banner)),
                None => passed_on(answer),
            }
        }
        200..=299 if htmx || !html => passed_on(answer),
        200..=299 => {
            let faux = faux(&state, &signed, &plugin, &answer, false).await;
            let chrome = Chrome::new(title(&state, &signed, &plugin).await, parts.uri.path())
                .signed(&signed, &csrf)
                .with_plugins(&state, &signed)
                .await;
            let fragment = String::from_utf8_lossy(&answer.body).into_owned();
            let (heading, actions, fragment) = lift_heading(&fragment);
            let chrome = chrome.within_plugin(heading.as_deref(), &fragment);
            let missing = missing(&state, &signed, &plugin).await;
            let here = parts.uri.path_and_query().map_or("/", |here| here.as_str()).to_string();
            match (PluginPage { chrome, faux, fragment, actions, missing, here }).render() {
                Ok(page) => Html(page).into_response(),
                Err(err) => super::error::WebError::from(err).into_response(),
            }
        }
        401 => to_sign_in(&parts),
        status => {
            let message = explain(&plugin, status, &answer.body);
            failing.respond(status, &message).await
        }
    }
}

/// The plugin's own navigation label names its pages; its ID does when it has none.
async fn title(state: &AppState, signed: &Signed, plugin: &str) -> String {
    let access = crate::session::access(state, signed).await;
    let label =
        access.plugins.get(plugin).and_then(|entry| entry.nav.first()).map(|nav| nav.label.clone());
    label.unwrap_or_else(|| plugin.to_string())
}

/// A fragment with its heading row taken out and sent up to the band instead.
///
/// The band sits outside whatever HTMX swaps, so a swap that lands on a different page — the
/// model a plugin has just written, say — would leave the page before it named at the top. The
/// heading and its actions go up out of band, by the ids the layout puts on them, so the band
/// follows the content it heads. Where the swap is the same page again the band is simply
/// rewritten as it was, and either way the row is not drawn twice.
fn reheaded(mut answer: Forwarded) -> Forwarded {
    let Ok(body) = std::str::from_utf8(&answer.body) else { return answer };
    let Some((at, heading)) = first_heading(body) else { return answer };
    let mut rest = String::with_capacity(body.len());
    rest.push_str(&body[..at.start]);
    rest.push_str(&body[at.end..]);
    let Some((actions, left)) = lift_actions(&rest, at.start) else { return answer };
    let banded = format!(
        "{left}\n\
         <h1 class=\"doc-page-header__title\" id=\"doc-page-title\" hx-swap-oob=\"true\">{}</h1>\n\
         <div class=\"doc-page-header__slot\" id=\"doc-page-actions\" hx-swap-oob=\"true\">{}</div>",
        escaped(&heading),
        match actions.trim().is_empty() {
            true => String::new(),
            false => format!("<div class=\"doc-page-header__actions\">{actions}</div>"),
        }
    );
    answer.body = banded.into_bytes().into();
    answer
}

/// Text as it goes back into markup.
fn escaped(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

/// A fragment with the faux data banner before it.
fn prefixed(mut answer: Forwarded, banner: &str) -> Forwarded {
    let mut body = Vec::with_capacity(banner.len() + answer.body.len());
    body.extend_from_slice(banner.as_bytes());
    body.extend_from_slice(&answer.body);
    answer.body = Bytes::from(body);
    answer
}

fn passed_on(answer: Forwarded) -> Response {
    let mut response = Response::new(axum::body::Body::from(answer.body));
    *response.status_mut() = StatusCode::from_u16(answer.status).unwrap_or(StatusCode::OK);
    for (name, value) in answer.headers.iter().filter(|(name, _)| returnable(name)) {
        if let (Ok(name), Ok(value)) =
            (HeaderName::try_from(name.as_str()), HeaderValue::from_str(value))
        {
            response.headers_mut().append(name, value);
        }
    }
    response
}

/// The backend's refusal in words, with a stopped plugin's state when that is the reason.
fn explain(plugin: &str, status: u16, body: &Bytes) -> String {
    let problem: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let detail = problem["detail"].as_str().map(str::to_string);
    match status {
        403 => match detail {
            Some(detail) => format!("This {detail}."),
            None => format!("You cannot use this in {plugin}."),
        },
        404 => format!("{plugin} has no such page."),
        503 => {
            let error =
                problem["error"].as_str().map(|error| format!(" ({error})")).unwrap_or_default();
            match problem["state"].as_str() {
                // Core's account of the plugin's lifecycle, which is what a state is for.
                Some(state) => {
                    format!("{plugin} is {state}{error}, so this page cannot be shown now.")
                }
                // No state means the plugin answered the 503 itself, or the call failed before
                // reaching it. What it said is then the only thing that explains the page, and
                // calling it "not running" sends whoever reads it looking in the wrong place.
                None => detail
                    .unwrap_or_else(|| format!("{plugin} could not show this page ({status}).")),
            }
        }
        504 => format!("{plugin} did not answer in time."),
        // Without a problem to read, the status is all there is to go on, and saying it is the
        // difference between "it broke" and knowing where to look.
        _ => detail.unwrap_or_else(|| format!("{plugin} could not show this page ({status}).")),
    }
}

/// What a failed `show` needs to answer with one: the request context once, reused across every
/// place a plugin's answer might be refused.
struct Failing<'a> {
    state: &'a AppState,
    signed: &'a Signed,
    csrf: &'a Csrf,
    uri: &'a http::Uri,
    htmx: bool,
    quiet: bool,
}

impl Failing<'_> {
    async fn respond(&self, status: u16, message: &str) -> Response {
        if self.quiet {
            return StatusCode::OK.into_response();
        }
        let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
        if self.htmx {
            let escaped = message.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
            let html = format!("<p class=\"doc-error-message\">{escaped}</p>");
            return (status, [(CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response();
        }
        let heading = status.canonical_reason().unwrap_or("Error");
        let chrome = Chrome::new(heading, self.uri.path())
            .signed(self.signed, self.csrf)
            .with_plugins(self.state, self.signed)
            .await;
        match Page::within(chrome, heading, message).render_html() {
            Ok(page) => (status, page).into_response(),
            Err(err) => err.into_response(),
        }
    }
}

fn unescaped(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&")
}

/// Text with its tags taken out and its spaces closed up.
fn text_of(html: &str) -> String {
    let mut text = String::with_capacity(html.len());
    let mut in_tag = false;
    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => text.push(c),
            _ => {}
        }
    }
    unescaped(&text.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!("{name}=\""))? + name.len() + 2;
    let end = tag[start..].find('"')?;
    Some(unescaped(&tag[start..start + end]))
}

/// A plugin page's first `<h1>` or `<h2>`, which names it: where it begins, where it ends, and
/// what it says — the text before any badge inside it, or all of it.
fn first_heading(fragment: &str) -> Option<(std::ops::Range<usize>, String)> {
    let (open, level) = ["<h1", "<h2"]
        .iter()
        .filter_map(|tag| Some((fragment.find(tag)?, &tag[1..])))
        .min_by_key(|(at, _)| *at)?;
    let opened = open + fragment[open..].find('>')? + 1;
    let closing = format!("</{level}>");
    let closed = opened + fragment[opened..].find(&closing)?;
    let inner = &fragment[opened..closed];
    let own = text_of(&inner[..inner.find('<').unwrap_or(inner.len())]);
    let text = if own.is_empty() { text_of(inner) } else { own };
    Some((open..closed + closing.len(), text)).filter(|(_, text)| !text.is_empty())
}

/// A plugin page's first `<h1>` or `<h2>`, which names it: a document can open with its own
/// `<h1>`.
pub fn heading(fragment: &str) -> Option<String> {
    first_heading(fragment).map(|(_, text)| text)
}

/// The same heading, taken out of the page: the layout draws it in the header band above the
/// content, as it does every other page's, so a plugin that writes its page the way it always
/// has is headed like the rest of the platform rather than headed twice.
/// When the heading stood in a `doc-heading-row`, what that row offers beside it (its
/// `doc-heading-row__actions`, such as an "Add" button) goes up with it, to the right of the
/// header band where every other page's actions are.
pub fn lift_heading(fragment: &str) -> (Option<String>, Option<String>, String) {
    match first_heading(fragment) {
        Some((at, text)) => {
            let mut rest = String::with_capacity(fragment.len());
            rest.push_str(&fragment[..at.start]);
            rest.push_str(&fragment[at.end..]);
            match lift_actions(&rest, at.start) {
                Some((actions, left)) => {
                    (Some(text), Some(actions).filter(|a| !a.is_empty()), left)
                }
                None => (Some(text), None, rest),
            }
        }
        None => (None, None, fragment.to_string()),
    }
}

/// The heading row a heading was just taken from at `at`, taken out too when nothing is left in it
/// but its actions: those actions, at the header band's size, and the page without the row.
fn lift_actions(rest: &str, at: usize) -> Option<(String, String)> {
    const ROW: &str = "<div class=\"doc-heading-row\">";
    const ACTIONS: &str = "<div class=\"doc-heading-row__actions\">";
    let row_start = rest[..at].trim_end().strip_suffix(ROW)?.len();
    let inner_start = row_start + ROW.len();
    let row_end = inner_start + closing_div(&rest[inner_start..])?;
    let inner = rest[inner_start..row_end].trim();
    let actions = match inner.strip_prefix(ACTIONS) {
        Some(actions) => {
            let end = closing_div(actions)?;
            if !actions[end + "</div>".len()..].trim().is_empty() {
                return None;
            }
            actions[..end].trim().replace(" doc-button--small", "")
        }
        None if inner.is_empty() => String::new(),
        None => return None,
    };
    let mut left = String::with_capacity(rest.len());
    left.push_str(&rest[..row_start]);
    left.push_str(&rest[row_end + "</div>".len()..]);
    Some((actions, left))
}

/// Where the `</div>` that closes a `<div>` opened just before `html` begins.
fn closing_div(html: &str) -> Option<usize> {
    let (mut depth, mut at) = (0usize, 0usize);
    loop {
        let close = at + html[at..].find("</div>")?;
        match html[at..].find("<div").map(|open| at + open) {
            Some(open) if open < close => {
                depth += 1;
                at = open + "<div".len();
            }
            _ if depth == 0 => return Some(close),
            _ => {
                depth -= 1;
                at = close + "</div>".len();
            }
        }
    }
}

/// The first `doc-tabs__tab` a plugin page marks `aria-current="page"`: its label and link.
pub fn current_tab(fragment: &str) -> Option<(String, String)> {
    let mut rest = fragment;
    while let Some(at) = rest.find("doc-tabs__tab\"") {
        let tag_start = rest[..at].rfind('<')?;
        let tag_end = at + rest[at..].find('>')?;
        let tag = &rest[tag_start..tag_end];
        let after = &rest[tag_end + 1..];
        if tag.contains("aria-current=\"page\"") {
            let label = text_of(&after[..after.find("</a>")?]);
            let href = attribute(tag, "href")?;
            return Some((label, href))
                .filter(|(label, href)| !label.is_empty() && href.starts_with('/'));
        }
        rest = after;
    }
    None
}

/// The steps a plugin page puts between its tab and itself, in order: each `<a class="doc-trail"
/// href="…" hidden>`, such as the kind a Catalogue resource is, so a page deeper than its tabs is
/// placed as deep as it is. Only links within the platform count.
pub fn trail(fragment: &str) -> Vec<(String, String)> {
    let mut steps = Vec::new();
    let mut rest = fragment;
    while let Some(at) = rest.find("class=\"doc-trail\"") {
        let Some(tag_start) = rest[..at].rfind('<') else { break };
        let Some(tag_end) = rest[at..].find('>').map(|end| at + end) else { break };
        let tag = &rest[tag_start..tag_end];
        let after = &rest[tag_end + 1..];
        let label = after.find("</a>").map(|end| text_of(&after[..end])).unwrap_or_default();
        if let Some(href) = attribute(tag, "href").filter(|href| href.starts_with('/'))
            && tag.starts_with("<a ")
            && !label.is_empty()
        {
            steps.push((label, href));
        }
        rest = after;
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;

    const PRINCIPAL: &str = r#"<div id="rbac">
  <nav class="doc-tabs" aria-label="Access control">
    <ul class="doc-tabs__list">
      <li class="doc-tabs__item"><a class="doc-tabs__tab" href="/p/rbac/groups">Groups</a></li>
      <li class="doc-tabs__item"><a class="doc-tabs__tab" href="/p/rbac/people" aria-current="page">People &amp; accounts</a></li>
    </ul>
  </nav>
<h2>t20admin <strong class="doc-badge">Off</strong></h2>
<nav class="doc-tabs"><ul><li><a class="doc-tabs__tab" href="?tab=x" aria-current="page">Details</a></li></ul></nav>"#;

    #[test]
    fn a_plugin_page_is_named_by_its_heading_and_placed_by_its_current_tab() {
        assert_eq!(heading(PRINCIPAL).as_deref(), Some("t20admin"), "badges are not its name");
        assert_eq!(
            current_tab(PRINCIPAL),
            Some(("People & accounts".to_string(), "/p/rbac/people".to_string())),
            "the plugin's own tabs come first, before any on the page"
        );
        assert_eq!(heading("<p>No heading</p>"), None);
        assert_eq!(
            heading(
                r#"<p class="doc-contents__heading">Space</p><h1 id="x">Guide</h1><h2>Part</h2>"#
            )
            .as_deref(),
            Some("Guide"),
            "a document's own h1 names it"
        );
        assert_eq!(
            heading(r#"<h2><a href="/x">Linked &lt;name&gt;</a></h2>"#).as_deref(),
            Some("Linked <name>")
        );
        assert_eq!(current_tab(r#"<a class="doc-tabs__tab" href="/a">A</a>"#), None);
    }
}
