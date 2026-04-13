//! FUSE filesystem implementation for Proton Drive.
//!
//! This module provides a virtual filesystem that mounts Proton Drive
//! as a local directory using FUSE (Filesystem in Userspace).
//!
//! It integrates with the proton-drive-sdk to fetch and display all
//! available node metadata including:
//! - File/folder names (decrypted)
//! - File sizes (claimed plaintext size)
//! - Modification times
//! - Creation times
//! - MIME types
//! - Author information
//! - Revision metadata

use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::Utc;
use console::style;
use fuse3::raw::prelude::*;
use fuse3::raw::reply::{ReplyCopyFileRange, ReplyCreated, ReplyWrite, ReplyXAttr};
use fuse3::{Errno, MountOptions, SetAttr};
use futures_util::stream;
use futures_util::stream::Stream;
use futures_util::StreamExt;
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

use proton_drive_sdk::api::events::{VolumeEventDto, VolumeEventType};
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::node::{Node, DegradedNode, NodeUid};
use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::revision::RevisionId;
use proton_drive_sdk::utils::PotentialObject;
use proton_drive_sdk::volume::VolumeId;
use proton_sdk_rs2::session::ProtonAPISession;

use crate::index::{IndexedNode, IndexEvent, NodeType, OfflineIndex, OfflineStatus, PendingMutation};
mod cache;
mod errors;
mod ignore;
mod maintenance;
mod models;
mod state;
mod thumbnail;
mod uploads;

use self::cache::{DiskCache, MAX_DISK_CACHE_SIZE};
use self::errors::{
    classify_download_error, is_permanent_upload_error, is_transient_error, is_stale_revision_error, MAX_UPLOAD_RETRIES,
};
use self::ignore::{IgnoreManager, PDCLIGNORE_FILENAME};
use self::maintenance::{add_gtk_bookmark, is_thumbnailer_process, remove_gtk_bookmark};
use self::models::{FsNode, ProtonFileMetadata, ProtonFolderMetadata};
use self::state::ProtonDriveFsInner;
use self::thumbnail::{
    get_thumbnail_config, has_cached_thumbnail, is_lock_file, plant_freedesktop_thumbnail,
};
use self::uploads::{PendingFile, PendingUploadStore, PersistentUpload, UploadTask, WriteBuffer};

pub use self::maintenance::{clear_cache, clear_pending_uploads};

/// Time-to-live for cached attributes.
const TTL: Duration = Duration::from_secs(1);

/// Root inode number (always 1 in FUSE).
const ROOT_INODE: u64 = 1;

/// Virtual folder inodes for the root-level structure.
/// These are synthetic folders that group different Proton Drive features.
const MYFILES_INODE: u64 = 2;

/// First inode available for actual Proton Drive nodes.
const FIRST_DYNAMIC_INODE: u64 = 100;

/// Default directory mode.
const DIR_MODE: u16 = 0o755;

/// Default file mode.
const FILE_MODE: u16 = 0o644;

/// Event polling interval (5 seconds for responsive updates).
const EVENT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// The Proton Drive FUSE filesystem.
pub struct ProtonDriveFs {
    inner: Arc<RwLock<ProtonDriveFsInner>>,
    client: Arc<RwLock<Option<ProtonDriveClient>>>,
    disk_cache: DiskCache,
    /// Offline index for metadata and offline file tracking
    index: Arc<OfflineIndex>,
    /// Whether the client appears to be online (updated on each request)
    is_online: Arc<std::sync::atomic::AtomicBool>,
    /// Multi-progress bar for concurrent downloads
    multi_progress: Arc<MultiProgress>,
    /// Ignore pattern manager
    ignore_manager: RwLock<IgnoreManager>,
    /// Background upload queue sender
    upload_tx: mpsc::UnboundedSender<UploadTask>,
    /// Upload queue receiver (moved to background task on init)
    upload_rx: RwLock<Option<mpsc::UnboundedReceiver<UploadTask>>>,
    /// Persistent upload store for resume capability
    pending_upload_store: Arc<PendingUploadStore>,
    /// Mount path for constructing file URIs (used for thumbnail caching)
    mount_path: PathBuf,
}

impl ProtonDriveFs {
    /// Create a new filesystem instance.
    pub fn new(multi_progress: Arc<MultiProgress>, mount_path: &Path) -> Result<Self> {
        // Initialize thumbnail config early (creates default config if needed)
        let config = get_thumbnail_config();
        tracing::info!("Loaded thumbnail config with {} allowed exes, {} blocked exes",
            config.allowed_exes.len(), config.blocked_exes.len());
        
        let ignore_manager = IgnoreManager::new()?;
        tracing::info!("Loaded global .pdclignore from {:?}", ignore_manager.global_ignore_path());
        
        let (upload_tx, upload_rx) = mpsc::unbounded_channel();
        let pending_upload_store = Arc::new(PendingUploadStore::new()?);
        
        // Open offline index
        let index_path = dirs::config_dir()
            .context("Could not determine config directory")?
            .join("pdcli")
            .join("index.db");
        let index = Arc::new(OfflineIndex::open(&index_path)?);
        tracing::info!("Opened offline index at {:?}", index_path);
        
        Ok(Self {
            inner: Arc::new(RwLock::new(ProtonDriveFsInner::new())),
            client: Arc::new(RwLock::new(None)),
            disk_cache: DiskCache::new(MAX_DISK_CACHE_SIZE)?,
            index,
            is_online: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            multi_progress,
            ignore_manager: RwLock::new(ignore_manager),
            upload_tx,
            upload_rx: RwLock::new(Some(upload_rx)),
            pending_upload_store,
            mount_path: mount_path.to_path_buf(),
        })
    }

    /// Load cached root folder info from the index.
    /// Returns (volume_id, link_id) if cached, None otherwise.
    fn load_cached_root_info(&self) -> Option<(String, String)> {
        let volume_id = self.index.get_sync_state("root_volume_id").ok()??;
        let link_id = self.index.get_sync_state("root_link_id").ok()??;
        
        if volume_id.is_empty() || link_id.is_empty() {
            return None;
        }
        
        Some((volume_id, link_id))
    }

