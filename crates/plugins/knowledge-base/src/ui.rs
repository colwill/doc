//! The Knowledge Base's pages at `/p/kb/...`: the Software Catalogue of services and their docs,
//! search as you type, spaces and their trees, documents, a docs panel for resource pages, and,
//! for those who can write, the sources spaces are filled from.

use std::collections::BTreeMap;

use askama::Template;
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::{Value, json};
use url::form_urlencoded::{Serializer, byte_serialize};
use uuid::Uuid;

use crate::Refusal;
use crate::api::{self, decoded, escape, query};
use crate::imports::{checked_resource, front_page, page_url};
use crate::removal;
use crate::sources;
use crate::store::{Space, Store};

mod wizard;

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    fn refused(refusal: &Refusal) -> Self {
        Self { notice: None, error: Some(refusal.detail.clone()) }
    }
}

type Form = Vec<(String, String)>;

fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(form: &Form, name: &str) -> Option<String> {
    form.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

/// How many of a service's repositories a panel reads the documentation of.
const CONNECTED_REPOSITORIES: usize = 20;
/// How much of a page its summary shows.
const SUMMARY_WORDS: usize = 70;

fn source_label(kind: &str) -> &'static str {
    match kind {
        "github" => "GitHub",
        "confluence" => "Confluence",
        "drive" => "Google Drive",
        "git" => "Git repository",
        "plugin" => "A plugin",
        _ => "Upload",
    }
}

fn pages(count: i64) -> String {
    match count {
        1 => "1 page".into(),
        count => format!("{count} pages"),
    }
}

fn when(text: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(text)
        .map(|at| at.format("%-d %B %Y %H:%M").to_string())
        .unwrap_or_else(|_| text.to_string())
}

fn text(value: &Value) -> String {
    value.as_str().unwrap_or_default().to_string()
}

fn text_of(value: &Value, key: &str) -> String {
    text(&value[key])
}

fn encoded(text: &str) -> String {
    byte_serialize(text.as_bytes()).collect::<String>().replace('+', "%20")
}

/// A resource's page in Resource Definitions, from `kind:name`.
fn resource_href(resource: &str) -> String {
    let (kind, name) = resource.split_once(':').unwrap_or(("service", resource));
    let name: Vec<String> = name.split('/').map(encoded).collect();
    format!("/p/resources/r/{kind}/{}", name.join("/"))
}

#[derive(Clone)]
pub struct Link {
    pub label: String,
    pub href: String,
    pub note: String,
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
    writes: bool,
}

struct Service {
    title: String,
    href: String,
    owner: String,
    docs: String,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    flash: Flash,
    writes: bool,
    services: Vec<Service>,
    hidden: Option<String>,
    spaces: usize,
}

struct Hit {
    title: String,
    href: String,
    space: String,
    snippet: String,
}

#[derive(Template)]
#[template(path = "results.html")]
struct Results {
    text: String,
    hits: Vec<Hit>,
}

struct Group {
    space: Link,
    documents: Vec<Link>,
    /// Whether the first of them is the space's front page.
    front: bool,
    /// Where its pages come from: the resource the space is bound to, the space, the plugin.
    tags: Vec<KindTag>,
    /// Pages left out of `documents` and counted instead, which the space's own page lists.
    more: usize,
}

/// The one page a service's documentation opens with: the README of the single repository it is
/// built from, with the opening of what it says.
struct Featured {
    page: Link,
    summary: String,
    /// Where it came from, in tags, as the service's own page shows it.
    tags: Vec<KindTag>,
}

#[derive(Template)]
#[template(path = "catalogue.html")]
struct CataloguePage {
    flash: Flash,
    /// What the page is: its sections `docs` and `links`, or the form `link`.
    section: &'static str,
    writes: bool,
    name: String,
    title: String,
    description: String,
    owner: Option<(String, String)>,
    resource_page: String,
    /// The service these docs are about, as a tag, so the page says what it is documentation of
    /// and links to it in the Catalogue — the same decoration a document carries.
    resource_tags: Vec<KindTag>,
    hidden: Option<String>,
    /// The README of the one repository this service is built from, shown here as the service's
    /// own documentation. None where several repositories document it, which is a contents list
    /// of them instead.
    front: Option<FrontPage>,
    groups: Vec<Group>,
    /// Whether the viewer administers DOC, and so may link spaces to the service.
    admin: bool,
    /// The spaces linked to the service, whose pages are its docs too.
    linked: Vec<Link>,
    /// Spaces an administrator could link, by key and name.
    linkable: Vec<(String, String)>,
}

/// A README read in place: where it came from, what it says, and where to read it on its own.
struct FrontPage {
    repository: Link,
    page: Link,
    html: String,
    /// The same tags the group would have carried, since the group is read in place here.
    tags: Vec<KindTag>,
}

struct SpaceRow {
    key: String,
    link: Link,
    resource: Option<(String, String)>,
    owners: Vec<KindTag>,
    sources: String,
    /// Who archived it and when, for a row in the archived section; empty for a space in use.
    archived: String,
}

#[derive(Template)]
#[template(path = "spaces.html")]
struct SpacesPage {
    flash: Flash,
    writes: bool,
    /// Whether the viewer administers DOC, and so may archive, restore and delete.
    admin: bool,
    spaces: Vec<SpaceRow>,
    /// The spaces put out of sight, shown to an administrator only: where they are restored, or
    /// deleted for good.
    archived: Vec<SpaceRow>,
}

#[derive(Template)]
#[template(path = "space.html")]
struct SpacePage {
    flash: Flash,
    /// What the page is: its sections `pages` and `owners`, or the forms `change` and `rename`.
    section: &'static str,
    writes: bool,
    /// Whether the viewer administers DOC, and so may delete the space.
    admin: bool,
    /// The repository the space is kept in step with, which deleting it stops following.
    follows: Option<String>,
    key: String,
    name: String,
    resource: Option<(String, String)>,
    owners: Vec<KindTag>,
    /// Offered when the space says nobody, or when somebody is changing who it says.
    owner_choices: Vec<crate::owners::Choice>,
    owners_problem: Option<String>,
    count: String,
    tree: String,
}

impl SpacePage {
    fn owners_problem(&self) -> Option<&str> {
        self.owners_problem.as_deref()
    }

    fn url(&self) -> String {
        format!("/p/kb/spaces/{}", self.key)
    }
}

impl CataloguePage {
    fn url(&self) -> String {
        format!("/p/kb/catalogue/{}", self.name)
    }

    /// The sections: the service's documentation, and the spaces linked to it where there are
    /// any or the viewer may link one.
    fn sections(&self) -> Vec<(&'static str, &'static str, String)> {
        let url = self.url();
        let mut sections = vec![("docs", "Documentation", url.clone())];
        if self.admin || !self.linked.is_empty() {
            sections.push(("links", "Linked spaces", format!("{url}/links")));
        }
        sections
    }
}

/// What the page says of an edit made in DOC.
struct Edited {
    by: String,
    when: String,
    /// Where the page comes from, in words, and the same starting a sentence.
    origin: String,
    origin_starting: String,
    /// Whether the source has changed the page since it was edited.
    changed: bool,
    /// Whether the source no longer has the page at all.
    gone: bool,
    /// Where a pull request would go, for a page from a GitHub repository.
    repository: bool,
    pull_request: Option<String>,
    /// Whether it was edited again after it was proposed.
    since: bool,
}

#[derive(Template)]
#[template(path = "document.html")]
struct DocumentPage {
    flash: Flash,
    writes: bool,
    /// `<space>/<path>` as the page's address writes it, which its edit routes follow.
    address: String,
    space: Link,
    /// The page's own first heading, as HTML, or its title.
    heading: String,
    edit_href: Option<String>,
    /// Where Agent Smith can be asked to run this page, when it is a runbook and the viewer may.
    runbook: Option<RunbookForm>,
    edited: Option<Edited>,
    html: String,
    source: &'static str,
    original: Option<String>,
    updated: String,
    resources: Vec<(String, String)>,
    tags: Vec<String>,
    contents: String,
    previous: Option<Link>,
    next: Option<Link>,
}

