//! The plugin's routes. Whoever reads Watercooler sees every thread, and writers start threads and
//! reply, to the thread or to any message in it, and like messages; whoever wrote a message, and
//! only they, may change it, and a thread's title with its first; moderators (DOC's administrators
//! and whoever holds `moderate`) may delete any message, or a whole thread. Each tag is checked with
//! Resource Definitions as the writer. New threads, replies, changes, likes and deletions are
//! published as `plugin.water.thread.created`, `.reply.created`, `.message.edited`,
//! `.message.liked`, `.message.deleted` and `.thread.deleted`; deletions are audited.

use std::collections::BTreeMap;

use doc_plugin_sdk::{Backend, Caller, Request, Response};
use serde::Deserialize;
use serde_json::{Value, json};
use url::form_urlencoded::byte_serialize;
use uuid::Uuid;

use crate::markdown::{self, Rendered};
use crate::store::{Message, Store, Thread};
use crate::{Refusal, cards, events};

type Answer = Result<(u16, Value), Refusal>;

const MAX_TAGS: usize = 20;
const MAX_BODY: usize = 20_000;

pub fn query(request: &Request, key: &str) -> Option<String> {
    url::form_urlencoded::parse(request.query.as_bytes())
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn body<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T, Refusal> {
    let bytes = if request.body.is_empty() { &b"{}"[..] } else { &request.body[..] };
    serde_json::from_slice(bytes)
        .map_err(|err| Refusal::bad(format!("the body is not what this takes: {err}")))
}

fn encoded(text: &str) -> String {
    byte_serialize(text.as_bytes()).collect::<String>().replace('+', "%20")
}

/// The writer's login, and who they are as `user:<id>` or `service:<id>`.
fn author(backend: &Backend) -> Result<(String, String), Refusal> {
    match backend.caller() {
        Some(Caller { kind, id: Some(id), label, .. }) if kind == "user" || kind == "service" => {
            Ok((label.clone().unwrap_or_else(|| id.clone()), format!("{kind}:{id}")))
        }
        _ => Err(Refusal::forbidden("discussions are for people and service accounts")),
    }
}

/// Where people see this plugin's pages in DOC.
pub fn page(path: &str) -> String {
    let base = std::env::var("DOC_PUBLIC_URL").unwrap_or_default();
    format!("{}/p/water/{path}", base.trim_end_matches('/'))
}

/// What Resource Definitions shows the caller, asked as them once for each resource.
pub struct Sight<'a> {
    backend: &'a Backend,
    seen: BTreeMap<String, Result<Value, u16>>,
}

impl<'a> Sight<'a> {
    pub fn new(backend: &'a Backend) -> Self {
        Self { backend, seen: BTreeMap::new() }
    }

    /// The resource and its connections, for a tag written `kind:name`. A plugin is the
    /// platform's rather than the Catalogue's, and answers the same way with no connections.
    pub async fn resource(&mut self, tag: &str) -> Result<Value, u16> {
        if let Some(found) = self.seen.get(tag) {
            return found.clone();
        }
        let (kind, name) = tag.split_once(':').unwrap_or_default();
        if kind == "plugin" {
            let found = match plugins(self.backend).await {
                Ok(known) => known
                    .into_iter()
                    .find(|plugin| plugin.id == name)
                    .map(|plugin| json!({ "resource": plugin.resource(), "connections": [] }))
                    .ok_or(404),
                Err(_) => Err(503),
            };
            self.seen.insert(tag.to_string(), found.clone());
            return found;
        }
        let path: Vec<String> = name.split('/').map(encoded).collect();
        let route = format!("resources/{kind}/{}", path.join("/"));
        let found = match self.backend.ask("resources", "GET", &route, None, None).await {
            Ok((200, answer)) => Ok(answer),
            Ok((status, _)) => Err(status),
            Err(_) => Err(503),
        };
        self.seen.insert(tag.to_string(), found.clone());
        found
    }

    async fn sees(&mut self, tag: &str) -> bool {
        self.resource(tag).await.is_ok()
    }
}

