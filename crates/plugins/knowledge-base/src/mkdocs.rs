//! What an MkDocs project says about itself: its name, where its pages are, and the order of its
//! navigation. Python's YAML tags, such as `!!python/name:`, are read and ignored.

use std::collections::BTreeMap;

use serde_yaml_ng::Value;

#[derive(Debug, Default)]
pub struct Site {
    pub name: Option<String>,
    pub docs_dir: String,
    /// Pages in navigation order, with the title the navigation gives each.
    pub nav: Vec<(Option<String>, String)>,
}

fn text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Tagged(tagged) => text(&tagged.value),
        _ => None,
    }
}

fn walk(value: &Value, into: &mut Vec<(Option<String>, String)>) {
    match value {
        Value::Sequence(items) => items.iter().for_each(|item| walk(item, into)),
        Value::String(path) => into.push((None, path.clone())),
        Value::Mapping(entries) => {
            for (title, target) in entries {
                match target {
                    Value::String(path) => into.push((text(title), path.clone())),
                    nested => walk(nested, into),
                }
            }
        }
        Value::Tagged(tagged) => walk(&tagged.value, into),
        _ => {}
    }
}

pub fn read(yaml: &str) -> Site {
    let parsed: Value = serde_yaml_ng::from_str(yaml).unwrap_or(Value::Null);
    let mut nav = Vec::new();
    if let Some(listed) = parsed.get("nav") {
        walk(listed, &mut nav);
    }
    nav.retain(|(_, path)| path.ends_with(".md") && !path.contains("://"));
    Site {
        name: parsed.get("site_name").and_then(text),
        docs_dir: parsed.get("docs_dir").and_then(text).unwrap_or_else(|| "docs".into()),
        nav,
    }
}

/// The pages under the docs directory: the navigation's first, in its order, then the rest by path.
pub fn pages(
    site: &Site,
    files: &BTreeMap<String, Vec<u8>>,
) -> Vec<(String, Option<String>, String)> {
    let prefix = match site.docs_dir.trim_matches('/') {
        "" | "." => String::new(),
        dir => format!("{dir}/"),
    };
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for (path, bytes) in files {
        let Some(inside) = path.strip_prefix(&prefix) else { continue };
        if !inside.ends_with(".md") {
            continue;
        }
        if let Ok(content) = String::from_utf8(bytes.clone()) {
            found.insert(inside.to_string(), content);
        }
    }
    let mut ordered = Vec::new();
    for (title, path) in &site.nav {
        if let Some(content) = found.remove(path.trim_start_matches("./")) {
            ordered.push((path.trim_start_matches("./").to_string(), title.clone(), content));
        }
    }
    ordered.extend(found.into_iter().map(|(path, content)| (path, None, content)));
    ordered
}
