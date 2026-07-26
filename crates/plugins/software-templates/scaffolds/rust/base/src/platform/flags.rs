//! This service's feature flags and runtime configuration, read from DOC. Everything that applies
//! to it is read in one call and kept here; the call carries an ETag, so a poll that finds nothing
//! new costs a `304` and nothing else.
//!
//! Every read names the value to fall back to, so a service whose flags cannot be reached keeps
//! running on its own defaults rather than stopping.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::Deserialize;
use serde_json::Value;

use super::Config;

#[derive(Debug, Default, Clone, Deserialize)]
struct Answer {
    #[serde(default)]
    flags: serde_json::Map<String, Value>,
    #[serde(default)]
    config: serde_json::Map<String, Value>,
    #[serde(default)]
    version: String,
    #[serde(default)]
    refresh_seconds: u64,
}

#[derive(Clone)]
pub struct Flags {
    held: Arc<RwLock<Answer>>,
}

impl Flags {
    /// Reads them once, then keeps them up to date in the background for as long as the process runs.
    pub async fn start(config: &Config) -> Self {
        let flags = Self { held: Arc::new(RwLock::new(Answer::default())) };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap_or_default();
        if let Err(err) = flags.read(&client, config).await {
            tracing::warn!(%err, "the flags could not be read; running on this service's defaults");
        }
        let polling = flags.clone();
        let config = config.clone();
        tokio::spawn(async move {
            let mut wait = config.flags_poll;
            loop {
                tokio::time::sleep(wait).await;
                if let Err(err) = polling.read(&client, &config).await {
                    tracing::warn!(%err, "the flags could not be read");
                }
                let refresh = polling.held.read().map(|held| held.refresh_seconds).unwrap_or(0);
                if refresh > 0 {
                    wait = Duration::from_secs(refresh);
                }
            }
        });
        flags
    }

    async fn read(&self, client: &reqwest::Client, config: &Config) -> anyhow::Result<()> {
        let version = self.held.read().map(|held| held.version.clone()).unwrap_or_default();
        let mut asking = client
            .get(&config.flags_url)
            .query(&[("service", &config.service), ("environment", &config.environment)]);
        if let Some(token) = &config.flags_token {
            asking = asking.bearer_auth(token);
        }
        if !version.is_empty() {
            asking = asking.header("if-none-match", format!("\"{version}\""));
        }
        let answer = asking.send().await?;
        if answer.status().as_u16() == 304 {
            return Ok(());
        }
        let answer = answer.error_for_status()?.json::<Answer>().await?;
        if let Ok(mut held) = self.held.write() {
            *held = answer;
        }
        Ok(())
    }

    fn value(&self, key: &str) -> Option<Value> {
        let held = self.held.read().ok()?;
        held.flags.get(key).or_else(|| held.config.get(key)).cloned()
    }

    /// A switch: on, off, or the fallback when the platform holds nothing for this service.
    pub fn bool(&self, key: &str, fallback: bool) -> bool {
        self.value(key).and_then(|value| value.as_bool()).unwrap_or(fallback)
    }

    /// A value read while the service runs, such as a message or a mode.
    pub fn string(&self, key: &str, fallback: &str) -> String {
        self.value(key)
            .and_then(|value| value.as_str().map(str::to_string))
            .unwrap_or_else(|| fallback.to_string())
    }

    /// A number read while the service runs, such as a limit or a timeout.
    pub fn number(&self, key: &str, fallback: i64) -> i64 {
        self.value(key).and_then(|value| value.as_i64()).unwrap_or(fallback)
    }

    /// A structured value, read into whatever type it should be.
    pub fn json<T: serde::de::DeserializeOwned>(&self, key: &str) -> Option<T> {
        serde_json::from_value(self.value(key)?).ok()
    }
}
