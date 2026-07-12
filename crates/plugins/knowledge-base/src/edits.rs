//! Pages edited in DOC, copy-on-write: an edit is DOC's own copy of a page, shown, searched and
//! served in its place, while what the source has goes on being synced beside it. A source only
//! changes when a pull request proposing the edit is merged, for a page from a GitHub repository.

use std::collections::BTreeMap;

use doc_plugin_sdk::Backend;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::Refusal;
use crate::imports;
use crate::markdown::{self, front_matter};
use crate::sources;
use crate::store::{Edit, Store, original_of};

/// What proposing an edit to its repository asks of the person, besides writing to the GitHub
/// plugin, which core checks.
pub const PULL_REQUESTS: &str = "plugin:github:pluginuser:pull-requests";

/// What the edit page starts from.
pub struct Editing {
    /// The page's path as it is kept, with `.md` where it has one.
    pub path: String,
    pub title: String,
    /// The page as Markdown, without its front matter; empty when there is none to start from.
    pub markdown: String,
    /// The page as it is shown, to start from when there is no Markdown: one from Confluence, say.
    pub html: Option<String>,
    /// Where each image the Markdown shows is kept in DOC, by the address the page writes.
    pub images: BTreeMap<String, String>,
    pub origin: Origin,
    /// Whether a source brought the page in, and would bring it back if it were deleted.
    pub sourced: bool,
}

/// Where a page comes from, in words, and the repository a pull request would go to.
pub struct Origin {
    pub words: String,
    pub repository: Option<String>,
}

/// How a save went.
pub enum Saved {
    Saved,
    /// It said what the source says, so there is no edit any more.
    Same,
}

