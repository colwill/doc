//! What a maturity model is made of: the kinds of thing it grades, the criteria it grades them
//! by, how each criterion is decided, and the grade that comes out.
//!
//! A criterion is deliberately small — one thing that is either met or not — because a grade
//! people argue with is a grade nobody acts on. What makes a model an organisation's or a team's
//! is which criteria it holds and what each is worth, not a different way of scoring.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

/// The best and worst a component can be graded, which is the whole scale people see.
pub const BEST: u8 = 10;
pub const WORST: u8 = 1;

/// What a criterion is about, which is also what a model grades. These are the Catalogue's own
/// kinds, so a criterion is written against the thing it judges and nothing has to be mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    Service,
    Repository,
    Documentation,
    CloudResource,
}

impl Kind {
    pub const ALL: [Self; 4] =
        [Self::Service, Self::Repository, Self::Documentation, Self::CloudResource];

    /// As a URL and the Catalogue's API name it.
    pub fn id(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Repository => "repository",
            Self::Documentation => "documentation",
            Self::CloudResource => "cloud-resource",
        }
    }

    /// As the Catalogue writes it in a resource reference and a document.
    pub fn catalogue(self) -> &'static str {
        match self {
            Self::Service => "Service",
            Self::Repository => "Repository",
            Self::Documentation => "Documentation",
            Self::CloudResource => "CloudResource",
        }
    }

    pub fn one(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Repository => "repository",
            Self::Documentation => "documentation page",
            Self::CloudResource => "cloud resource",
        }
    }

    pub fn many(self) -> &'static str {
        match self {
            Self::Service => "Services",
            Self::Repository => "Repositories",
            Self::Documentation => "Documentation",
            Self::CloudResource => "Cloud resources",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        let wanted = text.trim().to_ascii_lowercase().replace('_', "-");
        Self::ALL
            .into_iter()
            .find(|kind| kind.id() == wanted || kind.catalogue().eq_ignore_ascii_case(&wanted))
    }
}

/// How a criterion is decided.
///
/// Every one of these is answered from something the platform already knows, so turning a model on
/// costs nobody any work: the Catalogue says what a thing is connected to and who owns it, and the
/// plugins that measure a service already answer `api/readiness` for it (DOC-SPEC §9.2). `Manual`
/// is the way out for what no plugin can see, and it says who attested it and when.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "check", rename_all = "kebab-case")]
pub enum Check {
    /// Somebody says it is met, and their name and the day stand beside it. The default, since a
    /// criterion nobody has said how to decide is one only a person can.
    #[default]
    Manual,
    /// It is connected to at least `least` things of a kind in the Catalogue: a service with a
    /// repository, a repository with documentation.
    Connected {
        kind: String,
        #[serde(default = "one")]
        least: u32,
    },
    /// The Catalogue names a team that owns it.
    Owned,
    /// Its metadata in the Catalogue has `key` set, and where `one_of` is given, set to one of
    /// those: `lifecycle` being `production`, say.
    Metadata {
        key: String,
        #[serde(default)]
        one_of: Vec<String>,
    },
    /// A plugin that measures services says it is ready — the same answer the roadmap shows —
    /// with `warning` allowed or not.
    Readiness {
        plugin: String,
        #[serde(default)]
        allow_warning: bool,
    },
    /// Another model says the same component is at least this far along: what a production model
    /// asks of a service before anything else, that it is development ready first.
    ///
    /// The model it names is scored before this one, so the grade it reads is this run's and not
    /// yesterday's, and a model may not stand on one that stands on it.
    Model {
        model: Uuid,
        #[serde(default = "ten")]
        least: u8,
    },
}

fn one() -> u32 {
    1
}

/// Meeting everything, which is what "it has passed that model" means unless somebody says less.
fn ten() -> u8 {
    BEST
}

