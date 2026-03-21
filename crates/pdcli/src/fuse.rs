use fuser::{FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEntry, ReplyEmpty, ReplyWrite, Request, ReplyOpen};
use libc::ENOENT;
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::sync::Arc;
use parking_lot::{Mutex as PLMutex, Condvar as PLCondvar};
use tokio::runtime::Builder as RuntimeBuilder;
use crate::rusqlite_cache::{RusqliteCache, CachedNode};
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::volume::VolumeId;
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::node::NodeUid;

const TTL: Duration = Duration::from_secs(0);

const INO_ROOT: u64 = 1;
const INO_MYFILES: u64 = 2;
const INO_TRASH: u64 = 3;
const INO_PHOTOS: u64 = 4;
const INO_START: u64 = 100;

type DownloadWaiter = Arc<(PLMutex<Option<Result<PathBuf, String>>>, PLCondvar)>;

pub struct UploadBuffer {
    temp_path: PathBuf,
    parent_link_id: LinkId,
    name: String,
}

pub struct ProtonDriveFS {
    pub client: ProtonDriveClient,
    pub cache: Arc<RusqliteCache>,
    pub volume_id: VolumeId,
    pub root_link_id: LinkId,
    pub mount_point: PathBuf,
    pub pending_downloads: HashMap<String, DownloadWaiter>,
    pub pending_uploads: HashMap<u64, UploadBuffer>,
    pub next_fh: u64,
    pub probe_fhs: HashSet<u64>,
    pub probe_last_seen: HashMap<String, std::time::Instant>,
    pub intent_confirmed: HashSet<String>,
}

impl Drop for ProtonDriveFS {
    fn drop(&mut self) {
        let _ = std::process::Command::new("fusermount3")
            .arg("-u")
            .arg("-z")
            .arg(&self.mount_point)
            .output();
    }
}

impl ProtonDriveFS {
    fn dt_to_system_time(dt: chrono::DateTime<chrono::Utc>) -> SystemTime {
        let secs = dt.timestamp();
        let nanos = dt.timestamp_subsec_nanos();
        if secs >= 0 {
            UNIX_EPOCH + Duration::new(secs as u64, nanos)
        } else {
            UNIX_EPOCH - Duration::new((-secs) as u64, nanos)
        }
    }

    fn node_to_attr(&self, node: &CachedNode) -> FileAttr {
        let is_dir = node.node_type == "Folder" || node.node_type == "Album";
        let ino = node.inode.map(|i| i + INO_START).unwrap_or(INO_START);
        let size = node.size.unwrap_or(0).max(0) as u64;
        let mtime = Self::dt_to_system_time(node.modification_time);
        let crtime = Self::dt_to_system_time(node.creation_time);

        FileAttr {
            ino,
            size,
            blocks: (size + 511) / 512,
            atime: mtime,
            mtime,
            ctime: mtime,
            crtime,
            kind: if is_dir { FileType::Directory } else { FileType::RegularFile },
            perm: if is_dir { 0o755 } else { 0o644 },
            nlink: if is_dir { 2 } else { 1 },
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            flags: 0,
            blksize: 4096,
        }
    }

    fn vdir_attr(&self, ino: u64) -> FileAttr {
        let now = SystemTime::now();
        FileAttr {
            ino,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: UNIX_EPOCH,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0,
            flags: 0,
            blksize: 512,
        }
    }

    fn resolve_parent_link(&self, parent: u64) -> Option<Option<LinkId>> {
        match parent {
            INO_MYFILES => Some(Some(self.root_link_id.clone())),
            INO_TRASH => Some(None),
            _ if parent >= INO_START => {
                self.cache
                    .get_node_by_inode(parent - INO_START)
                    .ok()
                    .flatten()
                    .map(|n| Some(LinkId::new(n.link_id)))
            }
            _ => None,
        }
    }

