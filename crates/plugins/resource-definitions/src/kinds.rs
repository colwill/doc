//! The kinds of resource: which this plugin stores, which it reads from core or `rbac`, and how a
//! reference such as `Service:card-gateway` names one.

use std::fmt;

use serde::{Deserialize, Serialize};

use crate::model::Refusal;

/// **This order is the platform's order of kinds**, from the top of a drill-down to the bottom,
/// and it is declared here rather than worked out from whichever connections happen to exist.
/// Everything that shows kinds in order reads it: the service map's columns, the kinds overview,
/// the pickers, and a resource's connected things, which sort by this enum's `Ord`. A kind is
/// placed beside what it belongs to — `Documentation` after `Service`, because documentation is
/// documentation of a service or of a repository, and `DocumentationSource` beside it because
/// that is where the pages were read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Kind {
    Organisation,
    Service,
    /// A part of a service that runs or stores: an API, a worker, a job, a queue, a database, a
    /// cache. The test is whether it can be deployed or fail on its own, which is the granularity
    /// architecture questions are asked at (ADR-0017 §1). Kept here and synced by the Architecture
    /// plugin, and placed beside the service it belongs to.
    Component,
    Documentation,
    /// Where documentation is read from: a Knowledge Base source, such as a repository's docs
    /// folder or a Confluence space. The pages it brings in are `Documentation`, connected back
    /// to the source that brought them.
    DocumentationSource,
    Repository,
    Team,
    Role,
    User,
    ServiceAccount,
    CloudResource,
    Attribute,
    Permission,
}

/// Every kind, in the order above. The enum and this list are the same order, which `rank` and the
/// test below hold them to.
pub const ALL: [Kind; 13] = [
    Kind::Organisation,
    Kind::Service,
    Kind::Component,
    Kind::Documentation,
    Kind::DocumentationSource,
    Kind::Repository,
    Kind::Team,
    Kind::Role,
    Kind::User,
    Kind::ServiceAccount,
    Kind::CloudResource,
    Kind::Attribute,
    Kind::Permission,
];

/// Where to go to make one of a kind the catalogue only shows.
pub struct Made {
    pub href: &'static str,
    pub label: &'static str,
}

