//! What every change shares: who made it, the audit record and the event that announces it, and
//! who may assign a service account.

use std::time::Duration;

use doc_plugin_sdk::Backend;
use serde_json::{Value, json};

use crate::model::{Node, Refusal};
use crate::store::Store;

pub const CHANGED: &str = "plugin.resources.changed";

pub fn actor(backend: &Backend) -> String {
    backend
        .caller()
        .and_then(|caller| caller.label.clone().or_else(|| caller.id.clone()))
        .unwrap_or_else(|| "platform".into())
}

/// Audits a change and announces the catalogue's new version, for caches such as the Service Map's.
pub async fn record(
    backend: &Backend,
    store: &Store<'_>,
    action: &str,
    subject: &str,
    detail: Value,
) {
    if let Err(err) = backend.audit(action, Some(subject), detail).await {
        tracing::warn!(%err, action, "a change was made but could not be audited");
    }
    let version = store.version().await.unwrap_or_default();
    let change = json!({ "change": action, "subject": subject, "version": version });
    if let Err(err) = backend.publish(CHANGED, change).await {
        tracing::warn!(%err, action, "a change was made but could not be announced");
    }
}

/// Its owner may assign a service account, and so may an RBAC admin: anyone who can write to `rbac`.
pub async fn may_assign(
    backend: &Backend,
    store: &Store<'_>,
    account: &Node,
) -> Result<(), Refusal> {
    let caller = backend.caller().ok_or_else(|| Refusal::forbidden("nobody is asking"))?;
    if caller.admin {
        return Ok(());
    }
    let owner = store.service_account_owner(&account.reference).await?;
    if caller.kind == "user"
        && owner.is_some_and(|owner| caller.id.as_deref() == Some(&owner.to_string()))
    {
        return Ok(());
    }
    let access = backend
        .request("core.access", "access", json!({}), Duration::from_secs(10))
        .await
        .map_err(|err| Refusal::unavailable(format!("core could not say who you are: {err}")))?;
    if access["admin"] == true || access["plugins"]["rbac"]["write"] == true {
        return Ok(());
    }
    Err(Refusal::forbidden(format!(
        "only {}'s owner or an RBAC admin may assign it to a resource",
        account.name
    )))
}
