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

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use bytes::Bytes;
use chrono::{DateTime, Utc};
use console::style;
use fuse3::raw::prelude::*;
use fuse3::raw::reply::{ReplyCopyFileRange, ReplyCreated, ReplyWrite};
use fuse3::{Errno, MountOptions, SetAttr};
use futures_util::stream;
use futures_util::stream::Stream;
use futures_util::StreamExt;
use globset::{Glob, GlobSetBuilder, GlobSet};
use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, RwLock};
use tokio_util::sync::CancellationToken;

use proton_drive_sdk::api::events::{VolumeEventDto, VolumeEventType};
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::node::file::FileNode as DriveFileNode;
use proton_drive_sdk::node::folder::FolderNode as DriveFolderNode;
use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::revision::RevisionId;
use proton_drive_sdk::utils::PotentialObject;
use proton_drive_sdk::volume::VolumeId;
use proton_sdk_rs2::session::ProtonAPISession;

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

/// Event polling interval (5 seconds for responsive updates)
const EVENT_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Check if a filename is a LibreOffice/OpenOffice lock file.
/// Lock files have the pattern `.~lock.<filename>#`
fn is_lock_file(filename: &str) -> bool {
    filename.starts_with(".~lock.") && filename.ends_with('#')
}

/// The .pdclignore filename constant
const PDCLIGNORE_FILENAME: &str = ".pdclignore";

/// Default patterns for the global .pdclignore file
const DEFAULT_PDCLIGNORE_PATTERNS: &str = r#"# pdcli ignore file
# Lines starting with # are comments
# Patterns use gitignore-style glob matching

# Common temporary files
*.tmp
*.temp
*.swp
*.swo
*~
.goutputstream-*

# OS-generated files
.DS_Store
.DS_Store?
._*
.Spotlight-V100
.Trashes
Thumbs.db
ehthumbs.db
Desktop.ini

# Trash directories (Linux/freedesktop.org)
.Trash
.Trash-*

# Partial downloads
*.part
*.partial
*.download
*.crdownload

# Editor backup files
*~
*.bak
*.backup
\#*\#
.#*

# Log files
*.log

# Cache directories and files
.cache/
__pycache__/
*.pyc
.pytest_cache/
node_modules/
.npm/
.yarn/

# Build artifacts
*.o
*.obj
*.a
*.lib
*.so
*.dll
*.dylib

# IDE and editor directories
.idea/
.vscode/
*.xcworkspace/
*.xcodeproj/
"#;

/// Manager for .pdclignore patterns.
/// Supports a global config file and per-directory .pdclignore files.
struct IgnoreManager {
    /// Path to the global .pdclignore file in config directory
    global_ignore_path: PathBuf,
    /// Compiled global patterns (from config directory)
    global_patterns: GlobSet,
    /// Cached per-directory patterns: directory path -> compiled patterns
    /// Note: In FUSE context, we use inode paths, but patterns are loaded from content
    #[allow(dead_code)]
    directory_patterns: BTreeMap<u64, GlobSet>,
}

impl IgnoreManager {
    /// Create a new IgnoreManager, loading the global .pdclignore from config.
    fn new() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .context("Could not determine config directory")?
            .join("pdcli");
        
        std::fs::create_dir_all(&config_dir)
            .context("Failed to create config directory")?;
        
        let global_ignore_path = config_dir.join(PDCLIGNORE_FILENAME);
        
        // Create default .pdclignore if it doesn't exist
        if !global_ignore_path.exists() {
            std::fs::write(&global_ignore_path, DEFAULT_PDCLIGNORE_PATTERNS)
                .context("Failed to write default .pdclignore file")?;
            tracing::info!("Created default .pdclignore at {:?}", global_ignore_path);
        }
        
        // Load global patterns
        let global_patterns = Self::load_patterns_from_file(&global_ignore_path)?;
        
        Ok(Self {
            global_ignore_path,
            global_patterns,
            directory_patterns: BTreeMap::new(),
        })
    }

    /// Load and compile patterns from a .pdclignore file.
    fn load_patterns_from_file(path: &Path) -> Result<GlobSet> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read ignore file: {:?}", path))?;
        Self::compile_patterns(&content)
    }

    /// Compile patterns from a string (file content).
    fn compile_patterns(content: &str) -> Result<GlobSet> {
        let mut builder = GlobSetBuilder::new();
        
        for line in content.lines() {
            let line = line.trim();
            
            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            
            // Handle negation patterns (start with !) - we skip these for simplicity
            // Full gitignore compatibility would require more complex logic
            if line.starts_with('!') {
                tracing::debug!("Negation patterns not supported, skipping: {}", line);
                continue;
            }
            
            // Handle directory patterns (ending with /)
            // In gitignore, foo/ means match directory foo
            // We add the pattern both with and without the trailing slash
            let patterns_to_add = if line.ends_with('/') {
                let without_slash = line.trim_end_matches('/');
                vec![line.to_string(), without_slash.to_string()]
            } else {
                vec![line.to_string()]
            };
            
            for pattern in patterns_to_add {
                match Glob::new(&pattern) {
                    Ok(glob) => {
                        builder.add(glob);
                    }
                    Err(e) => {
                        tracing::warn!("Invalid glob pattern '{}': {}", pattern, e);
                    }
                }
            }
        }
        
        builder.build()
            .context("Failed to build glob set")
    }

    /// Check if a filename should be ignored based on global patterns.
    fn is_ignored_global(&self, filename: &str) -> bool {
        // Never ignore .pdclignore files themselves - they should be uploaded
        if filename == PDCLIGNORE_FILENAME {
            return false;
        }
        
        self.global_patterns.is_match(filename)
    }

    /// Check if a filename should be ignored based on local .pdclignore content.
    #[allow(dead_code)]
    fn is_ignored_by_content(&self, filename: &str, local_ignore_content: Option<&str>) -> bool {
        // Never ignore .pdclignore files themselves
        if filename == PDCLIGNORE_FILENAME {
            return false;
        }
        
        // Check global patterns first
        if self.global_patterns.is_match(filename) {
            return true;
        }
        
        // Check local patterns if provided
        if let Some(content) = local_ignore_content {
            if let Ok(local_patterns) = Self::compile_patterns(content) {
                if local_patterns.is_match(filename) {
                    return true;
                }
            }
        }
        
        false
    }

    /// Reload global patterns from the config file.
    #[allow(dead_code)]
    fn reload_global(&mut self) -> Result<()> {
        self.global_patterns = Self::load_patterns_from_file(&self.global_ignore_path)?;
        tracing::info!("Reloaded global .pdclignore patterns");
        Ok(())
    }
    
    /// Get the path to the global .pdclignore file.
    pub fn global_ignore_path(&self) -> &Path {
        &self.global_ignore_path
    }
}

/// Convert chrono DateTime to SystemTime.
fn datetime_to_system_time(dt: DateTime<Utc>) -> SystemTime {
    let timestamp = dt.timestamp();
    if timestamp >= 0 {
        UNIX_EPOCH + Duration::from_secs(timestamp as u64)
    } else {
        UNIX_EPOCH
    }
}

/// Metadata for a Proton Drive file.
#[derive(Debug, Clone)]
pub struct ProtonFileMetadata {
    /// Unique identifier (volume_id~link_id)
    pub uid: NodeUid,
    /// Parent node UID
    pub parent_uid: Option<NodeUid>,
    /// Decrypted file name
    pub name: String,
    /// MIME type (e.g., "image/jpeg", "application/pdf")
    pub mime_type: String,
    /// File size in bytes (plaintext, claimed by uploader)
    pub size: u64,
    /// Size on cloud storage (encrypted)
    pub size_on_cloud: u64,
    /// File creation time (when uploaded to Proton Drive)
    pub creation_time: DateTime<Utc>,
    /// File modification time (original file's mtime, if available)
    pub modification_time: Option<DateTime<Utc>>,
    /// Time when moved to trash (if trashed)
    pub trash_time: Option<DateTime<Utc>>,
    /// Author email (who created/uploaded the file)
    pub author_email: Option<String>,
    /// Name author email (who last renamed the file)
    pub name_author_email: Option<String>,
    /// Owner email
    pub owner_email: Option<String>,
    /// Owner organisation
    pub owner_organisation: Option<String>,
    /// Active revision UID (for downloading content)
    pub revision_uid: RevisionUid,
    /// Revision creation time
    pub revision_creation_time: DateTime<Utc>,
    /// SHA-1 hash of plaintext content (if available)
    pub content_sha1: Option<Vec<u8>>,
    /// Whether this is a photo
    pub is_photo: bool,
    /// Photo capture time (for photos only)
    pub capture_time: Option<DateTime<Utc>>,
}

