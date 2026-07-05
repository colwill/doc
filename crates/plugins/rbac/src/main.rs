//! RBAC core plugin (T26): groups as roles, assignments, attributes and onboarding rules, and the
//! permission provider core asks whenever it holds no cached decision for a caller.

mod access_map;
mod api;
mod checks;
mod model;
mod offboarding;
mod onboarding;
mod ops;
mod store;
mod ui;

use async_trait::async_trait;
use doc_plugin_sdk::{
    Backend, Capability, Classification, CustomPermission, Event, Manifest, Nav, Plugin,
    PluginError, Request, ResourcePanel, Response, RunInput, RunOutput,
};
use serde_json::Value;

#[derive(Default)]
struct Rbac;

#[async_trait]
impl Plugin for Rbac {
    async fn load(
        &mut self,
        backend: &Backend,
        _previous: Option<Value>,
    ) -> Result<(), PluginError> {
        tracing::info!(version = backend.version(), "rbac loaded");
        Ok(())
    }

    async fn unload(&mut self, _backend: &Backend) -> Result<Option<Value>, PluginError> {
        Ok(None)
    }

    async fn run(&self, backend: &Backend, input: RunInput) -> Result<RunOutput, PluginError> {
        Ok(RunOutput { payload: onboarding::run(backend, input.payload).await? })
    }

    async fn cancel(&self, _backend: &Backend) -> Result<(), PluginError> {
        Ok(())
    }

    async fn handle(&self, backend: &Backend, request: Request) -> Response {
        api::handle(backend, request).await
    }

    async fn on_event(&self, backend: &Backend, event: Event) -> Result<(), PluginError> {
        match event.topic.as_str() {
            onboarding::SIGNED_IN => onboarding::on_sign_in(backend, event.payload).await,
            offboarding::DEPROVISIONED => {
                offboarding::on_deprovisioned(backend, event.payload).await
            }
            ops::MERGED => ops::merged(backend, event.payload).await,
            _ => Ok(()),
        }
    }
}

doc_plugin_sdk::main!(
    Rbac,
    Manifest {
        id: "rbac".into(),
        classification: Classification::Synchronous,
        capabilities: vec![Capability::PermissionProvider, Capability::Offboarding],
        // `plugin:rbac:user:rw` alone reaches the groups its holder is in; this reaches them all.
        custom_permissions: vec![CustomPermission::user(ops::ADMIN)],
        subscriptions: vec![
            onboarding::SIGNED_IN.into(),
            offboarding::DEPROVISIONED.into(),
            ops::MERGED.into(),
        ],
        nav: vec![
            Nav::new("Access control", "/")
                .described(
                    "Groups, permissions and onboarding rules for people and service accounts"
                )
                .grouped("Admin")
        ],
        resource_panels: vec![ResourcePanel::new(
            "service",
            "Who can reach it",
            "/access-map/panel",
        )],
        data: store::declaration(),
        ..Manifest::default()
    }
);
