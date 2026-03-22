use anyhow::Result;
use chrono::{DateTime, Utc};
use rusqlite::{params, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use proton_drive_sdk::node::Node;
use proton_drive_sdk::node::photo::TimelineEntry;
use proton_drive_sdk::volume::VolumeId;
use proton_drive_sdk::links::LinkId;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;

pub struct RusqliteCache {
    pool: Pool<SqliteConnectionManager>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CachedNode {
    pub volume_id: String,
    pub link_id: String,
    pub parent_link_id: Option<String>,
    pub name: String,
    pub node_type: String,
    pub creation_time: DateTime<Utc>,
    pub modification_time: DateTime<Utc>,
    pub is_trashed: bool,
    pub size: Option<i64>,
    pub inode: Option<u64>,
    /// Capture time (EXIF or device time) for Photo nodes; None for all others.
    pub capture_time: Option<DateTime<Utc>>,
    /// Comma-separated PhotoTag integer values, e.g. "0,2" for Favorite+Video. None if unset.
    pub tags: Option<String>,
    /// Server-assigned thumbnail ID (type 1 preferred, type 2 as fallback). None for non-photo nodes.
    pub thumbnail_id: Option<String>,
}

fn row_to_cached_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<CachedNode> {
    Ok(CachedNode {
        volume_id: row.get(0)?,
        link_id: row.get(1)?,
        parent_link_id: row.get(2)?,
        name: row.get(3)?,
        node_type: row.get(4)?,
        creation_time: row.get::<_, String>(5)?.parse().unwrap_or_else(|_| Utc::now()),
        modification_time: row.get::<_, String>(6)?.parse().unwrap_or_else(|_| Utc::now()),
        is_trashed: row.get::<_, i32>(7)? != 0,
        size: row.get(8)?,
        inode: row.get(9)?,
        capture_time: row.get::<_, Option<String>>(10)?.and_then(|s| s.parse().ok()),
        tags: row.get(11)?,
        thumbnail_id: row.get(12)?,
    })
}

impl RusqliteCache {
    pub fn new(path: &Path) -> Result<Self> {
        let manager = SqliteConnectionManager::file(path);
        let pool = Pool::builder()
            .max_size(10)
            .build(manager)?;
        
        let cache = Self { pool };
        cache.init()?;
        Ok(cache)
    }

    fn init(&self) -> Result<()> {
        let conn = self.pool.get()?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS nodes (
                volume_id TEXT NOT NULL,
                link_id TEXT NOT NULL,
                parent_link_id TEXT,
                name TEXT NOT NULL,
                node_type TEXT NOT NULL,
                creation_time TEXT NOT NULL,
                modification_time TEXT NOT NULL,
                is_trashed INTEGER NOT NULL,
                size INTEGER,
                inode INTEGER UNIQUE,
                indexed INTEGER DEFAULT 0,
                PRIMARY KEY (volume_id, link_id)
            )",
            [],
        )?;

        // Migrations
        let columns = conn.prepare("PRAGMA table_info(nodes)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;

        if !columns.contains(&"indexed".to_string()) {
            let _ = conn.execute("ALTER TABLE nodes ADD COLUMN indexed INTEGER DEFAULT 0", []);
        }
        if !columns.contains(&"inode".to_string()) {
            let _ = conn.execute("ALTER TABLE nodes ADD COLUMN inode INTEGER", []);
            let _ = conn.execute("CREATE UNIQUE INDEX IF NOT EXISTS idx_nodes_inode ON nodes (inode)", []);
        }
        
        // Photo metadata columns (added later)
        if !columns.contains(&"capture_time".to_string()) {
            let _ = conn.execute("ALTER TABLE nodes ADD COLUMN capture_time TEXT", []);
        }
        if !columns.contains(&"tags".to_string()) {
            let _ = conn.execute("ALTER TABLE nodes ADD COLUMN tags TEXT", []);
        }
        if !columns.contains(&"thumbnail_id".to_string()) {
            let _ = conn.execute("ALTER TABLE nodes ADD COLUMN thumbnail_id TEXT", []);
        }

        // Ensure all nodes have valid non-zero inodes
        let _ = conn.execute("UPDATE nodes SET inode = rowid + 100 WHERE inode IS NULL", []);
        let _ = conn.execute("UPDATE nodes SET inode = rowid + 10000000 WHERE inode = 0", []);

        conn.execute("CREATE INDEX IF NOT EXISTS idx_nodes_parent ON nodes (volume_id, parent_link_id)", [])?;
        conn.execute("CREATE INDEX IF NOT EXISTS idx_nodes_type ON nodes (volume_id, node_type, is_trashed)", [])?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS file_cache (
                volume_id TEXT NOT NULL,
                link_id TEXT NOT NULL,
                local_path TEXT NOT NULL,
                download_time TEXT NOT NULL,
                PRIMARY KEY (volume_id, link_id)
            )",
            [],
        )?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS sync_state (
                volume_id TEXT PRIMARY KEY,
                latest_event_id TEXT,
                local_root TEXT
            )",
            [],
        )?;

        // Migration: add local_root if it doesn't exist
        let sync_columns = conn.prepare("PRAGMA table_info(sync_state)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?;

        if !sync_columns.contains(&"local_root".to_string()) {
            let _ = conn.execute("ALTER TABLE sync_state ADD COLUMN local_root TEXT", []);
        }

        conn.execute(
            "CREATE TABLE IF NOT EXISTS photos_index_state (
                volume_id TEXT PRIMARY KEY,
                timeline_cursor TEXT
            )",
            [],
        )?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS computer_sync_configs (
                device_id TEXT NOT NULL,
                local_path TEXT NOT NULL,
                PRIMARY KEY (device_id)
            )",
            [],
        )?;

        conn.execute(
            "CREATE TABLE IF NOT EXISTS computer_synced_files (
                device_id  TEXT NOT NULL,
                local_path TEXT NOT NULL,
                file_size  INTEGER NOT NULL,
                mtime_secs INTEGER NOT NULL,
                PRIMARY KEY (device_id, local_path)
            )",
            [],
        )?;

        // Create files directory for persistent cache
        let mut files_dir = PathBuf::from(conn.path().unwrap_or(""));
        files_dir.pop();
        files_dir.push("files");
        let _ = std::fs::create_dir_all(files_dir);

        Ok(())
    }

    pub fn upsert_node(&self, node: &Node, is_trashed: bool) -> Result<()> {
        self.upsert_nodes_batch(&[(node.clone(), is_trashed)])
    }

    pub fn upsert_nodes_batch(&self, items: &[(Node, bool)]) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;

        for (node, is_trashed) in items {
            let base = node.base();
            let (size, modification_time) = match node {
                Node::File(f) | Node::Photo(f) => (
                    Some(f.total_size_on_cloud_storage),
                    f.active_revision
                        .claimed_modification_time
                        .unwrap_or(base.creation_time)
                        .to_rfc3339(),
                ),
                _ => (None, base.creation_time.to_rfc3339()),
            };

                let thumbnail_id: Option<String> = match node {
                Node::File(f) | Node::Photo(f) => {
                    f.active_revision
                        .thumbnails
                        .iter()
                        .find(|t| t.r#type == 1)
                        .or_else(|| f.active_revision.thumbnails.iter().find(|t| t.r#type == 2))
                        .map(|t| t.id.clone())
                }
                _ => None,
            };

            tx.execute(
                "INSERT INTO nodes (
                    volume_id, link_id, parent_link_id, name, node_type,
                    creation_time, modification_time, is_trashed, size, inode,
                    thumbnail_id
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9,
                    COALESCE(
                        (SELECT inode FROM nodes WHERE volume_id = ?1 AND link_id = ?2),
                        (SELECT IFNULL(MAX(inode), 100) + 1 FROM nodes)
                    ),
                    ?10
                )
                ON CONFLICT(volume_id, link_id) DO UPDATE SET
                    parent_link_id = excluded.parent_link_id,
                    name = excluded.name,
                    node_type = excluded.node_type,
                    creation_time = excluded.creation_time,
                    modification_time = excluded.modification_time,
                    is_trashed = excluded.is_trashed,
                    size = excluded.size,
                    inode = CASE WHEN nodes.inode IS NULL OR nodes.inode = 0 THEN excluded.inode ELSE nodes.inode END,
                    thumbnail_id = COALESCE(excluded.thumbnail_id, nodes.thumbnail_id)",
                params![
                    base.uid.volume_id.raw(),
                    base.uid.link_id.raw(),
                    base.parent_uid.as_ref().map(|u| u.link_id.raw()),
                    base.name,
                    format!("{:?}", node.ty()),
                    base.creation_time.to_rfc3339(),
                    modification_time,
                    if *is_trashed { 1 } else { 0 },
                    size,
                    thumbnail_id,
                ],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn get_node_by_uid(&self, volume_id: &VolumeId, link_id: &LinkId) -> Result<Option<CachedNode>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE volume_id = ?1 AND link_id = ?2",
            params![volume_id.raw(), link_id.raw()],
            row_to_cached_node,
        ).optional().map_err(Into::into)
    }

    pub fn get_node_by_inode(&self, inode: u64) -> Result<Option<CachedNode>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE inode = ?1",
            params![inode],
            row_to_cached_node,
        ).optional().map_err(Into::into)
    }

    pub fn get_child_by_name(&self, volume_id: &VolumeId, parent_link_id: Option<&LinkId>, name: &str) -> Result<Option<CachedNode>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE volume_id = ?1 AND parent_link_id IS ?2 AND name = ?3
             ORDER BY is_trashed ASC LIMIT 1",
            params![volume_id.raw(), parent_link_id.map(|id| id.raw()), name],
            row_to_cached_node,
        ).optional().map_err(Into::into)
    }

    pub fn mark_folders_indexed_batch(&self, volume_id: &VolumeId, link_ids: &[LinkId]) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        for link_id in link_ids {
            tx.execute(
                "UPDATE nodes SET indexed = 1 WHERE volume_id = ?1 AND link_id = ?2",
                params![volume_id.raw(), link_id.raw()],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Returns `(indexed_count, total_count)` for all folders in the volume.
    pub fn get_folder_index_progress(&self, volume_id: &VolumeId) -> Result<(u64, u64)> {
        let conn = self.pool.get()?;
        let total: u64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE volume_id = ?1 AND (node_type = 'Folder' OR node_type = 'Album')",
            params![volume_id.raw()],
            |row| row.get(0),
        )?;
        let indexed: u64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE volume_id = ?1 AND (node_type = 'Folder' OR node_type = 'Album') AND indexed = 1",
            params![volume_id.raw()],
            |row| row.get(0),
        )?;
        Ok((indexed, total))
    }

    /// Returns the total number of non-trashed nodes cached for a volume.
    pub fn get_cached_node_count(&self, volume_id: &VolumeId) -> Result<u64> {
        let conn = self.pool.get()?;
        let count: u64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE volume_id = ?1 AND is_trashed = 0",
            params![volume_id.raw()],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    pub fn get_unindexed_folders(&self, volume_id: &VolumeId) -> Result<Vec<(LinkId, String)>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT link_id, name FROM nodes 
             WHERE volume_id = ?1 AND (node_type = 'Folder' OR node_type = 'Album') AND indexed = 0"
        )?;
        let rows = stmt.query_map(params![volume_id.raw()], |row| {
            Ok((LinkId::new(row.get(0)?), row.get(1)?))
        })?;
        let mut res = Vec::new();
        for r in rows {
            res.push(r?);
        }
        Ok(res)
    }

    /// Returns the link IDs of all folders already marked indexed — used to
    /// pre-populate the dedup set so already-done folders are never re-queued.
    pub fn get_indexed_folder_ids(&self, volume_id: &VolumeId) -> Result<Vec<LinkId>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT link_id FROM nodes
             WHERE volume_id = ?1 AND (node_type = 'Folder' OR node_type = 'Album') AND indexed = 1"
        )?;
        let rows = stmt.query_map(params![volume_id.raw()], |row| {
            Ok(LinkId::new(row.get(0)?))
        })?;
        let mut res = Vec::new();
        for r in rows {
            res.push(r?);
        }
        Ok(res)
    }

    pub fn update_node_size(&self, volume_id: &VolumeId, link_id: &LinkId, size: i64) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE nodes SET size = ?1 WHERE volume_id = ?2 AND link_id = ?3",
            params![size, volume_id.raw(), link_id.raw()],
        )?;
        Ok(())
    }

    pub fn list_trash(&self, volume_id: &VolumeId) -> Result<Vec<CachedNode>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE volume_id = ?1 AND is_trashed = 1"
        )?;
        let rows = stmt.query_map(params![volume_id.raw()], row_to_cached_node)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Find a trashed node by name (case-sensitive) in the given volume.
    /// Used by the FUSE `lookup` handler for the virtual Trash directory,
    /// where trashed nodes retain their original (non-null) parent_link_id.
    pub fn get_trashed_by_name(&self, volume_id: &VolumeId, name: &str) -> Result<Option<CachedNode>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE volume_id = ?1 AND is_trashed = 1 AND name = ?2 LIMIT 1",
            params![volume_id.raw(), name],
            row_to_cached_node,
        ).optional().map_err(Into::into)
    }

    pub fn delete_node(&self, volume_id: &VolumeId, link_id: &LinkId) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "DELETE FROM nodes WHERE volume_id = ?1 AND link_id = ?2",
            params![volume_id.raw(), link_id.raw()],
        )?;
        Ok(())
    }

    /// Insert a provisional (optimistic) node before the real upload/event arrives.
    /// The caller supplies a synthetic `link_id` string (e.g. `"pending:<uuid>"`).
    /// When the server responds the events loop will upsert the real node and the
    /// provisional entry can be deleted.
    pub fn insert_provisional_node(
        &self,
        volume_id: &VolumeId,
        provisional_link_id: &str,
        parent_link_id: Option<&LinkId>,
        name: &str,
        node_type: &str,
        size: Option<i64>,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        let now = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT OR IGNORE INTO nodes (
                volume_id, link_id, parent_link_id, name, node_type,
                creation_time, modification_time, is_trashed, size,
                inode
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, 0, ?7,
                (SELECT IFNULL(MAX(inode), 100) + 1 FROM nodes)
            )",
            params![
                volume_id.raw(),
                provisional_link_id,
                parent_link_id.map(|l| l.raw()),
                name,
                node_type,
                now,
                size,
            ],
        )?;
        Ok(())
    }

    pub fn mark_node_trashed(&self, volume_id: &VolumeId, link_id: &LinkId) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE nodes SET is_trashed = 1 WHERE volume_id = ?1 AND link_id = ?2",
            params![volume_id.raw(), link_id.raw()],
        )?;
        Ok(())
    }

    pub fn mark_node_untrashed(&self, volume_id: &VolumeId, link_id: &LinkId) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE nodes SET is_trashed = 0 WHERE volume_id = ?1 AND link_id = ?2",
            params![volume_id.raw(), link_id.raw()],
        )?;
        Ok(())
    }

    pub fn rename_cached_node(
        &self,
        volume_id: &VolumeId,
        link_id: &LinkId,
        new_name: Option<&str>,
        new_parent_link_id: Option<&LinkId>,
    ) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE nodes SET
                name = COALESCE(?3, name),
                parent_link_id = COALESCE(?4, parent_link_id)
             WHERE volume_id = ?1 AND link_id = ?2",
            params![
                volume_id.raw(),
                link_id.raw(),
                new_name,
                new_parent_link_id.map(|l| l.raw()),
            ],
        )?;
        Ok(())
    }

    pub fn list_children(&self, volume_id: &VolumeId, parent_link_id: Option<&LinkId>) -> Result<Vec<CachedNode>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE volume_id = ?1 AND parent_link_id IS ?2 AND is_trashed = 0"
        )?;
        let rows = stmt.query_map(params![volume_id.raw(), parent_link_id.map(|id| id.raw())], row_to_cached_node)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    pub fn set_sync_state(&self, volume_id: &VolumeId, latest_event_id: &str, local_root: Option<&Path>) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO sync_state (volume_id, latest_event_id, local_root) 
             VALUES (?1, ?2, ?3)
             ON CONFLICT(volume_id) DO UPDATE SET 
                latest_event_id = excluded.latest_event_id,
                local_root = COALESCE(excluded.local_root, sync_state.local_root)",
            params![
                volume_id.raw(),
                latest_event_id,
                local_root.map(|p| p.to_string_lossy())
            ],
        )?;
        Ok(())
    }

    pub fn get_sync_state(&self, volume_id: &VolumeId) -> Result<Option<(String, Option<PathBuf>)>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT latest_event_id, local_root FROM sync_state WHERE volume_id = ?1",
            params![volume_id.raw()],
            |row| {
                let event_id: String = row.get(0)?;
                let local_root: Option<String> = row.get(1)?;
                Ok((event_id, local_root.map(PathBuf::from)))
            },
        ).optional().map_err(Into::into)
    }

    pub fn register_download(&self, volume_id: &VolumeId, link_id: &LinkId, local_path: &Path) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT OR REPLACE INTO file_cache (volume_id, link_id, local_path, download_time) VALUES (?1, ?2, ?3, ?4)",
            params![
                volume_id.raw(),
                link_id.raw(),
                local_path.to_string_lossy(),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub fn get_cached_download(&self, volume_id: &VolumeId, link_id: &LinkId) -> Result<Option<PathBuf>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT local_path FROM file_cache WHERE volume_id = ?1 AND link_id = ?2",
            params![volume_id.raw(), link_id.raw()],
            |row| {
                let path: String = row.get(0)?;
                Ok(PathBuf::from(path))
            },
        ).optional().map_err(Into::into)
    }

    // ── Photo-specific queries ─────────────────────────────────────────────

    /// Update the capture_time and tags for a batch of Photo nodes from the
    /// timeline API (only updates rows that already exist in the cache).
    pub fn upsert_photo_metadata_batch(&self, volume_id: &VolumeId, entries: &[TimelineEntry]) -> Result<()> {
        let mut conn = self.pool.get()?;
        let tx = conn.transaction()?;
        for entry in entries {
            let tags_str: String = entry.tags.iter()
                .map(|t| (*t as u32).to_string())
                .collect::<Vec<_>>()
                .join(",");
            let tags_val = if tags_str.is_empty() { None } else { Some(tags_str) };
            tx.execute(
                "UPDATE nodes SET capture_time = ?1, tags = ?2 WHERE volume_id = ?3 AND link_id = ?4",
                params![
                    entry.capture_time.to_rfc3339(),
                    tags_val,
                    volume_id.raw(),
                    entry.uid.link_id.raw(),
                ],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// List all non-trashed Photo-type nodes in the given volume.
    pub fn list_all_photos(&self, volume_id: &VolumeId) -> Result<Vec<CachedNode>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE volume_id = ?1 AND node_type = 'Photo' AND is_trashed = 0
             ORDER BY COALESCE(capture_time, creation_time) DESC"
        )?;
        let rows = stmt.query_map(params![volume_id.raw()], row_to_cached_node)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// List Photo nodes that carry the given tag (PhotoTag enum raw u32 value).
    pub fn list_photos_by_tag(&self, volume_id: &VolumeId, tag: u32) -> Result<Vec<CachedNode>> {
        let conn = self.pool.get()?;
        let tag_pat = format!("%,{},%", tag);
        let mut stmt = conn.prepare(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE volume_id = ?1 AND node_type = 'Photo' AND is_trashed = 0
             AND ',' || COALESCE(tags,'') || ',' LIKE ?2
             ORDER BY COALESCE(capture_time, creation_time) DESC"
        )?;
        let rows = stmt.query_map(params![volume_id.raw(), tag_pat], row_to_cached_node)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// List all Album-type nodes (non-trashed) in the given volume.
    pub fn list_albums(&self, volume_id: &VolumeId) -> Result<Vec<CachedNode>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE volume_id = ?1 AND node_type = 'Album' AND is_trashed = 0
             ORDER BY name ASC"
        )?;
        let rows = stmt.query_map(params![volume_id.raw()], row_to_cached_node)?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Find an Album node by name (case-sensitive) in the given volume.
    pub fn get_album_by_name(&self, volume_id: &VolumeId, name: &str) -> Result<Option<CachedNode>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE volume_id = ?1 AND node_type = 'Album' AND name = ?2 AND is_trashed = 0 LIMIT 1",
            params![volume_id.raw(), name],
            row_to_cached_node,
        ).optional().map_err(Into::into)
    }

    /// Find any Photo node by name in the given volume.
    pub fn get_photo_by_name(&self, volume_id: &VolumeId, name: &str) -> Result<Option<CachedNode>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE volume_id = ?1 AND node_type = 'Photo' AND name = ?2 AND is_trashed = 0 LIMIT 1",
            params![volume_id.raw(), name],
            row_to_cached_node,
        ).optional().map_err(Into::into)
    }

    /// Find a Photo node by name that has a specific tag.
    pub fn get_photo_by_tag_and_name(&self, volume_id: &VolumeId, tag: u32, name: &str) -> Result<Option<CachedNode>> {
        let conn = self.pool.get()?;
        let tag_pat = format!("%,{},%", tag);
        conn.query_row(
            "SELECT volume_id, link_id, parent_link_id, name, node_type, creation_time, modification_time, is_trashed, size, inode, capture_time, tags, thumbnail_id 
             FROM nodes WHERE volume_id = ?1 AND node_type = 'Photo' AND name = ?2 AND is_trashed = 0
             AND ',' || COALESCE(tags,'') || ',' LIKE ?3 LIMIT 1",
            params![volume_id.raw(), name, tag_pat],
            row_to_cached_node,
        ).optional().map_err(Into::into)
    }

    /// Get the saved timeline cursor (last processed LinkID) for resumability.
    pub fn get_timeline_cursor(&self, volume_id: &VolumeId) -> Result<Option<String>> {
        let conn = self.pool.get()?;
        conn.query_row(
            "SELECT timeline_cursor FROM photos_index_state WHERE volume_id = ?1",
            params![volume_id.raw()],
            |row| row.get(0),
        ).optional().map_err(Into::into)
    }

    /// Save the timeline cursor so indexing can resume after a restart.
    pub fn set_timeline_cursor(&self, volume_id: &VolumeId, cursor_link_id: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO photos_index_state (volume_id, timeline_cursor) VALUES (?1, ?2)
             ON CONFLICT(volume_id) DO UPDATE SET timeline_cursor = excluded.timeline_cursor",
            params![volume_id.raw(), cursor_link_id],
        )?;
        Ok(())
    }

    /// Clear the timeline cursor so the next index cycle starts from the beginning.
    pub fn clear_timeline_cursor(&self, volume_id: &VolumeId) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "UPDATE photos_index_state SET timeline_cursor = NULL WHERE volume_id = ?1",
            params![volume_id.raw()],
        )?;
        Ok(())
    }

    // ── Computers sync config ──────────────────────────────────────────────

    /// Persist a local-folder→device sync mapping so it survives restarts.
    pub fn save_computer_sync_config(&self, device_id: &str, local_path: &std::path::Path) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO computer_sync_configs (device_id, local_path) VALUES (?1, ?2)
             ON CONFLICT(device_id) DO UPDATE SET local_path = excluded.local_path",
            params![device_id, local_path.to_string_lossy()],
        )?;
        Ok(())
    }

    /// Return all stored sync configs as (device_id, local_path) pairs.
    pub fn list_computer_sync_configs(&self) -> Result<Vec<(String, PathBuf)>> {
        let conn = self.pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT device_id, local_path FROM computer_sync_configs"
        )?;
        let rows = stmt.query_map([], |row| {
            let device_id: String = row.get(0)?;
            let local_path: String = row.get(1)?;
            Ok((device_id, PathBuf::from(local_path)))
        })?;
        rows.collect::<rusqlite::Result<Vec<_>>>().map_err(Into::into)
    }

    /// Remove a stored sync config (e.g. when a device is deleted).
    pub fn delete_computer_sync_config(&self, device_id: &str) -> Result<()> {
        let conn = self.pool.get()?;
        conn.execute(
            "DELETE FROM computer_sync_configs WHERE device_id = ?1",
            params![device_id],
        )?;
        Ok(())
    }

    // ── Computer synced-files registry ────────────────────────────────────

    /// Return true if the file at `local_path` was previously synced to
    /// `device_id` with the same size and modification time, meaning it does
    /// not need to be re-uploaded.
    pub fn is_file_unchanged_since_sync(
        &self,
        device_id: &str,
        local_path: &std::path::Path,
    ) -> bool {
        let meta = match std::fs::metadata(local_path) {
            Ok(m) => m,
            Err(_) => return false,
        };
        let size = meta.len() as i64;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let conn = match self.pool.get() {
            Ok(c) => c,
            Err(_) => return false,
        };
        conn.query_row(
            "SELECT 1 FROM computer_synced_files \
             WHERE device_id = ?1 AND local_path = ?2 \
             AND file_size = ?3 AND mtime_secs = ?4",
            params![device_id, local_path.to_string_lossy(), size, mtime],
            |_| Ok(()),
        ).is_ok()
    }

    /// Record that `local_path` was successfully synced to `device_id`.
    pub fn mark_file_synced(
        &self,
        device_id: &str,
        local_path: &std::path::Path,
    ) -> Result<()> {
        let meta = std::fs::metadata(local_path)?;
        let size = meta.len() as i64;
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let conn = self.pool.get()?;
        conn.execute(
            "INSERT INTO computer_synced_files (device_id, local_path, file_size, mtime_secs) \
             VALUES (?1, ?2, ?3, ?4) \
             ON CONFLICT(device_id, local_path) DO UPDATE \
             SET file_size = excluded.file_size, mtime_secs = excluded.mtime_secs",
            params![device_id, local_path.to_string_lossy(), size, mtime],
        )?;
        Ok(())
    }
}
