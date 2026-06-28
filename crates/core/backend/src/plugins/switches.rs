//! Turning a plugin off, and on again: somebody's decision that outlives the plugin's process. A
//! plugin that is off stays registered but is held `cancelled` — no runs, tasks or events — and is
//! offered to nobody: no navigation, panels, insights or operations, and its pages, API and routes
//! answer that it is turned off. It stays off across restarts and new versions, because a
//! registration settles `cancelled` rather than `running` while it is. Turning it on resumes it.
//!
//! A plugin can instead **follow a flag**: the platform reads its own flags from the flags plugin,
//! as the service `doc`, and turns the plugin on and off as the flag says. A flag that cannot be
//! read — the flags plugin is not there, the flag is not, or it is not true or false — leaves the
//! plugin as it is and says why.

use std::time::Duration;

use doc_eventbus::{ConsumerGroup, TopicFilter};
use doc_plugin_protocol::{Capability, Manifest, PluginState};
use serde_json::{Value, json};

use super::routes::ask_internal;
use super::{Following, TransitionError, TurnedOff, audit, cancel, transition};
use crate::api::AppState;
use crate::identity::Principal;

/// The service the platform reads its own flags as.
pub const PLATFORM_SERVICE: &str = "doc";
/// The plugin that answers them.
const FLAGS: &str = "flags";
/// How often the flags are read when nothing says one changed: a provider's flag changes without
/// telling anybody, and the flags plugin keeps what it last read for a while anyway.
const FOLLOW_EVERY: Duration = Duration::from_secs(15);
/// What the flags plugin publishes when one of DOC's own flags changes.
const CHANGED: &str = "plugin.flags.entry.changed";

/// Reads which plugins are off and which follow a flag, once, before any registers.
pub async fn load(state: &AppState) {
    match state.repos.plugins.plugin_switches().await {
        Ok(switches) => {
            for switch in switches {
                let off = TurnedOff { by: switch.updated_by, at: switch.updated_at };
                state.plugins.mark_off(&switch.plugin, Some(off));
            }
        }
        Err(err) => tracing::warn!(%err, "which plugins are turned off could not be read"),
    }
    match state.repos.plugins.plugin_flags().await {
        Ok(flags) => {
            for held in flags {
                let following = Following {
                    flag: held.flag,
                    by: held.updated_by,
                    at: held.updated_at,
                    read: None,
                    read_at: None,
                    problem: None,
                };
                state.plugins.set_following(&held.plugin, Some(following));
            }
        }
        Err(err) => tracing::warn!(%err, "which plugins follow a flag could not be read"),
    }
}

/// Turns `id` off or on by hand, answering the state its process is now in, or none while no
/// process is registered. A plugin that decides permissions or signs people in cannot be turned
/// off, and one that follows a flag is the flag's to turn on and off.
pub async fn switch(
    state: &AppState,
    id: &str,
    on: bool,
    by: &Principal,
) -> Result<Option<PluginState>, TransitionError> {
    if let Some(following) = state.plugins.following(id) {
        return Err(TransitionError::Follows(id.to_string(), following.flag));
    }
    turn(state, id, on, by, by.label(), json!({})).await
}

async fn turn(
    state: &AppState,
    id: &str,
    on: bool,
    by: &Principal,
    label: String,
    detail: Value,
) -> Result<Option<PluginState>, TransitionError> {
    let entry = state.plugins.get(id).await;
    if !on && let Some(needed) = entry.as_ref().and_then(|entry| needed(&entry.manifest)) {
        return Err(TransitionError::Needed(id.to_string(), needed));
    }
    if state.plugins.handing_over(id) {
        return Err(TransitionError::Busy(id.to_string()));
    }
    state
        .repos
        .plugins
        .set_plugin_switch(id, on, Some(&label))
        .await
        .map_err(|err| TransitionError::Call(err.to_string()))?;
    let off = TurnedOff { by: Some(label), at: chrono::Utc::now() };
    state.plugins.mark_off(id, (!on).then_some(off));
    let now = match (entry.map(|entry| entry.state), on) {
        (None, _) => None,
        (Some(PluginState::Running), false) => {
            cancel(state, id).await?;
            Some(PluginState::Cancelled)
        }
        (Some(PluginState::Cancelled), true) => {
            Some(transition(state, id, PluginState::Running, None).await?)
        }
        (Some(now), _) => Some(now),
    };
    let action = if on { "plugin.turned-on" } else { "plugin.turned-off" };
    let mut detail = detail;
    detail["state"] = json!(now);
    audit(state, by, action, id, detail).await;
    Ok(now)
}

