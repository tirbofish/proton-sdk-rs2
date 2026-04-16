pub mod store;
pub mod requests;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::volume::VolumeId;
use proton_drive_sdk::api::events::{VolumeEventDto, VolumeEventType};
use proton_drive_sdk::utils::PotentialObject;
use tokio::sync::{Notify, RwLock};
use tokio_util::sync::CancellationToken;

use self::requests::{PendingRequest, RequestKind, RequestStatus};
use self::store::InodeStore;
use crate::transfers::{TransferLog, TransferEntry, TransferKind, TransferStatus};

/// The central index that mediates between the Proton Drive API and the FUSE
/// filesystem. FUSE reads exclusively from this index. Online operations write
/// into it. Pending write operations (create, delete, rename) are submitted as
/// requests which a background worker picks up.
///
/// When connectivity is lost, requests pile up in `Pending` state. When the
/// worker or event poller detects that the network is back, it calls
/// `set_online(true)` which wakes the worker to drain the queue.
pub struct DriveIndex {
    pub store: Arc<RwLock<InodeStore>>,
    requests: Arc<RwLock<Vec<PendingRequest>>>,
    drive_client: ProtonDriveClient,
    volume_id: RwLock<Option<VolumeId>>,
    /// `true` when we believe the API is reachable.
    online: AtomicBool,
    /// Notified whenever new requests are submitted or connectivity is restored,
    /// so the worker can wake up immediately instead of polling on a timer.
    pub work_available: Arc<Notify>,
    /// On-disk cache directory for downloaded file content.
    cache_dir: PathBuf,
    /// Shared transfer log for UI visibility.
    pub transfer_log: TransferLog,
    /// Notified when bootstrap completes (volume_id is set).
    pub bootstrap_done: Arc<Notify>,
}

impl DriveIndex {
    /// Creates a new `DriveIndex` without requiring a volume ID upfront.
    /// Call `bootstrap_from_server()` to populate root after FUSE is mounted.
    pub fn new_deferred(drive_client: ProtonDriveClient, cache_dir: PathBuf) -> Self {
        // Ensure cache dir exists
        let _ = std::fs::create_dir_all(&cache_dir);
        Self {
            store: Arc::new(RwLock::new(InodeStore::new())),
            requests: Arc::new(RwLock::new(Vec::new())),
            drive_client,
            volume_id: RwLock::new(None),
            online: AtomicBool::new(true),
            work_available: Arc::new(Notify::new()),
            cache_dir,
            transfer_log: TransferLog::new(),
            bootstrap_done: Arc::new(Notify::new()),
        }
    }

    /// Creates a `DriveIndex` with a known volume ID (legacy path).
    pub fn new(drive_client: ProtonDriveClient, volume_id: VolumeId) -> Self {
        let cache_dir = dirs::cache_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("pdcli")
            .join("file_cache");
        let _ = std::fs::create_dir_all(&cache_dir);
        Self {
            store: Arc::new(RwLock::new(InodeStore::new())),
            requests: Arc::new(RwLock::new(Vec::new())),
            drive_client,
            volume_id: RwLock::new(Some(volume_id)),
            online: AtomicBool::new(true),
            work_available: Arc::new(Notify::new()),
            cache_dir,
            transfer_log: TransferLog::new(),
            bootstrap_done: Arc::new(Notify::new()),
        }
    }

    /// Bootstrap from the server: fetches My Files, sets volume_id, and populates root.
    /// Safe to call after FUSE is already mounted.
    pub async fn bootstrap_from_server(&self) -> anyhow::Result<()> {
        let folder = self.drive_client.get_my_files_folder().await?;
        let volume_id = folder.base.uid.volume_id.clone();

        // Set the volume ID
        {
            let mut vid = self.volume_id.write().await;
            *vid = Some(volume_id);
        }

        // Populate the root
        {
            let mut store = self.store.write().await;
            store.ensure_root();
            let uid = folder.base.uid.clone();
            let ino = store.insert_or_update_node(Node::Folder(folder), Some("My Files".to_string()));
            store.set_parent(ino, store::ROOT_INO);
            tracing::info!(ino, %uid, "bootstrapped My Files folder");
        }

        self.bootstrap_done.notify_waiters();
        Ok(())
    }

