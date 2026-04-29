use std::collections::HashMap;
use std::ffi::OsStr;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation,
    INodeNo, MountOption, OpenAccMode, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory,
    ReplyEmpty, ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
};
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;
use sha2::{Sha256, Digest};

use crate::app::ProtonDrive;
use crate::db::{FuseDb, InodeRow};
use crate::thumbnail::ThumbnailConfig;
use crate::transfer::{TransferDirection, TransferTracker};

const TTL: Duration = Duration::from_secs(1);
const ROOT_INO: u64 = 1;
const BLOCK_SIZE: u32 = 4096;

/// Global mountpoint path so signal handlers / panic hooks can unmount.
static MOUNT_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Online/offline state — updated by the event poll loop.
static ONLINE: AtomicBool = AtomicBool::new(true);

/// Returns whether the client currently has connectivity to the Proton API.
pub fn is_online() -> bool {
    ONLINE.load(Ordering::Relaxed)
}

/// Best-effort unmount via fusermount. Safe to call from signal handlers
/// (spawns a process — technically not async-signal-safe, but this runs
/// right before exit so it's the pragmatic choice).
pub fn force_unmount() {
    if let Some(path) = MOUNT_PATH.get() {
        let p = path.to_string_lossy();
        // Try fusermount3 first (modern), then fusermount (legacy).
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", "-z", &*p])
            .status()
            .or_else(|_| {
                std::process::Command::new("fusermount")
                    .args(["-u", "-z", &*p])
                    .status()
            });
    }
}

// ── Open-file bookkeeping ────────────────────────────────────────────

#[allow(dead_code)]
struct OpenFile {
    ino: u64,
    file: std::fs::File,
    writable: bool,
}

// ── The FUSE filesystem ──────────────────────────────────────────────

pub struct ProtonDriveFs {
    db: Arc<Mutex<FuseDb>>,
    cache_dir: PathBuf,
    drive: ProtonDriveClient,
    rt: tokio::runtime::Handle,
    next_fh: AtomicU64,
    open_files: RwLock<HashMap<u64, Mutex<OpenFile>>>,
    uid: u32,
    gid: u32,
    tracker: TransferTracker,
    thumb_config: ThumbnailConfig,
}

impl ProtonDriveFs {
    pub fn new(
        db: FuseDb,
        cache_dir: PathBuf,
        drive: ProtonDriveClient,
        rt: tokio::runtime::Handle,
        tracker: TransferTracker,
    ) -> Self {
        let thumb_config = ThumbnailConfig::load();
        Self {
            db: Arc::new(Mutex::new(db)),
            cache_dir,
            drive,
            rt,
            next_fh: AtomicU64::new(1),
            open_files: RwLock::new(HashMap::new()),
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            tracker,
            thumb_config,
        }
    }

    /// Start background tasks for event polling and journal flushing.
    pub fn spawn_background_workers(&self) {
        let db = self.db.clone();
        let drive = self.drive.clone();
        let cache_dir = self.cache_dir.clone();

        // Event poller
        self.rt.spawn(event_poll_loop(db.clone(), drive.clone()));

        // Journal flusher
        self.rt
            .spawn(journal_flush_loop(db, drive, cache_dir));
    }

    // ── Helpers ──────────────────────────────────────────────────────

    fn inode_to_attr(&self, row: &InodeRow) -> FileAttr {
        let kind = if row.is_dir {
            FileType::Directory
        } else {
            FileType::RegularFile
        };
        let perm = if row.is_dir { 0o755 } else { 0o644 };
        let mtime = UNIX_EPOCH + Duration::from_secs(row.mtime.max(0) as u64);
        let ctime = UNIX_EPOCH + Duration::from_secs(row.ctime.max(0) as u64);
        let nlink = if row.is_dir { 2 } else { 1 };
        let blocks = (row.size + 511) / 512;

        FileAttr {
            ino: INodeNo(row.ino),
            size: row.size,
            blocks,
            atime: mtime,
            mtime,
            ctime,
            crtime: ctime,
            kind,
            perm,
            nlink,
            uid: self.uid,
            gid: self.gid,
            rdev: 0,
            blksize: BLOCK_SIZE,
            flags: 0,
        }
    }

    /// Make sure the root inode has a real NodeUid (lazy-fetched from API).
    fn ensure_root_resolved(&self) {
        let needs_resolve = {
            let db = self.db.lock().unwrap();
            db.get_inode(ROOT_INO)
                .map(|r| r.node_uid.is_none())
                .unwrap_or(true)
        };

        if !needs_resolve {
            return;
        }

        match self.rt.block_on(self.drive.get_my_files_folder()) {
            Ok(folder) => {
                let uid_raw = folder.base.uid.raw();
                let vol = folder.base.uid.volume_id.raw().to_string();
                let link = folder.base.uid.link_id.raw().to_string();
                let db = self.db.lock().unwrap();
                let _ = db.update_node_uid(ROOT_INO, &uid_raw, &vol, &link);
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to resolve root folder from API");
            }
        }
    }

