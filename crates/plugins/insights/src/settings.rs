//! What Repository Insights is configured with: which merges have a repository scanned again, where
//! its archives come from, how long a scan may take and how many are kept. Where ccc itself is
//! found is the deployment's to say, never a setting, since a setting is typed into a page.

use std::path::PathBuf;
use std::time::Duration;

use doc_plugin_sdk::{Feature, Setting, SettingKind, Settings};
use serde_json::json;

pub const SCANS: &str = "scans";
pub const ADVISORIES: &str = "advisories";

pub const BRANCH: &str = "branch";
pub const REPOSITORIES: &str = "repositories";
pub const SOURCES: &str = "sources";
pub const TIMEOUT: &str = "timeout";
pub const KEEP: &str = "keep";

const DEFAULT_BRANCH: &str = "main";
const DEFAULT_TIMEOUT: f64 = 1_800.0;
const DEFAULT_KEEP: f64 = 30.0;

pub fn declared() -> Vec<Setting> {
    vec![
        Setting::text(BRANCH, "Branch")
            .defaulting(json!(DEFAULT_BRANCH))
            .hinted(
                "A pull request merged into this branch has its repository scanned again, as the \
                 branch stands then.",
            )
            .grouped("Scanning"),
        Setting::new(REPOSITORIES, "Only these repositories", SettingKind::List)
            .hinted(
                "owner/name, or owner/* for every repository of an owner. Leave it empty to scan \
                 every repository a merge is heard from.",
            )
            .grouped("Scanning"),
        Setting::new(SOURCES, "Where merges and archives come from", SettingKind::List)
            .defaulting(json!(["github", "ghe"]))
            .hinted(
                "Plugins that announce merged pull requests and give this one archive links, such \
                 as github and ghe. The first is used for a repository somebody asks for by name.",
            )
            .grouped("Scanning"),
        Setting::new(TIMEOUT, "Longest a scan may take", SettingKind::Duration)
            .defaulting(json!(DEFAULT_TIMEOUT))
            .hinted("A scan still going after this is stopped and reported as failed.")
            .grouped("Scanning"),
        Setting::new(KEEP, "Scans kept for each repository", SettingKind::Number)
            .defaulting(json!(DEFAULT_KEEP))
            .between(1.0, 500.0)
            .hinted("The newest are kept; they are what the trends are drawn from.")
            .grouped("Keeping"),
    ]
}

pub fn features() -> Vec<Feature> {
    vec![
        Feature::new(
            SCANS,
            "Repository insights",
            "Runs ccc over a repository each time a pull request is merged into its main branch, \
             and keeps what it finds: size, hotspots, services, lints, tests, security findings \
             and what changed since the scan before.",
        )
        .needs(&["github", "ghe"], "delivery-data"),
        Feature::new(
            ADVISORIES,
            "Dependency advisories",
            "Checks the packages in each repository's lockfiles against the OSV advisory database.",
        )
        .warning(
            "This sends the name and version of every package in each scanned repository's \
             lockfiles to osv.dev, the open vulnerability database Google runs. No code leaves \
             DOC, but the name of a private package would.",
        ),
    ]
}

/// The settings a scan and a merge are judged by, read once.
#[derive(Debug, Clone)]
pub struct Scanning {
    pub branch: String,
    pub repositories: Vec<String>,
    pub sources: Vec<String>,
    pub timeout: Duration,
    pub keep: usize,
    pub advisories: bool,
}

impl Scanning {
    pub fn read(settings: &Settings) -> Self {
        let lower = |key: &str| -> Vec<String> {
            settings
                .list(key)
                .iter()
                .map(|item| item.trim().to_ascii_lowercase())
                .filter(|item| !item.is_empty())
                .collect()
        };
        let branch = settings.some_text(BRANCH).map(|branch| branch.trim().to_string());
        Self {
            branch: branch.filter(|b| !b.is_empty()).unwrap_or_else(|| DEFAULT_BRANCH.into()),
            repositories: lower(REPOSITORIES),
            sources: lower(SOURCES),
            timeout: settings
                .duration(TIMEOUT)
                .filter(|timeout| !timeout.is_zero())
                .unwrap_or(Duration::from_secs_f64(DEFAULT_TIMEOUT)),
            keep: settings.number(KEEP).unwrap_or(DEFAULT_KEEP).clamp(1.0, 500.0) as usize,
            advisories: settings.feature(ADVISORIES),
        }
    }

    /// Whether `repository` (owner/name) is one to scan.
    pub fn wants(&self, repository: &str) -> bool {
        let repository = repository.to_ascii_lowercase();
        let owner = repository.split('/').next().unwrap_or_default();
        self.repositories.is_empty()
            || self.repositories.iter().any(|wanted| {
                *wanted == repository || wanted.strip_suffix("/*").is_some_and(|o| o == owner)
            })
    }

    /// The source a repository asked for by name is read from.
    pub fn first_source(&self) -> Option<&str> {
        self.sources.first().map(String::as_str)
    }
}

/// Where the ccc binary is: `DOC_INSIGHTS_CCC`, or `ccc` on the path.
pub fn ccc() -> PathBuf {
    std::env::var_os("DOC_INSIGHTS_CCC")
        .filter(|path| !path.is_empty())
        .map_or_else(|| PathBuf::from("ccc"), PathBuf::from)
}

/// Where the plugin installs ccc itself when none is found: `DOC_INSIGHTS_TOOLS_DIR`, or a
/// directory of its own beside where scans unpack.
pub fn tools_dir() -> PathBuf {
    std::env::var_os("DOC_INSIGHTS_TOOLS_DIR")
        .filter(|path| !path.is_empty())
        .map_or_else(|| work_dir().join("doc-insights-tools"), PathBuf::from)
}

/// Whether the plugin may install ccc when none is found; `DOC_INSIGHTS_INSTALL=no` forbids it.
pub fn installs() -> bool {
    std::env::var("DOC_INSIGHTS_INSTALL")
        .map_or(true, |value| !matches!(value.trim(), "no" | "false" | "0" | "off"))
}

/// Where scans unpack repositories: `DOC_INSIGHTS_WORK_DIR`, or the system's temporary directory.
pub fn work_dir() -> PathBuf {
    std::env::var_os("DOC_INSIGHTS_WORK_DIR")
        .filter(|path| !path.is_empty())
        .map_or_else(std::env::temp_dir, PathBuf::from)
}
