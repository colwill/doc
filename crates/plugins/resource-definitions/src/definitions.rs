//! Connection definitions, written `Connection[name](Kind, …)`: the order in which kinds are drilled
//! through, from the highest level down. Two kinds next to each other in a definition are the only
//! places an explicit connection may go.

use std::fmt;

use serde::Serialize;

use crate::kinds::Kind;
use crate::model::Refusal;

const MAX_NAME: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Definition {
    pub name: String,
    pub kinds: Vec<Kind>,
}

impl Definition {
    pub fn new(name: &str, kinds: Vec<Kind>) -> Result<Self, Refusal> {
        let name = name.trim();
        let named = !name.is_empty()
            && name.len() <= MAX_NAME
            && name.starts_with(|c: char| c.is_ascii_alphanumeric())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if !named {
            return Err(Refusal::bad(format!(
                "`{name}` is not a definition name: up to {MAX_NAME} letters, digits, `-`, `_` or `.`"
            )));
        }
        if kinds.len() < 2 {
            return Err(Refusal::bad(format!("{name} needs at least two kinds")));
        }
        for (index, kind) in kinds.iter().enumerate() {
            if kinds[..index].contains(kind) {
                return Err(Refusal::bad(format!("{name} names {} twice", kind.plural())));
            }
        }
        Ok(Self { name: name.to_string(), kinds })
    }

    /// Several definitions, separated by commas or new lines, as PLAN.md writes them.
    pub fn parse_all(text: &str) -> Result<Vec<Self>, Refusal> {
        let mut definitions = Vec::new();
        let mut rest = text.trim();
        while !rest.is_empty() {
            let (definition, after) = parse_one(rest)?;
            definitions.push(definition);
            rest = after.trim_start().trim_start_matches(',').trim_start();
        }
        Ok(definitions)
    }

    pub fn position(&self, kind: Kind) -> Option<usize> {
        self.kinds.iter().position(|candidate| *candidate == kind)
    }

    pub fn adjacent(&self, a: Kind, b: Kind) -> bool {
        self.kinds
            .windows(2)
            .any(|pair| (pair[0], pair[1]) == (a, b) || (pair[0], pair[1]) == (b, a))
    }

    /// Whether `path` follows this definition's kinds in order, from wherever it starts.
    pub fn follows(&self, path: &[Kind]) -> bool {
        let Some(first) = path.first().and_then(|kind| self.position(*kind)) else {
            return false;
        };
        self.kinds.get(first..first + path.len()).is_some_and(|kinds| kinds == path)
    }
}

impl fmt::Display for Definition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kinds: Vec<&str> = self.kinds.iter().map(|kind| kind.plural()).collect();
        write!(f, "Connection[{}]({})", self.name, kinds.join(", "))
    }
}

fn parse_one(text: &str) -> Result<(Definition, &str), Refusal> {
    let bad =
        || Refusal::bad(format!("`{}` is not written Connection[name](Kind, …)", text.trim()));
    let text = text.trim_start();
    let keyword =
        text.get(..10).filter(|word| word.eq_ignore_ascii_case("connection")).ok_or_else(bad)?;
    let rest = text[keyword.len()..].trim_start().strip_prefix('[').ok_or_else(bad)?;
    let (name, rest) = rest.split_once(']').ok_or_else(bad)?;
    let rest = rest.trim_start().strip_prefix('(').ok_or_else(bad)?;
    let (list, rest) = rest.split_once(')').ok_or_else(bad)?;
    let kinds = list
        .split(',')
        .map(|kind| crate::kinds::kind(kind.trim()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((Definition::new(name, kinds)?, rest))
}

/// The pairs `rbac` answers for and nobody stores: holders, permissions and attributes.
pub fn derived(a: Kind, b: Kind) -> bool {
    let pair = |x, y| (a, b) == (x, y) || (a, b) == (y, x);
    pair(Kind::Role, Kind::User)
        || pair(Kind::Role, Kind::ServiceAccount)
        || pair(Kind::Role, Kind::Permission)
        || a == Kind::Attribute
        || b == Kind::Attribute
}

/// Why two kinds cannot be connected by hand, when they cannot.
pub fn unconnectable(definitions: &[Definition], a: Kind, b: Kind) -> Option<String> {
    let (a_kinds, b_kinds) = (a.plural(), b.plural());
    if derived(a, b) || [a, b].contains(&Kind::Permission) {
        return Some(format!("{a_kinds} and {b_kinds} are related in rbac, not here"));
    }
    // A connection is the catalogue's own, so one end has to be something it writes: its own
    // resources, or the organisations and teams it keeps for the platform (T69). Two things it
    // only reads, such as a person and a service account, are connected where they are kept.
    if !a.written() && !b.written() {
        return Some(format!("{a_kinds} and {b_kinds} are both kept elsewhere"));
    }
    if !definitions.iter().any(|definition| definition.adjacent(a, b)) {
        return Some(format!(
            "no connection definition puts {a_kinds} next to {b_kinds}, so they cannot be related"
        ));
    }
    None
}

pub fn connectable(definitions: &[Definition], a: Kind, b: Kind) -> bool {
    unconnectable(definitions, a, b).is_none()
}

/// Whether the definitions put `a` above `b`: `Some(true)` when `a` is `b`'s parent, `Some(false)`
/// when it is its child, and `None` when nothing says, or when definitions disagree.
///
/// Which kind is above which is the definitions' to say. The declared order of the kinds (§9.2)
/// only says where a column is drawn, and the two can disagree on purpose: an Organisation is
/// above the documentation it keeps while being drawn two columns to its left.
pub fn above(definitions: &[Definition], a: Kind, b: Kind) -> Option<bool> {
    if a == b {
        return None;
    }
    let parent_of = |x: Kind, y: Kind| {
        definitions
            .iter()
            .any(|definition| definition.kinds.windows(2).any(|pair| (pair[0], pair[1]) == (x, y)))
    };
    match (parent_of(a, b), parent_of(b, a)) {
        (true, false) => Some(true),
        (false, true) => Some(false),
        _ => None,
    }
}
