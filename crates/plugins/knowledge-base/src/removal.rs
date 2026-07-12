//! What an administrator takes out of the Knowledge Base, and how.
//!
//! **Nothing is deleted in one step.** A space, a source or a followed repository is *archived*
//! first: it is kept whole, and only put out of sight — out of the listings, out of search, out
//! of the catalogue, and syncing no more. Restoring it puts all of that back. Deleting for good
//! is offered only on something already archived, and is the step that cannot be undone. Both
//! steps are an administrator's, and both are audited.
//!
//! A page is the exception: one page is deleted outright, because a page whose source still has
//! it comes back at the next sync anyway. Taking a repository's pages out for good is archiving
//! the repository and then deleting its space.

use doc_plugin_sdk::Backend;
use serde_json::{Value, json};

use crate::Refusal;
use crate::imports::REMOVED;
use crate::sources;
use crate::store::Store;

/// The repositories an administrator has stopped following, which the repository sync passes over.
const FORGOTTEN: &str = "repositories/forgotten";
const SOURCES_REMOVED: &str = "plugin.kb.documentation-source.removed";

/// Only somebody who administers DOC deletes from the Knowledge Base.
pub fn admin(backend: &Backend) -> Result<(), Refusal> {
    administers(backend, "deletes from the Knowledge Base")
}

/// Only somebody who administers DOC does `what`, such as "links spaces to services".
pub fn administers(backend: &Backend, what: &str) -> Result<(), Refusal> {
    match backend.caller().is_some_and(|caller| caller.admin) {
        true => Ok(()),
        false => Err(Refusal::forbidden(format!("only somebody who administers DOC {what}"))),
    }
}

pub fn is_admin(backend: &Backend) -> bool {
    admin(backend).is_ok()
}

pub fn who(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "someone".into())
}

async fn audited(backend: &Backend, action: &str, subject: &str, detail: Value) {
    if let Err(err) = backend.audit(action, Some(subject), detail).await {
        tracing::warn!(%err, action, subject, "a deletion from the Knowledge Base was not audited");
    }
}

/// The repositories nobody wants followed, as `owner/name`.
pub async fn forgotten(backend: &Backend) -> Vec<String> {
    match backend.state_get(FORGOTTEN).await {
        Ok(Some(Value::Array(names))) => {
            names.iter().filter_map(Value::as_str).map(str::to_string).collect()
        }
        _ => Vec::new(),
    }
}

async fn keep_forgotten(backend: &Backend, names: &[String]) -> Result<(), Refusal> {
    Ok(backend.state_set(FORGOTTEN, json!(names)).await?)
}

async fn forget(backend: &Backend, repositories: &[String]) -> Result<(), Refusal> {
    let mut names = forgotten(backend).await;
    for repository in repositories {
        if !names.iter().any(|name| name.eq_ignore_ascii_case(repository)) {
            names.push(repository.clone());
        }
    }
    names.sort_by_key(|name| name.to_ascii_lowercase());
    keep_forgotten(backend, &names).await
}

/// Takes one page out.
pub async fn delete_page(
    backend: &Backend,
    store: &Store<'_>,
    space: &str,
    path: &str,
) -> Result<(), Refusal> {
    admin(backend)?;
    if !store.delete_document(space, path).await? {
        return Err(Refusal::missing("there is no such page"));
    }
    let gone = json!({ "space": space, "path": path });
    if let Err(err) = backend.publish(REMOVED, gone.clone()).await {
        tracing::warn!(%err, "the catalogue was not told of a deleted page");
    }
    let detail = json!({ "space": space, "path": path, "by": who(backend) });
    audited(backend, "document.deleted", &format!("{space}/{path}"), detail).await;
    Ok(())
}