impl ProtonFileMetadata {
    /// Create from a decrypted FileNode
    pub fn from_file_node(node: &DriveFileNode, is_photo: bool, capture_time: Option<DateTime<Utc>>) -> Self {
        let revision = &node.active_revision;
        
        Self {
            uid: node.base.base.uid.clone(),
            parent_uid: node.base.base.parent_uid.clone(),
            name: node.base.base.name.clone(),
            mime_type: node.base.media_type.clone(),
            size: revision.claimed_size.unwrap_or(0) as u64,
            size_on_cloud: node.total_size_on_cloud_storage as u64,
            creation_time: node.base.base.creation_time,
            modification_time: revision.claimed_modification_time,
            trash_time: node.base.base.trash_time,
            author_email: node.base.base.author.as_ref().ok().and_then(|a| a.email_address.clone()),
            name_author_email: node.base.base.name_author.as_ref().ok().and_then(|a| a.email_address.clone()),
            owner_email: node.base.base.owned_by.as_ref().and_then(|o| o.email.clone()),
            owner_organisation: node.base.base.owned_by.as_ref().and_then(|o| o.organisation.clone()),
            revision_uid: revision.uid.clone(),
            revision_creation_time: revision.creation_time,
            content_sha1: revision.claimed_digests.sha1.clone(),
            is_photo,
            capture_time,
        }
    }
}

/// Metadata for a Proton Drive folder.
#[derive(Debug, Clone)]
pub struct ProtonFolderMetadata {
    /// Unique identifier (volume_id~link_id)
    pub uid: NodeUid,
    /// Parent node UID
    pub parent_uid: Option<NodeUid>,
    /// Decrypted folder name
    pub name: String,
    /// Folder creation time
    pub creation_time: DateTime<Utc>,
    /// Time when moved to trash (if trashed)
    pub trash_time: Option<DateTime<Utc>>,
    /// Author email (who created the folder)
    pub author_email: Option<String>,
    /// Name author email (who last renamed the folder)
    pub name_author_email: Option<String>,
    /// Owner email
    pub owner_email: Option<String>,
    /// Owner organisation
    pub owner_organisation: Option<String>,
    /// Whether this is an album
    pub is_album: bool,
}

impl ProtonFolderMetadata {
    /// Create from a decrypted FolderNode
    pub fn from_folder_node(node: &DriveFolderNode, is_album: bool) -> Self {
        Self {
            uid: node.base.uid.clone(),
            parent_uid: node.base.parent_uid.clone(),
            name: node.base.name.clone(),
            creation_time: node.base.creation_time,
            trash_time: node.base.trash_time,
            author_email: node.base.author.as_ref().ok().and_then(|a| a.email_address.clone()),
            name_author_email: node.base.name_author.as_ref().ok().and_then(|a| a.email_address.clone()),
            owner_email: node.base.owned_by.as_ref().and_then(|o| o.email.clone()),
            owner_organisation: node.base.owned_by.as_ref().and_then(|o| o.organisation.clone()),
            is_album,
        }
    }
}

/// Metadata for a degraded node (decryption failed for some fields)
#[derive(Debug, Clone)]
pub struct DegradedNodeMetadata {
    /// Unique identifier
    pub uid: NodeUid,
    /// Parent node UID
    pub parent_uid: Option<NodeUid>,
    /// Name (may be degraded/placeholder)
    pub name: String,
    /// Whether this is a file (vs folder)
    pub is_file: bool,
    /// MIME type (for files)
    pub mime_type: Option<String>,
    /// Size on cloud (encrypted size)
    pub size_on_cloud: Option<u64>,
    /// Creation time
    pub creation_time: DateTime<Utc>,
    /// Errors that occurred during decryption
    pub errors: Vec<String>,
}

/// A node in the filesystem (either file or directory).
#[derive(Debug, Clone)]
pub enum FsNode {
    /// A successfully decrypted file
    File(ProtonFileMetadata),
    /// A successfully decrypted folder
    Folder(ProtonFolderMetadata),
    /// A degraded node (partial decryption failure)
    Degraded(DegradedNodeMetadata),
}

impl FsNode {
    /// Get the node's UID
    pub fn uid(&self) -> &NodeUid {
        match self {
            FsNode::File(f) => &f.uid,
            FsNode::Folder(f) => &f.uid,
            FsNode::Degraded(d) => &d.uid,
        }
    }

    /// Get the node's name
    pub fn name(&self) -> &str {
        match self {
            FsNode::File(f) => &f.name,
            FsNode::Folder(f) => &f.name,
            FsNode::Degraded(d) => &d.name,
        }
    }

    /// Check if this is a directory
    pub fn is_dir(&self) -> bool {
        match self {
            FsNode::File(_) => false,
            FsNode::Folder(_) => true,
            FsNode::Degraded(d) => !d.is_file,
        }
    }

    /// Get FUSE file type
    pub fn file_type(&self) -> FileType {
        if self.is_dir() {
            FileType::Directory
        } else {
            FileType::RegularFile
        }
    }

    /// Get file size (0 for directories)
    pub fn size(&self) -> u64 {
        match self {
            FsNode::File(f) => f.size,
            FsNode::Folder(_) => 0,
            FsNode::Degraded(d) => d.size_on_cloud.unwrap_or(0),
        }
    }

    /// Get creation time
    pub fn creation_time(&self) -> SystemTime {
        let dt = match self {
            FsNode::File(f) => f.creation_time,
            FsNode::Folder(f) => f.creation_time,
            FsNode::Degraded(d) => d.creation_time,
        };
        datetime_to_system_time(dt)
    }

    /// Get modification time
    pub fn modification_time(&self) -> SystemTime {
        match self {
            FsNode::File(f) => {
                if let Some(mtime) = f.modification_time {
                    datetime_to_system_time(mtime)
                } else {
                    datetime_to_system_time(f.creation_time)
                }
            }
            FsNode::Folder(f) => datetime_to_system_time(f.creation_time),
            FsNode::Degraded(d) => datetime_to_system_time(d.creation_time),
        }
    }

    /// Build FUSE file attributes
    pub fn attr(&self, inode: u64) -> FileAttr {
        let size = self.size();
        let ctime = self.creation_time();
        let mtime = self.modification_time();
        let perm = if self.is_dir() { DIR_MODE } else { FILE_MODE };
        let nlink = if self.is_dir() { 2 } else { 1 };
        
        FileAttr {
            ino: inode,
            size,
            blocks: (size + 4095) / 4096,
            atime: mtime.into(),
            mtime: mtime.into(),
            ctime: ctime.into(),
            kind: self.file_type(),
            perm,
            nlink,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            blksize: 4096,
        }
    }

    /// Convert from SDK Node
    pub fn from_node(node: &Node) -> Self {
        match node {
            Node::File(f) => FsNode::File(ProtonFileMetadata::from_file_node(f, false, None)),
            Node::Photo(f) => {
                // Photo nodes have capture_time in album_uids context
                // For now, use revision creation time as proxy
                FsNode::File(ProtonFileMetadata::from_file_node(f, true, None))
            }
            Node::Folder(f) => FsNode::Folder(ProtonFolderMetadata::from_folder_node(f, false)),
            Node::Album(f) => FsNode::Folder(ProtonFolderMetadata::from_folder_node(f, true)),
        }
    }

    /// Convert from SDK DegradedNode
    pub fn from_degraded(node: &DegradedNode) -> Self {
        let (uid, parent_uid, name, is_file, mime_type, size_on_cloud, creation_time, errors) = match node {
            DegradedNode::File(f) | DegradedNode::Photo(f) => {
                let name = match &f.base.name {
                    PotentialObject::Node(n) => n.clone(),
                    PotentialObject::Degraded(_) => format!("[degraded-{}]", f.base.uid.link_id.raw()),
                };
                let errors: Vec<String> = f.base.errors.iter().map(|e| e.to_string()).collect();
                (
                    f.base.uid.clone(),
                    f.base.parent_uid.clone(),
                    name,
                    true,
                    Some(f.media_type.clone()),
                    Some(f.total_storage_quota_usage as u64),
                    f.base.creation_time,
                    errors,
                )
            }
            DegradedNode::Folder(f) | DegradedNode::Album(f) => {
                let name = match &f.base.name {
                    PotentialObject::Node(n) => n.clone(),
                    PotentialObject::Degraded(_) => format!("[degraded-{}]", f.base.uid.link_id.raw()),
                };
                let errors: Vec<String> = f.base.errors.iter().map(|e| e.to_string()).collect();
                (
                    f.base.uid.clone(),
                    f.base.parent_uid.clone(),
                    name,
                    false,
                    None,
                    None,
                    f.base.creation_time,
                    errors,
                )
            }
        };

        FsNode::Degraded(DegradedNodeMetadata {
            uid,
            parent_uid,
            name,
            is_file,
            mime_type,
            size_on_cloud,
            creation_time,
            errors,
        })
    }
}

/// Maximum disk cache size in bytes (default 1GB)
const MAX_DISK_CACHE_SIZE: u64 = 1024 * 1024 * 1024;

