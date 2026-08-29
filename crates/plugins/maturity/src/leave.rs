//! Leave to read the Catalogue as somebody.
//!
//! Every automatic criterion is decided from what the Catalogue holds, and the Catalogue answers
//! as whoever is asking. A run nobody started has nobody to ask as: the plugin itself holds no
//! `plugin:resources:user:ro`, so the Catalogue refuses it and the run scores nothing at all —
//! quietly, since a refusal for one model is only a problem in that run's output.
//!
//! So whenever somebody writes a model or a criterion, attests to one, or asks for a score, the
//! plugin takes leave to act as them and keeps it. The schedule and a Catalogue change then score
//! as whoever last gave it. Leave is theirs: it is checked as they are now, so it stops working
//! the moment they lose the right to read the Catalogue, and a new one replaces it.

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, PluginError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::ID;

/// Where the leave is kept between loads.
const STATE: &str = "reading";

/// What the plugin holds so a run nobody started can still read the Catalogue.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Reading {
    pub delegation: Option<Uuid>,
    /// Who gave it, as the page says so plainly rather than leaving it to be guessed.
    pub by: Option<String>,
    pub granted_at: Option<DateTime<Utc>>,
}

pub async fn reading(backend: &Backend) -> Reading {
    match backend.state_get(STATE).await {
        Ok(Some(value)) => serde_json::from_value(value).unwrap_or_default(),
        _ => Reading::default(),
    }
}

/// Takes leave to read the Catalogue as `who`, in place of whoever gave it before. Called
/// wherever somebody asks for scoring, so the plugin is never left without one while anybody is
/// using it. A failure is logged and nothing else: it must not stop the thing they actually did.
pub async fn grant(backend: &Backend, who: &str) {
    let purpose =
        format!("maturity: reading the Catalogue to score what the models grade, for {who}");
    let delegation = match backend.delegate(&purpose).await {
        Ok(delegation) => delegation,
        Err(err) => {
            tracing::warn!(%err, plugin = ID, "leave to read the Catalogue could not be taken");
            return;
        }
    };
    let before = reading(backend).await;
    if let Some(old) = before.delegation
        && old != delegation
        && let Err(err) = backend.revoke(old).await
    {
        tracing::warn!(%err, plugin = ID, "the earlier leave could not be given back");
    }
    let held = Reading {
        delegation: Some(delegation),
        by: Some(who.to_string()),
        granted_at: Some(Utc::now()),
    };
    if let Err(err) = backend.state_set(STATE, json!(held)).await {
        tracing::warn!(%err, plugin = ID, "leave to read the Catalogue could not be kept");
    }
}

/// Queues a scoring run as whoever gave leave, for a run nobody started. Answers what happened,
/// which is what the schedule's own run reports so a platform with no leave says so rather than
/// scoring nothing and looking healthy.
pub async fn score_as_whoever_gave_leave(
    backend: &Backend,
    payload: Value,
) -> Result<Value, PluginError> {
    let held = reading(backend).await;
    let (Some(delegation), Some(by)) = (held.delegation, held.by.clone()) else {
        tracing::warn!(
            plugin = ID,
            "nothing is scored on its own: nobody has given leave to read the Catalogue. Open a \
             model and score it once, and the schedule will follow as you."
        );
        return Ok(json!({
            "scored": false,
            "why": "nobody has given leave to read the Catalogue; score a model once and this \
                    follows as whoever did",
        }));
    };
    match backend.task_as(delegation, payload, Some(1)).await {
        Ok(task) => Ok(json!({ "scored": true, "task": task, "as": by })),
        Err(err) => {
            tracing::warn!(%err, plugin = ID, %by, "a run could not be queued as whoever gave leave");
            Ok(json!({ "scored": false, "why": err.detail(), "as": by }))
        }
    }
}