    /// Populates the root inode (1) with a virtual root, and inode 2 with the
    /// "My Files" folder from the server. If the server is unreachable, uses
    /// whatever is in the store already.
    pub async fn bootstrap(&self) -> anyhow::Result<()> {
        let mut store = self.store.write().await;

        // Inode 1 = virtual FUSE root (contains "My Files")
        store.ensure_root();

        // Try to fetch My Files from server
        match self.drive_client.get_my_files_folder().await {
            Ok(folder) => {
                let uid = folder.base.uid.clone();
                let name = "My Files".to_string();
                let ino = store.insert_or_update_node(
                    Node::Folder(folder),
                    Some(name),
                );
                // Register as child of root
                store.set_parent(ino, store::ROOT_INO);
                tracing::info!(ino, %uid, "bootstrapped My Files folder");
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not fetch My Files from server, using cached data");
            }
        }
        Ok(())
    }

    /// Returns the `ProtonDriveClient` for use by the worker / event loop.
    pub fn drive_client(&self) -> &ProtonDriveClient {
        &self.drive_client
    }

    pub async fn volume_id(&self) -> Option<VolumeId> {
        self.volume_id.read().await.clone()
    }

    // ── Connectivity ────────────────────────────────────────────────────

    /// Returns `true` if we believe the API is reachable.
    pub fn is_online(&self) -> bool {
        self.online.load(Ordering::Relaxed)
    }

    /// Update connectivity state. When transitioning from offline → online,
    /// all `AwaitingRetry` requests are moved back to `Pending` and the worker
    /// is woken up to drain the queue.
    pub async fn set_online(&self, online: bool) {
        let was_online = self.online.swap(online, Ordering::Relaxed);
        if online && !was_online {
            tracing::info!("connectivity restored — requeueing retryable requests");
            let mut reqs = self.requests.write().await;
            for req in reqs.iter_mut() {
                if matches!(req.status, RequestStatus::AwaitingRetry(_)) {
                    req.status = RequestStatus::Pending;
                }
            }
            self.work_available.notify_one();
        }
    }

    // ── Read operations (used by FUSE) ──────────────────────────────────

    /// Look up a child by name under `parent_ino`. Returns the child inode.
    pub async fn lookup(&self, parent_ino: u64, name: &str) -> Option<u64> {
        let store = self.store.read().await;
        store.lookup_child(parent_ino, name)
    }

    /// Get the cached node for `ino`.
    pub async fn get_node(&self, ino: u64) -> Option<store::IndexEntry> {
        let store = self.store.read().await;
        store.get(ino).cloned()
    }

    /// List children of `ino`. If children have not been fetched yet and we can
    /// reach the server, fetch and cache them first.
    pub async fn children(&self, ino: u64) -> Vec<(u64, store::IndexEntry)> {
        // Check if we already have children cached
        {
            let store = self.store.read().await;
            if store.has_children(ino) {
                return store.list_children(ino);
            }
        }

        // Try to fetch children from the server
        let node_uid = {
            let store = self.store.read().await;
            store.get(ino).and_then(|e| e.node_uid.clone())
        };

        if let Some(uid) = node_uid {
            let volume_id = self.volume_id().await;
            if let Some(vid) = volume_id {
                if let Ok(items) = self.drive_client.list_children(
                    vid,
                    Some(uid.link_id.clone()),
                ).await {
                    let mut store = self.store.write().await;
                    for item in items {
                        match item {
                            PotentialObject::Node(node) => {
                                let child_ino = store.insert_or_update_node(node, None);
                                store.set_parent(child_ino, ino);
                            }
                            PotentialObject::Degraded(degraded) => {
                                tracing::warn!(uid = %degraded.uid(), "skipping degraded node");
                            }
                        }
                    }
                    store.mark_children_fetched(ino);
                    return store.list_children(ino);
                }
            }
        }

        // Fallback: return whatever we have cached
        let store = self.store.read().await;
        store.list_children(ino)
    }

