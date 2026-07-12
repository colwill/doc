//! Sources a space is filled from on its own: a GitHub repository fetched through an archive link
//! the GitHub plugin hands out, or a Confluence space read page by page. Credentials come from the
//! plugin's environment by name, and each source's own schedule is checked every minute.

use std::collections::BTreeMap;
use std::time::Duration;

use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::telemetry::sent;
use doc_plugin_sdk::{Backend, PluginError};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::store::Store;
use crate::{Refusal, archive, imports};

const DOWNLOAD_LIMIT: u64 = 64 * 1024 * 1024;

pub async fn list(store: &Store<'_>) -> Result<Vec<Value>, Refusal> {
    store.sources().await
}

pub async fn get(store: &Store<'_>, id: Uuid) -> Result<Option<Value>, Refusal> {
    Ok(list(store).await?.into_iter().find(|source| source["id"] == json!(id)))
}

/// What the catalogue is told about the sources, as it is told about the pages they bring in:
/// `plugin.kb.documentation-source.synced` with every source, so each is a resource of its own
/// kind that a page connects back to and that names the repository it reads.
const ANNOUNCED: &str = "plugin.kb.documentation-source.synced";
/// How many sources one of those events carries.
const ANNOUNCED_AT_A_TIME: usize = 200;

/// GitHub said "not now" rather than "no": without a token it allows 60 archive links an hour, so
/// a platform reading its repositories can meet the limit. A source that met it is left as it was
/// and read on the next run, rather than standing as one that failed.
const LATER: &str = "not now";

/// The characters a resource may be named with; anything else stands for itself as a dash.
fn nameable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() || "-_.:/@+".contains(c) { c } else { '-' })
        .collect()
}

/// Where a source points, in the words its own page uses: a repository and a ref, a Confluence
/// space on a site, a Drive folder. It names the source in the catalogue and titles it there.
pub fn describe(source: &Value) -> String {
    let setting = |key: &str| source["settings"][key].as_str().unwrap_or_default().to_string();
    match source["kind"].as_str().unwrap_or_default() {
        "github" => match setting("path").as_str() {
            "" => format!("{} at {}", setting("repository"), setting("ref")),
            path => format!("{}/{path} at {}", setting("repository"), setting("ref")),
        },
        "confluence" => format!("{} on {}", setting("space_key"), setting("url")),
        "drive" => format!("folder {}", setting("folder")),
        "git" => match setting("commit").as_str() {
            "" => format!("{}, uploaded", setting("repository")),
            commit => format!(
                "{} at {}, uploaded",
                setting("repository"),
                &commit[..commit.len().min(12)]
            ),
        },
        "plugin" => format!("written by the {} plugin", setting("plugin")),
        _ => "imports through the CLI or the API".into(),
    }
}

/// A source's name in the catalogue: the space it fills, what kind of source it is, and where it
/// reads from, which together are one source and stay the same for as long as it does.
pub fn resource_name(source: &Value) -> String {
    let space = source["space"].as_str().unwrap_or_default();
    let kind = source["kind"].as_str().unwrap_or_default();
    let setting = |key: &str| source["settings"][key].as_str().unwrap_or_default().to_string();
    let where_from = match kind {
        "github" => match setting("path").as_str() {
            "" => setting("repository"),
            path => format!("{}/{path}", setting("repository")),
        },
        "confluence" => setting("space_key"),
        "drive" => setting("folder"),
        "git" => setting("repository"),
        "plugin" => setting("plugin"),
        _ => String::new(),
    };
    let named = match where_from.is_empty() {
        true => format!("{space}/{kind}"),
        false => format!("{space}/{kind}/{where_from}"),
    };
    nameable(&named)
}

/// A source as a catalogue document: what it is, where it reads from, and — for one that reads a
/// GitHub repository — that repository, so the catalogue shows what documents what.
fn source_document(source: &Value) -> Value {
    let kind = source["kind"].as_str().unwrap_or_default();
    let space = source["space"].as_str().unwrap_or_default();
    let label = match kind {
        "github" => "GitHub",
        "confluence" => "Confluence",
        "drive" => "Google Drive",
        "git" => "Git repository",
        "plugin" => "A plugin",
        _ => "Uploads",
    };
    // A GitHub source is connected to the repository it reads, which the GitHub plugin keeps in
    // the catalogue under the same `owner/name`. A connection to a repository nobody has synced
    // is left out rather than refused, so a deployment without GitHub is none the worse for it.
    let repositories = match kind {
        "github" => vec![source["settings"]["repository"].clone()],
        _ => Vec::new(),
    };
    json!({
        "kind": "DocumentationSource",
        "name": resource_name(source),
        "title": format!("{label}: {}", describe(source)),
        "description": format!("Fills the {space} space in the Knowledge Base."),
        "metadata": {
            "space": space,
            "source": kind,
            "where": describe(source),
            "schedule": source["schedule"],
            "settings": source["settings"],
        },
        "connections": { "Repositories": repositories },
    })
}

