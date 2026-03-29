use chrono::{DateTime, Utc};
use dashmap::DashMap;
use glob::Pattern;
use proton_drive_sdk::node::{DegradedNode, Node, NodeUid};
use proton_drive_sdk::utils::PotentialObject;
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone)]
pub struct IndexEntry {
    pub uid: NodeUid,
    pub parent_uid: Option<NodeUid>,
    pub name: String,
    pub is_folder: bool,
    pub size: Option<i64>,
    pub modification_time: Option<DateTime<Utc>>,
    pub media_type: Option<String>,
}

/// A snapshot of a folder's children used for UI-first rollback.
pub struct FolderSnapshot {
    pub parent_uid: NodeUid,
    pub child_uids: Vec<NodeUid>,
    pub entries: Vec<IndexEntry>,
}

pub struct NodeIndex {
    entries: Arc<DashMap<NodeUid, IndexEntry>>,
    children: Arc<DashMap<NodeUid, Vec<NodeUid>>>,
    /// Folders whose children have been fully loaded from the server.
    indexed: Arc<DashMap<NodeUid, bool>>,
    db: Option<Arc<Mutex<rusqlite::Connection>>>,
}

impl NodeIndex {
    pub fn new() -> Self {
        Self {
            entries: Arc::new(DashMap::new()),
            children: Arc::new(DashMap::new()),
            indexed: Arc::new(DashMap::new()),
            db: None,
        }
    }

    /// Creates a NodeIndex backed by a SQLite connection.
    /// Loads all previously-cached entries so the user starts with a warm cache.
    pub fn with_db(conn: rusqlite::Connection) -> anyhow::Result<Self> {
        let all = crate::db::load_all_entries(&conn)?;
        let indexed_uids = crate::db::load_indexed_folders(&conn)?;
        let s = Self {
            entries: Arc::new(DashMap::new()),
            children: Arc::new(DashMap::new()),
            indexed: Arc::new(DashMap::new()),
            db: Some(Arc::new(Mutex::new(conn))),
        };
        for entry in all {
            s.insert_inner(entry);
        }
        for uid in indexed_uids {
            s.indexed.insert(uid, true);
        }
        Ok(s)
    }

    /// Insert without writing to DB (used when loading from DB).
    fn insert_inner(&self, entry: IndexEntry) {
        if let Some(parent) = &entry.parent_uid {
            let uid = entry.uid.clone();
            let mut children = self.children.entry(parent.clone()).or_default();
            if !children.contains(&uid) {
                children.push(uid);
            }
        }
        self.entries.insert(entry.uid.clone(), entry);
    }

