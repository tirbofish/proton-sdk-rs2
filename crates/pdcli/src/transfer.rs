#![allow(dead_code)] // for now
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct TransferEntry {
    pub filename: String,
    pub direction: TransferDirection,
    pub bytes_transferred: i64,
    pub total_bytes: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    inner: Arc<Mutex<Vec<TransferEntry>>>,
}

impl TransferTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add(&self, filename: String, direction: TransferDirection, total_bytes: i64) -> usize {
        let mut entries = self.inner.lock().unwrap();
        let idx = entries.len();
        entries.push(TransferEntry {
            filename,
            direction,
            bytes_transferred: 0,
            total_bytes,
        });
        idx
    }

    pub fn progress_callback(&self, index: usize) -> Box<dyn Fn(i64, i64) + Send + Sync> {
        let inner = self.inner.clone();
        Box::new(move |transferred, total| {
            if let Ok(mut entries) = inner.lock() {
                if let Some(entry) = entries.get_mut(index) {
                    entry.bytes_transferred = transferred;
                    entry.total_bytes = total;
                }
            }
        })
    }

    pub fn snapshot(&self) -> Vec<TransferEntry> {
        self.inner.lock().unwrap().clone()
    }

    pub fn mark_complete(&self, index: usize) {
        if let Ok(mut entries) = self.inner.lock() {
            if let Some(entry) = entries.get_mut(index) {
                entry.bytes_transferred = entry.total_bytes;
            }
        }
    }

    pub fn mark_failed(&self, index: usize) {
        if let Ok(mut entries) = self.inner.lock() {
            // Remove failed entries so they don't linger.
            if index < entries.len() {
                entries.remove(index);
            }
        }
    }

    pub fn remove_completed(&self) {
        self.inner
            .lock()
            .unwrap()
            .retain(|e| e.bytes_transferred < e.total_bytes);
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
