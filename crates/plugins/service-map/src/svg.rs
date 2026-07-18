//! Drawing a layout as inline SVG with the platform's `doc-graph` classes: geometry in attributes,
//! everything visual in the stylesheet, and every node a link when it has somewhere to go.

use std::collections::BTreeSet;
use std::fmt::Write;

use crate::layout::{Layout, NODE_HEIGHT};

/// Longest label drawn in full; a longer one is cut, and whole in the node's tooltip.
const LABEL_CHARS: usize = 24;

pub fn escape(text: &str) -> String {
    text.chars()
        .map(|c| match c {
            '&' => "&amp;".to_string(),
            '<' => "&lt;".to_string(),
            '>' => "&gt;".to_string(),
            '"' => "&quot;".to_string(),
            '\'' => "&#39;".to_string(),
            c => c.to_string(),
        })
        .collect()
}

fn cut(label: &str) -> String {
    match label.chars().count() > LABEL_CHARS {
        true => format!("{}…", label.chars().take(LABEL_CHARS - 1).collect::<String>()),
        false => label.to_string(),
    }
}

pub fn draw(layout: &Layout, description: &str) -> String {
    let mut svg = String::new();
    let (width, height) = (layout.width, layout.height);
    let _ = write!(
        svg,
        r#"<svg class="doc-graph" viewBox="0 0 {width} {height}" width="{width}" height="{height}" role="img" aria-label="{}">"#,
        escape(description)
    );
    for column in &layout.columns {
        let _ = write!(
            svg,
            r#"<text class="doc-graph__heading" x="{}" y="24">{}</text>"#,
            column.x,
            escape(&column.label)
        );
    }
    // What the map is of: its own connections are drawn in colour without anyone reaching for
    // them, and every edge says which two nodes it joins so that hovering one can light the rest.
    let focused: BTreeSet<&str> = layout
        .nodes
        .iter()
        .filter(|node| node.item.focus)
        .map(|node| node.item.id.as_str())
        .collect();
    for edge in &layout.edges {
        let mut class = String::from("doc-graph__edge");
        if edge.link.derived {
            class.push_str(" doc-graph__edge--derived");
        }
        if focused.contains(edge.link.from.as_str()) || focused.contains(edge.link.to.as_str()) {
            class.push_str(" doc-graph__edge--near");
        }
        let _ = write!(
            svg,
            r#"<path class="{class}" data-from="{}" data-to="{}" d="{}" />"#,
            escape(&edge.link.from),
            escape(&edge.link.to),
            edge.path
        );
    }
    for node in &layout.nodes {
        let item = &node.item;
        let (rect, label) = match (item.focus, item.ghost) {
            (true, _) => (
                "doc-graph__node doc-graph__node--focus",
                "doc-graph__label doc-graph__label--focus",
            ),
            (false, true) => (
                "doc-graph__node doc-graph__node--ghost",
                "doc-graph__label doc-graph__label--ghost",
            ),
            (false, false) => ("doc-graph__node", "doc-graph__label"),
        };
        let tooltip = match item.note.is_empty() {
            true => escape(&item.label),
            false => format!("{} — {}", escape(&item.label), escape(&item.note)),
        };
        let shape = format!(
            r#"<title>{tooltip}</title><rect class="{rect}" x="{}" y="{}" width="{}" height="{}" rx="6" /><text class="{label}" x="{}" y="{}">{}</text>"#,
            node.x,
            node.y,
            node.width,
            node.height,
            node.x + 12,
            node.y + NODE_HEIGHT / 2 + 5,
            escape(&cut(&item.label))
        );
        let named = escape(&item.id);
        match &item.href {
            Some(href) => {
                let _ =
                    write!(svg, r#"<a href="{}" data-node="{named}">{shape}</a>"#, escape(href));
            }
            None => {
                let _ = write!(svg, r#"<g data-node="{named}">{shape}</g>"#);
            }
        }
    }
    svg.push_str("</svg>");
    svg
}