    /// Read file content for `ino`. Checks the on-disk cache first, downloads
    /// from the API on cache miss, and stores to disk. Supports cancellation.
    pub async fn read_file(&self, ino: u64, offset: u64, size: u32) -> Option<Vec<u8>> {
        let cache_path = self.cache_dir.join(format!("{ino}"));

        // Check on-disk cache first
        if cache_path.exists() {
            if let Ok(data) = tokio::fs::read(&cache_path).await {
                return Some(Self::slice_data(&data, offset, size));
            }
        }

        // Need to download — get the FileNode from the store
        let file_node = {
            let store = self.store.read().await;
            let entry = store.get(ino)?;
            match entry.node.as_ref()? {
                Node::File(f) | Node::Photo(f) => Some(f.clone()),
                _ => None,
            }
        }?;

        let name = {
            let store = self.store.read().await;
            store.get(ino).map(|e| e.name.clone()).unwrap_or_default()
        };

        // Create a cancellation token for this download
        let cancel_token = CancellationToken::new();

        // Register the download in the transfer log (with cancel token)
        let log_idx = self.transfer_log.add(TransferEntry {
            name: name.clone(),
            kind: TransferKind::Download,
            status: TransferStatus::InProgress,
            progress: Some(0.0),
            bytes_transferred: 0,
            total_bytes: file_node.total_size_on_cloud_storage,
            started_at: std::time::Instant::now(),
            error: None,
            cancel_token: Some(cancel_token.clone()),
        });

        let revision_uid = file_node.active_revision.uid.clone();

        // Download into an in-memory buffer with progress + cancellation
        let transfer_log = self.transfer_log.clone();
        let cancel = cancel_token.clone();
        let result: anyhow::Result<Vec<u8>> = async {
            let downloader = self.drive_client.get_file_downloader(revision_uid).await?;
            let buffer = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let writer = buffer.clone();

            let tl = transfer_log.clone();
            let on_progress: Box<dyn Fn(i64, i64) + Send + Sync> = Box::new(move |downloaded, total| {
                tl.set_progress(log_idx, downloaded, total);
            });

            let controller = downloader.download_to_stream(
                Box::new(MutexWriter(writer)),
                on_progress,
            );

            // Race the download against cancellation
            tokio::select! {
                res = controller.completion => {
                    res??;
                }
                _ = cancel.cancelled() => {
                    anyhow::bail!("download cancelled");
                }
            }

            let data = std::sync::Arc::try_unwrap(buffer)
                .unwrap_or_else(|arc| arc.lock().unwrap().clone().into())
                .into_inner()
                .unwrap();
            Ok(data)
        }.await;

        match result {
            Ok(data) => {
                self.transfer_log.set_done(log_idx);
                tracing::info!(ino, bytes = data.len(), "file downloaded and cached");
                let slice = Self::slice_data(&data, offset, size);
                // Write to disk cache
                if let Err(e) = tokio::fs::write(&cache_path, &data).await {
                    tracing::warn!(ino, error = %e, "failed to write file to disk cache");
                }
                Some(slice)
            }
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("cancelled") {
                    self.transfer_log.set_failed(log_idx, "Cancelled".to_string());
                    tracing::info!(ino, "download cancelled by user");
                } else {
                    self.transfer_log.set_failed(log_idx, msg);
                    tracing::error!(ino, error = %e, "file download failed");
                }
                None
            }
        }
    }

    fn slice_data(data: &[u8], offset: u64, size: u32) -> Vec<u8> {
        let start = (offset as usize).min(data.len());
        let end = (start + size as usize).min(data.len());
        data[start..end].to_vec()
    }

    // ── Write operations (queued as requests) ───────────────────────────

    /// Submit a pending request. Returns a request ID.
    /// The worker is notified immediately so it can pick it up.
    pub async fn submit_request(&self, kind: RequestKind) -> u64 {
        let mut reqs = self.requests.write().await;
        let id = reqs.len() as u64 + 1;
        reqs.push(PendingRequest {
            id,
            kind,
            status: RequestStatus::Pending,
            attempts: 0,
        });
        drop(reqs);
        self.work_available.notify_one();
        id
    }

    /// Take the next pending request for the worker to process.
    pub async fn take_pending_request(&self) -> Option<PendingRequest> {
        let mut reqs = self.requests.write().await;
        for req in reqs.iter_mut() {
            if req.status == RequestStatus::Pending {
                req.status = RequestStatus::InProgress;
                return Some(req.clone());
            }
        }
        None
    }

    /// Mark a request as completed and remove it.
    pub async fn complete_request(&self, id: u64) {
        let mut reqs = self.requests.write().await;
        reqs.retain(|r| r.id != id);
    }

    /// Mark a request as transiently failed. It stays in the queue and will be
    /// retried when connectivity is restored (via `set_online(true)`).
    pub async fn retry_later(&self, id: u64, error: String) {
        let mut reqs = self.requests.write().await;
        if let Some(req) = reqs.iter_mut().find(|r| r.id == id) {
            req.attempts += 1;
            req.status = RequestStatus::AwaitingRetry(error);
        }
    }

    /// Mark a request as permanently failed (will not be retried).
    pub async fn fail_request(&self, id: u64, error: String) {
        let mut reqs = self.requests.write().await;
        if let Some(req) = reqs.iter_mut().find(|r| r.id == id) {
            req.status = RequestStatus::Failed(error);
        }
    }

    /// Returns the number of requests currently waiting (Pending + AwaitingRetry).
    pub async fn pending_request_count(&self) -> usize {
        let reqs = self.requests.read().await;
        reqs.iter().filter(|r| {
            matches!(r.status, RequestStatus::Pending | RequestStatus::AwaitingRetry(_))
        }).count()
    }

    // ── Event processing ────────────────────────────────────────────────

    /// Apply a batch of volume events to the index.
    pub async fn apply_events(&self, events: &[VolumeEventDto]) {
        let volume_id = match self.volume_id().await {
            Some(vid) => vid,
            None => return,
        };
        let mut store = self.store.write().await;
        for event in events {
            let link_id = event.link.link_id.clone();
            let uid = NodeUid::new(volume_id.clone(), link_id.clone());

            match event.event_type() {
                Some(VolumeEventType::Delete) => {
                    if let Some(ino) = store.find_ino_by_uid(&uid) {
                        tracing::info!(%uid, ino, "event: removing node");
                        store.remove(ino);
                    }
                }
                Some(VolumeEventType::Create) => {
                    if let Some(parent_link_id) = &event.link.parent_link_id {
                        let parent_uid = NodeUid::new(
                            volume_id.clone(),
                            parent_link_id.clone(),
                        );
                        if let Some(parent_ino) = store.find_ino_by_uid(&parent_uid) {
                            store.invalidate_children(parent_ino);
                            tracing::info!(%parent_uid, parent_ino, "event: invalidated parent children for new node");
                        }
                    }
                }
                Some(VolumeEventType::UpdateMetadata) | Some(VolumeEventType::UpdateContent) => {
                    if let Some(ino) = store.find_ino_by_uid(&uid) {
                        store.invalidate_children(ino);
                        tracing::info!(%uid, ino, "event: invalidated node for update");
                    }
                }
                None => {
                    tracing::warn!(event_type = event.event_type, "unknown volume event type");
                }
            }
        }
    }
}

/// A `Write` adapter that writes into a `Mutex<Vec<u8>>`.
struct MutexWriter(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for MutexWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