/// Archives a space: everything in it is kept, and none of it is shown any more. Its sources stop
/// syncing with it, and the catalogue is told the pages and sources have gone, because an archived
/// space is not documentation anybody should be finding.
pub async fn archive_space(
    backend: &Backend,
    store: &Store<'_>,
    key: &str,
) -> Result<Deleted, Refusal> {
    admin(backend)?;
    let space =
        store.space(key).await?.ok_or_else(|| Refusal::missing("there is no such space"))?;
    if space.archived_at.is_some() {
        return Err(Refusal::bad("that space is archived already"));
    }
    if !store.set_space_archived(key, Some(&who(backend))).await? {
        return Err(Refusal::missing("there is no such space"));
    }
    let pages = store.pages_of(key).await?;
    for page in &pages {
        let path = page.get("path").and_then(Value::as_str).unwrap_or_default();
        if let Err(err) = backend.publish(REMOVED, json!({ "space": key, "path": path })).await {
            tracing::warn!(%err, "the catalogue was not told of an archived page");
            break;
        }
    }
    let gone: Vec<Value> = sources::list(store)
        .await?
        .iter()
        .filter(|source| source["space"].as_str() == Some(key))
        .map(|source| json!({ "kind": "DocumentationSource", "name": sources::resource_name(source) }))
        .collect();
    if !gone.is_empty()
        && let Err(err) = backend.publish(SOURCES_REMOVED, json!({ "documents": gone })).await
    {
        tracing::warn!(%err, "the catalogue was not told of archived sources");
    }
    let detail =
        json!({ "space": key, "name": space.name, "pages": pages.len(), "by": who(backend) });
    audited(backend, "space.archived", key, detail).await;
    Ok(Deleted { name: space.name, pages: pages.len(), repositories: Vec::new() })
}

/// Brings an archived space back, with its pages and its sources, and tells the catalogue.
pub async fn restore_space(
    backend: &Backend,
    store: &Store<'_>,
    key: &str,
) -> Result<Deleted, Refusal> {
    admin(backend)?;
    let space =
        store.space(key).await?.ok_or_else(|| Refusal::missing("there is no such space"))?;
    if space.archived_at.is_none() {
        return Err(Refusal::bad("that space is not archived"));
    }
    store.set_space_archived(key, None).await?;
    let pages = store.pages_of(key).await?;
    for page in &pages {
        let document = json!({
            "space": key,
            "path": page.get("path"),
            "title": page.get("title"),
            "url": crate::imports::page_url(key, page.get("path").and_then(Value::as_str).unwrap_or_default()),
            "resources": page.get("resources"),
            "source": page.get("source"),
        });
        if let Err(err) = backend.publish(crate::imports::IMPORTED, document).await {
            tracing::warn!(%err, "the catalogue was not told of a restored page");
            break;
        }
    }
    sources::announce(backend, store).await;
    let detail =
        json!({ "space": key, "name": space.name, "pages": pages.len(), "by": who(backend) });
    audited(backend, "space.restored", key, detail).await;
    Ok(Deleted { name: space.name, pages: pages.len(), repositories: Vec::new() })
}

/// Archives one source: it syncs no more, and the catalogue is told it has gone. The pages it
/// brought stay where they are — they are the space's, and the space says whether they are shown.
pub async fn archive_source(
    backend: &Backend,
    store: &Store<'_>,
    id: uuid::Uuid,
) -> Result<String, Refusal> {
    admin(backend)?;
    let source =
        store.any_source(id).await?.ok_or_else(|| Refusal::missing("there is no such source"))?;
    if !source["archived_at"].is_null() {
        return Err(Refusal::bad("that source is archived already"));
    }
    store.set_source_archived(id, Some(&who(backend))).await?;
    let gone = json!({ "kind": "DocumentationSource", "name": sources::resource_name(&source) });
    if let Err(err) = backend.publish(SOURCES_REMOVED, json!({ "documents": [gone] })).await {
        tracing::warn!(%err, "the catalogue was not told of an archived source");
    }
    let space = source["space"].as_str().unwrap_or_default().to_string();
    let detail = json!({ "source": id, "space": space, "by": who(backend) });
    audited(backend, "source.archived", &id.to_string(), detail).await;
    Ok(space)
}

/// Brings an archived source back, so it syncs again and the catalogue knows it.
pub async fn restore_source(
    backend: &Backend,
    store: &Store<'_>,
    id: uuid::Uuid,
) -> Result<String, Refusal> {
    admin(backend)?;
    let source =
        store.any_source(id).await?.ok_or_else(|| Refusal::missing("there is no such source"))?;
    if source["archived_at"].is_null() {
        return Err(Refusal::bad("that source is not archived"));
    }
    store.set_source_archived(id, None).await?;
    sources::announce(backend, store).await;
    let space = source["space"].as_str().unwrap_or_default().to_string();
    let detail = json!({ "source": id, "space": space, "by": who(backend) });
    audited(backend, "source.restored", &id.to_string(), detail).await;
    Ok(space)
}

/// Deletes an archived source for good, with nothing else touched.
pub async fn delete_source(
    backend: &Backend,
    store: &Store<'_>,
    id: uuid::Uuid,
) -> Result<String, Refusal> {
    admin(backend)?;
    let source =
        store.any_source(id).await?.ok_or_else(|| Refusal::missing("there is no such source"))?;
    if source["archived_at"].is_null() {
        return Err(Refusal::bad("archive the source first: deleting one cannot be undone"));
    }
    store.delete_source(id).await?;
    let space = source["space"].as_str().unwrap_or_default().to_string();
    let detail = json!({ "source": id, "space": space, "by": who(backend) });
    audited(backend, "source.deleted", &id.to_string(), detail).await;
    Ok(space)
}

