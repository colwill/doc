//! Each product a scope's services run, judged: which release it is, where that release stands
//! today, and what to say about it. Pages, the API, the readiness route and agents all start here.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{Duration, NaiveDate};
use doc_plugin_sdk::Backend;
use serde_json::{Value, json};

use crate::Refusal;
use crate::lifecycle::{self, Status, day};
use crate::scope::{self, Member, Scope, Used};
use crate::settings::Definitions;
use crate::store::{Product, Release};
use crate::timeline::{Lane, Span};

/// One product one service runs, judged.
#[derive(Debug, Clone)]
pub struct Judged {
    pub service: String,
    pub title: String,
    pub product: String,
    pub label: String,
    pub version: Option<String>,
    pub release: Option<Release>,
    pub status: Status,
    pub why: String,
    /// The product's newest release, where it is not the one run.
    pub newest: Option<String>,
    /// Where it was said, empty for the service's own metadata in the Catalogue.
    pub from: Vec<String>,
}

impl Judged {
    /// Where it came from, in a phrase a page can put in a cell.
    pub fn said_where(&self) -> String {
        match self.from.is_empty() {
            true => scope::OWN.to_string(),
            false => self.from.join(", "),
        }
    }

    /// Whether it was worked out from a repository rather than named in the Catalogue.
    pub fn discovered(&self) -> bool {
        !self.from.is_empty()
    }

    /// What it is called on a page: `Node.js 20`, or the product alone where no release matched.
    pub fn named(&self) -> String {
        match (&self.release, &self.version) {
            (Some(release), _) => format!("{} {}", self.label, release.name),
            (None, Some(version)) => format!("{} {version}", self.label),
            (None, None) => self.label.clone(),
        }
    }

    pub fn href(&self) -> String {
        format!("/p/eol/product?{}", scope::encoded(&[("name", &self.product)]))
    }

    pub fn json(&self) -> Value {
        let release = self.release.as_ref();
        json!({
            "service": self.service,
            "product": self.product,
            "label": self.label,
            "version": self.version,
            "release": release.map(|release| &release.name),
            "status": self.status,
            "said": self.why,
            "active_support_ends": release.and_then(|release| release.support_ends),
            "end_of_life": release.and_then(|release| release.eol),
            "extended_support_ends": release.and_then(|release| release.extended_ends),
            "latest": release.and_then(|release| release.latest.clone()),
            "newest_release": self.newest,
            "from": self.from,
        })
    }
}

/// A scope's services, the products they run, and each judged.
pub struct View {
    pub members: Vec<Member>,
    pub products: BTreeMap<String, Product>,
    pub judged: Vec<Judged>,
    pub today: NaiveDate,
}

impl View {
    /// Whether any service here names anything it runs.
    pub fn anything(&self) -> bool {
        self.members.iter().any(|member| !member.used.is_empty())
    }

    pub fn of(&self, service: &str) -> Vec<&Judged> {
        self.judged.iter().filter(|judged| judged.service == service).collect()
    }
}

/// Where a release stands, in a sentence.
pub fn said(release: &Release, status: Status, today: NaiveDate) -> String {
    let ends = |prefix: &str| match release.eol {
        Some(eol) if eol <= today => format!("{prefix}since {}", day(eol)),
        Some(eol) => format!("{prefix}{}", day(eol)),
        None => prefix.trim_end().to_string(),
    };
    let extended = release
        .extended_ends
        .filter(|ends| *ends > today)
        .map(|ends| format!("; extended support until {}", day(ends)))
        .unwrap_or_default();
    match status {
        Status::Ended => format!("{}{extended}", ends("End of life ")),
        Status::Ending => format!("{}{extended}", ends("End of life on ")),
        Status::Security => match release.eol {
            Some(eol) => format!("Security fixes only, until {}", day(eol)),
            None => "Security fixes only".to_string(),
        },
        Status::Supported => match (release.support_ends, release.eol) {
            (Some(support), Some(eol)) if support < eol => {
                format!("Active support until {}, security fixes until {}", day(support), day(eol))
            }
            (_, Some(eol)) => format!("Supported until {}", day(eol)),
            _ => "Supported, with no end of life announced".to_string(),
        },
        Status::Unknown => "Not known".to_string(),
    }
}

fn judge(
    member: &Member,
    used: &Used,
    products: &BTreeMap<String, Product>,
    today: NaiveDate,
    warn: i64,
) -> Judged {
    let product = products.get(&used.product);
    let label = product.map_or_else(
        || used.product.strip_prefix("url:").unwrap_or(&used.product).to_string(),
        |product| product.label.clone(),
    );
    let mut judged = Judged {
        service: member.name.clone(),
        title: member.title.clone(),
        product: used.product.clone(),
        label,
        version: used.version.clone(),
        release: None,
        status: Status::Unknown,
        why: String::new(),
        newest: None,
        from: used.from.clone(),
    };
    let Some(product) = product else {
        judged.why = "Its release cycles have not been read yet".into();
        return judged;
    };
    if product.releases.is_empty() {
        judged.why = product.problem.clone().unwrap_or_else(|| "No releases are known".into());
        return judged;
    }
    let newest = product.releases.first().map(|release| release.name.clone());
    let Some(version) = &used.version else {
        judged.why = match used.discovered() {
            true => format!(
                "No version is named there: add {}@<version> to the service in the Catalogue \
                 to have it judged",
                used.product
            ),
            false => format!(
                "No version is named: write {}@<version> in the Catalogue to have it judged",
                used.product
            ),
        };
        judged.newest = newest;
        return judged;
    };
    let Some(release) = lifecycle::matched(product, version) else {
        judged.why = format!("{} has no release {version}", product.label);
        judged.newest = newest;
        return judged;
    };
    judged.status = release.status(today, warn);
    judged.why = said(release, judged.status, today);
    judged.newest = newest.filter(|newest| *newest != release.name);
    judged.release = Some(release.clone());
    judged
}

