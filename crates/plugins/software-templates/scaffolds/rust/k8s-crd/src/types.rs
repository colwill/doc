//! The custom resource itself. The CRD in `config/crd/bases` is generated from these types, so the
//! schema a cluster enforces and the struct the code uses are the same thing:
//!
//! ```sh
//! cargo run -- crd > config/crd/bases/crd.yaml
//! ```

use kube::CustomResource;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Where a {{ scaffold.kind }} has got to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub enum Phase {
    #[default]
    Pending,
    Ready,
    Failed,
}

/// What somebody asks for. Everything a cluster will be held to belongs here, with the validation
/// that makes a wrong one impossible rather than merely reported.
#[derive(CustomResource, Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[kube(
    group = "{{ scaffold.group }}",
    version = "v1alpha1",
    kind = "{{ scaffold.kind }}",
    plural = "{{ scaffold.plural }}",
    shortname = "{{ scaffold.kind | lower }}",
    namespaced,
    status = "{{ scaffold.kind }}Status",
    printcolumn = r#"{"name":"Owner","type":"string","jsonPath":".spec.owner"}"#,
    printcolumn = r#"{"name":"Size","type":"integer","jsonPath":".spec.size"}"#,
    printcolumn = r#"{"name":"Phase","type":"string","jsonPath":".status.phase"}"#,
    printcolumn = r#"{"name":"Age","type":"date","jsonPath":".metadata.creationTimestamp"}"#
)]
#[serde(rename_all = "camelCase")]
pub struct {{ scaffold.kind }}Spec {
    /// The team answering for it, as the Catalogue in DOC names them.
    #[schemars(regex(pattern = r"^[a-z][a-z0-9-]{1,62}$"))]
    pub owner: String,

    /// How many of them to run.
    #[schemars(range(min = 0, max = 100))]
    pub size: i32,

    /// How long what it makes is kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(regex(pattern = r"^[0-9]+(h|m|s)$"))]
    pub retention: Option<String>,

    /// Passed through to whatever runs it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub settings: BTreeMap<String, String>,
}

/// What is actually true of it.
#[derive(Debug, Clone, Default, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct {{ scaffold.kind }}Status {
    #[serde(default)]
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default)]
    pub observed_generation: i64,
}