    fn start_background_download(
        &self,
        link_id: &LinkId,
        name: &str,
        size: Option<i64>,
    ) -> DownloadWaiter {
        let waiter: DownloadWaiter = Arc::new((PLMutex::new(None), PLCondvar::new()));
        let waiter_clone = waiter.clone();

        let dest_path = match crate::app_paths::resolve_paths() {
            Ok(p) => p.cache_dir.join("files").join(link_id.raw()),
            Err(e) => {
                {
                    let mut g = waiter.0.lock();
                    *g = Some(Err(e.to_string()));
                } // drop MutexGuard before moving waiter
                waiter.1.notify_all();
                return waiter;
            }
        };
        if let Some(parent) = dest_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let client = self.client.clone();
        let volume_id = self.volume_id.clone();
        let link_id = link_id.clone();
        let cache = self.cache.clone();
        let name = name.to_string();
        let size_str = size
            .map(|s| format_size(s.max(0) as u64))
            .unwrap_or_else(|| "?".to_string());

        eprintln!("\n  \x1b[36m↓\x1b[0m Hydrating '{}' ({})...", name, size_str);

        std::thread::spawn(move || {
            let rt = RuntimeBuilder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt for download");

            let node_uid = NodeUid::new(volume_id.clone(), link_id.clone());
            let result = rt.block_on(async move {
                client
                    .download_to_file(node_uid, &dest_path, Box::new(|_, _| {}))
                    .await
                    .map(|()| {
                        let _ = cache.register_download(&volume_id, &link_id, &dest_path);
                        dest_path
                    })
                    .map_err(|e| e.to_string())
            });

            match &result {
                Ok(_) => eprintln!("  \x1b[32m✓\x1b[0m '{}' ready.", name),
                Err(e) => eprintln!("  \x1b[31m✗\x1b[0m Failed to hydrate '{}': {}", name, e),
            }
            let mut g = waiter_clone.0.lock();
            *g = Some(result);
            waiter_clone.1.notify_all();
        });

        waiter
    }

    fn wait_for_download(waiter: &DownloadWaiter) -> Result<PathBuf, String> {
        let mut g = waiter.0.lock();
        waiter.1.wait_while(&mut g, |v| v.is_none());
        g.as_ref().unwrap().clone()
    }

    fn serve_file_range(path: &std::path::Path, offset: i64, size: u32, reply: ReplyData) {
        use std::io::{Read, Seek, SeekFrom};
        match std::fs::File::open(path) {
            Ok(mut file) => {
                let _ = file.seek(SeekFrom::Start(offset as u64));
                let mut buf = vec![0u8; size as usize];
                let mut total = 0usize;
                loop {
                    match file.read(&mut buf[total..]) {
                        Ok(0) => break,
                        Ok(n) => total += n,
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(_) => { reply.error(libc::EIO); return; }
                    }
                }
                reply.data(&buf[..total]);
            }
            Err(_) => reply.error(libc::EIO),
        }
    }

    fn do_trash_optimistic(&mut self, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        let name_str = name.to_string_lossy().to_string();

        let parent_link_opt = match self.resolve_parent_link(parent) {
            Some(l) => l,
            None => { reply.error(ENOENT); return; }
        };
        let node = match self.cache.get_child_by_name(&self.volume_id, parent_link_opt.as_ref(), &name_str) {
            Ok(Some(n)) => n,
            _ => { reply.error(ENOENT); return; }
        };

        let is_already_trashed = node.is_trashed || parent == INO_TRASH;
        let link_id = LinkId::new(node.link_id.clone());
        let node_uid = NodeUid::new(self.volume_id.clone(), link_id.clone());

        if is_already_trashed {
            let _ = self.cache.delete_node(&self.volume_id, &link_id);
        } else {
            let _ = self.cache.mark_node_trashed(&self.volume_id, &link_id);
        }
        reply.ok();

        let client = self.client.clone();
        std::thread::spawn(move || {
            let rt = RuntimeBuilder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt for trash");
            rt.block_on(async move {
                let result = if is_already_trashed {
                    client.delete_nodes_from_trash(vec![node_uid]).await
                } else {
                    client.trash_nodes(vec![node_uid]).await
                };
                if let Err(e) = result {
                    eprintln!("  \x1b[31m✗\x1b[0m Background trash error: {}", e);
                }
            });
        });
    }
}

impl Filesystem for ProtonDriveFS {
    fn init(&mut self, _req: &Request<'_>, _config: &mut fuser::KernelConfig) -> Result<(), libc::c_int> {
        Ok(())
    }

