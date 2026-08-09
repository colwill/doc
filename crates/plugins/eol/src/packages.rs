//! What Repository Insights found a repository's lockfiles to hold, read as End of life's: every
//! scan of a repository runs `ccc audit`, which resolves each package and its exact version, and
//! `insights` exports that list here. A package is a product where endoflife.date says so — each of
//! its products names the packages it is published as (`pkg:npm/react`, `pkg:pypi/django`) — so
//! nothing here keeps a list of its own of which packages matter.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use doc_plugin_sdk::{Backend, PluginError, Query};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::manifests::Found;

/// `insights`'s collection, as it exports it here.
pub const EXPORTED: &str = "insights.packages";
/// What `insights` announces once a repository's packages are listed.
pub const LISTED: &str = "plugin.insights.packages.listed";
/// Which package is which product, worked out from every product kept.
const INDEX_KEY: &str = "identifiers";
const INDEX_KEPT: Duration = Duration::from_secs(86_400);
/// The lockfiles named for one release in one repository; a monorepo may have dozens.
const FILES_NAMED: usize = 3;

/// A repository's packages as `insights` last listed them, without the list itself.
#[derive(Debug, Clone, Deserialize)]
pub struct Listed {
    pub id: String,
    pub repository: String,
    pub commit: String,
    pub listed_at: chrono::DateTime<chrono::Utc>,
    /// How many packages ccc resolved in it, most of which endoflife.date does not track.
    #[serde(default)]
    pub total: i64,
}

/// Where Repository Insights stands, for a page with nothing to show to say why.
pub enum Scanning {
    /// It is not installed, or does not share what it finds here.
    Absent,
    /// It is, and has listed the packages of this many repositories.
    Listed(usize),
}

/// Every repository `insights` has listed the packages of, newest first per repository; or
/// nothing where it is not installed or shares nothing here.
pub async fn every(backend: &Backend) -> Result<Vec<Listed>, PluginError> {
    let asked = Query::new(EXPORTED).fields(&["id", "repository", "commit", "listed_at", "total"]);
    match backend.query_all::<Listed>(asked).await {
        Err(err) if matches!(err.problem(), Some((403 | 404, _))) => Ok(Vec::new()),
        answered => answered,
    }
}

pub async fn scanning(backend: &Backend) -> Scanning {
    let asked = Query::new(EXPORTED).fields(&["id"]);
    match backend.query_all::<Value>(asked).await {
        Ok(listed) => Scanning::Listed(listed.len()),
        Err(_) => Scanning::Absent,
    }
}

/// One repository's packages, as listed, or nothing where none are.
pub async fn of(backend: &Backend, id: &str) -> Result<Option<(Listed, Vec<Value>)>, PluginError> {
    let Some(mut held) = backend.get::<Value>(EXPORTED, id).await? else { return Ok(None) };
    let packages = held["packages"].as_array_mut().map(std::mem::take).unwrap_or_default();
    let listed: Listed = serde_json::from_value(held).map_err(|err| {
        PluginError::from(format!("Repository Insights listed {id} oddly: {err}"))
    })?;
    Ok(Some((listed, packages)))
}

// ---- which package is which product ------------------------------------------------------------

/// A package URL's type for an ecosystem as OSV and ccc name it.
fn kind(ecosystem: &str) -> Option<&'static str> {
    Some(match ecosystem.to_ascii_lowercase().as_str() {
        "npm" => "npm",
        "pypi" => "pypi",
        "go" => "golang",
        "crates.io" | "cratesio" => "cargo",
        "nuget" => "nuget",
        "maven" => "maven",
        "rubygems" => "gem",
        "packagist" => "composer",
        "hex" => "hex",
        "pub" => "pub",
        _ => return None,
    })
}

/// A package's name as its ecosystem compares it: PyPI treats `-`, `_` and `.` alike, NuGet and
/// the rest ignore case, and Maven's `group/artifact` is OSV's `group:artifact`.
fn normalised(kind: &str, name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    match kind {
        "pypi" => {
            let mut out = String::with_capacity(lower.len());
            for c in lower.chars() {
                match c {
                    '-' | '_' | '.' if out.ends_with('-') => {}
                    '-' | '_' | '.' => out.push('-'),
                    c => out.push(c),
                }
            }
            out
        }
        "maven" => lower.replacen('/', ":", 1),
        _ => lower,
    }
}

