//! The Knowledge Base's JSON routes: imports, sources, spaces, documents and search.

use doc_plugin_sdk::{Backend, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::Refusal;
use crate::imports::{self, checked_resource, page_url, slug};
use crate::store::Store;
use crate::{archive, owners, runbooks, sources};

type Answer = Result<(u16, Value), Refusal>;

const MAX_RESULTS: i64 = 50;

/// Pages to import, as `imports/pages` takes them: each a path such as `guides/setup.md` and its
/// Markdown, front matter and all.
#[derive(serde::Deserialize)]
struct PagesImport {
    space: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    owners: Vec<String>,
    pages: Vec<PageText>,
}

#[derive(serde::Deserialize)]
struct PageText {
    path: String,
    content: String,
}

/// However many pages come as JSON, no more than an archive may hold.
const MAX_JSON_PAGES: usize = 2_000;

impl PagesImport {
    fn files(&self) -> Result<std::collections::BTreeMap<String, Vec<u8>>, Refusal> {
        if self.pages.is_empty() || self.pages.len() > MAX_JSON_PAGES {
            return Err(Refusal::bad(format!("import 1 to {MAX_JSON_PAGES} pages at a time")));
        }
        let mut files = std::collections::BTreeMap::new();
        for page in &self.pages {
            let path = page.path.trim().trim_start_matches('/');
            let safe = !path.is_empty()
                && path.ends_with(".md")
                && !path.split('/').any(|part| part.is_empty() || part == "." || part == "..");
            if !safe {
                return Err(Refusal::bad(format!("`{}` is not a Markdown page's path", page.path)));
            }
            files.insert(path.to_string(), page.content.as_bytes().to_vec());
        }
        Ok(files)
    }
}

/// A plugin writing pages into a space itself, such as DNS writing how to use it: only those its
/// `publisher-plugins` setting names. Each plugin's pages are its own source in the space, so it
/// replaces what it wrote before and nothing else. A space it makes says who keeps it, as any
/// space must; one that exists keeps whoever keeps it.
async fn published(backend: &Backend, request: &Request) -> Answer {
    let asking = match backend.caller() {
        Some(caller) if caller.kind == "plugin" => caller.id.clone().unwrap_or_default(),
        _ => return Err(Refusal::forbidden("discovery routes are for plugins")),
    };
    if !backend.settings().list(crate::PUBLISHER_PLUGINS).contains(&asking) {
        return Err(Refusal::forbidden(format!(
            "{asking} is not one of the plugins that publish pages; it asks to be added to {}",
            crate::PUBLISHER_PLUGINS
        )));
    }
    let asked: PagesImport = serde_json::from_slice(&request.body)
        .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))?;
    let files = asked.files()?;
    let key = slug(&asked.space);
    if key.is_empty() {
        return Err(Refusal::bad("name the space the pages go in"));
    }
    let store = Store(backend);
    if store.space(&key).await?.is_none() {
        let owners = owners::checked(&asked.owners)?;
        if owners.is_empty() {
            return Err(Refusal::bad(owners::NEEDED));
        }
        let name = asked.title.as_deref().map(str::trim).filter(|title| !title.is_empty());
        store.ensure_space(&key, name.unwrap_or(&key), None, &owners).await?;
    }
    let source = store.plugin_source(&key, &asking).await?;
    let origin = imports::Origin { source, web: None };
    // Written while the plugin waits, so its answer means the pages are there.
    let (import, pages) = imports::write_now(backend, &store, &files, &key, origin).await?;
    Ok((200, json!({ "import": import, "space": key, "pages": pages })))
}

pub fn query(request: &Request, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

/// Every value a query names, such as `?owner=team:a&owner=team:b`.
pub fn queries(request: &Request, key: &str) -> Vec<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .filter(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
}

pub fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// A snippet for HTML: everything escaped, then the matches marked.
pub fn snippet(raw: &str) -> String {
    escape(raw).replace('⟦', "<mark>").replace('⟧', "</mark>")
}

/// A source to add; which of the fields it needs depends on its kind.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewSource {
    kind: String,
    space: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    resource: Option<String>,
    /// Who looks after the space, each written `kind:name`: teams, or one organisation. Needed
    /// only when the space is new.
    #[serde(default)]
    owners: Vec<String>,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default, rename = "ref")]
    reference: Option<String>,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    flavour: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    space_key: Option<String>,
    #[serde(default)]
    credential: Option<String>,
    #[serde(default)]
    folder: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourceChange {
    schedule: Option<String>,
}