    /// Populate the children of a directory if not yet done.
    fn ensure_children_populated(&self, parent_ino: u64) {
        let parent = {
            let db = self.db.lock().unwrap();
            match db.get_inode(parent_ino) {
                Some(r) if r.is_dir && !r.children_populated => r,
                _ => return,
            }
        };

        // Resolve the NodeUid for this parent.
        let node_uid_str = match parent.node_uid {
            Some(ref u) => u.clone(),
            None if parent_ino == ROOT_INO => {
                self.ensure_root_resolved();
                let db = self.db.lock().unwrap();
                match db.get_inode(ROOT_INO).and_then(|r| r.node_uid) {
                    Some(u) => u,
                    None => return,
                }
            }
            None => return,
        };

        let node_uid = match NodeUid::try_parse(&node_uid_str) {
            Some(uid) => uid,
            None => return,
        };

        // Fetch children from Proton Drive API with a timeout so offline
        // doesn't block FUSE operations for the full HTTP timeout duration.
        use proton_drive_sdk::futures::StreamExt;
        let children = match self.rt.block_on(async {
            let result = tokio::time::timeout(
                Duration::from_secs(10),
                async {
                    let stream = self.drive.enumerate_folder_children(node_uid).await?;
                    tokio::pin!(stream);
                    let mut out = Vec::new();
                    while let Some(item) = stream.next().await {
                        out.push(item?);
                    }
                    Ok::<_, anyhow::Error>(out)
                }
            ).await;
            match result {
                Ok(inner) => inner,
                Err(_) => Err(anyhow::anyhow!("timed out (offline?)")),
            }
        }) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, parent_ino, "failed to enumerate children (offline?)");
                // Serve stale DB children if any exist from a previous session.
                let db = self.db.lock().unwrap();
                if !db.list_children(parent_ino).is_empty() {
                    tracing::info!(parent_ino, "serving stale children from cache");
                    let _ = db.set_children_populated(parent_ino);
                }
                return;
            }
        };

        let db = self.db.lock().unwrap();
        for child in children {
            match child {
                PotentialObject::Node(node) => {
                    self.insert_node_into_db(&db, parent_ino, &node);
                }
                PotentialObject::Degraded(deg) => {
                    self.insert_degraded_into_db(&db, parent_ino, &deg);
                }
            }
        }
        let _ = db.set_children_populated(parent_ino);
    }

    fn insert_node_into_db(&self, db: &FuseDb, parent_ino: u64, node: &Node) {
        let uid = node.uid();
        let uid_raw = uid.raw();

        if db.find_by_node_uid(&uid_raw).is_some() {
            return; // already present
        }

        let name = node.base().name.clone();
        let is_dir = matches!(node, Node::Folder(_) | Node::Album(_));
        let (size, media_type, revision_uid, mtime) = match node {
            Node::File(f) | Node::Photo(f) => {
                let sz = f.active_revision.claimed_size.unwrap_or(f.total_size_on_cloud_storage)
                    as u64;
                let rev = f.active_revision.uid.to_string();
                let mt = f
                    .active_revision
                    .claimed_modification_time
                    .map(|t| t.timestamp())
                    .unwrap_or_else(|| node.base().creation_time.timestamp());
                (sz, f.base.media_type.clone(), Some(rev), mt)
            }
            _ => (0u64, String::new(), None, node.base().creation_time.timestamp()),
        };

        let _ = db.insert_inode(
            parent_ino,
            &name,
            Some(&uid_raw),
            Some(uid.volume_id.raw()),
            Some(uid.link_id.raw()),
            is_dir,
            size,
            &media_type,
            revision_uid.as_deref(),
            mtime,
        );
    }

    fn insert_degraded_into_db(&self, db: &FuseDb, parent_ino: u64, node: &DegradedNode) {
        let uid = node.uid();
        let uid_raw = uid.raw();

        if db.find_by_node_uid(&uid_raw).is_some() {
            return;
        }

        let name = match node {
            DegradedNode::Folder(f) | DegradedNode::Album(f) => match &f.base.name {
                PotentialObject::Node(n) => n.clone(),
                _ => return,
            },
            DegradedNode::File(f) | DegradedNode::Photo(f) => match &f.base.name {
                PotentialObject::Node(n) => n.clone(),
                _ => return,
            },
        };

        let is_dir = matches!(node, DegradedNode::Folder(_) | DegradedNode::Album(_));
        let mtime = match node {
            DegradedNode::Folder(f) | DegradedNode::Album(f) => f.base.creation_time.timestamp(),
            DegradedNode::File(f) | DegradedNode::Photo(f) => f.base.creation_time.timestamp(),
        };

        let _ = db.insert_inode(
            parent_ino,
            &name,
            Some(&uid_raw),
            Some(uid.volume_id.raw()),
            Some(uid.link_id.raw()),
            is_dir,
            0,
            "",
            None,
            mtime,
        );
    }

    /// Cache path for a local-only inode (no revision yet).
    fn cache_path_for(&self, ino: u64) -> PathBuf {
        self.cache_dir.join(format!("ino-{}", ino))
    }

    /// Stable cache path keyed by revision_uid hash.
    /// Survives DB rebuilds so files remain available offline.
    fn cache_path_for_revision(&self, rev_str: &str) -> PathBuf {
        let mut hasher = Sha256::new();
        hasher.update(rev_str.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        self.cache_dir.join(hash)
    }

    /// Send a desktop notification (best-effort, never blocks on failure).
    fn notify(summary: &str, body: &str) {
        let _ = std::process::Command::new("notify-send")
            .args(["-a", "Proton Drive", "-i", "folder-download", summary, body])
            .spawn();
    }

    /// Download a file into the local cache if not already present.
    /// Returns Ok(path) on success, Err(None) if no revision exists (new local file),
    /// or Err(Some(msg)) if the download actually failed.
    fn ensure_file_cached(&self, row: &InodeRow) -> Result<PathBuf, Option<String>> {
        if row.is_dir {
            return Err(None);
        }

        // Already cached on disk?
        if let Some(ref p) = row.cached_path {
            let path = PathBuf::from(p);
            if path.exists() {
                return Ok(path);
            }
        }

        let rev_str = match row.revision_uid.as_ref() {
            Some(s) => s,
            None => {
                tracing::debug!(ino = row.ino, name = %row.name, "no revision_uid — file is local-only");
                return Err(None);
            }
        };
        let revision_uid = match RevisionUid::try_parse(rev_str) {
            Some(uid) => uid,
            None => {
                tracing::warn!(ino = row.ino, rev = %rev_str, "failed to parse revision_uid");
                return Err(Some(format!("invalid revision_uid: {}", rev_str)));
            }
        };

        // Use a stable, revision-based cache path so files survive DB rebuilds.
        let cache_path = self.cache_path_for_revision(rev_str);

        // Already downloaded in a previous session?
        if cache_path.exists() {
            tracing::info!(ino = row.ino, name = %row.name, "serving from offline cache");
            let db = self.db.lock().unwrap();
            let _ = db.set_cached_path(row.ino, cache_path.to_str());
            return Ok(cache_path);
        }

        // Register with transfer tracker for dashboard visibility.
        let idx = self.tracker.add(
            row.name.clone(),
            TransferDirection::Download,
            row.size as i64,
        );
        let on_progress = self.tracker.progress_callback(idx);

        tracing::info!(ino = row.ino, name = %row.name, "downloading file from Proton Drive");
        Self::notify("Downloading", &row.name);

        let res = self.rt.block_on(async {
            let downloader = self.drive.get_file_downloader(revision_uid).await?;
            let file = std::fs::File::create(&cache_path)?;
            let writer: Box<dyn std::io::Write + Send> = Box::new(std::io::BufWriter::new(file));
            let controller = downloader.download_to_stream(writer, on_progress);
            controller.completion.await??;
            Ok::<_, anyhow::Error>(())
        });

        match res {
            Ok(()) => {
                tracing::info!(ino = row.ino, name = %row.name, "download complete");
                let db = self.db.lock().unwrap();
                let _ = db.set_cached_path(row.ino, cache_path.to_str());
                self.tracker.mark_complete(idx);
                Self::notify("Download complete", &row.name);
                Ok(cache_path)
            }
            Err(e) => {
                tracing::error!(error = %e, ino = row.ino, name = %row.name, "file download failed");
                let _ = std::fs::remove_file(&cache_path);
                self.tracker.mark_failed(idx);
                Self::notify("Download failed", &format!("{}: {}", row.name, e));
                Err(Some(e.to_string()))
            }
        }
    }

    fn alloc_fh(&self) -> u64 {
        self.next_fh.fetch_add(1, Ordering::Relaxed)
    }
}