fn decoded(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex =
            |at: usize| text.get(at..at + 2).and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match (bytes[at], hex(at + 1)) {
            (b'%', Some(byte)) => {
                out.push(byte);
                at += 3;
            }
            (byte, _) => {
                out.push(byte);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// `pkg:npm/%40angular/core@15?x=y` as `npm/@angular/core`: its type and name, without its
/// version, qualifiers or subpath.
fn purl_key(purl: &str) -> Option<String> {
    let (kind, rest) = purl.strip_prefix("pkg:")?.split_once('/')?;
    let rest = rest.split(['?', '#']).next()?;
    // A namespace's own `@` is encoded, so the last one left starts a version.
    let name = match rest.rfind('@') {
        Some(at) if at > 0 => &rest[..at],
        _ => rest,
    };
    let kind = kind.to_ascii_lowercase();
    let name = normalised(&kind, &decoded(name));
    (!name.is_empty()).then(|| format!("{kind}/{name}"))
}

/// The packages one of endoflife.date's products is published as, from its identifiers.
pub fn package_keys(product: &Value) -> Vec<String> {
    let identifiers = product["identifiers"].as_array().map(Vec::as_slice).unwrap_or_default();
    let mut keys: Vec<String> = identifiers
        .iter()
        .filter(|identifier| identifier["type"] == "purl")
        .filter_map(|identifier| identifier["id"].as_str().and_then(purl_key))
        .collect();
    keys.sort();
    keys.dedup();
    keys
}

async fn index_kept(backend: &Backend) -> Result<BTreeMap<String, String>, String> {
    #[derive(Deserialize)]
    struct Published {
        product: String,
        #[serde(default)]
        packages: Option<Vec<String>>,
    }
    let asked = Query::new("products").fields(&["product", "packages"]);
    let kept: Vec<Published> = backend.query_all(asked).await.map_err(|err| err.to_string())?;
    let mut index = BTreeMap::new();
    for published in kept {
        for key in published.packages.unwrap_or_default() {
            index.entry(key).or_insert_with(|| published.product.clone());
        }
    }
    Ok(index)
}

/// Which package is which product, from DOC's copy of endoflife.date; the copy is read first where
/// it never has been. Kept a day, and forgotten whenever the copy changes.
pub async fn index(backend: &Backend) -> Result<BTreeMap<String, String>, String> {
    if let Ok(Some(kept)) = backend.cache_get(INDEX_KEY).await
        && let Ok(index) = serde_json::from_value::<BTreeMap<String, String>>(kept)
        && !index.is_empty()
    {
        return Ok(index);
    }
    let mut index = index_kept(backend).await?;
    if index.is_empty() && crate::mirror::held(backend).await.read_at.is_none() {
        crate::mirror::read(backend).await.map_err(|err| err.to_string())?;
        index = index_kept(backend).await?;
    }
    if index.is_empty() {
        return Err("endoflife.date names no package for any product".into());
    }
    if let Err(err) = backend.cache_set(INDEX_KEY, json!(index), Some(INDEX_KEPT)).await {
        tracing::debug!(%err, "which package is which product was not kept");
    }
    Ok(index)
}

/// The copy changed, so which package is which product is worked out again when next asked.
pub async fn forget(backend: &Backend) {
    if let Err(err) = backend.cache_delete(INDEX_KEY).await {
        tracing::debug!(%err, "which package is which product was not forgotten");
    }
}

/// The products among a repository's packages, each release once with the lockfiles that hold it.
/// What is only built with — a linter, a test runner — is not what a service runs, and is left out.
pub fn products(packages: &[Value], index: &BTreeMap<String, String>) -> Vec<Found> {
    let mut found: BTreeMap<(String, String), BTreeSet<String>> = BTreeMap::new();
    for package in packages.iter().filter(|package| package["dev"] != true) {
        let (Some(ecosystem), Some(name), Some(version)) = (
            package["ecosystem"].as_str(),
            package["name"].as_str(),
            package["version"].as_str().filter(|version| !version.is_empty()),
        ) else {
            continue;
        };
        let Some(kind) = kind(ecosystem) else { continue };
        let Some(product) = index.get(&format!("{kind}/{}", normalised(kind, name))) else {
            continue;
        };
        let lockfile = package["lockfile"].as_str().unwrap_or("a lockfile").to_string();
        found.entry((product.clone(), version.to_string())).or_default().insert(lockfile);
    }
    found
        .into_iter()
        .map(|((product, version), files)| {
            let mut named: Vec<String> = files.iter().take(FILES_NAMED).cloned().collect();
            if files.len() > FILES_NAMED {
                named.push(format!("{} more", files.len() - FILES_NAMED));
            }
            Found { product, version: Some(version), file: named.join(", ") }
        })
        .collect()
}
