use anyhow::{Context, Result};
use proton_drive_sdk::node::revision::RevisionUid;
use sha2::{Digest, Sha256};

/// Maximum disk cache size in bytes (default 1GB).
pub(super) const MAX_DISK_CACHE_SIZE: u64 = 1024 * 1024 * 1024;

/// Persistent disk cache for downloaded files.
/// Files are cached by revision UID so updated files are re-downloaded.
pub(super) struct DiskCache {
    cache_dir: std::path::PathBuf,
    max_size: u64,
}

impl DiskCache {
    pub(super) fn new(max_size: u64) -> Result<Self> {
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
        // Hash the revision UID to get a short, filesystem-safe filename.
        let uid_string = revision_uid.to_string();
        let mut hasher = Sha256::new();
        hasher.update(uid_string.as_bytes());
        let hash = hasher.finalize();
        let filename = format!("{:x}", hash);
        self.cache_dir.join(filename)
    }

    /// Check if content is cached without reading it.
    pub(super) fn contains(&self, revision_uid: &RevisionUid) -> bool {
        self.cache_path(revision_uid).exists()
    }

    /// Try to get cached content for a revision.
    pub(super) fn get(&self, revision_uid: &RevisionUid) -> Option<Vec<u8>> {
        let path = self.cache_path(revision_uid);
        if path.exists() {
            let _ = filetime::set_file_atime(&path, filetime::FileTime::now());
            std::fs::read(&path).ok()
        } else {
            None
        }
    }

    /// Store content in the cache.
    pub(super) fn put(&self, revision_uid: &RevisionUid, content: &[u8]) -> Result<()> {
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

        if total_size + new_size > self.max_size {
            entries.sort_by_key(|(_, _, atime)| *atime);

            let mut freed: u64 = 0;
            let target_free =
                (total_size + new_size).saturating_sub(self.max_size) + (self.max_size / 10);

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
    pub(super) fn size(&self) -> u64 {
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

/// Maximum memory cache size (128 MB).
const MAX_MEMORY_CACHE_SIZE: usize = 128 * 1024 * 1024;
/// Maximum file size to cache in memory (16 MB) - larger files go straight to disk.
const MAX_MEMORY_CACHE_ITEM_SIZE: usize = 16 * 1024 * 1024;
/// Maximum number of entries in memory cache.
const MAX_MEMORY_CACHE_ENTRIES: usize = 1000;

/// Bounded LRU memory cache for file content.
/// Only caches files smaller than MAX_MEMORY_CACHE_ITEM_SIZE.
/// Evicts oldest entries when total size exceeds MAX_MEMORY_CACHE_SIZE.
pub(super) struct MemoryCache {
    cache: lru::LruCache<u64, Vec<u8>>,
    current_size: usize,
}

impl MemoryCache {
    pub(super) fn new() -> Self {
        Self {
            cache: lru::LruCache::new(
                std::num::NonZeroUsize::new(MAX_MEMORY_CACHE_ENTRIES).unwrap(),
            ),
            current_size: 0,
        }
    }

    /// Get cached content for an inode.
    pub(super) fn get(&mut self, inode: &u64) -> Option<&Vec<u8>> {
        self.cache.get(inode)
    }

    /// Get cached content without promoting it in LRU order (read-only).
    pub(super) fn peek(&self, inode: &u64) -> Option<&Vec<u8>> {
        self.cache.peek(inode)
    }

    /// Check if content is cached without promoting it in LRU order.
    pub(super) fn contains(&self, inode: &u64) -> bool {
        self.cache.contains(inode)
    }

    /// Store content in the cache. Returns false if content is too large.
    pub(super) fn put(&mut self, inode: u64, content: Vec<u8>) -> bool {
        let content_size = content.len();

        if content_size > MAX_MEMORY_CACHE_ITEM_SIZE {
            tracing::trace!(
                "Not caching inode {} in memory: size {} exceeds limit {}",
                inode,
                content_size,
                MAX_MEMORY_CACHE_ITEM_SIZE
            );
            return false;
        }

        if let Some(old) = self.cache.pop(&inode) {
            self.current_size = self.current_size.saturating_sub(old.len());
        }

        while self.current_size + content_size > MAX_MEMORY_CACHE_SIZE {
            if let Some((evicted_inode, evicted_content)) = self.cache.pop_lru() {
                self.current_size = self.current_size.saturating_sub(evicted_content.len());
                tracing::debug!(
                    "Evicted inode {} from memory cache (freed {} bytes, current_size={})",
                    evicted_inode,
                    evicted_content.len(),
                    self.current_size
                );
            } else {
                break;
            }
        }

        self.current_size += content_size;
        self.cache.put(inode, content);
        true
    }

    /// Remove an inode from the cache.
    pub(super) fn remove(&mut self, inode: &u64) {
        if let Some(content) = self.cache.pop(inode) {
            self.current_size = self.current_size.saturating_sub(content.len());
        }
    }

    /// Clear all cached content.
    pub(super) fn clear(&mut self) {
        self.cache.clear();
        self.current_size = 0;
    }

    /// Get current memory usage.
    #[allow(dead_code)]
    pub(super) fn memory_usage(&self) -> usize {
        self.current_size
    }
}