fn required(value: Option<String>, what: &str) -> Result<String, Refusal> {
    value
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| Refusal::bad(format!("a source of this kind needs {what}")))
}

pub fn checked_schedule(schedule: Option<String>) -> Result<Option<String>, Refusal> {
    match schedule
        .map(|schedule| schedule.trim().to_string())
        .filter(|schedule| !schedule.is_empty())
    {
        Some(schedule) => match schedule.parse::<croner::Cron>() {
            Ok(_) => Ok(Some(schedule)),
            Err(err) => Err(Refusal::bad(format!("`{schedule}` is not a cron schedule: {err}"))),
        },
        None => Ok(None),
    }
}

/// What a kind of source keeps in its settings, checked.
fn settings_of(asked: NewSource) -> Result<Value, Refusal> {
    match asked.kind.as_str() {
        "github" => Ok(json!({
            "repository": required(asked.repository, "a repository, written owner/name")?,
            "ref": asked.reference.unwrap_or_else(|| "HEAD".into()),
            "path": asked.path.unwrap_or_default(),
        })),
        "confluence" => {
            let flavour = asked.flavour.unwrap_or_else(|| "cloud".into());
            if crate::confluence::Flavour::named(&flavour).is_none() {
                return Err(Refusal::bad("a Confluence source is cloud or datacenter"));
            }
            Ok(json!({
                "flavour": flavour,
                "url": required(asked.url, "the site's URL")?,
                "space_key": required(asked.space_key, "the Confluence space's key")?,
                "credential": required(asked.credential, "the name of its credential")?,
            }))
        }
        "drive" => Ok(json!({
            "folder": required(asked.folder, "the folder or shared drive's ID")?,
            "credential": required(asked.credential, "the name of its credential")?,
        })),
        other => Err(Refusal::bad(format!(
            "`{other}` is not a kind of source: github, confluence or drive"
        ))),
    }
}

/// Adds a source, making its space if it is new.
pub async fn add_source(
    backend: &Backend,
    store: &Store<'_>,
    asked: Value,
) -> Result<Value, Refusal> {
    let asked: NewSource = serde_json::from_value(asked)
        .map_err(|err| Refusal::bad(format!("the body is not a source: {err}")))?;
    let key = slug(&asked.space);
    if key.is_empty() {
        return Err(Refusal::bad("name the space the source fills"));
    }
    let resource = asked.resource.as_deref().map(checked_resource).transpose()?;
    let owners = owners::checked(&asked.owners)?;
    let schedule = checked_schedule(asked.schedule.clone())?;
    let (kind, name) = (asked.kind.clone(), asked.name.clone());
    let settings = settings_of(asked)?;
    // A space is made here when the source names one that does not exist yet, and a space says
    // who keeps it from the moment it is made.
    let held = store.space(&key).await?;
    if owners.is_empty() && held.is_none() {
        return Err(Refusal::bad(owners::NEEDED));
    }
    let space =
        store.ensure_space(&key, name.as_deref().unwrap_or(&key), resource.as_deref(), &[]).await?;
    // Set rather than written with the space, so the pages a space already holds are re-announced
    // when whoever keeps it changes.
    if !owners.is_empty() {
        owners::set(backend, store, &space, &owners).await?;
    }
    let id = Uuid::now_v7();
    store.add_source(id, &kind, &key, &settings, schedule.as_deref()).await?;
    // The catalogue holds a source of its own, which a page it brought in is connected to.
    sources::announce(backend, store).await;
    Ok(json!({ "id": id, "kind": kind, "space": key, "settings": settings, "schedule": schedule }))
}

pub async fn handle(backend: &Backend, request: Request) -> Response {
    crate::faux::check(backend).await;
    let response = answered(backend, request).await;
    crate::faux::marked(backend, response)
}

