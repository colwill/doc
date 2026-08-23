//! Staging: what an LLM hands in during a run, checked the same way whether Claude sent it
//! through a tool or an administrator's own agent through the API. Nothing staged leaves the plugin
//! until an administrator approves it, so the checks here are about shape, not about trust.

use serde::Deserialize;
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::Refusal;
use crate::store::{CATALOGUE, Item, KB, Run, SOURCES, STAGED, Store, WATER};

/// However much an LLM hands in, one run is reviewed by a person, so it stays within reach.
pub const MAX_ITEMS: usize = 5_000;
const MAX_PAGE: usize = 512 * 1024;
const MAX_DISCUSSION: usize = 20_000;
const MAX_TITLE: usize = 200;
const MAX_TAGS: usize = 10;
/// The kinds of the Catalogue a run may write. People, roles, permissions and service accounts
/// come from identity providers and RBAC, never from another tool's say-so.
pub const KINDS: [&str; 4] = ["Service", "Repository", "Team", "CloudResource"];

#[derive(Debug, Deserialize)]
pub struct NewPage {
    pub space: String,
    #[serde(default)]
    pub space_title: Option<String>,
    pub path: String,
    pub title: String,
    pub markdown: String,
    pub source: String,
    #[serde(default)]
    pub source_url: String,
}

#[derive(Debug, Deserialize)]
pub struct NewResource {
    pub document: Map<String, Value>,
    pub source: String,
    #[serde(default)]
    pub source_url: String,
}

#[derive(Debug, Deserialize)]
pub struct NewDiscussion {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source: String,
    #[serde(default)]
    pub source_url: String,
}

/// Lower-case letters, digits and single dashes, as the Knowledge Base keys its spaces.
pub fn slug(text: &str) -> String {
    let mut slug = String::new();
    for c in text.trim().chars() {
        match c {
            c if c.is_ascii_alphanumeric() => slug.push(c.to_ascii_lowercase()),
            _ if !slug.ends_with('-') && !slug.is_empty() => slug.push('-'),
            _ => {}
        }
    }
    slug.trim_end_matches('-').chars().take(56).collect()
}

fn title(text: &str) -> Result<String, Refusal> {
    let text = text.trim();
    match !text.is_empty() && text.chars().count() <= MAX_TITLE && !text.contains(char::is_control)
    {
        true => Ok(text.to_string()),
        false => Err(Refusal::bad(format!("a title is 1 to {MAX_TITLE} characters on one line"))),
    }
}

fn source(run: &Run, text: &str) -> Result<String, Refusal> {
    let text = text.trim().to_lowercase();
    if !SOURCES.iter().any(|(name, _)| *name == text) {
        let names: Vec<&str> = SOURCES.iter().map(|(name, _)| *name).collect();
        return Err(Refusal::bad(format!("`source` is one of {}", names.join(", "))));
    }
    if !run.sources.contains(&text) {
        return Err(Refusal::bad(format!("this run takes nothing from {text}")));
    }
    Ok(text)
}

fn link(text: &str) -> Result<String, Refusal> {
    let text = text.trim();
    match text.is_empty() || text.starts_with("https://") || text.starts_with("http://") {
        true => Ok(text.chars().take(2_000).collect()),
        false => Err(Refusal::bad("`source_url` is where it came from, as an http or https URL")),
    }
}

/// A page's path inside its space: folders and a name, ending `.md`.
fn page_path(text: &str) -> Result<String, Refusal> {
    let mut path: String = text.trim().trim_start_matches('/').to_string();
    if !path.ends_with(".md") {
        path.push_str(".md");
    }
    let parts: Vec<&str> = path.split('/').collect();
    let fine = path.len() <= 300
        && parts.iter().all(|part| {
            !part.is_empty()
                && *part != "."
                && *part != ".."
                && part.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
        });
    match fine {
        true => Ok(path),
        false => Err(Refusal::bad(format!(
            "`{text}` is not a page's path: folders and a name of a-z, 0-9, `-`, `_` and `.`, \
             such as guides/on-call.md"
        ))),
    }
}

/// Where a page lands: a space of its source's, so nothing a person made in the Knowledge Base is
/// ever replaced by an import.
pub fn space_key(source: &str, space: &str) -> Result<String, Refusal> {
    let slug = slug(space);
    if slug.is_empty() {
        return Err(Refusal::bad("name the space the page goes in"));
    }
    Ok(match slug.starts_with(&format!("{source}-")) {
        true => slug,
        false => format!("{source}-{slug}"),
    })
}

fn blank(run: &Run, destination: &str, key: String, title: String) -> Item {
    Item {
        id: Uuid::now_v7(),
        run: run.id,
        destination: destination.to_string(),
        key,
        title,
        space: None,
        space_title: None,
        path: None,
        content: String::new(),
        tags: Vec::new(),
        source: String::new(),
        source_url: String::new(),
        state: STAGED.to_string(),
        error: None,
        applied_at: None,
        updated_at: None,
    }
}

