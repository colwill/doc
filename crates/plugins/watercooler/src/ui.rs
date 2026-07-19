//! The plugin's pages at `/p/water/...`: recent threads and search, starting a thread, threads with
//! their replies, a tag page for every resource, the options `/` suggests while typing and the tag
//! box offers, and a panel for resource pages.

use askama::Template;
use chrono::DateTime;
use doc_plugin_sdk::{Backend, Request, Response};
use serde_json::Value;

use crate::Refusal;
use crate::api::{self, NewReply, NewThread, Sight, query};
use crate::markdown;
use std::collections::HashMap;

use uuid::Uuid;

use crate::store::{Like, Message, Store, Thread};

/// The kinds `/` and the tag box suggest: the ones people talk about.
const SUGGESTED: &str = "organisation,service,repository,team,documentation,cloudresource";

#[derive(Default)]
pub struct Flash {
    pub notice: Option<String>,
    pub error: Option<String>,
}

impl Flash {
    pub fn done(notice: impl Into<String>) -> Self {
        Self { notice: Some(notice.into()), error: None }
    }

    pub fn refused(detail: impl Into<String>) -> Self {
        Self { notice: None, error: Some(detail.into()) }
    }
}

pub type Form = Vec<(String, String)>;

pub fn form(request: &Request) -> Form {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

pub fn field(form: &Form, name: &str) -> Option<String> {
    form.iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

pub fn render<T: Template>(page: &T) -> Result<String, Refusal> {
    page.render().map_err(|err| Refusal::unavailable(format!("the page could not be drawn: {err}")))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn when(text: &str) -> String {
    DateTime::parse_from_rfc3339(text)
        .map_or_else(|_| text.to_string(), |at| at.format("%-d %b %Y, %H:%M UTC").to_string())
}

/// A search headline as HTML: escaped, with each match in bold.
fn highlighted(headline: &str) -> String {
    let escaped = escape(headline);
    let (mut html, mut rest, mut open) = (String::new(), escaped.as_str(), false);
    while let Some(at) = rest.find(if open { "]]" } else { "[[" }) {
        html.push_str(&rest[..at]);
        html.push_str(if open { "</strong>" } else { "<strong>" });
        open = !open;
        rest = &rest[at + 2..];
    }
    html.push_str(rest);
    if open {
        html.push_str("</strong>");
    }
    html
}

/// A tag as a label: `service:card-gateway` is card-gateway, a Service, linking to its page, in
/// the Service colour.
pub struct Tag {
    pub value: String,
    pub label: String,
    pub kind: String,
    /// The `doc-kind--*` modifier that colours it.
    pub css: String,
    pub href: String,
}

/// The `doc-kind--*` modifier for a kind as a tag writes it: `cloudresource` is cloud-resource.
pub fn kind_css(kind: &str) -> &str {
    match kind {
        "cloudresource" => "cloud-resource",
        "serviceaccount" => "service-account",
        other => other,
    }
}

pub fn tag(value: &str) -> Tag {
    let (kind, name) = value.split_once(':').unwrap_or(("", value));
    Tag {
        value: value.to_string(),
        label: name.to_string(),
        kind: markdown::kind_name(kind).to_string(),
        css: kind_css(kind).to_string(),
        href: markdown::tag_page(value),
    }
}

pub fn tags(list: &[String]) -> Vec<Tag> {
    list.iter().map(|value| tag(value)).collect()
}

/// The tags a form's tag box sent.
pub fn tags_in(form: &Form) -> Vec<String> {
    form.iter()
        .filter(|(key, value)| key == "tags" && !value.trim().is_empty())
        .map(|(_, value)| value.trim().to_string())
        .collect()
}

/// A tag written alone, as the search's tag box sends one, as the one badge it shows.
fn picked(tag: &str) -> Vec<Tag> {
    match tag.is_empty() {
        true => Vec::new(),
        false => vec![self::tag(tag)],
    }
}

/// A thread in a list.
pub struct Row {
    pub href: String,
    pub title: String,
    pub tags: Vec<Tag>,
    pub author: String,
    pub replies: i64,
    pub last: String,
    pub headline: String,
}

fn rows(threads: &[Thread]) -> Vec<Row> {
    threads
        .iter()
        .map(|thread| Row {
            href: format!("/p/water/threads/{}", thread.id),
            title: thread.title.clone(),
            tags: tags(&thread.tags),
            author: thread.author.clone(),
            replies: thread.replies,
            last: when(&thread.last_at),
            headline: thread.headline.as_deref().map(highlighted).unwrap_or_default(),
        })
        .collect()
}

/// How deep replies are indented; deeper ones sit at this depth and say whom they answer.
const MAX_DEPTH: usize = 6;

pub struct Post {
    pub id: String,
    pub author: String,
    pub when: String,
    pub html: String,
    /// When its author last changed it.
    pub edited: Option<String>,
    /// Whether the viewer wrote it, and so may change it.
    pub editable: bool,
    /// Whether the viewer moderates, and so may delete it.
    pub deletable: bool,
    pub first: bool,
    pub likes: usize,
    pub liked: bool,
    /// Who likes it, for the button's title.
    pub likers: String,
    /// Groups of replies to close before it, and to open for it, as the conversation nests.
    pub closes: usize,
    pub opens: usize,
    /// Whom it answers, said when it is deeper than the page indents.
    pub answers: Option<String>,
}

/// A message being changed by whoever wrote it, in place of the message on the thread's page.
pub struct Editing {
    pub id: String,
    /// The thread's title, when this is its first message, which carries it.
    pub title: Option<String>,
    pub body: String,
    /// The tags chosen beside the words, rather than written in them.
    pub picked: Vec<Tag>,
}

pub struct Choice {
    pub value: String,
    pub name: String,
    pub hint: String,
    /// The kind alone, for a label.
    pub kind: String,
    /// The `doc-kind--*` modifier that colours its label.
    pub css: String,
}

#[derive(Template)]
#[template(path = "blank.html")]
struct Blank {
    flash: Flash,
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    flash: Flash,
    writes: bool,
    threads: Vec<Row>,
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchPage {
    flash: Flash,
    q: String,
    tag: String,
    picked: Vec<Tag>,
    threads: Vec<Row>,
    asked: bool,
}

#[derive(Template)]
#[template(path = "compose.html")]
struct ComposePage {
    flash: Flash,
    writes: bool,
    title: String,
    body: String,
    picked: Vec<Tag>,
}

#[derive(Template)]
#[template(path = "thread.html")]
struct ThreadPage {
    flash: Flash,
    writes: bool,
    id: String,
    title: String,
    tags: Vec<Tag>,
    /// The opening post, alone, then the replies in conversation order.
    opening: Vec<Post>,
    replies: Vec<Post>,
    /// Groups of replies still open after the last.
    trailing: usize,
    /// The message being changed, if one is; empty otherwise.
    editing_id: String,
    editing: Option<Editing>,
    /// The message the reply form answers, when it is not the thread; empty otherwise.
    replying: String,
    body: String,
    picked: Vec<Tag>,
}

#[derive(Template)]
#[template(path = "like.html")]
struct LikeFragment {
    id: String,
    writes: bool,
    post: Post,
}

#[derive(Template)]
#[template(path = "tag.html")]
struct TagPage {
    flash: Flash,
    writes: bool,
    q: String,
    tag: String,
    picked: Vec<Tag>,
    title: String,
    kind: String,
    /// The `doc-kind--*` modifier for its kind.
    css: String,
    resource_href: String,
    gathered: Vec<Tag>,
    threads: Vec<Row>,
}

#[derive(Template)]
#[template(path = "suggest.html")]
struct SuggestFragment {
    choices: Vec<Choice>,
    note: String,
}

#[derive(Template)]
#[template(path = "tag_options.html")]
struct TagOptions {
    choices: Vec<Choice>,
    note: String,
}

#[derive(Template)]
#[template(path = "panel.html")]
struct PanelFragment {
    writes: bool,
    tag: String,
    page: String,
    threads: Vec<Row>,
}

/// The discussions a person is tagged in, newest first, on their dashboard.
#[derive(Template)]
#[template(path = "dashboard.html")]
struct TaggedFragment {
    page: String,
    threads: Vec<Row>,
}

pub async fn handle(backend: &Backend, request: &Request, path: &[&str]) -> Response {
    let fragment = matches!(
        path,
        ["panel" | "suggest" | "tag-options"]
            | ["dashboard", ..]
            | ["kudos", "panel" | "dashboard"]
            | ["threads", _, "messages", _, "like"]
    );
    let mut moved = None;
    match route(backend, request, path, &mut moved).await {
        Ok(html) => match moved {
            Some(url) => Response::html(html).with_header("hx-push-url", &url),
            None => Response::html(html),
        },
        Err(refusal) if fragment => {
            let text = format!(
                "<p class=\"doc-error-message\" role=\"alert\">{}</p>",
                escape(&refusal.detail)
            );
            Response::new(refusal.status, "text/html; charset=utf-8", text)
        }
        Err(refusal) => {
            let page = Blank { flash: Flash::refused(refusal.detail.clone()) };
            let html = page.render().unwrap_or_else(|_| escape(&refusal.detail));
            Response::new(refusal.status, "text/html; charset=utf-8", html)
        }
    }
}

/// `moved` is set to the address a page should be shown at when it is not the one asked for.
async fn route(
    backend: &Backend,
    request: &Request,
    path: &[&str],
    moved: &mut Option<String>,
) -> Result<String, Refusal> {
    let store = Store(backend);
    let mut sight = Sight::new(backend);
    match (request.method.as_str(), path) {
        ("GET", [] | [""]) => home(backend, Flash::default()).await,
        ("GET", ["search"]) => search(backend, request).await,
        ("GET", ["threads", "new"]) => {
            let tag: Vec<String> =
                query(request, "tag").and_then(|tag| api::tag_in(&tag).ok()).into_iter().collect();
            compose(backend, String::new(), String::new(), &tag, Flash::default())
        }
        ("POST", ["threads"]) => {
            let form = form(request);
            let asked = NewThread {
                title: field(&form, "title").unwrap_or_default(),
                body: field(&form, "body").unwrap_or_default(),
                tags: tags_in(&form),
            };
            let (title, body, chosen) =
                (asked.title.clone(), asked.body.clone(), asked.tags.clone());
            match api::start(backend, &mut sight, asked).await {
                Ok(thread) => {
                    *moved = Some(format!("/p/water/threads/{}", thread.id));
                    thread_page(
                        backend,
                        &thread.id.to_string(),
                        String::new(),
                        &[],
                        None,
                        None,
                        Flash::done("Started."),
                    )
                    .await
                }
                Err(refusal) if refusal.status >= 500 => Err(refusal),
                Err(refusal) => {
                    compose(backend, title, body, &chosen, Flash::refused(refusal.detail))
                }
            }
        }
        ("GET", ["threads", id]) => {
            let replying = query(request, "reply");
            thread_page(backend, id, String::new(), &[], None, replying, Flash::default()).await
        }
        ("GET", ["threads", id, "messages", message, "edit"]) => {
            let editing = editing(backend, id, message).await?;
            thread_page(backend, id, String::new(), &[], Some(editing), None, Flash::default())
                .await
        }
        ("POST", ["threads", id, "messages", message, "like"]) => {
            like_button(backend, id, message).await
        }
        ("POST", ["threads", id, "messages", message, "delete"]) => {
            match api::remove(backend, id, message).await? {
                api::Removed::Thread(title) => {
                    *moved = Some("/p/water/".into());
                    home(backend, Flash::done(format!("Deleted the discussion {title}."))).await
                }
                api::Removed::Message(author) => {
                    let done = Flash::done(format!("Deleted {author}'s reply."));
                    thread_page(backend, id, String::new(), &[], None, None, done).await
                }
            }
        }
        ("POST", ["threads", id, "messages", message]) => {
            let form = form(request);
            let asked = api::Edit {
                body: field(&form, "body").unwrap_or_default(),
                tags: tags_in(&form),
                title: form
                    .iter()
                    .any(|(key, _)| key == "title")
                    .then(|| field(&form, "title").unwrap_or_default()),
            };
            let again = Editing {
                id: (*message).to_string(),
                title: asked.title.clone(),
                body: asked.body.clone(),
                picked: tags(&asked.tags),
            };
            match api::edit(backend, &mut sight, id, message, asked).await {
                Ok((thread, _)) => {
                    *moved = Some(format!("/p/water/threads/{}", thread.id));
                    thread_page(backend, id, String::new(), &[], None, None, Flash::done("Saved."))
                        .await
                }
                Err(refusal) if refusal.status >= 500 || matches!(refusal.status, 403 | 404) => {
                    Err(refusal)
                }
                Err(refusal) => {
                    let flash = Flash::refused(refusal.detail);
                    thread_page(backend, id, String::new(), &[], Some(again), None, flash).await
                }
            }
        }
        ("POST", ["threads", id, "replies"]) => {
            let form = form(request);
            let text = field(&form, "body").unwrap_or_default();
            let chosen = tags_in(&form);
            let answering = field(&form, "parent");
            let parent = answering.as_deref().and_then(|parent| parent.parse().ok());
            let asked = NewReply { body: text.clone(), tags: chosen.clone(), parent };
            match api::reply(backend, &mut sight, id, asked).await {
                Ok(_) => {
                    let done = Flash::done("Replied.");
                    thread_page(backend, id, String::new(), &[], None, None, done).await
                }
                Err(refusal) if refusal.status >= 500 || refusal.status == 404 => Err(refusal),
                Err(refusal) => {
                    let flash = Flash::refused(refusal.detail);
                    thread_page(backend, id, text, &chosen, None, answering, flash).await
                }
            }
        }
        ("GET", ["tags", kind, name @ ..]) if !name.is_empty() => {
            let tag = api::tag_in(&format!("{kind}:{}", name.join("/")))?;
            let (resource, gathered) = api::gathered(&mut sight, &tag).await?;
            let threads = store.tagged(&gathered, 100).await?;
            let text = |key: &str| resource[key].as_str().unwrap_or_default().to_string();
            let (kind, name) = tag.split_once(':').unwrap_or_default();
            let title = match text("title") {
                title if title.is_empty() => text("name"),
                title => title,
            };
            // A plugin is not in the Catalogue: its tag leads to the plugin itself.
            let resource_href = match kind {
                "plugin" => api::plugins(backend)
                    .await?
                    .into_iter()
                    .find(|plugin| plugin.id == name)
                    .map_or_else(|| format!("/plugins/{name}"), |plugin| plugin.href()),
                _ => format!("/p/resources/r/{kind}/{name}"),
            };
            render(&TagPage {
                flash: Flash::default(),
                writes: backend.writes(),
                q: String::new(),
                title,
                kind: text("kind"),
                css: kind_css(kind).to_string(),
                resource_href,
                gathered: tags(&gathered[1..]),
                threads: rows(&threads),
                picked: picked(&tag),
                tag,
            })
        }
        (_, ["events", rest @ ..]) => {
            crate::event_pages::route(backend, request, rest, moved).await
        }
        (_, ["cards", rest @ ..]) => crate::card_pages::cards(backend, request, rest, moved).await,
        (_, ["kudos", rest @ ..]) => crate::card_pages::kudos(backend, request, rest).await,
        ("GET", ["suggest"]) => suggest(backend, request).await,
        ("GET", ["tag-options"]) => tag_options(backend, request).await,
        ("GET", ["dashboard", "tagged"]) => {
            let login =
                backend.caller().and_then(|caller| caller.label.clone()).unwrap_or_default();
            if login.is_empty() {
                return Err(Refusal::forbidden("a dashboard is a person's"));
            }
            let tag = format!("user:{login}");
            let threads = store.tagged(std::slice::from_ref(&tag), 5).await?;
            let page = markdown::tag_page(&tag);
            render(&TaggedFragment { page, threads: rows(&threads) })
        }
        ("GET", ["panel"]) => {
            let asked =
                query(request, "resource").ok_or_else(|| Refusal::bad("name the resource"))?;
            let tag = api::tag_in(&asked)?;
            let (_, gathered) = api::gathered(&mut sight, &tag).await?;
            let threads = store.tagged(&gathered, 5).await?;
            let page = markdown::tag_page(&tag);
            render(&PanelFragment { writes: backend.writes(), tag, page, threads: rows(&threads) })
        }
        _ => Err(Refusal::missing("no such page")),
    }
}

async fn home(backend: &Backend, flash: Flash) -> Result<String, Refusal> {
    let threads = Store(backend).recent(30).await?;
    render(&HomePage { flash, writes: backend.writes(), threads: rows(&threads) })
}

async fn search(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let text = query(request, "q").unwrap_or_default();
    let asked_tag = query(request, "tag").unwrap_or_default();
    let tag = match asked_tag.is_empty() {
        true => None,
        false => Some(api::tag_in(&asked_tag)?),
    };
    let store = Store(backend);
    let threads = match (text.is_empty(), &tag) {
        (false, tag) => store.search(&text, tag.as_deref(), 50).await?,
        (true, Some(tag)) => store.tagged(std::slice::from_ref(tag), 50).await?,
        (true, None) => Vec::new(),
    };
    render(&SearchPage {
        flash: Flash::default(),
        asked: !text.is_empty() || tag.is_some(),
        q: text,
        picked: picked(tag.as_deref().unwrap_or_default()),
        tag: tag.unwrap_or_default(),
        threads: rows(&threads),
    })
}

fn compose(
    backend: &Backend,
    title: String,
    body: String,
    chosen: &[String],
    flash: Flash,
) -> Result<String, Refusal> {
    render(&ComposePage { flash, writes: backend.writes(), title, body, picked: tags(chosen) })
}

/// A thread's messages as a conversation: the opening post, then each reply under what it
/// answers, oldest first among replies to the same message.
fn conversation(
    backend: &Backend,
    messages: &[Message],
    likes: &[Like],
) -> (Vec<Post>, Vec<Post>, usize) {
    let writes = backend.writes();
    let moderates = api::moderates(backend);
    let me = api::me(backend);
    let names: HashMap<Uuid, String> =
        messages.iter().map(|message| (message.id, message.author.clone())).collect();
    let post = |message: &Message, first: bool| {
        let liking: Vec<&Like> = likes.iter().filter(|like| like.message == message.id).collect();
        Post {
            id: message.id.to_string(),
            author: message.author.clone(),
            when: when(&message.created_at),
            html: message.html.clone(),
            edited: message.edited_at.as_deref().map(when),
            editable: writes && api::wrote(backend, message),
            deletable: moderates,
            first,
            likes: liking.len(),
            liked: me.as_ref().is_some_and(|me| liking.iter().any(|like| &like.by == me)),
            likers: liking.iter().map(|like| like.name.as_str()).collect::<Vec<_>>().join(", "),
            closes: 0,
            opens: 0,
            answers: None,
        }
    };
    let Some(opening) = messages.first() else { return (Vec::new(), Vec::new(), 0) };
    // What each reply answers; the opening post, or a message that is gone, is the thread.
    let answered = |message: &Message| {
        message.parent.filter(|parent| *parent != opening.id && names.contains_key(parent))
    };
    let mut children: HashMap<Option<Uuid>, Vec<&Message>> = HashMap::new();
    for message in messages.iter().skip(1) {
        children.entry(answered(message)).or_default().push(message);
    }
    let mut ordered: Vec<(&Message, usize)> = Vec::new();
    let mut stack: Vec<(&Message, usize)> =
        children.get(&None).into_iter().flatten().rev().map(|message| (*message, 1)).collect();
    while let Some((message, depth)) = stack.pop() {
        ordered.push((message, depth));
        for child in children.get(&Some(message.id)).into_iter().flatten().rev() {
            stack.push((child, depth + 1));
        }
    }
    let mut open = 0;
    let replies = ordered
        .into_iter()
        .map(|(message, depth)| {
            let mut shown = post(message, false);
            let groups = depth.min(MAX_DEPTH) - 1;
            shown.opens = groups.saturating_sub(open);
            shown.closes = open.saturating_sub(groups);
            open = groups;
            if depth > MAX_DEPTH {
                shown.answers = message.parent.and_then(|parent| names.get(&parent).cloned());
            }
            shown
        })
        .collect();
    (vec![post(opening, true)], replies, open)
}

async fn thread_page(
    backend: &Backend,
    id: &str,
    body: String,
    chosen: &[String],
    editing: Option<Editing>,
    replying: Option<String>,
    flash: Flash,
) -> Result<String, Refusal> {
    let store = Store(backend);
    let thread = api::found(&store, id).await?;
    let messages = store.messages(thread.id).await?;
    let likes = store.likes(thread.id).await?;
    let replying = replying
        .filter(|asked| messages.iter().skip(1).any(|message| &message.id.to_string() == asked))
        .unwrap_or_default();
    let (opening, replies, trailing) = conversation(backend, &messages, &likes);
    render(&ThreadPage {
        flash,
        writes: backend.writes(),
        id: thread.id.to_string(),
        title: thread.title.clone(),
        tags: tags(&thread.tags),
        opening,
        replies,
        trailing,
        editing_id: editing.as_ref().map(|editing| editing.id.clone()).unwrap_or_default(),
        editing,
        replying,
        body,
        picked: tags(chosen),
    })
}

/// A message's like button, drawn again after it is pressed.
async fn like_button(backend: &Backend, id: &str, message_id: &str) -> Result<String, Refusal> {
    let store = Store(backend);
    let thread = api::found(&store, id).await?;
    let (message, _) = api::message_of(&store, &thread, message_id).await?;
    let me = api::me(backend)
        .ok_or_else(|| Refusal::forbidden("liking is for people and service accounts"))?;
    let liked = store
        .likes(thread.id)
        .await?
        .iter()
        .any(|like| like.message == message.id && like.by == me);
    api::like(backend, id, message_id, !liked).await?;
    let likes = store.likes(thread.id).await?;
    let (mut opening, mut replies, _) =
        conversation(backend, &store.messages(thread.id).await?, &likes);
    let post = opening
        .drain(..)
        .chain(replies.drain(..))
        .find(|post| post.id == message.id.to_string())
        .ok_or_else(|| Refusal::missing("the message has gone"))?;
    render(&LikeFragment { id: thread.id.to_string(), writes: backend.writes(), post })
}

/// The form for changing a message, filled from it: the tags it was given beside its words are in
/// the tag box, and those written in its words stay there.
async fn editing(backend: &Backend, id: &str, message_id: &str) -> Result<Editing, Refusal> {
    let store = Store(backend);
    let thread = api::found(&store, id).await?;
    let (message, first) = api::message_of(&store, &thread, message_id).await?;
    if !api::wrote(backend, &message) {
        return Err(Refusal::forbidden("only whoever wrote a message can change it"));
    }
    let written = markdown::render(&message.body).tags;
    let beside: Vec<String> =
        message.tags.iter().filter(|tag| !written.contains(tag)).cloned().collect();
    Ok(Editing {
        id: message.id.to_string(),
        title: first.then(|| thread.title.clone()),
        body: message.body,
        picked: tags(&beside),
    })
}

/// What Resource Definitions finds for `words` among the kinds people talk about, best first, as
/// options whose value is the tag; with no words, the first of everything.
/// What a tag box or a `/` offers for what was typed: the platform's plugins whose ID or name
/// holds it, then the Catalogue's resources, `limit` in all.
async fn choices(backend: &Backend, words: &str, limit: usize) -> Result<Vec<Choice>, Refusal> {
    let wanted = words.trim().to_lowercase();
    let mut offered: Vec<Choice> = match wanted.is_empty() {
        // Before anything is typed, what to start from is the Catalogue's.
        true => Vec::new(),
        false => api::plugins(backend)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|plugin| {
                plugin.id.contains(&wanted) || plugin.title.to_lowercase().contains(&wanted)
            })
            .take(limit / 2)
            .map(|plugin| {
                let value = format!("plugin:{}", plugin.id);
                let hint = match plugin.title == plugin.id {
                    true => "Plugin".to_string(),
                    false => format!("Plugin · {}", plugin.title),
                };
                Choice { value, name: plugin.id, hint, kind: "Plugin".into(), css: "plugin".into() }
            })
            .collect(),
    };
    let rest = limit - offered.len();
    offered.extend(resources(backend, words, rest).await?);
    Ok(offered)
}

/// The Catalogue's resources for what was typed.
async fn resources(backend: &Backend, words: &str, limit: usize) -> Result<Vec<Choice>, Refusal> {
    let asked: String = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", words)
        .append_pair("kinds", SUGGESTED)
        .append_pair("limit", &limit.to_string())
        .finish();
    let found = match backend.ask("resources", "GET", "search", Some(&asked), None).await {
        Ok((200, found)) => found,
        Ok((status, answer)) => {
            let detail = answer["detail"].as_str().unwrap_or("no reason given").to_string();
            return Err(Refusal { status, detail });
        }
        Err(err) => return Err(Refusal::unavailable(format!("Resource Definitions: {err}"))),
    };
    Ok(found
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|resource: &Value| {
            let kind = resource["kind"].as_str()?;
            let name = resource["name"].as_str()?;
            let value = markdown::tag_of(&format!("{kind}:{name}"))?;
            let tagged = tag(&value);
            let shown = tagged.kind;
            let title =
                resource["title"].as_str().filter(|title| !title.is_empty() && *title != name);
            let hint = title.map_or_else(|| shown.clone(), |title| format!("{shown} · {title}"));
            Some(Choice { value, name: name.to_string(), hint, kind: shown, css: tagged.css })
        })
        .collect())
}

/// The resources Resource Definitions finds for what follows a `/`, as options to choose.
async fn suggest(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let typed = query(request, "typed").unwrap_or_default();
    let words = typed.split_once(':').map_or(typed.as_str(), |(_, name)| name).trim();
    if words.chars().count() < 2 {
        return render(&SuggestFragment { choices: Vec::new(), note: String::new() });
    }
    let mut choices = choices(backend, words, 8).await?;
    for choice in &mut choices {
        choice.value = format!("/{}", choice.value);
    }
    let note = match choices.is_empty() {
        true => format!("Nothing is called {typed}"),
        false => String::new(),
    };
    render(&SuggestFragment { choices, note })
}

/// What the tag box offers: resources that match what is typed, however loosely, or before
/// anything is typed, some to start from. Those already chosen are hidden by the box itself.
async fn tag_options(backend: &Backend, request: &Request) -> Result<String, Refusal> {
    let typed = query(request, "typed").unwrap_or_default();
    let choices = choices(backend, &typed, 12).await?;
    let note = match (choices.is_empty(), typed.is_empty()) {
        (false, _) => String::new(),
        (true, true) => "There is nothing to tag yet".to_string(),
        (true, false) => format!("Nothing matches {typed}"),
    };
    render(&TagOptions { choices, note })
}
