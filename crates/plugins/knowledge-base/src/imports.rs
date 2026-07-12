//! Imports as a chain of batch tasks. Each run renders a batch of staged pages, skips those whose
//! hash is unchanged, saves its place, and queues the next batch; the last run removes what the
//! import no longer has. Every document changed is announced for Resource Definitions.

use std::collections::{BTreeMap, BTreeSet};

use doc_plugin_sdk::{Backend, PluginError};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::Refusal;
use crate::markdown::{self, front_matter};
use crate::mkdocs::{self, Site};
use crate::store::{Space, Store};

/// Pages per run, well inside the 30 s a run may take.
const BATCH: i32 = 20;
pub const IMPORTED: &str = "plugin.kb.document.imported";
pub const REMOVED: &str = "plugin.kb.document.removed";

pub struct Started {
    pub import: Uuid,
    pub space: Space,
    pub pages: usize,
    pub task: Uuid,
}

/// A page to import: Markdown, Confluence storage format or a Drive export, with what its source
/// knows of it (`labels`, `version`, `url`, and `unchanged` when there is nothing to render).
pub struct Page {
    pub path: String,
    pub title: Option<String>,
    pub content: String,
    pub format: &'static str,
    pub extra: Value,
}

/// A space's key: lower case letters, digits and dashes.
pub fn slug(text: &str) -> String {
    let mut slug = String::new();
    for c in text.trim().chars() {
        match c {
            c if c.is_ascii_alphanumeric() => slug.push(c.to_ascii_lowercase()),
            _ if !slug.ends_with('-') && !slug.is_empty() => slug.push('-'),
            _ => {}
        }
    }
    slug.trim_end_matches('-').chars().take(64).collect()
}

/// `kind:name`, as a resource is named in Resource Definitions, with the kind in one spelling.
pub fn checked_resource(resource: &str) -> Result<String, Refusal> {
    let kind_of = |kind: &str| -> String {
        kind.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase()
    };
    match resource.trim().split_once(':') {
        Some((kind, name)) if !kind_of(kind).is_empty() && !name.trim().is_empty() => {
            Ok(format!("{}:{}", kind_of(kind), name.trim()))
        }
        _ => Err(Refusal::bad(format!(
            "write the resource as kind:name, such as service:card-gateway, not `{resource}`"
        ))),
    }
}

/// A space's front page: a `README` or an `index` at its root, whatever its case.
pub fn front_page(path: &str) -> bool {
    if path.contains('/') {
        return false;
    }
    let stem = path.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(path);
    stem.eq_ignore_ascii_case("readme") || stem.eq_ignore_ascii_case("index")
}

/// There was nothing to import. Said in one place because a source that is read on its own —
/// every repository is, with **Documentation from repositories** on — is not failing when the
/// repository simply holds no Markdown, and is recorded as empty rather than as broken.
pub const NO_PAGES: &str = "no Markdown pages were found";

/// A source an archive came from, and where its files can be read on the web, ending in `/`.
pub struct Origin {
    pub source: Uuid,
    pub web: Option<String>,
}

/// Stages an archive's pages, in navigation order, and queues the first batch.
pub async fn start(
    backend: &Backend,
    store: &Store<'_>,
    files: &BTreeMap<String, Vec<u8>>,
    space: Option<&str>,
    resource: Option<&str>,
    owners: &[String],
    origin: Option<Origin>,
) -> Result<Started, Refusal> {
    let (space, source, pages) =
        prepare(backend, store, files, space, resource, owners, origin).await?;
    stage(backend, store, space, source, &pages).await
}

/// The most pages written while the caller waits, which is one batch.
pub const AT_ONCE: usize = BATCH as usize;

/// A few pages, such as a plugin's own, written while the caller waits rather than queued: the
/// answer means they are there, and nothing is left running if the Knowledge Base restarts.
pub async fn write_now(
    backend: &Backend,
    store: &Store<'_>,
    files: &BTreeMap<String, Vec<u8>>,
    space: &str,
    origin: Origin,
) -> Result<(Uuid, usize), Refusal> {
    if files.len() > AT_ONCE {
        return Err(Refusal::bad(format!("write at most {AT_ONCE} pages at a time")));
    }
    let (space, source, pages) =
        prepare(backend, store, files, Some(space), None, &[], Some(origin)).await?;
    let by =
        backend.caller().and_then(|caller| caller.label.clone()).unwrap_or_else(|| "kb".into());
    let import = store.stage(&space.key, source, &by, &pages).await?;
    batch(backend, store, import, 0).await?;
    Ok((import, pages.len()))
}

