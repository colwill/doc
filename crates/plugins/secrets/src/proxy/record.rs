//! The audit (ADR-0014 §12). Every proxied call is written down — who asked, which account
//! answered for them, what they reached, and what came back — and a call that cannot be written
//! down is not made.
//!
//! That ordering is the whole design. Because DOC no longer asks the vendor to narrow anything,
//! the vendor's own log shows one machine account for everybody (§7): this record is the only
//! place the real answer exists, so it is a precondition of proxying rather than a side effect.
//! The buffer below is bounded, and a full one stops the proxy instead of serving on quietly.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use doc_plugin_sdk::{Backend, DataRequest};
use serde_json::json;
use uuid::Uuid;

use crate::store::{CALLS, Call, FAILED, KEYS};

/// How many records one flush writes, so a long outage drains in rounds rather than one call.
const PER_FLUSH: usize = 200;

/// Records held while core cannot be reached, and written when it comes back. Bounded, because
/// an unbounded buffer is a promise to lose the oldest records at the worst moment.
pub struct Recorder {
    held: Mutex<VecDeque<Call>>,
    limit: usize,
    /// Set when the buffer filled. The proxy refuses everything while it is set, which is the one
    /// refusal that is not itself recorded — nothing reached the vendor, so there is nothing the
    /// audit is missing, and the plugin reports it as an error rather than hiding it.
    full: AtomicBool,
    /// How many calls have been written since the process started, for the plugin's own page.
    written: AtomicU64,
    /// When each key was last used. A convenience for the Keys page, kept apart from the audit
    /// on purpose: it is written on a separate call that is allowed to fail, so a key deleted
    /// under us can never stop a record being written.
    used: Mutex<BTreeMap<Uuid, DateTime<Utc>>>,
}

impl Recorder {
    pub fn new(limit: usize) -> Self {
        Self {
            held: Mutex::new(VecDeque::new()),
            limit: limit.max(1),
            full: AtomicBool::new(false),
            written: AtomicU64::new(0),
            used: Mutex::new(BTreeMap::new()),
        }
    }

    /// Whether there is room to record what is about to happen. Asked **before** the call is
    /// forwarded, never after.
    pub fn room(&self) -> bool {
        !self.full.load(Ordering::Relaxed)
    }

    pub fn waiting(&self) -> usize {
        self.held.lock().map(|held| held.len()).unwrap_or(0)
    }

    pub fn written(&self) -> u64 {
        self.written.load(Ordering::Relaxed)
    }

    /// Writes one record. Called as the call ends, including from the guard below when a caller
    /// walks away mid-stream, so it never blocks and never fails.
    pub fn write(&self, call: Call) {
        let Ok(mut held) = self.held.lock() else {
            tracing::error!("the audit buffer is poisoned; the proxy is stopping");
            self.full.store(true, Ordering::Relaxed);
            return;
        };
        if let (Some(key), Ok(mut used)) = (call.key, self.used.lock()) {
            let at = used.entry(key).or_insert(call.at);
            *at = (*at).max(call.at);
        }
        held.push_back(call);
        if held.len() >= self.limit && !self.full.swap(true, Ordering::Relaxed) {
            tracing::error!(
                held = held.len(),
                "the audit buffer is full: the proxy is refusing calls until it drains, because \
                 a call that cannot be recorded must not be made"
            );
        }
    }