#[derive(Template)]
#[template(path = "edit.html")]
struct EditPage {
    flash: Flash,
    writes: bool,
    /// Whether the viewer administers DOC, and so may delete the page.
    admin: bool,
    space_key: String,
    /// The page's path as it is kept, which deleting it names.
    path: String,
    /// Whether a source brought it in, and would bring it back, and where from.
    sourced: bool,
    origin_starting: String,
    space: Link,
    address: String,
    page_href: String,
    title: String,
    explained: String,
    markdown: String,
    html: Option<String>,
    /// Where each image is kept, by the address the page writes, as JSON for the editor.
    images: String,
}

#[derive(Template)]
#[template(path = "panel.html")]
struct PanelFragment {
    /// The README that is this service's documentation, when one repository holds it all.
    featured: Option<Featured>,
    groups: Vec<Group>,
    /// Whether to head each group with its repository: only worth it when there are several.
    headings: bool,
    catalogue: Option<String>,
    /// The repositories this service is built from, named when there is nothing to show: a panel
    /// that is empty because nothing is written in them says so, rather than looking broken.
    repositories: Vec<String>,
}

/// A page as a dashboard lists it.
struct Changed {
    href: String,
    title: String,
    space: String,
    when: String,
}

#[derive(Template)]
#[template(path = "dashboard.html")]
struct RecentFragment {
    pages: Vec<Changed>,
    keeps: usize,
}

/// What somebody keeps, as spaces name it: `team:<name>` for each team they are in, and
/// `organisation:<name>` for each organisation those teams are in.
async fn kept_by(backend: &Backend) -> Result<Vec<String>, Refusal> {
    let caller = backend.caller().ok_or_else(|| Refusal::forbidden("nobody is asking"))?;
    let me = caller.id.clone().filter(|_| caller.kind == "user");
    let me = me.ok_or_else(|| Refusal::forbidden("a dashboard is a person's"))?;
    let held: Vec<Value> = backend
        .query_all(
            doc_plugin_sdk::Query::new("core.team-members")
                .filter(json!({ "user_id": me }))
                .fields(&["team_id"]),
        )
        .await?;
    let ids: Vec<Value> = held.iter().map(|row| row["team_id"].clone()).collect();
    if ids.is_empty() {
        return Ok(Vec::new());
    }
    let teams: Vec<Value> = backend
        .query_all(
            doc_plugin_sdk::Query::new("core.teams")
                .filter(json!({ "id": { "in": ids } }))
                .fields(&["name", "organisation_id"]),
        )
        .await?;
    let organisations: Vec<Value> =
        teams.iter().map(|team| team["organisation_id"].clone()).collect();
    let named: Vec<Value> = backend
        .query_all(
            doc_plugin_sdk::Query::new("core.organisations")
                .filter(json!({ "id": { "in": organisations } }))
                .fields(&["name"]),
        )
        .await?;
    let mut kept: Vec<String> =
        teams.iter().filter_map(|team| Some(format!("team:{}", team["name"].as_str()?))).collect();
    kept.extend(
        named.iter().filter_map(|org| Some(format!("organisation:{}", org["name"].as_str()?))),
    );
    Ok(kept)
}

/// What changed lately in the spaces somebody's teams keep, on their dashboard, newest first.
async fn recent(backend: &Backend, store: &Store<'_>) -> Result<String, Refusal> {
    let kept = kept_by(backend).await?;
    let spaces: BTreeMap<String, String> = store
        .spaces()
        .await?
        .into_iter()
        .filter(|space| space.owners.iter().any(|owner| kept.contains(owner)))
        .map(|space| (space.key, space.name))
        .collect();
    let keys: Vec<String> = spaces.keys().cloned().collect();
    let pages = store
        .recent_in(&keys, 6)
        .await?
        .iter()
        .map(|page| {
            let space = text(&page["space"]);
            Changed {
                href: page_url(&space, &text(&page["path"])),
                title: text(&page["title"]),
                when: when(&text(&page["_updated_at"])),
                space: spaces.get(&space).cloned().unwrap_or(space),
            }
        })
        .collect();
    render(&RecentFragment { pages, keeps: keys.len() })
}

#[derive(Template)]
#[template(path = "summary.html")]
struct SummaryFragment {
    /// None when the catalogue holds a page the Knowledge Base no longer has.
    page: Option<Link>,
    summary: String,
}

struct SourceRow {
    id: String,
    kind: &'static str,
    space: String,
    what: String,
    /// Its own page in Resource Definitions: a source is a resource there, of the kind
    /// `DocumentationSource`, so what it reads and what it fills are connected like anything else.
    resource: String,
    /// Kept by the repository sync rather than added by hand, so it has no schedule of its own.
    managed: bool,
    /// The repository it reads, for a GitHub source.
    repository: String,
    schedule: String,
    syncs: bool,
    last: String,
    badge: &'static str,
    state: &'static str,
    error: String,
    /// Who archived it and when, for a row in the archived section; empty for one in use.
    archived: String,
}

