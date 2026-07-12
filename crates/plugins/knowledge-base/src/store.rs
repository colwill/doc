//! What the Knowledge Base keeps: spaces, their sources and documents, full-text indexed, and the
//! files an import has staged for the batches still to come.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use doc_plugin_sdk::{
    Aggregate, Backend, Collection, DataRequest, Declaration, Field, ListOf, Measure, OnDelete,
    Order, Query,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use uuid::Uuid;

use crate::Refusal;

/// How many IDs one `in` condition carries.
const CHUNK: usize = 500;
const BATCH: usize = 100;
/// The words a search snippet shows around a match, and how many stretches of them at most.
const SNIPPET_WORDS: usize = 24;
const SNIPPET_FRAGMENTS: usize = 2;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "spaces",
            Collection::new()
                .field("key", Field::text().key())
                .field("name", Field::text().required())
                .field("resource", Field::text())
                .field(
                    "owners",
                    Field::list(ListOf::Text)
                        .required()
                        .default(json!([]))
                        .describe("The teams whose documentation this is, or the organisation"),
                )
                .field(
                    "archived_at",
                    Field::timestamp().describe("When it was put out of sight, if it has been"),
                )
                .field("archived_by", Field::text())
                .field(
                    "renamed_at",
                    Field::timestamp()
                        .describe("When an administrator named it, a name syncs then leave alone"),
                )
                .field("renamed_by", Field::text())
                .index(&["name"])
                // Ordered by when it was archived, so the index has to lead with it.
                .index(&["archived_at"]),
        )
        .collection(
            "sources",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "kind",
                    Field::text().required().one_of(&[
                        "upload",
                        "github",
                        "confluence",
                        "drive",
                        "git",
                        "plugin",
                    ]),
                )
                .field("space", Field::reference("spaces").required().on_delete(OnDelete::Cascade))
                .field("settings", Field::json().required().default(json!({})))
                .field("schedule", Field::text())
                .field(
                    "archived_at",
                    Field::timestamp().describe("When it was put out of sight, if it has been"),
                )
                .field("archived_by", Field::text())
                .field("last_sync_at", Field::timestamp())
                .field("last_state", Field::text())
                .field("last_error", Field::text())
                .field(
                    "managed",
                    Field::boolean()
                        .required()
                        .default(json!(false))
                        .describe("Kept in step with a repository by the repository sync"),
                )
                .field("upload_space", Field::text().describe("Set on a space's one upload source"))
                .unique(&["upload_space"])
                .index(&["space", "kind"]),
        )
        .collection(
            "documents",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("space", Field::reference("spaces").required().on_delete(OnDelete::Cascade))
                .field("path", Field::text().required())
                .field("title", Field::text().required())
                .field("position", Field::integer().required())
                .field("format", Field::text().required())
                .field("html", Field::text().required())
                .field("plain", Field::text().required())
                .field("front_matter", Field::json().required().default(json!({})))
                .field("content_hash", Field::text().required())
                .field("resources", Field::list(ListOf::Text).required().default(json!([])))
                .field("tags", Field::json().required().default(json!([])))
                .field("source", Field::reference("sources").on_delete(OnDelete::Null))
                .field("source_url", Field::text())
                .field("source_version", Field::text())
                .field(
                    "text",
                    Field::text().describe(
                        "The page as Markdown, front matter and all: what it is edited from",
                    ),
                )
                .unique(&["space", "path"])
                .index(&["space", "position", "path"])
                .search(&["title", "plain"]),
        )
        .collection(
            "edits",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("space", Field::reference("spaces").required().on_delete(OnDelete::Cascade))
                .field("path", Field::text().required())
                .field(
                    "markdown",
                    Field::text()
                        .required()
                        .describe("The page as it was edited in DOC, without its front matter"),
                )
                .field("by", Field::text().required())
                .field(
                    "original",
                    Field::json()
                        .required()
                        .describe("The page as its source has it now, which a discard puts back"),
                )
                .field(
                    "base_hash",
                    Field::text().required().describe("The source's version the edit began from"),
                )
                .field("source_gone", Field::boolean().required().default(json!(false)))
                .field(
                    "pull_request",
                    Field::text().describe("The pull request proposing the edit to its repository"),
                )
                .field("branch", Field::text())
                .field("proposed_at", Field::timestamp())
                .field("proposed_by", Field::text())
                .field(
                    "proposed_hash",
                    Field::text().describe("What the edit said when it was last proposed"),
                )
                .unique(&["space", "path"]),
        )
        .collection(
            "imports",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("space", Field::reference("spaces").required().on_delete(OnDelete::Cascade))
                .field("source", Field::reference("sources").on_delete(OnDelete::Null))
                .field("total", Field::integer().required())
                .field("done", Field::integer().required().default(json!(0)))
                .field("changed", Field::integer().required().default(json!(0)))
                .field("removed", Field::integer().required().default(json!(0)))
                .field("state", Field::text().required().default(json!("running")))
                .field("error", Field::text())
                .field("created_by", Field::text().required())
                .field("finished_at", Field::timestamp()),
        )
        .collection(
            "import-files",
            Collection::new()
                .field("id", Field::uuid().key())
                .field(
                    "import",
                    Field::reference("imports").required().on_delete(OnDelete::Cascade),
                )
                .field("position", Field::integer().required())
                .field("path", Field::text().required())
                .field("title", Field::text())
                .field("content", Field::text().required())
                .field("format", Field::text().required().default(json!("markdown")))
                .field("extra", Field::json().required().default(json!({})))
                .unique(&["import", "position"]),
        )
        .collection(
            "links",
            Collection::new()
                .field("id", Field::text().key().describe("<service>/<space>"))
                .field(
                    "service",
                    Field::text().required().describe("A service's name in the catalogue"),
                )
                .field("space", Field::reference("spaces").required().on_delete(OnDelete::Cascade))
                .field("by", Field::text().required())
                .unique(&["service", "space"])
                .index(&["service"]),
        )
        .collection(
            "attachments",
            Collection::new()
                .field("id", Field::uuid().key())
                .field("space", Field::reference("spaces").required().on_delete(OnDelete::Cascade))
                .field("page", Field::text().required())
                .field("name", Field::text().required())
                .field("content_type", Field::text().required())
                .field("bytes", Field::bytes().required())
                .unique(&["space", "page", "name"]),
        )
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Space {
    pub key: String,
    pub name: String,
    pub resource: Option<String>,
    /// When somebody archived it, and who. An archived space is kept whole and shown to nobody
    /// but an administrator, on the page where it is restored or deleted for good.
    #[serde(default)]
    pub archived_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub archived_by: Option<String>,
    /// Who keeps the space: `team:<name>` one or more times, or one `organisation:<name>`. Empty
    /// for a space made before a space had to say (see `owners`).
    #[serde(default)]
    pub owners: Vec<String>,
    #[serde(default)]
    pub documents: i64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Import {
    pub id: Uuid,
    pub space: String,
    pub source: Option<Uuid>,
    pub total: i32,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Staged {
    pub position: i32,
    pub path: String,
    pub title: Option<String>,
    pub content: String,
    pub format: String,
    pub extra: Value,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Existing {
    pub path: String,
    pub content_hash: String,
}

/// A page edited in DOC: its own copy, and the source's beside it.
#[derive(Debug, Clone, Deserialize)]
pub struct Edit {
    pub id: Uuid,
    pub path: String,
    pub markdown: String,
    pub by: String,
    /// The source's page as last synced: title, html, plain, format, front matter, resources,
    /// tags and text, as the documents collection keeps them.
    pub original: Value,
    pub base_hash: String,
    #[serde(default)]
    pub source_gone: bool,
    #[serde(default)]
    pub pull_request: Option<String>,
    #[serde(default)]
    pub branch: Option<String>,
    #[serde(default)]
    pub proposed_hash: Option<String>,
    #[serde(default, rename = "_updated_at")]
    pub updated_at: Option<DateTime<Utc>>,
}

/// What a page's source has of it, as an edit keeps it and a discard puts it back.
pub const ORIGINAL: [&str; 8] =
    ["title", "html", "plain", "format", "front_matter", "resources", "tags", "text"];

fn text(record: &Map<String, Value>, field: &str) -> String {
    record.get(field).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn read<T: serde::de::DeserializeOwned>(record: Map<String, Value>) -> Result<T, Refusal> {
    serde_json::from_value(Value::Object(record))
        .map_err(|err| Refusal::unavailable(format!("a stored record could not be read: {err}")))
}

fn now() -> String {
    Utc::now().to_rfc3339()
}

/// The fields of `ORIGINAL` a page record has: what a source said of a page.
pub fn original_of(row: &Value) -> Value {
    Value::Object(ORIGINAL.iter().map(|field| (field.to_string(), row[*field].clone())).collect())
}

fn folded(word: &str) -> String {
    word.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase()
}

/// A few words of `plain` around what `asked` matched, the matches between `⟦` and `⟧`.
pub fn snippet(plain: &str, asked: &str) -> String {
    let terms: Vec<String> = asked
        .split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty() && term != "or")
        .collect();
    let words: Vec<&str> = plain.split_whitespace().collect();
    let hit = |word: &str| {
        let word = folded(word);
        !word.is_empty() && terms.iter().any(|term| word.starts_with(term.as_str()))
    };
    let mut fragments: Vec<(usize, usize)> = Vec::new();
    for (at, word) in words.iter().enumerate() {
        if fragments.len() == SNIPPET_FRAGMENTS {
            break;
        }
        if hit(word) && fragments.last().is_none_or(|(_, end)| at >= *end) {
            let start = at.saturating_sub(SNIPPET_WORDS / 3);
            fragments.push((start, (start + SNIPPET_WORDS).min(words.len())));
        }
    }
    if fragments.is_empty() {
        fragments.push((0, SNIPPET_WORDS.min(words.len())));
    }
    fragments
        .iter()
        .map(|(start, end)| {
            words[*start..*end]
                .iter()
                .map(|word| if hit(word) { format!("⟦{word}⟧") } else { (*word).to_string() })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join(" … ")
}

/// What a deleted space held.
pub struct Emptied {
    pub space: Space,
    pub pages: Vec<String>,
    pub sources: Vec<Value>,
}

pub struct Store<'a>(pub &'a Backend);

impl Store<'_> {
    async fn all(&self, query: Query) -> Result<Vec<Map<String, Value>>, Refusal> {
        Ok(self.0.query_all(query).await?)
    }

    async fn first(&self, query: Query) -> Result<Option<Map<String, Value>>, Refusal> {
        Ok(self.0.query::<Map<String, Value>>(query.limit(1)).await?.records.into_iter().next())
    }

    pub async fn run(&self, writes: Vec<DataRequest>) -> Result<(), Refusal> {
        for batch in writes.chunks(BATCH) {
            self.0.batch(batch.to_vec()).await?;
        }
        Ok(())
    }

    /// Makes the space if it is new; a name, resource or owners given replace what it had. No
    /// owners given leaves the space's own alone, which is how a sync of one that has some does
    /// not have to name them again, and a name an administrator gave it is kept whatever is given.
    pub async fn ensure_space(
        &self,
        key: &str,
        name: &str,
        resource: Option<&str>,
        owners: &[String],
    ) -> Result<Space, Refusal> {
        let renamed = self
            .0
            .get::<Map<String, Value>>("spaces", key)
            .await?
            .filter(|space| space.get("renamed_at").is_some_and(|at| !at.is_null()))
            .and_then(|space| space.get("name").and_then(Value::as_str).map(str::to_string));
        let mut values = json!({ "key": key, "name": renamed.as_deref().unwrap_or(name) });
        if let Some(resource) = resource {
            values["resource"] = json!(resource);
        }
        if !owners.is_empty() {
            values["owners"] = json!(owners);
        }
        let (space, _): (Map<String, Value>, bool) =
            self.0.upsert("spaces", &["key"], values).await?;
        read(space)
    }

    /// An administrator's name for a space, which syncs keep from then on.
    pub async fn rename_space(&self, key: &str, name: &str, by: &str) -> Result<bool, Refusal> {
        let values = json!({ "name": name, "renamed_at": Utc::now(), "renamed_by": by });
        let changed: Option<Value> = self.0.update("spaces", key.to_string(), values, None).await?;
        Ok(changed.is_some())
    }

    /// Who looks after a space, replacing whoever did.
    pub async fn set_owners(&self, key: &str, owners: &[String]) -> Result<(), Refusal> {
        self.0.batch(vec![DataRequest::update("spaces", key, json!({ "owners": owners }))]).await?;
        Ok(())
    }

    /// Every page in a space as the catalogue is told about them, in the space's own order.
    pub async fn pages_of(&self, space: &str) -> Result<Vec<Map<String, Value>>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.pages_of(space));
        }
        let query = Query::new("documents")
            .filter(json!({ "space": space }))
            .order(Order::asc("space"))
            .order(Order::asc("position"))
            .order(Order::asc("path"))
            .fields(&["id", "path", "title", "resources", "source"]);
        self.all(query).await
    }

    /// Writes a page's resources, for when what its space is connected to changes.
    pub async fn set_page_resources(
        &self,
        writes: Vec<(String, Vec<String>)>,
    ) -> Result<(), Refusal> {
        let writes = writes
            .into_iter()
            .map(|(id, resources)| {
                DataRequest::update("documents", id, json!({ "resources": resources }))
            })
            .collect();
        self.run(writes).await
    }

    async fn counted(&self, spaces: Vec<Map<String, Value>>) -> Result<Vec<Space>, Refusal> {
        let counts = Aggregate::new("documents")
            .group_by("space")
            .measure("documents", Measure::Count("*".into()));
        let counted: BTreeMap<String, i64> = self
            .0
            .aggregate(counts)
            .await?
            .iter()
            .map(|group| {
                (text(group, "space"), group.get("documents").and_then(Value::as_i64).unwrap_or(0))
            })
            .collect();
        spaces
            .into_iter()
            .map(|mut space| {
                let documents = counted.get(&text(&space, "key")).copied().unwrap_or(0);
                space.insert("documents".into(), json!(documents));
                read(space)
            })
            .collect()
    }

    pub async fn space(&self, key: &str) -> Result<Option<Space>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.space(key));
        }
        let Some(space) = self.0.get::<Map<String, Value>>("spaces", key).await? else {
            return Ok(None);
        };
        Ok(self.counted(vec![space]).await?.into_iter().next())
    }

    /// The spaces in use: archived ones are out of sight until an administrator asks for them.
    pub async fn spaces(&self) -> Result<Vec<Space>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.spaces());
        }
        let query =
            Query::new("spaces").filter(json!({ "archived_at": null })).order(Order::asc("name"));
        let spaces = self.all(query).await?;
        self.counted(spaces).await
    }

    /// The spaces somebody archived, newest first: what an administrator restores or deletes.
    pub async fn archived_spaces(&self) -> Result<Vec<Space>, Refusal> {
        if crate::faux::corpus(self.0).await?.is_some() {
            return Ok(Vec::new());
        }
        let query = Query::new("spaces")
            .filter(json!({ "archived_at": { "is_null": false } }))
            .order(Order::desc("archived_at"));
        let spaces = self.all(query).await?;
        self.counted(spaces).await
    }

    /// Puts a space out of sight, or brings it back. Nothing in it is touched either way.
    pub async fn set_space_archived(&self, key: &str, by: Option<&str>) -> Result<bool, Refusal> {
        let values = match by {
            Some(by) => json!({ "archived_at": Utc::now(), "archived_by": by }),
            None => json!({ "archived_at": null, "archived_by": null }),
        };
        let changed: Option<Value> = self.0.update("spaces", key.to_string(), values, None).await?;
        Ok(changed.is_some())
    }

    /// The same for one source, which stops it syncing without touching the pages it brought.
    pub async fn set_source_archived(&self, id: Uuid, by: Option<&str>) -> Result<bool, Refusal> {
        let values = match by {
            Some(by) => json!({ "archived_at": Utc::now(), "archived_by": by }),
            None => json!({ "archived_at": null, "archived_by": null }),
        };
        let changed: Option<Value> = self.0.update("sources", id.to_string(), values, None).await?;
        Ok(changed.is_some())
    }

    /// The one upload source a space has, made the first time something is uploaded to it.
    pub async fn upload_source(&self, space: &str) -> Result<Uuid, Refusal> {
        let values = json!({ "kind": "upload", "space": space, "upload_space": space });
        let (source, _): (Map<String, Value>, bool) =
            self.0.upsert("sources", &["upload_space"], values).await?;
        text(&source, "id")
            .parse()
            .map_err(|_| Refusal::unavailable("the upload source was not saved"))
    }

    /// The source of the pages a plugin publishes into a space, made the first time. It is that
    /// plugin's alone, so publishing again replaces only what it wrote there and leaves what
    /// people and other sources put in the space alone.
    pub async fn plugin_source(&self, space: &str, plugin: &str) -> Result<Uuid, Refusal> {
        let held = self
            .all(Query::new("sources").filter(json!({ "space": space, "kind": "plugin" })))
            .await?
            .into_iter()
            .find(|source| {
                source.get("settings").map(|held| &held["plugin"]) == Some(&json!(plugin))
            });
        if let Some(held) = held {
            if held.get("archived_at").is_some_and(|at| !at.is_null()) {
                return Err(Refusal {
                    status: 409,
                    detail: format!(
                        "an administrator archived what {plugin} writes in {space}, so it writes \
                         nothing there until they restore it"
                    ),
                });
            }
            return text(&held, "id")
                .parse()
                .map_err(|_| Refusal::unavailable("a plugin's source has no usable ID"));
        }
        let id = Uuid::now_v7();
        self.add_source(id, "plugin", space, &json!({ "plugin": plugin }), None).await?;
        Ok(id)
    }

    /// A space's source for a git repository uploaded as a zip, made the first time and kept up
    /// to date with where the repository was when it was last uploaded.
    pub async fn git_source(&self, space: &str, settings: &Value) -> Result<Uuid, Refusal> {
        let held = self
            .all(Query::new("sources").filter(json!({ "space": space, "kind": "git" })))
            .await?
            .into_iter()
            .find(|source| {
                source.get("settings").map(|held| &held["repository"])
                    == Some(&settings["repository"])
            });
        if let Some(held) = held {
            let id: Uuid = text(&held, "id")
                .parse()
                .map_err(|_| Refusal::unavailable("a git source has no usable ID"))?;
            self.set_settings(id, settings).await?;
            return Ok(id);
        }
        let id = Uuid::now_v7();
        self.add(id, "git", space, settings, None, false).await?;
        Ok(id)
    }

    pub async fn sources(&self) -> Result<Vec<Value>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.sources());
        }
        self.sourced(json!({ "archived_at": null })).await
    }

    /// The sources somebody archived, which sync no more and are shown only to an administrator.
    pub async fn archived_sources(&self) -> Result<Vec<Value>, Refusal> {
        if crate::faux::corpus(self.0).await?.is_some() {
            return Ok(Vec::new());
        }
        self.sourced(json!({ "archived_at": { "is_null": false } })).await
    }

    /// Takes one source out for good. Its pages stay: they are the space's.
    pub async fn delete_source(&self, id: Uuid) -> Result<bool, Refusal> {
        Ok(self.0.delete("sources", id.to_string(), None).await?)
    }

    /// One source by its id, archived or not, for the page that restores or deletes it.
    pub async fn any_source(&self, id: Uuid) -> Result<Option<Value>, Refusal> {
        Ok(self.sourced(json!({ "id": id })).await?.into_iter().next())
    }

    async fn sourced(&self, filter: Value) -> Result<Vec<Value>, Refusal> {
        let query = Query::new("sources")
            .filter(filter)
            .order(Order::asc("space"))
            .order(Order::asc("kind"));
        Ok(self
            .all(query)
            .await?
            .into_iter()
            .map(|source| {
                json!({
                    "id": source.get("id"), "kind": source.get("kind"), "space": source.get("space"),
                    "settings": source.get("settings"), "schedule": source.get("schedule"),
                    "last_sync_at": source.get("last_sync_at"), "last_state": source.get("last_state"),
                    "last_error": source.get("last_error"), "created_at": source.get("_created_at"),
                    "managed": source.get("managed"), "archived_at": source.get("archived_at"),
                    "archived_by": source.get("archived_by"),
                })
            })
            .collect())
    }

    pub async fn add_source(
        &self,
        id: Uuid,
        kind: &str,
        space: &str,
        settings: &Value,
        schedule: Option<&str>,
    ) -> Result<(), Refusal> {
        self.add(id, kind, space, settings, schedule, false).await
    }

    /// A source the repository sync keeps, rather than one somebody added: it has no schedule of
    /// its own, because it is read when the repository it follows has been pushed to.
    pub async fn add_managed_source(
        &self,
        id: Uuid,
        space: &str,
        settings: &Value,
    ) -> Result<(), Refusal> {
        self.add(id, "github", space, settings, None, true).await
    }

    async fn add(
        &self,
        id: Uuid,
        kind: &str,
        space: &str,
        settings: &Value,
        schedule: Option<&str>,
        managed: bool,
    ) -> Result<(), Refusal> {
        let values = json!({
            "id": id, "kind": kind, "space": space, "settings": settings,
            "schedule": schedule, "managed": managed,
        });
        let _: Value = self.0.insert("sources", values).await?;
        Ok(())
    }

    /// What a managed source now reads, when the repository has moved to another default branch.
    pub async fn set_settings(&self, id: Uuid, settings: &Value) -> Result<(), Refusal> {
        let _: Option<Value> =
            self.0.update("sources", id.to_string(), json!({ "settings": settings }), None).await?;
        Ok(())
    }

    pub async fn set_schedule(&self, id: Uuid, schedule: Option<&str>) -> Result<(), Refusal> {
        let _: Option<Value> =
            self.0.update("sources", id.to_string(), json!({ "schedule": schedule }), None).await?;
        Ok(())
    }

    /// How a source's last sync went.
    pub async fn synced(&self, id: Uuid, state: &str, error: Option<&str>) -> Result<(), Refusal> {
        let set = json!({ "last_sync_at": now(), "last_state": state, "last_error": error });
        let _: Option<Value> = self.0.update("sources", id.to_string(), set, None).await?;
        Ok(())
    }

    /// It was not read, and not for a reason of its own: GitHub is limiting archive links. When it
    /// was last read is left as it was, so it is read again on the next run rather than waiting on
    /// whatever it was waiting on.
    pub async fn waiting(&self, id: Uuid, reason: &str) -> Result<(), Refusal> {
        let set = json!({ "last_state": "waiting", "last_error": reason });
        let _: Option<Value> = self.0.update("sources", id.to_string(), set, None).await?;
        Ok(())
    }

    /// Records an import and stages its pages, for the batches to work through in order.
    pub async fn stage(
        &self,
        space: &str,
        source: Uuid,
        by: &str,
        pages: &[crate::imports::Page],
    ) -> Result<Uuid, Refusal> {
        let id = Uuid::now_v7();
        let mut writes = vec![DataRequest::insert(
            "imports",
            json!({ "id": id, "space": space, "source": source, "total": pages.len(), "created_by": by }),
        )];
        writes.extend(pages.iter().enumerate().map(|(position, page)| {
            DataRequest::insert(
                "import-files",
                json!({
                    "import": id, "position": position, "path": page.path, "title": page.title,
                    "content": page.content, "format": page.format, "extra": page.extra,
                }),
            )
        }));
        self.run(writes).await?;
        Ok(id)
    }

    pub async fn import(&self, id: Uuid) -> Result<Option<Import>, Refusal> {
        Ok(self.0.get("imports", id.to_string()).await?)
    }

    pub async fn import_view(&self, id: Uuid) -> Result<Option<Value>, Refusal> {
        let Some(mut import) = self.0.get::<Map<String, Value>>("imports", id.to_string()).await?
        else {
            return Ok(None);
        };
        import.remove("source");
        if let Some(created) = import.remove("_created_at") {
            import.insert("created_at".into(), created);
        }
        import.retain(|field, _| !field.starts_with('_'));
        Ok(Some(Value::Object(import)))
    }

    pub async fn staged(
        &self,
        import: Uuid,
        from: i32,
        count: i32,
    ) -> Result<Vec<Staged>, Refusal> {
        let query = Query::new("import-files")
            .filter(json!({ "import": import, "position": { "gte": from } }))
            .order(Order::asc("import"))
            .order(Order::asc("position"))
            .limit(count.clamp(1, 1_000) as u32);
        let found = self.0.query::<Map<String, Value>>(query).await?.records;
        found.into_iter().map(read).collect()
    }

    /// Saves a batch of an import's pages, and how far the import has got, in one transaction:
    /// `done` pages read, of which `changed` were changed at their source.
    pub async fn save_pages(
        &self,
        import: Uuid,
        space: &str,
        source: Option<Uuid>,
        pages: &[Value],
        done: usize,
        changed: usize,
    ) -> Result<(), Refusal> {
        let mut writes: Vec<DataRequest> = pages
            .iter()
            .map(|page| {
                let mut values = page.clone();
                values["space"] = json!(space);
                values["source"] = json!(source);
                DataRequest::upsert("documents", &["space", "path"], values)
            })
            .collect();
        let progress = self.0.get::<Map<String, Value>>("imports", import.to_string()).await?;
        let (so_far, changed_so_far) = progress
            .map(|import| {
                let number = |field: &str| import.get(field).and_then(Value::as_i64).unwrap_or(0);
                (number("done"), number("changed"))
            })
            .unwrap_or_default();
        writes.push(DataRequest::update(
            "imports",
            import.to_string(),
            json!({ "done": so_far + done as i64, "changed": changed_so_far + changed as i64 }),
        ));
        self.run(writes).await
    }

    /// Marks an import done, clears what it staged, and records how its source's sync went.
    pub async fn finish(&self, import: &Import, removed: usize) -> Result<(), Refusal> {
        let set = json!({ "state": "succeeded", "removed": removed, "finished_at": now() });
        let _: Option<Value> = self.0.update("imports", import.id.to_string(), set, None).await?;
        self.0.delete_where("import-files", "id", json!({ "import": import.id })).await?;
        if let Some(source) = import.source {
            self.synced(source, "succeeded", None).await?;
        }
        Ok(())
    }

    /// The versions a source's pages were last imported at, by path.
    pub async fn versions(&self, source: Uuid) -> Result<BTreeMap<String, String>, Refusal> {
        let query = Query::new("documents")
            .filter(json!({ "source": source, "source_version": { "is_null": false } }))
            .fields(&["path", "source_version"]);
        Ok(self
            .all(query)
            .await?
            .iter()
            .map(|row| (text(row, "path"), text(row, "source_version")))
            .collect())
    }

    /// Keeps an attachment a page shows, replacing any of the same name.
    pub async fn attach(
        &self,
        space: &str,
        page: &str,
        attachment: &crate::confluence::Attachment,
    ) -> Result<(), Refusal> {
        let crate::confluence::Attachment { name, content_type, bytes } = attachment;
        self.attach_file(space, page, name, content_type, bytes).await
    }

    /// Keeps a file with page `page`, replacing one of the same name.
    pub async fn attach_file(
        &self,
        space: &str,
        page: &str,
        name: &str,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<(), Refusal> {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD.encode(bytes);
        let values = json!({
            "space": space, "page": page, "name": name,
            "content_type": content_type, "bytes": bytes,
        });
        let _: (Value, bool) =
            self.0.upsert("attachments", &["space", "page", "name"], values).await?;
        Ok(())
    }

    /// An attachment's type and bytes.
    pub async fn attachment(
        &self,
        space: &str,
        page: &str,
        name: &str,
    ) -> Result<Option<(String, Vec<u8>)>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.attachment(space, page, name));
        }
        use base64::Engine;
        let query =
            Query::new("attachments").filter(json!({ "space": space, "page": page, "name": name }));
        let Some(found) = self.first(query).await? else { return Ok(None) };
        let content_type = found
            .get("content_type")
            .and_then(Value::as_str)
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes =
            base64::engine::general_purpose::STANDARD.decode(text(&found, "bytes")).map_err(
                |err| Refusal::unavailable(format!("an attachment could not be read: {err}")),
            )?;
        Ok(Some((content_type, bytes)))
    }

    pub async fn hashes(&self, space: &str, paths: &[String]) -> Result<Vec<Existing>, Refusal> {
        let mut found = Vec::new();
        for chunk in paths.chunks(CHUNK) {
            let query = Query::new("documents")
                .filter(json!({ "space": space, "path": { "in": chunk } }))
                .fields(&["path", "content_hash"]);
            for row in self.all(query).await? {
                found.push(read(row)?);
            }
        }
        Ok(found)
    }

    /// Takes one page out, with the files attached to it and any edit of it; false when there was
    /// no such page.
    pub async fn delete_document(&self, space: &str, path: &str) -> Result<bool, Refusal> {
        let found = self
            .first(
                Query::new("documents")
                    .filter(json!({ "space": space, "path": path }))
                    .fields(&["id"]),
            )
            .await?;
        let Some(found) = found else { return Ok(false) };
        let attachments = self
            .all(
                Query::new("attachments")
                    .filter(json!({ "space": space, "page": path }))
                    .fields(&["id"]),
            )
            .await?;
        let mut writes: Vec<DataRequest> = attachments
            .iter()
            .map(|row| DataRequest::delete("attachments", text(row, "id")))
            .collect();
        if let Some(edit) = self.edit(space, path).await? {
            writes.push(DataRequest::delete("edits", edit.id.to_string()));
        }
        writes.push(DataRequest::delete("documents", text(&found, "id")));
        self.run(writes).await?;
        Ok(true)
    }

    /// One page's whole record, at its path or with `.md` added, as a page's address leaves it off.
    pub async fn document_row(
        &self,
        space: &str,
        path: &str,
    ) -> Result<Option<Map<String, Value>>, Refusal> {
        for path in [path.to_string(), format!("{path}.md")] {
            let query = Query::new("documents").filter(json!({ "space": space, "path": path }));
            if let Some(found) = self.first(query).await? {
                return Ok(Some(found));
            }
        }
        Ok(None)
    }

    /// Keeps the Markdown a page was written in, for a page synced before it was kept.
    pub async fn set_text(&self, document: &str, text: &str) -> Result<(), Refusal> {
        let _: Option<Value> =
            self.0.update("documents", document.to_string(), json!({ "text": text }), None).await?;
        Ok(())
    }

    /// Those of `paths` whose Markdown is not kept yet.
    pub async fn without_text(
        &self,
        space: &str,
        paths: &[String],
    ) -> Result<BTreeSet<String>, Refusal> {
        let mut found = BTreeSet::new();
        for chunk in paths.chunks(CHUNK) {
            let filter =
                json!({ "space": space, "path": { "in": chunk }, "text": { "is_null": true } });
            let query = Query::new("documents").filter(filter).fields(&["path"]);
            found.extend(self.all(query).await?.iter().map(|row| text(row, "path")));
        }
        Ok(found)
    }

    /// The names of the files kept with a page.
    pub async fn attachment_names(&self, space: &str, page: &str) -> Result<Vec<String>, Refusal> {
        let query = Query::new("attachments")
            .filter(json!({ "space": space, "page": page }))
            .fields(&["name"]);
        Ok(self.all(query).await?.iter().map(|row| text(row, "name")).collect())
    }

    pub async fn edit(&self, space: &str, path: &str) -> Result<Option<Edit>, Refusal> {
        let query = Query::new("edits").filter(json!({ "space": space, "path": path }));
        self.first(query).await?.map(read).transpose()
    }

    /// The edits among `paths`, by path.
    pub async fn edits_in(
        &self,
        space: &str,
        paths: &[String],
    ) -> Result<BTreeMap<String, Edit>, Refusal> {
        let mut found = BTreeMap::new();
        for chunk in paths.chunks(CHUNK) {
            let filter = json!({ "space": space, "path": { "in": chunk } });
            for row in self.all(Query::new("edits").filter(filter)).await? {
                let edit: Edit = read(row)?;
                found.insert(edit.path.clone(), edit);
            }
        }
        Ok(found)
    }

    /// An edit, and the page showing it, in one transaction.
    pub async fn save_edit(
        &self,
        document: &str,
        shown: Value,
        edit: Value,
    ) -> Result<(), Refusal> {
        self.run(vec![
            DataRequest::update("documents", document, shown),
            DataRequest::upsert("edits", &["space", "path"], edit),
        ])
        .await
    }

    /// Takes an edit away and puts back what the page's source has, in one transaction.
    pub async fn discard_edit(
        &self,
        document: &str,
        original: Value,
        edit: Uuid,
    ) -> Result<(), Refusal> {
        self.run(vec![
            DataRequest::update("documents", document, original),
            DataRequest::delete("edits", edit.to_string()),
        ])
        .await
    }

    /// The pull request an edit was proposed in, and what it said then.
    pub async fn proposed(
        &self,
        edit: Uuid,
        pull_request: &str,
        branch: &str,
        by: &str,
        hash: &str,
    ) -> Result<(), Refusal> {
        let set = json!({
            "pull_request": pull_request, "branch": branch, "proposed_at": now(),
            "proposed_by": by, "proposed_hash": hash,
        });
        let _: Option<Value> = self.0.update("edits", edit.to_string(), set, None).await?;
        Ok(())
    }

    /// What a sync brought for pages edited in DOC, which go on showing their edits: the source's
    /// page is kept beside each edit, and only its place and version move on the page itself.
    pub async fn divert(&self, space: &str, diverted: &[(Edit, Value)]) -> Result<(), Refusal> {
        let paths: Vec<String> = diverted.iter().map(|(edit, _)| edit.path.clone()).collect();
        let mut ids = BTreeMap::new();
        for chunk in paths.chunks(CHUNK) {
            let filter = json!({ "space": space, "path": { "in": chunk } });
            let query = Query::new("documents").filter(filter).fields(&["id", "path"]);
            ids.extend(
                self.all(query).await?.iter().map(|row| (text(row, "path"), text(row, "id"))),
            );
        }
        let mut writes = Vec::new();
        for (edit, row) in diverted {
            if let Some(id) = ids.get(&edit.path) {
                let moved = json!({
                    "content_hash": row["content_hash"], "position": row["position"],
                    "source_version": row["source_version"], "source_url": row["source_url"],
                });
                writes.push(DataRequest::update("documents", id.clone(), moved));
            }
            let kept = json!({ "original": original_of(row), "source_gone": false });
            writes.push(DataRequest::update("edits", edit.id.to_string(), kept));
        }
        self.run(writes).await
    }

    /// Edits whose source now says the same, which have nothing left to keep.
    pub async fn retire_edits(&self, edits: &[Uuid]) -> Result<(), Refusal> {
        self.run(edits.iter().map(|edit| DataRequest::delete("edits", edit.to_string())).collect())
            .await
    }

    /// Takes a space out with everything in it — its sources, pages, imports and files — and says
    /// what it held, for the catalogue: the paths of its pages and its sources.
    pub async fn delete_space(&self, key: &str) -> Result<Option<Emptied>, Refusal> {
        let Some(space) = self.space(key).await? else { return Ok(None) };
        let pages = self
            .all(Query::new("documents").filter(json!({ "space": key })).fields(&["path"]))
            .await?
            .iter()
            .map(|row| text(row, "path"))
            .collect();
        let sources = self
            .all(Query::new("sources").filter(json!({ "space": key })))
            .await?
            .into_iter()
            .map(Value::Object)
            .collect();
        self.0.delete("spaces", key, None).await?;
        Ok(Some(Emptied { space, pages, sources }))
    }

    /// Documents a finished import no longer has, which go with it: their paths.
    pub async fn remove_missing(&self, import: &Import) -> Result<Vec<String>, Refusal> {
        let staged = self
            .all(
                Query::new("import-files").filter(json!({ "import": import.id })).fields(&["path"]),
            )
            .await?;
        let kept: BTreeSet<String> = staged.iter().map(|row| text(row, "path")).collect();
        let documents = self
            .all(
                Query::new("documents")
                    .filter(json!({ "space": import.space, "source": import.source }))
                    .fields(&["id", "path"]),
            )
            .await?;
        let gone: Vec<&Map<String, Value>> =
            documents.iter().filter(|row| !kept.contains(&text(row, "path"))).collect();
        // A page edited in DOC is DOC's own copy, which stays when its source drops the page.
        let paths: Vec<String> = gone.iter().map(|row| text(row, "path")).collect();
        let edited = self.edits_in(&import.space, &paths).await?;
        let (stay, gone): (Vec<_>, Vec<_>) =
            gone.into_iter().partition(|row| edited.contains_key(&text(row, "path")));
        let mut writes: Vec<DataRequest> =
            gone.iter().map(|row| DataRequest::delete("documents", text(row, "id"))).collect();
        writes.extend(stay.iter().filter_map(|row| edited.get(&text(row, "path"))).map(|edit| {
            DataRequest::update("edits", edit.id.to_string(), json!({ "source_gone": true }))
        }));
        self.run(writes).await?;
        Ok(gone.iter().map(|row| text(row, "path")).collect())
    }

    /// The kind of source each of these came from; a page with none was uploaded.
    async fn source_kinds(&self) -> Result<BTreeMap<String, String>, Refusal> {
        let sources = self.all(Query::new("sources").fields(&["id", "kind"])).await?;
        Ok(sources.iter().map(|row| (text(row, "id"), text(row, "kind"))).collect())
    }

    pub async fn document(&self, space: &str, path: &str) -> Result<Option<Value>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.document(space, path));
        }
        let query = Query::new("documents").filter(json!({ "space": space, "path": path }));
        let Some(found) = self.first(query).await? else { return Ok(None) };
        let kinds = self.source_kinds().await?;
        let source = kinds.get(&text(&found, "source")).cloned().unwrap_or_else(|| "upload".into());
        let edited = self.edit(space, &text(&found, "path")).await?.map(|edit| {
            json!({ "by": edit.by, "at": edit.updated_at, "pull_request": edit.pull_request })
        });
        Ok(Some(json!({
            "space": found.get("space"), "path": found.get("path"), "title": found.get("title"),
            "html": found.get("html"), "front_matter": found.get("front_matter"),
            "resources": found.get("resources"), "tags": found.get("tags"),
            "source_url": found.get("source_url"), "updated_at": found.get("_updated_at"),
            "source": source, "edited": edited,
        })))
    }

    /// Every page that documents `resource` (written `kind:name`), whichever source it came from.
    pub async fn documenting(&self, resource: &str) -> Result<Vec<Value>, Refusal> {
        self.documenting_any(std::slice::from_ref(&resource.to_string()), &[]).await
    }

    /// Every page that documents any of them: what a service is documented by is its own pages and
    /// the pages written in the repositories it is built from.
    /// Every page naming any of `resources`, and every page in any of `spaces`: what a service's
    /// docs are, with the spaces linked to it.
    pub async fn documenting_any(
        &self,
        resources: &[String],
        spaces: &[String],
    ) -> Result<Vec<Value>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.documenting_any(resources, spaces));
        }
        if resources.is_empty() && spaces.is_empty() {
            return Ok(Vec::new());
        }
        let any: Vec<Value> = resources
            .iter()
            .map(|resource| json!({ "resources": { "contains": resource } }))
            .chain(spaces.iter().map(|space| json!({ "space": space })))
            .collect();
        let mut filter = json!({ "any": any });
        // A page in an archived space documents nothing any more: the panel on a service must not
        // point at a page nobody can open.
        let archived: Vec<String> =
            self.archived_spaces().await?.into_iter().map(|space| space.key).collect();
        if !archived.is_empty() {
            filter["space"] = json!({ "not_in": archived });
        }
        let query = Query::new("documents")
            .filter(filter)
            .order(Order::asc("space"))
            .order(Order::asc("position"))
            .order(Order::asc("path"))
            .fields(&["space", "path", "title", "source"]);
        let kinds = self.source_kinds().await?;
        Ok(self
            .all(query)
            .await?
            .into_iter()
            .map(|row| {
                let source = kinds.get(&text(&row, "source")).cloned().unwrap_or_else(|| "upload".into());
                json!({ "space": row.get("space"), "path": row.get("path"), "title": row.get("title"), "source": source })
            })
            .collect())
    }

    /// How many pages document each resource, by the kind of source they came from. A page in a
    /// space linked to a service documents that service too, counted once however it does.
    pub async fn documented(
        &self,
        links: &BTreeMap<String, Vec<String>>,
    ) -> Result<BTreeMap<String, BTreeMap<String, i64>>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.documented(links));
        }
        let kinds = self.source_kinds().await?;
        let documents =
            self.all(Query::new("documents").fields(&["resources", "source", "space"])).await?;
        let mut linked: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for (service, spaces) in links {
            for space in spaces {
                linked.entry(space.as_str()).or_default().push(format!("service:{service}"));
            }
        }
        let mut counted: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
        for row in &documents {
            let source =
                kinds.get(&text(row, "source")).cloned().unwrap_or_else(|| "upload".into());
            let mut named: BTreeSet<String> = row
                .get("resources")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
            named.extend(linked.get(text(row, "space").as_str()).into_iter().flatten().cloned());
            for resource in named {
                *counted.entry(resource).or_default().entry(source.clone()).or_default() += 1;
            }
        }
        Ok(counted)
    }

    /// Each service's linked spaces, by the service's name.
    pub async fn links(&self) -> Result<BTreeMap<String, Vec<String>>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.links());
        }
        let rows = self.all(Query::new("links").fields(&["service", "space"])).await?;
        let mut links: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for row in &rows {
            links.entry(text(row, "service")).or_default().push(text(row, "space"));
        }
        Ok(links)
    }

    pub async fn linked(&self, service: &str) -> Result<Vec<String>, Refusal> {
        Ok(self.links().await?.remove(service).unwrap_or_default())
    }

    /// Links a space to a service; false when it was already.
    pub async fn link(&self, service: &str, space: &str, by: &str) -> Result<bool, Refusal> {
        let values = json!({
            "id": format!("{service}/{space}"), "service": service, "space": space, "by": by,
        });
        match self.0.insert::<Value>("links", values).await {
            Ok(_) => Ok(true),
            Err(err) if err.is_duplicate() => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    /// Takes a space's link to a service away; false when there was none.
    pub async fn unlink(&self, service: &str, space: &str) -> Result<bool, Refusal> {
        Ok(self.0.delete("links", format!("{service}/{space}"), None).await?)
    }

    /// The pages most recently changed in `spaces`, newest first.
    pub async fn recent_in(&self, spaces: &[String], limit: usize) -> Result<Vec<Value>, Refusal> {
        if spaces.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            let pages = faux.every_page().into_iter();
            let kept =
                pages.filter(|page| spaces.iter().any(|space| page["space"] == json!(space)));
            return Ok(kept.take(limit).collect());
        }
        let query = Query::new("documents")
            .filter(json!({ "space": { "in": spaces } }))
            .order(Order::desc("_updated_at"))
            .limit(u32::try_from(limit).unwrap_or(10))
            .fields(&["space", "path", "title", "_updated_at"]);
        Ok(self
            .0
            .query::<Map<String, Value>>(query)
            .await?
            .records
            .into_iter()
            .map(Value::Object)
            .collect())
    }

    /// Every page in every space, with what says whether it is a runbook: its address, title,
    /// front matter, labels and what it documents. Archived spaces are the caller's to leave out.
    pub async fn every_page(&self) -> Result<Vec<Value>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.every_page());
        }
        let query = Query::new("documents")
            .order(Order::asc("space"))
            .order(Order::asc("position"))
            .order(Order::asc("path"))
            .fields(&["space", "path", "title", "front_matter", "tags", "resources"]);
        Ok(self.all(query).await?.into_iter().map(Value::Object).collect())
    }

    pub async fn tree(&self, space: &str) -> Result<Vec<Value>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.tree(space));
        }
        let query = Query::new("documents")
            .filter(json!({ "space": space }))
            .order(Order::asc("space"))
            .order(Order::asc("position"))
            .order(Order::asc("path"))
            .fields(&["path", "title", "position", "_updated_at"]);
        Ok(self
            .all(query)
            .await?
            .into_iter()
            .map(|row| {
                json!({ "path": row.get("path"), "title": row.get("title"), "position": row.get("position"), "updated_at": row.get("_updated_at") })
            })
            .collect())
    }

    /// Best matches first, with a snippet whose matches sit between `⟦` and `⟧`.
    pub async fn search(
        &self,
        text: &str,
        space: Option<&str>,
        limit: i64,
    ) -> Result<Vec<Value>, Refusal> {
        if let Some(faux) = crate::faux::corpus(self.0).await? {
            return Ok(faux.search(text, space, limit));
        }
        let mut query = Query::new("documents")
            .search(text)
            .fields(&["space", "path", "title", "plain"])
            .limit(limit.clamp(1, 1_000) as u32);
        // An archived space is out of search as it is out of the listings: it is kept, not shown.
        let archived: Vec<String> =
            self.archived_spaces().await?.into_iter().map(|space| space.key).collect();
        match (space, archived.is_empty()) {
            // Naming one does not get inside it: an archived space answers nothing to anybody
            // until it is restored.
            (Some(space), _) if archived.iter().any(|key| key == space) => return Ok(Vec::new()),
            (Some(space), _) => query = query.filter(json!({ "space": space })),
            (None, false) => query = query.filter(json!({ "space": { "not_in": archived } })),
            (None, true) => {}
        }
        let found = self.0.query::<Map<String, Value>>(query).await?.records;
        let count = found.len();
        Ok(found
            .into_iter()
            .enumerate()
            .map(|(at, row)| {
                json!({
                    "space": row.get("space"), "path": row.get("path"), "title": row.get("title"),
                    "rank": (count - at) as f64 / count as f64,
                    "snippet": snippet(&self::text(&row, "plain"), text),
                })
            })
            .collect())
    }
}