/// Persistent disk cache for downloaded files.
/// Files are cached by revision UID so updated files are re-downloaded.
struct DiskCache {
    cache_dir: std::path::PathBuf,
    max_size: u64,
}

impl DiskCache {
    fn new(max_size: u64) -> Result<Self> {
        let cache_dir = dirs::cache_dir()
            .context("Could not determine cache directory")?
            .join("pdcli")
            .join("files");
        
        std::fs::create_dir_all(&cache_dir)
            .context("Failed to create cache directory")?;
        
        Ok(Self { cache_dir, max_size })
    }

    /// Get the cache file path for a revision UID.
    fn cache_path(&self, revision_uid: &RevisionUid) -> std::path::PathBuf {
        // Hash the revision UID to get a short, filesystem-safe filename
        // RevisionUid string can be 250+ chars, but Linux limits filenames to 255 bytes
        let uid_string = revision_uid.to_string();
        let mut hasher = Sha256::new();
        hasher.update(uid_string.as_bytes());
        let hash = hasher.finalize();
        // Hex-encode the hash (64 chars, well under 255 limit)
        let filename = format!("{:x}", hash);
        self.cache_dir.join(filename)
    }

    /// Try to get cached content for a revision.
    fn get(&self, revision_uid: &RevisionUid) -> Option<Vec<u8>> {
        let path = self.cache_path(revision_uid);
        if path.exists() {
            // Touch the file to update access time for LRU
            let _ = filetime::set_file_atime(&path, filetime::FileTime::now());
            std::fs::read(&path).ok()
        } else {
            None
        }
    }

    /// Store content in the cache.
    fn put(&self, revision_uid: &RevisionUid, content: &[u8]) -> Result<()> {
        // Check if we need to evict
        self.maybe_evict(content.len() as u64)?;
        
        let path = self.cache_path(revision_uid);
        tracing::debug!("Writing {} bytes to cache: {:?}", content.len(), path);
        std::fs::write(&path, content)
            .with_context(|| format!("Failed to write cache file: {:?}", path))?;
        
        Ok(())
    }

    /// Evict old files if cache would exceed max size.
    fn maybe_evict(&self, new_size: u64) -> Result<()> {
        let mut entries: Vec<(std::path::PathBuf, u64, std::time::SystemTime)> = Vec::new();
        let mut total_size: u64 = 0;

        // Collect all cache entries with their sizes and access times
        if let Ok(dir) = std::fs::read_dir(&self.cache_dir) {
            for entry in dir.flatten() {
                if let Ok(metadata) = entry.metadata() {
                    let atime = metadata.accessed().unwrap_or(std::time::UNIX_EPOCH);
                    let size = metadata.len();
                    total_size += size;
                    entries.push((entry.path(), size, atime));
                }
            }
        }

        // If adding new_size would exceed limit, evict oldest files
        if total_size + new_size > self.max_size {
            // Sort by access time (oldest first)
            entries.sort_by_key(|(_, _, atime)| *atime);
            
            let mut freed: u64 = 0;
            let target_free = (total_size + new_size).saturating_sub(self.max_size) + (self.max_size / 10); // Free extra 10%

            for (path, size, _) in entries {
                if freed >= target_free {
                    break;
                }
                if std::fs::remove_file(&path).is_ok() {
                    freed += size;
                    tracing::debug!("Evicted cache file: {:?}", path);
                }
            }
        }

        Ok(())
    }

    /// Get current cache size.
    #[allow(dead_code)]
    fn size(&self) -> u64 {
        let mut total: u64 = 0;
        if let Ok(dir) = std::fs::read_dir(&self.cache_dir) {
            for entry in dir.flatten() {
                if let Ok(metadata) = entry.metadata() {
                    total += metadata.len();
                }
            }
        }
        total
    }
}

/// Pending file being written (not yet committed to Proton Drive).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PendingFile {
    /// Parent folder inode (unused but kept for debugging)
    #[allow(dead_code)]
    parent_inode: u64,
    /// Parent folder NodeUid
    parent_uid: NodeUid,
    /// File name
    name: String,
    /// MIME type
    mime_type: String,
    /// File content being buffered
    content: Vec<u8>,
    /// Creation time (unused but kept for debugging)
    #[allow(dead_code)]
    creation_time: DateTime<Utc>,
    /// Whether content has been modified since last upload
    dirty: bool,
}

/// Persistent upload data saved to disk for resume capability.
/// Maximum number of retries before giving up on a persistent upload.
const MAX_UPLOAD_RETRIES: u32 = 3;

/// Check if an error message indicates a transient/network failure that should be retried.
/// Returns true if the error is clearly a network/connection issue.
fn is_transient_error(error_msg: &str) -> bool {
    let transient_patterns = [
        "error sending request",  // reqwest connection errors
        "connection refused",
        "connection reset",
        "connection closed",
        "timed out",
        "timeout",
        "temporarily unavailable",
        "service unavailable",
        "503",  // Service Unavailable
        "502",  // Bad Gateway
        "504",  // Gateway Timeout
        "network",
        "dns",
        "resolve",
        "socket",
        "broken pipe",
        "connection aborted",
        "API error 429",  // Rate limited - should retry after delay
        "too many requests",
    ];
    
    let error_lower = error_msg.to_lowercase();
    transient_patterns.iter().any(|p| error_lower.contains(p))
}

/// Check if an error message indicates a permanent failure that should not be retried.
/// Returns true if the upload should be removed from the pending queue.
fn is_permanent_upload_error(error_msg: &str) -> bool {
    // First check if it's clearly a transient error - these should always be retried
    if is_transient_error(error_msg) {
        return false;
    }
    
    // API error codes that indicate permanent failures
    let permanent_patterns = [
        "API error 2500", // AlreadyExists - file with same name exists
        "API error 2501", // DoesNotExist - parent folder doesn't exist
        "API error 2000", // InvalidRequirements
        "API error 2001", // InvalidValue
        "API error 2011", // NotEnoughPermissions
        "API error 200001", // InsufficientQuota
        "API error 200002", // InsufficientSpace
        "API error 200003", // MaxFileSizeForFreeUser
        "API error 200300", // TooManyChildren
        "API error 200301", // NestingTooDeep
        "already exists", // Common error message pattern
        "not enough space", // Quota errors
        "quota exceeded", // Quota errors
        "permission denied", // Permission errors
    ];
    
    let error_lower = error_msg.to_lowercase();
    permanent_patterns.iter().any(|p| error_lower.contains(&p.to_lowercase()))
}

