//! Markdown to safe HTML: CommonMark with GitHub's tables, strikethrough, task lists and autolinks.
//! The renderer drops raw HTML and ammonia cleans what is left, so no script ever reaches a page;
//! links between pages are pointed at their new home.

use comrak::nodes::NodeValue;
use comrak::{Arena, Options, format_html, parse_document};
use serde_json::{Map, Value};

pub struct Rendered {
    pub title: Option<String>,
    pub html: String,
    /// The words of the page, for searching and for snippets.
    pub text: String,
}

/// YAML between `---` lines at the very top, and what follows it.
pub fn front_matter(source: &str) -> (Map<String, Value>, &str) {
    let Some(rest) = source.strip_prefix("---\n").or_else(|| source.strip_prefix("---\r\n")) else {
        return (Map::new(), source);
    };
    let end = ["\n---\n", "\n---\r\n", "\n...\n"]
        .iter()
        .filter_map(|fence| rest.find(fence).map(|at| (at, fence.len())))
        .min();
    let Some((at, fence)) = end else { return (Map::new(), source) };
    let front = serde_yaml_ng::from_str::<Value>(&rest[..at])
        .ok()
        .and_then(|value| value.as_object().cloned());
    (front.unwrap_or_default(), &rest[at + fence..])
}

fn options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.tasklist = true;
    options.extension.autolink = true;
    options.extension.footnotes = true;
    options
}

/// `rewrite` is asked about every link and says where it should go instead, if anywhere.
pub fn render(markdown: &str, rewrite: &dyn Fn(&str) -> Option<String>) -> Rendered {
    let arena = Arena::new();
    let options = options();
    let root = parse_document(&arena, markdown, &options);
    let mut title = None;
    let mut words: Vec<String> = Vec::new();
    for node in root.descendants() {
        let mut data = node.data_mut();
        match &mut data.value {
            NodeValue::Link(link) | NodeValue::Image(link) => {
                if let Some(target) = rewrite(&link.url) {
                    link.url = target;
                }
            }
            NodeValue::Heading(heading) if heading.level == 1 && title.is_none() => {
                drop(data);
                let text: Vec<String> = node
                    .descendants()
                    .filter_map(|inner| match &inner.data().value {
                        NodeValue::Text(text) => Some(text.to_string()),
                        NodeValue::Code(code) => Some(code.literal.clone()),
                        _ => None,
                    })
                    .collect();
                title = Some(text.concat()).filter(|title| !title.trim().is_empty());
            }
            NodeValue::Text(text) => words.push(text.to_string()),
            NodeValue::Code(code) => words.push(code.literal.clone()),
            NodeValue::CodeBlock(block) => words.push(block.literal.clone()),
            _ => {}
        }
    }
    let mut html = String::new();
    if format_html(root, &options, &mut html).is_err() {
        html.clear();
    }
    Rendered { title, html: ammonia::clean(&html), text: words.join(" ") }
}

/// Every link and image a page has, as written, for finding the files it points at.
pub fn targets(markdown: &str) -> Vec<String> {
    let arena = Arena::new();
    let root = parse_document(&arena, markdown, &options());
    root.descendants()
        .filter_map(|node| match &node.data().value {
            NodeValue::Link(link) | NodeValue::Image(link) => Some(link.url.clone()),
            _ => None,
        })
        .collect()
}

/// A relative link or image from `from`, a path in the archive, as a path in the archive; none for
/// anything absolute, elsewhere or only an anchor.
pub fn relative(from: &str, url: &str) -> Option<String> {
    if url.contains("://") || url.starts_with(['/', '#']) || url.starts_with("mailto:") {
        return None;
    }
    let target = url.split(['#', '?']).next().unwrap_or_default();
    let target: String =
        url::form_urlencoded::parse(format!("x={}", target.replace('+', "%2B")).as_bytes())
            .next()
            .map(|(_, value)| value.into_owned())
            .unwrap_or_default();
    if target.is_empty() {
        return None;
    }
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

/// Where a link from page `from` to `url` leads in the space, if it names another page of it.
pub fn page_link(space: &str, from: &str, url: &str) -> Option<String> {
    if url.contains("://") || url.starts_with(['/', '#']) || url.starts_with("mailto:") {
        return None;
    }
    let (target, anchor) =
        url.split_once('#').map_or((url, None), |(target, anchor)| (target, Some(anchor)));
    let target = target.strip_suffix(".md")?;
    let mut parts: Vec<&str> = from.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    let anchor = anchor.map(|anchor| format!("#{anchor}")).unwrap_or_default();
    Some(format!("/p/kb/docs/{space}/{}{anchor}", parts.join("/")))
}
