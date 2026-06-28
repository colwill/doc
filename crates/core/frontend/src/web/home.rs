//! The landing page and the search behind its box. The landing page is the person's dashboard: what
//! they chose to see at a glance (`dashboard`), and search looks through the pages' names and
//! descriptions and, when the Knowledge Base is running and the person can read it, its documents.

use askama::Template;
use axum::extract::{Extension, Query, State};
use axum::response::Html;
use serde::Deserialize;
use serde_json::Value;

use super::AppState;
use super::csrf::Csrf;
use super::error::WebError;
use super::navigation::{Card, cards};
use super::pages::Chrome;
use crate::session::{self, Signed};

/// The Knowledge Base's plugin ID; its documents are the other half of a search.
const KB: &str = "kb";
const WATER: &str = "water";
const CATALOGUE: &str = "resources";
/// Documents asked for in one search.
const DOCUMENTS: usize = 10;

#[derive(Template)]
#[template(path = "home.html")]
pub struct Home {
    pub chrome: Chrome,
    /// What the person's dashboard shows, in order.
    pub items: Vec<super::dashboard::Offer>,
    /// Whether they chose it, or it is what everyone starts with.
    pub chosen: bool,
    /// For an administrator, while DOC has not been set up.
    pub setup: Option<super::setup::Reminder>,
}

pub async fn home(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
) -> Result<Html<String>, WebError> {
    let access = session::access(&state, &signed).await;
    let chrome =
        Chrome::new("DOC", "/").bare().signed(&signed, &csrf).with_plugins(&state, &signed).await;
    let (items, chosen) = super::dashboard::shown(&state, &signed, &access).await;
    let setup = match access.admin {
        true => super::setup::reminder(&state, &signed).await,
        false => None,
    };
    Ok(Html(Home { chrome, items, chosen, setup }.render()?))
}

/// One stretch of a document's snippet: the words that matched are marked.
pub struct Stretch {
    pub text: String,
    pub matched: bool,
}

/// A document as the search lists it.
pub struct Document {
    pub href: String,
    pub title: String,
    pub space: String,
    pub snippet: Vec<Stretch>,
}

/// Something in the Catalogue: a service, a repository, a team, whatever it keeps.
pub struct Resource {
    pub href: String,
    pub title: String,
    pub kind: String,
}