fn text(row: &Map<String, Value>, field: &str) -> String {
    row.get(field).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn hashed(markdown: &str) -> String {
    hex::encode(Sha256::digest(normalised(markdown).as_bytes()))
}

/// Markdown as it is compared and kept: Unix line ends, no blank lines either end, one newline.
fn normalised(markdown: &str) -> String {
    let unix = markdown.replace("\r\n", "\n");
    format!("{}\n", unix.trim_start_matches('\n').trim_end())
}

fn body_of(text: &str) -> &str {
    front_matter(text).1
}

/// What a page has above its body: its front matter between `---` lines, or nothing.
fn front_block(text: &str) -> &str {
    &text[..text.len() - body_of(text).len()]
}

/// Whether the source now says what the edit says: a merged pull request, or the same change
/// made in the repository.
pub fn caught_up(source: Option<&str>, edit: &str) -> bool {
    source.is_some_and(|source| normalised(body_of(source)) == normalised(edit))
}

/// The whole file an edit makes: the source's own front matter, as it is written, above the
/// edited body.
pub fn compose(source: Option<&str>, original: &Value, markdown: &str) -> String {
    let markdown = normalised(markdown);
    match source {
        Some(source) => {
            let body = body_of(source);
            let gap = &body[..body.len() - body.trim_start_matches(['\r', '\n']).len()];
            format!("{}{gap}{markdown}", front_block(source))
        }
        None => match original["front_matter"].as_object().filter(|front| !front.is_empty()) {
            Some(front) => format!(
                "---\n{}---\n\n{markdown}",
                serde_yaml_ng::to_string(front).unwrap_or_default()
            ),
            None => markdown,
        },
    }
}

fn writes(backend: &Backend) -> Result<(), Refusal> {
    match backend.writes() {
        true => Ok(()),
        false => Err(Refusal::forbidden("editing a page needs plugin:kb:user:rw")),
    }
}

fn by(backend: &Backend) -> String {
    backend.caller().and_then(|caller| caller.label.clone()).unwrap_or_else(|| "someone".into())
}

/// A page that may be edited: there, in a space in use, and not faux.
async fn editable(
    backend: &Backend,
    store: &Store<'_>,
    space: &str,
    path: &str,
) -> Result<Map<String, Value>, Refusal> {
    if crate::faux::corpus(backend).await?.is_some() {
        return Err(Refusal::bad(
            "these are faux pages, shown while there is test data, and cannot be edited",
        ));
    }
    if store.space(space).await?.is_none_or(|space| space.archived_at.is_some()) {
        return Err(Refusal::missing(format!("{space} is archived or gone")));
    }
    store
        .document_row(space, path)
        .await?
        .ok_or_else(|| Refusal::missing(format!("{space} has no page {path}")))
}

async fn source_of(store: &Store<'_>, row: &Map<String, Value>) -> Result<Option<Value>, Refusal> {
    match text(row, "source").parse::<Uuid>() {
        Ok(id) => sources::get(store, id).await,
        Err(_) => Ok(None),
    }
}

pub async fn origin(store: &Store<'_>, row: &Map<String, Value>) -> Result<Origin, Refusal> {
    let source = source_of(store, row).await?;
    let setting = |key: &str| {
        source.as_ref().and_then(|source| source["settings"][key].as_str()).map(str::to_string)
    };
    let kind = source.as_ref().and_then(|source| source["kind"].as_str()).unwrap_or("upload");
    Ok(match kind {
        "github" => {
            let repository = setting("repository").unwrap_or_default();
            Origin { words: format!("{repository} on GitHub"), repository: Some(repository) }
        }
        "confluence" => Origin { words: "Confluence".into(), repository: None },
        "drive" => Origin { words: "Google Drive".into(), repository: None },
        "git" => Origin {
            words: format!("the uploaded copy of {}", setting("repository").unwrap_or_default()),
            repository: None,
        },
        "plugin" => Origin {
            words: format!("the {} plugin", setting("plugin").unwrap_or_default()),
            repository: None,
        },
        _ => Origin { words: "its upload".into(), repository: None },
    })
}

/// Where a page from a GitHub source is in its repository, from the address it was synced from:
/// what follows `/<owner>/<name>/blob/<ref>/`.
fn file_in_repository(source: &Value, row: &Map<String, Value>) -> Option<String> {
    let repository = source["settings"]["repository"].as_str()?;
    let reference = source["settings"]["ref"].as_str().filter(|r| !r.is_empty()).unwrap_or("HEAD");
    let url = row.get("source_url").and_then(Value::as_str)?;
    let (_, file) = url.split_once(&format!("/{repository}/blob/{reference}/"))?;
    Some(file.to_string()).filter(|file| !file.is_empty())
}

/// A page's file as its repository has it now, read through the GitHub plugin, which hands the
/// Knowledge Base a repository's files as it hands it their archives.
async fn read_file(backend: &Backend, source: &Value, row: &Map<String, Value>) -> Option<String> {
    let file = file_in_repository(source, row)?;
    let asked = json!({
        "repository": source["settings"]["repository"], "ref": source["settings"]["ref"],
        "path": file,
    });
    match backend.discovery("github", "POST", "files", None, Some(asked)).await {
        Ok((200, answer)) => answer["content"].as_str().map(str::to_string),
        Ok((status, answer)) => {
            tracing::info!(status, detail = %answer["detail"], file, "a page's file was not read");
            None
        }
        Err(err) => {
            tracing::info!(%err, file, "the GitHub plugin could not be asked for a page's file");
            None
        }
    }
}

/// The files kept with a page that its Markdown points at, as an import's `extra` names them:
/// a kept file is named by its file name.
fn attachments_extra(names: &[String], page: &str, markdown: &str) -> Value {
    let mut found = Map::new();
    for url in markdown::targets(markdown) {
        let Some(target) = markdown::relative(page, &url) else { continue };
        let file = target.rsplit('/').next().unwrap_or(&target);
        if names.iter().any(|name| name == file) {
            found.insert(url, json!(file));
        }
    }
    json!({ "attachments": found })
}

/// What the edit page starts from: the edit, or the page's Markdown, or the page as shown.
pub async fn start(
    backend: &Backend,
    store: &Store<'_>,
    space: &str,
    path: &str,
) -> Result<Editing, Refusal> {
    writes(backend)?;
    let row = editable(backend, store, space, path).await?;
    let stored = text(&row, "path");
    let edit = store.edit(space, &stored).await?;
    let origin = origin(store, &row).await?;
    let mut kept = row.get("text").and_then(Value::as_str).map(str::to_string);
    // A page from a repository synced before its Markdown was kept is read from the repository
    // now, so that what is edited, and proposed, is the page as it is written: its own links.
    if edit.is_none()
        && kept.is_none()
        && text(&row, "format") == "markdown"
        && origin.repository.is_some()
        && let Some(source) = source_of(store, &row).await?
        && let Some(read) = read_file(backend, &source, &row).await
    {
        store.set_text(&text(&row, "id"), &read).await?;
        kept = Some(read);
    }
    let markdown = match (&edit, &kept) {
        (Some(edit), _) => edit.markdown.clone(),
        (None, Some(kept)) => normalised(body_of(kept)),
        (None, None) => String::new(),
    };
    let markdown = if markdown.trim().is_empty() { String::new() } else { markdown };
    let names = store.attachment_names(space, &stored).await?;
    let extra = attachments_extra(&names, &stored, &markdown);
    let images = extra["attachments"]
        .as_object()
        .into_iter()
        .flatten()
        .filter_map(|(url, _)| {
            Some((url.clone(), imports::attachment_link(space, &stored, url, &extra)?))
        })
        .collect();
    Ok(Editing {
        sourced: text(&row, "source").parse::<Uuid>().is_ok(),
        title: text(&row, "title"),
        html: markdown.is_empty().then(|| text(&row, "html")),
        path: stored,
        markdown,
        images,
        origin,
    })
}

/// Tells the catalogue what a page is called and documents now.
async fn announce(
    backend: &Backend,
    store: &Store<'_>,
    row: &Map<String, Value>,
    title: &str,
    resources: &Value,
) -> Result<(), Refusal> {
    let source = match source_of(store, row).await? {
        Some(source) => sources::resource_name(&source),
        None => String::new(),
    };
    let (space, path) = (text(row, "space"), text(row, "path"));
    let document = json!({
        "space": space, "path": path, "title": title, "url": imports::page_url(&space, &path),
        "resources": resources, "source": source,
    });
    backend.publish(imports::IMPORTED, document).await?;
    Ok(())
}

/// Saves an edit as DOC's own copy of the page, which is shown from now on; the source keeps its
/// own. Saving what the source already says takes the edit away instead.
pub async fn save(
    backend: &Backend,
    store: &Store<'_>,
    space: &str,
    path: &str,
    markdown: &str,
) -> Result<Saved, Refusal> {
    writes(backend)?;
    let row = editable(backend, store, space, path).await?;
    let (stored, id) = (text(&row, "path"), text(&row, "id"));
    let markdown = normalised(markdown);
    if markdown.trim().is_empty() {
        return Err(Refusal::bad("a page cannot be saved empty"));
    }
    let edit = store.edit(space, &stored).await?;
    let original = match &edit {
        Some(edit) => edit.original.clone(),
        None => original_of(&Value::Object(row.clone())),
    };
    let source = original["text"].as_str();
    if caught_up(source, &markdown) {
        if let Some(edit) = edit {
            store.discard_edit(&id, original.clone(), edit.id).await?;
            announce(
                backend,
                store,
                &row,
                original["title"].as_str().unwrap_or_default(),
                &original["resources"],
            )
            .await?;
        }
        return Ok(Saved::Same);
    }
    let content = compose(source, &original, &markdown);
    let space_record =
        store.space(space).await?.ok_or_else(|| Refusal::missing(format!("{space} is gone")))?;
    let names = store.attachment_names(space, &stored).await?;
    let extra = attachments_extra(&names, &stored, &markdown);
    // Named by its front matter, its own first heading, or else what the source called it.
    let title = front_matter(&content)
        .0
        .get("title")
        .and_then(Value::as_str)
        .map(str::to_string)
        .or_else(|| markdown::render(&markdown, &|_| None).title)
        .or_else(|| original["title"].as_str().map(str::to_string));
    let done = imports::render(&space_record, &stored, "markdown", &content, title, &extra);
    let shown = json!({
        "title": done.title, "html": done.html, "plain": done.text, "format": "markdown",
        "text": content,
    });
    let values = json!({
        "space": space, "path": stored, "markdown": markdown, "by": by(backend),
        "original": original,
        "base_hash": edit.as_ref().map_or_else(|| text(&row, "content_hash"), |edit| edit.base_hash.clone()),
        "source_gone": edit.as_ref().is_some_and(|edit| edit.source_gone),
    });
    store.save_edit(&id, shown, values).await?;
    announce(backend, store, &row, &done.title, row.get("resources").unwrap_or(&Value::Null))
        .await?;
    Ok(Saved::Saved)
}

/// Takes DOC's copy away and shows what the source has again; a page its source no longer has
/// goes altogether. Whether the page is still there afterwards.
pub async fn discard(
    backend: &Backend,
    store: &Store<'_>,
    space: &str,
    path: &str,
) -> Result<bool, Refusal> {
    writes(backend)?;
    let row = editable(backend, store, space, path).await?;
    let stored = text(&row, "path");
    let edit = store
        .edit(space, &stored)
        .await?
        .ok_or_else(|| Refusal::missing("this page has not been edited in DOC"))?;
    if edit.source_gone {
        store.delete_document(space, &stored).await?;
        backend.publish(imports::REMOVED, json!({ "space": space, "path": stored })).await?;
        return Ok(false);
    }
    store.discard_edit(&text(&row, "id"), edit.original.clone(), edit.id).await?;
    let title = edit.original["title"].as_str().unwrap_or_default();
    announce(backend, store, &row, title, &edit.original["resources"]).await?;
    Ok(true)
}

/// Proposes an edit to the repository its page comes from, as a pull request the GitHub plugin
/// opens, or brings the one already open up to date. The pull request's address.
pub async fn propose(
    backend: &Backend,
    store: &Store<'_>,
    space: &str,
    path: &str,
) -> Result<String, Refusal> {
    writes(backend)?;
    let row = editable(backend, store, space, path).await?;
    let stored = text(&row, "path");
    let edit: Edit = store
        .edit(space, &stored)
        .await?
        .ok_or_else(|| Refusal::bad("save a change to this page before proposing it"))?;
    let source =
        source_of(store, &row).await?.filter(|source| source["kind"] == "github").ok_or_else(
            || Refusal::bad("only a page from a GitHub repository can be proposed to it"),
        )?;
    let file = file_in_repository(&source, &row).ok_or_else(|| {
        Refusal::unavailable(
            "DOC does not know where this page is in its repository; sync its source and try again",
        )
    })?;
    let content = match row.get("text").and_then(Value::as_str) {
        Some(content) => content.to_string(),
        None => compose(edit.original["text"].as_str(), &edit.original, &edit.markdown),
    };
    let (title, by) = (text(&row, "title"), by(backend));
    let asked = json!({
        "repository": source["settings"]["repository"],
        "base": source["settings"]["ref"].as_str().unwrap_or_default(),
        "path": file,
        "content": content,
        "branch": edit.branch,
        "title": format!("Update {title}"),
        "message": format!("Update {file}\n\nEdited in DOC by {by}."),
        "body": format!(
            "{by} edited **{title}** (`{file}`) in DOC, which shows this version in place of the \
             repository's. Once this is merged and DOC next reads the repository, DOC stops keeping \
             its own copy."
        ),
    });
    let (status, answer) =
        backend.ask("github", "POST", "pull-requests", None, Some(asked)).await.map_err(|err| {
            Refusal::unavailable(format!("the GitHub plugin could not be asked: {}", err.detail()))
        })?;
    let detail = answer["detail"].as_str().unwrap_or_default().to_string();
    match status {
        200 | 201 => {
            let url = answer["url"].as_str().unwrap_or_default().to_string();
            let branch = answer["branch"].as_str().unwrap_or_default();
            store.proposed(edit.id, &url, branch, &by, &hashed(&edit.markdown)).await?;
            Ok(url)
        }
        403 => Err(Refusal::forbidden(format!(
            "the GitHub plugin refused: {detail}. Opening a pull request needs {PULL_REQUESTS} and \
             write access to the GitHub plugin"
        ))),
        400..=499 => {
            Err(Refusal { status, detail: format!("the GitHub plugin refused: {detail}") })
        }
        _ => Err(Refusal { status: 502, detail: format!("the GitHub plugin failed: {detail}") }),
    }
}

/// Whether an edit has changed since it was last proposed.
pub fn changed_since_proposed(edit: &Edit) -> bool {
    edit.proposed_hash.as_deref().is_some_and(|proposed| proposed != hashed(&edit.markdown))
}