    /// Initialize with a ProtonDriveClient.
    pub async fn init_with_client(&self, client: ProtonDriveClient) -> Result<()> {
        // Try to load cached root folder info from index first (instant)
        let cached_root = self.load_cached_root_info();
        
        let (volume_id, root_uid) = if let Some((vol_id, link_id)) = cached_root {
            tracing::info!("Using cached root folder info (instant mount)");
            
            // We have cached root info - construct the UID
            let volume_id = VolumeId::new(vol_id.clone());
            let root_uid = NodeUid::new(volume_id.clone(), LinkId::new(link_id.clone()));
            
            // Verify it still exists in the background so mount is instant
            let client_clone = client.clone();
            let root_uid_clone = root_uid.clone();
            let index = self.index.clone();
            tokio::spawn(async move {
                match client_clone.get_node(root_uid_clone).await {
                    Ok(_) => tracing::debug!("Root folder verified"),
                    Err(e) => {
                        tracing::warn!("Cached root folder invalid, clearing: {}", e);
                        let _ = index.set_sync_state("root_volume_id", "");
                        let _ = index.set_sync_state("root_link_id", "");
                    }
                }
            });
            
            (volume_id, root_uid)
        } else {
            // No cached info - fetch from network
            tracing::info!("Fetching root folder from network (first mount)");
            let folder = client.get_my_files_folder().await
                .context("Failed to get My Files folder")?;
            
            // Cache for next time
            let _ = self.index.set_sync_state("root_volume_id", folder.base.uid.volume_id.raw());
            let _ = self.index.set_sync_state("root_link_id", folder.base.uid.link_id.raw());
            
            (folder.base.uid.volume_id.clone(), folder.base.uid.clone())
        };

        let mut inner = self.inner.write().await;
        
        // Store volume ID and root UID (pointing to My Files)
        inner.volume_id = Some(volume_id.clone());
        inner.root_uid = Some(root_uid.clone());
        
        // Create virtual root folder (inode 1)
        // This is a synthetic container that holds MyFiles, Computers, Photos
        let now = Utc::now();
        let dummy_volume = VolumeId::new("_virtual_".to_string());
        
        let virtual_root = FsNode::Folder(ProtonFolderMetadata {
            uid: NodeUid::new(dummy_volume.clone(), LinkId::new("_root_".to_string())),
            parent_uid: None,
            name: String::new(), // Root has no name
            creation_time: now,
            trash_time: None,
            author_email: None,
            name_author_email: None,
            owner_email: None,
            owner_organisation: None,
            is_album: false,
        });
        inner.nodes.insert(ROOT_INODE, virtual_root);
        inner.children.insert(ROOT_INODE, vec![MYFILES_INODE]);
        inner.loaded_folders.insert(ROOT_INODE); // Virtual root is always "loaded"
        
        // Create "MyFiles" virtual folder (inode 2) - backed by actual Proton folder
        let my_files_meta = ProtonFolderMetadata {
            uid: root_uid.clone(),
            parent_uid: None,
            name: "MyFiles".to_string(),
            creation_time: now,
            trash_time: None,
            author_email: None,
            name_author_email: None,
            owner_email: None,
            owner_organisation: None,
            is_album: false,
        };
        let my_files_node = FsNode::Folder(my_files_meta);
        inner.uid_to_inode.insert(root_uid.to_string(), MYFILES_INODE);
        // Also add to link_id_to_inode so event processing can find it
        inner.link_id_to_inode.insert(root_uid.link_id.raw().to_string(), MYFILES_INODE);
        inner.nodes.insert(MYFILES_INODE, my_files_node);
        inner.children.insert(MYFILES_INODE, Vec::new());
        
        drop(inner);
        
        // Store client
        *self.client.write().await = Some(client);
        
        // Spawn background upload processor
        let upload_rx = self.upload_rx.write().await.take();
        if let Some(mut rx) = upload_rx {
            let client = self.client.clone();
            let inner = self.inner.clone();
            let multi_progress = self.multi_progress.clone();
            let pending_upload_store = self.pending_upload_store.clone();
            let index = self.index.clone();
            
            tokio::spawn(async move {
                // Allow up to 3 concurrent file uploads
                const MAX_CONCURRENT_FILE_UPLOADS: usize = 3;
                let upload_semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_FILE_UPLOADS));
                
                while let Some(task) = rx.recv().await {
                    // Clone what we need for the spawned task
                    let client = client.clone();
                    let inner = inner.clone();
                    let multi_progress = multi_progress.clone();
                    let semaphore = upload_semaphore.clone();
                    let pending_upload_store = pending_upload_store.clone();
                    let index = index.clone();
                    
                    // Spawn each upload as a separate task so they run concurrently
                    tokio::spawn(async move {
                        // Acquire semaphore permit to limit concurrent uploads
                        let _permit = semaphore.acquire().await.unwrap();
                        
                        let client_guard = client.read().await;
                        let Some(client) = client_guard.as_ref() else {
                            tracing::error!("No client for background upload");
                            return;
                        };
                    
                    match task {
                        UploadTask::NewFile { inode, pending, persist_id } => {
                            tracing::info!("Background uploading new file '{}' ({} bytes)", pending.name, pending.content.len());
                            
                            let pb = multi_progress.add(ProgressBar::new(pending.content.len() as u64));
                            pb.set_style(
                                ProgressStyle::default_bar()
                                    .template("{spinner:.white.on_blue} {msg} [{bar:30.white.on_blue}] {bytes}/{total_bytes}")
                                    .unwrap()
                                    .progress_chars("█▓░")
                            );
                            pb.set_message(format!("↑ {}", pending.name));
                            pb.enable_steady_tick(Duration::from_millis(100));
                            
                            let size = pending.content.len() as i64;
                            
                            // Helper to handle upload errors - shows appropriate message and removes on permanent error
                            let handle_newfile_error = |persist_id: &Option<String>, name: &str, error: &anyhow::Error, pb: &ProgressBar| {
                                let error_msg = error.to_string();
                                // Extract a short error description for display
                                let short_error = if let Some(api_err) = error_msg.strip_prefix("API error ") {
                                    let truncated = if api_err.len() > 50 {
                                        format!("API {:.50}...", api_err)
                                    } else {
                                        format!("API {}", api_err)
                                    };
                                    truncated
                                } else if error_msg.len() > 40 {
                                    format!("{:.40}...", error_msg)
                                } else {
                                    error_msg.clone()
                                };
                                
                                if is_permanent_upload_error(&error_msg) {
                                    pb.finish_with_message(format!("{} {} ({})", style("✗").red(), name, short_error));
                                    tracing::warn!(
                                        "Permanent error for '{}': {} - removing from queue",
                                        name, error_msg
                                    );
                                    if let Some(id) = persist_id {
                                        if let Err(e) = pending_upload_store.remove(id) {
                                            tracing::warn!("Failed to remove failed upload: {}", e);
                                        }
                                    }
                                } else if is_transient_error(&error_msg) {
                                    pb.finish_with_message(format!("{} {} ({}, will retry)", style("⚠").yellow(), name, short_error));
                                    tracing::info!(
                                        "Network error for '{}': {} - will retry on next mount",
                                        name, error_msg
                                    );
                                    // Keep in persistent store for retry
                                } else {
                                    pb.finish_with_message(format!("{} {} ({}, will retry)", style("⚠").yellow(), name, short_error));
                                    tracing::warn!(
                                        "Unknown error for '{}': {} - will retry on next mount",
                                        name, error_msg
                                    );
                                    // Keep in persistent store for retry
                                }
                            };
                            
                            match client.get_file_uploader(
                                pending.parent_uid.clone(),
                                pending.name.clone(),
                                pending.mime_type.clone(),
                                size,
                                Some(std::time::SystemTime::now()),
                                None,
                                None,
                                true,
                            ).await {
                                Ok(uploader) => {
                                    let pb_clone = pb.clone();
                                    let content = pending.content.clone();
                                    match uploader.upload_from_stream(
                                        Box::new(std::io::Cursor::new(content.clone())),
                                        Vec::new(),
                                        Box::new(move |bytes, _total| {
                                            pb_clone.set_position(bytes as u64);
                                        }),
                                    ).await {
                                        Ok(node_uid) => {
                                            pb.finish_with_message(format!("{} {}", style("✓").green(), pending.name));
                                            
                                            // Remove from persistence store on success
                                            if let Some(id) = persist_id {
                                                if let Err(e) = pending_upload_store.remove(&id) {
                                                    tracing::warn!("Failed to remove persisted upload {}: {}", id, e);
                                                }
                                            }
                                            
                                            // Update Index first (Index → FUSE flow)
                                            // The Index event handler will update FUSE state
                                            if let Ok(potential_node) = client.get_node(node_uid.clone()).await {
                                                // Get parent_link_id from pending info
                                                let parent_link_id = pending.parent_uid.link_id.raw().to_string();
                                                let indexed = indexed_node_from_potential(&potential_node, &parent_link_id);
                                                
                                                if let Err(e) = index.upsert_node(&indexed) {
                                                    tracing::warn!("Failed to upsert uploaded node to index: {}", e);
                                                }
                                                
                                                // CRITICAL: Update FUSE state directly after upload
                                                // The index event handler can't find this node because the link_id
                                                // changed from "pending-*" to the real one.
                                                let mut inner = inner.write().await;
                                                
                                                // Get the old pending link_id before we replace it
                                                let old_link_id = inner.nodes.get(&inode)
                                                    .map(|n| n.uid().link_id.raw().to_string());
                                                
                                                // Build new FsNode with real metadata
                                                let new_fs_node = fs_node_from_indexed(&indexed);
                                                let new_link_id = node_uid.link_id.raw().to_string();
                                                let new_uid_str = node_uid.to_string();
                                                
                                                // Update node with real metadata
                                                inner.nodes.insert(inode, new_fs_node);
                                                
                                                // Update link_id_to_inode: remove old pending mapping, add real one
                                                if let Some(old_id) = old_link_id {
                                                    if old_id != new_link_id {
                                                        inner.link_id_to_inode.remove(&old_id);
                                                        tracing::debug!("Replaced pending link_id '{}' with real '{}'", old_id, new_link_id);
                                                    }
                                                }
                                                inner.link_id_to_inode.insert(new_link_id, inode);
                                                
                                                // Update uid_to_inode mapping
                                                inner.uid_to_inode.insert(new_uid_str, inode);
                                                
                                                // Clean up pending state and cache content
                                                inner.pending_files.remove(&inode);
                                                inner.file_cache.put(inode, content);
                                            }
                                        }
                                        Err(e) => {
                                            handle_newfile_error(&persist_id, &pending.name, &e, &pb);
                                        }
                                    }
                                }
                                Err(e) => {
                                    handle_newfile_error(&persist_id, &pending.name, &e, &pb);
                                }
                            }
                        }
                        UploadTask::NewRevision { inode, revision_uid, filename, content, persist_id } => {
                            // Read fresh content from file_cache if available (may have changed since queuing)
                            let fresh_content = {
                                let mut inner_guard = inner.write().await;
                                inner_guard.file_cache.get(&inode).cloned()
                            };
                            let content = fresh_content.unwrap_or(content);
                            
                            tracing::info!("Background uploading revision for '{}' ({} bytes)", filename, content.len());
                            
                            let pb = multi_progress.add(ProgressBar::new(content.len() as u64));
                            pb.set_style(
                                ProgressStyle::default_bar()
                                    .template("{spinner:.white.on_cyan} {msg} [{bar:30.white.on_cyan}] {bytes}/{total_bytes}")
                                    .unwrap()
                                    .progress_chars("█▓░")
                            );
                            pb.set_message(format!("↑ {}", filename));
                            pb.enable_steady_tick(Duration::from_millis(100));
                            
                            let size = content.len() as i64;
                            
                            // Update progress bar size in case content changed
                            pb.set_length(size as u64);
                            
                            // Fetch fresh revision_uid from server at execution time
                            // This handles race conditions where a previous upload completed between queue and execute
                            let node_uid = revision_uid.node_uid.clone();
                            let mut current_revision_uid = match client.get_node(node_uid.clone()).await {
                                Ok(PotentialObject::Node(Node::File(f) | Node::Photo(f))) => {
                                    tracing::debug!("Fetched fresh revision for '{}': {:?}", filename, f.active_revision.uid.revision_id);
                                    f.active_revision.uid.clone()
                                }
                                _ => {
                                    tracing::debug!("Using queued revision for '{}' (fresh fetch failed)", filename);
                                    revision_uid
                                }
                            };
                            
                            // Helper to handle upload errors - shows appropriate message and removes on permanent error
                            let handle_revision_error = |persist_id: &Option<String>, name: &str, error: &anyhow::Error, pb: &ProgressBar| -> bool {
                                let error_msg = error.to_string();
                                // Check for "draft already exists" error - this is recoverable
                                if error_msg.contains("2500") || error_msg.contains("Draft") {
                                    pb.set_message(format!("↑ {} (clearing stale draft...)", name));
                                    return true; // Signal to retry after clearing draft
                                }
                                // Check for "revision no longer up to date" or similar stale revision errors
                                // These can be recovered by refreshing revision_uid from server
                                if is_stale_revision_error(&error_msg) {
                                    pb.set_message(format!("↑ {} (refreshing revision...)", name));
                                    return true; // Signal to retry with fresh revision
                                }
                                // Extract a short error description for display
                                let short_error = if let Some(api_err) = error_msg.strip_prefix("API error ") {
                                    let truncated = if api_err.len() > 50 {
                                        format!("API {:.50}...", api_err)
                                    } else {
                                        format!("API {}", api_err)
                                    };
                                    truncated
                                } else if error_msg.len() > 40 {
                                    format!("{:.40}...", error_msg)
                                } else {
                                    error_msg.clone()
                                };
                                
                                if is_permanent_upload_error(&error_msg) {
                                    pb.finish_with_message(format!("{} {} ({})", style("✗").red(), name, short_error));
                                    tracing::warn!(
                                        "Permanent error for revision '{}': {} - removing from queue",
                                        name, error_msg
                                    );
                                    if let Some(id) = persist_id {
                                        if let Err(e) = pending_upload_store.remove(id) {
                                            tracing::warn!("Failed to remove failed upload: {}", e);
                                        }
                                    }
                                } else if is_transient_error(&error_msg) {
                                    pb.finish_with_message(format!("{} {} ({}, will retry)", style("⚠").yellow(), name, short_error));
                                    tracing::info!(
                                        "Network error for revision '{}': {} - will retry on next mount",
                                        name, error_msg
                                    );
                                    // Keep in persistent store for retry
                                } else {
                                    pb.finish_with_message(format!("{} {} ({}, will retry)", style("⚠").yellow(), name, short_error));
                                    tracing::warn!(
                                        "Unknown error for revision '{}': {} - will retry on next mount",
                                        name, error_msg
                                    );
                                    // Keep in persistent store for retry
                                }
                                false // Don't retry
                            };
                            
                            // Try up to 4 times to handle race conditions (2511 errors can chain)
                            let mut attempts = 0;
                            // current_revision_uid already fetched fresh above
                            loop {
                                attempts += 1;
                                if attempts > 4 {
                                    pb.finish_with_message(format!("{} {} (failed after {} retries)", style("✗").red(), filename, attempts - 1));
                                    break;
                                }
                                
                                match client.get_file_revision_uploader(
                                    current_revision_uid.clone(),
                                    size,
                                    Some(std::time::SystemTime::now()),
                                    None,
                                    None,
                                ).await {
                                    Ok(uploader) => {
                                        let pb_clone = pb.clone();
                                        let content_clone = content.clone();
                                        match uploader.upload_from_stream(
                                            Box::new(std::io::Cursor::new(content_clone)),
                                            Vec::new(),
                                            Box::new(move |bytes, _total| {
                                                pb_clone.set_position(bytes as u64);
                                            }),
                                        ).await {
                                            Ok(new_node_uid) => {
                                                pb.finish_with_message(format!("{} {}", style("✓").green(), filename));
                                                
                                                // Remove from persistence store on success
                                                if let Some(id) = &persist_id {
                                                    if let Err(e) = pending_upload_store.remove(id) {
                                                        tracing::warn!("Failed to remove persisted upload {}: {}", id, e);
                                                    }
                                                }
                                                
                                                // Update Index first (Index → FUSE flow)
                                                // The Index event handler will update FUSE state
                                                if let Ok(potential_node) = client.get_node(new_node_uid.clone()).await {
                                                    // Get parent_link_id from existing index entry
                                                    let link_id = new_node_uid.link_id.raw();
                                                    let parent_link_id = index.get_node(link_id)
                                                        .ok()
                                                        .flatten()
                                                        .and_then(|n| n.parent_link_id)
                                                        .unwrap_or_default();
                                                    
                                                    let indexed = indexed_node_from_potential(&potential_node, &parent_link_id);
                                                    
                                                    if let Err(e) = index.upsert_node(&indexed) {
                                                        tracing::warn!("Failed to upsert uploaded revision to index: {}", e);
                                                    }
                                                    
                                                    // Cache the content and clear pending status
                                                    let mut inner = inner.write().await;
                                                    inner.file_cache.put(inode, content.clone());
                                                    inner.pending_revision_uploads.remove(&inode);
                                                }
                                                break; // Success - exit retry loop
                                            }
                                            Err(e) => {
                                                let should_retry = handle_revision_error(&persist_id, &filename, &e, &pb);
                                                if should_retry {
                                                    // Delete stale draft and retry
                                                    let node_uid = current_revision_uid.node_uid.clone();
                                                    pb.set_message(format!("↑ {} (clearing draft...)", filename));
                                                    match client.delete_draft_revisions(node_uid.clone()).await {
                                                        Ok(n) => tracing::info!("Deleted {} draft revision(s) for {:?}", n, node_uid),
                                                        Err(e) => tracing::warn!("Failed to delete drafts: {}", e),
                                                    }
                                                    // Fetch latest revision ID for retry
                                                    if let Ok(potential) = client.get_node(node_uid.clone()).await {
                                                        if let PotentialObject::Node(Node::File(f) | Node::Photo(f)) = &potential {
                                                            current_revision_uid = f.active_revision.uid.clone();
                                                            pb.set_message(format!("↑ {}", filename));
                                                            continue; // Retry with fresh revision UID
                                                        }
                                                    }
                                                }
                                                break; // Don't retry
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        let should_retry = handle_revision_error(&persist_id, &filename, &e, &pb);
                                        if should_retry {
                                            // Delete stale draft and retry
                                            let node_uid = current_revision_uid.node_uid.clone();
                                            pb.set_message(format!("↑ {} (clearing draft...)", filename));
                                            match client.delete_draft_revisions(node_uid.clone()).await {
                                                Ok(n) => tracing::info!("Deleted {} draft revision(s) for {:?}", n, node_uid),
                                                Err(e) => tracing::warn!("Failed to delete drafts: {}", e),
                                            }
                                            // Fetch latest revision ID for retry  
                                            if let Ok(potential) = client.get_node(node_uid.clone()).await {
                                                if let PotentialObject::Node(Node::File(f) | Node::Photo(f)) = &potential {
                                                    current_revision_uid = f.active_revision.uid.clone();
                                                    pb.set_message(format!("↑ {}", filename));
                                                    continue; // Retry with fresh revision UID
                                                }
                                            }
                                        }
                                        break; // Don't retry
                                    }
                                }
                            }
                            
                            // Always clear pending status when task completes (success or failure)
                            {
                                let mut inner = inner.write().await;
                                inner.pending_revision_uploads.remove(&inode);
                            }
                        }
                        UploadTask::ResumePersisted(mut persisted) => {
                            // Check if we've exceeded max retries
                            if persisted.retry_count() >= MAX_UPLOAD_RETRIES {
                                tracing::warn!(
                                    "Giving up on upload '{}' after {} retries",
                                    persisted.name(),
                                    persisted.retry_count()
                                );
                                if let Err(e) = pending_upload_store.remove(persisted.id()) {
                                    tracing::warn!("Failed to remove failed upload: {}", e);
                                }
                                return;
                            }
                            
                            // Check if upload is stale (older than 24 hours)
                            if persisted.is_stale() {
                                tracing::warn!(
                                    "Removing stale upload '{}' (created more than 24 hours ago)",
                                    persisted.name()
                                );
                                if let Err(e) = pending_upload_store.remove(persisted.id()) {
                                    tracing::warn!("Failed to remove stale upload: {}", e);
                                }
                                return;
                            }
                            
                            // Resume a persisted upload from a previous session
                            let (name, content_len): (String, usize) = match &persisted {
                                PersistentUpload::NewFile { name, content, .. } => {
                                    (name.clone(), content.len())
                                }
                                PersistentUpload::NewRevision { filename, content, .. } => {
                                    (filename.clone(), content.len())
                                }
                            };
                            
                            let retry_info = if persisted.retry_count() > 0 {
                                format!(" (retry {})", persisted.retry_count())
                            } else {
                                String::new()
                            };
                            tracing::info!("Resuming persisted upload for '{}' ({} bytes){}", name, content_len, retry_info);
                            
                            let pb = multi_progress.add(ProgressBar::new(content_len as u64));
                            pb.set_style(
                                ProgressStyle::default_bar()
                                    .template("{spinner:.white.on_yellow} {msg} [{bar:30.white.on_yellow}] {bytes}/{total_bytes}")
                                    .unwrap()
                                    .progress_chars("█▓░")
                            );
                            pb.set_message(format!("⟳ {}", name));
                            pb.enable_steady_tick(Duration::from_millis(100));
                            
                            // Helper to handle upload failure
                            let handle_upload_error = |id: &str, name: &str, error: &anyhow::Error, persisted: &mut PersistentUpload, pb: &ProgressBar| {
                                let error_msg = error.to_string();
                                // Extract a short error description for display
                                let short_error = if let Some(api_err) = error_msg.strip_prefix("API error ") {
                                    // Show "API error XXXX: message" but truncate long messages
                                    let truncated = if api_err.len() > 50 {
                                        format!("API {:.50}...", api_err)
                                    } else {
                                        format!("API {}", api_err)
                                    };
                                    truncated
                                } else if error_msg.len() > 40 {
                                    format!("{:.40}...", error_msg)
                                } else {
                                    error_msg.clone()
                                };
                                
                                // Check if this is a permanent error
                                if is_permanent_upload_error(&error_msg) {
                                    pb.finish_with_message(format!("{} {} ({})", style("✗").red(), name, short_error));
                                    tracing::warn!(
                                        "Permanent error for '{}': {} - removing from queue",
                                        name, error_msg
                                    );
                                    if let Err(e) = pending_upload_store.remove(id) {
                                        tracing::warn!("Failed to remove failed upload: {}", e);
                                    }
                                } else if is_transient_error(&error_msg) {
                                    // Network/connection error - will retry
                                    persisted.increment_retry();
                                    pb.finish_with_message(format!(
                                        "{} {} ({}, retry {}/{})", 
                                        style("⚠").yellow(), name, short_error, persisted.retry_count(), MAX_UPLOAD_RETRIES
                                    ));
                                    tracing::info!(
                                        "Network error for '{}': {} - will retry (attempt {}/{})",
                                        name, error_msg, persisted.retry_count(), MAX_UPLOAD_RETRIES
                                    );
                                    if let Err(e) = pending_upload_store.save(persisted) {
                                        tracing::warn!("Failed to update retry count: {}", e);
                                    }
                                } else {
                                    // Unknown error - treat as transient but warn
                                    persisted.increment_retry();
                                    pb.finish_with_message(format!(
                                        "{} {} ({}, retry {}/{})", 
                                        style("⚠").yellow(), name, short_error, persisted.retry_count(), MAX_UPLOAD_RETRIES
                                    ));
                                    tracing::warn!(
                                        "Unknown error for '{}': {} - will retry (attempt {}/{})",
                                        name, error_msg, persisted.retry_count(), MAX_UPLOAD_RETRIES
                                    );
                                    if let Err(e) = pending_upload_store.save(persisted) {
                                        tracing::warn!("Failed to update retry count: {}", e);
                                    }
                                }
                            };
                            
                            match persisted.clone() {
                                PersistentUpload::NewFile { id, parent_uid, name, mime_type, content, .. } => {
                                    let size = content.len() as i64;
                                    match client.get_file_uploader(
                                        parent_uid.clone(),
                                        name.clone(),
                                        mime_type,
                                        size,
                                        Some(std::time::SystemTime::now()),
                                        None,
                                        None,
                                        true,
                                    ).await {
                                        Ok(uploader) => {
                                            let pb_clone = pb.clone();
                                            match uploader.upload_from_stream(
                                                Box::new(std::io::Cursor::new(content)),
                                                Vec::new(),
                                                Box::new(move |bytes, _total| {
                                                    pb_clone.set_position(bytes as u64);
                                                }),
                                            ).await {
                                                Ok(node_uid) => {
                                                    pb.finish_with_message(format!("{} {}", style("✓").green(), name));
                                                    // Remove from persistence store on success
                                                    if let Err(e) = pending_upload_store.remove(&id) {
                                                        tracing::warn!("Failed to remove persisted upload {}: {}", id, e);
                                                    }
                                                    
                                                    // Update Index (Index → FUSE flow)
                                                    if let Ok(potential_node) = client.get_node(node_uid).await {
                                                        let parent_link_id = parent_uid.link_id.raw().to_string();
                                                        let indexed = indexed_node_from_potential(&potential_node, &parent_link_id);
                                                        if let Err(e) = index.upsert_node(&indexed) {
                                                            tracing::warn!("Failed to upsert resumed upload to index: {}", e);
                                                        }
                                                    }
                                                    
                                                    tracing::info!("Successfully resumed upload for '{}'", name);
                                                }
                                                Err(e) => {
                                                    handle_upload_error(&id, &name, &e, &mut persisted, &pb);
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            handle_upload_error(&id, &name, &e, &mut persisted, &pb);
                                        }
                                    }
                                }
                                PersistentUpload::NewRevision { id, revision_uid, filename, content, .. } => {
                                    let size = content.len() as i64;
                                    let mut current_revision_uid = revision_uid.clone();
                                    
                                    // Try upload, clearing draft if needed (one retry)
                                    let mut cleared_draft = false;
                                    loop {
                                        match client.get_file_revision_uploader(
                                            current_revision_uid.clone(),
                                            size,
                                            Some(std::time::SystemTime::now()),
                                            None,
                                            None,
                                        ).await {
                                            Ok(uploader) => {
                                                let pb_clone = pb.clone();
                                                match uploader.upload_from_stream(
                                                    Box::new(std::io::Cursor::new(content.clone())),
                                                    Vec::new(),
                                                    Box::new(move |bytes, _total| {
                                                        pb_clone.set_position(bytes as u64);
                                                    }),
                                                ).await {
                                                    Ok(new_node_uid) => {
                                                        pb.finish_with_message(format!("{} {}", style("✓").green(), filename));
                                                        // Remove from persistence store on success
                                                        if let Err(e) = pending_upload_store.remove(&id) {
                                                            tracing::warn!("Failed to remove persisted upload {}: {}", id, e);
                                                        }
                                                        
                                                        // Update Index (Index → FUSE flow)
                                                        if let Ok(potential_node) = client.get_node(new_node_uid.clone()).await {
                                                            let link_id = new_node_uid.link_id.raw();
                                                            let parent_link_id = index.get_node(link_id)
                                                                .ok()
                                                                .flatten()
                                                                .and_then(|n| n.parent_link_id)
                                                                .unwrap_or_default();
                                                            let indexed = indexed_node_from_potential(&potential_node, &parent_link_id);
                                                            if let Err(e) = index.upsert_node(&indexed) {
                                                                tracing::warn!("Failed to upsert resumed revision to index: {}", e);
                                                            }
                                                        }
                                                        
                                                        tracing::info!("Successfully resumed revision upload for '{}'", filename);
                                                    }
                                                    Err(e) => {
                                                        let error_msg = e.to_string();
                                                        // Check for draft error or stale revision and try to recover
                                                        let is_draft_err = error_msg.contains("2500") || error_msg.contains("Draft");
                                                        let is_stale_err = is_stale_revision_error(&error_msg);
                                                        
                                                        if !cleared_draft && (is_draft_err || is_stale_err) {
                                                            pb.set_message(format!("⟳ {} (refreshing revision...)", filename));
                                                            let node_uid = current_revision_uid.node_uid.clone();
                                                            if is_draft_err {
                                                                let _ = client.delete_draft_revisions(node_uid.clone()).await;
                                                            }
                                                            // Get fresh revision UID
                                                            if let Ok(potential) = client.get_node(node_uid).await {
                                                                if let PotentialObject::Node(Node::File(f) | Node::Photo(f)) = &potential {
                                                                    current_revision_uid = f.active_revision.uid.clone();
                                                                    cleared_draft = true;
                                                                    pb.set_message(format!("⟳ {}", filename));
                                                                    continue; // Retry
                                                                }
                                                            }
                                                        }
                                                        handle_upload_error(&id, &filename, &e, &mut persisted, &pb);
                                                    }
                                                }
                                                break;
                                            }
                                            Err(e) => {
                                                let error_msg = e.to_string();
                                                // Check for draft error or stale revision and try to recover
                                                let is_draft_err = error_msg.contains("2500") || error_msg.contains("Draft");
                                                let is_stale_err = is_stale_revision_error(&error_msg);
                                                
                                                if !cleared_draft && (is_draft_err || is_stale_err) {
                                                    pb.set_message(format!("⟳ {} (refreshing revision...)", filename));
                                                    let node_uid = current_revision_uid.node_uid.clone();
                                                    if is_draft_err {
                                                        let _ = client.delete_draft_revisions(node_uid.clone()).await;
                                                    }
                                                    // Get fresh revision UID
                                                    if let Ok(potential) = client.get_node(node_uid).await {
                                                        if let PotentialObject::Node(Node::File(f) | Node::Photo(f)) = &potential {
                                                            current_revision_uid = f.active_revision.uid.clone();
                                                            cleared_draft = true;
                                                            pb.set_message(format!("⟳ {}", filename));
                                                            continue; // Retry
                                                        }
                                                    }
                                                }
                                                handle_upload_error(&id, &filename, &e, &mut persisted, &pb);
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    }); // End of spawned upload task
                }
            });
        }
        
        // Load and queue any pending uploads from previous sessions
        match self.pending_upload_store.load_all() {
            Ok(pending_uploads) => {
                if !pending_uploads.is_empty() {
                    tracing::info!("Found {} pending uploads from previous session", pending_uploads.len());
                    for persisted in pending_uploads {
                        // Skip stale or max-retried uploads (they'll be cleaned during actual processing)
                        if persisted.is_stale() {
                            tracing::warn!("Skipping stale upload '{}' (will be cleaned up)", persisted.name());
                            continue;
                        }
                        if persisted.retry_count() >= MAX_UPLOAD_RETRIES {
                            tracing::warn!("Skipping upload '{}' - exceeded max retries (will be cleaned up)", persisted.name());
                            continue;
                        }
                        
                        tracing::info!("Queueing resumed upload for '{}' (retry {})", persisted.name(), persisted.retry_count());
                        if let Err(e) = self.upload_tx.send(UploadTask::ResumePersisted(persisted)) {
                            tracing::error!("Failed to queue resumed upload: {}", e);
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Failed to load pending uploads: {}", e);
            }
        }
        
        // Start background event poller
        {
            let volume_id = {
                let inner = self.inner.read().await;
                inner.volume_id.clone()
            };
            
            if let Some(volume_id) = volume_id {
                let client = self.client.clone();
                let inner = self.inner.clone();
                let index = self.index.clone();
                let multi_progress = self.multi_progress.clone();
                
                // Get the initial event ID
                {
                    let client_guard = client.read().await;
                    if let Some(client) = client_guard.as_ref() {
                        match client.get_volume_latest_event_id(volume_id.clone()).await {
                            Ok(event_id) => {
                                tracing::info!("Starting event polling from event ID: {}", event_id);
                                let mut inner_guard = inner.write().await;
                                inner_guard.last_event_id = Some(event_id);
                            }
                            Err(e) => {
                                tracing::warn!("Failed to get latest event ID: {}", e);
                            }
                        }
                    }
                }
                
                tokio::spawn(async move {
                    // Small delay before first poll to let mount complete
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    tracing::info!("Event poller started, polling every {} seconds", EVENT_POLL_INTERVAL.as_secs());
                    
                    loop {
                        let event_id = {
                            let inner_guard = inner.read().await;
                            inner_guard.last_event_id.clone()
                        };
                        
                        let Some(last_event_id) = event_id else {
                            tokio::time::sleep(EVENT_POLL_INTERVAL).await;
                            continue;
                        };
                        
                        let client_guard = client.read().await;
                        let Some(client) = client_guard.as_ref() else {
                            tokio::time::sleep(EVENT_POLL_INTERVAL).await;
                            continue;
                        };
                        
                        // Poll for events
                        match client.poll_volume_events(volume_id.clone(), &last_event_id).await {
                            Ok(response) => {
                                let event_count = response.events.len();
                                
                                // Show spinner if we have events to process
                                let spinner = if event_count > 0 {
                                    let pb = multi_progress.add(ProgressBar::new_spinner());
                                    pb.set_style(
                                        ProgressStyle::default_spinner()
                                            .template("{spinner:.magenta} {msg}")
                                            .unwrap()
                                    );
                                    pb.enable_steady_tick(Duration::from_millis(80));
                                    pb.set_message(format!("Syncing {} change{} from server...", 
                                        event_count,
                                        if event_count == 1 { "" } else { "s" }
                                    ));
                                    Some(pb)
                                } else {
                                    None
                                };
                                
                                // Check if full refresh needed
                                if response.refresh {
                                    if let Some(pb) = &spinner {
                                        pb.set_message("Full refresh requested by server...");
                                    }
                                    tracing::warn!("Server requested full refresh - clearing cache");
                                    let mut inner_guard = inner.write().await;
                                    // Clear loaded folders to force re-fetch
                                    inner_guard.loaded_folders.clear();
                                    // Clear file cache
                                    inner_guard.file_cache.clear();
                                    inner_guard.last_event_id = Some(response.event_id);
                                    if let Some(pb) = spinner {
                                        pb.finish_and_clear();
                                    }
                                    continue;
                                }
                                
                                // Process each event
                                let mut processed = 0;
                                for event in &response.events {
                                    processed += 1;
                                    if let Some(pb) = &spinner {
                                        pb.set_message(format!("Syncing {}/{} changes from server...", 
                                            processed, event_count));
                                    }
                                    if let Err(e) = Self::process_event(&index, client, &volume_id, event).await {
                                        tracing::warn!("Failed to process event {}: {}", event.event_id, e);
                                    }
                                }
                                
                                // Clear spinner
                                if let Some(pb) = spinner {
                                    pb.finish_and_clear();
                                }
                                
                                // Update cursor
                                {
                                    let mut inner_guard = inner.write().await;
                                    inner_guard.last_event_id = Some(response.event_id.clone());
                                }
                                
                                // If more events available, poll immediately
                                if response.more {
                                    tracing::debug!("More events available, polling immediately");
                                    continue;
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to poll events: {}", e);
                            }
                        }
                        
                        // Wait before next poll
                        drop(client_guard);
                        tokio::time::sleep(EVENT_POLL_INTERVAL).await;
                    }
                });
            }
        }
        
        // Spawn index event handler task
        // ARCHITECTURE: This is the bridge between the index and FUSE.
        // When the index emits events, this handler updates FUSE state
        // by loading data FROM the index (the single source of truth).
        {
            let inner = self.inner.clone();
            let index = self.index.clone();
            let mut event_rx = self.index.subscribe();
            
            tokio::spawn(async move {
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
                            match event {
                                IndexEvent::NodeUpserted { link_id, parent_link_id, node_type: _ } => {
                                    // Load the node from index and update FUSE
                                    if let Ok(Some(indexed)) = index.get_node(&link_id) {
                                        let mut inner_guard = inner.write().await;
                                        
                                        // Convert IndexedNode to FsNode
                                        let fs_node = fs_node_from_indexed(&indexed);
                                        let node_name = fs_node.name().to_string();
                                        
                                        // Check if node already exists in FUSE
                                        if let Some(&existing_inode) = inner_guard.link_id_to_inode.get(&link_id) {
                                            // Update existing node
                                            inner_guard.nodes.insert(existing_inode, fs_node);
                                            // Clear file cache since content may have changed
                                            inner_guard.file_cache.remove(&existing_inode);
                                            tracing::debug!("Updated FUSE node '{}' (inode {}) from index", node_name, existing_inode);
                                        } else if let Some(parent) = parent_link_id {
                                            // New node - add to FUSE and parent's children
                                            if let Some(&parent_inode) = inner_guard.link_id_to_inode.get(&parent) {
                                                let inode = inner_guard.insert_node(fs_node, None);
                                                inner_guard.link_id_to_inode.insert(link_id.clone(), inode);
                                                
                                                // Add to parent's children list
                                                if let Some(children) = inner_guard.children.get_mut(&parent_inode) {
                                                    if !children.contains(&inode) {
                                                        children.push(inode);
                                                    }
                                                } else {
                                                    inner_guard.children.insert(parent_inode, vec![inode]);
                                                }
                                                tracing::info!("Added new FUSE node '{}' (inode {}) to parent {}", node_name, inode, parent_inode);
                                            } else {
                                                // Parent not in FUSE yet - that's fine, it will load when accessed
                                                tracing::debug!("Node '{}' parent not in FUSE yet, will appear when parent accessed", node_name);
                                            }
                                        }
                                    }
                                }
                                IndexEvent::NodeRemoved { link_id, parent_link_id } => {
                                    let mut inner_guard = inner.write().await;
                                    if let Some(&inode) = inner_guard.link_id_to_inode.get(&link_id) {
                                        // Get the name before removing
                                        let node_name = inner_guard.nodes.get(&inode)
                                            .map(|n| n.name().to_string())
                                            .unwrap_or_else(|| "unknown".to_string());
                                        
                                        // Recursively remove this node and all children from FUSE
                                        fn remove_recursive(inner: &mut ProtonDriveFsInner, inode: u64) -> usize {
                                            let mut count = 1;
                                            // Remove children first
                                            if let Some(children) = inner.children.remove(&inode) {
                                                for child_inode in children {
                                                    count += remove_recursive(inner, child_inode);
                                                }
                                            }
                                            // Remove the node itself
                                            if let Some(node) = inner.nodes.remove(&inode) {
                                                inner.uid_to_inode.remove(&node.uid().to_string());
                                                inner.link_id_to_inode.remove(node.uid().link_id.raw());
                                            }
                                            inner.file_cache.remove(&inode);
                                            inner.loaded_folders.remove(&inode);
                                            count
                                        }
                                        
                                        let removed_count = remove_recursive(&mut inner_guard, inode);
                                        
                                        // Remove from parent's children list
                                        if let Some(parent) = parent_link_id {
                                            if let Some(&parent_inode) = inner_guard.link_id_to_inode.get(&parent) {
                                                if let Some(children) = inner_guard.children.get_mut(&parent_inode) {
                                                    children.retain(|&i| i != inode);
                                                }
                                            }
                                        }
                                        
                                        tracing::info!("Removed FUSE node '{}' (inode {}) and {} total nodes", node_name, inode, removed_count);
                                    }
                                }
                                IndexEvent::ChildrenLoaded { parent_link_id, count } => {
                                    // Mark folder as not loaded so next access reloads from index
                                    // (which now has the fresh children)
                                    let mut inner_guard = inner.write().await;
                                    if let Some(&parent_inode) = inner_guard.link_id_to_inode.get(&parent_link_id) {
                                        inner_guard.loaded_folders.remove(&parent_inode);
                                        tracing::debug!("Folder {} will reload {} children from index on next access", parent_inode, count);
                                    }
                                }
                                IndexEvent::OfflineStatusChanged { link_id, available } => {
                                    let mut inner_guard = inner.write().await;
                                    if let Some(&inode) = inner_guard.link_id_to_inode.get(&link_id) {
                                        inner_guard.file_cache.remove(&inode);
                                        tracing::debug!("Invalidated cache for {} (offline={})", link_id, available);
                                    }
                                }
                                IndexEvent::MutationQueued { .. } | IndexEvent::MutationRemoved { .. } => {
                                    // Handled by sync task
                                }
                                IndexEvent::IndexCleared => {
                                    let mut inner_guard = inner.write().await;
                                    inner_guard.loaded_folders.clear();
                                    inner_guard.file_cache.clear();
                                    tracing::info!("Index cleared - invalidated all FUSE caches");
                                }
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                            tracing::warn!("Index event handler lagged {} events", n);
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                            tracing::debug!("Index event channel closed, stopping handler");
                            break;
                        }
                    }
                }
            });
        }

        // Spawn background sync task
        // This automatically syncs pending mutations (created while offline) when online
        {
            let client = self.client.clone();
            let index = self.index.clone();
            let is_online = self.is_online.clone();
            let mut event_rx = self.index.subscribe();

            // Sync interval - check for pending mutations periodically
            const SYNC_INTERVAL: Duration = Duration::from_secs(30);

            tokio::spawn(async move {
                // Initial sync delay to let mount complete
                tokio::time::sleep(Duration::from_secs(2)).await;

                // Initial sync of any pending mutations from previous sessions
                {
                    let client_guard = client.read().await;
                    if let Some(client) = client_guard.as_ref() {
                        if let Ok(count) = index.pending_mutation_count() {
                            if count > 0 {
                                tracing::info!("Starting initial sync of {} pending mutations", count);
                                if let Err(e) = background_sync(client, &index).await {
                                    tracing::warn!("Initial sync failed: {}", e);
                                }
                            }
                        }
                    }
                }

                let mut sync_interval = tokio::time::interval(SYNC_INTERVAL);
                sync_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

                loop {
                    tokio::select! {
                        // Periodic sync check
                        _ = sync_interval.tick() => {
                            // Only sync if online
                            if !is_online.load(std::sync::atomic::Ordering::Relaxed) {
                                continue;
                            }

                            // Check if there are pending mutations
                            let count = match index.pending_mutation_count() {
                                Ok(c) => c,
                                Err(e) => {
                                    tracing::warn!("Failed to check pending mutations: {}", e);
                                    continue;
                                }
                            };

                            if count == 0 {
                                continue;
                            }

                            tracing::debug!("Periodic sync: {} pending mutations", count);

                            let client_guard = client.read().await;
                            if let Some(client) = client_guard.as_ref() {
                                if let Err(e) = background_sync(client, &index).await {
                                    tracing::warn!("Periodic sync failed: {}", e);
                                }
                            }
                        }

                        // Event-driven sync trigger
                        event = event_rx.recv() => {
                            match event {
                                Ok(IndexEvent::MutationQueued { .. }) => {
                                    // A new mutation was queued - try to sync immediately if online
                                    if !is_online.load(std::sync::atomic::Ordering::Relaxed) {
                                        tracing::debug!("Mutation queued but offline, will sync later");
                                        continue;
                                    }

                                    // Small delay to batch multiple quick mutations
                                    tokio::time::sleep(Duration::from_millis(500)).await;

                                    let client_guard = client.read().await;
                                    if let Some(client) = client_guard.as_ref() {
                                        if let Err(e) = background_sync(client, &index).await {
                                            tracing::warn!("Event-triggered sync failed: {}", e);
                                        }
                                    }
                                }
                                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                                    tracing::debug!("Sync task: event channel closed, stopping");
                                    break;
                                }
                                _ => {} // Ignore other events
                            }
                        }
                    }
                }
            });
        }
        
        Ok(())
    }
    
    /// Process a single volume event.
    /// 
    /// ARCHITECTURE: Server events update the INDEX, which emits events
    /// that the index event handler uses to update FUSE state.
    /// This ensures the index is the single source of truth.
    async fn process_event(
        index: &Arc<OfflineIndex>,
        client: &ProtonDriveClient,
        volume_id: &VolumeId,
        event: &VolumeEventDto,
    ) -> Result<()> {
        let link_id = event.link.link_id.raw();
        let parent_link_id = event.link.parent_link_id.as_ref().map(|l| l.raw().to_string());
        let event_type = event.event_type();
        tracing::info!("Processing event {:?} for link {}", event_type, link_id);
        
        match event_type {
            Some(VolumeEventType::Create) | Some(VolumeEventType::UpdateMetadata) | Some(VolumeEventType::UpdateContent) => {
                // Fetch the node from server and update the index.
                // The index will emit NodeUpserted event, which the handler will use to update FUSE.
                let node_uid = NodeUid::new(volume_id.clone(), event.link.link_id.clone());
                
                match client.get_node(node_uid.clone()).await {
                    Ok(potential) => {
                        // Convert to IndexedNode and upsert into index
                        let parent_id = parent_link_id.clone().unwrap_or_default();
                        let indexed = indexed_node_from_potential(&potential, &parent_id);
                        
                        if let Err(e) = index.upsert_node(&indexed) {
                            tracing::warn!("Failed to upsert node {} to index: {}", link_id, e);
                        } else {
                            tracing::debug!("Updated index for node {} (event: {:?})", link_id, event_type);
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch node {} from server: {}", link_id, e);
                    }
                }
            }
            Some(VolumeEventType::Delete) => {
                // Delete from index (recursively in case it's a folder).
                // The index will emit NodeRemoved event, which the handler will use to remove from FUSE.
                match index.delete_node_recursive(link_id) {
                    Ok(count) => tracing::debug!("Deleted {} node(s) from index for {}", count, link_id),
                    Err(e) => tracing::warn!("Failed to delete node {} from index: {}", link_id, e),
                }
            }
            None => {
                tracing::warn!("Unknown event type '{}' for link {}", event.event_type, link_id);
            }
        }
        
        Ok(())
    }

    /// Load children of a folder if not already loaded.
    /// INDEX-FIRST: Loads from index immediately for instant response,
    /// then refreshes from network in background.
    async fn ensure_children_loaded(&self, inode: u64) -> Result<()> {
        // Validate inode
        if inode == 0 {
            return Err(anyhow::anyhow!("Invalid inode 0"));
        }
        
        // Check if already loaded in memory
        {
            let inner = self.inner.read().await;
            if inner.loaded_folders.contains(&inode) {
                return Ok(());
            }
        }

        // Get the folder's NodeUid and link_id
        let (folder_uid, folder_link_id) = {
            let inner = self.inner.read().await;
            match inner.nodes.get(&inode) {
                Some(FsNode::Folder(f)) => (f.uid.clone(), f.uid.link_id.raw().to_string()),
                Some(FsNode::Degraded(d)) if !d.is_file => (d.uid.clone(), d.uid.link_id.raw().to_string()),
                Some(other) => {
                    tracing::error!("ensure_children_loaded: inode {} is not a folder, it's a {:?}", inode, other.file_type());
                    return Err(anyhow::anyhow!("Not a folder"));
                }
                None => {
                    tracing::error!("ensure_children_loaded: inode {} not found in nodes", inode);
                    return Err(anyhow::anyhow!("Node not found"));
                }
            }
        };
        
        tracing::debug!("Loading children for folder {} (uid={})", inode, folder_uid);

        // INDEX-FIRST: Check if we have cached children in the index
        let indexed_children = self.index.get_children(&folder_link_id).unwrap_or_default();
        
        if !indexed_children.is_empty() {
            // Load from index immediately (instant!)
            tracing::info!("Loading {} children from index for folder {} (instant)", indexed_children.len(), inode);
            self.load_children_from_index(inode, indexed_children).await?;
            
            // Spawn background refresh from network
            let client = self.client.clone();
            let inner = self.inner.clone();
            let index = self.index.clone();
            let is_online = self.is_online.clone();
            let folder_uid = folder_uid.clone();
            let folder_link_id = folder_link_id.clone();
            
            tokio::spawn(async move {
                // Small delay to let the UI render first
                tokio::time::sleep(Duration::from_millis(100)).await;
                
                if let Err(e) = background_refresh_children(
                    &client, &inner, &index, &is_online, inode, &folder_uid, &folder_link_id
                ).await {
                    tracing::debug!("Background refresh failed for folder {}: {}", inode, e);
                }
            });
            
            return Ok(());
        }

        // No cached children - must load from network (blocking)
        tracing::debug!("No cached children for folder {}, loading from network", inode);
        let network_result = self.load_children_from_network(inode, &folder_uid, &folder_link_id).await;
        
        match network_result {
            Ok(()) => {
                // Successfully loaded from network, mark as online
                self.is_online.store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Err(e) => {
                // Network failed
                let error_str = e.to_string();
                if is_transient_error(&error_str) {
                    self.is_online.store(false, std::sync::atomic::Ordering::Relaxed);
                    tracing::warn!("Network unavailable and no cached children for folder {}", inode);
                }
                return Err(e);
            }
        }

        Ok(())
    }

    /// Load children from the network and update the index.
    /// 
    /// ARCHITECTURE: This updates the INDEX with fresh data from the network,
    /// then immediately loads from the index into FUSE (for synchronous access).
    /// This ensures the index is always the source of truth.
    async fn load_children_from_network(&self, inode: u64, folder_uid: &NodeUid, folder_link_id: &str) -> Result<()> {
        // Get children stream - release client lock immediately after creating stream.
        let children_stream = {
            let client = self.client.read().await;
            let client = client.as_ref().ok_or_else(|| anyhow::anyhow!("No client"))?;
            client.enumerate_folder_children(folder_uid.clone()).await?
        };

        // Collect all children (this does network I/O but doesn't hold any locks)
        let mut indexed_nodes = Vec::new();
        let mut children_stream = pin!(children_stream);
        let mut child_count = 0;
        
        while let Some(result) = children_stream.next().await {
            match result {
                Ok(potential) => {
                    let indexed = indexed_node_from_potential(&potential, folder_link_id);
                    indexed_nodes.push(indexed);
                    child_count += 1;
                }
                Err(e) => {
                    tracing::error!("Failed to fetch child {} for folder {}: {}", child_count, inode, e);
                    continue;
                }
            }
        }
        
        tracing::debug!("Loaded {} children for folder {} from network", child_count, inode);

        // Update the offline index (this is the source of truth)
        // Note: This emits ChildrenLoaded event, but since we're loading synchronously
        // below, the event handler will just see an already-loaded folder.
        if let Err(e) = self.index.upsert_children(folder_link_id, &indexed_nodes) {
            tracing::warn!("Failed to batch index children for folder {}: {}", folder_link_id, e);
        }

        // Now load from the index into FUSE (index is the source of truth)
        let indexed_children = self.index.get_children(folder_link_id).unwrap_or_default();
        self.load_children_from_index(inode, indexed_children).await?;

        // Spawn background task to fetch thumbnails for files
        self.spawn_thumbnail_fetcher(inode, folder_link_id).await;

        Ok(())
    }

    /// Spawn a background task to fetch and plant thumbnails for files in a folder.
    async fn spawn_thumbnail_fetcher(&self, inode: u64, _folder_link_id: &str) {
        // Collect files with thumbnails for background fetching
        let files_with_thumbnails: Vec<(u64, NodeUid, String, i64, PathBuf)> = {
            let inner = self.inner.read().await;
            let child_inodes = inner.children.get(&inode).cloned().unwrap_or_default();
            let mut files = Vec::new();
            let mut files_without_thumbs = 0;
            let mut cached_thumbs = 0;
            for &child_inode in &child_inodes {
                if let Some(FsNode::File(file_meta)) = inner.nodes.get(&child_inode) {
                    // Only process files with thumbnails
                    if let Some(ref thumb_id) = file_meta.thumbnail_id {
                        // Get the modification time for the thumbnail cache key
                        let mtime_secs = file_meta.modification_time
                            .unwrap_or(file_meta.creation_time)
                            .timestamp();
                        
                        // Build the relative path for this file
                        if let Some(rel_path) = inner.build_relative_path(child_inode) {
                            // Check if thumbnail is already cached locally
                            let full_path = self.mount_path.join("MyFiles").join(&rel_path);
                            if has_cached_thumbnail(&full_path, mtime_secs) {
                                cached_thumbs += 1;
                                continue;
                            }
                            
                            files.push((
                                child_inode,
                                file_meta.uid.clone(),
                                thumb_id.clone(),
                                mtime_secs,
                                rel_path,
                            ));
                        }
                    } else {
                        files_without_thumbs += 1;
                    }
                }
            }
            if cached_thumbs > 0 || files_without_thumbs > 0 || !files.is_empty() {
                tracing::debug!(
                    "Folder has {} files needing thumbnails, {} cached, {} without",
                    files.len(), cached_thumbs, files_without_thumbs
                );
            }
            files
        };

        // Spawn background task to fetch and plant thumbnails in parallel
        if !files_with_thumbnails.is_empty() {
            let client = self.client.clone();
            let mount_path = self.mount_path.clone();
            let count = files_with_thumbnails.len();
            
            tracing::debug!("Spawning thumbnail fetch task for {} files", count);
            
            tokio::spawn(async move {
                use futures_util::stream::StreamExt;
                
                // Limit concurrent thumbnail fetches to avoid overwhelming the API
                const MAX_CONCURRENT_THUMBNAILS: usize = 4;
                
                let results = futures_util::stream::iter(files_with_thumbnails)
                    .map(|(inode, node_uid, thumb_id, mtime_secs, rel_path)| {
                        let client = client.clone();
                        let mount_path = mount_path.clone();
                        async move {
                            // Build the full file path
                            let full_path = mount_path.join("MyFiles").join(&rel_path);
                            
                            // Try to fetch the thumbnail from Proton Drive
                            let thumbnail_data = {
                                let client_guard = client.read().await;
                                if let Some(ref c) = *client_guard {
                                    match c.fetch_thumbnail(node_uid.clone(), thumb_id.clone()).await {
                                        Ok(data) => Some(data),
                                        Err(e) => {
                                            tracing::debug!(
                                                "Failed to fetch thumbnail for inode {} ({}): {}",
                                                inode, rel_path.display(), e
                                            );
                                            None
                                        }
                                    }
                                } else {
                                    None
                                }
                            };
                            
                            // Plant the thumbnail in the freedesktop cache
                            if let Some(data) = thumbnail_data {
                                if let Err(e) = plant_freedesktop_thumbnail(&full_path, mtime_secs, &data) {
                                    tracing::debug!(
                                        "Failed to plant thumbnail for {}: {}",
                                        rel_path.display(), e
                                    );
                                    false
                                } else {
                                    true
                                }
                            } else {
                                false
                            }
                        }
                    })
                    .buffer_unordered(MAX_CONCURRENT_THUMBNAILS)
                    .collect::<Vec<bool>>()
                    .await;
                
                let planted = results.iter().filter(|&&x| x).count();
                if planted > 0 {
                    tracing::info!("Planted {}/{} thumbnails", planted, count);
                }
            });
        }
    }

    /// Load children from the offline index (used when network is unavailable).
    async fn load_children_from_index(&self, inode: u64, indexed_children: Vec<IndexedNode>) -> Result<()> {
        use proton_drive_sdk::links::LinkId;
        use proton_drive_sdk::node::revision::RevisionUid;
        use proton_drive_sdk::revision::RevisionId;
        
        let mut inner = self.inner.write().await;
        let mut child_inodes = Vec::new();
        
        for indexed in indexed_children {
            // Convert IndexedNode to FsNode
            let node_uid = NodeUid::new(
                VolumeId::new(indexed.volume_id.clone()),
                LinkId::new(indexed.link_id.clone()),
            );
            
            let fs_node = match indexed.node_type {
                NodeType::File => {
                    // Create a minimal file metadata from index
                    let revision_uid = indexed.revision_id.as_ref().map(|rev_id| {
                        RevisionUid::new(node_uid.clone(), RevisionId::new(rev_id.clone()))
                    }).unwrap_or_else(|| {
                        RevisionUid::new(node_uid.clone(), RevisionId::new("unknown".to_string()))
                    });
                    
                    FsNode::File(ProtonFileMetadata {
                        uid: node_uid,
                        parent_uid: indexed.parent_link_id.map(|p| {
                            // Get parent's volume_id from the inner state if possible
                            NodeUid::new(
                                VolumeId::new(indexed.volume_id.clone()),
                                LinkId::new(p),
                            )
                        }),
                        name: indexed.name,
                        mime_type: indexed.mime_type.unwrap_or_else(|| "application/octet-stream".to_string()),
                        size: indexed.size.unwrap_or(0) as u64,
                        size_on_cloud: indexed.size.unwrap_or(0) as u64,
                        creation_time: indexed.creation_time,
                        modification_time: indexed.modification_time,
                        trash_time: None,
                        author_email: None,
                        name_author_email: None,
                        owner_email: None,
                        owner_organisation: None,
                        revision_uid,
                        revision_creation_time: indexed.modification_time.unwrap_or(indexed.creation_time),
                        content_sha1: None,
                        is_photo: false,
                        capture_time: None,
                        thumbnail_id: None,
                    })
                }
                NodeType::Folder => {
                    FsNode::Folder(ProtonFolderMetadata {
                        uid: node_uid,
                        parent_uid: indexed.parent_link_id.map(|p| {
                            NodeUid::new(
                                VolumeId::new(indexed.volume_id.clone()),
                                LinkId::new(p),
                            )
                        }),
                        name: indexed.name,
                        creation_time: indexed.creation_time,
                        trash_time: None,
                        author_email: None,
                        name_author_email: None,
                        owner_email: None,
                        owner_organisation: None,
                        is_album: false,
                    })
                }
            };
            
            let child_inode = inner.insert_node(fs_node, None);
            child_inodes.push(child_inode);
        }
        
        inner.children.insert(inode, child_inodes);
        inner.loaded_folders.insert(inode);
        
        Ok(())
    }

    /// Get a node by inode.
    async fn get_node(&self, inode: u64) -> Option<FsNode> {
        let inner = self.inner.read().await;
        inner.nodes.get(&inode).cloned()
    }

    /// Get the revision UID for a file inode (without triggering download).
    async fn get_revision_uid(&self, inode: u64) -> Option<RevisionUid> {
        let inner = self.inner.read().await;
        if let Some(FsNode::File(file_meta)) = inner.nodes.get(&inode) {
            Some(file_meta.revision_uid.clone())
        } else {
            None
        }
    }

    /// Find a child node by name.
    async fn find_child(&self, parent_inode: u64, name: &OsStr) -> Option<(u64, FsNode)> {
        // Ensure children are loaded
        if self.ensure_children_loaded(parent_inode).await.is_err() {
            return None;
        }

        let name_str = name.to_string_lossy();
        let inner = self.inner.read().await;
        
        if let Some(children) = inner.children.get(&parent_inode) {
            for &child_inode in children {
                if let Some(child) = inner.nodes.get(&child_inode) {
                    if child.name() == name_str {
                        return Some((child_inode, child.clone()));
                    }
                }
            }
        }
        None
    }

    /// Get cached file content, downloading if necessary.
    /// Uses a two-tier cache: in-memory (fast, session-only) and disk (slower, persistent).
    async fn get_file_content(&self, inode: u64) -> Result<Vec<u8>> {
        // First check if this is a pending file (created locally, not yet uploaded)
        // These have fake revision UIDs like "pending-XXX~pending" and cannot be downloaded
        {
            let inner = self.inner.read().await;
            if let Some(pending) = inner.pending_files.get(&inode) {
                tracing::debug!("Returning buffered content for pending file inode {} ({} bytes)", inode, pending.content.len());
                return Ok(pending.content.clone());
            }
        }
        
        // Get file metadata to get revision UID and filename
        let (revision_uid, filename, file_size) = {
            let inner = self.inner.read().await;
            match inner.nodes.get(&inode) {
                Some(FsNode::File(f)) => (f.revision_uid.clone(), f.name.clone(), f.size),
                _ => return Err(anyhow::anyhow!("Not a file or degraded node")),
            }
        };
        
        // Check if this has a pending revision UID (shouldn't reach here, but safety check)
        if revision_uid.revision_id.raw().starts_with("pending-") {
            tracing::warn!("File inode {} has pending revision UID but no pending content", inode);
            return Ok(Vec::new());
        }

        // Skip downloading lock files - these are LibreOffice/OpenOffice temp files
        // that should not be auto-fetched by file managers
        if is_lock_file(&filename) {
            tracing::debug!("Skipping download for lock file: {}", filename);
            // Return empty content - lock files are only relevant to the app that created them
            return Ok(Vec::new());
        }

        // Check in-memory cache first (fastest)
        {
            let mut inner = self.inner.write().await;
            if let Some(content) = inner.file_cache.get(&inode).cloned() {
                tracing::trace!("Cache hit (memory) for inode {}", inode);
                // Ensure metadata size matches cached content size
                let actual_size = content.len() as u64;
                if actual_size != file_size {
                    if let Some(FsNode::File(f)) = inner.nodes.get_mut(&inode) {
                        f.size = actual_size;
                    }
                }
                return Ok(content);
            }
        }

        // Check persistent disk cache second
        if let Some(content) = self.disk_cache.get(&revision_uid) {
            tracing::trace!("Cache hit (disk) for inode {}", inode);
            // Store in memory cache for faster subsequent access if small enough
            // And update the file size to match actual cached content
            {
                let mut inner = self.inner.write().await;
                inner.file_cache.put(inode, content.clone());
                // Ensure metadata size matches cached content size
                let actual_size = content.len() as u64;
                if actual_size != file_size {
                    if let Some(FsNode::File(f)) = inner.nodes.get_mut(&inode) {
                        f.size = actual_size;
                    }
                }
            }
            return Ok(content);
        }

        // Check offline index for hydrated files
        {
            let link_id = revision_uid.node_uid.link_id.raw().to_string();
            if let Ok(status) = self.index.get_offline_status(&link_id) {
                if status == OfflineStatus::Available {
                    if let Ok(Some(path)) = self.index.get_offline_content_path(&link_id) {
                        if let Ok(content) = tokio::fs::read(&path).await {
                            tracing::info!("Serving file from offline cache: {}", path);
                            // Store in memory cache for faster subsequent access
                            {
                                let mut inner = self.inner.write().await;
                                inner.file_cache.put(inode, content.clone());
                            }
                            return Ok(content);
                        } else {
                            tracing::warn!("Offline cache file missing: {}", path);
                        }
                    }
                }
            }
        }

        // Check if we're offline - if so, return error
        if !self.is_online.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(anyhow::anyhow!("File not available offline and network is unavailable"));
        }

        // Mark as downloading (for emblem display in Nautilus)
        {
            let mut inner = self.inner.write().await;
            inner.downloading_inodes.insert(inode);
        }

        // Download the file content
        let client_guard = self.client.read().await;
        let client = client_guard.as_ref()
            .ok_or_else(|| anyhow::anyhow!("No client"))?;

        tracing::info!("Downloading file content for inode {} (cache miss)", inode);

        // Create progress bar with purple background + white progress
        // Format: [purple_bg]▓▓▓▓▓▓▓▓░░░░░░░░[/] filename (downloaded/total)
        let pb = self.multi_progress.add(ProgressBar::new(file_size));
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.white.on_magenta} {msg} [{bar:30.white.on_magenta}] {bytes}/{total_bytes}")
                .unwrap()
                .progress_chars("█▓░")
        );
        pb.set_message(format!("↓ {}", filename));
        pb.enable_steady_tick(Duration::from_millis(100));

        // Create a file downloader
        tracing::debug!("Creating file downloader for revision_uid={}", revision_uid);
        let downloader = client.get_file_downloader(revision_uid.clone()).await
            .context("Failed to create file downloader")?;
        tracing::debug!("File downloader created successfully");

        // Download to a shared buffer using Arc<Mutex<Vec<u8>>>
        let buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
        let buffer_clone = buffer.clone();
        
        struct SharedWriter {
            buffer: Arc<std::sync::Mutex<Vec<u8>>>,
        }
        
        impl std::io::Write for SharedWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                let mut guard = self.buffer.lock().unwrap();
                guard.extend_from_slice(buf);
                Ok(buf.len())
            }
            
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        
        let pb_clone = pb.clone();
        let writer = Box::new(SharedWriter { buffer: buffer_clone });
        let controller = downloader.download_to_stream(
            writer,
            Box::new(move |bytes_written, _total_bytes| {
                pb_clone.set_position(bytes_written as u64);
            }),
        );

        // Release the client lock during download
        drop(client_guard);
        
        // Wait for download to complete
        let download_result = controller.completion.await;
        match &download_result {
            Ok(Ok(())) => {},
            Ok(Err(e)) => {
                let error_msg = e.to_string();
                let (error_type, style_fn) = classify_download_error(&error_msg);
                tracing::error!("Download failed for inode {}: {:?}", inode, e);
                pb.abandon_with_message(format!("{} {} ({})", style_fn, filename, error_type));
                // Clear downloading flag on error
                let mut inner = self.inner.write().await;
                inner.downloading_inodes.remove(&inode);
            }
            Err(e) => {
                tracing::error!("Download task panicked for inode {}: {:?}", inode, e);
                pb.abandon_with_message(format!("{} {} (task panic)", style("✗").red(), filename));
                // Clear downloading flag on panic
                let mut inner = self.inner.write().await;
                inner.downloading_inodes.remove(&inode);
            }
        }
        download_result
            .context("Download task panicked")?
            .context("Download failed")?;

        // Clear downloading flag on success
        {
            let mut inner = self.inner.write().await;
            inner.downloading_inodes.remove(&inode);
        }

        // Finish progress bar with completion message
        pb.finish_with_message(format!("{} {}", style("✓").green(), filename));

        // Extract the content
        let content = Arc::try_unwrap(buffer)
            .map_err(|_| anyhow::anyhow!("Buffer still has references"))?
            .into_inner()
            .unwrap();

        let actual_size = content.len();
        tracing::info!("Downloaded {} bytes for inode {} (claimed: {})", actual_size, inode, file_size);

        // Update the file size in metadata to match actual downloaded size
        // This is critical: claimed_size might differ from actual decrypted size
        if actual_size as u64 != file_size {
            tracing::warn!(
                "Size mismatch for inode {}: claimed={}, actual={}. Updating metadata.",
                inode, file_size, actual_size
            );
            let mut inner = self.inner.write().await;
            if let Some(FsNode::File(f)) = inner.nodes.get_mut(&inode) {
                f.size = actual_size as u64;
            }
            drop(inner);
        }

        // Store in persistent disk cache
        if let Err(e) = self.disk_cache.put(&revision_uid, &content) {
            tracing::warn!("Failed to write to disk cache: {:?}", e);
        }

        // Store in memory cache if small enough
        {
            let mut inner = self.inner.write().await;
            inner.file_cache.put(inode, content.clone());
        }

        Ok(content)
    }

    /// Get the NodeUid for a given inode.
    async fn get_node_uid(&self, inode: u64) -> Option<NodeUid> {
        let inner = self.inner.read().await;
        inner.nodes.get(&inode).map(|n| n.uid().clone())
    }

    /// Get the parent folder's NodeUid for a given inode.
    #[allow(dead_code)]
    async fn get_parent_uid(&self, inode: u64) -> Option<NodeUid> {
        let inner = self.inner.read().await;
        match inner.nodes.get(&inode) {
            Some(FsNode::File(f)) => f.parent_uid.clone(),
            Some(FsNode::Folder(f)) => f.parent_uid.clone(),
            Some(FsNode::Degraded(d)) => d.parent_uid.clone(),
            None => None,
        }
    }

    /// Remove a node from the filesystem state.
    async fn remove_node(&self, parent_inode: u64, inode: u64) {
        let mut inner = self.inner.write().await;
        
        // Remove from nodes map
        if let Some(node) = inner.nodes.remove(&inode) {
            // Remove from uid_to_inode
            inner.uid_to_inode.remove(&node.uid().to_string());
        }
        
        // Remove from parent's children list
        if let Some(children) = inner.children.get_mut(&parent_inode) {
            children.retain(|&c| c != inode);
        }
        
        // Remove from file cache
        inner.file_cache.remove(&inode);
        
        // Remove pending files if any
        inner.pending_files.remove(&inode);
    }

    /// Upload a pending file to Proton Drive.
    async fn upload_pending_file(&self, inode: u64) -> Result<NodeUid> {
        let pending = {
            let inner = self.inner.read().await;
            inner.pending_files.get(&inode).cloned()
                .ok_or_else(|| anyhow::anyhow!("No pending file for inode {}", inode))?
        };

        // Don't upload local-only files (e.g., lock files)
        if pending.local_only {
            tracing::debug!("Skipping upload for local-only file: {}", pending.name);
            // Return a fake NodeUid for local-only files
            let inner = self.inner.read().await;
            let volume_id = inner.volume_id.clone().unwrap_or_else(|| VolumeId::new("local".to_string()));
            return Ok(NodeUid::new(volume_id, LinkId::new(format!("local-{}", inode))));
        }

        let client_guard = self.client.read().await;
        let client = client_guard.as_ref()
            .ok_or_else(|| anyhow::anyhow!("No client"))?;

        tracing::info!("Uploading pending file '{}' ({} bytes)", pending.name, pending.content.len());

        // Create progress bar
        let pb = self.multi_progress.add(ProgressBar::new(pending.content.len() as u64));
        pb.set_style(
            ProgressStyle::default_bar()
                .template("{spinner:.white.on_blue} {msg} [{bar:30.white.on_blue}] {bytes}/{total_bytes}")
                .unwrap()
                .progress_chars("█▓░")
        );
        pb.set_message(format!("↑ {}", pending.name));
        pb.enable_steady_tick(Duration::from_millis(100));

        let size = pending.content.len() as i64;

        // Get file uploader
        let uploader = client.get_file_uploader(
            pending.parent_uid.clone(),
            pending.name.clone(),
            pending.mime_type.clone(),
            size,
            Some(std::time::SystemTime::now()),
            None,
            None,
            true, // override_existing_draft_by_other_client
        ).await?;

        // Upload content
        let pb_clone = pb.clone();
        let node_uid = uploader.upload_from_stream(
            Box::new(std::io::Cursor::new(pending.content.clone())),
            Vec::new(), // No thumbnails for now
            Box::new(move |bytes, _total| {
                pb_clone.set_position(bytes as u64);
            }),
        ).await?;

        pb.finish_with_message(format!("{} {}", style("✓").green(), pending.name));

        // Fetch the uploaded node to get full metadata
        let potential_node = client.get_node(node_uid.clone()).await?;
        
        // Update Index first (Index → FUSE flow)
        // The Index event handler will update FUSE state
        let parent_link_id = pending.parent_uid.link_id.raw().to_string();
        let indexed = indexed_node_from_potential(&potential_node, &parent_link_id);
        
        if let Err(e) = self.index.upsert_node(&indexed) {
            tracing::warn!("Failed to upsert uploaded file to index: {}", e);
        }
        
        // CRITICAL: Update FUSE state directly after upload
        // The index event handler can't find this node because the link_id changed
        // from "pending-*" to the real one. We must update the mappings ourselves.
        let mut inner = self.inner.write().await;
        
        // Get the old pending link_id before we replace it
        let old_link_id = inner.nodes.get(&inode)
            .map(|n| n.uid().link_id.raw().to_string());
        
        // Build new FsNode with real metadata
        let new_fs_node = fs_node_from_indexed(&indexed);
        let new_link_id = node_uid.link_id.raw().to_string();
        let new_uid_str = node_uid.to_string();
        
        // Update node with real metadata
        inner.nodes.insert(inode, new_fs_node);
        
        // Update link_id_to_inode: remove old pending mapping, add real one
        if let Some(old_id) = old_link_id {
            if old_id != new_link_id {
                inner.link_id_to_inode.remove(&old_id);
                tracing::debug!("Replaced pending link_id '{}' with real '{}'", old_id, new_link_id);
            }
        }
        inner.link_id_to_inode.insert(new_link_id, inode);
        
        // Update uid_to_inode mapping
        inner.uid_to_inode.insert(new_uid_str, inode);
        
        // Clean up pending state and cache content
        inner.pending_files.remove(&inode);
        inner.file_cache.put(inode, pending.content);

        Ok(node_uid)
    }

    /// Upload the write buffer as a new revision of an existing file.
    async fn upload_write_buffer(&self, fh: u64) -> Result<()> {
        let (inode, content, is_new) = {
            let inner = self.inner.read().await;
            match inner.write_buffers.get(&fh) {
                Some(buf) if buf.dirty => (buf.inode, buf.content.clone(), buf.is_new),
                _ => return Ok(()), // Not dirty, nothing to do
            }
        };
        
        // Check if background upload is already in progress for this inode
        // If so, skip sync upload to avoid race condition (2511 errors)
        {
            let inner = self.inner.read().await;
            if inner.pending_revision_uploads.contains(&inode) {
                tracing::debug!("Background upload already pending for inode {}, skipping sync upload", inode);
                return Ok(()); // Background will handle it
            }
        }

        if is_new {
            // This is a new file, use the pending file upload
            self.upload_pending_file(inode).await?;
        } else {
            // This is an update to an existing file - upload as new revision
            let revision_uid = self.get_revision_uid(inode).await
                .ok_or_else(|| anyhow::anyhow!("No revision UID for inode {}", inode))?;

            let client_guard = self.client.read().await;
            let client = client_guard.as_ref()
                .ok_or_else(|| anyhow::anyhow!("No client"))?;

            let filename = {
                let inner = self.inner.read().await;
                inner.nodes.get(&inode).map(|n| n.name().to_string()).unwrap_or_default()
            };

            tracing::info!("Uploading new revision for '{}' ({} bytes)", filename, content.len());

            // Create progress bar
            let pb = self.multi_progress.add(ProgressBar::new(content.len() as u64));
            pb.set_style(
                ProgressStyle::default_bar()
                    .template("{spinner:.white.on_cyan} {msg} [{bar:30.white.on_cyan}] {bytes}/{total_bytes}")
                    .unwrap()
                    .progress_chars("█▓░")
            );
            pb.set_message(format!("↑ {}", filename));
            pb.enable_steady_tick(Duration::from_millis(100));

            let size = content.len() as i64;

            let uploader = client.get_file_revision_uploader(
                revision_uid,
                size,
                Some(std::time::SystemTime::now()),
                None,
                None,
            ).await?;

            let pb_clone = pb.clone();
            let new_node_uid = uploader.upload_from_stream(
                Box::new(std::io::Cursor::new(content.clone())),
                Vec::new(),
                Box::new(move |bytes, _total| {
                    pb_clone.set_position(bytes as u64);
                }),
            ).await?;

            pb.finish_with_message(format!("{} {}", style("✓").green(), filename));

            // Update Index first (Index → FUSE flow)
            // The Index event handler will update FUSE state
            let potential_node = client.get_node(new_node_uid.clone()).await?;
            
            // Get parent_link_id from existing index entry
            let link_id = new_node_uid.link_id.raw();
            let parent_link_id = self.index.get_node(link_id)
                .ok()
                .flatten()
                .and_then(|n| n.parent_link_id)
                .unwrap_or_default();
            
            let indexed = indexed_node_from_potential(&potential_node, &parent_link_id);
            
            if let Err(e) = self.index.upsert_node(&indexed) {
                tracing::warn!("Failed to upsert sync revision to index: {}", e);
            }
            
            // Cache the content
            let mut inner = self.inner.write().await;
            inner.file_cache.put(inode, content);
        }

        // Mark buffer as clean
        {
            let mut inner = self.inner.write().await;
            if let Some(buf) = inner.write_buffers.get_mut(&fh) {
                buf.dirty = false;
            }
        }

        Ok(())
    }

    /// Queue a write buffer for background upload (non-blocking).
    /// Returns immediately after queuing, upload happens asynchronously.
    async fn queue_write_buffer(&self, fh: u64) -> Result<()> {
        let (inode, content, is_new) = {
            let inner = self.inner.read().await;
            match inner.write_buffers.get(&fh) {
                Some(buf) if buf.dirty => (buf.inode, buf.content.clone(), buf.is_new),
                _ => return Ok(()), // Not dirty, nothing to do
            }
        };

        // Mark buffer as clean immediately (data is now "committed" to upload queue)
        {
            let mut inner = self.inner.write().await;
            if let Some(buf) = inner.write_buffers.get_mut(&fh) {
                buf.dirty = false;
            }
            // Also update the cache so subsequent reads see the new content
            inner.file_cache.put(inode, content.clone());
        }

        if is_new {
            // Queue upload of new file
            let pending = {
                let inner = self.inner.read().await;
                inner.pending_files.get(&inode).cloned()
            };
            
            if let Some(pending) = pending {
                // Skip upload for local-only files (e.g., lock files)
                if pending.local_only {
                    tracing::debug!("Skipping upload for local-only file: {}", pending.name);
                } else {
                    // Persist the upload for resume capability
                    let persist_id = PendingUploadStore::generate_id();
                    let persist_upload = PersistentUpload::NewFile {
                        id: persist_id.clone(),
                        parent_uid: pending.parent_uid.clone(),
                        name: pending.name.clone(),
                        mime_type: pending.mime_type.clone(),
                        content: pending.content.clone(),
                        retry_count: 0,
                        created_at: chrono::Utc::now().timestamp(),
                    };
                    if let Err(e) = self.pending_upload_store.save(&persist_upload) {
                        tracing::warn!("Failed to persist upload: {}", e);
                    }
                    
                    let _ = self.upload_tx.send(UploadTask::NewFile { 
                        inode, 
                        pending,
                        persist_id: Some(persist_id),
                    });
                    tracing::info!("Queued new file upload for inode {}", inode);
                }
            }
        } else {
            // Queue revision upload
            // Check if there's already a pending upload for this inode
            let already_pending = {
                let inner = self.inner.read().await;
                inner.pending_revision_uploads.contains(&inode)
            };
            
            if already_pending {
                // Another upload is already queued for this inode.
                // The worker will read fresh content from file_cache when it runs.
                tracing::debug!("Revision upload already pending for inode {}, skipping duplicate queue", inode);
                return Ok(());
            }
            
            let revision_uid = self.get_revision_uid(inode).await;
            let filename = {
                let mut inner = self.inner.write().await;
                // Mark as pending before getting filename
                inner.pending_revision_uploads.insert(inode);
                inner.nodes.get(&inode).map(|n| n.name().to_string()).unwrap_or_default()
            };
            
            if let Some(revision_uid) = revision_uid {
                // Persist the upload for resume capability
                let persist_id = PendingUploadStore::generate_id();
                let persist_upload = PersistentUpload::NewRevision {
                    id: persist_id.clone(),
                    revision_uid: revision_uid.clone(),
                    filename: filename.clone(),
                    content: content.clone(),
                    retry_count: 0,
                    created_at: chrono::Utc::now().timestamp(),
                };
                if let Err(e) = self.pending_upload_store.save(&persist_upload) {
                    tracing::warn!("Failed to persist upload: {}", e);
                }
                
                let _ = self.upload_tx.send(UploadTask::NewRevision {
                    inode,
                    revision_uid,
                    filename: filename.clone(),
                    content,
                    persist_id: Some(persist_id),
                });
                tracing::info!("Queued revision upload for '{}' (inode {})", filename, inode);
            } else {
                // No revision_uid - remove from pending set
                let mut inner = self.inner.write().await;
                inner.pending_revision_uploads.remove(&inode);
            }
        }

        Ok(())
    }

    /// Check if a file/folder name should be ignored based on .pdclignore patterns.
    /// Checks both the global .pdclignore and any local .pdclignore in the parent directory.
    async fn is_ignored(&self, parent_inode: u64, name: &str) -> bool {
        let ignore_mgr = self.ignore_manager.read().await;
        
        // Check global patterns first
        if ignore_mgr.is_ignored_global(name) {
            tracing::info!("File '{}' matches global .pdclignore pattern", name);
            return true;
        }

        // Check for local .pdclignore in the parent directory
        // First, look for a .pdclignore child in the parent
        let local_content = {
            let inner = self.inner.read().await;
            if let Some(children) = inner.children.get(&parent_inode) {
                let mut found_content = None;
                for &child_inode in children {
                    if let Some(node) = inner.nodes.get(&child_inode) {
                        if node.name() == PDCLIGNORE_FILENAME {
                            // Found a .pdclignore file, try to get its content
                            if let Some(cached) = inner.file_cache.peek(&child_inode) {
                                found_content = Some(String::from_utf8_lossy(cached).to_string());
                            }
                            break;
                        }
                    }
                }
                found_content
            } else {
                None
            }
        };

        // If we didn't find cached content, try to download it
        let local_ignore_content = if local_content.is_none() {
            // Try to find and download the .pdclignore
            let pdclignore_inode = self.find_child_by_name(parent_inode, PDCLIGNORE_FILENAME).await;
            if let Some(inode) = pdclignore_inode {
                // Download the file content
                if let Ok(content) = self.get_file_content(inode).await {
                    Some(String::from_utf8_lossy(&content).to_string())
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            local_content
        };

        // Check local patterns
        if let Some(ref content) = local_ignore_content {
            if let Ok(local_patterns) = IgnoreManager::compile_patterns(content) {
                if local_patterns.is_match(name) {
                    tracing::info!("File '{}' matches local .pdclignore pattern", name);
                    return true;
                }
            }
        }

        false
    }

    /// Find a child node by name and return its inode.
    async fn find_child_by_name(&self, parent_inode: u64, name: &str) -> Option<u64> {
        // Make sure children are loaded
        if self.ensure_children_loaded(parent_inode).await.is_err() {
            return None;
        }

        let inner = self.inner.read().await;
        if let Some(children) = inner.children.get(&parent_inode) {
            for &child_inode in children {
                if let Some(node) = inner.nodes.get(&child_inode) {
                    if node.name() == name {
                        return Some(child_inode);
                    }
                }
            }
        }
        None
    }

    /// Invalidate a folder's children list so it will be re-fetched.
    #[allow(dead_code)]
    async fn invalidate_folder(&self, inode: u64) {
        let mut inner = self.inner.write().await;
        inner.loaded_folders.remove(&inode);
    }
}

impl Filesystem for ProtonDriveFs {
    async fn init(&self, _req: Request) -> fuse3::Result<ReplyInit> {
        tracing::info!("Proton Drive filesystem initialized");
        Ok(ReplyInit {
            max_write: NonZeroU32::new(16 * 1024).unwrap(),
        })
    }

    async fn destroy(&self, _req: Request) {
        tracing::info!("Proton Drive filesystem destroyed");
    }

    async fn lookup(&self, req: Request, parent: u64, name: &OsStr) -> fuse3::Result<ReplyEntry> {
        tracing::info!("FUSE lookup: parent={}, name={:?}, uid={}, gid={}, pid={}", 
            parent, name, req.uid, req.gid, req.pid);
        
        let (inode, child) = self.find_child(parent, name).await
            .ok_or_else(Errno::new_not_exist)?;

        // Build base attributes
        let mut attr = child.attr(inode);
        
        // Override size with actual cached content size when available.
        // Server's claimed_size can differ from actual decrypted content size.
        if !child.is_dir() {
            let inner = self.inner.read().await;
            let claimed_size = attr.size;
            
            if let Some(pending) = inner.pending_files.get(&inode) {
                attr.size = pending.content.len() as u64;
                attr.blocks = (attr.size + 4095) / 4096;
            } else if let Some(content) = inner.file_cache.peek(&inode) {
                attr.size = content.len() as u64;
                attr.blocks = (attr.size + 4095) / 4096;
            } else if let FsNode::File(f) = &child {
                if let Some(content) = self.disk_cache.get(&f.revision_uid) {
                    attr.size = content.len() as u64;
                    attr.blocks = (attr.size + 4095) / 4096;
                }
            }
            
            if attr.size != claimed_size {
                tracing::debug!(
                    "lookup inode {}: using actual size={} instead of claimed={}",
                    inode, attr.size, claimed_size
                );
            }
        }

        Ok(ReplyEntry {
            ttl: TTL,
            attr,
            generation: 0,
        })
    }

    async fn getattr(
        &self,
        req: Request,
        inode: u64,
        fh: Option<u64>,
        flags: u32,
    ) -> fuse3::Result<ReplyAttr> {
        tracing::trace!("FUSE getattr: inode={}, fh={:?}, flags={:#x}, uid={}, gid={}, pid={}",
            inode, fh, flags, req.uid, req.gid, req.pid);
        
        let node = self.get_node(inode).await
            .ok_or_else(Errno::new_not_exist)?;

        // Build base attributes from node metadata
        let mut attr = node.attr(inode);
        
        // Override size with actual cached content size when available.
        // Server's claimed_size can differ from actual decrypted content size,
        // which causes apps like LibreOffice to report "corrupted" files.
        if !node.is_dir() {
            let inner = self.inner.read().await;
            let claimed_size = attr.size;
            
            // Check pending files first (locally created, not yet uploaded)
            if let Some(pending) = inner.pending_files.get(&inode) {
                let actual_size = pending.content.len() as u64;
                tracing::debug!(
                    "getattr inode {}: using pending file size={} (claimed={})",
                    inode, actual_size, claimed_size
                );
                attr.size = actual_size;
                attr.blocks = (actual_size + 4095) / 4096;
            }
            // Check memory cache
            else if let Some(content) = inner.file_cache.peek(&inode) {
                let actual_size = content.len() as u64;
                tracing::debug!(
                    "getattr inode {}: using memory cache size={} (claimed={})",
                    inode, actual_size, claimed_size
                );
                attr.size = actual_size;
                attr.blocks = (actual_size + 4095) / 4096;
            }
            // Check disk cache
            else if let FsNode::File(f) = &node {
                if let Some(content) = self.disk_cache.get(&f.revision_uid) {
                    let actual_size = content.len() as u64;
                    tracing::debug!(
                        "getattr inode {}: using disk cache size={} (claimed={})",
                        inode, actual_size, claimed_size
                    );
                    attr.size = actual_size;
                    attr.blocks = (actual_size + 4095) / 4096;
                } else {
                    tracing::debug!(
                        "getattr inode {}: using claimed_size={} (no cache found)",
                        inode, claimed_size
                    );
                }
            } else {
                tracing::debug!(
                    "getattr inode {}: using claimed_size={} (not a file node)",
                    inode, claimed_size
                );
            }
        }

        Ok(ReplyAttr {
            ttl: TTL,
            attr,
        })
    }

    async fn open(&self, req: Request, inode: u64, flags: u32) -> fuse3::Result<ReplyOpen> {
        tracing::info!("FUSE open: inode={}, flags={:#x}, uid={}, gid={}, pid={}",
            inode, flags, req.uid, req.gid, req.pid);
        
        let node = self.get_node(inode).await
            .ok_or_else(Errno::new_not_exist)?;

        if node.is_dir() {
            return Err(Errno::new_is_dir());
        }
        
        // Allocate file handle
        let fh = self.inner.read().await.next_fh.fetch_add(1, Ordering::SeqCst);
        
        // Check if file is being opened for writing
        let o_wronly: u32 = libc::O_WRONLY as u32;
        let o_rdwr: u32 = libc::O_RDWR as u32;
        let o_trunc: u32 = libc::O_TRUNC as u32;
        let is_write_mode = (flags & o_wronly) != 0 || (flags & o_rdwr) != 0;
        let is_truncate = (flags & o_trunc) != 0;
        
        // Get the filename for logging
        let filename = node.name();
        
        // Log process info for debugging
        let comm_path = format!("/proc/{}/comm", req.pid);
        let proc_name = std::fs::read_to_string(&comm_path)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        
        // Block known thumbnail generator processes from opening uncached files.
        // This prevents thumbnails from downloading entire files.
        // The is_cached check allows thumbnailers to read already-cached content.
        if is_thumbnailer_process(&proc_name, req.pid) && !is_write_mode {
            let is_cached = {
                let inner = self.inner.read().await;
                if inner.file_cache.contains(&inode) {
                    true
                } else if let Some(FsNode::File(f)) = inner.nodes.get(&inode) {
                    self.disk_cache.contains(&f.revision_uid)
                } else {
                    false
                }
            };
            
            if !is_cached {
                tracing::warn!(
                    "Blocking thumbnailer from uncached file: inode={}, file={}, process={} (pid {})",
                    inode, filename, proc_name, req.pid
                );
                // Return permission denied - thumbnailer will show generic icon
                return Err(Errno::from(libc::EACCES));
            }
            tracing::debug!(
                "Allowing thumbnailer to read cached file: inode={}, file={}, process={}",
                inode, filename, proc_name
            );
        }
        
        tracing::debug!(
            "Opening file: inode={}, file={}, process={} (pid {}), write_mode={}",
            inode, filename, proc_name, req.pid, is_write_mode
        );
            
        // Check if this is a pending file (created locally, not yet uploaded)
        let is_pending = {
            let inner = self.inner.read().await;
            inner.pending_files.contains_key(&inode)
        };
        
        // Set up write buffer if opened for writing.
        // For O_TRUNC we start fresh; otherwise we need existing content.
        // Don't pre-download for read-only opens - read() will fetch on-demand.
        if is_write_mode {
            let existing_content = if is_pending {
                let inner = self.inner.read().await;
                inner.pending_files.get(&inode)
                    .map(|p| p.content.clone())
                    .unwrap_or_default()
            } else if is_truncate {
                Vec::new()
            } else {
                // For write mode, we need to fetch the existing content.
                // Check cache first to avoid blocking download.
                let cache_content = {
                    let inner = self.inner.read().await;
                    inner.file_cache.peek(&inode).cloned()
                };
                
                if let Some(content) = cache_content {
                    content
                } else {
                    // Need to download - this could timeout for large files
                    // but write mode requires existing content
                    match self.get_file_content(inode).await {
                        Ok(content) => content,
                        Err(e) => {
                            tracing::error!("Failed to fetch file content for write: {:?}", e);
                            return Err(Errno::from(libc::EIO));
                        }
                    }
                }
            };
            
            let mut inner = self.inner.write().await;
            inner.write_buffers.insert(fh, WriteBuffer {
                inode,
                is_new: is_pending, // New files not yet uploaded should use NewFile upload path
                offset: 0,
                content: existing_content,
                dirty: is_truncate, // Truncated files are dirty
            });
            inner.fh_to_inode.insert(fh, inode);
            
            // If truncating, update the file size
            if is_truncate {
                if let Some(FsNode::File(f)) = inner.nodes.get_mut(&inode) {
                    f.size = 0;
                }
                inner.file_cache.put(inode, Vec::new());
            }
        }
        
        // Use FOPEN_DIRECT_IO to bypass kernel page cache.
        // This is critical because getattr returns the "claimed" size from Proton metadata,
        // which may differ from the actual downloaded file size. Without direct_io, the kernel
        // caches the wrong size and applications see truncated/corrupted documents on first open.
        Ok(ReplyOpen { fh, flags: FOPEN_DIRECT_IO })
    }

    async fn read(
        &self,
        req: Request,
        inode: u64,
        fh: u64,
        offset: u64,
        size: u32,
    ) -> fuse3::Result<ReplyData> {
        tracing::trace!("FUSE read: inode={}, fh={}, offset={}, size={}, uid={}, gid={}, pid={}",
            inode, fh, offset, size, req.uid, req.gid, req.pid);
        
        // First check if this is a pending file (created locally, not yet uploaded)
        {
            let inner = self.inner.read().await;
            if let Some(pending) = inner.pending_files.get(&inode) {
                let content = &pending.content;
                let offset = offset as usize;
                let size = size as usize;
                if offset >= content.len() {
                    return Ok(ReplyData { data: Bytes::new() });
                }
                let end = std::cmp::min(offset + size, content.len());
                tracing::debug!("Returning {} bytes from pending file (inode={})", end - offset, inode);
                return Ok(ReplyData { data: Bytes::copy_from_slice(&content[offset..end]) });
            }
        }
        
        // Normal read - download file content (cached)
        // Always serve content for any read request to ensure MIME detection works correctly
        let content = match self.get_file_content(inode).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to download file content: {}", e);
                return Err(Errno::from(libc::EIO));
            }
        };

        // Return the requested slice
        let offset = offset as usize;
        let size = size as usize;
        
        if offset >= content.len() {
            return Ok(ReplyData { data: Bytes::new() });
        }
        
        let end = std::cmp::min(offset + size, content.len());
        let data = Bytes::copy_from_slice(&content[offset..end]);
        
        Ok(ReplyData { data })
    }

    async fn readdir(
        &self,
        req: Request,
        inode: u64,
        fh: u64,
        offset: i64,
    ) -> fuse3::Result<ReplyDirectory<impl Stream<Item = fuse3::Result<DirectoryEntry>> + Send + '_>> {
        tracing::info!("FUSE readdir: inode={}, fh={}, offset={}, uid={}, gid={}, pid={}",
            inode, fh, offset, req.uid, req.gid, req.pid);
        
        // Ensure children are loaded
        self.ensure_children_loaded(inode).await
            .map_err(|e| {
                tracing::error!("Failed to load children: {}", e);
                Errno::new_not_exist()
            })?;
        
        let inner = self.inner.read().await;
        
        let node = inner.nodes.get(&inode)
            .ok_or_else(Errno::new_not_exist)?;

        if !node.is_dir() {
            return Err(Errno::new_is_not_dir());
        }

        // Get parent inode (for ".." entry)
        let parent_inode = match node {
            FsNode::Folder(f) => {
                if let Some(ref parent_uid) = f.parent_uid {
                    inner.uid_to_inode.get(&parent_uid.to_string()).copied().unwrap_or(ROOT_INODE)
                } else {
                    ROOT_INODE
                }
            }
            _ => ROOT_INODE,
        };

        let mut entries = vec![
            Ok(DirectoryEntry {
                inode,
                kind: FileType::Directory,
                name: OsString::from("."),
                offset: 1,
            }),
            Ok(DirectoryEntry {
                inode: parent_inode,
                kind: FileType::Directory,
                name: OsString::from(".."),
                offset: 2,
            }),
        ];

        if let Some(children) = inner.children.get(&inode) {
            for (i, &child_inode) in children.iter().enumerate() {
                // Validate inode - must be non-zero
                if child_inode == 0 {
                    continue;
                }
                if let Some(child) = inner.nodes.get(&child_inode) {
                    let child_name = child.name();
                    // Validate the name - FUSE doesn't allow empty names, NUL bytes, or slashes
                    if child_name.is_empty() || child_name.contains('\0') || child_name.contains('/') {
                        continue;
                    }
                    entries.push(Ok(DirectoryEntry {
                        inode: child_inode,
                        kind: child.file_type(),
                        name: OsString::from(child_name),
                        offset: (i + 3) as i64,
                    }));
                }
            }
        }

        Ok(ReplyDirectory {
            entries: stream::iter(entries.into_iter().skip(offset.max(0) as usize)),
        })
    }

    async fn readdirplus(
        &self,
        req: Request,
        parent: u64,
        fh: u64,
        offset: u64,
        lock_owner: u64,
    ) -> fuse3::Result<ReplyDirectoryPlus<impl Stream<Item = fuse3::Result<DirectoryEntryPlus>> + Send + '_>> {
        tracing::info!("FUSE readdirplus: parent={}, fh={}, offset={}, lock_owner={}, uid={}, gid={}, pid={}",
            parent, fh, offset, lock_owner, req.uid, req.gid, req.pid);
        
        // Ensure children are loaded
        if let Err(e) = self.ensure_children_loaded(parent).await {
            tracing::error!("Failed to load children for inode {}: {}", parent, e);
            return Err(Errno::new_not_exist());
        }
        tracing::debug!("Children loaded for inode {}", parent);
        
        let inner = self.inner.read().await;
        
        let node = match inner.nodes.get(&parent) {
            Some(n) => n,
            None => {
                tracing::error!("Node {} not found in readdirplus", parent);
                return Err(Errno::new_not_exist());
            }
        };

        if !node.is_dir() {
            tracing::error!("Node {} is not a directory in readdirplus", parent);
            return Err(Errno::new_is_not_dir());
        }

        let parent_attr = node.attr(parent);

        // Get parent's parent inode
        let grandparent_inode = match node {
            FsNode::Folder(f) => {
                if let Some(ref parent_uid) = f.parent_uid {
                    inner.uid_to_inode.get(&parent_uid.to_string()).copied().unwrap_or(ROOT_INODE)
                } else {
                    ROOT_INODE
                }
            }
            _ => ROOT_INODE,
        };
        
        let grandparent_attr = inner.nodes.get(&grandparent_inode)
            .map(|n| n.attr(grandparent_inode))
            .unwrap_or(parent_attr);

        let mut entries: Vec<fuse3::Result<DirectoryEntryPlus>> = vec![
            Ok(DirectoryEntryPlus {
                inode: parent,
                generation: 0,
                kind: FileType::Directory,
                name: OsString::from("."),
                offset: 1,
                attr: parent_attr,
                entry_ttl: TTL,
                attr_ttl: TTL,
            }),
            Ok(DirectoryEntryPlus {
                inode: grandparent_inode,
                generation: 0,
                kind: FileType::Directory,
                name: OsString::from(".."),
                offset: 2,
                attr: grandparent_attr,
                entry_ttl: TTL,
                attr_ttl: TTL,
            }),
        ];

        if let Some(children) = inner.children.get(&parent) {
            tracing::debug!("Building entries for {} children", children.len());
            for (i, &child_inode) in children.iter().enumerate() {
                // Validate inode - must be non-zero
                if child_inode == 0 {
                    tracing::error!("Child has invalid inode 0, skipping");
                    continue;
                }
                
                if let Some(child) = inner.nodes.get(&child_inode) {
                    let child_name = child.name();
                    // Validate the name - FUSE doesn't allow empty names or names with NUL bytes
                    if child_name.is_empty() {
                        tracing::warn!("Skipping child {} with empty name", child_inode);
                        continue;
                    }
                    if child_name.contains('\0') {
                        tracing::warn!("Skipping child {} with NUL byte in name: {:?}", child_inode, child_name);
                        continue;
                    }
                    // FUSE names must not contain '/'
                    if child_name.contains('/') {
                        tracing::warn!("Skipping child {} with slash in name: {:?}", child_inode, child_name);
                        continue;
                    }
                    
                    entries.push(Ok(DirectoryEntryPlus {
                        inode: child_inode,
                        generation: 0,
                        kind: child.file_type(),
                        name: OsString::from(child_name),
                        offset: (i + 3) as i64,
                        attr: child.attr(child_inode),
                        entry_ttl: TTL,
                        attr_ttl: TTL,
                    }));
                } else {
                    tracing::warn!("Child inode {} not found in nodes", child_inode);
                }
            }
        } else {
            tracing::debug!("No children for inode {}", parent);
        }
        
        // Release the lock before returning
        drop(inner);
        
        let num_entries = entries.len();
        let skip = offset as usize;
        tracing::debug!("Returning {} entries (skipping {})", num_entries.saturating_sub(skip), skip);

        Ok(ReplyDirectoryPlus {
            entries: stream::iter(entries.into_iter().skip(offset as usize)),
        })
    }

    async fn access(&self, req: Request, inode: u64, mask: u32) -> fuse3::Result<()> {
        tracing::trace!("FUSE access: inode={}, mask={:#o}, uid={}, gid={}, pid={}",
            inode, mask, req.uid, req.gid, req.pid);
        let _ = self.get_node(inode).await
            .ok_or_else(Errno::new_not_exist)?;
        Ok(())
    }

    async fn statfs(&self, req: Request, inode: u64) -> fuse3::Result<ReplyStatFs> {
        tracing::info!("FUSE statfs: inode={}, uid={}, gid={}, pid={}",
            inode, req.uid, req.gid, req.pid);
        
        // Get actual storage info from Proton Drive API
        let (used_bytes, max_bytes) = {
            let client = self.client.read().await;
            
            if let Some(client) = client.as_ref() {
                match client.get_user_storage_info().await {
                    Ok((used, max)) => (used as u64, max as u64),
                    Err(e) => {
                        tracing::warn!("Failed to get storage stats: {}", e);
                        (0, 15 * 1024 * 1024 * 1024u64) // Default 15GB
                    }
                }
            } else {
                (0, 15 * 1024 * 1024 * 1024u64) // Default values
            }
        };
        
        let block_size = 4096u64;
        let total_blocks = max_bytes / block_size;
        let used_blocks = used_bytes / block_size;
        let free_blocks = total_blocks.saturating_sub(used_blocks);
        
        Ok(ReplyStatFs {
            blocks: total_blocks,
            bfree: free_blocks,
            bavail: free_blocks,
            files: 1_000_000,
            ffree: 500_000,
            bsize: block_size as u32,
            namelen: 255,
            frsize: block_size as u32,
        })
    }

    async fn getxattr(
        &self,
        req: Request,
        inode: u64,
        name: &OsStr,
        size: u32,
    ) -> fuse3::Result<ReplyXAttr> {
        let name_str = name.to_string_lossy();
        tracing::debug!("FUSE getxattr: inode={}, name={:?}, size={}, uid={}, gid={}, pid={}",
            inode, name, size, req.uid, req.gid, req.pid);

        // Check if this file is currently downloading - expose "synchronizing" emblem
        // Nautilus reads user.metadata::emblems for file emblems
        if name_str == "user.metadata::emblems" {
            let inner = self.inner.read().await;
            if inner.downloading_inodes.contains(&inode) {
                // Return "emblem-synchronizing" emblem (sync icon)
                // Nautilus expects a null-terminated list of emblem names
                let emblem = b"emblem-synchronizing\0";
                if size == 0 {
                    return Ok(ReplyXAttr::Size(emblem.len() as u32));
                }
                return Ok(ReplyXAttr::Data(Bytes::from_static(emblem)));
            }
            // Check if file is pending upload
            if inner.pending_files.contains_key(&inode) {
                let emblem = b"emblem-synchronizing\0";
                if size == 0 {
                    return Ok(ReplyXAttr::Size(emblem.len() as u32));
                }
                return Ok(ReplyXAttr::Data(Bytes::from_static(emblem)));
            }
        }

        // No extended attributes for this file
        Err(Errno::from(libc::ENODATA))
    }

    async fn listxattr(
        &self,
        req: Request,
        inode: u64,
        size: u32,
    ) -> fuse3::Result<ReplyXAttr> {
        tracing::debug!("FUSE listxattr: inode={}, size={}, uid={}, gid={}, pid={}",
            inode, size, req.uid, req.gid, req.pid);

        // Verify the inode exists and check if it's downloading or pending
        let inner = self.inner.read().await;
        if !inner.nodes.contains_key(&inode) {
            return Err(Errno::new_not_exist());
        }

        // If downloading or pending upload, list the emblems xattr
        if inner.downloading_inodes.contains(&inode) || inner.pending_files.contains_key(&inode) {
            // List available xattrs - null-terminated names
            let xattrs = b"user.metadata::emblems\0";
            drop(inner);
            if size == 0 {
                return Ok(ReplyXAttr::Size(xattrs.len() as u32));
            }
            return Ok(ReplyXAttr::Data(Bytes::from_static(xattrs)));
        }
        drop(inner);

        // Return empty list - no extended attributes
        if size == 0 {
            Ok(ReplyXAttr::Size(0))
        } else {
            Ok(ReplyXAttr::Data(Bytes::new()))
        }
    }

    async fn release(
        &self,
        req: Request,
        inode: u64,
        fh: u64,
        flags: u32,
        _lock_owner: u64,
        flush: bool,
    ) -> fuse3::Result<()> {
        tracing::debug!("FUSE release: inode={}, fh={}, flags={:#x}, flush={}, uid={}, gid={}, pid={}",
            inode, fh, flags, flush, req.uid, req.gid, req.pid);
        
        // Queue any pending writes for background upload (non-blocking)
        if let Err(e) = self.queue_write_buffer(fh).await {
            tracing::error!("Failed to queue writes on release: {}", e);
            // Don't return error - release must succeed
        }
        
        // Clean up state
        {
            let mut inner = self.inner.write().await;
            inner.write_buffers.remove(&fh);
            inner.fh_to_inode.remove(&fh);
        }
        
        Ok(())
    }

    async fn mkdir(
        &self,
        req: Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
    ) -> fuse3::Result<ReplyEntry> {
        let name_str = name.to_string_lossy().to_string();
        tracing::info!("FUSE mkdir: parent={}, name={}, uid={}, gid={}, pid={}",
            parent, name_str, req.uid, req.gid, req.pid);

        // Check if the folder name should be ignored
        if self.is_ignored(parent, &name_str).await {
            tracing::warn!("Folder '{}' matches .pdclignore pattern, rejecting creation", name_str);
            return Err(Errno::from(libc::EPERM));
        }

        // Get parent folder UID
        let parent_uid = self.get_node_uid(parent).await
            .ok_or_else(|| {
                tracing::error!("Parent inode {} not found", parent);
                Errno::new_not_exist()
            })?;

        // Check parent is a folder
        let parent_node = self.get_node(parent).await
            .ok_or_else(Errno::new_not_exist)?;
        if !parent_node.is_dir() {
            return Err(Errno::new_is_not_dir());
        }

        // Create the folder via SDK
        let client_guard = self.client.read().await;
        let client = client_guard.as_ref()
            .ok_or_else(|| {
                tracing::error!("No client available");
                Errno::from(libc::EIO)
            })?;

        let folder_node = client.create_folder(
            parent_uid,
            name_str.clone(),
            Some(std::time::SystemTime::now()),
        ).await
            .map_err(|e| {
                tracing::error!("Failed to create folder '{}': {}", name_str, e);
                Errno::from(libc::EIO)
            })?;

        // Add to filesystem state
        let fs_node = FsNode::Folder(ProtonFolderMetadata::from_folder_node(&folder_node, false));
        let inode = {
            let mut inner = self.inner.write().await;
            let inode = inner.get_or_create_inode(&folder_node.base.uid);
            inner.nodes.insert(inode, fs_node.clone());
            inner.children.entry(parent).or_default().push(inode);
            inner.children.insert(inode, Vec::new()); // Empty children list
            inode
        };

        tracing::info!("Created folder '{}' with inode {}", name_str, inode);

        Ok(ReplyEntry {
            ttl: TTL,
            attr: fs_node.attr(inode),
            generation: 0,
        })
    }

    async fn create(
        &self,
        req: Request,
        parent: u64,
        name: &OsStr,
        _mode: u32,
        flags: u32,
    ) -> fuse3::Result<ReplyCreated> {
        let name_str = name.to_string_lossy().to_string();
        tracing::info!("FUSE create: parent={}, name={}, flags={:#x}, uid={}, gid={}, pid={}",
            parent, name_str, flags, req.uid, req.gid, req.pid);

        // Check if this is a lock file - allow it locally but don't upload
        let is_local_only = is_lock_file(&name_str);
        
        // Check if the file name should be ignored (but allow lock files locally)
        if !is_local_only && self.is_ignored(parent, &name_str).await {
            tracing::warn!("File '{}' matches .pdclignore pattern, rejecting creation", name_str);
            return Err(Errno::from(libc::EPERM));
        }
        
        if is_local_only {
            tracing::debug!("Creating local-only file (lock file): {}", name_str);
        }

        // Get parent folder UID
        let parent_uid = self.get_node_uid(parent).await
            .ok_or_else(|| {
                tracing::error!("Parent inode {} not found", parent);
                Errno::new_not_exist()
            })?;

        // Check parent is a folder
        let parent_node = self.get_node(parent).await
            .ok_or_else(Errno::new_not_exist)?;
        if !parent_node.is_dir() {
            return Err(Errno::new_is_not_dir());
        }

        // Determine MIME type from filename
        let mime_type = mime_guess::from_path(&name_str)
            .first_or_octet_stream()
            .to_string();

        // Create a pending file entry (will be uploaded on first write or release)
        let now = Utc::now();
        let pending = PendingFile {
            parent_inode: parent,
            parent_uid,
            name: name_str.clone(),
            mime_type,
            content: Vec::new(),
            creation_time: now,
            dirty: false,
            local_only: is_local_only,
        };

        // Allocate inode and file handle
        let (inode, fh) = {
            let mut inner = self.inner.write().await;
            let inode = inner.alloc_inode();
            let fh = inner.next_fh.fetch_add(1, Ordering::SeqCst);
            
            // Create a temporary file node for getattr calls
            let file_meta = ProtonFileMetadata {
                uid: NodeUid::new(
                    inner.volume_id.clone().unwrap_or_else(|| VolumeId::new("pending".to_string())),
                    LinkId::new(format!("pending-{}", inode)),
                ),
                parent_uid: Some(pending.parent_uid.clone()),
                name: name_str.clone(),
                mime_type: pending.mime_type.clone(),
                size: 0,
                size_on_cloud: 0,
                creation_time: now,
                modification_time: Some(now),
                trash_time: None,
                author_email: None,
                name_author_email: None,
                owner_email: None,
                owner_organisation: None,
                revision_uid: RevisionUid::new(
                    NodeUid::new(
                        inner.volume_id.clone().unwrap_or_else(|| VolumeId::new("pending".to_string())),
                        LinkId::new(format!("pending-{}", inode)),
                    ),
                    RevisionId::new("pending".to_string()),
                ),
                revision_creation_time: now,
                content_sha1: None,
                is_photo: false,
                capture_time: None,
                thumbnail_id: None, // New files don't have thumbnails yet
            };
            
            let fs_node = FsNode::File(file_meta);
            inner.nodes.insert(inode, fs_node);
            inner.children.entry(parent).or_default().push(inode);
            inner.pending_files.insert(inode, pending);
            inner.fh_to_inode.insert(fh, inode);
            
            // Create write buffer
            inner.write_buffers.insert(fh, WriteBuffer {
                inode,
                is_new: true,
                offset: 0,
                content: Vec::new(),
                dirty: false,
            });
            
            (inode, fh)
        };

        let attr = {
            let inner = self.inner.read().await;
            inner.nodes.get(&inode).map(|n| n.attr(inode))
                .unwrap_or_else(|| FileAttr {
                    ino: inode,
                    size: 0,
                    blocks: 0,
                    atime: std::time::SystemTime::now().into(),
                    mtime: std::time::SystemTime::now().into(),
                    ctime: std::time::SystemTime::now().into(),
                    kind: FileType::RegularFile,
                    perm: FILE_MODE,
                    nlink: 1,
                    uid: unsafe { libc::getuid() },
                    gid: unsafe { libc::getgid() },
                    rdev: 0,
                    blksize: 4096,
                })
        };

        tracing::info!("Created pending file '{}' with inode {} and fh {}", name_str, inode, fh);

        Ok(ReplyCreated {
            ttl: TTL,
            attr,
            generation: 0,
            fh,
            flags: FOPEN_DIRECT_IO,
        })
    }

    async fn write(
        &self,
        req: Request,
        inode: u64,
        fh: u64,
        offset: u64,
        data: &[u8],
        _write_flags: u32,
        _flags: u32,
    ) -> fuse3::Result<ReplyWrite> {
        tracing::debug!("FUSE write: inode={}, fh={}, offset={}, size={}, uid={}, gid={}, pid={}",
            inode, fh, offset, data.len(), req.uid, req.gid, req.pid);

        let written = data.len() as u32;

        // Update write buffer
        {
            let mut inner = self.inner.write().await;
            
            if let Some(buf) = inner.write_buffers.get_mut(&fh) {
                // Extend buffer if needed
                let end = offset as usize + data.len();
                if buf.content.len() < end {
                    buf.content.resize(end, 0);
                }
                
                // Copy data
                buf.content[offset as usize..end].copy_from_slice(data);
                buf.dirty = true;
                buf.offset = offset + data.len() as u64;
                
                // Also update pending file content if this is a new file
                if buf.is_new {
                    if let Some(pending) = inner.pending_files.get_mut(&inode) {
                        if pending.content.len() < end {
                            pending.content.resize(end, 0);
                        }
                        pending.content[offset as usize..end].copy_from_slice(data);
                        pending.dirty = true;
                    }
                }

                // Update file size in metadata
                if let Some(FsNode::File(f)) = inner.nodes.get_mut(&inode) {
                    f.size = f.size.max(end as u64);
                }
            } else {
                // No write buffer - create one (file was opened without O_WRONLY/O_RDWR)
                return Err(Errno::from(libc::EBADF));
            }
        }

        Ok(ReplyWrite { written })
    }

    async fn unlink(&self, req: Request, parent: u64, name: &OsStr) -> fuse3::Result<()> {
        let name_str = name.to_string_lossy().to_string();
        tracing::info!("FUSE unlink: parent={}, name={}, uid={}, gid={}, pid={}",
            parent, name_str, req.uid, req.gid, req.pid);

        // Find the child
        let (child_inode, child_node) = self.find_child(parent, name).await
            .ok_or_else(Errno::new_not_exist)?;

        if child_node.is_dir() {
            return Err(Errno::new_is_dir());
        }

        // Check if this is a pending file (not yet uploaded)
        {
            let inner = self.inner.read().await;
            if inner.pending_files.contains_key(&child_inode) {
                // Just remove from state, no need to call API
                drop(inner);
                self.remove_node(parent, child_inode).await;
                tracing::info!("Removed pending file '{}'", name_str);
                return Ok(());
            }
        }

        // Get node UID for API call
        let node_uid = child_node.uid().clone();

        // Show deletion spinner
        let pb = self.multi_progress.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.red} {msg}")
                .unwrap()
        );
        pb.set_message(format!("🗑 Deleting {}...", name_str));
        pb.enable_steady_tick(Duration::from_millis(80));

        // Trash the file via SDK
        let client_guard = self.client.read().await;
        let client = client_guard.as_ref()
            .ok_or_else(|| {
                pb.finish_and_clear();
                tracing::error!("No client available");
                Errno::from(libc::EIO)
            })?;

        let results = client.trash_nodes(vec![node_uid.clone()]).await
            .map_err(|e| {
                let error_str = e.to_string();
                pb.finish_with_message(format!("{} {} ({})", style("✗").red(), name_str, error_str));
                tracing::error!("Failed to trash file '{}': {}", name_str, error_str);
                // If the node is already gone on the server, still clean up local state
                if error_str.contains("not found") || error_str.contains("DoesNotExist") 
                    || error_str.contains("2501") {
                    Errno::new_not_exist()
                } else {
                    Errno::from(libc::EIO)
                }
            })?;

        // Check result
        if let Some(Err(e)) = results.get(&node_uid) {
            let error_str = e.to_string();
            tracing::error!("Failed to trash file '{}': {}", name_str, error_str);
            // If already not found, that's fine - just clean up index
            if error_str.contains("not found") || error_str.contains("DoesNotExist") 
                || error_str.contains("2501") {
                // Update index - event handler will update FUSE
                let link_id = node_uid.link_id.raw();
                if let Err(e) = self.index.delete_node(link_id) {
                    tracing::warn!("Failed to delete {} from index: {}", link_id, e);
                }
                pb.finish_with_message(format!("{} {}", style("✓").red(), name_str));
                tracing::info!("File '{}' already gone from server, updated index", name_str);
                return Ok(());
            }
            pb.finish_with_message(format!("{} {} ({})", style("✗").red(), name_str, error_str));
            return Err(Errno::from(libc::EIO));
        }

        // Server confirmed deletion - update INDEX.
        // The index will emit NodeRemoved event, which the event handler
        // will use to update FUSE state.
        let link_id = node_uid.link_id.raw();
        if let Err(e) = self.index.delete_node(link_id) {
            tracing::warn!("Failed to delete {} from index: {}", link_id, e);
        }

        pb.finish_with_message(format!("{} {}", style("✓").red(), name_str));
        tracing::info!("Trashed file '{}'", name_str);
        Ok(())
    }

    async fn rmdir(&self, req: Request, parent: u64, name: &OsStr) -> fuse3::Result<()> {
        let name_str = name.to_string_lossy().to_string();
        tracing::info!("FUSE rmdir: parent={}, name={}, uid={}, gid={}, pid={}",
            parent, name_str, req.uid, req.gid, req.pid);

        // Find the child
        let (_child_inode, child_node) = self.find_child(parent, name).await
            .ok_or_else(Errno::new_not_exist)?;

        if !child_node.is_dir() {
            return Err(Errno::new_is_not_dir());
        }

        // Get node UID for API call
        let node_uid = child_node.uid().clone();

        // Show deletion spinner
        let pb = self.multi_progress.add(ProgressBar::new_spinner());
        pb.set_style(
            ProgressStyle::default_spinner()
                .template("{spinner:.red} {msg}")
                .unwrap()
        );
        pb.set_message(format!("🗑 Deleting {}/...", name_str));
        pb.enable_steady_tick(Duration::from_millis(80));

        // Trash the folder via SDK (Proton Drive handles recursive deletion server-side)
        let client_guard = self.client.read().await;
        let client = client_guard.as_ref()
            .ok_or_else(|| {
                pb.finish_and_clear();
                tracing::error!("No client available");
                Errno::from(libc::EIO)
            })?;

        let results = client.trash_nodes(vec![node_uid.clone()]).await
            .map_err(|e| {
                let error_str = e.to_string();
                pb.finish_with_message(format!("{} {}/ ({})", style("✗").red(), name_str, error_str));
                tracing::error!("Failed to trash folder '{}': {}", name_str, error_str);
                // If the node is already gone on the server, still clean up local state
                if error_str.contains("not found") || error_str.contains("DoesNotExist") 
                    || error_str.contains("2501") {
                    Errno::new_not_exist()
                } else {
                    Errno::from(libc::EIO)
                }
            })?;

        // Check result
        if let Some(Err(e)) = results.get(&node_uid) {
            let error_str = e.to_string();
            tracing::error!("Failed to trash folder '{}': {}", name_str, error_str);
            // If already not found, that's fine - just update index
            if error_str.contains("not found") || error_str.contains("DoesNotExist") 
                || error_str.contains("2501") {
                // Update index recursively - event handler will update FUSE
                let link_id = node_uid.link_id.raw();
                if let Err(e) = self.index.delete_node_recursive(link_id) {
                    tracing::warn!("Failed to delete {} from index: {}", link_id, e);
                }
                pb.finish_with_message(format!("{} {}/", style("✓").red(), name_str));
                tracing::info!("Folder '{}' already gone from server, updated index", name_str);
                return Ok(());
            }
            pb.finish_with_message(format!("{} {}/ ({})", style("✗").red(), name_str, error_str));
            return Err(Errno::from(libc::EIO));
        }

        // Server confirmed deletion - update INDEX recursively (folder + children).
        // The index will emit NodeRemoved event, which the event handler
        // will use to update FUSE state.
        let link_id = node_uid.link_id.raw();
        if let Err(e) = self.index.delete_node_recursive(link_id) {
            tracing::warn!("Failed to delete {} from index: {}", link_id, e);
        }

        pb.finish_with_message(format!("{} {}/", style("✓").red(), name_str));
        tracing::info!("Trashed folder '{}' (with contents)", name_str);
        Ok(())
    }

    async fn rename(
        &self,
        req: Request,
        parent: u64,
        name: &OsStr,
        new_parent: u64,
        new_name: &OsStr,
    ) -> fuse3::Result<()> {
        let name_str = name.to_string_lossy().to_string();
        let new_name_str = new_name.to_string_lossy().to_string();
        tracing::info!("FUSE rename: parent={}, name={}, new_parent={}, new_name={}, uid={}, gid={}, pid={}",
            parent, name_str, new_parent, new_name_str, req.uid, req.gid, req.pid);

        // Check if the new name should be ignored (unless it's the same name)
        if name_str != new_name_str && self.is_ignored(new_parent, &new_name_str).await {
            tracing::warn!("New name '{}' matches .pdclignore pattern, rejecting rename", new_name_str);
            return Err(Errno::from(libc::EPERM));
        }

        // Find the source node
        let (src_inode, src_node) = self.find_child(parent, name).await
            .ok_or_else(Errno::new_not_exist)?;

        let node_uid = src_node.uid().clone();
        let is_same_parent = parent == new_parent;
        let is_same_name = name_str == new_name_str;

        // Get client
        let client_guard = self.client.read().await;
        let client = client_guard.as_ref()
            .ok_or_else(|| {
                tracing::error!("No client available");
                Errno::from(libc::EIO)
            })?;

        // Rename if name changed
        if !is_same_name {
            let new_media_type = if !src_node.is_dir() {
                Some(mime_guess::from_path(&new_name_str).first_or_octet_stream().to_string())
            } else {
                None
            };

            client.rename_node(node_uid.clone(), new_name_str.clone(), new_media_type).await
                .map_err(|e| {
                    tracing::error!("Failed to rename '{}' to '{}': {}", name_str, new_name_str, e);
                    Errno::from(libc::EIO)
                })?;
        }

        // Move if parent changed
        if !is_same_parent {
            let new_parent_uid = self.get_node_uid(new_parent).await
                .ok_or_else(|| {
                    tracing::error!("New parent inode {} not found", new_parent);
                    Errno::new_not_exist()
                })?;

            client.move_nodes(vec![node_uid.clone()], new_parent_uid).await
                .map_err(|e| {
                    tracing::error!("Failed to move '{}': {}", name_str, e);
                    Errno::from(libc::EIO)
                })?;
        }

        // Update filesystem state
        {
            let mut inner = self.inner.write().await;
            
            // Update node name if changed
            if !is_same_name {
                match inner.nodes.get_mut(&src_inode) {
                    Some(FsNode::File(f)) => f.name = new_name_str.clone(),
                    Some(FsNode::Folder(f)) => f.name = new_name_str.clone(),
                    Some(FsNode::Degraded(d)) => d.name = new_name_str.clone(),
                    None => {}
                }
            }

            // Update parent relationship if moved
            if !is_same_parent {
                // Remove from old parent's children
                if let Some(children) = inner.children.get_mut(&parent) {
                    children.retain(|&c| c != src_inode);
                }
                // Add to new parent's children
                inner.children.entry(new_parent).or_default().push(src_inode);
                
                // Update parent_uid in node
                let new_parent_uid = inner.nodes.get(&new_parent)
                    .map(|n| n.uid().clone());
                match inner.nodes.get_mut(&src_inode) {
                    Some(FsNode::File(f)) => f.parent_uid = new_parent_uid,
                    Some(FsNode::Folder(f)) => f.parent_uid = new_parent_uid,
                    Some(FsNode::Degraded(d)) => d.parent_uid = new_parent_uid,
                    None => {}
                }
            }
        }

        tracing::info!("Renamed '{}' to '{}'", name_str, new_name_str);
        Ok(())
    }

    async fn flush(
        &self,
        req: Request,
        inode: u64,
        fh: u64,
        _lock_owner: u64,
    ) -> fuse3::Result<()> {
        tracing::debug!("FUSE flush: inode={}, fh={}, uid={}, gid={}, pid={}",
            inode, fh, req.uid, req.gid, req.pid);

        // Queue upload for background processing (non-blocking)
        // The file is considered "saved" once it's in the upload queue
        self.queue_write_buffer(fh).await
            .map_err(|e| {
                tracing::error!("Failed to queue writes: {}", e);
                Errno::from(libc::EIO)
            })?;

        Ok(())
    }

    async fn fsync(
        &self,
        req: Request,
        inode: u64,
        fh: u64,
        _datasync: bool,
    ) -> fuse3::Result<()> {
        tracing::debug!("FUSE fsync: inode={}, fh={}, uid={}, gid={}, pid={}",
            inode, fh, req.uid, req.gid, req.pid);

        // fsync should wait for upload to complete (blocking for durability)
        self.upload_write_buffer(fh).await
            .map_err(|e| {
                tracing::error!("Failed to sync writes: {}", e);
                Errno::from(libc::EIO)
            })?;

        Ok(())
    }

    async fn setattr(
        &self,
        req: Request,
        inode: u64,
        _fh: Option<u64>,
        set_attr: SetAttr,
    ) -> fuse3::Result<ReplyAttr> {
        tracing::debug!("FUSE setattr: inode={}, uid={}, gid={}, pid={}, set_attr={:?}",
            inode, req.uid, req.gid, req.pid, set_attr);

        // For cloud storage, we can only really handle truncate (via size)
        // Other attrs like mode, uid, gid, atime, mtime are best-effort or ignored
        
        let node = self.get_node(inode).await
            .ok_or_else(Errno::new_not_exist)?;

        // Handle truncate (size=0 typically happens when overwriting a file)
        if let Some(new_size) = set_attr.size {
            if new_size == 0 {
                // Truncate to zero - clear the file cache
                let mut inner = self.inner.write().await;
                inner.file_cache.put(inode, Vec::new());
                if let Some(FsNode::File(f)) = inner.nodes.get_mut(&inode) {
                    f.size = 0;
                }
            }
            // Non-zero truncate is more complex and we'll handle it on write
        }

        // Return current attributes (we don't actually change mode/uid/gid on cloud)
        Ok(ReplyAttr {
            ttl: TTL,
            attr: node.attr(inode),
        })
    }

    async fn opendir(
        &self,
        req: Request,
        inode: u64,
        _flags: u32,
    ) -> fuse3::Result<ReplyOpen> {
        tracing::debug!("FUSE opendir: inode={}, uid={}, gid={}, pid={}",
            inode, req.uid, req.gid, req.pid);

        let node = self.get_node(inode).await
            .ok_or_else(Errno::new_not_exist)?;

        if !node.is_dir() {
            return Err(Errno::new_is_not_dir());
        }

        // Allocate a directory handle
        let fh = self.inner.read().await.next_fh.fetch_add(1, Ordering::SeqCst);

        Ok(ReplyOpen { fh, flags: 0 })
    }

    async fn releasedir(
        &self,
        req: Request,
        inode: u64,
        fh: u64,
        _flags: u32,
    ) -> fuse3::Result<()> {
        tracing::debug!("FUSE releasedir: inode={}, fh={}, uid={}, gid={}, pid={}",
            inode, fh, req.uid, req.gid, req.pid);
        Ok(())
    }

    async fn copy_file_range(
        &self,
        req: Request,
        inode: u64,
        _fh_in: u64,
        off_in: u64,
        inode_out: u64,
        fh_out: u64,
        off_out: u64,
        length: u64,
        _flags: u64,
    ) -> fuse3::Result<ReplyCopyFileRange> {
        tracing::info!("FUSE copy_file_range: src_inode={}, dst_inode={}, length={}, uid={}, gid={}, pid={}",
            inode, inode_out, length, req.uid, req.gid, req.pid);

        // For cloud storage, we can't do server-side partial copies
        // So we read from source and write to destination
        
        // Read source content
        let src_content = self.get_file_content(inode).await
            .map_err(|e| {
                tracing::error!("Failed to read source file: {}", e);
                Errno::from(libc::EIO)
            })?;

        let off_in = off_in as usize;
        let length = length as usize;

        if off_in >= src_content.len() {
            return Ok(ReplyCopyFileRange { copied: 0 });
        }

        let end = std::cmp::min(off_in + length, src_content.len());
        let data = &src_content[off_in..end];
        let copied = data.len();

        // Write to destination
        {
            let mut inner = self.inner.write().await;
            
            if let Some(buf) = inner.write_buffers.get_mut(&fh_out) {
                let off_out = off_out as usize;
                let new_end = off_out + copied;
                if buf.content.len() < new_end {
                    buf.content.resize(new_end, 0);
                }
                buf.content[off_out..new_end].copy_from_slice(data);
                buf.dirty = true;
                
                // Update file size
                if let Some(FsNode::File(f)) = inner.nodes.get_mut(&inode_out) {
                    f.size = f.size.max(new_end as u64);
                }
            } else {
                return Err(Errno::from(libc::EBADF));
            }
        }

        Ok(ReplyCopyFileRange { copied: copied as u64 })
    }
}

/// FOPEN_DIRECT_IO flag (1 << 0) - bypass page cache for this open file.
/// This ensures the kernel doesn't cache file size from getattr, which may differ
/// from the actual downloaded size. Without this, files may appear truncated.
const FOPEN_DIRECT_IO: u32 = 1 << 0;

/// Progress callback type for mount status updates.
pub type ProgressCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Mount Proton Drive at the specified path.
/// 
/// The `ready_tx` channel is used to signal when mount is ready.
pub async fn mount(
    mount_path: &Path,
    session: &ProtonAPISession,
    cancellation: CancellationToken,
    progress: Option<ProgressCallback>,
    ready_tx: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<()> {
    let report = |msg: &str| {
        if let Some(ref cb) = progress {
            cb(msg);
        }
    };

    let uid = unsafe { libc::getuid() };
    let gid = unsafe { libc::getgid() };

    // Check if allow_other is enabled in /etc/fuse.conf
    let allow_other_enabled = std::fs::read_to_string("/etc/fuse.conf")
        .map(|content| {
            content.lines().any(|line| {
                let trimmed = line.trim();
                trimmed == "user_allow_other" && !trimmed.starts_with('#')
            })
        })
        .unwrap_or(false);

    let mut mount_options = MountOptions::default();
    mount_options
        .fs_name("proton-drive")
        .force_readdir_plus(true)
        .uid(uid)
        .gid(gid);
    
    if allow_other_enabled {
        mount_options.allow_other(true);
    }

    report("Connecting to Proton Drive...");

    // Create ProtonDriveClient from session
    let client = ProtonDriveClient::new(session, None)
        .context("Failed to create Proton Drive client")?;

    report("Loading root folder...");

    // Create multi-progress bar for download tracking
    let multi_progress = Arc::new(MultiProgress::new());

    // Create and initialize filesystem
    let fs = ProtonDriveFs::new(multi_progress.clone(), mount_path)
        .context("Failed to create filesystem")?;
    fs.init_with_client(client).await
        .context("Failed to initialize filesystem")?;

    report("Starting FUSE filesystem...");

    tracing::info!("Mounting Proton Drive at {:?}", mount_path);

    let mount_handle = Session::new(mount_options)
        .mount_with_unprivileged(fs, mount_path)
        .await
        .context("Failed to mount filesystem")?;

    // Signal that mount is ready
    if let Some(tx) = ready_tx {
        let _ = tx.send(());
    }

    // Add GTK bookmark so it appears in Nautilus/Files sidebar immediately
    let mount_path_owned = mount_path.to_owned();
    add_gtk_bookmark(&mount_path_owned, "Proton Drive");

    tokio::select! {
        res = mount_handle => {
            res.context("Filesystem error")?;
        }
        _ = cancellation.cancelled() => {
            tracing::info!("Unmounting Proton Drive (Ctrl+C)...");
            println!("\n  Unmounting...");
            // MountHandle will be dropped here, triggering unmount
        }
    }

    // Remove bookmark on unmount
    remove_gtk_bookmark(&mount_path_owned);

    // Give FUSE time to finish cleanup
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Always try fusermount3 to ensure clean unmount
    tracing::debug!("Running fusermount3 cleanup...");
    let _ = std::process::Command::new("fusermount3")
        .args(["-u", "-z"])
        .arg(&mount_path_owned)
        .output();

    // Wait and verify unmount
    tokio::time::sleep(Duration::from_millis(100)).await;
    
    // Check if still mounted and retry
    if std::fs::read_dir(&mount_path_owned).is_err() && mount_path_owned.exists() {
        tracing::warn!("Mount still appears stale, forcing cleanup...");
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", "-z", "-q"])
            .arg(&mount_path_owned)
            .output();
    }

    println!("  Unmounted.");
    tracing::info!("Unmounted Proton Drive");
    Ok(())
}

/// Hydrate files for offline access.
///
/// This downloads all files in the specified path and stores them in the offline cache.
/// The path is relative to MyFiles (e.g., "Documents" or "Photos/2024").
pub async fn hydrate(
    client: &ProtonDriveClient,
    index: &Arc<OfflineIndex>,
    path: &str,
    recursive: bool,
    cancellation: CancellationToken,
) -> Result<()> {
    // Get the root folder
    let my_files_folder = client.get_my_files_folder().await
        .context("Failed to get My Files folder")?;

    let root_link_id = my_files_folder.base.uid.link_id.raw().to_string();
    let volume_id = my_files_folder.base.uid.volume_id.raw().to_string();

    // Index the root folder
    let root_node = IndexedNode {
        link_id: root_link_id.clone(),
        parent_link_id: None,
        volume_id: volume_id.clone(),
        name: "MyFiles".to_string(),
        node_type: NodeType::Folder,
        mime_type: None,
        size: None,
        revision_id: None,
        creation_time: my_files_folder.base.creation_time,
        modification_time: None,
        fetched_at: Utc::now(),
        local_only: false,
        pending_delete: false,
    };
    index.upsert_node(&root_node)?;

    // Navigate to the target path
    let path_components: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    let mut current_uid = my_files_folder.base.uid.clone();
    let mut current_link_id = root_link_id;

    for component in &path_components {
        // Check if we already have this child indexed
        if let Some(child) = index.get_child_by_name(&current_link_id, component)? {
            current_link_id = child.link_id.clone();
            current_uid = NodeUid::new(
                VolumeId::new(child.volume_id.clone()),
                LinkId::new(child.link_id.clone()),
            );
            continue;
        }

        // Need to fetch children
        let children_stream = client.enumerate_folder_children(current_uid.clone()).await
            .context("Failed to enumerate folder")?;

        let mut children_stream = pin!(children_stream);
        let mut found = false;

        while let Some(result) = children_stream.next().await {
            if cancellation.is_cancelled() {
                return Ok(());
            }

            match result {
                Ok(potential) => {
                    let indexed = indexed_node_from_potential(&potential, &current_link_id);
                    let node_link_id = indexed.link_id.clone();
                    let node_name = indexed.name.clone();
                    let node_type = indexed.node_type;

                    // Index this node
                    index.upsert_node(&indexed)?;

                    if node_name == *component {
                        found = true;
                        current_link_id = node_link_id.clone();
                        current_uid = NodeUid::new(
                            VolumeId::new(indexed.volume_id.clone()),
                            LinkId::new(node_link_id),
                        );

                        if node_type == NodeType::File {
                            anyhow::bail!("Path component '{}' is a file, not a folder", component);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Error fetching child: {}", e);
                }
            }
        }

        if !found {
            anyhow::bail!("Path component '{}' not found", component);
        }
    }

    // Now hydrate the target folder
    hydrate_folder(client, index, &current_uid, &current_link_id, recursive, cancellation).await
}

/// Recursively hydrate a folder.
async fn hydrate_folder(
    client: &ProtonDriveClient,
    index: &Arc<OfflineIndex>,
    folder_uid: &NodeUid,
    folder_link_id: &str,
    recursive: bool,
    cancellation: CancellationToken,
) -> Result<()> {
    use indicatif::{MultiProgress, ProgressBar, ProgressStyle};

    let multi_progress = MultiProgress::new();

    // Get folder name for display
    let folder_name = index.get_node(folder_link_id)?
        .map(|n| n.name.clone())
        .unwrap_or_else(|| "folder".to_string());

    let spinner = multi_progress.add(ProgressBar::new_spinner());
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap()
    );
    spinner.set_message(format!("Scanning {}...", folder_name));
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));

    // Enumerate folder children
    let children_stream = client.enumerate_folder_children(folder_uid.clone()).await
        .context("Failed to enumerate folder")?;

    let mut children_stream = pin!(children_stream);
    let mut files_to_hydrate: Vec<(NodeUid, String, String, i64)> = Vec::new();
    let mut subfolders: Vec<(NodeUid, String)> = Vec::new();

    while let Some(result) = children_stream.next().await {
        if cancellation.is_cancelled() {
            spinner.finish_with_message(format!("{} Cancelled", style("✗").red()));
            return Ok(());
        }

        match result {
            Ok(potential) => {
                let indexed = indexed_node_from_potential(&potential, folder_link_id);
                let link_id = indexed.link_id.clone();
                let name = indexed.name.clone();
                let node_type = indexed.node_type;
                let revision_id = indexed.revision_id.clone();
                let size = indexed.size.unwrap_or(0);

                // Index this node
                index.upsert_node(&indexed)?;

                match node_type {
                    NodeType::File => {
                        if let Some(rev_id) = revision_id {
                            // Check if already available offline
                            let status = index.get_offline_status(&link_id)?;
                            if status != OfflineStatus::Available {
                                files_to_hydrate.push((
                                    NodeUid::new(
                                        VolumeId::new(indexed.volume_id.clone()),
                                        LinkId::new(link_id),
                                    ),
                                    name,
                                    rev_id,
                                    size,
                                ));
                            }
                        }
                    }
                    NodeType::Folder => {
                        if recursive {
                            subfolders.push((
                                NodeUid::new(
                                    VolumeId::new(indexed.volume_id.clone()),
                                    LinkId::new(link_id),
                                ),
                                name,
                            ));
                        }
                    }
                }
            }
            Err(e) => {
                tracing::warn!("Error fetching child: {}", e);
            }
        }
    }

    spinner.finish_with_message(format!(
        "{} {} ({} files, {} folders)",
        style("✓").green(),
        folder_name,
        files_to_hydrate.len(),
        subfolders.len()
    ));

    // Download files
    if !files_to_hydrate.is_empty() {
        let cache_dir = dirs::cache_dir()
            .context("Could not determine cache directory")?
            .join("pdcli")
            .join("offline");
        std::fs::create_dir_all(&cache_dir)?;

        let total_size: i64 = files_to_hydrate.iter().map(|(_, _, _, s)| s).sum();
        let progress = multi_progress.add(ProgressBar::new(total_size as u64));
        progress.set_style(
            ProgressStyle::default_bar()
                .template("  {spinner:.cyan} [{bar:40.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("█▓░")
        );

        let mut downloaded: i64 = 0;

        for (file_uid, filename, revision_id, size) in files_to_hydrate {
            if cancellation.is_cancelled() {
                progress.finish_with_message(format!("{} Cancelled", style("✗").red()));
                return Ok(());
            }

            let file_progress = multi_progress.add(ProgressBar::new(size as u64));
            file_progress.set_style(
                ProgressStyle::default_bar()
                    .template("    {spinner:.white.on_magenta} {msg} [{bar:30.white.on_magenta}] {bytes}/{total_bytes}")
                    .unwrap()
                    .progress_chars("█▓░")
            );
            file_progress.set_message(format!("↓ {}", filename));
            file_progress.enable_steady_tick(std::time::Duration::from_millis(100));

            // Create revision UID
            let revision_uid = RevisionUid::new(file_uid.clone(), RevisionId::new(revision_id.clone()));

            // Download the file
            match download_file_to_cache(client, &revision_uid, &cache_dir, &file_progress).await {
                Ok(content_path) => {
                    let actual_size = std::fs::metadata(&content_path)
                        .map(|m| m.len() as i64)
                        .unwrap_or(size);

                    // Mark as offline in index
                    index.mark_offline(
                        file_uid.link_id.raw(),
                        &revision_id,
                        &content_path,
                        actual_size,
                    )?;

                    file_progress.finish_with_message(format!("{} {}", style("✓").green(), filename));
                    downloaded += actual_size;
                    progress.set_position(downloaded as u64);
                }
                Err(e) => {
                    file_progress.finish_with_message(format!("{} {} ({})", style("✗").red(), filename, e));
                    tracing::error!("Failed to download {}: {}", filename, e);
                }
            }
        }

        progress.finish_and_clear();
    }

    // Recursively process subfolders
    for (subfolder_uid, _subfolder_name) in subfolders {
        if cancellation.is_cancelled() {
            return Ok(());
        }

        let subfolder_link_id = subfolder_uid.link_id.raw().to_string();
        Box::pin(hydrate_folder(
            client,
            index,
            &subfolder_uid,
            &subfolder_link_id,
            recursive,
            cancellation.clone(),
        )).await?;
    }

    Ok(())
}

/// Download a file to the offline cache.
async fn download_file_to_cache(
    client: &ProtonDriveClient,
    revision_uid: &RevisionUid,
    cache_dir: &Path,
    progress: &ProgressBar,
) -> Result<String> {
    use sha2::{Digest, Sha256};

    // Generate a unique filename based on revision UID
    let uid_string = revision_uid.to_string();
    let mut hasher = Sha256::new();
    hasher.update(uid_string.as_bytes());
    let hash = hasher.finalize();
    let filename = format!("{:x}", hash);
    let content_path = cache_dir.join(&filename);

    // Check if already cached
    if content_path.exists() {
        return Ok(content_path.to_string_lossy().to_string());
    }

    // Create downloader
    let downloader = client.get_file_downloader(revision_uid.clone()).await
        .context("Failed to create file downloader")?;

    // Download to buffer
    let buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
    let buffer_clone = buffer.clone();

    struct SharedWriter {
        buffer: Arc<std::sync::Mutex<Vec<u8>>>,
    }

    impl std::io::Write for SharedWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            let mut guard = self.buffer.lock().unwrap();
            guard.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let progress_clone = progress.clone();
    let writer = Box::new(SharedWriter { buffer: buffer_clone });
    let controller = downloader.download_to_stream(
        writer,
        Box::new(move |bytes_written, _total_bytes| {
            progress_clone.set_position(bytes_written as u64);
        }),
    );

    // Wait for download
    controller.completion.await
        .context("Download task panicked")?
        .context("Download failed")?;

    // Write to cache file
    let content = Arc::try_unwrap(buffer)
        .map_err(|_| anyhow::anyhow!("Buffer still has references"))?
        .into_inner()
        .unwrap();

    std::fs::write(&content_path, &content)
        .context("Failed to write to cache")?;

    Ok(content_path.to_string_lossy().to_string())
}

/// Convert a PotentialObject to an IndexedNode.
fn indexed_node_from_potential(
    potential: &PotentialObject<Node, DegradedNode>,
    parent_link_id: &str,
) -> IndexedNode {
    match potential {
        PotentialObject::Node(node) => {
            match node {
                Node::File(f) | Node::Photo(f) => {
                    IndexedNode {
                        link_id: f.base.base.uid.link_id.raw().to_string(),
                        parent_link_id: Some(parent_link_id.to_string()),
                        volume_id: f.base.base.uid.volume_id.raw().to_string(),
                        name: f.base.base.name.clone(),
                        node_type: NodeType::File,
                        mime_type: Some(f.base.media_type.clone()),
                        size: f.active_revision.claimed_size,
                        revision_id: Some(f.active_revision.uid.revision_id.raw().to_string()),
                        creation_time: f.base.base.creation_time,
                        modification_time: f.active_revision.claimed_modification_time,
                        fetched_at: Utc::now(),
                        local_only: false,
                        pending_delete: false,
                    }
                }
                Node::Folder(f) | Node::Album(f) => {
                    IndexedNode {
                        link_id: f.base.uid.link_id.raw().to_string(),
                        parent_link_id: Some(parent_link_id.to_string()),
                        volume_id: f.base.uid.volume_id.raw().to_string(),
                        name: f.base.name.clone(),
                        node_type: NodeType::Folder,
                        mime_type: None,
                        size: None,
                        revision_id: None,
                        creation_time: f.base.creation_time,
                        modification_time: None,
                        fetched_at: Utc::now(),
                        local_only: false,
                        pending_delete: false,
                    }
                }
            }
        }
        PotentialObject::Degraded(degraded) => {
            match degraded {
                DegradedNode::File(f) | DegradedNode::Photo(f) => {
                    let name = match &f.base.name {
                        PotentialObject::Node(n) => n.clone(),
                        PotentialObject::Degraded(_) => format!("[degraded-{}]", f.base.uid.link_id.raw()),
                    };
                    IndexedNode {
                        link_id: f.base.uid.link_id.raw().to_string(),
                        parent_link_id: Some(parent_link_id.to_string()),
                        volume_id: f.base.uid.volume_id.raw().to_string(),
                        name,
                        node_type: NodeType::File,
                        mime_type: Some(f.media_type.clone()),
                        size: Some(f.total_storage_quota_usage),
                        revision_id: None,
                        creation_time: f.base.creation_time,
                        modification_time: None,
                        fetched_at: Utc::now(),
                        local_only: false,
                        pending_delete: false,
                    }
                }
                DegradedNode::Folder(f) | DegradedNode::Album(f) => {
                    let name = match &f.base.name {
                        PotentialObject::Node(n) => n.clone(),
                        PotentialObject::Degraded(_) => format!("[degraded-{}]", f.base.uid.link_id.raw()),
                    };
                    IndexedNode {
                        link_id: f.base.uid.link_id.raw().to_string(),
                        parent_link_id: Some(parent_link_id.to_string()),
                        volume_id: f.base.uid.volume_id.raw().to_string(),
                        name,
                        node_type: NodeType::Folder,
                        mime_type: None,
                        size: None,
                        revision_id: None,
                        creation_time: f.base.creation_time,
                        modification_time: None,
                        fetched_at: Utc::now(),
                        local_only: false,
                        pending_delete: false,
                    }
                }
            }
        }
    }
}

/// Convert an IndexedNode (from the database) to an FsNode (for FUSE).
/// This is used by the index event handler to populate FUSE from the index.
fn fs_node_from_indexed(indexed: &IndexedNode) -> FsNode {
    use proton_drive_sdk::links::LinkId;
    use proton_drive_sdk::node::revision::RevisionUid;
    use proton_drive_sdk::revision::RevisionId;
    
    let node_uid = NodeUid::new(
        VolumeId::new(indexed.volume_id.clone()),
        LinkId::new(indexed.link_id.clone()),
    );
    
    match indexed.node_type {
        NodeType::File => {
            let revision_uid = indexed.revision_id.as_ref().map(|rev_id| {
                RevisionUid::new(node_uid.clone(), RevisionId::new(rev_id.clone()))
            }).unwrap_or_else(|| {
                RevisionUid::new(node_uid.clone(), RevisionId::new("unknown".to_string()))
            });
            
            FsNode::File(ProtonFileMetadata {
                uid: node_uid,
                parent_uid: indexed.parent_link_id.as_ref().map(|p| {
                    NodeUid::new(
                        VolumeId::new(indexed.volume_id.clone()),
                        LinkId::new(p.clone()),
                    )
                }),
                name: indexed.name.clone(),
                mime_type: indexed.mime_type.clone().unwrap_or_else(|| "application/octet-stream".to_string()),
                size: indexed.size.unwrap_or(0) as u64,
                size_on_cloud: indexed.size.unwrap_or(0) as u64,
                creation_time: indexed.creation_time,
                modification_time: indexed.modification_time,
                trash_time: None,
                author_email: None,
                name_author_email: None,
                owner_email: None,
                owner_organisation: None,
                revision_uid,
                revision_creation_time: indexed.modification_time.unwrap_or(indexed.creation_time),
                content_sha1: None,
                is_photo: false,
                capture_time: None,
                thumbnail_id: None,
            })
        }
        NodeType::Folder => {
            FsNode::Folder(ProtonFolderMetadata {
                uid: node_uid,
                parent_uid: indexed.parent_link_id.as_ref().map(|p| {
                    NodeUid::new(
                        VolumeId::new(indexed.volume_id.clone()),
                        LinkId::new(p.clone()),
                    )
                }),
                name: indexed.name.clone(),
                creation_time: indexed.creation_time,
                trash_time: None,
                author_email: None,
                name_author_email: None,
                owner_email: None,
                owner_organisation: None,
                is_album: false,
            })
        }
    }
}

/// Background refresh of folder children from network.
/// 
/// ARCHITECTURE: This function ONLY updates the index. The index emits
/// a ChildrenLoaded event, which the index event handler uses to invalidate
/// FUSE caches so they reload from the updated index.
async fn background_refresh_children(
    client: &Arc<RwLock<Option<ProtonDriveClient>>>,
    _inner: &Arc<RwLock<ProtonDriveFsInner>>, // No longer used - FUSE updates via index events
    index: &Arc<OfflineIndex>,
    is_online: &Arc<std::sync::atomic::AtomicBool>,
    inode: u64,
    folder_uid: &NodeUid,
    folder_link_id: &str,
) -> Result<()> {
    // Get children stream
    let children_stream = {
        let client_guard = client.read().await;
        let client = client_guard.as_ref().ok_or_else(|| anyhow::anyhow!("No client"))?;
        client.enumerate_folder_children(folder_uid.clone()).await?
    };

    // Mark as online since we successfully connected
    is_online.store(true, std::sync::atomic::Ordering::Relaxed);

    // Collect all children
    let mut indexed_nodes = Vec::new();
    let mut children_stream = pin!(children_stream);
    
    while let Some(result) = children_stream.next().await {
        match result {
            Ok(potential) => {
                let indexed = indexed_node_from_potential(&potential, folder_link_id);
                indexed_nodes.push(indexed);
            }
            Err(e) => {
                tracing::debug!("Background refresh: failed to fetch child: {}", e);
                continue;
            }
        }
    }

    tracing::debug!("Background refresh: got {} children for folder {}", indexed_nodes.len(), inode);

    // Update the offline index (batch operation, emits ChildrenLoaded event).
    // The event handler will invalidate FUSE caches so they reload from the updated index.
    if let Err(e) = index.upsert_children(folder_link_id, &indexed_nodes) {
        tracing::warn!("Background refresh: failed to update index: {}", e);
    }

    Ok(())
}

/// Background sync of pending mutations (called automatically by mount).
/// This is a quiet version that just logs, doesn't print to console.
async fn background_sync(
    client: &ProtonDriveClient,
    index: &Arc<OfflineIndex>,
) -> Result<()> {
    let mutations = index.get_pending_mutations()?;

    if mutations.is_empty() {
        return Ok(());
    }

    tracing::info!("Background sync: processing {} pending mutations", mutations.len());

    for stored in mutations {
        let mutation_desc = match &stored.mutation {
            PendingMutation::CreateFile { name, .. } => format!("create file '{}'", name),
            PendingMutation::UpdateFile { link_id, .. } => format!("update file '{}'", link_id),
            PendingMutation::Rename { link_id, new_name, .. } => format!("rename '{}' to '{}'", link_id, new_name),
            PendingMutation::Delete { link_id } => format!("delete '{}'", link_id),
            PendingMutation::CreateFolder { name, .. } => format!("create folder '{}'", name),
        };

        let result = match &stored.mutation {
            PendingMutation::CreateFile { link_id: _, parent_link_id, name, mime_type, content_path } => {
                sync_create_file(client, index, parent_link_id, name, mime_type, content_path).await
            }
            PendingMutation::UpdateFile { link_id, revision_id: _, content_path } => {
                sync_update_file(client, index, link_id, content_path).await
            }
            PendingMutation::Rename { link_id, new_parent_link_id, new_name } => {
                sync_rename(client, index, link_id, new_parent_link_id.as_deref(), new_name).await
            }
            PendingMutation::Delete { link_id } => {
                sync_delete(client, index, link_id).await
            }
            PendingMutation::CreateFolder { link_id: _, parent_link_id, name } => {
                sync_create_folder(client, index, parent_link_id, name).await
            }
        };

        match result {
            Ok(()) => {
                // Remove mutation on success
                index.remove_mutation(stored.id)?;
                tracing::info!("Synced: {}", mutation_desc);
            }
            Err(e) => {
                // Increment retry count
                index.increment_mutation_retry(stored.id)?;
                tracing::warn!("Failed to sync {}: {}", mutation_desc, e);
            }
        }
    }

    Ok(())
}

async fn sync_create_file(
    client: &ProtonDriveClient,
    index: &Arc<OfflineIndex>,
    parent_link_id: &str,
    name: &str,
    mime_type: &str,
    content_path: &str,
) -> Result<()> {
    // Read content
    let content = std::fs::read(content_path)
        .context("Failed to read content file")?;

    // Get parent node to construct NodeUid
    let parent = index.get_node(parent_link_id)?
        .ok_or_else(|| anyhow::anyhow!("Parent node not found in index"))?;

    let parent_uid = NodeUid::new(
        VolumeId::new(parent.volume_id.clone()),
        LinkId::new(parent_link_id.to_string()),
    );

    // Create file uploader
    let uploader = client.get_file_uploader(
        parent_uid,
        name.to_string(),
        mime_type.to_string(),
        content.len() as i64,
        Some(std::time::SystemTime::now()),
        None,
        None,
        true,
    ).await.context("Failed to create file uploader")?;

    // Upload
    let _node_uid = uploader.upload_from_stream(
        Box::new(std::io::Cursor::new(content)),
        Vec::new(),
        Box::new(|_, _| {}),
    ).await.context("Upload failed")?;

    // Update index with real link_id
    // (The local_only node will be replaced when we next fetch from server)

    // Clean up content file
    let _ = std::fs::remove_file(content_path);

    Ok(())
}

async fn sync_update_file(
    client: &ProtonDriveClient,
    index: &Arc<OfflineIndex>,
    link_id: &str,
    content_path: &str,
) -> Result<()> {
    // Get node from index
    let node = index.get_node(link_id)?
        .ok_or_else(|| anyhow::anyhow!("Node not found in index"))?;

    let revision_id = node.revision_id
        .ok_or_else(|| anyhow::anyhow!("Node has no revision"))?;

    // Read content
    let content = std::fs::read(content_path)
        .context("Failed to read content file")?;

    let node_uid = NodeUid::new(
        VolumeId::new(node.volume_id),
        LinkId::new(link_id.to_string()),
    );

    let revision_uid = RevisionUid::new(node_uid, RevisionId::new(revision_id));

    // Create new revision uploader
    let uploader = client.get_file_revision_uploader(
        revision_uid,
        content.len() as i64,
        Some(std::time::SystemTime::now()),
        None,
        None,
    ).await.context("Failed to create revision uploader")?;

    // Upload
    uploader.upload_from_stream(
        Box::new(std::io::Cursor::new(content)),
        Vec::new(),
        Box::new(|_, _| {}),
    ).await.context("Upload failed")?;

    // Clean up content file
    let _ = std::fs::remove_file(content_path);

    Ok(())
}

async fn sync_rename(
    client: &ProtonDriveClient,
    index: &Arc<OfflineIndex>,
    link_id: &str,
    new_parent_link_id: Option<&str>,
    new_name: &str,
) -> Result<()> {
    // Get node from index
    let node = index.get_node(link_id)?
        .ok_or_else(|| anyhow::anyhow!("Node not found in index"))?;

    let node_uid = NodeUid::new(
        VolumeId::new(node.volume_id.clone()),
        LinkId::new(link_id.to_string()),
    );

    // If moving to new parent
    if let Some(new_parent_id) = new_parent_link_id {
        if Some(new_parent_id.to_string()) != node.parent_link_id {
            let new_parent = index.get_node(new_parent_id)?
                .ok_or_else(|| anyhow::anyhow!("New parent not found in index"))?;

            let new_parent_uid = NodeUid::new(
                VolumeId::new(new_parent.volume_id),
                LinkId::new(new_parent_id.to_string()),
            );

            client.move_nodes(vec![node_uid.clone()], new_parent_uid).await
                .context("Move failed")?;
        }
    }

    // If renaming
    if new_name != node.name {
        client.rename_node(node_uid, new_name.to_string(), None).await
            .context("Rename failed")?;
    }

    Ok(())
}

async fn sync_delete(
    client: &ProtonDriveClient,
    index: &Arc<OfflineIndex>,
    link_id: &str,
) -> Result<()> {
    // Get node from index
    let node = index.get_node(link_id)?
        .ok_or_else(|| anyhow::anyhow!("Node not found in index"))?;

    let node_uid = NodeUid::new(
        VolumeId::new(node.volume_id),
        LinkId::new(link_id.to_string()),
    );

    // Move to trash
    let results = client.trash_nodes(vec![node_uid.clone()]).await
        .context("Trash failed")?;
    
    // Check if the trash succeeded
    if let Some(Err(e)) = results.get(&node_uid) {
        anyhow::bail!("Trash failed: {}", e);
    }

    // Remove from index (recursive in case it's a folder with children)
    index.delete_node_recursive(link_id)?;

    Ok(())
}

async fn sync_create_folder(
    client: &ProtonDriveClient,
    index: &Arc<OfflineIndex>,
    parent_link_id: &str,
    name: &str,
) -> Result<()> {
    // Get parent node
    let parent = index.get_node(parent_link_id)?
        .ok_or_else(|| anyhow::anyhow!("Parent node not found in index"))?;

    let parent_uid = NodeUid::new(
        VolumeId::new(parent.volume_id),
        LinkId::new(parent_link_id.to_string()),
    );

    // Create folder
    let _folder_uid = client.create_folder(parent_uid, name.to_string(), None).await
        .context("Create folder failed")?;

    Ok(())
}
