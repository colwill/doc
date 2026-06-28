//! A plugin asking for what only another plugin's settings can give it (DOC-SPEC §9.15): to join a
//! list setting that plugin declared `requestable`, such as github's `archive-plugins`. There is
//! one request per plugin, target and setting; whoever may change the target's settings is told
//! once, in their inbox, and approves or denies it on its Settings page.

use chrono::{Duration, Utc};
use doc_eventbus::{Event, Topic};
use doc_permissions::Access;
use doc_plugin_protocol::calls::{AccessAnswer, AccessRequest};
use doc_plugin_protocol::{Manifest, Setting, SettingKind};
use doc_servicebus::{Address, Message};
use serde_json::{Value, json};
use uuid::Uuid;

use super::api::Refusal;
use super::settings;
use crate::api::AppState;
use crate::db::repositories::{AccessRequestRecord, RepositoryError};
use crate::identity::Principal;

pub const PENDING: &str = "pending";
pub const APPROVED: &str = "approved";
pub const DENIED: &str = "denied";
/// A reason is a sentence for an administrator, not a document.
const MAX_REASON: usize = 500;
/// How long a decided request stays on the Settings page.
const SHOWN_FOR_DAYS: i64 = 30;
/// The most people looked through for someone to tell.
const MAX_PEOPLE: usize = 5_000;

/// The setting another plugin may ask to join: declared `requestable`, and a list.
pub fn requestable<'a>(manifest: &'a Manifest, key: &str) -> Option<&'a Setting> {
    manifest.settings.iter().find(|setting| {
        setting.key == key && setting.requestable && setting.kind == SettingKind::List
    })
}