async fn answered(backend: &Backend, request: Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    // Faux data changes nothing, and nothing changes it: a change would go to the real Knowledge
    // Base, which is not what is being shown.
    let reads = request.method == "GET"
        || segments.as_slice() == ["api", "mcp"]
        || segments.as_slice() == ["ui", "search"];
    if !reads && crate::faux::shown(backend) {
        return Refusal {
            status: 409,
            detail: "faux data is on for the Knowledge Base, so nothing in it can be changed; \
                     turn it off under Provide faux data to on faux-data's Settings page"
                .into(),
        }
        .response();
    }
    match segments.as_slice() {
        ["api", "mcp"] => return crate::mcp::handle(backend, &request).await,
        // A page in a folder names it with its `/`s, however they were written in the address.
        ["ui", "attachments", space, page @ .., name] if !page.is_empty() => {
            let page: Vec<String> = page.iter().map(|part| decoded(part)).collect();
            return attachment(backend, space, &page.join("/"), name).await;
        }
        ["ui", route @ ..] => return crate::ui::handle(backend, &request, route).await,
        _ => {}
    }
    let answer = match segments.as_slice() {
        ["api", route @ ..] => api(backend, &request, route).await,
        ["discovery", "pages"] if request.method == "POST" => published(backend, &request).await,
        _ => Err(Refusal { status: 404, detail: "no such route".into() }),
    };
    match answer {
        Ok((status, value)) => Response::new(
            status,
            "application/json",
            serde_json::to_vec(&value).unwrap_or_default(),
        ),
        Err(refusal) => refusal.response(),
    }
}

/// Links a space to a service in the catalogue, or takes the link away, as an administrator: the
/// space's pages are then the service's docs too. What happened, in a sentence.
pub async fn link_space(
    backend: &Backend,
    store: &Store<'_>,
    service: &str,
    space: &str,
    link: bool,
) -> Result<String, Refusal> {
    crate::removal::administers(backend, "links spaces to services")?;
    let found = store
        .space(space)
        .await?
        .ok_or_else(|| Refusal::missing(format!("there is no space {space}")))?;
    let by = backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "someone".into());
    let changed = match link {
        true => store.link(service, &found.key, &by).await?,
        false => store.unlink(service, &found.key).await?,
    };
    if changed {
        let action = if link { "service.linked" } else { "service.unlinked" };
        let detail = json!({ "service": service, "space": found.key, "by": by });
        if let Err(err) = backend.audit(action, Some(service), detail).await {
            tracing::warn!(%err, "a space's link to a service was not audited");
        }
    }
    Ok(match (link, changed) {
        (true, true) => {
            format!("{} is linked to {service}: its pages are {service}'s docs too.", found.name)
        }
        (true, false) => format!("{} was linked to {service} already.", found.name),
        (false, true) => format!("{} is no longer linked to {service}.", found.name),
        (false, false) => format!("{} was not linked to {service}.", found.name),
    })
}

pub fn decoded(text: &str) -> String {
    url::form_urlencoded::parse(format!("x={}", text.replace('+', "%2B")).as_bytes())
        .next()
        .map(|(_, value)| value.into_owned())
        .unwrap_or_default()
}

/// Raster images, which a page may show inline; anything else is only ever downloaded.
const INLINE: [&str; 4] = ["image/png", "image/jpeg", "image/gif", "image/webp"];

/// An image or file a page shows. Nothing an attachment holds can run as part of DOC.
async fn attachment(backend: &Backend, space: &str, page: &str, name: &str) -> Response {
    let store = Store(backend);
    match store.attachment(&decoded(space), page, &decoded(name)).await {
        Ok(Some((content_type, bytes))) if INLINE.contains(&content_type.as_str()) => {
            Response::new(200, &content_type, bytes)
                .with_header("cache-control", "private, max-age=300")
                .with_header("x-content-type-options", "nosniff")
        }
        Ok(Some((_, bytes))) => Response::new(200, "application/octet-stream", bytes)
            .with_header("content-disposition", "attachment")
            .with_header("x-content-type-options", "nosniff"),
        Ok(None) => Refusal::missing("there is no such attachment").response(),
        Err(refusal) => refusal.response(),
    }
}

