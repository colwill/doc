//! What every service on this platform has: its settings, its telemetry and its feature flags.
//! DOC wrote this when the service was created; it is yours to change.

pub mod config;
pub mod flags;
pub mod telemetry;

pub use config::Config;
pub use flags::Flags;
pub use telemetry::Telemetry;
