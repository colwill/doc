//! The Raft log in redb. One writer thread batches appends into a single fsynced commit
//! (group commit, ADR-0002); readers use redb's concurrent read transactions.

use std::ops::{Bound, RangeBounds};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use openraft::storage::{LogFlushed, RaftLogStorage};
use openraft::{AnyError, LogState, OptionalSend, RaftLogReader, StorageIOError, Vote};
use redb::{Database, ReadableDatabase, TableDefinition};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::{mpsc, oneshot};

use crate::types::{BusStateMachine, BusTypes, Entry, LogId, NodeId, StorageError};

const LOGS: TableDefinition<'_, u64, &[u8]> = TableDefinition::new("logs");
const META: TableDefinition<'_, &str, &[u8]> = TableDefinition::new("meta");
const MAX_BATCH: usize = 256;

enum Write<M: BusStateMachine> {
    Append { rows: Vec<(u64, Vec<u8>)>, flushed: LogFlushed<BusTypes<M>> },
    Truncate { from: u64, done: oneshot::Sender<Result<(), String>> },
    Purge { upto: u64, done: oneshot::Sender<Result<(), String>> },
    Meta { key: &'static str, value: Vec<u8>, done: oneshot::Sender<Result<(), String>> },
}

pub struct LogStore<M: BusStateMachine> {
    db: Arc<Database>,
    writes: mpsc::UnboundedSender<Write<M>>,
}

impl<M: BusStateMachine> Clone for LogStore<M> {
    fn clone(&self) -> Self {
        Self { db: self.db.clone(), writes: self.writes.clone() }
    }
}

impl<M: BusStateMachine> LogStore<M> {
    pub fn open(dir: &Path, durable: bool) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let db = Database::create(dir.join("raft-log.redb")).context("opening the Raft log")?;
        let transaction = db.begin_write()?;
        transaction.open_table(LOGS)?;
        transaction.open_table(META)?;
        transaction.commit()?;
        let db = Arc::new(db);

        let (writes, mut receiver) = mpsc::unbounded_channel::<Write<M>>();
        let writer_db = db.clone();
        std::thread::Builder::new()
            .name("raft-log-writer".into())
            .spawn(move || {
                while let Some(first) = receiver.blocking_recv() {
                    let mut batch = vec![first];
                    while batch.len() < MAX_BATCH {
                        match receiver.try_recv() {
                            Ok(next) => batch.push(next),
                            Err(_) => break,
                        }
                    }
                    commit_batch(&writer_db, durable, batch);
                }
            })
            .context("starting the Raft log writer")?;

        Ok(Self { db, writes })
    }

    async fn submit(
        &self,
        make: impl FnOnce(oneshot::Sender<Result<(), String>>) -> Write<M>,
    ) -> Result<(), StorageError> {
        let (done, wait) = oneshot::channel();
        self.writes.send(make(done)).map_err(|_| write_error("the log writer stopped"))?;
        match wait.await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(message)) => Err(write_error(message)),
            Err(_) => Err(write_error("the log writer dropped the request")),
        }
    }

    async fn read<T: Send + 'static>(
        &self,
        f: impl FnOnce(&Database) -> Result<T, AnyError> + Send + 'static,
    ) -> Result<T, StorageError> {
        let db = self.db.clone();
        tokio::task::spawn_blocking(move || f(&db))
            .await
            .map_err(|e| StorageError::from(StorageIOError::read_logs(AnyError::new(&e))))?
            .map_err(|e| StorageError::from(StorageIOError::read_logs(e)))
    }

    async fn meta<T: DeserializeOwned + Send + 'static>(
        &self,
        key: &'static str,
    ) -> Result<Option<T>, StorageError> {
        self.read(move |db| {
            let transaction = db.begin_read().map_err(any)?;
            let table = transaction.open_table(META).map_err(any)?;
            match table.get(key).map_err(any)? {
                Some(value) => Ok(Some(serde_json::from_slice(value.value()).map_err(any)?)),
                None => Ok(None),
            }
        })
        .await
    }

    async fn put_meta<T: Serialize>(
        &self,
        key: &'static str,
        value: &T,
    ) -> Result<(), StorageError> {
        let bytes = serde_json::to_vec(value).map_err(|e| write_error(e.to_string()))?;
        self.submit(|done| Write::Meta { key, value: bytes, done }).await
    }
}

fn any<E: std::error::Error + 'static>(e: E) -> AnyError {
    AnyError::new(&e)
}

fn write_error(message: impl ToString) -> StorageError {
    StorageError::from(StorageIOError::write_logs(AnyError::error(message)))
}

