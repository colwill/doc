//! What `faux-data` provides and to whom: a toggle for each plugin it knows it can provide faux
//! data to, and the estate it makes the data up for — a seed, each service's profile, and the
//! services to make up where the Catalogue has none.

use std::collections::{BTreeMap, BTreeSet};

use doc_plugin_sdk::{Setting, SettingKind, Settings};
use serde_json::json;

use crate::estate::Profile;

pub const SEED: &str = "seed";
pub const PROFILES: &str = "profiles";
pub const SERVICES: &str = "services";

/// The plugins `faux-data` knows it can provide faux data to: each one's ID, what it is called,
/// and what it is given. Each ID is also the key of its toggle.
pub const CONSUMERS: [(&str, &str, &str); 6] = [
    (
        "dora",
        "DORA metrics",
        "Deployments, the commits they shipped, pull requests and incidents, as GitHub's delivery \
         data would give them.",
    ),
    (
        "cicd",
        "CI/CD/CT metrics",
        "GitHub Actions workflow runs of integration, delivery and testing, as GitHub's pipeline \
         data would give them.",
    ),
    (
        "reliability",
        "Reliability",
        "Outages, backups, restores and time watched, for services and for DOC's own parts and \
         plugins, as health checks, reports and DOC's status history would give them.",
    ),
    (
        "eol",
        "End of life",
        "What each service runs, and the release cycles endoflife.date would give for it.",
    ),
    (
        "roadmap",
        "Delivery roadmap",
        "Jira projects, their releases and the issues in them, as Jira's release data would give \
         them.",
    ),
    (
        "kb",
        "Knowledge Base",
        "A space of docs for each service's repository, read by a GitHub source, with the images \
         and files its pages attach, and an engineering handbook linked to every service.",
    ),
];

/// The services made up where the Catalogue has none, as `name=Title` lines.
const EXAMPLES: [&str; 4] = [
    "card-gateway=Card gateway",
    "developer-portal=Developer portal",
    "chargebacks=Chargebacks",
    "ledger=Ledger",
];

pub fn declared() -> Vec<Setting> {
    let mut settings: Vec<Setting> = CONSUMERS
        .iter()
        .map(|(id, title, gives)| {
            Setting::new(id, &format!("{title} ({id})"), SettingKind::Boolean)
                .defaulting(json!(false))
                .hinted(&format!(
                    "{gives} Its pages, panels, API and agents then show faux data, and say so at \
                     the top; nothing it keeps is changed, and turning this off shows live data \
                     again at once."
                ))
                .grouped("Provide faux data to")
        })
        .collect();
    settings.extend([
        Setting::new(SEED, "Seed", SettingKind::Number)
            .defaulting(json!(0))
            .between(0.0, 1_000_000.0)
            .hinted(
                "Another number makes another estate: every service's data comes out \
                 differently, but each still tells the story its profile gives it.",
            )
            .grouped("Estate"),
        Setting::new(PROFILES, "Profiles", SettingKind::List)
            .defaulting(json!([
                "card-gateway=thriving",
                "developer-portal=steady",
                "chargebacks=struggling",
                "ledger=failing",
            ]))
            .hinted(
                "service=thriving, steady, struggling or failing, a line each: how well a \
                 service does in every plugin at once. A service not named is given one from \
                 its name.",
            )
            .grouped("Estate"),
        Setting::new(SERVICES, "Services when the Catalogue has none", SettingKind::List)
            .defaulting(json!(EXAMPLES))
            .hinted(
                "name=Title, a line each: made up too, so a new platform has something to show.",
            )
            .grouped("Estate"),
    ]);
    settings
}

/// What the settings come to.
#[derive(Debug, Clone)]
pub struct Config {
    /// The plugins whose toggle is on.
    pub serving: BTreeSet<String>,
    pub seed: u64,
    pub profiles: BTreeMap<String, Profile>,
    /// Name and title.
    pub services: Vec<(String, String)>,
}

fn pairs(lines: Vec<String>) -> Vec<(String, String)> {
    lines
        .into_iter()
        .filter_map(|line| {
            let (name, value) = line.split_once('=')?;
            let name = name.trim().to_ascii_lowercase();
            (!name.is_empty()).then(|| (name, value.trim().to_string()))
        })
        .collect()
}

impl Config {
    pub fn read(settings: &Settings) -> Self {
        Self {
            serving: CONSUMERS
                .iter()
                .filter(|(id, ..)| settings.boolean(id))
                .map(|(id, ..)| (*id).to_string())
                .collect(),
            seed: settings.integer(SEED).unwrap_or_default().max(0) as u64,
            profiles: pairs(settings.list(PROFILES))
                .into_iter()
                .filter_map(|(name, profile)| Some((name, Profile::parse(&profile)?)))
                .collect(),
            services: pairs(settings.list(SERVICES))
                .into_iter()
                .map(|(name, title)| {
                    let title = if title.is_empty() { name.clone() } else { title };
                    (name, title)
                })
                .collect(),
        }
    }
}
