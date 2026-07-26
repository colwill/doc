//! The custom resource {{ values.name }} reconciles. The CRD is generated from this, so the type
//! and the schema cannot drift apart: `cargo run -- crd` writes it out.

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// What somebody asks for.
#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "{{ scaffold.group }}",
    version = "v1alpha1",
    kind = "{{ scaffold.kind }}",
    plural = "{{ scaffold.plural }}",
    namespaced,
    status = "{{ scaffold.kind }}Status",
    printcolumn = r#"{"name":"Size","type":"integer","jsonPath":".spec.size"}"#,
    printcolumn = r#"{"name":"Ready","type":"string","jsonPath":".status.ready"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct {{ scaffold.kind }}Spec {
    /// How many of them to run.
    #[schemars(range(min = 0, max = 100))]
    pub size: i32,

    /// What they say. DOC's `default-message` flag is used when this is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// What the cluster is actually doing about it.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct {{ scaffold.kind }}Status {
    /// Whether everything asked for is in place.
    pub ready: bool,
    /// Why it is, or is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The generation this status was worked out from.
    #[serde(default)]
    pub observed_generation: i64,
}