    pub fn insert(&self, entry: IndexEntry) {
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                let _ = crate::db::save_entry(&conn, &entry);
            }
        }
        self.insert_inner(entry);
    }

    pub fn remove(&self, uid: &NodeUid) {
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                let _ = crate::db::delete_entry(&conn, uid);
            }
        }
        if let Some((_, entry)) = self.entries.remove(uid) {
            if let Some(parent) = &entry.parent_uid {
                if let Some(mut children) = self.children.get_mut(parent) {
                    children.retain(|c| c != uid);
                }
            }
        }
        // Also remove any children (shallow remove — sub-children remain as orphans
        // but that's fine since they'll never be listed from a missing parent).
        self.children.remove(uid);
        self.indexed.remove(uid);
    }

    pub fn get(&self, uid: &NodeUid) -> Option<IndexEntry> {
        self.entries.get(uid).map(|e| e.clone())
    }

    pub fn get_children(&self, parent: &NodeUid) -> Vec<IndexEntry> {
        let uids = self.children.get(parent).map(|v| v.clone()).unwrap_or_default();
        let mut entries: Vec<IndexEntry> = uids
            .iter()
            .filter_map(|uid| self.entries.get(uid).map(|e| e.clone()))
            .collect();
        entries.sort_by(|a, b| {
            // folders first, then alphabetical
            b.is_folder.cmp(&a.is_folder).then(a.name.cmp(&b.name))
        });
        entries
    }

    pub fn is_indexed(&self, uid: &NodeUid) -> bool {
        self.indexed.get(uid).map(|v| *v).unwrap_or(false)
    }

    pub fn mark_indexed(&self, uid: &NodeUid) {
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                let _ = crate::db::mark_indexed(&conn, uid);
            }
        }
        self.indexed.insert(uid.clone(), true);
    }

    pub fn unmark_indexed(&self, uid: &NodeUid) {
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                let _ = crate::db::unmark_indexed(&conn, uid);
            }
        }
        self.indexed.remove(uid);
    }

    /// Clears all indexed markers. Used when the server requests a full refresh.
    pub fn unmark_all_indexed(&self) {
        self.indexed.clear();
    }

    pub fn find_child_by_name(&self, parent: &NodeUid, name: &str) -> Option<NodeUid> {
        let uids = self.children.get(parent)?;
        for uid in uids.iter() {
            if let Some(entry) = self.entries.get(uid) {
                if entry.name.eq_ignore_ascii_case(name) {
                    return Some(uid.clone());
                }
            }
        }
        None
    }

    /// Returns children of `parent` whose names match `pattern` (glob syntax).
    pub fn match_glob(&self, parent: &NodeUid, pattern: &str) -> Vec<IndexEntry> {
        let Ok(pat) = Pattern::new(pattern) else {
            return self.get_children(parent);
        };
        self.get_children(parent)
            .into_iter()
            .filter(|e| pat.matches(&e.name))
            .collect()
    }

    /// Snapshots the direct children list of `parent` for rollback.
    pub fn snapshot_children(&self, parent: &NodeUid) -> FolderSnapshot {
        let child_uids = self.children.get(parent).map(|v| v.clone()).unwrap_or_default();
        let entries = child_uids
            .iter()
            .filter_map(|uid| self.entries.get(uid).map(|e| e.clone()))
            .collect();
        FolderSnapshot { parent_uid: parent.clone(), child_uids, entries }
    }

    pub fn restore_snapshot(&self, snap: FolderSnapshot) {
        // Remove any entries that were optimistically added.
        self.children.insert(snap.parent_uid.clone(), snap.child_uids.clone());
        for entry in snap.entries {
            self.entries.insert(entry.uid.clone(), entry);
        }
    }

    /// Inserts a node returned from the SDK into the index.
    pub fn insert_node(&self, node: &PotentialObject<Node, DegradedNode>, parent_uid: Option<NodeUid>) {
        match node {
            PotentialObject::Node(n) => {
                let base = n.base();
                let (size, modification_time, media_type, is_folder) = match n {
                    Node::File(f) | Node::Photo(f) => (
                        f.active_revision.claimed_size,
                        f.active_revision.claimed_modification_time,
                        Some(f.base.media_type.clone()),
                        false,
                    ),
                    Node::Folder(_) | Node::Album(_) => (None, None, None, true),
                };
                self.insert(IndexEntry {
                    uid: base.uid.clone(),
                    parent_uid: base.parent_uid.clone().or(parent_uid),
                    name: base.name.clone(),
                    is_folder,
                    size,
                    modification_time,
                    media_type,
                });
            }
            PotentialObject::Degraded(d) => {
                let (base, is_folder) = match d {
                    DegradedNode::Folder(f) | DegradedNode::Album(f) => (&f.base, true),
                    DegradedNode::File(f) | DegradedNode::Photo(f) => (&f.base, false),
                };
                let name = match &base.name {
                    PotentialObject::Node(s) => s.clone(),
                    PotentialObject::Degraded(_) => "<encrypted>".to_string(),
                };
                self.insert(IndexEntry {
                    uid: base.uid.clone(),
                    parent_uid: base.parent_uid.clone().or(parent_uid),
                    name,
                    is_folder,
                    size: None,
                    modification_time: None,
                    media_type: None,
                });
            }
        }
    }

    /// Like `insert_node` but always overrides the parent with `forced_parent`,
    /// ignoring whatever parent the node itself declares. Needed for album photos
    /// whose `parent_uid` points to the photos root, not to the containing album.
    pub fn insert_node_force_parent(
        &self,
        node: &PotentialObject<Node, DegradedNode>,
        forced_parent: NodeUid,
    ) {
        match node {
            PotentialObject::Node(n) => {
                let base = n.base();
                let (size, modification_time, media_type, is_folder) = match n {
                    Node::File(f) | Node::Photo(f) => (
                        f.active_revision.claimed_size,
                        f.active_revision.claimed_modification_time,
                        Some(f.base.media_type.clone()),
                        false,
                    ),
                    Node::Folder(_) | Node::Album(_) => (None, None, None, true),
                };
                self.insert(IndexEntry {
                    uid: base.uid.clone(),
                    parent_uid: Some(forced_parent),
                    name: base.name.clone(),
                    is_folder,
                    size,
                    modification_time,
                    media_type,
                });
            }
            PotentialObject::Degraded(d) => {
                let (base, is_folder) = match d {
                    DegradedNode::Folder(f) | DegradedNode::Album(f) => (&f.base, true),
                    DegradedNode::File(f) | DegradedNode::Photo(f) => (&f.base, false),
                };
                let name = match &base.name {
                    PotentialObject::Node(s) => s.clone(),
                    PotentialObject::Degraded(_) => "<encrypted>".to_string(),
                };
                self.insert(IndexEntry {
                    uid: base.uid.clone(),
                    parent_uid: Some(forced_parent),
                    name,
                    is_folder,
                    size: None,
                    modification_time: None,
                    media_type: None,
                });
            }
        }
    }

    pub fn reparent(&self, uid: &NodeUid, new_parent: &NodeUid) {
        if let Some(mut entry) = self.entries.get_mut(uid) {
            if let Some(old_parent) = entry.parent_uid.clone() {
                if let Some(mut children) = self.children.get_mut(&old_parent) {
                    children.retain(|c| c != uid);
                }
            }
            entry.parent_uid = Some(new_parent.clone());
        }
        self.children.entry(new_parent.clone()).or_default().push(uid.clone());
    }

    pub fn rename_entry(&self, uid: &NodeUid, new_name: String) {
        if let Some(mut entry) = self.entries.get_mut(uid) {
            entry.name = new_name.clone();
            if let Some(db) = &self.db {
                if let Ok(conn) = db.lock() {
                    let _ = crate::db::save_entry(&conn, &*entry);
                }
            }
        }
    }

    /// Find the UID of a root-level entry by its name (for completer use).
    pub fn find_root_by_name(&self, name: &str) -> Option<NodeUid> {
        self.entries
            .iter()
            .find(|e| e.parent_uid.is_none() && e.name.eq_ignore_ascii_case(name))
            .map(|e| e.uid.clone())
    }

    /// Persist trash items (uid, name, is_folder) to the SQLite cache.
    pub fn save_trash_cache(&self, items: &[(NodeUid, String, bool)]) {
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                let _ = crate::db::save_trash_cache(&conn, items);
            }
        }
    }

    /// Load previously-cached trash items from SQLite.
    pub fn load_trash_cache(&self) -> Vec<(NodeUid, String, bool)> {
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                return crate::db::load_trash_cache(&conn);
            }
        }
        vec![]
    }

    /// Persist device rows to SQLite cache.
    pub fn save_devices_cache(&self, rows: &[crate::db::DeviceCacheRow]) {
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                let _ = crate::db::save_devices_cache(&conn, rows);
            }
        }
    }

    /// Load previously-cached device rows from SQLite.
    pub fn load_devices_cache(&self) -> Vec<crate::db::DeviceCacheRow> {
        if let Some(db) = &self.db {
            if let Ok(conn) = db.lock() {
                return crate::db::load_devices_cache(&conn);
            }
        }
        vec![]
    }
}
