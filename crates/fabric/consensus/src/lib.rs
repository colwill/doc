//! Raft consensus, storage, transport and client shared by the three fabric buses (ADR-0002).
//! A bus supplies a `BusStateMachine` and its own routes; everything else lives here.

// openraft's trait signatures return its own large error types, which we cannot box.
#![allow(clippy::result_large_err)]

pub mod client;
pub mod log;
pub mod metrics;
pub mod network;
pub mod node;
pub mod service;
pub mod snapshot;
pub mod types;

pub use client::{ClusterClient, ClusterError};
pub use metrics::BusMetrics;
pub use node::{Applied, BusService, Node, NodeConfig, NodeError, Redirect, Tuning, node_error};
pub use service::{BusArgs, init_tracing, run};
pub use snapshot::StateMachine;
pub use types::{BusStateMachine, BusTypes, NodeId, server_name_of};