/// What an import is made of: the space, the source its pages come from, and the pages in order,
/// with the files they link to kept as attachments.
async fn prepare(
    backend: &Backend,
    store: &Store<'_>,
    files: &BTreeMap<String, Vec<u8>>,
    space: Option<&str>,
    resource: Option<&str>,
    owners: &[String],
    origin: Option<Origin>,
) -> Result<(Space, Uuid, Vec<Page>), Refusal> {
    let site = ["mkdocs.yml", "mkdocs.yaml"]
        .iter()
        .find_map(|name| files.get(*name))
        .map(|yaml| mkdocs::read(&String::from_utf8_lossy(yaml)))
        .unwrap_or_else(|| Site { docs_dir: String::new(), ..Site::default() });
    let web = origin.as_ref().and_then(|origin| origin.web.as_deref());
    let folder = match site.docs_dir.trim_matches('/') {
        "" => String::new(),
        docs => format!("{docs}/"),
    };
    let mut attached: Vec<Attached> = Vec::new();
    let mut pages: Vec<Page> = mkdocs::pages(&site, files)
        .into_iter()
        .map(|(path, title, content)| {
            let mut extra = match web {
                Some(web) => json!({ "url": format!("{web}{folder}{path}") }),
                None => json!({}),
            };
            let files_of = attachable(files, &format!("{folder}{path}"), &path, &content);
            if !files_of.is_empty() {
                extra["attachments"] = json!(
                    files_of
                        .iter()
                        .map(|file| (file.url.clone(), file.name.clone()))
                        .collect::<BTreeMap<_, _>>()
                );
            }
            attached.extend(files_of);
            Page { extra, path, title, content, format: "markdown" }
        })
        .collect();
    attached.truncate(MAX_ATTACHMENTS);
    if pages.is_empty() {
        return Err(Refusal::bad(NO_PAGES));
    }
    // A repository's README is its front page, and a project with no navigation of its own is
    // otherwise ordered by path, which would bury it under whatever sorts before it.
    if site.nav.is_empty()
        && let Some(front) = pages.iter().position(|page| front_page(&page.path))
    {
        let page = pages.remove(front);
        pages.insert(0, page);
    }
    let key = match space.map(slug).filter(|key| !key.is_empty()) {
        Some(key) => key,
        None => site
            .name
            .as_deref()
            .map(slug)
            .filter(|key| !key.is_empty())
            .unwrap_or_else(|| "docs".into()),
    };
    let resource = resource.map(checked_resource).transpose()?;
    let owners = crate::owners::checked(owners)?;
    let existing = store.space(&key).await?;
    // A space that is being made here has to say who keeps it; one that already exists keeps
    // whoever it named, so a sync does not have to answer the question again.
    if owners.is_empty() && existing.is_none() {
        return Err(Refusal::bad(crate::owners::NEEDED));
    }
    // A site that names itself names its space; otherwise a space keeps the name it has, such as
    // the `owner/name` the repository sync gave it, and a new one is called by its key.
    let name = site
        .name
        .clone()
        .or_else(|| existing.map(|space| space.name))
        .unwrap_or_else(|| key.clone());
    let mut space = store.ensure_space(&key, &name, resource.as_deref(), &[]).await?;
    // Set rather than written with the space, so that naming somebody new re-announces the pages
    // the space already holds; the ones this import brings in pick the owners up from `space`.
    if !owners.is_empty() && owners != space.owners {
        crate::owners::set(backend, store, &space, &owners).await?;
        space.owners = owners;
    }
    let source = match origin {
        Some(origin) => origin.source,
        None => store.upload_source(&key).await?,
    };
    for file in &attached {
        store.attach_file(&key, &file.page, &file.name, file.content_type, &file.bytes).await?;
    }
    Ok((space, source, pages))
}

