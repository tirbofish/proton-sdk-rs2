use fuser::{FileAttr, FileType, Filesystem, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyEntry, ReplyEmpty, ReplyWrite, Request, ReplyOpen};
use libc::ENOENT;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use std::sync::Arc;
use parking_lot::{Mutex as PLMutex, Condvar as PLCondvar};
use tokio::runtime::Builder as RuntimeBuilder;
use crate::rusqlite_cache::{RusqliteCache, CachedNode};
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::node::{Node, NodeUid};
use proton_drive_sdk::volume::VolumeId;
use proton_drive_sdk::links::LinkId;
use proton_drive_sdk::utils::PotentialObject;
use futures::StreamExt;

const TTL: Duration = Duration::from_secs(0);

const INO_ROOT: u64 = 1;
const INO_MYFILES: u64 = 2;
const INO_PHOTOS: u64 = 4;
const INO_COMPUTERS: u64 = 5;
const INO_PHOTOS_ALBUMS: u64 = 6;
const INO_PHOTOS_ALL: u64 = 7;
const INO_PHOTOS_FAVS: u64 = 8;
const INO_PHOTOS_VIDEOS: u64 = 9;
const INO_PHOTOS_SCREENSHOTS: u64 = 10;
const INO_COMP_BASE: u64 = 50;
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
    pub photos_volume_id: Option<VolumeId>,
    #[allow(dead_code)]
    pub photos_root_link_id: Option<LinkId>,
    pub computers: Vec<(String, String, VolumeId, LinkId)>,
    pub mount_point: PathBuf,
    pub pending_downloads: HashMap<String, DownloadWaiter>,
    pub pending_uploads: HashMap<u64, UploadBuffer>,
    pub next_fh: u64,
    pub provisional_map: Arc<PLMutex<HashMap<String, String>>>,
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
        // For Photo nodes, prefer capture_time as mtime so photo apps (Fotema
        // etc.) see the real shot date rather than the upload/sync time.
        let effective_time = if node.node_type == "Photo" {
            node.capture_time.unwrap_or(node.modification_time)
        } else {
            node.modification_time
        };
        let mtime = Self::dt_to_system_time(effective_time);
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
            p if p >= INO_COMP_BASE && p < INO_START => {
                let idx = (p - INO_COMP_BASE) as usize;
                self.computers.get(idx).map(|(_, _, _, root)| Some(root.clone()))
            }
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
        node_volume_id: &VolumeId,
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
                }
                waiter.1.notify_all();
                return waiter;
            }
        };
        if let Some(parent) = dest_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let client = self.client.clone();
        let volume_id = node_volume_id.clone();
        let link_id = link_id.clone();
        let cache = self.cache.clone();
        let name = name.to_string();
        let size_bytes = size.unwrap_or(0).max(0) as u64;

        let pb = crate::commands::helpers::download_progress_bar(&name, size_bytes);

        std::thread::spawn(move || {
            let rt = RuntimeBuilder::new_current_thread()
                .enable_all()
                .build()
                .expect("tokio rt for download");

            let node_uid = NodeUid::new(volume_id.clone(), link_id.clone());
            let pb_cb = Arc::clone(&pb);
            let result = rt.block_on(async move {
                client
                    .download_to_file(node_uid, &dest_path, Box::new(move |done, total| {
                        if total > 0 { pb_cb.set_length(total as u64); }
                        if done >= 0 { pb_cb.set_position(done as u64); }
                    }))
                    .await
                    .map(|()| {
                        let _ = cache.register_download(&volume_id, &link_id, &dest_path);
                        dest_path
                    })
                    .map_err(|e| e.to_string())
            });

            match &result {
                Ok(_) => {
                    pb.set_position(pb.length().unwrap_or(size_bytes));
                    pb.println(format!("  ✓  {} completed", name));
                    pb.finish_and_clear();
                }
                Err(e) => {
                    pb.println(format!("  ✗  {} failed: {}", name, e));
                    pb.finish_and_clear();
                }
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

    /// Synchronously enumerate and cache all children of a folder.
    /// Forces parent_uid on each node so list_children queries work.
    #[allow(dead_code)]
    fn blocking_index_folder(
        client: &ProtonDriveClient,
        cache: &Arc<RusqliteCache>,
        folder_uid: NodeUid,
        parent_uid: NodeUid,
        is_trash: bool,
    ) {
        let client = client.clone();
        let cache = cache.clone();
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            let rt = RuntimeBuilder::new_current_thread().enable_all().build().expect("rt");
            rt.block_on(async move {
                if let Ok(stream) = client.enumerate_folder_children(folder_uid).await {
                    tokio::pin!(stream);
                    while let Some(item) = stream.next().await {
                        if let Ok(PotentialObject::Node(mut node)) = item {
                            node.set_parent_uid(Some(parent_uid.clone()));
                            let _ = cache.upsert_node(&node, is_trash);
                        }
                    }
                }
            });
            let _ = tx.send(());
        });
        let _ = rx.recv_timeout(std::time::Duration::from_secs(60));
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
        let link_id = LinkId::new(node.link_id.clone());
        let node_uid = NodeUid::new(self.volume_id.clone(), link_id.clone());
        let _ = self.cache.mark_node_trashed(&self.volume_id, &link_id);
        reply.ok();
        let client = self.client.clone();
        std::thread::spawn(move || {
            let rt = RuntimeBuilder::new_current_thread().enable_all().build().expect("tokio rt for trash");
            rt.block_on(async move {
                if let Err(e) = client.trash_nodes(vec![node_uid]).await {
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
            INO_ROOT | INO_MYFILES | INO_PHOTOS | INO_COMPUTERS
            | INO_PHOTOS_ALBUMS | INO_PHOTOS_ALL | INO_PHOTOS_FAVS
            | INO_PHOTOS_VIDEOS | INO_PHOTOS_SCREENSHOTS => reply.attr(&TTL, &self.vdir_attr(ino)),
            p if p >= INO_COMP_BASE && p < INO_START => reply.attr(&TTL, &self.vdir_attr(p)),
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
                "Photos" => reply.entry(&TTL, &self.vdir_attr(INO_PHOTOS), 0),
                "Computers" => reply.entry(&TTL, &self.vdir_attr(INO_COMPUTERS), 0),
                _ => reply.error(ENOENT),
            },
            INO_MYFILES => match self.cache.get_child_by_name(&self.volume_id, Some(&self.root_link_id), &name_str) {
                Ok(Some(node)) if !node.is_trashed => reply.entry(&TTL, &self.node_to_attr(&node), 0),
                _ => reply.error(ENOENT),
            },
            INO_PHOTOS => match name_str.as_ref() {
                "Albums"      => reply.entry(&TTL, &self.vdir_attr(INO_PHOTOS_ALBUMS), 0),
                "All"         => reply.entry(&TTL, &self.vdir_attr(INO_PHOTOS_ALL), 0),
                "Favorites"   => reply.entry(&TTL, &self.vdir_attr(INO_PHOTOS_FAVS), 0),
                "Videos"      => reply.entry(&TTL, &self.vdir_attr(INO_PHOTOS_VIDEOS), 0),
                "Screenshots" => reply.entry(&TTL, &self.vdir_attr(INO_PHOTOS_SCREENSHOTS), 0),
                _ => reply.error(ENOENT),
            },
            INO_PHOTOS_ALBUMS => {
                if let Some(vid) = &self.photos_volume_id {
                    match self.cache.get_album_by_name(vid, &name_str) {
                        Ok(Some(node)) => reply.entry(&TTL, &self.node_to_attr(&node), 0),
                        _ => reply.error(ENOENT),
                    }
                } else { reply.error(ENOENT); }
            }
            INO_PHOTOS_ALL => {
                if let Some(vid) = &self.photos_volume_id {
                    match self.cache.get_photo_by_name(vid, &name_str) {
                        Ok(Some(node)) => reply.entry(&TTL, &self.node_to_attr(&node), 0),
                        _ => reply.error(ENOENT),
                    }
                } else { reply.error(ENOENT); }
            }
            INO_PHOTOS_FAVS => {
                if let Some(vid) = &self.photos_volume_id {
                    match self.cache.get_photo_by_tag_and_name(vid, 0, &name_str) {
                        Ok(Some(node)) => reply.entry(&TTL, &self.node_to_attr(&node), 0),
                        _ => reply.error(ENOENT),
                    }
                } else { reply.error(ENOENT); }
            }
            INO_PHOTOS_VIDEOS => {
                if let Some(vid) = &self.photos_volume_id {
                    match self.cache.get_photo_by_tag_and_name(vid, 2, &name_str) {
                        Ok(Some(node)) => reply.entry(&TTL, &self.node_to_attr(&node), 0),
                        _ => reply.error(ENOENT),
                    }
                } else { reply.error(ENOENT); }
            }
            INO_PHOTOS_SCREENSHOTS => {
                if let Some(vid) = &self.photos_volume_id {
                    match self.cache.get_photo_by_tag_and_name(vid, 1, &name_str) {
                        Ok(Some(node)) => reply.entry(&TTL, &self.node_to_attr(&node), 0),
                        _ => reply.error(ENOENT),
                    }
                } else { reply.error(ENOENT); }
            }
            INO_COMPUTERS => {
                // Each computer is a virtual subdirectory named after the device.
                if let Some(idx) = self.computers.iter().position(|(_, name, _, _)| name == name_str.as_ref()) {
                    let comp_ino = INO_COMP_BASE + idx as u64;
                    reply.entry(&TTL, &self.vdir_attr(comp_ino), 0);
                } else {
                    reply.error(ENOENT);
                }
            }
            p if p >= INO_COMP_BASE && p < INO_START => {
                // Inside a specific computer's root folder.
                let idx = (p - INO_COMP_BASE) as usize;
                if let Some((_, _, vid, root)) = self.computers.get(idx) {
                    match self.cache.get_child_by_name(vid, Some(root), &name_str) {
                        Ok(Some(node)) if !node.is_trashed => reply.entry(&TTL, &self.node_to_attr(&node), 0),
                        _ => reply.error(ENOENT),
                    }
                } else {
                    reply.error(ENOENT);
                }
            }
            _ if parent >= INO_START => {
                if let Ok(Some(p_node)) = self.cache.get_node_by_inode(parent - INO_START) {
                    let vol = VolumeId::new(p_node.volume_id.clone());
                    match self.cache.get_child_by_name(&vol, Some(&LinkId::new(p_node.link_id)), &name_str) {
                        Ok(Some(node)) if !node.is_trashed => reply.entry(&TTL, &self.node_to_attr(&node), 0),
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

    fn mkdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, reply: ReplyEntry) {
        let name_str = name.to_string_lossy().to_string();
        let effective_parent = if parent == INO_ROOT { INO_MYFILES } else { parent };
        let parent_link = match self.resolve_parent_link(effective_parent) {
            Some(Some(link)) => link,
            _ => { reply.error(libc::EPERM); return; }
        };

        // If the parent is still a provisional node (i.e. its background
        // create_folder hasn't finished yet), block until the provisional_map
        // has the real link_id, then swap it in so the API receives a valid ID.
        // A "pending:" prefix means a file upload placeholder — mkdir inside a
        // file is always wrong, so reject immediately.
        let parent_link = if parent_link.raw().starts_with("pending:") {
            reply.error(libc::EPERM);
            return;
        } else if parent_link.raw().starts_with("provisional-dir-") {
            let prov_key = parent_link.raw().to_string();
            let mut real_lid: Option<String> = None;
            // Poll provisional_map for up to ~10 s (40 × 250 ms).
            for _ in 0..40 {
                real_lid = self.provisional_map.lock().get(&prov_key).cloned();
                if real_lid.is_some() { break; }
                // Also check that the provisional node still exists in the cache;
                // if it's gone but provisional_map has no entry, the create failed.
                if self.cache.get_node_by_uid(&self.volume_id, &parent_link).ok().flatten().is_none() {
                    if self.provisional_map.lock().get(&prov_key).is_none() {
                        reply.error(libc::EIO);
                        return;
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            match real_lid {
                Some(lid) => LinkId::new(lid),
                None => { reply.error(libc::ETIMEDOUT); return; }
            }
        } else {
            parent_link
        };

        // For inodes that live in a computer volume the volume_id from the prov-
        // isional_map is not available here; detect via the computers list.
        let volume_id = if effective_parent >= INO_COMP_BASE && effective_parent < INO_START {
            let idx = (effective_parent - INO_COMP_BASE) as usize;
            self.computers.get(idx).map(|(_, _, vid, _)| vid.clone()).unwrap_or_else(|| self.volume_id.clone())
        } else if effective_parent >= INO_START {
            // Look up the cached node's volume_id (computers nodes live in a
            // different volume than the main drive).
            self.cache.get_node_by_inode(effective_parent - INO_START)
                .ok().flatten()
                .map(|n| VolumeId::new(n.volume_id))
                .unwrap_or_else(|| self.volume_id.clone())
        } else {
            self.volume_id.clone()
        };

        self.next_fh += 1;
        let prov_id = format!("provisional-dir-{}", self.next_fh);
        let prov_link = LinkId::new(prov_id.clone());

        let _ = self.cache.insert_provisional_node(
            &volume_id, &prov_id, Some(&parent_link), &name_str, "Folder", None,
        );

        let node = match self.cache.get_node_by_uid(&volume_id, &prov_link) {
            Ok(Some(n)) => n,
            _ => { reply.error(libc::EIO); return; }
        };
        reply.entry(&TTL, &self.node_to_attr(&node), 0);

        let client = self.client.clone();
        let cache = self.cache.clone();
        let prov_map = self.provisional_map.clone();
        let parent_uid = NodeUid::new(volume_id.clone(), parent_link);
        std::thread::spawn(move || {
            let rt = RuntimeBuilder::new_current_thread().enable_all().build().expect("rt");
            rt.block_on(async move {
                match client.create_folder(parent_uid, name_str.clone(), None).await {
                    Ok(folder) => {
                        let real_lid = folder.base.uid.link_id.raw().to_string();
                        let _ = cache.upsert_nodes_batch(&[(Node::Folder(folder), false)]);
                        let _ = cache.delete_node(&volume_id, &prov_link);
                        prov_map.lock().insert(prov_id, real_lid);
                    }
                    Err(e) => {
                        eprintln!("  \x1b[31m✗\x1b[0m mkdir '{}' failed: {e}", name_str);
                        let _ = cache.delete_node(&volume_id, &prov_link);
                    }
                }
            });
        });
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
                entries.push((INO_PHOTOS, FileType::Directory, "Photos".to_string()));
                entries.push((INO_COMPUTERS, FileType::Directory, "Computers".to_string()));
            }
            INO_MYFILES => {
                if let Ok(nodes) = self.cache.list_children(&self.volume_id, Some(&self.root_link_id)) {
                    for node in nodes {
                        let kind = if node.node_type == "Folder" || node.node_type == "Album" { FileType::Directory } else { FileType::RegularFile };
                        entries.push((node.inode.unwrap_or(0) + INO_START, kind, node.name));
                    }
                }
            }
            INO_PHOTOS => {
                entries.push((INO_PHOTOS_ALBUMS,      FileType::Directory, "Albums".to_string()));
                entries.push((INO_PHOTOS_ALL,         FileType::Directory, "All".to_string()));
                entries.push((INO_PHOTOS_FAVS,        FileType::Directory, "Favorites".to_string()));
                entries.push((INO_PHOTOS_VIDEOS,      FileType::Directory, "Videos".to_string()));
                entries.push((INO_PHOTOS_SCREENSHOTS, FileType::Directory, "Screenshots".to_string()));
            }
            INO_PHOTOS_ALBUMS => {
                if let Some(vid) = &self.photos_volume_id.clone() {
                    if let Ok(albums) = self.cache.list_albums(vid) {
                        for node in albums {
                            entries.push((node.inode.unwrap_or(0) + INO_START, FileType::Directory, node.name));
                        }
                    }
                }
            }
            INO_PHOTOS_ALL => {
                if let Some(vid) = &self.photos_volume_id.clone() {
                    if let Ok(photos) = self.cache.list_all_photos(vid) {
                        for node in photos {
                            entries.push((node.inode.unwrap_or(0) + INO_START, FileType::RegularFile, node.name));
                        }
                    }
                }
            }
            INO_PHOTOS_FAVS => {
                if let Some(vid) = &self.photos_volume_id.clone() {
                    if let Ok(photos) = self.cache.list_photos_by_tag(vid, 0) {
                        for node in photos {
                            entries.push((node.inode.unwrap_or(0) + INO_START, FileType::RegularFile, node.name));
                        }
                    }
                }
            }
            INO_PHOTOS_VIDEOS => {
                if let Some(vid) = &self.photos_volume_id.clone() {
                    if let Ok(photos) = self.cache.list_photos_by_tag(vid, 2) {
                        for node in photos {
                            entries.push((node.inode.unwrap_or(0) + INO_START, FileType::RegularFile, node.name));
                        }
                    }
                }
            }
            INO_PHOTOS_SCREENSHOTS => {
                if let Some(vid) = &self.photos_volume_id.clone() {
                    if let Ok(photos) = self.cache.list_photos_by_tag(vid, 1) {
                        for node in photos {
                            entries.push((node.inode.unwrap_or(0) + INO_START, FileType::RegularFile, node.name));
                        }
                    }
                }
            }
            INO_COMPUTERS => {
                for (idx, (_, name, _, _)) in self.computers.iter().enumerate() {
                    entries.push((INO_COMP_BASE + idx as u64, FileType::Directory, name.clone()));
                }
            }
            p if p >= INO_COMP_BASE && p < INO_START => {
                // List the root folder of a specific computer.
                let idx = (p - INO_COMP_BASE) as usize;
                if let Some((_, _, vid, root)) = self.computers.get(idx).cloned() {
                    if let Ok(nodes) = self.cache.list_children(&vid, Some(&root)) {
                        for node in nodes {
                            let kind = if node.node_type == "Folder" || node.node_type == "Album" { FileType::Directory } else { FileType::RegularFile };
                            entries.push((node.inode.unwrap_or(0) + INO_START, kind, node.name));
                        }
                    }
                }
            }
            _ if ino >= INO_START => {
                if let Ok(Some(p_node)) = self.cache.get_node_by_inode(ino - INO_START) {
                    // Use the node's own volume_id so computer subdirectories
                    // (which live in a different volume) are listed correctly.
                    let vol = VolumeId::new(p_node.volume_id.clone());
                    if let Ok(nodes) = self.cache.list_children(&vol, Some(&LinkId::new(p_node.link_id))) {
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
        if ino < INO_START { reply.error(libc::EISDIR); return; }

        let access_mode = flags & libc::O_ACCMODE;
        if access_mode == libc::O_WRONLY {
            reply.error(libc::EACCES);
            return;
        }

        if self.cache.get_node_by_inode(ino - INO_START).ok().flatten().is_none() {
            reply.error(ENOENT);
            return;
        }

        self.next_fh += 1;
        reply.opened(self.next_fh, 0);
    }

    fn read(&mut self, _req: &Request<'_>, ino: u64, _fh: u64, offset: i64, size: u32, _flags: i32, _lock_owner: Option<u64>, reply: ReplyData) {
        if ino < INO_START { reply.error(libc::EISDIR); return; }

        let node = match self.cache.get_node_by_inode(ino - INO_START) {
            Ok(Some(n)) => n,
            _ => { reply.error(ENOENT); return; }
        };
        let link_id = LinkId::new(node.link_id.clone());
        let node_volume_id = VolumeId::new(node.volume_id.clone());

        if let Ok(Some(path)) = self.cache.get_cached_download(&node_volume_id, &link_id) {
            if path.exists() {
                Self::serve_file_range(&path, offset, size, reply);
                return;
            }
        }

        if let Some(ref _thumb_id) = node.thumbnail_id {
            if let Ok(paths) = crate::app_paths::resolve_paths() {
                let thumb_path = paths.cache_dir.join("thumbs").join(&node.link_id);
                if thumb_path.exists() {
                    Self::serve_file_range(&thumb_path, offset, size, reply);
                    return;
                }
            }
        }

        let is_computers_vol = self.photos_volume_id
            .as_ref()
            .map(|pv| node.volume_id != self.volume_id.raw() && node.volume_id != pv.raw())
            .unwrap_or(node.volume_id != self.volume_id.raw());
        if is_computers_vol {
            reply.error(libc::ENODATA);
            return;
        }

        if !self.pending_downloads.contains_key(&node.link_id) {
            let waiter = self.start_background_download(&node_volume_id, &link_id, &node.name, node.size);
            self.pending_downloads.insert(node.link_id.clone(), waiter);
        }
        let waiter = self.pending_downloads[&node.link_id].clone();
        match Self::wait_for_download(&waiter) {
            Ok(path) => {
                self.pending_downloads.remove(&node.link_id);
                Self::serve_file_range(&path, offset, size, reply);
            }
            Err(e) => {
                eprintln!("  \x1b[31m✗\x1b[0m Read failed for '{}': {}", node.name, e);
                reply.error(libc::EIO);
            }
        }
    }

    fn create(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, _mode: u32, _umask: u32, _flags: i32, reply: ReplyCreate) {
        let name_str = name.to_string_lossy().to_string();
        let parent_link = match self.resolve_parent_link(parent) {
            Some(Some(link)) => link,
            Some(None) => { reply.error(libc::EPERM); return; }
            None => { reply.error(ENOENT); return; }
        };

        self.next_fh += 1;
        let fh = self.next_fh;
        let provisional_id = format!("pending:{}", fh);
        let prov_link = LinkId::new(provisional_id.clone());

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

        let _ = self.cache.insert_provisional_node(
            &self.volume_id, &provisional_id, Some(&parent_link), &name_str, "File", Some(0),
        );
        let node = match self.cache.get_node_by_uid(&self.volume_id, &prov_link) {
            Ok(Some(n)) => n,
            _ => { reply.error(libc::EIO); return; }
        };

        self.pending_uploads.insert(fh, UploadBuffer {
            temp_path,
            parent_link_id: parent_link,
            name: name_str,
        });

        reply.created(&TTL, &self.node_to_attr(&node), 0, fh, 0);
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
        if let Some(buf) = self.pending_uploads.remove(&fh) {
            let provisional_id = format!("pending:{}", fh);
            let prov_link = LinkId::new(provisional_id.clone());

            if let Ok(size) = buf.temp_path.metadata().map(|m| m.len() as i64) {
                let _ = self.cache.update_node_size(&self.volume_id, &prov_link, size);
            }

            let client = self.client.clone();
            let volume_id = self.volume_id.clone();
            let cache = self.cache.clone();
            let prov_map = self.provisional_map.clone();
            let raw_parent = buf.parent_link_id.raw().to_string();
            let name = buf.name.clone();
            let temp_path = buf.temp_path.clone();
            std::thread::spawn(move || {
                let rt = RuntimeBuilder::new_current_thread().enable_all().build().expect("rt");
                rt.block_on(async move {
                    let real_parent_raw = if raw_parent.starts_with("provisional-dir-") {
                        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
                        loop {
                            { let m = prov_map.lock(); if let Some(r) = m.get(&raw_parent) { break r.clone(); } }
                            if std::time::Instant::now() >= deadline { break raw_parent.clone(); }
                            tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
                        }
                    } else {
                        raw_parent
                    };
                    let parent_uid = NodeUid::new(volume_id.clone(), LinkId::new(real_parent_raw));
                    let named_path = temp_path.parent().unwrap().join(&name);
                    let _ = std::fs::rename(&temp_path, &named_path);
                    eprintln!("  \x1b[36m↑\x1b[0m Uploading '{}' to Proton Drive...", name);
                    match client.upload_file(&named_path, parent_uid, false, Box::new(|_, _| {})).await {
                        Ok(_) => {
                            let _ = std::fs::remove_file(&named_path);
                            eprintln!("  \x1b[32m✓\x1b[0m '{}' uploaded.", name);
                        }
                        Err(e) => {
                            eprintln!("  \x1b[31m✗\x1b[0m Upload failed for '{}': {}", name, e);
                        }
                    }
                    let _ = cache.delete_node(&volume_id, &prov_link);
                });
            });
        }
        reply.ok();
    }

    fn unlink(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        tracing::trace!("[FUSE] unlink '{}'", name.to_string_lossy());
        self.do_trash_optimistic(parent, name, reply);
    }

    fn rmdir(&mut self, _req: &Request<'_>, parent: u64, name: &OsStr, reply: ReplyEmpty) {
        tracing::trace!("[FUSE] rmdir '{}'", name.to_string_lossy());
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

        tracing::trace!("[FUSE] rename '{}' → '{}' move={} rename={}", name_str, newname_str, need_move, need_rename);
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

#[allow(dead_code)]
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