    fn getattr(&mut self, _req: &Request<'_>, ino: u64, _fh: Option<u64>, reply: ReplyAttr) {
        match ino {
            INO_ROOT | INO_MYFILES | INO_TRASH | INO_PHOTOS => reply.attr(&TTL, &self.vdir_attr(ino)),
            _ if ino >= INO_START => {
                match self.cache.get_node_by_inode(ino - INO_START) {
                    Ok(Some(node)) => reply.attr(&TTL, &self.node_to_attr(&node)),
                    _ => reply.error(ENOENT),
                }
            }
            _ => reply.error(ENOENT),
        }
    }

    fn lookup(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEntry) {
        let name_str = name.to_string_lossy();
        match parent {
            INO_ROOT => match name_str.as_ref() {
                "MyFiles" => reply.entry(&TTL, &self.vdir_attr(INO_MYFILES), 0),
                "Trash" => reply.entry(&TTL, &self.vdir_attr(INO_TRASH), 0),
                "Photos" => reply.entry(&TTL, &self.vdir_attr(INO_PHOTOS), 0),
                _ => reply.error(ENOENT),
            },
            INO_MYFILES => match self.cache.get_child_by_name(&self.volume_id, Some(&self.root_link_id), &name_str) {
                Ok(Some(node)) => reply.entry(&TTL, &self.node_to_attr(&node), 0),
                _ => reply.error(ENOENT),
            },
            INO_TRASH => match self.cache.get_child_by_name(&self.volume_id, None, &name_str) {
                Ok(Some(node)) if node.is_trashed => reply.entry(&TTL, &self.node_to_attr(&node), 0),
                _ => reply.error(ENOENT),
            },
            _ if parent >= INO_START => {
                if let Ok(Some(p_node)) = self.cache.get_node_by_inode(parent - INO_START) {
                    match self.cache.get_child_by_name(&self.volume_id, Some(&LinkId::new(p_node.link_id)), &name_str) {
                        Ok(Some(node)) => reply.entry(&TTL, &self.node_to_attr(&node), 0),
                        _ => reply.error(ENOENT),
                    }
                } else { reply.error(ENOENT) }
            }
            _ => reply.error(ENOENT),
        }
    }

    fn access(&mut self, _req: &Request<'_>, _ino: u64, _mask: i32, reply: fuser::ReplyEmpty) {
        reply.ok();
    }

    fn statfs(&mut self, _req: &Request<'_>, _ino: u64, reply: fuser::ReplyStatfs) {
        reply.statfs(10_000_000, 10_000_000, 10_000_000, 1_000_000, 0, 512, 255, 512);
    }

    fn readdir(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, mut reply: ReplyDirectory) {
        let mut entries = Vec::new();
        entries.push((ino, FileType::Directory, ".".to_string()));
        entries.push((INO_ROOT, FileType::Directory, "..".to_string()));

        match ino {
            INO_ROOT => {
                entries.push((INO_MYFILES, FileType::Directory, "MyFiles".to_string()));
                entries.push((INO_TRASH, FileType::Directory, "Trash".to_string()));
                entries.push((INO_PHOTOS, FileType::Directory, "Photos".to_string()));
            }
            INO_MYFILES => {
                if let Ok(nodes) = self.cache.list_children(&self.volume_id, Some(&self.root_link_id)) {
                    for node in nodes {
                        let kind = if node.node_type == "Folder" || node.node_type == "Album" { FileType::Directory } else { FileType::RegularFile };
                        entries.push((node.inode.unwrap_or(0) + INO_START, kind, node.name));
                    }
                }
            }
            INO_TRASH => {
                if let Ok(nodes) = self.cache.list_trash(&self.volume_id) {
                    for node in nodes {
                        let kind = if node.node_type == "Folder" || node.node_type == "Album" { FileType::Directory } else { FileType::RegularFile };
                        entries.push((node.inode.unwrap_or(0) + INO_START, kind, node.name));
                    }
                }
            }
            _ if ino >= INO_START => {
                if let Ok(Some(p_node)) = self.cache.get_node_by_inode(ino - INO_START) {
                    if let Ok(nodes) = self.cache.list_children(&self.volume_id, Some(&LinkId::new(p_node.link_id))) {
                        for node in nodes {
                            let kind = if node.node_type == "Folder" || node.node_type == "Album" { FileType::Directory } else { FileType::RegularFile };
                            entries.push((node.inode.unwrap_or(0) + INO_START, kind, node.name));
                        }
                    }
                }
            }
            _ => {}
        }

        for (i, (entry_ino, kind, name)) in entries.into_iter().enumerate().skip(offset as usize) {
            if reply.add(entry_ino, (i as i64) + 1, kind, name) {
                break;
            }
        }
        reply.ok();
    }