/// The most files kept from one import, and the largest one.
const MAX_ATTACHMENTS: usize = 200;
const MAX_ATTACHMENT: usize = 5 * 1024 * 1024;
/// What a page may link to and keep: images it shows, and documents and data it links to. SVG is
/// kept but only ever downloaded, since one served from DOC could run script.
const ATTACHABLE: [(&str, &str); 12] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("svg", "image/svg+xml"),
    ("pdf", "application/pdf"),
    ("txt", "text/plain"),
    ("csv", "text/csv"),
    ("json", "application/json"),
    ("yaml", "application/yaml"),
    ("yml", "application/yaml"),
];

/// A file in the archive a page links to or shows, kept with the page.
struct Attached {
    page: String,
    /// The link as the page writes it, which is pointed at the kept file.
    url: String,
    name: String,
    content_type: &'static str,
    bytes: Vec<u8>,
}

/// The files page `path` — at `at` in the archive — links to or shows, where the archive has them
/// and they are of a kind kept, each named by its file name, made unique within the page.
fn attachable(
    files: &BTreeMap<String, Vec<u8>>,
    at: &str,
    path: &str,
    content: &str,
) -> Vec<Attached> {
    let mut found: Vec<Attached> = Vec::new();
    for url in markdown::targets(content) {
        if found.iter().any(|file| file.url == url) {
            continue;
        }
        let Some(target) = markdown::relative(at, &url) else { continue };
        let Some(bytes) = files.get(&target) else { continue };
        let extension =
            target.rsplit_once('.').map(|(_, ext)| ext.to_ascii_lowercase()).unwrap_or_default();
        let Some((_, content_type)) = ATTACHABLE.iter().find(|(ext, _)| *ext == extension) else {
            continue;
        };
        if bytes.len() > MAX_ATTACHMENT {
            continue;
        }
        let file = target.rsplit('/').next().unwrap_or(&target).to_string();
        let name = match found.iter().any(|kept| kept.name == file) {
            true => format!("{}-{file}", found.len()),
            false => file,
        };
        found.push(Attached {
            page: path.to_string(),
            url,
            name,
            content_type,
            bytes: bytes.clone(),
        });
    }
    found
}

/// Where a page's link to one of its kept files now leads.
pub fn attachment_link(space: &str, page: &str, url: &str, extra: &Value) -> Option<String> {
    let name = extra["attachments"][url].as_str()?;
    Some(format!(
        "/p/kb/attachments/{space}/{}/{}",
        crate::storage::percent(page),
        crate::storage::percent(name)
    ))
}

/// Stages pages for a space's source and queues the first batch.
pub async fn stage(
    backend: &Backend,
    store: &Store<'_>,
    space: Space,
    source: Uuid,
    pages: &[Page],
) -> Result<Started, Refusal> {
    let by =
        backend.caller().and_then(|caller| caller.label.clone()).unwrap_or_else(|| "kb".into());
    let import = store.stage(&space.key, source, &by, pages).await?;
    let task = backend.task(json!({ "import": import, "from": 0 })).await?;
    Ok(Started { import, space, pages: pages.len(), task })
}

/// A label that names a resource, such as `service-card-gateway`, as `service:card-gateway`.
pub fn labelled_resource(label: &str) -> Option<String> {
    // Organisations were called verticals, and older labels still say so.
    [
        ("service-account", "service-account"),
        ("cloud-resource", "cloud-resource"),
        ("organisation", "organisation"),
        ("vertical", "organisation"),
        ("service", "service"),
        ("repository", "repository"),
        ("team", "team"),
    ]
    .iter()
    .find_map(|(written, kind)| {
        label
            .strip_prefix(written)?
            .strip_prefix('-')
            .filter(|name| !name.is_empty())
            .map(|name| format!("{kind}:{name}"))
    })
}

/// What a page renders to: its title, safe HTML, words, front matter, tags and resources.
pub struct Rendered {
    pub title: String,
    pub html: String,
    pub text: String,
    pub front: serde_json::Map<String, Value>,
    pub tags: Vec<String>,
    pub resources: Vec<String>,
}