/// What the Catalogue finds, so the search in the header reaches it as it reaches documents.
async fn resources(state: &AppState, signed: &Signed, text: &str) -> Vec<Resource> {
    let asked: String = url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
    let route = format!("search?q={asked}&limit={DOCUMENTS}");
    let found = match state.backend.plugin_get(signed.token(), CATALOGUE, &route).await {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(%err, "the catalogue could not be searched");
            return Vec::new();
        }
    };
    found
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    let kind = item["kind"].as_str()?;
                    let key = item["key"].as_str().unwrap_or_default();
                    let name = item["name"].as_str().unwrap_or_default();
                    let slug = kind.to_lowercase();
                    Some(Resource {
                        href: format!("/p/resources/r/{slug}/{key}"),
                        title: match item["title"].as_str().unwrap_or_default() {
                            "" => name.to_string(),
                            title => format!("{name} · {title}"),
                        },
                        kind: kind.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A discussion in the Watercooler, which the search covers as it covers documents.
pub struct Discussion {
    pub href: String,
    pub title: String,
    pub detail: String,
}

/// Discussions whose title or messages match, as the Watercooler finds them.
async fn discussions(state: &AppState, signed: &Signed, text: &str) -> Vec<Discussion> {
    let asked: String = url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
    let route = format!("threads?q={asked}&limit={DOCUMENTS}");
    let found = match state.backend.plugin_get(signed.token(), WATER, &route).await {
        Ok(found) => found,
        Err(err) => {
            tracing::warn!(%err, "discussions could not be searched");
            return Vec::new();
        }
    };
    found
        .as_array()
        .map(|threads| {
            threads
                .iter()
                .filter_map(|thread| {
                    let id = thread["id"].as_str()?;
                    let replies = thread["replies"].as_i64().unwrap_or_default();
                    let author = thread["author"].as_str().unwrap_or_default();
                    Some(Discussion {
                        href: format!("/p/water/threads/{id}"),
                        title: thread["title"].as_str().unwrap_or_default().to_string(),
                        detail: match replies {
                            0 => format!("started by {author}"),
                            1 => format!("{author}, 1 reply"),
                            many => format!("{author}, {many} replies"),
                        },
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Template)]
#[template(path = "search.html")]
pub struct SearchPage {
    pub chrome: Chrome,
    pub text: String,
    pub pages: Vec<Card>,
    /// Whether the Knowledge Base is there to be searched; its heading is left out when not.
    pub searches_documents: bool,
    /// The documents found, or nothing at all when the Knowledge Base could not be asked.
    pub documents: Option<Vec<Document>>,
    /// The discussions found, empty when the Watercooler is not there or matches nothing.
    pub discussions: Vec<Discussion>,
    /// What the Catalogue holds that matches.
    pub resources: Vec<Resource>,
    /// Which section to show first: `"resources"`, `"documents"`, `"discussions"`, or empty.
    pub focus: &'static str,
}

#[derive(Debug, Deserialize)]
pub struct Asked {
    #[serde(default)]
    q: Option<String>,
    /// Where the search was opened from, so its own kind of result can be offered first.
    #[serde(default)]
    from: Option<String>,
}

/// Which kind of result belongs to `path`, so the header's search can put it first rather than
/// always in the one order: a document while reading the Knowledge Base, a discussion in the
/// Watercooler, something from the Catalogue on a team or a resource's own page. A page with no
/// kind of its own — most of a plugin, or the platform's own pages — asks for nothing special,
/// and the search keeps its usual order, pages first.
fn focus_of(path: &str) -> &'static str {
    if path.starts_with("/p/kb") {
        "documents"
    } else if path.starts_with("/p/water") {
        "discussions"
    } else if path.starts_with("/p/resources")
        || path.starts_with("/teams")
        || path.starts_with("/users")
        || path.starts_with("/service-accounts")
    {
        "resources"
    } else {
        ""
    }
}

/// A page matches when every word of the search is somewhere in its name or its description, so
/// "tokens scripts" finds Access tokens and "tokens calendar" finds nothing.
fn matches(card: &Card, words: &[String]) -> bool {
    let mut text = card.label.to_lowercase();
    if let Some(description) = &card.description {
        text.push(' ');
        text.push_str(&description.to_lowercase());
    }
    words.iter().all(|word| text.contains(word))
}

/// The Knowledge Base escapes its snippets and marks the words that matched, since it means them
/// to be drawn as HTML. The template escapes whatever it is given, so the text is read back here.
fn unescaped(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&amp;", "&")
}

/// A snippet split into what matched and what did not.
fn snippet(raw: &str) -> Vec<Stretch> {
    let mut stretches = Vec::new();
    let mut rest = raw;
    while let Some(at) = rest.find("<mark>") {
        let (before, marked) = rest.split_at(at);
        if !before.is_empty() {
            stretches.push(Stretch { text: unescaped(before), matched: false });
        }
        let marked = &marked["<mark>".len()..];
        match marked.find("</mark>") {
            Some(end) => {
                stretches.push(Stretch { text: unescaped(&marked[..end]), matched: true });
                rest = &marked[end + "</mark>".len()..];
            }
            None => {
                rest = marked;
                break;
            }
        }
    }
    if !rest.is_empty() {
        stretches.push(Stretch { text: unescaped(rest), matched: false });
    }
    stretches
}

/// A hit as the Knowledge Base returns it. A link anywhere but its own pages is dropped, so a
/// document cannot send someone off this platform.
fn document(hit: &Value) -> Option<Document> {
    let text = |field: &str| hit.get(field).and_then(Value::as_str).unwrap_or_default().to_string();
    let href = text("url");
    if !href.starts_with("/p/kb/") {
        return None;
    }
    Some(Document {
        href,
        title: text("title"),
        space: text("space"),
        snippet: snippet(&text("snippet")),
    })
}

async fn documents(state: &AppState, signed: &Signed, text: &str) -> Option<Vec<Document>> {
    let asked: String = url::form_urlencoded::byte_serialize(text.as_bytes()).collect();
    let route = format!("search?q={asked}&limit={DOCUMENTS}");
    match state.backend.plugin_get(signed.token(), KB, &route).await {
        Ok(found) => Some(
            found["results"]
                .as_array()
                .map(|results| results.iter().filter_map(document).collect())
                .unwrap_or_default(),
        ),
        Err(err) => {
            tracing::warn!(%err, "documents could not be searched");
            None
        }
    }
}

/// How many of each kind the header's search offers before somebody presses Enter.
const SUGGESTIONS: usize = 5;

#[derive(Template)]
#[template(path = "search_options.html")]
pub struct SearchOptions {
    pub text: String,
    pub pages: Vec<Card>,
    pub resources: Vec<Resource>,
    pub documents: Vec<Document>,
    pub discussions: Vec<Discussion>,
    /// Which of the above to offer first: `"resources"`, `"documents"`, `"discussions"`, or empty.
    pub focus: &'static str,
}

/// Whether this plugin is one the viewer can read and it is running.
fn reads(access: &crate::backend::Access, plugin: &str) -> bool {
    access.plugins.get(plugin).is_some_and(|plugin| plugin.read && plugin.running)
}

/// What the search in the header offers while somebody types: the pages they can open, what the
/// Knowledge Base finds, and searching everything for it. Each is a link, so choosing one goes.
pub async fn search_options(
    State(state): State<AppState>,
    signed: Signed,
    Query(asked): Query<Asked>,
) -> Result<Html<String>, WebError> {
    let access = session::access(&state, &signed).await;
    let text = asked.q.unwrap_or_default().trim().to_string();
    let focus = asked.from.as_deref().map(focus_of).unwrap_or_default();
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    let pages: Vec<Card> = match words.is_empty() {
        true => Vec::new(),
        false => cards(&access)
            .into_iter()
            .filter(|card| matches(card, &words))
            .take(SUGGESTIONS)
            .collect(),
    };
    let searches_documents = reads(&access, KB);
    let documents = match searches_documents && !text.is_empty() {
        true => documents(&state, &signed, &text)
            .await
            .unwrap_or_default()
            .into_iter()
            .take(SUGGESTIONS)
            .collect(),
        false => Vec::new(),
    };
    let discussions = match reads(&access, WATER) && !text.is_empty() {
        true => discussions(&state, &signed, &text).await.into_iter().take(SUGGESTIONS).collect(),
        false => Vec::new(),
    };
    let resources = match reads(&access, CATALOGUE) && !text.is_empty() {
        true => resources(&state, &signed, &text).await.into_iter().take(SUGGESTIONS).collect(),
        false => Vec::new(),
    };
    Ok(Html(SearchOptions { text, pages, resources, documents, discussions, focus }.render()?))
}

pub async fn search(
    State(state): State<AppState>,
    signed: Signed,
    Extension(csrf): Extension<Csrf>,
    Query(asked): Query<Asked>,
) -> Result<Html<String>, WebError> {
    let access = session::access(&state, &signed).await;
    let text = asked.q.unwrap_or_default().trim().to_string();
    let focus = asked.from.as_deref().map(focus_of).unwrap_or_default();
    let words: Vec<String> = text.split_whitespace().map(str::to_lowercase).collect();
    let pages = match words.is_empty() {
        true => Vec::new(),
        false => cards(&access).into_iter().filter(|card| matches(card, &words)).collect(),
    };
    let searches_documents = reads(&access, KB);
    let documents = match searches_documents && !text.is_empty() {
        true => documents(&state, &signed, &text).await,
        false => None,
    };
    let discussions = match reads(&access, WATER) && !text.is_empty() {
        true => discussions(&state, &signed, &text).await,
        false => Vec::new(),
    };
    let resources = match reads(&access, CATALOGUE) && !text.is_empty() {
        true => resources(&state, &signed, &text).await,
        false => Vec::new(),
    };
    let chrome =
        Chrome::new("Search", "/search").signed(&signed, &csrf).with_plugins(&state, &signed).await;
    let page = SearchPage {
        chrome,
        text,
        pages,
        searches_documents,
        documents,
        discussions,
        resources,
        focus,
    };
    Ok(Html(page.render()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_snippet_keeps_what_matched_apart_from_the_rest() {
        let raw = "deploy &lt;b&gt; the <mark>payments</mark> service &amp; <mark>api</mark>";
        let shown: Vec<(String, bool)> =
            snippet(raw).into_iter().map(|stretch| (stretch.text, stretch.matched)).collect();
        assert_eq!(
            shown,
            [
                ("deploy <b> the ".to_string(), false),
                ("payments".to_string(), true),
                (" service & ".to_string(), false),
                ("api".to_string(), true)
            ]
        );
    }

    #[test]
    fn a_document_link_must_stay_within_the_knowledge_base() {
        let hit = |url: &str| serde_json::json!({ "title": "T", "url": url, "snippet": "" });
        assert!(document(&hit("/p/kb/docs/ccc/index")).is_some());
        assert!(document(&hit("https://example.com")).is_none());
        assert!(document(&hit("/p/other/x")).is_none());
    }

    #[test]
    fn a_page_asks_for_the_result_that_belongs_where_it_is() {
        assert_eq!(focus_of("/p/water/"), "discussions");
        assert_eq!(focus_of("/p/water/threads/123"), "discussions");
        assert_eq!(focus_of("/p/kb/docs/ccc/index"), "documents");
        assert_eq!(focus_of("/p/resources/r/team/product"), "resources");
        assert_eq!(focus_of("/teams"), "resources");
        assert_eq!(focus_of("/p/rbac/groups"), "");
        assert_eq!(focus_of("/settings"), "");
    }

    #[test]
    fn pages_match_on_every_word_of_their_name_or_description() {
        let card = Card {
            label: "Access tokens".into(),
            href: "/tokens".into(),
            description: Some("Your personal tokens, for scripts".into()),
        };
        let words = |text: &str| text.split_whitespace().map(str::to_lowercase).collect::<Vec<_>>();
        assert!(matches(&card, &words("TOKENS scripts")));
        assert!(!matches(&card, &words("tokens calendar")));
    }
}