#[derive(Template)]
#[template(path = "sources.html")]
struct SourcesPage {
    flash: Flash,
    writes: bool,
    admin: bool,
    sources: Vec<SourceRow>,
    /// The sources put out of sight: where they are restored, or deleted for good.
    archived: Vec<SourceRow>,
    /// Repositories an administrator stopped following, which can be followed again.
    forgotten: Vec<String>,
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let store = Store(backend);
    let fragment = matches!(path, ["search" | "panel" | "summary" | "dashboard"]);
    // Where the address bar goes when a form lands somewhere other than where it was.
    let mut pushed: Option<String> = None;
    let page = match (request.method.as_str(), path) {
        ("GET", [] | [""]) => home(backend, &store).await,
        ("GET", ["search"]) => search(&store, request).await,
        ("GET", ["catalogue", name]) => {
            catalogue(backend, &store, &decoded(name), "docs", Flash::default()).await
        }
        ("GET", ["catalogue", name, "links"]) => {
            catalogue(backend, &store, &decoded(name), "links", Flash::default()).await
        }
        ("GET", ["catalogue", name, "links", "new"]) => {
            catalogue(backend, &store, &decoded(name), "link", Flash::default()).await
        }
        ("POST", ["catalogue", name, "links"]) => {
            let space = field(&form(request), "space").unwrap_or_default();
            pushed = Some(format!("/p/kb/catalogue/{name}/links"));
            linked(backend, &store, &decoded(name), &space, true).await
        }
        ("POST", ["catalogue", name, "links", "remove"]) => {
            let space = field(&form(request), "space").unwrap_or_default();
            linked(backend, &store, &decoded(name), &space, false).await
        }
        ("GET", ["spaces"]) => spaces(backend, &store, Flash::default()).await,
        ("POST", ["spaces", key, "archive"]) => {
            space_archived(backend, &store, &decoded(key)).await
        }
        ("POST", ["spaces", key, "restore"]) => {
            space_restored(backend, &store, &decoded(key)).await
        }
        ("POST", ["spaces", key, "delete"]) => space_deleted(backend, &store, &decoded(key)).await,
        ("POST", ["spaces", key, "pages", "delete"]) => {
            let path = field(&form(request), "path").unwrap_or_default();
            pushed = Some(format!("/p/kb/spaces/{key}"));
            Box::pin(page_deleted(backend, &store, &decoded(key), &path)).await
        }
        ("GET", ["spaces", key]) => {
            space(backend, &store, &decoded(key), "pages", None, Flash::default()).await
        }
        ("GET", ["spaces", key, "owners"]) => {
            space(backend, &store, &decoded(key), "owners", None, Flash::default()).await
        }
        ("GET", ["spaces", key, "owners", "change"]) => {
            space(backend, &store, &decoded(key), "change", None, Flash::default()).await
        }
        ("GET", ["spaces", key, "name"]) => {
            space(backend, &store, &decoded(key), "rename", None, Flash::default()).await
        }
        ("POST", ["spaces", key, "name"]) => {
            let (page, url) =
                Box::pin(renamed(backend, &store, &decoded(key), &form(request))).await;
            pushed = url;
            page
        }
        ("POST", ["spaces", key, "owners"]) => {
            let (page, url) = set_owners(backend, &store, &decoded(key), &form(request)).await;
            pushed = url;
            page
        }
        ("GET", ["docs", space, rest @ ..]) if !rest.is_empty() => {
            let path = decoded(&rest.join("/"));
            document(backend, &store, &decoded(space), &path, Flash::default()).await
        }
        ("GET", ["edit", space, rest @ ..]) if !rest.is_empty() => {
            Box::pin(edit_page(backend, &store, &decoded(space), &decoded(&rest.join("/")), None))
                .await
        }
        ("POST", [change @ ("edit" | "discard" | "propose"), space, rest @ ..])
            if !rest.is_empty() =>
        {
            let (space, path) = (decoded(space), decoded(&rest.join("/")));
            let markdown = form(request)
                .into_iter()
                .find(|(key, _)| key == "markdown")
                .map(|(_, value)| value)
                .unwrap_or_default();
            // Boxed, as is all it awaits: kept inline, a save's state is deep enough to overflow
            // the stack of a debug build.
            let (page, url) =
                Box::pin(edited(backend, &store, &space, &path, change, &markdown)).await;
            pushed = url;
            page
        }
        ("GET", ["panel"]) => panel(backend, &store, request).await,
        ("GET", ["dashboard"]) => recent(backend, &store).await,
        ("GET", ["summary"]) => summary(&store, request).await,
        ("GET", ["sources"]) => sources_page(backend, &store, Flash::default()).await,
        ("GET", ["sources", "new"]) => wizard::start(backend, &store).await,
        ("POST", ["sources", "new"]) => wizard::answer(backend, &store, &form(request)).await,
        ("POST", ["sources", "new", "upload"]) => wizard::upload(backend, &store, request).await,
        ("POST", ["sources", "repositories"]) => look_for_repositories(backend, &store).await,
        ("POST", ["sources", "repositories", change @ ("forget" | "follow")]) => {
            let repository = field(&form(request), "repository").unwrap_or_default();
            repository_changed(backend, &store, &repository, change).await
        }
        ("POST", ["sources", id, change]) => {
            source_change(backend, &store, id, change, &form(request)).await
        }
        _ => Err(Refusal::missing("no such page")),
    };
    match page {
        Ok(html) => match pushed {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) if fragment => {
            let text = format!(
                "<p class=\"doc-error-message\" role=\"alert\">{}</p>",
                escape(&refusal.detail)
            );
            Response::new(refusal.status, "text/html; charset=utf-8", text)
        }
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(&refusal), writes: backend.writes() };
            let html = page.render().unwrap_or_else(|_| escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

/// Links a space to a service, or takes the link away; only administrators may.
async fn linked(
    backend: &Backend,
    store: &Store<'_>,
    service: &str,
    space: &str,
    link: bool,
) -> Result<String, Refusal> {
    let said = api::link_space(backend, store, service, space, link).await?;
    catalogue(backend, store, service, "links", Flash::done(said)).await
}

/// Puts a space out of sight, keeping everything in it.
async fn space_archived(
    backend: &Backend,
    store: &Store<'_>,
    key: &str,
) -> Result<String, Refusal> {
    let archived = removal::archive_space(backend, store, key).await?;
    let said = format!(
        "Archived {}, with {}. Nothing is lost by it: restore it, or delete it for good, from \
         the archived spaces below.",
        archived.name,
        pages(archived.pages as i64)
    );
    spaces(backend, store, Flash::done(said)).await
}

/// Brings an archived space back, with its pages and its sources.
async fn space_restored(
    backend: &Backend,
    store: &Store<'_>,
    key: &str,
) -> Result<String, Refusal> {
    let restored = removal::restore_space(backend, store, key).await?;
    let said = format!("Restored {}, with {}.", restored.name, pages(restored.pages as i64));
    spaces(backend, store, Flash::done(said)).await
}

async fn space_deleted(backend: &Backend, store: &Store<'_>, key: &str) -> Result<String, Refusal> {
    let deleted = removal::delete_space(backend, store, key).await?;
    let mut said =
        format!("Deleted the space {}, with {}.", deleted.name, pages(deleted.pages as i64));
    if !deleted.repositories.is_empty() {
        said.push_str(&format!(
            " {} is no longer followed; follow it again on the Sources page.",
            deleted.repositories.join(", ")
        ));
    }
    spaces(backend, store, Flash::done(said)).await
}

async fn page_deleted(
    backend: &Backend,
    store: &Store<'_>,
    key: &str,
    path: &str,
) -> Result<String, Refusal> {
    removal::delete_page(backend, store, key, path).await?;
    let notice = Flash::done(format!("Deleted the page {path}."));
    space(backend, store, key, "pages", None, notice).await
}

/// Stops following a repository, or follows it again.
async fn repository_changed(
    backend: &Backend,
    store: &Store<'_>,
    repository: &str,
    change: &str,
) -> Result<String, Refusal> {
    let said = match change {
        "forget" => {
            let deleted = removal::forget_repository(backend, store, repository).await?;
            format!(
                "{repository} is no longer followed, and its space went with {}. Follow it again \
                 below.",
                pages(deleted.pages as i64)
            )
        }
        _ => match removal::follow_again(backend, repository).await? {
            true => format!("{repository} is followed again from the repository sync's next run."),
            false => format!("{repository} was being followed already."),
        },
    };
    sources_page(backend, store, Flash::done(said)).await
}

/// Services as the viewer may see them in Resource Definitions, or why they may not.
async fn services(backend: &Backend, text: Option<&str>) -> Result<Vec<Value>, String> {
    // Faux data's services are the ones its docs are for, whatever the Catalogue has.
    if let Some(faux) = crate::faux::corpus(backend).await.map_err(|refusal| refusal.detail)? {
        return Ok(match text {
            Some(text) => faux.service(text).into_iter().collect(),
            None => faux.listed(),
        });
    }
    let asked = {
        let mut asked = Serializer::new(String::new());
        asked.append_pair("kind", "Service").append_pair("limit", "500");
        if let Some(text) = text {
            asked.append_pair("q", text);
        }
        asked.finish()
    };
    match backend.ask("resources", "GET", "resources", Some(&asked), None).await {
        Ok((200, Value::Array(found))) => Ok(found),
        Ok((_, body)) => {
            Err(body["detail"].as_str().unwrap_or("Resource Definitions refused").to_string())
        }
        Err(err) => Err(format!("Resource Definitions could not be asked: {err}")),
    }
}

/// How many pages document a resource, and from which kinds of source.
fn counted(counts: Option<&BTreeMap<String, i64>>) -> String {
    match counts.filter(|counts| !counts.is_empty()) {
        None => "None yet".into(),
        Some(counts) => {
            let each: Vec<String> = counts
                .iter()
                .map(|(kind, count)| format!("{} {count}", source_label(kind)))
                .collect();
            format!("{}: {}", pages(counts.values().sum()), each.join(", "))
        }
    }
}

async fn home(backend: &Backend, store: &Store<'_>) -> Result<String, Refusal> {
    let documented = store.documented(&store.links().await?).await?;
    let (services, hidden) = match services(backend, None).await {
        Ok(found) => {
            let shown = found
                .iter()
                .map(|service| {
                    let name = text_of(service, "name");
                    let title = Some(text_of(service, "title")).filter(|title| !title.is_empty());
                    Service {
                        docs: counted(documented.get(&format!("service:{name}"))),
                        href: format!("/p/kb/catalogue/{}", encoded(&name)),
                        owner: text_of(service, "owner"),
                        title: title.unwrap_or(name),
                    }
                })
                .collect();
            (shown, None)
        }
        Err(reason) => (Vec::new(), Some(reason)),
    };
    let spaces = store.spaces().await?.len();
    render(&HomePage {
        flash: Flash::default(),
        writes: backend.writes(),
        services,
        hidden,
        spaces,
    })
}

async fn search(store: &Store<'_>, request: &Request) -> Result<String, Refusal> {
    let text = query(request, "q").unwrap_or_default();
    let mut hits = Vec::new();
    if !text.is_empty() {
        let spaces = spaces_by_key(store).await?;
        for hit in store.search(&text, query(request, "space").as_deref(), 20).await? {
            let space = text_of(&hit, "space");
            hits.push(Hit {
                title: text_of(&hit, "title"),
                href: page_url(&space, hit["path"].as_str().unwrap_or_default()),
                snippet: api::snippet(hit["snippet"].as_str().unwrap_or_default()),
                space: space_name(&spaces, &space),
            });
        }
    }
    render(&Results { text, hits })
}

/// Space names by key, for pages that show documents from more than one.
async fn spaces_by_key(store: &Store<'_>) -> Result<BTreeMap<String, Space>, Refusal> {
    Ok(store.spaces().await?.into_iter().map(|space| (space.key.clone(), space)).collect())
}

/// What a space is called, for a page that only needs the name.
fn space_name(spaces: &BTreeMap<String, Space>, key: &str) -> String {
    spaces.get(key).map(|space| space.name.clone()).unwrap_or_else(|| key.to_string())
}

/// A tag saying what something is and what it is called, coloured by its kind: `Repository`,
/// `Team`, `docs`. Shown wherever a page says where what it holds came from.
#[derive(Clone)]
struct KindTag {
    /// The `doc-kind--*` modifier, which is the kind as a URL names it.
    kind: String,
    label: String,
    name: String,
    href: String,
}

impl KindTag {
    fn new(kind: &str, label: &str, name: &str, href: &str) -> Self {
        Self {
            kind: kind.to_string(),
            label: label.to_string(),
            name: name.to_string(),
            href: href.to_string(),
        }
    }
}

/// A `kind:name` as a tag, coloured by its kind.
fn resource_tag(resource: &str) -> Option<KindTag> {
    let (kind, name) = resource.split_once(':')?;
    let label = kind.split(['-', '_']).map(capitalised).collect::<Vec<_>>().join(" ");
    Some(KindTag::new(kind, &label, name, &resource_href(&format!("{kind}:{name}"))))
}

/// Who looks after a space, as tags.
fn owner_tags(space: &Space) -> Vec<KindTag> {
    space.owners.iter().filter_map(|owner| resource_tag(owner)).collect()
}

/// The label naming a space's docs, which `grouped` points at the space's README once it knows
/// there is one.
const DOCS: &str = "docs";

/// Where a space's pages come from, in tags: the catalogue resource the space is bound to, whoever
/// looks after it, and the docs themselves: `docs` and what they are the docs of, leading to the
/// space. A repository's docs are called by the repository, `colwill/ccc`, whatever its space is.
fn origin(space: &Space) -> Vec<KindTag> {
    let mut tags: Vec<KindTag> =
        space.resource.as_deref().and_then(resource_tag).into_iter().collect();
    tags.extend(owner_tags(space));
    let named = space
        .resource
        .as_deref()
        .and_then(|resource| resource.strip_prefix("repository:"))
        .unwrap_or(&space.name);
    tags.push(KindTag::new("space", DOCS, named, &format!("/p/kb/spaces/{}", space.key)));
    tags
}

fn capitalised(word: &str) -> String {
    let mut letters = word.chars();
    letters.next().map(|first| first.to_uppercase().chain(letters).collect()).unwrap_or_default()
}

fn document_link(document: &Value, spaces: &BTreeMap<String, Space>) -> Link {
    let space = text_of(document, "space");
    Link {
        label: text_of(document, "title"),
        href: page_url(&space, document["path"].as_str().unwrap_or_default()),
        note: format!(
            "{}, {}",
            space_name(spaces, &space),
            source_label(document["source"].as_str().unwrap_or_default())
        ),
    }
}

async fn catalogue(
    backend: &Backend,
    store: &Store<'_>,
    name: &str,
    section: &'static str,
    flash: Flash,
) -> Result<String, Refusal> {
    let resource = format!("service:{name}");
    // What names the service, and what is written in the repositories it is built from: the same
    // rule as its Docs panel, so the two pages say the same thing about the same service.
    let mut wanted = vec![resource.clone()];
    wanted.extend(built_from(backend, &resource).await);
    let linked = store.linked(name).await?;
    let documents = store.documenting_any(&wanted, &linked).await?;
    let (service, hidden) = match services(backend, Some(name)).await {
        Ok(found) => match found.into_iter().find(|service| service["name"] == json!(name)) {
            Some(service) => (service, None),
            None if documents.is_empty() => {
                return Err(Refusal::missing(format!("there is no service {name}")));
            }
            None => (Value::Null, Some(format!("Resource Definitions has no service {name}."))),
        },
        Err(reason) => (Value::Null, Some(reason)),
    };
    let spaces = spaces_by_key(store).await?;
    // One group per space — for a service, one per repository it is built from — each opening
    // with that repository's README.
    let mut groups = grouped(&documents, &spaces);
    // Built from one repository: its README is this service's documentation, so it is read here
    // rather than linked to. Built from several, the page is a contents list of all of them.
    let front = match groups.as_mut_slice() {
        [only] if only.front => {
            let page = only.documents.remove(0);
            let path = documents
                .iter()
                .filter(|document| text_of(document, "space") == only.space.note)
                .map(|document| text_of(document, "path"))
                .find(|path| front_page(path))
                .unwrap_or_default();
            let tags = only.tags.clone();
            store.document(&only.space.note, &path).await?.map(|document| FrontPage {
                repository: only.space.clone(),
                page,
                html: text_of(&document, "html"),
                tags,
            })
        }
        _ => None,
    };
    // A README put back where it was taken from, so a repository that holds nothing else is not
    // left with an empty contents list under it.
    if front.is_none() {
        groups = grouped(&documents, &spaces);
    }
    groups.retain(|group| !group.documents.is_empty());
    let title = Some(text_of(&service, "title")).filter(|title| !title.is_empty());
    let admin = removal::is_admin(backend);
    let linkable = match admin {
        true => spaces
            .values()
            .filter(|space| !linked.contains(&space.key))
            .map(|space| (space.key.clone(), space.name.clone()))
            .collect(),
        false => Vec::new(),
    };
    let linked = linked
        .iter()
        .map(|key| Link {
            label: space_name(&spaces, key),
            href: format!("/p/kb/spaces/{key}"),
            note: key.clone(),
        })
        .collect();
    render(&CataloguePage {
        flash,
        section,
        admin,
        linked,
        linkable,
        writes: backend.writes(),
        front,
        name: name.to_string(),
        title: title.unwrap_or_else(|| name.to_string()),
        description: text_of(&service, "description"),
        owner: service["owner"]
            .as_str()
            .map(|team| (resource_href(&format!("team:{team}")), team.to_string())),
        resource_page: resource_href(&resource),
        resource_tags: resource_tag(&resource).into_iter().collect(),
        hidden,
        groups,
    })
}

async fn spaces(backend: &Backend, store: &Store<'_>, flash: Flash) -> Result<String, Refusal> {
    let mut kinds: BTreeMap<String, Vec<&'static str>> = BTreeMap::new();
    for source in sources::list(store).await? {
        let listed = kinds.entry(text_of(&source, "space")).or_default();
        let label = source_label(source["kind"].as_str().unwrap_or_default());
        if !listed.contains(&label) {
            listed.push(label);
        }
    }
    let row = |space: Space| SpaceRow {
        sources: kinds.get(&space.key).map(|kinds| kinds.join(", ")).unwrap_or_default(),
        owners: owner_tags(&space),
        archived: match (space.archived_at, &space.archived_by) {
            (Some(at), Some(by)) => {
                format!("Archived by {by}, {}", when(&at.to_rfc3339()))
            }
            (Some(at), None) => format!("Archived {}", when(&at.to_rfc3339())),
            _ => String::new(),
        },
        key: space.key.clone(),
        link: Link {
            label: space.name.clone(),
            href: format!("/p/kb/spaces/{}", space.key),
            note: pages(space.documents),
        },
        resource: space.resource.clone().map(|resource| (resource_href(&resource), resource)),
    };
    let spaces = store.spaces().await?.into_iter().map(row).collect();
    let admin = removal::is_admin(backend);
    let archived = match admin {
        true => store.archived_spaces().await?.into_iter().map(row).collect(),
        false => Vec::new(),
    };
    render(&SpacesPage { flash, writes: backend.writes(), admin, spaces, archived })
}

/// A folder's name as a heading: `payment-flow` as `Payment flow`.
fn humane(folder: &str) -> String {
    let spaced = folder.replace(['-', '_'], " ");
    let mut letters = spaced.chars();
    match letters.next() {
        Some(first) => first.to_uppercase().chain(letters).collect(),
        None => spaced,
    }
}

/// A space's pages as nested lists, a level for each folder, in the space's own order.
fn tree(space: &str, documents: &[Value]) -> String {
    let mut html = String::from("<ul>");
    let mut open: Vec<&str> = Vec::new();
    for document in documents {
        let path = document["path"].as_str().unwrap_or_default();
        let title = document["title"].as_str().unwrap_or(path);
        let parts: Vec<&str> = path.split('/').collect();
        let folders = &parts[..parts.len().saturating_sub(1)];
        let kept = open.iter().zip(folders).take_while(|(open, folder)| open == folder).count();
        html.push_str(&"</ul></li>".repeat(open.len() - kept));
        open.truncate(kept);
        for folder in &folders[kept..] {
            html.push_str(&format!("<li>{}<ul>", escape(&humane(folder))));
            open.push(folder);
        }
        let href = escape(&page_url(space, path));
        html.push_str(&format!("<li><a href=\"{href}\">{}</a></li>", escape(title)));
    }
    html.push_str(&"</ul></li>".repeat(open.len()));
    html.push_str("</ul>");
    html
}

/// A space's pages as the contents beside one of them: folders nest, and `current` is marked.
fn contents(space: &str, documents: &[Value], current: &str) -> String {
    let mut html = String::from("<ul class=\"doc-contents__list\">");
    let mut open: Vec<&str> = Vec::new();
    for document in documents {
        let path = document["path"].as_str().unwrap_or_default();
        let title = document["title"].as_str().unwrap_or(path);
        let parts: Vec<&str> = path.split('/').collect();
        let folders = &parts[..parts.len().saturating_sub(1)];
        let kept = open.iter().zip(folders).take_while(|(open, folder)| open == folder).count();
        html.push_str(&"</ul></li>".repeat(open.len() - kept));
        open.truncate(kept);
        for folder in &folders[kept..] {
            html.push_str(&format!(
                "<li class=\"doc-contents__item\"><span class=\"doc-contents__folder\">{}</span><ul class=\"doc-contents__list\">",
                escape(&humane(folder))
            ));
            open.push(folder);
        }
        let href = escape(&page_url(space, path));
        let here = if path == current { " aria-current=\"page\"" } else { "" };
        html.push_str(&format!(
            "<li class=\"doc-contents__item\"><a class=\"doc-contents__link\" href=\"{href}\"{here}>{}</a></li>",
            escape(title)
        ));
    }
    html.push_str(&"</ul></li>".repeat(open.len()));
    html.push_str("</ul>");
    html
}

/// The pages either side of `current`, in the space's own order.
fn neighbours(space: &str, documents: &[Value], current: &str) -> (Option<Link>, Option<Link>) {
    let link = |document: &Value| {
        let path = document["path"].as_str().unwrap_or_default();
        Link {
            label: document["title"].as_str().unwrap_or(path).to_string(),
            href: page_url(space, path),
            note: String::new(),
        }
    };
    let Some(at) = documents.iter().position(|document| document["path"].as_str() == Some(current))
    else {
        return (None, None);
    };
    let previous = at.checked_sub(1).and_then(|before| documents.get(before)).map(link);
    (previous, documents.get(at + 1).map(link))
}

async fn space(
    backend: &Backend,
    store: &Store<'_>,
    key: &str,
    section: &'static str,
    // `chosen` is what somebody has just chosen, if they have, to show in the picker again.
    chosen: Option<Vec<String>>,
    flash: Flash,
) -> Result<String, Refusal> {
    let found = store
        .space(key)
        .await?
        .ok_or_else(|| Refusal::missing(format!("there is no space {key}")))?;
    let documents = store.tree(key).await?;
    let owners = owner_tags(&found);
    // The picker is a page of its own, opened to change who looks after the space.
    let asking = section == "change";
    let chosen = chosen.unwrap_or_else(|| found.owners.clone());
    let owners_problem = match asking {
        true => crate::owners::checked(&chosen).err().map(|refusal| sentence(&refusal.detail)),
        false => None,
    };
    let follows = sources::list(store)
        .await?
        .into_iter()
        .find(|source| source["space"] == json!(key) && source["managed"] == json!(true))
        .and_then(|source| source["settings"]["repository"].as_str().map(str::to_string));
    render(&SpacePage {
        flash,
        section,
        writes: backend.writes(),
        admin: removal::is_admin(backend),
        follows,
        key: key.to_string(),
        name: found.name,
        resource: found.resource.map(|resource| (resource_href(&resource), resource)),
        owners,
        owner_choices: match asking && backend.writes() {
            true => crate::owners::choices(backend, &chosen).await,
            false => Vec::new(),
        },
        owners_problem,
        count: pages(found.documents),
        tree: tree(key, &documents),
    })
}

/// A message as a sentence: capitalised, and ended with a full stop.
fn sentence(text: &str) -> String {
    let mut letters = text.chars();
    let first = letters.next().map(|c| c.to_uppercase().collect::<String>()).unwrap_or_default();
    let text = format!("{first}{}", letters.as_str());
    match text.ends_with('.') {
        true => text,
        false => format!("{text}."),
    }
}

/// Says who looks after a space, and tells the catalogue by re-announcing every page it holds.
/// The longest name a space may be given.
const MAX_SPACE_NAME: usize = 120;

/// An administrator's name for a space. Its key stays, so links to its pages keep working.
async fn renamed(
    backend: &Backend,
    store: &Store<'_>,
    key: &str,
    form: &Form,
) -> (Result<String, Refusal>, Option<String>) {
    if let Err(refusal) = removal::administers(backend, "renames spaces") {
        return (Err(refusal), None);
    }
    let name = field(form, "name").unwrap_or_default().trim().to_string();
    let problem = match name.chars().count() {
        0 => Some("Give the space a name.".to_string()),
        n if n > MAX_SPACE_NAME => Some(format!("A name is at most {MAX_SPACE_NAME} characters.")),
        _ => None,
    };
    if let Some(problem) = problem {
        let flash = Flash::refused(&Refusal::bad(problem));
        return (space(backend, store, key, "rename", None, flash).await, None);
    }
    let was = match store.space(key).await {
        Ok(Some(found)) => found.name,
        Ok(None) => return (Err(Refusal::missing(format!("there is no space {key}"))), None),
        Err(err) => return (Err(err), None),
    };
    match store.rename_space(key, &name, &removal::who(backend)).await {
        Ok(true) => {
            let detail = json!({ "from": was, "to": name });
            if let Err(err) = backend.audit("space.renamed", Some(key), detail).await {
                tracing::warn!(%err, key, "a space was renamed but not audited");
            }
            let notice = match was == name {
                true => "Nothing changed: that is its name already.".to_string(),
                false => format!("Renamed {was} to {name}."),
            };
            let page = space(backend, store, key, "pages", None, Flash::done(notice)).await;
            (page, Some(format!("/p/kb/spaces/{key}")))
        }
        Ok(false) => (Err(Refusal::missing(format!("there is no space {key}"))), None),
        Err(err) => (Err(err), None),
    }
}

async fn set_owners(
    backend: &Backend,
    store: &Store<'_>,
    key: &str,
    form: &Form,
) -> (Result<String, Refusal>, Option<String>) {
    if let Err(refusal) = writer(backend) {
        return (Err(refusal), None);
    }
    let chosen: Vec<String> =
        form.iter().filter(|(name, _)| name == "owners").map(|(_, value)| value.clone()).collect();
    let found = match store.space(key).await {
        Ok(Some(found)) => found,
        Ok(None) => return (Err(Refusal::missing(format!("there is no space {key}"))), None),
        Err(err) => return (Err(err), None),
    };
    if chosen.is_empty() {
        let problem = "Choose the team or teams who look after this space, or the organisation.";
        let flash = Flash::refused(&Refusal::bad(problem));
        return (space(backend, store, key, "change", Some(Vec::new()), flash).await, None);
    }
    match crate::owners::set(backend, store, &found, &chosen).await {
        Ok(announced) => {
            let notice = match announced {
                0 => "Nothing changed: the space already says that.".to_string(),
                1 => "Saved. One page was told to the catalogue again.".to_string(),
                many => format!("Saved. {many} pages were told to the catalogue again."),
            };
            let page = space(backend, store, key, "owners", None, Flash::done(notice)).await;
            (page, Some(format!("/p/kb/spaces/{key}/owners")))
        }
        Err(refusal) => {
            let flash = Flash::refused(&refusal);
            (space(backend, store, key, "change", Some(chosen), flash).await, None)
        }
    }
}

/// Only web addresses are linked, whatever a source said a page's address was.
fn web_address(url: Option<&str>) -> Option<String> {
    url.filter(|url| url.starts_with("https://") || url.starts_with("http://")).map(str::to_string)
}

/// A page's own first heading, when it opens with one, as HTML, and the page without it: the
/// heading row draws it instead.
fn leading_heading(html: &str) -> Option<(String, String)> {
    let rest = html.trim_start().strip_prefix("<h1")?;
    let opened = rest.find('>')? + 1;
    let closed = rest.find("</h1>")?;
    let inner = rest.get(opened..closed)?.trim().to_string();
    Some((inner, rest[closed + "</h1>".len()..].to_string())).filter(|(inner, _)| !inner.is_empty())
}

/// Words as they start a sentence: the first letter a capital, and the rest as they were.
fn starting(words: &str) -> String {
    let mut letters = words.chars();
    letters.next().map(|first| first.to_uppercase().chain(letters).collect()).unwrap_or_default()
}

/// What the page says of its edit in DOC, if it has one.
async fn edited_note(
    store: &Store<'_>,
    space: &str,
    path: &str,
) -> Result<Option<Edited>, Refusal> {
    let Some(edit) = store.edit(space, path).await? else { return Ok(None) };
    let Some(row) = store.document_row(space, path).await? else { return Ok(None) };
    let origin = crate::edits::origin(store, &row).await?;
    let origin_starting = starting(&origin.words);
    Ok(Some(Edited {
        by: edit.by.clone(),
        when: edit.updated_at.map(|at| when(&at.to_rfc3339())).unwrap_or_default(),
        changed: row.get("content_hash").and_then(Value::as_str) != Some(edit.base_hash.as_str()),
        gone: edit.source_gone,
        repository: origin.repository.is_some(),
        since: crate::edits::changed_since_proposed(&edit),
        pull_request: edit.pull_request.clone(),
        origin: origin.words,
        origin_starting,
    }))
}

/// What asking Agent Smith to run a runbook sends: the page, and the environments to choose from.
pub struct RunbookForm {
    pub space: String,
    pub path: String,
    pub environments: Vec<Environment>,
}

/// One environment a runbook can be run against, as the page's choice offers it.
pub struct Environment {
    pub value: &'static str,
    pub label: &'static str,
    pub selected: bool,
}

/// The environments a runbook can be run against, `environment` chosen, when the viewer could ask
/// Agent Smith to run it: it is running, and they may use it.
async fn runbook_choices(backend: &Backend, environment: &str) -> Option<Vec<Environment>> {
    let asked = backend
        .request("core.access", "access", json!({}), std::time::Duration::from_secs(5))
        .await
        .ok()?;
    let agent = &asked["plugins"]["agent"];
    if agent["write"] != true || agent["running"] != true {
        return None;
    }
    let labels = [("development", "Development"), ("test", "Test"), ("production", "Production")];
    Some(
        crate::runbooks::ENVIRONMENTS
            .iter()
            .map(|value| Environment {
                value,
                label: labels
                    .iter()
                    .find(|(held, _)| held == value)
                    .map_or(value, |(_, label)| label),
                selected: *value == environment,
            })
            .collect(),
    )
}

async fn document(
    backend: &Backend,
    store: &Store<'_>,
    space: &str,
    path: &str,
    flash: Flash,
) -> Result<String, Refusal> {
    // An archived space answers for none of its pages: it is out of sight, not merely unlisted.
    if store.space(space).await?.is_some_and(|space| space.archived_at.is_some()) {
        return Err(Refusal::missing(format!("{space} is archived")));
    }
    let found = match store.document(space, path).await? {
        Some(found) => Some(found),
        None => store.document(space, &format!("{path}.md")).await?,
    };
    let found = found.ok_or_else(|| Refusal::missing(format!("{space} has no page {path}")))?;
    let texts = |key: &str| -> Vec<String> {
        let listed = found[key].as_array().into_iter().flatten();
        listed.filter_map(Value::as_str).map(str::to_string).collect()
    };
    let spaces = spaces_by_key(store).await?;
    let html = text_of(&found, "html");
    let (heading, html) =
        leading_heading(&html).unwrap_or_else(|| (escape(&text_of(&found, "title")), html));
    let current = text_of(&found, "path");
    let documents = store.tree(space).await?;
    let (previous, next) = neighbours(space, &documents, &current);
    let faux = crate::faux::corpus(backend).await?.is_some();
    let address = page_url(space, &current).trim_start_matches("/p/kb/docs/").to_string();
    let edited = match faux {
        true => None,
        false => edited_note(store, space, &current).await?,
    };
    let runbook =
        match crate::runbooks::environment_of(&current, &found["front_matter"], &found["tags"]) {
            Some(environment) => runbook_choices(backend, environment).await.map(|environments| {
                RunbookForm { space: space.to_string(), path: current.clone(), environments }
            }),
            None => None,
        };
    render(&DocumentPage {
        flash,
        writes: backend.writes(),
        edit_href: (backend.writes() && !faux).then(|| format!("/p/kb/edit/{address}")),
        runbook,
        edited,
        address,
        space: Link {
            label: space_name(&spaces, space),
            href: format!("/p/kb/spaces/{space}"),
            note: String::new(),
        },
        heading,
        html,
        source: source_label(found["source"].as_str().unwrap_or_default()),
        original: web_address(found["source_url"].as_str()),
        updated: when(found["updated_at"].as_str().unwrap_or_default()),
        resources: texts("resources")
            .into_iter()
            .map(|resource| (resource_href(&resource), resource))
            .collect(),
        tags: texts("tags"),
        contents: contents(space, &documents, &current),
        previous,
        next,
    })
}

/// The editor for one page, starting from DOC's copy of it if it has one, or from what somebody
/// tried to save, with why it was refused.
async fn edit_page(
    backend: &Backend,
    store: &Store<'_>,
    space: &str,
    path: &str,
    tried: Option<(String, Flash)>,
) -> Result<String, Refusal> {
    let mut editing = crate::edits::start(backend, store, space, path).await?;
    let flash = match tried {
        Some((markdown, flash)) => {
            editing.markdown = markdown;
            editing.html = None;
            flash
        }
        None => Flash::default(),
    };
    let spaces = spaces_by_key(store).await?;
    let page_href = page_url(space, &editing.path);
    let explained = match &editing.origin.repository {
        Some(repository) => format!(
            "What you save is DOC's own copy of the page, shown in place of the one in {repository}, \
             which does not change unless a pull request proposing your changes is merged."
        ),
        None => format!(
            "What you save is DOC's own copy of the page, shown in place of the one from {}, which \
             does not change.",
            editing.origin.words
        ),
    };
    render(&EditPage {
        flash,
        writes: backend.writes(),
        admin: removal::is_admin(backend),
        space_key: space.to_string(),
        path: editing.path.clone(),
        sourced: editing.sourced,
        origin_starting: starting(&editing.origin.words),
        space: Link {
            label: space_name(&spaces, space),
            href: format!("/p/kb/spaces/{space}"),
            note: String::new(),
        },
        address: page_href.trim_start_matches("/p/kb/docs/").to_string(),
        page_href,
        title: editing.title,
        explained,
        markdown: editing.markdown,
        html: editing.html,
        images: json!(editing.images).to_string(),
    })
}

/// Saves an edit, discards one or proposes one, and shows the page with how it went; where the
/// address bar goes when that is the page itself.
async fn edited(
    backend: &Backend,
    store: &Store<'_>,
    space: &str,
    path: &str,
    change: &str,
    markdown: &str,
) -> (Result<String, Refusal>, Option<String>) {
    use crate::edits::{self, Saved};
    // Every call is boxed, so this one's state holds pointers rather than all of theirs.
    let done = match change {
        "edit" => Box::pin(edits::save(backend, store, space, path, markdown)).await.map(|saved| {
            Some(match saved {
                Saved::Saved => "Saved. DOC shows your version of this page.".to_string(),
                Saved::Same => {
                    "Saved. It says what its source says, so DOC shows the source's page again."
                        .to_string()
                }
            })
        }),
        "discard" => Box::pin(edits::discard(backend, store, space, path)).await.map(|kept| {
            kept.then(|| "Discarded. DOC shows the page as its source has it.".to_string())
        }),
        _ => Box::pin(edits::propose(backend, store, space, path))
            .await
            .map(|url| Some(format!("Proposed to the repository in the pull request at {url}."))),
    };
    match done {
        Ok(Some(notice)) => {
            let page = Box::pin(document(backend, store, space, path, Flash::done(notice))).await;
            (page, Some(page_url(space, path)))
        }
        // The page went with its edit, its source having dropped it: its space is what is left.
        Ok(None) => {
            let notice =
                Flash::done("Discarded. Its source no longer has this page, so it is gone.");
            let page = Box::pin(self::space(backend, store, space, "pages", None, notice)).await;
            (page, Some(format!("/p/kb/spaces/{space}")))
        }
        Err(refusal) if change == "edit" => {
            let tried = Some((markdown.to_string(), Flash::refused(&refusal)));
            (Box::pin(edit_page(backend, store, space, path, tried)).await, None)
        }
        Err(refusal) => {
            let flash = Flash::refused(&refusal);
            (Box::pin(document(backend, store, space, path, flash)).await, None)
        }
    }
}

/// The repositories a resource is connected to in the catalogue, up to a few: documentation
/// written in a repository documents what that repository is part of, so a service's panel shows
/// what is written in the repositories it is built from as well as what names the service itself.
async fn built_from(backend: &Backend, resource: &str) -> Vec<String> {
    let Some((kind, name)) = resource.split_once(':') else { return Vec::new() };
    // A faux service is built from its stand-in repository.
    if kind == "service" && crate::faux::shown(backend) {
        return vec![format!("repository:faux/{}", name.to_ascii_lowercase())];
    }
    let path =
        format!("resources/{kind}/{}", name.split('/').map(encoded).collect::<Vec<_>>().join("/"));
    let answer = backend.ask("resources", "GET", &path, None, None).await;
    let Ok((200, detail)) = answer else { return Vec::new() };
    detail["connections"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|connection| connection["kind"] == json!("Repository"))
        .filter_map(|connection| connection["name"].as_str())
        .take(CONNECTED_REPOSITORIES)
        .map(|name| format!("repository:{name}"))
        .collect()
}

/// The opening of a page, in a few sentences: enough to know whether it is the page you want.
/// Cut at the end of a sentence where one ends near enough, and at a word otherwise.
fn opening(plain: &str) -> String {
    let words: Vec<&str> = plain.split_whitespace().collect();
    if words.len() <= SUMMARY_WORDS {
        return words.join(" ");
    }
    let taken = words[..SUMMARY_WORDS].join(" ");
    match taken.rfind(['.', '!', '?']) {
        // Only if the sentence ends somewhere in the last third, so a long opening sentence is
        // not cut back to almost nothing.
        Some(at) if at * 3 > taken.len() * 2 => taken[..=at].to_string(),
        _ => format!("{taken}…"),
    }
}

/// The page in `space` that the catalogue would have named `wanted`, for a path whose characters
/// were not all ones a resource may be named with.
async fn named_as(store: &Store<'_>, space: &str, wanted: &str) -> Result<Option<Value>, Refusal> {
    let nameable = |path: &str| -> String {
        path.strip_suffix(".md")
            .unwrap_or(path)
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || "-_.:/@+".contains(c) { c } else { '-' })
            .collect()
    };
    let paths = store.tree(space).await?;
    let found =
        paths.iter().map(|page| text_of(page, "path")).find(|path| nameable(path) == wanted);
    match found {
        Some(path) => store.document(space, &path).await,
        None => Ok(None),
    }
}

/// A page's own summary, for its resource page in the catalogue: what it says, and a way in.
async fn summary(store: &Store<'_>, request: &Request) -> Result<String, Refusal> {
    let resource = query(request, "resource").ok_or_else(|| Refusal::bad("name the resource"))?;
    let resource = checked_resource(&resource)?;
    let named = resource
        .strip_prefix("documentation:")
        .ok_or_else(|| Refusal::bad("a summary is of a Documentation resource"))?;
    // The resource is named `<space>/<path>`, with the `.md` taken off when it was catalogued.
    let (space, path) = named
        .split_once('/')
        .ok_or_else(|| Refusal::bad("a Documentation resource is named space/path"))?;
    let found = match store.document(space, path).await? {
        Some(found) => Some(found),
        None => match store.document(space, &format!("{path}.md")).await? {
            Some(found) => Some(found),
            // The catalogue holds a name with only the characters a name may have, so a page
            // called `Getting Started.md` is `Getting-Started` there. Where the path itself does
            // not find it, the page whose name *would be* this one is the page.
            None => named_as(store, space, path).await?,
        },
    };
    let spaces = spaces_by_key(store).await?;
    let (page, summary) = match found {
        None => (None, String::new()),
        Some(found) => {
            let link = Link {
                label: text_of(&found, "title"),
                href: page_url(space, &text_of(&found, "path")),
                note: space_name(&spaces, space),
            };
            (Some(link), opening(&text_of(&found, "plain")))
        }
    };
    render(&SummaryFragment { page, summary })
}

/// The pages grouped by the space they are in — for a service, one group per repository it is
/// built from — each group's front page first, and the groups in the order the pages came.
fn grouped(found: &[Value], spaces: &BTreeMap<String, Space>) -> Vec<Group> {
    let mut groups: Vec<Group> = Vec::new();
    for document in found {
        let key = text_of(document, "space");
        let link = Link {
            note: source_label(document["source"].as_str().unwrap_or_default()).to_string(),
            ..document_link(document, spaces)
        };
        let front = front_page(document["path"].as_str().unwrap_or_default());
        let group = match groups.iter_mut().find(|group| group.space.note == key) {
            Some(group) => group,
            None => {
                let tags = spaces.get(&key).map(origin).unwrap_or_default();
                groups.push(Group {
                    space: Link {
                        label: space_name(spaces, &key),
                        href: format!("/p/kb/spaces/{key}"),
                        note: key,
                    },
                    documents: Vec::new(),
                    front: false,
                    tags,
                    more: 0,
                });
                groups.last_mut().expect("the group just pushed")
            }
        };
        match front {
            true => {
                group.documents.insert(0, link);
                group.front = true;
            }
            false => group.documents.push(link),
        }
    }
    // A space with a README is read from there, so its docs label leads to it: `docs > colwill/ccc`
    // is the README of colwill/ccc.
    for group in groups.iter_mut().filter(|group| group.front) {
        let readme = group.documents[0].href.clone();
        for tag in group.tags.iter_mut().filter(|tag| tag.label == DOCS) {
            tag.href.clone_from(&readme);
        }
    }
    groups
}

async fn panel(backend: &Backend, store: &Store<'_>, request: &Request) -> Result<String, Refusal> {
    let resource = query(request, "resource").ok_or_else(|| Refusal::bad("name the resource"))?;
    let resource = checked_resource(&resource)?;
    let spaces = spaces_by_key(store).await?;
    let mut wanted = vec![resource.clone()];
    // Only for a service: a repository's own panel is about that repository, and a team's docs
    // are the ones that name the team rather than everything its services are built from.
    let mut linked = Vec::new();
    if let Some(service) = resource.strip_prefix("service:") {
        wanted.extend(built_from(backend, &resource).await);
        linked = store.linked(service).await?;
    }
    let found = store.documenting_any(&wanted, &linked).await?;
    let mut groups = grouped(&found, &spaces);
    // Built from one repository, and that repository has a README: the README is the service's
    // documentation, so the panel opens with it rather than listing it among the rest. Built from
    // several, it is a table of contents with each repository's README at the top of its own
    // heading, which is what a person reads down to find the one they want.
    let featured = match groups.as_mut_slice() {
        [only] if only.front => {
            let page = only.documents.remove(0);
            let path = found
                .iter()
                .filter(|document| text_of(document, "space") == only.space.note)
                .map(|document| text_of(document, "path"))
                .find(|path| front_page(path))
                .unwrap_or_default();
            let summary = store
                .document(&only.space.note, &path)
                .await?
                .map(|document| opening(&text_of(&document, "plain")))
                .unwrap_or_default();
            Some(Featured { summary, page, tags: only.tags.clone() })
        }
        _ => None,
    };
    // On a service's page the panel points rather than lists: each repository's README, and how
    // many more pages its space holds, which that space's page lists in full.
    if resource.starts_with("service:") {
        for group in &mut groups {
            // The featured README has left its group already; any other group's is its first.
            let keep = usize::from(group.front && featured.is_none());
            group.more = group.documents.len().saturating_sub(keep);
            group.documents.truncate(keep);
        }
    }
    groups.retain(|group| !group.documents.is_empty() || group.more > 0);
    let headings = groups.len() > 1;
    let catalogue =
        resource.strip_prefix("service:").map(|name| format!("/p/kb/catalogue/{}", encoded(name)));
    let repositories = match featured.is_some() || !groups.is_empty() {
        true => Vec::new(),
        false => wanted
            .iter()
            .filter_map(|wanted| wanted.strip_prefix("repository:"))
            .map(str::to_string)
            .collect(),
    };
    render(&PanelFragment { featured, groups, headings, catalogue, repositories })
}

fn source_row(source: &Value) -> SourceRow {
    let setting = |key: &str| text(&source["settings"][key]);
    let kind = source["kind"].as_str().unwrap_or_default();
    let what = match kind {
        "github" => match setting("path").as_str() {
            "" => format!("{} at {}", setting("repository"), setting("ref")),
            path => format!("{}/{path} at {}", setting("repository"), setting("ref")),
        },
        "confluence" => {
            format!("Space {} at {} ({})", setting("space_key"), setting("url"), setting("flavour"))
        }
        "drive" => format!("Folder {}", setting("folder")),
        "git" => {
            let at = match (setting("branch").as_str(), setting("commit").as_str()) {
                ("", "") => String::new(),
                (branch, "") => format!(" on {branch}"),
                ("", commit) => format!(" at {}", &commit[..commit.len().min(12)]),
                (branch, commit) => format!(" on {branch} at {}", &commit[..commit.len().min(12)]),
            };
            format!(
                "{}{at}, uploaded as a zip: upload it again to update it",
                setting("repository")
            )
        }
        "plugin" => format!(
            "Written by the {} plugin from what it knows, and written again when that changes",
            setting("plugin")
        ),
        _ => "Imports through the CLI or the API".into(),
    };
    let resource =
        resource_href(&format!("documentation-source:{}", sources::resource_name(source)));
    let (badge, state) = match source["last_state"].as_str() {
        Some("succeeded") => ("doc-badge--up", "Synced"),
        Some("failed") => ("doc-badge--error", "Failed"),
        Some("empty") => ("doc-badge--unknown", "No Markdown"),
        Some("waiting") => ("doc-badge--degraded", "Waiting for GitHub"),
        _ => ("doc-badge--unknown", "Never synced"),
    };
    SourceRow {
        id: text_of(source, "id"),
        kind: source_label(kind),
        space: text_of(source, "space"),
        what,
        resource,
        managed: source["managed"] == json!(true),
        repository: source["settings"]["repository"].as_str().unwrap_or_default().to_string(),
        schedule: text_of(source, "schedule"),
        syncs: !matches!(kind, "upload" | "git" | "plugin"),
        last: source["last_sync_at"].as_str().map(when).unwrap_or_default(),
        badge,
        state,
        error: text_of(source, "last_error"),
        archived: match (source["archived_at"].as_str(), source["archived_by"].as_str()) {
            (Some(at), Some(by)) => format!("Archived by {by}, {}", when(at)),
            (Some(at), None) => format!("Archived {}", when(at)),
            _ => String::new(),
        },
    }
}

fn writer(backend: &Backend) -> Result<(), Refusal> {
    match backend.writes() {
        true => Ok(()),
        false => Err(Refusal::forbidden(
            "only someone who can write to the Knowledge Base manages its sources",
        )),
    }
}

/// The repository sweep, run now rather than when its schedule next comes round: what it found,
/// what it took on and what it is reading, said plainly enough to tell where it stopped.
async fn look_for_repositories(backend: &Backend, store: &Store<'_>) -> Result<String, Refusal> {
    writer(backend)?;
    let done = crate::repositories::follow(backend, store)
        .await
        .map_err(|err| Refusal::unavailable(err.to_string()))?;
    let flash = match done["following"] == json!(true) {
        false => Flash::refused(&Refusal::bad(
            "Documentation from repositories is off. Turn it on under Features, and this reads \
             every repository the platform knows.",
        )),
        true => {
            let count = |key: &str| done[key].as_i64().unwrap_or_default();
            let reading = done["reading"].as_array().map(Vec::len).unwrap_or_default();
            Flash::done(format!(
                "Looked at {} repositories: took on {}, reading {} now. Reload to see how they \
                 went.",
                count("repositories"),
                count("added"),
                reading,
            ))
        }
    };
    sources_page(backend, store, flash).await
}

async fn sources_page(
    backend: &Backend,
    store: &Store<'_>,
    flash: Flash,
) -> Result<String, Refusal> {
    writer(backend)?;
    let sources = sources::list(store).await?.iter().map(source_row).collect();
    let admin = removal::is_admin(backend);
    let forgotten = match admin {
        true => removal::forgotten(backend).await,
        false => Vec::new(),
    };
    let archived: Vec<SourceRow> = match admin {
        true => store.archived_sources().await?.iter().map(source_row).collect(),
        false => Vec::new(),
    };
    render(&SourcesPage { flash, writes: true, admin, sources, archived, forgotten })
}

async fn source_change(
    backend: &Backend,
    store: &Store<'_>,
    id: &str,
    change: &str,
    form: &Form,
) -> Result<String, Refusal> {
    writer(backend)?;
    let id: Uuid = id.parse().map_err(|_| Refusal::bad("a source is named by its ID"))?;
    // Archived ones are not in the list any more, and are still restored and deleted from here.
    let source =
        store.any_source(id).await?.ok_or_else(|| Refusal::missing("there is no such source"))?;
    let named = format!(
        "The {} source for {}",
        source_label(source["kind"].as_str().unwrap_or_default()),
        text_of(&source, "space")
    );
    let flash = match change {
        "sync" => {
            let task = backend.task(json!({ "source": id })).await?;
            Flash::done(format!("{named} is syncing, as task {task}. Reload to see how it went."))
        }
        "archive" => {
            removal::archive_source(backend, store, id).await?;
            Flash::done(format!(
                "{named} is archived. It syncs no more and is out of the catalogue; the pages it \
                 brought in stay where they are. Restore it, or delete it for good, below."
            ))
        }
        "restore" => {
            removal::restore_source(backend, store, id).await?;
            Flash::done(format!("{named} is back, and syncs again."))
        }
        "delete" => {
            removal::delete_source(backend, store, id).await?;
            Flash::done(format!("{named} is deleted. The pages it brought in are still here."))
        }
        "schedule" => match api::checked_schedule(field(form, "schedule")) {
            Ok(schedule) => {
                store.set_schedule(id, schedule.as_deref()).await?;
                sources::announce(backend, store).await;
                Flash::done(match schedule {
                    Some(schedule) => format!("{named} now syncs on {schedule}."),
                    None => format!("{named} now syncs only when asked."),
                })
            }
            Err(refusal) => Flash::refused(&refusal),
        },
        _ => return Err(Refusal::missing("no such change")),
    };
    sources_page(backend, store, flash).await
}

#[cfg(test)]
mod contents_tests {
    use super::*;