/// What a deletion took out.
pub struct Deleted {
    pub name: String,
    pub pages: usize,
    /// The repositories no longer followed because of it.
    pub repositories: Vec<String>,
}

/// Takes a space out with everything in it. A space the repository sync keeps is its
/// repository's, which is then no longer followed, or the space would be back at the next run.
pub async fn delete_space(
    backend: &Backend,
    store: &Store<'_>,
    key: &str,
) -> Result<Deleted, Refusal> {
    admin(backend)?;
    // Archiving comes first, always: deleting is the one step that cannot be undone, and nobody
    // should reach it without having already put the space out of sight and lived with that.
    let space =
        store.space(key).await?.ok_or_else(|| Refusal::missing("there is no such space"))?;
    if space.archived_at.is_none() {
        return Err(Refusal::bad(
            "archive the space first: deleting it and its pages cannot be undone",
        ));
    }
    let emptied =
        store.delete_space(key).await?.ok_or_else(|| Refusal::missing("there is no such space"))?;
    let repositories: Vec<String> = emptied
        .sources
        .iter()
        .filter(|source| source["managed"] == json!(true))
        .filter_map(|source| source["settings"]["repository"].as_str().map(str::to_string))
        .collect();
    if !repositories.is_empty() {
        forget(backend, &repositories).await?;
    }
    for path in &emptied.pages {
        if let Err(err) = backend.publish(REMOVED, json!({ "space": key, "path": path })).await {
            tracing::warn!(%err, "the catalogue was not told of a deleted page");
            break;
        }
    }
    let gone: Vec<Value> = emptied
        .sources
        .iter()
        .map(|source| {
            json!({ "kind": "DocumentationSource", "name": sources::resource_name(source) })
        })
        .collect();
    if !gone.is_empty()
        && let Err(err) = backend.publish(SOURCES_REMOVED, json!({ "documents": gone })).await
    {
        tracing::warn!(%err, "the catalogue was not told of deleted sources");
    }
    let detail = json!({
        "space": key,
        "name": emptied.space.name,
        "pages": emptied.pages.len(),
        "sources": emptied.sources.len(),
        "repositories": repositories,
        "by": who(backend),
    });
    audited(backend, "space.deleted", key, detail).await;
    Ok(Deleted { name: emptied.space.name, pages: emptied.pages.len(), repositories })
}

/// Stops following a repository: its space is **archived**, not deleted, and the repository sync
/// passes it over until somebody follows it again. Nothing it brought in is lost by this, so
/// stopping by mistake costs only the time to follow it again.
pub async fn forget_repository(
    backend: &Backend,
    store: &Store<'_>,
    repository: &str,
) -> Result<Deleted, Refusal> {
    admin(backend)?;
    let followed = sources::list(store).await?.into_iter().find(|source| {
        source["managed"] == json!(true)
            && source["settings"]["repository"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case(repository))
    });
    match followed.and_then(|source| source["space"].as_str().map(str::to_string)) {
        Some(space) => {
            forget(backend, &[repository.to_string()]).await?;
            let mut archived = archive_space(backend, store, &space).await?;
            archived.repositories = vec![repository.to_string()];
            Ok(archived)
        }
        None => {
            forget(backend, &[repository.to_string()]).await?;
            let detail = json!({ "repository": repository, "by": who(backend) });
            audited(backend, "repository.forgotten", repository, detail).await;
            Ok(Deleted {
                name: repository.to_string(),
                pages: 0,
                repositories: vec![repository.to_string()],
            })
        }
    }
}

/// Follows a repository again, from the repository sync's next run.
pub async fn follow_again(backend: &Backend, repository: &str) -> Result<bool, Refusal> {
    admin(backend)?;
    let mut names = forgotten(backend).await;
    let before = names.len();
    names.retain(|name| !name.eq_ignore_ascii_case(repository));
    if names.len() == before {
        return Ok(false);
    }
    keep_forgotten(backend, &names).await?;
    let detail = json!({ "repository": repository, "by": who(backend) });
    audited(backend, "repository.followed", repository, detail).await;
    let topic = "plugin.kb.repository.followed";
    if let Err(err) = backend.publish(topic, json!({ "repository": repository })).await {
        tracing::warn!(%err, "following a repository again was not announced");
    }
    Ok(true)
}