impl Check {
    /// What kind of check it is, as a form and the API name it.
    pub fn id(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Connected { .. } => "connected",
            Self::Owned => "owned",
            Self::Metadata { .. } => "metadata",
            Self::Readiness { .. } => "readiness",
            Self::Model { .. } => "model",
        }
    }

    /// The model this criterion stands on, where it stands on one.
    pub fn stands_on(&self) -> Option<Uuid> {
        match self {
            Self::Model { model, .. } => Some(*model),
            _ => None,
        }
    }

    /// Whether anything but a person can decide it.
    pub fn automatic(&self) -> bool {
        !matches!(self, Self::Manual)
    }

    /// What it looks for, in a sentence, for a page and for the reason on a scorecard.
    pub fn about(&self) -> String {
        match self {
            Self::Manual => "Somebody attests to it".to_string(),
            Self::Connected { kind, least } => match Kind::parse(kind) {
                Some(kind) if *least <= 1 => {
                    format!("It is connected to a {} in the Catalogue", kind.one())
                }
                Some(kind) => format!(
                    "It is connected to {least} {} in the Catalogue",
                    kind.many().to_lowercase()
                ),
                None => format!("It is connected to {least} {kind} in the Catalogue"),
            },
            Self::Owned => "The Catalogue names a team that owns it".to_string(),
            Self::Metadata { key, one_of } if one_of.is_empty() => {
                format!("Its metadata sets `{key}`")
            }
            Self::Metadata { key, one_of } => {
                format!("Its metadata sets `{key}` to {}", or_list(one_of))
            }
            Self::Readiness { plugin, allow_warning: false } => {
                format!("{plugin} says it is ready")
            }
            Self::Readiness { plugin, allow_warning: true } => {
                format!("{plugin} does not say it is blocked")
            }
            Self::Model { least, .. } if *least >= BEST => {
                "It meets another model in full".to_string()
            }
            Self::Model { least, .. } => {
                format!("It grades at least {least} against another model")
            }
        }
    }

    /// The same sentence with the model named, where the name is to hand.
    pub fn about_with(&self, named: impl Fn(Uuid) -> Option<String>) -> String {
        match self {
            Self::Model { model, least } => {
                let name = named(*model).unwrap_or_else(|| "a model that has gone".to_string());
                match *least >= BEST {
                    true => format!("It meets {name} in full"),
                    false => format!("It grades at least {least} against {name}"),
                }
            }
            other => other.about(),
        }
    }

    /// The kinds this check can decide. A readiness route answers about services, so a criterion
    /// that asks one is only ever about a service.
    pub fn suits(&self, kind: Kind) -> bool {
        match self {
            Self::Readiness { .. } => kind == Kind::Service,
            Self::Owned => kind != Kind::Documentation,
            _ => true,
        }
    }

    /// Why it cannot be used for `kind`, for the form to say.
    pub fn unsuitable(&self, kind: Kind) -> Option<String> {
        if self.suits(kind) {
            return None;
        }
        Some(match self {
            Self::Readiness { .. } => format!(
                "the plugins that measure readiness answer about services, not a {}",
                kind.one()
            ),
            _ => format!("the Catalogue keeps no owner for a {}", kind.one()),
        })
    }
}

/// `a`, `a or b`, `a, b or c`.
fn or_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => format!("`{one}`"),
        [rest @ .., last] => {
            let rest: Vec<String> = rest.iter().map(|item| format!("`{item}`")).collect();
            format!("{} or `{last}`", rest.join(", "))
        }
    }
}

/// A grade from 1 to 10: one for a component that meets nothing, ten for one that meets every
/// criterion it is held to. Nothing in between is a pass or a fail — the point of a scale is that
/// somewhere in the middle is where most things are, and moving up is the work.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Grade(u8);

impl Default for Grade {
    fn default() -> Self {
        Self(WORST)
    }
}

