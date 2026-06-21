//! The state machine wrapper: applies commands, and writes snapshots to one file per node
//! without holding the apply lock while serialising (ADR-0002).

use std::io::{Cursor, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use anyhow::{Context, Result};
use openraft::storage::RaftStateMachine;
use openraft::{
    AnyError, EntryPayload, OptionalSend, RaftSnapshotBuilder, Snapshot, StorageIOError,
};
use parking_lot::RwLock;

use crate::types::{
    BusStateMachine, BusTypes, Entry, LogId, SnapshotMeta, StorageError, StoredMembership,
};

struct Inner<M> {
    machine: M,
    last_applied: Option<LogId>,
    last_membership: StoredMembership,
}

/// Shared state machine: Raft applies through it, and readers take the read lock for local reads.
pub struct StateMachine<M: BusStateMachine> {
    inner: Arc<RwLock<Inner<M>>>,
    dir: PathBuf,
    sequence: Arc<AtomicU64>,
}

impl<M: BusStateMachine> Clone for StateMachine<M> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone(), dir: self.dir.clone(), sequence: self.sequence.clone() }
    }
}

impl<M: BusStateMachine> StateMachine<M> {
    /// Opens the store and restores the newest snapshot, if there is one.
    pub fn open(dir: &Path, machine: M) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let store = Self {
            inner: Arc::new(RwLock::new(Inner {
                machine,
                last_applied: None,
                last_membership: StoredMembership::default(),
            })),
            dir: dir.to_path_buf(),
            sequence: Arc::new(AtomicU64::new(0)),
        };
        if let Some((meta, bytes)) = read_snapshot_file(dir)? {
            let snapshot: M::Snapshot =
                serde_json::from_slice(&bytes).context("reading the snapshot body")?;
            let mut inner = store.inner.write();
            inner.machine.restore(snapshot);
            inner.last_applied = meta.last_log_id;
            inner.last_membership = meta.last_membership.clone();
            tracing::info!(
                last_log_id = ?meta.last_log_id,
                bytes = bytes.len(),
                "restored the state machine from its snapshot"
            );
        }
        Ok(store)
    }

    /// Runs `f` against the state machine under a read lock, for local (possibly stale) reads.
    pub fn read<T>(&self, f: impl FnOnce(&M) -> T) -> T {
        let inner = self.inner.read();
        f(&inner.machine)
    }

    pub fn last_applied(&self) -> Option<LogId> {
        self.inner.read().last_applied
    }
}

fn write_snapshot_file(dir: &Path, meta: &SnapshotMeta, data: &[u8]) -> std::io::Result<()> {
    let meta_bytes = serde_json::to_vec(meta).map_err(std::io::Error::other)?;
    let temporary = dir.join("snapshot.tmp");
    {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(&(meta_bytes.len() as u32).to_be_bytes())?;
        file.write_all(&meta_bytes)?;
        file.write_all(data)?;
        file.sync_all()?;
    }
    std::fs::rename(&temporary, dir.join("snapshot.bin"))?;
    std::fs::File::open(dir)?.sync_all()
}

fn read_snapshot_file(dir: &Path) -> Result<Option<(SnapshotMeta, Vec<u8>)>> {
    let bytes = match std::fs::read(dir.join("snapshot.bin")) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err).context("reading the snapshot file"),
    };
    if bytes.len() < 4 {
        return Ok(None);
    }
    let length = u32::from_be_bytes(bytes[..4].try_into().expect("four bytes")) as usize;
    let meta =
        serde_json::from_slice(&bytes[4..4 + length]).context("reading snapshot metadata")?;
    Ok(Some((meta, bytes[4 + length..].to_vec())))
}

