//! Reconciling {{ scaffold.kind }}s: look at what was asked for, look at what the cluster has, and
//! make the second match the first. Every reconcile is a span, so a slow one shows up in the
//! platform's telemetry beside everything else.

use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::ConfigMap;
use kube::api::{Patch, PatchParams};
use kube::runtime::controller::Action;
use kube::{Api, Client, Resource, ResourceExt};
use serde_json::json;
use tracing::instrument;

use crate::platform::{Config, Flags};
use crate::types::{{ scaffold.kind }};

/// What every reconcile is given.
pub struct Context {
    pub client: Client,
    pub config: Config,
    pub flags: Flags,
}

#[derive(Debug, thiserror::Error)]
pub enum Problem {
    #[error("the cluster refused it: {0}")]
    Cluster(#[from] kube::Error),
}

/// Brings one {{ scaffold.kind }} to what it asks for.
#[instrument(skip_all, fields(resource = %wanted.name_any()))]
pub async fn reconcile(
    wanted: Arc<{{ scaffold.kind }}>,
    context: Arc<Context>,
) -> Result<Action, Problem> {
    // Turned off in DOC, the controller watches without changing anything: a way to stop it acting
    // without stopping it running.
    if !context.flags.bool("reconcile", true) {
        tracing::info!("reconciling is turned off in DOC; nothing was changed");
        return Ok(Action::requeue(Duration::from_secs(60)));
    }

    let namespace = wanted.namespace().unwrap_or_else(|| "default".to_string());
    let name = wanted.name_any();
    let owner = wanted.controller_owner_ref(&()).map(|owner| vec![owner]);

    let message = wanted
        .spec
        .message
        .clone()
        .filter(|message| !message.is_empty())
        .unwrap_or_else(|| context.flags.string("default-message", "Made by {{ values.name }}"));

    let maps: Api<ConfigMap> = Api::namespaced(context.client.clone(), &namespace);
    let wanted_map = json!({
        "apiVersion": "v1",
        "kind": "ConfigMap",
        "metadata": { "name": name, "namespace": namespace, "ownerReferences": owner },
        "data": { "message": message, "size": wanted.spec.size.to_string() },
    });
    maps.patch(&name, &PatchParams::apply("{{ values.name }}"), &Patch::Apply(&wanted_map)).await?;

    let resources: Api<{{ scaffold.kind }}> = Api::namespaced(context.client.clone(), &namespace);
    let status = json!({
        "status": {
            "ready": true,
            "reason": "Reconciled",
            "observedGeneration": wanted.meta().generation.unwrap_or_default(),
        }
    });
    resources
        .patch_status(&name, &PatchParams::apply("{{ values.name }}").force(), &Patch::Apply(&status))
        .await?;

    tracing::info!("everything asked for is in place");
    Ok(Action::requeue(Duration::from_secs(300)))
}

/// What to do when a reconcile fails: try again, more slowly each time.
pub fn on_error(_wanted: Arc<{{ scaffold.kind }}>, problem: &Problem, _context: Arc<Context>) -> Action {
    tracing::warn!(%problem, "the reconcile failed; it will be tried again");
    Action::requeue(Duration::from_secs(30))
}