/// Has `id` follow `flag`, or with none, go back to being turned on and off by hand as it is now.
/// Answers what it follows.
pub async fn follow(
    state: &AppState,
    id: &str,
    flag: Option<&str>,
    by: &Principal,
) -> Result<Option<Following>, TransitionError> {
    let flag = flag.map(str::trim).filter(|flag| !flag.is_empty()).map(str::to_ascii_lowercase);
    if let Some(flag) = &flag {
        let shaped = flag.len() <= 120
            && flag.chars().next().is_some_and(|letter| letter.is_ascii_alphanumeric())
            && flag.chars().all(|letter| letter.is_ascii_alphanumeric() || "-_.".contains(letter));
        if !shaped {
            return Err(TransitionError::Invalid(format!(
                "`{flag}` is not a flag: 1 to 120 letters, digits, dashes, underscores and dots"
            )));
        }
        if id == FLAGS {
            let detail = "the flags plugin reads the flags, so it cannot follow one";
            return Err(TransitionError::Invalid(detail.into()));
        }
        if let Some(needed) = state.plugins.get(id).await.and_then(|entry| needed(&entry.manifest))
        {
            return Err(TransitionError::Needed(id.to_string(), needed));
        }
    }
    let label = by.label();
    state
        .repos
        .plugins
        .set_plugin_flag(id, flag.as_deref(), Some(&label))
        .await
        .map_err(|err| TransitionError::Call(err.to_string()))?;
    let following = flag.clone().map(|flag| Following {
        flag,
        by: Some(label),
        at: chrono::Utc::now(),
        read: None,
        read_at: None,
        problem: None,
    });
    state.plugins.set_following(id, following.clone());
    let action = match following {
        Some(_) => "plugin.follows-flag",
        None => "plugin.stopped-following-flag",
    };
    audit(state, by, action, id, json!({ "flag": flag })).await;
    state.plugins.nudge.notify_one();
    Ok(following)
}

/// Reads the flags plugins follow whenever one of DOC's own flags changes, and every
/// `FOLLOW_EVERY` besides for a provider's, which change without saying.
pub fn watch(state: &AppState) {
    let follower = state.clone();
    tokio::spawn(async move {
        loop {
            follow_flags(&follower).await;
            tokio::select! {
                () = tokio::time::sleep(FOLLOW_EVERY) => {}
                () = follower.plugins.nudge.notified() => {}
            }
        }
    });
    let watcher = state.clone();
    tokio::spawn(async move {
        let Ok(filter) = TopicFilter::new(CHANGED) else { return };
        let group = ConsumerGroup::new("core.plugin-flags", filter);
        let mut subscription = match watcher.buses.events.subscribe(group).await {
            Ok(subscription) => subscription,
            Err(err) => {
                tracing::warn!(%err, "cannot watch {CHANGED}; followed flags are only polled");
                return;
            }
        };
        while let Some(delivery) = subscription.next().await {
            watcher.plugins.nudge.notify_one();
            let _ = subscription.ack(delivery.id).await;
        }
    });
}

/// Reads the flags plugins follow, once, and turns each plugin on or off as its flag says. A flag
/// that cannot be read leaves its plugin as it is, and says why.
pub async fn follow_flags(state: &AppState) {
    let following = state.plugins.all_following();
    if following.is_empty() {
        return;
    }
    let asking = json!({ "service": PLATFORM_SERVICE });
    let answer = match ask_internal(state, FLAGS, "platform", &asking).await {
        Ok(answer) => answer,
        Err(problem) => {
            let problem = format!("the flags plugin could not be asked: {problem}");
            for (id, _) in &following {
                state.plugins.note_read(id, Err(problem.clone()));
            }
            return;
        }
    };
    let flagger = Principal::Plugin { id: FLAGS.to_string() };
    for (id, following) in following {
        let flag = &following.flag;
        let on = match answer["flags"].get(flag).or_else(|| answer["config"].get(flag)) {
            Some(Value::Bool(on)) => *on,
            Some(_) => {
                state.plugins.note_read(&id, Err(format!("{flag} is not true or false")));
                continue;
            }
            None => {
                let problem = format!("no flag called {flag} reaches the platform");
                state.plugins.note_read(&id, Err(problem));
                continue;
            }
        };
        state.plugins.note_read(&id, Ok(on));
        if state.plugins.is_off(&id) != on {
            continue;
        }
        let detail = json!({ "flag": flag, "value": on });
        if let Err(err) = turn(state, &id, on, &flagger, format!("the flag {flag}"), detail).await {
            state.plugins.note_read(&id, Err(err.to_string()));
        }
    }
}

/// What a plugin does that the platform cannot do without, if anything.
fn needed(manifest: &Manifest) -> Option<&'static str> {
    manifest.capabilities.iter().find_map(|capability| match capability {
        Capability::PermissionProvider => Some("decides permissions"),
        Capability::IdentityProvider => Some("signs people in"),
        _ => None,
    })
}