pub fn page(run: &Run, asked: &NewPage) -> Result<Item, Refusal> {
    let source = source(run, &asked.source)?;
    let space = space_key(&source, &asked.space)?;
    let path = page_path(&asked.path)?;
    let title = title(&asked.title)?;
    if asked.markdown.trim().is_empty() || asked.markdown.len() > MAX_PAGE {
        return Err(Refusal::bad(format!(
            "a page is 1 byte to {} KiB of Markdown",
            MAX_PAGE / 1024
        )));
    }
    let source_url = link(&asked.source_url)?;
    // The title and where it came from travel with the page, unless it says so itself.
    let content = match asked.markdown.trim_start().starts_with("---") {
        true => asked.markdown.clone(),
        false => {
            let quoted = |text: &str| serde_json::to_string(text).unwrap_or_default();
            let mut front = format!("---\ntitle: {}\n", quoted(&title));
            if !source_url.is_empty() {
                front.push_str(&format!("source_url: {}\n", quoted(&source_url)));
            }
            front.push_str(&format!("imported_from: {source}\n---\n\n"));
            front + &asked.markdown
        }
    };
    let space_title = asked
        .space_title
        .as_deref()
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(|text| text.chars().take(MAX_TITLE).collect::<String>());
    let mut item = blank(run, KB, format!("{space}/{path}"), title);
    item.space = Some(space);
    item.space_title = space_title;
    item.path = Some(path);
    item.content = content;
    item.source = source;
    item.source_url = source_url;
    Ok(item)
}

pub fn resource(run: &Run, asked: NewResource) -> Result<Item, Refusal> {
    let source = source(run, &asked.source)?;
    let document = asked.document;
    let kind = document.get("kind").and_then(Value::as_str).unwrap_or_default().trim();
    let Some(kind) = KINDS.iter().find(|known| known.eq_ignore_ascii_case(kind)) else {
        return Err(Refusal::bad(format!(
            "a resource's `kind` is one of {}; people, roles and permissions come from elsewhere",
            KINDS.join(", ")
        )));
    };
    let name = document.get("name").and_then(Value::as_str).unwrap_or_default().trim();
    if name.is_empty() || name.len() > 200 {
        return Err(Refusal::bad("a resource's `name` is 1 to 200 characters"));
    }
    let allowed = [
        "kind",
        "name",
        "title",
        "description",
        "metadata",
        "owner",
        "organisation",
        "email",
        "connections",
    ];
    if let Some(unknown) = document.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(Refusal::bad(format!(
            "a resource has no `{unknown}`: it takes {}",
            allowed.join(", ")
        )));
    }
    let mut document = document.clone();
    document.insert("kind".into(), Value::String((*kind).to_string()));
    document.insert("name".into(), Value::String(name.to_string()));
    let content = serde_json::to_string_pretty(&document)
        .map_err(|err| Refusal::bad(format!("the document could not be written: {err}")))?;
    if content.len() > 64 * 1024 {
        return Err(Refusal::bad("a resource's document is at most 64 KiB"));
    }
    let shown = document.get("title").and_then(Value::as_str).unwrap_or(name);
    let mut item = blank(run, CATALOGUE, format!("{kind}:{name}"), title(shown)?);
    item.content = content;
    item.source = source;
    item.source_url = link(&asked.source_url)?;
    Ok(item)
}

pub fn discussion(run: &Run, asked: &NewDiscussion) -> Result<Item, Refusal> {
    let source = source(run, &asked.source)?;
    let title = title(&asked.title)?;
    if asked.body.trim().is_empty() || asked.body.chars().count() > MAX_DISCUSSION {
        return Err(Refusal::bad(format!("a discussion is 1 to {MAX_DISCUSSION} characters")));
    }
    if asked.tags.len() > MAX_TAGS {
        return Err(Refusal::bad(format!("a discussion has at most {MAX_TAGS} tags")));
    }
    let tags = asked
        .tags
        .iter()
        .map(|tag| tag.trim().to_string())
        .filter(|tag| !tag.is_empty())
        .map(|tag| match tag.split_once(':') {
            Some((_, name)) if !name.trim().is_empty() => Ok(tag.clone()),
            _ => Err(Refusal::bad(format!("tag `{tag}` as kind:name, such as service:payments"))),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut item = blank(run, WATER, title.clone(), title);
    item.content = asked.body.trim().to_string();
    item.tags = tags;
    item.source = source;
    item.source_url = link(&asked.source_url)?;
    Ok(item)
}

/// Stages a checked item in a run that is still taking them in, answering whether it was new.
pub async fn keep(store: &Store<'_>, run: &Run, item: &Item) -> Result<bool, Refusal> {
    if run.state != crate::store::COLLECTING {
        return Err(Refusal::conflict("this run takes nothing more: it has finished"));
    }
    if store.count(run.id).await? >= MAX_ITEMS {
        return Err(Refusal::conflict(format!(
            "a run holds at most {MAX_ITEMS} items; finish this one and start another"
        )));
    }
    store.stage(item).await
}
