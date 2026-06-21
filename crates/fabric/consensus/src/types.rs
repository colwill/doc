//! What a bus implements, and the Raft type configuration built from it.

use std::fmt::Debug;
use std::io::Cursor;
use std::marker::PhantomData;

use openraft::RaftTypeConfig;
use openraft::impls::{BasicNode, OneshotResponder};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub type NodeId = u64;

/// A bus supplies its commands, its replies and a snapshot of its state; everything else
/// (log, snapshots, network, elections) is the same for all three buses.
pub trait BusStateMachine: Send + Sync + 'static {
    type Command: Serialize + DeserializeOwned + Clone + Debug + Send + Sync + 'static;
    type Response: Serialize + DeserializeOwned + Clone + Debug + Default + Send + Sync + 'static;
    /// Must be cheap to produce (clone `Arc`s): it is taken under the apply lock, then
    /// serialised off it, so snapshots do not stall writes (ADR-0002).
    type Snapshot: Serialize + DeserializeOwned + Send + Sync + 'static;

    fn apply(&mut self, log_index: u64, command: Self::Command) -> Self::Response;
    fn snapshot(&self) -> Self::Snapshot;
    fn restore(&mut self, snapshot: Self::Snapshot);
}

/// The Raft type configuration for a bus. It carries no data; `M` only selects the types.
pub struct BusTypes<M>(PhantomData<fn() -> M>);

impl<M> Debug for BusTypes<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("BusTypes")
    }
}

impl<M> Clone for BusTypes<M> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<M> Copy for BusTypes<M> {}

impl<M> Default for BusTypes<M> {
    fn default() -> Self {
        Self(PhantomData)
    }
}

impl<M> PartialEq for BusTypes<M> {
    fn eq(&self, _other: &Self) -> bool {
        true
    }
}

impl<M> Eq for BusTypes<M> {}

impl<M> PartialOrd for BusTypes<M> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl<M> Ord for BusTypes<M> {
    fn cmp(&self, _other: &Self) -> std::cmp::Ordering {
        std::cmp::Ordering::Equal
    }
}

impl<M: BusStateMachine> RaftTypeConfig for BusTypes<M> {
    type D = M::Command;
    type R = M::Response;
    type NodeId = NodeId;
    type Node = BasicNode;
    type Entry = openraft::Entry<Self>;
    type SnapshotData = Cursor<Vec<u8>>;
    type AsyncRuntime = openraft::impls::TokioRuntime;
    type Responder = OneshotResponder<Self>;
}

pub type Raft<M> = openraft::Raft<BusTypes<M>>;
pub type Entry<M> = openraft::Entry<BusTypes<M>>;
pub type LogId = openraft::LogId<NodeId>;
pub type StoredMembership = openraft::StoredMembership<NodeId, BasicNode>;
pub type StorageError = openraft::StorageError<NodeId>;
pub type SnapshotMeta = openraft::SnapshotMeta<NodeId, BasicNode>;

/// Nodes are addressed as `host:port`; the host is also the name on the node's certificate.
pub fn server_name_of(addr: &str) -> &str {
    addr.split(':').next().unwrap_or(addr)
}
