//! The radar's fixed shape: four quadrants and four rings, as Thoughtworks and Backstage draw them,
//! and which way an entry last moved.

use serde::{Deserialize, Serialize};

pub struct Quadrant {
    pub id: &'static str,
    pub name: &'static str,
}

pub struct Ring {
    pub id: &'static str,
    pub name: &'static str,
    pub meaning: &'static str,
}

/// In drawing order: top left, top right, bottom left, bottom right.
pub const QUADRANTS: [Quadrant; 4] = [
    Quadrant { id: "techniques", name: "Techniques" },
    Quadrant { id: "platforms", name: "Platforms" },
    Quadrant { id: "tools", name: "Tools" },
    Quadrant { id: "languages-and-frameworks", name: "Languages & Frameworks" },
];

/// From the centre out.
pub const RINGS: [Ring; 4] = [
    Ring { id: "adopt", name: "Adopt", meaning: "Proven here. Use it by default where it fits." },
    Ring {
        id: "trial",
        name: "Trial",
        meaning: "Worth pursuing on a project that can handle the risk.",
    },
    Ring {
        id: "assess",
        name: "Assess",
        meaning: "Worth exploring, to understand how it would affect us.",
    },
    Ring {
        id: "hold",
        name: "Hold",
        meaning: "Don't start anything new with it; move away where we can.",
    },
];

pub fn quadrant_ids() -> Vec<&'static str> {
    QUADRANTS.iter().map(|quadrant| quadrant.id).collect()
}

pub fn ring_ids() -> Vec<&'static str> {
    RINGS.iter().map(|ring| ring.id).collect()
}

pub fn quadrant_index(id: &str) -> Option<usize> {
    QUADRANTS.iter().position(|quadrant| quadrant.id == id)
}

pub fn ring_index(id: &str) -> Option<usize> {
    RINGS.iter().position(|ring| ring.id == id)
}

pub fn quadrant_name(id: &str) -> &str {
    quadrant_index(id).map_or(id, |index| QUADRANTS[index].name)
}

pub fn ring_name(id: &str) -> &str {
    ring_index(id).map_or(id, |index| RINGS[index].name)
}

/// Which way an entry last moved. `In` is towards the centre, where Adopt is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Moved {
    New,
    In,
    Out,
    None,
}

impl Moved {
    pub const ALL: [&'static str; 4] = ["new", "in", "out", "none"];

    pub fn between(from: Option<&str>, to: &str) -> Self {
        match (from.and_then(ring_index), ring_index(to)) {
            (None, _) => Self::New,
            (Some(from), Some(to)) if to < from => Self::In,
            (Some(from), Some(to)) if to > from => Self::Out,
            _ => Self::None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::In => "in",
            Self::Out => "out",
            Self::None => "none",
        }
    }

    /// Backstage's `moved`: 1 in, -1 out, 0 otherwise.
    pub fn backstage(self) -> i64 {
        match self {
            Self::In => 1,
            Self::Out => -1,
            Self::New | Self::None => 0,
        }
    }

    pub fn words(self) -> &'static str {
        match self {
            Self::New => "new",
            Self::In => "moved in",
            Self::Out => "moved out",
            Self::None => "",
        }
    }
}
