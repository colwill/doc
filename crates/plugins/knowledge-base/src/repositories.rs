//! Markdown that lives in a repository is documentation. Nobody should have to add a source for
//! each one, so with **Documentation from repositories** switched on the Knowledge Base keeps a
//! source of its own for every repository the platform knows, and reads the Markdown in it the
//! way it reads any other GitHub source: through an archive link, every `.md` file wherever it
//! sits, into a space bound to that repository. The pages are then Documentation in the catalogue,
//! connected to the repository they came from, and under the **Docs** tab on its page.
//!
//! Repositories come from the catalogue rather than from GitHub directly, so a repository kept
//! there by `github`, by `ghe` or by an `apply` all count the same. A repository is read again
//! only once it has been pushed to since it was last read, so a run costs nothing for the ones
//! nobody has touched.

use doc_plugin_sdk::{Backend, PluginError};
use serde_json::{Value, json};
use url::form_urlencoded::Serializer;
use uuid::Uuid;

use crate::Refusal;
use crate::imports::slug;
use crate::sources;
use crate::store::Store;

/// The feature this is all behind: off until somebody turns it on, because reading every
/// repository is a decision a platform makes rather than something that should start happening.
pub const FEATURE: &str = "repository-docs";

pub const SCHEDULE_KEY: &str = "repository-docs-schedule";
pub const EXCLUDE_KEY: &str = "repository-docs-exclude";
pub const PER_RUN_KEY: &str = "repository-docs-per-run";
pub const SCHEDULE: &str = "*/15 * * * *";

/// How many repositories are read in one run. The rest come round on the next one, so a platform
/// that turns this on with a thousand repositories works through them rather than fetching a
/// thousand archives at once.
///
/// Twelve, because GitHub hands out 60 archive links an hour to anyone asking without a token: at
/// the schedule's default of four runs an hour that is 48, with room to spare. A deployment whose
/// GitHub plugin has a token is limited far more generously and can raise it.
pub const PER_RUN: i64 = 12;
/// However high it is set, one run does not read more than this.
const MOST_PER_RUN: usize = 200;
/// How many repositories are taken on in one run. A platform that turns this on with thousands of
/// them takes them on over the following runs rather than writing thousands of rows in one.
const NEW_PER_RUN: usize = 100;
/// How many the catalogue is asked for at a time.
const PAGE: usize = 500;
/// A repository that says nothing about when it was last pushed to is read again this often.
const STALE_HOURS: i64 = 24;
/// How many names are tried for a repository's space before giving up on it.
const MAX_SPACE_KEYS: usize = 10;

/// A repository in the catalogue, as much of it as this needs.
struct Repository {
    name: String,
    branch: String,
    pushed_at: Option<String>,
    /// The team the catalogue says owns it, as `team:<name>`, if it says.
    owner: Option<String>,
}

/// `acme/*`, `*-archive` or a whole name. Only `*` means anything, and it stands for any run of
/// characters, so the patterns an administrator writes behave the way they look.
fn matches(pattern: &str, name: &str) -> bool {
    let (pattern, name) = (pattern.trim().to_ascii_lowercase(), name.to_ascii_lowercase());
    if pattern.is_empty() {
        return false;
    }
    let mut at = 0usize;
    let parts: Vec<&str> = pattern.split('*').collect();
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        let found = match index {
            0 => name.starts_with(part).then_some(0),
            _ => name[at..].find(part).map(|found| at + found),
        };
        let Some(found) = found else { return false };
        at = found + part.len();
    }
    match parts.last() {
        Some(last) if !last.is_empty() => at == name.len(),
        _ => true,
    }
}

/// The repositories the catalogue holds, leaving out the archived ones and anything an
/// administrator excluded.
async fn repositories(backend: &Backend, excluded: &[String]) -> Result<Vec<Repository>, String> {
    let mut found = Vec::new();
    let mut from = 0usize;
    loop {
        let asked = Serializer::new(String::new())
            .append_pair("kind", "Repository")
            .append_pair("limit", &PAGE.to_string())
            .append_pair("offset", &from.to_string())
            .finish();
        let answer = backend.ask("resources", "GET", "resources", Some(&asked), None).await;
        let listed = match answer {
            Ok((200, Value::Array(listed))) => listed,
            Ok((_, body)) => {
                let detail = body["detail"].as_str().unwrap_or("the catalogue refused");
                return Err(format!("the repositories could not be read: {detail}"));
            }
            Err(err) => return Err(format!("the catalogue could not be asked: {err}")),
        };
        let count = listed.len();
        for repository in listed {
            let name = repository["name"].as_str().unwrap_or_default().to_string();
            let metadata = &repository["metadata"];
            if name.is_empty() || metadata["archived"] == json!(true) {
                continue;
            }
            if excluded.iter().any(|pattern| matches(pattern, &name)) {
                continue;
            }
            found.push(Repository {
                // Whichever team the catalogue says owns it, which becomes who looks after the
                // space its documentation goes in. A repository with no owning team leaves the
                // space unowned rather than being attached to somebody nobody chose.
                owner: repository["owner"]
                    .as_str()
                    .filter(|owner| !owner.is_empty())
                    .map(|owner| format!("{}:{owner}", crate::owners::TEAM)),
                branch: metadata["default_branch"]
                    .as_str()
                    .filter(|branch| !branch.is_empty())
                    .unwrap_or("main")
                    .to_string(),
                pushed_at: metadata["pushed_at"].as_str().map(str::to_string),
                name,
            });
        }
        if count < PAGE {
            return Ok(found);
        }
        from += count;
    }
}