    fn open(&mut self, _req: &Request<'_>, ino: u64, flags: i32, reply: ReplyOpen) {
        let access_mode = flags & libc::O_ACCMODE;
        if access_mode == libc::O_WRONLY || access_mode == libc::O_RDWR {
            reply.error(libc::EROFS);
            return;
        }
        if ino < INO_START { reply.error(libc::EISDIR); return; }

        let node = match self.cache.get_node_by_inode(ino - INO_START) {
            Ok(Some(n)) => n,
            _ => { reply.error(ENOENT); return; }
        };
        let link_id = LinkId::new(node.link_id.clone());
        eprintln!("  [FUSE] open '{}' flags={:#o}", node.name, flags);

        if let Ok(Some(path)) = self.cache.get_cached_download(&self.volume_id, &link_id) {
            if path.exists() {
                reply.opened(ino, fuser::consts::FOPEN_KEEP_CACHE);
                return;
            }
        }
        self.next_fh += 1;
        let fh = self.next_fh;

        if self.intent_confirmed.contains(&node.link_id) {
            eprintln!("  [FUSE] open '{}' → real open (intent confirmed), fh={}", node.name, fh);
            reply.opened(fh, fuser::consts::FOPEN_DIRECT_IO);
            return;
        }

        const PROBE_WINDOW: std::time::Duration = std::time::Duration::from_millis(500);
        let now = std::time::Instant::now();
        let is_probe = match self.probe_last_seen.get_mut(&node.link_id) {
            None => {
                self.probe_last_seen.insert(node.link_id.clone(), now);
                true
            }
            Some(last) if now.duration_since(*last) < PROBE_WINDOW => {
                *last = now;
                true
            }
            Some(_) => {
                self.probe_last_seen.remove(&node.link_id);
                self.intent_confirmed.insert(node.link_id.clone());
                false
            }
        };

        if is_probe {
            self.probe_fhs.insert(fh);
            eprintln!("  [FUSE] open '{}' → probe (scan), fh={}", node.name, fh);
        } else {
            eprintln!("  [FUSE] open '{}' → real open, fh={}", node.name, fh);
        }
        reply.opened(fh, fuser::consts::FOPEN_DIRECT_IO);
    }

    fn read(&mut self, _req: &Request<'_>, ino: u64, fh: u64, offset: i64, size: u32, _flags: i32, _lock_owner: Option<u64>, reply: ReplyData) {
        if ino < INO_START { reply.error(libc::EISDIR); return; }

        let node = match self.cache.get_node_by_inode(ino - INO_START) {
            Ok(Some(n)) => n,
            _ => { reply.error(ENOENT); return; }
        };
        let link_id = LinkId::new(node.link_id.clone());

        if let Ok(Some(path)) = self.cache.get_cached_download(&self.volume_id, &link_id) {
            if path.exists() {
                Self::serve_file_range(&path, offset, size, reply);
                return;
            }
        }

        if self.probe_fhs.contains(&fh) {
            reply.error(libc::ENODATA);
            return;
        }

        eprintln!("  [FUSE] read '{}' → not cached, hydrating...", node.name);
        if !self.pending_downloads.contains_key(&node.link_id) {
            let waiter = self.start_background_download(&link_id, &node.name, node.size);
            self.pending_downloads.insert(node.link_id.clone(), waiter);
        }
        let waiter = self.pending_downloads[&node.link_id].clone();
        match Self::wait_for_download(&waiter) {
            Ok(path) => {
                self.pending_downloads.remove(&node.link_id);
                self.intent_confirmed.remove(&node.link_id);
                Self::serve_file_range(&path, offset, size, reply);
            }
            Err(e) => {
                eprintln!("  \x1b[31m✗\x1b[0m Read failed for '{}': {}", node.name, e);
                self.intent_confirmed.remove(&node.link_id);
                reply.error(libc::EIO);
            }
        }
    }

