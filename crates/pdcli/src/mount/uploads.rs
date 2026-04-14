use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};

use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::node::NodeUid;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) struct PendingFile {
    #[allow(dead_code)]
    pub(super) parent_inode: u64,
    pub(super) parent_uid: NodeUid,
    pub(super) name: String,
    pub(super) mime_type: String,
    pub(super) content: Vec<u8>,
    #[allow(dead_code)]
    pub(super) creation_time: DateTime<Utc>,
    pub(super) dirty: bool,
    #[serde(default)]
    pub(super) local_only: bool,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub(super) enum PersistentUpload {
    NewFile {
        id: String,
        parent_uid: NodeUid,
        name: String,
        mime_type: String,
        content: Vec<u8>,
        #[serde(default)]
        retry_count: u32,
        #[serde(default = "default_timestamp")]
        created_at: i64,
    },
    NewRevision {
        id: String,
        revision_uid: RevisionUid,
        filename: String,
        content: Vec<u8>,
        #[serde(default)]
        retry_count: u32,
        #[serde(default = "default_timestamp")]
        created_at: i64,
    },
}

fn default_timestamp() -> i64 {
    chrono::Utc::now().timestamp()
}

impl PersistentUpload {
    pub(super) fn id(&self) -> &str {
        match self {
            PersistentUpload::NewFile { id, .. } => id,
            PersistentUpload::NewRevision { id, .. } => id,
        }
    }

    pub(super) fn name(&self) -> &str {
        match self {
            PersistentUpload::NewFile { name, .. } => name,
            PersistentUpload::NewRevision { filename, .. } => filename,
        }
    }

    pub(super) fn retry_count(&self) -> u32 {
        match self {
            PersistentUpload::NewFile { retry_count, .. } => *retry_count,
            PersistentUpload::NewRevision { retry_count, .. } => *retry_count,
        }
    }

    pub(super) fn increment_retry(&mut self) {
        match self {
            PersistentUpload::NewFile { retry_count, .. } => *retry_count += 1,
            PersistentUpload::NewRevision { retry_count, .. } => *retry_count += 1,
        }
    }

    fn created_at(&self) -> i64 {
        match self {
            PersistentUpload::NewFile { created_at, .. } => *created_at,
            PersistentUpload::NewRevision { created_at, .. } => *created_at,
        }
    }

    pub(super) fn is_stale(&self) -> bool {
        let now = chrono::Utc::now().timestamp();
        let age_hours = (now - self.created_at()) / 3600;
        age_hours > 24
    }
}

pub(super) struct PendingUploadStore {
    store_dir: PathBuf,
}

impl PendingUploadStore {
    pub(super) fn new() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("No config directory"))?
            .join("pdcli")
            .join("pending_uploads");

        std::fs::create_dir_all(&config_dir)
            .context("Failed to create pending uploads directory")?;

        Ok(Self {
            store_dir: config_dir,
        })
    }

    pub(super) fn generate_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{:x}", timestamp)
    }

    pub(super) fn save(&self, upload: &PersistentUpload) -> Result<()> {
        let path = self.store_dir.join(format!("{}.json", upload.id()));
        let data = serde_json::to_vec(upload).context("Failed to serialize upload")?;
        std::fs::write(&path, data)
            .with_context(|| format!("Failed to write upload file: {:?}", path))?;
        tracing::debug!("Saved pending upload: {}", upload.id());
        Ok(())
    }

    pub(super) fn remove(&self, id: &str) -> Result<()> {
        let path = self.store_dir.join(format!("{}.json", id));
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("Failed to remove upload file: {:?}", path))?;
            tracing::debug!("Removed completed upload: {}", id);
        }
        Ok(())
    }

    pub(super) fn load_all(&self) -> Result<Vec<PersistentUpload>> {
        let mut uploads = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&self.store_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    match std::fs::read(&path) {
                        Ok(data) => match serde_json::from_slice::<PersistentUpload>(&data) {
                            Ok(upload) => {
                                tracing::info!(
                                    "Found pending upload: {} ({:?})",
                                    upload.id(),
                                    match &upload {
                                        PersistentUpload::NewFile { name, .. } => name.clone(),
                                        PersistentUpload::NewRevision { filename, .. } => {
                                            filename.clone()
                                        }
                                    }
                                );
                                uploads.push(upload);
                            }
                            Err(e) => {
                                tracing::warn!("Failed to parse upload file {:?}: {}", path, e);
                            }
                        },
                        Err(e) => {
                            tracing::warn!("Failed to read upload file {:?}: {}", path, e);
                        }
                    }
                }
            }
        }

        Ok(uploads)
    }
}

#[derive(Debug)]
pub(super) struct WriteBuffer {
    pub(super) inode: u64,
    pub(super) is_new: bool,
    pub(super) offset: u64,
    pub(super) content: Vec<u8>,
    pub(super) dirty: bool,
}

/// Signal to the debounce processor that an inode needs uploading.
/// The processor will wait for the debounce period to expire before
/// actually triggering the upload.
#[derive(Debug, Clone)]
pub(super) struct DebounceTrigger {
    pub(super) inode: u64,
}

#[derive(Debug)]
pub(super) enum UploadTask {
    NewFile {
        inode: u64,
        pending: PendingFile,
        persist_id: Option<String>,
    },
    NewRevision {
        inode: u64,
        revision_uid: RevisionUid,
        filename: String,
        content: Vec<u8>,
        persist_id: Option<String>,
        /// Cache generation at the time this upload was queued.
        /// Used to detect if newer saves occurred during upload.
        generation: u64,
    },
    ResumePersisted(PersistentUpload),
}