fn rendered(page: &crate::store::Staged, space: &Space) -> Rendered {
    render(space, &page.path, &page.format, &page.content, page.title.clone(), &page.extra)
}

/// A page as it is shown: Markdown, Confluence storage format or HTML, with what its source knows
/// of it in `extra` (its `labels`, and the `attachments` its links point at).
pub fn render(
    space: &Space,
    path: &str,
    format: &str,
    content: &str,
    title: Option<String>,
    extra: &Value,
) -> Rendered {
    let stem = path.rsplit('/').next().unwrap_or(path).trim_end_matches(".md").to_string();
    let tags: Vec<String> = extra["labels"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect();
    match format {
        "confluence" => {
            let html = crate::storage::convert(content, &space.key, path, &slug);
            let mut front = serde_json::Map::new();
            front.insert(
                "resources".into(),
                json!(tags.iter().filter_map(|tag| labelled_resource(tag)).collect::<Vec<_>>()),
            );
            Rendered {
                title: title.unwrap_or(stem),
                text: crate::storage::words(&html),
                html,
                resources: resources_of(&front, space),
                front: serde_json::Map::new(),
                tags,
            }
        }
        "html" => {
            let html = ammonia::clean(content);
            let mut front = serde_json::Map::new();
            front.insert("resources".into(), extra["resources"].clone());
            Rendered {
                title: title.unwrap_or(stem),
                text: crate::storage::words(&html),
                html,
                resources: resources_of(&front, space),
                front: serde_json::Map::new(),
                tags,
            }
        }
        _ => {
            let (front, body) = front_matter(content);
            let resources = resources_of(&front, space);
            let markdown = markdown::render(body, &|url| {
                attachment_link(&space.key, path, url, extra)
                    .or_else(|| markdown::page_link(&space.key, path, url))
            });
            let title = title
                .or_else(|| front.get("title").and_then(Value::as_str).map(str::to_string))
                .or(markdown.title)
                .unwrap_or(stem);
            Rendered { title, html: markdown.html, text: markdown.text, front, tags, resources }
        }
    }
}

fn hashed(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.as_bytes());
        hash.update([0]);
    }
    hex::encode(hash.finalize())
}

/// A document's address: its path without `.md`.
pub fn page_url(space: &str, path: &str) -> String {
    format!("/p/kb/docs/{space}/{}", path.strip_suffix(".md").unwrap_or(path))
}

pub fn resources_of(front: &serde_json::Map<String, Value>, space: &Space) -> Vec<String> {
    let listed = match &front.get("resources") {
        Some(Value::Array(items)) => {
            items.iter().filter_map(Value::as_str).map(str::to_string).collect()
        }
        Some(Value::String(one)) => vec![one.clone()],
        _ => Vec::new(),
    };
    // What the page documents, and who keeps the space it is in: the catalogue is told both, and
    // connects the page to each of them (DOC-SPEC §9.2).
    let named: BTreeSet<String> = listed
        .into_iter()
        .chain(space.resource.clone())
        .chain(space.owners.iter().cloned())
        .filter_map(|resource| checked_resource(&resource).ok())
        .collect();
    named.into_iter().collect()
}

