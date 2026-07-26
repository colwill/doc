//! What the service reads from its environment at start-up. Anything that should change without a
//! deployment belongs in [`crate::platform::Flags`] instead, which is read while it runs.

use std::env;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    pub service: String,
    pub environment: String,
    pub address: String,
    pub flags_url: String,
    pub flags_token: Option<String>,
    pub flags_poll: Duration,
}

impl Config {
    /// The environment, falling back to what DOC knew when this service was created.
    pub fn load() -> Self {
        Self {
            service: text("OTEL_SERVICE_NAME", "{{ values.name }}"),
            environment: text("DOC_ENVIRONMENT", "{{ telemetry.environment }}"),
            address: text("ADDRESS", "0.0.0.0:8080"),
            flags_url: text("DOC_FLAGS_URL", "{{ flags.url }}"),
            flags_token: env::var("DOC_FLAGS_TOKEN").ok().filter(|token| !token.is_empty()),
            flags_poll: Duration::from_secs(seconds("DOC_FLAGS_POLL_SECONDS", 30)),
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self::load()
    }
}

fn text(name: &str, fallback: &str) -> String {
    env::var(name).ok().filter(|value| !value.is_empty()).unwrap_or_else(|| fallback.to_string())
}

fn seconds(name: &str, fallback: u64) -> u64 {
    env::var(name).ok().and_then(|value| value.parse().ok()).filter(|number| *number > 0).unwrap_or(fallback)
}
