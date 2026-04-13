use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use proton_drive_sdk::node::NodeUid;
use proton_drive_sdk::volume::VolumeId;

use super::cache::MemoryCache;
use super::models::FsNode;
use super::uploads::{PendingFile, WriteBuffer};
use super::{FIRST_DYNAMIC_INODE, MYFILES_INODE, ROOT_INODE};

/// Internal filesystem state.
pub(super) struct ProtonDriveFsInner {
    pub(super) nodes: BTreeMap<u64, FsNode>,
    pub(super) uid_to_inode: BTreeMap<String, u64>,
    pub(super) link_id_to_inode: BTreeMap<String, u64>,
    pub(super) children: BTreeMap<u64, Vec<u64>>,
    pub(super) loaded_folders: std::collections::HashSet<u64>,
    pub(super) file_cache: MemoryCache,
    pub(super) next_inode: AtomicU64,
    pub(super) root_uid: Option<NodeUid>,
    pub(super) volume_id: Option<VolumeId>,
    pub(super) next_fh: AtomicU64,
    pub(super) pending_files: BTreeMap<u64, PendingFile>,
    pub(super) write_buffers: BTreeMap<u64, WriteBuffer>,
    pub(super) fh_to_inode: BTreeMap<u64, u64>,
    pub(super) last_event_id: Option<String>,
    /// Inodes currently being downloaded (for emblem display)
    pub(super) downloading_inodes: std::collections::HashSet<u64>,
    /// Inodes with queued revision uploads (for deduplication)
    /// When a second save comes for same inode, we skip queuing since
    /// the worker will read fresh content from file_cache anyway.
    pub(super) pending_revision_uploads: std::collections::HashSet<u64>,
}

impl ProtonDriveFsInner {
    pub(super) fn new() -> Self {
        Self {
            nodes: BTreeMap::new(),
            uid_to_inode: BTreeMap::new(),
            link_id_to_inode: BTreeMap::new(),
            children: BTreeMap::new(),
            loaded_folders: std::collections::HashSet::new(),
            file_cache: MemoryCache::new(),
            next_inode: AtomicU64::new(FIRST_DYNAMIC_INODE),
            root_uid: None,
            volume_id: None,
            next_fh: AtomicU64::new(1),
            pending_files: BTreeMap::new(),
            write_buffers: BTreeMap::new(),
            fh_to_inode: BTreeMap::new(),
            last_event_id: None,
            downloading_inodes: std::collections::HashSet::new(),
            pending_revision_uploads: std::collections::HashSet::new(),
        }
    }

    pub(super) fn alloc_inode(&self) -> u64 {
        self.next_inode.fetch_add(1, Ordering::SeqCst)
    }

    pub(super) fn get_or_create_inode(&mut self, uid: &NodeUid) -> u64 {
        let uid_str = uid.to_string();
        if let Some(&inode) = self.uid_to_inode.get(&uid_str) {
            inode
        } else {
            let inode = self.alloc_inode();
            self.uid_to_inode.insert(uid_str, inode);
            inode
        }
    }

    pub(super) fn insert_node(&mut self, node: FsNode, parent_inode: Option<u64>) -> u64 {
        let uid = node.uid().clone();
        let link_id = uid.link_id.raw().to_string();
        let inode = self.get_or_create_inode(&uid);

        self.link_id_to_inode.insert(link_id, inode);
        self.nodes.insert(inode, node);

        if let Some(parent) = parent_inode {
            self.children.entry(parent).or_default().push(inode);
        }

        inode
    }

    pub(super) fn build_relative_path(&self, inode: u64) -> Option<PathBuf> {
        let mut path_components = Vec::new();
        let mut current_inode = inode;

        while current_inode != MYFILES_INODE && current_inode != ROOT_INODE {
            let node = self.nodes.get(&current_inode)?;
            path_components.push(node.name().to_string());

            let parent_uid = node.parent_uid()?;
            current_inode = *self.uid_to_inode.get(&parent_uid.to_string())?;
        }

        path_components.reverse();

        if path_components.is_empty() {
            return None;
        }

        let mut path = PathBuf::new();
        for component in path_components {
            path.push(component);
        }
        Some(path)
    }
}
