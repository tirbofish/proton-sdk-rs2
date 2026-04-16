use std::collections::HashMap;

use proton_drive_sdk::node::{Node, NodeType, NodeUid};

/// A special inode reserved for the FUSE mount root.
pub const ROOT_INO: u64 = 1;

/// A single entry in the inode table.
#[derive(Debug, Clone)]
pub struct IndexEntry {
    /// The Proton Drive NodeUid (None only for the virtual root).
    pub node_uid: Option<NodeUid>,
    /// Display name shown in the filesystem.
    pub name: String,
    /// Whether this entry is a directory.
    pub is_dir: bool,
    /// File size in bytes (0 for directories).
    pub size: u64,
    /// UTC timestamp (seconds since epoch) of last modification.
    pub mtime: i64,
    /// MIME type (e.g. "image/png"). Empty for directories.
    pub media_type: String,
    /// The full `Node` if available (for files we need revision info, etc).
    pub node: Option<Node>,
}

/// In-memory inode ↔ node mapping with parent/child relationships.
///
/// Everything the FUSE filesystem needs is served from here.
pub struct InodeStore {
    next_ino: u64,
    entries: HashMap<u64, IndexEntry>,
    /// parent_ino → vec of child inodes
    children: HashMap<u64, Vec<u64>>,
    /// child_ino → parent_ino
    parent_of: HashMap<u64, u64>,
    /// NodeUid → inode (reverse lookup)
    uid_to_ino: HashMap<NodeUid, u64>,
    /// Set of inodes whose children have been fetched from the server.
    children_fetched: std::collections::HashSet<u64>,
}

impl InodeStore {
    pub fn new() -> Self {
        Self {
            next_ino: ROOT_INO + 1,
            entries: HashMap::new(),
            children: HashMap::new(),
            parent_of: HashMap::new(),
            uid_to_ino: HashMap::new(),
            children_fetched: std::collections::HashSet::new(),
        }
    }

    /// Ensures the virtual root directory (inode 1) exists.
    pub fn ensure_root(&mut self) {
        if self.entries.contains_key(&ROOT_INO) {
            return;
        }
        self.entries.insert(ROOT_INO, IndexEntry {
            node_uid: None,
            name: String::new(),
            is_dir: true,
            size: 0,
            mtime: 0,
            media_type: String::new(),
            node: None,
        });
    }

    fn alloc_ino(&mut self) -> u64 {
        let ino = self.next_ino;
        self.next_ino += 1;
        ino
    }

    /// Insert a node or update it if it already exists (matched by NodeUid).
    /// Returns the assigned inode number.
    ///
    /// If `override_name` is `Some`, that name is used instead of the node's
    /// own decrypted name.
    pub fn insert_or_update_node(&mut self, node: Node, override_name: Option<String>) -> u64 {
        let uid = node.uid().clone();
        let name = override_name.unwrap_or_else(|| node.base().name.clone());
        let is_dir = matches!(node.ty(), NodeType::Folder | NodeType::Album);

        // Extract size, mtime and media_type from the file's active revision
        // when available, falling back to the node-level values.
        let (size, mtime, media_type) = match &node {
            Node::File(f) | Node::Photo(f) => {
                let rev = &f.active_revision;
                let size = rev.claimed_size
                    .map(|s| s as u64)
                    .unwrap_or(f.total_size_on_cloud_storage as u64);
                let mtime = rev.claimed_modification_time
                    .map(|t| t.timestamp())
                    .unwrap_or_else(|| f.base.base.creation_time.timestamp());
                (size, mtime, f.base.media_type.clone())
            }
            _ => (0, node.base().creation_time.timestamp(), String::new()),
        };

        let ino = if let Some(&existing) = self.uid_to_ino.get(&uid) {
            existing
        } else {
            let ino = self.alloc_ino();
            self.uid_to_ino.insert(uid.clone(), ino);
            ino
        };

        self.entries.insert(ino, IndexEntry {
            node_uid: Some(uid),
            name,
            is_dir,
            size,
            mtime,
            media_type,
            node: Some(node),
        });

        ino
    }

    /// Register `child_ino` as a child of `parent_ino`.
    pub fn set_parent(&mut self, child_ino: u64, parent_ino: u64) {
        // Remove from old parent if any
        if let Some(&old_parent) = self.parent_of.get(&child_ino) {
            if old_parent != parent_ino {
                if let Some(siblings) = self.children.get_mut(&old_parent) {
                    siblings.retain(|&c| c != child_ino);
                }
            }
        }
        self.parent_of.insert(child_ino, parent_ino);
        let siblings = self.children.entry(parent_ino).or_default();
        if !siblings.contains(&child_ino) {
            siblings.push(child_ino);
        }
    }

    pub fn get(&self, ino: u64) -> Option<&IndexEntry> {
        self.entries.get(&ino)
    }

    pub fn parent_ino(&self, ino: u64) -> Option<u64> {
        self.parent_of.get(&ino).copied()
    }

    pub fn lookup_child(&self, parent_ino: u64, name: &str) -> Option<u64> {
        self.children.get(&parent_ino)?.iter().find(|&&child| {
            self.entries.get(&child).map_or(false, |e| e.name == name)
        }).copied()
    }

    pub fn has_children(&self, ino: u64) -> bool {
        self.children_fetched.contains(&ino)
    }

    pub fn mark_children_fetched(&mut self, ino: u64) {
        self.children_fetched.insert(ino);
    }

    pub fn list_children(&self, parent_ino: u64) -> Vec<(u64, IndexEntry)> {
        self.children
            .get(&parent_ino)
            .map(|children| {
                children.iter().filter_map(|&ino| {
                    self.entries.get(&ino).map(|e| (ino, e.clone()))
                }).collect()
            })
            .unwrap_or_default()
    }

    pub fn find_ino_by_uid(&self, uid: &NodeUid) -> Option<u64> {
        self.uid_to_ino.get(uid).copied()
    }

    /// Remove an inode and all its children recursively.
    pub fn remove(&mut self, ino: u64) {
        // Collect children first to avoid borrow issues
        let child_inos: Vec<u64> = self.children.get(&ino).cloned().unwrap_or_default();
        for child in child_inos {
            self.remove(child);
        }

        if let Some(entry) = self.entries.remove(&ino) {
            if let Some(uid) = &entry.node_uid {
                self.uid_to_ino.remove(uid);
            }
        }

        // Remove from parent
        if let Some(&parent) = self.parent_of.get(&ino) {
            if let Some(siblings) = self.children.get_mut(&parent) {
                siblings.retain(|&c| c != ino);
            }
        }
        self.parent_of.remove(&ino);
        self.children.remove(&ino);
        self.children_fetched.remove(&ino);
    }

    /// Mark children as not-yet-fetched so the next listing triggers a refresh.
    pub fn invalidate_children(&mut self, ino: u64) {
        self.children_fetched.remove(&ino);
    }
}