/// What one service runs, judged.
///
/// Two files can name the same release in different words — a Dockerfile's `node:20` and a
/// `package.json`'s `>=20.10` are both Node.js 20 — so once each is judged they are one row again,
/// naming every place it was said. Only entries that landed on the same release are folded
/// together: 18 and 20 stay apart, because they are two different things to worry about.
pub fn judged_one(
    member: &Member,
    products: &BTreeMap<String, Product>,
    today: NaiveDate,
    warn_days: i64,
) -> Vec<Judged> {
    let mut judged: Vec<Judged> = Vec::new();
    for used in &member.used {
        let one = judge(member, used, products, today, warn_days);
        let same = judged.iter_mut().find(|held| {
            held.product == one.product
                && held.release.as_ref().map(|release| &release.name)
                    == one.release.as_ref().map(|release| &release.name)
                && (held.release.is_some() || held.version == one.version)
        });
        match same {
            Some(held) => {
                held.from.extend(one.from);
                held.from.sort();
                held.from.dedup();
                // The more exact of the two versions is the one to show.
                if one.version.as_ref().map_or(0, String::len)
                    > held.version.as_ref().map_or(0, String::len)
                {
                    held.version = one.version;
                }
            }
            None => judged.push(one),
        }
    }
    judged
}

/// A scope's services and what they run, judged today.
pub async fn read(backend: &Backend, scope: &Scope) -> Result<View, Refusal> {
    let members = scope::members(backend, scope).await?;
    let wanted: BTreeSet<String> = members
        .iter()
        .flat_map(|member| member.used.iter().map(|used| used.product.clone()))
        .collect();
    let products = lifecycle::products(backend, &wanted).await?;
    let definitions = Definitions::read(&backend.settings());
    let today = lifecycle::today();
    let mut judged: Vec<Judged> = members
        .iter()
        .flat_map(|member| judged_one(member, &products, today, definitions.warn_days))
        .collect();
    judged.sort_by(|one, two| {
        (one.status, one.release.as_ref().and_then(|r| r.eol), &one.service).cmp(&(
            two.status,
            two.release.as_ref().and_then(|r| r.eol),
            &two.service,
        ))
    });
    Ok(View { members, products, judged, today })
}

/// A release on a timeline: active support, then security fixes only, then extended support;
/// one still `in_use` past its end of life runs on in red to today.
pub fn lane(
    label: String,
    href: Option<String>,
    release: &Release,
    today: NaiveDate,
    in_use: bool,
) -> Lane {
    let start = release.released.unwrap_or(today - Duration::days(365));
    let support = release.support_ends.or(release.eol);
    let mut spans = Vec::new();
    let tone_of = |ended: bool| if ended { "muted" } else { "ready" };
    match (support, release.eol) {
        (Some(support), Some(eol)) if support < eol => {
            spans.push(Span {
                from: start,
                to: Some(support),
                tone: tone_of(support <= today),
                said: format!("Active support until {}", day(support)),
            });
            spans.push(Span {
                from: support,
                to: Some(eol),
                tone: if eol <= today { "muted" } else { "security" },
                said: format!("Security fixes only, until {}", day(eol)),
            });
        }
        (_, Some(eol)) => spans.push(Span {
            from: start,
            to: Some(eol),
            tone: tone_of(eol <= today),
            said: format!("Supported until {}", day(eol)),
        }),
        (_, None) => spans.push(Span {
            from: start,
            to: None,
            tone: if release.support_ended { "security" } else { "ready" },
            said: "No end of life announced".into(),
        }),
    }
    if let (Some(eol), Some(extended)) = (release.eol, release.extended_ends)
        && extended > eol
    {
        spans.push(Span {
            from: eol,
            to: Some(extended),
            tone: if extended <= today { "muted" } else { "done" },
            said: format!("Extended support until {}", day(extended)),
        });
    }
    if in_use
        && let Some(eol) = release.eol.filter(|eol| *eol < today)
        && release.extended_ends.is_none_or(|extended| extended < today)
    {
        spans.push(Span {
            from: release.extended_ends.unwrap_or(eol).max(eol),
            to: Some(today),
            tone: "error",
            said: format!("Run past its end of life, since {}", day(eol)),
        });
    }
    Lane { label, href, spans, marks: Vec::new() }
}

/// The stretch of time a set of releases is drawn over: from the oldest one's release, at most
/// three years back, to the latest end, at most four years on and at least a year on.
pub fn window(releases: &[&Release], today: NaiveDate) -> (NaiveDate, NaiveDate) {
    let earliest = releases.iter().filter_map(|release| release.released).min().unwrap_or(today);
    let latest = releases
        .iter()
        .filter_map(|release| release.extended_ends.or(release.eol))
        .max()
        .unwrap_or(today);
    let from = earliest.max(today - Duration::days(3 * 365)) - Duration::days(30);
    let to = latest.clamp(today + Duration::days(365), today + Duration::days(4 * 365))
        + Duration::days(30);
    (from, to)
}
