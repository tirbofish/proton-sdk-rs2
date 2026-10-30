#![allow(dead_code)] // for now
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TransferEntry {
    pub id: usize,
    pub filename: String,
    pub direction: TransferDirection,
    pub bytes_transferred: i64,
    pub total_bytes: i64,
    pub cancellable: bool,
    /// Average speed since the first byte moved; 0 until there is a second's worth.
    #[serde(default)]
    pub bytes_per_second: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransferDirection {
    Upload,
    Download,
}

impl TransferEntry {
    pub fn progress_fraction(&self) -> f32 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.bytes_transferred as f32 / self.total_bytes as f32
        }
    }
}

/// ```rust norun
/// let idx = tracker.add("document.pdf".into(), TransferDirection::Download, total_size);
/// let on_progress = tracker.progress_callback(idx);
/// // Pass on_progress to file_downloader.download(..., on_progress) or uploader.upload_from_stream(..., on_progress)
/// ```
#[derive(Debug, Clone, Default)]
pub struct TransferTracker {
    inner: Arc<
        Mutex<(
            usize,
            HashMap<usize, TransferEntry>,
            HashMap<usize, tokio::task::AbortHandle>,
            // When each transfer first moved, and how far it was then.
            HashMap<usize, (std::time::Instant, i64)>,
        )>,
    >,
}

impl TransferTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, filename: String, direction: TransferDirection, total_bytes: i64) -> usize {
        let mut state = self.inner.lock().unwrap();
        let idx = state.0;
        state.0 += 1;
        state.1.insert(
            idx,
            TransferEntry {
                id: idx,
                filename,
                direction,
                bytes_transferred: 0,
                total_bytes,
                cancellable: false,
                bytes_per_second: 0,
            },
        );
        idx
    }

    pub fn progress_callback(&self, index: usize) -> Box<dyn Fn(i64, i64) + Send + Sync> {
        let inner = self.inner.clone();
        Box::new(move |transferred, total| {
            if let Ok(mut entries) = inner.lock() {
                if let Some(entry) = entries.1.get_mut(&index) {
                    entry.bytes_transferred = transferred;
                    entry.total_bytes = total;
                    if transferred > 0 {
                        entries
                            .3
                            .entry(index)
                            .or_insert((std::time::Instant::now(), transferred));
                    }
                }
            }
        })
    }

    pub fn snapshot(&self) -> Vec<TransferEntry> {
        let state = self.inner.lock().unwrap();
        let mut entries: Vec<_> = state.1.values().cloned().collect();
        for entry in &mut entries {
            if let Some((started, start_bytes)) = state.3.get(&entry.id) {
                let elapsed = started.elapsed().as_secs_f64();
                if elapsed >= 1.0 {
                    entry.bytes_per_second =
                        ((entry.bytes_transferred - start_bytes).max(0) as f64 / elapsed) as i64;
                }
            }
        }
        entries.sort_by_key(|entry| entry.id);
        entries
    }

    pub fn register_cancel(&self, index: usize, handle: tokio::task::AbortHandle) {
        let mut state = self.inner.lock().unwrap();
        if let Some(entry) = state.1.get_mut(&index) {
            entry.cancellable = true;
            state.2.insert(index, handle);
        }
    }

    pub fn cancel(&self, index: usize) -> anyhow::Result<()> {
        let state = self.inner.lock().unwrap();
        let handle = state.2.get(&index).ok_or_else(|| {
            anyhow::anyhow!("transfer {index} is not cancellable or is no longer active")
        })?;
        handle.abort();
        Ok(())
    }

    pub fn mark_complete(&self, index: usize) {
        if let Ok(mut entries) = self.inner.lock() {
            entries.1.remove(&index);
            entries.2.remove(&index);
            entries.3.remove(&index);
        }
    }

    pub fn mark_failed(&self, index: usize) {
        if let Ok(mut entries) = self.inner.lock() {
            entries.1.remove(&index);
            entries.2.remove(&index);
            entries.3.remove(&index);
        }
    }

    pub fn remove_completed(&self) {
        self.inner
            .lock()
            .unwrap()
            .1
            .retain(|_, e| e.bytes_transferred < e.total_bytes);
    }
}

pub fn format_bytes(bytes: i64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let b = bytes as f64;
    if b >= GB {
        format!("{:.1} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.1} KB", b / KB)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completed_transfer_does_not_shift_active_progress() {
        let tracker = TransferTracker::new();
        let first = tracker.add("first".into(), TransferDirection::Download, 10);
        let second = tracker.add("second".into(), TransferDirection::Upload, 20);
        let progress = tracker.progress_callback(second);
        tracker.mark_complete(first);
        progress(8, 20);
        let entries = tracker.snapshot();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].filename, "second");
        assert_eq!(entries[0].bytes_transferred, 8);
    }

    #[tokio::test]
    async fn only_registered_downloads_can_be_cancelled() {
        let tracker = TransferTracker::new();
        let upload = tracker.add("upload".into(), TransferDirection::Upload, 10);
        assert!(tracker.cancel(upload).is_err());
        let download = tracker.add("download".into(), TransferDirection::Download, 10);
        let task = tokio::spawn(std::future::pending::<()>());
        tracker.register_cancel(download, task.abort_handle());
        assert!(
            tracker
                .snapshot()
                .iter()
                .any(|entry| entry.id == download && entry.cancellable)
        );
        tracker.cancel(download).unwrap();
        assert!(task.await.unwrap_err().is_cancelled());
        tracker.mark_failed(download);
        assert!(tracker.cancel(download).is_err());
    }
}
