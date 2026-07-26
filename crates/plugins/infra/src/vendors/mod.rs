//! One adapter for each vendor behind the same trait, each making, inspecting and deleting every
//! resource type the catalogue offers for it with the shared credentials in the plugin's secrets.
//! A setting comes from the environment, or from the file `<NAME>_FILE` names.

mod aws;
mod azure;
mod ec2;
mod gcp;
mod linode;

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;
use doc_plugin_sdk::protocol::Secret;
use doc_plugin_sdk::telemetry::external;
use serde_json::Value;

/// The key every adapter puts a machine's address under. A DNS record for the service a machine
/// was stood up for points at whatever is here, so every vendor has to agree on the name.
pub const ADDRESS: &str = "address";

/// The address in what a vendor said about a resource, where it has one yet.
pub fn address(detail: &Value) -> Option<String> {
    detail.get(ADDRESS).and_then(Value::as_str).map(str::to_string).filter(|at| !at.is_empty())
}

/// A resource as the vendor is asked for it.
#[derive(Clone)]
pub struct Wanted {
    pub kind: String,
    /// Its name at the vendor, unique and in the vendor's own rules.
    pub name: String,
    pub region: String,
    pub size: Option<String>,
    /// The DOC request, team and expiry it is labelled with.
    pub labels: BTreeMap<String, String>,
}

/// What a vendor made: its ID, what to show about it, and whether it is ready yet.
pub struct Made {
    pub id: String,
    pub detail: Value,
    pub ready: bool,
}

#[async_trait]
pub trait Adapter: Send + Sync {
    async fn create(&self, wanted: &Wanted) -> Result<Made, String>;
    /// What the vendor says about it now, or `None` once it no longer exists.
    async fn inspect(&self, wanted: &Wanted, id: &str) -> Result<Option<Made>, String>;
    async fn delete(&self, wanted: &Wanted, id: &str) -> Result<(), String>;
}

/// What core last told the plugin it is configured with (ADR-0007). An adapter reaches for a
/// credential deep inside a vendor call, with no `Backend` to hand, so the settings are kept here
/// and refreshed at every `load` — which the SDK also does after a settings change.
static CONFIGURED: std::sync::RwLock<Option<std::sync::Arc<doc_plugin_sdk::Settings>>> =
    std::sync::RwLock::new(None);

pub fn remember(backend: &doc_plugin_sdk::Backend) {
    if let Ok(mut held) = CONFIGURED.write() {
        *held = Some(backend.settings());
    }
}

/// A setting by the variable that names it: what an administrator set on the Settings page, or
/// what the deployment gives in `DOC_INFRA_*` (or the file `…_FILE` names), or nothing.
pub fn setting(name: &str) -> Option<String> {
    let configured = CONFIGURED.read().ok().and_then(|held| held.clone());
    let set = configured.and_then(|settings| settings.by_variable("infra", name));
    set.or_else(|| {
        let direct = std::env::var(name).ok().filter(|value| !value.trim().is_empty());
        direct.or_else(|| {
            let path = std::env::var(format!("{name}_FILE")).ok()?;
            std::fs::read_to_string(path).ok().map(|value| value.trim().to_string())
        })
    })
}

/// Refuses a resource type the adapter does not make.
fn offered(vendor: &str, wanted: &Wanted, types: &[&str]) -> Result<(), String> {
    match types.contains(&wanted.kind.as_str()) {
        true => Ok(()),
        false => Err(format!("{vendor} makes no {} here", wanted.kind)),
    }
}

/// The size a machine was asked for, which every vendor insists on.
fn sized(vendor: &str, wanted: &Wanted) -> Result<String, String> {
    wanted
        .size
        .clone()
        .filter(|size| !size.trim().is_empty())
        .ok_or_else(|| format!("{vendor} needs a size for a machine"))
}

/// A password nobody is told. Every vendor insists a machine has one even where the way in is a
/// key, so one is made, sent and forgotten: whoever is to reach the machine does it with the key
/// in the plugin's settings.
fn unknown_password() -> String {
    use ring::rand::SecureRandom;
    // Upper, lower, digits and punctuation, which is what the vendors' complexity rules ask for.
    const FROM: &[u8] = b"abcdefghijkmnopqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789-_.!@#%";
    let mut bytes = [0u8; 28];
    if ring::rand::SystemRandom::new().fill(&mut bytes).is_err() {
        // Only reachable if the system has no randomness at all, where nothing else would work
        // either; the request fails at the vendor rather than going out with a guessable password.
        return String::new();
    }
    let mut password: String =
        bytes.iter().map(|byte| FROM[*byte as usize % FROM.len()] as char).collect();
    password.push_str("aZ9-");
    password
}

fn needed(name: &str, vendor: &str) -> Result<String, String> {
    setting(name).ok_or_else(|| format!("{vendor} has no {name} in the plugin's secrets"))
}

fn credential(name: &str, vendor: &str) -> Result<Secret<String>, String> {
    needed(name, vendor).map(Secret::new)
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(concat!("doc-infra/", env!("CARGO_PKG_VERSION")))
        .build()
        .map_err(|err| err.to_string())
}

/// What a refusing vendor said, briefly.
async fn refused(vendor: &str, answer: reqwest::Response) -> String {
    let status = answer.status();
    let body = answer.text().await.unwrap_or_default();
    let body: String = body.chars().take(300).collect();
    format!("{vendor} answered {status}: {body}")
}

/// The adapter for a vendor, or why there is none.
pub fn adapter(vendor: &str) -> Result<Box<dyn Adapter>, String> {
    let (vendor, inner): (&'static str, Box<dyn Adapter>) = match vendor {
        "linode" => ("linode", Box::new(linode::Linode::from_settings()?)),
        "aws" => ("aws", Box::new(aws::Aws::from_settings()?)),
        "gcp" => ("gcp", Box::new(gcp::Gcp::from_settings()?)),
        "azure" => ("azure", Box::new(azure::Azure::from_settings()?)),
        other => return Err(format!("there is no adapter for {other}")),
    };
    Ok(Box::new(Counted { vendor, inner }))
}

/// Counts every call on a vendor, so failing ones show on the external APIs dashboard.
struct Counted {
    vendor: &'static str,
    inner: Box<dyn Adapter>,
}

#[async_trait]
impl Adapter for Counted {
    async fn create(&self, wanted: &Wanted) -> Result<Made, String> {
        let made = self.inner.create(wanted).await;
        external(self.vendor, "create", None, made.is_ok());
        made
    }

    async fn inspect(&self, wanted: &Wanted, id: &str) -> Result<Option<Made>, String> {
        let seen = self.inner.inspect(wanted, id).await;
        external(self.vendor, "inspect", None, seen.is_ok());
        seen
    }

    async fn delete(&self, wanted: &Wanted, id: &str) -> Result<(), String> {
        let deleted = self.inner.delete(wanted, id).await;
        external(self.vendor, "delete", None, deleted.is_ok());
        deleted
    }
}

/// A resource's name at the vendor, suffixed by its request; Azure's allows only letters and digits.
pub fn vendor_name(vendor: &str, name: &str, request: uuid::Uuid) -> String {
    let simple = request.simple().to_string();
    let suffix = &simple[simple.len() - 8..];
    match vendor {
        "azure" => {
            let letters: String =
                name.chars().filter(char::is_ascii_alphanumeric).take(13).collect();
            format!("doc{letters}{suffix}")
        }
        _ => format!("doc-{name}-{suffix}"),
    }
}