    fn documents() -> Vec<Value> {
        ["index.md", "guides/deploy.md", "guides/roll-back.md", "faq.md"]
            .iter()
            .map(|path| json!({ "path": path, "title": path.trim_end_matches(".md") }))
            .collect()
    }

    #[test]
    fn the_contents_nest_folders_and_mark_the_page_being_read() {
        let html = contents("ops", &documents(), "guides/deploy.md");
        assert!(html.contains(r#"<span class="doc-contents__folder">Guides</span>"#), "{html}");
        assert!(html.contains(
            r#"href="/p/kb/docs/ops/guides/deploy" aria-current="page">guides/deploy</a>"#
        ));
        assert_eq!(html.matches("aria-current").count(), 1);
        assert_eq!(html.matches("<ul").count(), html.matches("</ul>").count());
    }

    #[test]
    fn previous_and_next_follow_the_spaces_own_order() {
        let (previous, next) = neighbours("ops", &documents(), "guides/roll-back.md");
        assert_eq!(previous.map(|link| link.href).as_deref(), Some("/p/kb/docs/ops/guides/deploy"));
        assert_eq!(next.map(|link| link.label).as_deref(), Some("faq"));
        let (previous, _) = neighbours("ops", &documents(), "index.md");
        assert!(previous.is_none(), "the first page has nothing before it");
        let (_, next) = neighbours("ops", &documents(), "faq.md");
        assert!(next.is_none());
    }
}