/// A plugin registered with the platform, as a tag names it.
pub struct Plugin {
    pub id: String,
    /// What its navigation calls it, or its ID when it has none.
    pub title: String,
    /// Whether it has pages of its own, at `/p/<id>/`.
    pub paged: bool,
}

impl Plugin {
    /// Its page: its own pages where it has them, or what the platform says about it.
    pub fn href(&self) -> String {
        match self.paged {
            true => format!("/p/{}/", self.id),
            false => format!("/plugins/{}", self.id),
        }
    }

    /// The plugin as a tag's page reads a resource.
    fn resource(&self) -> Value {
        json!({ "kind": "Plugin", "name": self.id, "title": self.title })
    }
}

/// Every plugin registered with the platform, which anyone may tag a discussion with.
pub async fn plugins(backend: &Backend) -> Result<Vec<Plugin>, Refusal> {
    let query = doc_plugin_sdk::Query::new("core.plugins").fields(&["id", "display_name"]);
    let listed: Vec<Value> = backend
        .query_all(query)
        .await
        .map_err(|err| Refusal::unavailable(format!("the platform's plugins: {err}")))?;
    let mut plugins: Vec<Plugin> = listed
        .iter()
        .filter_map(|plugin| {
            let id = plugin["id"].as_str()?.to_string();
            let named = plugin["display_name"].as_str().filter(|name| !name.is_empty());
            Some(Plugin {
                title: named.map_or_else(|| id.clone(), str::to_string),
                paged: named.is_some(),
                id,
            })
        })
        .collect();
    plugins.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(plugins)
}

pub fn thread_shown(thread: &Thread) -> Value {
    json!({
        "id": thread.id,
        "title": thread.title,
        "author": thread.author,
        "tags": thread.tags,
        "replies": thread.replies,
        "last_at": thread.last_at,
        "created_at": thread.created_at,
        "headline": thread.headline,
        "url": page(&format!("threads/{}", thread.id)),
    })
}

fn message_shown(message: &Message) -> Value {
    json!({
        "id": message.id,
        "author": message.author,
        "body": message.body,
        "html": message.html,
        "tags": message.tags,
        "parent": message.parent,
        "created_at": message.created_at,
        "edited_at": message.edited_at,
    })
}

/// Who may take any message or thread out: DOC's administrators, and whoever holds `moderate`.
pub const MODERATE: &str = "moderate";

/// An ability rather than something read or written, so held at any scope (DOC-SPEC §6).
pub fn moderates(backend: &Backend) -> bool {
    backend.caller().is_some_and(|caller| caller.admin || caller.custom.contains_key(MODERATE))
}

/// What a moderator's deletion took out: a thread, by its title, or a reply, by its author.
pub enum Removed {
    Thread(String),
    Message(String),
}

/// Takes a message out, or the whole thread with its opening post; only moderators may.
pub async fn remove(
    backend: &Backend,
    thread_id: &str,
    message_id: &str,
) -> Result<Removed, Refusal> {
    if !moderates(backend) {
        return Err(Refusal::forbidden("only a moderator can delete a message"));
    }
    let store = Store(backend);
    let thread = found(&store, thread_id).await?;
    let (message, first) = message_of(&store, &thread, message_id).await?;
    if first {
        return remove_thread(backend, thread_id).await.map(|thread| Removed::Thread(thread.title));
    }
    store.remove(&message).await?;
    let detail = json!({
        "thread": thread.id,
        "title": thread.title,
        "message": message.id,
        "author": message.author,
        "by": who(backend),
    });
    if let Err(err) =
        backend.audit("message.deleted", Some(&thread.id.to_string()), detail.clone()).await
    {
        tracing::warn!(%err, "a deleted message was not audited");
    }
    announce(backend, "plugin.water.message.deleted", detail).await;
    Ok(Removed::Message(message.author))
}

