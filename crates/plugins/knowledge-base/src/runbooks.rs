//! Runbooks: pages saying what to do when something goes wrong, which Agent Smith can be asked to
//! run (FEAT-AGENT). A page is one when its front matter says `runbook: true`, one of its labels is
//! `runbook`, or its file or a folder it is in is called `runbook` or `runbooks`; front matter saying
//! `runbook: false` keeps a page out whatever it is called. Nothing is stored for it: it is read from
//! what every page keeps already, so pages imported before there were runbooks count too.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::Refusal;
use crate::imports::page_url;
use crate::store::Store;

/// The environments a runbook is run against, as Agent Smith offers them.
pub const ENVIRONMENTS: [&str; 3] = ["development", "test", "production"];

/// The environment a runbook is for, if the page is one: what its front matter's `environment`
/// says, and production when it says nothing, since that is where most runbooks are needed.
pub fn environment_of(path: &str, front: &Value, tags: &Value) -> Option<&'static str> {
    let said = match &front["runbook"] {
        Value::Bool(said) => Some(*said),
        Value::String(said) => match said.trim().to_ascii_lowercase().as_str() {
            "true" | "yes" => Some(true),
            "false" | "no" => Some(false),
            _ => None,
        },
        _ => None,
    };
    let labelled = tags
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .any(|tag| tag.trim().eq_ignore_ascii_case("runbook"));
    let named = path
        .trim_end_matches(".md")
        .split('/')
        .any(|part| part.eq_ignore_ascii_case("runbook") || part.eq_ignore_ascii_case("runbooks"));
    match said.unwrap_or(labelled || named) {
        true => Some(environment(front["environment"].as_str().unwrap_or_default())),
        false => None,
    }
}

/// An environment as front matter or a form writes it, as one of [`ENVIRONMENTS`].
pub fn environment(written: &str) -> &'static str {
    match written.trim().to_ascii_lowercase().as_str() {
        "development" | "dev" => "development",
        "test" | "testing" | "staging" => "test",
        _ => "production",
    }
}

fn text(value: &Value, key: &str) -> String {
    value[key].as_str().unwrap_or_default().to_string()
}

/// Every runbook in the spaces in use, in the order the spaces and their pages are in.
pub async fn list(store: &Store<'_>) -> Result<Vec<Value>, Refusal> {
    let names: BTreeMap<String, String> =
        store.spaces().await?.into_iter().map(|space| (space.key, space.name)).collect();
    let mut found = Vec::new();
    for page in store.every_page().await? {
        let (space, path) = (text(&page, "space"), text(&page, "path"));
        // A page in an archived space is out of sight, so it is no runbook anyone can run.
        let Some(name) = names.get(&space) else { continue };
        let Some(environment) = environment_of(&path, &page["front_matter"], &page["tags"]) else {
            continue;
        };
        found.push(json!({
            "space": space, "space_name": name, "path": path, "title": page["title"],
            "href": page_url(&space, &path), "resources": page["resources"],
            "environment": environment,
        }));
    }
    Ok(found)
}

/// One runbook, with what it says as Markdown and a hash of that, which is the version Agent Smith
/// runs and what an approval pins.
pub async fn one(store: &Store<'_>, space: &str, path: &str) -> Result<Value, Refusal> {
    let shown = store.space(space).await?.filter(|found| found.archived_at.is_none());
    let shown = shown.ok_or_else(|| Refusal::missing(format!("there is no space {space}")))?;
    let page = match store.document(space, path).await? {
        Some(page) => Some(page),
        None => store.document(space, &format!("{path}.md")).await?,
    };
    let page = page.ok_or_else(|| Refusal::missing(format!("{space} has no page {path}")))?;
    let path = text(&page, "path");
    let environment = environment_of(&path, &page["front_matter"], &page["tags"])
        .ok_or_else(|| Refusal::missing(format!("{path} in {space} is not a runbook")))?;
    let title = text(&page, "title");
    let mut markdown = crate::text::markdown(&text(&page, "html"), "");
    if !markdown.starts_with("# ") {
        markdown = format!("# {title}\n\n{markdown}");
    }
    let hash = hex::encode(Sha256::digest(markdown.as_bytes()));
    Ok(json!({
        "space": space, "space_name": shown.name, "path": path, "title": title,
        "href": page_url(space, &path), "resources": page["resources"],
        "environment": environment, "markdown": markdown, "hash": hash,
    }))
}