/// Classify a download error and return a human-readable description and styled icon.
/// This helps users understand whether a file is corrupted, has network issues, etc.
fn classify_download_error(error_msg: &str) -> (&'static str, console::StyledObject<&'static str>) {
    let error_lower = error_msg.to_lowercase();
    
    // Decryption/integrity failures - file is corrupted or was uploaded incorrectly
    if error_lower.contains("invalid mdc") 
        || error_lower.contains("mdc mismatch")
        || error_lower.contains("decrypt")
        || error_lower.contains("decryption")
        || error_lower.contains("integrity") {
        return ("corrupted - cannot decrypt", style("✗").red());
    }
    
    // Session key / crypto key issues
    if error_lower.contains("session key")
        || error_lower.contains("wrong key")
        || error_lower.contains("key mismatch") {
        return ("key error - cannot decrypt", style("✗").red());
    }
    
    // Signature verification failures
    if error_lower.contains("signature")
        || error_lower.contains("verification failed") {
        return ("signature invalid", style("✗").red());
    }
    
    // Network errors - transient, could retry
    if is_transient_error(&error_lower) {
        return ("network error", style("⚠").yellow());
    }
    
    // Block not found / storage errors
    if error_lower.contains("not found")
        || error_lower.contains("404")
        || error_lower.contains("block") {
        return ("file missing from storage", style("✗").red());
    }
    
    // Auth/permission errors
    if error_lower.contains("unauthorized")
        || error_lower.contains("forbidden")
        || error_lower.contains("401")
        || error_lower.contains("403") {
        return ("access denied", style("✗").red());
    }
    
    // Default fallback
    ("download error", style("✗").red())
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum PersistentUpload {
    /// Upload a new file
    NewFile {
        id: String,
        parent_uid: NodeUid,
        name: String,
        mime_type: String,
        content: Vec<u8>,
        /// Number of retry attempts
        #[serde(default)]
        retry_count: u32,
        /// Timestamp when the upload was created (for stale detection)
        #[serde(default = "default_timestamp")]
        created_at: i64,
    },
    /// Upload a new revision of an existing file  
    NewRevision {
        id: String,
        revision_uid: RevisionUid,
        filename: String,
        content: Vec<u8>,
        /// Number of retry attempts
        #[serde(default)]
        retry_count: u32,
        /// Timestamp when the upload was created (for stale detection)
        #[serde(default = "default_timestamp")]
        created_at: i64,
    },
}

/// Default timestamp function for serde
fn default_timestamp() -> i64 {
    chrono::Utc::now().timestamp()
}

impl PersistentUpload {
    fn id(&self) -> &str {
        match self {
            PersistentUpload::NewFile { id, .. } => id,
            PersistentUpload::NewRevision { id, .. } => id,
        }
    }
    
    fn name(&self) -> &str {
        match self {
            PersistentUpload::NewFile { name, .. } => name,
            PersistentUpload::NewRevision { filename, .. } => filename,
        }
    }
    
    fn retry_count(&self) -> u32 {
        match self {
            PersistentUpload::NewFile { retry_count, .. } => *retry_count,
            PersistentUpload::NewRevision { retry_count, .. } => *retry_count,
        }
    }
    
    fn increment_retry(&mut self) {
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
    
    /// Check if this upload is stale (older than 24 hours)
    fn is_stale(&self) -> bool {
        let now = chrono::Utc::now().timestamp();
        let age_hours = (now - self.created_at()) / 3600;
        age_hours > 24
    }
}

/// Persistent upload queue that survives app restarts.
struct PendingUploadStore {
    store_dir: PathBuf,
}

impl PendingUploadStore {
    fn new() -> Result<Self> {
        let config_dir = dirs::config_dir()
            .ok_or_else(|| anyhow::anyhow!("No config directory"))?
            .join("pdcli")
            .join("pending_uploads");
        
        std::fs::create_dir_all(&config_dir)
            .context("Failed to create pending uploads directory")?;
        
        Ok(Self { store_dir: config_dir })
    }
    
    /// Generate a unique ID for an upload.
    fn generate_id() -> String {
        use std::time::{SystemTime, UNIX_EPOCH};
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!("{:x}", timestamp)
    }
    
    /// Save a pending upload to disk.
    fn save(&self, upload: &PersistentUpload) -> Result<()> {
        let path = self.store_dir.join(format!("{}.json", upload.id()));
        let data = serde_json::to_vec(upload)
            .context("Failed to serialize upload")?;
        std::fs::write(&path, data)
            .with_context(|| format!("Failed to write upload file: {:?}", path))?;
        tracing::debug!("Saved pending upload: {}", upload.id());
        Ok(())
    }
    
    /// Remove a completed upload from disk.
    fn remove(&self, id: &str) -> Result<()> {
        let path = self.store_dir.join(format!("{}.json", id));
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("Failed to remove upload file: {:?}", path))?;
            tracing::debug!("Removed completed upload: {}", id);
        }
        Ok(())
    }
    
    /// Load all pending uploads from disk.
    fn load_all(&self) -> Result<Vec<PersistentUpload>> {
        let mut uploads = Vec::new();
        
        if let Ok(entries) = std::fs::read_dir(&self.store_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().map(|e| e == "json").unwrap_or(false) {
                    match std::fs::read(&path) {
                        Ok(data) => {
                            match serde_json::from_slice::<PersistentUpload>(&data) {
                                Ok(upload) => {
                                    tracing::info!("Found pending upload: {} ({:?})", 
                                        upload.id(), 
                                        match &upload {
                                            PersistentUpload::NewFile { name, .. } => name.clone(),
                                            PersistentUpload::NewRevision { filename, .. } => filename.clone(),
                                        }
                                    );
                                    uploads.push(upload);
                                }
                                Err(e) => {
                                    tracing::warn!("Failed to parse upload file {:?}: {}", path, e);
                                }
                            }
                        }
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

/// Write buffer for an open file handle.
#[derive(Debug)]
struct WriteBuffer {
    /// The inode being written
    inode: u64,
    /// Whether this is a new file (not yet on Proton)
    is_new: bool,
    /// Write offset (for detecting out-of-order writes)
    offset: u64,
    /// Buffered content
    content: Vec<u8>,
    /// Whether the buffer has been modified
    dirty: bool,
}

/// Background upload task.
#[derive(Debug)]
enum UploadTask {
    /// Upload a new file
    NewFile {
        inode: u64,
        pending: PendingFile,
        /// Persistence ID for resume capability
        persist_id: Option<String>,
    },
    /// Upload a new revision of an existing file
    NewRevision {
        inode: u64,
        revision_uid: RevisionUid,
        filename: String,
        content: Vec<u8>,
        /// Persistence ID for resume capability
        persist_id: Option<String>,
    },
    /// Resume a persisted upload (no inode, was saved from previous session)
    ResumePersisted(PersistentUpload),
}

/// Internal filesystem state.
struct ProtonDriveFsInner {
    /// Map from inode to node
    nodes: BTreeMap<u64, FsNode>,
    /// Map from NodeUid to inode
    uid_to_inode: BTreeMap<String, u64>,
    /// Map from LinkId to inode (for event processing)
    link_id_to_inode: BTreeMap<String, u64>,
    /// Children mapping: parent_inode -> child_inodes
    children: BTreeMap<u64, Vec<u64>>,
    /// Set of folders whose children have been loaded
    loaded_folders: std::collections::HashSet<u64>,
    /// File content cache: inode -> decrypted content
    file_cache: BTreeMap<u64, Vec<u8>>,
    /// Next inode number to allocate
    next_inode: AtomicU64,
    /// Root folder NodeUid
    root_uid: Option<NodeUid>,
    /// Volume ID
    volume_id: Option<VolumeId>,
    /// Next file handle to allocate
    next_fh: AtomicU64,
    /// File handles opened with O_NOATIME (likely thumbnailing)
    noatime_handles: std::collections::HashSet<u64>,
    /// Pending files being created (inode -> PendingFile)
    pending_files: BTreeMap<u64, PendingFile>,
    /// Write buffers for open file handles (fh -> WriteBuffer)
    write_buffers: BTreeMap<u64, WriteBuffer>,
    /// Mapping from file handle to inode for write tracking
    fh_to_inode: BTreeMap<u64, u64>,
    /// Last processed event ID for polling
    last_event_id: Option<String>,
}

impl ProtonDriveFsInner {
    fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            uid_to_inode: BTreeMap::new(),
            link_id_to_inode: BTreeMap::new(),
            children: BTreeMap::new(),
            loaded_folders: std::collections::HashSet::new(),
            file_cache: BTreeMap::new(),
            next_inode: AtomicU64::new(FIRST_DYNAMIC_INODE), // Virtual inodes 1-99 reserved
            root_uid: None,
            volume_id: None,
            next_fh: AtomicU64::new(1),
            noatime_handles: std::collections::HashSet::new(),
            pending_files: BTreeMap::new(),
            write_buffers: BTreeMap::new(),
            fh_to_inode: BTreeMap::new(),
            last_event_id: None,
        }
    }

    fn alloc_inode(&self) -> u64 {
        self.next_inode.fetch_add(1, Ordering::SeqCst)
    }

    fn get_or_create_inode(&mut self, uid: &NodeUid) -> u64 {
        let uid_str = uid.to_string();
        if let Some(&inode) = self.uid_to_inode.get(&uid_str) {
            inode
        } else {
            let inode = self.alloc_inode();
            self.uid_to_inode.insert(uid_str, inode);
            inode
        }
    }

    fn insert_node(&mut self, node: FsNode, parent_inode: Option<u64>) -> u64 {
        let uid = node.uid().clone();
        let link_id = uid.link_id.raw().to_string();
        let inode = self.get_or_create_inode(&uid);
        
        // Track by link_id for event processing
        self.link_id_to_inode.insert(link_id, inode);
        
        self.nodes.insert(inode, node);
        
        if let Some(parent) = parent_inode {
            self.children.entry(parent).or_default().push(inode);
        }
        
        inode
    }
}

/// The Proton Drive FUSE filesystem.
pub struct ProtonDriveFs {
    inner: Arc<RwLock<ProtonDriveFsInner>>,
    client: Arc<RwLock<Option<ProtonDriveClient>>>,
    disk_cache: DiskCache,
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
}

impl ProtonDriveFs {
    /// Create a new filesystem instance.
    pub fn new(multi_progress: Arc<MultiProgress>) -> Result<Self> {
        let ignore_manager = IgnoreManager::new()?;
        tracing::info!("Loaded global .pdclignore from {:?}", ignore_manager.global_ignore_path());
        
        let (upload_tx, upload_rx) = mpsc::unbounded_channel();
        let pending_upload_store = Arc::new(PendingUploadStore::new()?);
        
        Ok(Self {
            inner: Arc::new(RwLock::new(ProtonDriveFsInner::new())),
            client: Arc::new(RwLock::new(None)),
            disk_cache: DiskCache::new(MAX_DISK_CACHE_SIZE)?,
            multi_progress,
            ignore_manager: RwLock::new(ignore_manager),
            upload_tx,
            upload_rx: RwLock::new(Some(upload_rx)),
            pending_upload_store,
        })
    }

    /// Initialize with a ProtonDriveClient.
    pub async fn init_with_client(&self, client: ProtonDriveClient) -> Result<()> {
        // Get the root "My Files" folder
        let my_files_folder = client.get_my_files_folder().await
            .context("Failed to get My Files folder")?;

        let mut inner = self.inner.write().await;
        
        // Store volume ID and root UID (pointing to My Files)
        inner.volume_id = Some(my_files_folder.base.uid.volume_id.clone());
        inner.root_uid = Some(my_files_folder.base.uid.clone());
        
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
        let mut my_files_meta = ProtonFolderMetadata::from_folder_node(&my_files_folder, false);
        my_files_meta.name = "MyFiles".to_string(); // Override the name to show "MyFiles"
        let my_files_node = FsNode::Folder(my_files_meta);
        inner.uid_to_inode.insert(my_files_folder.base.uid.to_string(), MYFILES_INODE);
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
                                if is_permanent_upload_error(&error_msg) {
                                    pb.finish_with_message(format!("{} {} ({})", style("✗").red(), name, error_msg));
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
                                    pb.finish_with_message(format!("{} {} (network error, will retry)", style("⚠").yellow(), name));
                                    tracing::info!(
                                        "Network error for '{}': {} - will retry on next mount",
                                        name, error_msg
                                    );
                                    // Keep in persistent store for retry
                                } else {
                                    pb.finish_with_message(format!("{} {} (error, will retry)", style("⚠").yellow(), name));
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
                                            pb.finish_and_clear();
                                            
                                            // Remove from persistence store on success
                                            if let Some(id) = persist_id {
                                                if let Err(e) = pending_upload_store.remove(&id) {
                                                    tracing::warn!("Failed to remove persisted upload {}: {}", id, e);
                                                }
                                            }
                                            
                                            // Update node state
                                            if let Ok(potential_node) = client.get_node(node_uid.clone()).await {
                                                let mut inner = inner.write().await;
                                                inner.pending_files.remove(&inode);
                                                
                                                match &potential_node {
                                                    PotentialObject::Node(node) => {
                                                        let fs_node = FsNode::from_node(node);
                                                        inner.uid_to_inode.insert(node_uid.to_string(), inode);
                                                        inner.nodes.insert(inode, fs_node);
                                                        inner.file_cache.insert(inode, content);
                                                    }
                                                    PotentialObject::Degraded(degraded) => {
                                                        let fs_node = FsNode::from_degraded(degraded);
                                                        inner.uid_to_inode.insert(node_uid.to_string(), inode);
                                                        inner.nodes.insert(inode, fs_node);
                                                    }
                                                }
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
                            
                            // Helper to handle upload errors - shows appropriate message and removes on permanent error
                            let handle_revision_error = |persist_id: &Option<String>, name: &str, error: &anyhow::Error, pb: &ProgressBar| {
                                let error_msg = error.to_string();
                                if is_permanent_upload_error(&error_msg) {
                                    pb.finish_with_message(format!("{} {} ({})", style("✗").red(), name, error_msg));
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
                                    pb.finish_with_message(format!("{} {} (network error, will retry)", style("⚠").yellow(), name));
                                    tracing::info!(
                                        "Network error for revision '{}': {} - will retry on next mount",
                                        name, error_msg
                                    );
                                    // Keep in persistent store for retry
                                } else {
                                    pb.finish_with_message(format!("{} {} (error, will retry)", style("⚠").yellow(), name));
                                    tracing::warn!(
                                        "Unknown error for revision '{}': {} - will retry on next mount",
                                        name, error_msg
                                    );
                                    // Keep in persistent store for retry
                                }
                            };
                            
                            match client.get_file_revision_uploader(
                                revision_uid,
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
                                            pb.finish_and_clear();
                                            
                                            // Remove from persistence store on success
                                            if let Some(id) = persist_id {
                                                if let Err(e) = pending_upload_store.remove(&id) {
                                                    tracing::warn!("Failed to remove persisted upload {}: {}", id, e);
                                                }
                                            }
                                            
                                            // Update node state
                                            if let Ok(potential_node) = client.get_node(new_node_uid).await {
                                                let mut inner = inner.write().await;
                                                match &potential_node {
                                                    PotentialObject::Node(node) => {
                                                        let fs_node = FsNode::from_node(node);
                                                        inner.nodes.insert(inode, fs_node);
                                                        inner.file_cache.insert(inode, content);
                                                    }
                                                    PotentialObject::Degraded(degraded) => {
                                                        let fs_node = FsNode::from_degraded(degraded);
                                                        inner.nodes.insert(inode, fs_node);
                                                    }
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            handle_revision_error(&persist_id, &filename, &e, &pb);
                                        }
                                    }
                                }
                                Err(e) => {
                                    handle_revision_error(&persist_id, &filename, &e, &pb);
                                }
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
                                
                                // Check if this is a permanent error
                                if is_permanent_upload_error(&error_msg) {
                                    pb.finish_with_message(format!("{} {} ({})", style("✗").red(), name, error_msg));
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
                                        "{} {} (network error, retry {}/{})", 
                                        style("⚠").yellow(), name, persisted.retry_count(), MAX_UPLOAD_RETRIES
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
                                        "{} {} (error, retry {}/{})", 
                                        style("⚠").yellow(), name, persisted.retry_count(), MAX_UPLOAD_RETRIES
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
                                        parent_uid,
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
                                                Ok(_node_uid) => {
                                                    pb.finish_and_clear();
                                                    // Remove from persistence store on success
                                                    if let Err(e) = pending_upload_store.remove(&id) {
                                                        tracing::warn!("Failed to remove persisted upload {}: {}", id, e);
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
                                    match client.get_file_revision_uploader(
                                        revision_uid,
                                        size,
                                        Some(std::time::SystemTime::now()),
                                        None,
                                        None,
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
                                                Ok(_new_node_uid) => {
                                                    pb.finish_and_clear();
                                                    // Remove from persistence store on success
                                                    if let Err(e) = pending_upload_store.remove(&id) {
                                                        tracing::warn!("Failed to remove persisted upload {}: {}", id, e);
                                                    }
                                                    tracing::info!("Successfully resumed revision upload for '{}'", filename);
                                                }
                                                Err(e) => {
                                                    handle_upload_error(&id, &filename, &e, &mut persisted, &pb);
                                                }
                                            }
                                        }
                                        Err(e) => {
                                            handle_upload_error(&id, &filename, &e, &mut persisted, &pb);
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
                                if event_count > 0 {
                                    tracing::info!("Received {} events", event_count);
                                }
                                
                                // Check if full refresh needed
                                if response.refresh {
                                    tracing::warn!("Server requested full refresh - clearing cache");
                                    let mut inner_guard = inner.write().await;
                                    // Clear loaded folders to force re-fetch
                                    inner_guard.loaded_folders.clear();
                                    // Clear file cache
                                    inner_guard.file_cache.clear();
                                    inner_guard.last_event_id = Some(response.event_id);
                                    continue;
                                }
                                
                                // Process each event
                                for event in &response.events {
                                    if let Err(e) = Self::process_event(&inner, client, event).await {
                                        tracing::warn!("Failed to process event {}: {}", event.event_id, e);
                                    }
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
        
        Ok(())
    }
    
    /// Process a single volume event
    async fn process_event(
        inner: &Arc<RwLock<ProtonDriveFsInner>>,
        client: &ProtonDriveClient,
        event: &VolumeEventDto,
    ) -> Result<()> {
        let link_id = event.link.link_id.raw();
        let event_type = event.event_type();
        tracing::info!("Processing event {:?} for link {}", event_type, link_id);
        
        match event_type {
            Some(VolumeEventType::Create) => {
                // New node created - fetch and add it immediately
                let volume_id = {
                    let inner_guard = inner.read().await;
                    inner_guard.volume_id.clone()
                };
                
                let Some(volume_id) = volume_id else {
                    tracing::warn!("No volume_id available for Create event");
                    return Ok(());
                };
                
                // Fetch the new node
                let node_uid = NodeUid::new(volume_id, event.link.link_id.clone());
                match client.get_node(node_uid.clone()).await {
                    Ok(potential) => {
                        let fs_node = match &potential {
                            PotentialObject::Node(node) => FsNode::from_node(node),
                            PotentialObject::Degraded(degraded) => FsNode::from_degraded(degraded),
                        };
                        
                        let node_name = fs_node.name().to_string();
                        
                        let mut inner_guard = inner.write().await;
                        
                        // Find or create inode for this node
                        let inode = inner_guard.get_or_create_inode(&node_uid);
                        
                        // Add link_id mapping
                        inner_guard.link_id_to_inode.insert(link_id.to_string(), inode);
                        
                        // Add node to nodes map
                        inner_guard.nodes.insert(inode, fs_node);
                        
                        // Add to parent's children list
                        if let Some(parent_link_id) = &event.link.parent_link_id {
                            if let Some(&parent_inode) = inner_guard.link_id_to_inode.get(parent_link_id.raw()) {
                                if let Some(children) = inner_guard.children.get_mut(&parent_inode) {
                                    if !children.contains(&inode) {
                                        children.push(inode);
                                        tracing::info!("Added new node '{}' (inode {}) to parent {}", node_name, inode, parent_inode);
                                    }
                                } else {
                                    // Parent children list doesn't exist yet
                                    inner_guard.children.insert(parent_inode, vec![inode]);
                                    tracing::info!("Created children list for parent {} with new node '{}' (inode {})", parent_inode, node_name, inode);
                                }
                            } else {
                                // Parent not in cache yet - just invalidate so it gets loaded
                                tracing::debug!("Parent folder not in cache, node will appear when parent is accessed");
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Failed to fetch new node {}: {}", link_id, e);
                        // Fallback: invalidate parent folder
                        if let Some(parent_link_id) = &event.link.parent_link_id {
                            let mut inner_guard = inner.write().await;
                            if let Some(&parent_inode) = inner_guard.link_id_to_inode.get(parent_link_id.raw()) {
                                inner_guard.loaded_folders.remove(&parent_inode);
                            }
                        }
                    }
                }
            }
            Some(VolumeEventType::UpdateMetadata) | Some(VolumeEventType::UpdateContent) => {
                // Node updated - refresh if we have it cached
                let existing_inode = {
                    let inner_guard = inner.read().await;
                    inner_guard.link_id_to_inode.get(link_id).copied()
                };
                
                if let Some(inode) = existing_inode {
                    // Get volume_id from inner
                    let volume_id = {
                        let inner_guard = inner.read().await;
                        inner_guard.volume_id.clone()
                    };
                    
                    if let Some(volume_id) = volume_id {
                        // Fetch fresh node data
                        let node_uid = NodeUid::new(volume_id, event.link.link_id.clone());
                        match client.get_node(node_uid.clone()).await {
                            Ok(potential) => {
                                let fs_node = match &potential {
                                    PotentialObject::Node(node) => FsNode::from_node(node),
                                    PotentialObject::Degraded(degraded) => FsNode::from_degraded(degraded),
                                };
                                
                                let node_name = fs_node.name().to_string();
                                let is_content_update = matches!(event_type, Some(VolumeEventType::UpdateContent));
                                
                                let mut inner_guard = inner.write().await;
                                inner_guard.nodes.insert(inode, fs_node);
                                
                                // Clear file content cache for content updates
                                if is_content_update {
                                    inner_guard.file_cache.remove(&inode);
                                    tracing::info!("Updated content for '{}' (inode {})", node_name, inode);
                                } else {
                                    tracing::info!("Updated metadata for '{}' (inode {})", node_name, inode);
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to fetch updated node {}: {}", link_id, e);
                            }
                        }
                    }
                } else {
                    tracing::debug!("Update event for unknown node {} - ignoring", link_id);
                }
            }
            Some(VolumeEventType::Delete) => {
                // Node deleted or trashed
                let mut inner_guard = inner.write().await;
                if let Some(&inode) = inner_guard.link_id_to_inode.get(link_id) {
                    // Get the name before removing
                    let node_name = inner_guard.nodes.get(&inode)
                        .map(|n| n.name().to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    
                    // Remove from nodes
                    inner_guard.nodes.remove(&inode);
                    inner_guard.file_cache.remove(&inode);
                    inner_guard.children.remove(&inode);
                    
                    // Remove from parent's children list
                    if let Some(parent_link_id) = &event.link.parent_link_id {
                        if let Some(&parent_inode) = inner_guard.link_id_to_inode.get(parent_link_id.raw()) {
                            if let Some(siblings) = inner_guard.children.get_mut(&parent_inode) {
                                siblings.retain(|&i| i != inode);
                            }
                        }
                    }
                    
                    tracing::info!("Removed '{}' (inode {}) - deleted/trashed", node_name, inode);
                } else {
                    tracing::debug!("Delete event for unknown node {} - ignoring", link_id);
                }
            }
            None => {
                tracing::warn!("Unknown event type '{}' for link {}", event.event_type, link_id);
            }
        }
        
        Ok(())
    }

    /// Load children of a folder if not already loaded.
    async fn ensure_children_loaded(&self, inode: u64) -> Result<()> {
        // Check if already loaded
        {
            let inner = self.inner.read().await;
            if inner.loaded_folders.contains(&inode) {
                return Ok(());
            }
        }

        // Get the folder's NodeUid
        let folder_uid = {
            let inner = self.inner.read().await;
            match inner.nodes.get(&inode) {
                Some(FsNode::Folder(f)) => f.uid.clone(),
                Some(FsNode::Degraded(d)) if !d.is_file => d.uid.clone(),
                _ => return Err(anyhow::anyhow!("Not a folder")),
            }
        };

        // Get client
        let client = self.client.read().await;
        let client = client.as_ref().ok_or_else(|| anyhow::anyhow!("No client"))?;

        // Enumerate children
        let mut children_stream = pin!(client.enumerate_folder_children(folder_uid.clone()).await
            .context("Failed to enumerate children")?);

        let mut child_inodes = Vec::new();
        
        while let Some(result) = children_stream.next().await {
            let potential = result.context("Failed to fetch child")?;
            
            let fs_node = match &potential {
                PotentialObject::Node(node) => FsNode::from_node(node),
                PotentialObject::Degraded(degraded) => FsNode::from_degraded(degraded),
            };
            
            let child_inode = {
                let mut inner = self.inner.write().await;
                inner.insert_node(fs_node, None)
            };
            child_inodes.push(child_inode);
        }

        // Mark folder as loaded and store children
        {
            let mut inner = self.inner.write().await;
            inner.children.insert(inode, child_inodes);
            inner.loaded_folders.insert(inode);
        }

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
                tracing::debug!("Cache hit (memory) for inode {}", inode);
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
            tracing::debug!("Cache hit (disk) for inode {}", inode);
            // Also store in memory cache for faster subsequent access
            // And update the file size to match actual cached content
            {
                let mut inner = self.inner.write().await;
                inner.file_cache.insert(inode, content.clone());
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
            }
            Err(e) => {
                tracing::error!("Download task panicked for inode {}: {:?}", inode, e);
                pb.abandon_with_message(format!("{} {} (task panic)", style("✗").red(), filename));
            }
        }
        download_result
            .context("Download task panicked")?
            .context("Download failed")?;

        // Finish progress bar
        pb.finish_and_clear();

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

        // Store in memory cache
        {
            let mut inner = self.inner.write().await;
            inner.file_cache.insert(inode, content.clone());
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

        pb.finish_and_clear();

        // Fetch the uploaded node to get full metadata
        let potential_node = client.get_node(node_uid.clone()).await?;
        
        // Update the filesystem state
        let mut inner = self.inner.write().await;
        inner.pending_files.remove(&inode);
        
        // Update the node in our state
        match &potential_node {
            PotentialObject::Node(node) => {
                let fs_node = FsNode::from_node(node);
                inner.uid_to_inode.insert(node_uid.to_string(), inode);
                inner.nodes.insert(inode, fs_node);
                // Update cache with uploaded content
                inner.file_cache.insert(inode, pending.content);
            }
            PotentialObject::Degraded(degraded) => {
                let fs_node = FsNode::from_degraded(degraded);
                inner.uid_to_inode.insert(node_uid.to_string(), inode);
                inner.nodes.insert(inode, fs_node);
            }
        }

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

            pb.finish_and_clear();

            // Update the node with new revision info
            let potential_node = client.get_node(new_node_uid).await?;
            
            let mut inner = self.inner.write().await;
            match &potential_node {
                PotentialObject::Node(node) => {
                    let fs_node = FsNode::from_node(node);
                    inner.nodes.insert(inode, fs_node);
                    inner.file_cache.insert(inode, content);
                }
                PotentialObject::Degraded(degraded) => {
                    let fs_node = FsNode::from_degraded(degraded);
                    inner.nodes.insert(inode, fs_node);
                }
            }
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
            inner.file_cache.insert(inode, content.clone());
        }

        if is_new {
            // Queue upload of new file
            let pending = {
                let inner = self.inner.read().await;
                inner.pending_files.get(&inode).cloned()
            };
            
            if let Some(pending) = pending {
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
        } else {
            // Queue revision upload
            let revision_uid = self.get_revision_uid(inode).await;
            let filename = {
                let inner = self.inner.read().await;
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
                            if let Some(cached) = inner.file_cache.get(&child_inode) {
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

        Ok(ReplyEntry {
            ttl: TTL,
            attr: child.attr(inode),
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
        tracing::info!("FUSE getattr: inode={}, fh={:?}, flags={:#x}, uid={}, gid={}, pid={}",
            inode, fh, flags, req.uid, req.gid, req.pid);
        
        let node = self.get_node(inode).await
            .ok_or_else(Errno::new_not_exist)?;

        Ok(ReplyAttr {
            ttl: TTL,
            attr: node.attr(inode),
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
        
        // Get the filename for extension checking
        let filename = node.name();
        
        // Detect thumbnailing/preview requests:
        // O_NOATIME (0x40000) is ONLY used by thumbnail generators and file manager previews.
        // Normal applications opening files do NOT use this flag.
        // When you double-click a file, the viewer app opens it without O_NOATIME.
        let uses_noatime = (flags & O_NOATIME) != 0;
        let is_thumbnailer = is_thumbnailer_process(req.pid);
        
        // Block ALL O_NOATIME requests - they're never from legitimate user opens
        let should_block = uses_noatime || is_thumbnailer;
        
        // Log process info for debugging
        let comm_path = format!("/proc/{}/comm", req.pid);
        let proc_name = std::fs::read_to_string(&comm_path)
            .map(|s| s.trim().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        
        if should_block {
            tracing::debug!(
                "Blocking preview/thumbnail request: inode={}, file={}, process={} (pid {}), O_NOATIME={}",
                inode, filename, proc_name, req.pid, uses_noatime
            );
            self.inner.write().await.noatime_handles.insert(fh);
        } else {
            tracing::debug!(
                "Allowing open: inode={}, file={}, process={}, O_NOATIME={}, write_mode={}",
                inode, filename, proc_name, uses_noatime, is_write_mode
            );
            
            // Check if this is a pending file (created locally, not yet uploaded)
            let is_pending = {
                let inner = self.inner.read().await;
                inner.pending_files.contains_key(&inode)
            };
            
            // Pre-download the file content during open() to ensure correct file size.
            // This is critical because:
            // 1. getattr returns "claimed" size from Proton metadata (may be wrong)
            // 2. Applications call stat() after open() to get file size
            // 3. They then only read that many bytes
            // 4. If claimed size < actual size, file appears truncated/corrupt
            // By downloading here, we update metadata with the real size before any reads.
            // SKIP this for pending files - they have no remote content to download.
            let existing_content = if is_pending {
                // Pending file - use buffered content, no download needed
                let inner = self.inner.read().await;
                inner.pending_files.get(&inode)
                    .map(|p| p.content.clone())
            } else if !is_truncate {
                match self.get_file_content(inode).await {
                    Ok(content) => Some(content),
                    Err(e) => {
                        tracing::error!("Failed to pre-download file during open: {:?}", e);
                        return Err(Errno::from(libc::EIO));
                    }
                }
            } else {
                // O_TRUNC means start fresh
                Some(Vec::new())
            };
            
            // Set up write buffer if opened for writing
            if is_write_mode {
                let mut inner = self.inner.write().await;
                let content = existing_content.clone().unwrap_or_default();
                
                inner.write_buffers.insert(fh, WriteBuffer {
                    inode,
                    is_new: false,
                    offset: 0,
                    content: if is_truncate { Vec::new() } else { content },
                    dirty: is_truncate, // Truncated files are dirty
                });
                inner.fh_to_inode.insert(fh, inode);
                
                // If truncating, update the file size
                if is_truncate {
                    if let Some(FsNode::File(f)) = inner.nodes.get_mut(&inode) {
                        f.size = 0;
                    }
                    inner.file_cache.insert(inode, Vec::new());
                }
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
        tracing::info!("FUSE read: inode={}, fh={}, offset={}, size={}, uid={}, gid={}, pid={}",
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
        
        // Check if this is a thumbnailing request (opened with O_NOATIME)
        let is_thumbnailing = self.inner.read().await.noatime_handles.contains(&fh);
        
        // For thumbnailing requests, only serve cached content to avoid network downloads
        if is_thumbnailing {
            // Check memory cache first
            if let Some(content) = self.inner.read().await.file_cache.get(&inode).cloned() {
                let offset = offset as usize;
                let size = size as usize;
                if offset >= content.len() {
                    return Ok(ReplyData { data: Bytes::new() });
                }
                let end = std::cmp::min(offset + size, content.len());
                return Ok(ReplyData { data: Bytes::copy_from_slice(&content[offset..end]) });
            }
            
            // Check disk cache
            if let Some(revision_uid) = self.get_revision_uid(inode).await {
                if let Some(content) = self.disk_cache.get(&revision_uid) {
                    let offset = offset as usize;
                    let size = size as usize;
                    if offset >= content.len() {
                        return Ok(ReplyData { data: Bytes::new() });
                    }
                    let end = std::cmp::min(offset + size, content.len());
                    // Also store in memory cache
                    self.inner.write().await.file_cache.insert(inode, content.clone());
                    return Ok(ReplyData { data: Bytes::copy_from_slice(&content[offset..end]) });
                }
            }
            
            // Not cached - return empty for thumbnailing to avoid network download
            tracing::debug!("Skipping download for thumbnailing request (inode={})", inode);
            return Ok(ReplyData { data: Bytes::new() });
        }
        
        // Normal read - download file content (cached)
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
                if let Some(child) = inner.nodes.get(&child_inode) {
                    entries.push(Ok(DirectoryEntry {
                        inode: child_inode,
                        kind: child.file_type(),
                        name: OsString::from(child.name()),
                        offset: (i + 3) as i64,
                    }));
                }
            }
        }

        Ok(ReplyDirectory {
            entries: stream::iter(entries.into_iter().skip(offset as usize)),
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
        self.ensure_children_loaded(parent).await
            .map_err(|e| {
                tracing::error!("Failed to load children: {}", e);
                Errno::new_not_exist()
            })?;
        
        let inner = self.inner.read().await;
        
        let node = inner.nodes.get(&parent)
            .ok_or_else(Errno::new_not_exist)?;

        if !node.is_dir() {
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

        let mut entries = vec![
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
            for (i, &child_inode) in children.iter().enumerate() {
                if let Some(child) = inner.nodes.get(&child_inode) {
                    entries.push(Ok(DirectoryEntryPlus {
                        inode: child_inode,
                        generation: 0,
                        kind: child.file_type(),
                        name: OsString::from(child.name()),
                        offset: (i + 3) as i64,
                        attr: child.attr(child_inode),
                        entry_ttl: TTL,
                        attr_ttl: TTL,
                    }));
                }
            }
        }

        Ok(ReplyDirectoryPlus {
            entries: stream::iter(entries.into_iter().skip(offset as usize)),
        })
    }

    async fn access(&self, req: Request, inode: u64, mask: u32) -> fuse3::Result<()> {
        tracing::info!("FUSE access: inode={}, mask={:#o}, uid={}, gid={}, pid={}",
            inode, mask, req.uid, req.gid, req.pid);
        let _ = self.get_node(inode).await
            .ok_or_else(Errno::new_not_exist)?;
        Ok(())
    }

    async fn statfs(&self, req: Request, inode: u64) -> fuse3::Result<ReplyStatFs> {
        tracing::info!("FUSE statfs: inode={}, uid={}, gid={}, pid={}",
            inode, req.uid, req.gid, req.pid);
        // TODO: Get actual stats from Proton Drive API (storage quota, etc.)
        Ok(ReplyStatFs {
            blocks: 1024 * 1024 * 1024, // 4TB virtual
            bfree: 512 * 1024 * 1024,   // 2TB free
            bavail: 512 * 1024 * 1024,
            files: 1_000_000,
            ffree: 500_000,
            bsize: 4096,
            namelen: 255,
            frsize: 4096,
        })
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
            inner.noatime_handles.remove(&fh);
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

        // Check if the file name should be ignored (but allow .pdclignore files)
        if self.is_ignored(parent, &name_str).await {
            tracing::warn!("File '{}' matches .pdclignore pattern, rejecting creation", name_str);
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

        // Trash the file via SDK
        let client_guard = self.client.read().await;
        let client = client_guard.as_ref()
            .ok_or_else(|| {
                tracing::error!("No client available");
                Errno::from(libc::EIO)
            })?;

        let results = client.trash_nodes(vec![node_uid.clone()]).await
            .map_err(|e| {
                tracing::error!("Failed to trash file '{}': {}", name_str, e);
                Errno::from(libc::EIO)
            })?;

        // Check result
        if let Some(Err(e)) = results.get(&node_uid) {
            tracing::error!("Failed to trash file '{}': {}", name_str, e);
            return Err(Errno::from(libc::EIO));
        }

        // Remove from filesystem state
        self.remove_node(parent, child_inode).await;

        tracing::info!("Trashed file '{}'", name_str);
        Ok(())
    }

    async fn rmdir(&self, req: Request, parent: u64, name: &OsStr) -> fuse3::Result<()> {
        let name_str = name.to_string_lossy().to_string();
        tracing::info!("FUSE rmdir: parent={}, name={}, uid={}, gid={}, pid={}",
            parent, name_str, req.uid, req.gid, req.pid);

        // Find the child
        let (child_inode, child_node) = self.find_child(parent, name).await
            .ok_or_else(Errno::new_not_exist)?;

        if !child_node.is_dir() {
            return Err(Errno::new_is_not_dir());
        }

        // Get node UID for API call
        let node_uid = child_node.uid().clone();

        // Trash the folder via SDK (Proton Drive handles recursive deletion server-side)
        let client_guard = self.client.read().await;
        let client = client_guard.as_ref()
            .ok_or_else(|| {
                tracing::error!("No client available");
                Errno::from(libc::EIO)
            })?;

        let results = client.trash_nodes(vec![node_uid.clone()]).await
            .map_err(|e| {
                tracing::error!("Failed to trash folder '{}': {}", name_str, e);
                Errno::from(libc::EIO)
            })?;

        // Check result
        if let Some(Err(e)) = results.get(&node_uid) {
            tracing::error!("Failed to trash folder '{}': {}", name_str, e);
            return Err(Errno::from(libc::EIO));
        }

        // Remove from filesystem state (including any cached children)
        {
            let mut inner = self.inner.write().await;
            
            // Recursively remove children from local state
            fn remove_children_recursive(inner: &mut ProtonDriveFsInner, inode: u64) {
                if let Some(children) = inner.children.remove(&inode) {
                    for child_inode in children {
                        remove_children_recursive(inner, child_inode);
                        if let Some(node) = inner.nodes.remove(&child_inode) {
                            inner.uid_to_inode.remove(&node.uid().to_string());
                        }
                        inner.file_cache.remove(&child_inode);
                        inner.loaded_folders.remove(&child_inode);
                    }
                }
            }
            
            // Remove children first
            remove_children_recursive(&mut inner, child_inode);
            
            // Remove the folder itself
            if let Some(node) = inner.nodes.remove(&child_inode) {
                inner.uid_to_inode.remove(&node.uid().to_string());
            }
            inner.loaded_folders.remove(&child_inode);
            
            // Remove from parent's children list
            if let Some(parent_children) = inner.children.get_mut(&parent) {
                parent_children.retain(|&c| c != child_inode);
            }
        }

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
                inner.file_cache.insert(inode, Vec::new());
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

/// O_NOATIME flag value (0x40000) - used by thumbnail generators
const O_NOATIME: u32 = 0x40000;

/// FOPEN_DIRECT_IO flag (1 << 0) - bypass page cache for this open file.
/// This ensures the kernel doesn't cache file size from getattr, which may differ
/// from the actual downloaded size. Without this, files may appear truncated.
const FOPEN_DIRECT_IO: u32 = 1 << 0;

/// Check if a PID is a known thumbnailer/preview process.
/// Returns true if the process should be blocked from downloading.
/// Only blocks actual thumbnailer processes, not legitimate viewers.
fn is_thumbnailer_process(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    
    // Only block actual thumbnailer binaries - be conservative!
    // Do NOT block image viewers like eog, loupe, gwenview, etc.
    const THUMBNAILER_EXACT: &[&str] = &[
        "gnome-desktop-thu",  // gnome-desktop-thumbnailer (truncated at 15 chars)
        "evince-thumbnailer",
        "totem-video-thumb",
        "gdk-pixbuf-thumbn",
        "gs-thumbnailer",
        "raw-thumbnailer",
        "tumbler",
        "tracker-extract",
        "tracker-miner",
    ];
    
    // Check /proc/pid/comm first (fast path)
    let comm_path = format!("/proc/{}/comm", pid);
    if let Ok(comm) = std::fs::read_to_string(&comm_path) {
        let comm = comm.trim().to_lowercase();
        
        // Only block if name contains "thumbnailer" or "thumbnail"
        if comm.contains("thumbnailer") || comm.contains("thumbnail") {
            tracing::debug!("Blocking thumbnailer (comm): {} (pid {})", comm, pid);
            return true;
        }
        
        // Check exact matches for known thumbnailers
        for pattern in THUMBNAILER_EXACT {
            if comm == *pattern {
                tracing::debug!("Blocking thumbnailer (exact): {} (pid {})", comm, pid);
                return true;
            }
        }
    }
    
    // Check /proc/pid/cmdline for thumbnailer arguments
    let cmdline_path = format!("/proc/{}/cmdline", pid);
    if let Ok(cmdline) = std::fs::read_to_string(&cmdline_path) {
        let cmdline = cmdline.replace('\0', " ").to_lowercase();
        // Block if being used specifically for thumbnailing
        if cmdline.contains("--thumbnail") || 
           cmdline.contains("-thumbnail") ||
           cmdline.contains("thumbnailer") {
            tracing::debug!("Blocking thumbnailer (cmdline): {} (pid {})", 
                cmdline.chars().take(80).collect::<String>(), pid);
            return true;
        }
    }
    
    false
}

/// Progress callback type for mount status updates.
pub type ProgressCallback = Box<dyn Fn(&str) + Send + Sync>;

/// Clear the file download cache.
/// 
/// This removes all cached file contents from disk but keeps the session intact.
pub fn clear_cache() -> Result<()> {
    use console::style;
    use indicatif::{ProgressBar, ProgressStyle};
    
    let cache_dir = dirs::cache_dir()
        .context("Could not determine cache directory")?
        .join("pdcli")
        .join("files");
    
    if !cache_dir.exists() {
        println!("{}", style("Cache is already empty.").yellow());
        return Ok(());
    }
    
    // Count files and size
    let mut file_count = 0u64;
    let mut total_size = 0u64;
    
    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    file_count += 1;
                    total_size += metadata.len();
                }
            }
        }
    }
    
    if file_count == 0 {
        println!("{}", style("Cache is already empty.").yellow());
        return Ok(());
    }
    
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap()
    );
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));
    spinner.set_message(format!("Clearing {} cached files ({})...", 
        file_count,
        humanize_size(total_size)
    ));
    
    // Remove all files in cache directory
    if let Ok(entries) = std::fs::read_dir(&cache_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_file() {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    
    spinner.finish_with_message(format!(
        "{} Cleared {} cached files ({})",
        style("✓").green().bold(),
        file_count,
        humanize_size(total_size)
    ));
    
    Ok(())
}

/// Clear all pending uploads from the queue.
/// 
/// This removes all uploads that were waiting to be retried,
/// including those that failed with errors.
pub fn clear_pending_uploads() -> Result<()> {
    use console::style;
    use indicatif::{ProgressBar, ProgressStyle};
    
    let pending_dir = dirs::config_dir()
        .context("Could not determine config directory")?
        .join("pdcli")
        .join("pending_uploads");
    
    if !pending_dir.exists() {
        println!("{}", style("No pending uploads.").yellow());
        return Ok(());
    }
    
    // Count files and gather info
    let mut file_count = 0u64;
    let mut upload_names: Vec<String> = Vec::new();
    
    if let Ok(entries) = std::fs::read_dir(&pending_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                if let Ok(metadata) = entry.metadata() {
                    if metadata.is_file() {
                        file_count += 1;
                        
                        // Try to read the file name from the JSON
                        if let Ok(data) = std::fs::read(&path) {
                            if let Ok(upload) = serde_json::from_slice::<PersistentUpload>(&data) {
                                upload_names.push(upload.name().to_string());
                            }
                        }
                    }
                }
            }
        }
    }
    
    if file_count == 0 {
        println!("{}", style("No pending uploads.").yellow());
        return Ok(());
    }
    
    // Show what will be cleared
    println!();
    println!("{}", style("Pending uploads to be cleared:").bold());
    for name in &upload_names {
        println!("  {} {}", style("•").dim(), name);
    }
    println!();
    
    let spinner = ProgressBar::new_spinner();
    spinner.set_style(
        ProgressStyle::default_spinner()
            .template("{spinner:.cyan} {msg}")
            .unwrap()
    );
    spinner.enable_steady_tick(std::time::Duration::from_millis(80));
    spinner.set_message(format!("Clearing {} pending uploads...", file_count));
    
    // Remove all files in pending uploads directory
    if let Ok(entries) = std::fs::read_dir(&pending_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().map(|e| e == "json").unwrap_or(false) {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    
    spinner.finish_with_message(format!(
        "{} Cleared {} pending uploads",
        style("✓").green().bold(),
        file_count
    ));
    
    Ok(())
}

/// Format bytes as human-readable size
fn humanize_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    
    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

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

    let mut mount_options = MountOptions::default();
    mount_options
        .fs_name("proton-drive")
        .force_readdir_plus(true)
        .uid(uid)
        .gid(gid);
        // Removed read_only - now supports write operations

    report("Connecting to Proton Drive...");

    // Create ProtonDriveClient from session
    let client = ProtonDriveClient::new(session, None)
        .context("Failed to create Proton Drive client")?;

    report("Loading root folder...");

    // Create multi-progress bar for download tracking
    let multi_progress = Arc::new(MultiProgress::new());

    // Create and initialize filesystem
    let fs = ProtonDriveFs::new(multi_progress.clone())
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

    let mount_path_owned = mount_path.to_owned();

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
