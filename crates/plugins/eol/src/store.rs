//! What end of life keeps: DOC's own copy of every product endoflife.date tracks, as it answered,
//! and each service's own lifecycle file as last read; and what each repository connected to a
//! service was found to use. What a service's metadata names is read from the Catalogue instead.

use chrono::{DateTime, NaiveDate, Utc};
use doc_plugin_sdk::{Collection, Declaration, Field, ListOf};
use serde::{Deserialize, Serialize};
use serde_json::json;

pub fn declaration() -> Declaration {
    Declaration::default()
        .collection(
            "products",
            Collection::new()
                .field(
                    "product",
                    Field::text().key().describe(
                        "endoflife.date's name for it, or url:<address> for a service's own",
                    ),
                )
                .field("label", Field::text().required())
                .field("link", Field::text().max(2_000.0))
                .field("category", Field::text().max(64.0))
                .field("releases", Field::json().describe("Its release cycles, newest first"))
                .field("read_at", Field::timestamp().describe("When it was last read"))
                .field(
                    "tried_at",
                    Field::timestamp().describe("When it was last asked for, read or not"),
                )
                .field(
                    "problem",
                    Field::text().max(500.0).describe("Why it could not be read last time"),
                )
                .field(
                    "document",
                    Field::json().describe("endoflife.date's own answer for it, served as it was"),
                )
                .field("digest", Field::text().max(64.0).describe("Of that answer"))
                .field(
                    "changed_at",
                    Field::timestamp().describe("When endoflife.date's answer for it last changed"),
                )
                .field("listed", Field::boolean().describe("Whether endoflife.date still lists it"))
                .field("aliases", Field::list(ListOf::Text).default(json!([])))
                .field("tags", Field::list(ListOf::Text).default(json!([])))
                .field(
                    "packages",
                    Field::list(ListOf::Text)
                        .default(json!([]))
                        .describe("The packages it is published as, such as npm/react"),
                )
                .index(&["read_at"]),
        )
        .collection(
            "repositories",
            Collection::new()
                .field("repository", Field::text().key().describe("As the Catalogue names it"))
                .field(
                    "services",
                    Field::list(ListOf::Text)
                        .required()
                        .default(json!([]))
                        .describe("The services it is connected to in the Catalogue"),
                )
                .field("found", Field::json().describe("What its files say it is built on"))
                .field("files", Field::integer().describe("How many of its files were read"))
                .field(
                    "packages",
                    Field::json().describe(
                        "The products among the packages Repository Insights listed in it",
                    ),
                )
                .field("commit", Field::text().describe("The commit those packages were listed at"))
                .field(
                    "listed_at",
                    Field::timestamp().describe("When Repository Insights listed them"),
                )
                .field(
                    "listed",
                    Field::integer().describe("How many packages Repository Insights listed in it"),
                )
                .field(
                    "pushed_at",
                    Field::text().max(64.0).describe("When it was last pushed to, as read"),
                )
                .field("read_at", Field::timestamp())
                .field("problem", Field::text().max(500.0).describe("Why it could not be read"))
                .index(&["read_at"]),
        )
}

/// A repository as end of life last read it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Scanned {
    pub repository: String,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub found: Vec<crate::manifests::Found>,
    #[serde(default)]
    pub files: Option<i64>,
    /// The products among its packages, as Repository Insights last listed them. A record kept
    /// before they were holds none.
    #[serde(default, deserialize_with = "or_none")]
    pub packages: Vec<crate::manifests::Found>,
    #[serde(default)]
    pub commit: Option<String>,
    #[serde(default)]
    pub listed_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub listed: Option<i64>,
    #[serde(default)]
    pub pushed_at: Option<String>,
    #[serde(default)]
    pub read_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub problem: Option<String>,
}

fn or_none<'de, D, T>(deserializer: D) -> Result<Vec<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Ok(Option::<Vec<T>>::deserialize(deserializer)?.unwrap_or_default())
}

/// One release cycle of a product, such as Node.js 20: when it came out, when active support
/// ends, when its end of life is — the end of security fixes — and when any extended, usually
/// paid, support ends.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Release {
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub released: Option<NaiveDate>,
    #[serde(default)]
    pub lts: bool,
    /// The end of active support: bug fixes stop, security fixes carry on.
    #[serde(default)]
    pub support_ends: Option<NaiveDate>,
    /// Active support has ended, where no date is given for it.
    #[serde(default)]
    pub support_ended: bool,
    #[serde(default)]
    pub eol: Option<NaiveDate>,
    /// It has reached its end of life, where no date is given for it.
    #[serde(default)]
    pub ended: bool,
    #[serde(default)]
    pub extended_ends: Option<NaiveDate>,
    #[serde(default)]
    pub latest: Option<String>,
    #[serde(default)]
    pub latest_on: Option<NaiveDate>,
    #[serde(default)]
    pub link: Option<String>,
}

/// A product's fields without endoflife.date's own answer for it, which is large.
pub const SUMMARY: [&str; 12] = [
    "product",
    "label",
    "link",
    "category",
    "releases",
    "read_at",
    "tried_at",
    "problem",
    "changed_at",
    "listed",
    "aliases",
    "tags",
];

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Product {
    pub product: String,
    pub label: String,
    #[serde(default)]
    pub link: Option<String>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub releases: Vec<Release>,
    #[serde(default)]
    pub read_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub tried_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub problem: Option<String>,
    #[serde(default)]
    pub changed_at: Option<DateTime<Utc>>,
    /// `Some(false)` once endoflife.date stops listing it; kept before the copy was, `None`.
    #[serde(default)]
    pub listed: Option<bool>,
    #[serde(default, deserialize_with = "or_none")]
    pub aliases: Vec<String>,
    #[serde(default, deserialize_with = "or_none")]
    pub tags: Vec<String>,
}

impl Product {
    /// endoflife.date stopped listing it, and this is what it said when it last did.
    pub fn dropped(&self) -> bool {
        self.listed == Some(false)
    }
}