/// One run of an import: the batch from `from`, or from wherever a run cut short had got to.
pub async fn batch(
    backend: &Backend,
    store: &Store<'_>,
    import: Uuid,
    from: i32,
) -> Result<Value, PluginError> {
    let place = format!("import/{import}");
    let saved = backend.state_get(&place).await?.and_then(|value| value["from"].as_i64());
    let from = from.max(saved.and_then(|at| i32::try_from(at).ok()).unwrap_or(0));
    let Some(record) = store.import(import).await? else {
        return Ok(json!({ "import": import, "gone": true }));
    };
    let space = store
        .space(&record.space)
        .await?
        .ok_or_else(|| PluginError::from("the import's space is gone"))?;
    let staged = store.staged(import, from, BATCH).await?;
    let paths: Vec<String> = staged.iter().map(|page| page.path.clone()).collect();
    let known: BTreeMap<String, String> = store
        .hashes(&space.key, &paths)
        .await?
        .into_iter()
        .map(|existing| (existing.path, existing.content_hash))
        .collect();
    // Which source this import came from, named as the catalogue names it, so every page can be
    // connected to it there. A source that has since gone leaves the pages unconnected.
    let source = match record.source {
        Some(source) => crate::sources::get(store, source)
            .await?
            .as_ref()
            .map(crate::sources::resource_name)
            .unwrap_or_default(),
        None => String::new(),
    };
    // A page read before its Markdown was kept is saved once more for it, quietly, since nothing
    // the catalogue knows of it has changed.
    let untexted = store.without_text(&space.key, &paths).await?;
    let (mut rows, mut announced) = (Vec::new(), Vec::new());
    for page in &staged {
        if page.extra["unchanged"] == true {
            continue;
        }
        let done = rendered(page, &space);
        let listed = done.resources.join(",");
        let position = page.position.to_string();
        let version = page.extra["version"].to_string();
        let hash = hashed(&[&done.title, &position, &listed, &version, &page.content]);
        let markdown = page.format == "markdown";
        let unchanged = known.get(&page.path) == Some(&hash);
        if unchanged && !(markdown && untexted.contains(&page.path)) {
            continue;
        }
        let url = page_url(&space.key, &page.path);
        rows.push(json!({
            "path": page.path,
            "title": done.title,
            "position": page.position,
            "format": page.format,
            "html": done.html,
            "plain": done.text,
            "front_matter": done.front,
            "content_hash": hash,
            "resources": done.resources,
            "tags": done.tags,
            "source_url": page.extra["url"].as_str(),
            "source_version": match &page.extra["version"] {
                Value::Number(version) => Some(version.to_string()),
                Value::String(version) => Some(version.clone()),
                _ => None,
            },
            "text": markdown.then_some(&page.content),
        }));
        if unchanged {
            continue;
        }
        announced.push(json!({
            "space": space.key,
            "path": page.path,
            "title": done.title,
            "url": url,
            "resources": done.resources,
            "source": source,
        }));
    }
    // A page edited in DOC goes on showing its edit, and what its source now says is kept beside
    // the edit; unless the source has caught up with it, when the edit has nothing left to keep.
    let saving: Vec<String> =
        rows.iter().filter_map(|row| row["path"].as_str().map(str::to_string)).collect();
    let mut edits = store.edits_in(&space.key, &saving).await?;
    let (mut diverted, mut retired) = (Vec::new(), Vec::new());
    rows.retain(|row| {
        let Some(edit) = edits.remove(row["path"].as_str().unwrap_or_default()) else {
            return true;
        };
        if crate::edits::caught_up(row["text"].as_str(), &edit.markdown) {
            retired.push(edit.id);
            return true;
        }
        diverted.push((edit, row.clone()));
        false
    });
    announced.retain(|document| !diverted.iter().any(|(edit, _)| document["path"] == *edit.path));
    let changed = announced.len() + diverted.len();
    store.save_pages(import, &space.key, record.source, &rows, staged.len(), changed).await?;
    store.divert(&space.key, &diverted).await?;
    store.retire_edits(&retired).await?;
    for document in announced {
        backend.publish(IMPORTED, document).await?;
    }
    let next = from + i32::try_from(staged.len()).unwrap_or(BATCH);
    backend.state_set(&place, json!({ "from": next })).await?;
    if next < record.total {
        backend.task(json!({ "import": import, "from": next })).await?;
    } else {
        finish(backend, store, &record).await?;
    }
    Ok(
        json!({ "import": import, "from": from, "pages": staged.len(), "changed": changed, "next": next }),
    )
}

/// Removes what the import no longer has, and clears away what it staged.
async fn finish(
    backend: &Backend,
    store: &Store<'_>,
    record: &crate::store::Import,
) -> Result<(), PluginError> {
    let gone = store.remove_missing(record).await?;
    for path in &gone {
        backend.publish(REMOVED, json!({ "space": record.space, "path": path })).await?;
    }
    store.finish(record, gone.len()).await?;
    backend.state_delete(&format!("import/{}", record.id)).await?;
    Ok(())
}