impl Kind {
    /// Where it sits in the platform's order, counting from the top of a drill-down. Anything
    /// drawing kinds side by side — the service map's columns — puts them in this order.
    pub fn rank(self) -> usize {
        ALL.iter().position(|kind| *kind == self).unwrap_or(ALL.len())
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Organisation => "Organisation",
            Self::Service => "Service",
            Self::Component => "Component",
            Self::Repository => "Repository",
            Self::Team => "Team",
            Self::Role => "Role",
            Self::User => "User",
            Self::ServiceAccount => "ServiceAccount",
            Self::Documentation => "Documentation",
            Self::DocumentationSource => "DocumentationSource",
            Self::CloudResource => "CloudResource",
            Self::Attribute => "Attribute",
            Self::Permission => "Permission",
        }
    }

    pub fn plural(self) -> &'static str {
        match self {
            Self::Organisation => "Organisations",
            Self::Service => "Services",
            Self::Component => "Components",
            Self::Repository => "Repositories",
            Self::Team => "Teams",
            Self::Role => "Roles",
            Self::User => "Users",
            Self::ServiceAccount => "ServiceAccounts",
            Self::Documentation => "Documentation",
            Self::DocumentationSource => "DocumentationSources",
            Self::CloudResource => "CloudResources",
            Self::Attribute => "Attributes",
            Self::Permission => "Permissions",
        }
    }

    /// How a list of this kind is headed on a page, in words rather than the name's own spelling.
    pub fn heading(self) -> &'static str {
        match self {
            Self::Organisation => "Organisations",
            Self::Service => "Services",
            Self::Component => "Components",
            Self::Repository => "Repositories",
            Self::Team => "Teams",
            Self::Role => "Roles",
            Self::User => "People",
            Self::ServiceAccount => "Service accounts",
            Self::Documentation => "Documentation",
            Self::DocumentationSource => "Documentation sources",
            Self::CloudResource => "Cloud resources",
            Self::Attribute => "Attributes",
            Self::Permission => "Permissions",
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Organisation => "organisation",
            Self::Service => "service",
            Self::Component => "component",
            Self::Repository => "repository",
            Self::Team => "team",
            Self::Role => "role",
            Self::User => "user",
            Self::ServiceAccount => "service-account",
            Self::Documentation => "documentation",
            Self::DocumentationSource => "documentation-source",
            Self::CloudResource => "cloud-resource",
            Self::Attribute => "attribute",
            Self::Permission => "permission",
        }
    }

    /// Kept in this plugin's own table; the rest are read from core or `rbac` and never copied.
    pub fn stored(self) -> bool {
        matches!(
            self,
            Self::Service
                | Self::Component
                | Self::Repository
                | Self::Documentation
                | Self::DocumentationSource
                | Self::CloudResource
        )
    }

    /// Kept by the platform and read from `core.*`: people and service accounts, and since T69
    /// organisations and teams, which the catalogue shows and writes but does not keep.
    pub fn in_core(self) -> bool {
        matches!(self, Self::Organisation | Self::Team | Self::User | Self::ServiceAccount)
    }

    /// Kinds this plugin writes: its own, and the platform's organisations and teams.
    pub fn written(self) -> bool {
        self.stored() || matches!(self, Self::Organisation | Self::Team)
    }

    /// Where a kind the catalogue does not write is made, for the page that lists them: a role is
    /// one of `rbac`'s groups, and people and service accounts are the platform's own.
    pub fn made(self) -> Option<Made> {
        match self {
            Self::Role => {
                Some(Made { href: "/p/rbac/groups", label: "New role in Access control" })
            }
            Self::User => Some(Made { href: "/users", label: "New person in Users" }),
            Self::DocumentationSource => {
                Some(Made { href: "/p/kb/sources/new", label: "New source in the Knowledge Base" })
            }
            Self::ServiceAccount => {
                Some(Made { href: "/service-accounts", label: "New service account" })
            }
            _ => None,
        }
    }

    /// The plugins whose sync events may create and update this kind, besides `apply`.
    pub fn publishers(self) -> &'static [&'static str] {
        match self {
            Self::Component => &["architecture"],
            Self::Repository => &["github", "ghe"],
            Self::Documentation | Self::DocumentationSource => &["kb"],
            Self::CloudResource => &["infra"],
            _ => &[],
        }
    }

    /// Singular or plural, in any case, with or without separators: `service-accounts` works.
    pub fn parse(text: &str) -> Option<Self> {
        let wanted: String =
            text.chars().filter(char::is_ascii_alphanumeric).collect::<String>().to_lowercase();
        // Organisations were called verticals, and older YAML, links and stored rows still say so.
        if matches!(wanted.as_str(), "vertical" | "verticals" | "organization" | "organizations") {
            return Some(Self::Organisation);
        }
        ALL.into_iter().find(|kind| {
            kind.name().eq_ignore_ascii_case(&wanted) || kind.plural().eq_ignore_ascii_case(&wanted)
        })
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl TryFrom<String> for Kind {
    type Error = String;

    fn try_from(text: String) -> Result<Self, Self::Error> {
        Self::parse(&text).ok_or_else(|| format!("`{text}` is not a kind of resource"))
    }
}

impl From<Kind> for String {
    fn from(kind: Kind) -> Self {
        kind.name().to_string()
    }
}

/// The lowercase words a search may start a kind with: its whole name, and each word of a name
/// of two, so `acc` finds service accounts. Organisations are also found as verticals.
pub fn words(kind: Kind) -> impl Iterator<Item = String> + Clone {
    let whole = kind.name().to_lowercase();
    let parts: Vec<String> = kind.slug().split('-').map(str::to_string).collect();
    let older = (kind == Kind::Organisation).then(|| "vertical".to_string());
    std::iter::once(whole).chain(parts.into_iter().skip(1)).chain(older)
}

pub fn kind(text: &str) -> Result<Kind, Refusal> {
    Kind::parse(text).ok_or_else(|| Refusal::bad(format!("`{text}` is not a kind of resource")))
}

/// A resource by kind and name, written `Kind:name`; names may themselves hold `:` and `/`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
pub struct Ref {
    pub kind: Kind,
    pub name: String,
}

impl Ref {
    pub fn new(kind: Kind, name: impl Into<String>) -> Self {
        Self { kind, name: name.into() }
    }

    pub fn parse(text: &str) -> Result<Self, Refusal> {
        let (kind_text, name) = text.split_once(':').ok_or_else(|| {
            Refusal::bad(format!("`{text}` names no kind: write it as Kind:name"))
        })?;
        let name = name.trim();
        if name.is_empty() {
            return Err(Refusal::bad(format!("`{text}` names no resource")));
        }
        Ok(Self { kind: kind(kind_text.trim())?, name: name.to_string() })
    }
}

impl fmt::Display for Ref {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.kind, self.name)
    }
}

#[cfg(test)]
mod renamed {
    use super::*;

    #[test]
    fn a_vertical_is_read_as_the_organisation_it_became() {
        for written in ["vertical", "Verticals", "Organisation", "organisations", "organization"] {
            assert_eq!(Kind::parse(written), Some(Kind::Organisation), "{written}");
        }
        let old = Ref::parse("Vertical:payments").expect("an old reference");
        assert_eq!(old.to_string(), "Organisation:payments");
        assert_eq!(Kind::Organisation.slug(), "organisation");
    }

    /// The enum's order and `ALL` are the same order, and both are the platform's. A kind added to
    /// one and not the other would sort one way in a list and another on the map.
    #[test]
    fn the_order_is_declared_once() {
        let mut sorted = ALL;
        sorted.sort();
        assert_eq!(sorted, ALL, "ALL is not in the enum's own order");
        for (at, kind) in ALL.iter().enumerate() {
            assert_eq!(kind.rank(), at, "{kind} is ranked wrong");
        }
        // Documentation belongs with what it documents, not at the far end of the drill-down.
        assert!(Kind::Documentation.rank() > Kind::Service.rank());
        assert!(Kind::Documentation.rank() < Kind::Repository.rank());
        assert!(Kind::DocumentationSource.rank() < Kind::Repository.rank());
    }
}
