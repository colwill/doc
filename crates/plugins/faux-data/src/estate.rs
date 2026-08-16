//! The estate faux data is made up for: every service a plugin asks about, each with one profile
//! that holds in every plugin — so a failing service deploys rarely, breaks CI, misses its
//! objectives, runs releases past their end of life and ships late, everywhere at once — and the
//! stand-ins its data is made up for: a repository, a Jira project, the products it runs and a
//! health subject, named so none can be taken for a real one.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::{Value, json};

use crate::dice::Dice;
use crate::settings::Config;

/// How well a service does, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    Thriving,
    Steady,
    Struggling,
    Failing,
}

impl Profile {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "thriving" => Some(Self::Thriving),
            "steady" => Some(Self::Steady),
            "struggling" => Some(Self::Struggling),
            "failing" => Some(Self::Failing),
            _ => None,
        }
    }

    /// Its place, best first, which each generator's table of behaviours is indexed by.
    pub fn index(self) -> usize {
        self as usize
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Thriving => "Thriving",
            Self::Steady => "Steady",
            Self::Struggling => "Struggling",
            Self::Failing => "Failing",
        }
    }

    pub fn badge(self) -> &'static str {
        match self {
            Self::Thriving | Self::Steady => "ready",
            Self::Struggling => "degraded",
            Self::Failing => "error",
        }
    }

    /// Most services do well and a few do not: a share from 0 to 1 as a profile.
    pub fn of_share(share: f64) -> Self {
        match share {
            share if share < 0.35 => Self::Thriving,
            share if share < 0.7 => Self::Steady,
            share if share < 0.95 => Self::Struggling,
            _ => Self::Failing,
        }
    }
}

/// A service's profile: as the settings name it, or else from its name.
pub fn profile(config: &Config, service: &str) -> Profile {
    match config.profiles.get(service) {
        Some(profile) => *profile,
        None => Profile::of_share(Dice::seeded(config.seed, service, -1).unit()),
    }
}

/// The repository a service's pipelines and deployments are made up for.
pub fn repository(service: &str) -> String {
    format!("faux/{}", service.to_ascii_lowercase())
}

/// The service a stand-in repository is for.
pub fn of_repository(repository: &str) -> &str {
    repository.strip_prefix("faux/").unwrap_or(repository)
}

/// A service's Jira project: the first four letters of its name.
pub fn project(service: &str) -> String {
    let letters: String = service.chars().filter(char::is_ascii_alphabetic).take(4).collect();
    match letters.is_empty() {
        true => "FAUX".to_string(),
        false => letters.to_ascii_uppercase(),
    }
}

/// What a service runs, `product@version` as a Catalogue would say it: all current when it
/// thrives; one release on security fixes only; two ending soon; several past their end of life,
/// and a product named without a version, when it fails.
pub fn products(profile: Profile) -> &'static [&'static str] {
    const RUNS: [&[&str]; 4] = [
        &["nodejs@24.9.0", "postgresql@17", "redis@7.4", "ubuntu@24.04"],
        &["nodejs@22", "react@19", "python@3.12", "postgresql@15"],
        &["eclipse-temurin@17", "spring-boot@3.3", "postgresql@16", "redis@7.2", "ubuntu@22.04"],
        &[
            "eclipse-temurin@11",
            "spring-boot@2.7.18",
            "postgresql@13",
            "ubuntu@20.04",
            "nodejs@20",
            "python",
        ],
    ];
    RUNS[profile.index()]
}

/// Everything a plugin needs to know of one service to ask for its faux data.
pub fn of_service(config: &Config, service: &str) -> Value {
    let profile = profile(config, service);
    json!({
        "profile": profile,
        "repositories": [repository(service)],
        "projects": [project(service)],
        "products": products(profile),
        "subject": format!("service:{service}"),
    })
}

/// `estate?service=a&service=b`: each service asked about, and the services made up where the
/// Catalogue has none.
pub fn answer(config: &Config, services: &[String]) -> Value {
    let services: BTreeMap<&String, Value> =
        services.iter().map(|service| (service, of_service(config, service))).collect();
    let defaults: Vec<Value> = config
        .services
        .iter()
        .map(|(name, title)| json!({ "name": name, "title": title }))
        .collect();
    json!({ "services": services, "defaults": defaults })
}
