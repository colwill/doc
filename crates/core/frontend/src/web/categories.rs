//! The categories plugins are sorted into (`[[plugins.categories]]` in the backend's
//! configuration): a section each on the Plugins page, and a pill beside a plugin's name wherever
//! else one is shown, so a plugin reads as the same kind of thing everywhere.

use crate::backend::Category;

/// How many colours the pills take in turn, one category after another. Each category keeps its
/// colour on every page, since it comes from where the category sits in the configuration.
const TONES: usize = 6;

/// What the Plugins page calls the plugins in no category.
const OTHER: &str = "Other";

/// A plugin's category as its pill shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pill {
    pub name: String,
    /// Which of the pill colours, from 1.
    pub tone: usize,
}

/// The category a plugin is in, if any: the first that names it.
pub fn pill(categories: &[Category], plugin: &str) -> Option<Pill> {
    let at = categories.iter().position(|category| category.plugins.iter().any(|p| p == plugin))?;
    Some(Pill { name: categories[at].name.clone(), tone: at % TONES + 1 })
}

/// One category's part of a list, headed by its name.
#[derive(Debug, Clone)]
pub struct Section<T> {
    pub name: String,
    /// The anchor it is linked by, from its name.
    pub anchor: String,
    pub items: Vec<T>,
}

/// Sorts `items` into a section per category, in the categories' order, keeping each section in
/// the order the items came. A category with nothing in it is left out, and anything in no
/// category goes last, under Other.
pub fn sections<T>(
    categories: &[Category],
    items: Vec<T>,
    plugin: impl Fn(&T) -> &str,
) -> Vec<Section<T>> {
    let mut sorted: Vec<Section<T>> = categories
        .iter()
        .map(|category| Section {
            name: category.name.clone(),
            anchor: anchor(&category.name),
            items: Vec::new(),
        })
        .chain(std::iter::once(Section {
            name: OTHER.to_string(),
            anchor: anchor(OTHER),
            items: Vec::new(),
        }))
        .collect();
    for item in items {
        let at = categories
            .iter()
            .position(|category| category.plugins.iter().any(|p| p == plugin(&item)))
            .unwrap_or(categories.len());
        sorted[at].items.push(item);
    }
    sorted.retain(|section| !section.items.is_empty());
    sorted
}

fn anchor(name: &str) -> String {
    let slug: String = name
        .to_lowercase()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    format!("category-{}", slug.trim_matches('-'))
}
