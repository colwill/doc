//! Descriptions and notes are Markdown: CommonMark with GitHub's tables, strikethrough, task lists
//! and autolinks. Raw HTML is dropped and ammonia cleans the rest.

use comrak::{Options, markdown_to_html};

pub fn render(markdown: &str) -> String {
    let mut options = Options::default();
    options.extension.table = true;
    options.extension.strikethrough = true;
    options.extension.tasklist = true;
    options.extension.autolink = true;
    ammonia::clean(&markdown_to_html(markdown, &options))
}
