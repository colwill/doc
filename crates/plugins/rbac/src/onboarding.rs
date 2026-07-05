//! Onboarding: at every sign-in the provider's default permission and whatever each matching rule
//! grants are added where missing, so a grant that has gone missing comes back.

use std::collections::{BTreeMap, BTreeSet};

use doc_plugin_sdk::{Backend, PluginError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::checks;
use crate::model::{Holder, Refusal, Rule, SignIn, membership};
use crate::ops::record;
use crate::store::Store;

pub const SIGNED_IN: &str = "platform.iam.user.signed-in";

#[derive(Deserialize)]
struct SignedIn {
    user: SignedInUser,
    provider: String,
    #[serde(default)]
    first: bool,
    #[serde(default)]
    organisations: Vec<String>,
    #[serde(default)]
    teams: Vec<String>,
}

#[derive(Deserialize)]
struct SignedInUser {
    id: Uuid,
    login: String,
}

#[derive(Debug, Default, Serialize)]
pub struct Applied {
    pub permissions: Vec<String>,
    pub attributes: BTreeMap<String, String>,
    /// Grants that cannot be made, such as membership of a group deleted since the rule was written.
    pub skipped: Vec<String>,
}

impl Applied {
    fn is_empty(&self) -> bool {
        self.permissions.is_empty() && self.attributes.is_empty()
    }
}

pub async fn on_sign_in(backend: &Backend, payload: Value) -> Result<(), PluginError> {
    let event: SignedIn = serde_json::from_value(payload)
        .map_err(|err| PluginError::Message(format!("a sign-in could not be read: {err}")))?;
    let sign_in = SignIn {
        user: event.user.id,
        login: event.user.login,
        provider: event.provider,
        organisations: event.organisations,
        teams: event.teams,
    };
    let store = Store(backend);
    store.record_sign_in(&sign_in).await?;
    let applied = apply(&store, &sign_in, &store.rules().await?, true, false).await?;
    for skipped in &applied.skipped {
        tracing::warn!(user = %sign_in.login, %skipped, "an onboarding grant was skipped");
    }
    if !applied.is_empty() {
        let action = if event.first { "onboarding.first-sign-in" } else { "onboarding.sign-in" };
        let detail = json!({ "user": sign_in.login, "applied": applied });
        record(backend, action, &sign_in.user.to_string(), detail).await;
    }
    Ok(())
}

/// A run applies the rules to one user now, as their next sign-in would.
pub async fn run(backend: &Backend, payload: Value) -> Result<Value, PluginError> {
    let user: Uuid = payload
        .get("user")
        .and_then(Value::as_str)
        .and_then(|id| id.parse().ok())
        .ok_or_else(|| PluginError::Message("a run needs the `user` to onboard".into()))?;
    let store = Store(backend);
    let sign_in =
        store.sign_in(user).await?.ok_or_else(|| Refusal::missing(format!("no user {user}")))?;
    let applied = apply(&store, &sign_in, &store.rules().await?, true, false).await?;
    if !applied.is_empty() {
        let detail = json!({ "user": sign_in.login, "applied": applied });
        record(backend, "onboarding.applied", &user.to_string(), detail).await;
    }
    Ok(json!(applied))
}

/// What every enabled rule that matches, and the provider's default if asked, would add; `dry` adds nothing.
pub async fn apply(
    store: &Store<'_>,
    sign_in: &SignIn,
    rules: &[Rule],
    provider: bool,
    dry: bool,
) -> Result<Applied, Refusal> {
    let holder = Holder::User { id: sign_in.user };
    let known = store.known().await?;
    let held: BTreeSet<String> =
        store.assignments_of(&holder).await?.into_iter().map(|held| held.permission).collect();
    let attributes = store.attributes(&holder).await?;
    let mut wanted = Vec::new();
    if provider && known.plugins.contains(&sign_in.provider) {
        let source = format!("provider {}", sign_in.provider);
        wanted.push((format!("plugin:{}:user", sign_in.provider), source));
    }
    let mut applied = Applied::default();
    for rule in rules.iter().filter(|rule| rule.enabled) {
        if !rule.conditions.matches(sign_in, &attributes) {
            continue;
        }
        let source = format!("rule {}", rule.name);
        let groups = rule.grants.groups.iter().map(|group| membership(&group.plugin, &group.name));
        wanted.extend(
            groups.chain(rule.grants.permissions.iter().cloned()).map(|p| (p, source.clone())),
        );
        for (key, value) in &rule.grants.attributes {
            if !attributes.contains_key(key) {
                applied.attributes.entry(key.clone()).or_insert_with(|| value.clone());
            }
        }
    }
    for (text, source) in wanted {
        let permission = match checks::permission(store, &known, &holder, &text).await {
            Ok(permission) => permission,
            Err(refusal) => {
                applied.skipped.push(format!("{text} from {source}: {}", refusal.detail));
                continue;
            }
        };
        let text = permission.to_string();
        if held.contains(&text) || applied.permissions.contains(&text) {
            continue;
        }
        if !dry {
            let by = format!("onboarding, {source}");
            store.grant(&holder, &permission, "onboarding", &by).await?;
        }
        applied.permissions.push(text);
    }
    if !dry {
        for (key, value) in &applied.attributes {
            store.set_attribute(&holder, key, value, false).await?;
        }
    }
    Ok(applied)
}
