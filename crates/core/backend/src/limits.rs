//! Rate limits: unknown bearer tokens and refused sign-ins per client, registrations per plugin,
//! and calls on a plugin's public routes (its inbound webhooks) per plugin and client. Windows are
//! a minute long and fixed, and each process keeps its own.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, FromRequestParts};
use http::request::Parts;
use ipnet::IpNet;
use parking_lot::{Mutex, RwLock};

use crate::api::AppState;
use crate::config;

const WINDOW: Duration = Duration::from_secs(60);
/// Past this many keys, windows that have ended are dropped, so the map cannot grow unbounded.
const SWEEP_AT: usize = 10_000;

pub struct Limiter {
    limit: u32,
    windows: Mutex<HashMap<String, (Instant, u32)>>,
}

impl Limiter {
    pub fn per_minute(limit: u32) -> Self {
        Self { limit: limit.max(1), windows: Mutex::default() }
    }

    /// Counts one attempt, or refuses it with how long to wait once the window's limit is spent.
    pub fn hit(&self, key: &str) -> Result<(), Duration> {
        let now = Instant::now();
        let mut windows = self.windows.lock();
        if windows.len() >= SWEEP_AT {
            windows.retain(|_, (started, _)| now.duration_since(*started) < WINDOW);
        }
        let (started, count) = windows.entry(key.to_string()).or_insert((now, 0));
        if now.duration_since(*started) >= WINDOW {
            (*started, *count) = (now, 0);
        }
        if *count >= self.limit {
            return Err(WINDOW - now.duration_since(*started));
        }
        *count += 1;
        Ok(())
    }

    /// How long the key must wait, if it has spent its window's limit; nothing is counted.
    pub fn waiting(&self, key: &str) -> Option<Duration> {
        let windows = self.windows.lock();
        let (started, count) = windows.get(key)?;
        let elapsed = started.elapsed();
        (elapsed < WINDOW && *count >= self.limit).then(|| WINDOW - elapsed)
    }
}

pub struct Limits {
    pub auth_failures: Limiter,
    pub registrations: Limiter,
    pub public_calls: Limiter,
    trusted: Vec<IpNet>,
    /// Trusted proxies named by host, such as `frontend` in Compose, and what they resolve to now.
    hosts: Vec<String>,
    resolved: RwLock<Vec<IpAddr>>,
}

impl Limits {
    pub fn new(limits: &config::Limits, trusted_proxies: &[String]) -> Self {
        let (mut trusted, mut hosts) = (Vec::new(), Vec::new());
        for entry in trusted_proxies {
            match entry.parse::<IpNet>().or_else(|_| entry.parse::<IpAddr>().map(IpNet::from)) {
                Ok(net) => trusted.push(net),
                Err(_) => hosts.push(entry.clone()),
            }
        }
        Self {
            auth_failures: Limiter::per_minute(limits.auth_failures_per_minute),
            registrations: Limiter::per_minute(limits.registrations_per_minute),
            public_calls: Limiter::per_minute(limits.public_calls_per_minute),
            trusted,
            hosts,
            resolved: RwLock::default(),
        }
    }

    /// Looks the named proxies up now and every minute, since a container's address can change.
    pub fn resolve_hosts(self: &Arc<Self>) {
        if self.hosts.is_empty() {
            return;
        }
        let limits = self.clone();
        tokio::spawn(async move {
            loop {
                let mut found = Vec::new();
                for host in &limits.hosts {
                    match tokio::net::lookup_host((host.as_str(), 0)).await {
                        Ok(addresses) => found.extend(addresses.map(|address| address.ip())),
                        Err(err) => tracing::warn!(%host, %err, "a trusted proxy did not resolve"),
                    }
                }
                *limits.resolved.write() = found;
                tokio::time::sleep(WINDOW).await;
            }
        });
    }

    /// Who is calling: the peer, or the client a trusted proxy such as the frontend forwards for.
    pub fn client(&self, parts: &Parts) -> String {
        let Some(ConnectInfo(peer)) = parts.extensions.get::<ConnectInfo<SocketAddr>>() else {
            return "unknown".into();
        };
        if !self.trusts(peer.ip()) {
            return peer.ip().to_string();
        }
        let forwarded = parts
            .headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .filter_map(|hop| hop.trim().parse::<IpAddr>().ok())
            .collect::<Vec<_>>();
        // The nearest hop a trusted proxy did not add is the client; the rest could be anyone's.
        let client = forwarded.iter().rev().find(|hop| !self.trusts(**hop)).or(forwarded.first());
        client.copied().unwrap_or(peer.ip()).to_string()
    }

    fn trusts(&self, address: IpAddr) -> bool {
        self.trusted.iter().any(|net| net.contains(&address))
            || self.resolved.read().contains(&address)
    }
}

/// The caller's address as the limits see it, for a handler that counts its own failures.
pub struct Client(pub String);

impl FromRequestParts<AppState> for Client {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(state.limits.client(parts)))
    }
}
