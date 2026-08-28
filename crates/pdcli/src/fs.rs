use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{
    Config, Errno, FileAttr, FileHandle, FileType, Filesystem, FopenFlags, Generation, INodeNo,
    MountOption, OpenAccMode, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyStatfs, ReplyWrite, Request,
};
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::revision::RevisionUid;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;
use proton_sdk_rs2::session::ProtonAPISession;
use sha2::{Digest, Sha256};

use crate::app::ProtonDrive;
use crate::db::{FuseDb, InodeRow};
use crate::pdignore::IgnoreMatcher;
use crate::thumbnail::ThumbnailConfig;
use crate::transfer::{TransferDirection, TransferTracker};

const TTL: Duration = Duration::from_secs(0);
const ROOT_INO: u64 = 1;
const BLOCK_SIZE: u32 = 4096;
// File opens may wait for a remote download. Keep metadata and directory
// requests flowing while one of those opens is in progress.
const FUSE_REQUEST_THREADS: usize = 8;

/// Global mountpoint path so signal handlers / panic hooks can unmount.
static MOUNT_PATH: OnceLock<PathBuf> = OnceLock::new();

/// Online/offline state — updated by the event poll loop.
static ONLINE: AtomicBool = AtomicBool::new(true);
static FORCE_OFFLINE: AtomicBool = AtomicBool::new(false);
static SYNC_PAUSED: AtomicBool = AtomicBool::new(false);
static SYNC_NOW: AtomicBool = AtomicBool::new(false);
static JOURNAL_NOTIFY: tokio::sync::Notify = tokio::sync::Notify::const_new();
const FILE_UPLOAD_CONCURRENCY: usize = 4;

/// Returns whether the client currently has connectivity to the Proton API.
pub fn is_online() -> bool {
    ONLINE.load(Ordering::Relaxed) && !FORCE_OFFLINE.load(Ordering::Relaxed)
}

pub fn set_force_offline(force: bool) {
    FORCE_OFFLINE.store(force, Ordering::Relaxed);
    if force {
        ONLINE.store(false, Ordering::Relaxed);
    }
}

pub fn is_sync_paused() -> bool {
    SYNC_PAUSED.load(Ordering::Relaxed)
}

pub fn toggle_sync_paused() -> bool {
    let new_value = !SYNC_PAUSED.load(Ordering::Relaxed);
    SYNC_PAUSED.store(new_value, Ordering::Relaxed);
    new_value
}

pub fn retry_sync_now() {
    wake_journal();
}

pub(crate) fn sync_notify() -> &'static tokio::sync::Notify {
    &JOURNAL_NOTIFY
}

fn wake_journal() {
    SYNC_NOW.store(true, Ordering::Relaxed);
    JOURNAL_NOTIFY.notify_one();
}

fn enqueue_journal_and_wake(db: &FuseDb, event_type: &str, ino: u64, payload: &str) {
    let _ = db.enqueue_journal(event_type, ino, payload);
    wake_journal();
}

pub fn default_mountpoint() -> anyhow::Result<PathBuf> {
    Ok(dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("failed to resolve home directory"))?
        .join("ProtonDrive"))
}

pub fn unmount_path(path: &std::path::Path) {
    let p = path.to_string_lossy();
    let _ = std::process::Command::new("fusermount3")
        .args(["-u", "-z", &*p])
        .status()
        .or_else(|_| {
            std::process::Command::new("fusermount")
                .args(["-u", "-z", &*p])
                .status()
        });
}