/// Takes a thread out with everything in it; only moderators may.
pub async fn remove_thread(backend: &Backend, thread_id: &str) -> Result<Thread, Refusal> {
    if !moderates(backend) {
        return Err(Refusal::forbidden("only a moderator can delete a discussion"));
    }
    let store = Store(backend);
    let thread = found(&store, thread_id).await?;
    store.remove_thread(thread.id).await?;
    let detail = json!({
        "thread": thread.id,
        "title": thread.title,
        "author": thread.author,
        "replies": thread.replies,
        "by": who(backend),
    });
    if let Err(err) =
        backend.audit("thread.deleted", Some(&thread.id.to_string()), detail.clone()).await
    {
        tracing::warn!(%err, "a deleted thread was not audited");
    }
    announce(backend, "plugin.water.thread.deleted", detail).await;
    Ok(thread)
}

/// Who the caller is, as a record of what they did says.
fn who(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "someone".into())
}

/// Likes a message as the caller, or takes their like back; how many like it then, and whether
/// the caller is one.
pub async fn like(
    backend: &Backend,
    thread_id: &str,
    message_id: &str,
    liked: bool,
) -> Result<(i64, bool), Refusal> {
    let (login, reference) = author(backend)?;
    let store = Store(backend);
    let thread = found(&store, thread_id).await?;
    let (message, _) = message_of(&store, &thread, message_id).await?;
    let changed = match liked {
        true => store.like(&message, &reference, &login).await?,
        false => store.unlike(message.id, &reference).await?,
    };
    if changed && liked {
        let payload = json!({
            "thread": thread.id,
            "message": message.id,
            "title": thread.title,
            "author": message.author,
            "by": login,
            "url": page(&format!("threads/{}#m-{}", thread.id, message.id)),
        });
        announce(backend, "plugin.water.message.liked", payload).await;
    }
    let likes = store.likes(thread.id).await?;
    let count = likes.iter().filter(|like| like.message == message.id).count() as i64;
    Ok((count, liked))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewThread {
    pub title: String,
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewReply {
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    /// The message it answers; none, or the opening post, answers the thread.
    #[serde(default)]
    pub parent: Option<Uuid>,
}

/// A change to a message by whoever wrote it: its words and the tags added beside them, and for a
/// thread's first message, the thread's title.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Edit {
    pub body: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub title: Option<String>,
}

/// The caller as a message records its author, `user:<id>` or `service:<id>`.
pub fn me(backend: &Backend) -> Option<String> {
    author(backend).ok().map(|(_, reference)| reference)
}

/// Whether the caller wrote `message`, and so may change it.
pub fn wrote(backend: &Backend, message: &Message) -> bool {
    author(backend).is_ok_and(|(_, reference)| message.by(&reference))
}

fn titled(title: &str) -> Result<String, Refusal> {
    let title = title.trim();
    match title.is_empty() || title.chars().count() > 200 {
        true => Err(Refusal::bad("a thread's title is 1 to 200 characters")),
        false => Ok(title.to_string()),
    }
}

/// A message of a thread, with whether it is the thread's first.
pub async fn message_of(
    store: &Store<'_>,
    thread: &Thread,
    id: &str,
) -> Result<(Message, bool), Refusal> {
    let missing = || Refusal::missing("there is no such message in this thread");
    let id: Uuid = id.parse().map_err(|_| missing())?;
    let messages = store.messages(thread.id).await?;
    let first = messages.first().is_some_and(|first| first.id == id);
    let message = messages.into_iter().find(|message| message.id == id).ok_or_else(missing)?;
    Ok((message, first))
}

/// Changes a message, which only whoever wrote it may do.
pub async fn edit(
    backend: &Backend,
    sight: &mut Sight<'_>,
    thread_id: &str,
    message_id: &str,
    asked: Edit,
) -> Result<(Thread, Message), Refusal> {
    let (_, reference) = author(backend)?;
    let store = Store(backend);
    let thread = found(&store, thread_id).await?;
    let (message, first) = message_of(&store, &thread, message_id).await?;
    if !message.by(&reference) {
        return Err(Refusal::forbidden("only whoever wrote a message can change it"));
    }
    let title = match (&asked.title, first) {
        (Some(title), true) => Some(titled(title)?),
        (Some(_), false) => {
            return Err(Refusal::bad("only a thread's first message has its title"));
        }
        (None, _) => None,
    };
    let (rendered, tags) = written(sight, &asked.body, &asked.tags).await?;
    if first && tags.is_empty() {
        return Err(Refusal::bad(
            "a thread's first message tags at least one resource: choose one in Tags",
        ));
    }
    let edited = Message {
        body: asked.body.trim().to_string(),
        html: rendered.html,
        text: rendered.text,
        tags,
        ..message
    };
    store.edit(&edited, title.as_deref()).await?;
    let thread = store.thread(thread.id).await?.ok_or_else(|| Refusal::missing("it has gone"))?;
    let edited = store.message(edited.id).await?.ok_or_else(|| Refusal::missing("it has gone"))?;
    let payload = json!({
        "thread": thread.id,
        "message": edited.id,
        "first": first,
        "title": thread.title,
        "author": edited.author,
        "tags": edited.tags,
        "url": page(&format!("threads/{}#m-{}", thread.id, edited.id)),
    });
    announce(backend, "plugin.water.message.edited", payload).await;
    Ok((thread, edited))
}

/// A message rendered, with the tags written in it and those asked for, each checked as the writer.
async fn written(
    sight: &mut Sight<'_>,
    text: &str,
    asked: &[String],
) -> Result<(Rendered, Vec<String>), Refusal> {
    if text.trim().is_empty() || text.chars().count() > MAX_BODY {
        return Err(Refusal::bad(format!("a message is 1 to {MAX_BODY} characters")));
    }
    let rendered = markdown::render(text);
    let mut tags = rendered.tags.clone();
    for wanted in asked {
        let tag = markdown::tag_of(wanted.trim().trim_start_matches('/')).ok_or_else(|| {
            Refusal::bad(format!("`{wanted}` is not a resource or a plugin, written kind:name"))
        })?;
        if !tags.contains(&tag) {
            tags.push(tag);
        }
    }
    if tags.len() > MAX_TAGS {
        return Err(Refusal::bad(format!("a message tags up to {MAX_TAGS} resources and plugins")));
    }
    for tag in &tags {
        if !sight.sees(tag).await {
            return Err(Refusal::bad(format!("there is no {tag} to tag, or you cannot see it")));
        }
    }
    Ok((rendered, tags))
}

async fn announce(backend: &Backend, topic: &str, payload: Value) {
    if let Err(err) = backend.publish(topic, payload).await {
        tracing::warn!(%err, %topic, "a discussion event was not published");
    }
}

pub async fn found(store: &Store<'_>, id: &str) -> Result<Thread, Refusal> {
    let id: Uuid = id.parse().map_err(|_| Refusal::missing("there is no such thread"))?;
    store.thread(id).await?.ok_or_else(|| Refusal::missing("there is no such thread"))
}

/// Starts a thread, which must tag at least one resource.
pub async fn start(
    backend: &Backend,
    sight: &mut Sight<'_>,
    asked: NewThread,
) -> Result<Thread, Refusal> {
    let (login, reference) = author(backend)?;
    let title = asked.title.trim().to_string();
    if title.is_empty() || title.chars().count() > 200 {
        return Err(Refusal::bad("a thread's title is 1 to 200 characters"));
    }
    let (rendered, tags) = written(sight, &asked.body, &asked.tags).await?;
    if tags.is_empty() {
        return Err(Refusal::bad(
            "tag the thread with at least one resource: choose one in Tags, such as card-gateway",
        ));
    }
    let thread = Thread {
        id: Uuid::now_v7(),
        title,
        author: login.clone(),
        tags: tags.clone(),
        replies: 0,
        last_at: String::new(),
        created_at: String::new(),
        headline: None,
    };
    let first = Message {
        id: Uuid::now_v7(),
        thread: thread.id,
        author: login,
        author_ref: reference,
        body: asked.body.trim().to_string(),
        html: rendered.html,
        text: rendered.text,
        tags,
        created_at: String::new(),
        edited_at: None,
        parent: None,
    };
    let store = Store(backend);
    store.start(&thread, &first).await?;
    let payload = json!({
        "thread": thread.id,
        "title": thread.title,
        "author": thread.author,
        "tags": thread.tags,
        "url": page(&format!("threads/{}", thread.id)),
    });
    announce(backend, "plugin.water.thread.created", payload).await;
    store.thread(thread.id).await?.ok_or_else(|| Refusal::missing("the thread has gone"))
}

pub async fn reply(
    backend: &Backend,
    sight: &mut Sight<'_>,
    id: &str,
    asked: NewReply,
) -> Result<(Thread, Message), Refusal> {
    let (login, reference) = author(backend)?;
    let store = Store(backend);
    let thread = found(&store, id).await?;
    let messages = store.messages(thread.id).await?;
    let parent = match asked.parent {
        None => None,
        Some(parent) if messages.first().is_some_and(|first| first.id == parent) => None,
        Some(parent) if messages.iter().any(|message| message.id == parent) => Some(parent),
        Some(_) => return Err(Refusal::bad("the message it answers is not in this thread")),
    };
    let (rendered, tags) = written(sight, &asked.body, &asked.tags).await?;
    let message = Message {
        id: Uuid::now_v7(),
        thread: thread.id,
        author: login,
        author_ref: reference,
        body: asked.body.trim().to_string(),
        html: rendered.html,
        text: rendered.text,
        tags,
        created_at: String::new(),
        edited_at: None,
        parent,
    };
    store.reply(&message).await?;
    let payload = json!({
        "thread": thread.id,
        "reply": message.id,
        "parent": message.parent,
        "title": thread.title,
        "author": message.author,
        "tags": message.tags,
        "url": page(&format!("threads/{}#m-{}", thread.id, message.id)),
    });
    announce(backend, "plugin.water.reply.created", payload).await;
    let thread = store.thread(thread.id).await?.ok_or_else(|| Refusal::missing("it has gone"))?;
    Ok((thread, message))
}

/// A tag page's resource, and the tags it gathers: its own, and for an organisation each resource in it.
pub async fn gathered(sight: &mut Sight<'_>, tag: &str) -> Result<(Value, Vec<String>), Refusal> {
    let answer = sight.resource(tag).await.map_err(|status| match status {
        400 | 403 | 404 | 409 => {
            Refusal::missing(format!("there is no {tag}, or you cannot see it"))
        }
        _ => Refusal::unavailable("Resource Definitions could not be asked"),
    })?;
    let mut tags = vec![tag.to_string()];
    if tag.starts_with("organisation:") {
        for connection in answer["connections"].as_array().into_iter().flatten() {
            let (Some(kind), Some(name)) =
                (connection["kind"].as_str(), connection["name"].as_str())
            else {
                continue;
            };
            if connection["direction"] == "out"
                && let Some(inner) = markdown::tag_of(&format!("{kind}:{name}"))
                && !tags.contains(&inner)
            {
                tags.push(inner);
            }
        }
    }
    Ok((answer["resource"].clone(), tags))
}

/// A tag from a path or a form: `kind:name` or `/kind:name`.
pub fn tag_in(text: &str) -> Result<String, Refusal> {
    markdown::tag_of(text.trim().trim_start_matches('/')).ok_or_else(|| {
        Refusal::bad(format!("`{text}` is not a resource or a plugin, written kind:name"))
    })
}

fn limit_of(request: &Request) -> Result<i64, Refusal> {
    let limit = query(request, "limit").map_or(Ok(50), |text| text.parse::<i64>());
    match limit {
        Ok(limit) if (1..=100).contains(&limit) => Ok(limit),
        _ => Err(Refusal::bad("a limit is 1 to 100")),
    }
}

pub async fn handle(backend: &Backend, request: Request) -> Response {
    let path = request.path.trim_end_matches('/').to_string();
    let segments: Vec<&str> = path.split('/').collect();
    let answer = match segments.as_slice() {
        ["ui", route @ ..] => return crate::ui::handle(backend, &request, route).await,
        ["api", route @ ..] => api(backend, &request, route).await,
        _ => Err(Refusal::missing("no such route")),
    };
    match answer {
        Ok((204, _)) => Response::new(204, "application/json", Vec::new()),
        Ok((status, value)) => Response::new(
            status,
            "application/json",
            serde_json::to_vec(&value).unwrap_or_default(),
        ),
        Err(refusal) => refusal.response(),
    }
}

async fn api(backend: &Backend, request: &Request, path: &[&str]) -> Answer {
    let store = Store(backend);
    let mut sight = Sight::new(backend);
    match (request.method.as_str(), path) {
        ("GET", ["threads"]) => {
            let limit = limit_of(request)?;
            let tag = query(request, "tag").map(|tag| tag_in(&tag)).transpose()?;
            let threads = match (query(request, "q"), &tag) {
                (Some(text), tag) => store.search(&text, tag.as_deref(), limit).await?,
                (None, Some(tag)) => store.tagged(std::slice::from_ref(tag), limit).await?,
                (None, None) => store.recent(limit).await?,
            };
            Ok((200, json!(threads.iter().map(thread_shown).collect::<Vec<_>>())))
        }
        ("POST", ["threads"]) => {
            let thread = start(backend, &mut sight, body(request)?).await?;
            Ok((201, thread_shown(&thread)))
        }
        ("GET", ["threads", id]) => {
            let thread = found(&store, id).await?;
            let messages = store.messages(thread.id).await?;
            let likes = store.likes(thread.id).await?;
            let mut shown = thread_shown(&thread);
            shown["messages"] = json!(
                messages
                    .iter()
                    .map(|message| {
                        let mut shown = message_shown(message);
                        let by: Vec<&str> = likes
                            .iter()
                            .filter(|like| like.message == message.id)
                            .map(|like| like.name.as_str())
                            .collect();
                        shown["likes"] = json!(by.len());
                        shown["liked_by"] = json!(by);
                        shown
                    })
                    .collect::<Vec<_>>()
            );
            Ok((200, shown))
        }
        ("DELETE", ["threads", id]) => {
            remove_thread(backend, id).await?;
            Ok((204, Value::Null))
        }
        ("DELETE", ["threads", id, "messages", message]) => {
            remove(backend, id, message).await?;
            Ok((204, Value::Null))
        }
        ("PUT" | "DELETE", ["threads", id, "messages", message, "like"]) => {
            let (likes, liked) = like(backend, id, message, request.method == "PUT").await?;
            Ok((200, json!({ "likes": likes, "liked": liked })))
        }
        ("PUT", ["threads", id, "messages", message]) => {
            let (_, edited) = edit(backend, &mut sight, id, message, body(request)?).await?;
            Ok((200, message_shown(&edited)))
        }
        ("POST", ["threads", id, "replies"]) => {
            let (thread, message) = reply(backend, &mut sight, id, body(request)?).await?;
            Ok((201, json!({ "thread": thread_shown(&thread), "reply": message_shown(&message) })))
        }
        ("GET", ["events"]) => {
            let listed = store.events().await?;
            Ok((200, json!(listed.iter().map(events::event_shown).collect::<Vec<_>>())))
        }
        ("POST", ["events"]) => {
            let event = events::create(backend, &mut sight, body(request)?).await?;
            Ok((201, events::event_shown(&event)))
        }
        ("GET", ["events", id]) => {
            let event = events::found(&store, id).await?;
            let mut shown = events::event_shown(&event);
            let registrations = store.registrations(event.id).await?;
            shown["registrations"] = events::registrations_shown(&registrations);
            shown["submissions"] = json!(store.submissions(event.id).await?);
            let entries = store.entries(event.id).await?;
            let board = events::leaderboard(&event, &entries);
            shown["leaderboard"] = json!(board.iter().map(|(entrant, score, count)| json!({ "entrant": entrant, "best": score, "entries": count })).collect::<Vec<_>>());
            if event.kind == "game-night" {
                let answers = events::answers(backend, &event).await;
                shown["rsvps"] = json!(
                    answers
                        .iter()
                        .map(|(who, response)| json!({ "who": who, "response": response }))
                        .collect::<Vec<_>>()
                );
            }
            Ok((200, shown))
        }
        ("POST", ["events", id, "registrations"]) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Registering {
                team: String,
            }
            let asked: Registering = body(request)?;
            let (event, added) = events::register(backend, &mut sight, id, &asked.team).await?;
            Ok((if added { 201 } else { 200 }, events::event_shown(&event)))
        }
        ("POST", ["events", id, "submissions"]) => {
            let event = events::submit(backend, &mut sight, id, body(request)?).await?;
            Ok((200, events::event_shown(&event)))
        }
        ("POST", ["events", id, "winners"]) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Crowning {
                winners: Vec<events::Winner>,
            }
            let asked: Crowning = body(request)?;
            let event = events::crown(backend, &mut sight, id, asked.winners).await?;
            Ok((200, events::event_shown(&event)))
        }
        ("POST", ["events", id, "entries"]) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Entering {
                score: f64,
                #[serde(default)]
                note: String,
            }
            let asked: Entering = body(request)?;
            let event = events::enter(backend, id, asked.score, &asked.note).await?;
            Ok((201, events::event_shown(&event)))
        }
        ("POST", ["events", id, "rsvp"]) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Answering {
                response: String,
            }
            let asked: Answering = body(request)?;
            let event = events::rsvp(backend, id, &asked.response).await?;
            Ok((200, events::event_shown(&event)))
        }
        ("POST", ["events", id, "close"]) => {
            let event = events::close(backend, id).await?;
            Ok((200, events::event_shown(&event)))
        }
        ("DELETE", ["events", id]) => {
            events::cancel(backend, id).await?;
            Ok((204, Value::Null))
        }
        ("GET", ["cards"]) => {
            let me = cards::login(backend)?;
            let listed: Vec<Value> = store
                .cards()
                .await?
                .iter()
                .filter(|card| !cards::hidden_from(card, &me))
                .map(cards::card_shown)
                .collect();
            Ok((200, json!(listed)))
        }
        ("POST", ["cards"]) => {
            let card = cards::make_card(backend, &mut sight, body(request)?).await?;
            Ok((201, cards::card_shown(&card)))
        }
        ("GET", ["cards", id]) => {
            let card = cards::found_card(&store, &cards::login(backend)?, id).await?;
            let mut shown = cards::card_shown(&card);
            shown["signed"] = json!(store.signatures(card.id).await?);
            Ok((200, shown))
        }
        ("POST", ["cards", id, "signatures"]) => {
            #[derive(Deserialize)]
            #[serde(deny_unknown_fields)]
            struct Signing {
                message: String,
            }
            let asked: Signing = body(request)?;
            let card = cards::sign(backend, id, &asked.message).await?;
            Ok((200, cards::card_shown(&card)))
        }
        ("DELETE", ["cards", id]) => {
            cards::delete_card(backend, id).await?;
            Ok((204, Value::Null))
        }
        ("GET", ["kudos"]) => {
            let to = query(request, "to");
            let wanted = to.as_deref().map(|to| match to.strip_prefix("team:") {
                Some(team) => ("team", team),
                None => ("user", to.trim_start_matches("user:")),
            });
            let listed = store.kudos(wanted, limit_of(request)?).await?;
            Ok((200, json!(listed.iter().map(cards::kudos_shown).collect::<Vec<_>>())))
        }
        ("POST", ["kudos"]) => {
            let kudos = cards::give(backend, &mut sight, body(request)?).await?;
            Ok((201, cards::kudos_shown(&kudos)))
        }
        ("GET", ["tags", kind, name @ ..]) if !name.is_empty() => {
            let tag = tag_in(&format!("{kind}:{}", name.join("/")))?;
            let (resource, tags) = gathered(&mut sight, &tag).await?;
            let threads = store.tagged(&tags, limit_of(request)?).await?;
            let shown: Vec<Value> = threads.iter().map(thread_shown).collect();
            Ok((200, json!({ "tag": tag, "resource": resource, "tags": tags, "threads": shown })))
        }
        _ => Err(Refusal::missing("no such route")),
    }
}