async fn api(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let store = Store(backend);
    match (request.method.as_str(), path) {
        ("POST", ["git"]) => {
            let files = archive::zipped(&request.body).map_err(Refusal::bad)?;
            let space = query(request, "space").unwrap_or_default();
            let resource = query(request, "resource");
            let owners = queries(request, "owner");
            let imported =
                crate::git::import(backend, &store, files, &space, resource.as_deref(), &owners)
                    .await?;
            Ok((
                202,
                json!({
                    "repository": imported.repository,
                    "space": imported.space,
                    "pages": imported.pages,
                }),
            ))
        }
        ("POST", ["imports"]) => {
            let files = archive::read(&request.body).map_err(Refusal::bad)?;
            let space = query(request, "space");
            let resource = query(request, "resource");
            let owners = queries(request, "owner");
            let started = imports::start(
                backend,
                &store,
                &files,
                space.as_deref(),
                resource.as_deref(),
                &owners,
                None,
            )
            .await?;
            Ok((
                202,
                json!({
                    "task": started.task,
                    "import": started.import,
                    "space": started.space.key,
                    "pages": started.pages,
                }),
            ))
        }
        // The same import, from pages written as JSON rather than an archive: how another plugin,
        // which can only send JSON, brings Markdown in (the Data Vacuum, FEAT-VACUUM).
        ("POST", ["imports", "pages"]) => {
            let asked: PagesImport = serde_json::from_slice(&request.body)
                .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))?;
            let files = asked.files()?;
            let key = slug(&asked.space);
            if key.is_empty() {
                return Err(Refusal::bad("name the space the pages go in"));
            }
            let name = asked.title.as_deref().map(str::trim).filter(|title| !title.is_empty());
            let resource = asked.resource.as_deref().filter(|resource| !resource.is_empty());
            store.ensure_space(&key, name.unwrap_or(&key), resource, &asked.owners).await?;
            let started =
                imports::start(backend, &store, &files, Some(&key), resource, &asked.owners, None)
                    .await?;
            Ok((
                202,
                json!({
                    "task": started.task,
                    "import": started.import,
                    "space": started.space.key,
                    "pages": started.pages,
                }),
            ))
        }
        ("GET", ["imports", id]) => {
            let id: Uuid = id.parse().map_err(|_| Refusal::bad("an import is named by its ID"))?;
            let found = store
                .import_view(id)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such import"))?;
            Ok((200, found))
        }
        ("GET", ["search"]) => {
            let text = query(request, "q")
                .ok_or_else(|| Refusal::bad("say what to search for with `q`"))?;
            let limit = query(request, "limit")
                .and_then(|limit| limit.parse().ok())
                .unwrap_or(20)
                .clamp(1, MAX_RESULTS);
            let found = store.search(&text, query(request, "space").as_deref(), limit).await?;
            let results: Vec<Value> = found
                .into_iter()
                .map(|mut hit| {
                    let space = hit["space"].as_str().unwrap_or_default().to_string();
                    let path = hit["path"].as_str().unwrap_or_default().to_string();
                    hit["snippet"] = json!(snippet(hit["snippet"].as_str().unwrap_or_default()));
                    hit["url"] = json!(page_url(&space, &path));
                    hit
                })
                .collect();
            Ok((200, json!({ "query": text, "results": results })))
        }
        ("GET", ["spaces"]) => Ok((200, json!(store.spaces().await?))),
        ("GET", ["spaces", "archived"]) => {
            crate::removal::admin(backend)?;
            let archived: Vec<Value> = store
                .archived_spaces()
                .await?
                .into_iter()
                .map(|space| json!({ "key": space.key, "name": space.name, "pages": space.documents, "archived_at": space.archived_at, "archived_by": space.archived_by }))
                .collect();
            Ok((200, json!({ "spaces": archived })))
        }
        ("GET", ["spaces", key]) => {
            let space = store
                .space(key)
                .await?
                .ok_or_else(|| Refusal::missing(format!("there is no space {key}")))?;
            Ok((200, json!({ "space": space, "documents": store.tree(key).await? })))
        }
        ("GET", ["runbooks"]) => Ok((200, json!({ "runbooks": runbooks::list(&store).await? }))),
        ("GET", ["runbooks", space, rest @ ..]) if !rest.is_empty() => {
            Ok((200, runbooks::one(&store, space, &rest.join("/")).await?))
        }
        ("GET", ["documents", space, rest @ ..]) if !rest.is_empty() => {
            let wanted = rest.join("/");
            let found = match store.document(space, &wanted).await? {
                Some(found) => Some(found),
                None => store.document(space, &format!("{wanted}.md")).await?,
            };
            Ok((
                200,
                found.ok_or_else(|| Refusal::missing(format!("{space} has no page {wanted}")))?,
            ))
        }
        // Archiving is the first step and deleting the second: `DELETE` on a space that has not
        // been archived is refused, and says so.
        ("POST", ["spaces", key, "archive"]) => {
            let archived = crate::removal::archive_space(backend, &store, key).await?;
            Ok((200, json!({ "archived": key, "pages": archived.pages })))
        }
        ("POST", ["spaces", key, "restore"]) => {
            let restored = crate::removal::restore_space(backend, &store, key).await?;
            Ok((200, json!({ "restored": key, "pages": restored.pages })))
        }
        ("POST", ["sources", id, change @ ("archive" | "restore")]) => {
            let id = id.parse().map_err(|_| Refusal::bad("a source is named by its ID"))?;
            let archiving = *change == "archive";
            let space = match archiving {
                true => crate::removal::archive_source(backend, &store, id).await?,
                false => crate::removal::restore_source(backend, &store, id).await?,
            };
            let said = match archiving {
                true => "archived",
                false => "restored",
            };
            Ok((200, json!({ said: id, "space": space })))
        }
        ("DELETE", ["sources", id]) => {
            let id = id.parse().map_err(|_| Refusal::bad("a source is named by its ID"))?;
            let space = crate::removal::delete_source(backend, &store, id).await?;
            Ok((200, json!({ "deleted": id, "space": space })))
        }
        ("DELETE", ["spaces", key]) => {
            let deleted = crate::removal::delete_space(backend, &store, key).await?;
            Ok((
                200,
                json!({ "deleted": key, "pages": deleted.pages, "unfollowed": deleted.repositories }),
            ))
        }
        ("DELETE", ["documents", space, rest @ ..]) if !rest.is_empty() => {
            let wanted = rest.join("/");
            // Asked for as it is shown, without its `.md`, or as it is kept.
            let path = match store.document(space, &wanted).await? {
                Some(_) => wanted,
                None => format!("{wanted}.md"),
            };
            crate::removal::delete_page(backend, &store, space, &path).await?;
            Ok((200, json!({ "deleted": { "space": space, "path": path } })))
        }
        ("GET", ["repositories", "unfollowed"]) => {
            crate::removal::admin(backend)?;
            Ok((200, json!(crate::removal::forgotten(backend).await)))
        }
        ("DELETE", ["repositories", owner, name]) => {
            let repository = format!("{owner}/{name}");
            let deleted = crate::removal::forget_repository(backend, &store, &repository).await?;
            Ok((200, json!({ "unfollowed": repository, "pages": deleted.pages })))
        }
        ("POST", ["repositories", owner, name, "follow"]) => {
            let repository = format!("{owner}/{name}");
            let followed = crate::removal::follow_again(backend, &repository).await?;
            Ok((200, json!({ "repository": repository, "followed_again": followed })))
        }
        ("GET", ["services", service, "spaces"]) => Ok((200, json!(store.linked(service).await?))),
        ("PUT" | "DELETE", ["services", service, "spaces", space]) => {
            let said = link_space(backend, &store, service, space, request.method == "PUT").await?;
            Ok((
                200,
                json!({ "service": service, "spaces": store.linked(service).await?, "said": said }),
            ))
        }
        ("GET", ["sources"]) => Ok((200, json!(sources::list(&store).await?))),
        ("POST", ["sources"]) => {
            let asked = serde_json::from_slice(&request.body)
                .map_err(|err| Refusal::bad(format!("the body is not a source: {err}")))?;
            Ok((201, add_source(backend, &store, asked).await?))
        }
        ("PATCH", ["sources", id]) => {
            let id: Uuid = id.parse().map_err(|_| Refusal::bad("a source is named by its ID"))?;
            let change: SourceChange = serde_json::from_slice(&request.body).map_err(|err| {
                Refusal::bad(format!("the body is not a change to a source: {err}"))
            })?;
            let schedule = checked_schedule(change.schedule)?;
            sources::get(&store, id)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such source"))?;
            store.set_schedule(id, schedule.as_deref()).await?;
            sources::announce(backend, &store).await;
            Ok((200, sources::get(&store, id).await?.unwrap_or_default()))
        }
        ("POST", ["sources", id, "sync"]) => {
            let id: Uuid = id.parse().map_err(|_| Refusal::bad("a source is named by its ID"))?;
            sources::get(&store, id)
                .await?
                .ok_or_else(|| Refusal::missing("there is no such source"))?;
            let task = backend.task(json!({ "source": id })).await?;
            Ok((202, json!({ "task": task, "source": id })))
        }
        _ => Err(Refusal { status: 404, detail: "no such route".into() }),
    }
}
