//! What end-of-life data is read from and judged by: endoflife.date, or a copy a platform runs of
//! its own; the hosts a service may point its own lifecycle data at; and how far ahead an ending is
//! worth warning about.

use doc_plugin_sdk::{Feature, Setting, SettingKind, Settings};
use serde_json::json;

pub const LIFECYCLE: &str = "lifecycle";
pub const REPOSITORIES: &str = "repositories";

pub const BASE_URL: &str = "base-url";
pub const CUSTOM_HOSTS: &str = "custom-hosts";
pub const WARN_WITHIN: &str = "warn-within";
pub const REFRESH_SCHEDULE: &str = "refresh-schedule";
pub const REFRESH: &str = "40 4 * * *";
pub const SOURCES: &str = "sources";
pub const READ_SCHEDULE: &str = "read-schedule";
pub const READ: &str = "10 3 * * *";
/// The most repositories read in one run, however many a platform has.
pub const MOST_READ: i64 = 500;

const DAY: f64 = 86_400.0;
const ENDOFLIFE: &str = "https://endoflife.date";

pub fn declared() -> Vec<Setting> {
    vec![
        Setting::new(BASE_URL, "endoflife.date", SettingKind::Url)
            .defaulting(json!(ENDOFLIFE))
            .hinted(
                "Where release cycles are read from: endoflife.date, or a copy of it your \
                 organisation runs. Its /api/v1/products/full is read in one request, and DOC \
                 keeps its own copy of every product, which pages, judging and teams' tools read \
                 instead.",
            )
            .grouped("Sources")
            .of_feature(LIFECYCLE),
        Setting::new(
            CUSTOM_HOSTS,
            "Hosts a service's own lifecycle data may come from",
            SettingKind::List,
        )
        .hinted(
            "A service can point endoflife.date/url-location at a JSON file of its own \
                 product's releases, in endoflife.date's format. It is read only from a host \
                 listed here, such as raw.githubusercontent.com or an internal docs host, since \
                 anybody who can change a service in the Catalogue could otherwise have DOC fetch \
                 any address.",
        )
        .defaulting(json!([]))
        .grouped("Sources")
        .of_feature(LIFECYCLE),
        Setting::new(SOURCES, "Plugins that give archives of a repository", SettingKind::List)
            .defaulting(json!(["github"]))
            .hinted(
                "A repository's runtimes are read from its files through an archive link its \
                 source plugin hands out, the way Repository Insights reads one. Name the plugins \
                 to ask, such as github or ghe; the first that answers for a repository is used.",
            )
            .grouped("Repositories")
            .of_feature(REPOSITORIES),
        Setting::new(READ_SCHEDULE, "When to read repositories again", SettingKind::Cron)
            .defaulting(json!(READ))
            .hinted(
                "A cron expression in UTC. Which services each repository belongs to is read \
                 again then, with any packages Repository Insights has listed since. A scan it \
                 finishes is read at once anyway, and a connection changed in the Catalogue a \
                 couple of minutes later. Where runtimes are read from a repository's files, it \
                 is fetched again only when it has been pushed to since.",
            )
            .grouped("Repositories")
            .of_feature(LIFECYCLE),
        Setting::new(WARN_WITHIN, "Warn of an end of life within", SettingKind::Duration)
            .defaulting(json!(180.0 * DAY))
            .hinted("A release that reaches its end of life within this long is Ending soon.")
            .grouped("Judging"),
        Setting::new(REFRESH_SCHEDULE, "When to read release cycles again", SettingKind::Cron)
            .defaulting(json!(REFRESH))
            .hinted(
                "A cron expression in UTC. Every product endoflife.date tracks is read again then, \
                 in one request, and each service's own lifecycle file. A read that fails keeps \
                 the copy as it was and is tried again when End of life next loads.",
            )
            .grouped("Sources")
            .of_feature(LIFECYCLE),
    ]
}

pub fn features() -> Vec<Feature> {
    vec![
        Feature::new(
            LIFECYCLE,
            "End-of-life data",
            "Works out what each service runs from the repositories connected to it — the \
             packages Repository Insights finds in their lockfiles when it scans them with ccc — \
             and from anything its metadata names, reads each product's release cycles from \
             endoflife.date, and shows which are supported, ending soon or past their end of \
             life.",
        )
        .on(),
        Feature::new(
            REPOSITORIES,
            "Runtimes from repository files",
            "ccc lists the packages a repository locks, not the runtimes, images and databases \
             it names elsewhere. This reads those from the files a repository already keeps — \
             Dockerfiles, docker-compose, go.mod, .nvmrc, .tool-versions, pom.xml, workflow \
             files and the rest — by fetching each repository connected to a service itself.",
        ),
    ]
}

#[derive(Debug, Clone)]
pub struct Definitions {
    /// With no trailing slash.
    pub base: String,
    /// Lower case.
    pub custom_hosts: Vec<String>,
    pub warn_days: i64,
    /// The plugins asked for an archive of a repository, in the order to ask them.
    pub sources: Vec<String>,
}

impl Definitions {
    pub fn read(settings: &Settings) -> Self {
        let base = settings.some_text(BASE_URL).unwrap_or_else(|| ENDOFLIFE.to_string());
        Self {
            base: base.trim().trim_end_matches('/').to_string(),
            custom_hosts: settings
                .list(CUSTOM_HOSTS)
                .into_iter()
                .map(|host| host.trim().to_ascii_lowercase())
                .filter(|host| !host.is_empty())
                .collect(),
            warn_days: settings
                .duration(WARN_WITHIN)
                .map_or(180, |warn| i64::try_from(warn.as_secs() / 86_400).unwrap_or(180)),
            sources: settings
                .list(SOURCES)
                .into_iter()
                .map(|source| source.trim().to_ascii_lowercase())
                .filter(|source| !source.is_empty())
                .collect(),
        }
    }
}
