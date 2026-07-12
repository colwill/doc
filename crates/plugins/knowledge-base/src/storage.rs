//! Confluence's storage format to safe HTML. Links to pages, images and attachments are pointed at
//! their new homes, code and panel macros become plain HTML, and everything else Confluence adds is
//! dropped before ammonia cleans the rest.

use std::sync::LazyLock;

use regex::{Captures, Regex};

fn pattern(text: &str) -> Regex {
    Regex::new(text).unwrap_or_else(|err| unreachable!("a fixed pattern is valid: {err}"))
}

static CDATA: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?s)<!\[CDATA\[(.*?)\]\]>"));
static PARAMETER: LazyLock<Regex> =
    LazyLock::new(|| pattern(r"(?s)<ac:parameter\b[^>]*>.*?</ac:parameter>"));
static CODE: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r#"(?s)<ac:structured-macro\b[^>]*ac:name="(?:code|noformat)"[^>]*>.*?<ac:plain-text-body>(.*?)</ac:plain-text-body>.*?</ac:structured-macro>"#,
    )
});
static PANEL: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r#"(?s)<ac:structured-macro\b[^>]*ac:name="(?:info|note|warning|tip|panel)"[^>]*>.*?<ac:rich-text-body>(.*?)</ac:rich-text-body>.*?</ac:structured-macro>"#,
    )
});
static LINK: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?s)<ac:link\b[^>]*>(.*?)</ac:link>"));
static IMAGE: LazyLock<Regex> = LazyLock::new(|| pattern(r"(?s)<ac:image\b[^>]*>(.*?)</ac:image>"));
static TITLE: LazyLock<Regex> = LazyLock::new(|| pattern(r#"ri:content-title="([^"]*)""#));
static FILENAME: LazyLock<Regex> = LazyLock::new(|| pattern(r#"ri:filename="([^"]*)""#));
static URL: LazyLock<Regex> = LazyLock::new(|| pattern(r#"ri:value="([^"]*)""#));
static LINK_TEXT: LazyLock<Regex> = LazyLock::new(|| {
    pattern(
        r"(?s)<ac:(?:plain-text-link-body|link-body)>(.*?)</ac:(?:plain-text-link-body|link-body)>",
    )
});
static TAGS: LazyLock<Regex> = LazyLock::new(|| pattern(r"<[^>]*>"));

fn escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

fn unescape(text: &str) -> String {
    text.replace("&quot;", "\"").replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}

pub fn percent(name: &str) -> String {
    url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>().replace('+', "%20")
}

/// Storage format as safe HTML for page `page` of `space`; `path_of` turns a title into a path.
pub fn convert(storage: &str, space: &str, page: &str, path_of: &dyn Fn(&str) -> String) -> String {
    let text = CODE.replace_all(storage, |found: &Captures<'_>| {
        let code = CDATA.replace_all(&found[1], "$1").into_owned();
        format!("<pre><code>{}</code></pre>", escape(&code))
    });
    let text = PANEL.replace_all(&text, "<blockquote>$1</blockquote>");
    let text = PARAMETER.replace_all(&text, "");
    let text = LINK.replace_all(&text, |found: &Captures<'_>| {
        let inner = &found[1];
        let Some(title) = TITLE.captures(inner).map(|title| unescape(&title[1])) else {
            return LINK_TEXT
                .captures(inner)
                .map(|body| CDATA.replace_all(&body[1], "$1").into_owned())
                .unwrap_or_default();
        };
        let label = LINK_TEXT
            .captures(inner)
            .map(|body| CDATA.replace_all(&body[1], "$1").into_owned())
            .filter(|label| !label.trim().is_empty())
            .unwrap_or_else(|| escape(&title));
        format!("<a href=\"/p/kb/docs/{space}/{}\">{label}</a>", path_of(&title))
    });
    let text = IMAGE.replace_all(&text, |found: &Captures<'_>| {
        let inner = &found[1];
        if let Some(name) = FILENAME.captures(inner).map(|name| unescape(&name[1])) {
            let source = format!("/p/kb/attachments/{space}/{}/{}", percent(page), percent(&name));
            return format!("<img src=\"{source}\" alt=\"{}\">", escape(&name));
        }
        match URL.captures(inner) {
            Some(url) => format!("<img src=\"{}\" alt=\"\">", &url[1]),
            None => String::new(),
        }
    });
    let text = CDATA.replace_all(&text, |found: &Captures<'_>| escape(&found[1]));
    ammonia::clean(&text)
}

/// The words of a converted page, for searching.
pub fn words(html: &str) -> String {
    let spaced = TAGS.replace_all(html, " ");
    unescape(&spaced).split_whitespace().collect::<Vec<_>>().join(" ")
}
