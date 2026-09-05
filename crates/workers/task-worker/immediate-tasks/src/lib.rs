//! TaskWorker immediate tasks pool, used inside the backend and frontend. Nothing is written down:
//! the caller is holding a request open, so work that misses the deadline is an error rather than
//! something to retry, and a full pool is refused rather than queued without limit.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ImmediateError {
    #[error("the immediate pool is busy")]
    Busy,
    #[error("the work did not finish within {0:?}")]
    Timeout(Duration),
    #[error("{0}")]
    Failed(String),
}

#[derive(Clone)]
pub struct Pool {
    permits: Arc<Semaphore>,
    deadline: Duration,
}

impl Pool {
    pub fn new(concurrency: usize, deadline: Duration) -> Self {
        Self { permits: Arc::new(Semaphore::new(concurrency.max(1))), deadline }
    }

    pub fn deadline(&self) -> Duration {
        self.deadline
    }

    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }

    /// Waiting for a permit counts against the deadline, so a caller never waits longer than it
    /// asked to whether the pool was busy or the work was slow.
    pub async fn run<F, T>(&self, work: F) -> Result<T, ImmediateError>
    where
        F: Future<Output = Result<T, String>> + Send,
    {
        let started = tokio::time::Instant::now();
        let permit = tokio::time::timeout(self.deadline, self.permits.clone().acquire_owned())
            .await
            .map_err(|_| ImmediateError::Busy)?
            .map_err(|err| ImmediateError::Failed(err.to_string()))?;
        let left = self.deadline.saturating_sub(started.elapsed());
        let outcome = tokio::time::timeout(left, work).await;
        drop(permit);
        match outcome {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(reason)) => Err(ImmediateError::Failed(reason)),
            Err(_) => Err(ImmediateError::Timeout(self.deadline)),
        }
    }
}

impl Default for Pool {
    fn default() -> Self {
        Self::new(32, Duration::from_secs(5))
    }
}