/// Best-effort unmount via fusermount. Safe to call from signal handlers
/// (spawns a process — technically not async-signal-safe, but this runs
/// right before exit so it's the pragmatic choice).
pub fn force_unmount() {
    if let Some(path) = MOUNT_PATH.get() {
        unmount_path(path);
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
    storage_info: Arc<RwLock<Option<(i64, i64)>>>,
    next_fh: AtomicU64,
    open_files: RwLock<HashMap<u64, Mutex<OpenFile>>>,
    uid: u32,
    gid: u32,
    tracker: TransferTracker,
    thumb_config: ThumbnailConfig,
    populating: Arc<Mutex<HashSet<u64>>>,
}

impl ProtonDriveFs {
    pub fn new(
        db: FuseDb,
        cache_dir: PathBuf,
        drive: ProtonDriveClient,
        rt: tokio::runtime::Handle,
        tracker: TransferTracker,
        storage_info: Option<(i64, i64)>,
    ) -> Self {
        let thumb_config = ThumbnailConfig::load();
        Self {
            db: Arc::new(Mutex::new(db)),
            cache_dir,
            drive,
            rt,
            storage_info: Arc::new(RwLock::new(storage_info)),
            next_fh: AtomicU64::new(1),
            open_files: RwLock::new(HashMap::new()),
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            tracker,
            thumb_config,
            populating: Arc::new(Mutex::new(HashSet::new())),
        }
    }

    /// Start background tasks for event polling and journal flushing.
    pub fn spawn_background_workers(&self) {
        let db = self.db.clone();
        let drive = self.drive.clone();
        let cache_dir = self.cache_dir.clone();
        let storage_info = self.storage_info.clone();

        // Event poller
        self.rt.spawn(event_poll_loop(db.clone(), drive.clone()));

        // Journal flusher
        self.rt
            .spawn(journal_flush_loop(db, drive.clone(), cache_dir));

        // Quota refresher. FUSE statfs must never do network I/O because file
        // managers call it synchronously while opening the mount.
        self.rt
            .spawn(storage_info_refresh_loop(drive.clone(), storage_info));

        // Prefetch My Files so the first `ls` does not wait on decrypt.
        let my_files_ino = {
            let db = self.db.lock().unwrap();
            db.ensure_my_files_root().ok()
        };
        if let Some(ino) = my_files_ino {
            self.start_populate(ino);
        }
        let computers_ino = {
            let db = self.db.lock().unwrap();
            db.ensure_computers_root().ok()
        };
        if let Some(ino) = computers_ino {
            self.start_populate(ino);
        }

        self.rt
            .spawn(crate::computers::sync_loop(self.drive.clone()));
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
        let size = self.attr_size(row);
        let blocks = (size + 511) / 512;

        FileAttr {
            ino: INodeNo(row.ino),
            size,
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

    fn attr_size(&self, row: &InodeRow) -> u64 {
        if row.is_dir {
            return 0;
        }

        row.cached_path
            .as_ref()
            .and_then(|path| std::fs::metadata(path).ok())
            .map(|metadata| metadata.len())
            .unwrap_or(row.size)
    }

    fn ensure_my_files_root(&self) -> Option<u64> {
        let db = self.db.lock().unwrap();
        match db.ensure_my_files_root() {
            Ok(ino) => Some(ino),
            Err(e) => {
                tracing::warn!(error = %e, "failed to ensure MyFiles root");
                None
            }
        }
    }

    fn ensure_computers_root(&self) -> Option<u64> {
        let db = self.db.lock().unwrap();
        match db.ensure_computers_root() {
            Ok(ino) => Some(ino),
            Err(e) => {
                tracing::warn!(error = %e, "failed to ensure Computers root");
                None
            }
        }
    }

    fn computers_ino(&self) -> Option<u64> {
        self.db.lock().unwrap().computers_inode().map(|row| row.ino)
    }

    fn is_protected_parent(&self, parent_ino: u64) -> bool {
        parent_ino == ROOT_INO || self.computers_ino() == Some(parent_ino)
    }

    /// Populate the children of a directory if not yet done.
    ///
    /// Cached names are returned immediately. A network refresh runs in the
    /// background. The FUSE thread only waits when this folder has never been
    /// listed (empty cache).
    fn ensure_children_populated(&self, parent_ino: u64) {
        if parent_ino == ROOT_INO {
            if self.ensure_my_files_root().is_some() && self.ensure_computers_root().is_some() {
                let db = self.db.lock().unwrap();
                let _ = db.set_children_populated(ROOT_INO);
            }
            return;
        }

        if self.computers_ino() == Some(parent_ino) {
            let has_cached = {
                let db = self.db.lock().unwrap();
                !db.list_children(parent_ino).is_empty()
            };
            self.start_populate(parent_ino);
            if !has_cached {
                self.wait_until_populated(parent_ino);
            }
            return;
        }

        let (needs_fetch, has_cached) = {
            let db = self.db.lock().unwrap();
            match db.get_inode(parent_ino) {
                Some(r) if r.is_dir && !r.children_populated => {
                    (true, !db.list_children(parent_ino).is_empty())
                }
                _ => (false, false),
            }
        };
        if !needs_fetch {
            return;
        }

        self.start_populate(parent_ino);
        if has_cached {
            return;
        }
        self.wait_until_populated(parent_ino);
    }

    fn start_populate(&self, parent_ino: u64) {
        {
            let mut populating = self.populating.lock().unwrap();
            if !populating.insert(parent_ino) {
                return;
            }
        }
        let db = self.db.clone();
        let drive = self.drive.clone();
        let populating = self.populating.clone();
        self.rt.spawn(async move {
            if let Err(e) = populate_folder_children(db, drive, parent_ino).await {
                tracing::warn!(error = %e, parent_ino, "failed to enumerate children");
            }
            populating.lock().unwrap().remove(&parent_ino);
        });
    }

    fn wait_until_populated(&self, parent_ino: u64) {
        for _ in 0..200 {
            {
                let db = self.db.lock().unwrap();
                if db
                    .get_inode(parent_ino)
                    .is_some_and(|r| r.children_populated)
                {
                    return;
                }
            }
            if !self.populating.lock().unwrap().contains(&parent_ino) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn insert_node_into_db(db: &FuseDb, parent_ino: u64, node: &Node) {
        let uid = node.uid();
        let uid_raw = uid.raw();

        let name = node.base().name.clone();
        let is_dir = matches!(node, Node::Folder(_) | Node::Album(_));
        let (size, media_type, revision_uid, mtime) = match node {
            Node::File(f) | Node::Photo(f) => {
                let sz = f
                    .active_revision
                    .claimed_size
                    .unwrap_or(f.total_size_on_cloud_storage) as u64;
                let rev = f.active_revision.uid.to_string();
                let mt = f
                    .active_revision
                    .claimed_modification_time
                    .map(|t| t.timestamp())
                    .unwrap_or_else(|| node.base().creation_time.timestamp());
                (sz, f.base.media_type.clone(), Some(rev), mt)
            }
            _ => (
                0u64,
                String::new(),
                None,
                node.base().creation_time.timestamp(),
            ),
        };

        if let Some(existing) = db.find_by_node_uid(&uid_raw) {
            let _ = db.update_remote_metadata(
                existing.ino,
                size,
                &media_type,
                revision_uid.as_deref(),
                mtime,
            );
            return;
        }

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

    fn insert_degraded_into_db(db: &FuseDb, parent_ino: u64, node: &DegradedNode) {
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
                if let Ok(metadata) = path.metadata() {
                    let actual_size = metadata.len();
                    if actual_size != row.size {
                        let db = self.db.lock().unwrap();
                        let _ = db.update_size_only(row.ino, actual_size);
                    }
                }
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
            if let Ok(metadata) = cache_path.metadata() {
                let _ = db.update_size_only(row.ino, metadata.len());
            }
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

        // Run on the Tokio pool. Handle::block_on from a FUSE thread plus
        // tokio::spawn inside the downloader can stall until the kernel
        // times the open() out as EIO.
        let drive = self.drive.clone();
        let cache_path_task = cache_path.clone();
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        self.rt.spawn(async move {
            let result = async {
                let downloader = drive.get_file_downloader(revision_uid).await?;
                let file = std::fs::File::create(&cache_path_task)?;
                let writer: Box<dyn std::io::Write + Send> =
                    Box::new(std::io::BufWriter::new(file));
                let controller = downloader.download_to_stream(writer, on_progress);
                controller.completion.await??;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let _ = tx.send(result);
        });
        let res = rx
            .recv()
            .unwrap_or_else(|_| Err(anyhow::anyhow!("download task dropped")));

        match res {
            Ok(()) => {
                tracing::info!(ino = row.ino, name = %row.name, "download complete");
                let db = self.db.lock().unwrap();
                let _ = db.set_cached_path(row.ino, cache_path.to_str());
                if let Ok(metadata) = cache_path.metadata() {
                    let _ = db.update_size_only(row.ino, metadata.len());
                }
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

    fn enqueue_upload_for_row(&self, mut row: InodeRow) {
        if !row.dirty || row.is_dir {
            return;
        }

        {
            let db = self.db.lock().unwrap();
            if is_upload_ignored(&db, &row) {
                tracing::info!(ino = row.ino, name = %row.name, "skipping upload ignored by .pdignore");
                let _ = db.set_dirty(row.ino, false);
                let _ = db.record_sync_event("local", "ignored", Some(&row.name), None);
                return;
            }
        }

        if row.node_uid.is_some() && row.revision_uid.is_none() {
            if let Some(ref node_uid_str) = row.node_uid {
                if let Some(uid) = NodeUid::try_parse(node_uid_str) {
                    match self.rt.block_on(self.drive.get_node_uncached(uid)) {
                        Ok(PotentialObject::Node(Node::File(f)))
                        | Ok(PotentialObject::Node(Node::Photo(f))) => {
                            row.revision_uid = Some(f.active_revision.uid.to_string());
                        }
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                ino = row.ino,
                                "failed to refresh revision before queuing file update"
                            );
                        }
                    }
                }
            }
        }

        let db = self.db.lock().unwrap();
        enqueue_dirty_upload(&db, &row);
    }
}

fn enqueue_dirty_upload(db: &FuseDb, row: &InodeRow) {
    if db.has_pending_journal(row.ino) {
        return;
    }
    let event = if row.node_uid.is_some() && row.revision_uid.is_some() {
        "update_revision"
    } else if row.node_uid.is_some() {
        tracing::warn!(
            ino = row.ino,
            "remote file is dirty but has no active revision; deferring upload"
        );
        return;
    } else {
        "create_file"
    };
    let payload = serde_json::json!({
        "ino": row.ino,
        "cached_path": row.cached_path,
        "parent_ino": row.parent_ino,
        "name": row.name,
        "node_uid": row.node_uid,
        "revision_uid": row.revision_uid,
        "size": row.size,
        "media_type": row.media_type,
        "mtime": row.mtime,
    });
    enqueue_journal_and_wake(db, event, row.ino, &payload.to_string());
}

async fn find_child_file(
    drive: &ProtonDriveClient,
    parent: NodeUid,
    name: &str,
) -> anyhow::Result<Option<(NodeUid, RevisionUid)>> {
    let children =
        proton_drive_sdk::node::folder::FolderOperations::enumerate_children(drive, parent).await?;
    for child in children {
        match child {
            PotentialObject::Node(Node::File(f) | Node::Photo(f)) if f.base.base.name == name => {
                return Ok(Some((
                    f.base.base.uid.clone(),
                    f.active_revision.uid.clone(),
                )));
            }
            _ => {}
        }
    }
    Ok(None)
}

async fn populate_folder_children(
    db: Arc<Mutex<FuseDb>>,
    drive: ProtonDriveClient,
    parent_ino: u64,
) -> anyhow::Result<()> {
    use proton_drive_sdk::futures::StreamExt;

    let parent = {
        let db = db.lock().unwrap();
        db.get_inode(parent_ino)
            .ok_or_else(|| anyhow::anyhow!("missing inode {parent_ino}"))?
    };
    if !parent.is_dir {
        return Ok(());
    }

    let (my_files_ino, computers_ino) = {
        let db = db.lock().unwrap();
        (
            db.my_files_inode().map(|row| row.ino),
            db.computers_inode().map(|row| row.ino),
        )
    };
    if Some(parent_ino) == computers_ino {
        return populate_computers(db, drive, parent_ino).await;
    }

    let mut node_uid_str = parent.node_uid.clone();
    if node_uid_str.is_none() && is_online() && Some(parent_ino) == my_files_ino {
        let folder = drive.get_my_files_folder().await?;
        let uid_raw = folder.base.uid.raw();
        let vol = folder.base.uid.volume_id.raw().to_string();
        let link = folder.base.uid.link_id.raw().to_string();
        let db = db.lock().unwrap();
        db.update_node_uid(parent_ino, &uid_raw, &vol, &link)?;
        node_uid_str = Some(uid_raw);
    }

    let node_uid_str = node_uid_str.ok_or_else(|| anyhow::anyhow!("folder has no node uid"))?;
    let node_uid = NodeUid::try_parse(&node_uid_str)
        .ok_or_else(|| anyhow::anyhow!("invalid node uid {node_uid_str}"))?;

    if !is_online() {
        let db = db.lock().unwrap();
        if !db.list_children(parent_ino).is_empty() {
            db.set_children_populated(parent_ino)?;
        }
        return Ok(());
    }

    let stream = drive.enumerate_folder_children(node_uid).await?;
    tokio::pin!(stream);
    let mut children = Vec::new();
    while let Some(item) = stream.next().await {
        children.push(item?);
    }

    let db = db.lock().unwrap();
    let mut remote_uids = HashSet::new();
    for child in children {
        match child {
            PotentialObject::Node(node) => {
                if node.base().trash_time.is_some() {
                    continue;
                }
                remote_uids.insert(node.uid().raw());
                ProtonDriveFs::insert_node_into_db(&db, parent_ino, &node);
            }
            PotentialObject::Degraded(deg) => {
                remote_uids.insert(deg.uid().raw());
                ProtonDriveFs::insert_degraded_into_db(&db, parent_ino, &deg);
            }
        }
    }
    for local in db.list_children(parent_ino) {
        let Some(ref uid) = local.node_uid else {
            continue;
        };
        if remote_uids.contains(uid) || local.dirty || db.has_pending_journal(local.ino) {
            continue;
        }
        remove_inode_tree(&db, &local);
    }
    db.set_children_populated(parent_ino)?;
    Ok(())
}

async fn populate_computers(
    db: Arc<Mutex<FuseDb>>,
    drive: ProtonDriveClient,
    computers_ino: u64,
) -> anyhow::Result<()> {
    if !is_online() {
        let db = db.lock().unwrap();
        if !db.list_children(computers_ino).is_empty() {
            db.set_children_populated(computers_ino)?;
        }
        return Ok(());
    }

    let devices = drive.list_devices().await?;
    let db = db.lock().unwrap();
    let mut remote_uids = HashSet::new();
    let mut taken = HashSet::new();
    for device in devices {
        let uid_raw = device.root_uid.raw();
        remote_uids.insert(uid_raw.clone());
        let name = crate::computers::fuse_device_name(&device.name, &device.device_id, &taken);
        taken.insert(name.clone());
        if let Some(existing) = db.find_by_node_uid(&uid_raw) {
            if existing.name != name || existing.parent_ino != computers_ino {
                let _ = db.rename_inode(existing.ino, computers_ino, &name);
            }
            continue;
        }
        let _ = db.insert_inode(
            computers_ino,
            &name,
            Some(&uid_raw),
            Some(device.root_uid.volume_id.raw()),
            Some(device.root_uid.link_id.raw()),
            true,
            0,
            "",
            None,
            device.create_time.timestamp(),
        );
    }
    for local in db.list_children(computers_ino) {
        match local.node_uid {
            Some(ref uid)
                if remote_uids.contains(uid)
                    || local.dirty
                    || db.has_pending_journal(local.ino) => {}
            _ => remove_inode_tree(&db, &local),
        }
    }
    db.set_children_populated(computers_ino)?;
    Ok(())
}

fn remove_inode_tree(db: &FuseDb, row: &InodeRow) {
    for child in db.list_children(row.ino) {
        remove_inode_tree(db, &child);
    }
    if let Some(ref cp) = row.cached_path {
        let _ = std::fs::remove_file(cp);
    }
    let _ = db.delete_inode(row.ino);
}

fn apply_remote_delete(db: &FuseDb, link_id: &str) {
    if let Some(row) = db.find_by_link_id(link_id) {
        tracing::info!(name = %row.name, link_id, "remote delete");
        let _ = db.record_sync_event("web", "delete", Some(&row.name), Some(link_id));
        let parent = row.parent_ino;
        remove_inode_tree(db, &row);
        let _ = db.clear_children_populated(parent);
    } else {
        let _ = db.record_sync_event("web", "delete", None, Some(link_id));
    }
}

// ── Filesystem trait implementation ──────────────────────────────────

impl Filesystem for ProtonDriveFs {
    fn init(&mut self, _req: &Request, _config: &mut fuser::KernelConfig) -> std::io::Result<()> {
        tracing::info!("ProtonDriveFs: FUSE init");
        // Ensure root inode row exists.
        let db = self.db.lock().unwrap();
        if db.get_inode(ROOT_INO).is_none() {
            db.insert_root(None, None, None)
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        }
        db.ensure_my_files_root()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
        db.ensure_computers_root()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
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

        {
            let db = self.db.lock().unwrap();
            if let Some(row) = db.lookup_child(parent_ino, name_str) {
                let attr = self.inode_to_attr(&row);
                reply.entry(&TTL, &attr, Generation(0));
                return;
            }
        }

        // Lazily populate children only after the local cache misses. This
        // keeps Nautilus folder naming responsive for local/offline creates.
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

        // Handle truncation.
        if let Some(new_size) = size {
            let row_to_enqueue = {
                let db = self.db.lock().unwrap();
                let _ = db.update_size(ino_u64, new_size);

                // Truncate the cached file if present.
                if let Some(row) = db.get_inode(ino_u64) {
                    if let Some(ref cp) = row.cached_path {
                        if let Ok(f) = std::fs::OpenOptions::new().write(true).open(cp) {
                            let _ = f.set_len(new_size);
                        }
                    }
                }
                let _ = db.set_dirty(ino_u64, true);
                db.get_inode(ino_u64)
            };
            if let Some(row) = row_to_enqueue {
                self.enqueue_upload_for_row(row);
            }
        }

        let db = self.db.lock().unwrap();
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

    fn open(&self, req: &Request, ino: fuser::INodeNo, flags: fuser::OpenFlags, reply: ReplyOpen) {
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
        let is_cached = row
            .cached_path
            .as_ref()
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
                if !is_online() {
                    reply.error(Errno::EHOSTUNREACH);
                } else {
                    reply.error(Errno::EIO);
                }
                return;
            }
        };

        let writable = matches!(
            flags.acc_mode(),
            OpenAccMode::O_WRONLY | OpenAccMode::O_RDWR
        );

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
        let row = {
            let db = self.db.lock().unwrap();
            db.get_inode(ino_u64)
        };
        if let Some(row) = row {
            self.enqueue_upload_for_row(row);
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

        if self.is_protected_parent(parent_ino) {
            reply.error(Errno::EACCES);
            return;
        }

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
            parent_ino,
            name_str,
            None,
            None,
            None,
            false,
            0,
            &media_type,
            None,
            0,
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
        reply.created(
            &TTL,
            &attr,
            Generation(0),
            FileHandle(fh),
            FopenFlags::empty(),
        );
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

        if self.is_protected_parent(parent_ino) {
            reply.error(Errno::EACCES);
            return;
        }

        let db = self.db.lock().unwrap();
        if db.lookup_child(parent_ino, name_str).is_some() {
            reply.error(Errno::EEXIST);
            return;
        }

        let new_ino =
            match db.insert_inode(parent_ino, name_str, None, None, None, true, 0, "", None, 0) {
                Ok(ino) => ino,
                Err(e) => {
                    tracing::warn!(error = %e, "mkdir db insert failed");
                    reply.error(Errno::EIO);
                    return;
                }
            };

        // Enqueue for upstream creation.
        if let Some(row) = db.get_inode(new_ino) {
            if is_upload_ignored(&db, &row) {
                tracing::info!(ino = new_ino, name = %name_str, "skipping folder upload ignored by .pdignore");
                let _ = db.record_sync_event("local", "ignored", Some(name_str), None);
            } else {
                let payload = serde_json::json!({
                    "ino": new_ino,
                    "parent_ino": parent_ino,
                    "name": name_str,
                });
                enqueue_journal_and_wake(&db, "create_folder", new_ino, &payload.to_string());
            }
        }
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

        if self.is_protected_parent(parent_ino) {
            reply.error(Errno::EACCES);
            return;
        }

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
            enqueue_journal_and_wake(&db, "delete", row.ino, &payload.to_string());
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

        if self.is_protected_parent(parent_ino) {
            reply.error(Errno::EACCES);
            return;
        }

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

        drop(db);
        self.ensure_children_populated(row.ino);
        let db = self.db.lock().unwrap();

        // Check if directory is empty.
        if !db.list_children(row.ino).is_empty() {
            reply.error(Errno::ENOTEMPTY);
            return;
        }

        let payload = serde_json::json!({ "node_uid": row.node_uid });
        if row.node_uid.is_some() {
            enqueue_journal_and_wake(&db, "delete", row.ino, &payload.to_string());
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

        if self.is_protected_parent(parent_ino) || self.is_protected_parent(newparent_ino) {
            reply.error(Errno::EACCES);
            return;
        }

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
            if let Some(ref node_uid) = existing.node_uid {
                let payload = serde_json::json!({ "node_uid": node_uid });
                enqueue_journal_and_wake(&db, "delete", existing.ino, &payload.to_string());
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
                enqueue_journal_and_wake(&db, "rename", row.ino, &payload.to_string());
            } else {
                // Move (possibly + rename).
                let new_parent_node_uid = db.get_inode(newparent_ino).and_then(|r| r.node_uid);
                let payload = serde_json::json!({
                    "node_uid": node_uid,
                    "new_parent_node_uid": new_parent_node_uid,
                    "new_name": newname_str,
                });
                enqueue_journal_and_wake(&db, "move", row.ino, &payload.to_string());
            }
        }

        reply.ok();
    }

    fn statfs(&self, _req: &Request, _ino: fuser::INodeNo, reply: ReplyStatfs) {
        let (used, total) = self
            .storage_info
            .read()
            .unwrap()
            .unwrap_or((0, 1_000_000_000)); // 1 GB fallback before quota warmup/offline

        let total = total.max(0) as u64;
        let used = used.max(0) as u64;
        let bsize = BLOCK_SIZE as u64;
        let blocks = total / bsize;
        let bfree = total.saturating_sub(used) / bsize;

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

fn is_upload_ignored(db: &FuseDb, row: &InodeRow) -> bool {
    if row.ino == ROOT_INO {
        return false;
    }

    let path = match db.inode_path(row.ino) {
        Some(path) => path,
        None => return false,
    };
    let my_files_index = path
        .iter()
        .position(|part| part.name == "MyFiles")
        .map(|index| index + 1)
        .unwrap_or(1);
    if path.len() <= my_files_index {
        return false;
    }

    let relative_parts = path[my_files_index..]
        .iter()
        .map(|part| part.name.as_str())
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>();
    if relative_parts.is_empty() {
        return false;
    }

    let relative_path = relative_parts.join("/");
    let mut global_matcher = IgnoreMatcher::new();
    global_matcher.add_ignore_text(&crate::pdignore::load_global_text());
    let mut ignored = global_matcher
        .check(&relative_path, row.is_dir)
        .unwrap_or(false);

    let target_index = path.len() - 1;
    for scope_index in my_files_index.saturating_sub(1)..target_index {
        let scope = &path[scope_index];
        if !scope.is_dir {
            continue;
        }

        let scoped_parts = path[(scope_index + 1)..]
            .iter()
            .map(|part| part.name.as_str())
            .filter(|name| !name.is_empty())
            .collect::<Vec<_>>();
        if scoped_parts.is_empty() {
            continue;
        }

        let Some(ignore_row) = db.lookup_child(scope.ino, ".pdignore") else {
            continue;
        };
        let Some(ignore_path) = ignore_row.cached_path else {
            continue;
        };
        let Ok(ignore_text) = std::fs::read_to_string(ignore_path) else {
            continue;
        };

        let mut matcher = IgnoreMatcher::new();
        matcher.add_ignore_text(&ignore_text);
        if let Some(scoped_ignored) = matcher.check(&scoped_parts.join("/"), row.is_dir) {
            ignored = scoped_ignored;
        }
    }

    ignored
}

// ── Background event-poll loop ───────────────────────────────────────

async fn storage_info_refresh_loop(
    drive: ProtonDriveClient,
    storage_info: Arc<RwLock<Option<(i64, i64)>>>,
) {
    loop {
        tokio::time::sleep(Duration::from_secs(300)).await;

        if FORCE_OFFLINE.load(Ordering::Relaxed) {
            continue;
        }

        match tokio::time::timeout(Duration::from_secs(10), drive.get_user_storage_info()).await {
            Ok(Ok(info)) => {
                *storage_info.write().unwrap() = Some(info);
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "failed to refresh storage quota");
            }
            Err(_) => {
                tracing::warn!("timed out refreshing storage quota");
            }
        }
    }
}

async fn event_poll_loop(db: Arc<Mutex<FuseDb>>, drive: ProtonDriveClient) {
    use proton_drive_sdk::api::events::VolumeEventType;

    // Resolve volume_id from the Proton "My files" inode.
    let volume_id = {
        let cached_vol = {
            let db = db.lock().unwrap();
            db.my_files_inode().and_then(|r| r.volume_id)
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
            None => match drive.get_volume_latest_event_id(volume_id.clone()).await {
                Ok(c) => {
                    let _ = db.lock().unwrap().set_event_cursor(volume_id.raw(), &c);
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
        if FORCE_OFFLINE.load(Ordering::Relaxed) {
            ONLINE.store(false, Ordering::Relaxed);
            tokio::time::sleep(Duration::from_secs(5)).await;
            continue;
        }

        let resp = match drive.poll_volume_events(volume_id.clone(), &cursor).await {
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
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }
        };

        for event in &resp.events {
            let link_id_str = event.link.link_id.raw().to_string();
            if matches!(event.event_type(), Some(VolumeEventType::Delete)) || event.link.is_trashed
            {
                let db = db.lock().unwrap();
                apply_remote_delete(&db, &link_id_str);
                continue;
            }
            match event.event_type() {
                Some(VolumeEventType::Create) => {
                    let db = db.lock().unwrap();
                    let _ = db.record_sync_event("web", "create", None, Some(&link_id_str));
                    if let Some(parent_link_id) = &event.link.parent_link_id {
                        if let Some(parent_row) = db.find_by_link_id(parent_link_id.raw()) {
                            let _ = db.clear_children_populated(parent_row.ino);
                        }
                    }
                }
                Some(VolumeEventType::UpdateMetadata) | Some(VolumeEventType::UpdateContent) => {
                    let db = db.lock().unwrap();
                    let event_type = match event.event_type() {
                        Some(VolumeEventType::UpdateContent) => "update-content",
                        _ => "update-metadata",
                    };
                    if let Some(row) = db.find_by_link_id(&link_id_str) {
                        let _ = db.record_sync_event(
                            "web",
                            event_type,
                            Some(&row.name),
                            Some(&link_id_str),
                        );
                        if let Some(ref cp) = row.cached_path {
                            let _ = std::fs::remove_file(cp);
                        }
                        let _ = db.set_cached_path(row.ino, None);
                        let _ = db.clear_children_populated(row.parent_ino);
                    } else {
                        let _ = db.record_sync_event("web", event_type, None, Some(&link_id_str));
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

        if !resp.more {
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    }
}

// ── Background journal-flush loop ────────────────────────────────────

async fn journal_flush_loop(db: Arc<Mutex<FuseDb>>, drive: ProtonDriveClient, cache_dir: PathBuf) {
    loop {
        let _ = SYNC_NOW.swap(false, Ordering::Relaxed);
        let can_flush = is_online()
            && !SYNC_PAUSED.load(Ordering::Relaxed)
            && !FORCE_OFFLINE.load(Ordering::Relaxed);

        if can_flush {
            let entries = {
                let db = db.lock().unwrap();
                db.load_pending_journal(20)
            };

            let mut file_batch = Vec::new();
            for entry in entries {
                if matches!(entry.event_type.as_str(), "create_file" | "update_revision") {
                    file_batch.push(entry);
                } else {
                    flush_file_entries(&db, &drive, &cache_dir, std::mem::take(&mut file_batch))
                        .await;
                    let result = process_journal_entry(&db, &drive, &cache_dir, &entry).await;
                    apply_journal_result(&db, &entry, result);
                }
            }
            flush_file_entries(&db, &drive, &cache_dir, file_batch).await;

            let _ = db.lock().unwrap().delete_completed_journal();

            let dirty = db.lock().unwrap().dirty_files();
            for row in dirty {
                let db_guard = db.lock().unwrap();
                enqueue_dirty_upload(&db_guard, &row);
            }
        }

        tokio::select! {
            _ = JOURNAL_NOTIFY.notified() => {}
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
}

fn apply_journal_result(
    db: &Mutex<FuseDb>,
    entry: &crate::db::JournalEntry,
    result: anyhow::Result<()>,
) {
    let db = db.lock().unwrap();
    match result {
        Ok(()) => {
            let _ = db.update_journal_status(entry.id, "done", None);
        }
        Err(e) => {
            let msg = e.to_string();
            let is_permanent = msg.contains("No such file or directory")
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

async fn flush_file_entries(
    db: &Arc<Mutex<FuseDb>>,
    drive: &ProtonDriveClient,
    cache_dir: &std::path::Path,
    entries: Vec<crate::db::JournalEntry>,
) {
    if entries.is_empty() {
        return;
    }
    // ponytail: 4-wide uploads; raise if the API stops 429ing
    for chunk in entries.chunks(FILE_UPLOAD_CONCURRENCY) {
        let mut set = tokio::task::JoinSet::new();
        for entry in chunk {
            let db = db.clone();
            let drive = drive.clone();
            let cache_dir = cache_dir.to_path_buf();
            let entry = entry.clone();
            set.spawn(async move {
                let result = process_journal_entry(&db, &drive, &cache_dir, &entry).await;
                (entry, result)
            });
        }
        while let Some(joined) = set.join_next().await {
            match joined {
                Ok((entry, result)) => apply_journal_result(db, &entry, result),
                Err(e) => tracing::warn!(error = %e, "upload task panicked"),
            }
        }
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
    let my_files_ino = {
        let db_guard = db.lock().unwrap();
        db_guard.ensure_my_files_root().ok()
    };

    if Some(parent_ino) == my_files_ino {
        let folder = drive
            .get_my_files_folder()
            .await
            .map_err(|e| anyhow::anyhow!("failed to resolve root from API: {}", e))?;
        let uid_raw = folder.base.uid.raw();
        let vol = folder.base.uid.volume_id.raw().to_string();
        let link = folder.base.uid.link_id.raw().to_string();
        let db_guard = db.lock().unwrap();
        let _ = db_guard.update_node_uid(parent_ino, &uid_raw, &vol, &link);
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

fn cache_path_for_revision_in(cache_dir: &std::path::Path, rev_str: &str) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(rev_str.as_bytes());
    let hash = format!("{:x}", hasher.finalize());
    cache_dir.join(hash)
}

fn move_cache_to_revision_path(current_path: &str, stable_cache_path: PathBuf) -> PathBuf {
    let current_path = PathBuf::from(current_path);
    if current_path == stable_cache_path {
        return stable_cache_path;
    }
    if let Some(parent) = stable_cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if std::fs::rename(&current_path, &stable_cache_path).is_err() {
        let _ = std::fs::copy(&current_path, &stable_cache_path);
    }
    stable_cache_path
}

async fn process_journal_entry(
    db: &Arc<Mutex<FuseDb>>,
    drive: &ProtonDriveClient,
    cache_dir: &std::path::Path,
    entry: &crate::db::JournalEntry,
) -> anyhow::Result<()> {
    let payload: serde_json::Value = serde_json::from_str(&entry.payload)?;

    if matches!(
        entry.event_type.as_str(),
        "create_folder" | "create_file" | "update_revision"
    ) {
        let ignored = {
            let db_guard = db.lock().unwrap();
            db_guard
                .get_inode(entry.ino)
                .map(|row| is_upload_ignored(&db_guard, &row))
                .unwrap_or(false)
        };
        if ignored {
            let db_guard = db.lock().unwrap();
            if let Some(row) = db_guard.get_inode(entry.ino) {
                tracing::info!(
                    ino = row.ino,
                    name = %row.name,
                    event = %entry.event_type,
                    "skipping pending upload ignored by .pdignore"
                );
                let _ = db_guard.set_dirty(row.ino, false);
                let _ = db_guard.record_sync_event("local", "ignored", Some(&row.name), None);
            }
            return Ok(());
        }
    }

    match entry.event_type.as_str() {
        "create_folder" => {
            let row = {
                let db_guard = db.lock().unwrap();
                match db_guard.get_inode(entry.ino) {
                    Some(row) => row,
                    None => return Ok(()),
                }
            };
            if row.node_uid.is_some() {
                return Ok(());
            }
            let parent_ino = row.parent_ino;
            let name = row.name.clone();

            let parent_node_uid = resolve_parent_node_uid(db, drive, parent_ino).await?;
            let parent_uid = NodeUid::parse(&parent_node_uid).map_err(|e| anyhow::anyhow!(e))?;

            let folder = drive.create_folder(parent_uid, name, None).await?;

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
            let row = {
                let db_guard = db.lock().unwrap();
                match db_guard.get_inode(entry.ino) {
                    Some(row) => row,
                    None => return Ok(()),
                }
            };
            if row.node_uid.is_some() && row.revision_uid.is_some() {
                return Ok(());
            }
            let parent_ino = row.parent_ino;
            let name = row.name.clone();
            let cached_path = row
                .cached_path
                .clone()
                .or_else(|| payload["cached_path"].as_str().map(ToOwned::to_owned))
                .ok_or_else(|| anyhow::anyhow!("missing cached_path"))?;
            let size = row.size as i64;
            let media_type = if row.media_type.is_empty() {
                "application/octet-stream".to_string()
            } else {
                row.media_type.clone()
            };

            let parent_node_uid = resolve_parent_node_uid(db, drive, parent_ino).await?;
            let parent_uid = NodeUid::parse(&parent_node_uid).map_err(|e| anyhow::anyhow!(e))?;

            let last_mod = Some(row.mtime)
                .filter(|&t| t > 0)
                .map(|t| UNIX_EPOCH + Duration::from_secs(t as u64));

            let uploader = drive
                .get_file_uploader(
                    parent_uid.clone(),
                    name.clone(),
                    media_type,
                    size,
                    last_mod,
                    None,
                    None,
                    true,
                )
                .await?;

            let file = tokio::fs::File::open(&cached_path).await?;
            let reader: Box<dyn tokio::io::AsyncRead + Unpin + Send> = Box::new(file);
            let node_uid = match uploader
                .upload_from_stream(reader, vec![], Box::new(|_, _| {}))
                .await
            {
                Ok(uid) => uid,
                Err(e) if e.to_string().contains("already exists") => {
                    let Some((_uid, rev)) = find_child_file(drive, parent_uid, &name).await? else {
                        return Err(e);
                    };
                    let uploader = drive
                        .get_file_revision_uploader(rev, size, last_mod, None, None)
                        .await?;
                    let file = tokio::fs::File::open(&cached_path).await?;
                    let reader: Box<dyn tokio::io::AsyncRead + Unpin + Send> = Box::new(file);
                    uploader
                        .upload_from_stream(reader, vec![], Box::new(|_, _| {}))
                        .await?
                }
                Err(e) => return Err(e),
            };

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
            | Ok(PotentialObject::Node(Node::Photo(f))) = drive.get_node_uncached(node_uid).await
            {
                let rev_uid = f.active_revision.uid.to_string();
                let size = f
                    .active_revision
                    .claimed_size
                    .unwrap_or(f.total_size_on_cloud_storage);
                let db_guard = db.lock().unwrap();
                let _ = db_guard.update_revision(entry.ino, &rev_uid, size as u64);
                let stable_cache_path = cache_path_for_revision_in(cache_dir, &rev_uid);
                let stable_cache_path =
                    move_cache_to_revision_path(&cached_path, stable_cache_path);
                let _ = db_guard.set_cached_path(entry.ino, stable_cache_path.to_str());
            }
        }

        "update_revision" => {
            let current_row = {
                let db_guard = db.lock().unwrap();
                db_guard.get_inode(entry.ino)
            };
            let current_row = match current_row {
                Some(row) => row,
                None => return Ok(()),
            };
            let _node_uid_str = payload["node_uid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing node_uid"))?;
            let rev_uid_str = payload["revision_uid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing revision_uid"))?;
            let cached_path = current_row
                .cached_path
                .clone()
                .or_else(|| payload["cached_path"].as_str().map(ToOwned::to_owned))
                .ok_or_else(|| anyhow::anyhow!("missing cached_path"))?;
            let size = current_row.size as i64;

            let revision_uid = RevisionUid::parse(rev_uid_str).map_err(|e| anyhow::anyhow!(e))?;

            let last_mod = Some(current_row.mtime)
                .filter(|&t| t > 0)
                .map(|t| UNIX_EPOCH + Duration::from_secs(t as u64));

            let uploader = drive
                .get_file_revision_uploader(revision_uid, size, last_mod, None, None)
                .await?;

            let file = tokio::fs::File::open(&cached_path).await?;
            let reader: Box<dyn tokio::io::AsyncRead + Unpin + Send> = Box::new(file);
            let new_node_uid = uploader
                .upload_from_stream(reader, vec![], Box::new(|_, _| {}))
                .await?;

            // Refresh the node to get the new revision UID.
            if let Ok(PotentialObject::Node(Node::File(f)))
            | Ok(PotentialObject::Node(Node::Photo(f))) =
                drive.get_node_uncached(new_node_uid).await
            {
                let rev_uid = f.active_revision.uid.to_string();
                let stable_cache_path = cache_path_for_revision_in(cache_dir, &rev_uid);
                let stable_cache_path =
                    move_cache_to_revision_path(&cached_path, stable_cache_path);
                let db = db.lock().unwrap();
                let _ = db.update_revision(
                    entry.ino,
                    &rev_uid,
                    f.active_revision.claimed_size.unwrap_or(size) as u64,
                );
                let _ = db.set_cached_path(entry.ino, stable_cache_path.to_str());
                let _ = db.set_dirty(entry.ino, false);
            }
        }

        "rename" => {
            if db.lock().unwrap().get_inode(entry.ino).is_none() {
                return Ok(());
            }
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
            if db.lock().unwrap().get_inode(entry.ino).is_none() {
                return Ok(());
            }
            let node_uid_str = payload["node_uid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing node_uid"))?;
            let new_parent_str = payload["new_parent_node_uid"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("missing new_parent"))?;
            let new_name = payload["new_name"].as_str();

            let uid = NodeUid::parse(node_uid_str).map_err(|e| anyhow::anyhow!(e))?;
            let new_parent = NodeUid::parse(new_parent_str).map_err(|e| anyhow::anyhow!(e))?;

            drive.move_nodes(vec![uid.clone()], new_parent).await?;

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

pub async fn spawn_fuse_session(
    session: &ProtonAPISession,
    transfer_tracker: TransferTracker,
    force_offline: bool,
) -> anyhow::Result<fuser::BackgroundSession> {
    let config_dir = platform_dirs::AppDirs::new(Some("pdcli"), false)
        .ok_or_else(|| anyhow::anyhow!("failed to resolve config directory"))?
        .config_dir;
    let cache_dir = config_dir.join("fuse_cache");
    std::fs::create_dir_all(&cache_dir)?;
    let db_path = config_dir.join("fuse.db");
    let mountpoint = default_mountpoint()?;
    std::fs::create_dir_all(&mountpoint)?;

    let db = FuseDb::open(&db_path)?;
    let drive = ProtonDriveClient::new(session, None)?;

    set_force_offline(force_offline);
    if !force_offline {
        if let Err(e) = drive.get_my_files_folder().await {
            tracing::warn!(error = %e, "failed to unlock My Files share");
        }
    }

    // Seed root inode.
    db.insert_root(None, None, None)?;
    db.ensure_my_files_root()?;
    db.ensure_computers_root()?;
    reconcile_cached_file_sizes(&db);

    let rt = tokio::runtime::Handle::current();
    let storage_info = if force_offline {
        None
    } else {
        match tokio::time::timeout(Duration::from_secs(10), drive.get_user_storage_info()).await {
            Ok(Ok(info)) => Some(info),
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "failed to load storage quota before mounting");
                None
            }
            Err(_) => {
                tracing::warn!("timed out loading storage quota before mounting");
                None
            }
        }
    };
    let fs = ProtonDriveFs::new(db, cache_dir, drive, rt, transfer_tracker, storage_info);
    fs.spawn_background_workers();

    tracing::info!(mount = %mountpoint.display(), "mounting FUSE filesystem");

    // Clean up any stale mount from a previous crash.
    unmount_path(&mountpoint);

    let mut config = Config::default();
    config.mount_options = vec![MountOption::FSName("proton-drive".into())];
    config.n_threads = Some(FUSE_REQUEST_THREADS);
    let bg = fuser::spawn_mount2(fs, &mountpoint, &config).map_err(|e| {
        anyhow::anyhow!(
            "FUSE mount failed at {}: {e}. On WSL install fuse3 (`sudo apt install fuse3`) and mount on the Linux filesystem, not /mnt/c.",
            mountpoint.display()
        )
    })?;
    let _ = MOUNT_PATH.set(mountpoint);
    tracing::info!("FUSE filesystem mounted successfully");
    Ok(bg)
}

fn reconcile_cached_file_sizes(db: &FuseDb) {
    for (ino, cached_path) in db.cached_file_inodes() {
        let Ok(metadata) = std::fs::metadata(&cached_path) else {
            continue;
        };
        let _ = db.update_size_only(ino, metadata.len());
    }
}

impl Drop for ProtonDrive {
    fn drop(&mut self) {
        // Only unmount when this process owns the FUSE session. The GUI normally
        // delegates mounting to the daemon, so ordinary window shutdown must not
        // tear down the daemon-owned mount.
        if let Some(session) = self.fuse_session.take() {
            drop(session);
            force_unmount();
        }
    }
}