    /// Writes what is waiting to core. Whatever core would not take goes back to the front, in
    /// order, so the log stays in the order things happened.
    pub async fn flush(&self, backend: &Backend) -> usize {
        let batch: Vec<Call> = {
            let Ok(mut held) = self.held.lock() else { return 0 };
            let taking = held.len().min(PER_FLUSH);
            held.drain(..taking).collect()
        };
        if batch.is_empty() {
            self.full.store(false, Ordering::Relaxed);
            return 0;
        }
        let writes: Vec<DataRequest> =
            batch.iter().map(|call| DataRequest::insert(CALLS, call.values())).collect();
        match backend.batch(writes).await {
            Ok(_) => {
                self.written.fetch_add(batch.len() as u64, Ordering::Relaxed);
                if let Ok(held) = self.held.lock()
                    && held.len() < self.limit
                {
                    self.full.store(false, Ordering::Relaxed);
                }
                batch.len()
            }
            Err(err) => {
                tracing::warn!(%err, held = batch.len(), "the audit was not written yet; holding it");
                if let Ok(mut held) = self.held.lock() {
                    for call in batch.into_iter().rev() {
                        held.push_front(call);
                    }
                    if held.len() >= self.limit {
                        self.full.store(true, Ordering::Relaxed);
                    }
                }
                0
            }
        }
    }
}

/// Notes on each key when it was last used, for the Keys page. Separate from the audit, and
/// allowed to fail: nothing here is the record of anything.
impl Recorder {
    pub async fn touch(&self, backend: &Backend) {
        let used: Vec<(Uuid, DateTime<Utc>)> = {
            let Ok(mut used) = self.used.lock() else { return };
            std::mem::take(&mut *used).into_iter().collect()
        };
        for (key, at) in used {
            if let Err(err) = backend
                .update::<serde_json::Value>(KEYS, json!(key), json!({ "last_used_at": at }), None)
                .await
            {
                tracing::debug!(%err, %key, "a key's last use was not noted");
            }
        }
    }
}

/// A call in progress, and the record it will leave behind. It is written when this is dropped,
/// which is the only way to be sure it happens: a caller who disconnects halfway through a clone
/// has still made the call, and their record says so.
pub struct Ledger {
    call: Mutex<Option<Call>>,
    sent: AtomicU64,
    received: AtomicU64,
    recorder: std::sync::Arc<Recorder>,
    started: std::time::Instant,
}

impl Ledger {
    pub fn new(call: Call, recorder: std::sync::Arc<Recorder>) -> Self {
        Self {
            call: Mutex::new(Some(call)),
            sent: AtomicU64::new(0),
            received: AtomicU64::new(0),
            recorder,
            started: std::time::Instant::now(),
        }
    }

    pub fn sent(&self, bytes: usize) {
        self.sent.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub fn received(&self, bytes: usize) {
        self.received.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    /// What the vendor answered, and its own ID for the call where it gave one, so the two logs
    /// can be joined on more than a timestamp.
    pub fn answered(&self, status: u16, vendor_call: Option<String>) {
        self.amend(|call| {
            call.status = Some(status.into());
            if let Some(id) = vendor_call {
                call.vendor_call = id;
            }
        });
    }

    /// The stream did not finish: the caller went away, the vendor did, or a ceiling was hit.
    pub fn cut_short(&self, why: &str) {
        self.amend(|call| {
            call.cut_short = true;
            if call.detail.is_empty() {
                call.detail = why.to_string();
            }
        });
    }

    pub fn failed(&self, why: &str) {
        self.amend(|call| {
            call.outcome = FAILED.to_string();
            call.detail = why.to_string();
        });
    }

    fn amend(&self, change: impl FnOnce(&mut Call)) {
        if let Ok(mut held) = self.call.lock()
            && let Some(call) = held.as_mut()
        {
            change(call);
        }
    }
}

impl Drop for Ledger {
    fn drop(&mut self) {
        let Ok(mut held) = self.call.lock() else { return };
        let Some(mut call) = held.take() else { return };
        call.sent = self.sent.load(Ordering::Relaxed) as i64;
        call.received = self.received.load(Ordering::Relaxed) as i64;
        call.ms = self.started.elapsed().as_millis().min(i64::MAX as u128) as i64;
        self.recorder.write(call);
    }
}

/// A record with the fields every call has, however it turns out.
pub fn started(replica: &str) -> Call {
    Call {
        id: Uuid::now_v7(),
        at: Utc::now(),
        correlation: Uuid::now_v7().to_string(),
        replica: replica.to_string(),
        ..Call::default()
    }
}