    fn create(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let name_str = name.to_string_lossy().to_string();
        eprintln!("  [FUSE] create '{}' in parent inode {}", name_str, parent);

        let parent_link = match self.resolve_parent_link(parent) {
            Some(Some(link)) => link,
            Some(None) => { reply.error(libc::EPERM); return; }
            None => { reply.error(ENOENT); return; }
        };

        self.next_fh += 1;
        let fh = self.next_fh;
        let fake_ino = 1_000_000_000u64 + fh;

        let temp_path = match crate::app_paths::resolve_paths() {
            Ok(p) => p.cache_dir.join("uploads").join(format!("{}.tmp", fh)),
            Err(e) => { eprintln!("  \x1b[31m✗\x1b[0m create: cannot resolve cache dir: {}", e); reply.error(libc::EIO); return; }
        };
        if let Some(dir) = temp_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::File::create(&temp_path) {
            Ok(_) => {}
            Err(e) => { eprintln!("  \x1b[31m✗\x1b[0m create: temp file error: {}", e); reply.error(libc::EIO); return; }
        }

        self.pending_uploads.insert(fh, UploadBuffer {
            temp_path,
            parent_link_id: parent_link,
            name: name_str.clone(),
        });

        let now = std::time::SystemTime::now();
        let attr = FileAttr {
            ino: fake_ino,
            size: 0,
            blocks: 0,
            atime: now, mtime: now, ctime: now, crtime: now,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: unsafe { libc::getuid() },
            gid: unsafe { libc::getgid() },
            rdev: 0, flags: 0, blksize: 4096,
        };
        reply.created(&TTL, &attr, 0, fh, 0);
    }

    fn write(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, offset: i64, data: &[u8], _write_flags: u32, _flags: i32, _lock_owner: Option<u64>, reply: ReplyWrite) {
        use std::io::{Seek, SeekFrom, Write};
        if let Some(buf) = self.pending_uploads.get(&fh) {
            match std::fs::OpenOptions::new().write(true).open(&buf.temp_path) {
                Ok(mut f) => {
                    let _ = f.seek(SeekFrom::Start(offset as u64));
                    match f.write_all(data) {
                        Ok(_) => { reply.written(data.len() as u32); }
                        Err(e) => { eprintln!("  \x1b[31m✗\x1b[0m write error: {}", e); reply.error(libc::EIO); }
                    }
                }
                Err(e) => { eprintln!("  \x1b[31m✗\x1b[0m write open error: {}", e); reply.error(libc::EIO); }
            }
        } else {
            reply.error(libc::EBADF);
        }
    }