fn commit_batch<M: BusStateMachine>(db: &Database, durable: bool, batch: Vec<Write<M>>) {
    let result = (|| -> Result<(), String> {
        let mut transaction = db.begin_write().map_err(|e| e.to_string())?;
        if !durable {
            transaction.set_durability(redb::Durability::None).map_err(|e| e.to_string())?;
        }
        {
            let mut logs = transaction.open_table(LOGS).map_err(|e| e.to_string())?;
            let mut meta = transaction.open_table(META).map_err(|e| e.to_string())?;
            for write in &batch {
                match write {
                    Write::Append { rows, .. } => {
                        for (index, bytes) in rows {
                            logs.insert(*index, bytes.as_slice()).map_err(|e| e.to_string())?;
                        }
                    }
                    Write::Truncate { from, .. } => {
                        logs.retain_in(*from.., |_, _| false).map_err(|e| e.to_string())?;
                    }
                    Write::Purge { upto, .. } => {
                        logs.retain_in(..=*upto, |_, _| false).map_err(|e| e.to_string())?;
                    }
                    Write::Meta { key, value, .. } => {
                        meta.insert(*key, value.as_slice()).map_err(|e| e.to_string())?;
                    }
                }
            }
        }
        transaction.commit().map_err(|e| e.to_string())
    })();

    for write in batch {
        match write {
            Write::Append { flushed, .. } => match &result {
                Ok(()) => flushed.log_io_completed(Ok(())),
                Err(message) => {
                    flushed.log_io_completed(Err(std::io::Error::other(message.clone())))
                }
            },
            Write::Truncate { done, .. } | Write::Purge { done, .. } | Write::Meta { done, .. } => {
                let _ = done.send(result.clone());
            }
        }
    }
}

impl<M: BusStateMachine> RaftLogReader<BusTypes<M>> for LogStore<M> {
    async fn try_get_log_entries<RB: RangeBounds<u64> + Clone + std::fmt::Debug + OptionalSend>(
        &mut self,
        range: RB,
    ) -> Result<Vec<Entry<M>>, StorageError> {
        let bounds: (Bound<u64>, Bound<u64>) =
            (range.start_bound().cloned(), range.end_bound().cloned());
        self.read(move |db| {
            let transaction = db.begin_read().map_err(any)?;
            let table = transaction.open_table(LOGS).map_err(any)?;
            let mut entries = Vec::new();
            for row in table.range(bounds).map_err(any)? {
                let (_, value) = row.map_err(any)?;
                entries.push(serde_json::from_slice(value.value()).map_err(any)?);
            }
            Ok(entries)
        })
        .await
    }
}

impl<M: BusStateMachine> RaftLogStorage<BusTypes<M>> for LogStore<M> {
    type LogReader = Self;

    async fn get_log_state(&mut self) -> Result<LogState<BusTypes<M>>, StorageError> {
        let last_purged: Option<LogId> = self.meta("last_purged").await?;
        let last = self
            .read(|db| {
                let transaction = db.begin_read().map_err(any)?;
                let table = transaction.open_table(LOGS).map_err(any)?;
                match table.range::<u64>(..).map_err(any)?.next_back() {
                    Some(row) => {
                        let (_, value) = row.map_err(any)?;
                        let entry: serde_json::Value =
                            serde_json::from_slice(value.value()).map_err(any)?;
                        Ok(Some(
                            serde_json::from_value::<LogId>(entry["log_id"].clone())
                                .map_err(any)?,
                        ))
                    }
                    None => Ok(None),
                }
            })
            .await?;
        Ok(LogState { last_purged_log_id: last_purged, last_log_id: last.or(last_purged) })
    }

    async fn get_log_reader(&mut self) -> Self::LogReader {
        self.clone()
    }

    async fn save_vote(&mut self, vote: &Vote<NodeId>) -> Result<(), StorageError> {
        self.put_meta("vote", vote).await
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<NodeId>>, StorageError> {
        self.meta("vote").await
    }

    async fn save_committed(&mut self, committed: Option<LogId>) -> Result<(), StorageError> {
        self.put_meta("committed", &committed).await
    }

    async fn read_committed(&mut self) -> Result<Option<LogId>, StorageError> {
        Ok(self.meta::<Option<LogId>>("committed").await?.flatten())
    }

    async fn append<I>(
        &mut self,
        entries: I,
        flushed: LogFlushed<BusTypes<M>>,
    ) -> Result<(), StorageError>
    where
        I: IntoIterator<Item = Entry<M>> + OptionalSend,
        I::IntoIter: OptionalSend,
    {
        let mut rows = Vec::new();
        for entry in entries {
            let bytes = serde_json::to_vec(&entry).map_err(|e| write_error(e.to_string()))?;
            rows.push((entry.log_id.index, bytes));
        }
        self.writes
            .send(Write::Append { rows, flushed })
            .map_err(|_| write_error("the log writer stopped"))
    }

    async fn truncate(&mut self, log_id: LogId) -> Result<(), StorageError> {
        self.submit(|done| Write::Truncate { from: log_id.index, done }).await
    }

    async fn purge(&mut self, log_id: LogId) -> Result<(), StorageError> {
        self.put_meta("last_purged", &log_id).await?;
        self.submit(|done| Write::Purge { upto: log_id.index, done }).await
    }
}