/// The list as it stands now, whether it was set here, by the deployment or not at all.
pub async fn members(
    state: &AppState,
    target: &str,
    manifest: &Manifest,
    key: &str,
) -> Vec<String> {
    let resolved = settings::resolve(state, target, manifest).await;
    resolved
        .current
        .iter()
        .find(|current| current.key == key)
        .and_then(|current| current.value.as_array())
        .map(|listed| listed.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

fn answer(record: &AccessRequestRecord, raised: bool) -> AccessAnswer {
    AccessAnswer {
        state: record.state.clone(),
        id: Some(record.id),
        raised,
        decided_by: record.decided_by.clone(),
        decided_at: record.decided_at,
    }
}

fn failed(err: &RepositoryError) -> Refusal {
    Refusal::unavailable(err.to_string())
}

/// `requester` asking to join `request.plugin`'s `request.setting`. Asking again answers where
/// the request stands without telling anybody twice; a request approved once but taken out of the
/// list since is raised again, since the plugin needs it again.
pub async fn ask(
    state: &AppState,
    requester: &str,
    request: AccessRequest,
) -> Result<AccessAnswer, Refusal> {
    let target = request.plugin.trim();
    if target == requester {
        return Err(Refusal::bad("a plugin cannot ask itself for access"));
    }
    let manifest = settings::manifest_of(state, target)
        .await
        .ok_or_else(|| Refusal::new(404, "not-found", format!("{target} is not a plugin here")))?;
    let Some(setting) = requestable(&manifest, &request.setting) else {
        return Err(Refusal::new(
            400,
            "not-requestable",
            format!(
                "{target}'s `{}` is not a setting another plugin may ask to join",
                request.setting
            ),
        ));
    };
    if members(state, target, &manifest, &setting.key).await.iter().any(|held| held == requester) {
        return Ok(AccessAnswer {
            state: "granted".into(),
            id: None,
            raised: false,
            decided_by: None,
            decided_at: None,
        });
    }
    let held = state
        .repos
        .plugins
        .access_request_for(requester, target, &setting.key)
        .await
        .map_err(|err| failed(&err))?;
    if let Some(held) = held.as_ref().filter(|held| held.state != APPROVED) {
        return Ok(answer(held, false));
    }
    let reason: String = request.reason.trim().chars().take(MAX_REASON).collect();
    let record = AccessRequestRecord {
        id: held.map_or_else(Uuid::now_v7, |held| held.id),
        requester: requester.to_string(),
        target: target.to_string(),
        setting: setting.key.clone(),
        reason,
        state: PENDING.into(),
        created_at: Utc::now(),
        decided_at: None,
        decided_by: None,
    };
    state.repos.plugins.put_access_request(&record).await.map_err(|err| failed(&err))?;
    tracing::info!(requester, target, setting = %setting.key, "a plugin asked for access");
    notify(state, &record, setting).await;
    announce(state, &record, "requested").await;
    Ok(answer(&record, true))
}

/// Tells everyone who may change the target's settings, in their inbox, as the plugin asking.
async fn notify(state: &AppState, record: &AccessRequestRecord, setting: &Setting) {
    let people = match state.repos.identity.list_users().await {
        Ok(people) => people,
        Err(err) => {
            tracing::warn!(%err, "nobody could be told of an access request");
            return;
        }
    };
    let Ok(address) = Address::plugin("notifications") else { return };
    let label = match setting.label.is_empty() {
        true => setting.key.clone(),
        false => setting.label.clone(),
    };
    let title = format!("{} asks to be added to {}'s {label}", record.requester, record.target);
    let body = match record.reason.is_empty() {
        true => "Approve or deny it on its Settings page.".to_string(),
        false => format!("{} Approve or deny it on its Settings page.", record.reason),
    };
    let url = format!("/plugins/{}/settings#access-requests", record.target);
    let from = Principal::Plugin { id: record.requester.clone() }.reference();
    let mut told = 0;
    for listed in people.into_iter().take(MAX_PEOPLE) {
        let id = listed.user.id;
        let person = Principal::User(listed.user);
        if person.disabled() {
            continue;
        }
        if !crate::api::plugin_settings::admitted(state, &person, &record.target, Access::Write)
            .await
            .unwrap_or_default()
        {
            continue;
        }
        let body = json!({ "user": id, "title": title, "body": body, "url": url });
        let payload = json!({ "method": "POST", "body": body });
        let message =
            Message::new(address.clone(), "discovery/notify", payload).as_principal(&from);
        match state.buses.services.send(message).await {
            Ok(_) => told += 1,
            Err(err) => {
                tracing::warn!(%err, "an administrator could not be told of an access request")
            }
        }
    }
    tracing::info!(told, target = %record.target, "administrators told of an access request");
}

/// `platform.plugin.<requester>.access.<requested|approved|denied>`, for the plugin that asked.
async fn announce(state: &AppState, record: &AccessRequestRecord, what: &str) {
    let Ok(topic) = Topic::new(format!("platform.plugin.{}.access.{what}", record.requester))
    else {
        return;
    };
    let payload = json!({
        "id": record.id,
        "plugin": record.target,
        "setting": record.setting,
        "state": record.state,
        "decided_by": record.decided_by,
    });
    let event = Event::new(topic, super::SOURCE, payload);
    if let Err(err) = state.buses.events.publish(event).await {
        tracing::warn!(%err, "an access request's change was not announced");
    }
}

/// Records an administrator's decision, audits it and tells the plugin that asked. Approving has
/// already added the plugin to the setting, through an ordinary save.
pub async fn decided(
    state: &AppState,
    mut record: AccessRequestRecord,
    by: &Principal,
    approved: bool,
) -> Result<AccessRequestRecord, RepositoryError> {
    record.state = if approved { APPROVED } else { DENIED }.to_string();
    record.decided_at = Some(Utc::now());
    record.decided_by = Some(by.label());
    state.repos.plugins.put_access_request(&record).await?;
    let action = if approved { "plugin.access.approved" } else { "plugin.access.denied" };
    let detail =
        json!({ "requester": record.requester, "setting": record.setting, "id": record.id });
    super::audit(state, by, action, &record.target, detail).await;
    announce(state, &record, if approved { "approved" } else { "denied" }).await;
    Ok(record)
}

/// Requests for the target's settings, as its Settings page lists them: every one still waiting,
/// and those decided in the last month.
pub async fn listed(state: &AppState, target: &str, manifest: &Manifest) -> Vec<Value> {
    let held = state.repos.plugins.access_requests(target).await.unwrap_or_else(|err| {
        tracing::warn!(target, %err, "access requests could not be read");
        Vec::new()
    });
    let since = Utc::now() - Duration::days(SHOWN_FOR_DAYS);
    held.iter()
        .filter(|record| record.state == PENDING || record.decided_at.is_some_and(|at| at > since))
        .map(|record| {
            let label = manifest
                .settings
                .iter()
                .find(|setting| setting.key == record.setting)
                .map(|setting| setting.label.clone())
                .filter(|label| !label.is_empty())
                .unwrap_or_else(|| record.setting.clone());
            json!({
                "id": record.id,
                "requester": record.requester,
                "setting": record.setting,
                "setting_label": label,
                "reason": record.reason,
                "state": record.state,
                "created_at": record.created_at,
                "decided_at": record.decided_at,
                "decided_by": record.decided_by,
            })
        })
        .collect()
}
