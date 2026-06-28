//! DOC backend: the HTTP API, plugin host, permission checks and storage owner.

pub mod api;
pub mod auth;
pub mod bootstrap;
pub mod config;
pub mod data;
pub mod db;
pub mod fabric;
pub mod identity;
pub mod limits;
pub mod permissions;
pub mod plugins;
pub mod scoped;
pub mod secrets;
pub mod server;
pub mod status;
pub mod teams;
pub mod telemetry;

#[cfg(test)]
mod testing;