impl Grade {
    /// A grade from what was met and what was asked, by weight. Meeting everything is the only way
    /// to ten, and meeting nothing is one, so neither end can be reached by rounding.
    pub fn of(met: u32, asked: u32) -> Self {
        if asked == 0 || met == 0 {
            return Self(WORST);
        }
        if met >= asked {
            return Self(BEST);
        }
        let span = f64::from(BEST - WORST);
        let share = f64::from(met) / f64::from(asked);
        // Rounded, then held below ten: only everything is ten.
        let grade = f64::from(WORST) + (share * span).round();
        Self((grade as u8).clamp(WORST, BEST - 1))
    }

    pub fn value(self) -> u8 {
        self.0
    }

    /// The modifier the design system colours it by: `doc-grade--1` is red, `--10` green.
    pub fn class(self) -> String {
        format!("doc-grade--{}", self.0)
    }

    /// What it is called, so a page never shows a bare number.
    pub fn word(self) -> &'static str {
        match self.0 {
            1..=2 => "Just started",
            3..=4 => "Getting there",
            5..=6 => "Halfway",
            7..=8 => "Nearly there",
            9 => "Almost all",
            _ => "Meets everything",
        }
    }

    pub fn parse(value: i64) -> Self {
        Self((value.clamp(i64::from(WORST), i64::from(BEST))) as u8)
    }
}

/// A component a model grades: a resource in the Catalogue, written the way everything else in
/// DOC writes one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Component {
    pub kind: Kind,
    pub name: String,
}

impl Component {
    pub fn reference(&self) -> String {
        format!("{}:{}", self.kind.id(), self.name)
    }

    pub fn parse(reference: &str) -> Option<Self> {
        let (kind, name) = reference.split_once(':')?;
        let name = name.trim();
        (!name.is_empty())
            .then(|| Some(Self { kind: Kind::parse(kind)?, name: name.to_string() }))
            .flatten()
    }

    /// Its page in the Catalogue.
    pub fn href(&self) -> String {
        format!("/p/resources/r/{}/{}", self.kind.id(), self.name)
    }
}

/// Who a model belongs to: the organisation as a whole, or one team. The same choice a Knowledge
/// Base space makes (DOC-SPEC §9.2), and for the same reason — somebody has to keep it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Owner {
    pub kind: String,
    pub name: String,
}

impl Owner {
    pub const ORGANISATION: &'static str = "organisation";
    pub const TEAM: &'static str = "team";

    pub fn parse(reference: &str) -> Result<Self, String> {
        let (kind, name) = reference
            .split_once(':')
            .ok_or_else(|| format!("write who keeps it as kind:name, not `{reference}`"))?;
        let kind = kind.trim().to_ascii_lowercase();
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err("name the organisation or team that keeps it".to_string());
        }
        if kind != Self::ORGANISATION && kind != Self::TEAM {
            return Err(format!("a model is kept by an organisation or a team, not by `{kind}`"));
        }
        Ok(Self { kind, name })
    }

    pub fn reference(&self) -> String {
        format!("{}:{}", self.kind, self.name)
    }

    pub fn is_organisation(&self) -> bool {
        self.kind == Self::ORGANISATION
    }

    pub fn label(&self) -> String {
        match self.is_organisation() {
            true => format!("the {} organisation", self.name),
            false => format!("team {}", self.name),
        }
    }

    pub fn href(&self) -> String {
        format!("/p/resources/r/{}/{}", self.kind, self.name)
    }
}

/// What one criterion came to for one component.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Result_ {
    pub criterion: String,
    pub title: String,
    pub weight: u32,
    pub met: bool,
    /// Why, in the words a page shows: what was looked for and what was found.
    pub why: String,
    /// True where nothing could be read, so a page can tell "not met" from "not known".
    #[serde(default)]
    pub unknown: bool,
}

impl Result_ {
    pub fn json(&self) -> Value {
        json!({
            "criterion": self.criterion,
            "title": self.title,
            "weight": self.weight,
            "met": self.met,
            "why": self.why,
            "unknown": self.unknown,
        })
    }
}