// ── Filesystem trait implementation ──────────────────────────────────

impl Filesystem for ProtonDriveFs {
    fn init(
        &mut self,
        _req: &Request,
        _config: &mut fuser::KernelConfig,
    ) -> std::io::Result<()> {
        tracing::info!("ProtonDriveFs: FUSE init");
        // Ensure root inode row exists.
        let db = self.db.lock().unwrap();
        if db.get_inode(ROOT_INO).is_none() {
            db.insert_root(None, None, None)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        }
        Ok(())
    }

    fn destroy(&mut self) {
        tracing::info!("ProtonDriveFs: FUSE destroy");
    }

    fn lookup(&self, _req: &Request, parent: fuser::INodeNo, name: &OsStr, reply: ReplyEntry) {
        let parent_ino: u64 = parent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Lazily populate children on first lookup into a directory.
        self.ensure_children_populated(parent_ino);

        let db = self.db.lock().unwrap();
        match db.lookup_child(parent_ino, name_str) {
            Some(row) => {
                let attr = self.inode_to_attr(&row);
                reply.entry(&TTL, &attr, Generation(0));
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn getattr(
        &self,
        _req: &Request,
        ino: fuser::INodeNo,
        _fh: Option<fuser::FileHandle>,
        reply: ReplyAttr,
    ) {
        let ino_u64: u64 = ino.0;
        let db = self.db.lock().unwrap();
        match db.get_inode(ino_u64) {
            Some(row) => {
                let attr = self.inode_to_attr(&row);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn setattr(
        &self,
        _req: &Request,
        ino: fuser::INodeNo,
        _mode: Option<u32>,
        _uid: Option<u32>,
        _gid: Option<u32>,
        size: Option<u64>,
        _atime: Option<fuser::TimeOrNow>,
        _mtime: Option<fuser::TimeOrNow>,
        _ctime: Option<SystemTime>,
        _fh: Option<fuser::FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<fuser::BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let ino_u64: u64 = ino.0;
        let db = self.db.lock().unwrap();

        // Handle truncation.
        if let Some(new_size) = size {
            let _ = db.update_size(ino_u64, new_size);

            // Truncate the cached file if present.
            if let Some(row) = db.get_inode(ino_u64) {
                if let Some(ref cp) = row.cached_path {
                    if let Ok(f) = std::fs::OpenOptions::new().write(true).open(cp) {
                        let _ = f.set_len(new_size);
                    }
                }
            }
        }

        match db.get_inode(ino_u64) {
            Some(row) => {
                let attr = self.inode_to_attr(&row);
                reply.attr(&TTL, &attr);
            }
            None => reply.error(Errno::ENOENT),
        }
    }

    fn readdir(
        &self,
        _req: &Request,
        ino: fuser::INodeNo,
        _fh: fuser::FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let ino_u64: u64 = ino.0;

        self.ensure_children_populated(ino_u64);

        let db = self.db.lock().unwrap();
        let parent_ino = db
            .get_inode(ino_u64)
            .map(|r| r.parent_ino)
            .unwrap_or(ROOT_INO);

        // Build entries: ".", "..", then children.
        let mut entries: Vec<(u64, fuser::FileType, String)> = Vec::new();
        entries.push((ino_u64, FileType::Directory, ".".into()));
        entries.push((parent_ino, FileType::Directory, "..".into()));

        for child in db.list_children(ino_u64) {
            let ft = if child.is_dir {
                FileType::Directory
            } else {
                FileType::RegularFile
            };
            entries.push((child.ino, ft, child.name));
        }

        for (i, (child_ino, ft, name)) in entries.iter().enumerate().skip(offset as usize) {
            // reply.add returns true when the buffer is full.
            if reply.add(INodeNo(*child_ino), (i + 1) as u64, *ft, name) {
                break;
            }
        }
        reply.ok();
    }

    fn opendir(
        &self,
        _req: &Request,
        _ino: fuser::INodeNo,
        _flags: fuser::OpenFlags,
        reply: ReplyOpen,
    ) {
        reply.opened(FileHandle(0), FopenFlags::empty());
    }

    fn releasedir(
        &self,
        _req: &Request,
        _ino: fuser::INodeNo,
        _fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }

    fn open(
        &self,
        req: &Request,
        ino: fuser::INodeNo,
        flags: fuser::OpenFlags,
        reply: ReplyOpen,
    ) {
        let ino_u64: u64 = ino.0;
        let row = {
            let db = self.db.lock().unwrap();
            match db.get_inode(ino_u64) {
                Some(r) => r,
                None => {
                    reply.error(Errno::ENOENT);
                    return;
                }
            }
        };

        if row.is_dir {
            reply.error(Errno::EISDIR);
            return;
        }

        // Block thumbnailer processes from triggering downloads of uncached files.
        let is_cached = row.cached_path.as_ref()
            .map(|p| std::path::Path::new(p).exists())
            .unwrap_or(false);
        if !is_cached && self.thumb_config.is_blocked_process(req.pid()) {
            tracing::info!(
                ino = ino_u64, pid = req.pid(), name = %row.name,
                "blocked thumbnailer from triggering download"
            );
            reply.error(Errno::EACCES);
            return;
        }

        // Ensure the file is downloaded into the local cache.
        let cache_path = match self.ensure_file_cached(&row) {
            Ok(p) => p,
            Err(None) => {
                // No revision — file is local-only (newly created), use empty cache file.
                let p = self.cache_path_for(ino_u64);
                if !p.exists() {
                    if let Err(e) = std::fs::File::create(&p) {
                        tracing::warn!(error = %e, "failed to create empty cache file");
                        reply.error(Errno::EIO);
                        return;
                    }
                }
                let db = self.db.lock().unwrap();
                let _ = db.set_cached_path(ino_u64, p.to_str());
                p
            }
            Err(Some(msg)) => {
                // Download actually failed — report I/O error to caller.
                tracing::error!(ino = ino_u64, error = %msg, "cannot open file: download failed");
                reply.error(Errno::EIO);
                return;
            }
        };

        let writable = matches!(flags.acc_mode(), OpenAccMode::O_WRONLY | OpenAccMode::O_RDWR);

        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(writable)
            .open(&cache_path)
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "failed to open cache file");
                reply.error(Errno::EIO);
                return;
            }
        };

        let fh = self.alloc_fh();
        self.open_files.write().unwrap().insert(
            fh,
            Mutex::new(OpenFile {
                ino: ino_u64,
                file,
                writable,
            }),
        );

        reply.opened(FileHandle(fh), FopenFlags::empty());
    }

    fn read(
        &self,
        _req: &Request,
        _ino: fuser::INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        size: u32,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyData,
    ) {
        let fh_u64: u64 = fh.0;
        let files = self.open_files.read().unwrap();
        let entry = match files.get(&fh_u64) {
            Some(e) => e,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        let of = entry.lock().unwrap();
        let mut buf = vec![0u8; size as usize];
        match of.file.read_at(&mut buf, offset) {
            Ok(n) => {
                buf.truncate(n);
                reply.data(&buf);
            }
            Err(e) => {
                tracing::warn!(error = %e, "read failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn write(
        &self,
        _req: &Request,
        ino: fuser::INodeNo,
        fh: fuser::FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: fuser::WriteFlags,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        reply: ReplyWrite,
    ) {
        let ino_u64: u64 = ino.0;
        let fh_u64: u64 = fh.0;
        let files = self.open_files.read().unwrap();
        let entry = match files.get(&fh_u64) {
            Some(e) => e,
            None => {
                reply.error(Errno::EBADF);
                return;
            }
        };

        let of = entry.lock().unwrap();
        match of.file.write_at(data, offset) {
            Ok(n) => {
                // Update size in DB.
                let new_end = offset + n as u64;
                let db = self.db.lock().unwrap();
                if let Some(row) = db.get_inode(ino_u64) {
                    if new_end > row.size {
                        let _ = db.update_size(ino_u64, new_end);
                    }
                }
                let _ = db.set_dirty(ino_u64, true);
                reply.written(n as u32);
            }
            Err(e) => {
                tracing::warn!(error = %e, "write failed");
                reply.error(Errno::EIO);
            }
        }
    }

    fn flush(
        &self,
        _req: &Request,
        _ino: fuser::INodeNo,
        fh: fuser::FileHandle,
        _lock_owner: fuser::LockOwner,
        reply: ReplyEmpty,
    ) {
        let fh_u64: u64 = fh.0;
        let files = self.open_files.read().unwrap();
        if let Some(entry) = files.get(&fh_u64) {
            let of = entry.lock().unwrap();
            let _ = of.file.sync_all();
        }
        reply.ok();
    }

    fn release(
        &self,
        _req: &Request,
        ino: fuser::INodeNo,
        fh: fuser::FileHandle,
        _flags: fuser::OpenFlags,
        _lock_owner: Option<fuser::LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        let ino_u64: u64 = ino.0;
        let fh_u64: u64 = fh.0;

        // Remove from open-file table (drops the File handle).
        self.open_files.write().unwrap().remove(&fh_u64);

        // If the file was dirtied, enqueue a journal entry for upload.
        let db = self.db.lock().unwrap();
        if let Some(row) = db.get_inode(ino_u64) {
            if row.dirty {
                let payload = serde_json::json!({
                    "ino": ino_u64,
                    "cached_path": row.cached_path,
                    "parent_ino": row.parent_ino,
                    "name": row.name,
                    "node_uid": row.node_uid,
                    "revision_uid": row.revision_uid,
                    "size": row.size,
                    "media_type": row.media_type,
                    "mtime": row.mtime,
                });
                let event = if row.node_uid.is_some() && row.revision_uid.is_some() {
                    "update_revision"
                } else {
                    "create_file"
                };
                let _ = db.enqueue_journal(event, ino_u64, &payload.to_string());
            }
        }

        reply.ok();
    }

    fn create(
        &self,
        _req: &Request,
        parent: fuser::INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let parent_ino: u64 = parent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let db = self.db.lock().unwrap();
        // Reject duplicates.
        if db.lookup_child(parent_ino, name_str).is_some() {
            reply.error(Errno::EEXIST);
            return;
        }

        let media_type = mime_guess::from_path(name_str)
            .first_or_octet_stream()
            .to_string();
        let new_ino = match db.insert_inode(
            parent_ino, name_str, None, None, None, false, 0, &media_type, None, 0,
        ) {
            Ok(ino) => ino,
            Err(e) => {
                tracing::warn!(error = %e, "failed to insert inode");
                reply.error(Errno::EIO);
                return;
            }
        };
        let _ = db.set_dirty(new_ino, true);

        // Create an empty cache file.
        let cache_path = self.cache_dir.join(new_ino.to_string());
        if let Err(e) = std::fs::File::create(&cache_path) {
            tracing::warn!(error = %e, "failed to create cache file");
            reply.error(Errno::EIO);
            return;
        }
        let _ = db.set_cached_path(new_ino, cache_path.to_str());

        // Open it for the caller.
        let writable = (flags & libc::O_ACCMODE) != libc::O_RDONLY;
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .write(writable)
            .open(&cache_path)
        {
            Ok(f) => f,
            Err(e) => {
                tracing::warn!(error = %e, "failed to re-open cache file");
                reply.error(Errno::EIO);
                return;
            }
        };
        drop(db);

        let fh = self.alloc_fh();
        self.open_files.write().unwrap().insert(
            fh,
            Mutex::new(OpenFile {
                ino: new_ino,
                file,
                writable,
            }),
        );

        let row_for_attr = InodeRow {
            ino: new_ino,
            parent_ino,
            name: name_str.to_string(),
            node_uid: None,
            volume_id: None,
            link_id: None,
            is_dir: false,
            size: 0,
            media_type,
            revision_uid: None,
            mtime: 0,
            ctime: 0,
            cached_path: cache_path.to_str().map(|s| s.to_string()),
            dirty: true,
            children_populated: false,
        };
        let attr = self.inode_to_attr(&row_for_attr);
        reply.created(&TTL, &attr, Generation(0), FileHandle(fh), FopenFlags::empty());
    }

    fn mkdir(
        &self,
        _req: &Request,
        parent: fuser::INodeNo,
        name: &OsStr,
        _mode: u32,
        _umask: u32,
        reply: ReplyEntry,
    ) {
        let parent_ino: u64 = parent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let db = self.db.lock().unwrap();
        if db.lookup_child(parent_ino, name_str).is_some() {
            reply.error(Errno::EEXIST);
            return;
        }

        let new_ino = match db.insert_inode(parent_ino, name_str, None, None, None, true, 0, "", None, 0) {
            Ok(ino) => ino,
            Err(e) => {
                tracing::warn!(error = %e, "mkdir db insert failed");
                reply.error(Errno::EIO);
                return;
            }
        };

        // Enqueue for upstream creation.
        let payload = serde_json::json!({
            "ino": new_ino,
            "parent_ino": parent_ino,
            "name": name_str,
        });
        let _ = db.enqueue_journal("create_folder", new_ino, &payload.to_string());
        drop(db);

        let row = InodeRow {
            ino: new_ino,
            parent_ino,
            name: name_str.to_string(),
            node_uid: None,
            volume_id: None,
            link_id: None,
            is_dir: true,
            size: 0,
            media_type: String::new(),
            revision_uid: None,
            mtime: 0,
            ctime: 0,
            cached_path: None,
            dirty: false,
            children_populated: true, // no children yet
        };
        let attr = self.inode_to_attr(&row);
        reply.entry(&TTL, &attr, Generation(0));
    }

    fn unlink(&self, _req: &Request, parent: fuser::INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent_ino: u64 = parent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let db = self.db.lock().unwrap();
        let row = match db.lookup_child(parent_ino, name_str) {
            Some(r) => r,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if row.is_dir {
            reply.error(Errno::EISDIR);
            return;
        }

        let payload = serde_json::json!({ "node_uid": row.node_uid });
        if row.node_uid.is_some() {
            let _ = db.enqueue_journal("delete", row.ino, &payload.to_string());
        }
        let _ = db.delete_inode(row.ino);

        // Clean up cached file.
        if let Some(ref p) = row.cached_path {
            let _ = std::fs::remove_file(p);
        }

        reply.ok();
    }

    fn rmdir(&self, _req: &Request, parent: fuser::INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let parent_ino: u64 = parent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        let db = self.db.lock().unwrap();
        let row = match db.lookup_child(parent_ino, name_str) {
            Some(r) => r,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        if !row.is_dir {
            reply.error(Errno::ENOTDIR);
            return;
        }

        // Check if directory is empty.
        if !db.list_children(row.ino).is_empty() {
            reply.error(Errno::ENOTEMPTY);
            return;
        }

        let payload = serde_json::json!({ "node_uid": row.node_uid });
        if row.node_uid.is_some() {
            let _ = db.enqueue_journal("delete", row.ino, &payload.to_string());
        }
        let _ = db.delete_inode(row.ino);

        reply.ok();
    }

    fn rename(
        &self,
        _req: &Request,
        parent: fuser::INodeNo,
        name: &OsStr,
        newparent: fuser::INodeNo,
        newname: &OsStr,
        _flags: fuser::RenameFlags,
        reply: ReplyEmpty,
    ) {
        let parent_ino: u64 = parent.0;
        let newparent_ino: u64 = newparent.0;
        let name_str = match name.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };
        let newname_str = match newname.to_str() {
            Some(s) => s,
            None => {
                reply.error(Errno::EINVAL);
                return;
            }
        };

        let db = self.db.lock().unwrap();
        let row = match db.lookup_child(parent_ino, name_str) {
            Some(r) => r,
            None => {
                reply.error(Errno::ENOENT);
                return;
            }
        };

        // Overwrite target if it already exists.
        if let Some(existing) = db.lookup_child(newparent_ino, newname_str) {
            if existing.is_dir && !db.list_children(existing.ino).is_empty() {
                reply.error(Errno::ENOTEMPTY);
                return;
            }
            let _ = db.delete_inode(existing.ino);
        }

        let _ = db.rename_inode(row.ino, newparent_ino, newname_str);

        // Determine which journal event to queue.
        if let Some(ref node_uid) = row.node_uid {
            if parent_ino == newparent_ino {
                // Pure rename.
                let payload = serde_json::json!({
                    "node_uid": node_uid,
                    "new_name": newname_str,
                });
                let _ = db.enqueue_journal("rename", row.ino, &payload.to_string());
            } else {
                // Move (possibly + rename).
                let new_parent_node_uid = db.get_inode(newparent_ino).and_then(|r| r.node_uid);
                let payload = serde_json::json!({
                    "node_uid": node_uid,
                    "new_parent_node_uid": new_parent_node_uid,
                    "new_name": newname_str,
                });
                let _ = db.enqueue_journal("move", row.ino, &payload.to_string());
            }
        }

        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: fuser::INodeNo, reply: ReplyStatfs) {
        // Try to get real quota from the API (with a short timeout for offline).
        let (used, total) = self
            .rt
            .block_on(async {
                tokio::time::timeout(
                    Duration::from_secs(5),
                    self.drive.get_user_storage_info(),
                ).await
            })
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or((0, 1_000_000_000)); // 1 GB fallback when offline

        let bsize = BLOCK_SIZE as u64;
        let blocks = total as u64 / bsize;
        let bfree = (total - used).max(0) as u64 / bsize;

        reply.statfs(
            blocks,       // blocks
            bfree,        // bfree
            bfree,        // bavail
            u64::MAX,     // files
            u64::MAX,     // ffree
            bsize as u32, // bsize
            256,          // namelen
            bsize as u32, // frsize
        );
    }

    fn access(
        &self,
        _req: &Request,
        _ino: fuser::INodeNo,
        _mask: fuser::AccessFlags,
        reply: ReplyEmpty,
    ) {
        reply.ok();
    }
}

// ── Background event-poll loop ───────────────────────────────────────

async fn event_poll_loop(db: Arc<Mutex<FuseDb>>, drive: ProtonDriveClient) {
    use proton_drive_sdk::api::events::VolumeEventType;

    // Wait a moment for the root to be resolved before starting.
    tokio::time::sleep(Duration::from_secs(5)).await;

    // Resolve volume_id from root inode.
    let volume_id = {
        let cached_vol = {
            let db = db.lock().unwrap();
            db.get_inode(ROOT_INO).and_then(|r| r.volume_id)
        };
        match cached_vol {
            Some(v) => proton_drive_sdk::volume::VolumeId::new(v),
            None => {
                // Root not resolved yet; try fetching it.
                match drive.get_my_files_folder().await {
                    Ok(f) => f.base.uid.volume_id,
                    Err(e) => {
                        tracing::error!(error = %e, "event_poll: cannot resolve volume");
                        return;
                    }
                }
            }
        }
    };

    // Obtain initial cursor.
    let mut cursor = {
        let stored = db.lock().unwrap().get_event_cursor(volume_id.raw());
        match stored {
            Some(c) => c,
            None => match drive
                .get_volume_latest_event_id(volume_id.clone())
                .await
            {
                Ok(c) => {
                    let _ = db
                        .lock()
                        .unwrap()
                        .set_event_cursor(volume_id.raw(), &c);
                    c
                }
                Err(e) => {
                    tracing::error!(error = %e, "event_poll: cannot get initial cursor");
                    return;
                }
            },
        }
    };

    loop {
        tokio::time::sleep(Duration::from_secs(30)).await;

        let resp = match drive
            .poll_volume_events(volume_id.clone(), &cursor)
            .await
        {
            Ok(r) => {
                if !ONLINE.swap(true, Ordering::Relaxed) {
                    tracing::info!("connectivity restored — back online");
                }
                r
            }
            Err(e) => {
                if ONLINE.swap(false, Ordering::Relaxed) {
                    tracing::warn!(error = %e, "connectivity lost — switching to offline mode");
                }
                continue;
            }
        };

        for event in &resp.events {
            let link_id_str = event.link.link_id.raw().to_string();
            match event.event_type() {
                Some(VolumeEventType::Delete) => {
                    let db = db.lock().unwrap();
                    if let Some(row) = db.find_by_link_id(&link_id_str) {
                        // Remove cached file.
                        if let Some(ref cp) = row.cached_path {
                            let _ = std::fs::remove_file(cp);
                        }
                        let _ = db.delete_inode(row.ino);
                    }
                }
                Some(VolumeEventType::Create) => {
                    // Invalidate parent so next readdir re-fetches.
                    if let Some(parent_link_id) = &event.link.parent_link_id {
                        let db = db.lock().unwrap();
                        if let Some(parent_row) = db.find_by_link_id(parent_link_id.raw()) {
                            let _ = db.clear_children_populated(parent_row.ino);
                        }
                    }
                }
                Some(VolumeEventType::UpdateMetadata) | Some(VolumeEventType::UpdateContent) => {
                    let db = db.lock().unwrap();
                    if let Some(row) = db.find_by_link_id(&link_id_str) {
                        // Invalidate cache so next open re-downloads.
                        if let Some(ref cp) = row.cached_path {
                            let _ = std::fs::remove_file(cp);
                        }
                        let _ = db.set_cached_path(row.ino, None);

                        // Also invalidate parent children so metadata refreshes.
                        let _ = db.clear_children_populated(row.parent_ino);
                    }
                }
                _ => {}
            }
        }

        cursor = resp.event_id.clone();
        let _ = db
            .lock()
            .unwrap()
            .set_event_cursor(volume_id.raw(), &cursor);
    }
}

// ── Background journal-flush loop ────────────────────────────────────

async fn journal_flush_loop(
    db: Arc<Mutex<FuseDb>>,
    drive: ProtonDriveClient,
    _cache_dir: PathBuf,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Skip flush attempts when offline to avoid spamming failed requests.
        if !ONLINE.load(Ordering::Relaxed) {
            continue;
        }

        let entries = {
            let db = db.lock().unwrap();
            db.load_pending_journal(20)
        };

        for entry in entries {
            let result = process_journal_entry(&db, &drive, &entry).await;

            let db = db.lock().unwrap();
            match result {
                Ok(()) => {
                    let _ = db.update_journal_status(entry.id, "done", None);
                }
                Err(e) => {
                    let msg = e.to_string();
                    let is_permanent = msg.contains("already exists")
                        || msg.contains("No such file or directory")
                        || msg.contains("inode deleted")
                        || msg.contains("missing cached_path")
                        || msg.contains("missing name");
                    let exceeded_retries = entry.retry_count >= 10;

                    if is_permanent || exceeded_retries {
                        tracing::warn!(
                            id = entry.id,
                            event = %entry.event_type,
                            error = %msg,
                            retries = entry.retry_count,
                            "journal entry permanently failed"
                        );
                        let _ = db.update_journal_status(entry.id, "failed", Some(&msg));
                    } else {
                        tracing::warn!(
                            id = entry.id,
                            event = %entry.event_type,
                            error = %msg,
                            retries = entry.retry_count,
                            "journal flush failed, will retry"
                        );
                        let _ = db.update_journal_status(entry.id, "pending", Some(&msg));
                    }
                }
            }
        }

        // Purge completed and permanently failed entries.
        let _ = db.lock().unwrap().delete_completed_journal();
    }
}

/// Resolve a parent inode's node_uid, fetching from the API if needed (e.g. root).
async fn resolve_parent_node_uid(
    db: &Arc<Mutex<FuseDb>>,
    drive: &ProtonDriveClient,
    parent_ino: u64,
) -> anyhow::Result<String> {
    // Check if already resolved in DB.
    {
        let db_guard = db.lock().unwrap();
        if let Some(uid) = db_guard.get_inode(parent_ino).and_then(|r| r.node_uid) {
            return Ok(uid);
        }
    }

    // Not resolved — try to resolve it from the API.
    if parent_ino == ROOT_INO {
        let folder = drive.get_my_files_folder().await
            .map_err(|e| anyhow::anyhow!("failed to resolve root from API: {}", e))?;
        let uid_raw = folder.base.uid.raw();
        let vol = folder.base.uid.volume_id.raw().to_string();
        let link = folder.base.uid.link_id.raw().to_string();
        let db_guard = db.lock().unwrap();
        let _ = db_guard.update_node_uid(ROOT_INO, &uid_raw, &vol, &link);
        return Ok(uid_raw);
    }

    // Non-root parent: check if it has volume_id + link_id we can reconstruct from.
    let node_uid_str = {
        let db_guard = db.lock().unwrap();
        db_guard.get_inode(parent_ino).and_then(|r| {
            let vol = r.volume_id?;
            let link = r.link_id?;
            Some(format!("{}~{}", vol, link))
        })
    };

    if let Some(uid_str) = node_uid_str {
        let db_guard = db.lock().unwrap();
        let _ = db_guard.update_node_uid(
            parent_ino,
            &uid_str,
            &uid_str.split_once('~').unwrap().0,
            &uid_str.split_once('~').unwrap().1,
        );
        return Ok(uid_str);
    }

    Err(anyhow::anyhow!("parent not resolved"))
}

async fn process_journal_entry(
    db: &Arc<Mutex<FuseDb>>,
    drive: &ProtonDriveClient,
    entry: &crate::db::JournalEntry,
) -> anyhow::Result<()> {
    let payload: serde_json::Value = serde_json::from_str(&entry.payload)?;

    match entry.event_type.as_str() {
        "create_folder" => {
            let parent_ino = payload["parent_ino"].as_u64().unwrap_or(ROOT_INO);
            let name = payload["name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing name"))?;

            let parent_node_uid = resolve_parent_node_uid(db, drive, parent_ino).await?;
            let parent_uid = NodeUid::parse(&parent_node_uid)
                .map_err(|e| anyhow::anyhow!(e))?;

            let folder = drive
                .create_folder(parent_uid, name.to_string(), None)
                .await?;

            let uid_raw = folder.base.uid.raw();
            let db = db.lock().unwrap();
            let _ = db.update_node_uid(
                entry.ino,
                &uid_raw,
                folder.base.uid.volume_id.raw(),
                folder.base.uid.link_id.raw(),
            );
        }

        "create_file" => {
            let parent_ino = payload["parent_ino"].as_u64().unwrap_or(ROOT_INO);
            let name = payload["name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing name"))?;
            let cached_path = payload["cached_path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing cached_path"))?;
            let size = payload["size"].as_u64().unwrap_or(0) as i64;
            let media_type = payload["media_type"].as_str().unwrap_or("application/octet-stream");

            let parent_node_uid = resolve_parent_node_uid(db, drive, parent_ino).await?;
            let parent_uid =
                NodeUid::parse(&parent_node_uid).map_err(|e| anyhow::anyhow!(e))?;

            let last_mod = payload["mtime"].as_i64()
                .filter(|&t| t > 0)
                .map(|t| UNIX_EPOCH + Duration::from_secs(t as u64));

            let uploader = drive
                .get_file_uploader(
                    parent_uid,
                    name.to_string(),
                    media_type.to_string(),
                    size,
                    last_mod,
                    None,
                    None,
                    false,
                )
                .await?;

            let file = tokio::fs::File::open(cached_path).await?;
            let reader: Box<dyn tokio::io::AsyncRead + Unpin + Send> = Box::new(file);
            let node_uid = uploader
                .upload_from_stream(reader, vec![], Box::new(|_, _| {}))
                .await?;

            let uid_raw = node_uid.raw();
            {
                let db_guard = db.lock().unwrap();
                let _ = db_guard.update_node_uid(
                    entry.ino,
                    &uid_raw,
                    node_uid.volume_id.raw(),
                    node_uid.link_id.raw(),
                );
                let _ = db_guard.set_dirty(entry.ino, false);
            }

            // Fetch the newly created node to obtain its revision_uid.
            // Without this, subsequent edits queue another create_file instead
            // of update_revision, causing "already exists" errors.
            if let Ok(PotentialObject::Node(Node::File(f)))
            | Ok(PotentialObject::Node(Node::Photo(f))) =
                drive.get_node_uncached(node_uid).await
            {
                let rev_uid = f.active_revision.uid.to_string();
                let size = f.active_revision.claimed_size
                    .unwrap_or(f.total_size_on_cloud_storage);
                let db_guard = db.lock().unwrap();
                let _ = db_guard.update_revision(entry.ino, &rev_uid, size as u64);
                // Restore cached_path since update_revision clears it.
                let _ = db_guard.set_cached_path(entry.ino, Some(cached_path));
            }
        }

        "update_revision" => {
            let _node_uid_str = payload["node_uid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing node_uid"))?;
            let rev_uid_str = payload["revision_uid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing revision_uid"))?;
            let cached_path = payload["cached_path"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing cached_path"))?;
            let size = payload["size"].as_u64().unwrap_or(0) as i64;

            let revision_uid = RevisionUid::parse(rev_uid_str)
                .map_err(|e| anyhow::anyhow!(e))?;

            let last_mod = payload["mtime"].as_i64()
                .filter(|&t| t > 0)
                .map(|t| UNIX_EPOCH + Duration::from_secs(t as u64));

            let uploader = drive
                .get_file_revision_uploader(revision_uid, size, last_mod, None, None)
                .await?;

            let file = tokio::fs::File::open(cached_path).await?;
            let reader: Box<dyn tokio::io::AsyncRead + Unpin + Send> = Box::new(file);
            let new_node_uid = uploader
                .upload_from_stream(reader, vec![], Box::new(|_, _| {}))
                .await?;

            // Refresh the node to get the new revision UID.
            if let Ok(PotentialObject::Node(Node::File(f)))
            | Ok(PotentialObject::Node(Node::Photo(f))) =
                drive.get_node_uncached(new_node_uid).await
            {
                let db = db.lock().unwrap();
                let _ = db.update_revision(
                    entry.ino,
                    &f.active_revision.uid.to_string(),
                    f.active_revision.claimed_size.unwrap_or(size) as u64,
                );
                let _ = db.set_dirty(entry.ino, false);
            }
        }

        "rename" => {
            let node_uid_str = payload["node_uid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing node_uid"))?;
            let new_name = payload["new_name"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing new_name"))?;

            let uid = NodeUid::parse(node_uid_str).map_err(|e| anyhow::anyhow!(e))?;
            drive.rename_node(uid, new_name.to_string(), None).await?;
        }

        "move" => {
            let node_uid_str = payload["node_uid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing node_uid"))?;
            let new_parent_str = payload["new_parent_node_uid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing new_parent"))?;
            let new_name = payload["new_name"].as_str();

            let uid = NodeUid::parse(node_uid_str).map_err(|e| anyhow::anyhow!(e))?;
            let new_parent =
                NodeUid::parse(new_parent_str).map_err(|e| anyhow::anyhow!(e))?;

            drive
                .move_nodes(vec![uid.clone()], new_parent)
                .await?;

            // Apply rename if name also changed.
            if let Some(name) = new_name {
                drive.rename_node(uid, name.to_string(), None).await?;
            }
        }

        "delete" => {
            let node_uid_str = match payload["node_uid"].as_str() {
                Some(s) => s,
                None => return Ok(()), // locally-created node that was never synced
            };
            let uid = NodeUid::parse(node_uid_str).map_err(|e| anyhow::anyhow!(e))?;
            drive.trash_nodes(vec![uid]).await?;
        }

        other => {
            tracing::warn!(event_type = other, "unknown journal event type");
        }
    }

    Ok(())
}

// ── Mount entry-point (called from app.rs) ───────────────────────────

impl ProtonDrive {
    pub fn mount_fuse(&mut self) {
        // Only mount once.
        if self.fuse_session.is_some() {
            return;
        }

        let session = match &self.state {
            crate::app::AppState::Authenticated(s) => s.clone(),
            _ => return,
        };

        let config_dir = platform_dirs::AppDirs::new(Some("pdcli"), false)
            .expect("config dir")
            .config_dir;
        let cache_dir = config_dir.join("fuse_cache");
        std::fs::create_dir_all(&cache_dir).ok();
        let db_path = config_dir.join("fuse.db");
        let mountpoint = dirs::home_dir()
            .expect("home dir")
            .join("ProtonDrive");
        std::fs::create_dir_all(&mountpoint).ok();

        let db = match FuseDb::open(&db_path) {
            Ok(db) => db,
            Err(e) => {
                tracing::error!(error = %e, "failed to open FUSE database");
                return;
            }
        };

        let drive = match ProtonDriveClient::new(&session, None) {
            Ok(d) => d,
            Err(e) => {
                tracing::error!(error = %e, "failed to create ProtonDriveClient");
                return;
            }
        };

        // Seed root inode.
        if let Err(e) = db.insert_root(None, None, None) {
            tracing::error!(error = %e, "failed to insert root inode");
            return;
        }

        let rt = tokio::runtime::Handle::current();
        let fs = ProtonDriveFs::new(db, cache_dir, drive, rt, self.transfer_tracker.clone());
        fs.spawn_background_workers();

        tracing::info!(mount = %mountpoint.display(), "mounting FUSE filesystem");

        // Clean up any stale mount from a previous crash.
        let _ = std::process::Command::new("fusermount3")
            .args(["-u", "-z", &*mountpoint.to_string_lossy()])
            .status()
            .or_else(|_| {
                std::process::Command::new("fusermount")
                    .args(["-u", "-z", &*mountpoint.to_string_lossy()])
                    .status()
            });

        let mut config = Config::default();
        config.mount_options = vec![MountOption::FSName("proton-drive".into())];
        match fuser::spawn_mount2(
            fs,
            &mountpoint,
            &config,
        ) {
            Ok(bg) => {
                // Register globally so signal/panic handlers can unmount.
                let _ = MOUNT_PATH.set(mountpoint);
                self.fuse_session = Some(bg);
                tracing::info!("FUSE filesystem mounted successfully");
            }
            Err(e) => {
                tracing::error!(error = %e, "FUSE mount failed");
            }
        }
    }
}

impl Drop for ProtonDrive {
    fn drop(&mut self) {
        // Drop the BackgroundSession first (triggers fuser's own unmount).
        if let Some(session) = self.fuse_session.take() {
            drop(session);
        }
        // Belt-and-suspenders: also call fusermount in case fuser's unmount
        // failed or was skipped.
        force_unmount();
    }
}