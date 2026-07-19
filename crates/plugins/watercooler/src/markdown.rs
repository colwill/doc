//! Messages are Markdown: CommonMark with GitHub's tables, strikethrough, task lists and autolinks.
//! Raw HTML is dropped and ammonia cleans the rest. A `/kind:name` word tags a resource or, as
//! `/plugin:id`, a plugin, and becomes a link to its tag page named after it.

use comrak::nodes::{AstNode, NodeLink, NodeValue};
use comrak::{Arena, Options, format_html, parse_document};

/// The kinds a tag may name, as they are written after the `/`: the Catalogue's, and the
/// platform's own plugins, which are not in the Catalogue but are talked about as much.
const KINDS: [&str; 13] = [
    "plugin",
    "organisation",
    "service",
    "component",
    "repository",
    "team",
    "role",
    "user",
    "serviceaccount",
    "documentation",
    "cloudresource",
    "attribute",
    "permission",
];

pub struct Rendered {
    pub html: String,
    /// The words of the message, for searching.
    pub text: String,
    /// Each resource it tags, as `kind:name`.
    pub tags: Vec<String>,
}

/// `kind:name` with the kind in one spelling, if the word names a resource.
pub fn tag_of(word: &str) -> Option<String> {
    let (kind, name) = word.trim().split_once(':')?;
    let kind: String =
        kind.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_ascii_lowercase();
    // Organisations were called verticals, and older tags still say so.
    let kind = match kind.as_str() {
        "vertical" | "organization" => "organisation".to_string(),
        _ => kind,
    };
    let fine = |c: char| c.is_ascii_alphanumeric() || "._~/@+-".contains(c);
    let named = !name.is_empty() && name.len() <= 200 && name.chars().all(fine);
    (KINDS.contains(&kind.as_str()) && named).then(|| format!("{kind}:{name}"))
}

/// A tag's kind as a person reads it: `cloudresource` is a Cloud resource.
pub fn kind_name(kind: &str) -> &str {
    match kind {
        "organisation" => "Organisation",
        "service" => "Service",
        "repository" => "Repository",
        "team" => "Team",
        "role" => "Role",
        "user" => "User",
        "serviceaccount" => "Service account",
        "documentation" => "Documentation",
        "cloudresource" => "Cloud resource",
        "attribute" => "Attribute",
        "permission" => "Permission",
        "plugin" => "Plugin",
        other => other,
    }
}

/// Where a tag's page is.
pub fn tag_page(tag: &str) -> String {
    let (kind, name) = tag.split_once(':').unwrap_or(("", tag));
    format!("/p/water/tags/{kind}/{name}")
}

/// Each tag written in `text`, with where its `/` starts and where it ends.
fn tokens(text: &str) -> Vec<(usize, usize, String)> {
    let mut found = Vec::new();
    for (start, _) in text.match_indices('/') {
        let after_space = text[..start]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_whitespace() || "([{".contains(c));
        if !after_space {
            continue;
        }
        let rest = &text[start + 1..];
        let end = rest
            .find(|c: char| c.is_whitespace() || ",;!?)]}\"'".contains(c))
            .unwrap_or(rest.len());
        let word = rest[..end].trim_end_matches(['.', ':']);
        if let Some(tag) = tag_of(word) {
            found.push((start, start + 1 + word.len(), tag));
        }
    }
    found
}

fn options() -> Options<'static> {
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.tasklist = true;
    options.extension.autolink = true;
    options
}

pub fn render(markdown: &str) -> Rendered {
    let arena = Arena::new();
    let options = options();
    let root = parse_document(&arena, markdown, &options);
    let mut words: Vec<String> = Vec::new();
    let mut texts = Vec::new();
    for node in root.descendants() {
        match &node.data().value {
            NodeValue::Text(text) => {
                words.push(text.to_string());
                let linked = node.ancestors().any(|above| {
                    matches!(above.data().value, NodeValue::Link(_) | NodeValue::Image(_))
                });
                if !linked {
                    texts.push(node);
                }
            }
            NodeValue::Code(code) => words.push(code.literal.clone()),
            NodeValue::CodeBlock(block) => words.push(block.literal.clone()),
            _ => {}
        }
    }
    let mut tags: Vec<String> = Vec::new();
    for node in texts {
        let text = match &node.data().value {
            NodeValue::Text(text) => text.to_string(),
            _ => continue,
        };
        let found = tokens(&text);
        if found.is_empty() {
            continue;
        }
        let piece =
            |text: &str| arena.alloc(AstNode::from(NodeValue::Text(text.to_string().into())));
        let mut last = 0;
        for (start, end, tag) in found {
            if start > last {
                node.insert_before(piece(&text[last..start]));
            }
            let target = NodeLink { url: tag_page(&tag), title: tag.clone() };
            let link = arena.alloc(AstNode::from(NodeValue::Link(Box::new(target))));
            link.append(piece(tag.split_once(':').map_or(tag.as_str(), |(_, name)| name)));
            node.insert_before(link);
            if !tags.contains(&tag) {
                tags.push(tag);
            }
            last = end;
        }
        if last < text.len() {
            node.insert_before(piece(&text[last..]));
        }
        node.detach();
    }
    let mut html = String::new();
    if format_html(root, &options, &mut html).is_err() {
        html.clear();
    }
    Rendered { html: ammonia::clean(&html), text: words.join(" "), tags }
}

#[cfg(test)]
mod renamed {
    #[test]
    fn an_old_vertical_tag_names_its_organisation() {
        assert_eq!(super::tag_of("vertical:payments").as_deref(), Some("organisation:payments"));
        assert_eq!(
            super::tag_of("Organisation:payments").as_deref(),
            Some("organisation:payments")
        );
    }
}