/// Tells the catalogue about every source there is. Sent when one is added or rescheduled, and
/// once at load so a Knowledge Base that was filled before any of this still says where its
/// pages came from.
pub async fn announce(backend: &Backend, store: &Store<'_>) {
    let sources = match list(store).await {
        Ok(sources) if sources.is_empty() => return,
        Ok(sources) => sources,
        Err(refusal) => {
            let detail = refusal.detail;
            tracing::warn!(detail, "the sources could not be read for the catalogue");
            return;
        }
    };
    // A platform following its repositories has a source for each of them, so they go in
    // batches rather than as one event the size of the whole Knowledge Base.
    let documents: Vec<Value> = sources.iter().map(source_document).collect();
    for batch in documents.chunks(ANNOUNCED_AT_A_TIME) {
        if let Err(err) = backend.publish(ANNOUNCED, json!({ "documents": batch })).await {
            tracing::warn!(%err, "the catalogue could not be told about the sources");
            return;
        }
    }
}

async fn download(url: &str) -> Result<Vec<u8>, String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .user_agent(concat!("doc-kb/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| err.to_string())?;
    let answer = client.get(url).send().await;
    sent("github", "download", &answer);
    let answer = answer.map_err(|err| format!("the archive could not be fetched: {err}"))?;
    if !answer.status().is_success() {
        return Err(format!("the archive link answered {}", answer.status()));
    }
    if answer.content_length().is_some_and(|length| length > DOWNLOAD_LIMIT) {
        return Err("the archive is too large".into());
    }
    let bytes =
        answer.bytes().await.map_err(|err| format!("the archive could not be read: {err}"))?;
    Ok(bytes.to_vec())
}

/// Only what lies under `path` in the repository, as if it were the whole of it.
fn under(files: BTreeMap<String, Vec<u8>>, path: &str) -> BTreeMap<String, Vec<u8>> {
    let prefix = path.trim_matches('/');
    if prefix.is_empty() {
        return files;
    }
    let prefix = format!("{prefix}/");
    files
        .into_iter()
        .filter_map(|(name, bytes)| Some((name.strip_prefix(&prefix)?.to_string(), bytes)))
        .collect()
}

/// What core last told the plugin it is configured with (ADR-0007), kept where a source's fetch
/// can reach it without a `Backend`. Refreshed at every `load`, which the SDK also does after a
/// settings change.
static CONFIGURED: std::sync::RwLock<Option<std::sync::Arc<doc_plugin_sdk::Settings>>> =
    std::sync::RwLock::new(None);

pub fn remember(backend: &Backend) {
    if let Ok(mut held) = CONFIGURED.write() {
        *held = Some(backend.settings());
    }
}

/// A credential named in a source's settings. An administrator adds it by that name on the
/// plugin's **Settings** page; a deployment may still give it as `DOC_KB_CREDENTIAL_<NAME>`, or
/// the file `…_FILE` names, which is what is used when nothing is set in DOC.
pub fn credential(name: &str) -> Result<Secret<String>, String> {
    let held = CONFIGURED.read().ok().and_then(|held| held.clone());
    if let Some(secret) = held.and_then(|settings| settings.named(&name.to_ascii_lowercase())) {
        return Ok(secret);
    }
    let variable: String = format!("DOC_KB_CREDENTIAL_{name}")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
        .collect();
    let from_file = std::env::var(format!("{variable}_FILE")).ok().map(|path| {
        std::fs::read_to_string(&path)
            .map(|value| value.trim().to_string())
            .map_err(|err| format!("{path}: {err}"))
    });
    let read = match from_file {
        Some(read) => read,
        None => std::env::var(&variable).map(|value| value.trim().to_string()).map_err(|_| {
            format!(
                "no credential {name}: add one by that name on the Knowledge Base's \
                     Settings page, or set {variable} in the deployment"
            )
        }),
    };
    read.map(Secret::new)
}

/// One run of a source: fetch it and start its import, whose batches join this run's chain.
pub async fn sync(backend: &Backend, store: &Store<'_>, id: Uuid) -> Result<Value, PluginError> {
    // An archived source, or one in an archived space, brings nothing in: archiving is what
    // stops it, and a scheduled run that started before it was archived stops here.
    if let Some(archived) = store.any_source(id).await?
        && !archived["archived_at"].is_null()
    {
        return Err(PluginError::from("that source is archived, so it syncs no more"));
    }
    let source = get(store, id).await?.ok_or_else(|| PluginError::from("the source is gone"))?;
    let space = source["space"].as_str().unwrap_or_default();
    if store.space(space).await?.is_some_and(|space| space.archived_at.is_some()) {
        return Err(PluginError::from("that space is archived, so its sources sync no more"));
    }
    let result = match source["kind"].as_str() {
        Some("github") => github(backend, store, id, &source).await,
        Some("confluence") => confluence(backend, store, id, &source).await,
        Some("drive") => drive(backend, store, id, &source).await,
        Some("plugin") => Err(PluginError::from(
            "a plugin's pages are written by that plugin, which writes them again itself when \
             what they say changes",
        )),
        Some("git") => Err(PluginError::from(
            "a git repository uploaded as a zip is brought up to date by uploading it again",
        )),
        Some(kind) => Err(PluginError::from(format!("a {kind} source cannot be synced yet"))),
        None => Err(PluginError::from("the source names no kind")),
    };
    if let Err(err) = &result {
        let said = err.to_string();
        // A repository with no Markdown in it is not a source that failed: it is read, there was
        // nothing to take, and it is left alone until the repository is pushed to again.
        if said.contains(imports::NO_PAGES) {
            store.synced(id, "empty", None).await?;
            return Ok(json!({ "source": id, "pages": 0, "empty": true }));
        }
        // Nor is one GitHub would not hand a link to yet. Leaving when it was last read alone is
        // what brings it back on the next run.
        if said.starts_with(LATER) {
            let reason = said.trim_start_matches(LATER).trim_start_matches([':', ' ']).to_string();
            store.waiting(id, &reason).await?;
            return Ok(json!({ "source": id, "waiting": true }));
        }
        store.synced(id, "failed", Some(&said)).await?;
    }
    result
}

async fn github(
    backend: &Backend,
    store: &Store<'_>,
    id: Uuid,
    source: &Value,
) -> Result<Value, PluginError> {
    let settings = &source["settings"];
    let space = source["space"].as_str().unwrap_or_default().to_string();
    {
        let asked = json!({ "repository": settings["repository"], "ref": settings["ref"] });
        let (status, link) = backend
            .discovery("github", "POST", "archive-links", None, Some(asked))
            .await
            .map_err(|err| format!("the GitHub plugin could not be asked: {}", err.detail()))?;
        if status == 429 {
            return Err(PluginError::from(format!("{LATER}: {}", link["detail"])));
        }
        if status != 200 {
            return Err(PluginError::from(format!(
                "GitHub would not give an archive link: {}",
                link["detail"]
            )));
        }
        let url =
            link["url"].as_str().ok_or_else(|| PluginError::from("the archive link was empty"))?;
        let bytes = download(url).await.map_err(PluginError::from)?;
        let path = settings["path"].as_str().unwrap_or_default().trim_matches('/');
        let files = under(archive::files(&bytes).map_err(PluginError::from)?, path);
        let web = link["web"].as_str().map(|web| match path {
            "" => web.to_string(),
            path => format!("{web}{path}/"),
        });
        let origin = imports::Origin { source: id, web };
        let started =
            imports::start(backend, store, &files, Some(&space), None, &[], Some(origin)).await?;
        Ok(json!({ "source": id, "import": started.import, "pages": started.pages }))
    }
}

/// Every page in tree order; bodies, labels and attachments only for those whose version changed.
async fn confluence(
    backend: &Backend,
    store: &Store<'_>,
    id: Uuid,
    source: &Value,
) -> Result<Value, PluginError> {
    let settings = &source["settings"];
    let key = source["space"].as_str().unwrap_or_default();
    let flavour =
        crate::confluence::Flavour::named(settings["flavour"].as_str().unwrap_or("cloud"))
            .ok_or_else(|| PluginError::from("a Confluence source is cloud or datacenter"))?;
    let secret = credential(settings["credential"].as_str().unwrap_or_default())
        .map_err(PluginError::from)?;
    let site = settings["url"].as_str().unwrap_or_default();
    let client =
        crate::confluence::Confluence::new(site, flavour, secret).map_err(PluginError::from)?;
    let found = client
        .pages(settings["space_key"].as_str().unwrap_or_default())
        .await
        .map_err(PluginError::from)?;
    let known = store.versions(id).await?;
    let mut pages = Vec::new();
    let mut fetched = 0;
    for page in &found {
        let path = imports::slug(&page.title);
        let version = page.version.to_string();
        let url = client.page_url(page);
        if known.get(&path) == Some(&version) {
            let extra = json!({ "version": page.version, "url": url, "unchanged": true });
            pages.push(imports::Page {
                path,
                title: Some(page.title.clone()),
                content: String::new(),
                format: "confluence",
                extra,
            });
            continue;
        }
        let (body, labels) = client.body(page).await.map_err(PluginError::from)?;
        for attachment in client.attachments(page).await.map_err(PluginError::from)? {
            store.attach(key, &path, &attachment).await?;
        }
        fetched += 1;
        let extra = json!({ "version": page.version, "url": url, "labels": labels });
        pages.push(imports::Page {
            path,
            title: Some(page.title.clone()),
            content: body,
            format: "confluence",
            extra,
        });
    }
    let space =
        store.space(key).await?.ok_or_else(|| PluginError::from("the source's space is gone"))?;
    let started = imports::stage(backend, store, space, id, &pages).await?;
    Ok(
        json!({ "source": id, "import": started.import, "pages": started.pages, "fetched": fetched }),
    )
}

/// A Drive file's place in the space: its folders and name as a path, Markdown keeping `.md`.
fn drive_path(file: &crate::drive::File) -> String {
    let mut parts: Vec<String> = file.folders.iter().map(|folder| imports::slug(folder)).collect();
    let name = match file.is_markdown() {
        true => format!(
            "{}.md",
            imports::slug(file.name.trim_end_matches(".markdown").trim_end_matches(".md"))
        ),
        false => imports::slug(file.name.trim_end_matches(".txt")),
    };
    parts.push(name);
    parts.join("/")
}

/// The changes feed first: after a sync that succeeded, nothing changed means nothing to do.
async fn drive(
    backend: &Backend,
    store: &Store<'_>,
    id: Uuid,
    source: &Value,
) -> Result<Value, PluginError> {
    let settings = &source["settings"];
    let key = source["space"].as_str().unwrap_or_default();
    let secret = credential(settings["credential"].as_str().unwrap_or_default())
        .map_err(PluginError::from)?;
    let client = crate::drive::Drive::new(&secret).await.map_err(PluginError::from)?;
    let place = format!("drive/{id}");
    let saved = backend
        .state_get(&place)
        .await?
        .and_then(|value| value["token"].as_str().map(str::to_string));
    let settled = source["last_state"] == "succeeded";
    let token = match saved {
        Some(saved) => {
            let (changed, next) = client.changed_since(&saved).await.map_err(PluginError::from)?;
            if !changed && settled {
                backend.state_set(&place, json!({ "token": next })).await?;
                store.synced(id, "succeeded", None).await?;
                return Ok(json!({ "source": id, "changed": false }));
            }
            next
        }
        None => client.start_token().await.map_err(PluginError::from)?,
    };
    let files = client
        .files(settings["folder"].as_str().unwrap_or_default())
        .await
        .map_err(PluginError::from)?;
    let known = store.versions(id).await?;
    let mut pages = Vec::new();
    let mut fetched = 0;
    for file in &files {
        let path = drive_path(file);
        let extra = json!({ "version": file.version, "url": file.url });
        if known.get(&path) == Some(&file.version) {
            let mut extra = extra;
            extra["unchanged"] = json!(true);
            pages.push(imports::Page {
                path,
                title: Some(file.name.clone()),
                content: String::new(),
                format: "html",
                extra,
            });
            continue;
        }
        let content = client.content(file).await.map_err(PluginError::from)?;
        fetched += 1;
        let (content, format, title) = match (file.is_document(), file.is_markdown()) {
            (true, _) => (content, "html", Some(file.name.clone())),
            (false, true) => (content, "markdown", None),
            (false, false) => {
                let escaped =
                    content.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;");
                (
                    format!("<pre>{escaped}</pre>"),
                    "html",
                    Some(file.name.trim_end_matches(".txt").to_string()),
                )
            }
        };
        pages.push(imports::Page { path, title, content, format, extra });
    }
    let space =
        store.space(key).await?.ok_or_else(|| PluginError::from("the source's space is gone"))?;
    let started = imports::stage(backend, store, space, id, &pages).await?;
    backend.state_set(&place, json!({ "token": token })).await?;
    Ok(
        json!({ "source": id, "import": started.import, "pages": started.pages, "fetched": fetched }),
    )
}

/// Queues a sync of every source whose own schedule has come round since it last synced.
pub async fn due(backend: &Backend, store: &Store<'_>) -> Result<Value, PluginError> {
    let now = chrono::Utc::now();
    let mut started = Vec::new();
    for source in list(store).await? {
        let Some(schedule) = source["schedule"].as_str().filter(|schedule| !schedule.is_empty())
        else {
            continue;
        };
        let Ok(cron) = schedule.parse::<croner::Cron>() else { continue };
        let since =
            source["last_sync_at"].as_str().or(source["created_at"].as_str()).unwrap_or_default();
        let Ok(since) = chrono::DateTime::parse_from_rfc3339(since) else { continue };
        let next = cron.find_next_occurrence(&since.with_timezone(&chrono::Utc), false);
        if next.is_ok_and(|next| next <= now) {
            let id = source["id"].clone();
            backend.task(json!({ "source": id })).await?;
            started.push(id);
        }
    }
    Ok(json!({ "synced": started }))
}
