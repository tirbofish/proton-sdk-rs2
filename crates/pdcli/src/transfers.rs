use std::sync::{Arc, RwLock};
use std::time::Instant;

use tokio_util::sync::CancellationToken;

/// Shared transfer log visible to both the FUSE/index layer and the egui UI.
/// Uses `std::sync::RwLock` (not tokio) so it can be updated synchronously
/// from any thread — including download progress callbacks that may not run
/// on the tokio runtime.
#[derive(Clone)]
pub struct TransferLog {
    inner: Arc<RwLock<Vec<TransferEntry>>>,
}

impl TransferLog {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Insert a new transfer. Returns its index for later updates.
    pub fn add(&self, entry: TransferEntry) -> usize {
        let mut log = self.inner.write().unwrap();
        let idx = log.len();
        log.push(entry);
        idx
    }

    /// Update progress of an existing transfer.
    pub fn set_progress(&self, idx: usize, downloaded: i64, total: i64) {
        let mut log = self.inner.write().unwrap();
        if let Some(entry) = log.get_mut(idx) {
            entry.bytes_transferred = downloaded;
            entry.total_bytes = total;
            if total > 0 {
                entry.progress = Some(downloaded as f32 / total as f32);
            }
        }
    }

    /// Mark a transfer as completed.
    pub fn set_done(&self, idx: usize) {
        let mut log = self.inner.write().unwrap();
        if let Some(entry) = log.get_mut(idx) {
            entry.status = TransferStatus::Done;
            entry.progress = Some(1.0);
        }
    }

    /// Mark a transfer as failed.
    pub fn set_failed(&self, idx: usize, error: String) {
        let mut log = self.inner.write().unwrap();
        if let Some(entry) = log.get_mut(idx) {
            entry.status = TransferStatus::Failed;
            entry.error = Some(error);
        }
    }

    /// Return a snapshot of all transfers for the UI.
    pub fn snapshot(&self) -> Vec<TransferEntry> {
        self.inner.read().unwrap().clone()
    }

    /// Remove completed/failed transfers older than `max_age`.
    pub fn prune(&self, max_age: std::time::Duration) {
        let cutoff = Instant::now() - max_age;
        let mut log = self.inner.write().unwrap();
        log.retain(|e| {
            matches!(e.status, TransferStatus::Pending | TransferStatus::InProgress)
                || e.started_at > cutoff
        });
    }

    /// Cancel a transfer by index. Sets status to Failed("Cancelled") and
    /// triggers the cancellation token if present.
    pub fn cancel(&self, idx: usize) {
        let mut log = self.inner.write().unwrap();
        if let Some(entry) = log.get_mut(idx) {
            if matches!(entry.status, TransferStatus::InProgress | TransferStatus::Pending) {
                if let Some(token) = entry.cancel_token.take() {
                    token.cancel();
                }
                entry.status = TransferStatus::Failed;
                entry.error = Some("Cancelled".to_string());
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransferEntry {
    pub name: String,
    pub kind: TransferKind,
    pub status: TransferStatus,
    pub progress: Option<f32>,
    pub bytes_transferred: i64,
    pub total_bytes: i64,
    pub started_at: Instant,
    pub error: Option<String>,
    /// If set, cancelling this token aborts the download.
    pub cancel_token: Option<CancellationToken>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferKind {
    Download,
    Upload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferStatus {
    Pending,
    InProgress,
    Done,
    Failed,
}