/// The space a repository's pages go into: its whole name, so two organisations with a repository
/// of the same name keep their own. A space that somebody made themselves is never taken over —
/// one already bound to this repository is used again, and anything else is stepped around.
async fn space_for(store: &Store<'_>, repository: &str, resource: &str) -> Result<String, Refusal> {
    let wanted = slug(repository);
    for attempt in 0..MAX_SPACE_KEYS {
        let key = match attempt {
            0 => wanted.clone(),
            1 => format!("{wanted}-repo"),
            _ => format!("{wanted}-repo-{attempt}"),
        };
        match store.space(&key).await? {
            None => return Ok(key),
            Some(space) if space.resource.as_deref() == Some(resource) => return Ok(key),
            Some(_) => continue,
        }
    }
    Err(Refusal::bad(format!("no space could be named for {repository}")))
}

/// Whether a repository has been pushed to since its source last read it. A repository that says
/// nothing about when it was pushed to — one synced by an older GitHub plugin — is read again a
/// day after it was last read, rather than never or every time.
fn worth_reading(repository: &Repository, source: Option<&Value>) -> bool {
    let Some(source) = source else { return true };
    let Some(last) = source["last_sync_at"].as_str() else { return true };
    let Ok(last) = chrono::DateTime::parse_from_rfc3339(last) else { return true };
    match repository
        .pushed_at
        .as_deref()
        .and_then(|at| chrono::DateTime::parse_from_rfc3339(at).ok())
    {
        Some(pushed) => pushed > last,
        None => chrono::Utc::now().signed_duration_since(last).num_hours() >= STALE_HOURS,
    }
}

/// One run: every repository gets a source, and the ones that have moved are read again.
pub async fn follow(backend: &Backend, store: &Store<'_>) -> Result<Value, PluginError> {
    let settings = backend.settings();
    if !settings.feature(FEATURE) {
        return Ok(json!({ "following": false }));
    }
    // The repositories an administrator stopped following are passed over like excluded ones.
    let mut excluded = settings.list(EXCLUDE_KEY);
    excluded.extend(crate::removal::forgotten(backend).await);
    let per_run = settings
        .integer(PER_RUN_KEY)
        .filter(|per_run| *per_run > 0)
        .map(|per_run| usize::try_from(per_run).unwrap_or(MOST_PER_RUN))
        .unwrap_or(PER_RUN as usize)
        .min(MOST_PER_RUN);
    let repositories = repositories(backend, &excluded).await.map_err(PluginError::from)?;
    let known = sources::list(store).await?;
    let (mut added, mut started) = (0usize, Vec::new());
    for repository in &repositories {
        let settings =
            json!({ "repository": repository.name, "ref": repository.branch, "path": "" });
        let held = known.iter().find(|source| {
            source["managed"] == json!(true)
                && source["settings"]["repository"] == json!(&repository.name)
        });
        let id = match held {
            Some(source) => {
                // The repository moved to another default branch: read that one from now on.
                if source["settings"]["ref"] != json!(&repository.branch) {
                    let id = source["id"].as_str().and_then(|id| id.parse::<Uuid>().ok());
                    if let Some(id) = id {
                        store.set_settings(id, &settings).await?;
                    }
                }
                source["id"].as_str().and_then(|id| id.parse::<Uuid>().ok())
            }
            None if added >= NEW_PER_RUN => continue,
            None => {
                // The space is bound to the repository, so every page imported into it is
                // connected to that repository in the catalogue without a word of front matter.
                let resource = format!("repository:{}", repository.name);
                let space = space_for(store, &repository.name, &resource).await?;
                let owners: Vec<String> = repository.owner.clone().into_iter().collect();
                store.ensure_space(&space, &repository.name, Some(&resource), &owners).await?;
                let id = Uuid::now_v7();
                store.add_managed_source(id, &space, &settings).await?;
                added += 1;
                Some(id)
            }
        };
        let Some(id) = id else { continue };
        if started.len() >= per_run || !worth_reading(repository, held) {
            continue;
        }
        backend.task(json!({ "source": id })).await?;
        started.push(json!(id));
    }
    if added > 0 {
        sources::announce(backend, store).await;
    }
    Ok(json!({
        "following": true,
        "repositories": repositories.len(),
        "added": added,
        "reading": started,
        // Said plainly, because a first run on a large platform is meant to be one of several.
        "more_to_take_on": added >= NEW_PER_RUN,
    }))
}