    fn release(&mut self, _req: &Request<'_>, _ino: u64, fh: u64, _flags: i32, _lock_owner: Option<u64>, _flush: bool, reply: ReplyEmpty) {
        self.probe_fhs.remove(&fh);
        if let Some(buf) = self.pending_uploads.remove(&fh) {
            eprintln!("  [FUSE] release '{}': uploading in background...", buf.name);

            let provisional_id = format!("pending:{}", fh);
            let file_size = buf.temp_path.metadata().map(|m| m.len() as i64).ok();
            let _ = self.cache.insert_provisional_node(
                &self.volume_id,
                &provisional_id,
                Some(&buf.parent_link_id),
                &buf.name,
                "File",
                file_size,
            );

            let client = self.client.clone();
            let volume_id = self.volume_id.clone();
            let cache = self.cache.clone();
            let parent_uid = NodeUid::new(volume_id.clone(), buf.parent_link_id);
            let name = buf.name.clone();
            let temp_path = buf.temp_path.clone();
            let prov_id = provisional_id.clone();
            std::thread::spawn(move || {
                let rt = RuntimeBuilder::new_current_thread().enable_all().build().expect("rt");
                rt.block_on(async move {
                    let named_path = temp_path.parent().unwrap().join(&name);
                    let _ = std::fs::rename(&temp_path, &named_path);
                    eprintln!("  \x1b[36m↑\x1b[0m Uploading '{}' to Proton Drive...", name);
                    match client.upload_file(&named_path, parent_uid, false, Box::new(|_, _| {})).await {
                        Ok(_) => {
                            let _ = std::fs::remove_file(&named_path);
                            eprintln!("  \x1b[32m✓\x1b[0m '{}' uploaded.", name);
                            let fake_link = proton_drive_sdk::links::LinkId::new(prov_id);
                            let _ = cache.delete_node(&volume_id, &fake_link);
                        }
                        Err(e) => {
                            eprintln!("  \x1b[31m✗\x1b[0m Upload failed for '{}': {}", name, e);
                            let fake_link = proton_drive_sdk::links::LinkId::new(prov_id);
                            let _ = cache.delete_node(&volume_id, &fake_link);
                        }
                    }
                });
            });
        }
        reply.ok();
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        eprintln!("  [FUSE] unlink '{}'", name.to_string_lossy());
        self.do_trash_optimistic(parent, name, reply);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        eprintln!("  [FUSE] rmdir '{}'", name.to_string_lossy());
        self.do_trash_optimistic(parent, name, reply);
    }

    fn rename(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, newparent: u64, newname: &OsStr, _flags: u32, reply: ReplyEmpty) {
        let name_str = name.to_string_lossy().to_string();
        let newname_str = newname.to_string_lossy().to_string();

        let parent_link_opt = match self.resolve_parent_link(parent) {
            Some(l) => l,
            None => { reply.error(ENOENT); return; }
        };
        let node = match self.cache.get_child_by_name(&self.volume_id, parent_link_opt.as_ref(), &name_str) {
            Ok(Some(n)) => n,
            _ => { reply.error(ENOENT); return; }
        };

        let link_id = LinkId::new(node.link_id.clone());
        let node_uid = NodeUid::new(self.volume_id.clone(), link_id.clone());

        let newparent_link = match newparent {
            INO_MYFILES => self.root_link_id.clone(),
            _ if newparent >= INO_START => match self.cache.get_node_by_inode(newparent - INO_START) {
                Ok(Some(n)) => LinkId::new(n.link_id),
                _ => { reply.error(ENOENT); return; }
            },
            _ => { reply.error(libc::EINVAL); return; }
        };

        let need_move = newparent != parent;
        let need_rename = newname_str != name_str;

        eprintln!("  [FUSE] rename '{}' → '{}' move={} rename={}", name_str, newname_str, need_move, need_rename);
        let new_name_opt: Option<&str> = if need_rename { Some(&newname_str) } else { None };
        let new_parent_opt: Option<&LinkId> = if need_move { Some(&newparent_link) } else { None };
        let _ = self.cache.rename_cached_node(&self.volume_id, &link_id, new_name_opt, new_parent_opt);
        reply.ok();

        let client = self.client.clone();
        let volume_id = self.volume_id.clone();
        let newparent_link_clone = newparent_link.clone();
        let newname_str_clone = newname_str.clone();
        std::thread::spawn(move || {
            let rt = RuntimeBuilder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt for rename");
            rt.block_on(async move {
                if need_move {
                    let new_parent_uid = NodeUid::new(volume_id.clone(), newparent_link_clone);
                    if let Err(e) = client.move_nodes(vec![node_uid.clone()], new_parent_uid).await {
                        eprintln!("  \x1b[31m✗\x1b[0m Move failed in background: {}", e);
                        return;
                    }
                }
                if need_rename {
                    if let Err(e) = client.rename_node(node_uid, newname_str_clone, None).await {
                        eprintln!("  \x1b[31m✗\x1b[0m Rename failed in background: {}", e);
                    }
                }
            });
        });
    }
}

fn format_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut i = 0;
    while size >= 1024.0 && i < UNITS.len() - 1 {
        size /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{} B", bytes) } else { format!("{:.1} {}", size, UNITS[i]) }
}