impl<M: BusStateMachine> RaftSnapshotBuilder<BusTypes<M>> for StateMachine<M> {
    async fn build_snapshot(&mut self) -> Result<Snapshot<BusTypes<M>>, StorageError> {
        let started = Instant::now();
        let (snapshot, last_applied, last_membership) = {
            let inner = self.inner.read();
            (inner.machine.snapshot(), inner.last_applied, inner.last_membership.clone())
        };
        let sequence = self.sequence.fetch_add(1, Ordering::SeqCst) + 1;
        let snapshot_id = match last_applied {
            Some(id) => format!("{}-{}-{sequence}", id.leader_id, id.index),
            None => format!("empty-{sequence}"),
        };
        let meta = SnapshotMeta { last_log_id: last_applied, last_membership, snapshot_id };

        let dir = self.dir.clone();
        let for_file = meta.clone();
        let data = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, std::io::Error> {
            let data = serde_json::to_vec(&snapshot).map_err(std::io::Error::other)?;
            write_snapshot_file(&dir, &for_file, &data)?;
            Ok(data)
        })
        .await
        .map_err(|e| StorageError::from(StorageIOError::write_snapshot(None, AnyError::new(&e))))?
        .map_err(|e| {
            StorageError::from(StorageIOError::write_snapshot(
                Some(meta.signature()),
                AnyError::new(&e),
            ))
        })?;

        tracing::info!(
            last_log_id = ?meta.last_log_id,
            bytes = data.len(),
            ms = started.elapsed().as_millis() as u64,
            "snapshot written"
        );
        Ok(Snapshot { meta, snapshot: Box::new(Cursor::new(data)) })
    }
}

impl<M: BusStateMachine> RaftStateMachine<BusTypes<M>> for StateMachine<M> {
    type SnapshotBuilder = Self;

    async fn applied_state(&mut self) -> Result<(Option<LogId>, StoredMembership), StorageError> {
        let inner = self.inner.read();
        Ok((inner.last_applied, inner.last_membership.clone()))
    }

    async fn apply<I>(&mut self, entries: I) -> Result<Vec<M::Response>, StorageError>
    where
        I: IntoIterator<Item = Entry<M>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let mut inner = self.inner.write();
        let mut responses = Vec::new();
        for entry in entries {
            inner.last_applied = Some(entry.log_id);
            match entry.payload {
                EntryPayload::Blank => responses.push(M::Response::default()),
                EntryPayload::Normal(command) => {
                    let response = inner.machine.apply(entry.log_id.index, command);
                    responses.push(response);
                }
                EntryPayload::Membership(membership) => {
                    inner.last_membership = StoredMembership::new(Some(entry.log_id), membership);
                    responses.push(M::Response::default());
                }
            }
        }
        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self::SnapshotBuilder {
        self.clone()
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Cursor<Vec<u8>>>, StorageError> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<(), StorageError> {
        let data = snapshot.into_inner();
        let restored: M::Snapshot = serde_json::from_slice(&data).map_err(|e| {
            StorageError::from(StorageIOError::read_snapshot(
                Some(meta.signature()),
                AnyError::new(&e),
            ))
        })?;
        {
            let mut inner = self.inner.write();
            inner.machine.restore(restored);
            inner.last_applied = meta.last_log_id;
            inner.last_membership = meta.last_membership.clone();
        }
        let dir = self.dir.clone();
        let for_file = meta.clone();
        tokio::task::spawn_blocking(move || write_snapshot_file(&dir, &for_file, &data))
            .await
            .map_err(|e| {
                StorageError::from(StorageIOError::write_snapshot(None, AnyError::new(&e)))
            })?
            .map_err(|e| {
                StorageError::from(StorageIOError::write_snapshot(
                    Some(meta.signature()),
                    AnyError::new(&e),
                ))
            })?;
        tracing::info!(last_log_id = ?meta.last_log_id, "installed a snapshot from the leader");
        Ok(())
    }

    async fn get_current_snapshot(
        &mut self,
    ) -> Result<Option<Snapshot<BusTypes<M>>>, StorageError> {
        let dir = self.dir.clone();
        let read = tokio::task::spawn_blocking(move || read_snapshot_file(&dir))
            .await
            .map_err(|e| {
                StorageError::from(StorageIOError::read_snapshot(None, AnyError::new(&e)))
            })?
            .map_err(|e| {
                StorageError::from(StorageIOError::read_snapshot(
                    None,
                    AnyError::error(e.to_string()),
                ))
            })?;
        Ok(read.map(|(meta, data)| Snapshot { meta, snapshot: Box::new(Cursor::new(data)) }))
    }
}
