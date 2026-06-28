//! Context tokens: attached to every call the backend makes to a plugin, and the only way a plugin
//! can act as the principal that call is for. Each is bound to one plugin and stops working the
//! moment its call returns, so nothing can be done in a caller's name after the caller has gone.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use base64::Engine;
use parking_lot::Mutex;

use doc_plugin_protocol::Guard;
use doc_secret::Secret;
use uuid::Uuid;

use crate::identity::Principal;

const PREFIX: &str = "doc_ctx_";
const TOKEN_BYTES: usize = 32;
/// Expired tokens are only swept once there are this many, which keeps issuing cheap.
const SWEEP_AT: usize = 256;

struct Issued {
    plugin: String,
    principal: Principal,
    expires: Instant,
    /// The chain of the task run this call is part of, which work it queues joins.
    chain: Option<Uuid>,
    /// The plugin that relayed this call, and what it may not change: both carry on to whatever
    /// the plugin asks while it handles it.
    via: Option<String>,
    guard: Option<Guard>,
}

#[derive(Default)]
pub struct Contexts {
    issued: Mutex<HashMap<String, Issued>>,
}

/// Revokes its token when dropped, so an early return or a call cut off by its deadline cannot
/// leave one working.
pub struct Context {
    contexts: Arc<Contexts>,
    token: Secret<String>,
}

impl Context {
    /// The token itself, for the one header that hands it to the plugin.
    pub fn token(&self) -> &str {
        self.token.expose()
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        self.contexts.issued.lock().remove(self.token.expose());
    }
}

impl Contexts {
    pub fn issue(
        self: &Arc<Self>,
        plugin: &str,
        principal: Principal,
        ttl: Duration,
    ) -> Result<Context, String> {
        let mut bytes = [0u8; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|err| format!("no random bytes: {err}"))?;
        let token =
            format!("{PREFIX}{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes));
        let mut issued = self.issued.lock();
        if issued.len() >= SWEEP_AT {
            let now = Instant::now();
            issued.retain(|_, entry| entry.expires > now);
        }
        let expires = Instant::now() + ttl;
        let entry = Issued {
            plugin: plugin.to_string(),
            principal,
            expires,
            chain: None,
            via: None,
            guard: None,
        };
        issued.insert(token.clone(), entry);
        Ok(Context { contexts: self.clone(), token: Secret::new(token) })
    }

    /// As `issue`, for a call that runs a task: what it queues joins `chain`.
    pub fn issue_in(
        self: &Arc<Self>,
        plugin: &str,
        principal: Principal,
        ttl: Duration,
        chain: Uuid,
    ) -> Result<Context, String> {
        let context = self.issue(plugin, principal, ttl)?;
        if let Some(entry) = self.issued.lock().get_mut(context.token()) {
            entry.chain = Some(chain);
        }
        Ok(context)
    }

    /// Says that the call a token is for was relayed by `via`, limited by `guard`.
    pub fn relayed(&self, token: &str, via: Option<&str>, guard: Option<Guard>) {
        if let Some(entry) = self.issued.lock().get_mut(token) {
            entry.via = via.map(str::to_string);
            entry.guard = guard;
        }
    }

    /// Who relayed the call a token is for, and what it may not change.
    pub fn relay_of(&self, plugin: &str, token: &str) -> (Option<String>, Option<Guard>) {
        let issued = self.issued.lock();
        match issued.get(token).filter(|entry| entry.plugin == plugin) {
            Some(entry) => (entry.via.clone(), entry.guard),
            None => (None, None),
        }
    }

    pub fn chain(&self, plugin: &str, token: &str) -> Option<Uuid> {
        let issued = self.issued.lock();
        issued.get(token).filter(|entry| entry.plugin == plugin).and_then(|entry| entry.chain)
    }

    /// Only for the plugin it was issued to, and only while its call is still running.
    pub fn resolve(&self, plugin: &str, token: &str) -> Option<Principal> {
        let issued = self.issued.lock();
        let entry = issued.get(token)?;
        (entry.plugin == plugin && entry.expires > Instant::now()).then(|| entry.principal.clone())
    }
}
