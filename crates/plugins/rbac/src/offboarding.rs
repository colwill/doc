//! Offboarding (T67): when an identity provider says somebody has left the directory it speaks
//! for, the rules here decide what happens to them. Core carries each part out and audits it; this
//! only chooses. Nothing happens at all without a rule, so a platform offboards no one by default.

use doc_plugin_sdk::protocol::calls::OffboardRequest;
use doc_plugin_sdk::{Backend, PluginError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::model::Refusal;
use crate::store::Store;

pub const DEPROVISIONED: &str = "platform.iam.user.deprovisioned";

/// What one rule does about a leaver, and whose leavers it answers for.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OffboardingRule {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The provider whose leavers it answers for; empty means all of them.
    #[serde(default)]
    pub provider: String,
    #[serde(default)]
    pub disable: bool,
    #[serde(default)]
    pub remove_identity: bool,
    #[serde(default)]
    pub remove_provided_memberships: bool,
    #[serde(default)]
    pub remove_memberships: bool,
    #[serde(default)]
    pub revoke_tokens: bool,
    #[serde(default)]
    pub enabled: bool,
}

impl OffboardingRule {
    fn answers_for(&self, provider: &str) -> bool {
        self.enabled && (self.provider.is_empty() || self.provider.eq_ignore_ascii_case(provider))
    }

    /// Whether it asks for anything at all; one that asks for nothing is left alone.
    pub fn does_something(&self) -> bool {
        self.disable
            || self.remove_identity
            || self.remove_provided_memberships
            || self.remove_memberships
            || self.revoke_tokens
    }
}

/// Somebody gone from a provider's directory, as core announces it.
#[derive(Debug, Deserialize)]
struct Deprovisioned {
    user: Uuid,
    provider: String,
    #[serde(default)]
    login: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

pub async fn rules(store: &Store<'_>) -> Result<Vec<OffboardingRule>, Refusal> {
    store.offboarding_rules().await
}

/// What every rule that answers for this provider asks for, taken together: anything any of them
/// wants, since two rules that both apply cannot mean less than one of them.
fn asked(rules: &[OffboardingRule], event: &Deprovisioned) -> Option<OffboardRequest> {
    let applying: Vec<&OffboardingRule> = rules
        .iter()
        .filter(|rule| rule.answers_for(&event.provider) && rule.does_something())
        .collect();
    if applying.is_empty() {
        return None;
    }
    let any = |pick: fn(&OffboardingRule) -> bool| applying.iter().any(|rule| pick(rule));
    let reason = match &event.reason {
        Some(reason) => format!(
            "{} left {} ({reason})",
            event.login.as_deref().unwrap_or("they"),
            event.provider
        ),
        None => format!("{} left {}", event.login.as_deref().unwrap_or("they"), event.provider),
    };
    Some(OffboardRequest {
        user: event.user,
        disable: any(|rule| rule.disable),
        remove_identity: any(|rule| rule.remove_identity).then(|| event.provider.clone()),
        remove_provided_memberships: any(|rule| rule.remove_provided_memberships)
            .then(|| event.provider.clone()),
        remove_memberships: any(|rule| rule.remove_memberships),
        revoke_tokens: any(|rule| rule.revoke_tokens),
        reason: Some(reason),
    })
}

pub async fn on_deprovisioned(backend: &Backend, payload: Value) -> Result<(), PluginError> {
    let event: Deprovisioned = serde_json::from_value(payload)
        .map_err(|err| PluginError::Message(format!("a leaver could not be read: {err}")))?;
    let store = Store(backend);
    let rules = rules(&store).await.map_err(|refusal| PluginError::Message(refusal.detail))?;
    let Some(request) = asked(&rules, &event) else {
        tracing::info!(
            provider = %event.provider,
            "somebody left, and no offboarding rule answers for that provider"
        );
        return Ok(());
    };
    let done = backend
        .offboard(request)
        .await
        .map_err(|err| PluginError::Message(format!("they could not be offboarded: {err}")))?;
    tracing::info!(
        user = %event.user,
        provider = %event.provider,
        disabled = done.disabled,
        identities = done.identities_removed,
        memberships = done.memberships_removed,
        tokens = done.tokens_revoked,
        "offboarded somebody who left"
    );
    let detail = json!({
        "provider": event.provider,
        "login": event.login,
        "disabled": done.disabled,
        "identities_removed": done.identities_removed,
        "memberships_removed": done.memberships_removed,
        "tokens_revoked": done.tokens_revoked,
    });
    let _ = backend.audit("offboarded", Some(&event.user.to_string()), detail).await;
    Ok(())
}
